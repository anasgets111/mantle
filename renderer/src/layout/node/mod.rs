//! Typed, validated properties for `VirtualNode`. `resolve_properties` reads each ordinary `Signal`
//! once per node/pass (ADR-0044 decision 1); `SurfaceTopology`'s five fields and every node's
//! optional `id` stay raw and reject signals. A `panel`'s other properties are live fields, not
//! exceptions. Plain tables remain metamethod-backed, so each `table.get` can still run `__index`;
//! see `EdgeInsets`'s read. A signal resolving to another signal errors rather than
//! reading again, while `MAX_TREE_DEPTH` bounds recursive tree construction.

mod animate;
mod content;
mod paint_style;
pub(crate) mod prop;
mod spec;
mod style;
mod surface;
mod toplevel;

// Paint-only value types are imported, not re-exported; `paint_style` is their sole reader (ADR-0068).
use style::parse_radius;

#[cfg(test)]
pub use animate::Animatable;
pub(crate) use animate::{Animations, Params};
pub use animate::{Dissolve, ShaderParam, TransitionSpec, Tween, advance, depart, is_paint_only, retarget};
#[cfg(test)]
pub(crate) use animate::{Keyframe, Spring, easing_names};
#[cfg(test)]
pub(crate) use content::TextRun;
pub(crate) use content::{Content, Font, Live, MaxLines, Region};
pub use content::{Elide, StyleRun, TextAlign, Wrap, font_runs};
pub use paint_style::{PaintStyle, paint_style};
pub(crate) use spec::{Children, Items, Limit, Root};
pub use spec::{SecureSubmitTarget, SurfaceSpec, lock_spec, parse_list_children};
// `wayland::tests`' and `instance::tests`' fixtures name it `node::LockSpec`; nothing else does.
#[cfg(test)]
pub use spec::LockSpec;
pub use style::{
    Affine, BorderColor, ClipShape, Effect, Fill, Gradient, GradientShape, Mask, MaskSource, Shadow, Transform,
    apply_affine, invert_affine, parse_effect, parse_transform,
};
pub(crate) use style::{Axes, CornerShape, Cursor, Direction, EdgeColors, Insets, Scale, ShadowMode};
pub use surface::{Anchor, Exclusive, KeyboardInteractivity, LayerKind, PanelSpec, SurfaceTopology, panel_spec};
pub(crate) use toplevel::{AnchorRect, PopupExtent};
pub use toplevel::{
    ConstraintAdjustment, PopupAnchor, PopupOffset, PopupSpec, SizeHint, WindowSpec, popup_spec, window_spec,
};

/// A node's property map, keyed by the `&'static str` the config's spelling was matched against
/// (ADR-0219). `FxHashMap` for thirty lookups a node against short literal keys, where SipHash's
/// setup costs more than the comparison (ADR-0218).
pub type PropMap = rustc_hash::FxHashMap<&'static str, Value>;

use mlua::{Lua, Value};

use crate::lua::luacats::{LuaType, spelled};
use crate::lua::marshal;
pub(crate) use crate::lua::nodes::properties::{self as fields, Property};
use crate::lua::signal;
use prop::Prop;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeMode {
    Pixels(f32),
    Percent(f32),
    Content,
    Fill,
}

crate::lua::luacats::lua_shape! {
    /// Per-edge pixels; a missing edge is `0`.
    #[alias = "Edges"]
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct EdgeInsets {
        pub top?: f32,
        pub right?: f32,
        pub bottom?: f32,
        pub left?: f32,
    }
}

impl EdgeInsets {
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

prop::keywords! {
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub enum Align {
        #[default]
        Start,
        Center,
        End,
        Stretch,
    }
}

/// A parsed colour in `0.0..=1.0`, stored as `f32` because femtovg's `Color::rgbaf` takes that
/// form directly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

spelled!(Rgba => prop::Color::lua());

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("unsupported node kind `{0}`")]
    UnsupportedNodeKind(String),
    #[error("invalid value for `{property}`: {detail}")]
    InvalidProperty { property: String, detail: String },
    #[error("`{0}` is a Signal handle, not a plain value -- read it via :get() before returning it from shell.lua")]
    UnsupportedSignalProperty(String),
    /// `prepare` exceeded `MAX_TREE_DEPTH`, from a literal cycle or a depth-generating signal.
    /// `depth` is the 1-based refused level, always `max + 1`.
    #[error(
        "node tree exceeds the maximum depth of {max} levels (at `{kind}`, level {depth}) -- a node holding itself in `children`?"
    )]
    TreeTooDeep { kind: String, depth: u32, max: u32 },
    /// A whole `Scene::apply` exceeded `LAYOUT_PASS_CAP`. Unlike a getter's `InvalidProperty`,
    /// this can happen with no `Signal`, through a resolved table's Lua `__index`.
    #[error(
        "the layout pass exceeded its CPU budget -- a property getter or an `__index` metamethod that does not return?"
    )]
    PassBudgetExceeded,
}

impl LayoutError {
    /// Names the surface this came from, added by `layout::scene::Scene::apply_admitting` as it
    /// walks instances. A whole-scene re-resolve reported one property name for a config with a
    /// dozen surfaces (`invalid value for \`background\`` and nothing else), leaving a reader to
    /// grep every surface that has one; the instance id is right there in the loop.
    ///
    /// It extends `detail` rather than wrapping in a new variant so that `InvalidProperty` stays
    /// the variant callers match on, `property` keeps naming the property alone, and no reader of
    /// this enum has to learn a wrapper. The other variants already name the node kind or the whole
    /// pass, which is enough to find them, and none of them has a free-form field to extend.
    pub(crate) fn on_surface(self, surface: &str) -> Self {
        let Self::InvalidProperty { property, detail } = self else {
            return self;
        };
        Self::InvalidProperty { property, detail: format!("on `{surface}`: {detail}") }
    }

    /// Prepends one step of the walk that reached the failing node, added by `layout::scene`'s
    /// `prepare` for each child it descends into. Segments accumulate as the error unwinds, so the
    /// detail carries the whole path from the surface down.
    ///
    /// The surface alone was not enough. On 2026-09-08 a lock screen reported `invalid value for
    /// \`content\`: on \`lock_screen@eDP-1\`: expected a string or an array of runs, got
    /// Integer(0)` and froze on its last good scene; that surface holds a dozen `text` nodes and
    /// the message distinguished none of them. Reading the config did not find it either, because
    /// the value came from a capability payload no static check evaluates.
    ///
    /// Indices are positions among a parent's `children`, so they are stable to read against the
    /// config but not identities: a `list` renumbers its rows as its source changes.
    pub(crate) fn in_child(self, index: usize, kind: &str) -> Self {
        let Self::InvalidProperty { property, detail } = self else {
            return self;
        };
        Self::InvalidProperty { property, detail: format!("{kind}[{index}] > {detail}") }
    }
}

/// [`marshal::only_keys`] for a property's sub-table, naming the property.
pub(crate) fn only_keys(property: &str, table: &mlua::Table, keys: &[&str]) -> Result<(), LayoutError> {
    marshal::only_keys(table, keys).map_err(|detail| invalid(property, detail))
}

/// The `Signal` held unresolved in `property`, or `None` for any other value. A structural slot
/// (`hover`, `scroll`, `geometry`) holding something else is inert rather than an error: the
/// engine's write handles refuse every kind it must not write.
pub(crate) fn signal_at(properties: &PropMap, property: &str) -> Option<signal::Signal> {
    let Some(Value::UserData(ud)) = properties.get(property) else {
        return None;
    };
    signal::from_userdata(ud)
}

/// Crate-visible for `layout::scene::Scene::apply_one_instance`; all crate `InvalidProperty`
/// values use this helper.
pub(crate) fn invalid(property: &str, detail: impl Into<String>) -> LayoutError {
    LayoutError::InvalidProperty { property: property.to_string(), detail: detail.into() }
}

/// Elements accepted from one config-supplied array: a node's `children`, a `list`'s `source`, and
/// a `text`'s style runs.
///
/// `scene::MAX_TREE_DEPTH` bounds depth; this bounds width, which nothing else does. The layout
/// pass budget does not cover it: that deadline is enforced through a Lua hook, and filling a
/// `children` array is a Rust loop with no Lua in it, so it never fires. Reachable without malice
/// -- a generator that does not terminate, or a `list` over a longer array than anyone expected.
///
/// ponytail: this bounds one node's fan-out, not the whole tree, so depth 64 times this is still
/// far more nodes than any real config builds. A total per-pass node budget is the upgrade, and is
/// what the supervisor's `capabilities::tray::MAX_MENU_NODES` does for the one tree that already needed it.
pub(crate) const MAX_ARRAY_ELEMENTS: usize = 10_000;

/// Maximum rejected-value preview, separate from `marshal::MAX_STRING_BYTES`: 200 bytes bounds a
/// `rescue` `error_log` line without limiting valid string properties.
const MAX_ERROR_VALUE_PREVIEW_BYTES: usize = 200;

/// Crate-visible because `layout::scene`'s `list` parser reports bad `source`, `itemfn`, or `key`
/// values through it. Formats a rejected value for [`invalid`] without first formatting an
/// oversized string in full:
/// `rect { radius = string.rep("x", 20 * 1024 * 1024) }` would otherwise allocate and escape 20 MB
/// on the Wayland dispatch thread. `marshal::check_string`'s 64KB cap does not apply because the
/// wrong-type path never reaches [`checked_string`]. Measured against this function:
///
/// | size | `format!("{value:?}")` | this function |
/// |---|---|---|
/// | 1 MB | 1.64 ms | 0.0088 ms |
/// | 20 MB | 23.96 ms | 0.0057 ms |
/// | 100 MB | 93.88 ms | 0.0061 ms |
///
/// Cost follows the cap, not input size: 23.96 ms exceeds one 60fps frame. Paint does not validate
/// `background`/`radius` (ADR-0068), so the cap bounds the `rescue` message, not a frame.
/// `oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix` guards this.
pub(crate) fn preview_for_error(value: &Value) -> String {
    let Value::String(s) = value else {
        return format!("{value:?}");
    };
    // Borrow the Lua buffer and measure before formatting: this is O(1) and copies nothing.
    let bytes = s.as_bytes();
    let total_len = bytes.len();
    if total_len <= MAX_ERROR_VALUE_PREVIEW_BYTES {
        return format!("{value:?}");
    }
    // Slice before formatting, so a 20 MB string costs O(200 bytes), not O(len). Lossy rendering
    // is intentional: this is a log preview, and the byte boundary may split a codepoint.
    let prefix = String::from_utf8_lossy(&bytes[..MAX_ERROR_VALUE_PREVIEW_BYTES]);
    format!(
        "String({prefix:?}...) -- {total_len} bytes total, truncated to the first {MAX_ERROR_VALUE_PREVIEW_BYTES} here"
    )
}

/// Applies the Lua numeric checks before parser ranges, then checks the narrowed `f32`. This covers
/// literal and `resolve_properties`-resolved numbers, including integers outside `2^53`. A finite
/// `1e300` passes `check_number` but becomes `f32::INFINITY`; without the second check, an
/// unchecked property could reach layout arithmetic, where `inf * 0.0` is `NaN` and
/// `snap_to_physical`'s `as i32` silently becomes 0 (ADR-0044 decision 1).
fn value_as_f32(property: &str, value: &Value) -> Result<Option<f32>, LayoutError> {
    match value {
        Value::Integer(i) => {
            let checked = marshal::check_integer(*i).map_err(|e| invalid(property, e.to_string()))?;
            Ok(Some(checked as f32))
        }
        Value::Number(n) => {
            let checked = marshal::check_number(*n).map_err(|e| invalid(property, e.to_string()))?;
            let narrowed = checked as f32;
            if !narrowed.is_finite() {
                return Err(invalid(
                    property,
                    format!("must be finite, got {checked} which overflows f32 to {narrowed}"),
                ));
            }
            Ok(Some(narrowed))
        }
        _ => Ok(None),
    }
}

/// Runs a Lua string through `marshal::check_string`'s 64KB cap and returns it owned.
fn checked_string(property: &str, s: &mlua::LuaString) -> Result<String, LayoutError> {
    let s = s.to_string_lossy();
    marshal::check_string(&s).map_err(|e| invalid(property, e.to_string()))?;
    Ok(s)
}

/// Strict `#RRGGBB` / `#RRGGBBAA` hex colour parsing (`rect.background`, `border_color`,
/// `text.foreground`). No 3-digit shorthand, no named colours, no bare digits without `#`: none
/// of them is specified.
fn parse_hex_color(property: &str, s: &str) -> Result<Rgba, LayoutError> {
    let Some(digits) = s.strip_prefix('#') else {
        return Err(invalid(property, format!("hex colour must start with `#`, got `{s}`")));
    };
    // Digit check before length check, deliberately: every multi-byte UTF-8 byte is >= 0x80 and
    // fails `is_ascii_hexdigit`, giving non-ASCII input (`"#日本語"`) the accurate diagnosis. After
    // it, the string is known ASCII, so `.len()` is a character count, not the wrong byte count
    // (9 bytes, 3 characters) `"#日本語"` would otherwise report.
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(property, format!("hex colour must contain only hex digits, got `{s}`")));
    }
    if digits.len() != 6 && digits.len() != 8 {
        return Err(invalid(
            property,
            format!("hex colour must have 6 or 8 hex digits after `#`, got {} in `{s}`", digits.len()),
        ));
    }
    let channel = |range: std::ops::Range<usize>| -> f32 {
        u8::from_str_radix(&digits[range], 16).expect("digits validated as hex above") as f32 / 255.0
    };
    let a = if digits.len() == 8 { channel(6..8) } else { 1.0 };
    Ok(Rgba { r: channel(0..2), g: channel(2..4), b: channel(4..6), a })
}

/// Whether `property` is one [`resolve_properties`] copies through untouched on a node of this
/// `kind`: its field's type reads it raw ([`prop::Prop::RAW`]), so
/// [`reject_signal_in_structural_field`] still sees a signal to refuse and a [`prop::Handle`] keeps
/// the signal it names. Resolving then rejecting is unimplementable: once read, a signal's value is
/// indistinguishable from a literal. A panel's `layer`/`anchor`/`monitor`/`namespace` are structural
/// because `get_layer_surface` fixes them at creation, a popup's `parent` because `get_popup` pins
/// one (ADR-0051 decision 1); what a live request can change stays bound (ADR-0044 decision 1).
pub(crate) fn is_structural_property(kind: &str, property: &str) -> bool {
    crate::lua::nodes::accepted(kind, property).is_some_and(|row| row.raw)
}

/// One node's raw property map with every `Signal` replaced by its current value (ADR-0044 decision
/// 1). Called once per node per pass, as that node enters reconciliation; everything downstream
/// (this module's parsers, `layout::scene`'s sizing/positioning passes, `ResolvedNode::properties`)
/// reads the result, not the raw map. Once, and once is load-bearing: `Signal::get_value` runs a
/// `computed` signal's Lua closure, and a closure that is not a pure function of unchanged state
/// (`os.clock()`, `math.random`, an accumulator upvalue) answers differently on every call, so one
/// read per property makes the resolved tree a snapshot of one pass and stops ADR-0021's
/// per-`get_value` 5ms budget being paid four times over for one property. The snapshot covers the
/// *signals* only: a plain table with an `__index` metamethod is copied through as-is, and each
/// `table.get` a parser makes still runs it again; see [`EdgeInsets`]'s read. Nor is
/// this ADR-0044 decision 3's rejected memoization, which caches *across* pushes and needs an
/// invalidation rule no push has. Per entry: a key [`is_structural_property`] names for this node's
/// `kind` is copied through raw, signal and all. A `Value::UserData` holding a `Signal` is read
/// through `Signal::get_value` and the *result* stored in its place; a result that is itself a
/// `Signal` is an error naming the property, not a second read, avoiding an unbounded loop on a
/// cyclic construction. A result of `Value::Nil` **omits the key entirely**: ADR-0044 decision 1's
/// amendment ("a signal resolving to nil means the property is absent") falls out of the map rather
/// than being re-checked in every parser. Matters at boot: `RendererClient::run_startup_evaluation`
/// runs before the poll loop drains any inbound frame, so every `shared::Capability::ALL` signal
/// still reads `nil` at the first `Scene::apply`, and a bare capability binding must not fail
/// layout there; also consistent with a Lua table's own inability to store `nil`, so `visible =
/// nil` reads the same. Everything else, including a `UserData` that is not a `Signal`, is copied
/// through unchanged, for whichever parser reads it. Every property resolves, including ones no
/// parser reads today: the resolved map is what the paint stage reads a colour or radius straight
/// off (`ResolvedNode::properties`), and no property is out of a `Signal`'s reach, so there
/// is no subset safe to skip. A getter that raises fails the whole apply, even for a property
/// nothing downstream looked at: deferring would mean keeping the getter around to re-run later,
/// the second read this function prevents.
///
/// Evaluates every property holding a [`crate::lua::signal::Signal`] once per pass. Non-signal
/// properties remain untouched in the map. When no signals are present, resolution completes
/// in place with no allocations or sorting.
pub fn resolve_properties(mut properties: PropMap, kind: &str, lua: &Lua) -> Result<PropMap, LayoutError> {
    if properties.values().any(|v| matches!(v, Value::UserData(_))) {
        // Sorted, and the sort is the point: unsorted, two failing properties on one node name
        // whichever bucket the hasher put first. `renderer/src/socket/client/resolve.rs` puts this message in the
        // `rescue` global's `error_log` for a human to read (ADR-0024), so which one a broken config
        // names must come from the config. `two_failing_properties_always_report_the_same_one` guards it.
        let mut keys: Vec<&'static str> = properties.keys().copied().collect();
        keys.sort_unstable();
        for property in keys {
            if is_structural_property(kind, property) {
                // The writer of a `geometry` rect is not its reader: a moved rect re-resolves only
                // the nodes that read it.
                if property != "geometry"
                    && let Some(cell) = signal_at(&properties, property).and_then(|s| s.cell_id())
                {
                    signal::note_read(lua, cell);
                }
                continue;
            }
            let Some(signal) = signal_at(&properties, property) else {
                continue;
            };
            // Name the node kind: a config has many `background`s, and the bare property left a reader
            // grepping every one of them. `Scene::apply_admitting` adds the surface.
            let value = signal
                .get_value(lua)
                .map_err(|e| invalid(property, format!("Signal getter on a `{kind}` node failed: {e}")))?;
            match value {
                Value::UserData(_) => {
                    return Err(invalid(
                        property,
                        "a Signal resolved to another Signal -- resolution happens exactly once, not to a fixed point",
                    ));
                }
                Value::Nil => {
                    properties.remove(property);
                }
                value => {
                    properties.insert(property, value);
                }
            }
        }
    }
    // Callbacks and the two input flags have no parser: the input handlers read them where they
    // fire, where a wrong type could only be ignored. Sorted like the loop above.
    let mut typed: Vec<&'static str> = properties
        .keys()
        .copied()
        .filter(|property| property.starts_with("on_") || matches!(*property, "submit" | "autofocus"))
        .collect();
    typed.sort_unstable();
    for property in typed {
        let value = &properties[property];
        let (ok, expected) = if property.starts_with("on_") {
            (matches!(value, Value::Function(_)), "a function")
        } else {
            (matches!(value, Value::Boolean(_)), "a boolean")
        };
        if !ok {
            return Err(invalid(property, format!("expected {expected}, got {}", preview_for_error(value))));
        }
    }
    // `on_hover` fires on the crossing its node's own `hover` signal reports, so that signal is
    // where the "was it hovered last pass" memory lives and there is no second one (ADR-0095).
    // Without a slot the callback is unreachable, and this is the kind of silence
    // `deserialize_lua_table`'s unknown-key rejection exists to end: a config that declared it
    // would watch a handler never fire with nothing anywhere saying why.
    if properties.contains_key("on_hover") && !properties.contains_key("hover") {
        return Err(invalid(
            "on_hover",
            "declared without a `hover` slot on the same node -- add `hover = hover(\"a-name\")`, which is what remembers whether this node was hovered last pass, and so what tells its crossings from another node's",
        ));
    }
    Ok(properties)
}

/// The carve-outs from decision 1's "parsers resolve a `Signal`" rule, the rows typed
/// [`prop::Structural`]: [`SurfaceTopology`]'s five fields, a popup's `parent`, and every node's
/// optional `id` (ADR-0045 decision 1) keep rejecting one outright.
/// The unifying reason: each is read exactly once per evaluation and a *structural* decision
/// (where a surface is placed, or which retained node a fresh one is) is then made and acted on. A
/// `Signal` is free to change between passes, so admitting one here would leave that decision
/// resting on a value that no longer holds. Every other property is read for the geometry or
/// appearance of the pass it was read in, so a later change simply produces different output next
/// pass. Concretely: `surface_topology` runs on every `Scene::apply` so `renderer/src/socket/client/resolve.rs`'s
/// `pending_surfaces` can diff it against `applied_topology` and choose what to rebuild
/// (ADR-0216); a surface could otherwise move layer or monitor with no rebuild. `id` is
/// `pair_children_by_id_then_position`'s reconcile identity, matched once per `Scene::apply` to
/// pair a fresh child against its retained counterpart; a later-changing value would make "the
/// same node as last time" ambiguous. ADR-0044 decision 1 leaves both out: a gap, not a rejected
/// case. This only works because [`resolve_properties`] passes the keys
/// [`is_structural_property`] names through raw: these fields alone read the un-resolved value,
/// since a resolved signal is indistinguishable from a literal by the time it reaches a map.
fn reject_signal_in_structural_field(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

/// A test's property map, built the way production builds one: through the deserializer, which is
/// what matches a key to the `&'static str` a [`PropMap`] holds.
#[cfg(test)]
pub(crate) fn props_from_table(table: &mlua::Table) -> PropMap {
    crate::lua::nodes::deserialize_lua_table(table).unwrap().properties
}

/// [`props_from_table`] for a table with no `kind`. `rect` accepts every property these parsers
/// read.
#[cfg(test)]
pub(crate) fn rect_props(lua: &mlua::Lua, src: &str) -> PropMap {
    let table: mlua::Table = lua.load(src).eval().unwrap();
    table.set("kind", "rect").unwrap();
    props_from_table(&table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Fit;
    use crate::lua::nodes::deserialize_lua_table;

    #[test]
    fn a_signal_resolving_to_another_signal_is_an_error() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let inner = crate::lua::signal::Signal::new_live(Value::Integer(5), crate::lua::signal::DirtyFlag::new()).0;
        let inner_userdata = lua.create_userdata(inner).unwrap();
        let outer =
            crate::lua::signal::Signal::new_live(Value::UserData(inner_userdata), crate::lua::signal::DirtyFlag::new())
                .0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", outer).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(
            resolve_properties(node.properties, "text", &lua).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    /// A *resolved* property bag whose `property` slot held a live signal currently reading `nil`
    /// -- exactly the state every rostered capability's global is in before its first
    /// `StateSnapshot` (`renderer/src/socket/client/mod.rs`'s `RendererClient::new` seeds all of
    /// `shared::Capability::ALL` at `Value::Nil`), which is what a config binding a bare capability
    /// signal resolves at startup. Routed through [`resolve_properties`] because that is where the
    /// nil rule now lives: the key is omitted from the resolved map rather than each parser
    /// checking for a `Value::Nil` of its own.
    fn props_with_nil_signal(lua: &mlua::Lua, kind: &str, property: &str) -> PropMap {
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Nil, crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", kind).unwrap();
        table.set(property, signal).unwrap();
        resolve_properties(props_from_table(&table), kind, lua).unwrap()
    }

    #[test]
    fn a_signal_resolving_to_nil_takes_each_parsers_absent_property_default() {
        let lua = mlua::Lua::new();
        assert!(
            !props_with_nil_signal(&lua, "rect", "width").contains_key("width"),
            "the rule is one omitted key, not a Nil each parser re-checks"
        );
        assert_eq!(
            fields::common::width.read(&props_with_nil_signal(&lua, "rect", "width")).unwrap(),
            SizeMode::Content
        );
        assert_eq!(
            fields::common::padding.read(&props_with_nil_signal(&lua, "rect", "padding")).unwrap(),
            EdgeInsets::default()
        );
        assert_eq!(
            fields::common::align_h.read(&props_with_nil_signal(&lua, "rect", "align_h")).unwrap(),
            Align::Start
        );
        assert!(fields::common::visible.read(&props_with_nil_signal(&lua, "rect", "visible")).unwrap());
        assert_eq!(fields::flow::spacing.read(&props_with_nil_signal(&lua, "row", "spacing")).unwrap(), 0.0);
        assert_eq!(fields::text::font_size.read(&props_with_nil_signal(&lua, "text", "font_size")).unwrap(), 12.0);
        assert!(fields::root::child.read(&props_with_nil_signal(&lua, "panel", "child")).unwrap().is_none());
        assert!(fields::stack::children.read(&props_with_nil_signal(&lua, "row", "children")).unwrap().is_empty());
        assert_eq!(fields::text::content.read(&props_with_nil_signal(&lua, "text", "content")).unwrap().0, "");
        assert_eq!(fields::icon::size.read(&props_with_nil_signal(&lua, "icon", "size")).unwrap(), 12.0);
        assert_eq!(fields::icon::name.read(&props_with_nil_signal(&lua, "icon", "name")).unwrap(), "");
        assert_eq!(fields::image::source.read(&props_with_nil_signal(&lua, "image", "source")).unwrap(), "");
        assert_eq!(fields::image::fit.read(&props_with_nil_signal(&lua, "image", "fit")).unwrap(), Fit::Cover);
    }

    #[test]
    fn resolve_properties_copies_a_structural_field_through_raw_so_it_can_still_be_rejected() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(node.properties, "rect", &lua).unwrap();

        assert!(matches!(resolved.get("id"), Some(Value::UserData(_))), "id must survive the resolve step unresolved");
        assert!(
            matches!(fields::common::id.read(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id")
        );
    }

    /// The pairing rule (ADR-0095). `on_hover` fires on the crossing its node's `hover` signal
    /// reports, so without a slot the callback is unreachable -- refused, rather than left to be a
    /// handler a config watches never fire.
    #[test]
    fn on_hover_without_a_hover_slot_is_refused() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("on_hover", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        assert!(matches!(resolve_properties(node.properties, "rect", &lua).unwrap_err(),
                LayoutError::InvalidProperty { property, .. } if property == "on_hover"));
    }

    #[test]
    fn a_wrong_typed_callback_or_input_flag_is_refused_by_name() {
        let lua = mlua::Lua::new();
        for (source, property, expected) in [
            (r#"{ kind = "button", on_click = "quit" }"#, "on_click", "expected a function, got String(\"quit\")"),
            (r#"{ kind = "button", submit = 1 }"#, "submit", "expected a boolean, got Integer(1)"),
            (r#"{ kind = "textfield", autofocus = "yes" }"#, "autofocus", "expected a boolean, got String(\"yes\")"),
        ] {
            let table: mlua::Table = lua.load(format!("return {source}")).eval().unwrap();
            let err = resolve_properties(props_from_table(&table), "button", &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, detail } if p == property && detail == expected),
                "{source}: {err}"
            );
        }
    }

    #[test]
    fn on_hover_alongside_a_hover_slot_resolves() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let (over, _rect) = crate::lua::signal::Signal::new_hover(crate::lua::signal::DirtyFlag::new(), Value::Nil);
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("hover", over).unwrap();
        table.set("on_hover", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_properties(node.properties, "rect", &lua).unwrap();
        assert!(matches!(resolved.get("on_hover"), Some(Value::Function(_))), "a Function is not a Signal to resolve");
        assert!(matches!(resolved.get("hover"), Some(Value::UserData(_))), "the slot stays the handle it was");
    }

    /// The sort in [`resolve_properties`] (ADR-0024). The fixture is `opacity` and `background`
    /// because the map reaches them in that order, so dropping the sort fails this; the first
    /// assertion is what keeps a hasher change from quietly making the second one vacuous.
    #[test]
    fn two_failing_properties_always_report_the_same_one() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                    return {
                        kind = "rect",
                        background = computed({}, function() error("background boom") end),
                        opacity = computed({}, function() error("opacity boom") end),
                    }
                    "#,
            )
            .eval()
            .unwrap();
        let props = props_from_table(&table);

        let unsorted: Vec<&str> = props.keys().copied().collect();
        assert_eq!(unsorted, ["opacity", "background"], "the fixture must not already be in sorted order");

        let err = resolve_properties(props, "rect", &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "background"),
            "a broken config must name the property its own text names first, got: {err}"
        );
    }

    #[test]
    fn oversized_string_property_error_message_is_bounded() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", radius = string.rep("Q", 20 * 1024 * 1024) }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { property, detail } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(property, "radius");
        assert!(
            detail.len() < 1024,
            "a 20 MB input must not produce a multi-megabyte error message, got {} bytes",
            detail.len()
        );
    }

    #[test]
    fn oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", radius = string.rep("Q", 20 * 1024 * 1024) }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert!(detail.contains("expected a number"), "{detail}");
        assert!(detail.contains("String("), "must still name the rejected type: {detail}");
        assert!(detail.contains("QQQ"), "must show a recognizable prefix of the value: {detail}");
        assert!(
            detail.contains(&(20 * 1024 * 1024).to_string()),
            "must state the real length, or a truncated preview reads as the whole value: {detail}"
        );
    }

    #[test]
    fn short_string_property_error_message_is_unchanged() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = "banana" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(detail, "expected a number, got String(\"banana\")");
    }

    #[test]
    fn non_string_variant_error_message_is_unchanged() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(detail, "expected a number, got Boolean(true)");
    }
}

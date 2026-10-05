//! Typed, validated properties for `VirtualNode`. `resolve_declared` reads each ordinary `Signal`
//! at most once per node per pass (ADR-0044 decision 1); `SurfaceTopology`'s five fields and every
//! node's optional `id` stay raw and reject signals. A `panel`'s other properties are live fields, not
//! exceptions. Plain tables remain metamethod-backed, so each `table.get` can still run `__index`;
//! see `NumberOrEdges`'s read. A signal resolving to another signal errors rather than
//! reading again, while `MAX_TREE_DEPTH` bounds recursive tree construction.

mod animate;
mod content;
pub(crate) mod corner;
pub(crate) mod input;
mod paint_style;
mod vector_path;
#[cfg(test)]
pub(crate) use vector_path::PathCommand;
pub(crate) use vector_path::{PathCommands, PathData, PathOp, StrokeCap, StrokeJoin, TrimAxis, VectorPath, tweened};
pub(crate) mod prop;
mod spec;
mod style;
mod surface;
mod toplevel;

// Paint-only value types are imported, not re-exported; `paint_style` is their sole reader (ADR-0068).
use style::parse_radius;

pub use animate::Animatable;
#[cfg(test)]
pub(crate) use animate::KeyframeInput;
pub(crate) use animate::MoveTween;
#[cfg(test)]
pub(crate) use animate::TransitionInput;
#[cfg(test)]
pub(crate) use animate::{AnimationSpec, Easing, ExitBlock, SpringConstants, animatable_name, easing_names};
pub(crate) use animate::{Animations, Params};
pub use animate::{
    Dissolve, MoveSpec, ShaderParam, TransitionSpec, Tween, advance, depart, is_paint_only, retarget,
    retarget_measured, retarget_scroll, scroll_spec, scroll_target,
};
#[cfg(test)]
pub(crate) use content::CaretKeys;
pub(crate) use content::{Caret, Content, Font, FontVariations, Live, MaxLines, Region};
pub use content::{Elide, StyleRun, TextAlign, Wrap, font_runs};
#[cfg(test)]
pub(crate) use content::{SpanKind, TextRun};
pub use paint_style::{CaptureTarget, CaretStyle, PaintStyle, Typeface, paint_style};
pub(crate) use spec::{Children, Items, Limit, Root};
pub use spec::{ItemPass, ListMemo, SecureSubmitTarget, SurfaceSpec, list_children, lock_spec};
#[cfg(test)]
pub(crate) use style::{BackdropKeys, GradientStop, ShaderKeys, ShadowLayer};
// `wayland::tests`' and `instance::tests`' fixtures name it `node::LockSpec`; nothing else does.
#[cfg(test)]
pub use spec::LockSpec;
pub use style::{
    Affine, BorderColor, BorderPaint, ClipShape, Effect, Fill, Gradient, GradientShape, IDENTITY_AFFINE, Mask,
    MaskSource, Shadow, Tone, Transform, apply_affine, compose_affine, invert_affine, parse_effect, parse_transform,
    transformed_bounds,
};
pub(crate) use style::{
    Axes, ColorOrEdges, CornerShape, Cursor, Direction, EffectKeys, Effects, NumberOrCorners, NumberOrEdges, Scale,
    ShadowMode, Shadows,
};
pub use surface::{Anchor, Exclusive, KeyboardInteractivity, LayerKind, PanelSpec, SurfaceTopology, panel_spec};
#[cfg(test)]
pub(crate) use toplevel::Adjustment;
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
    pub(crate) struct EdgesInput {
        top: Option<f32>,
        right: Option<f32>,
        bottom: Option<f32>,
        left: Option<f32>,
    }
}

impl EdgesInput {
    fn into_edges(self) -> EdgeInsets {
        EdgeInsets {
            top: self.top.unwrap_or(0.0),
            right: self.right.unwrap_or(0.0),
            bottom: self.bottom.unwrap_or(0.0),
            left: self.left.unwrap_or(0.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EdgeInsets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

spelled!(EdgeInsets => EdgesInput::lua());

crate::lua::luacats::lua_shape! {
    /// Per-corner radius px; a missing corner is `0`.
    #[alias = "Corners"]
    pub(crate) struct CornersInput {
        top_left: Option<f32>,
        top_right: Option<f32>,
        bottom_right: Option<f32>,
        bottom_left: Option<f32>,
    }
}

/// Corner radii clockwise from the top left, in px, and the `corner_smoothing` they share. Negative
/// is a scoop (`corner_shape`), on every corner at once; zero is square. Smoothing is never
/// combined with a scoop (`parse_radius`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Radii(pub [f32; 4], pub f32);

impl Radii {
    pub fn is_zero(self) -> bool {
        self.0.iter().all(|r| *r == 0.0)
    }

    pub fn scoop(self) -> bool {
        self.0.iter().any(|r| *r < 0.0)
    }

    /// CSS's rule for radii too big for a `w` by `h` box: all shrink by one factor until the two
    /// on each side fit, so corners keep their proportions.
    pub fn fit(self, w: f32, h: f32) -> Self {
        let [tl, tr, br, bl] = self.0.map(f32::abs);
        let sides = [(w, tl + tr), (h, tr + br), (w, br + bl), (h, bl + tl)];
        let k = sides.iter().filter(|(_, sum)| *sum > 0.0).fold(1.0, |k, (side, sum)| f32::min(k, side / sum));
        // A side under zero (the scoop's `w - 2 * HAIR`) would flip every arc.
        self * k.max(0.0)
    }

    /// The smoothed outline of each corner of a `w` by `h` box, `None` where it is square or the
    /// smoothing is `0`, which the circular paths draw as they always have. Each
    /// corner's budget is its share of the shorter side it meets, as figma-squircle splits a side
    /// between two corners; call it on [`fit`](Self::fit)ted radii.
    pub fn squircles(self, w: f32, h: f32) -> [Option<corner::Squircle>; 4] {
        let [tl, tr, br, bl] = self.0;
        let share = |r: f32, next: f32, side: f32| if r + next > 0.0 { r / (r + next) * side } else { side };
        let budget = |r: f32, across: f32, down: f32| share(r, across, w).min(share(r, down, h));
        [(tl, budget(tl, tr, bl)), (tr, budget(tr, tl, br)), (br, budget(br, bl, tr)), (bl, budget(bl, br, tl))]
            .map(|(r, budget)| (r > 0.0 && self.1 > 0.0).then(|| corner::Squircle::new(r, self.1, budget)))
    }
}

impl std::ops::Mul<f32> for Radii {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self(self.0.map(|r| r * k), self.1)
    }
}

impl From<f32> for Radii {
    fn from(r: f32) -> Self {
        Self([r; 4], 0.0)
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

impl From<Rgba> for femtovg::Color {
    fn from(Rgba { r, g, b, a }: Rgba) -> Self {
        Self::rgbaf(r, g, b, a)
    }
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
    /// Every node a failed pass refused, so one run names them all rather than the first. Flat:
    /// each entry is one node's error, never another `Several`.
    #[error("{}", several(.0))]
    Several(Vec<LayoutError>),
}

/// Most failures one report lists. A config broken past this gets the count, and twenty are
/// enough to start on.
const SHOWN_FAILURES: usize = 20;

fn several(errors: &[LayoutError]) -> String {
    let mut out = format!("{} nodes failed:", errors.len());
    for err in errors.iter().take(SHOWN_FAILURES) {
        out.push_str(&format!("\n  {err}"));
    }
    if errors.len() > SHOWN_FAILURES {
        out.push_str(&format!("\n  and {} more", errors.len() - SHOWN_FAILURES));
    }
    out
}

impl LayoutError {
    /// Add diagnostic paths only after a reader fails, keeping allocation off the success path.
    pub(crate) fn under(self, prefix: &str) -> Self {
        match self {
            Self::InvalidProperty { property, detail } => {
                Self::InvalidProperty { property: format!("{prefix}{property}"), detail }
            }
            Self::UnsupportedSignalProperty(path) => Self::UnsupportedSignalProperty(format!("{prefix}{path}")),
            other => other,
        }
    }

    /// One error as itself, several as [`Self::Several`], each message once. `errors` is flat and
    /// never empty.
    pub(crate) fn many(errors: Vec<Self>) -> Self {
        let mut seen = std::collections::HashSet::new();
        let mut errors: Vec<Self> = errors.into_iter().filter(|err| seen.insert(err.to_string())).collect();
        if errors.len() == 1 { errors.remove(0) } else { Self::Several(errors) }
    }

    /// The single errors inside, for a caller adding a path segment or a surface to each.
    pub(crate) fn into_each(self) -> Vec<Self> {
        match self {
            Self::Several(errors) => errors,
            other => vec![other],
        }
    }

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
    ///
    /// `site` is the line that built the child: indices say where in the tree, not which line of
    /// which file, and a node a helper function returns has no index in the file at all.
    pub(crate) fn in_child(self, index: usize, kind: &str, site: Option<crate::lua::location::Site>) -> Self {
        let Self::InvalidProperty { property, detail } = self else {
            return self;
        };
        let detail = match site {
            Some(site) => format!("{kind}[{index}] ({site}) > {detail}"),
            None => format!("{kind}[{index}] > {detail}"),
        };
        Self::InvalidProperty { property, detail }
    }
}

/// [`marshal::only_keys`] for a property's sub-table, naming the property.
pub(crate) fn only_keys(property: &str, table: &mlua::Table, keys: &[&str]) -> Result<(), LayoutError> {
    marshal::only_keys(table, keys).map_err(|detail| invalid(property, detail))
}

/// Whether `a` and `b` are the same Lua object, by address, which is what a retained declaration
/// or list input has to be to skip a rebuild. Not the signals' comparison of contents.
pub(crate) fn same_lua_value(a: &Value, b: &Value) -> bool {
    a.type_name() == b.type_name() && a.to_pointer() == b.to_pointer()
}

/// The `Signal` held unresolved in `property`, or `None` for any other value. A structural slot
/// (`hover`, `scroll`, `geometry`, `elided`) holding something else is inert rather than an error: the
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
/// literal and `resolve_declared`-resolved numbers, including integers outside `2^53`. A finite
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

/// Whether `property` is one [`resolve_declared`] copies through untouched on a node of this
/// `kind`: its field's type reads it raw ([`prop::Prop::RAW`]), so
/// [`reject_signal_in_structural_field`] still sees a signal to refuse and a [`prop::Handle`] keeps
/// the signal it names. Resolving then rejecting is unimplementable: once read, a signal's value is
/// indistinguishable from a literal. A panel's `layer`/`anchor`/`output`/`namespace` are structural
/// because `get_layer_surface` fixes them at creation, a popup's `parent` because `get_popup` pins
/// one (ADR-0051 decision 1); what a live request can change stays bound (ADR-0044 decision 1).
pub(crate) fn is_structural_property(kind: &str, property: &str) -> bool {
    crate::lua::nodes::accepted(kind, property).is_some_and(|row| row.raw)
}

/// One node's raw property map with every `Signal` replaced by its current value (ADR-0044 decision
/// 1). Called at most once per node per pass, as that node enters reconciliation, and not at all
/// for a node whose last resolve still holds (ADR-0270); everything downstream
/// (this module's parsers, `layout::scene`'s sizing/positioning passes, `ResolvedNode::properties`)
/// reads the result, not the raw map. Once, and once is load-bearing: `Signal::get_value` runs a
/// `computed` signal's Lua closure, and a closure that is not a pure function of unchanged state
/// (`os.clock()`, `math.random`, an accumulator upvalue) answers differently on every call, so one
/// read per property makes the resolved tree a snapshot of one pass and stops ADR-0021's
/// per-`get_value` 2.5ms budget being paid four times over for one property. The snapshot covers the
/// *signals* only: a plain table with an `__index` metamethod is copied through as-is, and each
/// `table.get` a parser makes still runs it again; see `NumberOrEdges`'s read. Per entry: a key [`is_structural_property`] names for this node's
/// `kind` is copied through raw, signal and all. A table value is walked by [`resolve_nested`],
/// which copies the tables that hold a signal. A `Value::UserData` holding a `Signal` is read
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
/// Evaluates every property holding a [`crate::lua::signal::Signal`]. Non-signal
/// properties remain untouched in the map. When no signal is present, at the top level or in a
/// table, resolution completes in place with no allocations or sorting. `tables_plain` (see
/// [`tables_plain`]) vouches that the walked tables hold no signal, so they are not scanned again.
pub(crate) fn resolve_declared(
    mut properties: PropMap,
    kind: &str,
    tables_plain: bool,
    lua: &Lua,
) -> Result<PropMap, LayoutError> {
    if holds_signals(&properties, tables_plain) {
        // Sorted, and the sort is the point: unsorted, two failing properties on one node name
        // whichever bucket the hasher put first. `renderer/src/socket/client/resolve.rs` puts this message in the
        // `rescue` global's `error_log` for a human to read (ADR-0024), so which one a broken config
        // names must come from the config. `two_failing_properties_always_report_the_same_one` guards it.
        let mut keys: Vec<&'static str> = properties.keys().copied().collect();
        keys.sort_unstable();
        for property in keys {
            if is_structural_property(kind, property) {
                // The writer of a layout measurement is not its reader: a change re-resolves only
                // the nodes that read it. A `scroll` slot's holder reads nothing of it either; the
                // scene notes it outside the node's own reads (`scene::resolve`).
                if !matches!(property, "geometry" | "scroll" | "elided")
                    && let Some(cell) = signal_at(&properties, property).and_then(|s| s.cell_id())
                {
                    signal::note_read(lua, cell);
                }
                continue;
            }
            // A seed is read once at creation, so a write to its signal must not re-resolve the node.
            let read = || resolved_value(&properties, kind, property, tables_plain, lua);
            match if property == "initial_text" { signal::untracked(lua, read) } else { read() }? {
                Some(Value::Nil) => properties.remove(property),
                Some(value) => properties.insert(property, value),
                None => continue,
            };
        }
    }
    // `effect` is every kind's row but `backdrop` is a box's, and a key's kind is no row's.
    if let Some(Value::Table(effect)) = properties.get("effect")
        && effect.contains_key("backdrop").unwrap_or(false)
        && crate::lua::nodes::properties::kind_bit(kind)
            .is_some_and(|bit| bit & crate::lua::nodes::properties::BOX == 0)
    {
        return Err(invalid("effect.backdrop", format!("`{kind}` has no backdrop; it is for box kinds only")));
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

/// Settle derived property outputs before a retained node checks whether its reads changed.
pub(crate) fn settle_property_signals(
    properties: &PropMap,
    kind: &str,
    tables_plain: bool,
    lua: &Lua,
) -> Result<(), LayoutError> {
    if !holds_signals(properties, tables_plain) {
        return Ok(());
    }
    let mut keys: Vec<_> = properties.keys().copied().collect();
    keys.sort_unstable();
    for property in keys {
        if !is_structural_property(kind, property) {
            resolved_value(properties, kind, property, tables_plain, lua)?;
        }
    }
    Ok(())
}

/// ponytail: 8 tables, past the deepest shape a parser reads (5); deeper, `input::plain` refuses. Upgrade: per-row depth.
const NESTED_SIGNAL_DEPTH: usize = 8;

/// Tables a scan visits before leaving the rest to [`resolve_nested`], whose `seen` map bounds a
/// table reached many ways.
const SCAN_TABLES: usize = 256;

/// The enclosing tables of a walk, so a back-reference keeps the original table.
type TablePath = [*const std::ffi::c_void; NESTED_SIGNAL_DEPTH];

/// Whether a walk leaves `table` (at address `at`) alone: past the depth limit, under a metatable,
/// or already on the `path` of enclosing tables.
fn walk_skips(table: &mlua::Table, at: *const std::ffi::c_void, path: &TablePath, depth: usize) -> bool {
    depth == NESTED_SIGNAL_DEPTH || table.metatable().is_some() || path[..depth].contains(&at)
}

/// Whether `table` under `property` is walked for signals: `child`/`children` hold node tables,
/// each resolved as its own node, and the child-table cache keys them by address.
fn walked_table<'a>(property: &str, value: &'a Value) -> Option<&'a mlua::Table> {
    match value {
        Value::Table(table) if !matches!(property, "child" | "children") => Some(table),
        _ => None,
    }
}

/// Whether no walked table of `properties` may hold a signal. The resolve memo keeps the answer
/// while its declaration holds; a signal put into a table afterwards is not seen, and the parser
/// refuses it.
pub(crate) fn tables_plain(properties: &PropMap) -> bool {
    !properties.iter().any(|(property, value)| walked_table(property, value).is_some_and(may_hold_signal))
}

/// Whether any property holds a userdata, or (unless `tables_plain`) a walked table may hold a signal.
fn holds_signals(properties: &PropMap, tables_plain: bool) -> bool {
    properties.iter().any(|(property, value)| {
        matches!(value, Value::UserData(_))
            || !tables_plain && walked_table(property, value).is_some_and(may_hold_signal)
    })
}

/// Whether a walk of `table` may find a signal, scanned without allocating; a scan past
/// [`SCAN_TABLES`] says yes and leaves the answer to the walk.
/// ponytail: mlua converts each scanned value (~70 ns); upgrade: a raw lua_next scan.
fn may_hold_signal(table: &mlua::Table) -> bool {
    fn scan(table: &mlua::Table, path: &mut TablePath, depth: usize, budget: &mut usize) -> bool {
        let at = table.to_pointer();
        if walk_skips(table, at, path, depth) {
            return false;
        }
        let Some(left) = budget.checked_sub(1) else { return true };
        *budget = left;
        path[depth] = at;
        table.pairs::<Value, Value>().any(|pair| match pair {
            Ok((_, Value::UserData(ud))) => signal::is_signal(&ud),
            Ok((_, Value::Table(inner))) => scan(&inner, path, depth + 1, budget),
            Ok(_) => false,
            Err(_) => true,
        })
    }
    let mut budget = SCAN_TABLES;
    scan(table, &mut [std::ptr::null(); NESTED_SIGNAL_DEPTH], 0, &mut budget)
}

/// `property`'s value with its signals read, top level or nested; `Some(Nil)` makes it absent and
/// `None` means it holds no signal.
fn resolved_value(
    properties: &PropMap,
    kind: &str,
    property: &str,
    tables_plain: bool,
    lua: &Lua,
) -> Result<Option<Value>, LayoutError> {
    let value = &properties[property];
    if let Some(table) = walked_table(property, value).filter(|table| !tables_plain && may_hold_signal(table)) {
        let mut path = [std::ptr::null(); NESTED_SIGNAL_DEPTH];
        let walked = resolve_nested(table, kind, 0, &mut path, &mut Default::default(), lua);
        return Ok(walked.map_err(|e| e.under(property))?.map(Value::Table));
    }
    match value {
        Value::UserData(ud) => read_signal(ud, kind, property, lua),
        _ => Ok(None),
    }
}

/// `ud`'s value when it is a signal, `Nil` when that reads nil; `None` for other userdata.
fn read_signal(ud: &mlua::AnyUserData, kind: &str, property: &str, lua: &Lua) -> Result<Option<Value>, LayoutError> {
    let Some(signal) = signal::from_userdata(ud) else { return Ok(None) };
    Ok(Some(resolve_signal(&signal, kind, property, lua)?.unwrap_or(Value::Nil)))
}

/// A copy of `table` with each signal inside it read, or `None` when it holds none, so a
/// signal-free table is shared, not copied. A signal's result is never walked: resolution stays
/// exactly once. Raw reads; a table with a metatable is left to its parser, which refuses a signal.
/// `seen` is keyed by depth too, since a walk cut at the depth limit depends on where it began.
fn resolve_nested(
    table: &mlua::Table,
    kind: &str,
    depth: usize,
    path: &mut TablePath,
    seen: &mut rustc_hash::FxHashMap<(*const std::ffi::c_void, usize), Option<mlua::Table>>,
    lua: &Lua,
) -> Result<Option<mlua::Table>, LayoutError> {
    let at = table.to_pointer();
    if walk_skips(table, at, path, depth) {
        return Ok(None);
    }
    if let Some(walked) = seen.get(&(at, depth)) {
        return Ok(walked.clone());
    }
    path[depth] = at;
    let lua_err = |e: mlua::Error| invalid("", e.to_string());
    let len = table.raw_len();
    let mut copy: Option<mlua::Table> = None;
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(lua_err)?;
        let read = match &value {
            Value::Table(inner) => resolve_nested(inner, kind, depth + 1, path, seen, lua).map(|t| t.map(Value::Table)),
            Value::UserData(ud) => read_signal(ud, kind, "", lua),
            _ => Ok(None),
        };
        let Some(fresh) = read.map_err(|e| e.under(&key_path(&key)))? else { continue };
        // Compacting would shift every later entry, a gradient stop's colour into its position.
        if fresh.is_nil() && matches!(key, Value::Integer(i) if i >= 1 && i as usize <= len) {
            return Err(invalid(&key_path(&key), "a signal in an array read nil, which would leave a hole"));
        }
        let target = match &copy {
            Some(target) => target.clone(),
            None => {
                let target = lua.create_table().map_err(lua_err)?;
                table.for_each(|key: Value, value: Value| target.raw_set(key, value)).map_err(lua_err)?;
                copy.insert(target).clone()
            }
        };
        target.raw_set(key, fresh).map_err(lua_err)?;
    }
    seen.insert((at, depth), copy.clone());
    Ok(copy)
}

/// `key` as the path segment the parsers name it by: `[2]` or `.top`.
fn key_path(key: &Value) -> String {
    match key {
        Value::Integer(i) => format!("[{i}]"),
        Value::String(s) => format!(".{}", s.display()),
        other => format!("[{}]", preview_for_error(other)),
    }
}

/// `signal`'s value for `property`, `None` for nil: the property is absent.
fn resolve_signal(
    signal: &signal::Signal,
    kind: &str,
    property: &str,
    lua: &Lua,
) -> Result<Option<Value>, LayoutError> {
    // Name the node kind: a config has many `background`s, and the bare property left a reader
    // grepping every one of them. `Scene::apply_admitting` adds the surface.
    let value = signal.get_value(lua).map_err(|e| {
        invalid(property, format!("Signal getter on a `{kind}` node failed: {}", crate::lua::describe(&e)))
    })?;
    match value {
        Value::UserData(_) => Err(invalid(
            property,
            "a Signal resolved to another Signal -- resolution happens exactly once, not to a fixed point",
        )),
        Value::Nil => Ok(None),
        value => Ok(Some(value)),
    }
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
/// (ADR-0216); a surface could otherwise move layer or output with no rebuild. `id` is
/// `pair_children_by_id_then_position`'s reconcile identity, matched once per `Scene::apply` to
/// pair a fresh child against its retained counterpart; a later-changing value would make "the
/// same node as last time" ambiguous. ADR-0044 decision 1 leaves both out: a gap, not a rejected
/// case. This only works because [`resolve_declared`] passes the keys
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

/// A VM with the signal globals (`state`, `computed`, ...) a property under test may bind.
#[cfg(test)]
pub(crate) fn signal_lua() -> mlua::Lua {
    let lua = mlua::Lua::new();
    crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
    lua
}

/// [`signal_lua`] plus the node constructors (`row { ... }`), for a tree built from source.
#[cfg(test)]
pub(crate) fn scene_lua() -> mlua::Lua {
    let lua = signal_lua();
    crate::lua::nodes::register_node_constructors(&lua).unwrap();
    lua
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Fit;
    use crate::lua::nodes::deserialize_lua_table;

    #[test]
    fn a_signal_resolving_to_another_signal_is_an_error() {
        let lua = signal_lua();
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
            resolve_declared(node.properties, "text", false, &lua).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    /// A *resolved* property bag whose `property` slot held a live signal currently reading `nil`
    /// -- exactly the state every rostered capability's global is in before its first
    /// `StateSnapshot` (`renderer/src/socket/client/mod.rs`'s `RendererClient::new` seeds all of
    /// `shared::Capability::ALL` at `Value::Nil`), which is what a config binding a bare capability
    /// signal resolves at startup. Routed through [`resolve_declared`] because that is where the
    /// nil rule now lives: the key is omitted from the resolved map rather than each parser
    /// checking for a `Value::Nil` of its own.
    fn props_with_nil_signal(lua: &mlua::Lua, kind: &str, property: &str) -> PropMap {
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Nil, crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", kind).unwrap();
        table.set(property, signal).unwrap();
        resolve_declared(props_from_table(&table), kind, false, lua).unwrap()
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
        assert_eq!(fields::typeface::font_size.read(&props_with_nil_signal(&lua, "text", "font_size")).unwrap(), 12.0);
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
        let lua = signal_lua();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_declared(node.properties, "rect", false, &lua).unwrap();

        assert!(matches!(resolved.get("id"), Some(Value::UserData(_))), "id must survive the resolve step unresolved");
        assert!(
            matches!(fields::common::id.read(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id")
        );
    }

    /// Raw rows, `children` and a list's `source` keep the declared table, signals and all.
    #[test]
    fn raw_rows_children_and_a_list_source_are_not_walked_for_signals() {
        let lua = scene_lua();
        for (kind, src, property) in [
            ("panel", r#"id = "bar", layer = "top", anchor = { top = state("a", true) }"#, "anchor"),
            ("column", "children = { rect { width = state('w', 1) } }", "children"),
            ("list", "source = { { a = state('s', 1) } }, itemfn = function() end", "source"),
        ] {
            let table: mlua::Table = lua.load(format!(r#"return {{ kind = "{kind}", {src} }}"#)).eval().unwrap();
            let declared = props_from_table(&table);
            let resolved = resolve_declared(declared.clone(), kind, false, &lua).unwrap();
            assert!(same_lua_value(&declared[property], &resolved[property]), "{property}");
        }
        let table: mlua::Table = lua
            .load(r#"return { kind = "panel", id = "bar", layer = "top", anchor = { top = state("a", true) } }"#)
            .eval()
            .unwrap();
        let resolved = resolve_declared(props_from_table(&table), "panel", false, &lua).unwrap();
        assert!(fields::panel::anchor.read(&resolved).unwrap_err().to_string().contains("`top`"));
    }

    /// A key's kind is no row's: `effect` is every kind's, `backdrop` in it a box's.
    #[test]
    fn effect_backdrop_is_refused_off_a_box_kind_naming_the_key() {
        let lua = signal_lua();
        let src = "return { effect = { blur = state('blur', 2), backdrop = { blur = state('b', 4) } } }";
        for kind in ["rect", "row", "panel"] {
            let resolved = resolve_declared(rect_props(&lua, src), kind, false, &lua).unwrap();
            let Value::Table(effect) = &resolved["effect"] else { panic!() };
            assert_eq!(effect.raw_get::<i64>("blur").unwrap(), 2, "a nested signal resolves");
        }
        for kind in ["text", "image", "list"] {
            let err = resolve_declared(rect_props(&lua, src), kind, false, &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "effect.backdrop"),
                "{kind}: {err:?}"
            );
        }
        let blur_only = rect_props(&lua, "return { effect = { blur = 2 } }");
        assert!(resolve_declared(blur_only, "text", false, &lua).is_ok(), "`blur` is every kind's");
    }

    /// A cycle or a table reached many ways is walked once; past the depth limit a signal stays.
    #[test]
    fn a_cyclic_shared_or_over_deep_table_ends_its_walk() {
        let lua = signal_lua();
        let props =
            rect_props(&lua, "local m = { top = state('top', 4) } for i = 1, 20 do m[i] = m end return { margin = m }");
        let Value::Table(margin) = &resolve_declared(props, "rect", false, &lua).unwrap()["margin"] else { panic!() };
        assert_eq!(margin.raw_get::<i64>("top").unwrap(), 4);
        // A back-reference keeps the original table, signal and all, for the parser to refuse.
        assert!(
            matches!(margin.raw_get::<Value>(1).unwrap(), Value::Table(m) if m.raw_get::<Value>("top").unwrap().is_userdata())
        );
        let shader: mlua::Table = lua
            .load(r#"local t = { x = state("x", 1) } for _ = 1, 9 do t = { t } end return { kind = "shader", params = t }"#)
            .eval()
            .unwrap();
        let mut table = resolve_declared(props_from_table(&shader), "shader", false, &lua).unwrap()["params"].clone();
        while let Value::Table(inner) = table {
            table = inner.raw_get(1).unwrap_or(Value::Nil);
            if table.is_nil() {
                table = inner.raw_get("x").unwrap();
                break;
            }
        }
        assert!(matches!(table, Value::UserData(_)), "a signal past the depth limit is left for the parser to refuse");
    }

    /// A table cut at the depth limit is walked whole where it is reached shallower.
    #[test]
    fn a_table_reached_deep_and_shallow_resolves_where_it_is_shallow() {
        let lua = signal_lua();
        let shader: mlua::Table = lua
            .load(
                r#"local t = { { x = state("x", 1) } }
                local deep = t for _ = 1, 6 do deep = { deep } end
                return { kind = "shader", params = { deep, t } }"#,
            )
            .eval()
            .unwrap();
        let resolved = resolve_declared(props_from_table(&shader), "shader", false, &lua).unwrap();
        let x: Value = lua
            .load("return function(p) return p[2][1].x end")
            .eval::<mlua::Function>()
            .unwrap()
            .call(resolved["params"].clone())
            .unwrap();
        assert_eq!(x, Value::Integer(1));
    }

    /// Compacting would shift later entries, so a hole is refused by its path.
    #[test]
    fn a_signal_reading_nil_inside_an_array_is_refused() {
        let lua = signal_lua();
        let props = rect_props(
            &lua,
            r##"return { background = { gradient = "linear", stops = { { 0, "#ffffff" }, { 1, state("hole") } } } }"##,
        );
        let err = resolve_declared(props, "rect", false, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "background.stops[2][2]"),
            "{err:?}"
        );
    }

    /// The pairing rule (ADR-0095). `on_hover` fires on the crossing its node's `hover` signal
    /// reports, so without a slot the callback is unreachable -- refused, rather than left to be a
    /// handler a config watches never fire.
    #[test]
    fn on_hover_without_a_hover_slot_is_refused() {
        let lua = signal_lua();
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("on_hover", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        assert!(matches!(resolve_declared(node.properties, "rect", false, &lua).unwrap_err(),
                LayoutError::InvalidProperty { property, .. } if property == "on_hover"));
    }

    #[test]
    fn a_wrong_typed_callback_or_input_flag_is_refused_by_name() {
        let lua = mlua::Lua::new();
        for (source, property, expected) in [
            (r#"{ kind = "row", on_click = "quit" }"#, "on_click", "expected a function, got String(\"quit\")"),
            (r#"{ kind = "icon", submit = 1 }"#, "submit", "expected a boolean, got Integer(1)"),
            (r#"{ kind = "textfield", autofocus = "yes" }"#, "autofocus", "expected a boolean, got String(\"yes\")"),
        ] {
            let table: mlua::Table = lua.load(format!("return {source}")).eval().unwrap();
            let err = resolve_declared(props_from_table(&table), "rect", false, &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, detail } if p == property && detail == expected),
                "{source}: {err}"
            );
        }
    }

    #[test]
    fn on_hover_alongside_a_hover_slot_resolves() {
        let lua = signal_lua();
        let (over, _rect) = crate::lua::signal::Signal::new_hover(crate::lua::signal::DirtyFlag::new(), Value::Nil);
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("hover", over).unwrap();
        table.set("on_hover", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        let resolved = resolve_declared(node.properties, "rect", false, &lua).unwrap();
        assert!(matches!(resolved.get("on_hover"), Some(Value::Function(_))), "a Function is not a Signal to resolve");
        assert!(matches!(resolved.get("hover"), Some(Value::UserData(_))), "the slot stays the handle it was");
    }

    /// The sort in [`resolve_declared`] (ADR-0024). The fixture is `opacity` and `background`
    /// because the map reaches them in that order, so dropping the sort fails this; the first
    /// assertion is what keeps a hasher change from quietly making the second one vacuous.
    #[test]
    fn two_failing_properties_always_report_the_same_one() {
        let lua = signal_lua();
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

        let err = resolve_declared(props, "rect", false, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "background"),
            "a broken config must name the property its own text names first, got: {err}"
        );
    }

    #[test]
    fn oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix() {
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
        assert_eq!(detail, "expected a number or a table, got String(\"banana\")");
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
        assert_eq!(detail, "expected a number or a table, got Boolean(true)");
    }
}

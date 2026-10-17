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
mod list;
pub(crate) mod melt;
mod paint_style;
pub(crate) mod shape;
mod vector_path;
pub use shape::Shape;
#[cfg(test)]
pub(crate) use vector_path::PathCommand;
pub(crate) use vector_path::{
    PathCommands, PathData, PathOp, Segment, StrokeCap, StrokeJoin, TrimAxis, VectorPath, tweened,
};
pub(crate) mod prop;
mod resolve;
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
pub(crate) use animate::{AnimationSpec, Easing, Motion};
pub(crate) use animate::{Animations, Images, Params, byte};
pub use animate::{
    Dissolve, MAX_SHADER_IMAGES, MoveSpec, ShaderImage, ShaderParam, TYPED, TransitionSpec, Tween, advance, commit,
    depart, is_paint_only, retarget, retarget_measured, retarget_scroll, scroll_spec, scroll_target, step, sync,
};
#[cfg(test)]
pub(crate) use animate::{ExitBlock, SpringConstants, animatable_name, easing_names};
pub(crate) use content::{Caret, Content, Font, FontVariations, Live, MaxLines, Region, Selection};
#[cfg(test)]
pub(crate) use content::{CaretKeys, SelectionKeys};
pub use content::{Elide, StyleRun, TextAlign, Wrap, font_runs};
#[cfg(test)]
pub(crate) use content::{SpanKind, TextRun};
pub use list::{ItemPass, ListMemo, View, list_children};
pub(crate) use list::{Items, Limit, least_move};
pub use paint_style::{CaptureTarget, CaretStyle, Multiline, PaintStyle, Typeface, paint_style};
use resolve::{reject_signal_in_structural_field, resolve_signal};
pub(crate) use resolve::{resolve_declared, settle_property_signals, tables_plain};
pub(crate) use spec::{Children, Root};
pub use spec::{SecureSubmitTarget, SurfaceSpec, lock_spec};
#[cfg(test)]
pub(crate) use style::{BackdropKeys, GradientStop, ShaderKeys, ShadowLayer};
// `wayland::tests`' and `instance::tests`' fixtures name it `node::LockSpec`; nothing else does.
#[cfg(test)]
pub use spec::LockSpec;
pub use style::{
    Affine, Blend, BorderColor, BorderPaint, ClipShape, Effect, EffectShader, Fill, Gradient, GradientShape,
    IDENTITY_AFFINE, Mask, MaskSource, Ring, Shadow, Tone, Transform, apply_affine, compose_affine, invert_affine,
    parse_effect, parse_transform, transformed_bounds,
};
pub(crate) use style::{
    Axes, Background, ColorOrEdges, CornerShape, Cursor, Direction, EffectKeys, Effects, NumberOrCorners,
    NumberOrEdges, Scale, ShadowMode, Shadows, is_layer_list, layer_fill,
};
pub use surface::{Anchor, Exclusive, KeyboardInteractivity, LayerKind, PanelSpec, SurfaceTopology, panel_spec};
#[cfg(test)]
pub(crate) use toplevel::Adjustment;
pub(crate) use toplevel::{AnchorRect, PopupExtent};
pub use toplevel::{
    ConstraintAdjustment, Decorations, PopupAnchor, PopupOffset, PopupSpec, SizeHint, WindowSpec, popup_spec,
    window_spec,
};

/// A node's property map, keyed by the `&'static str` the config's spelling was matched against
/// (ADR-0219). `FxHashMap` for thirty lookups a node against short literal keys, where SipHash's
/// setup costs more than the comparison (ADR-0218).
pub type PropMap = rustc_hash::FxHashMap<&'static str, Value>;

use mlua::{Lua, Value};

use crate::lua::location::Site;
use crate::lua::luacats::{LuaType, spelled};
use crate::lua::marshal;
pub(crate) use crate::lua::nodes::properties::{self as fields, Property};
use crate::lua::signal;
use crate::text::snap::LogicalRect;
use prop::Prop;
use std::rc::Rc;

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

/// A box's shape: corner radii clockwise from the top left, in px, and the `corner_smoothing` they
/// share, or a `shape` in their place. Negative is a scoop (`corner_shape`), on every corner at
/// once; zero is square. Smoothing is never combined with a scoop (`parse_radius`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Radii(pub [f32; 4], pub f32, pub Option<Rc<Shape>>);

impl Radii {
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|r| *r == 0.0) && self.2.is_none()
    }

    pub fn scoop(&self) -> bool {
        self.0.iter().any(|r| *r < 0.0)
    }

    /// Whether a box gradient draws this shape's shadow: not a scoop or a `shape` (ADR-0260).
    pub fn analytic(&self) -> bool {
        !self.scoop() && self.2.is_none()
    }

    /// `rect` grown to hold an outline reaching past it.
    pub fn bounds(&self, rect: LogicalRect) -> LogicalRect {
        self.2.as_ref().map_or(rect, |outline| outline.bounds(rect))
    }

    /// Whether the shape of a box at `rect` holds `(x, y)`.
    pub fn contains(&self, rect: LogicalRect, point: crate::layout::hit::LogicalPoint) -> bool {
        match &self.2 {
            Some(outline) => outline.contains(rect, kurbo::Point::new(f64::from(point.x), f64::from(point.y))),
            None => rect.contains(point),
        }
    }

    /// CSS's rule for radii too big for a `w` by `h` box: all shrink by one factor until the two
    /// on each side fit, so corners keep their proportions.
    pub fn fit(&self, w: f32, h: f32) -> Self {
        let [tl, tr, br, bl] = self.0.map(f32::abs);
        let sides = [(w, tl + tr), (h, tr + br), (w, br + bl), (h, bl + tl)];
        let k = sides.iter().filter(|(_, sum)| *sum > 0.0).fold(1.0, |k, (side, sum)| f32::min(k, side / sum));
        // A side under zero (the scoop's `w - 2 * HAIR`) would flip every arc.
        self.clone() * k.max(0.0)
    }

    /// The smoothed outline of each corner of a `w` by `h` box, `None` where it is square or the
    /// smoothing is `0`, which the circular paths draw as they always have. Each
    /// corner's budget is its share of the shorter side it meets, as figma-squircle splits a side
    /// between two corners; call it on [`fit`](Self::fit)ted radii.
    pub fn squircles(&self, w: f32, h: f32) -> [Option<corner::Squircle>; 4] {
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
        let outline = self.2.map(|outline| if k == 1.0 { outline } else { Rc::new(outline.scaled(k)) });
        Self(self.0.map(|r| r * k), self.1, outline)
    }
}

impl From<f32> for Radii {
    fn from(r: f32) -> Self {
        Self([r; 4], 0.0, None)
    }
}

impl EdgeInsets {
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }

    pub fn scaled(self, k: f32) -> Self {
        Self { top: self.top * k, right: self.right * k, bottom: self.bottom * k, left: self.left * k }
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
    #[error("{}", invalid_property(property, detail, at.as_deref()))]
    InvalidProperty { property: String, detail: String, at: Option<Box<Trail>> },
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

/// One node on the way down to a failure, `kind[index] (site)` in a path; no `index` for a lone `child`.
#[derive(Debug)]
struct Step {
    kind: String,
    index: Option<usize>,
    site: Option<Site>,
}

/// Where an `InvalidProperty` happened: the nodes walked from the failing one up to its root, then
/// the surface instance. Boxed so a property read's `Result` stays 56 bytes, not 96.
#[derive(Debug, Default)]
pub struct Trail {
    steps: Vec<Step>,
    surface: Option<String>,
}

/// `file:N: kind[i]: invalid value for \`p\`: problem (at root (site) > parent (site) on \`surface\`)`.
/// The failing node's line leads, as in Lua's own errors, so the path names only the nodes above it;
/// it closes the problem's first line, ahead of any traceback. An empty `property` has no header.
fn invalid_property(property: &str, detail: &str, at: Option<&Trail>) -> String {
    let (steps, surface) = at.map_or((&[][..], None), |at| (&at.steps[..], at.surface.as_deref()));
    // A node built from a plain table has no site; the nearest ancestor's line leads and leaves the path.
    let near = steps.iter().enumerate().find_map(|(i, step)| Some((i, step.site?)));
    let lead = near.map_or(String::new(), |(_, site)| format!("{site}: {}: ", steps[0]));
    let above = steps.iter().enumerate().rev().filter(|&(i, _)| near.is_none_or(|(near, _)| i != 0 && i != near));
    let path: Vec<String> =
        above.map(|(_, step)| step.site.map_or_else(|| step.to_string(), |at| format!("{step} ({at})"))).collect();
    let path = (!path.is_empty()).then(|| format!("at {}", path.join(" > ")));
    let context: Vec<String> = [path, surface.map(|surface| format!("on `{surface}`"))].into_iter().flatten().collect();
    let context = if context.is_empty() { String::new() } else { format!(" ({})", context.join(" ")) };
    let head = if property.is_empty() { String::new() } else { format!("invalid value for `{property}`: ") };
    let (first, rest) = detail.split_at(detail.find('\n').unwrap_or(detail.len()));
    format!("{lead}{head}{first}{context}{rest}")
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.index {
            Some(index) => write!(f, "{}[{}]", self.kind, index + 1),
            None => write!(f, "{}", self.kind),
        }
    }
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
            Self::InvalidProperty { property, detail, at } => {
                Self::InvalidProperty { property: format!("{prefix}{property}"), detail, at }
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

    /// Names the surface instance, added by `layout::scene::Scene::apply_admitting`: one property
    /// name alone left a config with a dozen surfaces to grep every one. Only `InvalidProperty`
    /// carries a [`Trail`]; the other variants already name the node kind or the whole pass.
    pub(crate) fn on_surface(mut self, surface: &str) -> Self {
        if let Self::InvalidProperty { at, .. } = &mut self {
            at.get_or_insert_default().surface = Some(surface.to_string());
        }
        self
    }

    /// Adds the next node up the walk that reached the failure, from `layout::scene`'s `prepare`
    /// for each child and from the scene for the root. On 2026-09-08 a lock screen's `content`
    /// error named only the surface, which held a dozen `text` nodes, and froze on its last good
    /// scene. `site` names the line that built the node, which an index cannot for a node a helper
    /// returned; `index`, a position among `children` (`None` for a lone `child`), tells apart
    /// siblings built on one line. A `list` renumbers its rows as its source changes.
    pub(crate) fn in_node(mut self, kind: &str, index: Option<usize>, site: Option<Site>) -> Self {
        if let Self::InvalidProperty { at, .. } = &mut self {
            at.get_or_insert_default().steps.push(Step { kind: kind.to_string(), index, site });
        }
        self
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
/// (`hover`, `scroll`, `geometry`, `elided`, `settled`) holding something else is inert rather than an error: the
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
    LayoutError::InvalidProperty { property: property.to_string(), detail: detail.into(), at: None }
}

/// Elements accepted from one config-supplied array: a node's `children`, a `list`'s `items`, and
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

/// Crate-visible because `layout::scene`'s `list` parser reports bad `items`, `each`, or `key`
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
    match value {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => format!("boolean {b}"),
        Value::Integer(i) => format!("integer {i}"),
        Value::Number(n) => marshal::number_word(n),
        // Borrow the Lua buffer and measure before formatting: this is O(1) and copies nothing.
        Value::String(s) if s.as_bytes().len() <= MAX_ERROR_VALUE_PREVIEW_BYTES => format!("string {s:?}"),
        Value::String(s) => oversized(&s.as_bytes()),
        other => other.type_name().into(),
    }
}

/// [`preview_for_error`] for a string Rust already holds.
pub(crate) fn preview_str(s: &str) -> String {
    if s.len() <= MAX_ERROR_VALUE_PREVIEW_BYTES { format!("string {s:?}") } else { oversized(s.as_bytes()) }
}

/// Slice before formatting, so a 20 MB string costs O(200 bytes), not O(len). Lossy rendering is
/// intentional: this is a log preview, and the byte boundary may split a codepoint.
fn oversized(bytes: &[u8]) -> String {
    let prefix = String::from_utf8_lossy(&bytes[..MAX_ERROR_VALUE_PREVIEW_BYTES]);
    let total_len = bytes.len();
    format!(
        "string {prefix:?}... -- {total_len} bytes total, truncated to the first {MAX_ERROR_VALUE_PREVIEW_BYTES} here"
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

/// [`parse_hex_color`] of a Lua string, borrowed rather than copied when it is UTF-8.
fn lua_hex_color(property: &str, s: &mlua::LuaString) -> Result<Rgba, LayoutError> {
    let text = s.to_str().map_err(|_| invalid(property, "hex colour is not valid UTF-8"))?;
    marshal::check_string(&text).map_err(|e| invalid(property, e.to_string()))?;
    parse_hex_color(property, &text)
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

/// A test's property map, built the way production builds one: through the deserializer, which is
/// what matches a key to the `&'static str` a [`PropMap`] holds.
#[cfg(test)]
pub(crate) fn props_from_table(table: &mlua::Table) -> PropMap {
    std::rc::Rc::unwrap_or_clone(crate::lua::nodes::deserialize_lua_table(table).unwrap().properties)
}

/// [`props_from_table`] for the table `src` returns, `kind` included.
#[cfg(test)]
pub(crate) fn props_from_src(lua: &mlua::Lua, src: impl AsRef<str>) -> PropMap {
    props_from_table(&lua.load(src.as_ref()).eval().unwrap())
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
            resolve_declared(&node.properties, "text", false, &lua).unwrap_err(),
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
        resolve_declared(&props_from_table(&table), kind, false, lua).unwrap()
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

        let resolved = resolve_declared(&node.properties, "rect", false, &lua).unwrap();

        assert!(matches!(resolved.get("id"), Some(Value::UserData(_))), "id must survive the resolve step unresolved");
        assert!(
            matches!(fields::common::id.read(&resolved).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id")
        );
    }

    /// Raw rows, `children` and a list's `items` keep the declared table, signals and all.
    #[test]
    fn raw_rows_children_and_a_list_source_are_not_walked_for_signals() {
        let lua = scene_lua();
        for (kind, src, property) in [
            ("panel", r#"id = "bar", layer = "top", anchor = { top = state("a", true) }"#, "anchor"),
            ("column", "children = { rect { width = state('w', 1) } }", "children"),
            ("list", "items = { { a = state('s', 1) } }, each = function() end", "items"),
        ] {
            let declared = props_from_src(&lua, format!(r#"return {{ kind = "{kind}", {src} }}"#));
            let resolved = resolve_declared(&declared, kind, false, &lua).unwrap();
            assert!(same_lua_value(&declared[property], &resolved[property]), "{property}");
        }
        let resolved = resolve_declared(
            &props_from_src(
                &lua,
                r#"return { kind = "panel", id = "bar", layer = "top", anchor = { top = state("a", true) } }"#,
            ),
            "panel",
            false,
            &lua,
        )
        .unwrap();
        assert!(fields::panel::anchor.read(&resolved).unwrap_err().to_string().contains("`top`"));
    }

    /// A key's kind is no row's: `effect` is every kind's, `backdrop` in it a box's.
    #[test]
    fn effect_backdrop_is_refused_off_a_box_kind_naming_the_key() {
        let lua = signal_lua();
        let src = "return { effect = { blur = state('blur', 2), backdrop = { blur = state('b', 4) } } }";
        for kind in ["rect", "row", "panel"] {
            let resolved = resolve_declared(&rect_props(&lua, src), kind, false, &lua).unwrap();
            let Value::Table(effect) = &resolved["effect"] else { panic!() };
            assert_eq!(effect.raw_get::<i64>("blur").unwrap(), 2, "a nested signal resolves");
        }
        for kind in ["text", "image", "list"] {
            let err = resolve_declared(&rect_props(&lua, src), kind, false, &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "effect.backdrop"),
                "{kind}: {err:?}"
            );
        }
        let blur_only = rect_props(&lua, "return { effect = { blur = 2 } }");
        assert!(resolve_declared(&blur_only, "text", false, &lua).is_ok(), "`blur` is every kind's");
        let reads = |input: &str| {
            let src = format!("return {{ effect = {{ shader = {{ source = '/s.frag', input = '{input}' }} }} }}");
            resolve_declared(&rect_props(&lua, &src), "text", false, &lua)
        };
        for input in ["backdrop", "desktop"] {
            let err = reads(input).unwrap_err();
            assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "effect.shader.input"));
        }
        assert!(reads("content").is_ok(), "a content shader is every kind's");
    }

    /// A cycle or a table reached many ways is walked once; past the depth limit a signal stays.
    #[test]
    fn a_cyclic_shared_or_over_deep_table_ends_its_walk() {
        let lua = signal_lua();
        let props =
            rect_props(&lua, "local m = { top = state('top', 4) } for i = 1, 20 do m[i] = m end return { margin = m }");
        let Value::Table(margin) = &resolve_declared(&props, "rect", false, &lua).unwrap()["margin"] else { panic!() };
        assert_eq!(margin.raw_get::<i64>("top").unwrap(), 4);
        // A back-reference keeps the original table, signal and all, for the parser to refuse.
        assert!(
            matches!(margin.raw_get::<Value>(1).unwrap(), Value::Table(m) if m.raw_get::<Value>("top").unwrap().is_userdata())
        );
        let shader: mlua::Table = lua
            .load(r#"local t = { x = state("x", 1) } for _ = 1, 9 do t = { t } end return { kind = "shader", params = t }"#)
            .eval()
            .unwrap();
        let mut table = resolve_declared(&props_from_table(&shader), "shader", false, &lua).unwrap()["params"].clone();
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
        let resolved = resolve_declared(&props_from_table(&shader), "shader", false, &lua).unwrap();
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
        let err = resolve_declared(&props, "rect", false, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "background.stops[2][2]"),
            "{err:?}"
        );
    }

    /// The pairing rule (ADR-0095). `on_hover` fires on the crossing its node's `hover` signal's
    /// pointer record reports, so without a slot the callback is unreachable -- refused, rather than left to be a
    /// handler a config watches never fire.
    #[test]
    fn on_hover_without_a_hover_slot_is_refused() {
        let lua = signal_lua();
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("on_hover", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        let node = deserialize_lua_table(&table).unwrap();

        assert!(matches!(resolve_declared(&node.properties, "rect", false, &lua).unwrap_err(),
                LayoutError::InvalidProperty { property, .. } if property == "on_hover"));
    }

    #[test]
    fn a_wrong_typed_callback_or_input_flag_is_refused_by_name() {
        let lua = mlua::Lua::new();
        for (source, property, expected) in [
            (r#"{ kind = "row", on_click = "quit" }"#, "on_click", "expected a function, got string \"quit\""),
            (r#"{ kind = "icon", submits_secure = 1 }"#, "submits_secure", "expected a boolean, got integer 1"),
            (r#"{ kind = "textfield", autofocus = "yes" }"#, "autofocus", "expected a boolean, got string \"yes\""),
        ] {
            let err =
                resolve_declared(&props_from_src(&lua, format!("return {source}")), "rect", false, &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, detail, .. } if p == property && detail == expected),
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

        let resolved = resolve_declared(&node.properties, "rect", false, &lua).unwrap();
        assert!(matches!(resolved.get("on_hover"), Some(Value::Function(_))), "a Function is not a Signal to resolve");
        assert!(matches!(resolved.get("hover"), Some(Value::UserData(_))), "the slot stays the handle it was");
    }

    /// The sort in [`resolve_declared`] (ADR-0024). The fixture is `opacity` and `background`
    /// because the map reaches them in that order, so dropping the sort fails this; the first
    /// assertion is what keeps a hasher change from quietly making the second one vacuous.
    #[test]
    fn two_failing_properties_always_report_the_same_one() {
        let lua = signal_lua();
        let props = props_from_src(
            &lua,
            r#"
                    return {
                        kind = "rect",
                        background = computed({}, function() error("background boom") end),
                        opacity = computed({}, function() error("opacity boom") end),
                    }
                    "#,
        );

        let unsorted: Vec<&str> = props.keys().copied().collect();
        assert_eq!(unsorted, ["opacity", "background"], "the fixture must not already be in sorted order");

        let err = resolve_declared(&props, "rect", false, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "background"),
            "a broken config must name the property its own text names first, got: {err}"
        );
    }

    #[test]
    fn oversized_string_property_error_still_names_type_and_shows_a_recognizable_prefix() {
        let lua = mlua::Lua::new();
        let props = props_from_src(&lua, r#"return { kind = "rect", radius = string.rep("Q", 20 * 1024 * 1024) }"#);
        let err = parse_radius(&props).unwrap_err();
        let LayoutError::InvalidProperty { property, detail, .. } = &err else {
            panic!("expected InvalidProperty, got {err}");
        };
        assert_eq!(property, "radius");
        assert!(
            detail.len() < 1024,
            "a 20 MB input must not produce a multi-megabyte error message, got {} bytes",
            detail.len()
        );
        assert!(detail.contains("expected a number"), "{detail}");
        assert!(detail.contains("string \""), "must still name the rejected type: {detail}");
        assert!(detail.contains("QQQ"), "must show a recognizable prefix of the value: {detail}");
        assert!(
            detail.contains(&(20 * 1024 * 1024).to_string()),
            "must state the real length, or a truncated preview reads as the whole value: {detail}"
        );
    }

    #[test]
    fn a_rust_string_previews_like_a_lua_one_and_is_capped() {
        assert_eq!(preview_str("#12"), "string \"#12\"");
        let long = preview_str(&"Q".repeat(1 << 20));
        assert!(long.len() < 400 && long.contains("1048576 bytes total"), "{long}");
    }

    #[test]
    fn property_error_messages_for_string_and_non_string_values_are_unchanged() {
        let lua = mlua::Lua::new();
        for (value, expected) in [
            (r#""banana""#, "expected a number or a table, got string \"banana\""),
            ("true", "expected a number or a table, got boolean true"),
        ] {
            let props = props_from_src(&lua, format!(r#"return {{ kind = "rect", radius = {value} }}"#));
            let err = parse_radius(&props).unwrap_err();
            let LayoutError::InvalidProperty { detail, .. } = &err else {
                panic!("expected InvalidProperty, got {err}");
            };
            assert_eq!(detail, expected, "{value}");
        }
    }
}

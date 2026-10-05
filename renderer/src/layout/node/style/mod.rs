//! Box-model and paint-adjacent value types.

use cursor_icon::CursorIcon;
use mlua::Value;

use super::prop::{keywords, within as row_within, within_range};
use super::*;
use crate::lua::luacats::lua_shape;

mod fill;
#[cfg(test)]
pub(crate) use fill::GradientStop;
pub(crate) use fill::{Background, BackgroundLayer, MAX_BACKGROUNDS, is_layer_list, layer_fill};
pub use fill::{Fill, Gradient, GradientShape, Mask, MaskSource};

mod transform;
pub use transform::{
    Affine, IDENTITY_AFFINE, Transform, apply_affine, compose_affine, invert_affine, parse_transform,
    transformed_bounds,
};

/// `"NN%"` (`^\d+(\.\d+)?%$`) as `SizeMode::Percent`. Not a confirmed spec syntax: the base
/// property table only documents integer/`"fill"` for width/height, though `Percent(f32)` is
/// named as a size class with no literal Lua form given. See ADR-0023.
pub(super) fn parse_percent(s: &str) -> Option<f32> {
    let digits = s.strip_suffix('%')?;
    let mut parts = digits.splitn(2, '.');
    let int_part = parts.next()?;
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(frac_part) = parts.next()
        && (frac_part.is_empty() || !frac_part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    digits.parse::<f32>().ok().map(|n| n / 100.0)
}

spelled!(SizeMode => "Length");

/// `width`/`height`: pixels within the row's range (ADR-0021), `"fill"`, or `"NN%"`. The map is a
/// [`super::resolve_declared`] result, so absent covers both omission and a signal resolving to `nil`.
impl Prop for SizeMode {
    type Out = SizeMode;
    fn read(row: &Property, value: Option<&Value>) -> Result<SizeMode, LayoutError> {
        let Some(value) = value else {
            return Ok(SizeMode::Content);
        };
        if let Some(n) = value_as_f32(row.name, value)? {
            return Ok(SizeMode::Pixels(row_within(row, n)?));
        }
        if let Value::String(s) = value {
            if &*s.as_bytes() == b"fill" {
                return Ok(SizeMode::Fill);
            }
            if let Ok(s_str) = s.to_str()
                && let Some(pct) = parse_percent(&s_str)
            {
                return Ok(SizeMode::Percent(pct));
            }
        }
        Err(invalid(
            row.name,
            format!(
                "expected a number, \"fill\", or a \"NN%\" string (Content sizing has no literal -- omit the property instead), got {}",
                preview_for_error(value)
            ),
        ))
    }
}

/// `margin`/`padding`/`border_width`: a number sets all four edges, a table each; an absent edge is
/// 0, and a row with a range bounds every edge. Parsed once per node per pass: `table.get` is
/// metamethod-aware, so every consumer reading it again would re-run `__index`, and two reads could
/// disagree about one child's margin. Those reads are plain Lua outside any signal, so
/// `LayoutPassBudget`, not ADR-0021's per-getter cap, bounds them.
pub(crate) struct NumberOrEdges;

spelled!(NumberOrEdges => format!("{}|{}", f32::lua(), EdgeInsets::lua()));

impl Prop for NumberOrEdges {
    type Out = EdgeInsets;
    fn read(row: &Property, value: Option<&Value>) -> Result<EdgeInsets, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(EdgeInsets::default());
        };
        let insets = if let Some(n) = value_as_f32(property, value)? {
            EdgeInsets { top: n, right: n, bottom: n, left: n }
        } else {
            let Value::Table(table) = value else {
                return Err(invalid(
                    property,
                    format!("expected a number or a table, got {}", preview_for_error(value)),
                ));
            };
            EdgesInput::read(property, table)?.into_edges()
        };
        for n in [insets.top, insets.right, insets.bottom, insets.left] {
            row_within(row, n)?;
        }
        Ok(insets)
    }
}

keywords! {
    /// `corner_shape`: CSS's `corner-shape` names.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CornerShape {
        Round,
        /// A quarter circle cut in, centred on the box's corner point.
        Scoop,
    }
}

/// `radius`: a number sets all four corners, a table each; an absent corner is 0, and the row's
/// range bounds every corner.
pub(crate) struct NumberOrCorners;

spelled!(NumberOrCorners => format!("{}|{}", f32::lua(), CornersInput::lua()));

impl Prop for NumberOrCorners {
    type Out = Radii;
    fn read(row: &Property, value: Option<&Value>) -> Result<Radii, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(Radii::default());
        };
        let radii = if let Some(n) = value_as_f32(property, value)? {
            [n; 4]
        } else {
            let Value::Table(table) = value else {
                return Err(invalid(
                    property,
                    format!("expected a number or a table, got {}", preview_for_error(value)),
                ));
            };
            let c = CornersInput::read(property, table)?;
            [c.top_left, c.top_right, c.bottom_right, c.bottom_left].map(|r| r.unwrap_or(0.0))
        };
        for n in radii {
            row_within(row, n)?;
        }
        Ok(Radii(radii, 0.0, None))
    }
}

/// `radius` and `corner_smoothing`, the radius negated under `corner_shape = "scoop"`.
pub fn parse_radius(properties: &PropMap) -> Result<Radii, LayoutError> {
    let radius = fields::paint::radius.read(properties)?;
    let smoothing = fields::paint::corner_smoothing.read(properties)?;
    let scoop = fields::paint::corner_shape.read(properties)? == CornerShape::Scoop;
    if let Some(outline) = fields::paint::outline.read(properties)? {
        if !radius.is_zero() || smoothing > 0.0 || scoop {
            return Err(invalid("outline", "replaces radius, corner_shape and corner_smoothing; round it with corner"));
        }
        return Ok(Radii([0.0; 4], 0.0, Some(outline)));
    }
    if scoop {
        if smoothing > 0.0 {
            return Err(invalid(
                "corner_smoothing",
                "cannot combine with corner_shape = \"scoop\"; smoothing needs \"round\"",
            ));
        }
        return Ok(radius * -1.0);
    }
    Ok(Radii(radius.0, smoothing, None))
}

/// The range an overshooting easing is clamped into: the property table's `range`, else
/// `[0, 8192]`; `margin`, which no parser bounds, tweens through negatives as `translate` does.
/// The tween clamps every numeric property, even those without a row range. `spacing`, icon
/// `size`, and `margin` accept negatives. `padding` takes the `[0, 8192]` range, though the solver
/// would absorb a negative: overflow is spelled with a negative `margin`, so padding only insets.
/// `snap_to_physical` bounds the coordinates that reach `wl_region`. Otherwise, give a row a range
/// only where a consumer refuses the value.
///
/// `radius` and `border_width` share the `8192` ceiling with `width`/`height`. It is femtovg's:
/// above roughly 8.4e6 `curve_divisions` (`path/cache.rs:911`) divides by `acos(1.0) == 0.0`, and
/// `inf as u32` becomes `u32::MAX`, so billions of iterations and tens of GB of vertices land on
/// the Wayland dispatch thread. Below zero, `radius = -4` silently squares
/// corners (`path.rs:458` treats under 0.1 as unrounded) and `border_width = -4` clamps to 0 and
/// clears paint alpha.
/// ponytail: femtovg 0.27's `curve_divisions` is unclamped; lift the ceiling once it clamps.
///
/// `font_size` alone floors at 1. A zero size gives the shaper a zero line height. Flooring in the
/// row rather than in a consumer covers the tween too, which clamps into this same range. Icon
/// `size` needs no floor: it becomes a `Measure::Square` and the painter takes its pixels from the
/// resolved box, so it never reaches a shaper.
pub(super) fn range_of(property: &str) -> (f32, f32) {
    if property == "scroll" {
        return (0.0, f32::MAX);
    }
    crate::lua::nodes::range(property).unwrap_or(if property == "margin" { (-8192.0, 8192.0) } else { (0.0, 8192.0) })
}

/// What an absent key in `property`'s `{ x, y }` or edge table means: the identity for that
/// property.
pub(super) fn axis_default(property: &str) -> f32 {
    match property {
        "scale" => 1.0,
        "origin" => 0.5,
        _ => 0.0,
    }
}

/// `row`'s `{ x, y }` table, absent keys at [`axis_default`], both within its range.
fn xy(row: &Property, value: &Value) -> Result<(f32, f32), LayoutError> {
    let property = row.name;
    let Value::Table(table) = value else {
        return Err(invalid(property, format!("expected an {{ x, y }} table, got {}", preview_for_error(value))));
    };
    let Axes { x, y } = Axes::read(property, table)?;
    Ok((row_within(row, x.unwrap_or(axis_default(property)))?, row_within(row, y.unwrap_or(axis_default(property)))?))
}

// `translate`, `origin`: a per-axis pair, which `xy` reads as `(x, y)`.
lua_shape! {
    /// A missing axis takes the property's default.
    #[alias = "Axes"]
    pub(crate) struct Axes {
        x: Option<f32>,
        y: Option<f32>,
    }
}

impl Prop for Axes {
    type Out = (f32, f32);
    fn read(row: &Property, value: Option<&Value>) -> Result<(f32, f32), LayoutError> {
        let default = axis_default(row.name);
        value.map_or(Ok((default, default)), |value| xy(row, value))
    }
}

/// `scale`: one factor for both axes, or [`Axes`].
pub(crate) struct Scale;

spelled!(Scale => format!("{}|{}", f32::lua(), Axes::lua()));

impl Prop for Scale {
    type Out = (f32, f32);
    fn read(row: &Property, value: Option<&Value>) -> Result<(f32, f32), LayoutError> {
        let Some(value) = value else {
            return <Axes as Prop>::read(row, None);
        };
        match value_as_f32(row.name, value)? {
            Some(n) => Ok((row_within(row, n)?, n)),
            None => xy(row, value),
        }
    }
}

keywords! {
    /// Whether a node clips children to its box, lets `radius` shape the clip, or leaves them on
    /// the parent's. Rounded clipping needs an offscreen target and composite; a square clip is a
    /// free GPU scissor.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ClipShape {
        /// The node's rectangle with square corners.
        Box,
        /// The node's rounded shape, using the same arc as its background fill.
        Rounded,
        /// Nothing: children keep the parent's clip, so a wrapper does not cut their shadows.
        None,
    }
}

impl ClipShape {
    /// `clip` as declared; absent, a surface or a scroll viewport cuts to its box and any other
    /// node lets its children spill, as CSS `overflow: visible`.
    pub(crate) fn of(kind: &str, properties: &PropMap) -> Result<Self, LayoutError> {
        if properties.contains_key(fields::paint::clip.row.name) {
            return fields::paint::clip.read(properties);
        }
        // The buffer cuts a surface anyway; its box keeps every command, damage rect and layer inside it.
        let surface = fields::kind_bit(kind).is_some_and(|bit| bit & fields::SURFACES != 0);
        let scrolls = fields::flow::scroll.read(properties)?.is_some();
        Ok(if surface || scrolls { Self::Box } else { Self::None })
    }
}

// `None` means "not painted", the same absence `Fill` returns for a missing fill: an edge at
// width 0 needs no colour, and one with a colour at width 0 still paints nothing, so the drawing
// pass gets the same answer either way. The table form gives no per-edge default, so an absent edge
// takes `None` rather than an invented one.
lua_shape! {
    /// Per-edge colours.
    #[alias = "BorderColors"]
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct BorderColor {
        pub top: Option<Rgba>,
        pub right: Option<Rgba>,
        pub bottom: Option<Rgba>,
        pub left: Option<Rgba>,
    }
}

/// What a border is painted with: per-edge colours, or one gradient along the whole outline.
#[derive(Debug, Clone, PartialEq)]
pub enum BorderPaint {
    Edges(BorderColor),
    Gradient(Gradient),
}

impl Default for BorderPaint {
    fn default() -> Self {
        Self::Edges(BorderColor::default())
    }
}

/// `border_color`: one colour for every edge, [`BorderColor`], or a [`Gradient`] over the whole outline.
pub(crate) struct ColorOrEdges;

spelled!(ColorOrEdges => format!("{}|{}|{}", Rgba::lua(), BorderColor::lua(), Gradient::lua()));

impl Prop for ColorOrEdges {
    type Out = BorderPaint;
    fn read(row: &Property, value: Option<&Value>) -> Result<BorderPaint, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(BorderPaint::default());
        };
        if let Value::String(s) = value {
            let color = Some(parse_hex_color(property, &checked_string(property, s)?)?);
            return Ok(BorderPaint::Edges(BorderColor { top: color, right: color, bottom: color, left: color }));
        }
        let Value::Table(table) = value else {
            return Err(invalid(property, format!("expected a string or a table, got {}", preview_for_error(value))));
        };
        // ponytail: a gradient spans the whole outline, so an edge's own gradient is refused; upgrade: clip each edge's band to its gradient.
        if table.contains_key("gradient").map_err(|e| invalid(property, e.to_string()))? {
            return Ok(BorderPaint::Gradient(Gradient::read(property, table)?));
        }
        BorderColor::read(property, table).map(BorderPaint::Edges)
    }
}

keywords! {
    /// `list.direction`: which of `row`'s or `column`'s layout a list borrows.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Direction {
        Vertical,
        Horizontal,
    }
}

impl Direction {
    /// The borrowed kind, rather than adding a third layout arm.
    pub fn kind(self) -> &'static str {
        match self {
            Direction::Vertical => "column",
            Direction::Horizontal => "row",
        }
    }
}

/// A drop shadow in logical pixels, CSS `box-shadow`'s terms: `blur` is the radius (sigma is half
/// of it), `spread` grows the shape before blurring (ADR-0254).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub color: Rgba,
    pub blur: f32,
    pub offset: (f32, f32),
    pub spread: f32,
    /// CSS `box-shadow: inset`: cast inside the padding box instead of around the box (ADR-0331).
    pub inset: bool,
    /// How the layer composites onto what is under it.
    pub blend: Blend,
}

impl Shadow {
    /// Qt's `MultiEffect` rule: a shadow shows once it has alpha and a blur, an offset or a spread.
    fn shows(&self) -> bool {
        self.color.a > 0.0 && (self.blur > 0.0 || self.spread != 0.0 || self.offset != (0.0, 0.0))
    }
}

/// What a node's own painted output is filtered by (ADR-0254). `shadows` are the layers that
/// show, first on top; `blur` is `effect.blur`, CSS `filter: blur()`'s sigma; `backdrop` is
/// `effect.backdrop.blur`, `backdrop-filter: blur()`'s (ADR-0256). `0` is off. `tone` and
/// `backdrop_tone` are the colour filters after each blur (ADR-0334). `content_shadow` is
/// `shadow_mode = "content"`: the shadows are cast by the painted subtree, not the box's shape
/// (ADR-0260).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Effect {
    pub shadows: Vec<Shadow>,
    /// The `inset` layers, first on top; they draw inside the box, so they never open a layer or grow its reach.
    pub inset: Vec<Shadow>,
    /// A program run over the painted subtree, before `blur` and `tone` (ADR-0336).
    pub shader: Option<EffectShader>,
    /// A program run over what the surface painted under the node, drawn before the node.
    pub backdrop_shader: Option<EffectShader>,
    pub blur: f32,
    pub tone: Tone,
    pub backdrop: f32,
    pub backdrop_tone: Tone,
    /// `effect.backdrop.mask`: scales the glass's coverage.
    pub backdrop_mask: Option<Mask>,
    pub content_shadow: bool,
    /// `blend`: how the finished subtree composites onto the backdrop, after every filter.
    pub blend: Blend,
}

impl Effect {
    /// Whether the node's own output needs an offscreen: a shadow, a shader, a blur, a colour filter or a blend.
    pub fn layers(&self) -> bool {
        !self.shadows.is_empty()
            || self.shader.is_some()
            || self.blur > 0.0
            || !self.tone.is_identity()
            || self.blend != Blend::Normal
    }
}

/// `effect.shader`: an absolute `.frag` path, its `params`, and `padding` logical px around the box
/// the program may read and draw (ADR-0336).
#[derive(Debug, Clone, PartialEq)]
pub struct EffectShader {
    pub source: std::path::PathBuf,
    pub params: Vec<ShaderParam>,
    pub images: Vec<ShaderImage>,
    pub padding: f32,
    /// `u_progress`, as on a `shader` node.
    pub progress: f32,
}

keywords! {
    /// `effect.shader.input`: the pixels the program reads as `u_input`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum ShaderInput {
        /// The node's painted subtree.
        #[default]
        Content,
        /// What the surface painted under the node.
        Backdrop,
    }
}

keywords! {
    /// CSS `mix-blend-mode`, and Apple's `plus-lighter` and `plus-darker`, for a node, a
    /// `background` layer or a `shadows` layer. The order is the blend program's `u_mode`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Blend {
        #[default]
        Normal,
        Multiply,
        Screen,
        Overlay,
        Darken,
        Lighten,
        ColorDodge,
        ColorBurn,
        HardLight,
        SoftLight,
        Difference,
        Exclusion,
        Hue,
        Saturation,
        Color,
        Luminosity,
        PlusLighter,
        PlusDarker,
    }
}

/// The most `effect.shader.padding` grows the offscreen, in logical px.
const SHADER_PADDING: (f32, f32) = (0.0, 512.0);

/// CSS `saturate()`, `brightness()` and `contrast()` in that order, each `1` for off, applied to
/// straight sRGB as the CSS shorthands do (ADR-0334).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tone {
    pub saturate: f32,
    pub brightness: f32,
    pub contrast: f32,
}

impl Default for Tone {
    fn default() -> Self {
        Self { saturate: 1.0, brightness: 1.0, contrast: 1.0 }
    }
}

impl Tone {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// The Filter Effects `saturate()` matrix, column-major for GL: `s` on the diagonal and
    /// `1 - s` times the luma weights everywhere.
    pub fn saturate_columns(&self) -> [f32; 9] {
        const LUMA: [f32; 3] = [0.213, 0.715, 0.072];
        let s = self.saturate;
        std::array::from_fn(|i| (1.0 - s) * LUMA[i / 3] + if i % 3 == i / 3 { s } else { 0.0 })
    }
}

keywords! {
    /// `shadow_mode`: CSS `box-shadow` of the box shape, or `drop-shadow` of everything painted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ShadowMode {
        Box,
        Content,
    }
}

// ponytail: 16 layers, each a gradient quad or, in content mode, a blur pass; raise it with a measured budget.
const MAX_SHADOWS: usize = 16;
/// A layer's `blur` and each `effect` blur, and a layer's `offset` and `spread`, in px; the tween clamps into the same ranges.
pub(super) const SHADOW_BLUR: (f32, f32) = (0.0, 8192.0);
/// Each `effect` colour filter's factor; past 8 `saturate` and `contrast` have clipped every channel.
pub(super) const TONE: (f32, f32) = (0.0, 8.0);
pub(super) const SHADOW_REACH: (f32, f32) = (-8192.0, 8192.0);

lua_shape! {
    /// One `shadows` layer.
    #[alias = "ShadowLayer"]
    pub(crate) struct ShadowLayer {
        color: Option<Rgba>,
        blur: Option<f32>,
        offset: Option<Axes>,
        spread: Option<f32>,
        inset: Option<bool>,
        blend: Option<Blend> as Option<BlendAlias>,
    }
}

/// `Blend` by its stub alias.
#[cfg(test)]
struct BlendAlias;
#[cfg(test)]
spelled!(BlendAlias => "Blend");

lua_shape! {
    /// `effect.backdrop`: CSS `backdrop-filter`, a box kind's only.
    #[alias = "BackdropEffect"]
    #[derive(Default)]
    pub(crate) struct BackdropKeys {
        pub(crate) blur: Option<f32>,
        pub(crate) saturate: Option<f32>,
        pub(crate) brightness: Option<f32>,
        pub(crate) contrast: Option<f32>,
        pub(crate) mask: Option<Mask>,
    }
}

lua_shape! {
    /// `effect.shader`: a config fragment shader over the node's painted subtree or its backdrop.
    #[alias = "EffectShader"]
    pub(crate) struct ShaderKeys {
        pub(crate) source: std::path::PathBuf,
        pub(crate) input: Option<ShaderInput>,
        pub(crate) params: Value as Option<Params>,
        pub(crate) images: Value as Option<Images>,
        pub(crate) padding: Option<f32>,
        pub(crate) progress: Option<f32>,
    }
}

lua_shape! {
    /// `effect`: CSS `filter` for a node.
    #[alias = "Effect"]
    #[derive(Default)]
    pub(crate) struct EffectKeys {
        pub(crate) blur: Option<f32>,
        pub(crate) saturate: Option<f32>,
        pub(crate) brightness: Option<f32>,
        pub(crate) contrast: Option<f32>,
        pub(crate) shader: Option<ShaderKeys>,
        pub(crate) backdrop: Option<BackdropKeys>,
    }
}

/// `effect`: the node's pixel filters, each blur in `[0, 8192]` and each colour filter in `[0, 8]`.
/// Absent keys are off.
pub(crate) struct Effects;

spelled!(Effects => EffectKeys::lua());

impl Prop for Effects {
    type Out = EffectKeys;
    fn read(row: &Property, value: Option<&Value>) -> Result<EffectKeys, LayoutError> {
        let table = match value {
            None => return Ok(EffectKeys::default()),
            Some(Value::Table(table)) => table,
            Some(value) => {
                return Err(invalid(row.name, format!("expected a table, got {}", preview_for_error(value))));
            }
        };
        let keys = EffectKeys::read(row.name, table)?;
        let within = |key: &str, range, n: Option<f32>| {
            n.map(|n| within_range(&format!("{}.{key}", row.name), range, n)).transpose()
        };
        let backdrop = match keys.backdrop {
            Some(b) => Some(BackdropKeys {
                blur: within("backdrop.blur", SHADOW_BLUR, b.blur)?,
                saturate: within("backdrop.saturate", TONE, b.saturate)?,
                brightness: within("backdrop.brightness", TONE, b.brightness)?,
                contrast: within("backdrop.contrast", TONE, b.contrast)?,
                mask: match b.mask {
                    Some(Mask { source: MaskSource::Node(_), .. }) => {
                        return Err(invalid("effect.backdrop.mask", "takes a `gradient` or a `source`, not a `node`"));
                    }
                    mask => mask,
                },
            }),
            None => None,
        };
        let shader = match keys.shader {
            Some(shader) => {
                if !shader.source.is_absolute() {
                    return Err(invalid(
                        "effect.shader.source",
                        format!("expected an absolute path, got `{}`", shader.source.display()),
                    ));
                }
                Some(ShaderKeys {
                    padding: within("shader.padding", SHADER_PADDING, shader.padding)?,
                    progress: within("shader.progress", range_of("progress"), shader.progress)?,
                    ..shader
                })
            }
            None => None,
        };
        Ok(EffectKeys {
            blur: within("blur", SHADOW_BLUR, keys.blur)?,
            shader,
            saturate: within("saturate", TONE, keys.saturate)?,
            brightness: within("brightness", TONE, keys.brightness)?,
            contrast: within("contrast", TONE, keys.contrast)?,
            backdrop,
        })
    }
}

/// `shadows`: CSS `box-shadow`'s list, first on top. `None` when absent.
pub(crate) struct Shadows;

spelled!(Shadows => Vec::<ShadowLayer>::lua());

impl Prop for Shadows {
    type Out = Option<Vec<Shadow>>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let table = match value {
            None => return Ok(None),
            Some(Value::Table(table)) => table,
            Some(value) => {
                return Err(invalid(row.name, format!("expected a layer array, got {}", preview_for_error(value))));
            }
        };
        let within = |n: f32, range| within_range(row.name, range, n);
        let len = input::array_len(row.name, table, MAX_SHADOWS)?;
        let mut shadows = Vec::with_capacity(len);
        for i in 1..=len {
            let name = format!("{}[{i}]", row.name);
            let layer: mlua::Table = table.raw_get(i).map_err(|e| invalid(&name, e.to_string()))?;
            let ShadowLayer { color, blur, offset, spread, inset, blend } = ShadowLayer::read(&name, &layer)?;
            let Axes { x, y } = offset.unwrap_or(Axes { x: None, y: None });
            shadows.push(Shadow {
                color: color.unwrap_or(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                blur: within(blur.unwrap_or(0.0), SHADOW_BLUR)?,
                offset: (within(x.unwrap_or(0.0), SHADOW_REACH)?, within(y.unwrap_or(0.0), SHADOW_REACH)?),
                spread: within(spread.unwrap_or(0.0), SHADOW_REACH)?,
                inset: inset.unwrap_or(false),
                blend: blend.unwrap_or_default(),
            });
        }
        Ok(Some(shadows))
    }
}

/// `shadows` and `effect`, every kind, and a box's `shadow_mode`. Only layers that would draw are
/// kept, so paint never opens an offscreen for one.
pub fn parse_effect(properties: &PropMap) -> Result<Effect, LayoutError> {
    use fields::{common, paint};
    let mut shadows = common::shadows.read(properties)?.unwrap_or_default();
    shadows.retain(Shadow::shows);
    let content_shadow = paint::shadow_mode.read(properties)? == ShadowMode::Content;
    let (inset, shadows): (Vec<_>, Vec<_>) = shadows.into_iter().partition(|shadow| shadow.inset);
    if content_shadow && !inset.is_empty() {
        return Err(invalid("shadows", "an `inset` layer needs `shadow_mode = \"box\"`"));
    }
    let filters = common::effect.read(properties)?;
    let tone = |saturate: Option<f32>, brightness: Option<f32>, contrast: Option<f32>| Tone {
        saturate: saturate.unwrap_or(1.0),
        brightness: brightness.unwrap_or(1.0),
        contrast: contrast.unwrap_or(1.0),
    };
    let backdrop = filters.backdrop.unwrap_or_default();
    let (mut shader, mut backdrop_shader) = (None, None);
    if let Some(keys) = filters.shader {
        let program = EffectShader {
            source: keys.source,
            params: super::animate::parse_shader_params("effect.shader.params", &keys.params)?,
            images: super::animate::parse_shader_images("effect.shader.images", &keys.images)?,
            padding: keys.padding.unwrap_or(0.0),
            progress: keys.progress.unwrap_or(0.0),
        };
        match keys.input.unwrap_or_default() {
            ShaderInput::Content => shader = Some(program),
            ShaderInput::Backdrop => backdrop_shader = Some(program),
        }
    }
    Ok(Effect {
        shadows,
        inset,
        shader,
        backdrop_shader,
        blur: filters.blur.unwrap_or(0.0),
        tone: tone(filters.saturate, filters.brightness, filters.contrast),
        backdrop: backdrop.blur.unwrap_or(0.0),
        backdrop_tone: tone(backdrop.saturate, backdrop.brightness, backdrop.contrast),
        backdrop_mask: backdrop.mask,
        content_shadow,
        blend: common::blend.read(properties)?,
    })
}

/// `cursor`: CSS names such as `"pointer"`, `"text"`, `"grab"`, and resize edges, or `None`
/// for the default rule (ADR-0107; `layout::hit::cursor_under`). `cursor_icon` and
/// `wp_cursor_shape_v1` use the same names, so the compositor reads the config string directly.
pub(crate) struct Cursor;

spelled!(Cursor => "Cursor");

impl Prop for Cursor {
    type Out = Option<CursorIcon>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<CursorIcon>, LayoutError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let Value::String(name) = value else {
            return Err(invalid(row.name, format!("must be a cursor name string, got {}", preview_for_error(value))));
        };
        let name = name.to_str().map_err(|_| invalid(row.name, "must be UTF-8"))?;
        name.parse::<CursorIcon>().map(Some).map_err(|_| {
            invalid(row.name, format!("unknown cursor name {name:?}; the names are CSS's, like \"pointer\""))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::signal_lua;
    use crate::lua::nodes::deserialize_lua_table;

    #[test]
    fn width_absent_is_content() {
        let props = PropMap::default();
        assert_eq!(fields::common::width.read(&props).unwrap(), SizeMode::Content);
    }

    #[test]
    fn width_integer_is_pixels() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = 32 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(fields::common::width.read(&props).unwrap(), SizeMode::Pixels(32.0));
    }

    #[test]
    fn width_fill_string_is_fill() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = "fill" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(fields::common::width.read(&props).unwrap(), SizeMode::Fill);
    }

    #[test]
    fn width_percent_string_divides_by_100() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = "50%" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(fields::common::width.read(&props).unwrap(), SizeMode::Percent(0.5));
    }

    #[test]
    fn width_above_the_8192_ceiling_is_invalid_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = 8193 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(fields::common::width.read(&props).unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn a_negative_width_is_invalid_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = -5 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(fields::common::width.read(&props).unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn width_garbage_string_is_invalid_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = "banana" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(fields::common::width.read(&props).unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn height_content_error_names_omission_as_the_spelling() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", height = "content" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::common::height.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "height" && detail.contains("omit the property")),
            "must name omission as how Content sizing is spelled: {err}"
        );
    }

    #[test]
    fn margin_reads_named_edges_defaulting_absent_ones_to_zero() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", margin = { top = 4, left = 2 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let insets = fields::common::margin.read(&props).unwrap();
        assert_eq!(insets, EdgeInsets { top: 4.0, right: 0.0, bottom: 0.0, left: 2.0 });
    }

    #[test]
    fn padding_reads_named_edges_defaulting_absent_ones_to_zero() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", padding = { top = 4, left = 2 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let insets = fields::common::padding.read(&props).unwrap();
        assert_eq!(insets, EdgeInsets { top: 4.0, right: 0.0, bottom: 0.0, left: 2.0 });
    }

    #[test]
    fn margin_scalar_broadcasts_to_all_four_edges() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", margin = 10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::common::margin.read(&props).unwrap(),
            EdgeInsets { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 }
        );
    }

    #[test]
    fn padding_scalar_broadcasts_to_all_four_edges() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", padding = 10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::common::padding.read(&props).unwrap(),
            EdgeInsets { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 }
        );
    }

    #[test]
    fn margin_negative_value_is_accepted() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", margin = -10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::common::margin.read(&props).unwrap(),
            EdgeInsets { top: -10.0, right: -10.0, bottom: -10.0, left: -10.0 }
        );
    }

    #[test]
    fn negative_padding_is_invalid_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", padding = -10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(
            matches!(fields::common::padding.read(&props), Err(LayoutError::InvalidProperty { property, .. }) if property == "padding")
        );
        let table: mlua::Table = lua.load(r#"return { kind = "rect", padding = { left = -10 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(
            matches!(fields::common::padding.read(&props), Err(LayoutError::InvalidProperty { property, .. }) if property == "padding")
        );
    }

    #[test]
    fn visible_absent_defaults_true() {
        let props = PropMap::default();
        assert!(fields::common::visible.read(&props).unwrap());
    }

    #[test]
    fn a_signal_userdata_in_a_geometry_slot_resolves_to_its_current_value() {
        let lua = signal_lua();
        let signal =
            crate::lua::signal::Signal::new_live(Value::Boolean(false), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("visible", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let resolved = resolve_declared(node.properties, "rect", false, &lua).unwrap();
        assert!(
            !fields::common::visible.read(&resolved).unwrap(),
            "must read the signal's current value, not error on the handle"
        );
    }

    #[test]
    fn spacing_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "row", spacing = 1e300 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            fields::flow::spacing.read(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "spacing"
        ));
    }

    #[test]
    fn background_absent_is_none() {
        let props = PropMap::default();
        assert!(fields::paint::background.read(&props).unwrap().is_empty());
    }

    #[test]
    fn background_six_digit_hex_is_opaque() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#336699" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::background.read(&props).unwrap(),
            vec![(
                Fill::Color(Rgba { r: 0x33 as f32 / 255.0, g: 0x66 as f32 / 255.0, b: 0x99 as f32 / 255.0, a: 1.0 }),
                Blend::Normal
            )]
        );
    }

    #[test]
    fn background_eight_digit_hex_carries_its_own_alpha() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#33669980" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::background.read(&props).unwrap(),
            vec![(
                Fill::Color(Rgba {
                    r: 0x33 as f32 / 255.0,
                    g: 0x66 as f32 / 255.0,
                    b: 0x99 as f32 / 255.0,
                    a: 0x80 as f32 / 255.0,
                }),
                Blend::Normal
            )]
        );
    }

    /// One value stays valid beside a list, first layer on top; `{ fill = .., blend = .. }` is a
    /// layer, a layer table takes no other key, and 16 is the ceiling.
    #[test]
    fn background_takes_a_list_of_layers() {
        use crate::layout::node::prop::Keyword;
        let lua = mlua::Lua::new();
        let read = |src: &str| {
            let table: mlua::Table =
                lua.load(format!("return {{ kind = 'rect', background = {src} }}")).eval().unwrap();
            fields::paint::background.read(&props_from_table(&table))
        };
        let colour = |hex: &str| (Fill::Color(parse_hex_color("background", hex).unwrap()), Blend::Normal);
        let grey = "{ gradient = 'radial', stops = { { 0, '#000000' }, { 1, '#ffffff' } } }";
        assert_eq!(read("'#112233'").unwrap(), [colour("#112233")]);
        assert_eq!(read(&format!("{{ '#112233', {{ fill = '#445566' }}, {grey} }}")).unwrap().len(), 3);
        assert_eq!(read("{ { fill = '#445566' } }").unwrap(), [colour("#445566")]);
        assert_eq!(read("{ '#112233', '#445566' }").unwrap(), [colour("#112233"), colour("#445566")]);
        assert!(matches!(read(grey).unwrap().as_slice(), [(Fill::Gradient(_), _)]), "a gradient table is not a list");
        assert!(read("{}").unwrap().is_empty());
        assert!(read(&format!("{{ {} }}", vec!["'#112233'"; 16].join(","))).is_ok());
        let err = read(&format!("{{ {} }}", vec!["'#112233'"; 17].join(","))).unwrap_err();
        assert!(err.to_string().contains("at most 16"), "{err}");
        let (multiply, _) = colour("#112233");
        assert_eq!(read("{ { fill = '#112233', blend = 'multiply' } }").unwrap(), [(multiply, Blend::Multiply)]);
        for (at, mode) in Blend::NAMES.iter().enumerate() {
            let layer = read(&format!("{{ {{ fill = '#112233', blend = '{mode}' }} }}")).unwrap();
            assert_eq!(layer[0].1, Blend::VALUES[at], "{mode}");
        }
        assert_eq!(Blend::NAMES.len(), 18);
        assert!(Blend::NAMES.contains(&"plus_lighter") && Blend::NAMES.contains(&"color_dodge"));
        for bad in ["'x'", "'plus-lighter'", "1"] {
            let err = read(&format!("{{ {{ fill = '#112233', blend = {bad} }} }}")).unwrap_err();
            assert!(err.to_string().contains("background[1]") && err.to_string().contains("blend"), "{err}");
        }
        let err = read("{ { fill = '#112233', mode = 'multiply' } }").unwrap_err();
        assert!(err.to_string().contains("background[1]"), "{err}");
        assert!(read("{ '#112233', 5 }").unwrap_err().to_string().contains("background[2]"));
    }

    #[test]
    fn background_without_a_leading_hash_is_rejected_naming_the_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", background = "336699" }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::background.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("must start with `#`")),
            "must name the missing `#`, not just some invalid-property error: {err}"
        );
    }

    #[test]
    fn background_with_the_wrong_digit_count_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#369" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::background.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 3")),
            "must be the digit-count rule specifically, naming 3 digits: {err}"
        );
    }

    #[test]
    fn background_with_non_hex_characters_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#zzzzzz" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::background.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("only hex digits")),
            "must be the hex-digit rule specifically, not the digit-count rule: {err}"
        );
    }

    #[test]
    fn a_non_ascii_colour_string_gets_the_hex_digit_diagnosis_not_a_byte_count() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#日本語" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::background.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("only hex digits") && !detail.contains("got 9")),
            "non-ASCII input must get the hex-digit diagnosis, not a byte-length count: {err}"
        );
    }

    #[test]
    fn background_wrong_type_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", background = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::background.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("expected a hex colour or a gradient table")),
            "{err}"
        );
    }

    #[test]
    fn uppercase_hex_parses_the_same_as_lowercase() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#FF0000" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::background.read(&props).unwrap(),
            vec![(Fill::Color(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }), Blend::Normal)]
        );
    }

    #[test]
    fn a_seven_digit_hex_is_rejected_naming_the_digit_count() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#1234567" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::background.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 7")),
            "{err}"
        );
    }

    #[test]
    fn a_bare_hash_is_rejected_for_wrong_digit_count() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::background.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "background" && detail.contains("6 or 8") && detail.contains("got 0")),
            "{err}"
        );
    }

    /// Absent `clip` cuts only a surface or a scroll viewport; a declared one is kept as written.
    #[test]
    fn clip_absent_cuts_only_a_surface_or_a_scroll_viewport() {
        let lua = signal_lua();
        let clip = |kind: &str, fields: &str| {
            let src = format!(r#"return {{ kind = "{kind}", {fields} }}"#);
            let table: mlua::Table = lua.load(&src).eval().unwrap();
            ClipShape::of(kind, &deserialize_lua_table(&table).unwrap().properties).unwrap()
        };
        for kind in ["rect", "row", "column", "list"] {
            assert_eq!(clip(kind, ""), ClipShape::None, "{kind}");
        }
        for kind in ["row", "column", "list"] {
            assert_eq!(clip(kind, r#"scroll = scroll("s")"#), ClipShape::Box, "{kind}");
        }
        assert_eq!(clip("row", r#"scroll = scroll("s"), clip = "none""#), ClipShape::None);
        for kind in ["panel", "window", "popup", "lock"] {
            assert_eq!(clip(kind, ""), ClipShape::Box, "{kind}");
        }
        assert_eq!(clip("rect", r#"clip = "rounded""#), ClipShape::Rounded);
    }

    #[test]
    fn clip_reads_every_shape() {
        for (declared, expected) in
            [("box", ClipShape::Box), ("rounded", ClipShape::Rounded), ("none", ClipShape::None)]
        {
            let lua = mlua::Lua::new();
            let src = format!(r#"return {{ kind = "rect", clip = "{declared}" }}"#);
            let table: mlua::Table = lua.load(&src).eval().unwrap();
            let props = deserialize_lua_table(&table).unwrap().properties;
            assert_eq!(fields::paint::clip.read(&props).unwrap(), expected, "clip = {declared:?}");
        }
    }

    /// The whole point of a named shape over a boolean: `clip = true` would have to mean something,
    /// and the two shapes are not on/off, a node clips either way.
    #[test]
    fn an_unknown_clip_shape_is_rejected_naming_both() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", clip = "Rounded" }"#).eval().unwrap();
        let props = deserialize_lua_table(&table).unwrap().properties;
        let err = fields::paint::clip.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "clip" && detail.contains("`box`, `rounded`")),
            "got {err:?}"
        );

        let table: mlua::Table = lua.load(r#"return { kind = "rect", clip = true }"#).eval().unwrap();
        let props = deserialize_lua_table(&table).unwrap().properties;
        assert!(
            matches!(fields::paint::clip.read(&props).unwrap_err(), LayoutError::InvalidProperty { property, .. } if property == "clip")
        );
    }

    #[test]
    fn radius_absent_defaults_to_zero() {
        let props = PropMap::default();
        assert_eq!(parse_radius(&props).unwrap(), 0.0.into());
    }

    #[test]
    fn radius_reads_the_number() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = 6 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_radius(&props).unwrap(), Radii::from(6.0));
    }

    #[test]
    fn radius_reads_a_table_per_corner_and_scoop_negates_it() {
        let lua = Lua::new();
        let radii = |extra: &str| {
            let src = format!("return {{ kind = 'rect', radius = {{ top_left = 4, bottom_right = 8 }}, {extra} }}");
            let table: mlua::Table = lua.load(&src).eval().unwrap();
            parse_radius(&deserialize_lua_table(&table).unwrap().properties).unwrap()
        };
        assert_eq!(radii(""), Radii([4.0, 0.0, 8.0, 0.0], 0.0, None));
        assert_eq!(radii("corner_shape = 'scoop'"), Radii([-4.0, 0.0, -8.0, 0.0], 0.0, None));
        assert_eq!(Radii([20.0, 20.0, 0.0, 0.0], 0.0, None).fit(30.0, 100.0), Radii([15.0, 15.0, 0.0, 0.0], 0.0, None));
        assert_eq!(Radii::from(4.0).fit(-1.0, 10.0), Radii::default(), "a negative side never flips an arc");
        for bad in ["{ top_left = -1 }", "{ top_left = 0/0 }", "{ nope = 1 }"] {
            let src = format!("return {{ kind = 'rect', radius = {bad} }}");
            let table: mlua::Table = lua.load(&src).eval().unwrap();
            assert!(parse_radius(&deserialize_lua_table(&table).unwrap().properties).is_err(), "{bad}");
        }
    }

    /// One smoothing value rides every corner of the table; a scoop refuses it, naming both.
    #[test]
    fn corner_smoothing_rides_the_radii_and_a_scoop_refuses_it() {
        let lua = Lua::new();
        let parse = |extra: &str| {
            let src = format!("return {{ kind = 'rect', radius = {{ top_left = 4, bottom_right = 8 }}, {extra} }}");
            let table: mlua::Table = lua.load(&src).eval().unwrap();
            parse_radius(&deserialize_lua_table(&table).unwrap().properties)
        };
        assert_eq!(parse("corner_smoothing = 0.6").unwrap(), Radii([4.0, 0.0, 8.0, 0.0], 0.6, None));
        assert_eq!(parse("corner_smoothing = 0").unwrap(), parse("").unwrap());
        let err = parse("corner_shape = 'scoop', corner_smoothing = 0.5").unwrap_err().to_string();
        assert!(err.contains("corner_smoothing") && err.contains("scoop"), "{err}");
        assert!(parse("corner_shape = 'scoop', corner_smoothing = 0").is_ok());
        for bad in ["-0.1", "1.5", "0/0"] {
            let err = parse(&format!("corner_smoothing = {bad}")).unwrap_err().to_string();
            assert!(err.contains("corner_smoothing"), "{bad}: {err}");
        }
    }

    #[test]
    fn radius_wrong_type_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("expected a number")),
            "{err}"
        );
    }

    #[test]
    fn a_negative_radius_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = -4 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn radius_above_8192_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = 8193 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = parse_radius(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "radius" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_absent_defaults_to_all_zero() {
        let props = PropMap::default();
        assert_eq!(fields::paint::border_width.read(&props).unwrap(), EdgeInsets::default());
    }

    #[test]
    fn border_width_scalar_broadcasts_to_all_four_edges() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = 3 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::border_width.read(&props).unwrap(),
            EdgeInsets { top: 3.0, right: 3.0, bottom: 3.0, left: 3.0 }
        );
    }

    #[test]
    fn border_width_table_sets_edges_independently() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", border_width = { top = 2, left = 5 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::border_width.read(&props).unwrap(),
            EdgeInsets { top: 2.0, right: 0.0, bottom: 0.0, left: 5.0 }
        );
    }

    #[test]
    fn border_width_wrong_type_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::border_width.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("expected a number or a table")),
            "{err}"
        );
    }

    #[test]
    fn a_negative_border_width_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = -4 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::border_width.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_above_8192_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = 8193 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::border_width.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn border_width_table_form_out_of_range_edge_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_width = { top = 8193 } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::border_width.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_width" && detail.contains("[0, 8192]")),
            "must be the range rule, naming the bound: {err}"
        );
    }

    #[test]
    fn a_signal_nested_in_a_margin_edge_table_resolves_and_a_raising_one_names_the_edge() {
        let lua = signal_lua();
        let props = rect_props(&lua, "return { margin = { top = state('top', 4), left = 2 } }");
        let resolved = crate::layout::node::resolve_declared(props, "rect", false, &lua).unwrap();
        assert_eq!(
            fields::common::margin.read(&resolved).unwrap(),
            fields::common::margin.read(&rect_props(&lua, "return { margin = { top = 4, left = 2 } }")).unwrap()
        );
        let props =
            rect_props(&lua, "return { margin = { top = state('boom', 0):map(function() error('boom') end) } }");
        let err = crate::layout::node::resolve_declared(props, "rect", false, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "margin.top"), "{err:?}");
    }

    #[test]
    fn border_color_absent_is_all_none() {
        let props = PropMap::default();
        assert_eq!(fields::paint::border_color.read(&props).unwrap(), BorderPaint::default());
    }

    #[test]
    fn border_color_scalar_string_broadcasts_to_all_four_edges() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", border_color = "#ff0000" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let red = Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 });
        assert_eq!(
            fields::paint::border_color.read(&props).unwrap(),
            BorderPaint::Edges(BorderColor { top: red, right: red, bottom: red, left: red })
        );
    }

    #[test]
    fn border_color_table_sets_edges_independently_leaving_absent_edges_none() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua
            .load(r##"return { kind = "rect", border_color = { top = "#ff0000", left = "#00ff00" } }"##)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::border_color.read(&props).unwrap(),
            BorderPaint::Edges(BorderColor {
                top: Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
                right: None,
                bottom: None,
                left: Some(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            })
        );
    }

    #[test]
    fn border_color_takes_a_gradient_table_and_refuses_one_per_edge() {
        let lua = mlua::Lua::new();
        let read = |src: &str| {
            let table: mlua::Table =
                lua.load(format!("return {{ kind = \"rect\", border_color = {src} }}")).eval().unwrap();
            fields::paint::border_color.read(&props_from_table(&table))
        };
        let stops = r##"stops = { { 0, "#ff0000" }, { 1, "#0000ff" } }"##;
        let Ok(BorderPaint::Gradient(gradient)) = read(&format!(r#"{{ gradient = "conic", angle = 90, {stops} }}"#))
        else {
            panic!("a gradient table is a gradient border")
        };
        assert_eq!(gradient.shape, GradientShape::Conic { angle: 90.0 });
        assert_eq!(gradient.stops.len(), 2);
        for bad in [
            format!(r#"{{ top = {{ gradient = "linear", {stops} }} }}"#),
            format!(r##"{{ gradient = "linear", top = "#ff0000", {stops} }}"##),
        ] {
            let err = read(&bad).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "border_color"),
                "{bad}: {err}"
            );
        }
        let err = read(r##"{ gradient = "linear", stops = { { 0, "#ff0000" } } }"##).unwrap_err();
        assert!(err.to_string().contains("at least two"), "{err}");
    }

    #[test]
    fn border_color_malformed_hex_in_a_table_is_rejected_naming_the_edge() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", border_color = { top = "not-a-color" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::border_color.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("top") && detail.contains("must start with `#`")),
            "must name the failing edge, not just `border_color`: {err}"
        );
    }

    #[test]
    fn a_malformed_hex_on_a_non_top_edge_names_that_edge() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "rect", border_color = { right = "not-a-color" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::border_color.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("right")),
            "must name `right`, the edge that actually failed: {err}"
        );
    }

    #[test]
    fn a_signal_nested_in_a_border_color_edge_table_resolves() {
        let lua = signal_lua();
        let props = rect_props(&lua, r##"return { border_color = { top = state("red", "#ff0000") } }"##);
        let resolved = crate::layout::node::resolve_declared(props, "rect", false, &lua).unwrap();
        let plain = rect_props(&lua, r##"return { border_color = { top = "#ff0000" } }"##);
        assert_eq!(
            fields::paint::border_color.read(&resolved).unwrap(),
            fields::paint::border_color.read(&plain).unwrap()
        );
    }

    #[test]
    fn border_color_wrong_type_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", border_color = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::paint::border_color.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "border_color" && detail.contains("expected a string or a table")),
            "{err}"
        );
    }

    /// ADR-0336. `effect.shader` needs an absolute `source`, defaults `input` and `padding`, reads
    /// `params` as a `shader` node does, and refuses what it does not know.
    #[test]
    fn effect_shader_parses_its_keys_and_refuses_the_rest() {
        let lua = Lua::new();
        let parse = |src: &str| parse_effect(&rect_props(&lua, src));
        let shader = |padding| EffectShader {
            source: "/s.frag".into(),
            params: vec![("a".into(), vec![2.0]), ("b".into(), vec![1.0, 2.0])],
            images: vec![("map".into(), "/m.png".into())],
            padding,
            progress: -0.5,
        };
        let full = r#"return { effect = { shader = { source = "/s.frag", input = "content", padding = 12, progress = -0.5,
            params = { a = 2, b = { 1, 2 } }, images = { map = "/m.png" } } } }"#;
        assert_eq!(parse(full).unwrap(), Effect { shader: Some(shader(12.0)), ..Effect::default() });
        let bare = parse(r#"return { effect = { shader = { source = "/s.frag" } } }"#).unwrap();
        let shader = bare.shader.as_ref().unwrap();
        assert_eq!((shader.padding, shader.params.len(), shader.progress), (0.0, 0, 0.0));
        assert!(bare.layers(), "a shader alone needs the offscreen");
        // A backdrop shader draws before the node, from a copy, and needs no offscreen of its own.
        let under =
            parse(r#"return { effect = { shader = { source = "/s.frag", input = "backdrop", padding = 4 } } }"#);
        let under = under.unwrap();
        assert_eq!((under.shader.is_none(), under.backdrop_shader.as_ref().map(|s| s.padding)), (true, Some(4.0)));
        assert!(!under.layers());
        for (src, property) in [
            (r#"return { effect = { shader = { source = "/s.frag", input = "behind" } } }"#, "effect.shader"),
            (r#"return { blend = "plus-lighter" }"#, "blend"),
            (r#"return { blend = 1 }"#, "blend"),
            (r#"return { shadows = { { blur = 1, blend = "x" } } }"#, "shadows[1]"),
            (r#"return { effect = { shader = { source = "s.frag" } } }"#, "effect.shader.source"),
            (r#"return { effect = { shader = { source = "" } } }"#, "effect.shader.source"),
            (r#"return { effect = { shader = {} } }"#, "effect.shader"),
            (r#"return { effect = { shader = "/s.frag" } }"#, "effect"),
            (r#"return { effect = { shader = { source = "/s.frag", glow = 1 } } }"#, "effect.shader"),
            (r#"return { effect = { shader = { source = "/s.frag", padding = -1 } } }"#, "effect.shader.padding"),
            (r#"return { effect = { shader = { source = "/s.frag", padding = 513 } } }"#, "effect.shader.padding"),
            (r#"return { effect = { shader = { source = "/s.frag", progress = 8193 } } }"#, "effect.shader.progress"),
            (r#"return { effect = { shader = { source = "/s.frag", progress = -8193 } } }"#, "effect.shader.progress"),
            (
                r#"return { effect = { shader = { source = "/s.frag", params = { a = "x" } } } }"#,
                "effect.shader.params.a",
            ),
            (
                r#"return { effect = { shader = { source = "/s.frag", images = { u_map = "/m.png" } } } }"#,
                "effect.shader.images.u_map",
            ),
            (
                r#"return { effect = { shader = { source = "/s.frag", images = { map = "m.png" } } } }"#,
                "effect.shader.images.map",
            ),
        ] {
            let err = parse(src).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: got, .. } if got == property),
                "{src}: {err:?}"
            );
        }
    }

    /// Qt's `MultiEffect` defaults: a shadow is opaque black until coloured, and is absent until
    /// a blur, an offset or a spread would show it.
    #[test]
    fn a_shadow_is_black_until_coloured_and_absent_until_it_would_show() {
        let lua = Lua::new();
        let parse = |src: &str| parse_effect(&rect_props(&lua, src));
        assert_eq!(parse("return {}").unwrap(), Effect::default());
        assert!(parse(r##"return { shadows = { { color = "#ff000080" } } }"##).unwrap().shadows.is_empty());
        let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let shadow =
            Shadow { color: black, blur: 8.0, offset: (0.0, -2.0), spread: -1.0, inset: false, blend: Blend::Normal };
        assert_eq!(
            parse("return { shadows = { { blur = 8, offset = { y = -2 }, spread = -1 } } }").unwrap().shadows,
            [shadow]
        );
        let screen = parse(r#"return { shadows = { { blur = 8, blend = "screen" } } }"#).unwrap().shadows;
        assert_eq!(screen[0].blend, Blend::Screen);
        let node = parse(r#"return { blend = "plus_darker" }"#).unwrap();
        assert_eq!(node, Effect { blend: Blend::PlusDarker, ..Effect::default() });
        assert!(node.layers(), "a blended node draws offscreen");
        // Each layer takes the defaults, keeps its order, and drops when it would not show.
        let layers = r##"return { shadows = { { blur = 8, offset = { y = -2 }, spread = -1 }, { color = "#ff0000" },
            { color = "#ff000000", blur = 4 }, { offset = { x = 3 } } } }"##;
        let below = Shadow { blur: 0.0, offset: (3.0, 0.0), spread: 0.0, ..shadow };
        assert_eq!(parse(layers).unwrap().shadows, [shadow, below]);
        assert_eq!(parse("return { effect = { blur = 3 } }").unwrap(), Effect { blur: 3.0, ..Effect::default() });
        assert_eq!(
            parse("return { effect = { backdrop = { blur = 8 } } }").unwrap(),
            Effect { backdrop: 8.0, ..Effect::default() }
        );
        let tone = |saturate, brightness, contrast| Tone { saturate, brightness, contrast };
        assert_eq!(
            parse("return { effect = { saturate = 2, contrast = 0 } }").unwrap(),
            Effect { tone: tone(2.0, 1.0, 0.0), ..Effect::default() }
        );
        assert_eq!(
            parse("return { effect = { backdrop = { saturate = 8, brightness = 0.5 } } }").unwrap(),
            Effect { backdrop_tone: tone(8.0, 0.5, 1.0), ..Effect::default() }
        );
        assert!(parse("return { effect = { saturate = 1, backdrop = { contrast = 1 } } }").unwrap().tone.is_identity());
        let content = Effect { content_shadow: true, ..Effect::default() };
        assert_eq!(parse(r#"return { shadow_mode = "content" }"#).unwrap(), content);
        assert_eq!(parse(r#"return { shadow_mode = "box" }"#).unwrap(), Effect::default());
        for (src, property) in [
            ("return { effect = { blur = -1 } }", "effect.blur"),
            ("return { effect = { backdrop = { blur = 8193 } } }", "effect.backdrop.blur"),
            ("return { effect = { saturate = -0.1 } }", "effect.saturate"),
            ("return { effect = { brightness = 8.5 } }", "effect.brightness"),
            ("return { effect = { backdrop = { contrast = 9 } } }", "effect.backdrop.contrast"),
            ("return { effect = { backdrop = { saturate = -1 } } }", "effect.backdrop.saturate"),
            ("return { effect = { backdrop = { brightness = 8.5 } } }", "effect.backdrop.brightness"),
            ("return { effect = { contrast = 0.5, hue = 2 } }", "effect"),
            ("return { effect = { blur = 1, glow = 2 } }", "effect"),
            ("return { effect = { backdrop = { blur = 1, glow = 2 } } }", "effect.backdrop"),
            ("return { effect = { backdrop_blur = 2 } }", "effect"),
            (r#"return { effect = { backdrop = { mask = { node = "m" } } } }"#, "effect.backdrop.mask"),
            ("return { effect = 3 }", "effect"),
            ("return { shadows = { { color = 3, blur = 1 } } }", "shadows[1]"),
            (r#"return { shadow_mode = "Drop" }"#, "shadow_mode"),
            ("return { shadows = { { blur = -1 } } }", "shadows"),
            ("return { shadows = { { blur = 1, glow = 2 } } }", "shadows[1]"),
            ("return { shadows = { {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} } }", "shadows"),
            (r#"return { shadow_mode = "content", shadows = { { blur = 4, inset = true } } }"#, "shadows"),
            ("return { shadows = { { blur = 4, inset = 1 } } }", "shadows[1]"),
        ] {
            let err = parse(src).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property),
                "{src}: {err:?}"
            );
        }
    }

    /// ADR-0331: `inset` splits a layer off `shadows`, keeping its order and the same defaults, and a
    /// node takes 16 layers, inset and outer together.
    #[test]
    fn an_inset_layer_is_split_from_the_outer_ones_and_the_cap_is_16() {
        let lua = Lua::new();
        let parse = |src: &str| parse_effect(&rect_props(&lua, src));
        let effect =
            parse("return { shadows = { { blur = 2 }, { blur = 4, inset = true }, { spread = 1, inset = true } } }")
                .unwrap();
        assert_eq!(effect.shadows.iter().map(|s| (s.blur, s.inset)).collect::<Vec<_>>(), [(2.0, false)]);
        assert_eq!(
            effect.inset.iter().map(|s| (s.blur, s.spread, s.inset)).collect::<Vec<_>>(),
            [(4.0, 0.0, true), (0.0, 1.0, true)]
        );
        let only = parse("return { shadows = { { blur = 4, inset = true } } }").unwrap();
        assert!(!only.layers(), "an inset layer opens no offscreen");
        let layers = |n: usize| format!("return {{ shadows = {{ {} }} }}", "{ blur = 1, inset = true },".repeat(n));
        assert_eq!(parse(&layers(16)).unwrap().inset.len(), 16);
        assert!(parse(&layers(17)).is_err());
    }

    /// Filter Effects `saturate()`: the matrix on `(192, 96, 64)` gives the spec's own numbers, `0`
    /// is the luma grey and `1` leaves the colour alone.
    #[test]
    fn the_saturate_matrix_is_the_filter_effects_one() {
        let apply = |s: f32, rgb: [f32; 3]| {
            let m = Tone { saturate: s, ..Tone::default() }.saturate_columns();
            std::array::from_fn::<f32, 3, _>(|row| (0..3).map(|col| m[col * 3 + row] * rgb[col]).sum())
        };
        let close = |got: [f32; 3], want: [f32; 3]| got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-3);
        let colour = [192.0, 96.0, 64.0];
        assert!(close(apply(2.0, colour), [269.856, 77.856, 13.856]), "{:?}", apply(2.0, colour));
        assert!(close(apply(0.0, colour), [114.144; 3]), "{:?}", apply(0.0, colour));
        assert!(close(apply(1.0, colour), colour));
        assert!(Tone::default().is_identity() && !Tone { contrast: 0.5, ..Tone::default() }.is_identity());
    }
}

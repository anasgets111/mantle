//! Box-model and paint-adjacent value types. `table_number` is shared by `toplevel`'s size hints,
//! popup offsets, and anchor rectangles.

use cursor_icon::CursorIcon;
use mlua::Value;

use super::prop::{keywords, within as row_within};
use super::*;
use crate::lua::luacats::lua_shape;

mod transform;
pub use transform::{Affine, Transform, apply_affine, invert_affine, parse_transform};

/// `"NN%"` (`^\d+(\.\d+)?%$`) as `SizeMode::Percent`. Not a confirmed spec syntax: the base
/// property table only documents integer/`"Fill"` for width/height, though `Percent(f32)` is
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

/// `width`/`height`: pixels within the row's range (ADR-0021), `"Fill"`, or `"NN%"`. The map is a
/// [`resolve_properties`] result, so absent covers both omission and a signal resolving to `nil`.
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
            if &*s.as_bytes() == b"Fill" {
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
                "expected a number, \"Fill\", or a \"NN%\" string (Content sizing has no literal -- omit the property instead), got {}",
                preview_for_error(value)
            ),
        ))
    }
}

/// Reads one numeric field from a table-valued property. `None` means absent; callers choose
/// whether that defaults to 0 or is required. Nested `Signal`s are refused and errors name
/// `{property}.{key}`.
pub(super) fn table_number(property: &str, table: &mlua::Table, key: &str) -> Result<Option<f32>, LayoutError> {
    match table_field(property, table, key)? {
        Value::Nil => Ok(None),
        other => value_as_f32(property, &other)?
            .ok_or_else(|| invalid(property, format!("`{key}` must be a number, got {}", preview_for_error(&other))))
            .map(Some),
    }
}

/// `margin`/`padding`/`border_width`: a number sets all four edges, a table each; an absent edge is
/// 0, and a row with a range bounds every edge. Parsed once per node per pass: `table.get` is
/// metamethod-aware, so every consumer reading it again would re-run `__index`, and two reads could
/// disagree about one child's margin. Those reads are plain Lua outside any signal, so
/// `LayoutPassBudget`, not ADR-0021's per-getter cap, bounds them.
pub(crate) struct Insets;

spelled!(Insets => format!("{}|{}", f32::lua(), EdgeInsets::lua()));

impl Prop for Insets {
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
            only_keys(property, table, EdgeInsets::KEYS)?;
            // An absent edge is 0; [`table_number`] rejects nested `Signal`s.
            let edge =
                |key: &str| -> Result<f32, LayoutError> { Ok(table_number(property, table, key)?.unwrap_or(0.0)) };
            EdgeInsets { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? }
        };
        for n in [insets.top, insets.right, insets.bottom, insets.left] {
            row_within(row, n)?;
        }
        Ok(insets)
    }
}

/// A box's fill: one colour, or a gradient across its box (ADR-0255).
#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
    Color(Rgba),
    Gradient(Gradient),
}

/// Colour stops across a box, shared by `background` and `mask` (ADR-0255). Positions are
/// ascending fractions of the gradient line, radius or turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    pub shape: GradientShape,
    pub stops: Vec<(f32, Rgba)>,
}

/// CSS's geometry: `angle` in degrees clockwise from the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GradientShape {
    /// Along `angle` through the centre, spanning the box so its corners take the end stops.
    Linear { angle: f32 },
    /// An ellipse from the centre out to the box's edges.
    Radial,
    /// Around the centre, starting at `angle`.
    Conic { angle: f32 },
}

/// `mask`: what multiplies the alpha of a node's paint and subtree (ADR-0255).
#[derive(Debug, Clone, PartialEq)]
pub struct Mask {
    pub source: MaskSource,
    /// Keep what the mask covers out instead of in: Qt's `invert`.
    pub invert: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MaskSource {
    Gradient(Gradient),
    /// An image file stretched over the box; only its alpha counts.
    Image(String),
}

spelled!(Fill => "Color|Gradient");

/// `background`. Absent is `None`, not transparent black: `fill_rect` skips it, while
/// `#RRGGBBAA` with `AA = 00` remains an explicit transparent fill.
impl Prop for Fill {
    type Out = Option<Fill>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<Fill>, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(None);
        };
        match value {
            Value::String(s) => Ok(Some(Fill::Color(parse_hex_color(property, &checked_string(property, s)?)?))),
            Value::Table(table) => {
                only_keys(property, table, &["gradient", "angle", "stops"])?;
                Ok(Some(Fill::Gradient(parse_gradient(property, table)?)))
            }
            _ => Err(invalid(
                property,
                format!("expected a hex colour or a gradient table, got {}", preview_for_error(value)),
            )),
        }
    }
}

spelled!(Mask => "Mask");

/// `mask = { gradient = ..., stops = ... }` or `mask = { source = path }`, either with `invert`.
impl Prop for Mask {
    type Out = Option<Mask>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<Mask>, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(None);
        };
        let Value::Table(table) = value else {
            return Err(invalid(property, format!("expected a table, got {}", preview_for_error(value))));
        };
        only_keys(property, table, &["gradient", "angle", "stops", "source", "invert"])?;
        let field = |key: &str| table_field(property, table, key);
        let invert = match field("invert")? {
            Value::Nil => false,
            Value::Boolean(b) => b,
            other => {
                return Err(invalid(
                    property,
                    format!("`invert` must be a boolean, got {}", preview_for_error(&other)),
                ));
            }
        };
        let gradient = ["gradient", "stops", "angle"].into_iter().map(field).collect::<Result<Vec<_>, _>>()?;
        let source = match (field("source")?, gradient.iter().all(Value::is_nil)) {
            (Value::Nil, false) => MaskSource::Gradient(parse_gradient(property, table)?),
            (Value::String(s), true) => {
                let path = checked_string(property, &s)?;
                if path.is_empty() {
                    return Err(invalid(property, "`source` must not be empty"));
                }
                MaskSource::Image(path)
            }
            (Value::Nil, true) | (Value::String(_), false) => {
                return Err(invalid(property, "name exactly one of `source` or a gradient"));
            }
            (other, _) => {
                let got = preview_for_error(&other);
                return Err(invalid(property, format!("`source` must be a path string, got {got}")));
            }
        };
        Ok(Some(Mask { source, invert }))
    }
}

/// One field of a config table; a nested signal is refused.
fn table_field(
    property: &str,
    table: &mlua::Table,
    key: impl mlua::IntoLua + std::fmt::Display + Copy,
) -> Result<Value, LayoutError> {
    match table.get(key).map_err(|e| invalid(property, e.to_string()))? {
        Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!("{property}.{key}"))),
        other => Ok(other),
    }
}

/// `{ gradient = "Linear"|"Radial"|"Conic", angle?, stops = { { position, colour }, ... } }`.
fn parse_gradient(property: &str, table: &mlua::Table) -> Result<Gradient, LayoutError> {
    let angle = table_number(property, table, "angle")?;
    let shape = match table_field(property, table, "gradient")? {
        Value::String(s) if s.as_bytes() == b"Linear" => GradientShape::Linear { angle: angle.unwrap_or(180.0) },
        Value::String(s) if s.as_bytes() == b"Conic" => GradientShape::Conic { angle: angle.unwrap_or(0.0) },
        Value::String(s) if s.as_bytes() == b"Radial" && angle.is_none() => GradientShape::Radial,
        Value::String(s) if s.as_bytes() == b"Radial" => {
            return Err(invalid(property, "a `Radial` gradient takes no `angle`"));
        }
        other => {
            let got = preview_for_error(&other);
            return Err(invalid(property, format!("`gradient` must be one of `Linear`, `Radial`, `Conic`, got {got}")));
        }
    };
    let Value::Table(list) = table_field(property, table, "stops")? else {
        return Err(invalid(property, "`stops` must be a list of { position, colour } pairs"));
    };
    let mut stops: Vec<(f32, Rgba)> = Vec::new();
    for stop in list.sequence_values::<Value>() {
        let stop = stop.map_err(|e| invalid(property, e.to_string()))?;
        let pair = match stop {
            Value::Table(pair) => (table_field(property, &pair, 1)?, table_field(property, &pair, 2)?),
            other => (other, Value::Nil),
        };
        let (Some(at), Value::String(color)) = (value_as_f32(property, &pair.0)?, &pair.1) else {
            return Err(invalid(property, "each stop must be a { position, colour } pair"));
        };
        if !(0.0..=1.0).contains(&at) {
            return Err(invalid(property, format!("stop positions must be within [0, 1], got {at}")));
        }
        if stops.last().is_some_and(|(last, _)| at < *last) {
            return Err(invalid(property, "stop positions must be ascending"));
        }
        stops.push((at, parse_hex_color(property, &checked_string(property, color)?)?));
    }
    if stops.len() < 2 {
        return Err(invalid(property, "a gradient needs at least two stops"));
    }
    Ok(Gradient { shape, stops })
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

/// `radius`, negated under `corner_shape = "Scoop"`.
pub fn parse_radius(properties: &PropMap) -> Result<f32, LayoutError> {
    let radius = fields::paint::radius.read(properties)?;
    Ok(if fields::paint::corner_shape.read(properties)? == CornerShape::Scoop { -radius } else { radius })
}

/// The range an overshooting easing is clamped into: the property table's `range`, else
/// `[0, 8192]`; `margin`, which no parser bounds, tweens through negatives as `translate` does.
/// The tween clamps every numeric property, while `spacing`, icon `size`, `margin` and `padding`
/// take no parser bound: out of range there is a layout the solver absorbs, not a crash, and
/// `snap_to_physical` bounds the coordinates that reach `wl_region`. Give a row a range only where a
/// consumer refuses the value.
///
/// `radius` and `border_width` share the `8192` ceiling with `width`/`height`. It is
/// femtovg 0.26's: above roughly 8.4e6 `curve_divisions` (`path/cache.rs:911`) divides by
/// `acos(1.0) == 0.0`, and `inf as u32` becomes `u32::MAX`, so billions of iterations and tens of
/// GB of vertices land on the Wayland dispatch thread. Below zero, `radius = -4` silently squares
/// corners (`path.rs:458` treats under 0.1 as unrounded) and `border_width = -4` clamps to 0 and
/// clears paint alpha.
///
/// `font_size` alone floors at 1. `line_height` is `font_size * 1.2` and cosmic-text's
/// `Buffer::new` asserts a non-zero line height, so a zero aborts the Renderer. Flooring in the
/// row rather than in a consumer covers the tween too, which clamps into this same range. Icon
/// `size` needs no floor: it becomes a `Measure::Square` and the painter takes its pixels from the
/// resolved box, so it never reaches a shaper.
pub(super) fn range_of(property: &str) -> (f32, f32) {
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
    only_keys(property, table, Axes::KEYS)?;
    let axis = |key| table_number(property, table, key).map(|n| n.unwrap_or(axis_default(property)));
    let Axes { x, y } = Axes { x: row_within(row, axis("x")?)?, y: row_within(row, axis("y")?)? };
    Ok((x, y))
}

// `translate`, `origin`, `shadow_offset`: a per-axis pair, read as `(x, y)`.
lua_shape! {
    /// A missing axis takes the property's default.
    #[alias = "Axes"]
    pub(crate) struct Axes {
        x?: f32,
        y?: f32,
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
            return Axes::read(row, None);
        };
        match value_as_f32(row.name, value)? {
            Some(n) => Ok((row_within(row, n)?, n)),
            None => xy(row, value),
        }
    }
}

keywords! {
    /// Whether a node clips children to its box or lets `radius` shape the clip. `Box` is the
    /// default because rounded clipping needs an offscreen target and composite, while a square
    /// clip is a free GPU scissor.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum ClipShape {
        /// The node's rectangle with square corners.
        #[default]
        Box,
        /// The node's rounded shape, using the same arc as its background fill.
        Rounded,
        /// Nothing: children keep the parent's clip, so a wrapper does not cut their shadows.
        None,
    }
}

// `None` means "not painted", the same absence [`Fill`] returns for a missing fill: an edge at
// width 0 needs no colour, and one with a colour at width 0 still paints nothing, so the drawing
// pass gets the same answer either way. The table form gives no per-edge default, so an absent edge
// takes `None` rather than an invented one.
lua_shape! {
    /// Per-edge colours; a signal inside is refused.
    #[alias = "BorderColors"]
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct BorderColor {
        pub top: Option<Rgba>,
        pub right: Option<Rgba>,
        pub bottom: Option<Rgba>,
        pub left: Option<Rgba>,
    }
}

/// `border_color`: one colour for every edge, or [`BorderColor`].
pub(crate) struct EdgeColors;

spelled!(EdgeColors => format!("{}|{}", Rgba::lua(), BorderColor::lua()));

impl Prop for EdgeColors {
    type Out = BorderColor;
    fn read(row: &Property, value: Option<&Value>) -> Result<BorderColor, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(BorderColor::default());
        };
        if let Value::String(s) = value {
            let color = Some(parse_hex_color(property, &checked_string(property, s)?)?);
            return Ok(BorderColor { top: color, right: color, bottom: color, left: color });
        }
        let Value::Table(table) = value else {
            return Err(invalid(property, format!("expected a string or a table, got {}", preview_for_error(value))));
        };
        only_keys(property, table, BorderColor::KEYS)?;
        // Metamethod-aware, but parsed once per node by `paint_style` (ADR-0068).
        let edge = |key: &str| -> Result<Option<Rgba>, LayoutError> {
            let v: Value = table.get(key).map_err(|e| invalid(property, e.to_string()))?;
            // Name the edge as well as the property; the shared string/color parsers only know the
            // property.
            let name_edge = |e: LayoutError| match e {
                LayoutError::InvalidProperty { property, detail } => {
                    LayoutError::InvalidProperty { property, detail: format!("`{key}`: {detail}") }
                }
                other => other,
            };
            match v {
                Value::Nil => Ok(None),
                // Reject nested signals rather than misreporting them as bad hex.
                Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!("{property}.{key}"))),
                Value::String(s) => {
                    let s = checked_string(property, &s).map_err(name_edge)?;
                    Ok(Some(parse_hex_color(property, &s).map_err(name_edge)?))
                }
                other => Err(invalid(
                    property,
                    format!("`{key}` must be a hex colour string, got {}", preview_for_error(&other)),
                )),
            }
        };
        Ok(BorderColor { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? })
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
}

/// What a node's own painted output is filtered by (ADR-0254). `blur` is `content_blur`, CSS
/// `filter: blur()`'s sigma; `backdrop` is `backdrop_blur`, `backdrop-filter: blur()`'s (ADR-0256).
/// `0` is off. `content_shadow` is `shadow_mode = "Content"`: the shadow is cast by the painted
/// subtree, not the box's shape (ADR-0260).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Effect {
    pub shadow: Option<Shadow>,
    pub blur: f32,
    pub backdrop: f32,
    pub content_shadow: bool,
}

keywords! {
    /// `shadow_mode`: CSS `box-shadow` of the box shape, or `drop-shadow` of everything painted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ShadowMode {
        Box,
        Content,
    }
}

/// `shadow_*` and `content_blur`, every kind, and a box's `backdrop_blur` and `shadow_mode`. `None` when the shadow
/// would draw nothing, so paint never opens an offscreen for it.
pub fn parse_effect(properties: &PropMap) -> Result<Effect, LayoutError> {
    use fields::{common, paint};
    let color = common::shadow_color.read(properties)?.expect("`shadow_color` has a default");
    let offset = common::shadow_offset.read(properties)?;
    let blur = common::shadow_blur.read(properties)?;
    let spread = common::shadow_spread.read(properties)?;
    let shows = color.a > 0.0 && (blur > 0.0 || spread != 0.0 || offset != (0.0, 0.0));
    Ok(Effect {
        shadow: shows.then_some(Shadow { color, blur, offset, spread }),
        blur: common::content_blur.read(properties)?,
        backdrop: paint::backdrop_blur.read(properties)?,
        content_shadow: paint::shadow_mode.read(properties)? == ShadowMode::Content,
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
        let table: mlua::Table = lua.load(r#"return { kind = "rect", width = "Fill" }"#).eval().unwrap();
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
        let table: mlua::Table = lua.load(r#"return { kind = "rect", height = "Content" }"#).eval().unwrap();
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
    fn padding_negative_value_is_accepted() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", padding = -10 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::common::padding.read(&props).unwrap(),
            EdgeInsets { top: -10.0, right: -10.0, bottom: -10.0, left: -10.0 }
        );
    }

    #[test]
    fn visible_absent_defaults_true() {
        let props = PropMap::default();
        assert!(fields::common::visible.read(&props).unwrap());
    }

    #[test]
    fn a_signal_userdata_in_a_geometry_slot_resolves_to_its_current_value() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal =
            crate::lua::signal::Signal::new_live(Value::Boolean(false), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("visible", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let resolved = resolve_properties(node.properties, "rect", &lua).unwrap();
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
        assert_eq!(fields::paint::background.read(&props).unwrap(), None);
    }

    #[test]
    fn background_six_digit_hex_is_opaque() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#336699" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::background.read(&props).unwrap(),
            Some(Fill::Color(Rgba { r: 0x33 as f32 / 255.0, g: 0x66 as f32 / 255.0, b: 0x99 as f32 / 255.0, a: 1.0 }))
        );
    }

    #[test]
    fn background_eight_digit_hex_carries_its_own_alpha() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", background = "#33669980" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::paint::background.read(&props).unwrap(),
            Some(Fill::Color(Rgba {
                r: 0x33 as f32 / 255.0,
                g: 0x66 as f32 / 255.0,
                b: 0x99 as f32 / 255.0,
                a: 0x80 as f32 / 255.0,
            }))
        );
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
            Some(Fill::Color(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }))
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

    const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    const CLEAR: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.0 };

    fn eval_props(lua: &mlua::Lua, lua_src: &str) -> PropMap {
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        props_from_table(&table)
    }

    fn rejects<T: std::fmt::Debug>(result: Result<T, LayoutError>, name: &str, needle: &str) {
        let err = result.unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == name && detail.contains(needle)),
            "expected `{name}` naming {needle:?}, got {err:?}"
        );
    }

    /// CSS's defaults: a linear gradient runs to the bottom, a conic one starts at the top.
    #[test]
    fn a_background_gradient_parses_each_shape_with_its_default_angle() {
        let lua = mlua::Lua::new();
        let stops = r##"stops = { { 0, "#ffffff" }, { 1, "#ffffff00" } }"##;
        for (shape, expected) in [
            ("Linear", GradientShape::Linear { angle: 180.0 }),
            ("Radial", GradientShape::Radial),
            ("Conic", GradientShape::Conic { angle: 0.0 }),
        ] {
            let src = format!(r#"return {{ kind = "rect", background = {{ gradient = "{shape}", {stops} }} }}"#);
            let Some(Fill::Gradient(gradient)) = fields::paint::background.read(&eval_props(&lua, &src)).unwrap()
            else {
                panic!("{shape}")
            };
            assert_eq!(gradient, Gradient { shape: expected, stops: vec![(0.0, WHITE), (1.0, CLEAR)] }, "{shape}");
        }
    }

    /// femtovg's stop texture stops walking at the first stop past `1`, and paints nothing between
    /// two stops that run backwards, so both are refused rather than drawn wrong.
    #[test]
    fn gradient_stops_must_be_two_or_more_ascending_positions_in_the_unit_range() {
        let lua = mlua::Lua::new();
        let with = |stops: &str| {
            eval_props(
                &lua,
                &format!(r#"return {{ kind = "rect", background = {{ gradient = "Linear", stops = {stops} }} }}"#),
            )
        };
        rejects(fields::paint::background.read(&with(r##"{ { 0, "#ffffff" } }"##)), "background", "at least two");
        rejects(
            fields::paint::background.read(&with(r##"{ { 0, "#ffffff" }, { 1.5, "#ffffff" } }"##)),
            "background",
            "[0, 1]",
        );
        rejects(
            fields::paint::background.read(&with(r##"{ { 0.6, "#ffffff" }, { 0.4, "#ffffff" } }"##)),
            "background",
            "ascending",
        );
        rejects(
            fields::paint::background.read(&with(r##"{ { 0, "white" }, { 1, "#ffffff" } }"##)),
            "background",
            "`#`",
        );
        rejects(
            fields::paint::background.read(&with(r##"{ "#ffffff", "#000000" }"##)),
            "background",
            "{ position, colour }",
        );
        rejects(fields::paint::background.read(&with("nil")), "background", "`stops`");
    }

    #[test]
    fn an_unknown_gradient_shape_or_a_radial_angle_is_refused() {
        let lua = mlua::Lua::new();
        let src = r##"return { kind = "rect", background = { gradient = "Box", stops = {} } }"##;
        rejects(fields::paint::background.read(&eval_props(&lua, src)), "background", "`Linear`, `Radial`, `Conic`");
        let src = r##"return { kind = "rect", background = { gradient = "Radial", angle = 45,
            stops = { { 0, "#ffffff" }, { 1, "#000000" } } } }"##;
        rejects(fields::paint::background.read(&eval_props(&lua, src)), "background", "angle");
    }

    /// One gradient shape for `background` and `mask`, so a fade is written the way a fill is.
    #[test]
    fn a_mask_is_a_gradient_or_an_image_source_either_inverted() {
        let lua = mlua::Lua::new();
        let src = r##"return { kind = "rect", mask = { gradient = "Linear",
            stops = { { 0, "#ffffff00" }, { 1, "#ffffff" } } } }"##;
        let gradient =
            Gradient { shape: GradientShape::Linear { angle: 180.0 }, stops: vec![(0.0, CLEAR), (1.0, WHITE)] };
        assert_eq!(
            fields::paint::mask.read(&eval_props(&lua, src)).unwrap(),
            Some(Mask { source: MaskSource::Gradient(gradient), invert: false })
        );

        let src = r#"return { kind = "rect", mask = { source = "/tmp/shape.svg", invert = true } }"#;
        assert_eq!(
            fields::paint::mask.read(&eval_props(&lua, src)).unwrap(),
            Some(Mask { source: MaskSource::Image("/tmp/shape.svg".into()), invert: true })
        );
    }

    #[test]
    fn a_mask_naming_both_sources_or_neither_is_refused() {
        let lua = mlua::Lua::new();
        rejects(
            fields::paint::mask.read(&eval_props(&lua, r#"return { kind = "rect", mask = "/tmp/a.png" }"#)),
            "mask",
            "table",
        );
        rejects(
            fields::paint::mask.read(&eval_props(&lua, r#"return { kind = "rect", mask = { invert = true } }"#)),
            "mask",
            "one of",
        );
        let both = r##"return { kind = "rect", mask = { source = "/a.png", gradient = "Radial",
            stops = { { 0, "#ffffff" }, { 1, "#000000" } } } }"##;
        rejects(fields::paint::mask.read(&eval_props(&lua, both)), "mask", "one of");
        let stray = r##"return { kind = "rect", mask = { source = "/a.png", stops = { { 0, "#ffffff" } } } }"##;
        rejects(fields::paint::mask.read(&eval_props(&lua, stray)), "mask", "one of");
        let stray = r#"return { kind = "rect", mask = { source = "/a.png", angle = 90 } }"#;
        rejects(fields::paint::mask.read(&eval_props(&lua, stray)), "mask", "one of");
        rejects(
            fields::paint::mask.read(&eval_props(&lua, r#"return { kind = "rect", mask = { source = "" } }"#)),
            "mask",
            "empty",
        );
        let src = r#"return { kind = "rect", mask = { source = "/a.png", invert = 1 } }"#;
        rejects(fields::paint::mask.read(&eval_props(&lua, src)), "mask", "invert");
    }

    #[test]
    fn clip_absent_defaults_to_the_nodes_box() {
        let props = PropMap::default();
        assert_eq!(fields::paint::clip.read(&props).unwrap(), ClipShape::Box);
    }

    #[test]
    fn clip_reads_every_shape() {
        for (declared, expected) in
            [("Box", ClipShape::Box), ("Rounded", ClipShape::Rounded), ("None", ClipShape::None)]
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
        let table: mlua::Table = lua.load(r#"return { kind = "rect", clip = "Circle" }"#).eval().unwrap();
        let props = deserialize_lua_table(&table).unwrap().properties;
        let err = fields::paint::clip.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "clip" && detail.contains("`Box`, `Rounded`")),
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
        assert_eq!(parse_radius(&props).unwrap(), 0.0);
    }

    #[test]
    fn radius_reads_the_number() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", radius = 6 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_radius(&props).unwrap(), 6.0);
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
    fn a_signal_nested_in_a_margin_edge_table_is_rejected_naming_the_edge() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Integer(4), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        let margin = lua.create_table().unwrap();
        margin.set("top", signal).unwrap();
        table.set("margin", margin).unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            fields::common::margin.read(&props).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "margin.top"
        ));
    }

    #[test]
    fn border_color_absent_is_all_none() {
        let props = PropMap::default();
        assert_eq!(fields::paint::border_color.read(&props).unwrap(), BorderColor::default());
    }

    #[test]
    fn border_color_scalar_string_broadcasts_to_all_four_edges() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "rect", border_color = "#ff0000" }"##).eval().unwrap();
        let props = props_from_table(&table);
        let red = Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 });
        assert_eq!(
            fields::paint::border_color.read(&props).unwrap(),
            BorderColor { top: red, right: red, bottom: red, left: red }
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
            BorderColor {
                top: Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
                right: None,
                bottom: None,
                left: Some(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            }
        );
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
    fn a_signal_nested_in_a_border_color_edge_table_is_rejected_naming_the_edge() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let hex = lua.create_string("#ff0000").unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(hex), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        let border_color = lua.create_table().unwrap();
        border_color.set("top", signal).unwrap();
        table.set("border_color", border_color).unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            fields::paint::border_color.read(&props).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "border_color.top"
        ));
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

    /// Qt's `MultiEffect` defaults: a shadow is opaque black until coloured, and is absent until
    /// a blur, an offset or a spread would show it.
    #[test]
    fn a_shadow_is_black_until_coloured_and_absent_until_it_would_show() {
        let lua = Lua::new();
        let parse = |src: &str| parse_effect(&rect_props(&lua, src));
        assert_eq!(parse("return {}").unwrap(), Effect::default());
        assert_eq!(parse(r##"return { shadow_color = "#ff000080" }"##).unwrap().shadow, None);
        let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        assert_eq!(
            parse("return { shadow_blur = 8, shadow_offset = { y = -2 }, shadow_spread = -1 }").unwrap().shadow,
            Some(Shadow { color: black, blur: 8.0, offset: (0.0, -2.0), spread: -1.0 })
        );
        assert_eq!(parse("return { content_blur = 3 }").unwrap(), Effect { blur: 3.0, ..Effect::default() });
        assert_eq!(parse("return { backdrop_blur = 8 }").unwrap(), Effect { backdrop: 8.0, ..Effect::default() });
        let content = Effect { content_shadow: true, ..Effect::default() };
        assert_eq!(parse(r#"return { shadow_mode = "Content" }"#).unwrap(), content);
        assert_eq!(parse(r#"return { shadow_mode = "Box" }"#).unwrap(), Effect::default());
        for (src, property) in [
            ("return { content_blur = -1 }", "content_blur"),
            ("return { backdrop_blur = -1 }", "backdrop_blur"),
            ("return { shadow_blur = -1 }", "shadow_blur"),
            ("return { shadow_color = 3, shadow_blur = 1 }", "shadow_color"),
            (r#"return { shadow_mode = "Drop" }"#, "shadow_mode"),
        ] {
            let err = parse(src).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property),
                "{src}: {err:?}"
            );
        }
    }
}

//! Box-model and paint-adjacent value types.

use cursor_icon::CursorIcon;
use mlua::Value;

use super::prop::{keywords, within as row_within};
use super::*;
use crate::lua::luacats::lua_shape;

mod fill;
#[cfg(test)]
pub(crate) use fill::GradientStop;
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
        Ok(Radii(radii))
    }
}

/// `radius`, negated under `corner_shape = "scoop"`.
pub fn parse_radius(properties: &PropMap) -> Result<Radii, LayoutError> {
    let radius = fields::paint::radius.read(properties)?;
    Ok(if fields::paint::corner_shape.read(properties)? == CornerShape::Scoop { radius * -1.0 } else { radius })
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

// `translate`, `origin`, `shadow_offset`: a per-axis pair, which `xy` reads as `(x, y)`.
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

/// `border_color`: one colour for every edge, or [`BorderColor`].
pub(crate) struct ColorOrEdges;

spelled!(ColorOrEdges => format!("{}|{}", Rgba::lua(), BorderColor::lua()));

impl Prop for ColorOrEdges {
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
        BorderColor::read(property, table)
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
/// `0` is off. `content_shadow` is `shadow_mode = "content"`: the shadow is cast by the painted
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

    #[test]
    fn clip_absent_defaults_to_the_nodes_box() {
        let props = PropMap::default();
        assert_eq!(fields::paint::clip.read(&props).unwrap(), ClipShape::Box);
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
        assert_eq!(radii(""), Radii([4.0, 0.0, 8.0, 0.0]));
        assert_eq!(radii("corner_shape = 'scoop'"), Radii([-4.0, 0.0, -8.0, 0.0]));
        assert_eq!(Radii([20.0, 20.0, 0.0, 0.0]).fit(30.0, 100.0), Radii([15.0, 15.0, 0.0, 0.0]));
        assert_eq!(Radii::from(4.0).fit(-1.0, 10.0), Radii::default(), "a negative side never flips an arc");
        for bad in ["{ top_left = -1 }", "{ top_left = 0/0 }", "{ nope = 1 }"] {
            let src = format!("return {{ kind = 'rect', radius = {bad} }}");
            let table: mlua::Table = lua.load(&src).eval().unwrap();
            assert!(parse_radius(&deserialize_lua_table(&table).unwrap().properties).is_err(), "{bad}");
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
        let resolved = crate::layout::node::resolve_properties(props, "rect", &lua).unwrap();
        assert_eq!(
            fields::common::margin.read(&resolved).unwrap(),
            fields::common::margin.read(&rect_props(&lua, "return { margin = { top = 4, left = 2 } }")).unwrap()
        );
        let props =
            rect_props(&lua, "return { margin = { top = state('boom', 0):map(function() error('boom') end) } }");
        let err = crate::layout::node::resolve_properties(props, "rect", &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "margin.top"), "{err:?}");
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
    fn a_signal_nested_in_a_border_color_edge_table_resolves() {
        let lua = signal_lua();
        let props = rect_props(&lua, r##"return { border_color = { top = state("red", "#ff0000") } }"##);
        let resolved = crate::layout::node::resolve_properties(props, "rect", &lua).unwrap();
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
        assert_eq!(parse(r#"return { shadow_mode = "content" }"#).unwrap(), content);
        assert_eq!(parse(r#"return { shadow_mode = "box" }"#).unwrap(), Effect::default());
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

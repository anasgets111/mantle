//! Box-model and paint-adjacent value types.

use cursor_icon::CursorIcon;
use mlua::Value;

use super::prop::{keywords, within as row_within, within_range, within_value};
use super::*;
use crate::lua::luacats::lua_shape;

mod fill;
#[cfg(test)]
pub(crate) use fill::GradientStop;
pub(crate) use fill::{Background, BackgroundLayer, MAX_BACKGROUNDS, is_layer_list, layer_fill};
pub use fill::{Fill, Gradient, GradientShape, Mask, MaskSource};

mod effect;
#[cfg(test)]
pub(crate) use effect::{BackdropKeys, ShaderKeys, ShadowLayer};
pub use effect::{Blend, Effect, EffectShader, Shadow, Tone, parse_effect};
pub(crate) use effect::{EffectKeys, Effects, ShadowMode, Shadows};
pub(super) use effect::{SHADOW_BLUR, SHADOW_REACH, TONE};

mod transform;
pub use transform::{
    Affine, IDENTITY_AFFINE, Transform, apply_affine, compose_affine, invert_affine, parse_transform,
    transformed_bounds,
};

/// `"NN%"` (`^\d+(\.\d+)?%$`) as `SizeMode::Percent` (ADR-0023).
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
            return Ok(SizeMode::Pixels(within_value(row, n, value)?));
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
    if let Some(outline) = fields::paint::shape.read(properties)? {
        if !radius.is_zero() || smoothing > 0.0 || scoop {
            return Err(invalid("shape", "replaces radius, corner_shape and corner_smoothing; round it with corner"));
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
            Some(n) => Ok((within_value(row, n, value)?, n)),
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
            let color = Some(lua_hex_color(property, s)?);
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

lua_shape! {
    /// A stroke around a box, `offset` px outside its edge (inside when negative), CSS `outline` with `outline-offset`.
    #[alias = "Ring"]
    struct RingInput {
        width: f32,
        color: Rgba,
        offset: Option<f32>,
    }
}

/// `ring`, `offset` defaulted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ring {
    pub width: f32,
    pub color: Rgba,
    pub offset: f32,
}

impl LuaType for Ring {
    fn lua() -> String {
        "Ring".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        RingInput::classes(out);
    }
}

/// `ring`: `None` when absent.
impl Prop for Ring {
    type Out = Option<Ring>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let table = match value {
            None => return Ok(None),
            Some(Value::Table(table)) => table,
            Some(value) => {
                return Err(invalid(row.name, format!("expected a table, got {}", preview_for_error(value))));
            }
        };
        let RingInput { width, color, offset } = RingInput::read(row.name, table)?;
        Ok(Some(Ring {
            width: within_range(row.name, SHADOW_BLUR, width)?,
            color,
            offset: within_range(row.name, SHADOW_REACH, offset.unwrap_or(0.0))?,
        }))
    }
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
    fn width_reads_each_spelling_and_refuses_the_rest() {
        let lua = mlua::Lua::new();
        for (field, expected) in [
            ("", Some(SizeMode::Content)),
            ("width = 32", Some(SizeMode::Pixels(32.0))),
            ("width = 'fill'", Some(SizeMode::Fill)),
            ("width = '50%'", Some(SizeMode::Percent(0.5))),
            ("width = 8193", None),
            ("width = -5", None),
            ("width = 'banana'", None),
        ] {
            let props = props_from_src(&lua, format!("return {{ kind = 'rect', {field} }}"));
            match (fields::common::width.read(&props), expected) {
                (Ok(got), Some(want)) => assert_eq!(got, want, "{field}"),
                (Err(LayoutError::InvalidProperty { .. }), None) => {}
                (got, _) => panic!("{field}: {got:?}"),
            }
        }
    }

    #[test]
    fn absent_style_properties_take_their_defaults() {
        let props = PropMap::default();
        assert!(fields::common::visible.read(&props).unwrap());
        assert!(fields::paint::background.read(&props).unwrap().is_empty());
        assert_eq!(parse_radius(&props).unwrap(), 0.0.into());
        assert_eq!(fields::paint::border_width.read(&props).unwrap(), EdgeInsets::default());
        assert_eq!(fields::paint::border_color.read(&props).unwrap(), BorderPaint::default());
    }

    #[test]
    fn height_content_error_names_omission_as_the_spelling() {
        let lua = mlua::Lua::new();
        let props = props_from_src(&lua, r#"return { kind = "rect", height = "content" }"#);
        let err = fields::common::height.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "height" && detail.contains("omit the property")),
            "must name omission as how Content sizing is spelled: {err}"
        );
    }

    #[test]
    fn box_insets_read_scalar_and_edge_table() {
        let lua = mlua::Lua::new();
        for (name, field) in [("margin", &fields::common::margin), ("padding", &fields::common::padding)] {
            let read = |v: &str| field.read(&props_from_src(&lua, format!("return {{ kind = 'rect', {name} = {v} }}")));
            assert_eq!(
                read("{ top = 4, left = 2 }").unwrap(),
                EdgeInsets { top: 4.0, right: 0.0, bottom: 0.0, left: 2.0 },
                "{name}"
            );
            assert_eq!(read("10").unwrap(), EdgeInsets { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 }, "{name}");
            if name == "padding" {
                for value in ["-10", "{ left = -10 }"] {
                    assert!(
                        matches!(read(value), Err(LayoutError::InvalidProperty { property, .. }) if property == "padding"),
                        "padding {value}: negative is invalid"
                    );
                }
            } else {
                let negative = EdgeInsets { top: -10.0, right: -10.0, bottom: -10.0, left: -10.0 };
                assert_eq!(read("-10").unwrap(), negative, "a negative margin is accepted");
            }
        }
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
        let resolved = resolve_declared(&node.properties, "rect", false, &lua).unwrap();
        assert!(
            !fields::common::visible.read(&resolved).unwrap(),
            "must read the signal's current value, not error on the handle"
        );
    }

    #[test]
    fn spacing_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        let lua = mlua::Lua::new();
        let props = props_from_src(&lua, r#"return { kind = "row", spacing = 1e300 }"#);
        assert!(matches!(
            fields::flow::spacing.read(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "spacing"
        ));
    }

    #[test]
    fn background_hex_forms_parse() {
        let lua = mlua::Lua::new();
        let byte = |b: u8| b as f32 / 255.0;
        for (hex, expected) in [
            ("#336699", Rgba { r: byte(0x33), g: byte(0x66), b: byte(0x99), a: 1.0 }),
            ("#33669980", Rgba { r: byte(0x33), g: byte(0x66), b: byte(0x99), a: byte(0x80) }),
            ("#FF0000", Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
        ] {
            let props = props_from_src(&lua, format!("return {{ kind = 'rect', background = '{hex}' }}"));
            assert_eq!(
                fields::paint::background.read(&props).unwrap(),
                [(Fill::Color(expected), Blend::Normal)],
                "{hex}"
            );
        }
    }

    #[test]
    fn background_colour_rejections_name_the_rule() {
        let lua = mlua::Lua::new();
        for (value, needles, must_not) in [
            (r##""336699""##, &["must start with `#`"][..], ""),
            (r##""#369""##, &["6 or 8", "got 3"], ""),
            (r##""#zzzzzz""##, &["only hex digits"], ""),
            (r##""#日本語""##, &["only hex digits"], "got 9"),
            ("true", &["expected a hex colour or a gradient table"], ""),
            (r##""#1234567""##, &["6 or 8", "got 7"], ""),
            (r##""#""##, &["6 or 8", "got 0"], ""),
        ] {
            let props = props_from_src(&lua, format!("return {{ kind = 'rect', background = {value} }}"));
            let err = fields::paint::background.read(&props).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, detail, .. }
                    if property == "background" && needles.iter().all(|n| detail.contains(n)) && (must_not.is_empty() || !detail.contains(must_not))),
                "{value}: {err}"
            );
        }
    }

    /// One value stays valid beside a list, first layer on top; `{ fill = .., blend = .. }` is a
    /// layer, a layer table takes no other key, and 16 is the ceiling.
    #[test]
    fn background_takes_a_list_of_layers() {
        use crate::layout::node::prop::Keyword;
        let lua = mlua::Lua::new();
        let read = |src: &str| {
            fields::paint::background
                .read(&props_from_src(&lua, format!("return {{ kind = 'rect', background = {src} }}")))
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

    /// Absent `clip` cuts only a surface or a scroll viewport; a declared one is kept as written.
    #[test]
    fn clip_absent_cuts_only_a_surface_or_a_scroll_viewport() {
        let lua = signal_lua();
        let clip = |kind: &str, fields: &str| {
            let src = format!(r#"return {{ kind = "{kind}", {fields} }}"#);
            ClipShape::of(kind, &props_from_src(&lua, src)).unwrap()
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
        let lua = mlua::Lua::new();
        for (declared, expected) in
            [("box", ClipShape::Box), ("rounded", ClipShape::Rounded), ("none", ClipShape::None)]
        {
            let props = props_from_src(&lua, format!(r#"return {{ kind = "rect", clip = "{declared}" }}"#));
            assert_eq!(fields::paint::clip.read(&props).unwrap(), expected, "clip = {declared:?}");
        }
    }

    /// The whole point of a named shape over a boolean: `clip = true` would have to mean something,
    /// and the two shapes are not on/off, a node clips either way.
    #[test]
    fn an_unknown_clip_shape_is_rejected_naming_both() {
        let lua = mlua::Lua::new();
        let err =
            fields::paint::clip.read(&props_from_src(&lua, r#"return { kind = "rect", clip = "oval" }"#)).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "clip" && detail.contains("`box`, `rounded`")),
            "got {err:?}"
        );
        let err =
            fields::paint::clip.read(&props_from_src(&lua, r#"return { kind = "rect", clip = true }"#)).unwrap_err();
        assert!(matches!(err, LayoutError::InvalidProperty { property, .. } if property == "clip"));
    }

    #[test]
    fn radius_out_of_range_is_rejected() {
        let lua = mlua::Lua::new();
        for value in ["-4", "8193"] {
            let err = parse_radius(&props_from_src(&lua, format!("return {{ kind = 'rect', radius = {value} }}")))
                .unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "radius" && detail.contains("[0, 8192]")),
                "{value}: must be the range rule, naming the bound: {err}"
            );
        }
    }

    #[test]
    fn radius_reads_a_table_per_corner_and_scoop_negates_it() {
        let lua = Lua::new();
        let radii = |extra: &str| {
            let src = format!("return {{ kind = 'rect', radius = {{ top_left = 4, bottom_right = 8 }}, {extra} }}");
            parse_radius(&props_from_src(&lua, src)).unwrap()
        };
        assert_eq!(radii(""), Radii([4.0, 0.0, 8.0, 0.0], 0.0, None));
        assert_eq!(radii("corner_shape = 'scoop'"), Radii([-4.0, 0.0, -8.0, 0.0], 0.0, None));
        assert_eq!(Radii([20.0, 20.0, 0.0, 0.0], 0.0, None).fit(30.0, 100.0), Radii([15.0, 15.0, 0.0, 0.0], 0.0, None));
        assert_eq!(Radii::from(4.0).fit(-1.0, 10.0), Radii::default(), "a negative side never flips an arc");
        for bad in ["{ top_left = -1 }", "{ top_left = 0/0 }", "{ nope = 1 }"] {
            let src = format!("return {{ kind = 'rect', radius = {bad} }}");
            assert!(parse_radius(&props_from_src(&lua, src)).is_err(), "{bad}");
        }
    }

    /// One smoothing value rides every corner of the table; a scoop refuses it, naming both.
    #[test]
    fn corner_smoothing_rides_the_radii_and_a_scoop_refuses_it() {
        let lua = Lua::new();
        let parse = |extra: &str| {
            let src = format!("return {{ kind = 'rect', radius = {{ top_left = 4, bottom_right = 8 }}, {extra} }}");
            parse_radius(&props_from_src(&lua, src))
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
    fn border_width_takes_a_scalar_for_every_edge_or_a_table_per_edge() {
        let lua = mlua::Lua::new();
        let read = |value: &str| {
            fields::paint::border_width
                .read(&props_from_src(&lua, format!("return {{ kind = 'rect', border_width = {value} }}")))
        };
        assert_eq!(read("3").unwrap(), EdgeInsets { top: 3.0, right: 3.0, bottom: 3.0, left: 3.0 });
        assert_eq!(read("{ top = 2, left = 5 }").unwrap(), EdgeInsets { top: 2.0, right: 0.0, bottom: 0.0, left: 5.0 });
    }

    #[test]
    fn a_bad_border_width_is_refused_naming_the_rule() {
        let lua = mlua::Lua::new();
        for (value, rule) in [
            ("-4", "[0, 8192]"),
            ("8193", "[0, 8192]"),
            ("{ top = 8193 }", "[0, 8192]"),
            ("true", "expected a number or a table"),
        ] {
            let props = props_from_src(&lua, format!("return {{ kind = 'rect', border_width = {value} }}"));
            let err = fields::paint::border_width.read(&props).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "border_width" && detail.contains(rule)),
                "{value}: {err}"
            );
        }
    }

    #[test]
    fn a_signal_nested_in_a_margin_edge_table_resolves_and_a_raising_one_names_the_edge() {
        let lua = signal_lua();
        let props = rect_props(&lua, "return { margin = { top = state('top', 4), left = 2 } }");
        let resolved = crate::layout::node::resolve_declared(&props, "rect", false, &lua).unwrap();
        assert_eq!(
            fields::common::margin.read(&resolved).unwrap(),
            fields::common::margin.read(&rect_props(&lua, "return { margin = { top = 4, left = 2 } }")).unwrap()
        );
        let props =
            rect_props(&lua, "return { margin = { top = state('boom', 0):map(function() error('boom') end) } }");
        let err = crate::layout::node::resolve_declared(&props, "rect", false, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "margin.top"), "{err:?}");
    }

    #[test]
    fn border_color_takes_a_string_for_every_edge_or_a_table_leaving_absent_edges_none() {
        let lua = mlua::Lua::new();
        let read = |value: &str| {
            fields::paint::border_color
                .read(&props_from_src(&lua, format!("return {{ kind = 'rect', border_color = {value} }}")))
        };
        let (red, green) =
            (Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }), Some(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }));
        assert_eq!(
            read(r##""#ff0000""##).unwrap(),
            BorderPaint::Edges(BorderColor { top: red, right: red, bottom: red, left: red })
        );
        assert_eq!(
            read(r##"{ top = "#ff0000", left = "#00ff00" }"##).unwrap(),
            BorderPaint::Edges(BorderColor { top: red, right: None, bottom: None, left: green })
        );
    }

    #[test]
    fn border_color_takes_a_gradient_table_and_refuses_one_per_edge() {
        let lua = mlua::Lua::new();
        let read = |src: &str| {
            fields::paint::border_color
                .read(&props_from_src(&lua, format!("return {{ kind = \"rect\", border_color = {src} }}")))
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
    fn a_bad_border_color_is_refused_naming_the_edge_that_failed() {
        let lua = mlua::Lua::new();
        for (value, details) in [
            (r#"{ top = "not-a-color" }"#, ["top", "must start with `#`"]),
            (r#"{ right = "not-a-color" }"#, ["right", "must start with `#`"]),
            ("true", ["expected a string or a table", ""]),
        ] {
            let props = props_from_src(&lua, format!("return {{ kind = 'rect', border_color = {value} }}"));
            let err = fields::paint::border_color.read(&props).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "border_color" && details.iter().all(|d| detail.contains(d))),
                "{value}: {err}"
            );
        }
    }

    #[test]
    fn a_signal_nested_in_a_border_color_edge_table_resolves() {
        let lua = signal_lua();
        let props = rect_props(&lua, r##"return { border_color = { top = state("red", "#ff0000") } }"##);
        let resolved = crate::layout::node::resolve_declared(&props, "rect", false, &lua).unwrap();
        let plain = rect_props(&lua, r##"return { border_color = { top = "#ff0000" } }"##);
        assert_eq!(
            fields::paint::border_color.read(&resolved).unwrap(),
            fields::paint::border_color.read(&plain).unwrap()
        );
    }
}

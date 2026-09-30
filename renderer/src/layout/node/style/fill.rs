//! Colour and image fills: Lua input shapes, validation and resolved paint values.

use mlua::Value;

use crate::layout::node::prop::{Prop, keywords};
use crate::layout::node::{LayoutError, Property, Rgba, checked_string, invalid, parse_hex_color, preview_for_error};
use crate::lua::luacats::{LuaType, lua_shape, spelled};

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
    pub stops: Vec<GradientStop>,
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

pub(crate) type GradientStop = (f32, Rgba);

#[cfg(test)]
struct StopAlias;
#[cfg(test)]
spelled!(StopAlias => "GradientStop");

keywords! {
    #[derive(Clone, Copy)]
    enum GradientKind { Linear, Radial, Conic }
}

lua_shape! {
    #[alias = "Gradient"]
    struct GradientInput {
        gradient: GradientKind,
        angle: Option<f32>,
        stops: Vec<GradientStop> as Vec<StopAlias>,
    }
}

lua_shape! {
    #[alias = "Mask"]
    struct MaskInput {
        gradient: Option<GradientKind>,
        angle: Option<f32>,
        stops: Option<Vec<GradientStop>> as Option<Vec<StopAlias>>,
        source: Option<String>,
        invert: Option<bool>,
    }
}

impl GradientInput {
    fn into_gradient(self, property: &str) -> Result<Gradient, LayoutError> {
        let shape = match (self.gradient, self.angle) {
            (GradientKind::Linear, angle) => GradientShape::Linear { angle: angle.unwrap_or(180.0) },
            (GradientKind::Conic, angle) => GradientShape::Conic { angle: angle.unwrap_or(0.0) },
            (GradientKind::Radial, None) => GradientShape::Radial,
            (GradientKind::Radial, Some(_)) => return Err(invalid(property, "a `Radial` gradient takes no `angle`")),
        };
        if let Some((at, _)) = self.stops.iter().find(|(at, _)| !(0.0..=1.0).contains(at)) {
            return Err(invalid(property, format!("stop positions must be within [0, 1], got {at}")));
        }
        if self.stops.windows(2).any(|pair| pair[1].0 < pair[0].0) {
            return Err(invalid(property, "stop positions must be ascending"));
        }
        if self.stops.len() < 2 {
            return Err(invalid(property, "a gradient needs at least two stops"));
        }
        Ok(Gradient { shape, stops: self.stops })
    }
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
                Ok(Some(Fill::Gradient(GradientInput::read(property, table)?.into_gradient(property)?)))
            }
            _ => Err(invalid(
                property,
                format!("expected a hex colour or a gradient table, got {}", preview_for_error(value)),
            )),
        }
    }
}

impl LuaType for Gradient {
    fn lua() -> String {
        "Gradient".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        GradientInput::classes(out);
    }
}

impl LuaType for Mask {
    fn lua() -> String {
        "Mask".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        MaskInput::classes(out);
    }
}

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
        let MaskInput { gradient, angle, stops, source, invert } = MaskInput::read(property, table)?;
        let any_gradient = gradient.is_some() || angle.is_some() || stops.is_some();
        let source = match (source, any_gradient) {
            (Some(_), true) | (None, false) => {
                return Err(invalid(property, "name exactly one of `source` or a gradient"));
            }
            (Some(path), false) if path.is_empty() => return Err(invalid(property, "`source` must not be empty")),
            (Some(path), false) => MaskSource::Image(path),
            (None, true) => {
                let (Some(gradient), Some(stops)) = (gradient, stops) else {
                    return Err(invalid(property, "a mask gradient needs both `gradient` and `stops`"));
                };
                MaskSource::Gradient(GradientInput { gradient, angle, stops }.into_gradient(property)?)
            }
        };
        Ok(Some(Mask { source, invert: invert.unwrap_or(false) }))
    }
}

#[cfg(test)]
mod tests {
    use super::{Fill, Gradient, GradientShape, Mask, MaskSource};
    use crate::layout::node::{LayoutError, PropMap, Rgba, fields, props_from_table};
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
            "`stops[1]` must be [number, Color]",
        );
        rejects(fields::paint::background.read(&with("nil")), "background", "`stops`");
    }

    #[test]
    fn an_unknown_gradient_shape_or_a_radial_angle_is_refused() {
        let lua = mlua::Lua::new();
        let src = r#"return { kind = "rect", background = { stops = {} } }"#;
        rejects(
            fields::paint::background.read(&eval_props(&lua, src)),
            "background",
            r#"`gradient` must be "Linear"|"Radial"|"Conic", got Nil"#,
        );
        let src = r##"return { kind = "rect", background = { gradient = "Box", stops = {} } }"##;
        rejects(fields::paint::background.read(&eval_props(&lua, src)), "background", r#""Linear"|"Radial"|"Conic""#);
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

    /// A signal binds a whole property, never a key inside one, however deep.
    #[test]
    fn a_signal_nested_in_a_gradient_is_refused_by_its_path() {
        let lua = crate::layout::node::signal_lua();
        let src = r##"return { kind = "rect", background = { gradient = "Linear",
            stops = { { 0, state("#ffffff") }, { 1, "#000000" } } } }"##;
        let err = fields::paint::background.read(&eval_props(&lua, src)).unwrap_err();
        assert!(
            matches!(&err, LayoutError::UnsupportedSignalProperty(path) if path == "background.stops[1][2]"),
            "{err:?}"
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
        let src = r#"return { kind = "rect", mask = { source = false, gradient = "Radial" } }"#;
        rejects(fields::paint::mask.read(&eval_props(&lua, src)), "mask", "`source` must be string");
    }
}

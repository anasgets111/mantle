//! `Animatable`: values a tween can sit between, told apart by shape.

use std::rc::Rc;

use mlua::{Lua, Value};

use crate::layout::node::prop::{Keyword, Prop};
use crate::layout::node::style::{
    BackgroundLayer, MAX_BACKGROUNDS, SHADOW_BLUR, SHADOW_REACH, TONE, axis_default, parse_percent, range_of,
};
use crate::layout::node::{
    Axes, Blend, CornersInput, EdgeInsets, EdgesInput, EffectKeys, Effects, LayoutError, PathCommands, PathData, Rgba,
    Ring, Shadow, Shadows, Shape, fields, input, invalid, is_layer_list, layer_fill, lua_hex_color, parse_hex_color,
    tweened, value_as_f32,
};
use crate::lua::luacats::spelled;

/// The one thing a hex colour has to look like to reach `parse_hex_color` again next pass.
fn hex_of(color: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", byte(color.r), byte(color.g), byte(color.b), byte(color.a))
}

/// A channel as the byte a hex colour holds.
pub(crate) fn byte(channel: f32) -> u8 {
    (channel.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// `a` to `b` at `t`, each channel kept in `[0, 1]`.
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let blend = |x: f32, y: f32| (x + (y - x) * t).clamp(0.0, 1.0);
    Rgba { r: blend(a.r, b.r), g: blend(a.g, b.g), b: blend(a.b, b.b), a: blend(a.a, b.a) }
}

/// `shadow` at zero alpha: an unset layer paints nothing, without a second hue to cross.
fn faded(shadow: &Shadow) -> Shadow {
    Shadow { color: clear(shadow.color), ..*shadow }
}

fn clear(color: Rgba) -> Rgba {
    Rgba { a: 0.0, ..color }
}

/// `layer` at zero alpha: an unset colour paints nothing, and a gradient has no fade to give.
fn faded_layer(layer: &Layer) -> Layer {
    match layer {
        Layer::Color(color, blend) => Layer::Color(clear(*color), *blend),
        snap => snap.clone(),
    }
}

/// A value a tween can sit between, told apart by shape rather than by which property holds it.
/// `Percent` is a `"NN%"` size held as a fraction; `Fields` is a table of numbers under one of
/// three key sets, the edges `{ top, right, bottom, left }`, the corners `{ top_left, .. }` or the
/// axes `{ x, y }`, an absent key reading as the property's default (`0`, or `1` for a `scale`).
/// `Path` is a path's `commands`, which tween point by point only between lists of the same ops
/// and hole flags. `Shadows` is a `shadows` list, tweened layer by layer. `Layers` is a `background`
/// list: colour layers mix, any other layer snaps to the target's, and a layer one side lacks fades. `Effect` is an `effect`'s
/// `[blur, saturate, brightness, contrast]`, the same four of its `backdrop` and its `shader.progress`,
/// a missing key reading as off: `0` for a blur, `1` for a colour filter; the rest of its `shader`
/// table and its `backdrop.mask` are carried as the target has them and never tweened (ADR-0336). Two different shapes
/// snap, so a fill that switches between `"45%"` and `"fill"` or a margin that switches between a
/// number and a table takes the new value at once.
#[derive(Debug, Clone, PartialEq)]
pub enum Animatable {
    Number(f32),
    Percent(f32),
    Color(Rgba),
    Fields { keys: &'static [&'static str], values: [f32; 4] },
    Path(Rc<PathData>),
    Shape(Rc<Shape>),
    Shadows(Vec<Shadow>),
    Ring(Ring),
    Layers(Vec<Layer>),
    Effect([f32; 9], Kept),
}

/// What an `effect` holds that no tween moves. Equal when the shaders are: a mask table the config
/// builds anew each pass must not restart the tween.
#[derive(Debug, Clone, Default)]
pub struct Kept {
    shader: Option<Value>,
    mask: Option<Value>,
}

impl PartialEq for Kept {
    fn eq(&self, other: &Self) -> bool {
        self.shader == other.shader
    }
}

/// One `background` layer in a tween: a colour and its blend (which snaps), or a gradient carried
/// as written (it snaps).
#[derive(Debug, Clone, PartialEq)]
pub enum Layer {
    Color(Rgba, Blend),
    Snap(Value),
}

/// An `effect` with every filter off: blurs `0`, colour filters `1`, `shader.progress` `0`.
const EFFECT_OFF: [f32; 9] = [0.0, 1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 0.0];

spelled!(Animatable => format!(
    "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
    f32::lua(),
    String::lua(),
    EdgeInsets::lua(),
    CornersInput::lua(),
    Axes::lua(),
    EffectKeys::lua(),
    PathCommands::lua(),
    Shape::lua(),
    Shadows::lua(),
    Ring::lua(),
    Vec::<BackgroundLayer>::lua()
));

impl Animatable {
    /// `property`'s value when it is not set, in this value's shape: `1` for `opacity`, `trim_end`
    /// and `scale`, `0` otherwise, per axis or edge for a table. A percent or a colour has no identity
    /// to speak of and stays where it is.
    pub(super) fn identity(&self, property: &str) -> Self {
        match *self {
            // No empty drawing has this one's ops to tween from.
            Self::Path(_) | Self::Shape(_) => self.clone(),
            Self::Number(_) => {
                Self::Number(if matches!(property, "opacity" | "trim_end") { 1.0 } else { axis_default(property) })
            }
            Self::Fields { keys, .. } => Self::Fields { keys, values: [axis_default(property); 4] },
            Self::Shadows(ref layers) => Self::Shadows(layers.iter().map(faded).collect()),
            Self::Ring(ring) => Self::Ring(Ring { width: 0.0, color: clear(ring.color), ..ring }),
            Self::Layers(ref layers) => Self::Layers(layers.iter().map(faded_layer).collect()),
            Self::Effect(..) => Self::Effect(EFFECT_OFF, Kept::default()),
            // An unset size is nothing, and an unset colour paints nothing, which is that colour
            // at zero alpha rather than a second hue to cross on the way out.
            Self::Percent(_) => Self::Percent(0.0),
            Self::Color(colour) => Self::Color(clear(colour)),
        }
    }

    /// The typed reading of `property`'s current value, or `None` when the value is a shape no
    /// tween carries (`"fill"`, a boolean, a table of colours, absent): the caller snaps then. A
    /// `#` string that fails its colour parse is an error, the same one the property's own parser
    /// raises.
    pub(crate) fn from_value(property: &str, value: Option<&Value>) -> Result<Option<Self>, LayoutError> {
        let Some(value) = value else { return Ok(None) };
        // By name: a command array has no shape of its own that a table of edges or axes lacks.
        if property == "commands" {
            return Ok(Some(Self::Path(PathCommands::read(&fields::path::commands.row, Some(value))?)));
        }
        if property == "shape" {
            return Ok(Shape::read(&fields::paint::shape.row, Some(value))?.map(Self::Shape));
        }
        if property == "effect" {
            let keys = Effects::read(&fields::common::effect.row, Some(value))?;
            let b = keys.backdrop.unwrap_or_default();
            let given = [
                keys.blur,
                keys.saturate,
                keys.brightness,
                keys.contrast,
                b.blur,
                b.saturate,
                b.brightness,
                b.contrast,
                keys.shader.as_ref().and_then(|shader| shader.progress),
            ];
            let get = |table: &mlua::Table, key| table.get::<Value>(key).ok().filter(|v| !v.is_nil());
            let table = value.as_table();
            let kept = Kept {
                shader: table.and_then(|table| get(table, "shader")),
                mask: table
                    .and_then(|table| get(table, "backdrop"))
                    .and_then(|backdrop| backdrop.as_table().and_then(|table| get(table, "mask"))),
            };
            return Ok(Some(Self::Effect(std::array::from_fn(|i| given[i].unwrap_or(EFFECT_OFF[i])), kept)));
        }
        if property == "shadows" {
            return Ok(Shadows::read(&fields::common::shadows.row, Some(value))?.map(Self::Shadows));
        }
        if property == "ring" {
            return Ok(<Ring as Prop>::read(&fields::paint::ring.row, Some(value))?.map(Self::Ring));
        }
        if property == "background" {
            // A lone colour or gradient is a one-layer list, so it mixes with a list on the other side.
            let lone = match value {
                Value::String(s) if s.as_bytes().starts_with(b"#") => {
                    Some(Layer::Color(lua_hex_color(property, s)?, Blend::Normal))
                }
                Value::Table(table) if !is_layer_list(table) => Some(Layer::Snap(value.clone())),
                _ => None,
            };
            if let Some(layer) = lone {
                return Ok(Some(Self::Layers(vec![layer])));
            }
        }
        if property == "background"
            && let Value::Table(table) = value
        {
            let len = input::array_len(property, table, MAX_BACKGROUNDS)?;
            let layers = (1..=len).map(|i| {
                let layer: Value = table.raw_get(i).map_err(|e| invalid(property, e.to_string()))?;
                let name = format!("{property}[{i}]");
                let (fill, blend) = match layer.as_table().and_then(|t| layer_fill(t).map(|fill| (t, fill))) {
                    Some((t, fill)) => (fill, input::field::<Option<Blend>>(&name, t, "blend")?.unwrap_or_default()),
                    None => (layer.clone(), Blend::Normal),
                };
                match &fill {
                    Value::String(s) if s.as_bytes().starts_with(b"#") => {
                        Ok(Layer::Color(lua_hex_color(&name, s)?, blend))
                    }
                    _ => Ok(Layer::Snap(layer)),
                }
            });
            return Ok(Some(Self::Layers(layers.collect::<Result<_, LayoutError>>()?)));
        }
        match value {
            Value::String(s) => {
                let s = s.to_str().map_err(|e| invalid(property, e.to_string()))?;
                if s.starts_with('#') {
                    return Ok(Some(Self::Color(parse_hex_color(property, &s)?)));
                }
                Ok(parse_percent(&s).map(Self::Percent))
            }
            Value::Table(table) => {
                let has = |key: &str| table.contains_key(key).unwrap_or(false);
                let keys = [Axes::KEYS, CornersInput::KEYS]
                    .into_iter()
                    .find(|keys| keys.iter().any(|key| has(key)))
                    .unwrap_or(EdgesInput::KEYS);
                // Any other key makes it another shape, such as a gradient (ADR-0255).
                let known =
                    |key: &Value| matches!(key, Value::String(s) if keys.iter().any(|k| s.as_bytes() == k.as_bytes()));
                if table.pairs::<Value, Value>().any(|pair| pair.map_or(true, |(key, _)| !known(&key))) {
                    return Ok(None);
                }
                let mut values = [axis_default(property); 4];
                for (slot, key) in values.iter_mut().zip(keys) {
                    let field: Value = table.get(*key).map_err(|e| invalid(property, e.to_string()))?;
                    if field.is_nil() {
                        continue;
                    }
                    let Some(n) = value_as_f32(property, &field)? else { return Ok(None) };
                    *slot = n;
                }
                Ok(Some(Self::Fields { keys, values }))
            }
            _ => Ok(value_as_f32(property, value)?.map(Self::Number)),
        }
    }

    /// This value minus `to`, component by component, zero-padded to a fixed width so the four
    /// shapes compare as one vector. Two shapes that cannot mix have no displacement between them
    /// and answer zero, which is the same thing [`Self::lerp`] does with such a pair: snap.
    pub(super) fn delta(&self, to: &Self) -> [f32; 4] {
        let mut out = [0.0; 4];
        match (self, to) {
            (Self::Number(a), Self::Number(b)) | (Self::Percent(a), Self::Percent(b)) => out[0] = a - b,
            (Self::Fields { keys, values: a }, Self::Fields { keys: other, values: b }) if keys == other => {
                for ((slot, x), y) in out.iter_mut().zip(a).zip(b) {
                    *slot = x - y;
                }
            }
            // The two levels share four slots, the larger displacement of each pair standing for it.
            (Self::Effect(a, _), Self::Effect(b, _)) => {
                for (i, (x, y)) in a.iter().zip(b).enumerate() {
                    if (x - y).abs() > out[i % 4].abs() {
                        out[i % 4] = x - y;
                    }
                }
            }
            (Self::Color(a), Self::Color(b)) => out = [a.r - b.r, a.g - b.g, a.b - b.b, a.a - b.a],
            // ponytail: a path, shadow or background list gives a spring no velocity; per-point rates would carry it.
            _ => {}
        }
        out
    }

    pub(super) fn lerp(&self, to: &Self, t: f32, property: &str) -> Self {
        match (self, to) {
            (Self::Number(a), Self::Number(b)) => {
                let (low, high) = range_of(property);
                Self::Number((a + (b - a) * t).clamp(low, high))
            }
            (Self::Percent(a), Self::Percent(b)) => Self::Percent((a + (b - a) * t).max(0.0)),
            (Self::Fields { keys, values: a }, Self::Fields { keys: other, values: b }) if keys == other => {
                let (low, high) = range_of(property);
                let mut values = [0.0; 4];
                for ((slot, x), y) in values.iter_mut().zip(a).zip(b) {
                    *slot = (x + (y - x) * t).clamp(low, high);
                }
                Self::Fields { keys, values }
            }
            (Self::Effect(a, _), Self::Effect(b, kept)) => Self::Effect(
                std::array::from_fn(|i| {
                    let (lo, hi) = match i {
                        8 => range_of("progress"),
                        _ if i % 4 == 0 => SHADOW_BLUR,
                        _ => TONE,
                    };
                    (a[i] + (b[i] - a[i]) * t).clamp(lo, hi)
                }),
                kept.clone(),
            ),
            (Self::Color(a), Self::Color(b)) => Self::Color(mix(*a, *b, t)),
            // The shorter list pads with the other's layers faded out, as `identity` fades them.
            (Self::Shadows(a), Self::Shadows(b)) => {
                let at = |list: &[Shadow], other: &[Shadow], i: usize| {
                    list.get(i).copied().unwrap_or_else(|| faded(&other[i]))
                };
                let clamp = |n: f32, (low, high): (f32, f32)| n.clamp(low, high);
                let layers = (0..a.len().max(b.len())).map(|i| {
                    let (x, y) = (at(a, b, i), at(b, a, i));
                    let lerp = |p: f32, q: f32| p + (q - p) * t;
                    let offset = |p: f32, q: f32| clamp(lerp(p, q), SHADOW_REACH);
                    // ponytail: an outer and an inset layer do not blend; the target's shows at once.
                    if x.inset != y.inset {
                        return y;
                    }
                    Shadow {
                        inset: y.inset,
                        blend: y.blend,
                        color: mix(x.color, y.color, t),
                        blur: clamp(lerp(x.blur, y.blur), SHADOW_BLUR),
                        offset: (offset(x.offset.0, y.offset.0), offset(x.offset.1, y.offset.1)),
                        spread: clamp(lerp(x.spread, y.spread), SHADOW_REACH),
                    }
                });
                Self::Shadows(layers.collect())
            }
            (Self::Ring(a), Self::Ring(b)) => {
                let lerp = |p: f32, q: f32| p + (q - p) * t;
                Self::Ring(Ring {
                    width: lerp(a.width, b.width).clamp(SHADOW_BLUR.0, SHADOW_BLUR.1),
                    color: mix(a.color, b.color, t),
                    offset: lerp(a.offset, b.offset).clamp(SHADOW_REACH.0, SHADOW_REACH.1),
                })
            }
            (Self::Layers(a), Self::Layers(b)) => {
                let layers = (0..a.len().max(b.len())).filter_map(|i| match (a.get(i), b.get(i)) {
                    // The blend snaps to the target's.
                    (Some(Layer::Color(x, _)), Some(Layer::Color(y, b))) => Some(Layer::Color(mix(*x, *y, t), *b)),
                    (Some(Layer::Color(x, b)), None) => Some(Layer::Color(mix(*x, clear(*x), t), *b)),
                    (None, Some(Layer::Color(y, b))) => Some(Layer::Color(mix(clear(*y), *y, t), *b)),
                    // A snapping layer, or a colour that meets one, takes the target's at once; one the target lacks is gone.
                    (_, y) => y.cloned(),
                });
                Self::Layers(layers.collect())
            }
            (Self::Path(a), Self::Path(b)) => a.lerp(b, t).map_or_else(|| to.clone(), |path| Self::Path(Rc::new(path))),
            (Self::Shape(a), Self::Shape(b)) => a.lerp(b, t).map_or_else(|| to.clone(), |o| Self::Shape(Rc::new(o))),
            // The two shapes come from the same property, so this pair cannot be mixed; snap to
            // the target rather than guess if it ever is.
            _ => to.clone(),
        }
    }

    /// The value written back into a resolved property map for the parsers to read.
    pub(super) fn to_value(&self, lua: &Lua) -> mlua::Result<Value> {
        Ok(match *self {
            Self::Number(n) => Value::Number(f64::from(n)),
            // Three decimals: enough that a 147ms tween over a 6px meter never repeats a frame,
            // and the shape `parse_percent` reads (`^\d+(\.\d+)?%$`, no exponent, no sign).
            Self::Percent(p) => Value::String(lua.create_string(format!("{:.3}%", p * 100.0))?),
            Self::Color(color) => Value::String(lua.create_string(hex_of(color))?),
            Self::Fields { keys, values } => {
                let table = lua.create_table_with_capacity(0, keys.len())?;
                for (key, value) in keys.iter().zip(values) {
                    table.set(*key, value)?;
                }
                Value::Table(table)
            }
            Self::Path(ref path) => tweened(lua, path)?,
            Self::Shape(ref outline) => tweened(lua, outline)?,
            Self::Effect(values, ref kept) => {
                let level = |values: &[f32]| {
                    lua.create_table_from(
                        ["blur", "saturate", "brightness", "contrast"].into_iter().zip(values.iter().copied()),
                    )
                };
                let table = level(&values[..4])?;
                let backdrop = level(&values[4..8])?;
                if let Some(mask) = &kept.mask {
                    backdrop.set("mask", mask.clone())?;
                }
                table.set("backdrop", backdrop)?;
                if let Some(Value::Table(shader)) = &kept.shader {
                    // A copy: the config's own table is not ours to write.
                    let tweened =
                        lua.create_table_from(shader.pairs::<Value, Value>().collect::<mlua::Result<Vec<_>>>()?)?;
                    tweened.set("progress", values[8])?;
                    table.set("shader", tweened)?;
                }
                Value::Table(table)
            }
            Self::Layers(ref layers) => {
                let list = lua.create_table_with_capacity(layers.len(), 0)?;
                for layer in layers {
                    list.push(match layer {
                        Layer::Color(color, Blend::Normal) => Value::String(lua.create_string(hex_of(*color))?),
                        Layer::Color(color, blend) => Value::Table(
                            lua.create_table_from([("fill", hex_of(*color)), ("blend", blend.name().into())])?,
                        ),
                        Layer::Snap(value) => value.clone(),
                    })?;
                }
                Value::Table(list)
            }
            Self::Ring(ring) => Value::Table(lua.create_table_from([
                ("width", Value::Number(f64::from(ring.width))),
                ("color", Value::String(lua.create_string(hex_of(ring.color))?)),
                ("offset", Value::Number(f64::from(ring.offset))),
            ])?),
            Self::Shadows(ref layers) => {
                let list = lua.create_table_with_capacity(layers.len(), 0)?;
                for shadow in layers {
                    let offset = lua.create_table_from([("x", shadow.offset.0), ("y", shadow.offset.1)])?;
                    let layer = lua.create_table_with_capacity(0, 5)?;
                    layer.set("color", hex_of(shadow.color))?;
                    layer.set("blur", shadow.blur)?;
                    layer.set("offset", offset)?;
                    layer.set("spread", shadow.spread)?;
                    layer.set("inset", shadow.inset)?;
                    layer.set("blend", shadow.blend.name())?;
                    list.push(layer)?;
                }
                Value::Table(list)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::layout::node::animate::Easing;
    use crate::layout::node::animate::sequence;

    #[test]
    fn overshooting_easings_leave_the_unit_range_and_the_property_range_clamp_catches_them() {
        assert!(Easing::OutBack.apply(0.7) > 1.0);
        let from = Animatable::Number(40.0);
        let to = Animatable::Number(0.0);
        assert_eq!(from.lerp(&to, Easing::OutBack.apply(0.7), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(&to, Easing::OutBack.apply(0.7), "margin"), Animatable::Number(n) if n < 0.0));
        // A path's trim lands on exactly 1, so a closed ring strokes untrimmed rather than open.
        let (empty, whole) = (Animatable::Number(0.0), Animatable::Number(1.0));
        assert_eq!(empty.lerp(&whole, Easing::OutBack.apply(0.7), "trim_end"), whole);
        assert_eq!(whole.lerp(&empty, Easing::OutBack.apply(0.7), "trim_start"), empty);

        // `InBack` winds backwards, so the clamp bites at the start of a growing width rather than
        // at the end of a shrinking one the way `OutBack`'s does above.
        let (from, to) = (Animatable::Number(0.0), Animatable::Number(40.0));
        assert_eq!(from.lerp(&to, Easing::InBack.apply(0.3), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(&to, Easing::InBack.apply(0.3), "margin"), Animatable::Number(n) if n < 0.0));
    }

    /// `animate.shape` between two melts of two parts tweens each part's edges and the melt.
    #[test]
    fn melts_tween_part_by_part() {
        let lua = Lua::new();
        let melt = |right: u32, melt: u32| {
            let src = format!(
                "return {{ melt = {melt}, parts = {{ {{ left = 0, top = 0, right = 20, bottom = 20 }}, \
                 {{ left = 10, top = 10, right = {right}, bottom = 40 }} }} }}"
            );
            Animatable::from_value("shape", Some(&lua.load(src).eval().unwrap())).unwrap().unwrap()
        };
        let Animatable::Shape(mid) = melt(100, 0).lerp(&melt(60, 8), 0.5, "shape") else { panic!("a shape") };
        let Animatable::Shape(expected) = melt(80, 4) else { panic!("a shape") };
        assert_eq!(*mid, *expected);
        let rect = crate::text::snap::LogicalRect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 };
        assert!((mid.bounds(rect).width - 80.0).abs() < 0.1, "the tweened part reaches x = 80");
    }

    #[test]
    fn paths_tween_point_by_point_only_between_matching_ops() {
        let lua = Lua::new();
        let path = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("commands", Some(&value)).unwrap().unwrap()
        };
        let points = |value: &Animatable| match value {
            Animatable::Path(commands) => commands.iter().map(|(_, p)| p.to_vec()).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        let from = path("return {{op='M',points={0,0}},{op='A',points={10,10,4,0,90}}}");
        let to = path("return {{op='M',points={8,-8}},{op='A',points={20,30,8,90,-90}}}");
        assert_eq!(points(&from.lerp(&to, 0.5, "commands")), [vec![4.0, -4.0], vec![15.0, 20.0, 6.0, 45.0, 0.0]]);
        // An overshooting easing past `from` would shrink the radius below zero.
        assert_eq!(points(&from.lerp(&to, -2.0, "commands"))[1][2], 0.0);
        assert_eq!(from.identity("commands"), from, "nothing to tween in from");
        for other in [
            "return {{op='M',points={0,0}},{op='L',points={10,10}}}",
            "return {{op='M',points={0,0},hole=true},{op='A',points={10,10,4,0,90}}}",
            "return {{op='M',points={0,0}}}",
        ] {
            let other = path(other);
            assert_eq!(from.lerp(&other, 0.5, "commands"), other, "different ops snap");
        }
        let unset = path("return {{op='M',points={0,0}}}");
        let explicit = path("return {{op='M',points={4,4},hole=false}}");
        assert_eq!(points(&unset.lerp(&explicit, 0.5, "commands")), [vec![2.0, 2.0]], "hole = false is no hole");
        let huge = path("return {{op='A',points={0,0,1,3e38,3e38}}}");
        let opposite = path("return {{op='A',points={0,0,1,-3e38,-3e38}}}");
        assert!(
            points(&huge.lerp(&opposite, 1.5, "commands"))[0].iter().all(|n| n.is_finite()),
            "angles never overflow"
        );
        let value: Value = lua.load("return {{{op='M',points={0,0}}}, {{op='M',points={10,20}}}}").eval().unwrap();
        let morph = sequence::parse_sequence(
            "commands",
            "animate.commands",
            &value,
            Duration::from_millis(100),
            sequence::Curve::Eased(Easing::Linear),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(points(&morph.at(Duration::from_millis(50), "commands")), [vec![5.0, 10.0]], "keyframes morph");
        let back = from.lerp(&to, 0.25, "commands");
        assert_eq!(Animatable::from_value("commands", Some(&back.to_value(&lua).unwrap())).unwrap(), Some(back));
        assert!(
            Animatable::from_value("commands", Some(&Value::Integer(1))).is_err(),
            "a bad target fails as the property would"
        );
    }

    #[test]
    fn a_corner_table_tweens_per_corner_and_clamps_at_zero() {
        let lua = Lua::new();
        let table = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("radius", Some(&value)).unwrap().unwrap()
        };
        let (from, to) = (table("return { top_left = 10 }"), table("return { bottom_right = 20 }"));
        let values = |t| match from.lerp(&to, t, "radius") {
            Animatable::Fields { keys, values } => (keys == CornersInput::KEYS, values),
            other => panic!("{other:?}"),
        };
        assert_eq!(values(0.5), (true, [5.0, 0.0, 10.0, 0.0]));
        assert_eq!(values(-1.0).1[2], 0.0, "an overshoot never goes negative");
        assert_eq!(Animatable::Number(2.0).lerp(&to, 0.5, "radius"), to, "a number against a corner table snaps");
    }

    #[test]
    fn an_edge_table_halfway_is_the_per_edge_midpoint_and_absent_edges_are_zero() {
        let lua = Lua::new();
        let table = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("margin", Some(&value)).unwrap().unwrap()
        };
        let mid =
            table("return { top = 10, left = -20 }").lerp(&table("return { top = 20, right = 8 }"), 0.5, "margin");
        assert_eq!(mid, Animatable::Fields { keys: EdgesInput::KEYS, values: [15.0, 4.0, 0.0, -10.0] });
        let Value::Table(back) = mid.to_value(&lua).unwrap() else { panic!("edges write back as a table") };
        assert_eq!(back.get::<f32>("left").unwrap(), -10.0);
        let colours: Value = lua.load(r##"return { top = "#ff0000" }"##).eval().unwrap();
        assert_eq!(Animatable::from_value("border_color", Some(&colours)).unwrap(), None, "colour edges snap");
        // Read as edges, it would write `{ top = 0, ... }` back into `background` and fail the pass.
        let gradient: Value = lua
            .load(r##"return { gradient = "linear", stops = { { 0, "#000000" }, { 1, "#ffffff" } } }"##)
            .eval()
            .unwrap();
        assert_eq!(
            Animatable::from_value("background", Some(&gradient)).unwrap(),
            Some(Animatable::Layers(vec![Layer::Snap(gradient.clone())])),
            "a lone gradient is one snapping layer"
        );
        assert_eq!(Animatable::from_value("border_color", Some(&gradient)).unwrap(), None, "a gradient border snaps");
    }

    #[test]
    fn an_axis_table_tweens_on_x_and_y_and_an_absent_scale_axis_is_one() {
        let lua = Lua::new();
        let table = |src: &str, property: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value(property, Some(&value)).unwrap().unwrap()
        };
        let mid = table("return { x = 1 }", "scale").lerp(&table("return { x = 2, y = 3 }", "scale"), 0.5, "scale");
        assert_eq!(mid, Animatable::Fields { keys: Axes::KEYS, values: [1.5, 2.0, 1.0, 1.0] });
        let Value::Table(back) = mid.to_value(&lua).unwrap() else { panic!("axes write back as a table") };
        assert_eq!((back.get::<f32>("x").unwrap(), back.get::<f32>("y").unwrap()), (1.5, 2.0));
        assert!(!back.contains_key("top").unwrap());
        // A number against a table snaps: a `scale = 2` meeting `scale = { x = 2 }`.
        let snapped = Animatable::Number(2.0).lerp(&table("return { x = 2 }", "scale"), 0.5, "scale");
        assert_eq!(snapped, table("return { x = 2 }", "scale"));
    }

    /// Colour layers mix pairwise, a layer one side lacks fades, a gradient layer snaps to the
    /// target's, and a list round-trips through the value written back for the parsers.
    #[test]
    fn background_layers_tween_pairwise_and_an_extra_layer_fades() {
        let lua = Lua::new();
        let layers = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("background", Some(&value)).unwrap().unwrap()
        };
        let from = layers(r##"return { "#000000ff" }"##);
        let to = layers(r##"return { { fill = "#ffffffff", blend = "screen" }, { fill = "#ff0000ff" } }"##);
        let Animatable::Layers(mid) = from.lerp(&to, 0.5, "background") else { panic!("a layer list") };
        let Layer::Color(first, blend) = mid[0] else { panic!("a colour") };
        assert_eq!((first.r, first.a, blend), (0.5, 1.0, Blend::Screen), "the colour tweens, the blend snaps");
        let Layer::Color(second, _) = mid[1] else { panic!("a colour") };
        assert_eq!((second.r, second.a), (1.0, 0.5), "the extra layer fades in");
        let Animatable::Layers(out) = to.lerp(&from, 0.5, "background") else { panic!("a layer list") };
        assert_eq!(out.len(), 2, "and fades out");
        let gradient =
            layers(r##"return { { gradient = "radial", stops = { { 0, "#000000" }, { 1, "#ffffff" } } } }"##);
        assert_eq!(from.lerp(&gradient, 0.1, "background"), gradient, "a gradient snaps");
        assert_eq!(gradient.lerp(&from, 0.1, "background"), from, "also away from one");
        let lone = layers(r##"return "#000000ff""##);
        let Animatable::Layers(mid) = lone.lerp(&to, 0.5, "background") else { panic!("a layer list") };
        let Layer::Color(first, _) = mid[0] else { panic!("a colour") };
        assert_eq!(first.r, 0.5, "a lone colour mixes with a list instead of snapping");
        let Value::Table(written) = to.to_value(&lua).unwrap() else { panic!("a list writes back as a table") };
        assert_eq!(Animatable::from_value("background", Some(&Value::Table(written))).unwrap(), Some(to));
    }

    /// Layer by layer; a layer only one side has fades in at its own geometry, and a spring's
    /// overshoot stops at the blur's floor.
    #[test]
    fn shadow_layers_tween_pairwise_and_an_extra_layer_fades() {
        let lua = Lua::new();
        let layers = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("shadows", Some(&value)).unwrap().unwrap()
        };
        let from = layers(r##"return { { color = "#000000", blur = 4, offset = { y = 2 } } }"##);
        let to = layers(
            r##"return { { color = "#000000", blur = 8, offset = { y = 6 } }, { color = "#ff0000", spread = 2 } }"##,
        );
        let Animatable::Shadows(mid) = from.lerp(&to, 0.5, "shadows") else { panic!("a shadow list") };
        assert_eq!((mid[0].blur, mid[0].offset), (6.0, (0.0, 4.0)));
        assert_eq!((mid[1].color.r, mid[1].color.a, mid[1].spread), (1.0, 0.5, 2.0));
        let Animatable::Shadows(under) = from.lerp(&to, -2.0, "shadows") else { panic!("a shadow list") };
        assert_eq!(under[0].blur, 0.0, "clamped to the blur's range");
        let back = layers(
            r##"return { { color = "#000000", blur = 8, offset = { y = 6 } }, { color = "#ff0000", spread = 2 } }"##,
        );
        let Value::Table(written) = to.to_value(&lua).unwrap() else { panic!("a list writes back as a table") };
        assert_eq!(Animatable::from_value("shadows", Some(&Value::Table(written))).unwrap(), Some(back));
    }

    /// Width, colour and offset tween together, a ring that appears grows from no width, and the
    /// value writes back as the table the parser reads.
    #[test]
    fn a_ring_tweens_its_width_colour_and_offset_and_grows_from_nothing() {
        let lua = Lua::new();
        let ring = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("ring", Some(&value)).unwrap().unwrap()
        };
        let from = ring(r##"return { width = 2, color = "#000000", offset = 0 }"##);
        let to = ring(r##"return { width = 4, color = "#ff0000", offset = 4 }"##);
        let Animatable::Ring(mid) = from.lerp(&to, 0.5, "ring") else { panic!("a ring") };
        assert_eq!((mid.width, mid.offset, mid.color.r), (3.0, 2.0, 0.5));
        let Animatable::Ring(unset) = to.identity("ring") else { panic!("a ring") };
        assert_eq!((unset.width, unset.color.a, unset.offset), (0.0, 0.0, 4.0));
        let Value::Table(written) = to.to_value(&lua).unwrap() else { panic!("a table") };
        assert_eq!(Animatable::from_value("ring", Some(&Value::Table(written))).unwrap(), Some(to));
    }

    /// ADR-0331: an inset layer tweens against an inset layer, a pair that differs in `inset` shows
    /// the target's layer at once, and `inset` survives the write back.
    #[test]
    fn inset_layers_tween_pairwise_and_a_flag_mismatch_snaps_to_the_target() {
        let lua = Lua::new();
        let layers = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("shadows", Some(&value)).unwrap().unwrap()
        };
        let inset = |blur: u8| layers(&format!("return {{ {{ blur = {blur}, inset = true }} }}"));
        let Animatable::Shadows(mid) = inset(4).lerp(&inset(8), 0.5, "shadows") else { panic!("a shadow list") };
        assert_eq!((mid[0].blur, mid[0].inset), (6.0, true));
        let outer = layers("return { { blur = 4 } }");
        let Animatable::Shadows(snap) = outer.lerp(&inset(8), 0.25, "shadows") else { panic!("a shadow list") };
        assert_eq!((snap[0].blur, snap[0].inset), (8.0, true));
        let Value::Table(written) = inset(8).to_value(&lua).unwrap() else { panic!("a table") };
        assert_eq!(Animatable::from_value("shadows", Some(&Value::Table(written))).unwrap(), Some(inset(8)));
    }

    /// A key only one side sets tweens from or to its off value, `0` for a blur and `1` for a colour
    /// filter, and the value round-trips as an `effect` table.
    #[test]
    fn effect_keys_tween_and_a_missing_key_reads_its_off_value() {
        let lua = Lua::new();
        let effect = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("effect", Some(&value)).unwrap().unwrap()
        };
        let (from, to) = (effect("return { blur = 8 }"), effect("return { backdrop = { blur = 4 } }"));
        let Value::Table(mid) = from.lerp(&to, 0.5, "effect").to_value(&lua).unwrap() else { panic!("a table") };
        let backdrop: mlua::Table = mid.get("backdrop").unwrap();
        assert_eq!((mid.get::<f32>("blur").unwrap(), backdrop.get::<f32>("blur").unwrap()), (4.0, 2.0));
        let (from, to) = (effect("return { saturate = 3 }"), effect("return { backdrop = { contrast = 0 } }"));
        let Value::Table(mid) = from.lerp(&to, 0.5, "effect").to_value(&lua).unwrap() else { panic!("a table") };
        let backdrop: mlua::Table = mid.get("backdrop").unwrap();
        assert_eq!((mid.get::<f32>("saturate").unwrap(), mid.get::<f32>("contrast").unwrap()), (2.0, 1.0));
        assert_eq!((backdrop.get::<f32>("contrast").unwrap(), backdrop.get::<f32>("saturate").unwrap()), (0.5, 1.0));
        // The shader is not tweened: the target's table carries through at once (ADR-0336).
        let (from, to) = (effect("return { blur = 8 }"), effect(r#"return { shader = { source = "/s.frag" } }"#));
        let Value::Table(mid) = from.lerp(&to, 0.5, "effect").to_value(&lua).unwrap() else { panic!("a table") };
        assert_eq!(mid.get::<mlua::Table>("shader").unwrap().get::<String>("source").unwrap(), "/s.frag");
        assert_eq!(mid.get::<f32>("blur").unwrap(), 4.0);
        let empty = effect("return {}");
        let Value::Table(back) = empty.to_value(&lua).unwrap() else { panic!("a table") };
        lua.globals().set("e", back).unwrap();
        let props = crate::layout::node::rect_props(&lua, "return { effect = e }");
        assert_eq!(crate::layout::node::parse_effect(&props).unwrap(), Default::default());
        assert!(Animatable::from_value("effect", Some(&lua.load("return { glow = 1 }").eval().unwrap())).is_err());
    }

    #[test]
    fn a_colour_halfway_is_the_channel_midpoint_and_round_trips_as_hex() {
        let lua = Lua::new();
        let black = Animatable::from_value("border_color", Some(&Value::String(lua.create_string("#000000").unwrap())))
            .unwrap()
            .unwrap();
        let white = Animatable::from_value("border_color", Some(&Value::String(lua.create_string("#ffffff").unwrap())))
            .unwrap()
            .unwrap();
        let mid = black.lerp(&white, 0.5, "border_color");
        let Value::String(hex) = mid.to_value(&lua).unwrap() else { panic!("a colour writes back as a string") };
        assert_eq!(hex.to_str().unwrap(), "#808080ff");
    }

    #[test]
    fn a_percent_halfway_is_the_midpoint_and_round_trips_as_a_percent_string() {
        let lua = Lua::new();
        let pct = |s: &str| {
            Animatable::from_value("width", Some(&Value::String(lua.create_string(s).unwrap()))).unwrap().unwrap()
        };
        let mid = pct("40%").lerp(&pct("60%"), 0.5, "width");
        let Value::String(text) = mid.to_value(&lua).unwrap() else { panic!("a percent writes back as a string") };
        assert_eq!(text.to_str().unwrap(), "50.000%");
        let fill = Value::String(lua.create_string("fill").unwrap());
        assert_eq!(Animatable::from_value("width", Some(&fill)).unwrap(), None, "`Fill` is not an endpoint");
    }

    #[test]
    fn an_effect_displacement_keeps_the_larger_of_each_slots_two_levels() {
        let effect = |values| Animatable::Effect(values, Kept::default());
        let moved = effect([2.0, -1.0, 0.0, 0.0, -3.0, 0.0, 0.0, 0.0, 0.5]);
        let from = effect([1.0, 1.0, 0.0, 0.0, -2.0, 0.0, 0.0, 0.0, 0.0]);
        // Slot 0's two levels tie at 1 and -1: the first one kept.
        assert_eq!(moved.delta(&from), [1.0, -2.0, 0.0, 0.0]);
    }
}

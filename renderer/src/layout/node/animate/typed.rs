//! Typed samples: a paint-only tick writes a [`TYPED`] tween's value into the parsed style and
//! leaves the property map behind, which saves a Lua table and a parse per tween per frame. The
//! sample stays in [`Tween::shown`] for a retarget; [`sync`] writes it into the map for any other
//! reader. `layout::scene::tick` is the one place that puts a sample in the style.

use std::rc::Rc;
use std::time::Instant;

use mlua::Lua;

use super::{Animatable, Motion, Tween, byte};
use crate::layout::node::{LayoutError, PropMap, Rgba, invalid};

/// The properties whose samples a paint-only tick writes straight into the parsed style: the
/// opacity, transform and effect groups.
pub const TYPED: &[&str] = &["opacity", "translate", "scale", "rotate", "origin", "shadows", "effect"];

/// `color` as the hex string the map route writes and the parser reads back.
fn quantized(color: Rgba) -> Rgba {
    let channel = |c: f32| f32::from(byte(c)) / 255.0;
    Rgba { r: channel(color.r), g: channel(color.g), b: channel(color.b), a: channel(color.a) }
}

fn to_value(sample: &Animatable, lua: &Lua) -> Result<mlua::Value, LayoutError> {
    sample.to_value(lua).map_err(|e| invalid("animate", e.to_string()))
}

/// Samples each running tween at `now`. A tween on the map route writes its value into
/// `properties`; with `typed`, a [`TYPED`] one writes nothing and its sample comes back for the
/// caller to read into the style, then to [`commit`] once that read has succeeded.
pub fn step(
    tweens: &mut [Tween],
    properties: &mut Rc<PropMap>,
    now: Instant,
    lua: &Lua,
    typed: bool,
) -> Result<Vec<(&'static str, Animatable)>, LayoutError> {
    let mut samples = Vec::new();
    for tween in tweens {
        // `layout::scene::Scene::advance_scrolls` writes a scroll's offset into its signal.
        if tween.resting || tween.property == "scroll" {
            continue;
        }
        let mut sample = tween.at(now);
        if typed && TYPED.contains(&tween.property) {
            if let Animatable::Shadows(layers) = &mut sample {
                layers.iter_mut().for_each(|layer| layer.color = quantized(layer.color));
            }
            samples.push((tween.property, sample));
        } else {
            // A content-sized axis's run holds no key between layouts, so this may insert.
            Rc::make_mut(properties).insert(tween.property, to_value(&sample, lua)?);
            tween.shown = None;
        }
        tween.resting = matches!(tween.spec.motion, Motion::Sequence(_)) && tween.done(now);
    }
    Ok(samples)
}

/// Keeps each of `samples` as its tween's `shown`, or in the map when it is the one that ends the
/// tween, then drops the tweens that have arrived. A sequence that has played out stays, resting
/// on its last frame, so a pass can tell it from one it has never started (ADR-0152).
pub fn commit(
    tweens: &mut Vec<Tween>,
    properties: &mut Rc<PropMap>,
    samples: Vec<(&'static str, Animatable)>,
    now: Instant,
    lua: &Lua,
) -> Result<(), LayoutError> {
    for (property, sample) in samples {
        let tween = tweens.iter_mut().find(|tween| tween.property == property).expect("a sample names a running tween");
        tween.shown = if tween.done(now) {
            Rc::make_mut(properties).insert(property, to_value(&sample, lua)?);
            None
        } else {
            Some(sample)
        };
    }
    tweens.retain(|tween| {
        tween.property == "scroll" || matches!(tween.spec.motion, Motion::Sequence(_)) || !tween.done(now)
    });
    Ok(())
}

/// Writes the samples left in [`Tween::shown`] into `properties`, for a reader of the map.
pub fn sync(tweens: &mut [Tween], properties: &mut PropMap, lua: &Lua) -> Result<(), LayoutError> {
    for tween in tweens {
        if let Some(sample) = tween.shown.take() {
            properties.insert(tween.property, to_value(&sample, lua)?);
        }
    }
    Ok(())
}

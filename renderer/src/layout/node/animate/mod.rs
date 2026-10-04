//! `animate`: per-property tweens on a retained node (ADR-0145). A node names the properties it
//! wants eased and how long; when a pass resolves a different target for one of them, the node's
//! [`Tween`] carries the displayed value from where it was to where it is going, and
//! `layout::scene::Scene::tick` advances it between passes without running any Lua.
//!
//! This module owns the arithmetic; `parse` reads the Lua entries and `easing` holds the curves.
//! Where the tween lives, when one starts and what a tick relays out are `layout::scene`'s.

use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Lua, Value};

use super::prop::Prop;
use super::style::{SHADOW_BLUR, SHADOW_REACH, TONE, axis_default, parse_percent, range_of};
use super::{
    Axes, CornersInput, EdgeInsets, EdgesInput, EffectKeys, Effects, LayoutError, PathCommands, PathData, PropMap,
    Rgba, Shadow, Shadows, fields, invalid, parse_hex_color, tweened, value_as_f32,
};
use crate::lua::luacats::spelled;

mod easing;
mod move_tween;
mod parse;
mod sequence;
#[cfg(test)]
pub(crate) use sequence::KeyframeInput;
mod spring;
mod transition;
pub(crate) use easing::Easing;
pub(crate) use move_tween::MoveTween;
pub(crate) use parse::Animations;
pub(crate) use parse::ExitBlock;
pub use parse::MoveSpec;
#[cfg(test)]
pub(crate) use parse::animatable_name;
#[cfg(test)]
use parse::parse_animate;
use parse::parse_exit;

/// The named easings, the `EasingName` alias's members.
#[cfg(test)]
pub(crate) fn easing_names() -> impl Iterator<Item = &'static str> {
    Easing::NAMES.iter().map(|(name, _)| *name)
}
use sequence::Sequence;
use spring::Spring;
#[cfg(test)]
pub(crate) use spring::SpringConstants;
pub(crate) use transition::Params;
#[cfg(test)]
pub(crate) use transition::TransitionInput;
pub use transition::{Dissolve, ShaderParam, TransitionSpec};

/// The one thing a hex colour has to look like to reach `parse_hex_color` again next pass.
fn hex_of(color: Rgba) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", byte(color.r), byte(color.g), byte(color.b), byte(color.a))
}

/// `a` to `b` at `t`, each channel kept in `[0, 1]`.
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let blend = |x: f32, y: f32| (x + (y - x) * t).clamp(0.0, 1.0);
    Rgba { r: blend(a.r, b.r), g: blend(a.g, b.g), b: blend(a.b, b.b), a: blend(a.a, b.a) }
}

/// `shadow` at zero alpha: an unset layer paints nothing, without a second hue to cross.
fn faded(shadow: &Shadow) -> Shadow {
    Shadow { color: Rgba { a: 0.0, ..shadow.color }, ..*shadow }
}

/// How one property eases: `animate = { width = 200 }` or
/// `animate = { width = { duration = 200, easing = "out_cubic", from = 0 } }`.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimationSpec {
    /// What kind of motion this is, and the only place its timing lives.
    pub motion: Motion,
    /// How long the property holds still before the motion starts (ADR-0153). Zero when absent.
    /// It offsets a sequence's whole run, loops and all, rather than each cycle.
    pub delay: Duration,
    /// Where a node that has never displayed this property starts: a fresh node's entry, or a
    /// property it did not carry last pass. Absent means the value is taken as it is.
    pub from: Option<Animatable>,
}

/// How a property gets where it is going. The three are exclusive, and which one an entry names
/// decides which of its other keys mean anything -- a `duration` beside `keyframes` is the frames'
/// default, and beside a `spring` it is nothing at all, which the parser refuses rather than let
/// this enum carry a dead field (ADR-0154).
#[derive(Debug, Clone, PartialEq)]
pub enum Motion {
    /// One pass along a curve of progress, from what the node displays to what a pass resolved
    /// (ADR-0145).
    Eased { duration: Duration, easing: Easing },
    /// The property walks a list of values and reads nothing resolved for it (ADR-0152).
    Sequence(Sequence),
    /// A mass on a spring: no duration, and it carries its velocity through a change of target
    /// (ADR-0154).
    Spring(Spring),
}

/// Starts a removed node's exit tweens (ADR-0150): each `animate.exit` target from the value the
/// node displays, or from the property's identity when it never set one (an absent `opacity` is
/// `1`, an absent `scale` is `1`, anything else `0`), over the block's shared `duration` and
/// `easing`. Every tween the node was already running is dropped where it stood, so the exit
/// block alone decides how long the node lives. Returns whether anything is now in flight: a node
/// with nothing to ease is dropped at once.
pub fn depart(
    kind: &str,
    tweens: &mut Vec<Tween>,
    properties: &mut PropMap,
    now: Instant,
    lua: &Lua,
) -> Result<bool, LayoutError> {
    let Some(animate) = fields::common::animate.read(properties)? else { return Ok(false) };
    let block: Value = animate.get("exit").map_err(|e| invalid("animate.exit", e.to_string()))?;
    let Some(ExitBlock { spec, targets }) = parse_exit(kind, &block)? else { return Ok(false) };
    // Everything already in flight stops here, at the value it had reached. The exit owns the
    // node's motion from now on, so its lifetime is the block's duration and not that plus
    // whatever an interrupted entry animation had left to run.
    tweens.clear();
    for (property, target) in targets {
        let from =
            Animatable::from_value(property, properties.get(property))?.unwrap_or_else(|| target.identity(property));
        let tween =
            Tween { property, from, to: target, started: now, spec: spec.clone(), reversal: None, resting: false };
        properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
        tweens.push(tween);
    }
    Ok(!tweens.is_empty())
}

/// A value a tween can sit between, told apart by shape rather than by which property holds it.
/// `Percent` is a `"NN%"` size held as a fraction; `Fields` is a table of numbers under one of
/// three key sets, the edges `{ top, right, bottom, left }`, the corners `{ top_left, .. }` or the
/// axes `{ x, y }`, an absent key reading as the property's default (`0`, or `1` for a `scale`).
/// `Path` is a path's `commands`, which tween point by point only between lists of the same ops
/// and hole flags. `Shadows` is a `shadows` list, tweened layer by layer. `Effect` is an `effect`'s
/// `[blur, saturate, brightness, contrast]` and the same four of its `backdrop`, a missing key
/// reading as off: `0` for a blur, `1` for a colour filter. Two different shapes
/// snap, so a fill that switches between `"45%"` and `"fill"` or a margin that switches between a
/// number and a table takes the new value at once.
#[derive(Debug, Clone, PartialEq)]
pub enum Animatable {
    Number(f32),
    Percent(f32),
    Color(Rgba),
    Fields { keys: &'static [&'static str], values: [f32; 4] },
    Path(Rc<PathData>),
    Shadows(Vec<Shadow>),
    Effect([f32; 8]),
}

/// An `effect` with every filter off: blurs `0`, colour filters `1`.
const EFFECT_OFF: [f32; 8] = [0.0, 1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0];

spelled!(Animatable => format!(
    "{}|{}|{}|{}|{}|{}|{}|{}",
    f32::lua(),
    String::lua(),
    EdgeInsets::lua(),
    CornersInput::lua(),
    Axes::lua(),
    EffectKeys::lua(),
    PathCommands::lua(),
    Shadows::lua()
));

impl Animatable {
    /// `property`'s value when it is not set, in this value's shape: `1` for `opacity`, `trim_end`
    /// and `scale`, `0` otherwise, per axis or edge for a table. A percent or a colour has no identity
    /// to speak of and stays where it is.
    fn identity(&self, property: &str) -> Self {
        match *self {
            // No empty drawing has this one's ops to tween from.
            Self::Path(_) => self.clone(),
            Self::Number(_) => {
                Self::Number(if matches!(property, "opacity" | "trim_end") { 1.0 } else { axis_default(property) })
            }
            Self::Fields { keys, .. } => Self::Fields { keys, values: [axis_default(property); 4] },
            Self::Shadows(ref layers) => Self::Shadows(layers.iter().map(faded).collect()),
            Self::Effect(_) => Self::Effect(EFFECT_OFF),
            // An unset size is nothing, and an unset colour paints nothing, which is that colour
            // at zero alpha rather than a second hue to cross on the way out.
            Self::Percent(_) => Self::Percent(0.0),
            Self::Color(colour) => Self::Color(Rgba { a: 0.0, ..colour }),
        }
    }

    /// The typed reading of `property`'s current value, or `None` when the value is a shape no
    /// tween carries (`"fill"`, a boolean, a table of colours, absent): the caller snaps then. A
    /// `#` string that fails its colour parse is an error, the same one the property's own parser
    /// raises.
    pub fn from_value(property: &str, value: Option<&Value>) -> Result<Option<Self>, LayoutError> {
        let Some(value) = value else { return Ok(None) };
        // By name: a command array has no shape of its own that a table of edges or axes lacks.
        if property == "commands" {
            return Ok(Some(Self::Path(PathCommands::read(&fields::path::commands.row, Some(value))?)));
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
            ];
            return Ok(Some(Self::Effect(std::array::from_fn(|i| given[i].unwrap_or(EFFECT_OFF[i])))));
        }
        if property == "shadows" {
            return Ok(Shadows::read(&fields::common::shadows.row, Some(value))?.map(Self::Shadows));
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
    fn delta(&self, to: &Self) -> [f32; 4] {
        let mut out = [0.0; 4];
        match (self, to) {
            (Self::Number(a), Self::Number(b)) | (Self::Percent(a), Self::Percent(b)) => out[0] = a - b,
            (Self::Fields { keys, values: a }, Self::Fields { keys: other, values: b }) if keys == other => {
                for ((slot, x), y) in out.iter_mut().zip(a).zip(b) {
                    *slot = x - y;
                }
            }
            // The two levels share four slots, the larger displacement of each pair standing for it.
            (Self::Effect(a), Self::Effect(b)) => {
                for (i, (x, y)) in a.iter().zip(b).enumerate() {
                    if (x - y).abs() > out[i % 4].abs() {
                        out[i % 4] = x - y;
                    }
                }
            }
            (Self::Color(a), Self::Color(b)) => out = [a.r - b.r, a.g - b.g, a.b - b.b, a.a - b.a],
            // ponytail: a path or shadow list gives a spring no velocity; per-point rates would carry it.
            _ => {}
        }
        out
    }

    fn lerp(&self, to: &Self, t: f32, property: &str) -> Self {
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
            (Self::Effect(a), Self::Effect(b)) => Self::Effect(std::array::from_fn(|i| {
                let (lo, hi) = if i % 4 == 0 { SHADOW_BLUR } else { TONE };
                (a[i] + (b[i] - a[i]) * t).clamp(lo, hi)
            })),
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
                    Shadow {
                        color: mix(x.color, y.color, t),
                        blur: clamp(lerp(x.blur, y.blur), SHADOW_BLUR),
                        offset: (offset(x.offset.0, y.offset.0), offset(x.offset.1, y.offset.1)),
                        spread: clamp(lerp(x.spread, y.spread), SHADOW_REACH),
                    }
                });
                Self::Shadows(layers.collect())
            }
            (Self::Path(a), Self::Path(b)) => a.lerp(b, t).map_or_else(|| to.clone(), |path| Self::Path(Rc::new(path))),
            // The two shapes come from the same property, so this pair cannot be mixed; snap to
            // the target rather than guess if it ever is.
            _ => to.clone(),
        }
    }

    /// The value written back into a resolved property map for the parsers to read.
    pub fn to_value(&self, lua: &Lua) -> mlua::Result<Value> {
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
            Self::Effect(values) => {
                let level = |values: &[f32]| {
                    lua.create_table_from(
                        ["blur", "saturate", "brightness", "contrast"].into_iter().zip(values.iter().copied()),
                    )
                };
                let table = level(&values[..4])?;
                table.set("backdrop", level(&values[4..])?)?;
                Value::Table(table)
            }
            Self::Shadows(ref layers) => {
                let list = lua.create_table_with_capacity(layers.len(), 0)?;
                for shadow in layers {
                    let offset = lua.create_table_from([("x", shadow.offset.0), ("y", shadow.offset.1)])?;
                    let layer = lua.create_table_with_capacity(0, 4)?;
                    layer.set("color", hex_of(shadow.color))?;
                    layer.set("blur", shadow.blur)?;
                    layer.set("offset", offset)?;
                    layer.set("spread", shadow.spread)?;
                    list.push(layer)?;
                }
                Value::Table(list)
            }
        })
    }
}

/// One property of one retained node, in flight from `from` to `to`. `started` is when the pass
/// that saw the target change ran; progress is elapsed time over the spec's duration (ADR-0130
/// decision 2), never accumulated frame deltas.
#[derive(Debug, Clone, PartialEq)]
pub struct Tween {
    pub property: &'static str,
    /// Where the motion began, and where it is bound for. Both are ignored under
    /// [`Motion::Sequence`] -- a sequence reads its own frames -- and hold its first and last for
    /// a reader.
    pub from: Animatable,
    pub to: Animatable,
    pub started: Instant,
    pub spec: AnimationSpec,
    /// The logical start of an eased transition and its shortened duration after reversals.
    reversal: Option<Reversal>,
    /// A finite sequence that has played out (ADR-0152). It stays in the list so a pass does not
    /// start it over, holding the property at its last frame, but it no longer asks for frames.
    /// Always false for a plain tween, which is dropped the moment it arrives.
    pub resting: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Reversal {
    origin: Animatable,
    factor: f32,
}

impl Tween {
    fn eased_duration(&self, duration: Duration) -> Duration {
        duration.mul_f64(f64::from(self.reversal.as_ref().map_or(1.0, |reversal| reversal.factor)))
    }

    /// How far into the motion itself `now` is: time since the tween started, less the spec's
    /// `delay`. Saturating, so the whole delay window reads as zero and both callers below hold
    /// at the beginning without a branch of their own -- every easing answers 0 at 0, and a
    /// sequence's first frame is where it starts.
    fn progressed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started).saturating_sub(self.spec.delay)
    }

    pub fn at(&self, now: Instant) -> Animatable {
        let elapsed = self.progressed(now);
        let progress = match &self.spec.motion {
            // The lead-in holds the value the run opens on. `progressed` saturates to zero through
            // it, which is where every easing and every spring starts anyway, but a sequence
            // opening on a jump plays that jump at zero, and would spend the whole delay showing
            // the value after it rather than the one before (ADR-0153).
            Motion::Sequence(sequence) if now.saturating_duration_since(self.started) < self.spec.delay => {
                return sequence.frames[0].value.clone();
            }
            Motion::Sequence(sequence) => return sequence.at(elapsed, self.property),
            // Exactly the target, not `from + (to - from) * 1`: a settled spring is still off by its
            // threshold, and once the tween is dropped `retarget` reads this value back as the target
            // it compares against, so anything short of `to` starts a new tween on the next pass.
            _ if self.done(now) => return self.to.clone(),
            Motion::Eased { duration, easing } => {
                easing.apply((elapsed.as_secs_f32() / self.eased_duration(*duration).as_secs_f32()).min(1.0))
            }
            Motion::Spring(spring) => spring.at(elapsed),
        };
        self.from.lerp(&self.to, progress, self.property)
    }

    pub fn done(&self, now: Instant) -> bool {
        let elapsed = self.progressed(now);
        match &self.spec.motion {
            Motion::Sequence(sequence) => sequence.done(elapsed),
            Motion::Eased { duration, .. } => elapsed >= self.eased_duration(*duration),
            Motion::Spring(spring) => spring.done(elapsed),
        }
    }
}

/// Reconciles a node's tweens against the targets a pass just resolved, and writes the displayed
/// value of each into `properties` for the parsers to read. `retained` is the node this one was
/// matched to, as the tweens it carried and the properties it last displayed; `None` is a new
/// node, which takes its targets as they are.
///
/// A target that differs from the retained target starts a tween from the value on screen: the
/// one the retained map holds, which is what the last pass or tick painted, whether that was a
/// resting value or the middle of an earlier tween. A target that matches keeps the running
/// tween, so a pass that re-resolves for some unrelated signal does not restart motion. A property
/// `animate` stopped naming loses its tween and snaps. A property nothing displayed yet, on a new
/// node or one that lacked it, starts from the spec's `from` when there is one.
pub fn retarget(
    kind: &str,
    retained: Option<(&[Tween], &PropMap)>,
    properties: &mut PropMap,
    now: Instant,
    lua: &Lua,
) -> Result<(Vec<Tween>, Option<MoveSpec>), LayoutError> {
    let (specs, movement) = parse::parse_animate(kind, properties)?;
    let (running, shown) = retained.map_or((&[][..], None), |(running, shown)| (running, Some(shown)));
    let mut tweens = Vec::with_capacity(specs.len());
    for (property, spec) in specs {
        let running = running.iter().find(|t| t.property == property);
        // The property holds the signal; a wheel starts the run (`retarget_scroll`) and a pass keeps it.
        if property == "scroll" {
            tweens.extend(running.cloned());
            continue;
        }
        // A sequence drives the property rather than easing to it (ADR-0152), so it needs no
        // target and reads nothing the pass resolved. The same list going round again is the same
        // run, played out or not; a different list is a new one, from its first frame.
        if let Motion::Sequence(sequence) = &spec.motion {
            let carried = running.filter(|prior| prior.spec.motion == spec.motion);
            let mut tween = match carried {
                Some(prior) => Tween { spec, ..prior.clone() },
                None => Tween {
                    property,
                    from: sequence.frames[0].value.clone(),
                    to: sequence.frames.last().expect("a parsed sequence has frames").value.clone(),
                    started: now,
                    spec,
                    reversal: None,
                    resting: false,
                },
            };
            // Against `now`, not against what the carried run was resting on: `advance` is the only
            // other place this is decided, and a pass can both finish a run it never ticked and
            // hand a played-out one a fresh `delay`. Carrying the old flag through either of those
            // leaves the tree disagreeing with the clock -- a finished run still asking for frame
            // callbacks, or a re-delayed one resting so hard that `animating` never asks for the
            // first.
            tween.resting = tween.done(now);
            properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
            tweens.push(tween);
            continue;
        }
        let Some(target) = Animatable::from_value(property, properties.get(property))? else {
            // A content-sized axis has no target until layout measures it: keep the run for
            // `retarget_measured`.
            if matches!(property, "width" | "height") && properties.get(property).is_none() {
                tweens.extend(running.cloned());
            }
            continue;
        };
        let displayed = match shown {
            Some(shown) => Animatable::from_value(property, shown.get(property))?,
            None => None,
        };
        let Some(displayed) = displayed.or_else(|| spec.from.clone()) else { continue };
        let retained_target = running.map_or(&displayed, |tween| &tween.to);
        let tween = match running {
            _ if *retained_target != target => {
                if displayed == target {
                    continue;
                }
                // A spring that is already moving hands its rate to the run replacing it, so a
                // target that changes mid-flight bends the motion instead of restarting it from
                // still (ADR-0154). An eased tween returning to its prior endpoint shortens its
                // run; another target begins a full one.
                let reversal = if let Motion::Eased { duration, easing } = &spec.motion {
                    let previous = running.and_then(|prior| match (&prior.spec.motion, prior.reversal.clone()) {
                        (Motion::Eased { duration: old_duration, easing: old_easing }, Some(state))
                            if old_duration == duration
                                && old_easing == easing
                                && state.origin == target
                                && !prior.done(now)
                                && now.saturating_duration_since(prior.started) >= prior.spec.delay =>
                        {
                            Some((prior, state))
                        }
                        _ => None,
                    });
                    Some(match previous {
                        Some((prior, state)) => {
                            let elapsed = prior.progressed(now);
                            let phase = elapsed.as_secs_f32() / prior.eased_duration(*duration).as_secs_f32();
                            let factor =
                                (easing.apply(phase) * state.factor + 1.0 - state.factor).abs().clamp(0.0, 1.0);
                            Reversal { origin: prior.to.clone(), factor }
                        }
                        None => Reversal { origin: displayed.clone(), factor: 1.0 },
                    })
                } else {
                    None
                };
                let spec = handed_over(spec, running, &displayed, &target, now);
                Tween { property, from: displayed, to: target, started: now, spec, reversal, resting: false }
            }
            Some(running) if !running.done(now) => {
                // A spring's `velocity` is the rate the last retarget handed it, not a number the
                // config wrote, and re-parsing the entry always yields one at rest. Taking the
                // fresh spec whole would stop a moving spring dead on the first pass any unrelated
                // signal caused, so one whose constants still match carries the spring across.
                // Only the spring: the rest of the entry is re-read, so an edited `delay` lands on
                // a spring still in its lead-in. Editing a constant takes the new spring at its parsed
                // rest -- either way the run continues, it is the motion under it that changed.
                let mut spec = spec;
                // A delay only means something before motion starts; re-reading it after that would
                // rewind the run, as a staggered card does when a newcomer shifts its index.
                if now.saturating_duration_since(running.started) >= running.spec.delay {
                    spec.delay = running.spec.delay;
                }
                if let (Motion::Spring(fresh), Motion::Spring(prior)) = (&spec.motion, &running.spec.motion)
                    && fresh.constants == prior.constants
                {
                    spec.motion = Motion::Spring(*prior);
                }
                Tween { spec, ..running.clone() }
            }
            _ => continue,
        };
        properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
        tweens.push(tween);
    }
    Ok((tweens, movement))
}

/// The tween for a content-sized `width` or `height` after layout measured it: `shown` is the
/// size on screen, `measured` the size the content wants. A run already bound for `measured` is
/// kept, so an unrelated pass does not restart it. Any other change eases from `shown`, so an
/// interrupted run starts where it was. Returns the size to lay out at, or `None` when the axis
/// follows its content.
///
/// ponytail: eased only (springs and keyframes snap), and a reversal does not shorten its run.
pub fn retarget_measured(
    kind: &str,
    properties: &PropMap,
    tweens: &mut Vec<Tween>,
    property: &'static str,
    shown: f32,
    measured: f32,
    now: Instant,
) -> Result<Option<f32>, LayoutError> {
    let running = tweens.iter().position(|tween| tween.property == property).map(|at| tweens.remove(at));
    let (mut specs, _) = parse::parse_animate(kind, properties)?;
    let Some(spec) = specs.remove(property).filter(|spec| matches!(spec.motion, Motion::Eased { .. })) else {
        return Ok(None);
    };
    let to = Animatable::Number(measured);
    let tween = match running {
        Some(running) if running.to == to && !running.done(now) => running,
        _ if shown == measured => return Ok(None),
        _ => Tween {
            property,
            from: Animatable::Number(shown),
            to,
            started: now,
            spec,
            reversal: Some(Reversal { origin: Animatable::Number(shown), factor: 1.0 }),
            resting: false,
        },
    };
    let Animatable::Number(size) = tween.at(now) else { unreachable!("a size tween holds numbers") };
    tweens.push(tween);
    Ok(Some(size))
}

/// The properties a tween can move without asking the solver anything: what they change is what a
/// node paints, never the box it was given. `layout::scene::solver::taffy_style` reads none of them, and
/// `layout::scene::solver::measure_for` reads a text's content, size, family and wrapping but not its
/// colour, so a tick whose every running tween names one of these can re-derive the paint in place
/// and leave the taffy pass out entirely (`layout::scene::Scene::tick`).
///
/// `opacity` is in `LayoutStyle` and still belongs here: the solver never receives it, `finish`
/// only copies it onto the node, and `layout::paint` multiplies it down the subtree.
///
/// The transform properties belong here too (ADR-0261): hit testing reads them at event time, and
/// the regions they move are re-derived for every ticked surface.
///
/// `scroll` too: `layout::scene::Scene::advance_scrolls` moves the children, not the tick.
const PAINT_ONLY: &[&str] = &[
    "scroll",
    "opacity",
    "background",
    "border_color",
    "foreground",
    "progress",
    "commands",
    "fill",
    "stroke",
    "stroke_width",
    "trim_start",
    "trim_end",
    "shift",
    "radius",
    "corner_smoothing",
    "shadows",
    "effect",
    "translate",
    "scale",
    "rotate",
    "origin",
];

/// Whether a tween on `property` can be advanced by a paint-only tick; see [`PAINT_ONLY`].
pub fn is_paint_only(property: &str) -> bool {
    PAINT_ONLY.contains(&property)
}

/// Advances every tween in `tweens` to `now`, writing the displayed values into `properties` and
/// dropping the ones that have arrived. A sequence that has played out is kept instead, resting on
/// its last frame, because the list alone is what a pass has to tell a finished run from one it
/// has never started (ADR-0152).
pub fn advance(tweens: &mut Vec<Tween>, properties: &mut PropMap, now: Instant, lua: &Lua) -> Result<(), LayoutError> {
    for tween in tweens.iter_mut() {
        // `layout::scene::Scene::advance_scrolls` writes a scroll's offset into its signal.
        if tween.resting || tween.property == "scroll" {
            continue;
        }
        // A content-sized axis's run holds no key between layouts, so this may insert.
        properties.insert(tween.property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
        tween.resting = matches!(tween.spec.motion, Motion::Sequence(_)) && tween.done(now);
    }
    tweens.retain(|tween| {
        tween.property == "scroll" || matches!(tween.spec.motion, Motion::Sequence(_)) || !tween.done(now)
    });
    Ok(())
}

/// `spec` for a run from `from` to `to` replacing `running`: a moving spring hands over its rate,
/// so a target that changes mid-flight bends the motion instead of restarting it (ADR-0154).
fn handed_over(
    spec: AnimationSpec,
    running: Option<&Tween>,
    from: &Animatable,
    to: &Animatable,
    now: Instant,
) -> AnimationSpec {
    match (spec.motion, running) {
        (Motion::Spring(spring), Some(running)) => {
            AnimationSpec { motion: Motion::Spring(spring.handed(running, from, to, now)), ..spec }
        }
        (motion, _) => AnimationSpec { motion, ..spec },
    }
}

/// A container's `animate.scroll` entry, if it names one.
pub fn scroll_spec(kind: &str, properties: &PropMap) -> Result<Option<AnimationSpec>, LayoutError> {
    Ok(parse::parse_animate(kind, properties)?.0.remove("scroll"))
}

/// A wheel notch or `:reveal` under `spec`: the run from `shown`, the offset on screen, to `target`.
pub fn retarget_scroll(spec: AnimationSpec, tweens: &mut Vec<Tween>, shown: f32, target: f32, now: Instant) {
    let running = tweens.iter().position(|tween| tween.property == "scroll").map(|at| tweens.remove(at));
    let (from, to) = (Animatable::Number(shown), Animatable::Number(target));
    let spec = handed_over(spec, running.as_ref(), &from, &to, now);
    if shown != target {
        tweens.push(Tween { property: "scroll", from, to, started: now, spec, reversal: None, resting: false });
    }
}

/// Where a running scroll tween is bound for.
pub fn scroll_target(tweens: &[Tween]) -> Option<f32> {
    tweens.iter().find(|tween| tween.property == "scroll").and_then(|tween| match tween.to {
        Animatable::Number(target) => Some(target),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::layout::node::rect_props;

    impl AnimationSpec {
        /// The eased pair, for the tests that only care about timing. Panics on the other two
        /// motions, which is what a test asserting a duration wants when it is handed a sequence.
        fn eased(&self) -> (Duration, Easing) {
            match &self.motion {
                Motion::Eased { duration, easing } => (*duration, *easing),
                other => panic!("expected an eased motion, got {other:?}"),
            }
        }
    }

    /// The spec `src` declares for `width`, and the message refusing `src`: between them, what
    /// every parser test below asks.
    pub(super) fn spec(lua: &Lua, src: &str) -> AnimationSpec {
        parse_animate("rect", &rect_props(lua, src)).unwrap().0.remove("width").unwrap()
    }

    pub(super) fn refused(lua: &Lua, src: &str) -> String {
        parse_animate("rect", &rect_props(lua, src)).unwrap_err().to_string()
    }

    #[test]
    fn move_accepts_eased_timing_and_refuses_other_motion_forms() {
        let lua = Lua::new();
        let move_spec = |src: &str| parse::parse_animate("rect", &rect_props(&lua, src)).unwrap().1.unwrap();
        assert_eq!(move_spec("return { animate = { move = 120 } }").duration, Duration::from_millis(120));
        let timed = move_spec("return { animate = { move = { duration = 200, delay = 30, easing = 'linear' } } }");
        assert_eq!(
            (timed.duration, timed.delay, timed.easing),
            (Duration::from_millis(200), Duration::from_millis(30), Easing::Linear)
        );
        for field in ["from = { x = 10 }", "keyframes = { 0, 1 }", "spring = {}", "loops = 2", "unknown = 1"] {
            let src = format!("return {{ animate = {{ move = {{ duration = 100, {field} }} }} }}");
            assert!(parse::parse_animate("rect", &rect_props(&lua, &src)).is_err(), "{field}");
        }
        for entry in ["0", "-1", "{}"] {
            let src = format!("return {{ animate = {{ move = {entry} }} }}");
            assert!(parse::parse_animate("rect", &rect_props(&lua, &src)).is_err(), "{entry}");
        }
    }

    #[test]
    fn a_signal_in_an_entry_or_its_spring_resolves_with_animate() {
        let lua = crate::layout::node::signal_lua();
        for (bound, plain) in [
            ("width = state(\"w\", 200)", "width = 200"),
            (
                "width = { spring = { stiffness = state(\"k\", 200), damping = 10 } }",
                "width = { spring = { stiffness = 200, damping = 10 } }",
            ),
        ] {
            let props = rect_props(&lua, &format!("return {{ animate = {{ {bound} }} }}"));
            let resolved = crate::layout::node::resolve_declared(props, "rect", false, &lua).unwrap();
            let plain =
                parse_animate("rect", &rect_props(&lua, &format!("return {{ animate = {{ {plain} }} }}"))).unwrap();
            assert_eq!(parse_animate("rect", &resolved).unwrap(), plain, "{bound}");
        }
    }

    /// Resolution is exactly once: a signal inside a signal's result is not read.
    #[test]
    fn a_signal_inside_a_mapped_animate_entry_names_the_animation() {
        let lua = crate::layout::node::signal_lua();
        let props = rect_props(
            &lua,
            "return { animate = state(\"on\", 0):map(function() return { width = state(\"w\", 200) } end) }",
        );
        let resolved = crate::layout::node::resolve_declared(props, "rect", false, &lua).unwrap();
        let err = parse_animate("rect", &resolved).unwrap_err();
        assert!(matches!(err, LayoutError::UnsupportedSignalProperty(ref path) if path == "animate.width"), "{err:?}");
    }

    #[test]
    fn steps_reads_a_metamethod_once_even_when_its_next_value_would_disagree() {
        for (values, accepted) in [("4, 'bad'", true), ("'bad', 4", false)] {
            let lua = Lua::new();
            let props = rect_props(
                &lua,
                &format!(
                    r#"
                reads = 0
                local values = {{ {values} }}
                local easing = setmetatable({{}}, {{ __index = function(_, key)
                    assert(key == "steps")
                    reads = reads + 1
                    return values[reads]
                end }})
                return {{ animate = {{ width = {{ duration = 200, easing = easing }} }} }}
            "#
                ),
            );
            assert_eq!(parse_animate("rect", &props).is_ok(), accepted);
            assert_eq!(lua.globals().get::<u32>("reads").unwrap(), 1);
        }
    }

    #[test]
    fn an_animation_from_scale_keeps_the_missing_axis_default() {
        let lua = Lua::new();
        let props = rect_props(&lua, "return { animate = { scale = { duration = 200, from = { x = 2 } } } }");
        let spec = parse_animate("rect", &props).unwrap().0.remove("scale").unwrap();
        assert_eq!(spec.from, Some(Animatable::Fields { keys: Axes::KEYS, values: [2.0, 1.0, 1.0, 1.0] }));
    }

    #[test]
    fn out_back_overshoots_and_the_number_clamp_catches_it() {
        assert!(Easing::OutBack.apply(0.7) > 1.0);
        let from = Animatable::Number(40.0);
        let to = Animatable::Number(0.0);
        assert_eq!(from.lerp(&to, Easing::OutBack.apply(0.7), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(&to, Easing::OutBack.apply(0.7), "margin"), Animatable::Number(n) if n < 0.0));
        // A path's trim lands on exactly 1, so a closed ring strokes untrimmed rather than open.
        let (empty, whole) = (Animatable::Number(0.0), Animatable::Number(1.0));
        assert_eq!(empty.lerp(&whole, Easing::OutBack.apply(0.7), "trim_end"), whole);
        assert_eq!(whole.lerp(&empty, Easing::OutBack.apply(0.7), "trim_start"), empty);
    }

    /// The overshooting families leave `[0, 1]` on purpose; the property's own range is what pulls
    /// them back, so a `width` easing to `0` never hands the parser a negative.
    #[test]
    fn back_elastic_and_bounce_overshoot_and_the_range_clamp_catches_them() {
        assert!(Easing::InBack.apply(0.3) < 0.0, "Back winds up before it moves");
        assert!(Easing::OutElastic.apply(0.4) > 1.0, "Elastic rings past the target");
        // `InBack` winds backwards, so the clamp bites at the start of a growing width rather than
        // at the end of a shrinking one the way `OutBack`'s does above.
        let (from, to) = (Animatable::Number(0.0), Animatable::Number(40.0));
        assert_eq!(from.lerp(&to, Easing::InBack.apply(0.3), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(&to, Easing::InBack.apply(0.3), "margin"), Animatable::Number(n) if n < 0.0));
        assert!(Easing::InBounce.apply(0.5) >= 0.0 && Easing::OutBounce.apply(0.5) <= 1.0, "Bounce stays inside");
    }

    /// A four-number table is CSS `cubic-bezier`, solved for `y` at the parameter whose `x` is the
    /// progress. The identity control points are exactly `Linear`, which is the cheapest proof the
    /// solve is not off by a parameter.
    #[test]
    fn a_four_number_easing_is_a_cubic_bezier() {
        let lua = Lua::new();
        let parsed = |src: &str| {
            parse_animate("rect", &rect_props(&lua, src))
                .unwrap()
                .0
                .remove("width")
                .expect("width has a spec")
                .eased()
                .1
        };
        let linear = parsed("return { animate = { width = { duration = 1, easing = { 0, 0, 1, 1 } } } }");
        assert_eq!(linear, Easing::Bezier { x1: 0.0, y1: 0.0, x2: 1.0, y2: 1.0 });
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            assert!((linear.apply(t) - t).abs() < 1e-4, "the identity curve is Linear, off at {t}");
        }
        // CSS `ease-in-out`, whose control points are symmetric, so the curve is too.
        let ease = parsed("return { animate = { width = { duration = 1, easing = { 0.42, 0, 0.58, 1 } } } }");
        assert!((ease.apply(0.5) - 0.5).abs() < 1e-4);
        assert!((ease.apply(0.25) + ease.apply(0.75) - 1.0).abs() < 1e-4);
        assert!(ease.apply(0.25) < 0.25, "it starts slower than linear");
    }

    /// `{ steps = n }` holds each value and lands on the target only at the end, which is what a
    /// blinking or ticking indicator wants instead of a smooth ramp.
    #[test]
    fn a_steps_easing_jumps_and_only_the_last_step_reaches_the_target() {
        let lua = Lua::new();
        let steps = parse_animate(
            "rect",
            &rect_props(&lua, "return { animate = { width = { duration = 1, easing = { steps = 4 } } } }"),
        )
        .unwrap()
        .0
        .remove("width")
        .expect("width has a spec")
        .eased()
        .1;
        assert_eq!(steps, Easing::Steps(4));
        assert_eq!(steps.apply(0.0), 0.0);
        assert_eq!(steps.apply(0.1), 0.0);
        assert_eq!(steps.apply(0.3), 0.25);
        assert_eq!(steps.apply(0.99), 0.75);
        assert_eq!(steps.apply(1.0), 1.0);
    }

    #[test]
    fn a_table_easing_that_is_neither_shape_is_refused() {
        let lua = Lua::new();

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = { 2, 0, 0.5, 1 } } } }");
        assert!(text.contains("`x1` and `x2`") && text.contains("2"), "{text}");

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = { steps = 0 } } } }");
        assert!(text.contains("steps") && text.contains("[1, 1000]"), "{text}");

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = { 0.5, 0.5 } } } }");
        assert!(text.contains("x1, y1, x2, y2") && text.contains("steps"), "{text}");

        let text = refused(&lua, "return { animate = { width = { duration = 1, easing = 4 } } }");
        assert!(text.contains("easing is a name"), "{text}");
    }

    #[test]
    fn a_bare_number_is_a_duration_with_the_default_easing() {
        let lua = Lua::new();
        let specs = parse_animate("rect", &rect_props(&lua, "return { animate = { width = 200 } }")).unwrap().0;
        assert_eq!(
            specs["width"],
            AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_millis(200), easing: Easing::InOutQuad },
                delay: Duration::ZERO,
                from: None,
            }
        );
    }

    #[test]
    fn a_table_names_its_easing() {
        let lua = Lua::new();
        let specs = parse_animate(
            "rect",
            &rect_props(
                &lua,
                r##"return { animate = { background = { duration = 150, easing = "out_cubic", from = "#000000" } } }"##,
            ),
        )
        .unwrap()
        .0;
        assert_eq!(specs["background"].eased().1, Easing::OutCubic);
        assert_eq!(specs["background"].from, Some(Animatable::Color(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 })));
    }

    #[test]
    fn an_unknown_easing_is_refused_naming_the_known_ones() {
        let lua = Lua::new();
        let err = parse_animate(
            "rect",
            &rect_props(&lua, r#"return { animate = { width = { duration = 1, easing = "Bouncy" } } }"#),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("animate.width") && text.contains("Bouncy") && text.contains("out_back"), "{text}");
    }

    #[test]
    fn a_property_the_kind_does_not_have_is_refused_by_name() {
        let lua = Lua::new();
        let err = parse_animate("rect", &rect_props(&lua, "return { animate = { widht = 200 } }")).unwrap_err();
        assert!(err.to_string().contains("`widht`") && err.to_string().contains("`rect`"), "{err}");
        // A real property whose value is not a tween shape is fine to name; it snaps.
        assert!(parse_animate("rect", &rect_props(&lua, "return { animate = { visible = 200 } }")).is_ok());
        // A `lock` declares `width` only to refuse it, so there is nothing to animate.
        let err = parse_animate("lock", &rect_props(&lua, "return { animate = { width = 200 } }")).unwrap_err();
        assert!(err.to_string().contains("`width`") && err.to_string().contains("`lock`"), "{err}");
    }

    #[test]
    fn a_bad_exit_block_is_refused_on_a_live_pass() {
        let lua = Lua::new();

        let text = refused(&lua, "return { animate = { exit = 200 } }");
        assert!(text.contains("animate.exit") && text.contains("expected a table"), "{text}");

        let text = refused(&lua, "return { animate = { exit = { duration = 100, widht = 0 } } }");
        assert!(text.contains("`widht`") && text.contains("`rect`"), "{text}");

        let text = refused(&lua, r#"return { animate = { exit = { duration = 100, opacity = "gone" } } }"#);
        assert!(text.contains("animate.exit.opacity"), "{text}");

        let text = refused(&lua, "return { animate = { exit = { opacity = 0 } } }");
        assert!(text.contains("animate.exit") && text.contains("duration"), "{text}");

        // A block naming nothing has nothing to time, so it stays legal and simply never runs.
        assert!(parse_animate("rect", &rect_props(&lua, "return { animate = { exit = {} } }")).is_ok());
    }

    /// A departing node eases from what it displays, and from the property's own identity when it
    /// never set one: an absent `opacity` is `1`, not the `0` a bare number would default to.
    #[test]
    fn departing_starts_each_target_at_the_displayed_value_or_the_property_identity() {
        let lua = Lua::new();
        let mut properties = rect_props(
            &lua,
            r#"return { width = 40, animate = { exit = { duration = 100, width = 0, opacity = 0 } } }"#,
        );
        let mut tweens = Vec::new();
        let now = Instant::now();
        assert!(depart("rect", &mut tweens, &mut properties, now, &lua).unwrap());
        let started: BTreeMap<&str, &Tween> = tweens.iter().map(|t| (t.property, t)).collect();
        assert_eq!(started["width"].from, Animatable::Number(40.0), "the displayed width");
        assert_eq!(started["opacity"].from, Animatable::Number(1.0), "an absent opacity is opaque");
        assert_eq!(started["opacity"].to, Animatable::Number(0.0));
        let table = lua.load("return { kind = 'path', animate = { exit = { duration = 100, trim_end = 0 } } }");
        let mut path = crate::layout::node::props_from_table(&table.eval().unwrap());
        let mut trims = Vec::new();
        assert!(depart("path", &mut trims, &mut path, now, &lua).unwrap());
        assert_eq!(trims[0].from, Animatable::Number(1.0), "an absent trim_end strokes the whole path");

        // Nothing to ease: the caller drops the node instead of holding it for a frame.
        let mut nothing = rect_props(&lua, "return { width = 40 }");
        assert!(!depart("rect", &mut Vec::new(), &mut nothing, now, &lua).unwrap());
    }

    /// Departing is the end of everything else the node was doing. Otherwise a card interrupted
    /// mid-entry outlives its own exit block: the leftover tween keeps `advance_leaving` from
    /// dropping it, and keeps painting a transition nobody coordinated with the exit.
    #[test]
    fn departing_replaces_every_tween_the_node_was_already_running() {
        let lua = Lua::new();
        let mut properties = rect_props(
            &lua,
            r##"return { width = 40, background = "#ff0000",
                animate = { background = 5000, exit = { duration = 100, opacity = 0 } } }"##,
        );
        let now = Instant::now();
        let mut tweens = vec![Tween {
            property: "background",
            from: Animatable::Color(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
            to: Animatable::Color(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            started: now,
            spec: AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_secs(5), easing: Easing::Linear },
                delay: Duration::ZERO,
                from: None,
            },
            reversal: None,
            resting: false,
        }];
        assert!(depart("rect", &mut tweens, &mut properties, now, &lua).unwrap());
        let properties: Vec<&str> = tweens.iter().map(|t| t.property).collect();
        assert_eq!(properties, ["opacity"], "the five-second background tween does not outlive the exit");
    }

    /// An unset percent is nothing and an unset colour paints nothing, so both have a real value
    /// to leave from. Falling through to the target instead would tween a value to itself and
    /// hold the node for the whole duration showing no motion at all.
    #[test]
    fn an_unset_percent_or_colour_departs_from_nothing_rather_than_from_the_target() {
        let lua = Lua::new();
        let mut properties = rect_props(
            &lua,
            r##"return { animate = { exit = { duration = 100, width = "0%", background = "#3366ff" } } }"##,
        );
        let mut tweens = Vec::new();
        assert!(depart("rect", &mut tweens, &mut properties, Instant::now(), &lua).unwrap());
        let started: BTreeMap<&str, &Tween> = tweens.iter().map(|t| (t.property, t)).collect();
        assert_eq!(started["width"].from, Animatable::Percent(0.0));
        assert_eq!(started["background"].from, Animatable::Color(Rgba { r: 0.2, g: 0.4, b: 1.0, a: 0.0 }));
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
        assert_eq!(Animatable::from_value("background", Some(&gradient)).unwrap(), None, "a gradient snaps");
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
        let empty = effect("return {}");
        let Value::Table(back) = empty.to_value(&lua).unwrap() else { panic!("a table") };
        lua.globals().set("e", back).unwrap();
        let props = crate::layout::node::rect_props(&lua, "return { effect = e }");
        assert_eq!(crate::layout::node::parse_effect(&props).unwrap(), Default::default());
        assert!(Animatable::from_value("effect", Some(&lua.load("return { glow = 1 }").eval().unwrap())).is_err());
    }

    #[test]
    fn a_duration_that_rounds_to_no_milliseconds_is_refused() {
        let lua = Lua::new();
        for src in ["return { animate = { width = 0 } }", "return { animate = { width = 0.1 } }"] {
            // A bound written in floats lets `0.1` through and rounding then makes it nothing, so
            // the bound is on the milliseconds the tween actually gets: a tween of none reports
            // itself finished on the frame it starts and never moves.
            assert!(refused(&lua, src).contains("[1, 60000]"), "{src}");
        }
    }

    #[test]
    fn a_colour_halfway_is_the_channel_midpoint_and_round_trips_as_hex() {
        let lua = Lua::new();
        let black = Animatable::from_value("background", Some(&Value::String(lua.create_string("#000000").unwrap())))
            .unwrap()
            .unwrap();
        let white = Animatable::from_value("background", Some(&Value::String(lua.create_string("#ffffff").unwrap())))
            .unwrap()
            .unwrap();
        let mid = black.lerp(&white, 0.5, "background");
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

    /// A pass that re-resolves for some unrelated signal keeps the running tween and its spec, not
    /// the freshly parsed one. For every other motion that would be harmless -- the curve is
    /// the same curve -- but a spring's `velocity` is the rate the last retarget handed it rather
    /// than anything the config wrote, and parsing yields one at rest. Any unrelated change would
    /// stop a moving spring dead, which is the one thing the hand-over exists to prevent.
    /// A bezier's `y` is deliberately unbounded, so a curve that overshoots hard makes the
    /// endpoint error visible: bisection stops about 1e-6 of a parameter short of the ends, and
    /// the curve's steep opening multiplies that into a value nowhere near where the tween starts.
    #[test]
    fn a_bezier_begins_on_its_source_and_ends_on_its_target_however_far_its_controls_reach() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &rect_props(
                &lua,
                "return { animate = { width = { duration = 100, easing = { 0, 1000000, 1, 1000000 } } } }",
            ),
        )
        .unwrap()
        .0
        .remove("width")
        .unwrap();
        let started = Instant::now();
        let tween = Tween {
            property: "width",
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec,
            reversal: None,
            resting: false,
        };
        assert_eq!(tween.at(started), Animatable::Number(0.0), "it starts where it starts");
        assert_eq!(tween.at(started + Duration::from_millis(100)), Animatable::Number(100.0), "and lands on target");
    }

    /// `duration` and `keyframes` beside a spring were already refused; `easing` was parsed and
    /// then dropped on the floor, so a config could tune a curve that never ran. A field the
    /// chosen motion does not read is a config that believes something it is not getting.
    #[test]
    fn a_field_belonging_to_another_motion_is_refused_rather_than_ignored() {
        let lua = Lua::new();
        let spring = "spring = { stiffness = 200, damping = 10 }";
        for beside in ["easing = \"linear\"", "duration = 200", "loops = 3"] {
            let text = refused(&lua, &format!("return {{ animate = {{ width = {{ {spring}, {beside} }} }} }}"));
            assert!(text.contains("a `spring` has no"), "{beside}: {text}");
        }
        let text = refused(&lua, &format!("return {{ animate = {{ width = {{ {spring}, duration = \"oops\" }} }} }}"));
        assert!(text.contains("expected a duration in ms"), "{text}");
        // `loops` without a list to walk was read by nobody at all, typo and count alike.
        let text = refused(&lua, "return { animate = { width = { duration = 10, loops = 3 } } }");
        assert!(text.contains("`loops`"), "{text}");
    }

    #[test]
    fn a_settled_tween_shows_its_target_exactly() {
        let lua = Lua::new();
        let started = Instant::now();
        for src in [
            "return { animate = { width = { spring = { stiffness = 200, damping = 10 } } } }",
            "return { animate = { width = { duration = 100, easing = \"out_cubic\" } } }",
        ] {
            let tween = Tween {
                property: "width",
                from: Animatable::Number(0.0),
                to: Animatable::Number(0.3),
                started,
                spec: spec(&lua, src),
                reversal: None,
                resting: false,
            };
            let settled = started + Duration::from_secs(60);
            assert!(tween.done(settled), "{src}");
            assert_eq!(tween.at(settled), Animatable::Number(0.3), "{src}");
        }
    }

    #[test]
    fn a_delay_holds_the_start_value_then_runs_the_whole_duration() {
        let started = Instant::now();
        let tween = Tween {
            property: "width",
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec: AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_millis(100), easing: Easing::Linear },
                delay: Duration::from_millis(50),
                from: None,
            },
            reversal: None,
            resting: false,
        };
        assert_eq!(tween.at(started), Animatable::Number(0.0));
        assert_eq!(
            tween.at(started + Duration::from_millis(50)),
            Animatable::Number(0.0),
            "still held at the hand-off"
        );
        assert_eq!(tween.at(started + Duration::from_millis(100)), Animatable::Number(50.0), "halfway, 50 ms late");
        assert_eq!(tween.at(started + Duration::from_millis(150)), Animatable::Number(100.0));
        // The delay is added to the life of the tween, not taken out of it.
        assert!(!tween.done(started + Duration::from_millis(100)));
        assert!(tween.done(started + Duration::from_millis(150)));
    }

    #[test]
    fn a_pass_that_lengthens_the_delay_of_a_moving_tween_does_not_rewind_it() {
        let lua = Lua::new();
        let started = Instant::now();
        let running = [Tween {
            property: "width",
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec: AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_millis(200), easing: Easing::Linear },
                delay: Duration::ZERO,
                from: None,
            },
            reversal: None,
            resting: false,
        }];
        let shown: PropMap = PropMap::from_iter([("width", Value::Number(50.0))]);
        let mut properties =
            rect_props(&lua, "return { width = 100, animate = { width = { duration = 200, delay = 120 } } }");
        let now = started + Duration::from_millis(100);
        retarget("rect", Some((&running[..], &shown)), &mut properties, now, &lua).unwrap();
        let displayed = Animatable::from_value("width", properties.get("width")).unwrap();
        assert_eq!(displayed, Some(Animatable::Number(50.0)), "halfway stays halfway");
    }

    #[test]
    fn an_eased_tween_shortens_reversals_without_restarting_the_full_duration() {
        let lua = Lua::new();
        let start = Instant::now();
        let mut forward_props = rect_props(
            &lua,
            "return { width = 100, animate = { width = { duration = 300, easing = \"linear\", from = 0 } } }",
        );
        let forward = retarget("rect", None, &mut forward_props, start, &lua).unwrap().0;
        let at = |tween: &Tween, now| match tween.at(now) {
            Animatable::Number(value) => value,
            other => panic!("expected a number, got {other:?}"),
        };
        let turn = start + Duration::from_millis(270);
        assert_eq!(at(&forward[0], turn), 90.0);

        let shown = PropMap::from_iter([("width", forward[0].at(turn).to_value(&lua).unwrap())]);
        let mut back_props =
            rect_props(&lua, "return { width = 0, animate = { width = { duration = 300, easing = \"linear\" } } }");
        let back = retarget("rect", Some((&forward, &shown)), &mut back_props, turn, &lua).unwrap().0;
        assert_eq!(at(&back[0], turn + Duration::from_millis(135)), 45.0);
        assert!(back[0].done(turn + Duration::from_millis(270)));

        let turn_again = turn + Duration::from_millis(135);
        let shown = PropMap::from_iter([("width", back[0].at(turn_again).to_value(&lua).unwrap())]);
        let mut forward_props =
            rect_props(&lua, "return { width = 100, animate = { width = { duration = 300, easing = \"linear\" } } }");
        let forward_again = retarget("rect", Some((&back, &shown)), &mut forward_props, turn_again, &lua).unwrap().0;
        assert!(!forward_again[0].done(turn_again + Duration::from_millis(164)));
        assert!(forward_again[0].done(turn_again + Duration::from_millis(166)));
    }

    #[test]
    fn eased_reversal_uses_eased_progress_and_survives_an_unrelated_pass() {
        let lua = Lua::new();
        let start = Instant::now();
        let forward_source =
            "return { width = 100, animate = { width = { duration = 100, easing = \"out_cubic\", from = 0 } } }";
        let back_source = "return { width = 0, animate = { width = { duration = 100, easing = \"out_cubic\" } } }";
        let mut forward_props = rect_props(&lua, forward_source);
        let forward = retarget("rect", None, &mut forward_props, start, &lua).unwrap().0;
        let turn = start + Duration::from_millis(50);
        assert_eq!(forward[0].at(turn), Animatable::Number(87.5));
        let shown = PropMap::from_iter([("width", forward[0].at(turn).to_value(&lua).unwrap())]);
        let mut back_props = rect_props(&lua, back_source);
        let back = retarget("rect", Some((&forward, &shown)), &mut back_props, turn, &lua).unwrap().0;
        assert_eq!(back[0].at(turn), Animatable::Number(87.5));
        assert!(!back[0].done(turn + Duration::from_millis(87)));
        assert!(back[0].done(turn + Duration::from_millis(88)));

        let mut same_props = rect_props(&lua, back_source);
        let carried = retarget("rect", Some((&back, &shown)), &mut same_props, turn, &lua).unwrap().0;
        assert!(!carried[0].done(turn + Duration::from_millis(87)));
        assert!(carried[0].done(turn + Duration::from_millis(88)));
    }

    #[test]
    fn a_delay_is_a_whole_number_of_milliseconds_within_a_minute() {
        let lua = Lua::new();
        let specs =
            parse_animate("rect", &rect_props(&lua, "return { animate = { width = { duration = 10, delay = 40 } } }"))
                .unwrap()
                .0;
        assert_eq!(specs["width"].delay, Duration::from_millis(40));
        let bare = parse_animate("rect", &rect_props(&lua, "return { animate = { width = 10 } }")).unwrap().0;
        assert_eq!(bare["width"].delay, Duration::ZERO, "absent is no delay");
        let zeroed =
            parse_animate("rect", &rect_props(&lua, "return { animate = { width = { duration = 10, delay = 0 } } }"))
                .unwrap()
                .0;
        assert_eq!(zeroed["width"].delay, Duration::ZERO, "zero is the default written out, not a refusal");

        let cases: [(&str, &[&str]); 5] = [
            ("delay = 60001", &["delay", "[0, 60000]"]),
            ("delay = -1", &["delay", "[0, 60000]"]),
            // A value that is not a number at all is a typo, not a zero: `value_as_f32` cannot
            // tell a string from an absent key, so a silent `Duration::ZERO` would drop the
            // lead-in without saying so, while the same typo on `duration` fails the pass.
            (r#"delay = "50""#, &["expected a delay in ms"]),
            ("delay = {}", &["expected a delay in ms"]),
            (r#"keyframes = { 0, { value = 1, duration = "5" } }"#, &["expected a duration in ms"]),
        ];
        for (entry, wanted) in cases {
            let text = refused(&lua, &format!("return {{ animate = {{ width = {{ duration = 10, {entry} }} }} }}"));
            assert!(wanted.iter().all(|want| text.contains(want)), "{entry}: {text}");
        }
    }

    #[test]
    fn a_tween_reads_from_at_its_start_and_to_at_its_end() {
        let started = Instant::now();
        let tween = Tween {
            property: "width",
            from: Animatable::Number(40.0),
            to: Animatable::Number(90.0),
            started,
            spec: AnimationSpec {
                motion: Motion::Eased { duration: Duration::from_millis(100), easing: Easing::Linear },
                delay: Duration::ZERO,
                from: None,
            },
            reversal: None,
            resting: false,
        };
        assert_eq!(tween.at(started), Animatable::Number(40.0));
        assert_eq!(tween.at(started + Duration::from_millis(50)), Animatable::Number(65.0));
        assert_eq!(tween.at(started + Duration::from_millis(500)), Animatable::Number(90.0));
        assert!(tween.done(started + Duration::from_millis(100)));
    }
}

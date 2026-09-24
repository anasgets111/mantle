//! `animate`: per-property tweens on a retained node (ADR-0145). A node names the properties it
//! wants eased and how long; when a pass resolves a different target for one of them, the node's
//! [`Tween`] carries the displayed value from where it was to where it is going, and
//! `layout::scene::Scene::tick` advances it between passes without running any Lua.
//!
//! This module owns the parsing and the arithmetic. Where the tween lives, when one starts and
//! what a tick relays out are `layout::scene`'s.
//!
//! The easing curves themselves are `easing`'s.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use mlua::{Lua, Value};

use super::style::{axis_default, parse_percent, range_of};
use super::{LayoutError, PropMap, Rgba, invalid, only_keys, parse_hex_color, preview_for_error, value_as_f32};

mod easing;
mod sequence;
mod spring;
mod transition;
use easing::Easing;
use sequence::{Sequence, parse_sequence};
use spring::{Spring, parse_spring};
pub(in crate::layout::node) use transition::parse_shader_params;
pub use transition::{Dissolve, ShaderParam, TransitionSpec, parse_transition};

/// The one thing a hex colour has to look like to reach `parse_hex_color` again next pass.
fn hex_of(color: Rgba) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", byte(color.r), byte(color.g), byte(color.b), byte(color.a))
}

/// How one property eases: `animate = { width = 200 }` or
/// `animate = { width = { duration = 200, easing = "OutCubic", from = 0 } }`.
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

/// The name an `animate` entry eases, refused if `kind` does not have it. `animate` itself is not
/// one: a block cannot ease the block.
fn animatable_name(kind: &str, property: &str, field: &str) -> Result<&'static str, LayoutError> {
    if property == "z" {
        return Err(invalid(field, "`z` snaps; it cannot animate"));
    }
    crate::lua::nodes::accepted_name(kind, property)
        .filter(|name| *name != "animate")
        .ok_or_else(|| invalid(field, format!("`{property}` is not a property of a `{kind}` node")))
}

/// `animate`'s table, resolved: which properties ease and how. Absent means none. The table
/// itself may be a signal, resolved like any other property; entries inside it are plain values.
/// A name `kind` does not accept is refused, so a misspelling fails the pass instead of silently
/// snapping; what the value is decides whether it can tween ([`Animatable::from_value`]), the way
/// Qt registers interpolators by type rather than by property.
pub fn parse_animate(kind: &str, properties: &PropMap) -> Result<BTreeMap<&'static str, AnimationSpec>, LayoutError> {
    let Some(value) = properties.get("animate") else {
        return Ok(BTreeMap::new());
    };
    let Value::Table(table) = value else {
        return Err(invalid(
            "animate",
            format!("expected a table of property names to durations, got {}", preview_for_error(value)),
        ));
    };
    // Sorted in and sorted out: Lua seeds its own string hashes, so two broken entries -- or two
    // that fail to retarget below -- would otherwise name either one, run to run (ADR-0024).
    let mut raw = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, entry) = pair.map_err(|e| invalid("animate", e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid("animate", format!("keys are property names, got {}", preview_for_error(&key))));
        };
        let property = key.to_str().map_err(|e| invalid("animate", e.to_string()))?;
        raw.insert((*property).to_owned(), entry);
    }

    let mut out = BTreeMap::new();
    for (property, entry) in raw {
        // The one key that is not a property name (ADR-0150). Checked here rather than only when
        // the node departs, so a typo in the block is refused while the node is still in the tree.
        if property == "exit" {
            parse_exit(kind, &entry)?;
            continue;
        }
        let name = animatable_name(kind, &property, "animate")?;
        if let Value::Table(spec) = &entry {
            only_keys(
                &format!("animate.{name}"),
                spec,
                &["duration", "delay", "easing", "from", "keyframes", "loops", "spring"],
            )?;
        }
        out.insert(name, parse_spec(name, &entry)?);
    }
    Ok(out)
}

/// One entry's spec: a bare duration, or `{ duration, easing, from }`, or those beside a
/// `keyframes` list and a `loops` count (ADR-0152), or a `spring` instead of any timing at all
/// (ADR-0154). `from` is read as a value of `property`.
fn parse_spec(property: &str, entry: &Value) -> Result<AnimationSpec, LayoutError> {
    let field = format!("animate.{property}");
    // The bare form says the duration and nothing else: `animate = { width = 200 }`.
    let Value::Table(spec) = entry else {
        let duration = parse_millis(&field, "duration", entry, 1)?
            .ok_or_else(|| invalid(&field, format!("expected a duration in ms, got {}", preview_for_error(entry))))?;
        let motion = Motion::Eased { duration, easing: Easing::default() };
        return Ok(AnimationSpec { motion, delay: Duration::ZERO, from: None });
    };
    let get = |key: &str| -> Result<Value, LayoutError> { spec.get(key).map_err(|e| invalid(&field, e.to_string())) };

    let from = match get("from")? {
        Value::Nil => None,
        from => Some(Animatable::from_value(property, Some(&from))?.ok_or_else(|| {
            invalid(&field, format!("`from` must be a value a tween can carry, got {}", preview_for_error(&from)))
        })?),
    };
    let delay = parse_millis(&field, "delay", &get("delay")?, 0)?.unwrap_or(Duration::ZERO);

    // Which motion this is decides which of the timing fields are read at all, so the ones
    // belonging to another are refused rather than parsed and dropped. That is the rule ADR-0152
    // already applied to `from` beside `keyframes`: an `easing` silently ignored beside a spring
    // is a config that believes it tuned something.
    let spring = parse_spring(&field, spec)?;
    let keyframes = get("keyframes")?;
    if spring.is_some() && !keyframes.is_nil() {
        return Err(invalid(
            &field,
            "`spring` and `keyframes` are two different motions: a spring settles on one target, a sequence walks a list"
                .to_string(),
        ));
    }
    if spring.is_some() {
        for name in ["duration", "easing", "loops"] {
            if !get(name)?.is_nil() {
                return Err(invalid(
                    &field,
                    format!("a `spring` has no `{name}`: what it does is decided by its stiffness and damping"),
                ));
            }
        }
    } else if keyframes.is_nil() && !get("loops")?.is_nil() {
        return Err(invalid(
            &field,
            "`loops` counts the walks of a `keyframes` list, and this entry has none".to_string(),
        ));
    }

    let motion = match spring {
        Some(spring) => Motion::Spring(spring),
        None => {
            let duration = get("duration")?;
            let duration = parse_millis(&field, "duration", &duration, 1)?.ok_or_else(|| {
                invalid(&field, format!("expected a duration in ms, got {}", preview_for_error(&duration)))
            })?;
            let easing = parse_easing(&field, &get("easing")?)?;
            match parse_sequence(property, &field, spec, &keyframes, duration, easing)? {
                Some(sequence) => {
                    if from.is_some() {
                        return Err(invalid(
                            &field,
                            "`from` and `keyframes` say the same thing twice: a sequence starts on its own first frame"
                                .to_string(),
                        ));
                    }
                    Motion::Sequence(sequence)
                }
                None => Motion::Eased { duration, easing },
            }
        }
    };
    Ok(AnimationSpec { motion, delay, from })
}

/// One `duration` or `delay`, in whole milliseconds: `None` when the field is absent, an error
/// when it is there and is not a number.
///
/// The two are told apart here rather than by `value_as_f32`, which answers `None` for a string as
/// readily as for `nil` and would let a typo read as an omission and take the default. `least` is
/// the smallest the field may round to, which is `1` wherever zero means no motion at all: a
/// `duration` of `0.1` clears any bound written in floats and then rounds to nothing, leaving a
/// tween that reports itself finished the instant it starts. Whole milliseconds because
/// `from_secs_f32` would carry `200` as `200.000003ms`.
fn parse_millis(field: &str, what: &str, value: &Value, least: u64) -> Result<Option<Duration>, LayoutError> {
    if value.is_nil() {
        return Ok(None);
    }
    let millis = value_as_f32(field, value)?
        .ok_or_else(|| invalid(field, format!("expected a {what} in ms, got {}", preview_for_error(value))))?;
    let rounded = millis.round() as u64;
    if !(0.0..=60_000.0).contains(&millis) || rounded < least {
        return Err(invalid(field, format!("{what} must be within [{least}, 60000] ms, got {millis}")));
    }
    Ok(Some(Duration::from_millis(rounded)))
}

/// The shared spec and every `(property, target)` pair of one `animate.exit` block.
type ExitBlock = (AnimationSpec, Vec<(&'static str, Animatable)>);

/// A spec's `easing`: a name, a four-number table read as CSS `cubic-bezier(x1, y1, x2, y2)`, or
/// `{ steps = n }` (ADR-0151). Absent is `InOutQuad`.
fn parse_easing(field: &str, value: &Value) -> Result<Easing, LayoutError> {
    match value {
        Value::Nil => Ok(Easing::default()),
        Value::String(name) => {
            let name = name.to_str().map_err(|e| invalid(field, e.to_string()))?;
            Easing::parse(&name).ok_or_else(|| {
                let known: Vec<String> = Easing::NAMES.iter().map(|(n, _)| format!("`{n}`")).collect();
                invalid(field, format!("expected one of {}, got `{name}`", known.join(", ")))
            })
        }
        Value::Table(table) => {
            let steps: Value = table.get("steps").map_err(|e| invalid(field, e.to_string()))?;
            if !steps.is_nil() {
                only_keys(field, table, &["steps"])?;
                let steps = value_as_f32(field, &steps)?
                    .ok_or_else(|| invalid(field, format!("`steps` is a count, got {}", preview_for_error(&steps))))?;
                if steps < 1.0 || steps > 1000.0 || steps.fract() != 0.0 {
                    return Err(invalid(field, format!("`steps` must be a whole count in [1, 1000], got {steps}")));
                }
                return Ok(Easing::Steps(steps as u32));
            }
            let mut points = [0.0f32; 4];
            for (index, slot) in points.iter_mut().enumerate() {
                let point: Value = table.get(index + 1).map_err(|e| invalid(field, e.to_string()))?;
                *slot = value_as_f32(field, &point)?.ok_or_else(|| {
                    invalid(field, "a table easing is `{ x1, y1, x2, y2 }` or `{ steps = n }`".to_string())
                })?;
            }
            // Only the control `x` are bounded, and CSS bounds them for the same reason: outside
            // `[0, 1]` the curve doubles back and one progress has several answers. The `y` are
            // free, which is what lets a Bezier overshoot the way `OutBack` does.
            if !(0.0..=1.0).contains(&points[0]) || !(0.0..=1.0).contains(&points[2]) {
                return Err(invalid(
                    field,
                    format!("a Bezier's `x1` and `x2` must be within [0, 1], got {} and {}", points[0], points[2]),
                ));
            }
            Ok(Easing::Bezier { x1: points[0], y1: points[1], x2: points[2], y2: points[3] })
        }
        other => Err(invalid(
            field,
            format!("easing is a name, `{{ x1, y1, x2, y2 }}` or `{{ steps = n }}`, got {}", preview_for_error(other)),
        )),
    }
}

/// `animate.exit`'s block, resolved: `{ duration, easing, <property> = <target>, ... }`, one spec
/// for every named target. The targets are what the node eases to once the tree no longer holds
/// it (ADR-0150).
/// A block naming no target is a no-op and needs no duration, so it resolves to `None`.
fn parse_exit(kind: &str, block: &Value) -> Result<Option<ExitBlock>, LayoutError> {
    let Value::Table(exit) = block else {
        return match block {
            Value::Nil => Ok(None),
            other => Err(invalid("animate.exit", format!("expected a table, got {}", preview_for_error(other)))),
        };
    };
    let mut out = Vec::new();
    for pair in exit.pairs::<Value, Value>() {
        let (key, target) = pair.map_err(|e| invalid("animate.exit", e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid("animate.exit", format!("keys are property names, got {}", preview_for_error(&key))));
        };
        let property = key.to_str().map_err(|e| invalid("animate.exit", e.to_string()))?;
        if matches!(&*property, "duration" | "delay" | "easing" | "spring") {
            continue;
        }
        let name = animatable_name(kind, &property, "animate.exit")?;
        let field = format!("animate.exit.{name}");
        let target = Animatable::from_value(name, Some(&target))?.ok_or_else(|| {
            invalid(&field, format!("must be a value a tween can carry, got {}", preview_for_error(&target)))
        })?;
        out.push((name, target));
    }
    // Last, so an empty block stays legal while one with targets must say how long they take.
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some((parse_spec("exit", block)?, out)))
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
    let Some(Value::Table(animate)) = properties.get("animate") else { return Ok(false) };
    let block: Value = animate.get("exit").map_err(|e| invalid("animate.exit", e.to_string()))?;
    let Some((spec, targets)) = parse_exit(kind, &block)? else { return Ok(false) };
    // Everything already in flight stops here, at the value it had reached. The exit owns the
    // node's motion from now on, so its lifetime is the block's duration and not that plus
    // whatever an interrupted entry animation had left to run.
    tweens.clear();
    for (property, target) in targets {
        let from =
            Animatable::from_value(property, properties.get(property))?.unwrap_or_else(|| target.identity(property));
        let tween = Tween { property, from, to: target, started: now, spec: spec.clone(), resting: false };
        properties.insert(property, tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?);
        tweens.push(tween);
    }
    Ok(!tweens.is_empty())
}

/// A value a tween can sit between, told apart by shape rather than by which property holds it.
/// `Percent` is a `"NN%"` size held as a fraction; `Fields` is a table of numbers under one of
/// two key sets, the edges `{ top, right, bottom, left }` or the axes `{ x, y }`, an absent key
/// reading as the property's default (`0`, or `1` for a `scale`). Two different shapes snap, so
/// a fill that switches between `"45%"` and `"Fill"` or a margin that switches between a number
/// and a table takes the new value at once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Animatable {
    Number(f32),
    Percent(f32),
    Color(Rgba),
    Fields { keys: &'static [&'static str], values: [f32; 4] },
}

const EDGES: &[&str] = &["top", "right", "bottom", "left"];
const AXES: &[&str] = &["x", "y"];

impl Animatable {
    /// `property`'s value when it is not set, in this value's shape: `1` for `opacity` and
    /// `scale`, `0` otherwise, per axis or edge for a table. A percent or a colour has no identity
    /// to speak of and stays where it is.
    fn identity(self, property: &str) -> Self {
        match self {
            Self::Number(_) => Self::Number(if property == "opacity" { 1.0 } else { axis_default(property) }),
            Self::Fields { keys, .. } => Self::Fields { keys, values: [axis_default(property); 4] },
            // An unset size is nothing, and an unset colour paints nothing, which is that colour
            // at zero alpha rather than a second hue to cross on the way out.
            Self::Percent(_) => Self::Percent(0.0),
            Self::Color(colour) => Self::Color(Rgba { a: 0.0, ..colour }),
        }
    }

    /// The typed reading of `property`'s current value, or `None` when the value is a shape no
    /// tween carries (`"Fill"`, a boolean, a table of colours, absent): the caller snaps then. A
    /// `#` string that fails its colour parse is an error, the same one the property's own parser
    /// raises.
    pub fn from_value(property: &str, value: Option<&Value>) -> Result<Option<Self>, LayoutError> {
        let Some(value) = value else { return Ok(None) };
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
                let keys = if has("x") || has("y") { AXES } else { EDGES };
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
    fn delta(self, to: Self) -> [f32; 4] {
        let mut out = [0.0; 4];
        match (self, to) {
            (Self::Number(a), Self::Number(b)) | (Self::Percent(a), Self::Percent(b)) => out[0] = a - b,
            (Self::Fields { keys, values: a }, Self::Fields { keys: other, values: b }) if keys == other => {
                for ((slot, x), y) in out.iter_mut().zip(a).zip(b) {
                    *slot = x - y;
                }
            }
            (Self::Color(a), Self::Color(b)) => out = [a.r - b.r, a.g - b.g, a.b - b.b, a.a - b.a],
            _ => {}
        }
        out
    }

    fn lerp(self, to: Self, t: f32, property: &str) -> Self {
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
            (Self::Color(a), Self::Color(b)) => {
                let mix = |x: f32, y: f32| (x + (y - x) * t).clamp(0.0, 1.0);
                Self::Color(Rgba { r: mix(a.r, b.r), g: mix(a.g, b.g), b: mix(a.b, b.b), a: mix(a.a, b.a) })
            }
            // The two shapes come from the same property, so this pair cannot be mixed; snap to
            // the target rather than guess if it ever is.
            _ => to,
        }
    }

    /// The value written back into a resolved property map for the parsers to read.
    pub fn to_value(self, lua: &Lua) -> mlua::Result<Value> {
        Ok(match self {
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
    /// A finite sequence that has played out (ADR-0152). It stays in the list so a pass does not
    /// start it over, holding the property at its last frame, but it no longer asks for frames.
    /// Always false for a plain tween, which is dropped the moment it arrives.
    pub resting: bool,
}

impl Tween {
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
                return sequence.frames[0].value;
            }
            Motion::Sequence(sequence) => return sequence.at(elapsed, self.property),
            Motion::Eased { duration, easing } => {
                easing.apply((elapsed.as_secs_f32() / duration.as_secs_f32()).min(1.0))
            }
            Motion::Spring(spring) => spring.at(elapsed),
        };
        self.from.lerp(self.to, progress, self.property)
    }

    pub fn done(&self, now: Instant) -> bool {
        let elapsed = self.progressed(now);
        match &self.spec.motion {
            Motion::Sequence(sequence) => sequence.done(elapsed),
            Motion::Eased { duration, .. } => elapsed >= *duration,
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
) -> Result<Vec<Tween>, LayoutError> {
    let specs = parse_animate(kind, properties)?;
    let (running, shown) = retained.map_or((&[][..], None), |(running, shown)| (running, Some(shown)));
    let mut tweens = Vec::with_capacity(specs.len());
    for (property, spec) in specs {
        let running = running.iter().find(|t| t.property == property);
        // A sequence drives the property rather than easing to it (ADR-0152), so it needs no
        // target and reads nothing the pass resolved. The same list going round again is the same
        // run, played out or not; a different list is a new one, from its first frame.
        if let Motion::Sequence(sequence) = &spec.motion {
            let carried = running.filter(|prior| prior.spec.motion == spec.motion);
            let mut tween = match carried {
                Some(prior) => Tween { spec, ..prior.clone() },
                None => Tween {
                    property,
                    from: sequence.frames[0].value,
                    to: sequence.frames.last().expect("a parsed sequence has frames").value,
                    started: now,
                    spec,
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
            continue;
        };
        let displayed = match shown {
            Some(shown) => Animatable::from_value(property, shown.get(property))?,
            None => None,
        };
        let Some(displayed) = displayed.or(spec.from) else { continue };
        let retained_target = running.map_or(displayed, |tween| tween.to);
        let tween = match running {
            _ if retained_target != target => {
                // A spring that is already moving hands its rate to the run replacing it, so a
                // target that changes mid-flight bends the motion instead of restarting it from
                // still (ADR-0154). Every other motion starts over, which is what a curve of
                // progress can do.
                let spec = match (spec.motion, running) {
                    (Motion::Spring(spring), Some(running)) => {
                        AnimationSpec { motion: Motion::Spring(spring.handed(running, displayed, target, now)), ..spec }
                    }
                    (motion, _) => AnimationSpec { motion, ..spec },
                };
                Tween { property, from: displayed, to: target, started: now, spec, resting: false }
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
                    && fresh.constants() == prior.constants()
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
    Ok(tweens)
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
const PAINT_ONLY: &[&str] = &[
    "opacity",
    "background",
    "border_color",
    "foreground",
    "progress",
    "radius",
    "shadow_color",
    "shadow_blur",
    "shadow_offset",
    "shadow_spread",
    "content_blur",
    "backdrop_blur",
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
        if tween.resting {
            continue;
        }
        // `retarget` wrote the key when it started the tween, so this never inserts.
        *properties.get_mut(tween.property).expect("a tween's property is in the map it was started from") =
            tween.at(now).to_value(lua).map_err(|e| invalid("animate", e.to_string()))?;
        tween.resting = matches!(tween.spec.motion, Motion::Sequence(_)) && tween.done(now);
    }
    tweens.retain(|tween| matches!(tween.spec.motion, Motion::Sequence(_)) || !tween.done(now));
    Ok(())
}

#[cfg(test)]
mod tests {
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
        parse_animate("rect", &rect_props(lua, src)).unwrap().remove("width").unwrap()
    }

    pub(super) fn refused(lua: &Lua, src: &str) -> String {
        parse_animate("rect", &rect_props(lua, src)).unwrap_err().to_string()
    }

    #[test]
    fn out_back_overshoots_and_the_number_clamp_catches_it() {
        assert!(Easing::OutBack.apply(0.7) > 1.0);
        let from = Animatable::Number(40.0);
        let to = Animatable::Number(0.0);
        assert_eq!(from.lerp(to, Easing::OutBack.apply(0.7), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(to, Easing::OutBack.apply(0.7), "margin"), Animatable::Number(n) if n < 0.0));
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
        assert_eq!(from.lerp(to, Easing::InBack.apply(0.3), "width"), Animatable::Number(0.0));
        assert!(matches!(from.lerp(to, Easing::InBack.apply(0.3), "margin"), Animatable::Number(n) if n < 0.0));
        assert!(Easing::InBounce.apply(0.5) >= 0.0 && Easing::OutBounce.apply(0.5) <= 1.0, "Bounce stays inside");
    }

    /// A four-number table is CSS `cubic-bezier`, solved for `y` at the parameter whose `x` is the
    /// progress. The identity control points are exactly `Linear`, which is the cheapest proof the
    /// solve is not off by a parameter.
    #[test]
    fn a_four_number_easing_is_a_cubic_bezier() {
        let lua = Lua::new();
        let parsed = |src: &str| {
            parse_animate("rect", &rect_props(&lua, src)).unwrap().remove("width").expect("width has a spec").eased().1
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
        let specs = parse_animate("rect", &rect_props(&lua, "return { animate = { width = 200 } }")).unwrap();
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
                r##"return { animate = { background = { duration = 150, easing = "OutCubic", from = "#000000" } } }"##,
            ),
        )
        .unwrap();
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
        assert!(text.contains("animate.width") && text.contains("Bouncy") && text.contains("OutBack"), "{text}");
    }

    #[test]
    fn a_property_the_kind_does_not_have_is_refused_by_name() {
        let lua = Lua::new();
        let err = parse_animate("rect", &rect_props(&lua, "return { animate = { widht = 200 } }")).unwrap_err();
        assert!(err.to_string().contains("`widht`") && err.to_string().contains("`rect`"), "{err}");
        // A real property whose value is not a tween shape is fine to name; it snaps.
        assert!(parse_animate("rect", &rect_props(&lua, "return { animate = { visible = 200 } }")).is_ok());
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
    fn an_edge_table_halfway_is_the_per_edge_midpoint_and_absent_edges_are_zero() {
        let lua = Lua::new();
        let table = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            Animatable::from_value("margin", Some(&value)).unwrap().unwrap()
        };
        let mid = table("return { top = 10, left = -20 }").lerp(table("return { top = 20, right = 8 }"), 0.5, "margin");
        assert_eq!(mid, Animatable::Fields { keys: EDGES, values: [15.0, 4.0, 0.0, -10.0] });
        let Value::Table(back) = mid.to_value(&lua).unwrap() else { panic!("edges write back as a table") };
        assert_eq!(back.get::<f32>("left").unwrap(), -10.0);
        let colours: Value = lua.load(r##"return { top = "#ff0000" }"##).eval().unwrap();
        assert_eq!(Animatable::from_value("border_color", Some(&colours)).unwrap(), None, "colour edges snap");
        // Read as edges, it would write `{ top = 0, ... }` back into `background` and fail the pass.
        let gradient: Value = lua
            .load(r##"return { gradient = "Linear", stops = { { 0, "#000000" }, { 1, "#ffffff" } } }"##)
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
        let mid = table("return { x = 1 }", "scale").lerp(table("return { x = 2, y = 3 }", "scale"), 0.5, "scale");
        assert_eq!(mid, Animatable::Fields { keys: AXES, values: [1.5, 2.0, 1.0, 1.0] });
        let Value::Table(back) = mid.to_value(&lua).unwrap() else { panic!("axes write back as a table") };
        assert_eq!((back.get::<f32>("x").unwrap(), back.get::<f32>("y").unwrap()), (1.5, 2.0));
        assert!(!back.contains_key("top").unwrap());
        // A number against a table snaps: a `scale = 2` meeting `scale = { x = 2 }`.
        let snapped = Animatable::Number(2.0).lerp(table("return { x = 2 }", "scale"), 0.5, "scale");
        assert_eq!(snapped, table("return { x = 2 }", "scale"));
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
        let mid = black.lerp(white, 0.5, "background");
        let Value::String(hex) = mid.to_value(&lua).unwrap() else { panic!("a colour writes back as a string") };
        assert_eq!(hex.to_str().unwrap(), "#808080ff");
    }

    #[test]
    fn a_percent_halfway_is_the_midpoint_and_round_trips_as_a_percent_string() {
        let lua = Lua::new();
        let pct = |s: &str| {
            Animatable::from_value("width", Some(&Value::String(lua.create_string(s).unwrap()))).unwrap().unwrap()
        };
        let mid = pct("40%").lerp(pct("60%"), 0.5, "width");
        let Value::String(text) = mid.to_value(&lua).unwrap() else { panic!("a percent writes back as a string") };
        assert_eq!(text.to_str().unwrap(), "50.000%");
        let fill = Value::String(lua.create_string("Fill").unwrap());
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
        .remove("width")
        .unwrap();
        let started = Instant::now();
        let tween = Tween {
            property: "width",
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec,
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
        for beside in ["easing = \"Linear\"", "duration = 200", "duration = \"oops\"", "loops = 3"] {
            let text = refused(&lua, &format!("return {{ animate = {{ width = {{ {spring}, {beside} }} }} }}"));
            assert!(text.contains("a `spring` has no"), "{beside}: {text}");
        }
        // `loops` without a list to walk was read by nobody at all, typo and count alike.
        let text = refused(&lua, "return { animate = { width = { duration = 10, loops = 3 } } }");
        assert!(text.contains("`loops`"), "{text}");
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
    fn a_delay_is_a_whole_number_of_milliseconds_within_a_minute() {
        let lua = Lua::new();
        let specs =
            parse_animate("rect", &rect_props(&lua, "return { animate = { width = { duration = 10, delay = 40 } } }"))
                .unwrap();
        assert_eq!(specs["width"].delay, Duration::from_millis(40));
        let bare = parse_animate("rect", &rect_props(&lua, "return { animate = { width = 10 } }")).unwrap();
        assert_eq!(bare["width"].delay, Duration::ZERO, "absent is no delay");
        let zeroed =
            parse_animate("rect", &rect_props(&lua, "return { animate = { width = { duration = 10, delay = 0 } } }"))
                .unwrap();
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
            resting: false,
        };
        assert_eq!(tween.at(started), Animatable::Number(40.0));
        assert_eq!(tween.at(started + Duration::from_millis(50)), Animatable::Number(65.0));
        assert_eq!(tween.at(started + Duration::from_millis(500)), Animatable::Number(90.0));
        assert!(tween.done(started + Duration::from_millis(100)));
    }
}

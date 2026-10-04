//! `animate`'s Lua side: an entry table read into an [`AnimationSpec`], and `animate.exit` into
//! its targets.

use std::collections::BTreeMap;
use std::time::Duration;

use mlua::Value;

use super::super::input::{self, Input, parse_millis, required_duration};
use super::super::prop::Prop;
use super::super::{LayoutError, PropMap, Property, fields, invalid, only_keys, preview_for_error, value_as_f32};
use super::easing::{BezierPoints, Steps};
#[cfg(test)]
use super::sequence::KeyframeInput;
use super::sequence::{Curve, Loops, parse_sequence};
use super::spring::SpringConstants;
use super::{Animatable, AnimationSpec, Easing, Motion};
#[cfg(test)]
use crate::lua::luacats::optional;
use crate::lua::luacats::{LuaType, lua_shape};

/// The name an `animate` entry eases, refused if `kind` does not have it. `animate` itself is not
/// one: a block cannot ease the block.
pub(crate) fn animatable_name(kind: &str, property: &str, field: &str) -> Result<&'static str, LayoutError> {
    if property == "z" {
        return Err(invalid(field, "`z` snaps; it cannot animate"));
    }
    crate::lua::nodes::accepted(kind, property)
        .map(|row| row.name)
        .filter(|name| *name != "animate" && !crate::lua::nodes::refused(kind, name))
        .ok_or_else(|| invalid(field, format!("`{property}` is not a property of a `{kind}` node")))
}

/// `animate`'s table, resolved: which properties ease and how. Absent means none. The table
/// itself may be a signal, and a signal inside it resolves with it (`node::resolve_declared`).
/// A name `kind` does not accept is refused, so a misspelling fails the pass instead of silently
/// snapping; what the value is decides whether it can tween ([`Animatable::from_value`]), the way
/// Qt registers interpolators by type rather than by property.
pub(super) fn parse_animate(
    kind: &str,
    properties: &PropMap,
) -> Result<(BTreeMap<&'static str, AnimationSpec>, Option<MoveSpec>), LayoutError> {
    let Some(table) = fields::common::animate.read(properties)? else {
        return Ok((BTreeMap::new(), None));
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
    let mut movement = None;
    for (property, entry) in raw {
        // The one key that is not a property name (ADR-0150). Checked here rather than only when
        // the node departs, so a typo in the block is refused while the node is still in the tree.
        if property == "exit" {
            parse_exit(kind, &entry)?;
            continue;
        }
        if property == "move" {
            movement = Some(parse_move(&entry)?);
            continue;
        }
        let name = animatable_name(kind, &property, "animate")?;
        let spec = parse_spec(name, &entry)?;
        if name == "scroll" && matches!(spec.motion, Motion::Sequence(_)) {
            return Err(invalid(
                "animate.scroll",
                "a wheel's target is eased or sprung to; `keyframes` cannot drive it",
            ));
        }
        out.insert(name, spec);
    }
    Ok((out, movement))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveSpec {
    pub duration: Duration,
    pub delay: Duration,
    pub easing: Easing,
}

lua_shape! {
    #[alias = "MoveAnimation"]
    struct MoveInput {
        duration: Duration,
        delay: Option<Duration>,
        easing: Option<Easing>,
    }
}

impl LuaType for MoveSpec {
    fn lua() -> String {
        "MoveAnimation".into()
    }

    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        let mut table = Vec::new();
        MoveInput::classes(&mut table);
        out.push(format!("{}|{}", Duration::lua(), table.concat()));
    }
}

fn parse_move(entry: &Value) -> Result<MoveSpec, LayoutError> {
    let Value::Table(table) = entry else {
        let duration = parse_millis("animate.move", "duration", entry, 1)?.ok_or_else(|| {
            invalid("animate.move", format!("expected a duration in ms, got {}", preview_for_error(entry)))
        })?;
        return Ok(MoveSpec { duration, delay: Duration::ZERO, easing: Easing::default() });
    };
    let MoveInput { duration, delay, easing } = MoveInput::read("animate.move", table)?;
    Ok(MoveSpec {
        duration: required_duration("animate.move", Some(duration))?,
        delay: delay.unwrap_or_default(),
        easing: easing.unwrap_or_default(),
    })
}

/// `animate`: a table of property names to animations, which [`parse_animate`] reads against the
/// node's kind.
pub(crate) struct Animations;

impl LuaType for Animations {
    fn lua() -> String {
        "Animations".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        out.push(BTreeMap::<&str, AnimationSpec>::lua());
    }
}

lua_shape! {
    #[alias = "Animation"]
    struct AnimationInput {
        duration: Option<Duration>,
        delay: Option<Duration>,
        easing: Option<Easing>,
        from: Value as Option<Animatable>,
        spring: Option<SpringConstants>,
        // Bare frames need the animated property's defaults; parse_sequence also rejects holes.
        keyframes: Value as Option<Vec<KeyframeInput>>,
        loops: Option<Loops>,
    }
}

impl LuaType for AnimationSpec {
    fn lua() -> String {
        "Animation".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        let mut table = Vec::new();
        AnimationInput::classes(&mut table);
        out.push(format!("{}|{}", Duration::lua(), table.concat()));
    }
}

impl Prop for Animations {
    type Out = Option<mlua::Table>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<mlua::Table>, LayoutError> {
        match value {
            None => Ok(None),
            Some(Value::Table(table)) => Ok(Some(table.clone())),
            Some(other) => Err(invalid(
                row.name,
                format!("expected a table of property names to durations, got {}", preview_for_error(other)),
            )),
        }
    }
}

/// One entry's spec: a bare duration, or `{ duration, easing, from }`, or those beside a
/// `keyframes` list and a `loops` count (ADR-0152), or a `spring` instead of any timing at all
/// (ADR-0154). Beside `keyframes`, a `spring` is each segment's curve in place of `easing`. `from`
/// is read as a value of `property`.
pub(super) fn parse_spec(property: &str, entry: &Value) -> Result<AnimationSpec, LayoutError> {
    // The bare form says the duration and nothing else: `animate = { width = 200 }`.
    let Value::Table(spec) = entry else {
        let field = format!("animate.{property}");
        if matches!(entry, Value::UserData(_)) {
            return Err(LayoutError::UnsupportedSignalProperty(field));
        }
        let duration = parse_millis(&field, "duration", entry, 1)?
            .ok_or_else(|| invalid(&field, format!("expected a duration in ms, got {}", preview_for_error(entry))))?;
        let motion = Motion::Eased { duration, easing: Easing::default() };
        return Ok(AnimationSpec { motion, delay: Duration::ZERO, from: None });
    };
    AnimationInput::read(property, spec).map_err(|error| error.under("animate."))?.into_animation(property)
}

impl AnimationInput {
    fn into_animation(self, property: &str) -> Result<AnimationSpec, LayoutError> {
        let field = format!("animate.{property}");
        let Self { duration, delay, easing, from, spring, keyframes, loops } = self;
        let from = match from {
            Value::Nil => None,
            value => Some(Animatable::from_value(property, Some(&value))?.ok_or_else(|| {
                invalid(&field, format!("`from` must be a value a tween can carry, got {}", preview_for_error(&value)))
            })?),
        };
        let delay = delay.unwrap_or_default();
        // Beside `keyframes` a spring is each segment's curve, so the list's `duration` and `loops` still apply.
        let keyed = !keyframes.is_nil();
        // Refuse timing fields a spring would ignore, so a config cannot tune a motion with inert keys, ADR-0152.
        if spring.is_some() {
            for (name, present) in [
                ("duration", duration.is_some() && !keyed),
                ("easing", easing.is_some()),
                ("loops", loops.is_some() && !keyed),
            ] {
                if present {
                    return Err(invalid(
                        &field,
                        format!("a `spring` has no `{name}`: what it does is decided by its stiffness and damping"),
                    ));
                }
            }
        } else if !keyed && loops.is_some() {
            return Err(invalid(&field, "`loops` counts the walks of a `keyframes` list, and this entry has none"));
        }
        let motion = match spring {
            Some(constants) if !keyed => Motion::Spring(constants.into_spring(&field)?),
            spring => {
                let duration = required_duration(&field, duration)?;
                let easing = easing.unwrap_or_default();
                let curve = match spring {
                    Some(constants) => Curve::Spring(constants.into_spring(&field)?),
                    None => Curve::Eased(easing),
                };
                match parse_sequence(property, &field, &keyframes, duration, curve, loops)? {
                    Some(sequence) => {
                        if from.is_some() {
                            return Err(invalid(
                                &field,
                                "`from` and `keyframes` say the same thing twice: a sequence starts on its own first frame",
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
}

/// The shared spec and every `(property, target)` pair of one `animate.exit` block.
pub(crate) struct ExitBlock {
    pub spec: AnimationSpec,
    pub targets: Vec<(&'static str, Animatable)>,
}

impl ExitBlock {
    const TIMING: &[&str] = &["duration", "delay", "easing", "spring"];
}

impl LuaType for ExitBlock {
    fn lua() -> String {
        "Exit".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        let fields: String = AnimationInput::lua_fields()
            .into_iter()
            .filter(|(key, ..)| Self::TIMING.contains(key))
            .map(|(key, optional_field, ty, _)| format!("{}: {ty}, ", optional(key.to_string(), optional_field)))
            .collect();
        out.push(format!("{{ {fields}[string]: any }}\n"));
    }
}

/// A spec's `easing`: a name, a four-number table read as CSS `cubic-bezier(x1, y1, x2, y2)`, or
/// `{ steps = n }` (ADR-0151). Absent is `InOutQuad`.
pub(super) fn parse_easing(field: &str, value: &Value) -> Result<Easing, LayoutError> {
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
                only_keys(field, table, Steps::KEYS)?;
                // The discriminator already ran __index; reading it again can return a different value.
                return Steps { steps: input::read(field, "steps", steps)? }.into_easing(field);
            }
            let mut points = BezierPoints::default();
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

impl Input for Easing {
    fn from_value(property: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        if value.is_nil() { Ok(None) } else { parse_easing(property, value).map(Some) }
    }
}

/// `animate.exit`'s block, resolved: `{ duration, easing, <property> = <target>, ... }`, one spec
/// for every named target. The targets are what the node eases to once the tree no longer holds
/// it (ADR-0150).
/// A block naming no target is a no-op and needs no duration, so it resolves to `None`.
pub(super) fn parse_exit(kind: &str, block: &Value) -> Result<Option<ExitBlock>, LayoutError> {
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
        if ExitBlock::TIMING.contains(&property.as_ref()) {
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
    Ok(Some(ExitBlock { spec: AnimationInput::read_fields("exit", exit)?.into_animation("exit")?, targets: out }))
}

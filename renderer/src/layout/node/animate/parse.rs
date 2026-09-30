//! `animate`'s Lua side: an entry table read into an [`AnimationSpec`], and `animate.exit` into
//! its targets.

use std::collections::BTreeMap;
use std::time::Duration;

use mlua::Value;

use super::super::prop::Prop;
use super::super::{LayoutError, PropMap, Property, fields, invalid, only_keys, preview_for_error, value_as_f32};
use super::easing::{BezierPoints, Steps};
#[cfg(test)]
use super::sequence::Loops;
use super::sequence::{Keyframe, parse_sequence};
#[cfg(test)]
use super::spring::SpringConstants;
use super::spring::{Spring, parse_spring};
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
        .filter(|name| *name != "animate")
        .ok_or_else(|| invalid(field, format!("`{property}` is not a property of a `{kind}` node")))
}

/// `animate`'s table, resolved: which properties ease and how. Absent means none. The table
/// itself may be a signal, resolved like any other property; entries inside it are plain values.
/// A name `kind` does not accept is refused, so a misspelling fails the pass instead of silently
/// snapping; what the value is decides whether it can tween ([`Animatable::from_value`]), the way
/// Qt registers interpolators by type rather than by property.
pub fn parse_animate(kind: &str, properties: &PropMap) -> Result<BTreeMap<&'static str, AnimationSpec>, LayoutError> {
    let Some(table) = fields::common::animate.read(properties)? else {
        return Ok(BTreeMap::new());
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
            only_keys(&format!("animate.{name}"), spec, AnimationInput::KEYS)?;
        }
        out.insert(name, parse_spec(name, &entry)?);
    }
    Ok(out)
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
    #[expect(dead_code, reason = "the parser's accepted keys and Lua input types")]
    struct AnimationInput {
        duration: Option<Duration>,
        delay: Duration as Option<Duration>,
        easing: Easing as Option<Easing>,
        from: Option<Animatable>,
        spring: Option<Spring> as Option<SpringConstants>,
        keyframes: Option<Vec<Keyframe>>,
        loops: Option<u32> as Option<Loops>,
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
/// (ADR-0154). `from` is read as a value of `property`.
pub(super) fn parse_spec(property: &str, entry: &Value) -> Result<AnimationSpec, LayoutError> {
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
pub(super) fn parse_millis(
    field: &str,
    what: &str,
    value: &Value,
    least: u64,
) -> Result<Option<Duration>, LayoutError> {
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
                let steps = value_as_f32(field, &steps)?
                    .ok_or_else(|| invalid(field, format!("`steps` is a count, got {}", preview_for_error(&steps))))?;
                if steps < 1.0 || steps > 1000.0 || steps.fract() != 0.0 {
                    return Err(invalid(field, format!("`steps` must be a whole count in [1, 1000], got {steps}")));
                }
                return Ok(Easing::Steps(steps as u32));
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
    Ok(Some(ExitBlock { spec: parse_spec("exit", block)?, targets: out }))
}

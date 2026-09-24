use std::rc::Rc;
use std::time::Duration;

use mlua::Value;

use super::easing::Easing;
use super::{Animatable, parse_easing, parse_millis};
use crate::layout::node::{LayoutError, invalid, only_keys, preview_for_error, value_as_f32};
use crate::lua::luacats::lua_shape;

// One stop in a keyframe list: a value, and how the segment arriving at it is timed. The first
// frame's own `duration` and `easing` are never read -- nothing eases into a beginning.
lua_shape! {
    /// A bare value, or a frame with its own timing. `duration = 0` jumps; repeating the previous value holds.
    #[alias = "Keyframe"]
    #[derive(Debug, Clone, PartialEq)]
    pub struct Keyframe {
        pub value: Animatable,
        pub duration: Duration as Option<Duration>,
        pub easing: Easing as Option<Easing>,
    }
}

/// A property walking a list of values, some number of times (ADR-0152).
#[derive(Debug, Clone, PartialEq)]
pub struct Sequence {
    /// At least two: the value it starts on, then one per segment. Shared rather than owned
    /// because the retained tree is cloned whole every frame and this list never changes after it
    /// is parsed -- a reference count moves instead of every keyframe in every running sequence.
    pub frames: Rc<[Keyframe]>,
    /// `None` is `Animation.Infinite`: it repeats for as long as the entry is there.
    pub loops: Option<u32>,
    /// One time through, which is every segment but the first frame's. Summed once here because
    /// both `at` and `done` want it on every tick of every running sequence, and the frames it
    /// sums are fixed when the entry is parsed.
    cycle: Duration,
}

impl Sequence {
    /// `None` when one time through would take no time, which is a list that is all jumps. Endless
    /// it would ask the compositor for a frame forever while showing one still value; counted it
    /// is a snap, which a config writes by leaving `animate` off the property (ADR-0152).
    fn new(frames: Vec<Keyframe>, loops: Option<u32>) -> Option<Self> {
        let cycle: Duration = frames.iter().skip(1).map(|frame| frame.duration).sum();
        (!cycle.is_zero()).then_some(Self { frames: frames.into(), loops, cycle })
    }

    /// The value `elapsed` into the run: the segment holding that instant, eased. A segment of no
    /// duration is a jump rather than a stop, so it is stepped over and its value shows only as
    /// the start of whatever follows.
    pub(super) fn at(&self, elapsed: Duration, property: &str) -> Animatable {
        let last = self.frames.last().expect("a parsed sequence has frames").value;
        if let Some(loops) = self.loops
            && elapsed >= self.cycle * loops
        {
            return last;
        }
        // The phase and the walk across segments stay in whole nanoseconds, and only the chosen
        // segment's fraction of itself becomes a float. An endless sequence runs for the life of
        // the process, and an `f32`'s 24-bit mantissa loses a millisecond of resolution after
        // about two hours of elapsed time and a whole 100 ms cycle after a fortnight, at which
        // point the phase stops advancing and the animation freezes and jumps. Subtracting the
        // segments in floats has the smaller version of the same fault: `0.4 - 0.3 < 0.1` holds in
        // `f32`, so an instant landing exactly on a boundary reads as just short of it and a jump
        // scheduled there waits for the next frame.
        let mut at = Duration::from_nanos((elapsed.as_nanos() % self.cycle.as_nanos()) as u64);
        for pair in self.frames.windows(2) {
            let (start, end) = (&pair[0], &pair[1]);
            if at < end.duration {
                let progress = end.easing.apply(at.as_secs_f32() / end.duration.as_secs_f32());
                return start.value.lerp(end.value, progress, property);
            }
            at -= end.duration;
        }
        last
    }

    /// Whether a run that started `elapsed` ago has played out. An infinite one never has.
    pub(super) fn done(&self, elapsed: Duration) -> bool {
        self.loops.is_some_and(|loops| elapsed >= self.cycle * loops)
    }
}

/// An entry's `keyframes` and `loops`, if it has them. A frame is a bare value, or a table naming
/// its own `duration` and `easing` in place of the entry's; the first frame is where the property
/// starts and the timing on it is never read. `loops` is a count or `"Infinite"`, one by default.
pub(super) fn parse_sequence(
    property: &str,
    field: &str,
    spec: &mlua::Table,
    keyframes: &Value,
    duration: Duration,
    easing: Easing,
) -> Result<Option<Sequence>, LayoutError> {
    let Value::Table(keyframes) = keyframes else {
        return match keyframes {
            Value::Nil => Ok(None),
            other => Err(invalid(field, format!("`keyframes` is a list of values, got {}", preview_for_error(other)))),
        };
    };
    let mut frames = Vec::new();
    for (index, frame) in keyframes.sequence_values::<Value>().enumerate() {
        let frame = frame.map_err(|e| invalid(field, e.to_string()))?;
        let at = format!("{field}.keyframes[{}]", index + 1);
        let (value, duration, easing) = match frame {
            // A frame that names nothing of its own is still a table when the value is one, so an
            // explicit `value` key is what tells the two apart.
            Value::Table(table) if table.contains_key("value").unwrap_or(false) => {
                only_keys(&at, &table, Keyframe::KEYS)?;
                let value: Value = table.get("value").map_err(|e| invalid(&at, e.to_string()))?;
                let own: Value = table.get("duration").map_err(|e| invalid(&at, e.to_string()))?;
                // Absent takes the entry's. Zero is allowed where the entry's own is not: a
                // segment that takes no time is a jump.
                let own = parse_millis(&at, "duration", &own, 0)?.unwrap_or(duration);
                let named: Value = table.get("easing").map_err(|e| invalid(&at, e.to_string()))?;
                let named = if named.is_nil() { easing } else { parse_easing(&at, &named)? };
                (value, own, named)
            }
            plain => (plain, duration, easing),
        };
        let value = Animatable::from_value(property, Some(&value))?.ok_or_else(|| {
            invalid(&at, format!("must be a value a tween can carry, got {}", preview_for_error(&value)))
        })?;
        frames.push(Keyframe { value, duration, easing });
    }
    if frames.len() < 2 {
        return Err(invalid(field, format!("`keyframes` needs at least two values, got {}", frames.len())));
    }
    // A list is read from index 1 until the first hole, so `{ [1] = 0, [2] = 1, [4] = 0 }` would
    // quietly become two frames. Count what the table actually holds and refuse the mismatch: a
    // config that miscounted its own loop should fail the pass, like every other typo here.
    let mut entries = 0usize;
    for pair in keyframes.pairs::<Value, Value>() {
        pair.map_err(|e| invalid(field, e.to_string()))?;
        entries += 1;
    }
    if entries != frames.len() {
        return Err(invalid(
            field,
            format!("`keyframes` is a list; it holds {entries} entries but only {} run from index 1", frames.len()),
        ));
    }
    let loops: Value = spec.get("loops").map_err(|e| invalid(field, e.to_string()))?;
    let loops = match &loops {
        Value::Nil => Some(1),
        Value::String(name) if name.to_str().is_ok_and(|name| name == "Infinite") => None,
        counted => match value_as_f32(field, counted)? {
            Some(count) if (1.0..=10_000.0).contains(&count) && count.fract() == 0.0 => Some(count as u32),
            _ => {
                return Err(invalid(
                    field,
                    format!(
                        "`loops` is a whole count in [1, 10000] or \"Infinite\", got {}",
                        preview_for_error(counted)
                    ),
                ));
            }
        },
    };
    Sequence::new(frames, loops).map(Some).ok_or_else(|| {
        invalid(field, "every `keyframes` segment lasts no time: a sequence that takes none is a jump".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::animate::tests::refused;
    use crate::layout::node::animate::*;
    use crate::layout::node::rect_props;

    impl AnimationSpec {
        fn sequence(&self) -> Option<Sequence> {
            match &self.motion {
                Motion::Sequence(sequence) => Some(sequence.clone()),
                _ => None,
            }
        }
    }

    /// ADR-0150: the exit block is checked by the same live pass that checks the rest of
    /// `animate`, so a typo is refused while the node is still in the tree to be named, not on the
    /// pass that removed it.
    /// ADR-0152: a bare value takes the entry's timing, a table names its own, and the first
    /// frame is only a starting point -- nothing eases into it, so its timing is never read.
    #[test]
    fn a_keyframe_list_takes_the_entrys_timing_unless_a_frame_names_its_own() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &rect_props(
                &lua,
                r#"return { animate = { opacity = { duration = 200, easing = "Linear", loops = "Infinite",
                    keyframes = { 1, 0.4, { value = 1, duration = 50, easing = "OutCubic" } } } } }"#,
            ),
        )
        .unwrap()
        .remove("opacity")
        .expect("opacity has a spec");
        let sequence = spec.sequence().expect("the entry named keyframes");
        assert_eq!(sequence.loops, None, "\"Infinite\" is no count at all");
        assert_eq!(sequence.frames.len(), 3);
        assert_eq!(sequence.frames[1].duration, Duration::from_millis(200), "the entry's duration");
        assert_eq!(sequence.frames[2].duration, Duration::from_millis(50), "its own");
        assert_eq!(sequence.frames[2].easing, Easing::OutCubic);
        assert_eq!(sequence.cycle, Duration::from_millis(250), "the first frame's timing is not in it");
    }

    /// The walk itself: each segment eases between its own two frames, an exhausted run holds the
    /// last one, and an endless run wraps rather than stopping.
    #[test]
    fn a_sequence_walks_its_frames_and_wraps_only_when_it_loops() {
        let lua = Lua::new();
        let sequence = |src: &str| {
            parse_animate("rect", &rect_props(&lua, src)).unwrap().remove("opacity").unwrap().sequence().unwrap()
        };
        // Compared with a tolerance: the wrap is an `f32` remainder, so 250 ms into a 200 ms cycle
        // lands a hair under 50 rather than on it.
        let at = |sequence: &Sequence, millis: u64| {
            let Animatable::Number(value) = sequence.at(Duration::from_millis(millis), "opacity") else {
                panic!("an opacity sequence carries numbers")
            };
            value
        };

        let once = sequence(
            r#"return { animate = { opacity = { duration = 100, easing = "Linear", keyframes = { 0, 1, 0 } } } }"#,
        );
        for (millis, want, why) in [
            (0, 0.0, "the first frame"),
            (50, 0.5, "halfway up"),
            (100, 1.0, "the first segment's end is the second's start"),
            (150, 0.5, "halfway back down"),
            (200, 0.0, "played out, holding the last frame"),
            (5_000, 0.0, "and holding it however long after"),
        ] {
            assert!((at(&once, millis) - want).abs() < 1e-5, "{why}: got {}", at(&once, millis));
        }
        assert!(once.done(Duration::from_millis(200)) && !once.done(Duration::from_millis(199)));

        let endless = sequence(
            r#"return { animate = { opacity = { duration = 100, easing = "Linear", loops = "Infinite",
                keyframes = { 0, 1, 0 } } } }"#,
        );
        assert!((at(&endless, 250) - 0.5).abs() < 1e-5, "back round the first segment: got {}", at(&endless, 250));
        assert!(!endless.done(Duration::from_secs(3_600)));
    }

    /// A frame of no duration is a jump, not a stop: it is stepped over, and its value shows as
    /// the start of whatever follows.
    #[test]
    fn a_frame_with_no_duration_jumps_and_the_frame_after_it_holds() {
        let lua = Lua::new();
        let sequence = parse_animate(
            "rect",
            &rect_props(
                &lua,
                r#"return { animate = { opacity = { duration = 100, loops = 2, keyframes = {
                    0, { value = 0, duration = 100 }, { value = 1, duration = 0 },
                    { value = 1, duration = 100 } } } } }"#,
            ),
        )
        .unwrap()
        .remove("opacity")
        .unwrap()
        .sequence()
        .unwrap();
        let at = |millis: u64| sequence.at(Duration::from_millis(millis), "opacity");
        assert_eq!(sequence.cycle, Duration::from_millis(200), "the jump costs no time");
        assert_eq!(at(50), Animatable::Number(0.0), "held dark");
        assert_eq!(at(100), Animatable::Number(1.0), "the jump lands and the hold at 1 begins");
        assert_eq!(at(150), Animatable::Number(1.0));
        assert_eq!(at(250), Animatable::Number(0.0), "second time round");
        assert_eq!(at(400), Animatable::Number(1.0), "two loops done, holding the last frame");
    }

    #[test]
    fn a_malformed_keyframe_list_is_refused() {
        let lua = Lua::new();
        let cases: [(&str, &[&str]); 7] = [
            ("keyframes = { 1 }", &["at least two"]),
            ("keyframes = 3", &["`keyframes` is a list"]),
            (r#"keyframes = { 1, "Fill" }"#, &["keyframes[2]"]),
            ("loops = 0, keyframes = { 1, 0 }", &["`loops`", "Infinite"]),
            ("keyframes = { 1, { value = 0, duration = -5 } }", &["keyframes[2]", "[0, 60000]"]),
            // A hole truncates the read at index 3, so the two frames that survive would have
            // passed the length check while the config quietly lost one.
            ("keyframes = { [1] = 1, [2] = 0, [4] = 1 }", &["3 entries", "index 1"]),
            ("from = 0, keyframes = { 1, 0 }", &["`from` and `keyframes`"]),
        ];
        for (entry, wanted) in cases {
            let text = refused(&lua, &format!("return {{ animate = {{ opacity = {{ duration = 1, {entry} }} }} }}"));
            assert!(wanted.iter().all(|want| text.contains(want)), "{entry}: {text}");
        }
    }

    /// An endless sequence runs as long as the shell does, so the phase has to come from whole
    /// nanoseconds rather than seconds in an `f32`, whose mantissa is out of milliseconds after a
    /// couple of hours and out of whole cycles after a fortnight.
    #[test]
    fn an_endless_sequence_keeps_its_phase_after_a_fortnight() {
        let lua = Lua::new();
        let sequence = parse_animate(
            "rect",
            &rect_props(
                &lua,
                r#"return { animate = { opacity = { duration = 100, easing = "Linear", loops = "Infinite",
                    keyframes = { 0, 1 } } } }"#,
            ),
        )
        .unwrap()
        .remove("opacity")
        .unwrap()
        .sequence()
        .unwrap();
        let at = |elapsed: Duration| match sequence.at(elapsed, "opacity") {
            Animatable::Number(value) => value,
            other => panic!("an opacity sequence carries numbers, got {other:?}"),
        };
        let fortnight = Duration::from_secs(14 * 24 * 60 * 60);
        for (offset, want) in [(0, 0.0), (25, 0.25), (50, 0.5), (75, 0.75)] {
            let elapsed = fortnight + Duration::from_millis(offset);
            assert!((at(elapsed) - want).abs() < 1e-5, "a fortnight and {offset} ms in: got {}", at(elapsed));
        }
        assert!(at(fortnight) != at(fortnight + Duration::from_millis(1)), "and it still moves per millisecond");
    }

    /// One time through takes no time only when every segment is a jump. Counted, that is a snap a
    /// config writes by leaving `animate` off the property; endless, it asks the compositor for a
    /// frame forever while showing one still value, which is a wakelock with nothing to show.
    #[test]
    fn a_sequence_whose_every_segment_is_a_jump_is_refused() {
        let lua = Lua::new();
        let text = refused(
            &lua,
            r#"return { animate = { width = { duration = 100, loops = "Infinite",
                keyframes = { 0, { value = 1, duration = 0 } } } } }"#,
        );
        assert!(text.contains("takes none is a jump"), "{text}");
    }

    /// The lead-in holds where the run opens. Every easing and every spring read elapsed zero as
    /// their own start, but a sequence opening on a jump plays that jump at zero, so the delay
    /// must not show the value after it.
    #[test]
    fn a_delay_before_a_sequence_holds_its_first_frame_rather_than_its_first_jump() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &rect_props(
                &lua,
                r#"return { animate = { width = { duration = 100, delay = 1000,
                    keyframes = { 40, { value = 0, duration = 0 }, 40 } } } }"#,
            ),
        )
        .unwrap()
        .remove("width")
        .unwrap();
        let started = Instant::now();
        let Motion::Sequence(ref sequence) = spec.motion else { panic!("a sequence") };
        let first = sequence.frames[0].value;
        let tween = Tween { property: "width", from: first, to: first, started, spec: spec.clone(), resting: false };
        assert_eq!(tween.at(started), Animatable::Number(40.0), "the lead-in holds the first frame");
        assert_eq!(tween.at(started + Duration::from_millis(999)), Animatable::Number(40.0), "for all of it");
        assert_eq!(tween.at(started + Duration::from_millis(1000)), Animatable::Number(0.0), "then the jump lands");
    }

    /// The phase walk stays in whole nanoseconds. In `f32` the segment subtraction rounds against
    /// the comparison -- `0.4 - 0.3 < 0.1` holds -- so an instant landing exactly on a boundary
    /// reads as a hair short of it and picks the segment that has just ended.
    #[test]
    fn an_instant_exactly_on_a_segment_boundary_belongs_to_the_segment_it_begins() {
        let lua = Lua::new();
        let spec = parse_animate(
            "rect",
            &rect_props(
                &lua,
                r#"return { animate = { width = { duration = 100, easing = "Linear",
                    keyframes = { 0, { value = 10, duration = 300 }, { value = 20, duration = 100 },
                        { value = 99, duration = 0 }, { value = 5, duration = 100 } } } } }"#,
            ),
        )
        .unwrap()
        .remove("width")
        .unwrap();
        let Motion::Sequence(sequence) = spec.motion else { panic!("a sequence") };
        // 300 + 100 lands exactly where the second segment ends and a jump to 99 fires. In floats
        // the walk arrives with 0.099999994 left of a 0.1 segment and reports itself still inside
        // it, a hair short of a jump that should already have happened.
        assert_eq!(sequence.at(Duration::from_millis(400), "width"), Animatable::Number(99.0));
    }

    /// A delay offsets a sequence's whole run once, not each time round: the phase is measured
    /// from the moment the first frame is left, so an endless loop keeps its cycle.
    #[test]
    fn a_delay_offsets_a_sequence_once_rather_than_every_cycle() {
        let lua = Lua::new();
        let specs = parse_animate(
            "rect",
            &rect_props(&lua, "return { animate = { opacity = { duration = 100, delay = 40, keyframes = { 0, 1 }, loops = \"Infinite\" } } }"),
        )
        .unwrap();
        let started = Instant::now();
        let tween = Tween {
            property: "opacity",
            from: Animatable::Number(0.0),
            to: Animatable::Number(1.0),
            started,
            spec: specs["opacity"].clone(),
            resting: false,
        };
        let at = |ms| match tween.at(started + Duration::from_millis(ms)) {
            Animatable::Number(n) => n,
            other => panic!("{other:?}"),
        };
        assert!(at(0) < 1e-6 && at(40) < 1e-6, "held on the first frame through the delay");
        assert!((at(90) - 0.5).abs() < 1e-3, "half a cycle in, 40 ms late");
        assert!((at(190) - 0.5).abs() < 1e-3, "and half of the next one, still 40 ms late");
    }
}

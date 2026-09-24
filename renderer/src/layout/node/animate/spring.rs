use std::time::{Duration, Instant};

use mlua::Value;

use super::{Animatable, Motion, Tween};
use crate::layout::node::{LayoutError, invalid, preview_for_error, value_as_f32};

/// Which closed-form solution a spring's constants put it in. Underdamped rings past the target,
/// overdamped crawls in without reaching it, and the boundary between them is its own formula
/// because both of the others divide by the distance to it.
enum Regime {
    /// Underdamped, carrying the frequency it rings at.
    Ringing(f32),
    /// Overdamped, carrying its two decay rates, the faster first.
    Crawling(f32, f32),
    /// Critically damped.
    Critical,
}

/// A mass on a spring, in units of the displacement it has left to cross: it starts one
/// displacement from the target and settles on it, so one scalar drives a number, a percent, a
/// colour and an edge table alike, and the rest threshold below is dimensionless rather than
/// needing to know pixels from opacity.
///
/// Solved in closed form rather than integrated per frame. [`Tween::at`] has to be a pure
/// function of elapsed time -- a pass and a tick both call it, and the value carries no state
/// across reconciliation (ADR-0152) -- so stepping a velocity forward per frame would be a
/// second source of truth and would drift with the frame rate. The closed form also hands over
/// an exact rate when the target moves, which is the whole reason a spring is here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    /// The pull toward the target, `k/m`, per second squared.
    pub stiffness: f32,
    /// The drag on the way, `c/m`, per second. `2 * sqrt(stiffness)` is critical damping: below
    /// it the spring overshoots and rings, above it it crawls in without ever crossing.
    pub damping: f32,
    /// Where the displacement is already heading when this run begins, as a fraction of that
    /// displacement per second. Zero for a spring starting at rest; a retarget hands the running
    /// spring's own rate over here, which is how the motion keeps its velocity through a change
    /// of target instead of restarting from still.
    pub velocity: f32,
    /// When the displacement is inside [`Spring::REST`] for good, computed once at parse from a
    /// bound on the envelope. Conservative on purpose: too long only keeps a tween that is
    /// already sitting on its target, while too short would drop it mid-flight.
    settles: Duration,
}

impl Spring {
    /// How close to the target counts as arrived, as a fraction of the original displacement.
    /// A thousandth of the distance is under half a pixel on anything this shell lays out and
    /// under a step of 8-bit colour.
    const REST: f32 = 1e-3;

    /// No spring may ask for frames for longer than this, whatever its constants say.
    const LONGEST: f32 = 60.0;

    /// The most velocity a hand-over may carry, in displacements per second. A target that lands
    /// almost where the value already is makes the projection below enormous; without a bound the
    /// next run would shoot away from a target it was arriving at.
    const FASTEST: f32 = 100.0;

    pub fn new(stiffness: f32, damping: f32, velocity: f32) -> Self {
        let mut spring = Self { stiffness, damping, velocity, settles: Duration::ZERO };
        spring.settles = Duration::from_secs_f32(spring.settle_time());
        spring
    }

    /// `zeta * omega0`, the rate the envelope decays at, and `omega0^2 - (zeta * omega0)^2`, whose
    /// sign says which of the three solutions applies. Both fall out of `damping` and `stiffness`
    /// alone, so every arm below reads them rather than recomputing the algebra.
    fn decay_and_gap(&self) -> (f32, f32) {
        let half_damping = self.damping / 2.0;
        (half_damping, self.stiffness - half_damping * half_damping)
    }

    /// Displacement left, as a fraction of the original: `1` when the run begins and `0` on the
    /// target. It may pass through zero and come back, which is the overshoot, and the caller
    /// clamps that to the property's own range exactly as it does `OutBack`'s.
    fn displacement(&self, seconds: f32) -> f32 {
        let (decay, gap) = self.decay_and_gap();
        // `s(0) = 1` and `s'(0) = -velocity` in all three arms: the run starts one displacement
        // out and is already closing at `velocity` displacements per second.
        let slope = decay - self.velocity;
        match self.regime(gap) {
            Regime::Ringing(ringing) => {
                (-decay * seconds).exp() * ((ringing * seconds).cos() + slope / ringing * (ringing * seconds).sin())
            }
            Regime::Crawling(fast, slow) => {
                let (near, far) = self.overdamped_weights(fast, slow);
                near * (fast * seconds).exp() + far * (slow * seconds).exp()
            }
            Regime::Critical => (1.0 + slope * seconds) * (-decay * seconds).exp(),
        }
    }

    /// The rate `displacement` is changing at, which is negative while the spring closes on its
    /// target. What a retarget hands to the next run.
    fn rate(&self, seconds: f32) -> f32 {
        let (decay, gap) = self.decay_and_gap();
        let slope = decay - self.velocity;
        match self.regime(gap) {
            // Differentiating `exp(-decay t) * (cos(w t) + (slope / w) sin(w t))` and collecting
            // the two terms; at `t = 0` it is `slope - decay`, which is `-velocity`.
            Regime::Ringing(ringing) => {
                let (cos, sin) = ((ringing * seconds).cos(), (ringing * seconds).sin());
                (-decay * seconds).exp() * ((slope - decay) * cos - (ringing + slope * decay / ringing) * sin)
            }
            Regime::Crawling(fast, slow) => {
                let (near, far) = self.overdamped_weights(fast, slow);
                near * fast * (fast * seconds).exp() + far * slow * (slow * seconds).exp()
            }
            Regime::Critical => (slope - decay * (1.0 + slope * seconds)) * (-decay * seconds).exp(),
        }
    }

    /// Which of the three solutions applies. The comparison is against a fraction of `stiffness`
    /// rather than a fixed number because `gap` is in units of stiffness: an absolute threshold
    /// would call a soft spring critical and a stiff one never.
    fn regime(&self, gap: f32) -> Regime {
        if gap.abs() <= self.stiffness * 1e-6 {
            return Regime::Critical;
        }
        if gap > 0.0 {
            return Regime::Ringing(gap.sqrt());
        }
        let (decay, _) = self.decay_and_gap();
        let spread = (-gap).sqrt();
        // The far root is the sum of two terms of one sign, so it loses nothing. The near one is
        // their difference, and for a heavily overdamped spring they agree to every bit an `f32`
        // has: `stiffness = 0.0001, damping = 10000` gives `spread` exactly `decay`, a near root
        // of exactly zero, and a displacement that never changes -- the value would sit still for
        // a minute and then jump. The roots multiply to `stiffness`, so the near one comes from
        // the far one instead of from a subtraction.
        let far = -decay - spread;
        Regime::Crawling(far, self.stiffness / far)
    }

    /// How the starting displacement and rate split between those two rates.
    fn overdamped_weights(&self, fast: f32, slow: f32) -> (f32, f32) {
        let near = (-self.velocity - slow) / (fast - slow);
        (near, 1.0 - near)
    }

    /// A bound on how long the displacement takes to fall inside [`Self::REST`] and stay there.
    /// Every arm bounds the solution above by `amplitude * exp(-rate * t)` and inverts that, so
    /// the answer is never early. The critically damped arm carries a linear factor that no plain
    /// exponential bounds, so it is charged to half the decay and the factor's own maximum.
    fn settle_time(&self) -> f32 {
        let (decay, gap) = self.decay_and_gap();
        let slope = decay - self.velocity;
        let (amplitude, rate) = match self.regime(gap) {
            Regime::Ringing(ringing) => ((1.0 + (slope / ringing).powi(2)).sqrt(), decay),
            Regime::Crawling(fast, slow) => {
                let (near, far) = self.overdamped_weights(fast, slow);
                // The slower root is the one still moving once the other has gone.
                (near.abs() + far.abs(), -slow)
            }
            Regime::Critical => {
                let half = decay / 2.0;
                // `(1 + slope * t) * exp(-half * t)` peaks where its derivative vanishes; before
                // that point it has not yet grown, so the value at `t = 0` stands.
                let peak = (1.0 / half - 1.0 / slope.abs()).max(0.0);
                ((1.0 + slope.abs() * peak) * (-half * peak).exp(), half)
            }
        };
        if rate <= 0.0 {
            return Self::LONGEST;
        }
        ((amplitude / Self::REST).max(1.0).ln() / rate).clamp(0.0, Self::LONGEST)
    }

    /// Progress toward the target: `0` at the start, `1` on it, and past `1` while it overshoots.
    /// Pinned exactly to `1` once settled so the property lands on the value a pass resolved
    /// rather than a thousandth away from it.
    pub(super) fn at(&self, elapsed: Duration) -> f32 {
        if elapsed >= self.settles {
            return 1.0;
        }
        1.0 - self.displacement(elapsed.as_secs_f32())
    }

    pub(super) fn done(&self, elapsed: Duration) -> bool {
        elapsed >= self.settles
    }

    /// The pair a config wrote, apart from the velocity a retarget handed this one. Two springs
    /// agreeing here are the same spring at different points of the same motion.
    pub(super) fn constants(&self) -> (f32, f32) {
        (self.stiffness, self.damping)
    }

    /// This spring's constants, started at the rate `running` had reached rather than at rest.
    ///
    /// Both runs read `value = to + s * (from - to)`, so the value's own rate is `s'` times the
    /// displacement it is measured against. Matching the two across the hand-over gives the new
    /// run's starting rate as the old one's projected onto the new displacement, which is exact
    /// for a single number and the closest one scalar comes for a colour or an edge table whose
    /// components are not moving in step.
    ///
    /// ponytail: one scalar for every shape, so a colour crossing a hue keeps its speed but not
    /// its direction per channel. Carrying an `Animatable`-shaped velocity would fix that, and is
    /// worth doing when something animates a colour by spring and the difference shows.
    pub(super) fn handed(self, running: &Tween, displayed: Animatable, target: Animatable, now: Instant) -> Self {
        let Motion::Spring(prior) = &running.spec.motion else { return self };
        // The run's whole displacement, not what is left of it. `displayed - running.to` is that
        // whole displacement already scaled by how much remains, so projecting it would hand over
        // the true velocity times the fraction still to cross: near zero for a retarget late in a
        // run that is still moving briskly, and sign-flipped once an overshoot has carried the
        // value past its target.
        let was = running.from.delta(running.to);
        let becomes = displayed.delta(target);
        let square: f32 = becomes.iter().map(|axis| axis * axis).sum();
        if square <= f32::EPSILON {
            return self;
        }
        let projected: f32 = was.iter().zip(becomes).map(|(old, new)| old * new).sum::<f32>() / square;
        // `rate` is negative while a spring closes, and `velocity` counts the same motion as
        // positive, hence the sign. Bounded so that a hand-over onto a displacement of almost
        // nothing cannot fling the next run across the screen.
        let carried = -prior.rate(running.progressed(now).as_secs_f32()) * projected;
        Self::new(self.stiffness, self.damping, carried.clamp(-Self::FASTEST, Self::FASTEST))
    }
}

/// An entry's `spring`, if it has one: `{ stiffness, damping }`, both required and both positive.
/// No defaults, because a spring whose constants are implicit is a mystery to read and to tune,
/// and no `mass`: it divides out of both constants, so naming it would be a third number that
/// only rescales the two above.
pub(super) fn parse_spring(field: &str, spec: &mlua::Table) -> Result<Option<Spring>, LayoutError> {
    let spring: Value = spec.get("spring").map_err(|e| invalid(field, e.to_string()))?;
    let Value::Table(spring) = spring else {
        return match spring {
            Value::Nil => Ok(None),
            other => Err(invalid(
                field,
                format!("`spring` is a table of `stiffness` and `damping`, got {}", preview_for_error(&other)),
            )),
        };
    };
    let read = |name: &str, highest: f32| -> Result<f32, LayoutError> {
        let at = format!("{field}.spring.{name}");
        let value: Value = spring.get(name).map_err(|e| invalid(&at, e.to_string()))?;
        match value_as_f32(&at, &value)? {
            Some(number) if number > 0.0 && number <= highest => Ok(number),
            _ => Err(invalid(
                &at,
                format!("`{name}` is a number within (0, {highest}], got {}", preview_for_error(&value)),
            )),
        }
    };
    let stiffness = read("stiffness", 100_000.0)?;
    let damping = read("damping", 10_000.0)?;
    // A run that a pass starts fresh is at rest; `retarget` is the only thing that begins one
    // already moving, and it rebuilds the spring to say so.
    Ok(Some(Spring::new(stiffness, damping, 0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::animate::tests::{refused, spec};
    use crate::layout::node::animate::*;
    use crate::layout::node::rect_props;

    /// The three regimes and one hand-over, against a Runge-Kutta integration of
    /// `s'' + damping * s' + stiffness * s = 0` done outside this module -- not against another
    /// arm of the same `match`, which is the tautology the easing table was rewritten to avoid.
    #[test]
    fn every_spring_regime_matches_an_integration_of_the_equation_it_solves() {
        /// A spring's constants, its starting velocity, and what the integration says its
        /// displacement and rate are at three instants.
        struct Row {
            name: &'static str,
            stiffness: f32,
            damping: f32,
            velocity: f32,
            at: [(f32, f32, f32); 3],
        }
        #[rustfmt::skip]
        let reference = &[
            Row { name: "underdamped",   stiffness: 200.0, damping: 10.0, velocity: 0.0, at: [
                (0.05,  0.795370, -7.232_43), (0.10,  0.371074, -8.88951), (0.25, -0.300436,  0.714015)] },
            Row { name: "critical",      stiffness: 100.0, damping: 20.0, velocity: 0.0, at: [
                (0.05,  0.909796, -3.032653), (0.10,  0.735759, -3.678794), (0.25,  0.287297, -2.052125)] },
            Row { name: "overdamped",    stiffness: 100.0, damping: 40.0, velocity: 0.0, at: [
                (0.05,  0.930295, -2.0781),   (0.10,  0.822263, -2.139091), (0.25,  0.551353, -1.477107)] },
            Row { name: "with velocity", stiffness: 200.0, damping: 10.0, velocity: 3.0, at: [
                (0.05,  0.686884, -8.533672), (0.10,  0.237731, -8.669304), (0.25, -0.289726,  1.508_22)] },
        ];
        for Row { name, stiffness, damping, velocity, at } in reference {
            let spring = Spring::new(*stiffness, *damping, *velocity);
            for (seconds, displacement, rate) in at {
                let got = spring.displacement(*seconds);
                assert!(
                    (got - displacement).abs() < 1e-4,
                    "{name} displacement at {seconds}s: got {got}, want {displacement}"
                );
                let got = spring.rate(*seconds);
                assert!((got - rate).abs() < 1e-3, "{name} rate at {seconds}s: got {got}, want {rate}");
            }
        }
    }

    #[test]
    fn a_spring_starts_on_its_source_ends_on_its_target_and_is_allowed_to_overshoot() {
        let spring = Spring::new(200.0, 10.0, 0.0);
        assert!(spring.at(Duration::ZERO).abs() < 1e-6, "no progress before it moves");
        assert!(spring.at(Duration::from_millis(250)) > 1.0, "an underdamped spring passes its target");
        assert_eq!(spring.at(spring.settles), 1.0, "and is pinned exactly on it once settled");
        assert_eq!(spring.at(spring.settles * 2), 1.0);

        // The overshoot is the property's own range to absorb, exactly as `OutBack`'s is.
        let past = spring.at(Duration::from_millis(250));
        assert_eq!(
            Animatable::Number(1.0).lerp(Animatable::Number(0.0), past, "opacity"),
            Animatable::Number(0.0),
            "opacity cannot go negative"
        );
    }

    /// Every spring has to stop asking for frames, and every spring has to move. The second half
    /// is what the extreme pairs are here for: a heavily overdamped one whose near root is the
    /// difference of two `f32` that agree to the last bit gets exactly zero and holds its starting
    /// value for the full minute before snapping, which a settles-only assertion passes.
    #[test]
    fn every_spring_settles_within_a_minute_and_none_settles_before_it_arrives() {
        for (stiffness, damping) in [
            (200.0, 10.0),
            (100.0, 20.0),
            (100.0, 40.0),
            (1.0, 0.01),
            (100_000.0, 10_000.0),
            (0.0001, 10_000.0),
            (100_000.0, 0.0001),
            (0.0001, 0.0001),
        ] {
            let spring = Spring::new(stiffness, damping, 0.0);
            // Sampled across the whole window rather than at one instant: a barely damped spring
            // is still ringing at its midpoint and may be heading either way there, so no single
            // reading says whether it is alive. What every one of these must not be is *still* --
            // a displacement of exactly `1` at every instant, with a rate of exactly `0`, which is
            // the shape the cancellation produced. A spring in treacle is allowed to be slow, and
            // one of these pairs really does need a hundred million seconds.
            let mut moved = false;
            for step in 0..=20 {
                let seconds = spring.settles.as_secs_f32() * step as f32 / 20.0;
                let (displacement, rate) = (spring.displacement(seconds), spring.rate(seconds));
                assert!(displacement.is_finite() && rate.is_finite(), "{stiffness}/{damping}: {displacement}, {rate}");
                moved |= displacement != 1.0;
            }
            assert!(moved, "{stiffness}/{damping} holds its starting value at every instant");
        }
        for (stiffness, damping) in [(200.0, 10.0), (100.0, 20.0), (100.0, 40.0), (1.0, 0.01), (100_000.0, 10_000.0)] {
            let spring = Spring::new(stiffness, damping, 0.0);
            assert!(spring.settles <= Duration::from_secs_f32(Spring::LONGEST), "{stiffness}/{damping} never stops");
            assert!(spring.done(spring.settles), "{stiffness}/{damping} is not done when it says it is");
            // Whatever it says, it really is within `REST` of the target by then -- the bound is
            // allowed to be late, never early.
            let displacement = spring.displacement(spring.settles.as_secs_f32());
            assert!(
                displacement.abs() <= Spring::REST || spring.settles.as_secs_f32() >= Spring::LONGEST,
                "{stiffness}/{damping} calls itself settled {displacement} out"
            );
        }
    }

    /// The whole reason a spring is here: a target that moves mid-flight bends the motion instead
    /// of restarting it. An eased tween has no way to do this, so it is the comparison.
    #[test]
    fn a_retargeted_spring_keeps_the_speed_it_had_where_an_easing_would_start_over() {
        let lua = Lua::new();
        let sprung = spec(&lua, "return { animate = { width = { spring = { stiffness = 200, damping = 10 } } } }");
        let started = Instant::now();
        let running = Tween {
            property: "width",
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec: sprung.clone(),
            resting: false,
        };
        let midway = started + Duration::from_millis(50);
        let displayed = running.at(midway);
        assert!(matches!(displayed, Animatable::Number(n) if n > 0.0 && n < 100.0), "{displayed:?}");

        // The target moves further out in the same direction; the new run must already be moving.
        let Motion::Spring(fresh) = sprung.motion else { panic!("parsed a spring") };
        let handed = fresh.handed(&running, displayed, Animatable::Number(200.0), midway);
        assert!(handed.velocity > 0.0, "the hand-over carries the speed it had, got {}", handed.velocity);
        assert!(
            handed.rate(0.0) < fresh.rate(0.0),
            "and so closes faster at its first instant than a run starting from still"
        );

        // A target that moves the other way hands over a velocity pointing away from it, which is
        // what makes the value swing through rather than snap back.
        let backwards = fresh.handed(&running, displayed, Animatable::Number(-50.0), midway);
        assert!(backwards.velocity < 0.0, "got {}", backwards.velocity);
    }

    /// The hand-over's magnitude, not just its sign. Matching the value's own rate across the
    /// swap gives `-rate(t) * (D_old . D_new) / |D_new|^2`, and the run's displacement there is
    /// the whole of it -- `from - to`, not `displayed - to`, which is that same displacement
    /// already scaled by how much is left to cross. Scaling it twice hands over almost nothing
    /// exactly when the hand-over matters most: late in a run, or once an overshoot has taken the
    /// value past its target and the leftover has changed sign.
    #[test]
    fn the_handed_velocity_matches_the_rate_the_value_was_actually_moving_at() {
        let spring = Spring::new(200.0, 10.0, 0.0);
        let started = Instant::now();
        let running = Tween {
            property: "width",
            from: Animatable::Number(0.0),
            to: Animatable::Number(100.0),
            started,
            spec: AnimationSpec { motion: Motion::Spring(spring), delay: Duration::ZERO, from: None },
            resting: false,
        };
        // 220 ms in, an underdamped spring of these constants is past its target and coming back,
        // so the displacement left has the opposite sign to the one it started with.
        for millis in [20, 60, 220] {
            let now = started + Duration::from_millis(millis);
            let displayed = running.at(now);
            let target = Animatable::Number(300.0);
            let handed = spring.handed(&running, displayed, target, now);

            let old_span = -100.0_f32;
            let Animatable::Number(shown) = displayed else { panic!("a number") };
            let new_span = shown - 300.0;
            let want = -spring.rate(Duration::from_millis(millis).as_secs_f32()) * old_span / new_span;
            assert!((handed.velocity - want).abs() < 1e-3, "at {millis} ms: handed {}, want {want}", handed.velocity);
        }
    }

    #[test]
    fn a_moving_spring_survives_a_pass_that_changes_nothing_about_it() {
        let lua = Lua::new();
        let source = "return { width = 300, animate = { width = { spring = { stiffness = 200, damping = 10 } } } }";
        let started = Instant::now();
        let carried = Spring::new(200.0, 10.0, 40.0);
        let running = [Tween {
            property: "width",
            from: Animatable::Number(0.0),
            to: Animatable::Number(300.0),
            started,
            spec: AnimationSpec { motion: Motion::Spring(carried), delay: Duration::ZERO, from: None },
            resting: false,
        }];
        let shown: PropMap = PropMap::from_iter([("width", Value::Number(120.0))]);

        let mut properties = rect_props(&lua, source);
        let now = started + Duration::from_millis(30);
        let tweens = retarget("rect", Some((&running[..], &shown)), &mut properties, now, &lua).unwrap();
        let Motion::Spring(kept) = tweens[0].spec.motion else { panic!("still a spring") };
        assert_eq!(kept.velocity, 40.0, "the handed velocity survives an unrelated resolve");
        assert_eq!(tweens[0].started, started, "and the run is the same run, not a fresh one");

        // Only the spring is carried, not the entry around it: an edited `delay` beside untouched
        // constants still lands, where taking the running spec whole would have swallowed it.
        let waiting = [Tween {
            spec: AnimationSpec { motion: Motion::Spring(carried), delay: Duration::from_millis(1000), from: None },
            ..running[0].clone()
        }];
        let mut properties = rect_props(&lua, source);
        let tweens = retarget("rect", Some((&waiting[..], &shown)), &mut properties, now, &lua).unwrap();
        let Motion::Spring(kept) = tweens[0].spec.motion else { panic!("still a spring") };
        assert_eq!(kept.velocity, 40.0, "the handed rate carries");
        assert_eq!(tweens[0].spec.delay, Duration::ZERO, "and the edited delay lands on it");

        // Editing either constant is a config change: the new spring arrives at its parsed rest.
        // The run it lands in is still the one already going -- nothing here restarts a tween whose
        // target never moved, so `started` and `from` are the running one's.
        let stiffer = "return { width = 300, animate = { width = { spring = { stiffness = 400, damping = 10 } } } }";
        let mut properties = rect_props(&lua, stiffer);
        let tweens = retarget("rect", Some((&running[..], &shown)), &mut properties, now, &lua).unwrap();
        let Motion::Spring(fresh) = tweens[0].spec.motion else { panic!("still a spring") };
        assert_eq!((fresh.stiffness, fresh.velocity), (400.0, 0.0), "an edited constant is a new spring");
        assert_eq!(tweens[0].started, started, "carried by the run already going");
        assert_eq!(tweens[0].from, Animatable::Number(0.0), "which keeps the value it set out from");
    }

    #[test]
    fn a_spring_is_two_positive_numbers_and_says_nothing_about_duration() {
        let lua = Lua::new();

        let Motion::Spring(spring) =
            spec(&lua, "return { animate = { width = { spring = { stiffness = 220, damping = 26 } } } }").motion
        else {
            panic!("expected a spring")
        };
        assert_eq!((spring.stiffness, spring.damping, spring.velocity), (220.0, 26.0, 0.0));

        let cases: [(&str, &[&str]); 6] = [
            ("duration = 10, spring = { stiffness = 1, damping = 1 }", &["has no `duration`"]),
            ("spring = { stiffness = 1, damping = 1 }, keyframes = { 0, 1 }", &["two different motions"]),
            ("spring = { damping = 26 }", &["stiffness", "(0, 100000]"]),
            ("spring = { stiffness = 220, damping = 0 }", &["damping", "(0, 10000]"]),
            ("spring = 220", &["table of `stiffness` and `damping`"]),
            // Without a spring the duration is still required, so lifting it is scoped to the one.
            (r#"easing = "Linear""#, &["expected a duration in ms"]),
        ];
        for (entry, wanted) in cases {
            let text = refused(&lua, &format!("return {{ animate = {{ width = {{ {entry} }} }} }}"));
            assert!(wanted.iter().all(|want| text.contains(want)), "{entry}: {text}");
        }
    }
}

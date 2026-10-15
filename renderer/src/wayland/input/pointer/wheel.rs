//! The wheel: a scroll signal's offset (ADR-0069) or a node's `on_wheel` (ADR-0116 decision 2).

use shared::warn;

use super::*;
use crate::lua::call_logged;
use crate::lua::signal::Signal;

/// Mouse-wheel notch size in logical pixels (ADR-0069 decision 6): flat 39, about three
/// lines of 13px text. A per-container step would need a font size. Touchpad pixels divide by it
/// only for `on_wheel` fractions.
const WHEEL_STEP_PIXELS: f32 = 39.0;

const VALUE120_PER_NOTCH: f32 = 120.0;

/// Scroll distance in logical pixels (ADR-0069 decision 6). Use touchpad `pixels` as sent; use
/// `value120` (120 per notch) only without pixels, or compositors sending both double the motion.
fn wheel_delta(pixels: f64, value120: i32) -> f32 {
    if pixels != 0.0 {
        return pixels as f32;
    }
    value120 as f32 / VALUE120_PER_NOTCH * WHEEL_STEP_PIXELS
}

/// Whole notches may ease under `animate.scroll`. A high-resolution wheel's fractions arrive many
/// to a notch and follow the hand like a touchpad's pixels, rather than restart the run each event.
fn is_notch(value120: i32) -> bool {
    value120 != 0 && value120 % VALUE120_PER_NOTCH as i32 == 0
}

/// `on_wheel`'s notches (ADR-0116 decision 2 amendment), positive away from the user. `value120`
/// wins: a wheel's pixels are not `WHEEL_STEP_PIXELS` per notch.
fn wheel_steps(pixels: f64, value120: i32) -> f32 {
    if value120 != 0 {
        return -(value120 as f32) / VALUE120_PER_NOTCH;
    }
    -(pixels as f32) / WHEEL_STEP_PIXELS
}

/// Deepest scrollable node in a hit path: its depth, signal and axis.
fn scroll_holder(path: &[&layout::ResolvedNode]) -> Option<(usize, Signal, layout::scene::MainAxis)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        let signal = layout::node::signal_at(&node.properties, "scroll")?;
        let axis = layout::scene::main_axis_of(node.kind, &node.properties).ok()??;
        Some((depth, signal, axis))
    })
}

/// A touchpad scroll under way: the signal and the fingers' cumulative travel, sampled by the
/// `on_drag` velocity sampler.
pub(in crate::wayland) struct Fingers {
    signal: Signal,
    travel: f64,
    motion: Motion,
}

impl Fingers {
    /// Tracks finger-source motion; any other source, or another signal, starts over. On `stop`
    /// returns the signal and the px/s the fingers left with.
    fn feed(
        state: &mut Option<Self>,
        time: u32,
        source: Option<wl_pointer::AxisSource>,
        moved: Option<(Signal, f32)>,
        stop: bool,
    ) -> Option<(Signal, f32)> {
        if source != Some(wl_pointer::AxisSource::Finger) {
            *state = None;
            return None;
        }
        if let Some((signal, delta)) = moved {
            if state.as_ref().is_none_or(|held| held.signal.cell_id() != signal.cell_id()) {
                *state = Some(Self { signal, travel: 0.0, motion: Motion::default() });
            }
            if let Some(held) = state {
                held.travel += f64::from(delta);
                held.motion.velocity(Some((time, (held.travel, 0.0))));
            }
        }
        let mut held = state.take_if(|_| stop)?;
        // The lift is a sample too: fingers that rested before it read as still.
        let (velocity, _) = held.motion.velocity(Some((time, (held.travel, 0.0))));
        Some((held.signal, velocity as f32))
    }
}

impl App {
    /// Feeds one axis event to [`Fingers`]; on the lift, sends the offset coasting.
    pub(in crate::wayland::input) fn coast_after_fingers(
        &mut self,
        time: u32,
        source: Option<wl_pointer::AxisSource>,
        moved: Option<(Signal, f32)>,
        stop: bool,
    ) {
        let Some((signal, velocity)) = Fingers::feed(&mut self.finger_scroll, time, source, moved, stop) else {
            return;
        };
        let flung = self.client.fling(&signal, velocity, std::time::Instant::now());
        if !flung.is_empty() {
            self.mark_surfaces_stale(&flung);
        }
    }

    /// A press stops a glide under the pointer.
    pub(in crate::wayland::input) fn stop_glide_under(&mut self, index: usize, position: (f64, f64)) {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else { return };
        let path = layout::hit::hit_path(tree, layout::hit::LogicalPoint::from_position(position));
        if let Some((_, signal, _)) = scroll_holder(&path) {
            self.client.stop_scroll(&signal);
        }
    }

    /// Apply one wheel event to the deepest scrollable or `on_wheel` node (ADR-0116 decision 2).
    /// Use touchpad pixels or `value120` (ADR-0069 decision 6); ignore deprecated `discrete`,
    /// which compositors that still send also accompany with `value120`.
    /// Innermost wins with no parent chaining: a wheel over a list stops there at its end, unlike
    /// browser chaining to the parent; no config needs those edge cases. The offset written is
    /// unclamped: `layout::scene` owns the bound and writes back what it used. Returns the signal moved
    /// and the distance, for a fling to measure.
    /// `on_wheel` receives the vertical axis only.
    pub(in crate::wayland::input) fn scroll_at(
        &mut self,
        index: usize,
        position: (f64, f64),
        horizontal_px: f64,
        horizontal_steps: i32,
        vertical_px: f64,
        vertical_steps: i32,
    ) -> Option<(crate::lua::signal::Signal, f32)> {
        let surface_id = self.surfaces[index].surface_id.clone();
        let tree = self.client.scene().surface(&surface_id)?;
        let point = layout::hit::LogicalPoint::from_position(position);
        let path = layout::hit::hit_path(tree, point);
        // Deepest scrollable under the pointer wins.
        let scrollable = scroll_holder(&path);
        let wheel = wheel_target(&path);
        // A multiline field deeper than both scrolls its own rows while it has rows to scroll.
        let field = path.iter().rposition(|node| node.kind == "textfield").filter(|depth| {
            scrollable.as_ref().is_none_or(|(scroll_depth, ..)| depth > scroll_depth)
                && wheel.as_ref().is_none_or(|(wheel_depth, ..)| depth > wheel_depth)
        });
        let field = field.map(|depth| path[depth].id);
        drop(path);
        if let Some(id) = field
            && self.wheel_field(&surface_id, id, wheel_delta(vertical_px, vertical_steps))
        {
            return None;
        }
        if let Some((depth, rect, on_wheel)) = wheel
            && scrollable.as_ref().is_none_or(|(scroll_depth, ..)| depth > *scroll_depth)
        {
            let steps = wheel_steps(vertical_px, vertical_steps);
            if steps == 0.0 {
                return None;
            }
            let on_wheel = on_wheel.clone();
            let lua = self.client.lua();
            match (rect_table(lua, rect), modifiers_table(lua, self.modifiers_held())) {
                (Ok(rect), Ok(held)) => {
                    call_logged(&on_wheel, (rect, steps, held), format_args!("{surface_id}: on_wheel"))
                }
                (Err(e), _) | (_, Err(e)) => warn!("{surface_id}: could not build on_wheel's arguments: {e}"),
            }
            return None;
        }
        let (_, signal, axis) = scrollable?;
        let (pixels, steps) = match axis {
            layout::scene::MainAxis::Horizontal => (horizontal_px, horizontal_steps),
            layout::scene::MainAxis::Vertical => (vertical_px, vertical_steps),
        };
        let delta = wheel_delta(pixels, steps);
        if delta == 0.0 {
            return None;
        }
        let moved = self.client.wheel(&signal, delta, is_notch(steps), std::time::Instant::now());
        // A pass would re-resolve nothing a scroll changes; the tree's own rects are enough.
        if !moved.is_empty() {
            self.mark_surfaces_stale(&moved);
            self.apply_resolved_surface_state_for(&[&moved]);
            self.refresh_hover_after_layout();
        }
        Some((signal, delta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0069 decision 6. The rest of `scroll_at` needs a compositor to deliver a notch;
    /// this is the half that does not.
    #[test]
    fn a_touchpads_pixels_are_used_as_sent_and_a_wheels_steps_are_converted_without_narrowing() {
        assert_eq!(wheel_delta(17.5, 0), 17.5, "a touchpad reports a distance and it is taken");
        assert_eq!(wheel_delta(-17.5, 0), -17.5, "including upward");
        assert_eq!(wheel_delta(0.0, 120), WHEEL_STEP_PIXELS, "one notch is one step");
        assert_eq!(wheel_delta(0.0, -240), -2.0 * WHEEL_STEP_PIXELS, "two notches up");
        assert_eq!(wheel_delta(0.0, 60), WHEEL_STEP_PIXELS / 2.0, "high-resolution wheels send fractions of a notch");
        // A compositor sending both is sending the same motion twice: the distance wins.
        assert_eq!(wheel_delta(17.5, 120), 17.5);
        assert_eq!(wheel_delta(0.0, 0), 0.0, "an event carrying no motion scrolls nothing");
        // Reachable rather than absurd: `AxisScroll::merge` sums `value120` across every axis event
        // queued before the next `Frame` (smithay-client-toolkit 0.21.1), so one frame of dispatch
        // lag behind a fast wheel accumulates past `i16::MAX`. Narrowing it to an `i16` turned that
        // into a delta of zero, so a hard flick scrolled nothing.
        assert_eq!(wheel_delta(0.0, 120 * 400), 400.0 * WHEEL_STEP_PIXELS, "a large step count still scrolls");
    }

    /// Fingers lifting at speed coast; a rest before the lift, a wheel or a hard stop do not.
    #[test]
    fn only_finger_motion_leaves_a_speed_and_the_lift_reads_it() {
        use wl_pointer::AxisSource::{Finger, Wheel};
        let lua = crate::layout::node::scene_lua();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        let moved = |delta| Some((signal.clone(), delta));
        let mut state = None;
        assert!(Fingers::feed(&mut state, 1000, Some(Finger), moved(10.0), false).is_none(), "moving, not lifted");
        Fingers::feed(&mut state, 1050, Some(Finger), moved(20.0), false);
        let (_, speed) = Fingers::feed(&mut state, 1060, Some(Finger), None, true).expect("a lift");
        assert!((speed - 20.0 / 0.06).abs() < 0.1, "20 px after the first sample, over its 60 ms: {speed}");
        assert!(state.is_none(), "the lift ends the gesture");

        Fingers::feed(&mut state, 2000, Some(Finger), moved(10.0), false);
        Fingers::feed(&mut state, 2050, Some(Finger), moved(10.0), false);
        let (_, speed) = Fingers::feed(&mut state, 2400, Some(Finger), None, true).expect("a lift");
        assert_eq!(speed, 0.0, "a rest before the lift reads still");

        Fingers::feed(&mut state, 3000, Some(Finger), moved(10.0), false);
        assert!(Fingers::feed(&mut state, 3050, Some(Wheel), moved(39.0), false).is_none());
        assert!(state.is_none(), "a wheel notch ends finger tracking");
        assert!(Fingers::feed(&mut state, 3060, Some(Wheel), None, true).is_none(), "and never flings");
    }

    #[test]
    fn only_whole_notches_ease() {
        assert!(is_notch(120) && is_notch(-240), "a click wheel, one or two notches in a frame");
        assert!(!is_notch(15) && !is_notch(-60), "a free-spinning high-resolution wheel");
        assert!(!is_notch(0), "a touchpad");
    }

    #[test]
    fn wheel_steps_are_notches_positive_away_from_the_user() {
        assert_eq!(wheel_steps(0.0, -120), 1.0, "one notch up is +1");
        assert_eq!(wheel_steps(0.0, 240), -2.0, "two notches down are -2");
        assert_eq!(wheel_steps(-WHEEL_STEP_PIXELS as f64 / 2.0, 0), 0.5, "a touchpad swipe is a fraction of a notch");
        assert_eq!(wheel_steps(0.0, 0), 0.0);
        assert_eq!(wheel_steps(15.0, 120), -1.0, "value120 wins over pixels sent with it");
    }
}

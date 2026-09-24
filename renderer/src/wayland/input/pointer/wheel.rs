//! The wheel: a scroll signal's offset (ADR-0069) or a button's `on_wheel` (ADR-0116 decision 2).

use shared::warn;

use super::*;

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

/// `on_wheel`'s notches (ADR-0116 decision 2 amendment), positive away from the user. `value120`
/// wins: a wheel's pixels are not `WHEEL_STEP_PIXELS` per notch.
fn wheel_steps(pixels: f64, value120: i32) -> f32 {
    if value120 != 0 {
        return -(value120 as f32) / VALUE120_PER_NOTCH;
    }
    -(pixels as f32) / WHEEL_STEP_PIXELS
}

impl App {
    /// Apply one wheel event to the deepest scrollable or `on_wheel` button (ADR-0116 decision 2).
    /// Use touchpad pixels or `value120` (ADR-0069 decision 6); ignore deprecated `discrete`,
    /// which compositors that still send also accompany with `value120`.
    /// Innermost wins with no parent chaining: a wheel over a list stops there at its end, unlike
    /// browser chaining to the parent; no config needs those edge cases. The offset written is
    /// unclamped: `layout::scene` owns the bound and writes back what it used.
    /// `on_wheel` receives the vertical axis only.
    pub(super) fn scroll_at(
        &mut self,
        index: usize,
        position: (f64, f64),
        horizontal_px: f64,
        horizontal_steps: i32,
        vertical_px: f64,
        vertical_steps: i32,
    ) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(tree) = self.client.scene().surface(&surface_id) else {
            return;
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(tree, point);
        // Deepest scrollable under the pointer wins.
        let scrollable = path.iter().enumerate().rev().find_map(|(depth, node)| {
            let signal = layout::node::signal_at(&node.properties, "scroll")?;
            let axis = layout::scene::main_axis_of(node.kind, &node.properties).ok()??;
            Some((depth, signal, axis))
        });
        let wheel = wheel_button(&path);
        if let Some((depth, rect, on_wheel)) = wheel
            && scrollable.as_ref().is_none_or(|(scroll_depth, ..)| depth > *scroll_depth)
        {
            let steps = wheel_steps(vertical_px, vertical_steps);
            if steps == 0.0 {
                return;
            }
            let on_wheel = on_wheel.clone();
            drop(path);
            match rect_table(self.client.lua(), rect) {
                Ok(rect) => {
                    if let Err(e) = on_wheel.call::<()>((rect, steps)) {
                        warn!("{surface_id}: on_wheel raised, ignoring it: {e}");
                    }
                }
                Err(e) => warn!("{surface_id}: could not build on_wheel's rect argument: {e}"),
            }
            return;
        }
        let Some((_, signal, axis)) = scrollable else {
            return;
        };
        let (pixels, steps) = match axis {
            layout::scene::MainAxis::Horizontal => (horizontal_px, horizontal_steps),
            layout::scene::MainAxis::Vertical => (vertical_px, vertical_steps),
        };
        let delta = wheel_delta(pixels, steps);
        if delta == 0.0 {
            return;
        }
        let Some(handle) = signal.scroll_handle() else {
            return;
        };
        let current = signal.scroll_offset().unwrap_or(0.0);
        handle.set_changed(mlua::Value::Number(f64::from(current + delta)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0069 decision 6. The rest of `scroll_at` needs a compositor to deliver a notch;
    /// this is the half that does not.
    #[test]
    fn a_touchpads_pixels_are_used_as_sent_and_a_wheels_steps_are_converted() {
        assert_eq!(wheel_delta(17.5, 0), 17.5, "a touchpad reports a distance and it is taken");
        assert_eq!(wheel_delta(-17.5, 0), -17.5, "including upward");
        assert_eq!(wheel_delta(0.0, 120), WHEEL_STEP_PIXELS, "one notch is one step");
        assert_eq!(wheel_delta(0.0, -240), -2.0 * WHEEL_STEP_PIXELS, "two notches up");
        assert_eq!(wheel_delta(0.0, 60), WHEEL_STEP_PIXELS / 2.0, "high-resolution wheels send fractions of a notch");
    }

    /// A compositor that sends both is sending the same motion twice, so the distance wins and the
    /// step count is not added on top of it.
    #[test]
    fn a_step_count_is_ignored_when_a_distance_came_with_it() {
        assert_eq!(wheel_delta(17.5, 120), 17.5);
    }

    #[test]
    fn wheel_steps_are_notches_positive_away_from_the_user() {
        assert_eq!(wheel_steps(0.0, -120), 1.0, "one notch up is +1");
        assert_eq!(wheel_steps(0.0, 240), -2.0, "two notches down are -2");
        assert_eq!(wheel_steps(-WHEEL_STEP_PIXELS as f64 / 2.0, 0), 0.5, "a touchpad swipe is a fraction of a notch");
        assert_eq!(wheel_steps(0.0, 0), 0.0);
        assert_eq!(wheel_steps(15.0, 120), -1.0, "value120 wins over pixels sent with it");
    }

    #[test]
    fn a_wheel_event_carrying_no_motion_scrolls_nothing() {
        assert_eq!(wheel_delta(0.0, 0), 0.0);
    }

    /// The `value120` that would have been silently dropped by narrowing it to an `i16` first.
    ///
    /// Reachable rather than absurd: `AxisScroll::merge` sums `value120` across every axis event
    /// queued before the next `Frame` (`ret.value120 += other.value120`, smithay-client-toolkit
    /// 0.21.1), so one frame of dispatch lag behind a fast wheel accumulates past `i16::MAX`. The
    /// old narrowing turned that into a delta of zero, which returns early without writing the
    /// signal at all -- a hard flick scrolling nothing, at exactly the moment the client was
    /// already behind.
    #[test]
    fn an_implausibly_large_step_count_still_scrolls_rather_than_becoming_zero() {
        assert!(wheel_delta(0.0, 120 * 400) > 0.0);
    }
}

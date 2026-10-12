//! Drag velocity sampling for `on_drag`.

const VELOCITY_WINDOW_MS: u32 = 100;
/// Shorter spans divide sensor jitter by almost nothing: 1 px 1 ms after a rest is not 1000 px/s.
const VELOCITY_MIN_SPAN_MS: u32 = 8;

/// The press and motions of a held drag, for `on_drag`'s surface-logical velocity in px/s.
#[derive(Default)]
pub(super) struct Motion {
    /// `(time, position)` within [`VELOCITY_WINDOW_MS`] of the newest, by the compositor's `u32`
    /// millisecond clock, which wraps.
    samples: std::collections::VecDeque<(u32, (f64, f64))>,
    /// The last velocity measured over at least [`VELOCITY_MIN_SPAN_MS`].
    velocity: (f64, f64),
}

impl Motion {
    /// The velocity after the sample at `time`, if any. Zero when it is the window's only sample,
    /// as for a pointer still that long; the last measured one over a span too short to measure,
    /// and for an event with no time (a leave), so a flick off the edge keeps its speed.
    pub(super) fn velocity(&mut self, sample: Option<(u32, (f64, f64))>) -> (f64, f64) {
        let Some((time, at)) = sample else { return self.velocity };
        self.samples.push_back((time, at));
        while self.samples.front().is_some_and(|(then, _)| time.wrapping_sub(*then) > VELOCITY_WINDOW_MS) {
            self.samples.pop_front();
        }
        let Some(&(then, from)) = self.samples.front() else { return self.velocity };
        let span = time.wrapping_sub(then);
        if self.samples.len() == 1 {
            self.velocity = (0.0, 0.0);
        } else if span >= VELOCITY_MIN_SPAN_MS {
            let seconds = f64::from(span) / 1000.0;
            self.velocity = ((at.0 - from.0) / seconds, (at.1 - from.1) / seconds);
        }
        self.velocity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_velocity_spans_the_last_100_ms_of_event_time_and_a_still_pointer_reads_zero() {
        let mut motion = Motion::default();
        let mut at = |time, x: f64, y: f64| motion.velocity(Some((time, (x, y))));
        assert_eq!(at(1000, 10.0, 10.0), (0.0, 0.0), "the press alone");
        assert_eq!(at(1050, 20.0, 5.0), (200.0, -100.0));
        assert_eq!(at(1100, 40.0, 5.0), (300.0, -50.0), "from the press, 100 ms back");
        assert_eq!(at(1150, 40.0, 5.0), (200.0, 0.0), "the press fell out");
        assert_eq!(at(1260, 40.0, 5.0), (0.0, 0.0), "still for 100 ms before release");
        assert_eq!(at(1261, 41.0, 5.0), (0.0, 0.0), "1 px 1 ms after a rest is too short to measure");
        assert_eq!(at(1270, 50.0, 5.0), (1000.0, 0.0));
        assert_eq!(at(1272, 50.0, 5.0), (833.3333333333334, 0.0), "the span reaches back to the rest");
        assert_eq!(motion.velocity(None), (833.3333333333334, 0.0), "a leave keeps the last motion's velocity");
        let mut pruned = Motion::default();
        pruned.velocity(Some((0, (0.0, 0.0))));
        assert_eq!(pruned.velocity(Some((95, (95.0, 0.0)))), (1000.0, 0.0));
        assert_eq!(pruned.velocity(Some((101, (140.0, 0.0)))), (1000.0, 0.0), "a 6 ms span keeps the last measured");
        let mut wrapping = Motion::default();
        wrapping.velocity(Some((u32::MAX - 9, (0.0, 0.0))));
        assert_eq!(wrapping.velocity(Some((10, (20.0, 0.0)))), (1000.0, 0.0), "the u32 clock wraps");
    }
}

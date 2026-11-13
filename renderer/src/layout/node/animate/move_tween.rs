use std::time::Instant;

use super::MoveSpec;

#[derive(Debug, Clone)]
pub(crate) struct MoveTween {
    from: (f32, f32),
    pub(crate) offset: (f32, f32),
    pub(crate) started: Instant,
    spec: MoveSpec,
}

impl MoveTween {
    pub(crate) fn new(from: (f32, f32), spec: MoveSpec, now: Instant) -> Self {
        Self { from, offset: from, started: now, spec }
    }

    #[cfg(test)]
    pub(crate) fn test(offset: (f32, f32)) -> Self {
        use std::time::Duration;

        use super::Easing;

        Self::new(
            offset,
            MoveSpec { duration: Duration::from_millis(100), delay: Duration::ZERO, easing: Easing::Linear },
            Instant::now(),
        )
    }

    pub(crate) fn advance(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started).saturating_sub(self.spec.delay);
        if elapsed >= self.spec.duration {
            return false;
        }
        let progress = self.spec.easing.apply(elapsed.as_secs_f32() / self.spec.duration.as_secs_f32());
        self.offset = (self.from.0 * (1.0 - progress), self.from.1 * (1.0 - progress));
        true
    }

    pub(crate) fn holds_until(&self, now: Instant) -> Option<Instant> {
        if self.spec.duration.is_zero() {
            return None;
        }
        let lead = self.spec.delay.saturating_sub(now.saturating_duration_since(self.started));
        (!lead.is_zero()).then_some(self.started + self.spec.delay)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::layout::node::Easing;

    #[test]
    fn move_holds_through_delay_eases_then_finishes() {
        let ms = Duration::from_millis;
        let start = Instant::now() + ms(10);
        let spec = |duration, delay, easing| MoveSpec { duration: ms(duration), delay: ms(delay), easing };
        let mut movement = MoveTween::new((-20.0, 10.0), spec(100, 50, Easing::InQuad), start);
        let dl = start + ms(50);
        assert!([start - ms(1), start, start + ms(49)].iter().all(|&t| movement.holds_until(t) == Some(dl)));
        assert!(movement.advance(start + ms(49)) && movement.offset == (-20.0, 10.0));
        assert!(movement.holds_until(dl).is_none() && movement.advance(dl) && movement.offset == (-20.0, 10.0));
        assert!(movement.holds_until(start + ms(100)).is_none() && movement.advance(start + ms(100)));
        assert_eq!(movement.offset, (-15.0, 7.5));
        assert!(movement.holds_until(start + ms(150)).is_none() && !movement.advance(start + ms(150)));
        let no_delay = MoveTween::new((-20.0, 10.0), spec(100, 0, Easing::Linear), start);
        let mut no_dur = MoveTween::new((-20.0, 10.0), spec(0, 50, Easing::Linear), start);
        assert!(no_delay.holds_until(start).is_none() && no_dur.holds_until(start).is_none() && !no_dur.advance(start));
    }
}

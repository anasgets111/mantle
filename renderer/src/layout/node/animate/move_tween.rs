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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::layout::node::Easing;

    #[test]
    fn move_holds_through_delay_eases_then_finishes() {
        let start = Instant::now();
        let mut movement = MoveTween::new(
            (-20.0, 10.0),
            MoveSpec { duration: Duration::from_millis(100), delay: Duration::from_millis(50), easing: Easing::InQuad },
            start,
        );
        assert!(movement.advance(start + Duration::from_millis(49)));
        assert_eq!(movement.offset, (-20.0, 10.0));
        assert!(movement.advance(start + Duration::from_millis(100)));
        assert_eq!(movement.offset, (-15.0, 7.5));
        assert!(!movement.advance(start + Duration::from_millis(150)));
    }
}

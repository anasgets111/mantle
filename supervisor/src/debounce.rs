//! Debounce deadline shared by the directory watchers: quiet for `quiet` fires, a burst that never
//! goes quiet fires [`MAX_WAIT`] after its first event.

use std::time::Duration;

use tokio::time::Instant;

/// Longest a steady trickle of events can hold a trigger off.
pub const MAX_WAIT: Duration = Duration::from_secs(2);

/// The pending trigger of one burst: first event time and fire time.
#[derive(Default)]
pub struct Burst(Option<(Instant, Instant)>);

impl Burst {
    /// Records an event: fire `quiet` from now, but never later than [`MAX_WAIT`] after the first.
    pub fn bump(&mut self, quiet: Duration) {
        let now = Instant::now();
        let first = self.0.map_or(now, |(first, _)| first);
        self.0 = Some((first, (now + quiet).min(first + MAX_WAIT)));
    }

    /// When the trigger fires, or `None` with no event pending.
    pub fn at(&self) -> Option<Instant> {
        self.0.map(|(_, at)| at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_steady_trickle_fires_at_the_max_wait_and_a_quiet_burst_at_the_debounce() {
        let quiet = Duration::from_millis(200);
        let mut burst = Burst::default();
        assert_eq!(burst.at(), None);
        let first = Instant::now();
        for _ in 0..40 {
            burst.bump(quiet);
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        assert_eq!(burst.at(), Some(first + MAX_WAIT));

        burst = Burst::default();
        burst.bump(quiet);
        assert_eq!(burst.at(), Some(Instant::now() + quiet));
    }
}

//! When the loop hands freed memory back: a full Lua collect plus `malloc_trim`, debounced.

use std::time::{Duration, Instant};

/// Quiet time a request waits out, so a burst of releases (a filter narrowing on every keystroke)
/// costs one trim after the last one. Longer than a typist's gap between keys.
const SETTLE: Duration = Duration::from_millis(1000);
/// Minimum spacing between trims. A trim costs a full GC, ms to tens of ms on a big heap; one per
/// 5 s caps that near 0.5% of a core while memory still returns within seconds.
const SPACING: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct Trim {
    due: Option<Instant>,
    last: Option<Instant>,
}

impl Trim {
    /// Something freed memory in bulk. Restarts the settle wait, never earlier than `SPACING`
    /// after the last trim.
    /// ponytail: steady typing defers the trim until it stops. Upgrade: cap the deferral.
    pub(super) fn request(&mut self, now: Instant) {
        let earliest = self.last.map_or(now, |last| last + SPACING);
        self.due = Some((now + SETTLE).max(earliest));
    }

    /// The loop's wake for a pending trim.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.due
    }

    /// Whether the trim is due at `now`; true clears it.
    pub(super) fn take_due(&mut self, now: Instant) -> bool {
        let due = self.due.is_some_and(|due| due <= now);
        if due {
            self.due = None;
            self.last = Some(now);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trims a loop ticking every 10 ms runs over `keys` requests 200 ms apart, then idling.
    fn trims(keys: u32) -> u32 {
        let start = Instant::now();
        let mut trim = Trim::default();
        let mut count = 0;
        for tick in 0..(keys * 20 + 1000) {
            let now = start + Duration::from_millis(u64::from(tick) * 10);
            if tick % 20 == 0 && tick < keys * 20 {
                trim.request(now);
            }
            count += u32::from(trim.take_due(now));
        }
        count
    }

    #[test]
    fn a_burst_of_narrowing_keystrokes_trims_once_after_it_ends() {
        assert_eq!(trims(1), 1);
        assert_eq!(trims(50), 1, "before the debounce this was 50, one per shed");
    }

    #[test]
    fn a_trim_waits_out_the_spacing_and_still_lands() {
        let start = Instant::now();
        let mut trim = Trim::default();
        trim.request(start);
        assert!(!trim.take_due(start), "settles first");
        assert!(trim.take_due(start + SETTLE));
        let again = start + SETTLE + Duration::from_millis(10);
        trim.request(again);
        assert_eq!(trim.deadline(), Some(start + SETTLE + SPACING));
        assert!(!trim.take_due(again + SETTLE));
        assert!(trim.take_due(start + SETTLE + SPACING));
        assert_eq!(trim.deadline(), None);
    }
}

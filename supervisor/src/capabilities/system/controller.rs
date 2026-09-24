//! [`SystemController`] feeds `mantle.system.time` from one wall-clock-aligned task, refreshed
//! every second.
//!
//! `system.time` has no interval argument, so it ticks unconditionally from construction to
//! shutdown, aligning its first wake to the wall-clock second boundary
//! ([`time_until_next_second`]).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;

/// `mantle.system`'s payload, pushed once a second.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SystemState {
    /// Unix epoch seconds, as `os.date` takes them.
    pub time: i64,
    /// Seconds since `system` was first used; excludes suspend. Take durations from it, since NTP moves `time`.
    // ponytail: `Instant` is `CLOCK_MONOTONIC`; suspend-inclusive timing wants a `CLOCK_BOOTTIME` field.
    pub monotonic: i64,
}

/// Wakes `main.rs`'s `select!` for a fresh `StateSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSignal {
    Changed,
}

/// `SystemTime::now()`'s epoch truncated to whole seconds for `time`; one pinned seam.
pub fn epoch_seconds(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0)
}

/// Duration from `elapsed_since_epoch` to the next whole-second boundary. Pure over `Duration` for
/// clock-free tests.
///
/// ponytail: aligns once at startup, then uses a steady 1-second `tokio::time::interval` on
/// monotonic `Instant`. It does not track wall-clock drift. With no NTP step or suspend/resume,
/// both clocks run closely enough for this shell's sessions; a step (`settimeofday`, NTP slew,
/// resume) leaves the old boundary until restart. Upgrade by re-deriving alignment each tick and
/// rebuilding the interval after a discrepancy of a few milliseconds; not worth a check in this
/// 1Hz loop until observed.
pub fn time_until_next_second(elapsed_since_epoch: Duration) -> Duration {
    Duration::from_secs(1) - Duration::from_nanos(u64::from(elapsed_since_epoch.subsec_nanos()))
}

pub struct SystemController {
    state: Arc<Mutex<SystemState>>,
}

impl SystemController {
    /// Seeds `time` immediately, so an early client sees the current second, not stale zero; the
    /// ticking task takes over afterward.
    pub fn new(signal_tx: UnboundedSender<SystemSignal>) -> Self {
        let started = Instant::now();
        let state = Arc::new(Mutex::new(SystemState { time: epoch_seconds(SystemTime::now()), monotonic: 0 }));

        tokio::spawn(run_clock_task(Arc::clone(&state), signal_tx, started));

        Self { state }
    }

    /// Current state for `main.rs`'s signal-channel `select!` snapshot push.
    pub fn snapshot(&self) -> SystemState {
        self.state.lock().expect("system state mutex poisoned").clone()
    }
}

/// Aligns its first wake to the next wall-clock second, then ticks a steady one-second interval.
async fn run_clock_task(state: Arc<Mutex<SystemState>>, signal_tx: UnboundedSender<SystemSignal>, started: Instant) {
    let delay = time_until_next_second(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default());
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + delay, Duration::from_secs(1));

    loop {
        ticker.tick().await;
        let sampled =
            SystemState { time: epoch_seconds(SystemTime::now()), monotonic: started.elapsed().as_secs() as i64 };
        *state.lock().expect("system state mutex poisoned") = sampled;
        if signal_tx.send(SystemSignal::Changed).is_err() {
            return; // main.rs's select! loop is gone; nothing left to notify
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_seconds_is_pinned_against_a_known_instant() {
        let known = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(epoch_seconds(known), 1_700_000_000, "must be exact -- Lua reads this straight as an integer");
    }

    #[test]
    fn epoch_seconds_is_seconds_not_milliseconds() {
        // Millis "now" would be roughly 1_700_000_000_000, three orders larger.
        let known = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let seconds = epoch_seconds(known);
        assert!((1_000_000_000..2_000_000_000).contains(&seconds), "plausible unix epoch seconds range, got {seconds}");
    }

    #[test]
    fn time_until_next_second_is_the_complement_of_the_subsecond_remainder() {
        assert_eq!(time_until_next_second(Duration::from_millis(300)), Duration::from_millis(700));
        assert_eq!(time_until_next_second(Duration::new(10, 999_000_000)), Duration::from_millis(1));
    }

    #[test]
    fn time_until_next_second_is_a_full_second_when_already_on_the_boundary() {
        assert_eq!(time_until_next_second(Duration::from_secs(5)), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn new_seeds_time_synchronously_before_any_tick_fires() {
        // The first tick is a second away; clients in that second must read real epoch, not zero.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SystemController::new(tx);

        let seeded = controller.snapshot();
        assert!(seeded.time > 1_700_000_000, "seeded from the real clock, not defaulted");
        assert_eq!(seeded.monotonic, 0, "zero is the epoch, not missing data");
    }
}

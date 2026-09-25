//! [`SystemController`] feeds `mantle.system` from one task that wakes on wall-clock multiples of
//! its interval, one second until `system:configure` changes it.

use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
use shared::{debug, error};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

/// `mantle.system`'s payload, pushed on each tick of `interval`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SystemState {
    /// Unix epoch seconds, as `os.date` takes them.
    pub time: i64,
    /// Seconds since `system` was first used, as of the last push; excludes suspend. Take durations
    /// from it, since NTP moves `time`.
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

/// `system:configure`'s table. An absent `interval` keeps the current one.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SystemConfigure {
    /// Seconds between pushes, each on a multiple of it since the epoch, so `60` lands on every
    /// minute; `1` is the default and `0` stops them.
    pub interval: Option<u64>,
}

/// The first epoch second after `now` that is a multiple of `interval` (non-zero).
fn next_boundary(now: Duration, interval: u64) -> u64 {
    (now.as_secs() / interval + 1) * interval
}

/// Duration from `elapsed_since_epoch` to the next whole-second boundary. Pure over `Duration` for
/// clock-free tests.
pub fn time_until_next_second(elapsed_since_epoch: Duration) -> Duration {
    Duration::from_secs(1) - Duration::from_nanos(u64::from(elapsed_since_epoch.subsec_nanos()))
}

/// The next wall-clock second as a tokio deadline. Every 1 Hz-multiple poller starts here, so
/// their pushes reach the Renderer together and share one re-resolve (ADR-0044 decision 2).
pub fn next_wall_clock_second() -> tokio::time::Instant {
    tokio::time::Instant::now()
        + time_until_next_second(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default())
}

pub struct SystemController {
    state: Arc<Mutex<SystemState>>,
    interval: watch::Sender<u64>,
}

impl SystemController {
    /// Seeds `time` immediately, so an early client sees the current second, not stale zero; the
    /// ticking task takes over afterward.
    pub fn new(signal_tx: UnboundedSender<SystemSignal>) -> Self {
        let started = Instant::now();
        let state = Arc::new(Mutex::new(SystemState { time: epoch_seconds(SystemTime::now()), monotonic: 0 }));

        let (interval, interval_rx) = watch::channel(1);

        tokio::spawn(run_clock_task(Arc::clone(&state), signal_tx, started, interval_rx));

        Self { state, interval }
    }

    /// Applies `system:configure(cfg)`; a new interval takes effect with an immediate push.
    pub fn configure(&self, cfg: SystemConfigure) {
        debug!("configure: interval={:?}", cfg.interval);
        // Capped so the absolute deadline stays far inside `time_t`.
        if let Some(seconds) = cfg.interval
            && self.interval.send(seconds.min(u32::MAX.into())).is_err()
        {
            debug!("clock task is gone, interval update dropped");
        }
    }

    /// Current state for `main.rs`'s signal-channel `select!` snapshot push.
    pub fn snapshot(&self) -> SystemState {
        self.state.lock().expect("system state mutex poisoned").clone()
    }
}

/// Wakes on an absolute `CLOCK_REALTIME` timerfd, so a tick due during suspend fires on resume and
/// a clock step (NTP, `settimeofday`) cancels the timer and re-aligns it at once. Parked, with no
/// timer, while the interval is `0`.
async fn run_clock_task(
    state: Arc<Mutex<SystemState>>,
    signal_tx: UnboundedSender<SystemSignal>,
    started: Instant,
    mut interval_rx: watch::Receiver<u64>,
) {
    let timer = match TimerFd::new(ClockId::CLOCK_REALTIME, TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC) {
        Ok(timer) => timer,
        Err(err) => return error!("cannot create the clock timer: {err}; mantle.system keeps its first reading"),
    };
    let readiness = match AsyncFd::new(timer.as_fd()) {
        Ok(readiness) => readiness,
        Err(err) => return error!("cannot poll the clock timer: {err}; mantle.system keeps its first reading"),
    };
    let mut interval = *interval_rx.borrow_and_update();
    loop {
        if interval == 0 {
            if interval_rx.changed().await.is_err() {
                return; // SystemController is gone; nothing can reconfigure this task
            }
        } else {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            let deadline = TimeSpec::from_duration(Duration::from_secs(next_boundary(now, interval)));
            let flags = TimerSetTimeFlags::TFD_TIMER_ABSTIME | TimerSetTimeFlags::TFD_TIMER_CANCEL_ON_SET;
            if let Err(err) = timer.set(Expiration::OneShot(deadline), flags) {
                return error!("cannot arm the clock timer: {err}");
            }
            tokio::select! {
                // `wait` is `Ok` on expiry and on a clock step (`ECANCELED`); both push.
                fired = readiness.readable() => match fired {
                    Ok(mut guard) => {
                        if guard.try_io(|_| timer.wait().map_err(std::io::Error::from)).is_err() {
                            continue; // spurious readiness; re-arm
                        }
                    }
                    Err(err) => return error!("clock timer failed: {err}"),
                },
                changed = interval_rx.changed() => if changed.is_err() { return },
            }
        }
        interval = *interval_rx.borrow_and_update();
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
    fn next_boundary_is_the_next_multiple_of_the_interval_strictly_after_now() {
        assert_eq!(next_boundary(Duration::from_millis(125_300), 60), 180, "a minute tick lands on :00");
        assert_eq!(next_boundary(Duration::from_secs(120), 60), 180, "a tick on the boundary waits a full interval");
        assert_eq!(next_boundary(Duration::from_millis(125_300), 1), 126);
    }

    #[test]
    fn time_until_next_second_is_a_full_second_when_already_on_the_boundary() {
        assert_eq!(time_until_next_second(Duration::from_secs(5)), Duration::from_secs(1));
    }

    /// Real time, since the timer is a kernel `CLOCK_REALTIME` timerfd tokio cannot pause.
    #[tokio::test]
    async fn zero_parks_the_clock_and_a_new_interval_pushes_at_once() {
        use tokio::time::timeout;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SystemController::new(tx);
        tokio::task::yield_now().await; // let the clock task arm first

        controller.configure(SystemConfigure { interval: Some(0) });
        assert!(timeout(Duration::from_millis(100), rx.recv()).await.is_ok(), "a reconfigure pushes at once");
        assert!(timeout(Duration::from_millis(1200), rx.recv()).await.is_err(), "0 parks: no tick");

        controller.configure(SystemConfigure { interval: Some(1) });
        assert!(timeout(Duration::from_millis(100), rx.recv()).await.is_ok());
        assert!(timeout(Duration::from_millis(1100), rx.recv()).await.is_ok(), "then one tick a second");
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

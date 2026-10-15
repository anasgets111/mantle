//! [`ClockController`] feeds `mantle.clock` from one task that wakes on wall-clock multiples of
//! its interval, one second until `clock:configure` changes it.

pub use shared::state::clock::ClockState;

use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
use shared::action::ClockConfigure;
use shared::state::clock::monotonic_seconds;
use shared::{debug, error};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

/// `SystemTime::now()`'s epoch truncated to whole seconds for `time`; one pinned seam.
pub fn epoch_seconds(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0)
}

fn sample() -> ClockState {
    ClockState { time: epoch_seconds(SystemTime::now()), monotonic: monotonic_seconds() }
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

pub struct ClockController {
    state: Arc<Mutex<ClockState>>,
    interval: watch::Sender<u64>,
}

impl ClockController {
    /// Seeds `time` immediately, so an early client sees the current second, not stale zero; the
    /// ticking task takes over afterward.
    pub fn new(signal_tx: UnboundedSender<()>) -> Self {
        let state = Arc::new(Mutex::new(sample()));

        let (interval, interval_rx) = watch::channel(1);

        tokio::spawn(run_clock_task(Arc::clone(&state), signal_tx, interval_rx));

        Self { state, interval }
    }

    /// Applies `clock:configure(cfg)`; a new interval takes effect with an immediate push.
    pub fn configure(&self, cfg: ClockConfigure) {
        debug!("configure: interval={:?}", cfg.interval);
        // Capped so the absolute deadline stays far inside `time_t`.
        if let Some(seconds) = cfg.interval
            && self.interval.send(seconds.min(u32::MAX.into())).is_err()
        {
            debug!("clock task is gone, interval update dropped");
        }
    }

    /// Current state for `main.rs`'s signal-channel `select!` snapshot push.
    pub fn snapshot(&self) -> ClockState {
        self.state.lock().expect("clock state mutex poisoned").clone()
    }
}

/// Wakes on an absolute `CLOCK_REALTIME` timerfd, so a tick due during suspend fires on resume and
/// a clock step (NTP, `settimeofday`) cancels the timer and re-aligns it at once. Parked, with no
/// timer, while the interval is `0`.
async fn run_clock_task(
    state: Arc<Mutex<ClockState>>,
    signal_tx: UnboundedSender<()>,
    mut interval_rx: watch::Receiver<u64>,
) {
    let timer = match TimerFd::new(ClockId::CLOCK_REALTIME, TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC) {
        Ok(timer) => timer,
        Err(err) => return error!("cannot create the clock timer: {err}; mantle.clock keeps its first reading"),
    };
    let readiness = match AsyncFd::new(timer.as_fd()) {
        Ok(readiness) => readiness,
        Err(err) => return error!("cannot poll the clock timer: {err}; mantle.clock keeps its first reading"),
    };
    let mut interval = *interval_rx.borrow_and_update();
    // `new` seeded the state; announce it instead of waiting for the first boundary.
    if signal_tx.send(()).is_err() {
        return;
    }
    loop {
        if interval == 0 {
            if interval_rx.changed().await.is_err() {
                return; // ClockController is gone; nothing can reconfigure this task
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
                    Err(err) => return error!("timer failed: {err}"),
                },
                changed = interval_rx.changed() => if changed.is_err() { return },
            }
        }
        interval = *interval_rx.borrow_and_update();
        *state.lock().expect("clock state mutex poisoned") = sample();
        if signal_tx.send(()).is_err() {
            return; // main.rs's select! loop is gone; nothing left to notify
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_seconds_is_exact() {
        assert_eq!(epoch_seconds(UNIX_EPOCH + Duration::from_secs(1_700_000_000)), 1_700_000_000);
    }

    #[test]
    fn time_until_next_second_is_the_complement_of_the_subsecond_remainder() {
        assert_eq!(time_until_next_second(Duration::from_millis(300)), Duration::from_millis(700));
        assert_eq!(time_until_next_second(Duration::new(10, 999_000_000)), Duration::from_millis(1));
        assert_eq!(time_until_next_second(Duration::from_secs(5)), Duration::from_secs(1), "on the boundary");
    }

    #[test]
    fn next_boundary_is_the_next_multiple_of_the_interval_strictly_after_now() {
        assert_eq!(next_boundary(Duration::from_millis(125_300), 60), 180, "a minute tick lands on :00");
        assert_eq!(next_boundary(Duration::from_secs(120), 60), 180, "a tick on the boundary waits a full interval");
        assert_eq!(next_boundary(Duration::from_millis(125_300), 1), 126);
    }

    /// Real time, since the timer is a kernel `CLOCK_REALTIME` timerfd tokio cannot pause.
    #[tokio::test]
    async fn zero_parks_the_clock_and_a_new_interval_pushes_at_once() {
        use tokio::time::timeout;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = ClockController::new(tx);
        assert!(timeout(Duration::from_millis(100), rx.recv()).await.is_ok(), "the seed is announced at once");

        controller.configure(ClockConfigure { interval: Some(0) });
        assert!(timeout(Duration::from_millis(100), rx.recv()).await.is_ok(), "a reconfigure pushes at once");
        assert!(timeout(Duration::from_millis(1200), rx.recv()).await.is_err(), "0 parks: no tick");

        controller.configure(ClockConfigure { interval: Some(1) });
        assert!(timeout(Duration::from_millis(100), rx.recv()).await.is_ok());
        assert!(timeout(Duration::from_millis(1100), rx.recv()).await.is_ok(), "then one tick a second");
    }

    #[tokio::test]
    async fn new_seeds_time_synchronously_before_any_tick_fires() {
        // The first tick is a second away; clients in that second must read real epoch, not zero.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = ClockController::new(tx);

        let seeded = controller.snapshot();
        assert!(seeded.time > 1_700_000_000, "seeded from the real clock, not defaulted");
        assert!(
            seeded.monotonic > 0.0 && seeded.monotonic <= monotonic_seconds(),
            "the system-wide clock, not an epoch of this capability's start"
        );
    }
}

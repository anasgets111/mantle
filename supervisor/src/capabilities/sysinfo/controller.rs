//! [`SysinfoController`] runs configurable metric poll tasks feeding one `SysinfoState`.

pub use shared::state::sysinfo::SysinfoState;

use std::time::Duration;

use shared::action::SysinfoConfigure;
use shared::debug;

/// Whether a metric task ticks or parks with zero wakeups (ADR-0035), reevaluated when its
/// `watch::Receiver` reports an interval change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollMode {
    /// `interval == 0`: no timer; await only `watch::Receiver::changed()`.
    Dormant,
    /// `interval != 0`: race `tokio::time::interval_at(_).tick()` against
    /// `watch::Receiver::changed()`.
    Ticking(Duration),
}

pub fn poll_mode(interval: Duration) -> PollMode {
    if interval.is_zero() { PollMode::Dormant } else { PollMode::Ticking(interval) }
}

/// Owns the poll tasks and their state. Not `Clone`: synchronous, non-blocking `configure`
/// uses `&SysinfoController` directly.
pub struct SysinfoController {
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    cpu_interval: tokio::sync::watch::Sender<Duration>,
    ram_interval: tokio::sync::watch::Sender<Duration>,
    temp_interval: tokio::sync::watch::Sender<Duration>,
    disk_interval: tokio::sync::watch::Sender<Duration>,
    gpu_interval: tokio::sync::watch::Sender<Duration>,
    net_interval: tokio::sync::watch::Sender<Duration>,
}

impl SysinfoController {
    /// Spawns dormant tasks (`Duration::ZERO`) until Lua calls `configure`. They share
    /// `signal_tx` and signal only after real-tick state updates. Temp inputs resolve on the first
    /// tick and again whenever a read fails.
    pub fn new(
        proc_root: std::path::PathBuf,
        hwmon_root: std::path::PathBuf,
        signal_tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Self {
        let state = std::sync::Arc::new(std::sync::Mutex::new(SysinfoState::default()));

        let (cpu_interval, cpu_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (ram_interval, ram_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (temp_interval, temp_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (disk_interval, disk_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (gpu_interval, gpu_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (net_interval, net_rx) = tokio::sync::watch::channel(Duration::ZERO);

        tokio::spawn(run_cpu_task(proc_root.clone(), cpu_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_ram_task(proc_root.clone(), ram_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_temp_task(hwmon_root.clone(), temp_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_disk_task(disk_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_gpu_task(hwmon_root, gpu_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_net_task(proc_root, net_rx, std::sync::Arc::clone(&state), signal_tx));

        Self { state, cpu_interval, ram_interval, temp_interval, disk_interval, gpu_interval, net_interval }
    }

    /// Applies parsed `sysinfo:configure(cfg)`: present intervals wake, retime, or suspend their
    /// task at `0`; absent ones stay unchanged. `send` errors only after task panic, logged here.
    pub fn configure(&self, cfg: SysinfoConfigure) {
        debug!(
            "configure: cpu={:?} ram={:?} temp={:?} disk={:?} gpu={:?} net={:?}",
            cfg.cpu_interval,
            cfg.ram_interval,
            cfg.temp_interval,
            cfg.disk_interval,
            cfg.gpu_interval,
            cfg.net_interval
        );
        let send = |seconds: Option<u64>, sender: &tokio::sync::watch::Sender<Duration>, name: &str| {
            if let Some(sec) = seconds
                // Capped because tokio's `interval` adds it to an `Instant`, which aborts near `i64::MAX` seconds.
                && sender.send(Duration::from_secs(sec.min(u32::MAX.into()))).is_err()
            {
                debug!("{name} task is gone, {name}_interval update dropped");
            }
        };
        send(cfg.cpu_interval, &self.cpu_interval, "cpu");
        send(cfg.ram_interval, &self.ram_interval, "ram");
        send(cfg.temp_interval, &self.temp_interval, "temp");
        send(cfg.disk_interval, &self.disk_interval, "disk");
        send(cfg.gpu_interval, &self.gpu_interval, "gpu");
        send(cfg.net_interval, &self.net_interval, "net");
    }

    /// Current combined state for `main.rs`'s signal-channel `select!` snapshot push.
    pub fn snapshot(&self) -> SysinfoState {
        self.state.lock().expect("sysinfo state mutex poisoned").clone()
    }
}

/// Writes one task's fields and publishes only if that changed something.
///
/// Every send hydrates a `StateSnapshot`, which marks the scene dirty and costs a whole re-resolve
/// (ADR-0044 decision 2). A machine at rest reports the same rounded percentage and the same whole
/// Celsius for minutes together, so an unconditional send would buy a re-resolve per tick for no
/// new information. The sample itself is still taken and still stored: `cpu_percent` needs the
/// counters for the next delta whether or not the rounded result moved.
fn publish_if_changed(
    state: &std::sync::Mutex<SysinfoState>,
    signal_tx: &tokio::sync::mpsc::UnboundedSender<()>,
    write: impl FnOnce(&mut SysinfoState) -> bool,
) {
    // Drop the lock before sending: the receiver hydrates a snapshot and must never wait on a
    // poll task's mutex to do it.
    let changed = {
        let mut state = state.lock().expect("sysinfo state mutex poisoned");
        write(&mut state)
    };
    if changed {
        let _ = signal_tx.send(());
    }
}

/// Stores `sample` and reports a change; a failed sample (`None`) keeps the last good value rather
/// than blanking it for a whole interval.
fn replace_if_sampled<T: PartialEq>(slot: &mut T, sample: Option<T>) -> bool {
    sample.is_some_and(|sample| std::mem::replace(slot, sample) != *slot)
}

/// One metric's poll loop: parked while its interval is zero, otherwise `tick(fresh)` at the next
/// wall-clock second and once per interval after. `fresh` marks the first tick after entry or a
/// dormant spell; the next one comes a full interval later, so a delta never spans a few ms.
async fn run_async_ticker<F, Fut>(mut interval_rx: tokio::sync::watch::Receiver<Duration>, mut tick: F)
where
    F: FnMut(bool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut fresh = true;
    loop {
        let interval = *interval_rx.borrow_and_update();
        match poll_mode(interval) {
            PollMode::Dormant => {
                fresh = true;
                if interval_rx.changed().await.is_err() {
                    return; // every SysinfoController that could reconfigure this task is gone
                }
            }
            PollMode::Ticking(duration) => {
                // One sample on entry, not on every reconfigure: a reload would re-sample and blip cpu.
                let first = std::mem::take(&mut fresh);
                if first {
                    tick(true).await;
                }
                // On the clock's second, so a whole-second interval lands in `system`'s push turn.
                let second = crate::capabilities::system::controller::next_wall_clock_second();
                let mut ticker = tokio::time::interval_at(if first { second + duration } else { second }, duration);
                loop {
                    tokio::select! {
                        _ = ticker.tick() => tick(false).await,
                        changed = interval_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            break; // interval reconfigured -- rebuild dormant/ticking in the outer loop
                        }
                    }
                }
            }
        }
    }
}

/// [`run_async_ticker`] for synchronous reads. `previous` is the tick's memory across samples,
/// cleared on a fresh tick: `/proc/stat` counters are cumulative since boot, so a delta across a
/// dormant spell is bogus.
async fn run_ticker<T>(interval_rx: tokio::sync::watch::Receiver<Duration>, mut tick: impl FnMut(&mut Option<T>)) {
    let mut previous = None;
    run_async_ticker(interval_rx, |fresh| {
        if fresh {
            previous = None;
        }
        tick(&mut previous);
        std::future::ready(())
    })
    .await
}

/// `cpu_percent` task. The first tick after cold start or resume stores only a sample.
async fn run_cpu_task(
    proc_root: std::path::PathBuf,
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<()>,
) {
    run_ticker(interval_rx, |previous| match super::cpu::read_sample(&proc_root) {
        Ok(sample) => {
            if let Some(prev) = previous.take() {
                let percent = super::cpu::delta_percent(&prev, &sample);
                publish_if_changed(&state, &signal_tx, |state| {
                    let changed = state.cpu_percent != percent;
                    state.cpu_percent = percent;
                    changed
                });
            }
            *previous = Some(sample);
        }
        Err(err) => debug!("failed to read /proc/stat: {err}"),
    })
    .await
}

/// `ram_percent`/`swap_percent` task. One `/proc/meminfo` read per tick; `swap_percent` rides
/// `ram_interval` with no separate interval (ADR-0035).
async fn run_ram_task(
    proc_root: std::path::PathBuf,
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<()>,
) {
    run_ticker(interval_rx, |_: &mut Option<()>| match super::ram::read_meminfo(&proc_root) {
        Ok(info) => {
            let (ram_percent, swap_percent) = super::ram::compute_percentages(&info);
            publish_if_changed(&state, &signal_tx, |state| {
                let changed = state.ram_percent != ram_percent || state.swap_percent != swap_percent;
                state.ram_percent = ram_percent;
                state.swap_percent = swap_percent;
                changed
            });
        }
        Err(err) => debug!("failed to read /proc/meminfo: {err}"),
    })
    .await
}

/// `temp_cores`/`temp_gpu` task. Each tick reads the inputs resolved so far; `temp_gpu` rides
/// `temp_interval`.
async fn run_temp_task(
    hwmon_root: std::path::PathBuf,
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let (mut core_inputs, mut gpu_input) = (Vec::new(), None);
    run_ticker(interval_rx, |_: &mut Option<()>| {
        let temp_cores = super::temp::sample_temp_cores(&mut core_inputs, &hwmon_root);
        let temp_gpu = super::temp::sample_temp_gpu(&mut gpu_input, &hwmon_root);
        publish_if_changed(&state, &signal_tx, |state| {
            let changed = state.temp_cores != temp_cores || state.temp_gpu != temp_gpu;
            state.temp_cores = temp_cores;
            state.temp_gpu = temp_gpu;
            changed
        });
    })
    .await
}

/// `disks` task. Each tick queries storage topology via `disk::read_disks`.
async fn run_disk_task(
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<()>,
) {
    run_async_ticker(interval_rx, |_| async {
        let disks = super::disk::read_disks().await;
        publish_if_changed(&state, &signal_tx, |state| replace_if_sampled(&mut state.disks, disks));
    })
    .await
}

/// `gpu` telemetry task. Each tick samples GPU load, VRAM, and temperature via `gpu::sample_gpu`.
async fn run_gpu_task(
    hwmon_root: std::path::PathBuf,
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let drm_root = hwmon_root.parent().unwrap_or(&hwmon_root).join("drm");
    let mut gpu_input = None;
    run_async_ticker(interval_rx, |_| {
        let temp_gpu = super::temp::sample_temp_gpu(&mut gpu_input, &hwmon_root);
        let (drm_root, state, signal_tx) = (&drm_root, &state, &signal_tx);
        async move {
            // ponytail: a failed or timed-out sample keeps the last reading, so an unplugged eGPU
            // stays until restart. Upgrade: drop it after N consecutive misses.
            let gpu = super::gpu::sample_gpu(drm_root, temp_gpu).await;
            publish_if_changed(state, signal_tx, |state| replace_if_sampled(&mut state.gpu, gpu.map(Some)));
        }
    })
    .await
}

/// `net_rx_bytes_sec`/`net_tx_bytes_sec` task. The first tick stores a baseline sample; later ticks
/// compute bytes per second over elapsed monotonic time.
async fn run_net_task(
    proc_root: std::path::PathBuf,
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<()>,
) {
    run_ticker(interval_rx, |previous: &mut Option<(super::net::NetSample, std::time::Instant)>| {
        match super::net::read_sample(&proc_root) {
            Ok(sample) => {
                let now = std::time::Instant::now();
                if let Some((prev_sample, prev_time)) = previous.take() {
                    let elapsed = (now - prev_time).as_secs_f64();
                    let (rx, tx) = super::net::delta_rate(&prev_sample, &sample, elapsed);
                    publish_if_changed(&state, &signal_tx, |state| {
                        let changed = state.net_rx_bytes_sec != rx || state.net_tx_bytes_sec != tx;
                        state.net_rx_bytes_sec = rx;
                        state.net_tx_bytes_sec = tx;
                        changed
                    });
                }
                *previous = Some((sample, now));
            }
            Err(err) => debug!("failed to read /proc/net/dev: {err}"),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn the_first_sample_is_immediate_and_a_reconfigure_does_not_resample() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::watch::channel(std::time::Duration::from_secs(60));
        let seen = count.clone();
        tokio::spawn(super::run_ticker(rx, move |_: &mut Option<()>| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let samples = || count.load(std::sync::atomic::Ordering::SeqCst);
        tokio::task::yield_now().await;
        assert_eq!(samples(), 1);
        // The next sample waits a whole interval, so the first delta is not a few ms wide.
        tokio::time::advance(std::time::Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert_eq!(samples(), 1);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(samples(), 2);
        tx.send(std::time::Duration::from_secs(60)).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(samples(), 2);
    }

    #[test]
    fn poll_mode_is_dormant_at_zero_and_ticking_otherwise() {
        assert_eq!(super::poll_mode(std::time::Duration::ZERO), super::PollMode::Dormant);
        assert_eq!(
            super::poll_mode(std::time::Duration::from_secs(5)),
            super::PollMode::Ticking(std::time::Duration::from_secs(5))
        );
    }

    /// `publish_if_changed` needs a state and a channel; both tasks and this test build them the
    /// same way, so a helper keeps the four cases below to their point.
    fn state_and_channel() -> (
        std::sync::Arc<std::sync::Mutex<super::SysinfoState>>,
        tokio::sync::mpsc::UnboundedSender<()>,
        tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (std::sync::Arc::new(std::sync::Mutex::new(super::SysinfoState::default())), tx, rx)
    }

    #[test]
    fn a_failed_sample_keeps_the_last_good_value() {
        let mut disks = vec![1];
        assert!(!super::replace_if_sampled(&mut disks, None));
        assert_eq!(disks, [1]);
        assert!(!super::replace_if_sampled(&mut disks, Some(vec![1])));
        assert!(super::replace_if_sampled(&mut disks, Some(vec![2])));
        assert_eq!(disks, [2]);
    }

    #[test]
    fn a_sample_that_moved_a_field_publishes() {
        let (state, tx, mut rx) = state_and_channel();
        super::publish_if_changed(&state, &tx, |state| {
            let changed = state.cpu_percent != 42;
            state.cpu_percent = 42;
            changed
        });
        assert_eq!(rx.try_recv(), Ok(()));
        assert_eq!(state.lock().unwrap().cpu_percent, 42);
    }

    #[test]
    fn a_sample_that_measured_the_same_number_publishes_nothing() {
        // The whole point: an idle machine reports the same rounded percentage tick after tick,
        // and each publish would cost a scene re-resolve (ADR-0044 decision 2).
        let (state, tx, mut rx) = state_and_channel();
        state.lock().unwrap().cpu_percent = 7;
        super::publish_if_changed(&state, &tx, |state| {
            let changed = state.cpu_percent != 7;
            state.cpu_percent = 7;
            changed
        });
        assert!(rx.try_recv().is_err(), "an unchanged sample must not hydrate a snapshot");
    }

    #[test]
    fn sysinfo_state_default_is_zero_and_nil_before_the_first_sample() {
        let state = super::SysinfoState::default();
        assert_eq!(state.cpu_percent, 0);
        assert_eq!(state.ram_percent, 0);
        assert_eq!(state.swap_percent, 0);
        assert_eq!(state.temp_cores, Vec::<i64>::new());
        assert_eq!(state.temp_gpu, None, "undetected is nil, not 0");
        assert!(state.disks.is_empty());
        assert_eq!(state.gpu, None);
        assert_eq!(state.net_rx_bytes_sec, 0);
        assert_eq!(state.net_tx_bytes_sec, 0);
    }
}

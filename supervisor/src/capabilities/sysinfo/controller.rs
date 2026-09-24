//! [`SysinfoController`] runs three configurable poll tasks, for cpu, ram+swap, and
//! temp_cores+temp_gpu, feeding one `SysinfoState` (ADR-0035).

use std::time::Duration;

use shared::debug;

/// `mantle.sysinfo`'s payload; `nil` until `configure` sets an interval and a reading changes a field.
/// Pushes only on a change (ADR-0035).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SysinfoState {
    /// CPU utilization across all cores, `0` to `100`, rounded down; `0` until two samples form a delta.
    pub cpu_percent: u8,
    /// Physical memory in use (`MemTotal - MemAvailable`), `0` to `100`, rounded down.
    pub ram_percent: u8,
    /// Swap in use, `0` to `100`, rounded down; also `0` without swap.
    pub swap_percent: u8,
    /// CPU temperatures in whole Celsius: per core (`coretemp`) or per CCD (`k10temp`), else one
    /// package or `acpitz` reading; empty without a sensor. An unreadable sensor is skipped.
    pub temp_cores: Vec<i64>,
    /// `amdgpu`, `nouveau` or `nvidia` hwmon temperature in whole Celsius, or `-1` without a
    /// readable one.
    pub temp_gpu: i64,
}

impl Default for SysinfoState {
    /// Pre-first-sample sentinels (ADR-0035): `0` for the three percent fields; `temp_gpu` uses
    /// its IDL-mandated `-1`.
    fn default() -> Self {
        Self { cpu_percent: 0, ram_percent: 0, swap_percent: 0, temp_cores: Vec::new(), temp_gpu: -1 }
    }
}

/// Whether a metric task ticks or parks with zero wakeups (ADR-0035), reevaluated when its
/// `watch::Receiver` reports an interval change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollMode {
    /// `interval == 0`: no timer; await only `watch::Receiver::changed()`.
    Dormant,
    /// `interval != 0`: race `tokio::time::interval(_).tick()` against
    /// `watch::Receiver::changed()`.
    Ticking(Duration),
}

pub fn poll_mode(interval: Duration) -> PollMode {
    if interval.is_zero() { PollMode::Dormant } else { PollMode::Ticking(interval) }
}

/// Wakes `main.rs`'s `select!` to push a `StateSnapshot`; a named single variant keeps the arm
/// clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysinfoSignal {
    Changed,
}

/// `sysinfo:configure`'s table. Absent keys keep their interval; one wrong-typed key drops the call.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SysinfoConfigure {
    /// Seconds between CPU reads; `0` (the default) stops them.
    pub cpu_interval: Option<u64>,
    /// Seconds between memory and swap reads; `0` (the default) stops them.
    pub ram_interval: Option<u64>,
    /// Seconds between temperature reads; `0` (the default) stops them.
    pub temp_interval: Option<u64>,
}

/// Owns the three poll tasks and their state. Not `Clone`: synchronous, non-blocking `configure`
/// uses `&SysinfoController` directly.
pub struct SysinfoController {
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    cpu_interval: tokio::sync::watch::Sender<Duration>,
    ram_interval: tokio::sync::watch::Sender<Duration>,
    temp_interval: tokio::sync::watch::Sender<Duration>,
}

impl SysinfoController {
    /// Spawns all three dormant (`Duration::ZERO`) until Lua calls `configure`. They share
    /// `signal_tx` and signal only after real-tick state updates. Resolve temp inputs once here;
    /// `hwmon_root` is not threaded into the task.
    pub fn new(
        proc_root: std::path::PathBuf,
        hwmon_root: std::path::PathBuf,
        signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
    ) -> Self {
        let state = std::sync::Arc::new(std::sync::Mutex::new(SysinfoState::default()));

        let (cpu_interval, cpu_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (ram_interval, ram_rx) = tokio::sync::watch::channel(Duration::ZERO);
        let (temp_interval, temp_rx) = tokio::sync::watch::channel(Duration::ZERO);

        let core_inputs = super::temp::resolve_temp_cores_inputs(&hwmon_root);
        if core_inputs.is_empty() {
            debug!("no CPU temperature sensor found under {}; temp_cores will stay empty", hwmon_root.display());
        }
        let gpu_input = super::temp::resolve_gpu_input(&hwmon_root);
        if gpu_input.is_none() {
            debug!("no GPU temperature sensor found under {}; temp_gpu will report -1", hwmon_root.display());
        }

        tokio::spawn(run_cpu_task(proc_root.clone(), cpu_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_ram_task(proc_root, ram_rx, std::sync::Arc::clone(&state), signal_tx.clone()));
        tokio::spawn(run_temp_task(core_inputs, gpu_input, temp_rx, std::sync::Arc::clone(&state), signal_tx));

        Self { state, cpu_interval, ram_interval, temp_interval }
    }

    /// Applies parsed `sysinfo:configure(cfg)`: present intervals wake, retime, or suspend their
    /// task at `0`; absent ones stay unchanged. `send` errors only after task panic, logged here.
    pub fn configure(&self, cfg: SysinfoConfigure) {
        debug!("configure: cpu={:?} ram={:?} temp={:?}", cfg.cpu_interval, cfg.ram_interval, cfg.temp_interval);
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
    signal_tx: &tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
    write: impl FnOnce(&mut SysinfoState) -> bool,
) {
    // Drop the lock before sending: the receiver hydrates a snapshot and must never wait on a
    // poll task's mutex to do it.
    let changed = {
        let mut state = state.lock().expect("sysinfo state mutex poisoned");
        write(&mut state)
    };
    if changed {
        let _ = signal_tx.send(SysinfoSignal::Changed);
    }
}

/// One metric's poll loop: parked while its interval is zero, otherwise `tick` once per interval.
/// `previous` is the tick's memory across samples, cleared on going dormant: `/proc/stat` counters
/// are cumulative since boot, so a delta across a dormant spell is bogus.
async fn run_ticker<T>(mut interval_rx: tokio::sync::watch::Receiver<Duration>, mut tick: impl FnMut(&mut Option<T>)) {
    let mut previous = None;
    loop {
        let interval = *interval_rx.borrow_and_update();
        match poll_mode(interval) {
            PollMode::Dormant => {
                previous = None;
                if interval_rx.changed().await.is_err() {
                    return; // every SysinfoController that could reconfigure this task is gone
                }
            }
            PollMode::Ticking(duration) => {
                let mut ticker = tokio::time::interval(duration);
                ticker.tick().await; // tokio::time::interval's first tick fires immediately; consume it unused
                loop {
                    tokio::select! {
                        _ = ticker.tick() => tick(&mut previous),
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

/// `cpu_percent` task. The first tick after cold start or resume stores only a sample.
async fn run_cpu_task(
    proc_root: std::path::PathBuf,
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
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
    signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
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

/// `temp_cores`/`temp_gpu` task. Each tick reads only the inputs `new` resolved; `temp_gpu` rides
/// `temp_interval`.
async fn run_temp_task(
    core_inputs: Vec<std::path::PathBuf>,
    gpu_input: Option<std::path::PathBuf>,
    interval_rx: tokio::sync::watch::Receiver<Duration>,
    state: std::sync::Arc<std::sync::Mutex<SysinfoState>>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<SysinfoSignal>,
) {
    run_ticker(interval_rx, |_: &mut Option<()>| {
        let temp_cores = super::temp::read_temp_cores(&core_inputs);
        let temp_gpu = super::temp::read_temp_gpu(gpu_input.as_deref());
        publish_if_changed(&state, &signal_tx, |state| {
            let changed = state.temp_cores != temp_cores || state.temp_gpu != temp_gpu;
            state.temp_cores = temp_cores;
            state.temp_gpu = temp_gpu;
            changed
        });
    })
    .await
}

#[cfg(test)]
mod tests {
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
        tokio::sync::mpsc::UnboundedSender<super::SysinfoSignal>,
        tokio::sync::mpsc::UnboundedReceiver<super::SysinfoSignal>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (std::sync::Arc::new(std::sync::Mutex::new(super::SysinfoState::default())), tx, rx)
    }

    #[test]
    fn a_sample_that_moved_a_field_publishes() {
        let (state, tx, mut rx) = state_and_channel();
        super::publish_if_changed(&state, &tx, |state| {
            let changed = state.cpu_percent != 42;
            state.cpu_percent = 42;
            changed
        });
        assert_eq!(rx.try_recv(), Ok(super::SysinfoSignal::Changed));
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
    fn sysinfo_state_default_matches_the_pre_first_sample_sentinels() {
        let state = super::SysinfoState::default();
        assert_eq!(state.cpu_percent, 0);
        assert_eq!(state.ram_percent, 0);
        assert_eq!(state.swap_percent, 0);
        assert_eq!(state.temp_cores, Vec::<i64>::new());
        assert_eq!(state.temp_gpu, -1, "matches the IDL's own -1 undetected sentinel, not 0");
    }
}

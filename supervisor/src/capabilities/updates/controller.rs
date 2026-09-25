//! [`UpdatesController`]: `mantle.updates` write-action dispatcher and state owner (ADR-0034).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use shared::{debug, warn};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use super::backend::{Backend, UpdateCandidate};
use super::reboot::{REBOOT_MARKER, run_reboot_marker_task};
use crate::capabilities::system::controller::epoch_seconds;
use crate::process;

/// `mantle.updates`'s payload.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct UpdatesState {
    /// Package manager, e.g. `"pacman"`, from the first push, which comes at start; `nil` when none is
    /// supported, and then every action is ignored (ADR-0134).
    pub package_manager: Option<String>,
    /// AUR helper found at start, `"paru"` or `"yay"`, or `nil`; used only once `configure` sets
    /// `aur` (ADR-0250).
    pub aur_helper: Option<String>,
    /// Always `#packages`.
    pub count: u32,
    /// Pending upgrades. A failed check keeps the last good list.
    pub packages: Vec<UpdateCandidate>,
    /// Unix seconds of the last successful check (or the `checked_at` seed), else `nil`.
    pub last_successful_check: Option<i64>,
    /// Why the last check failed, or `nil` after a success. A check never modifies the system.
    pub check_error: Option<String>,
    /// Why AUR packages are missing: the last check's AUR query failed, or `aur` is on with no
    /// `aur_helper`; `nil` otherwise. `packages` still holds the repos' answer.
    pub aur_error: Option<String>,
    /// A check is running.
    pub checking: bool,
    /// Check failures in a row; a success resets it to `0`.
    pub consecutive_check_failures: u32,
    /// An install is running; the `install_*` fields describe the latest run.
    pub installing: bool,
    /// 1-based number of the package being installed, e.g. pacman's `(2/5)`; `0` before the first.
    pub install_current_step: u32,
    /// Packages in the transaction; `0` until the first step line, so draw progress as indeterminate.
    pub install_total_steps: u32,
    /// Package being installed; empty before the first step line.
    pub install_current_package: String,
    /// Package manager's exit code for the last install (`0` success); `nil` while running, before
    /// one, or when a signal killed it.
    pub install_exit_code: Option<i32>,
    /// Unix seconds when the last install's process ended, whatever its status; `nil` while
    /// running, before one, or when it failed to spawn.
    pub install_finished_at: Option<i64>,
    /// The last 200 lines of install output, stdout and stderr interleaved, newest last; cleared
    /// when an install starts.
    pub install_log: Vec<String>,
    /// Why the package manager could not be run or waited on, or `nil`. Its own failures are
    /// `install_exit_code`.
    pub install_error: Option<String>,
    /// `/run/mantle-reboot-required` exists, watched live. Mantle never writes it; anything you set up
    /// may, a pacman hook for example, and `/run` empties on reboot.
    pub reboot_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatesSignal {
    Changed,
}

/// `configure`'s table. One wrong-typed key drops the whole call (ADR-0034).
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct UpdatesConfigure {
    /// Seconds between scheduled checks, the first at once unless `last_successful_check` is
    /// younger; `0` checks only on `check`.
    #[serde(rename = "interval")]
    pub interval_secs: u64,
    /// Persisted Unix seconds of the last successful check. Seeds `last_successful_check` only
    /// while that is `nil`, so a restart need not recheck at once.
    pub checked_at: Option<i64>,
    /// Persisted `packages` from that check, seeded on the same terms; ignored without `checked_at`.
    #[serde(default, deserialize_with = "crate::capabilities::lua_list")]
    pub packages: Vec<UpdateCandidate>,
    /// Also check the AUR and install through `aur_helper` (ADR-0250). Sends every foreign package
    /// name to aur.archlinux.org and builds without PKGBUILD review.
    #[serde(default)]
    pub aur: bool,
}

/// Tail length for [`UpdatesState::install_log`]. Enough to hold a failure and nearby lines; a
/// 2,000-package run belongs in a file, not a state payload reserialized on every progress line.
const LOG_TAIL_LINES: usize = 200;

/// Cloneable so `main.rs` can hand an `Arc`-backed copy to the spawned install task.
#[derive(Clone)]
pub struct UpdatesController {
    /// Package manager backend, or `None`; actions then no-op instead of failing a check.
    backend: Option<Arc<dyn Backend>>,
    state: Arc<Mutex<UpdatesState>>,
    interval_tx: watch::Sender<Duration>,
    /// `updates:check` nudge. Capacity one plus `try_send` collapses a burst into one check.
    check_now_tx: tokio::sync::mpsc::Sender<()>,
    events: UnboundedSender<UpdatesSignal>,
}

impl UpdatesController {
    /// Detects the package manager and starts a dormant scheduler (`Duration::ZERO`) until
    /// `updates:configure`. Pushes immediately so `package_manager` can decide indicator presence,
    /// including on machines with no backend and no later scheduler event. This makes the indicator
    /// appear at login rather than after the first check.
    pub fn new(events: UnboundedSender<UpdatesSignal>) -> Self {
        Self::with_backend(super::backend::detect().map(Arc::from), PathBuf::from(REBOOT_MARKER), events)
    }

    /// [`UpdatesController::new`] with a caller-supplied backend and marker path for tests. The
    /// path is a parameter because a real `/run` marker, written by any pacman run on the machine
    /// running the suite, otherwise pushes an extra `Changed` into every scheduler test.
    fn with_backend(
        backend: Option<Arc<dyn Backend>>,
        reboot_marker: PathBuf,
        events: UnboundedSender<UpdatesSignal>,
    ) -> Self {
        let state = Arc::new(Mutex::new(UpdatesState {
            package_manager: backend.as_ref().map(|backend| backend.name().to_string()),
            aur_helper: backend.as_ref().and_then(|backend| backend.aur_helper()).map(String::from),
            ..UpdatesState::default()
        }));
        let (interval_tx, interval_rx) = watch::channel(Duration::ZERO);
        let (check_now_tx, check_now_rx) = tokio::sync::mpsc::channel(1);
        if let Some(backend) = backend.clone() {
            tokio::spawn(run_check_task(backend, interval_rx, check_now_rx, Arc::clone(&state), events.clone()));
            tokio::spawn(run_reboot_marker_task(reboot_marker, Arc::clone(&state), events.clone()));
        }
        let _ = events.send(UpdatesSignal::Changed);
        Self { backend, state, interval_tx, check_now_tx, events }
    }

    /// Sets the schedule and optionally seeds a remembered check time (ADR-0113 amendment). Uses
    /// the seed only before this process checks, never moving `last_successful_check` backwards.
    /// Seeding pushes because the field is Lua-visible.
    pub fn configure(&self, configure: UpdatesConfigure) {
        let Some(backend) = &self.backend else { return };
        debug!("configure: interval_secs={}", configure.interval_secs);
        let aur_error = backend.set_aur(configure.aur);
        let mut guard = self.state.lock().expect("mutex poisoned");
        let mut changed = guard.aur_error != aur_error;
        guard.aur_error = aur_error;
        if let Some(checked_at) = configure.checked_at
            && guard.last_successful_check.is_none()
        {
            guard.last_successful_check = Some(checked_at);
            guard.count = configure.packages.len() as u32;
            guard.packages = configure.packages;
            changed = true;
        }
        drop(guard);
        if changed {
            let _ = self.events.send(UpdatesSignal::Changed);
        }
        // Capped because tokio's `interval` adds it to an `Instant`, which aborts near `i64::MAX` seconds.
        if self.interval_tx.send(Duration::from_secs(configure.interval_secs.min(u32::MAX.into()))).is_err() {
            warn!("configure called but the check task is gone; ignored");
        }
    }

    /// `updates:check()`: runs one check regardless of schedule, including dormant mode. Refuses a
    /// second request while checking; the in-flight answer is the requested answer.
    pub fn check_now(&self) {
        if self.backend.is_none() {
            debug!("check() called on a machine with no package manager this Supervisor speaks; ignored");
            return;
        }
        if self.state.lock().expect("mutex poisoned").checking {
            debug!("check() called while a check is already running; ignored");
            return;
        }
        if self.check_now_tx.try_send(()).is_err() {
            debug!("check() could not be queued (one is already pending, or the check task is gone); ignored");
        }
    }

    /// `updates:install()`. Logged no-op without a backend or during another install; package
    /// managers share one database lock. The check-and-set is one critical section, so concurrent
    /// calls cannot both observe `installing == false` and launch upgrades.
    pub async fn install(&self) {
        let Some(backend) = self.backend.clone() else {
            debug!("install() called on a machine with no package manager this Supervisor speaks; ignored");
            return;
        };
        {
            let mut guard = self.state.lock().expect("mutex poisoned");
            if guard.installing {
                drop(guard);
                debug!("install() called while an install is already running; ignored");
                return;
            }
            guard.installing = true;
            guard.install_current_step = 0;
            guard.install_total_steps = 0;
            guard.install_current_package = String::new();
            guard.install_error = None;
            guard.install_exit_code = None;
            guard.install_finished_at = None;
            guard.install_log.clear();
        }
        let _ = self.events.send(UpdatesSignal::Changed);
        run_install(backend, Arc::clone(&self.state), self.events.clone()).await;
    }

    pub fn snapshot(&self) -> UpdatesState {
        self.state.lock().expect("mutex poisoned").clone()
    }
}

/// Runs until every `UpdatesController` (and its `Clone`s) drops. Spawned only with a backend. Each
/// check uses `tokio::task::spawn_blocking`: `Backend::check` waits on blocking subprocesses.
async fn run_check_task(
    backend: Arc<dyn Backend>,
    mut interval_rx: watch::Receiver<Duration>,
    mut check_now_rx: tokio::sync::mpsc::Receiver<()>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
) {
    loop {
        let interval = *interval_rx.borrow_and_update();
        if interval.is_zero() {
            // Dormant still answers `check_now`; a config may use a button without a timer.
            tokio::select! {
                changed = interval_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                asked = check_now_rx.recv() => {
                    if asked.is_none() {
                        return;
                    }
                    run_one_check(&backend, &state, &events).await;
                }
            }
        } else {
            let mut ticker = tokio::time::interval(interval);
            // Checks can exceed a short interval. `Burst` would hammer mirrors with missed
            // ticks; `Delay` resumes after the check.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // `interval` ticks immediately, so an hourly schedule checks now, not in an hour
            // (ADR-0113 amendment). Skip only when this process has a fresh check: the
            // controller outlives config generations, and an unconditional skip would let every
            // save reset the hour, so a day of editing would never check.
            if !first_check_is_due(
                state.lock().expect("mutex poisoned").last_successful_check,
                epoch_seconds(SystemTime::now()),
                interval,
            ) {
                ticker.tick().await;
            }
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        run_one_check(&backend, &state, &events).await;
                    }
                    asked = check_now_rx.recv() => {
                        if asked.is_none() {
                            return;
                        }
                        run_one_check(&backend, &state, &events).await;
                    }
                    changed = interval_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break; // interval reconfigured; rebuild the dormant/ticking outer loop
                    }
                }
            }
        }
    }
}

/// Runs one scheduled or manual check. Pushes when `checking` rises and when the result is written,
/// so the state is visible during the sync. Failures preserve `count`/`packages` and write
/// only `check_error`.
async fn run_one_check(
    backend: &Arc<dyn Backend>,
    state: &Arc<Mutex<UpdatesState>>,
    events: &UnboundedSender<UpdatesSignal>,
) {
    state.lock().expect("mutex poisoned").checking = true;
    let _ = events.send(UpdatesSignal::Changed);

    let backend = Arc::clone(backend);
    let result = tokio::task::spawn_blocking(move || backend.check()).await;

    let mut guard = state.lock().expect("mutex poisoned");
    guard.checking = false;
    match result.map_err(|join_err| format!("check task panicked: {join_err}")) {
        Ok(Ok(report)) => {
            guard.count = report.packages.len() as u32;
            guard.packages = report.packages;
            guard.aur_error = report.aur_error;
            guard.last_successful_check = Some(epoch_seconds(SystemTime::now()));
            guard.check_error = None;
            guard.consecutive_check_failures = 0;
        }
        Ok(Err(err)) | Err(err) => {
            guard.check_error = Some(err);
            guard.consecutive_check_failures = guard.consecutive_check_failures.saturating_add(1);
        }
    }
    drop(guard);
    let _ = events.send(UpdatesSignal::Changed);
}

/// Whether the interval's immediate first tick should check or be consumed. Due with no process
/// check, or when the last success is at least `interval` old.
fn first_check_is_due(last_successful_check: Option<i64>, now: i64, interval: Duration) -> bool {
    let Some(last) = last_successful_check else { return true };
    now.saturating_sub(last) >= interval.as_secs() as i64
}

/// Assumes `state.installing` and its progress fields were set by `UpdatesController::install`'s
/// atomic check-and-set. Runs `Backend::install_command` against the live system as root, reads
/// stdout line by line, parses progress into `state`, and never exposes raw output to Lua
/// (ADR-0034).
async fn run_install(
    backend: Arc<dyn Backend>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
) {
    let command = backend.install_command();
    let child = match process::spawn_group_leader_piped(&command.program, &command.arguments) {
        Ok(child) => child,
        Err(err) => {
            let message = format!("failed to spawn {}: {err}", command.program);
            warn!("{message}");
            let mut guard = state.lock().expect("mutex poisoned");
            guard.installing = false;
            guard.install_error = Some(message);
            drop(guard);
            let _ = events.send(UpdatesSignal::Changed);
            return;
        }
    };
    run_install_with_child(backend, state, events, child).await;
}

/// Testable stdout loop for [`run_install`]. Sends `UpdatesSignal::Changed` on every parsed line
/// (ADR-0034), not only at completion.
async fn run_install_with_child(
    backend: Arc<dyn Backend>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
    mut child: tokio::process::Child,
) {
    // Drain stderr concurrently: ~64KiB of warnings can fill the kernel pipe, block the
    // single-threaded manager, and leave `installing` stuck at `true`. Await the drain after exit;
    // detached reading can lose the final failure lines.
    let stderr_drain = child.stderr.take().map(|stderr| {
        let state = Arc::clone(&state);
        let events = events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(2; "install stderr: {line}");
                push_log_line(&mut state.lock().expect("mutex poisoned").install_log, line);
                let _ = events.send(UpdatesSignal::Changed);
            }
        })
    });

    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let step = backend.parse_install_step(&line);
            let mut guard = state.lock().expect("mutex poisoned");
            push_log_line(&mut guard.install_log, line);
            if let Some(step) = step {
                guard.install_current_step = step.current;
                guard.install_total_steps = step.total;
                guard.install_current_package = step.package;
            }
            drop(guard);
            // Every line, not only the ones that parse as progress: gating on progress left the
            // log frozen until the first `(n/m)`. The download phase stays silent regardless,
            // because pacman prints nothing per package without a tty (measured: its `wchar` does
            // not move for the whole download). A pty is the only cure and costs ANSI and `\r`
            // handling; `updates.count` and `download_size` cover the gap in config instead.
            let _ = events.send(UpdatesSignal::Changed);
        }
    }

    let status = child.wait().await;
    // Await after process exit; only then does the pipe close and the drain finish.
    if let Some(drain) = stderr_drain {
        let _ = drain.await;
    }
    let mut guard = state.lock().expect("mutex poisoned");
    guard.installing = false;
    guard.install_finished_at = Some(epoch_seconds(SystemTime::now()));
    match status {
        // `None` means the process was killed by a signal.
        Ok(status) => {
            if !status.success() {
                warn!("install exited with {status}: {}", guard.install_log.last().map_or("", String::as_str));
            }
            guard.install_exit_code = status.code();
        }
        Err(err) => guard.install_error = Some(format!("failed to wait on the install command: {err}")),
    }
    drop(guard);
    let _ = events.send(UpdatesSignal::Changed);
}

/// Appends to the install-log tail, dropping its oldest line at capacity. Keep a `Vec`, not a
/// `VecDeque`: the state serializes as a JSON array, and shifting 200 pointers is not the cost
/// here.
fn push_log_line(log: &mut Vec<String>, line: String) {
    if log.len() >= LOG_TAIL_LINES {
        log.remove(0);
    }
    log.push(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::updates::backend::{CheckReport, InstallCommand, InstallStep};

    /// A marker path under a fresh tempdir, so a scheduler test never watches the real `/run`
    /// file and never sees the `Changed` its presence would push.
    fn no_marker() -> std::path::PathBuf {
        tempfile::tempdir().unwrap().keep().join("reboot-required")
    }

    /// A backend whose check always fails without touching the network, and whose install-side
    /// answers are `pacman`'s real ones. What is under test around it is the scheduler and the
    /// stdout loop; the parsing has its own tests next to the parser.
    struct StubBackend;

    impl Backend for StubBackend {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn check(&self) -> Result<CheckReport, String> {
            Err("this stub cannot check anything".to_string())
        }

        fn install_command(&self) -> InstallCommand {
            InstallCommand { program: "true".to_string(), arguments: Vec::new() }
        }

        fn parse_install_step(&self, line: &str) -> Option<InstallStep> {
            crate::capabilities::updates::pacman::install::parse_install_step(line)
        }
    }

    fn candidate() -> UpdateCandidate {
        UpdateCandidate {
            name: "linux".into(),
            old_version: "6.1".into(),
            new_version: "6.2".into(),
            download_size: 42,
            installed_size: 0,
            repository: "core".into(),
        }
    }

    #[tokio::test]
    async fn a_remembered_check_seeds_an_empty_slot_with_its_list_and_never_overwrites_a_real_one() {
        let (controller, mut events_rx) = failing_controller().await;

        // Without a time the list has no age and is dropped, so a badge cannot light up from a
        // list nobody can call fresh.
        controller.configure(UpdatesConfigure {
            interval_secs: 0,
            checked_at: None,
            packages: vec![candidate()],
            aur: false,
        });
        assert_eq!(controller.snapshot().count, 0);

        controller.configure(UpdatesConfigure {
            interval_secs: 0,
            checked_at: Some(1_800_000_000),
            packages: vec![candidate()],
            aur: false,
        });
        let seeded = controller.snapshot();
        assert_eq!(seeded.last_successful_check, Some(1_800_000_000));
        assert_eq!(seeded.packages, vec![candidate()]);
        assert_eq!(seeded.count, 1, "`count` is always `#packages`, seeded or checked");
        assert_eq!(events_rx.recv().await, Some(UpdatesSignal::Changed), "a seed is Lua-visible, so it pushes");

        // A second seed is a later config reload, not a later check: the slot is taken.
        controller.configure(UpdatesConfigure {
            interval_secs: 0,
            checked_at: Some(1_700_000_000),
            packages: vec![],
            aur: false,
        });
        let kept = controller.snapshot();
        assert_eq!(
            kept.last_successful_check,
            Some(1_800_000_000),
            "this must never move the last-check time backwards"
        );
        assert_eq!(kept.count, 1);
    }

    /// A controller over [`StubBackend`], so every check fails without touching the network. What
    /// is under test is the scheduler around the check, not any real package manager: a real sync
    /// needs a real mirror and is verified live (see `pacman/check.rs`).
    ///
    /// The construction push is consumed here, so each test's own assertions start from the first
    /// signal it actually caused.
    async fn failing_controller() -> (UpdatesController, tokio::sync::mpsc::UnboundedReceiver<UpdatesSignal>) {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = UpdatesController::with_backend(Some(Arc::new(StubBackend)), no_marker(), events_tx);
        assert_eq!(events_rx.recv().await, Some(UpdatesSignal::Changed), "construction pushes the backend's name");
        (controller, events_rx)
    }

    /// The two pushes one check makes: `checking` up, then the answer.
    async fn await_one_check(events_rx: &mut tokio::sync::mpsc::UnboundedReceiver<UpdatesSignal>) {
        for _ in 0..2 {
            events_rx.recv().await.expect("the check task must push at both edges of a check");
        }
    }

    #[tokio::test]
    async fn a_manual_check_runs_with_no_schedule_configured_at_all() {
        // Dormant mode answers `check_now`; a config may use a button without a timer.
        let (controller, mut events_rx) = failing_controller().await;

        controller.check_now();
        await_one_check(&mut events_rx).await;

        let snapshot = controller.snapshot();
        assert!(!snapshot.checking, "checking must fall again once the answer is written");
        assert!(snapshot.check_error.is_some(), "a db root with no local/ cannot be checked");
        assert_eq!(snapshot.consecutive_check_failures, 1);
        assert_eq!(snapshot.last_successful_check, None);
    }

    #[tokio::test]
    async fn failed_checks_count_up_and_leave_the_last_good_answer_alone() {
        let (controller, mut events_rx) = failing_controller().await;
        // A count from an earlier good check, which a failure must not blank.
        controller.state.lock().unwrap().count = 3;

        controller.check_now();
        await_one_check(&mut events_rx).await;
        controller.check_now();
        await_one_check(&mut events_rx).await;

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.consecutive_check_failures, 2);
        assert_eq!(snapshot.count, 3, "a failed check reports the failure, it does not clear the list");
    }

    #[test]
    fn the_first_check_of_a_process_is_due_immediately() {
        // The boot case, and the whole point of the change: a config asking for an hourly check
        // wants to know what is pending now, not at the end of the first hour.
        assert!(first_check_is_due(None, 1_800_000_000, Duration::from_secs(3600)));
    }

    #[test]
    fn a_reconfigure_within_the_interval_waits_rather_than_syncing_again() {
        // A config reload re-invokes `configure`, and the controller outlives the generation that
        // did it. Without this, every save would be another sync against a mirror.
        let last = 1_800_000_000;
        assert!(!first_check_is_due(Some(last), last + 60, Duration::from_secs(3600)));
    }

    #[test]
    fn a_check_older_than_the_interval_is_due_again() {
        let last = 1_800_000_000;
        let interval = Duration::from_secs(3600);
        assert!(first_check_is_due(Some(last), last + 3600, interval), "exactly one interval old is due");
        assert!(first_check_is_due(Some(last), last + 7200, interval));
    }

    #[test]
    fn a_last_check_stamped_in_the_future_does_not_underflow_into_due() {
        // A clock stepped backwards (an NTP correction, a suspend across a timezone fix) leaves a
        // stamp ahead of `now`. Saturating, so that reads as "checked recently", not as a negative
        // age that compares below the interval by accident.
        let last = 1_800_000_000;
        assert!(!first_check_is_due(Some(last), last - 5000, Duration::from_secs(3600)));
    }

    #[tokio::test]
    async fn a_machine_with_no_package_manager_says_so_once_and_then_refuses_every_action() {
        // The whole point of the field: an indicator asks `package_manager` whether it belongs on
        // the bar, and on a machine with no manager nothing else would ever push to tell it.
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = UpdatesController::with_backend(None, no_marker(), events_tx);

        assert_eq!(events_rx.recv().await, Some(UpdatesSignal::Changed));
        assert_eq!(controller.snapshot().package_manager, None);

        controller.configure(UpdatesConfigure {
            interval_secs: 3600,
            checked_at: Some(1_800_000_000),
            packages: vec![],
            aur: false,
        });
        controller.check_now();
        controller.install().await;

        assert_eq!(
            controller.snapshot(),
            UpdatesState::default(),
            "no action may write state on a machine there is no manager to act with"
        );
        assert_eq!(events_rx.try_recv().ok(), None, "and none of them may push");
    }

    #[tokio::test]
    async fn a_detected_backend_names_itself_before_anything_has_been_checked() {
        let (controller, _events_rx) = failing_controller().await;

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.package_manager.as_deref(), Some("stub"));
        assert_eq!(snapshot.count, 0);
        assert_eq!(snapshot.last_successful_check, None, "naming the manager is not a check");
    }

    #[test]
    fn updates_state_default_has_no_updates_and_no_errors() {
        let state = UpdatesState::default();
        assert_eq!(state.package_manager, None);
        assert_eq!(state.count, 0);
        assert!(state.packages.is_empty());
        assert_eq!(state.last_successful_check, None);
        assert_eq!(state.check_error, None);
        assert!(!state.installing);
    }

    #[tokio::test]
    async fn run_install_with_child_parses_progress_and_detects_a_successful_completion() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo ':: Synchronizing package databases...'; echo '(1/2) installing nss (3.127-1 -> 3.128-1)'; echo '(2/2) upgrading gnome-autoar'; exit 0".to_string()],
        )
        .expect("spawn a stub install script");

        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert_eq!(snapshot.install_current_step, 2);
        assert_eq!(snapshot.install_total_steps, 2);
        assert_eq!(snapshot.install_current_package, "gnome-autoar");
        assert_eq!(snapshot.install_error, None);

        // Every line rides the signal, not only the two that parse as progress: the `::` line is
        // the shape the whole download phase prints, and gating on progress froze the log there.
        let mut signal_count = 0;
        while events_rx.try_recv().is_ok() {
            signal_count += 1;
        }
        assert_eq!(signal_count, 4, "one signal per output line plus one completion signal");
    }

    #[tokio::test]
    async fn run_install_with_child_reports_pacmans_own_exit_code_rather_than_a_sentence() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped("sh", &["-c".to_string(), "exit 1".to_string()])
            .expect("spawn a failing stub");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert_eq!(snapshot.install_exit_code, Some(1));
        assert!(snapshot.install_finished_at.is_some());
        assert_eq!(
            snapshot.install_error, None,
            "the package manager answering with a failure is not the Supervisor failing to ask"
        );
    }

    #[tokio::test]
    async fn the_install_log_keeps_both_streams_and_survives_a_line_that_is_not_progress() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let child = process::spawn_group_leader_piped(
            "sh",
            &["-c".to_string(), "echo ':: Synchronizing package databases...'; echo 'error: target not found' 1>&2; echo '(1/1) upgrading nss'; exit 0".to_string()],
        )
        .expect("spawn a chatty stub");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child).await;

        let log = state.lock().unwrap().install_log.clone();
        assert!(log.contains(&":: Synchronizing package databases...".to_string()), "{log:?}");
        assert!(log.contains(&"(1/1) upgrading nss".to_string()), "{log:?}");
        assert!(
            log.contains(&"error: target not found".to_string()),
            "stderr is where a package manager says why it failed, so it has to be in the tail by \
             the time the install is reported finished: {log:?}"
        );
    }

    #[test]
    fn the_install_log_drops_the_oldest_line_once_it_is_full() {
        let mut log: Vec<String> = Vec::new();
        for index in 0..(LOG_TAIL_LINES + 5) {
            push_log_line(&mut log, index.to_string());
        }

        assert_eq!(log.len(), LOG_TAIL_LINES);
        assert_eq!(log.first().map(String::as_str), Some("5"), "the oldest five are the ones gone");
        assert_eq!(log.last().map(String::as_str), Some((LOG_TAIL_LINES + 4).to_string().as_str()));
    }
}

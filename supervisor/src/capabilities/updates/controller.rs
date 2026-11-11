//! [`UpdatesController`]: `mantle.updates` write-action dispatcher and state owner (ADR-0034).

pub use shared::state::updates::UpdatesState;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use shared::{debug, warn};
use std::os::unix::fs::OpenOptionsExt;
use std::process::Stdio;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::time::Instant;

use super::backend::{Backend, InstallCommand};
use super::reboot::{REBOOT_MARKER, run_reboot_marker_task};
use crate::capabilities::epoch_seconds;
use crate::process::registry::BoundedLines;
use shared::action::UpdatesConfigure;
#[cfg(test)]
use shared::state::updates::UpdateCandidate;

/// Tail length for [`UpdatesState::install_log`]. Enough to hold a failure and nearby lines; a
/// 2,000-package run belongs in a file, not a state payload reserialized on every progress line.
const LOG_TAIL_LINES: usize = 200;

/// How often a scheduled check asks whether it is due; also the retry delay after a failure.
const CHECK_POLL: Duration = Duration::from_secs(300);

/// Cloneable so `main.rs` can hand an `Arc`-backed copy to the spawned install task.
#[derive(Clone)]
pub struct UpdatesController {
    /// Package manager backend, or `None`; actions then no-op instead of failing a check.
    backend: Option<Arc<dyn Backend>>,
    state: Arc<Mutex<UpdatesState>>,
    interval_tx: watch::Sender<Duration>,
    /// `updates:check` nudge. Capacity one plus `try_send` collapses a burst into one check.
    check_now_tx: tokio::sync::mpsc::Sender<()>,
    events: UnboundedSender<()>,
}

impl UpdatesController {
    /// Detects the package manager and starts a dormant scheduler (`Duration::ZERO`) until
    /// `updates:configure`. Pushes immediately so `package_manager` can decide indicator presence,
    /// including on machines with no backend and no later scheduler event. This makes the indicator
    /// appear at login rather than after the first check.
    pub fn new(events: UnboundedSender<()>) -> Self {
        Self::with_backend(super::backend::detect().map(Arc::from), PathBuf::from(REBOOT_MARKER), events)
    }

    /// [`UpdatesController::new`] with a caller-supplied backend and marker path for tests. The
    /// path is a parameter because a real `/run` marker, written by any pacman run on the machine
    /// running the suite, otherwise pushes an extra `Changed` into every scheduler test.
    fn with_backend(backend: Option<Arc<dyn Backend>>, reboot_marker: PathBuf, events: UnboundedSender<()>) -> Self {
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
        let _ = events.send(());
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
            guard.packages = configure.packages;
            changed = true;
        }
        drop(guard);
        if changed {
            let _ = self.events.send(());
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
        let _ = self.events.send(());
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
    events: UnboundedSender<()>,
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
            // `next` is an interval after the last start on tokio's clock, which stops in suspend, so
            // asking the wall clock every `CHECK_POLL` catches an overdue check and retries a failed
            // one. A config reload within the interval does not check again (ADR-0113 amendment).
            let wall = || {
                let last = state.lock().expect("mutex poisoned").last_successful_check;
                until_due(last, epoch_seconds(SystemTime::now()), interval)
            };
            let mut next = Instant::now() + wall();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(next.min(Instant::now() + CHECK_POLL)) => {
                        if Instant::now() >= next || wall().is_zero() {
                            next = Instant::now() + interval;
                            run_one_check(&backend, &state, &events).await;
                        }
                    }
                    asked = check_now_rx.recv() => {
                        if asked.is_none() {
                            return;
                        }
                        next = Instant::now() + interval;
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
/// so the state is visible during the sync. Failures preserve `packages` and write
/// only `check_error`.
async fn run_one_check(backend: &Arc<dyn Backend>, state: &Arc<Mutex<UpdatesState>>, events: &UnboundedSender<()>) {
    state.lock().expect("mutex poisoned").checking = true;
    let _ = events.send(());
    let started = epoch_seconds(SystemTime::now());

    // No timeout: every command a check runs has its own limit, and a check given up on would
    // still be running beside the next, sharing its download files.
    let backend = Arc::clone(backend);
    let result = tokio::task::spawn_blocking(move || backend.check())
        .await
        .map_err(|join_err| format!("check task panicked: {join_err}"));

    let mut guard = state.lock().expect("mutex poisoned");
    guard.checking = false;
    match result {
        Ok(Ok(report)) => {
            guard.packages = report.packages;
            guard.aur_error = report.aur_error;
            guard.last_successful_check = Some(started);
            guard.check_error = None;
            guard.consecutive_check_failures = 0;
        }
        Ok(Err(err)) | Err(err) => {
            guard.check_error = Some(err);
            guard.consecutive_check_failures = guard.consecutive_check_failures.saturating_add(1);
        }
    }
    drop(guard);
    let _ = events.send(());
}

/// Wall-clock time until a check is due: none with no successful check, once the last is
/// `interval` old, or when its stamp is in the future (a clock stepped back; the check rewrites it).
fn until_due(last_successful_check: Option<i64>, now: i64, interval: Duration) -> Duration {
    let Some(age) = last_successful_check.map(|last| now.saturating_sub(last)) else { return Duration::ZERO };
    u64::try_from(age).map_or(Duration::ZERO, |age| interval.saturating_sub(Duration::from_secs(age)))
}

/// Assumes `state.installing` and its progress fields were set by `UpdatesController::install`'s
/// atomic check-and-set. Runs `Backend::install_command` against the live system as root, reads
/// stdout line by line, parses progress into `state`, and never exposes raw output to Lua
/// (ADR-0034).
async fn run_install(backend: Arc<dyn Backend>, state: Arc<Mutex<UpdatesState>>, events: UnboundedSender<()>) {
    let command = backend.install_command();
    let log = shared::runtime_root().map(|root| root.join("install.log"));
    let (child, output) = match log.and_then(|log| spawn_logged(&command, &log, "tail")) {
        Ok(spawned) => spawned,
        Err(err) => {
            let message = format!("failed to spawn {}: {err}", command.program);
            warn!("{message}");
            let mut guard = state.lock().expect("mutex poisoned");
            guard.installing = false;
            guard.install_error = Some(message);
            drop(guard);
            let _ = events.send(());
            return;
        }
    };
    run_install_with_child(backend, state, events, child, output).await;
}

/// Starts `command` writing into `log` (opened only here, `O_NOFOLLOW`; root pacman inherits the fd),
/// and GNU `tail` following it until the command is reaped. No pipe, so our exit cannot SIGPIPE it.
/// The installer inherits a lock on the log with the fd, so while it runs another install is refused.
// ponytail: a restarted Supervisor neither knows nor shows a running install, only refuses a second.
// Upgrade: record the installer's pid beside the log and follow it again.
fn spawn_logged(command: &InstallCommand, log: &Path, follower: &str) -> std::io::Result<(Child, Option<ChildStdout>)> {
    let open = |write: bool| {
        let mut options = std::fs::OpenOptions::new();
        options.read(!write).append(write).create(write).mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW).open(log)
    };
    let (file, reader) = (open(true)?, open(false)?);
    crate::instance::take_lock(&file).map_err(|err| {
        std::io::Error::other(format!("an update started before the shell restarted is still running ({err})"))
    })?;
    file.set_len(0)?;
    let mut installer = Command::new(&command.program);
    installer.args(&command.arguments).process_group(0).stdin(Stdio::null()).stdout(file.try_clone()?).stderr(file);
    let child = installer.spawn()?;
    let pid = format!("--pid={}", child.id().unwrap_or_default());
    // Without a reader the install still runs, so it is still tracked to its exit.
    let tail = Command::new(follower).args(["-c", "+1", "-f", &pid]).stdin(reader).stdout(Stdio::piped()).spawn();
    let tail = tail.map_err(|err| warn!("cannot follow {}: {err}; install_log stays empty", log.display()));
    Ok((child, tail.ok().and_then(|mut tail| tail.stdout.take())))
}

/// Testable stdout loop for [`run_install`]. Wakes `main.rs` on every parsed line
/// (ADR-0034), not only at completion.
async fn run_install_with_child(
    backend: Arc<dyn Backend>,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<()>,
    mut child: tokio::process::Child,
    output: Option<ChildStdout>,
) {
    let read = async {
        let Some(output) = output else { return };
        let mut lines = BoundedLines::new(output, "install log".into());
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
            // Every line, not only progress: gating left the log frozen until the first package operation.
            // Pacman prints nothing per package during a download without a tty (a pty would cost
            // ANSI and `\r` handling), so that phase stays silent
            // (its `wchar` does not move); `#updates.packages` and `download_size` cover the gap in config.
            let _ = events.send(());
        }
    };
    // Reap while reading: `tail --pid` takes an unreaped installer for a live one.
    let (status, ()) = tokio::join!(child.wait(), read);
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
    let _ = events.send(());
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
    use crate::capabilities::test_support::{poll_until, within};
    use crate::capabilities::updates::backend::{CheckReport, InstallStep};
    use crate::process;

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
            crate::capabilities::updates::pacman::install::parse_install_step(&mut Default::default(), line)
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
        assert!(controller.snapshot().packages.is_empty());

        controller.configure(UpdatesConfigure {
            interval_secs: 0,
            checked_at: Some(1_800_000_000),
            packages: vec![candidate()],
            aur: false,
        });
        let seeded = controller.snapshot();
        assert_eq!(seeded.last_successful_check, Some(1_800_000_000));
        assert_eq!(seeded.packages, vec![candidate()]);
        assert_eq!(events_rx.recv().await, Some(()), "a seed is Lua-visible, so it pushes");

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
        assert_eq!(kept.packages, vec![candidate()]);
    }

    /// A controller over [`StubBackend`], so every check fails without touching the network. What
    /// is under test is the scheduler around the check, not any real package manager: a real sync
    /// needs a real mirror and is verified live (see `pacman/check.rs`).
    ///
    /// The construction push is consumed here, so each test's own assertions start from the first
    /// signal it actually caused.
    async fn failing_controller() -> (UpdatesController, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = UpdatesController::with_backend(Some(Arc::new(StubBackend)), no_marker(), events_tx);
        assert_eq!(events_rx.recv().await, Some(()), "construction pushes the backend's name");
        (controller, events_rx)
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_check_is_never_overlapped_by_the_next() {
        // Two pacman checks at once share the sync root's `.db.part` files and can swap a
        // half-downloaded db in.
        struct Hung(std::sync::Mutex<std::sync::mpsc::Receiver<()>>, std::sync::atomic::AtomicUsize);
        impl Backend for Hung {
            fn name(&self) -> &'static str {
                "hung"
            }
            fn check(&self) -> Result<CheckReport, String> {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = self.0.lock().unwrap().recv();
                Err("released".into())
            }
            fn install_command(&self) -> InstallCommand {
                StubBackend.install_command()
            }
            fn parse_install_step(&self, _: &str) -> Option<InstallStep> {
                None
            }
        }
        let (release, hold) = std::sync::mpsc::channel();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let backend = Arc::new(Hung(hold.into(), Default::default()));
        let controller = UpdatesController::with_backend(Some(backend.clone()), no_marker(), events_tx);
        events_rx.recv().await.unwrap();

        controller.check_now();
        events_rx.recv().await.unwrap();
        // Paused time does not auto-advance while a blocking task is outstanding.
        tokio::time::advance(Duration::from_secs(3600)).await;
        for _ in 0..100 {
            if !controller.snapshot().checking {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(controller.snapshot().checking, "a check still running was given up on");
        controller.check_now();
        drop(release);
        events_rx.recv().await.unwrap();
        assert_eq!(backend.1.load(std::sync::atomic::Ordering::SeqCst), 1, "a second check ran beside the first");
        assert!(!controller.snapshot().checking);
    }

    /// The two pushes one check makes: `checking` up, then the answer.
    async fn await_one_check(events_rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>) {
        for _ in 0..2 {
            events_rx.recv().await.expect("the check task must push at both edges of a check");
        }
    }

    #[tokio::test]
    async fn failed_checks_count_up_and_leave_the_last_good_answer_alone() {
        let (controller, mut events_rx) = failing_controller().await;
        let fresh = controller.snapshot();
        assert_eq!(fresh.package_manager.as_deref(), Some("stub"));
        assert!(fresh.packages.is_empty());
        assert_eq!(fresh.last_successful_check, None, "naming the manager is not a check");
        // A count from an earlier good check, which a failure must not blank.
        controller.state.lock().unwrap().packages = vec![candidate(); 3];

        // Dormant mode answers `check_now`; a config may use a button without a timer.
        controller.check_now();
        await_one_check(&mut events_rx).await;
        let snapshot = controller.snapshot();
        assert!(!snapshot.checking, "checking must fall again once the answer is written");
        assert!(snapshot.check_error.is_some());
        assert_eq!(snapshot.consecutive_check_failures, 1);
        assert_eq!(snapshot.last_successful_check, None);

        controller.check_now();
        await_one_check(&mut events_rx).await;

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.consecutive_check_failures, 2);
        assert_eq!(snapshot.packages.len(), 3, "a failed check reports the failure, it does not clear the list");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_scheduled_check_is_retried_within_minutes_not_a_whole_interval() {
        let (controller, mut events_rx) = failing_controller().await;
        controller.configure(UpdatesConfigure {
            interval_secs: 86_400,
            checked_at: None,
            packages: vec![],
            aur: false,
        });
        await_one_check(&mut events_rx).await;
        assert_eq!(controller.snapshot().consecutive_check_failures, 1);

        tokio::time::timeout(Duration::from_secs(301), await_one_check(&mut events_rx))
            .await
            .expect("a failure at login, before the network is up, must not wait a day");
        assert_eq!(controller.snapshot().consecutive_check_failures, 2);
    }

    /// A backend whose every check succeeds once the test sends on its channel, so a check lasts
    /// as long as the test advances paused time before releasing it.
    struct Released(std::sync::Mutex<std::sync::mpsc::Receiver<()>>);

    impl Backend for Released {
        fn name(&self) -> &'static str {
            "released"
        }
        fn check(&self) -> Result<CheckReport, String> {
            let _ = self.0.lock().unwrap().recv();
            Ok(CheckReport { packages: vec![], aur_error: None })
        }
        fn install_command(&self) -> InstallCommand {
            StubBackend.install_command()
        }
        fn parse_install_step(&self, _: &str) -> Option<InstallStep> {
            None
        }
    }

    #[tokio::test(start_paused = true)]
    async fn scheduled_checks_start_one_interval_apart_however_long_each_takes() {
        for interval in [60, 3600] {
            let (release, hold) = std::sync::mpsc::channel();
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            let controller =
                UpdatesController::with_backend(Some(Arc::new(Released(hold.into()))), no_marker(), events_tx);
            events_rx.recv().await.unwrap();
            controller.configure(UpdatesConfigure {
                interval_secs: interval,
                checked_at: None,
                packages: vec![],
                aur: false,
            });
            let mut starts = Vec::new();
            for _ in 0..3 {
                tokio::time::timeout(Duration::from_secs(interval + 600), events_rx.recv())
                    .await
                    .expect("the next scheduled check never started");
                starts.push(tokio::time::Instant::now());
                // A check lasting 5 s of paused time.
                tokio::time::advance(Duration::from_secs(5)).await;
                release.send(()).unwrap();
                events_rx.recv().await.unwrap();
            }
            for pair in starts.windows(2) {
                let gap = (pair[1] - pair[0]).as_secs_f64();
                assert!((gap - interval as f64).abs() < 1.0, "interval {interval}: checks started {gap} s apart");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_remembered_check_waits_out_the_rest_of_its_interval_unless_it_is_from_the_future() {
        for (age, wait) in [(60, 3540), (-86_400, 0)] {
            let (controller, mut events_rx) = failing_controller().await;
            controller.configure(UpdatesConfigure {
                interval_secs: 3600,
                checked_at: Some(epoch_seconds(SystemTime::now()) - age),
                packages: vec![],
                aur: false,
            });
            events_rx.recv().await.expect("a seed pushes");
            let start = tokio::time::Instant::now();
            await_one_check(&mut events_rx).await;
            // A clock stepped back must not stall checks until it catches up.
            // A wall-clock second may tick between the seed and the scheduler reading it.
            let waited = start.elapsed().as_secs();
            assert!(waited + 1 >= wait && waited <= wait, "a check stamped {age} s ago waited {waited} s");
        }
    }

    #[test]
    fn a_check_is_due_by_the_age_of_the_last_stamp() {
        let last = 1_800_000_000;
        let hour = Duration::from_secs(3600);
        // (last stamp, now, wait). A reload re-invokes `configure` on a controller that outlives the
        // generation, so a reconfigure within the interval must not sync again.
        for (stamp, now, wait, case) in [
            (None, last, 0, "the first check of a process is due at once"),
            (Some(last), last + 60, 3540, "a reconfigure within the interval waits out the rest"),
            (Some(last), last + 3600, 0, "exactly one interval old is due"),
            (Some(last), last + 7200, 0, "older than the interval"),
            // A clock stepped backwards leaves a stamp ahead of `now`. Reading it as recent would
            // stall checks until the clock catches up, across restarts since `checked_at` persists.
            (Some(last), last - 5000, 0, "a stamp in the future is due, and the check rewrites it"),
            (Some(i64::MAX), i64::MIN, 0, "and the subtraction cannot overflow"),
        ] {
            assert_eq!(until_due(stamp, now, hour), Duration::from_secs(wait), "{case}");
        }
    }

    #[tokio::test]
    async fn a_machine_with_no_package_manager_says_so_once_and_then_refuses_every_action() {
        // The whole point of the field: an indicator asks `package_manager` whether it belongs on
        // the bar, and on a machine with no manager nothing else would ever push to tell it.
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = UpdatesController::with_backend(None, no_marker(), events_tx);

        assert_eq!(events_rx.recv().await, Some(()));
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

    /// A fake installer running `script`, logged under a fresh tempdir.
    fn installer(script: &str) -> (Child, Option<ChildStdout>) {
        let log = tempfile::tempdir().unwrap().keep().join("install.log");
        spawn_logged(&InstallCommand { program: "sh".into(), arguments: process::sh_args(script) }, &log, "tail")
            .unwrap()
    }

    #[tokio::test]
    async fn an_installer_outlives_its_reader_and_its_whole_output_lands_in_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let (log, go) = (dir.path().join("install.log"), dir.path().join("go"));
        // Waits, within a bound, for the test to drop its reader, then writes on.
        let script = format!(
            "echo first; i=0; while [ ! -e '{}' ] && [ $i -lt 500 ]; do sleep 0.01; i=$((i+1)); done; \
             i=0; while [ $i -lt 2000 ]; do echo line$i; i=$((i+1)); done",
            go.display()
        );
        let command = InstallCommand { program: "sh".into(), arguments: process::sh_args(&script) };
        let (mut child, output) = spawn_logged(&command, &log, "tail").unwrap();
        let mut lines = BoundedLines::new(output.unwrap(), "test".into());
        assert_eq!(within(lines.next_line()).await.unwrap().as_deref(), Some("first"));
        // The Supervisor exits, restarts or crashes.
        drop(lines);
        std::fs::write(&go, "").unwrap();

        assert_eq!(within(child.wait()).await.unwrap().code(), Some(0), "the installer died with its reader");
        let written = std::fs::read_to_string(&log).unwrap();
        assert_eq!(written.lines().count(), 2001);
        assert_eq!(written.lines().last(), Some("line1999"));
    }

    #[tokio::test]
    async fn a_symlink_planted_at_the_install_log_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (log, target) = (dir.path().join("install.log"), dir.path().join("target"));
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &log).unwrap();

        let command = InstallCommand { program: "true".into(), arguments: vec![] };
        assert!(spawn_logged(&command, &log, "tail").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }

    #[tokio::test]
    async fn run_install_with_child_parses_progress_reports_the_exit_code_and_logs_both_streams() {
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let (child, output) = installer(
            "echo ':: Synchronizing package databases...'; echo '(1/2) installing nss (3.127-1 -> 3.128-1)'; echo '(2/2) upgrading gnome-autoar'; exit 0",
        );

        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child, output).await;

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

        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let (child, output) = installer("exit 1");

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child, output).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert_eq!(snapshot.install_exit_code, Some(1), "pacman's own exit code rather than a sentence");
        assert!(snapshot.install_finished_at.is_some());
        assert_eq!(
            snapshot.install_error, None,
            "the package manager answering with a failure is not the Supervisor failing to ask"
        );

        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let (child, output) = installer(
            "echo ':: Synchronizing package databases...'; echo 'error: target not found' 1>&2; echo '(1/1) upgrading nss'; exit 0",
        );

        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child, output).await;

        let log = state.lock().unwrap().install_log.clone();
        assert!(log.contains(&":: Synchronizing package databases...".to_string()), "{log:?}");
        assert!(log.contains(&"(1/1) upgrading nss".to_string()), "{log:?}");
        assert!(
            log.contains(&"error: target not found".to_string()),
            "stderr is where a package manager says why it failed, so it has to be in the tail by \
             the time the install is reported finished: {log:?}"
        );
    }

    #[tokio::test]
    async fn pacman_piped_install_counts_packages_and_resets_between_runs() {
        use crate::capabilities::updates::pacman::PacmanBackend;

        let backend = Arc::new(PacmanBackend::new(Default::default(), Default::default(), None));
        // Reconstructed from pacman's C-locale summary and package-operation callbacks.
        for (transcript, expected, exit_code) in [
            (
                "Packages (5) old-1 [removal] new-2 kvantum-2 linux-2 lib-1
:: Processing package changes...
removing old...
installing new...
upgrading kvantum...
reinstalling linux...
downgrading lib...
",
                (5, 5, "lib"),
                0,
            ),
            (
                "Packages (2) first-1 second-1
installing first...
Packages (1) helper-1
installing helper...
",
                (1, 1, "helper"),
                0,
            ),
            ("", (0, 0, ""), 1),
            (
                "upgrading orphan...
",
                (0, 0, "orphan"),
                0,
            ),
        ] {
            backend.install_command();
            let state = Arc::new(Mutex::new(UpdatesState::default()));
            let (child, output) = installer(&format!(
                "cat <<'PACMAN'
{transcript}PACMAN
exit {exit_code}"
            ));
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            run_install_with_child(backend.clone(), state.clone(), events_tx, child, output).await;
            let snapshot = state.lock().unwrap().clone();
            assert_eq!(
                (
                    snapshot.install_current_step,
                    snapshot.install_total_steps,
                    snapshot.install_current_package.as_str()
                ),
                expected,
            );
            assert_eq!(snapshot.install_exit_code, Some(exit_code));
            assert_eq!(snapshot.install_log, transcript.lines().collect::<Vec<_>>());
            let mut signals = 0;
            while events_rx.try_recv().is_ok() {
                signals += 1;
            }
            assert_eq!(signals, transcript.lines().count() + 1);
        }
    }

    #[tokio::test]
    async fn a_line_that_is_not_utf8_neither_ends_the_log_nor_breaks_the_install() {
        // A Latin-1 scriptlet line. Stopping the read there closes the pipe, and the child's next
        // write dies of SIGPIPE mid-transaction.
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let script =
            "printf 'caf\\351\\n'; printf 'err\\351\\n' >&2; echo after; echo after-err >&2; printf no-newline";
        let (child, output) = installer(script);
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child, output).await;

        let snapshot = state.lock().unwrap().clone();
        let mut log = snapshot.install_log;
        log.sort();
        assert_eq!(log, ["after", "after-err", "caf\u{FFFD}", "err\u{FFFD}", "no-newline"]);
        assert_eq!(snapshot.install_exit_code, Some(0), "the child must outlive its odd byte");
    }

    #[tokio::test]
    async fn an_install_whose_log_reader_cannot_start_is_still_tracked_to_its_exit() {
        let log = tempfile::tempdir().unwrap().keep().join("install.log");
        let command = InstallCommand { program: "sh".into(), arguments: process::sh_args("echo unread; exit 3") };
        let (child, output) = spawn_logged(&command, &log, "/nonexistent/tail")
            .expect("the installer is running, so the install must be tracked rather than reported failed");
        let state = Arc::new(Mutex::new(UpdatesState { installing: true, ..UpdatesState::default() }));
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        run_install_with_child(Arc::new(StubBackend), Arc::clone(&state), events_tx, child, output).await;

        let snapshot = state.lock().unwrap().clone();
        assert!(!snapshot.installing);
        assert_eq!((snapshot.install_exit_code, snapshot.install_error), (Some(3), None));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "unread\n", "the file still has it");
    }

    #[tokio::test]
    async fn a_second_install_is_refused_while_the_first_still_writes_the_log() {
        // The Supervisor restarted mid-install, so nothing but the log knows the first still runs.
        let dir = tempfile::tempdir().unwrap();
        let (log, go) = (dir.path().join("install.log"), dir.path().join("go"));
        let sh = |script: String| InstallCommand { program: "sh".into(), arguments: process::sh_args(&script) };
        let first = format!(
            "echo first; i=0; while [ ! -e '{}' ] && [ $i -lt 500 ]; do sleep 0.01; i=$((i+1)); done; echo last",
            go.display()
        );
        let (mut first, _output) = spawn_logged(&sh(first), &log, "tail").unwrap();
        poll_until(|| std::fs::read_to_string(&log).ok().filter(|text| text == "first\n")).await;

        let refused = spawn_logged(&sh("echo second".into()), &log, "tail");
        assert!(refused.unwrap_err().to_string().contains("still running"));
        std::fs::write(&go, "").unwrap();
        within(first.wait()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "first\nlast\n", "the first install's log is whole");

        let (mut next, _output) = spawn_logged(&sh("echo next".into()), &log, "tail").unwrap();
        within(next.wait()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "next\n", "a finished install's log is replaced");
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

//! [`ProcessesController`] owns the programs a config declared with `session_process`.
//!
//! The difference from `process.run` is lifetime, and it is the whole feature. A `process.run`
//! child belongs to the generation that spawned it, and `reap_processes` kills its
//! group when that Renderer is replaced; a config wanting a program to survive has to orphan it
//! with `setsid` and then re-find it through a lock file and `/proc`, because by then nothing in
//! the shell still holds it. That costs a config a pid, the pid's kernel start time, and a
//! two-second poll, all to answer a question the kernel would answer for free to whoever held the
//! handle.
//!
//! Here the Supervisor holds it. The Supervisor outlives every Renderer, so a replacement is not
//! an event a session process can observe, and the identity question never arises.
//!
//! ## One task per running program, and why signalling goes through it
//!
//! [`supervise`] owns the `Child` and selects between its exit and a request channel. Every signal
//! is therefore sent from the task that has not yet reaped the process, so the pid it names is
//! still reserved by the kernel and cannot have been recycled under it. Sending from the
//! controller instead -- by keeping a pid in the map -- would reopen exactly the window the pid
//! plus start-time pair exists to cover elsewhere. `tokio::process::Child::wait` is cancel-safe,
//! which is what lets the `select!` re-arm it after each request.

pub use shared::state::processes::{ProcessesState, SessionProcess};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use shared::{debug, error, warn};
use tokio::process::Child;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::capabilities::clock::controller::epoch_seconds;

/// How long a stopping program has after its declared stop signal before `SIGKILL`.
///
/// Deliberately not `process::DEFAULT_REAP_GRACE`. That 100ms suits a `process.run` helper that
/// has nothing to finish; a session process is declared precisely because it is doing something
/// long, and the first thing that will use this writes a video container whose index is appended
/// on the way out. Killing it at 100ms would leave the file unplayable, which is the failure the
/// declared stop signal exists to avoid. Tests wait it out in real time, so they get a shorter one.
const STOP_GRACE: Duration = Duration::from_millis(if cfg!(test) { 2000 } else { 5000 });

/// What [`supervise`] accepts while its program is up.
enum Request {
    /// One signal to the process itself, not its group: a pause or a reload is addressed to the
    /// program a config named, not to helpers it happened to spawn.
    Signal(Signal),
    /// Stop the group with the signal the declaration names when this is handled, escalating to
    /// `SIGKILL` after [`STOP_GRACE`].
    Stop,
}

/// A declared name. `live` exists only while its program is up.
struct Entry {
    stop_signal: Signal,
    live: Option<Live>,
    public: SessionProcess,
}

/// The running half: the channel [`supervise`] selects on and its handle for the shutdown reap to
/// await.
struct Live {
    requests: UnboundedSender<Request>,
    task: JoinHandle<()>,
}

type Entries = Arc<Mutex<HashMap<String, Entry>>>;

pub struct ProcessesController {
    entries: Entries,
    signal_tx: UnboundedSender<()>,
}

impl ProcessesController {
    pub fn new(signal_tx: UnboundedSender<()>) -> Self {
        Self { entries: Arc::new(Mutex::new(HashMap::new())), signal_tx }
    }

    pub fn snapshot(&self) -> ProcessesState {
        let guard = self.entries.lock().expect("processes entries mutex poisoned");
        ProcessesState { sessions: guard.iter().map(|(name, entry)| (name.clone(), entry.public.clone())).collect() }
    }

    /// `session_process { name, stop_signal }`, re-sent every evaluation the way `storage:open`
    /// is, so an edited stop signal lands on reload without disturbing a program already up.
    ///
    /// Declaring is what makes a name exist; `start` on an undeclared one is refused rather than
    /// creating the entry, so a config cannot run a program it never named.
    pub fn declare(&self, name: &str, stop_signal: Signal) {
        debug!("session_process declared: {name:?} signal={stop_signal:?}");
        let mut guard = self.entries.lock().expect("processes entries mutex poisoned");
        match guard.get_mut(name) {
            Some(entry) => entry.stop_signal = stop_signal,
            None => {
                guard.insert(name.to_string(), Entry { stop_signal, live: None, public: SessionProcess::default() });
            }
        }
        drop(guard);
        let _ = self.signal_tx.send(());
    }

    /// Spawns `cmd`, replacing this name's last run. A name already up is left alone: `start`
    /// twice is the same program, not two of it, and the config's own `running` says which.
    ///
    /// stdio is inherited rather than piped. A session process outlives the generation that
    /// started it, so there is no callback for its output to reach -- the shell's own log is the
    /// honest destination, and a config that wants a program's output wants `process.run`.
    pub fn start(&self, name: &str, cmd: &str, args: &[String]) {
        let mut guard = self.entries.lock().expect("processes entries mutex poisoned");
        let Some(entry) = guard.get_mut(name) else {
            debug!("refused to start {name:?}; no session_process declared that name");
            return;
        };
        if entry.public.running {
            debug!("{name:?} is already running as pid {:?}; ignoring start", entry.public.pid);
            return;
        }

        entry.public = SessionProcess::default();
        match crate::process::spawn_group_leader(cmd, args, &[]) {
            Ok(child) => {
                let pid = child.id();
                entry.public.running = true;
                entry.public.pid = pid;
                entry.public.started_at = Some(epoch_seconds(SystemTime::now()));
                let (requests_tx, requests_rx) = unbounded_channel();
                let task = tokio::spawn(supervise(
                    name.to_string(),
                    child,
                    requests_rx,
                    Arc::clone(&self.entries),
                    self.signal_tx.clone(),
                ));
                entry.live = Some(Live { requests: requests_tx, task });
            }
            Err(err) => {
                // The config is waiting on `running`; a reason it cannot read is a hang.
                entry.public.start_error = format!("{cmd}: {err}");
                warn!("{name:?} failed to start {cmd:?}: {err}");
            }
        }
        drop(guard);
        let _ = self.signal_tx.send(());
    }

    /// Sends one signal to a running program. Silent when it is not up: a config acting on state
    /// one push old is ordinary, not an error worth a log line per click.
    pub fn signal(&self, name: &str, signal: Signal) {
        self.request(name, Request::Signal(signal));
    }

    /// Asks a running program to stop with the signal its declaration named.
    pub fn stop(&self, name: &str) {
        self.request(name, Request::Stop);
    }

    fn request(&self, name: &str, request: Request) {
        let guard = self.entries.lock().expect("processes entries mutex poisoned");
        if let Some(live) = guard.get(name).and_then(|entry| entry.live.as_ref()) {
            let _ = live.requests.send(request);
        }
    }

    /// Stops every running program and waits for it, the counterpart to `reap_processes`.
    ///
    /// Awaited rather than fired and forgotten: these are the processes whose exit path matters,
    /// which is why they were declared with a stop signal in the first place. [`supervise`] bounds
    /// its own wait at `STOP_GRACE` plus 2s, so this cannot hold shutdown open indefinitely.
    pub async fn reap_all(&self) {
        let live: Vec<(String, Live)> = {
            let mut guard = self.entries.lock().expect("processes entries mutex poisoned");
            guard.iter_mut().filter_map(|(name, entry)| entry.live.take().map(|live| (name.clone(), live))).collect()
        };
        for (name, live) in live {
            let _ = live.requests.send(Request::Stop);
            // Dropping `live.requests` also reads as a stop to `supervise`; the explicit request
            // above covers the ordinary case where it is still selecting.
            drop(live.requests);
            if let Err(err) = live.task.await {
                warn!("{name:?}'s supervising task did not finish cleanly on shutdown: {err}");
            }
        }
    }
}

/// Owns one running program until it exits, and is the only place its pid is signalled.
async fn supervise(
    name: String,
    mut child: Child,
    mut requests: UnboundedReceiver<Request>,
    entries: Entries,
    signal_tx: UnboundedSender<()>,
) {
    let Some(raw_pid) = child.id() else {
        error!("{name:?} spawned without a pid; nothing to supervise");
        return;
    };
    let pid = Pid::from_raw(raw_pid as i32);

    let status = loop {
        tokio::select! {
            // Cancel-safe, so losing this race to a request leaves the wait intact.
            status = child.wait() => break status,
            request = requests.recv() => match request {
                Some(Request::Signal(signal)) => {
                    // The process, not the group: a pause belongs to the program that was named.
                    if let Err(err) = crate::process::gone_is_ok(kill(pid, signal)) {
                        warn!("{name:?} could not be sent {signal}: {err}");
                    }
                }
                // `None` means the controller is gone and nothing can ask again; leaving the
                // program orphaned would be worse than stopping it.
                Some(Request::Stop) | None => {
                    let stop_signal = declared_stop_signal(&entries, &name);
                    break crate::process::stop_process_group(&mut child, stop_signal, STOP_GRACE).await;
                }
            },
        }
    };

    let exit_code = match status {
        Ok(status) => status.code(),
        Err(err) => {
            warn!("{name:?} could not be waited on: {err}");
            None
        }
    };

    {
        let mut guard = entries.lock().expect("processes entries mutex poisoned");
        if let Some(entry) = guard.get_mut(&name) {
            entry.public.running = false;
            entry.public.exit_code = exit_code;
            entry.live = None;
        }
    }
    let _ = signal_tx.send(());
}

/// The signal `name`'s declaration names *now*, read here rather than captured when [`supervise`]
/// started: a reload redeclares under a program already up, and `start` is long past. Names are
/// only ever added to the map, so the fallback is unreachable and `SIGTERM` merely has to stop a
/// program nothing can describe any more.
fn declared_stop_signal(entries: &Entries, name: &str) -> Signal {
    let guard = entries.lock().expect("processes entries mutex poisoned");
    guard.get(name).map_or(Signal::SIGTERM, |entry| entry.stop_signal)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    /// A controller whose programs die with the test. A panic drops `supervise` with its child
    /// unreaped, and the loops below would run on under the user's session.
    struct Reaping {
        controller: ProcessesController,
        _signals: UnboundedReceiver<()>,
    }

    impl std::ops::Deref for Reaping {
        type Target = ProcessesController;
        fn deref(&self) -> &ProcessesController {
            &self.controller
        }
    }

    impl Drop for Reaping {
        fn drop(&mut self) {
            for pid in self.snapshot().sessions.values().filter(|s| s.running).filter_map(|s| s.pid) {
                let _ = nix::sys::signal::killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
        }
    }

    /// Tests read `snapshot`; the signal channel is only held open.
    fn controller() -> Reaping {
        // A test binary run as a background job inherits INT ignored, which no child `sh` can
        // trap; the Supervisor catches INT (`termination`), so its children start at the default.
        // SAFETY: restores the default disposition; no handler is installed.
        unsafe { nix::sys::signal::signal(Signal::SIGINT, nix::sys::signal::SigHandler::SigDfl) }
            .expect("reset SIGINT");
        let (tx, rx) = unbounded_channel();
        Reaping { controller: ProcessesController::new(tx), _signals: rx }
    }

    fn session(controller: &ProcessesController, name: &str) -> SessionProcess {
        controller.snapshot().sessions.get(name).cloned().unwrap_or_default()
    }

    /// Polls `snapshot` until `predicate` holds. A real process is being waited on, so the exit
    /// lands whenever the kernel says so; a fixed sleep would either flake or be slow.
    async fn until(controller: &ProcessesController, name: &str, predicate: impl Fn(&SessionProcess) -> bool) {
        // `within`'s five seconds plus `STOP_GRACE`, so a SIGKILL escalation fits.
        let wait = async {
            while !predicate(&session(controller, name)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let last = || session(controller, name);
        tokio::time::timeout(STOP_GRACE + Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("timed out on {name:?}: {:?}", last()));
    }

    /// Idles ~30s rather than forever: a test binary killed outright runs no `Drop`, and its orphans
    /// must still end by themselves.
    const IDLE: &str = "for _ in $(seq 600); do sleep 0.05; done";

    fn shell(script: &str) -> (String, Vec<String>) {
        ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
    }

    /// A shell that installs `trap`, then touches `marker`, then idles.
    ///
    /// The marker is the point of this: `start` sets `running` synchronously, before the child has
    /// reached its own first instruction, so a test that signals as soon as `running` is true
    /// races the `exec` and kills a process whose trap does not exist yet. That race produced a
    /// signalled death with no status, which is exactly what these tests assert did not happen.
    fn trapping_shell(trap: &str, marker: &Path) -> (String, Vec<String>) {
        shell(&format!("{trap}; : > {}; {IDLE}", marker.display()))
    }

    async fn until_ready(marker: &Path) {
        crate::capabilities::test_support::poll_until(|| marker.exists().then_some(())).await;
    }

    #[tokio::test]
    async fn a_declared_program_runs_and_its_exit_status_comes_back() {
        let controller = controller();
        controller.declare("t", Signal::SIGTERM);
        assert!(!session(&controller, "t").running, "declaring names a program; it does not run one");

        let (cmd, args) = shell("exit 3");
        controller.start("t", &cmd, &args);
        assert!(session(&controller, "t").pid.is_some(), "a spawned program has its pid immediately");

        until(&controller, "t", |s| !s.running).await;
        assert_eq!(session(&controller, "t").exit_code, Some(3));
    }

    #[tokio::test]
    async fn a_signal_reaches_the_program_itself() {
        let controller = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGTERM);
        let (cmd, args) = trapping_shell("trap 'exit 42' USR2", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        controller.signal("t", Signal::SIGUSR2);
        until(&controller, "t", |s| !s.running).await;
        // 42 is the trap's own status, so this could not have come from the default disposition.
        assert_eq!(session(&controller, "t").exit_code, Some(42));
    }

    #[tokio::test]
    async fn stop_sends_the_signal_the_declaration_named_rather_than_sigterm() {
        let controller = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGINT);
        // Exits 9 only on INT. TERM would end it with no status at all, which is the bug this
        // guards: a recorder reaped with the wrong signal leaves its container unfinished.
        let (cmd, args) = trapping_shell("trap 'exit 9' INT; trap '' TERM", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        controller.stop("t");
        until(&controller, "t", |s| !s.running).await;
        assert_eq!(session(&controller, "t").exit_code, Some(9));
    }

    #[tokio::test]
    async fn a_redeclared_stop_signal_reaches_a_program_that_was_already_running_by_stop_and_by_shutdown() {
        for shutdown in [false, true] {
            let controller = controller();
            let ready = tempfile::tempdir().expect("tempdir");
            let marker = ready.path().join("armed");
            controller.declare("t", Signal::SIGTERM);
            // Two statuses so the signal that arrived is readable: 11 is the one `supervise` used to
            // capture at `start` and keep.
            let (cmd, args) = trapping_shell("trap 'exit 11' TERM; trap 'exit 22' INT", &marker);
            controller.start("t", &cmd, &args);
            until_ready(&marker).await;

            // A reload re-sends every declaration. This one changed and the program did not restart.
            controller.declare("t", Signal::SIGINT);
            if shutdown {
                controller.reap_all().await;
            } else {
                controller.stop("t");
                until(&controller, "t", |s| !s.running).await;
            }
            assert_eq!(
                session(&controller, "t").exit_code,
                Some(22),
                "stop and shutdown must use the declaration as it stands now (shutdown: {shutdown})"
            );
        }
    }

    #[tokio::test]
    async fn starting_a_name_no_declaration_claimed_is_refused_rather_than_creating_it() {
        let controller = controller();
        let (cmd, args) = shell("exit 0");
        controller.start("never-declared", &cmd, &args);
        assert!(
            controller.snapshot().sessions.is_empty(),
            "an undeclared name must stay absent, so a typo reads nil instead of a program that never starts"
        );
    }

    #[tokio::test]
    async fn a_second_start_leaves_the_running_program_alone() {
        let controller = controller();
        controller.declare("t", Signal::SIGTERM);
        let (cmd, args) = shell(IDLE);
        controller.start("t", &cmd, &args);
        until(&controller, "t", |s| s.running).await;
        let first = session(&controller, "t").pid;

        controller.start("t", &cmd, &args);
        assert_eq!(session(&controller, "t").pid, first, "start twice is the same program, not two of it");

        controller.reap_all().await;
    }

    #[tokio::test]
    async fn a_command_that_is_not_there_says_why_instead_of_looking_like_a_slow_start() {
        let controller = controller();
        controller.declare("t", Signal::SIGTERM);
        controller.start("t", "mantle-no-such-binary", &[]);

        let state = session(&controller, "t");
        assert!(!state.running);
        assert!(
            state.start_error.contains("mantle-no-such-binary"),
            "the reason has to name the command; a config watching `running` would otherwise wait forever: {state:?}"
        );
    }

    #[tokio::test]
    async fn a_start_clears_what_the_last_run_left_behind() {
        let controller = controller();
        controller.declare("t", Signal::SIGTERM);
        controller.start("t", "mantle-no-such-binary", &[]);
        assert!(!session(&controller, "t").start_error.is_empty());

        let (cmd, args) = shell("exit 0");
        controller.start("t", &cmd, &args);
        assert!(session(&controller, "t").start_error.is_empty(), "a new run must not wear the last one's failure");
        until(&controller, "t", |s| !s.running).await;
        assert_eq!(session(&controller, "t").exit_code, Some(0));
    }

    #[tokio::test]
    async fn the_shutdown_reap_stops_everything_and_waits_for_it() {
        let controller = controller();
        controller.declare("a", Signal::SIGTERM);
        controller.declare("b", Signal::SIGTERM);
        let (cmd, args) = shell(IDLE);
        controller.start("a", &cmd, &args);
        controller.start("b", &cmd, &args);
        until(&controller, "a", |s| s.running).await;
        until(&controller, "b", |s| s.running).await;

        controller.reap_all().await;
        // Awaited, not fired and forgotten: the state is already settled when this returns.
        assert!(!session(&controller, "a").running);
        assert!(!session(&controller, "b").running);
    }

    #[tokio::test]
    async fn a_test_that_fails_still_kills_the_program_it_started() {
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        let controller = controller();
        controller.declare("t", Signal::SIGTERM);
        let (cmd, args) = trapping_shell("trap '' INT TERM", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;
        let pid = session(&controller, "t").pid.expect("a running program has a pid");

        // What a panic's unwind does; `reap_all` never runs.
        drop(controller);
        assert!(crate::process::exited(&[pid]).await, "pid {pid} outlived its test");
    }

    #[tokio::test]
    async fn a_program_that_ignores_its_stop_signal_is_escalated() {
        let controller = controller();
        let ready = tempfile::tempdir().expect("tempdir");
        let marker = ready.path().join("armed");
        controller.declare("t", Signal::SIGINT);
        let (cmd, args) = trapping_shell("trap '' INT", &marker);
        controller.start("t", &cmd, &args);
        until_ready(&marker).await;

        // `STOP_GRACE` is real time, so this is the one test that waits it out.
        controller.stop("t");
        until(&controller, "t", |s| !s.running).await;
        assert_eq!(
            session(&controller, "t").exit_code,
            None,
            "SIGKILL is not an exit status; a signalled death reports no code"
        );
    }
}

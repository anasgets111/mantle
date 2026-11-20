//! What the Supervisor knows between one loop iteration and the next.
//!
//! `run_supervisor`'s `select!` owns event order; this owns each operation's state. Receivers stay
//! in `main.rs`, one per arm. `select!` drops losing futures before running the winner, allowing
//! `child.wait()` and `&mut supervisor` in separate branches. Most operations address only the
//! authoritative generation via `self.authoritative.generation_id`; `hydrate` is the exception.
//! Tests use a private bus and a stub child to exercise boot hydration without a live shell.

use std::collections::HashMap;
use std::time::Instant;

use shared::{Capability, SupervisorFrame, debug, error, info, notice, warn};

use crate::capabilities::lock::session::{Debt, LockPhase, RelockReason};
use crate::capabilities::lock::{self, LockController};
use crate::capabilities::polkit::{self, Answer, PolkitController};
use crate::capabilities::{Capabilities, Signal};
use crate::generation::{
    Authoritative, RESTART_COOLDOWN, RESTART_LIMIT, RESTART_WINDOW, Renderer, RendererDeparture, RestartBrake,
    classify_departure, departure_report, unrespawnable,
};
use crate::memory;
use crate::pam_worker;
use crate::polkit::AgentRequest;
use crate::process::registry::{LiveProcesses, reap_processes, take_generation};
use crate::snapshot::{Published, push_snapshot};
use crate::socket;
use crate::{process, send_frame_logged};

/// Supervisor loop state.
pub(crate) struct Supervisor {
    /// Live Renderer connections by generation.
    pub(crate) registry: socket::GenerationRegistry,
    /// Generation whose frames count and receive outbound frames.
    pub(crate) authoritative: Authoritative,
    /// Capability controllers and senders (ADR-0076).
    pub(crate) capabilities: Capabilities,
    /// Lock capability, built here rather than in `Capabilities` (ADR-0052).
    pub(crate) lock: LockController,
    /// The lock's PAM conversation frames, tagged by conversation serial (ADR-0381).
    pub(crate) pam_events_tx: pam_worker::Events,
    /// Onscreen polkit challenge, built here like `lock` (ADR-0114).
    polkit: PolkitController,
    /// Polkit counterpart to `pam_events_tx`.
    polkit_events_tx: pam_worker::Events,

    /// Persistent `$XDG_RUNTIME_DIR/mantle/session-locked-$WAYLAND_DISPLAY` marker (ADR-0060).
    pub(crate) locked_flag: lock::SessionLockedFlag,
    /// logind half of the same fact (ADR-0138), publishing `loginctl show-session`'s `LockedHint`
    /// whenever the marker changes.
    pub(crate) session_bridge: lock::logind::SessionBridge,
    /// Last snapshot per capability, replayed to each new generation by [`Supervisor::hydrate`]. Its
    /// revision counts published snapshots for the memory report (ADR-0305).
    pub(crate) last_snapshots: HashMap<Capability, Published>,
    /// The portal's icon theme, replayed like `last_snapshots`.
    icon_theme: Option<String>,
    /// Id for the next crash replacement.
    next_generation_id: u32,
    pub(crate) renderer: Renderer,
    pub(crate) restart_brake: RestartBrake,
    pub(crate) respawn_at: Option<std::time::Instant>,
    /// When the authoritative child was spawned, until it reports `CanLock`; for the startup bound.
    pub(crate) spawned_at: Option<Instant>,
    /// The generation killed for the lock, until its exit replaces it.
    pub(crate) killed: Option<u32>,
    /// Set only by [`Supervisor::replace_departed_renderer`]; distinguishes shutdown reaping from
    /// an already-gone Renderer.
    pub(crate) renderer_departed: bool,
    /// The process exit status [`Self::reap`] returns.
    exit_code: i32,
    pub(crate) lock_phase: LockPhase,
    /// A lock asked for (Lua, logind, sleep) that no acquisition sent since has answered and no
    /// correct password since has superseded.
    pub(crate) lock_wanted: bool,
    /// The last `SetSessionLock` id; never reused while this process runs.
    pub(crate) lock_id: u64,
    /// The generations that replayed their snapshots and that reported `CanLock`. A lock goes out
    /// once both are the authoritative one: after the replay (ADR-0058 decision 4), to a Wayland
    /// thread that has resolved its config.
    pub(crate) hydrated: Option<u32>,
    pub(crate) resolved: Option<u32>,
    /// Tracked `process.run` children (ADR-0026).
    pub(crate) processes: LiveProcesses,
    process_done_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
    /// `process.kill` and departed-generation reaps, off the loop; [`Supervisor::reap`] drains them.
    reaps: tokio::task::JoinSet<()>,
    /// Fires when the departed generation's children are gone; see [`Supervisor::dispatch_process_command`].
    departed_reaped: Option<tokio::sync::oneshot::Receiver<()>>,
    /// The sleep delay's wait (ADR-0385), answered by the acquisition's `Locked` or `Refused`, or by
    /// a first `CanLock(false)` with no lock owed. Kept past the delay's timeout: it is also the
    /// sleep's lock intent.
    pub(crate) sleep_waits: Option<tokio::sync::oneshot::Sender<()>>,
    /// Test hook replacing [`pam_worker::start_lock_conversation`] so tests never re-exec `/proc/self/exe`.
    #[cfg(test)]
    pub(crate) pam_starter: Option<std::sync::Arc<dyn Fn(&'static str, u64) -> pam_worker::Conversation + Send + Sync>>,
    /// Test hook overriding [`pam_worker::fingerprint_installed`] so tests never probe `/etc/pam.d`.
    #[cfg(test)]
    pub(crate) fingerprint_installed: Option<bool>,
}

impl Supervisor {
    /// Generation-0 child, spawned before construction so failure is fatal (ADR-0025).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        registry: socket::GenerationRegistry,
        boot_child: tokio::process::Child,
        renderer: Renderer,
        capabilities: Capabilities,
        lock: LockController,
        locked_flag: lock::SessionLockedFlag,
        session_bridge: lock::logind::SessionBridge,
        pam_events_tx: pam_worker::Events,
        polkit_events_tx: pam_worker::Events,
        process_done_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
    ) -> Self {
        // Read before boot registration; reading later would race it.
        let lock_phase = match locked_flag.is_set() {
            true => LockPhase::Recovering { debt: Debt::new(RelockReason::SupervisorRestarted), retry_at: None },
            false => LockPhase::Idle,
        };
        if lock_phase != LockPhase::Idle {
            info!(
                "the session was locked when the last Supervisor stopped and the compositor has not unlocked it, so the boot Renderer will be asked \
                 to take that lock over (ADR-0060)"
            );
        }
        let mut supervisor = Self {
            registry,
            authoritative: Authoritative { generation_id: 0, child: boot_child, can_lock: None },
            capabilities,
            lock,
            pam_events_tx,
            polkit: PolkitController::default(),
            polkit_events_tx,
            locked_flag,
            session_bridge,
            last_snapshots: HashMap::new(),
            icon_theme: None,
            next_generation_id: 1,
            renderer,
            restart_brake: RestartBrake::default(),
            respawn_at: None,
            spawned_at: Some(Instant::now()),
            killed: None,
            renderer_departed: false,
            exit_code: 0,
            lock_phase,
            lock_wanted: false,
            lock_id: 0,
            hydrated: None,
            resolved: None,
            processes: HashMap::new(),
            process_done_tx,
            reaps: tokio::task::JoinSet::new(),
            departed_reaped: None,
            sleep_waits: None,
            #[cfg(test)]
            pam_starter: None,
            #[cfg(test)]
            fingerprint_installed: None,
        };
        // Boot-built controllers have no lazy reader to publish their initial state. An owed relock
        // leaves `lock` nil until its `LockReport`: a default `active = false` here would tell the
        // shell the session is unlocked while the compositor still holds it (ADR-0060).
        if supervisor.lock_phase == LockPhase::Idle {
            supervisor.push_lock_state();
        }
        supervisor.push_polkit_state();
        supervisor
    }

    fn push_polkit_state(&mut self) {
        push_snapshot(
            &self.registry,
            self.authoritative.generation_id,
            &mut self.last_snapshots,
            Capability::Polkit,
            &self.polkit.snapshot(),
        );
    }

    /// Handles a polkitd challenge or withdrawal (ADR-0114).
    pub(crate) fn handle_polkit_request(&mut self, request: AgentRequest) {
        let changed = match request {
            AgentRequest::Begin { call, reply } => self.polkit.begin(call, reply, nix::unistd::Uid::current().as_raw()),
            AgentRequest::Cancel { cookie } => self.polkit.cancel(cookie.as_deref()),
        };
        if changed {
            self.push_polkit_state();
        }
    }

    /// Routes one `secure_submit` by capability and action. Every arm that does not hand `secret`
    /// on drops it, which scrubs it; arms are ordered, the last is the catch-all.
    pub(crate) async fn route_secure_submit(&mut self, mut submit: shared::SecureSubmit) {
        // `mem::take` moves the plaintext out of the `ZeroizeOnDrop` frame into a scrubbing owner.
        let secret = shared::Zeroizing::new(std::mem::take(&mut submit.secret));
        let generation_id = submit.generation_id;
        match (submit.capability, submit.action.as_str(), submit.name.take()) {
            // ADR-0028, ADR-0114.
            (Capability::Polkit, "authenticate", None) => self.begin_polkit_authentication(secret, generation_id),
            // ADR-0029: empty secret means open network; non-empty is the WPA-PSK password.
            (Capability::Network, "connect", None) => self.capabilities.connect_prompted(generation_id, secret),
            (Capability::Network, "vpn_secret", name) => {
                if let Some(target) = name {
                    self.capabilities.submit_vpn_secret(target, secret);
                }
            }
            (Capability::Bluetooth, "pair", name) => {
                if let Some(target) = name {
                    self.capabilities.submit_pairing(target, secret);
                }
            }
            (Capability::Lock, "authenticate", None) => self.begin_lock_authentication(secret, generation_id),
            (Capability::Secrets, "store", name) => {
                if let Some(name) = name.filter(|name| shared::valid_secret_name(name)) {
                    self.capabilities.start(Capability::Secrets).await;
                    self.capabilities.store_secret(name, secret);
                }
            }
            // Other capability/actions (ADR-0015's textfield/IPC half) are not implemented. The
            // length stays out of the log: it is the one thing about a misaddressed password worth
            // an attacker's time.
            (capability, action, _) => {
                debug!(
                    "generation {generation_id}'s secure_submit received: capability={capability:?} action={action:?}"
                );
            }
        }
    }

    /// Answers the polkit helper's prompt, starting the helper on the first submit; `secret` is
    /// scrubbed on refusal here and by the conversation otherwise.
    fn begin_polkit_authentication(&mut self, secret: shared::Zeroizing<Vec<u8>>, generation_id: u32) {
        let events = self.polkit_events_tx.clone();
        let start = move |serial, uid, cookie| pam_worker::start_polkit_conversation(uid, cookie, serial, events);
        if self.polkit.submit(secret, start) {
            self.push_polkit_state();
        } else {
            warn!(
                "generation {generation_id}'s secure_submit(polkit, authenticate) arrived with no challenge on screen, or with an answer already with PAM; dropping"
            );
        }
    }

    /// One frame of the polkit helper's conversation. On success polkitd was already told;
    /// release the held `BeginAuthentication` reply in the required order.
    pub(crate) fn record_polkit_message(&mut self, serial: u64, message: shared::PamMessage) {
        match self.polkit.hear(serial, message) {
            Answer::Stale => debug!("dropping polkit conversation {serial}'s frame; it is no longer the one on screen"),
            Answer::Changed | Answer::Failed => self.push_polkit_state(),
            Answer::Succeeded { reply } => {
                self.push_polkit_state();
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Pushes one roster capability signal as a snapshot (ADR-0076).
    pub(crate) fn push_capability_signal(&mut self, signal: Signal) {
        self.capabilities.push(signal, &self.registry, self.authoritative.generation_id, &mut self.last_snapshots);
    }

    pub(crate) fn set_icon_theme(&mut self, theme: Option<String>) {
        send_frame_logged(&self.registry, self.authoritative.generation_id, &SupervisorFrame::IconTheme(theme.clone()));
        self.icon_theme = theme;
    }

    /// Asks the authoritative generation to re-evaluate its config (ADR-0216).
    pub(crate) fn begin_reload(&self) {
        send_frame_logged(&self.registry, self.authoritative.generation_id, &SupervisorFrame::Reevaluate);
    }

    /// Every `last_snapshots` entry for `--profile`: serialized bytes, pushes sent and pushes
    /// deduped, largest first. All of them, because which capability's published state grows
    /// with traffic is the question, not the premise. Serializing to measure is the work
    /// `push_snapshot` already does per change, and this runs once per report.
    pub(crate) fn snapshot_sizes(&self) -> Vec<memory::SnapshotStat> {
        let mut sizes: Vec<memory::SnapshotStat> = self
            .last_snapshots
            .iter()
            .map(|(capability, last)| (capability.as_str(), last.payload.len(), last.revision, last.deduped))
            .collect();
        sizes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        sizes
    }

    /// Replays snapshots to a newly registered authoritative generation, then gives it a lock owed
    /// or wanted once it has also resolved its config.
    pub(crate) fn hydrate(&mut self, generation_id: u32) {
        if generation_id != self.authoritative.generation_id {
            return;
        }
        debug!("hydrating generation {generation_id} with {} snapshots", self.last_snapshots.len());
        for (capability, published) in &self.last_snapshots {
            crate::snapshot::send(&self.registry, generation_id, *capability, published);
        }
        if self.icon_theme.is_some() {
            send_frame_logged(&self.registry, generation_id, &SupervisorFrame::IconTheme(self.icon_theme.clone()));
        }
        self.hydrated = Some(generation_id);
        self.sync_lock(Instant::now());
    }

    /// Reports authoritative Renderer death and spawns a replacement (ADR-0058); `true` stops the loop.
    pub(crate) async fn replace_departed_renderer(
        &mut self,
        status: std::io::Result<std::process::ExitStatus>,
    ) -> bool {
        let departure = match status {
            Ok(status) => classify_departure(status),
            Err(err) => {
                warn!("failed to wait on generation {}'s renderer: {err}", self.authoritative.generation_id);
                RendererDeparture::Failed { code: -1 }
            }
        };
        let generation_id = self.authoritative.generation_id;
        // A refused Renderer leaves on the hangup's exit 70 or the kill after it, so the record
        // decides, not the code. Before the brake: every replacement would find the same
        // compositor, gone or unable to host the shell, or would be the same refused binary.
        let refused = self.registry.take_protocol_refusal(generation_id);
        let stop = refused.then_some("it speaks another protocol").or_else(|| unrespawnable(departure));
        let was_locked = self.lock.snapshot().active;
        warn!("{}", departure_report(departure, generation_id, was_locked, stop.is_none()));
        self.renderer_departed = true;
        self.lock_departed();
        // At departure rather than respawn: from here the pid is reaped and must not be signalled.
        self.registry.forget_generation(generation_id);

        // Stop the way a SIGTERM does.
        if let Some(reason) = stop {
            if refused {
                self.exit_code = shared::EXIT_PROTOCOL_MISMATCH;
            }
            error!("{reason}, so a replacement renderer cannot help; shutting down");
            return true;
        }
        self.respawn_renderer().await
    }

    /// Kills `generation_id`'s Renderer group, hung up on and still running (`kill_after_hangup`),
    /// if it is the authoritative child and not yet reaped. Only this loop reaps it, so an unreaped
    /// pid is still this Renderer's and its group cannot have been reissued (#105).
    pub(crate) fn kill_hung_up(&self, generation_id: u32) {
        self.kill_renderer(generation_id, "is still running after the hangup");
    }

    pub(crate) fn kill_renderer(&self, generation_id: u32, why: &str) {
        if let Some(pgid) = self.hung_up_group(generation_id) {
            warn!("generation {generation_id} (pid {pgid}) {why}; killing it");
            let _ = process::signal_group_best_effort(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }

    fn hung_up_group(&self, generation_id: u32) -> Option<nix::unistd::Pid> {
        let child = &self.authoritative.child;
        let pid = child.id().filter(|_| generation_id == self.authoritative.generation_id)?;
        Some(nix::unistd::Pid::from_raw(pid as i32))
    }

    /// Spawns the replacement unless the brake defers it to `respawn_at` (ADR-0058 decision 3); `true` stops the loop.
    pub(crate) async fn respawn_renderer(&mut self) -> bool {
        if !self.restart_brake.allow(std::time::Instant::now()) {
            warn!(
                "{RESTART_LIMIT} renderers died within {RESTART_WINDOW:?}; respawning in {RESTART_COOLDOWN:?} (ADR-0058 decision 3)"
            );
            self.respawn_at = Some(std::time::Instant::now() + RESTART_COOLDOWN);
            return false;
        }
        self.respawn_at = None;
        let replacement_generation_id = self.next_generation_id;
        self.next_generation_id += 1;
        match self.renderer.spawn(replacement_generation_id) {
            Ok(child) => {
                if let Some(pid) = child.id() {
                    self.registry.expect_generation(replacement_generation_id, nix::unistd::Pid::from_raw(pid as i32));
                }
                // The departed generation's id must not stay claimable by whatever inherits its pid.
                let departed = self.authoritative.generation_id;
                // Unknown until it says: its config may have changed since the last one read it.
                self.authoritative = Authoritative { generation_id: replacement_generation_id, child, can_lock: None };
                self.renderer_departed = false;
                self.spawned_at = Some(Instant::now());
                notice!("spawned generation {replacement_generation_id} to replace it");
                self.capabilities.forget_departed_requests(departed);
                // Otherwise the dead id keeps its idle fan-out entry (a failed push per idle
                // transition, and any inhibit it held) and its `process.run` children.
                if let Some(idle) = self.capabilities.idle() {
                    idle.reset_registrations(departed).await;
                }
                let mut gone = take_generation(&mut self.processes, departed);
                let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
                self.departed_reaped = Some(reaped_rx);
                while self.reaps.try_join_next().is_some() {}
                self.reaps.spawn(async move {
                    reap_processes(&mut gone).await;
                    let _ = reaped_tx.send(());
                });
                // Registration replays every `last_snapshots` entry via `hydrate`.
                false
            }
            Err(err) => {
                // A package upgrade or `just swap` can leave the binary briefly unexecutable.
                // ponytail: retries forever, so a never-executable binary leaves a headless session; cap the count.
                error!("could not spawn a replacement renderer: {err}; retrying in {RESTART_COOLDOWN:?}");
                self.respawn_at = Some(std::time::Instant::now() + RESTART_COOLDOWN);
                false
            }
        }
    }

    /// Routes a roster command to its controller (ADR-0037); `lock` and `polkit` are answered here
    /// (ADR-0052, ADR-0114).
    pub(crate) fn dispatch_capability_command(&mut self, capability: Capability, envelope: &shared::CommandEnvelope) {
        // Keep polkit here beside the cancel path (ADR-0114).
        if capability == Capability::Polkit {
            if polkit::dispatch(&mut self.polkit, envelope) {
                self.push_polkit_state();
            }
            return;
        }
        if capability == Capability::Lock {
            if lock::dispatch(&self.lock, envelope) {
                self.request_lock();
            }
            return;
        }
        self.capabilities.dispatch(capability, envelope);
    }

    /// Routes a `process` command (ADR-0026).
    pub(crate) async fn dispatch_process_command(&mut self, envelope: &shared::CommandEnvelope) {
        // The old generation's children must be gone first, or the new one can start a single-instance
        // daemon that the old copy still holds. ponytail: holds the loop up to ~2 s, only while that reap runs.
        if let Some(reaped) = self.departed_reaped.take() {
            let _ = reaped.await;
        }
        process::registry::dispatch(
            &mut self.processes,
            &self.registry,
            &self.process_done_tx,
            &mut self.reaps,
            envelope,
        );
    }

    /// Reaps authoritative Renderer and live `process.run` children with SIGTERM/SIGKILL and
    /// `DEFAULT_REAP_GRACE`, and returns the process exit status. Skip a Renderer already collected
    /// by [`Supervisor::replace_departed_renderer`], avoiding a misleading "already reaped" log.
    pub(crate) async fn reap(&mut self) -> i32 {
        if !self.renderer_departed
            && let Err(err) =
                process::reap_process_group(&mut self.authoritative.child, process::DEFAULT_REAP_GRACE).await
        {
            warn!(
                "failed to reap authoritative generation {}'s renderer on shutdown: {err}",
                self.authoritative.generation_id
            );
        }
        reap_processes(&mut self.processes).await;
        // Kills and reaps still inside their SIGTERM grace would otherwise die with the runtime, unkilled.
        while self.reaps.join_next().await.is_some() {}
        // Session processes are not in `self.processes`: they outlive generations by design, so
        // the per-generation sweep never sees them and this is their only reap.
        self.capabilities.reap_sessions().await;
        self.exit_code
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::capabilities::lock::session::Release;

    use std::sync::{Arc, Mutex};

    use crate::capabilities::test_support::{PrivateBus, fake_program, private_bus, within};
    use tokio::sync::mpsc;

    /// Kills any tracked process groups on drop so a panicking test cannot orphan children. A
    /// tracked child must stay unreaped until then, or be released before the test reaps it: a
    /// reaped pid may be another group's by the drop. The Supervisor drops a replaced child
    /// unreaped, and the fake Renderer outlives the test.
    #[derive(Default)]
    pub(crate) struct ChildGuard(Vec<nix::unistd::Pid>);

    impl ChildGuard {
        pub(crate) fn track(&mut self, child: &tokio::process::Child) {
            if let Some(pid) = child.id() {
                self.0.push(nix::unistd::Pid::from_raw(pid as i32));
            }
        }

        pub(crate) fn release(&mut self, child: &tokio::process::Child) {
            let tracked = self.0.len();
            self.0.retain(|pid| Some(pid.as_raw() as u32) != child.id());
            assert!(self.0.len() < tracked, "release a tracked child before reaping it");
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            for pid in self.0.drain(..) {
                let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
            }
        }
    }

    pub(crate) fn make_fake_renderer(dir: &std::path::Path) -> std::path::PathBuf {
        fake_program(dir, "fake-renderer", "exec sleep 30").into()
    }

    /// The PAM conversations a Supervisor started, by mode and serial.
    pub(crate) type Conversations = Arc<Mutex<Vec<(&'static str, u64)>>>;

    /// A Supervisor booted in `dir` over an exited `/bin/true`, recording the PAM conversations it
    /// starts. The box keeps its other channels open for the test's length.
    pub(crate) async fn booted(
        bus: &PrivateBus,
        dir: &std::path::Path,
    ) -> (Supervisor, Conversations, Box<dyn std::any::Any>) {
        let connection = bus.connection().await;
        let (sound_tx, sounds) = std::sync::mpsc::sync_channel(1);
        let (idle_tx, idle) = mpsc::unbounded_channel();
        let (capabilities, signals) = Capabilities::new(connection.clone(), sound_tx, idle_tx);
        let (logind_tx, logind) = mpsc::unbounded_channel();
        let bridge = lock::logind::SessionBridge::new(connection, logind_tx).await;
        let (pam_tx, pam) = mpsc::unbounded_channel();
        let (polkit_tx, polkit) = mpsc::unbounded_channel();
        let (done_tx, done) = mpsc::unbounded_channel();
        let renderer = Renderer::new("/bin/true".into(), dir, dir, None, 0, None);
        let child = renderer.spawn(0).unwrap();
        let mut supervisor = Supervisor::new(
            socket::GenerationRegistry::default(),
            child,
            renderer,
            capabilities,
            LockController::new(),
            lock::SessionLockedFlag::at(dir.join("session-locked")),
            bridge,
            pam_tx,
            polkit_tx,
            done_tx,
        );
        let started = Conversations::default();
        let seen = Arc::clone(&started);
        supervisor.pam_starter = Some(Arc::new(move |mode, serial| {
            seen.lock().unwrap().push((mode, serial));
            pam_worker::Conversation::detached().0
        }));
        supervisor.fingerprint_installed = Some(false);
        supervisor.authoritative.child.wait().await.unwrap();
        (supervisor, started, Box::new((sounds, idle, signals, logind, pam, polkit, done)))
    }

    /// The authoritative generation registering, replaying and resolving its config; returns what
    /// it is sent from then on.
    pub(crate) fn up(supervisor: &mut Supervisor) -> mpsc::Receiver<Vec<u8>> {
        let (tx, frames) = mpsc::channel(64);
        let generation_id = supervisor.authoritative.generation_id;
        supervisor.registry.register(generation_id, tx, Default::default());
        supervisor.hydrate(generation_id);
        supervisor.record_can_lock(true);
        frames
    }

    /// Each `SetSessionLock` sent so far, in order, as `(locked, id)`.
    pub(crate) fn sent(frames: &mut mpsc::Receiver<Vec<u8>>) -> Vec<(bool, u64)> {
        std::iter::from_fn(|| frames.try_recv().ok())
            .filter_map(|bytes| match serde_json::from_slice(&bytes).unwrap() {
                SupervisorFrame::SetSessionLock(command) => Some((command.locked, command.id)),
                _ => None,
            })
            .collect()
    }

    /// The authoritative generation's report about lock `id`.
    pub(crate) fn report(supervisor: &mut Supervisor, id: u64, outcome: shared::LockOutcome) {
        supervisor.record_lock_report(supervisor.authoritative.generation_id, shared::LockReport { id, outcome });
    }

    /// Answers the lock's running password conversation with success, as the worker does after a
    /// right password, and checks the PAM path took it.
    pub(crate) fn pam_succeeds(supervisor: &mut Supervisor, started: &Conversations) {
        let serial = started.lock().unwrap().iter().rev().find(|(mode, _)| *mode == pam_worker::PASSWORD).unwrap().1;
        supervisor.record_pam_message(serial, shared::PamMessage::Outcome(shared::PamOutcome::Success));
        assert!(supervisor.lock.snapshot().unlocking, "the PAM answer must have been heard");
    }

    /// Lock `id` asked for and granted on the authoritative generation.
    pub(crate) fn held(supervisor: &mut Supervisor, frames: &mut mpsc::Receiver<Vec<u8>>) -> u64 {
        supervisor.request_lock();
        let [(true, id)] = sent(frames)[..] else { panic!("one lock is sent") };
        report(supervisor, id, shared::LockOutcome::Locked);
        assert!(matches!(supervisor.lock_phase, LockPhase::Held { release: Release::None, .. }));
        id
    }

    /// The authoritative Renderer dying of a crash, and its replacement spawned.
    pub(crate) async fn crash(supervisor: &mut Supervisor) {
        use std::os::unix::process::ExitStatusExt;
        supervisor.restart_brake = RestartBrake::default();
        assert!(!supervisor.replace_departed_renderer(Ok(std::process::ExitStatus::from_raw(9))).await);
    }

    pub(crate) fn marked(dir: &std::path::Path) -> bool {
        dir.join("session-locked").exists()
    }

    #[tokio::test]
    async fn boot_built_capabilities_hydrate_their_first_state_and_a_locked_boot_retakes_the_lock_after_the_replay() {
        let bus = private_bus().await;
        for locked in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if locked {
                std::fs::File::create(dir.path().join("session-locked")).unwrap();
            }
            let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
            let (tx, mut frames) = mpsc::channel(16);
            supervisor.registry.register(0, tx, Default::default());
            supervisor.hydrate(0);
            supervisor.record_can_lock(true);
            let mut snapshots = HashMap::new();
            let mut lock_after = None;
            while let Ok(bytes) = frames.try_recv() {
                match serde_json::from_slice(&bytes).unwrap() {
                    SupervisorFrame::StateSnapshot(snapshot) => {
                        assert_eq!(snapshot.revision, 1, "replay must not advance the revision");
                        snapshots.insert(snapshot.capability, snapshot.payload);
                    }
                    SupervisorFrame::SetSessionLock(command) => lock_after = Some((command, snapshots.len())),
                    other => panic!("boot hydration sent {other:?}"),
                }
            }
            assert_eq!(snapshots["polkit"]["active"], false);
            if locked {
                // A default `active = false` would tell the shell the session is unlocked while the
                // compositor still holds it (ADR-0060).
                assert!(!snapshots.contains_key("lock"), "an owed relock must not read as unlocked");
                let replayed = snapshots.len();
                assert_eq!(
                    lock_after,
                    Some((shared::SetSessionLock { locked: true, id: 1 }, replayed)),
                    "after the replay"
                );
            } else {
                assert_eq!(snapshots["lock"]["active"], false);
                assert_eq!(lock_after, None, "an unlocked boot must not request a lock");
            }
        }
    }

    #[tokio::test]
    async fn hydrate_ignores_stale_generations_and_replays_icon_theme() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        supervisor.set_icon_theme(Some("Papirus".into()));

        let (stale_tx, mut stale_frames) = mpsc::channel(8);
        supervisor.registry.register(99, stale_tx, Default::default());
        supervisor.hydrate(99);
        assert!(stale_frames.try_recv().is_err(), "stale registration must not be hydrated");

        let (tx, mut frames) = mpsc::channel(8);
        supervisor.registry.register(0, tx, Default::default());
        let polkit = &supervisor.last_snapshots[&Capability::Polkit];
        let stat = ("polkit", polkit.payload.len(), polkit.revision, polkit.deduped);
        assert!(supervisor.snapshot_sizes().contains(&stat), "--profile reports the stored bytes");
        let expected_polkit: serde_json::Value = serde_json::from_slice(&polkit.payload).unwrap();
        for _reconnect in 0..2 {
            supervisor.hydrate(0);
            let mut saw_theme = None;
            let mut saw_polkit = None;
            while let Ok(bytes) = frames.try_recv() {
                match serde_json::from_slice(&bytes).unwrap() {
                    SupervisorFrame::IconTheme(theme) => saw_theme = Some(theme),
                    SupervisorFrame::StateSnapshot(snapshot) if snapshot.capability == Capability::Polkit.as_str() => {
                        saw_polkit = Some(snapshot.payload);
                    }
                    _ => {}
                }
            }
            assert_eq!(saw_theme, Some(Some("Papirus".into())));
            assert_eq!(saw_polkit.as_ref(), Some(&expected_polkit));
        }

        supervisor.set_icon_theme(None);
        while frames.try_recv().is_ok() {}
        supervisor.hydrate(0);
        while let Ok(bytes) = frames.try_recv() {
            assert!(!matches!(serde_json::from_slice(&bytes).unwrap(), SupervisorFrame::IconTheme(_)));
        }
    }

    #[tokio::test]
    async fn respawn_renderer_tracks_generations_and_reaps_departed_children() {
        use std::os::unix::process::ExitStatusExt;

        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let fake = make_fake_renderer(dir.path());
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let mut guard = ChildGuard::default();
        supervisor.renderer = Renderer::new(fake.clone(), dir.path(), dir.path(), None, 0, None);

        // Seed one child on departed gen 0 and another on gen 99; only gen 0 should reap on swap.
        process::registry::spawn_and_register_process(&mut supervisor.processes, 0, 1, "/bin/sleep", &["5".into()])
            .unwrap();
        process::registry::spawn_and_register_process(&mut supervisor.processes, 99, 2, "/bin/sleep", &["5".into()])
            .unwrap();

        supervisor.renderer_departed = true;
        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        let p1 = nix::unistd::Pid::from_raw(supervisor.authoritative.child.id().unwrap() as i32);
        assert_eq!(supervisor.authoritative.generation_id, 1);
        assert!(!supervisor.renderer_departed);
        assert_eq!(supervisor.registry.may_claim(1, p1), Some(true));
        assert_eq!(supervisor.registry.may_claim(1, nix::unistd::Pid::from_raw(p1.as_raw() + 1)), Some(false));
        within(supervisor.departed_reaped.take().unwrap()).await.unwrap();
        assert!(!supervisor.processes.contains_key(&(0, 1)));
        assert!(supervisor.processes.contains_key(&(99, 2)));

        // A departure forgets gen 1's pid and spawns gen 2.
        assert!(!supervisor.replace_departed_renderer(Ok(std::process::ExitStatus::from_raw(9))).await);
        guard.track(&supervisor.authoritative.child);
        assert_eq!(supervisor.registry.may_claim(1, p1), None, "departed pid expectation must be cleared");
        assert_eq!(supervisor.authoritative.generation_id, 2);

        // A compositor that cannot host the shell stops the Supervisor instead of spawning gen 3.
        let unsupported = std::process::ExitStatus::from_raw(shared::EXIT_COMPOSITOR_UNSUPPORTED << 8);
        assert!(supervisor.replace_departed_renderer(Ok(unsupported)).await);
        assert_eq!(supervisor.authoritative.generation_id, 2);
        assert_eq!(supervisor.exit_code, 0);

        // #63: a Renderer refused for its protocol leaves on the hangup's exit 70, which alone
        // respawns; the refusal record stops the Supervisor with `EXIT_PROTOCOL_MISMATCH` instead.
        supervisor.registry.refuse_protocol(2);
        assert!(supervisor.replace_departed_renderer(Ok(std::process::ExitStatus::from_raw(70 << 8))).await);
        assert_eq!(supervisor.authoritative.generation_id, 2);
        assert!(!supervisor.registry.take_protocol_refusal(2), "the record is taken once");

        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        assert_eq!(supervisor.authoritative.generation_id, 3);

        // renderer_departed = false ensures shutdown reap kills the live replacement child too, and
        // the stop's exit status is what `run_supervisor` hands `process::exit`.
        guard.release(&supervisor.authoritative.child);
        assert_eq!(supervisor.reap().await, shared::EXIT_PROTOCOL_MISMATCH);
        assert!(supervisor.authoritative.child.try_wait().unwrap().is_some());
        assert!(supervisor.processes.is_empty());
    }

    #[tokio::test]
    async fn a_hung_up_renderer_is_killed_only_while_it_is_the_unreaped_authoritative_child() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let mut guard = ChildGuard::default();
        assert_eq!(supervisor.hung_up_group(0), None, "a reaped generation is never signalled");

        supervisor.renderer = Renderer::new(make_fake_renderer(dir.path()), dir.path(), dir.path(), None, 0, None);
        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        assert_eq!(supervisor.hung_up_group(0), None, "a departed generation's request is stale");

        guard.release(&supervisor.authoritative.child);
        supervisor.kill_hung_up(1);
        let status = within(supervisor.authoritative.child.wait()).await.unwrap();
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(9), "the wedged group is killed");
    }

    #[tokio::test]
    async fn respawn_renderer_schedules_cooldown_on_spawn_failure_and_restart_brake() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let fake = make_fake_renderer(dir.path());
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let mut guard = ChildGuard::default();

        supervisor.renderer = Renderer::new(dir.path().join("missing-renderer"), dir.path(), dir.path(), None, 0, None);
        let before_fail = std::time::Instant::now();
        assert!(!supervisor.respawn_renderer().await);
        assert_eq!(supervisor.authoritative.generation_id, 0);
        assert!(supervisor.respawn_at.is_some_and(|at| at > before_fail));

        // Recovery clears respawn_at (gen 1 was consumed by the failed attempt); the 4th trips the brake.
        supervisor.renderer = Renderer::new(fake, dir.path(), dir.path(), None, 0, None);
        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        assert_eq!(supervisor.authoritative.generation_id, 2);
        assert!(supervisor.respawn_at.is_none());

        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        assert_eq!(supervisor.authoritative.generation_id, 3);

        let before_brake = std::time::Instant::now();
        assert!(!supervisor.respawn_renderer().await);
        assert_eq!(supervisor.authoritative.generation_id, 3, "brake must defer before spawning");
        assert!(supervisor.respawn_at.is_some_and(|at| at > before_brake));

        guard.release(&supervisor.authoritative.child);
        supervisor.reap().await;
    }
}

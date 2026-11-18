//! What the Supervisor knows between one loop iteration and the next.
//!
//! `run_supervisor`'s `select!` owns event order; this owns each operation's state. Receivers stay
//! in `main.rs`, one per arm. `select!` drops losing futures before running the winner, allowing
//! `child.wait()` and `&mut supervisor` in separate branches. Most operations address only the
//! authoritative generation via `self.authoritative.generation_id`; `hydrate` is the exception.
//! Tests use a private bus and a stub child to exercise boot hydration without a live shell.

use std::collections::HashMap;

use shared::{Capability, SupervisorFrame, debug, error, info, notice, warn};

use crate::capabilities::conversation::Heard;
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
use crate::snapshot::{Published, push_snapshot, send_owned};
use crate::socket;
use crate::{process, send_frame_logged};

/// One state, so a respawn's `Owed` replaces an `InFlight` whose Renderer died before reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relock {
    Idle,
    /// Asked of the next generation to connect.
    Owed(RelockReason),
    /// Asked of the connected generation; its `LockReport` is the answer.
    InFlight(RelockReason),
}

impl Relock {
    fn start(&mut self) -> Option<RelockReason> {
        let Self::Owed(reason) = *self else { return None };
        *self = Self::InFlight(reason);
        Some(reason)
    }

    /// The dead generation's in-flight ask gets no report; an `Owed` one still stands.
    fn abandon(&mut self) {
        if matches!(self, Self::InFlight(_)) {
            *self = Self::Idle;
        }
    }

    fn finish(&mut self) -> Option<RelockReason> {
        let Self::InFlight(reason) = *self else { return None };
        *self = Self::Idle;
        Some(reason)
    }
}

/// Why a generation must take an unrequested lock. Causes differ in logs but both mean the
/// compositor holds a lock with nothing of ours on it; a named enum beats an ambiguous `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelockReason {
    /// ADR-0058 decision 4: lock-holder Renderer died; replacement spawned.
    RendererReplaced,
    /// ADR-0060: runtime marker was set, so a previous Supervisor died while locked.
    SupervisorRestarted,
}

impl RelockReason {
    /// Subject noun for outcome messages.
    fn subject(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the replacement",
            RelockReason::SupervisorRestarted => "the restarted shell",
        }
    }

    /// Why the lock is being retaken, for the in-flight log.
    fn because(self) -> &'static str {
        match self {
            RelockReason::RendererReplaced => "the Renderer that held it died",
            RelockReason::SupervisorRestarted => "the Supervisor that held it died",
        }
    }
}

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
    locked_flag: lock::SessionLockedFlag,
    /// logind half of the same fact (ADR-0138), publishing `loginctl show-session`'s `LockedHint`
    /// whenever the marker changes.
    session_bridge: lock::logind::SessionBridge,
    /// Last snapshot per capability, replayed to each new generation by [`Supervisor::hydrate`]. Its
    /// revision counts published snapshots for the memory report (ADR-0305).
    last_snapshots: HashMap<Capability, Published>,
    /// The portal's icon theme, replayed like `last_snapshots`.
    icon_theme: Option<String>,
    /// Id for the next crash replacement.
    next_generation_id: u32,
    renderer: Renderer,
    restart_brake: RestartBrake,
    pub(crate) respawn_at: Option<std::time::Instant>,
    /// Set only by [`Supervisor::replace_departed_renderer`]; distinguishes shutdown reaping from
    /// an already-gone Renderer.
    renderer_departed: bool,
    /// The process exit status [`Self::reap`] returns.
    exit_code: i32,
    /// ADR-0058 decision 4, ADR-0060: intent after lock-holder death or locked startup.
    relock: Relock,
    /// Tracked `process.run` children (ADR-0026).
    pub(crate) processes: LiveProcesses,
    process_done_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
    /// `process.kill` and departed-generation reaps, off the loop; [`Supervisor::reap`] drains them.
    reaps: tokio::task::JoinSet<()>,
    /// Fires when the departed generation's children are gone; see [`Supervisor::dispatch_process_command`].
    departed_reaped: Option<tokio::sync::oneshot::Receiver<()>>,
    /// The sleep delay's wait (ADR-0385), answered by the next lock report, whatever it says, or
    /// by a replacement's first `CanLock` while that is unknown.
    sleep_waits: Option<tokio::sync::oneshot::Sender<()>>,
    /// Test hook replacing [`pam_worker::start_lock_conversation`] so tests never re-exec `/proc/self/exe`.
    #[cfg(test)]
    pam_starter: Option<std::sync::Arc<dyn Fn(&'static str, u64) -> pam_worker::Conversation + Send + Sync>>,
    /// Test hook overriding [`pam_worker::fingerprint_installed`] so tests never probe `/etc/pam.d`.
    #[cfg(test)]
    fingerprint_installed: Option<bool>,
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
        let relock =
            locked_flag.is_set().then_some(RelockReason::SupervisorRestarted).map_or(Relock::Idle, Relock::Owed);
        if relock != Relock::Idle {
            info!(
                "the session was locked when the last Supervisor stopped and the compositor has not unlocked it, so the boot Renderer will be asked \
                 to take that lock over (ADR-0060)"
            );
        }
        let mut supervisor = Self {
            registry,
            authoritative: Authoritative { generation_id: 0, child: boot_child, can_lock: Some(false) },
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
            renderer_departed: false,
            exit_code: 0,
            relock,
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
        if supervisor.relock == Relock::Idle {
            supervisor.push_lock_state();
        }
        supervisor.push_polkit_state();
        supervisor
    }

    /// Sends `SetSessionLock` and its state snapshot; every command is a lock-screen-visible state
    /// change (ADR-0052 decision 4).
    pub(crate) fn send_lock_command(&mut self, command: shared::SetSessionLock) {
        send_frame_logged(&self.registry, self.authoritative.generation_id, &SupervisorFrame::SetSessionLock(command));
        self.push_lock_state();
    }

    /// Pushes lock state and revision from the loop, since `main` builds its controller
    /// (ADR-0052 decision 4).
    pub(crate) fn push_lock_state(&mut self) {
        push_snapshot(
            &self.registry,
            self.authoritative.generation_id,
            &mut self.last_snapshots,
            Capability::Lock,
            &self.lock.snapshot(),
        );
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

    /// Starts the lock's worker for this session's user, in `mode`.
    fn lock_starter(&self, mode: &'static str) -> impl FnOnce(u64) -> pam_worker::Conversation + use<> {
        #[cfg(test)]
        let starter = self.pam_starter.clone();
        let events = self.pam_events_tx.clone();
        move |serial| {
            #[cfg(test)]
            if let Some(starter) = starter {
                return starter(mode, serial);
            }
            pam_worker::start_lock_conversation(nix::unistd::Uid::current().as_raw(), mode, serial, events)
        }
    }

    /// Starts the lock's fingerprint conversation when its stack is installed (ADR-0382).
    fn listen_for_fingerprint(&self) {
        #[cfg(test)]
        let installed = self.fingerprint_installed.unwrap_or_else(pam_worker::fingerprint_installed);
        #[cfg(not(test))]
        let installed = pam_worker::fingerprint_installed();
        if installed {
            self.lock.listen(self.lock_starter(pam_worker::FINGERPRINT));
        }
    }

    /// Answers the lock's PAM prompt (ADR-0381). ADR-0042, ADR-0052: this and
    /// `record_pam_message` are the only unlock path.
    fn begin_lock_authentication(&mut self, secret: shared::Zeroizing<Vec<u8>>, generation_id: u32) {
        if !self.lock.submit(secret, self.lock_starter(pam_worker::PASSWORD)) {
            debug!(
                "generation {generation_id}'s secure_submit(lock, authenticate) arrived with no lock held, or with an answer already with PAM; dropping"
            );
            return;
        }
        // A cheap retry after a fault or an expired lockout ended the fingerprint conversation.
        self.listen_for_fingerprint();
        self.push_lock_state();
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
            .map(|(capability, last)| {
                let bytes = last.snapshot.payload.to_string().len();
                (capability.as_str(), bytes, last.snapshot.revision, last.deduped)
            })
            .collect();
        sizes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        sizes
    }

    /// Replays snapshots to a newly registered authoritative generation, then gives it an owed
    /// lock.
    pub(crate) fn hydrate(&mut self, generation_id: u32) {
        if generation_id != self.authoritative.generation_id {
            return;
        }
        debug!("hydrating generation {generation_id} with {} snapshots", self.last_snapshots.len());
        for (capability, last) in std::mem::take(&mut self.last_snapshots) {
            let snapshot = send_owned(&self.registry, generation_id, last.snapshot);
            self.last_snapshots.insert(capability, Published { snapshot, deduped: last.deduped });
        }
        if self.icon_theme.is_some() {
            send_frame_logged(&self.registry, generation_id, &SupervisorFrame::IconTheme(self.icon_theme.clone()));
        }
        // ADR-0058 decision 4: replay before relock, or one default frame looks like a broken
        // shell. No Supervisor-side auth-capability check here; Renderer reports Refused if its
        // tree cannot reach PAM (ADR-0052 decision 3).
        if let Some(reason) = self.relock.start() {
            info!(
                "asking generation {generation_id} to take the session lock over, because {} (ADR-0058 decision 4, ADR-0060)",
                reason.because()
            );
            self.lock.lock();
        }
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
        if let Some(pgid) = self.hung_up_group(generation_id) {
            warn!("generation {generation_id} (pid {pgid}) is still running after the hangup; killing it");
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
        // At respawn, not departure, and `requested` counts: a lock asked for during a cooldown is still owed.
        let state = self.lock.snapshot();
        let was_locked = state.active || state.requested;
        let replacement_generation_id = self.next_generation_id;
        self.next_generation_id += 1;
        match self.renderer.spawn(replacement_generation_id) {
            Ok(child) => {
                if let Some(pid) = child.id() {
                    self.registry.expect_generation(replacement_generation_id, nix::unistd::Pid::from_raw(pid as i32));
                }
                // The departed generation's id must not stay claimable by whatever inherits its pid.
                let departed = self.authoritative.generation_id;
                let can_lock = self.authoritative.can_lock.filter(|can_lock| !can_lock);
                self.authoritative = Authoritative { generation_id: replacement_generation_id, child, can_lock };
                self.renderer_departed = false;
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
                self.relock.abandon();
                if was_locked {
                    // ADR-0058 decision 4: the lock object died; `active` is stale. `RendererLost`
                    // lets `lock()` proceed, then waits for a connection to send it.
                    self.lock.record(lock::LockEvent::RendererLost);
                    self.relock = Relock::Owed(RelockReason::RendererReplaced);
                    info!(
                        "the session is still locked, so generation {replacement_generation_id} will be asked to retake the lock once it connects"
                    );
                }
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

    /// `loginctl lock-session` (ADR-0138) arrives as logind's `Lock` signal and takes the same lock
    /// path and already-locked guard as `mantle.lock:lock()`.
    pub(crate) fn lock_requested_by_logind(&mut self) {
        info!("logind asked for a lock (loginctl lock-session)");
        self.lock.lock();
        self.push_lock_state();
    }

    /// logind announced sleep (ADR-0385): lock as `mantle.lock:lock()` does and answer `locked` once
    /// the lock is up or refused, or at once if it already is up. A lock animating out after a
    /// correct password is retaken once its release lands; see [`Supervisor::record_lock_report`].
    pub(crate) fn lock_before_sleep(&mut self, locked: tokio::sync::oneshot::Sender<()>) {
        let Some(can_lock) = self.authoritative.can_lock else {
            info!("the system is about to sleep; waiting for the new Renderer to say whether it can lock");
            self.sleep_waits = Some(locked);
            return;
        };
        if !can_lock {
            debug!("the system is about to sleep; this config has no lock surface, so it is not locked");
            let _ = locked.send(());
            return;
        }
        let state = self.lock.snapshot();
        if state.active && !state.unlocking {
            let _ = locked.send(());
            return;
        }
        self.sleep_waits = Some(locked);
        // An owed relock is the lock already; a second frame ends a Renderer that reads neither (#123).
        if self.relock == Relock::Idle {
            info!("the system is about to sleep; locking first");
            self.lock.lock();
            self.push_lock_state();
        }
    }

    /// `RendererFrame::CanLock`; settles a sleep that waited on an unknown answer.
    pub(crate) fn record_can_lock(&mut self, can_lock: bool) {
        let unknown = self.authoritative.can_lock.replace(can_lock).is_none();
        if let Some(locked) = self.sleep_waits.take_if(|_| unknown) {
            self.lock_before_sleep(locked);
        }
    }

    /// One frame of the lock's PAM conversation (ADR-0042, ADR-0381). The serial rejects frames
    /// from a conversation whose lock is gone: PAM can outlive its lock while the compositor or
    /// idle timer changes locks. A failure that showed a prompt starts the next attempt at once;
    /// one that showed none waits for a submit, or a broken stack would spin.
    pub(crate) fn record_pam_message(&mut self, serial: u64, message: shared::PamMessage) {
        match self.lock.hear(serial, message) {
            Heard::Stale => {
                debug!("dropping pam conversation {serial}'s frame; it no longer belongs to the lock on the glass")
            }
            Heard::Changed => self.push_lock_state(),
            Heard::Ended { outcome, prompted } => {
                // Every answer: a wrong password logged nothing at all once, so a lock screen that
                // would not open read the same whether PAM said no or the attempt never arrived.
                info!("pam answered {outcome:?} for lock conversation {serial}");
                let succeeded = outcome == shared::PamOutcome::Success;
                if !succeeded && prompted {
                    self.lock.converse(self.lock_starter(pam_worker::PASSWORD));
                }
                // Push before scheduling: `unlocking` is now true, and the config cannot animate a
                // window it has not been told about (ADR-0190).
                self.push_lock_state();
                if succeeded {
                    self.lock.unlock_after_animation();
                }
            }
        }
    }

    /// Records the authoritative lock answer (ADR-0052 decision 4).
    pub(crate) fn record_lock_report(&mut self, report: shared::LockReport) {
        if let Some(who) = self.relock.finish() {
            let who = who.subject();
            match &report.outcome {
                shared::LockOutcome::Locked => {
                    info!("{who} took the session lock over; the lock screen is back on the glass")
                }
                // Only the compositor's fallback is onscreen, not `lock_stays_authenticatable`.
                shared::LockOutcome::Refused(reason) => error!(
                    "{who} could not take the session lock over: {reason}. The session stays locked with no lock screen on it, \
                     so the way back in is a VT switch (ADR-0058 decision 4, ADR-0060)"
                ),
                other => warn!("{who}'s lock re-acquisition ended as {other:?} rather than a lock"),
            }
        }
        // Derive from the outcome before recording; the marker stays "locked" through RendererLost,
        // which clears `active` (ADR-0060).
        let change = lock::compositor_lock_change(&report.outcome);
        self.locked_flag.apply(change);
        // Publish the same outcome-derived change to logind, keeping `LockedHint` and the marker
        // aligned (ADR-0138).
        match change {
            lock::SessionLock::Taken => self.session_bridge.publish_locked_hint(true),
            lock::SessionLock::Released => self.session_bridge.publish_locked_hint(false),
            lock::SessionLock::Unchanged => {}
        }
        self.lock.record(lock::LockEvent::Reported(report.outcome));
        if let Some(locked) = self.sleep_waits.take() {
            if change == lock::SessionLock::Released {
                // An unlock that was underway when sleep was announced: lock again before suspend.
                self.sleep_waits = Some(locked);
                self.lock.lock();
            } else {
                let _ = locked.send(());
            }
        }
        // PAM starts with the lock, so its prompt is on screen before anything is typed (ADR-0381).
        self.lock.converse(self.lock_starter(pam_worker::PASSWORD));
        self.listen_for_fingerprint();
        self.push_lock_state();
    }

    /// Routes a roster command to its controller (ADR-0037); `lock` is separate (ADR-0052).
    pub(crate) fn dispatch_capability_command(&mut self, capability: Capability, envelope: &shared::CommandEnvelope) {
        // Keep polkit here beside the cancel path (ADR-0114).
        if capability == Capability::Polkit {
            if polkit::dispatch(&mut self.polkit, envelope) {
                self.push_polkit_state();
            }
            return;
        }
        self.capabilities.dispatch(capability, envelope, &self.lock);
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
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use crate::capabilities::test_support::{PrivateBus, fake_program, private_bus, within};
    use tokio::sync::mpsc;

    /// Kills any tracked process groups on drop so a panicking test cannot orphan children. A
    /// tracked child must stay unreaped until then, or be released before the test reaps it: a
    /// reaped pid may be another group's by the drop. The Supervisor drops a replaced child
    /// unreaped, and the fake Renderer outlives the test.
    #[derive(Default)]
    struct ChildGuard(Vec<nix::unistd::Pid>);

    impl ChildGuard {
        fn track(&mut self, child: &tokio::process::Child) {
            if let Some(pid) = child.id() {
                self.0.push(nix::unistd::Pid::from_raw(pid as i32));
            }
        }

        fn release(&mut self, child: &tokio::process::Child) {
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

    fn make_fake_renderer(dir: &std::path::Path) -> std::path::PathBuf {
        fake_program(dir, "fake-renderer", "exec sleep 30").into()
    }

    fn install_pam_spy(supervisor: &mut Supervisor) -> Arc<Mutex<Vec<&'static str>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        supervisor.pam_starter = Some(Arc::new(move |mode, _serial| {
            recorded.lock().expect("mutex poisoned").push(mode);
            pam_worker::Conversation::detached().0
        }));
        calls
    }

    /// A Supervisor booted in `dir` over an exited `/bin/true`, with its lock commands. The box
    /// keeps its other channels open for the test's length.
    async fn booted(
        bus: &PrivateBus,
        dir: &std::path::Path,
    ) -> (Supervisor, mpsc::UnboundedReceiver<shared::SetSessionLock>, Box<dyn std::any::Any>) {
        let connection = bus.connection().await;
        let (sound_tx, sounds) = std::sync::mpsc::sync_channel(1);
        let (idle_tx, idle) = mpsc::unbounded_channel();
        let (capabilities, signals) = Capabilities::new(connection.clone(), sound_tx, idle_tx);
        let (lock_tx, lock_commands) = mpsc::unbounded_channel();
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
            LockController::new(lock_tx),
            lock::SessionLockedFlag::at(dir.join("session-locked")),
            bridge,
            pam_tx,
            polkit_tx,
            done_tx,
        );
        supervisor.pam_starter = Some(Arc::new(|_, _| pam_worker::Conversation::detached().0));
        supervisor.fingerprint_installed = Some(false);
        supervisor.authoritative.child.wait().await.unwrap();
        (supervisor, lock_commands, Box::new((sounds, idle, signals, logind, pam, polkit, done)))
    }

    #[tokio::test]
    async fn boot_built_capabilities_hydrate_their_first_state_but_an_owed_relock_holds_lock_back() {
        let bus = private_bus().await;
        for locked in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if locked {
                std::fs::File::create(dir.path().join("session-locked")).unwrap();
            }
            let (mut supervisor, mut lock_commands, _open) = booted(&bus, dir.path()).await;

            // A locked boot relocks on its first registration, so only the unlocked one replays twice.
            let generations: &[u32] = if locked { &[0] } else { &[0, 1] };
            for &generation_id in generations {
                supervisor.authoritative.generation_id = generation_id;
                let (tx, mut frames) = mpsc::channel(4);
                supervisor.registry.register(generation_id, tx, Default::default());
                supervisor.hydrate(generation_id);
                let mut snapshots = HashMap::new();
                while let Ok(bytes) = frames.try_recv() {
                    let SupervisorFrame::StateSnapshot(snapshot) = serde_json::from_slice(&bytes).unwrap() else {
                        panic!("boot hydration sends only snapshots");
                    };
                    assert_eq!(snapshot.revision, 1, "replay must not advance the revision");
                    snapshots.insert(snapshot.capability, snapshot.payload);
                }
                assert_eq!(snapshots["polkit"]["active"], false);
                if locked {
                    assert!(!snapshots.contains_key("lock"), "an owed relock must not read as unlocked");
                    assert_eq!(lock_commands.try_recv().unwrap(), shared::SetSessionLock { locked: true });
                } else {
                    assert_eq!(snapshots["lock"]["active"], false);
                    assert!(lock_commands.try_recv().is_err(), "an unlocked boot must not request a lock");
                }
            }
        }
    }

    #[tokio::test]
    async fn sleep_locks_and_waits_for_the_next_lock_report_unless_already_locked() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, mut lock_commands, _open) = booted(&bus, dir.path()).await;
        supervisor.authoritative.can_lock = Some(true);

        let (locked_tx, mut locked) = tokio::sync::oneshot::channel();
        supervisor.lock_before_sleep(locked_tx);
        assert_eq!(lock_commands.try_recv().unwrap(), shared::SetSessionLock { locked: true });
        assert!(locked.try_recv().is_err(), "the delay must hold until the Renderer answers");
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Refused("none".into()) });
        assert_eq!(locked.try_recv(), Ok(()), "a refused lock has nothing left to wait for");

        supervisor.lock.record(lock::LockEvent::Reported(shared::LockOutcome::Locked));
        let (locked_tx, mut locked) = tokio::sync::oneshot::channel();
        supervisor.lock_before_sleep(locked_tx);
        assert_eq!(locked.try_recv(), Ok(()), "an existing lock releases the delay at once");
        assert!(lock_commands.try_recv().is_err());

        // A correct password, then the lid closes while the lock animates out.
        supervisor.lock.record(lock::LockEvent::Authenticated(shared::PamOutcome::Success));
        let (locked_tx, mut locked) = tokio::sync::oneshot::channel();
        supervisor.lock_before_sleep(locked_tx);
        assert!(locked.try_recv().is_err(), "a lock on its way out must not release the delay");
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Unlocked });
        assert_eq!(lock_commands.try_recv().unwrap(), shared::SetSessionLock { locked: true }, "relock after it lands");
        assert!(locked.try_recv().is_err(), "the delay holds until the relock answers");
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Refused("none".into()) });
        assert_eq!(locked.try_recv(), Ok(()));
    }

    #[tokio::test]
    async fn sleep_after_a_respawn_waits_for_the_replacement_to_say_whether_it_can_lock() {
        use tokio::sync::oneshot::error::TryRecvError::Empty;
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, mut lock_commands, _open) = booted(&bus, dir.path()).await;
        let sleep = |supervisor: &mut Supervisor| {
            let (locked_tx, locked) = tokio::sync::oneshot::channel();
            supervisor.lock_before_sleep(locked_tx);
            locked
        };
        let report = |outcome| shared::LockReport { outcome };
        // A config without a lock never holds a suspend, at start or after a respawn.
        assert_eq!(sleep(&mut supervisor).try_recv(), Ok(()));
        assert!(!supervisor.respawn_renderer().await);
        assert_eq!(sleep(&mut supervisor).try_recv(), Ok(()));
        assert!(lock_commands.try_recv().is_err(), "a config that cannot lock must not be asked to");

        // A replacement for a config that could lock: the suspend waits, then the lock is asked for.
        supervisor.record_can_lock(true);
        assert!(!supervisor.respawn_renderer().await);
        let mut waiting = sleep(&mut supervisor);
        assert_eq!((waiting.try_recv(), lock_commands.try_recv().is_err()), (Err(Empty), true));
        supervisor.record_can_lock(true);
        assert_eq!(lock_commands.try_recv().unwrap(), shared::SetSessionLock { locked: true });
        assert_eq!(waiting.try_recv(), Err(Empty), "held until the lock is up");
        supervisor.record_lock_report(report(shared::LockOutcome::Locked));
        assert_eq!(waiting.try_recv(), Ok(()));

        // Locked when it died: the owed relock's report answers the wait.
        assert!(!supervisor.respawn_renderer().await);
        let mut waiting = sleep(&mut supervisor);
        supervisor.record_can_lock(true);
        assert_eq!((waiting.try_recv(), lock_commands.try_recv().is_err()), (Err(Empty), true), "no second lock");
        supervisor.record_lock_report(report(shared::LockOutcome::Locked));
        assert_eq!(waiting.try_recv(), Ok(()));
        // The new config dropped its lock: released on its first `CanLock`, with nothing asked.
        supervisor.record_lock_report(report(shared::LockOutcome::Unlocked));
        supervisor.restart_brake = RestartBrake::default();
        assert!(!supervisor.respawn_renderer().await);
        let mut waiting = sleep(&mut supervisor);
        assert_eq!(waiting.try_recv(), Err(Empty));
        supervisor.record_can_lock(false);
        assert_eq!((waiting.try_recv(), lock_commands.try_recv().is_err()), (Ok(()), true));
    }

    #[test]
    fn a_respawn_discards_the_relock_in_flight() {
        let mut relock = Relock::Owed(RelockReason::SupervisorRestarted);
        assert_eq!(relock.start(), Some(RelockReason::SupervisorRestarted));
        relock.abandon();
        assert_eq!(relock, Relock::Idle);
        relock = Relock::Owed(RelockReason::SupervisorRestarted);
        relock.abandon();
        assert_eq!(relock, Relock::Owed(RelockReason::SupervisorRestarted), "an unsent ask survives a respawn");
        relock = Relock::Owed(RelockReason::RendererReplaced);
        assert_eq!(relock.finish(), None, "the dead generation's ask must not claim the next report");
        assert_eq!(relock.start(), Some(RelockReason::RendererReplaced));
        assert_eq!(relock.finish(), Some(RelockReason::RendererReplaced));
        assert_eq!(relock, Relock::Idle);
    }

    #[tokio::test]
    async fn hydrate_ignores_stale_generations_replays_icon_theme_and_relocks_once() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("session-locked")).unwrap();
        let (mut supervisor, mut lock_commands, _open) = booted(&bus, dir.path()).await;
        supervisor.set_icon_theme(Some("Papirus".into()));

        let (stale_tx, mut stale_frames) = mpsc::channel(8);
        supervisor.registry.register(99, stale_tx, Default::default());
        supervisor.hydrate(99);
        assert!(stale_frames.try_recv().is_err(), "stale registration must not be hydrated");
        assert!(lock_commands.try_recv().is_err());
        assert_eq!(supervisor.relock, Relock::Owed(RelockReason::SupervisorRestarted));

        let (tx, mut frames) = mpsc::channel(8);
        supervisor.registry.register(0, tx, Default::default());
        let expected_polkit = supervisor.last_snapshots[&Capability::Polkit].snapshot.payload.clone();
        // A reconnect replays both state and theme without asking to lock a second time.
        for reconnect in [false, true] {
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
            if reconnect {
                assert!(lock_commands.try_recv().is_err(), "owed relock must fire only once");
            } else {
                assert_eq!(lock_commands.try_recv().unwrap(), shared::SetSessionLock { locked: true });
            }
            assert_eq!(supervisor.relock, Relock::InFlight(RelockReason::SupervisorRestarted));
        }

        supervisor.set_icon_theme(None);
        while frames.try_recv().is_ok() {}
        supervisor.hydrate(0);
        while let Ok(bytes) = frames.try_recv() {
            assert!(!matches!(serde_json::from_slice(&bytes).unwrap(), SupervisorFrame::IconTheme(_)));
        }
    }

    #[tokio::test]
    async fn respawn_renderer_tracks_generations_reaps_departed_children_and_preserves_lock_intent() {
        use std::os::unix::process::ExitStatusExt;

        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let fake = make_fake_renderer(dir.path());
        let (mut supervisor, mut lock_commands, _open) = booted(&bus, dir.path()).await;
        let mut guard = ChildGuard::default();
        supervisor.renderer = Renderer::new(fake.clone(), dir.path(), dir.path(), None, 0, None);

        // Seed one child on departed gen 0 and another on gen 99; only gen 0 should reap on swap.
        process::registry::spawn_and_register_process(&mut supervisor.processes, 0, 1, "/bin/sleep", &["5".into()])
            .unwrap();
        process::registry::spawn_and_register_process(&mut supervisor.processes, 99, 2, "/bin/sleep", &["5".into()])
            .unwrap();

        supervisor.renderer_departed = true;
        supervisor.relock = Relock::InFlight(RelockReason::SupervisorRestarted);
        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        let p1 = nix::unistd::Pid::from_raw(supervisor.authoritative.child.id().unwrap() as i32);
        assert_eq!(supervisor.authoritative.generation_id, 1);
        assert!(!supervisor.renderer_departed);
        assert_eq!(supervisor.relock, Relock::Idle);
        assert_eq!(supervisor.registry.may_claim(1, p1), Some(true));
        assert_eq!(supervisor.registry.may_claim(1, nix::unistd::Pid::from_raw(p1.as_raw() + 1)), Some(false));
        within(supervisor.departed_reaped.take().unwrap()).await.unwrap();
        assert!(!supervisor.processes.contains_key(&(0, 1)));
        assert!(supervisor.processes.contains_key(&(99, 2)));

        // Locked (active = true, requested = false) departure forgets gen 1 and owes relock on gen 2.
        supervisor.lock.record(lock::LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(!supervisor.replace_departed_renderer(Ok(std::process::ExitStatus::from_raw(9))).await);
        guard.track(&supervisor.authoritative.child);
        assert_eq!(supervisor.registry.may_claim(1, p1), None, "departed pid expectation must be cleared");
        assert_eq!(supervisor.authoritative.generation_id, 2);
        assert!(!supervisor.lock.snapshot().active);
        assert_eq!(supervisor.relock, Relock::Owed(RelockReason::RendererReplaced));

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

        // Requested-only lock (active = false, requested = true) must also owe relock on gen 3.
        supervisor.relock = Relock::Idle;
        supervisor.lock.lock();
        assert_eq!(lock_commands.try_recv().unwrap(), shared::SetSessionLock { locked: true });
        assert!(supervisor.lock.snapshot().requested && !supervisor.lock.snapshot().active);
        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        assert_eq!(supervisor.authoritative.generation_id, 3);
        assert!(!supervisor.lock.snapshot().requested);
        assert_eq!(supervisor.relock, Relock::Owed(RelockReason::RendererReplaced));

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
        let (mut supervisor, _lock_commands, _open) = booted(&bus, dir.path()).await;
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
        let (mut supervisor, _lock_commands, _open) = booted(&bus, dir.path()).await;
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

    #[tokio::test]
    async fn record_lock_report_updates_flag_logind_pam_and_relock_attribution() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, mut lock_commands, _open) = booted(&bus, dir.path()).await;
        let (bridge, mut hints, _lock_requests, _logind_peer) = lock::logind::tests::test_bridge().await;
        supervisor.session_bridge = bridge;
        let pam_calls = install_pam_spy(&mut supervisor);
        let (tx, mut frames) = mpsc::channel(8);
        supervisor.registry.register(0, tx, Default::default());

        // An unrelated Locked report before hydration must not consume an Owed relock.
        supervisor.relock = Relock::Owed(RelockReason::RendererReplaced);
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Locked });
        assert_eq!(supervisor.relock, Relock::Owed(RelockReason::RendererReplaced));
        assert!(dir.path().join("session-locked").exists());
        assert_eq!(within(hints.recv()).await, Some(true));
        assert!(supervisor.lock.snapshot().active);
        assert!(supervisor.lock.snapshot().fingerprint.is_none());
        assert_eq!(*pam_calls.lock().unwrap(), vec![pam_worker::PASSWORD]);
        let mut last_lock = None;
        while let Ok(bytes) = frames.try_recv() {
            if let SupervisorFrame::StateSnapshot(s) = serde_json::from_slice(&bytes).unwrap()
                && s.capability == Capability::Lock.as_str()
            {
                last_lock = Some(s.payload);
            }
        }
        assert_eq!(last_lock.unwrap()["active"], true);

        // Unlocked clears the marker, logind hint, and lock snapshot without starting PAM.
        pam_calls.lock().unwrap().clear();
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Unlocked });
        assert!(!dir.path().join("session-locked").exists());
        assert_eq!(within(hints.recv()).await, Some(false));
        assert!(!supervisor.lock.snapshot().active);
        assert!(pam_calls.lock().unwrap().is_empty());

        // Hydrating moves Owed -> InFlight; the next Locked report attributes it and starts fingerprint too.
        supervisor.fingerprint_installed = Some(true);
        supervisor.hydrate(0);
        assert_eq!(lock_commands.try_recv().unwrap(), shared::SetSessionLock { locked: true });
        assert_eq!(supervisor.relock, Relock::InFlight(RelockReason::RendererReplaced));
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Locked });
        assert_eq!(supervisor.relock, Relock::Idle);
        assert!(supervisor.lock.snapshot().fingerprint.is_some());
        assert_eq!(*pam_calls.lock().unwrap(), vec![pam_worker::PASSWORD, pam_worker::FINGERPRINT]);
        assert_eq!(within(hints.recv()).await, Some(true));

        // Refused after an InFlight relock clears Relock while preserving the marker and hint.
        supervisor.relock = Relock::InFlight(RelockReason::SupervisorRestarted);
        pam_calls.lock().unwrap().clear();
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Refused("busy".into()) });
        assert_eq!(supervisor.relock, Relock::Idle);
        assert!(dir.path().join("session-locked").exists());
        assert!(hints.try_recv().is_err());

        // Finished clears the marker and hint.
        supervisor.record_lock_report(shared::LockReport { outcome: shared::LockOutcome::Finished });
        assert!(!dir.path().join("session-locked").exists());
        assert_eq!(within(hints.recv()).await, Some(false));
    }
}

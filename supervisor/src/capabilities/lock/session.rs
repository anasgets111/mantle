//! The session lock's phase as the Supervisor drives it: which `SetSessionLock` goes to which
//! generation and when, which `LockReport` counts, and what a departure, a password or a sleep
//! owes (ADR-0042, ADR-0052, ADR-0058 decision 4, ADR-0060, ADR-0385). Generations and event
//! routing stay in `Supervisor`.

use std::time::{Duration, Instant};

use shared::{Capability, SupervisorFrame, debug, error, info};

use crate::capabilities::conversation::Heard;
use crate::capabilities::lock;
use crate::generation::RESTART_COOLDOWN;
use crate::pam_worker;
use crate::send_frame_logged;
use crate::snapshot::push_snapshot;
use crate::supervisor::Supervisor;

/// The session lock as this Supervisor knows it. One phase, so at most one acquisition or release
/// is in flight, and every `LockReport` is checked against it by generation and id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockPhase {
    Idle,
    /// `Lock{id}` is queued to `generation`. `debt` if the compositor may already hold a lock.
    Acquiring {
        generation: u32,
        id: u64,
        debt: Option<Debt>,
    },
    /// `generation` reported `Locked{id}`.
    Held {
        generation: u32,
        id: u64,
        release: Release,
    },
    /// The compositor may hold a lock nothing of ours draws (ADR-0058 decision 4, ADR-0060): the
    /// next ready generation takes it over, no sooner than `retry_at`.
    Recovering {
        debt: Debt,
        retry_at: Option<Instant>,
    },
}

/// A held lock's way out (ADR-0042, ADR-0190).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    None,
    /// A correct password answered this lock; it is released at the end of the animation window.
    Scheduled(Instant),
    /// `Unlock{id}` is queued; the lock ends on `Unlocked{id}`.
    Sent,
}

/// A lock the compositor may hold, and the wait before asking again after a takeover was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Debt {
    reason: RelockReason,
    backoff: Duration,
}

impl Debt {
    pub(crate) fn new(reason: RelockReason) -> Self {
        Self { reason, backoff: Duration::ZERO }
    }
}

/// The first wait after a refused takeover: sway can refuse one before it has processed the dead
/// holder's disconnect. Doubling to `RESTART_COOLDOWN` keeps a config that cannot lock at one
/// refusal per 30 s.
const RETAKE_BACKOFF: Duration = Duration::from_secs(1);

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

impl Supervisor {
    /// `mantle.lock:lock()`, logind's `Lock`, or a sleep. A held lock stays held; one animating out
    /// after a correct password stays up; one already being released is taken again once it lands.
    pub(crate) fn request_lock(&mut self) {
        self.lock_wanted = true;
        self.lock.record(lock::LockEvent::LockRequested);
        if let LockPhase::Held { release: release @ Release::Scheduled(_), .. } = &mut self.lock_phase {
            *release = Release::None;
            info!("a lock was asked for while the lock screen animated out; it stays up");
            self.lock.record(lock::LockEvent::ReleaseCancelled);
            self.lock.converse(self.lock_starter(pam_worker::PASSWORD));
            self.listen_for_fingerprint();
        }
        self.sync_lock(Instant::now());
        self.push_lock_state();
    }

    fn lock_ready(&self) -> bool {
        let generation = Some(self.authoritative.generation_id);
        !self.renderer_departed && self.hydrated == generation && self.resolved == generation
    }

    /// Sends what the phase owes the authoritative generation by `now`: a due release, or a lock
    /// that is wanted or owed. The only place a `SetSessionLock` is decided.
    pub(crate) fn sync_lock(&mut self, now: Instant) {
        if !self.lock_ready() {
            return;
        }
        match self.lock_phase {
            LockPhase::Held { generation, id, release: Release::Scheduled(at) } if at <= now => {
                if self.send_lock(shared::SetSessionLock { locked: false, id }) {
                    self.lock_phase = LockPhase::Held { generation, id, release: Release::Sent };
                }
            }
            LockPhase::Idle if self.lock_wanted => self.acquire(None),
            LockPhase::Recovering { debt, retry_at } if self.lock_wanted || retry_at.is_none_or(|at| at <= now) => {
                self.acquire(Some(debt))
            }
            _ => {}
        }
    }

    /// When [`Self::sync_lock`] next has something to send; `main.rs` sleeps until then.
    pub(crate) fn next_lock_deadline(&self) -> Option<Instant> {
        match self.lock_phase {
            _ if !self.lock_ready() => None,
            LockPhase::Held { release: Release::Scheduled(at), .. } => Some(at),
            LockPhase::Recovering { retry_at, .. } => retry_at,
            _ => None,
        }
    }

    pub(crate) fn lock_deadline_passed(&mut self) {
        self.sync_lock(Instant::now());
    }

    /// Asks the authoritative generation for a new lock, writing the marker first: the compositor
    /// may grant it to a Renderer that then outlives this process, and a restart must know.
    fn acquire(&mut self, debt: Option<Debt>) {
        self.lock_id += 1;
        let (generation, id) = (self.authoritative.generation_id, self.lock_id);
        self.locked_flag.set(true);
        if let Some(debt) = debt {
            info!(
                "asking generation {generation} to take the session lock over, because {} (ADR-0058 decision 4, ADR-0060)",
                debt.reason.because()
            );
        }
        if self.send_lock(shared::SetSessionLock { locked: true, id }) {
            self.lock_phase = LockPhase::Acquiring { generation, id, debt };
        }
    }

    /// Queues `command`. A refused enqueue (no connection, or a full queue, which hangs up) is not
    /// sent: the phase stays, and the next registration's hydrate sends it.
    fn send_lock(&mut self, command: shared::SetSessionLock) -> bool {
        let generation = self.authoritative.generation_id;
        let sent = send_frame_logged(&self.registry, generation, &SupervisorFrame::SetSessionLock(command));
        if !sent {
            self.hydrated = None;
        }
        sent
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
    pub(crate) fn begin_lock_authentication(&mut self, secret: shared::Zeroizing<Vec<u8>>, generation_id: u32) {
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

    /// `loginctl lock-session` (ADR-0138) arrives as logind's `Lock` signal and takes the same path
    /// as `mantle.lock:lock()`.
    pub(crate) fn lock_requested_by_logind(&mut self) {
        info!("logind asked for a lock (loginctl lock-session)");
        self.request_lock();
    }

    /// logind announced sleep (ADR-0385): lock as `mantle.lock:lock()` does and answer `locked` once
    /// a lock asked for since is up or refused, or at once if one is held and staying up.
    pub(crate) fn lock_before_sleep(&mut self, locked: tokio::sync::oneshot::Sender<()>) {
        let Some(can_lock) = self.authoritative.can_lock else {
            info!("the system is about to sleep; waiting for the Renderer to say whether it can lock");
            self.sleep_waits = Some(locked);
            return;
        };
        // Only with no lock taken or owed: a held, releasing or lost one is still the session's.
        if !can_lock && self.lock_phase == LockPhase::Idle {
            debug!("the system is about to sleep; this config has no lock surface, so it is not locked");
            let _ = locked.send(());
            return;
        }
        self.request_lock();
        if matches!(self.lock_phase, LockPhase::Held { release: Release::None, .. }) {
            let _ = locked.send(());
            return;
        }
        info!("the system is about to sleep; locking first");
        self.sleep_waits = Some(locked);
    }

    /// `RendererFrame::CanLock`; settles a sleep that waited on an unknown answer.
    // ponytail: a sleep settles on the first `CanLock`, before snapshots can show the field; upgrade: after hydration.
    pub(crate) fn record_can_lock(&mut self, can_lock: bool) {
        self.resolved = Some(self.authoritative.generation_id);
        let unknown = self.authoritative.can_lock.replace(can_lock).is_none();
        match self.sleep_waits.take_if(|_| unknown) {
            Some(locked) => self.lock_before_sleep(locked),
            None => self.sync_lock(Instant::now()),
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
                // ADR-0042: the only way to a release. The password answered after any lock asked
                // for while this one was up, so that ask is spent.
                if succeeded && let LockPhase::Held { release: release @ Release::None, .. } = &mut self.lock_phase {
                    *release = Release::Scheduled(Instant::now() + self.lock.unlock_animation());
                    self.lock_wanted = false;
                    self.sync_lock(Instant::now());
                }
            }
        }
    }

    /// Applies a Renderer's `LockReport` (ADR-0052 decision 4). It counts only for the lock in
    /// flight that it names, by generation and id: an acquisition's `Locked` or `Refused`, a held
    /// lock's `Finished`, a sent release's `Unlocked`. Any other changes nothing, not even the marker.
    pub(crate) fn record_lock_report(&mut self, generation_id: u32, report: shared::LockReport) {
        use shared::LockOutcome::{Finished, Locked, Refused, Unlocked};
        let names = |generation, id| (generation, id) == (generation_id, report.id);
        match (self.lock_phase, &report.outcome) {
            (LockPhase::Acquiring { generation, id, debt }, Locked) if names(generation, id) => {
                if let Some(debt) = debt {
                    info!("{} took the session lock over; the lock screen is back on the glass", debt.reason.subject());
                }
                self.lock_phase = LockPhase::Held { generation, id, release: Release::None };
                self.lock_wanted = false;
                self.session_bridge.publish_locked_hint(true);
            }
            (LockPhase::Acquiring { generation, id, debt }, Refused(reason)) if names(generation, id) => {
                self.lock_wanted = false;
                if let Some(debt) = debt {
                    // The compositor may still hold the lock, so the debt stands (ADR-0060).
                    let backoff = (debt.backoff * 2).clamp(RETAKE_BACKOFF, RESTART_COOLDOWN);
                    error!(
                        "{} could not take the session lock over: {reason}. The session may stay locked with no lock screen on it until \
                         a retry in {backoff:?} succeeds; the way back in meanwhile is a VT switch (ADR-0058 decision 4, ADR-0060)",
                        debt.reason.subject()
                    );
                    let retry_at = Some(Instant::now() + backoff);
                    self.lock_phase = LockPhase::Recovering { debt: Debt { backoff, ..debt }, retry_at };
                } else {
                    // Nothing was taken and nothing older is owed.
                    self.lock_phase = LockPhase::Idle;
                    self.locked_flag.set(false);
                }
            }
            // Finished: the compositor ended the lock itself. sway, niri and Hyprland unlock the
            // session on it, so the marker clears with it.
            (LockPhase::Held { generation, id, .. }, Finished)
            | (LockPhase::Held { generation, id, release: Release::Sent }, Unlocked)
                if names(generation, id) =>
            {
                self.lock_phase = LockPhase::Idle;
                self.locked_flag.set(false);
                self.session_bridge.publish_locked_hint(false);
            }
            (_, outcome) => {
                debug!(
                    "ignoring generation {generation_id}'s {outcome:?} for lock {}: not the lock in flight",
                    report.id
                );
                return;
            }
        }
        let answered = matches!(report.outcome, Locked | Refused(_));
        self.lock.record(lock::LockEvent::Reported(report.outcome));
        if let Some(locked) = self.sleep_waits.take_if(|_| answered) {
            let _ = locked.send(());
        }
        // PAM starts with the lock, so its prompt is on screen before anything is typed (ADR-0381).
        self.lock.converse(self.lock_starter(pam_worker::PASSWORD));
        self.listen_for_fingerprint();
        // A lock wanted since this one began, or owed, goes out now.
        self.sync_lock(Instant::now());
        self.push_lock_state();
    }

    /// The authoritative Renderer died (ADR-0058 decision 4): its lock object died with it, granted
    /// or not, so the compositor may hold a lock nothing draws. A scheduled release dies too; a
    /// wanted lock stays wanted.
    pub(crate) fn lock_departed(&mut self) {
        let debt = match self.lock_phase {
            LockPhase::Acquiring { debt, .. } => Some(debt.unwrap_or(Debt::new(RelockReason::RendererReplaced))),
            LockPhase::Held { .. } => Some(Debt::new(RelockReason::RendererReplaced)),
            _ => None,
        };
        if let Some(debt) = debt {
            // ponytail: a dead Renderer's late `Unlocked` costs one more password; upgrade: an EOF-drain gate.
            self.lock_phase = LockPhase::Recovering { debt, retry_at: None };
            info!("the session may still be locked, so its replacement will be asked to retake the lock once it is up");
        }
        self.lock.record(lock::LockEvent::RendererLost);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

    use crate::capabilities::test_support::{private_bus, within};
    use crate::supervisor::tests::{
        ChildGuard, booted, crash, held, make_fake_renderer, marked, pam_succeeds, report, sent, up,
    };

    /// A lock asked for before the generation is up, as `loginctl lock-session` at login or during a
    /// respawn: sent once both the replay and the config's first resolve are done, and only once.
    #[tokio::test]
    async fn a_lock_asked_before_the_renderer_is_up_goes_once_after_its_replay_and_first_resolve() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        supervisor.lock_requested_by_logind();
        supervisor.request_lock();
        assert!(!marked(dir.path()), "nothing was sent, so nothing can be held");

        let (tx, mut frames) = mpsc::channel(64);
        supervisor.registry.register(0, tx, Default::default());
        supervisor.hydrate(0);
        assert_eq!(sent(&mut frames), [], "not before the Wayland thread has resolved its config");
        supervisor.record_can_lock(true);
        assert_eq!(sent(&mut frames), [(true, 1)]);
        assert!(marked(dir.path()), "set with the lock; its order is the failed-enqueue test's");
        // A reconnect replays again; the lock in flight is not asked twice.
        supervisor.hydrate(0);
        supervisor.record_can_lock(true);
        supervisor.request_lock();
        assert_eq!(sent(&mut frames), []);
    }

    /// Only the acquisition in flight, by generation and id, moves the phase, the marker or logind's
    /// hint: a late `Locked` after an unlock must not lock the next restart (finding 6).
    #[tokio::test]
    async fn a_report_counts_only_for_the_acquisition_it_names() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
        let (bridge, mut hints, _lock_requests, _logind_peer) = lock::logind::tests::test_bridge().await;
        supervisor.session_bridge = bridge;
        let mut frames = up(&mut supervisor);
        supervisor.request_lock();
        let refused = || shared::LockOutcome::Refused("other".into());
        for (generation, id, outcome) in [
            (0, 2, shared::LockOutcome::Locked),
            (7, 1, shared::LockOutcome::Locked),
            (0, 2, refused()),
            (0, 1, shared::LockOutcome::Unlocked),
            (0, 1, shared::LockOutcome::Finished),
        ] {
            supervisor.record_lock_report(generation, shared::LockReport { id, outcome });
            assert!(matches!(supervisor.lock_phase, LockPhase::Acquiring { id: 1, .. }), "{:?}", supervisor.lock_phase);
        }
        report(&mut supervisor, 1, shared::LockOutcome::Locked);
        assert_eq!(within(hints.recv()).await, Some(true));
        assert_eq!(started.lock().unwrap().len(), 1, "PAM starts with the lock");
        pam_succeeds(&mut supervisor, &started);
        assert_eq!(sent(&mut frames), [(true, 1), (false, 1)]);
        for (generation, id, outcome) in [
            (7, 1, shared::LockOutcome::Unlocked),
            (0, 2, shared::LockOutcome::Unlocked),
            (7, 1, shared::LockOutcome::Finished),
            (0, 1, shared::LockOutcome::Locked),
            (0, 1, refused()),
        ] {
            supervisor.record_lock_report(generation, shared::LockReport { id, outcome });
            assert!(matches!(supervisor.lock_phase, LockPhase::Held { release: Release::Sent, .. }));
        }
        report(&mut supervisor, 1, shared::LockOutcome::Unlocked);
        assert_eq!(within(hints.recv()).await, Some(false));
        assert!(!marked(dir.path()));
        // A replayed or late `Locked` for the released lock, from any generation.
        for generation in [0, 9] {
            supervisor
                .record_lock_report(generation, shared::LockReport { id: 1, outcome: shared::LockOutcome::Locked });
        }
        assert_eq!(supervisor.lock_phase, LockPhase::Idle);
        assert!(!marked(dir.path()) && !supervisor.lock.snapshot().active);
        assert!(hints.try_recv().is_err());
    }

    /// The marker goes down before the lock frame, so a Supervisor that dies before the answer
    /// leaves its successor knowing the compositor may hold one (finding 2).
    #[tokio::test]
    async fn a_supervisor_that_dies_with_a_lock_in_flight_leaves_its_successor_to_retake_it() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        supervisor.request_lock();
        assert_eq!(sent(&mut frames), [(true, 1)]);
        drop(supervisor);
        let (successor, _started, _open) = booted(&bus, dir.path()).await;
        let debt = Debt::new(RelockReason::SupervisorRestarted);
        assert_eq!(successor.lock_phase, LockPhase::Recovering { debt, retry_at: None });
    }

    /// A refusal proves nothing was taken, so a lock asked for clears the marker it wrote; a refused
    /// takeover proves nothing about the lock the compositor already holds (finding 3).
    #[tokio::test]
    async fn a_refused_lock_clears_its_marker_but_a_refused_takeover_keeps_it_and_retries_with_backoff() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        supervisor.request_lock();
        assert_eq!(sent(&mut frames), [(true, 1)]);
        report(&mut supervisor, 1, shared::LockOutcome::Refused("no lock node".into()));
        assert_eq!((supervisor.lock_phase, marked(dir.path())), (LockPhase::Idle, false));
        assert_eq!(supervisor.lock.snapshot().error, "no lock node");

        std::fs::File::create(dir.path().join("session-locked")).unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        let mut waits = Vec::new();
        for id in 1..=7 {
            assert_eq!(sent(&mut frames), [(true, id)]);
            let before = Instant::now();
            report(&mut supervisor, id, shared::LockOutcome::Refused("a lock client already holds it".into()));
            assert!(marked(dir.path()), "the compositor may still hold the dead holder's lock");
            let retry = supervisor.next_lock_deadline().expect("a retry is armed");
            waits.push((retry - before).as_secs());
            supervisor.sync_lock(retry - Duration::from_millis(1));
            assert_eq!(sent(&mut frames), [], "not before the backoff");
            supervisor.sync_lock(retry);
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
        // A lock asked for during the backoff goes at once; its refusal spends it, so none follows.
        assert_eq!(sent(&mut frames), [(true, 8)]);
        report(&mut supervisor, 8, shared::LockOutcome::Refused("a lock client already holds it".into()));
        supervisor.request_lock();
        assert_eq!(sent(&mut frames), [(true, 9)]);
        report(&mut supervisor, 9, shared::LockOutcome::Refused("a lock client already holds it".into()));
        supervisor.sync_lock(Instant::now());
        assert_eq!(sent(&mut frames), []);
        supervisor.request_lock();
        report(&mut supervisor, 10, shared::LockOutcome::Locked);
        assert!(matches!(supervisor.lock_phase, LockPhase::Held { id: 10, .. }) && marked(dir.path()));
    }

    #[tokio::test]
    async fn a_correct_password_releases_its_own_lock_once_the_window_ends() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        let id = held(&mut supervisor, &mut frames);
        supervisor.lock.set_unlock_animation(Duration::from_millis(300));
        pam_succeeds(&mut supervisor, &started);
        let at = supervisor.next_lock_deadline().expect("the release is timed");
        supervisor.sync_lock(at - Duration::from_millis(1));
        assert_eq!(sent(&mut frames), [], "the lock stays up through its out-animation");
        supervisor.sync_lock(at);
        assert_eq!(sent(&mut frames), [(false, id)]);
        assert_eq!(supervisor.next_lock_deadline(), None, "sent once");
        assert!(supervisor.lock.snapshot().active, "active until the Renderer reports the release");
        report(&mut supervisor, id, shared::LockOutcome::Unlocked);
        assert!(!supervisor.lock.snapshot().active && !marked(dir.path()));
    }

    /// Option (b): a lock asked for while the screen animates out after a correct password keeps the
    /// session locked, with a fresh prompt; once the unlock is sent, it locks again after it lands.
    #[tokio::test]
    async fn a_lock_asked_during_the_unlock_animation_keeps_the_session_locked() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        let id = held(&mut supervisor, &mut frames);
        supervisor.lock.set_unlock_animation(Duration::from_millis(300));
        pam_succeeds(&mut supervisor, &started);
        let at = supervisor.next_lock_deadline().unwrap();
        supervisor.lock_requested_by_logind();
        assert!(!supervisor.lock.snapshot().unlocking, "the shell sees the lock stay");
        assert_eq!(started.lock().unwrap().len(), 2, "with a new password conversation");
        assert_eq!(supervisor.next_lock_deadline(), None);
        supervisor.sync_lock(at);
        assert_eq!(sent(&mut frames), [], "the answered release is revoked");

        // The next password releases it; a lock asked for after the unlock went out follows it.
        supervisor.lock.set_unlock_animation(Duration::ZERO);
        pam_succeeds(&mut supervisor, &started);
        assert_eq!(sent(&mut frames), [(false, id)]);
        supervisor.request_lock();
        assert_eq!(sent(&mut frames), []);
        report(&mut supervisor, id, shared::LockOutcome::Unlocked);
        assert_eq!(sent(&mut frames), [(true, id + 1)]);
    }

    /// A lock asked for while one is up is kept until the next password: `Finished` already on
    /// its way must not swallow it, and the password must not be followed by a relock.
    #[tokio::test]
    async fn a_lock_asked_while_one_is_held_survives_its_finish_but_not_the_next_password() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        let id = held(&mut supervisor, &mut frames);
        supervisor.request_lock();
        assert_eq!(sent(&mut frames), [], "the lock up already answers it");
        report(&mut supervisor, id, shared::LockOutcome::Finished);
        assert_eq!(sent(&mut frames), [(true, id + 1)], "the compositor ended the lock before the ask was read");

        report(&mut supervisor, id + 1, shared::LockOutcome::Locked);
        supervisor.request_lock();
        pam_succeeds(&mut supervisor, &started);
        report(&mut supervisor, id + 1, shared::LockOutcome::Unlocked);
        assert_eq!(sent(&mut frames), [(false, id + 1)], "the password came after the ask, so it stays unlocked");
    }

    /// ADR-0058 decision 4: a Renderer that dies holding the lock, taking it, or animating it out
    /// leaves the compositor locked; its replacement retakes it once, and no answered release
    /// reaches it.
    #[tokio::test]
    async fn a_renderer_that_dies_with_a_lock_leaves_it_to_its_replacement_and_its_release_with_it() {
        let bus = private_bus().await;
        for scheduled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
            let mut frames = up(&mut supervisor);
            let id = held(&mut supervisor, &mut frames);
            if scheduled {
                supervisor.lock.set_unlock_animation(Duration::from_millis(300));
                pam_succeeds(&mut supervisor, &started);
            }
            let stale = started.lock().unwrap().last().unwrap().1;
            crash(&mut supervisor).await;
            assert!(matches!(supervisor.lock_phase, LockPhase::Recovering { .. }) && marked(dir.path()));
            assert!(!supervisor.lock.snapshot().active && !supervisor.lock.snapshot().unlocking);
            let wrong = shared::PamMessage::Outcome(shared::PamOutcome::Success);
            supervisor.record_pam_message(stale, wrong);
            assert!(!supervisor.lock.snapshot().unlocking, "the dead lock's conversation answers nothing");

            // The replacement dies before answering: still owed, once, to the next one.
            let mut replacement = up(&mut supervisor);
            assert_eq!(sent(&mut replacement), [(true, id + 1)]);
            crash(&mut supervisor).await;
            supervisor.lock_requested_by_logind();
            let mut next = up(&mut supervisor);
            assert_eq!(sent(&mut next), [(true, id + 2)], "{scheduled}: one lock and no unlock");
            report(&mut supervisor, id + 2, shared::LockOutcome::Locked);
            assert!(!supervisor.lock_wanted, "the logind ask is answered by the takeover");
        }
    }

    /// Without a drain gate, the departed Renderer's release is not believed: its replacement
    /// relocks, and a late report from it changes nothing.
    #[tokio::test]
    async fn a_departed_renderers_reports_never_move_its_replacements_lock() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        let id = held(&mut supervisor, &mut frames);
        pam_succeeds(&mut supervisor, &started);
        assert_eq!(sent(&mut frames), [(false, id)]);
        crash(&mut supervisor).await;
        let mut replacement = up(&mut supervisor);
        assert_eq!(sent(&mut replacement), [(true, id + 1)]);
        report(&mut supervisor, id + 1, shared::LockOutcome::Locked);
        for outcome in [shared::LockOutcome::Unlocked, shared::LockOutcome::Finished, shared::LockOutcome::Locked] {
            supervisor.record_lock_report(0, shared::LockReport { id, outcome });
        }
        let replacement = LockPhase::Held { id: id + 1, generation: 1, release: Release::None };
        assert_eq!(supervisor.lock_phase, replacement);
        assert!(marked(dir.path()) && supervisor.lock.snapshot().active);
    }

    /// Lua's `mantle.lock:lock()` is the same request; there is no unlock action (ADR-0042).
    #[tokio::test]
    async fn lua_lock_takes_the_request_path_and_lua_cannot_unlock() {
        use crate::capabilities::test_support::envelope;
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let mut frames = up(&mut supervisor);
        let action = |name| envelope(Capability::Lock, name, Vec::new());
        supervisor.dispatch_capability_command(Capability::Lock, &action("lock"));
        report(&mut supervisor, 1, shared::LockOutcome::Locked);
        supervisor.dispatch_capability_command(Capability::Lock, &action("unlock"));
        assert_eq!(sent(&mut frames), [(true, 1)]);
    }

    /// No lock deadline is left in the past once it is handled, or `main.rs`'s timer arm would
    /// fire every turn.
    #[tokio::test]
    async fn every_lock_deadline_moves_on_once_it_passes() {
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
        let settles = |supervisor: &mut Supervisor| {
            let due = supervisor.next_lock_deadline().expect("a deadline is armed");
            supervisor.sync_lock(due);
            assert!(supervisor.next_lock_deadline().is_none_or(|next| next > due), "{:?}", supervisor.lock_phase);
        };
        let mut frames = up(&mut supervisor);
        let id = held(&mut supervisor, &mut frames);
        supervisor.lock.set_unlock_animation(Duration::from_millis(300));
        pam_succeeds(&mut supervisor, &started);
        settles(&mut supervisor);
        assert_eq!(sent(&mut frames), [(false, id)]);
        report(&mut supervisor, id, shared::LockOutcome::Unlocked);
        // A release whose enqueue fails waits for the next registration instead.
        let id = held(&mut supervisor, &mut frames);
        pam_succeeds(&mut supervisor, &started);
        drop(frames);
        settles(&mut supervisor);
        assert!(
            matches!(supervisor.lock_phase, LockPhase::Held { id: held, release: Release::Scheduled(_), .. } if held == id)
        );
        // A refused takeover's retry, sent or not.
        crash(&mut supervisor).await;
        let mut frames = up(&mut supervisor);
        let [(true, id)] = sent(&mut frames)[..] else { panic!() };
        report(&mut supervisor, id, shared::LockOutcome::Refused("busy".into()));
        settles(&mut supervisor);
        report(&mut supervisor, id + 1, shared::LockOutcome::Refused("busy".into()));
        drop(frames);
        settles(&mut supervisor);
    }

    #[tokio::test]
    async fn sleep_locks_and_waits_for_the_lock_it_asked_for_unless_one_is_up_and_staying() {
        use tokio::sync::oneshot::error::TryRecvError::Empty;
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, started, _open) = booted(&bus, dir.path()).await;
        let sleep = |supervisor: &mut Supervisor| {
            let (locked_tx, locked) = tokio::sync::oneshot::channel();
            supervisor.lock_before_sleep(locked_tx);
            locked
        };
        // Before the boot Renderer says whether it can lock, the suspend waits to learn it.
        let mut waiting = sleep(&mut supervisor);
        assert_eq!(waiting.try_recv(), Err(Empty));
        let mut frames = up(&mut supervisor);
        assert_eq!(sent(&mut frames), [(true, 1)]);
        assert_eq!(waiting.try_recv(), Err(Empty), "the delay must hold until the Renderer answers");
        report(&mut supervisor, 1, shared::LockOutcome::Refused("none".into()));
        assert_eq!(waiting.try_recv(), Ok(()), "a refused lock has nothing left to wait for");

        let id = held(&mut supervisor, &mut frames);
        assert_eq!(sleep(&mut supervisor).try_recv(), Ok(()), "a lock up releases the delay at once");
        assert_eq!(sent(&mut frames), []);

        // A correct password, then the lid closes while the lock animates out: it stays up.
        supervisor.lock.set_unlock_animation(Duration::from_millis(300));
        pam_succeeds(&mut supervisor, &started);
        assert_eq!(sleep(&mut supervisor).try_recv(), Ok(()));
        assert!(!supervisor.lock.snapshot().unlocking);

        // Once the unlock is out, the delay waits for the relock it asked for.
        supervisor.lock.set_unlock_animation(Duration::ZERO);
        pam_succeeds(&mut supervisor, &started);
        assert_eq!(sent(&mut frames), [(false, id)]);
        let mut waiting = sleep(&mut supervisor);
        report(&mut supervisor, id, shared::LockOutcome::Unlocked);
        assert_eq!(waiting.try_recv(), Err(Empty), "a release does not answer the lock asked for");
        assert_eq!(sent(&mut frames), [(true, id + 1)]);
        report(&mut supervisor, id + 1, shared::LockOutcome::Locked);
        assert_eq!(waiting.try_recv(), Ok(()));
    }

    #[tokio::test]
    async fn sleep_after_a_respawn_waits_for_the_replacement_to_say_whether_it_can_lock() {
        use tokio::sync::oneshot::error::TryRecvError::Empty;
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        let sleep = |supervisor: &mut Supervisor| {
            let (locked_tx, locked) = tokio::sync::oneshot::channel();
            supervisor.lock_before_sleep(locked_tx);
            locked
        };
        // A config without a lock does not hold a suspend once it has said so; after a respawn, only
        // until the replacement says so too, as its config may have changed.
        supervisor.record_can_lock(false);
        assert_eq!(sleep(&mut supervisor).try_recv(), Ok(()));
        crash(&mut supervisor).await;
        let mut waiting = sleep(&mut supervisor);
        assert_eq!(waiting.try_recv(), Err(Empty));
        supervisor.record_can_lock(false);
        assert_eq!(waiting.try_recv(), Ok(()));

        // A replacement for a config that could lock: the suspend waits, then the lock is asked for.
        supervisor.record_can_lock(true);
        crash(&mut supervisor).await;
        let mut waiting = sleep(&mut supervisor);
        assert_eq!(waiting.try_recv(), Err(Empty));
        let mut frames = up(&mut supervisor);
        let [(true, id)] = sent(&mut frames)[..] else { panic!("one lock is sent") };
        assert_eq!(waiting.try_recv(), Err(Empty), "held until the lock is up");
        report(&mut supervisor, id, shared::LockOutcome::Locked);
        assert_eq!(waiting.try_recv(), Ok(()));

        // Locked when it died: the takeover's report answers the wait, and no second lock goes.
        crash(&mut supervisor).await;
        let mut waiting = sleep(&mut supervisor);
        let mut frames = up(&mut supervisor);
        assert_eq!(sent(&mut frames), [(true, id + 1)]);
        assert_eq!(waiting.try_recv(), Err(Empty));
        report(&mut supervisor, id + 1, shared::LockOutcome::Locked);
        assert_eq!(waiting.try_recv(), Ok(()));

        // A replacement that cannot lock still owes the lock the compositor may hold, so the
        // suspend waits for the takeover's answer.
        crash(&mut supervisor).await;
        let mut waiting = sleep(&mut supervisor);
        let (tx, mut frames) = mpsc::channel(64);
        let generation = supervisor.authoritative.generation_id;
        supervisor.registry.register(generation, tx, Default::default());
        supervisor.hydrate(generation);
        supervisor.record_can_lock(false);
        assert_eq!(sent(&mut frames), [(true, id + 2)]);
        assert_eq!(waiting.try_recv(), Err(Empty));
        report(&mut supervisor, id + 2, shared::LockOutcome::Refused("no lock node".into()));
        assert_eq!(waiting.try_recv(), Ok(()));
    }

    /// A lock asked for while no Renderer is up, in the restart brake's cooldown or after a failed
    /// spawn, arms no deadline to fire every turn and goes to the next Renderer up.
    #[tokio::test]
    async fn a_lock_asked_while_no_renderer_is_up_goes_to_the_next_one() {
        use std::os::unix::process::ExitStatusExt;
        let bus = private_bus().await;
        let dir = tempfile::tempdir().unwrap();
        let mut guard = ChildGuard::default();
        let fake = make_fake_renderer(dir.path());
        let renderer = |path: &std::path::Path| {
            crate::generation::Renderer::new(path.into(), dir.path(), dir.path(), None, 0, None)
        };
        let died = || Ok(std::process::ExitStatus::from_raw(9));
        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        supervisor.renderer = renderer(&fake);
        for _ in 0..=crate::generation::RESTART_LIMIT {
            assert!(!supervisor.replace_departed_renderer(died()).await);
            guard.track(&supervisor.authoritative.child);
        }
        assert!(supervisor.respawn_at.is_some(), "the brake holds the next spawn back");
        supervisor.request_lock();
        assert_eq!((supervisor.next_lock_deadline(), supervisor.lock_wanted), (None, true), "the brake's cooldown");

        let (mut supervisor, _started, _open) = booted(&bus, dir.path()).await;
        supervisor.renderer = renderer(&dir.path().join("missing"));
        assert!(!supervisor.replace_departed_renderer(died()).await);
        assert!(supervisor.respawn_at.is_some(), "the spawn failed");
        supervisor.request_lock();
        assert_eq!(supervisor.next_lock_deadline(), None);
        supervisor.renderer = renderer(&fake);
        assert!(!supervisor.respawn_renderer().await);
        guard.track(&supervisor.authoritative.child);
        let mut frames = up(&mut supervisor);
        assert_eq!(sent(&mut frames), [(true, 1)]);
    }
}

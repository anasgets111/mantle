use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;

use super::state::{LockEvent, LockState, accepts_outcome, apply, may_authenticate, releases};

/// Owns [`LockState`] and the outbound `SetSessionLock` queue. Not `Clone`: `main.rs` mutates it
/// inline in `select!` (unlike `KeyboardController`).
pub struct LockController {
    /// Shared so a scheduled release can re-read the acquisition it was scheduled for; see
    /// [`LockController::unlock_after_animation`].
    state: Arc<Mutex<LockState>>,
    commands_tx: UnboundedSender<shared::SetSessionLock>,
    /// Milliseconds [`LockController::unlock_after_animation`] waits before releasing, clamped to
    /// [`MAX_UNLOCK_ANIMATION`] by the setter. Zero, and so unchanged from before ADR-0190, until a
    /// config sets it. An atomic rather than a second mutex: it is written once at config load and
    /// read once per unlock, and it has no invariant tying it to the lock state beside it.
    unlock_animation_ms: AtomicU64,
}

impl LockController {
    /// `commands_tx` returns commands to `main.rs`, the only holder of the authoritative generation
    /// id, which a swap reassigns.
    pub fn new(commands_tx: UnboundedSender<shared::SetSessionLock>) -> Self {
        Self { state: Arc::new(Mutex::new(LockState::default())), commands_tx, unlock_animation_ms: AtomicU64::new(0) }
    }

    /// Sends `lock()`, unless a lock is already active. Renderer answers `Nothing` for
    /// `(locked: true, lock_held: true)` without `LockReport`, so recording that request would shut
    /// the swap gate forever.
    ///
    /// ponytail: `active` can lag Renderer by one socket hop while `Finished` is in flight, so a
    /// new request can be dropped.
    /// Upgrade by reporting `Nothing` as a `LockOutcome`, giving every request an
    /// event and removing this guard.
    pub fn lock(&self) {
        {
            let mut state = self.state.lock().unwrap();
            if state.active {
                debug!("a lock is already held; dropping a lock() that cannot change anything");
                return;
            }
            apply(&mut state, LockEvent::LockRequested);
        }
        self.send(shared::SetSessionLock { locked: true });
    }

    /// Orders unlock only from `main.rs`'s `secure_submit(lock, authenticate)` arm after
    /// `PamOutcome::Success`; it is absent from [`dispatch`] so ADR-0042 is checkable in one arm.
    /// Records no event: Renderer `Unlocked` clears `active`.
    pub fn unlock(&self) {
        self.send(shared::SetSessionLock { locked: false });
    }

    /// Records how long a lock stays up after a correct password, clamped to
    /// [`MAX_UNLOCK_ANIMATION`] (ADR-0190).
    pub fn set_unlock_animation(&self, window: Duration) {
        let clamped = window.min(MAX_UNLOCK_ANIMATION);
        self.unlock_animation_ms.store(clamped.as_millis() as u64, Ordering::Relaxed);
    }

    /// What [`LockController::set_unlock_animation`] last accepted.
    pub fn unlock_animation(&self) -> Duration {
        Duration::from_millis(self.unlock_animation_ms.load(Ordering::Relaxed))
    }

    /// Releases the lock the password answered, after the configured window if there is one.
    ///
    /// **The release names its acquisition.** `record_authentication` checks that a PAM answer
    /// belongs to the lock on the glass (`accepts_outcome`), but a *delayed* release outlives that
    /// check: a lock can end and another begin while the timer sleeps, and an unconditional unlock
    /// would then release a lock nobody authenticated for -- the ADR-0042 bypass, arrived at by
    /// waiting instead of by a Lua action. So the acquisition is captured here and re-read when the
    /// timer fires; if it has moved on, the lock this was authorized to release is already gone and
    /// there is nothing to do.
    ///
    /// A poisoned state lock refuses to send. It cannot tell which lock it would be releasing, and
    /// a wrong release is worse than a late one -- the panic that poisoned it will already have
    /// taken this process down, which leaves the compositor holding the lock either way.
    ///
    /// ponytail: a runtime shutdown inside the window drops the sleeping task and the release with
    /// it, leaving the compositor holding a lock the user has already answered. The window is at
    /// most [`MAX_UNLOCK_ANIMATION`], so this is a shutdown landing in a 600ms hole, but it is a
    /// real way to be locked out. Upgrade path: hold the pending release in `main.rs`'s loop, which
    /// can flush it on the way down; that is where the generation id already lives.
    pub fn unlock_after_animation(&self) {
        let window = self.unlock_animation();
        if window.is_zero() {
            self.unlock();
            return;
        }
        let Ok(acquisition) = self.state.lock().map(|state| state.acquisition) else {
            // Nothing to name the release with, so take the immediate one rather than a blind one.
            self.unlock();
            return;
        };
        let state = Arc::clone(&self.state);
        let commands_tx = self.commands_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(window).await;
            let release = match state.lock() {
                Ok(state) => releases(&state, acquisition),
                Err(_) => {
                    error!("the lock state is poisoned; refusing to release a lock it cannot identify");
                    false
                }
            };
            if !release {
                return;
            }
            if commands_tx.send(shared::SetSessionLock { locked: false }).is_err() {
                warn!("the command channel is closed; the unlock after its animation was dropped");
            }
        });
    }

    pub fn record(&self, event: LockEvent) {
        apply(&mut self.state.lock().unwrap(), event);
    }

    pub fn snapshot(&self) -> LockState {
        self.state.lock().unwrap().clone()
    }

    /// Atomically admits and marks one PAM conversation, or refuses. `main.rs` spawns it, so a
    /// getter plus record would race. `Some` carries the acquisition for
    /// [`Self::record_authentication`], the only moment the worker's lock number is known.
    pub fn try_begin_authentication(&self) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        if !may_authenticate(&state) {
            return None;
        }
        apply(&mut state, LockEvent::AuthenticationStarted);
        Some(state.acquisition)
    }

    /// Applies a PAM answer to its acquisition or refuses it. `main.rs`'s `pam_outcomes` arm then
    /// avoids unlocking a lock that no longer exists (see [`accepts_outcome`]).
    pub fn record_authentication(&self, acquisition: u64, outcome: shared::PamOutcome) -> bool {
        let mut state = self.state.lock().unwrap();
        if !accepts_outcome(&state, acquisition) {
            return false;
        }
        apply(&mut state, LockEvent::Authenticated(outcome));
        true
    }

    /// A closed channel means `main.rs`'s loop is gone; log and drop.
    fn send(&self, command: shared::SetSessionLock) {
        if self.commands_tx.send(command).is_err() {
            warn!("the command channel is closed; dropping {command:?}");
        }
    }
}

/// Every action `mantle.lock:invoke(...)` accepts. There is no `unlock`: a lock screen's Lua button
/// callback would make it a one-click path past PAM, forbidden by ADR-0042. Unknown `"unlock"` is
/// logged and dropped.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LockAction {
    /// Locks the session.
    Lock,
    /// Holds the lock open after PAM says yes, for an animation out (ADR-0190). Clamped to
    /// [`MAX_UNLOCK_ANIMATION`]; no `ms` is no animation.
    SetUnlockAnimation {
        #[serde(default)]
        ms: Option<u64>,
    },
}

/// The longest a lock may stay up after a correct password.
///
/// A ceiling and not a preference: this window is time the user has authenticated and is still
/// looking at a lock screen, and a config that asked for ten seconds of it -- by typo, or by
/// deriving the number from something that went wrong -- would be indistinguishable from a shell
/// that has hung. 600ms is comfortably past anything that reads as an animation rather than a
/// fault.
pub const MAX_UNLOCK_ANIMATION: Duration = Duration::from_millis(600);

/// `mantle.lock` action dispatch (ADR-0037).
pub fn dispatch(controller: &LockController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<LockAction>(&envelope.params) else { return };
    match action {
        LockAction::Lock => controller.lock(),
        LockAction::SetUnlockAnimation { ms } => {
            controller.set_unlock_animation(Duration::from_millis(ms.unwrap_or_default()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0190. The window is time the user has authenticated and is still looking at a lock
    /// screen, so the ceiling is the engine's and not the config's.
    #[test]
    fn the_unlock_animation_is_clamped_to_the_engines_ceiling() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);
        assert_eq!(controller.unlock_animation(), Duration::ZERO, "no animation until a config asks");

        controller.set_unlock_animation(Duration::from_millis(300));
        assert_eq!(controller.unlock_animation(), Duration::from_millis(300), "an ordinary window is taken");

        controller.set_unlock_animation(Duration::from_secs(10));
        assert_eq!(controller.unlock_animation(), MAX_UNLOCK_ANIMATION, "a config cannot hold an answered lock open");

        controller.set_unlock_animation(Duration::ZERO);
        assert_eq!(controller.unlock_animation(), Duration::ZERO, "and it can put the window back");
    }

    #[test]
    fn lock_queues_the_set_session_lock_command_and_records_the_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        controller.lock();
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));
        assert!(!controller.snapshot().active, "the lock is not active until the Renderer reports Locked");

        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(controller.snapshot().active);

        // main.rs's PAM-outcome arm is this method's only caller.
        controller.unlock();
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: false }));
        assert!(
            controller.snapshot().active,
            "active clears on the Renderer's Unlocked report, not on the order going out"
        );
    }

    #[test]
    fn try_begin_authentication_admits_exactly_one_attempt_at_a_time() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        assert!(
            controller.try_begin_authentication().is_none(),
            "no lock is held, so there is nothing to authenticate against"
        );
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let acquisition = controller.try_begin_authentication().expect("a held lock admits the first attempt");
        assert!(controller.snapshot().authenticating);
        assert!(
            controller.try_begin_authentication().is_none(),
            "the second submission must find the first still in flight"
        );

        assert!(controller.record_authentication(acquisition, shared::PamOutcome::AuthFailed));
        assert!(controller.try_begin_authentication().is_some(), "the worker's answer released it");
    }

    #[test]
    fn a_stale_failure_neither_counts_against_the_new_lock_nor_overwrites_its_message() {
        // The same binding, on the other outcome: a stale failure landing after the new
        // acquisition's reset would show the user a failure they never made.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let stale = controller.try_begin_authentication().expect("a held lock admits the attempt");
        controller.record(LockEvent::Reported(shared::LockOutcome::Finished));
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));

        assert!(
            !controller.record_authentication(stale, shared::PamOutcome::AuthFailed),
            "the refusal is reported so main.rs can log it"
        );

        let state = controller.snapshot();
        assert_eq!(state.attempts, 0, "the new lock has seen no attempts");
        assert_eq!(state.error, "", "and nothing to say about one");
    }

    #[test]
    fn a_second_lock_while_one_is_held_is_dropped_rather_than_left_unresolved() {
        // The Renderer answers Nothing to a locked:true it already holds, with no LockReport --
        // so a requested set here would have no event to clear it.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        controller.lock();
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        controller.lock();

        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));
        assert!(rx.try_recv().is_err(), "the second lock() cannot change anything, so it is not sent either");
        assert!(
            !controller.snapshot().requested,
            "and above all it does not shut the swap gate on an event that is never coming"
        );
    }

    #[test]
    fn dispatch_routes_lock_and_ignores_an_unknown_action() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        for action in ["lock", "nonsense"] {
            dispatch(&controller, &envelope(action));
        }

        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));
        assert!(rx.try_recv().is_err(), "an unknown action must be logged, not turned into a command");
    }

    #[test]
    fn dispatch_refuses_to_unlock() {
        // ADR-0042: an unlock action would make walking past PAM one mouse click. The
        // asymmetry with lock is deliberate, pinned so it isn't re-added as an oversight.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        dispatch(&controller, &envelope("unlock"));

        assert!(rx.try_recv().is_err(), "unlock must not be commandable from Lua");
    }

    fn envelope(action: &str) -> shared::CommandEnvelope {
        shared::CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "command".to_string(),
            params: shared::CommandParams {
                generation_id: 0,
                capability: "lock".to_string(),
                action: action.to_string(),
                arguments: Vec::new(),
                expected_revision: 0,
            },
            id: 1,
        }
    }
}

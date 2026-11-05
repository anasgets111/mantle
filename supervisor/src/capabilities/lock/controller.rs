use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared::state::lock::{FingerprintState, PamNotice, PamPrompt};
use shared::{debug, error, info, warn};
use tokio::sync::mpsc::UnboundedSender;

use super::state::{LockEvent, LockState, apply, may_authenticate, releases};
use crate::capabilities::conversation::{Dialogue, Heard, PamFields};
use crate::pam_worker::Conversation;
use shared::action::LockAction;

/// Owns [`LockState`] and the outbound `SetSessionLock` queue. Not `Clone`: `main.rs` mutates it
/// inline in `select!` (unlike `KeyboardController`).
pub struct LockController {
    /// Shared so a scheduled release can re-read the acquisition it was scheduled for; see
    /// [`LockController::unlock_after_animation`].
    state: Arc<Mutex<LockState>>,
    commands_tx: UnboundedSender<shared::SetSessionLock>,
    /// Milliseconds [`LockController::unlock_after_animation`] waits before releasing, clamped to
    /// [`MAX_UNLOCK_ANIMATION`] by the setter. Zero until a config sets it. An atomic rather than a
    /// second mutex: it is written once at config load and read once per unlock, and it has no
    /// invariant tying it to the lock state beside it.
    unlock_animation_ms: AtomicU64,
    /// The lock's PAM conversation (ADR-0381). Locked after `state` wherever both are held.
    dialogue: Mutex<Dialogue>,
    /// The fingerprint conversation beside it (ADR-0382). Locked after `dialogue`.
    fingerprint: Mutex<Dialogue>,
}

/// What the fingerprint dialogue writes: its lines into `mantle.lock.fingerprint`, a prompt and the
/// answer flag into scratch, since nobody can answer a fingerprint stack's question.
struct Scan<'a>(Option<PamPrompt>, &'a mut Vec<PamNotice>, bool);

impl PamFields for Scan<'_> {
    fn pam(&mut self) -> (&mut Option<PamPrompt>, &mut Vec<PamNotice>, &mut bool) {
        (&mut self.0, &mut *self.1, &mut self.2)
    }
}

/// Ends the fingerprint conversation; `mantle.lock.fingerprint` turns nil.
fn stop_fingerprint(state: &mut LockState, fingerprint: &mut Dialogue) {
    fingerprint.stop(&mut Scan(None, &mut Vec::new(), false));
    state.fingerprint = None;
}

impl LockController {
    /// `commands_tx` returns commands to `main.rs`, the only holder of the authoritative generation
    /// id, which a swap reassigns.
    pub fn new(commands_tx: UnboundedSender<shared::SetSessionLock>) -> Self {
        let mut dialogue = Dialogue::default();
        dialogue.info_refuses = true;
        Self {
            state: Arc::new(Mutex::new(LockState::default())),
            commands_tx,
            unlock_animation_ms: AtomicU64::new(0),
            dialogue: Mutex::new(dialogue),
            fingerprint: Mutex::default(),
        }
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
            let mut state = self.state.lock().expect("mutex poisoned");
            if state.active {
                debug!("a lock is already held; dropping a lock() that cannot change anything");
                return;
            }
            apply(&mut state, LockEvent::LockRequested);
        }
        self.send(shared::SetSessionLock { locked: true });
    }

    /// Orders unlock only from `main.rs`'s PAM conversation arm after
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

    /// Releases the lock the password answered, after the configured window if there is one. With or
    /// without a window it refuses unless a recorded password answered the current acquisition.
    ///
    /// **The release names its acquisition.** `hear` checks that a PAM answer
    /// belongs to the conversation of the lock on the glass, but a *delayed* release outlives that
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
        let acquisition = match self.state.lock() {
            Err(_) => {
                error!("the lock state is poisoned; refusing to release a lock it cannot identify");
                return;
            }
            Ok(state) if !releases(&state, state.acquisition) => {
                warn!("refusing to release a lock no recorded password answered");
                return;
            }
            Ok(state) => state.acquisition,
        };
        if window.is_zero() {
            self.unlock();
            return;
        }
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

    /// Applies `event`. A new lock or a lock ending ends the running conversation, so nothing it
    /// says later can answer for the lock on the glass.
    pub fn record(&self, event: LockEvent) {
        let mut state = self.state.lock().expect("mutex poisoned");
        let relock = event == LockEvent::Reported(shared::LockOutcome::Locked);
        apply(&mut state, event);
        if relock || !state.active {
            self.dialogue.lock().expect("mutex poisoned").stop(&mut *state);
            stop_fingerprint(&mut state, &mut self.fingerprint.lock().expect("mutex poisoned"));
        }
    }

    pub fn snapshot(&self) -> LockState {
        self.state.lock().expect("mutex poisoned").clone()
    }

    /// Starts PAM for the lock on the glass unless it already runs (ADR-0381): after `Locked`,
    /// and after a failure that showed a prompt.
    pub fn converse(&self, start: impl FnOnce(u64) -> Conversation) {
        let state = self.state.lock().expect("mutex poisoned");
        let mut dialogue = self.dialogue.lock().expect("mutex poisoned");
        if state.active && !state.unlocking && !dialogue.is_live() {
            dialogue.start(start);
        }
    }

    /// Starts the fingerprint conversation unless it already runs: after `Locked` and after a
    /// password submit, never on its own after it ends (ADR-0382).
    pub fn listen(&self, start: impl FnOnce(u64) -> Conversation) {
        let mut state = self.state.lock().expect("mutex poisoned");
        let mut fingerprint = self.fingerprint.lock().expect("mutex poisoned");
        if state.active && !state.unlocking && !fingerprint.is_live() {
            fingerprint.start(start);
            state.fingerprint = Some(FingerprintState::default());
        }
    }

    /// `secure_submit(lock, authenticate)`: answers PAM's prompt, or holds the answer until one
    /// comes, starting a conversation if none runs. Refused with no lock held, while unlocking, or
    /// while an answer is already with PAM.
    pub fn submit(&self, secret: shared::Zeroizing<Vec<u8>>, start: impl FnOnce(u64) -> Conversation) -> bool {
        let mut state = self.state.lock().expect("mutex poisoned");
        if !may_authenticate(&state) {
            return false;
        }
        self.dialogue.lock().expect("mutex poisoned").submit(&mut *state, secret, start)
    }

    /// Applies one frame of conversation `serial`. The first `Success` unlocks and stops the other
    /// conversation. A password verdict also counts or clears the attempt; a fingerprint that ends
    /// any other way, or asks a question, only turns fingerprint off (ADR-0382).
    pub fn hear(&self, serial: u64, message: shared::PamMessage) -> Heard {
        let mut state = self.state.lock().expect("mutex poisoned");
        let mut dialogue = self.dialogue.lock().expect("mutex poisoned");
        let mut fingerprint = self.fingerprint.lock().expect("mutex poisoned");
        if !fingerprint.runs(serial) {
            let heard = dialogue.hear(&mut *state, serial, message);
            if let Heard::Ended { outcome, .. } = &heard {
                if *outcome == shared::PamOutcome::Success {
                    stop_fingerprint(&mut state, &mut fingerprint);
                }
                apply(&mut state, LockEvent::Authenticated(outcome.clone()));
            }
            return heard;
        }
        let mut scan = Scan(None, &mut state.fingerprint.get_or_insert_default().messages, false);
        let heard = fingerprint.hear(&mut scan, serial, message);
        if let Some(asked) = scan.0.take() {
            warn!(
                "the mantle-fingerprint PAM stack asked {:?}, which nobody can answer; fingerprint is off until a password submit or the next lock",
                asked.text
            );
            stop_fingerprint(&mut state, &mut fingerprint);
            return Heard::Changed;
        }
        match heard {
            Heard::Ended { outcome: shared::PamOutcome::Success, .. } => {
                dialogue.stop(&mut *state);
                state.fingerprint = None;
                apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
                heard
            }
            Heard::Ended { outcome, .. } => {
                info!(
                    "fingerprint conversation {serial} ended {outcome:?}; off until a password submit or the next lock"
                );
                state.fingerprint = None;
                Heard::Changed
            }
            heard => heard,
        }
    }

    /// A closed channel means `main.rs`'s loop is gone; log and drop.
    fn send(&self, command: shared::SetSessionLock) {
        if self.commands_tx.send(command).is_err() {
            warn!("the command channel is closed; dropping {command:?}");
        }
    }
}

/// The longest a lock may stay up after a correct password.
///
/// A ceiling, not a preference: the user has authenticated and still sees a lock screen, so a
/// config asking for ten seconds of it would look like a hung shell. 600ms is past any animation.
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

    /// Starts a detached conversation and records its serial and answer receiver.
    fn start_into(
        slot: &mut Option<(u64, tokio::sync::mpsc::UnboundedReceiver<shared::Zeroizing<Vec<u8>>>)>,
    ) -> impl FnOnce(u64) -> Conversation + '_ {
        move |serial| {
            let (conversation, answers) = Conversation::detached();
            *slot = Some((serial, answers));
            conversation
        }
    }

    fn prompt() -> shared::PamMessage {
        shared::PamMessage::Prompt { text: "Password:".into(), echo: false }
    }

    fn secret() -> shared::Zeroizing<Vec<u8>> {
        shared::Zeroizing::new(b"hunter2".to_vec())
    }

    /// A held lock whose conversation showed a prompt, and that conversation's serial.
    fn locked() -> (LockController, tokio::sync::mpsc::UnboundedReceiver<shared::SetSessionLock>, u64) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let mut started = None;
        controller.converse(start_into(&mut started));
        let (serial, _) = started.expect("a confirmed lock starts PAM");
        assert_eq!(controller.hear(serial, prompt()), Heard::Changed);
        (controller, rx, serial)
    }

    #[test]
    fn pam_starts_only_for_a_held_lock_and_one_answer_is_with_it_at_a_time() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);
        let mut started = None;
        controller.converse(start_into(&mut started));
        assert!(!controller.submit(secret(), start_into(&mut started)), "no lock is held");
        assert!(started.is_none(), "so no worker starts either");

        let (controller, _rx, serial) = locked();
        let mut again = None;
        controller.converse(start_into(&mut again));
        assert!(again.is_none(), "one conversation per lock");
        assert_eq!(controller.snapshot().prompt.map(|p| p.text), Some("Password:".to_string()));
        assert!(controller.submit(secret(), start_into(&mut again)));
        assert!(controller.snapshot().authenticating);
        assert!(!controller.submit(secret(), start_into(&mut again)), "the second Enter finds the first with PAM");

        let failed = shared::PamMessage::Outcome(shared::PamOutcome::AuthFailed);
        assert_eq!(
            controller.hear(serial, failed),
            Heard::Ended { outcome: shared::PamOutcome::AuthFailed, prompted: true }
        );
        let state = controller.snapshot();
        assert_eq!((state.attempts, state.error.as_str(), state.authenticating), (1, "authentication failed", false));
        controller.converse(start_into(&mut again));
        assert!(again.is_some(), "the caller restarts PAM for the next attempt");
    }

    /// Stock pam_faillock's lockout is an info line, so on the lock one before the prompt makes the
    /// next submit restart PAM (ADR-0381).
    #[test]
    fn an_early_info_line_on_the_lock_restarts_pam_on_submit() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let mut started = None;
        controller.converse(start_into(&mut started));
        let (serial, _) = started.expect("a confirmed lock starts PAM");
        let locked = shared::PamMessage::Message { kind: shared::PamNoticeKind::Info, text: "locked".into() };
        controller.hear(serial, locked);
        controller.hear(serial, prompt());
        let mut again = None;
        assert!(controller.submit(secret(), start_into(&mut again)));
        assert!(again.is_some(), "the refused worker was replaced");
    }

    /// The ADR-0042 bypass, in order: a worker started against lock N answers after the compositor
    /// ended lock N and lock N+1 took the glass. Its verdict, right or wrong, applies to nothing.
    #[test]
    fn a_stale_verdict_neither_releases_nor_counts_against_the_lock_that_replaced_its_own() {
        for outcome in [shared::PamOutcome::Success, shared::PamOutcome::AuthFailed] {
            let (controller, mut rx, stale) = locked();
            controller.record(LockEvent::Reported(shared::LockOutcome::Finished));
            assert_eq!(controller.snapshot().prompt, None, "the lock's prompt left with it");
            controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
            assert_eq!(controller.hear(stale, shared::PamMessage::Outcome(outcome)), Heard::Stale);
            let state = controller.snapshot();
            assert!(!state.unlocking && state.attempts == 0 && state.error.is_empty(), "{state:?}");
            controller.unlock_after_animation();
            assert!(rx.try_recv().is_err(), "nobody authenticated for the lock on the glass");
        }
    }

    /// A lock confirmed again without ending (a respawned Renderer retaking it) is a new lock: the
    /// old conversation ends with it, and only a new one starts.
    #[test]
    fn a_second_confirmed_lock_ends_the_first_lock_s_conversation() {
        let (controller, _rx, stale) = locked();
        controller.record(LockEvent::LockRequested);
        assert!(controller.snapshot().prompt.is_some(), "a request alone ends nothing");
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        assert_eq!(controller.snapshot().prompt, None);
        assert_eq!(controller.hear(stale, prompt()), Heard::Stale);
        let mut started = None;
        controller.converse(start_into(&mut started));
        assert!(started.is_some_and(|(serial, _)| serial != stale));
    }

    /// [`locked`] with a fingerprint conversation beside the password's, and its serial.
    fn scanning() -> (LockController, tokio::sync::mpsc::UnboundedReceiver<shared::SetSessionLock>, u64, u64) {
        let (controller, rx, password) = locked();
        let mut started = None;
        controller.listen(start_into(&mut started));
        let (fingerprint, _) = started.expect("a held lock starts fingerprint");
        assert_eq!(controller.snapshot().fingerprint, Some(FingerprintState::default()));
        (controller, rx, password, fingerprint)
    }

    fn outcome(outcome: shared::PamOutcome) -> shared::PamMessage {
        shared::PamMessage::Outcome(outcome)
    }

    /// ADR-0382: either conversation's first `Success` unlocks and ends the other, whose late
    /// verdict, right or wrong, applies to nothing.
    #[test]
    fn the_first_success_unlocks_and_the_loser_is_stale() {
        let (controller, mut rx, password, fingerprint) = scanning();
        let heard = controller.hear(fingerprint, outcome(shared::PamOutcome::Success));
        assert_eq!(heard, Heard::Ended { outcome: shared::PamOutcome::Success, prompted: false });
        let state = controller.snapshot();
        assert!(state.unlocking && state.prompt.is_none() && state.fingerprint.is_none(), "{state:?}");
        assert_eq!(controller.hear(password, outcome(shared::PamOutcome::AuthFailed)), Heard::Stale);
        assert_eq!(controller.snapshot().attempts, 0, "the stopped password counts nothing");
        controller.unlock_after_animation();
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: false }));

        let (controller, _rx, password, fingerprint) = scanning();
        assert!(controller.submit(secret(), |_| unreachable!("the conversation is running")));
        assert!(matches!(controller.hear(password, outcome(shared::PamOutcome::Success)), Heard::Ended { .. }));
        assert_eq!(controller.snapshot().fingerprint, None, "the password's match stops the scan");
        assert_eq!(controller.hear(fingerprint, outcome(shared::PamOutcome::Success)), Heard::Stale);
    }

    /// A fingerprint that ends without a match is not a wrong password and does not restart itself:
    /// only `listen`, after a submit or a new lock, brings it back.
    #[test]
    fn a_failed_fingerprint_counts_nothing_and_stays_off_until_asked_again() {
        let (controller, _rx, _password, fingerprint) = scanning();
        let missed = shared::PamMessage::Message { kind: shared::PamNoticeKind::Info, text: "Failed to match".into() };
        assert_eq!(controller.hear(fingerprint, missed), Heard::Changed);
        let state = controller.snapshot();
        assert_eq!(state.fingerprint.map(|f| f.messages.len()), Some(1));
        assert!(state.messages.is_empty(), "the password's lines are its own");
        assert_eq!(
            controller.hear(fingerprint, outcome(shared::PamOutcome::AuthFailed)),
            Heard::Changed,
            "not Ended, so the caller restarts nothing"
        );
        let state = controller.snapshot();
        assert_eq!((state.attempts, state.error.as_str(), state.fingerprint), (0, "", None));
        assert!(state.prompt.is_some(), "the password conversation runs on");

        let mut again = None;
        controller.listen(start_into(&mut again));
        let (restarted, _) = again.take().expect("a submit starts it again");
        controller.listen(start_into(&mut again));
        assert!(again.is_none(), "one fingerprint conversation at a time");
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        assert_eq!(controller.snapshot().fingerprint, None, "a relock ends it");
        assert_eq!(controller.hear(restarted, outcome(shared::PamOutcome::Success)), Heard::Stale);
        controller.listen(start_into(&mut again));
        assert!(again.is_some(), "the next lock starts it again");
    }

    /// A fingerprint stack that asks a question is misconfigured: nobody can type at a scan.
    #[test]
    fn a_fingerprint_prompt_stops_it_and_never_reaches_the_lock_screen() {
        let (controller, _rx, _password, fingerprint) = scanning();
        let asked = shared::PamMessage::Prompt { text: "PIN:".into(), echo: false };
        assert_eq!(controller.hear(fingerprint, asked), Heard::Changed);
        let state = controller.snapshot();
        assert_eq!(state.prompt.map(|p| p.text), Some("Password:".to_string()), "still the password's");
        assert_eq!(state.fingerprint, None);
        assert_eq!(controller.hear(fingerprint, outcome(shared::PamOutcome::Success)), Heard::Stale);
    }

    /// A held lock with one attempt answered `outcome`.
    fn answered(
        outcome: shared::PamOutcome,
    ) -> (LockController, tokio::sync::mpsc::UnboundedReceiver<shared::SetSessionLock>) {
        let (controller, rx, serial) = locked();
        assert!(controller.submit(secret(), |_| unreachable!("the conversation is running")));
        assert!(matches!(controller.hear(serial, shared::PamMessage::Outcome(outcome)), Heard::Ended { .. }));
        (controller, rx)
    }

    #[test]
    fn with_no_window_only_a_correct_password_releases_the_lock() {
        let (controller, mut rx) = answered(shared::PamOutcome::AuthFailed);
        controller.unlock_after_animation();
        assert!(rx.try_recv().is_err(), "a wrong password must not unlock, whoever asks");

        let (controller, mut rx) = answered(shared::PamOutcome::Success);
        controller.unlock_after_animation();
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: false }));
        assert!(rx.try_recv().is_err(), "one release per answer");
    }

    #[tokio::test(start_paused = true)]
    async fn a_windowed_release_fires_after_the_window_only_for_the_lock_it_answered() {
        let window = Duration::from_millis(300);
        let (controller, mut rx) = answered(shared::PamOutcome::Success);
        controller.set_unlock_animation(window);
        controller.unlock_after_animation();
        tokio::time::sleep(window / 2).await;
        assert!(rx.try_recv().is_err(), "the lock stays up through its out-animation");
        tokio::time::sleep(window).await;
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: false }));

        let (controller, mut rx) = answered(shared::PamOutcome::Success);
        controller.set_unlock_animation(window);
        controller.unlock_after_animation();
        controller.record(LockEvent::Reported(shared::LockOutcome::Finished));
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        tokio::time::sleep(window * 2).await;
        assert!(rx.try_recv().is_err(), "a stale timer must not release a lock nobody authenticated (ADR-0042)");
    }

    #[tokio::test(start_paused = true)]
    async fn a_windowed_wrong_password_never_releases() {
        let window = Duration::from_millis(300);
        let (controller, mut rx) = answered(shared::PamOutcome::AuthFailed);
        controller.set_unlock_animation(window);
        controller.unlock_after_animation();
        tokio::time::sleep(window * 2).await;
        assert!(rx.try_recv().is_err(), "a wrong password must not unlock after the window either");
    }

    #[test]
    fn a_poisoned_state_refuses_to_release() {
        let (controller, mut rx) = answered(shared::PamOutcome::Success);
        std::thread::scope(|scope| {
            let poisoner = scope.spawn(|| {
                let _held = controller.state.lock().expect("not yet poisoned");
                panic!("poison the lock state");
            });
            assert!(poisoner.join().is_err());
        });
        controller.unlock_after_animation();
        assert!(rx.try_recv().is_err(), "a lock that cannot be identified is not released");
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
    fn dispatch_routes_lock_ignores_an_unknown_action_and_refuses_to_unlock() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = LockController::new(tx);

        dispatch(&controller, &envelope("lock"));
        assert_eq!(rx.try_recv().ok(), Some(shared::SetSessionLock { locked: true }));

        dispatch(&controller, &envelope("nonsense"));
        assert!(rx.try_recv().is_err(), "an unknown action must be logged, not turned into a command");

        // ADR-0042: an unlock action would make walking past PAM one mouse click. The
        // asymmetry with lock is deliberate, pinned so it isn't re-added as an oversight.
        dispatch(&controller, &envelope("unlock"));
        assert!(rx.try_recv().is_err(), "unlock must not be commandable from Lua");
    }

    fn envelope(action: &str) -> shared::CommandEnvelope {
        shared::CommandEnvelope {
            params: shared::CommandParams {
                generation_id: 0,
                capability: "lock".to_string(),
                action: action.to_string(),
                arguments: Vec::new(),
                legacy_expected_revision: 0,
            },
            id: 1,
        }
    }
}

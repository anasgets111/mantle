use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use shared::state::conversation::{PamNotice, PamPrompt};
use shared::state::lock::FingerprintState;
use shared::{info, warn};

use super::state::{LockEvent, LockState, apply, may_authenticate};
use crate::capabilities::conversation::{Dialogue, Heard, PamFields};
use crate::pam_worker::Conversation;
use shared::action::LockAction;

/// Owns [`LockState`], the `mantle.lock` payload, and the PAM conversations behind it. Which lock
/// is on the glass, and every `SetSessionLock`, is `Supervisor`'s.
#[derive(Default)]
pub struct LockController {
    state: Mutex<LockState>,
    /// Milliseconds the Supervisor waits after a correct password before releasing, clamped to
    /// [`MAX_UNLOCK_ANIMATION`] by the setter. Zero until a config sets it.
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
    pub fn new() -> Self {
        let mut dialogue = Dialogue::default();
        dialogue.info_refuses = true;
        Self { dialogue: Mutex::new(dialogue), ..Self::default() }
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
}

/// The longest a lock may stay up after a correct password.
///
/// A ceiling, not a preference: the user has authenticated and still sees a lock screen, so a
/// config asking for ten seconds of it would look like a hung shell. 600ms is past any animation.
pub const MAX_UNLOCK_ANIMATION: Duration = Duration::from_millis(600);

/// `mantle.lock` action dispatch (ADR-0037); `true` for `lock()`, which the Supervisor answers.
pub fn dispatch(controller: &LockController, envelope: &shared::CommandEnvelope) -> bool {
    let Some(action) = crate::parse_action::<LockAction>(&envelope.params) else { return false };
    match action {
        LockAction::Lock => return true,
        LockAction::SetUnlockAnimation { ms } => {
            controller.set_unlock_animation(Duration::from_millis(ms.unwrap_or_default()))
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::envelope;

    /// ADR-0190. The window is time the user has authenticated and is still looking at a lock
    /// screen, so the ceiling is the engine's and not the config's.
    #[test]
    fn the_unlock_animation_is_clamped_to_the_engines_ceiling() {
        let controller = LockController::new();
        assert_eq!(controller.unlock_animation(), Duration::ZERO, "no animation until a config asks");

        controller.set_unlock_animation(Duration::from_millis(300));
        assert_eq!(controller.unlock_animation(), Duration::from_millis(300), "an ordinary window is taken");

        controller.set_unlock_animation(Duration::from_secs(10));
        assert_eq!(controller.unlock_animation(), MAX_UNLOCK_ANIMATION, "a config cannot hold an answered lock open");

        controller.set_unlock_animation(Duration::ZERO);
        assert_eq!(controller.unlock_animation(), Duration::ZERO, "and it can put the window back");
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
    fn locked() -> (LockController, u64) {
        let controller = LockController::new();
        controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
        let mut started = None;
        controller.converse(start_into(&mut started));
        let (serial, _) = started.expect("a confirmed lock starts PAM");
        assert_eq!(controller.hear(serial, prompt()), Heard::Changed);
        (controller, serial)
    }

    #[test]
    fn pam_starts_only_for_a_held_lock_and_one_answer_is_with_it_at_a_time() {
        let controller = LockController::new();
        let mut started = None;
        controller.converse(start_into(&mut started));
        assert!(!controller.submit(secret(), start_into(&mut started)), "no lock is held");
        assert!(started.is_none(), "so no worker starts either");

        let (controller, serial) = locked();
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
        let controller = LockController::new();
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
            let (controller, stale) = locked();
            controller.record(LockEvent::Reported(shared::LockOutcome::Finished));
            assert_eq!(controller.snapshot().prompt, None, "the lock's prompt left with it");
            controller.record(LockEvent::Reported(shared::LockOutcome::Locked));
            assert_eq!(controller.hear(stale, shared::PamMessage::Outcome(outcome)), Heard::Stale);
            let state = controller.snapshot();
            assert!(!state.unlocking && state.attempts == 0 && state.error.is_empty(), "{state:?}");
        }
    }

    /// A lock confirmed again without ending (a respawned Renderer retaking it) is a new lock: the
    /// old conversation ends with it, and only a new one starts.
    #[test]
    fn a_second_confirmed_lock_ends_the_first_lock_s_conversation() {
        let (controller, stale) = locked();
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
    fn scanning() -> (LockController, u64, u64) {
        let (controller, password) = locked();
        let mut started = None;
        controller.listen(start_into(&mut started));
        let (fingerprint, _) = started.expect("a held lock starts fingerprint");
        assert_eq!(controller.snapshot().fingerprint, Some(FingerprintState::default()));
        (controller, password, fingerprint)
    }

    fn outcome(outcome: shared::PamOutcome) -> shared::PamMessage {
        shared::PamMessage::Outcome(outcome)
    }

    /// ADR-0382: either conversation's first `Success` unlocks and ends the other, whose late
    /// verdict, right or wrong, applies to nothing.
    #[test]
    fn the_first_success_unlocks_and_the_loser_is_stale() {
        let (controller, password, fingerprint) = scanning();
        let heard = controller.hear(fingerprint, outcome(shared::PamOutcome::Success));
        assert_eq!(heard, Heard::Ended { outcome: shared::PamOutcome::Success, prompted: false });
        let state = controller.snapshot();
        assert!(state.unlocking && state.prompt.is_none() && state.fingerprint.is_none(), "{state:?}");
        assert_eq!(controller.hear(password, outcome(shared::PamOutcome::AuthFailed)), Heard::Stale);
        assert_eq!(controller.snapshot().attempts, 0, "the stopped password counts nothing");

        let (controller, password, fingerprint) = scanning();
        assert!(controller.submit(secret(), |_| unreachable!("the conversation is running")));
        assert!(matches!(controller.hear(password, outcome(shared::PamOutcome::Success)), Heard::Ended { .. }));
        assert_eq!(controller.snapshot().fingerprint, None, "the password's match stops the scan");
        assert_eq!(controller.hear(fingerprint, outcome(shared::PamOutcome::Success)), Heard::Stale);
    }

    /// A fingerprint that ends without a match is not a wrong password and does not restart itself:
    /// only `listen`, after a submit or a new lock, brings it back.
    #[test]
    fn a_failed_fingerprint_counts_nothing_and_stays_off_until_asked_again() {
        let (controller, _password, fingerprint) = scanning();
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
        let (controller, _password, fingerprint) = scanning();
        let asked = shared::PamMessage::Prompt { text: "PIN:".into(), echo: false };
        assert_eq!(controller.hear(fingerprint, asked), Heard::Changed);
        let state = controller.snapshot();
        assert_eq!(state.prompt.map(|p| p.text), Some("Password:".to_string()), "still the password's");
        assert_eq!(state.fingerprint, None);
        assert_eq!(controller.hear(fingerprint, outcome(shared::PamOutcome::Success)), Heard::Stale);
    }

    #[test]
    fn dispatch_routes_lock_ignores_an_unknown_action_and_refuses_to_unlock() {
        let controller = LockController::new();
        let action = |name: &str| dispatch(&controller, &envelope(shared::Capability::Lock, name, Vec::new()));
        assert!(action("lock"));
        assert!(!action("nonsense"), "an unknown action must be logged, not turned into a command");
        // ADR-0042: an unlock action would make walking past PAM one mouse click. The
        // asymmetry with lock is deliberate, pinned so it isn't re-added as an oversight.
        assert!(!action("unlock"), "unlock must not be commandable from Lua");
        assert!(!action("set_unlock_animation"));
        assert_eq!(controller.unlock_animation(), Duration::ZERO);
    }
}

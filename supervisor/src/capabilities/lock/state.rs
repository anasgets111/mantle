use shared::state::conversation::{PamNotice, PamPrompt};
pub use shared::state::lock::LockState;

/// Events from Lua `lock()`, the PAM worker, or the Renderer, all applied by pure [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// A lock was asked for: `lock.lock()`, logind's `Lock`, or a sleep.
    LockRequested,
    /// Verdict of the running conversation (ADR-0381), checked against its serial by
    /// `LockController::hear`.
    Authenticated(shared::PamOutcome),
    /// A lock asked for during the unlock animation keeps the session locked (ADR-0190).
    ReleaseCancelled,
    /// Renderer report of the lock outcome.
    Reported(shared::LockOutcome),
    /// The `ext_session_lock_v1` holder died without a report (ADR-0058 decision 4). The compositor
    /// keeps the session locked; only this shell's ability to speak for it ended.
    RendererLost,
}

/// Pure synchronous transition table, testable without socket or PAM. Only the Renderer's report
/// moves `active`; a request or successful password is still unconfirmed.
pub fn apply(state: &mut LockState, event: LockEvent) {
    match event {
        // An in-place reload must not retain the old config's refusal reason (ADR-0052 decision 3).
        LockEvent::LockRequested => state.error.clear(),
        LockEvent::ReleaseCancelled => state.unlocking = false,
        LockEvent::Authenticated(shared::PamOutcome::Success) => {
            state.authenticating = false;
            state.error.clear();
            // The password was right and the lock is still up: the whole of the out-animation's
            // window, and true whether or not one is configured (ADR-0190).
            state.unlocking = true;
        }
        LockEvent::Authenticated(outcome) => {
            state.authenticating = false;
            // Only a rejected password counts. `StartFailed` and `PamError` are the worker not
            // running, which the user at the lock screen cannot answer, and a config drawing a
            // limit from `attempts` would shut them out for it.
            if matches!(outcome, shared::PamOutcome::AuthFailed | shared::PamOutcome::MaxTries) {
                state.attempts += 1;
            }
            state.error = error_for_outcome(&outcome);
        }
        LockEvent::Reported(shared::LockOutcome::Locked) => {
            state.active = true;
            state.authenticating = false;
            state.attempts = 0;
            state.error.clear();
            // A fresh lock is not a lock being left: an idle timer can lock again while the last
            // unlock's animation is still playing, and the new screen must come up locked.
            state.unlocking = false;
        }
        // Nothing was taken or protected (ADR-0052 decision 3).
        LockEvent::Reported(shared::LockOutcome::Refused(reason)) => {
            state.authenticating = false;
            // A refusal after Success (renderer never granted the lock) leaves nothing to exit.
            state.unlocking = false;
            state.error = reason;
        }
        // `Finished` after `Locked` is teardown, not failure; Renderer sets `mantle.rescue` after
        // lock surfaces are gone (ADR-0052 decision 4). Keep `attempts`.
        LockEvent::Reported(shared::LockOutcome::Finished | shared::LockOutcome::Unlocked) => {
            state.active = false;
            state.authenticating = false;
            state.error.clear();
            // The lock is off the glass, so there is nothing left to animate out of.
            state.unlocking = false;
        }
        // Nothing of ours is on the glass and no answer can apply; keep the error.
        LockEvent::RendererLost => {
            state.active = false;
            state.authenticating = false;
            // The animating screen died with its Renderer; left true, the replacement's lock screen
            // would draw permanently mid-exit. `LockRequested` does not clear it either.
            state.unlocking = false;
        }
    }
}

/// Whether `secure_submit(lock, authenticate)` may answer PAM. Without `active`, any config
/// textfield gets an unbounded password oracle; without `!authenticating`, held Enter sends one
/// answer per keypress, each paying `pam_unix`'s delay; without `!unlocking`, a submit mid-exit
/// restarts PAM.
pub fn may_authenticate(state: &LockState) -> bool {
    state.active && !state.authenticating && !state.unlocking
}

impl crate::capabilities::conversation::PamFields for LockState {
    fn pam(&mut self) -> (&mut Option<PamPrompt>, &mut Vec<PamNotice>, &mut bool) {
        (&mut self.prompt, &mut self.messages, &mut self.authenticating)
    }
}

/// The lock screen's failed-authentication line, not `mantle.rescue`, which ordinary config
/// surfaces draw behind the lock (ADR-0052 decision 4). `Success` has no message. Polkit uses the
/// same words.
pub(crate) fn error_for_outcome(outcome: &shared::PamOutcome) -> String {
    match outcome {
        shared::PamOutcome::Success => String::new(),
        shared::PamOutcome::AuthFailed => "authentication failed".to_string(),
        shared::PamOutcome::MaxTries => "too many attempts".to_string(),
        // Names the repair, because no password can work and the user is looking at the only
        // screen that will not tell them so. The detail stays for the log.
        shared::PamOutcome::StartFailed(err) => {
            format!("authentication is unavailable; a terminal login can repair it. {err}")
        }
        shared::PamOutcome::PamError(err) => format!("authentication error: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state mid-session with one failure already recorded, so a transition's effect on
    /// `attempts`/`error` is visible against the default.
    fn locked_with_one_failure() -> LockState {
        LockState {
            active: true,
            authenticating: true,
            attempts: 1,
            error: "authentication failed".to_string(),
            prompt: None,
            messages: Vec::new(),
            fingerprint: None,
            unlocking: false,
        }
    }

    #[test]
    fn losing_the_renderer_frees_the_lock_for_a_replacement_and_rejects_an_in_flight_answer() {
        let mut state = locked_with_one_failure();
        // A correct password whose unlock the Renderer could not read (#123).
        state.unlocking = true;

        apply(&mut state, LockEvent::RendererLost);

        assert!(!state.active, "the lock object died with the process that held it");
        // A stranded authenticating flag would refuse every future attempt via may_authenticate.
        assert!(!state.authenticating, "the conversation's answer can no longer be applied, so its slot must be free");
        assert!(!state.unlocking, "the replacement's lock screen must take a password again");
    }

    /// A worker that will not start is not a wrong password. Counting it spends an allowance the
    /// user cannot avoid, and a config drawing a limit from `attempts` would shut them out for an
    /// infrastructure failure.
    #[test]
    fn a_failure_to_start_authentication_does_not_count_as_an_attempt() {
        let mut state = LockState { active: true, ..LockState::default() };

        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::StartFailed("no worker".into())));
        assert_eq!(state.attempts, 0);
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::PamError("broken".into())));
        assert_eq!(state.attempts, 0);

        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert_eq!(state.attempts, 1, "a rejected password still counts");
        assert!(state.active, "and the lock is held throughout");
        assert!(!state.authenticating, "with another attempt allowed after each");
    }

    #[test]
    fn lock_requested_clears_a_previous_attempts_refusal_reason_without_claiming_the_lock() {
        let mut state = LockState { error: "no lock node is declared".to_string(), ..LockState::default() };
        apply(&mut state, LockEvent::LockRequested);
        assert_eq!(
            state,
            LockState::default(),
            "a fresh lock() must not show the last attempt's reason, and must not claim active before the Renderer reports it"
        );
    }

    #[test]
    fn authentication_needs_a_held_lock_and_no_attempt_already_in_flight() {
        assert!(
            !may_authenticate(&LockState::default()),
            "an unlocked session must not be a PAM oracle any config can drive from a bar textfield"
        );
        assert!(may_authenticate(&LockState { active: true, ..LockState::default() }));
        assert!(
            !may_authenticate(&LockState { active: true, authenticating: true, ..LockState::default() }),
            "a held-down Enter key must not spawn one PAM worker per keypress"
        );
        assert!(
            !may_authenticate(&LockState { active: true, unlocking: true, ..LockState::default() }),
            "a submit during the out-animation after Success must not start PAM again"
        );
    }

    #[test]
    fn locked_marks_the_session_active_and_resets_the_attempt_counter() {
        let mut state = LockState { attempts: 3, error: "authentication failed".to_string(), ..LockState::default() };
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: false,
                attempts: 0,
                error: String::new(),
                prompt: None,
                messages: Vec::new(),
                fingerprint: None,
                unlocking: false
            }
        );
    }

    #[test]
    fn refused_records_the_reason_and_leaves_the_session_unlocked() {
        let mut state = LockState::default();
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Refused("no lock node is declared".to_string())));
        assert_eq!(
            state,
            LockState {
                active: false,
                authenticating: false,
                attempts: 0,
                error: "no lock node is declared".to_string(),
                prompt: None,
                messages: Vec::new(),
                fingerprint: None,
                unlocking: false
            }
        );
    }

    #[test]
    fn a_failed_authentication_counts_an_attempt_and_keeps_the_session_locked() {
        let mut state = LockState { active: true, authenticating: true, ..LockState::default() };
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: false,
                attempts: 1,
                error: "authentication failed".to_string(),
                prompt: None,
                messages: Vec::new(),
                fingerprint: None,
                unlocking: false
            }
        );

        // Two identical consecutive failures are one unchanged error string -- the counter is the
        // only thing that tells the config the second one happened.
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert_eq!(state.attempts, 2);
    }

    #[test]
    fn a_successful_authentication_stops_authenticating_but_does_not_itself_unlock() {
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: false,
                attempts: 1,
                error: String::new(),
                prompt: None,
                messages: Vec::new(),
                fingerprint: None,
                // PAM said yes and the lock is still up: the window a config animates out in
                // (ADR-0190).
                unlocking: true
            },
            "active clears only when the Renderer reports Unlocked -- the lock is on the glass until unlock_and_destroy actually runs"
        );
    }

    /// ADR-0190. `unlocking` is the window a config animates its lock screen out in: open from the
    /// moment PAM says yes, shut by the release. Every way the lock can come back must shut it, or
    /// a screen that locks again mid-animation comes up already playing its own exit.
    #[test]
    fn unlocking_opens_on_a_correct_password_and_shuts_on_every_way_the_lock_ends() {
        let mut state = locked_with_one_failure();
        assert!(!state.unlocking, "a lock nobody has answered is not on its way out");

        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        assert!(state.unlocking, "a correct password opens the window");
        assert!(state.active, "and the lock is still on the glass while it is open");

        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Unlocked));
        assert!(!state.unlocking, "the release shuts it");

        // A relock during the animation: an idle timer can fire while the last unlock is still
        // playing, and the new lock screen must not come up mid-exit.
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(!state.unlocking, "a fresh lock is not a lock being left");

        // A refusal after Success ends the exit window, or authentication stays refused forever.
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        assert!(!may_authenticate(&state));
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Refused("never granted".to_string())));
        assert!(may_authenticate(&state));

        // A lock asked for during the animation keeps the lock up, and the screen locked.
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        apply(&mut state, LockEvent::ReleaseCancelled);
        assert!(state.active && may_authenticate(&state), "{state:?}");

        // A wrong password does not open it.
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert!(!state.unlocking);
    }

    #[test]
    fn unlocked_and_finished_both_clear_the_session() {
        for outcome in [shared::LockOutcome::Unlocked, shared::LockOutcome::Finished] {
            let mut state = locked_with_one_failure();
            apply(&mut state, LockEvent::Reported(outcome.clone()));
            assert_eq!(
                state,
                LockState {
                    active: false,
                    authenticating: false,
                    attempts: 1,
                    error: String::new(),
                    prompt: None,
                    messages: Vec::new(),
                    fingerprint: None,
                    unlocking: false
                },
                "{outcome:?}"
            );
        }
    }
}

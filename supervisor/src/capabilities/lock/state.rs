/// `obelisk.lock`'s payload (ADR-0052 decision 4). `attempts` counts failed authentications since
/// acquisition. Lua cannot rebuild it from layout-time state (ADR-0044), so identical failures
/// leave one `error` string; empty `error` means no failure, like `keyboard.active_layout`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct LockState {
    /// The Renderer confirmed the session locked. A requested but unconfirmed lock remains `false`;
    /// [`apply`] changes this only from the Renderer report.
    pub active: bool,
    /// A password is with PAM and unanswered. `pam_unix` takes about a second, so this drives a
    /// spinner; a second submit is refused while true.
    pub authenticating: bool,
    /// PAM answers against the held lock, including success. Resets to `0` only on a new confirmed
    /// lock, so it is per-acquisition, not per-failure; lockout rules read it with `error`.
    pub attempts: u32,
    /// Drawable reason for the last failure, e.g. `"too many attempts"`. Empty before attempts or
    /// after success; rewritten on every PAM answer and cleared on a new lock.
    pub error: String,
    /// PAM has said yes and the lock is still on the glass, which is the window a config animates
    /// its lock screen out in (ADR-0190).
    ///
    /// True between a successful password and the Renderer's `Unlocked` report. With no
    /// `unlock_animation` configured that window is as short as the round trip; with one it is at
    /// least that long. Nothing a config does can extend it: the release is scheduled by the
    /// Supervisor the moment PAM answers, and this is a readout of that, not a handle on it.
    pub unlocking: bool,
    /// `SetSessionLock { locked: true }` is in flight before Renderer confirmation.
    /// `#[serde(skip)]`: bookkeeping, not payload. `active` must mean only Renderer confirmation,
    /// but a respawn must still retake a lock that was asked for and not yet confirmed (ADR-0058).
    #[serde(skip)]
    pub requested: bool,
    /// Acquisition number, bumped only on Renderer `Locked`. `#[serde(skip)]` like `requested`.
    /// PAM can outlive its lock (`pam_unix` ~1s, `PAM_EXCHANGE_TIMEOUT` 30s): `finished` after
    /// `locked` (also `loginctl unlock-session`) can end N while an idle timer takes N+1. The
    /// number binds an answer to its question and blocks stale success (`accepts_outcome`).
    #[serde(skip)]
    pub acquisition: u64,
}

/// Events from Lua `lock()`, the PAM worker, or the Renderer, all applied by pure [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// `lock.lock()` reached `lock::dispatch`; `SetSessionLock` is on its way out.
    LockRequested,
    /// `secure_submit(lock, authenticate)` arrived and PAM is starting.
    AuthenticationStarted,
    /// Answer from the re-exec'd worker (ADR-0028), checked by
    /// `LockController::record_authentication` against its acquisition.
    Authenticated(shared::PamOutcome),
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
        LockEvent::LockRequested => {
            state.requested = true;
            state.error.clear();
        }
        LockEvent::AuthenticationStarted => state.authenticating = true,
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
        // Only confirmed locks advance acquisition, invalidating answers for the prior lock.
        LockEvent::Reported(shared::LockOutcome::Locked) => {
            state.active = true;
            state.requested = false;
            state.authenticating = false;
            state.attempts = 0;
            state.error.clear();
            // A fresh lock is not a lock being left: an idle timer can lock again while the last
            // unlock's animation is still playing, and the new screen must come up locked.
            state.unlocking = false;
            state.acquisition += 1;
        }
        // Nothing was taken or protected (ADR-0052 decision 3).
        LockEvent::Reported(shared::LockOutcome::Refused(reason)) => {
            state.requested = false;
            state.authenticating = false;
            state.error = reason;
        }
        // `Finished` after `Locked` is teardown, not failure; Renderer sets `obelisk.rescue` after
        // lock surfaces are gone (ADR-0052 decision 4). Keep `attempts`.
        LockEvent::Reported(shared::LockOutcome::Finished | shared::LockOutcome::Unlocked) => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
            state.error.clear();
            // The lock is off the glass, so there is nothing left to animate out of.
            state.unlocking = false;
        }
        // Clear `active` so a replacement can re-acquire (`lock` drops requests while active), and
        // clear `authenticating` (see [`accepts_outcome`]); keep acquisition and error.
        LockEvent::RendererLost => {
            state.active = false;
            state.requested = false;
            state.authenticating = false;
            // The screen that was animating out is gone with its Renderer. Left true this survives
            // into the replacement's first push, and a config that reads it draws a lock screen
            // permanently mid-exit; `LockRequested` and `Refused` do not clear it either, so
            // nothing short of a *successful* reacquisition would.
            state.unlocking = false;
        }
    }
}

/// Whether a release scheduled for `acquisition` still applies (ADR-0190). Pure so the stale-timer
/// case can be checked without sleeping: the task re-reads exactly this before it sends.
pub fn releases(state: &LockState, acquisition: u64) -> bool {
    state.acquisition == acquisition && state.unlocking
}

/// What [`shared::LockOutcome`] says about the compositor's lock, distinct from [`apply`]
/// (ADR-0060). `LockState.active` means this shell holds it, but the compositor lock outlives
/// that: `RendererLost` clears `active` while the session stays locked. A restarted Supervisor
/// reads this outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLock {
    Taken,
    Released,
    Unchanged,
}

/// Exhaustive mapping: every new [`shared::LockOutcome`] must decide whether the session is locked,
/// rather than defaulting to [`SessionLock::Unchanged`].
pub fn compositor_lock_change(outcome: &shared::LockOutcome) -> SessionLock {
    match outcome {
        shared::LockOutcome::Locked => SessionLock::Taken,
        shared::LockOutcome::Unlocked | shared::LockOutcome::Finished => SessionLock::Released,
        // A refusal took and released nothing; preserve an earlier generation's marker.
        shared::LockOutcome::Refused(_) => SessionLock::Unchanged,
    }
}

/// Whether `secure_submit(lock, authenticate)` may start PAM. Without `active`, any config
/// textfield gets an unbounded password oracle; without `!authenticating`, held Enter spawns one
/// re-exec'd worker per keypress, each retaining a plaintext secret and paying `pam_unix`'s delay.
pub fn may_authenticate(state: &LockState) -> bool {
    state.active && !state.authenticating
}

/// Whether `acquisition` from `LockController::try_begin_authentication` still names the lock on
/// screen. Lock N can finish before a stale answer while N+1 makes `active` true again; only the
/// number binds the password to its lock. It also covers teardown clearing `authenticating` while
/// worker N runs, which could otherwise admit worker N+1 with two plaintext copies (only one can
/// apply; the older dies within `PAM_EXCHANGE_TIMEOUT`). Mismatches drop safely, with
/// `pam_worker::ReportOnDrop` and every [`apply`] arm clearing `active`/`acquisition` also clearing
/// `authenticating`.
pub fn accepts_outcome(state: &LockState, acquisition: u64) -> bool {
    state.active && state.acquisition == acquisition
}

/// The lock screen's failed-authentication line, not `obelisk.rescue`, which ordinary config
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

    #[test]
    fn only_the_compositor_confirming_a_lock_sets_the_marker_and_only_a_release_clears_it() {
        // A refusal never took a lock, so it must leave whatever the marker already said alone.
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Locked), SessionLock::Taken);
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Unlocked), SessionLock::Released);
        assert_eq!(compositor_lock_change(&shared::LockOutcome::Finished), SessionLock::Released);
        assert_eq!(
            compositor_lock_change(&shared::LockOutcome::Refused("no lock node".into())),
            SessionLock::Unchanged
        );
    }

    /// A state mid-session with one failure already recorded, so a transition's effect on
    /// `attempts`/`error` is visible against the default. `acquisition` is non-zero for the same
    /// reason: a transition that renumbered the lock would otherwise look like one that didn't.
    fn locked_with_one_failure() -> LockState {
        LockState {
            active: true,
            authenticating: true,
            attempts: 1,
            error: "authentication failed".to_string(),
            requested: false,
            acquisition: 4,
            unlocking: false,
        }
    }

    #[test]
    fn losing_the_renderer_clears_active_so_a_replacement_may_request_the_lock_again() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        assert!(!state.active, "the lock object died with the process that held it");
        assert!(!state.requested, "a request nothing will answer must not stay pending forever");
        // LockController::lock refuses while active. Without clearing it here, the replacement's
        // re-acquisition is dropped before it reaches the wire (ADR-0058 decision 4).
    }

    #[test]
    fn losing_the_renderer_rejects_a_pam_answer_that_was_already_in_flight() {
        let mut state = locked_with_one_failure();
        let in_flight = state.acquisition;

        apply(&mut state, LockEvent::RendererLost);

        assert!(
            !accepts_outcome(&state, in_flight),
            "a password answered against a lock whose holder has since died must not be applied: the unlock it would \
             order is an unlock nobody authenticated for"
        );
    }

    #[test]
    fn losing_the_renderer_releases_the_authenticating_flag_it_was_holding() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        // A stranded authenticating flag would refuse every future attempt via may_authenticate.
        assert!(!state.authenticating, "the conversation's answer can no longer be applied, so its slot must be free");
    }

    #[test]
    fn losing_the_renderer_does_not_renumber_the_acquisition() {
        let mut state = locked_with_one_failure();

        apply(&mut state, LockEvent::RendererLost);

        // Only a confirmed Locked moves acquisition; bumping here would number a lock never taken.
        assert_eq!(state.acquisition, 4);
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
            LockState { requested: true, ..LockState::default() },
            "a fresh lock() must not show the last attempt's reason, and must not claim active before the Renderer reports it"
        );
    }

    #[test]
    fn authentication_needs_a_held_lock_and_no_attempt_already_in_flight() {
        assert!(
            !may_authenticate(&LockState::default()),
            "an unlocked session must not be a PAM oracle any config can drive from a bar textfield"
        );
        assert!(
            !may_authenticate(&LockState { requested: true, ..LockState::default() }),
            "an unconfirmed request is not a lock screen on the glass yet"
        );
        assert!(may_authenticate(&LockState { active: true, ..LockState::default() }));
        assert!(
            !may_authenticate(&LockState { active: true, authenticating: true, ..LockState::default() }),
            "a held-down Enter key must not spawn one PAM worker per keypress"
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
                requested: false,
                acquisition: 1,
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
                requested: false,
                acquisition: 0,
                unlocking: false
            }
        );
    }

    #[test]
    fn authentication_started_sets_authenticating() {
        let mut state = LockState { active: true, ..LockState::default() };
        apply(&mut state, LockEvent::AuthenticationStarted);
        assert_eq!(
            state,
            LockState {
                active: true,
                authenticating: true,
                attempts: 0,
                error: String::new(),
                requested: false,
                acquisition: 0,
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
                requested: false,
                acquisition: 0,
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
                requested: false,
                acquisition: 4,
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

        // A wrong password does not open it.
        let mut state = locked_with_one_failure();
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::AuthFailed));
        assert!(!state.unlocking);
    }

    /// ADR-0190, and the reason the delayed release names its acquisition: a lock that has already
    /// ended must not be released by the timer belonging to the one before it. Arriving at
    /// ADR-0042's forbidden unlock by waiting is still arriving at it.
    #[test]
    fn a_release_scheduled_for_one_lock_does_not_apply_to_the_next() {
        let mut state = locked_with_one_failure();
        let authenticated = state.acquisition;
        apply(&mut state, LockEvent::Authenticated(shared::PamOutcome::Success));
        assert!(releases(&state, authenticated), "the lock the password answered is still the one to release");

        // That lock ends and another takes its place while the timer is still sleeping.
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Finished));
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(
            !releases(&state, authenticated),
            "a lock nobody authenticated for must not be opened by the last lock's timer"
        );
        assert!(!releases(&state, state.acquisition), "and not by its own number either, with no password given");
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
                    requested: false,
                    acquisition: 4,
                    unlocking: false
                },
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn a_stale_pam_outcome_cannot_release_the_lock_that_replaced_the_one_it_authenticated_against() {
        // The bypass, in order: a worker started against lock N is still running (pam_unix ~1s,
        // PAM_EXCHANGE_TIMEOUT allows 30) when the compositor ends lock N and something else
        // takes lock N+1. Nothing about active/authenticating distinguishes the two by then; only
        // the acquisition number does.
        let mut state = LockState::default();
        apply(&mut state, LockEvent::LockRequested);
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        apply(&mut state, LockEvent::AuthenticationStarted);
        let acquisition = state.acquisition;
        assert!(
            accepts_outcome(&state, acquisition),
            "the ordinary case: the answer is about the lock still on the glass"
        );

        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Finished));
        assert!(
            !accepts_outcome(&state, acquisition),
            "the lock the password was typed against is gone; the answer is about nothing"
        );

        apply(&mut state, LockEvent::LockRequested);
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        assert!(
            !accepts_outcome(&state, acquisition),
            "a new lock is on the glass and nobody has authenticated against it -- accepting the old worker's Success here is the bypass"
        );
        assert!(accepts_outcome(&state, state.acquisition), "and the new lock's own attempt is still accepted");
    }
}

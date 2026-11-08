//! `mantle.polkit`: polkitd's pending challenge and the dialog's one action (ADR-0114).
//!
//! `crate::polkit` owns D-Bus; this owns the config state and held reply. Like `lock`, `main.rs`
//! builds and pushes it because the bus callback, `secure_submit` frame, and PAM answer all land
//! in that loop.

pub use shared::state::polkit::{PolkitIdentity, PolkitState};

use shared::{error, info, warn};
use tokio::sync::oneshot;

use crate::capabilities::conversation::{Dialogue, Heard, PamFields};
use crate::pam_worker::Conversation;
use crate::polkit::{AgentError, BeginAuthenticationCall, unix_user_uids};
use shared::action::PolkitAction;
use shared::state::conversation::{PamNotice, PamPrompt};

/// polkitd's pending `BeginAuthentication` call and its completing reply.
struct Pending {
    /// Only the cookie outlives `begin`. The display fields and uids move into [`PolkitState`];
    /// keeping the whole call would hold the request's details and subject for the challenge's life.
    cookie: String,
    reply: oneshot::Sender<Result<(), AgentError>>,
    /// The helper's conversation, started by the first submit (ADR-0381).
    dialogue: Dialogue,
}

/// Effect of a PAM answer on the pending challenge.
pub enum Answer {
    /// Cookie no longer names the on-screen challenge: it was canceled or replaced during PAM.
    Stale,
    /// A prompt or line to show.
    Changed,
    /// Recorded in `error`; the dialog stays open and the next submit starts a new helper.
    Failed,
    /// The challenge is over; the helper told polkitd and `reply` ends the held call.
    Succeeded { reply: oneshot::Sender<Result<(), AgentError>> },
}

#[derive(Default)]
pub struct PolkitController {
    state: PolkitState,
    pending: Option<Pending>,
}

impl PolkitController {
    pub fn snapshot(&self) -> PolkitState {
        self.state.clone()
    }

    /// polkitd's `BeginAuthentication`, admitting one at a time. A second request is answered
    /// `Cancelled`, not queued or shown, because one password field cannot serve two callers; the
    /// refused caller retries or reports the failure. A request without `unix-user` is also
    /// canceled. `me` is this session's uid, the default identity when offered (ADR-0381).
    /// Returns whether the request became state.
    pub fn begin(
        &mut self,
        call: BeginAuthenticationCall,
        reply: oneshot::Sender<Result<(), AgentError>>,
        me: u32,
    ) -> bool {
        let uids = unix_user_uids(&call.identities);
        let Some(&first) = uids.first() else {
            warn!("challenge {:?} carried no unix-user identity; cancelling it", call.cookie);
            let _ = reply.send(Err(AgentError::Cancelled));
            return false;
        };
        if self.pending.is_some() {
            warn!("challenge {:?} arrived while another is on screen; cancelling it", call.cookie);
            let _ = reply.send(Err(AgentError::Cancelled));
            return false;
        }
        self.state = PolkitState {
            active: true,
            message: call.message,
            action_id: call.action_id,
            icon_name: call.icon_name,
            identity: if uids.contains(&me) { me } else { first },
            // ponytail: `getpwuid_r` inline, fast for local passwd; `spawn_blocking` if a
            // networked NSS backend appears.
            identities: uids
                .into_iter()
                .map(|uid| PolkitIdentity { uid, name: crate::pam_worker::username_for(uid).unwrap_or_default() })
                .collect(),
            ..PolkitState::default()
        };
        self.pending = Some(Pending { cookie: call.cookie, reply, dialogue: Dialogue::default() });
        true
    }

    /// Ends the challenge with `Cancelled`: dialog cancel (`None`) or polkitd's
    /// `CancelAuthentication` for the matching `Some(cookie)`. Returns whether one was open.
    pub fn cancel(&mut self, cookie: Option<&str>) -> bool {
        let Some(pending) = self.pending.take_if(|p| cookie.is_none_or(|c| c == p.cookie)) else {
            return false;
        };
        let _ = pending.reply.send(Err(AgentError::Cancelled));
        self.state = PolkitState::default();
        true
    }

    /// `secure_submit(polkit, authenticate)`. The first submit starts the helper and holds the
    /// answer for its prompt; closing a helper that waits on a prompt fails PAM, so none runs
    /// before anything is typed. Refused with no challenge or while an answer is with PAM.
    pub fn submit(
        &mut self,
        secret: shared::Zeroizing<Vec<u8>>,
        start: impl FnOnce(u64, u32, String) -> Conversation,
    ) -> bool {
        let Some(pending) = self.pending.as_mut() else { return false };
        let (uid, cookie) = (self.state.identity, pending.cookie.clone());
        pending.dialogue.submit(&mut self.state, secret, |serial| start(serial, uid, cookie))
    }

    /// Authenticates as `uid` from now on (#7). Ends a running helper, whose late verdict is then
    /// stale, and clears its prompt, lines, error and held answer; the next submit starts one for
    /// `uid`. Not an attempt. Returns whether state changed.
    pub fn select_identity(&mut self, uid: u32) -> bool {
        let Some(pending) = self.pending.as_mut() else { return false };
        if uid == self.state.identity {
            return false;
        }
        if !self.state.identities.iter().any(|identity| identity.uid == uid) {
            warn!("select_identity({uid}) names no identity challenge {:?} offered; ignoring it", pending.cookie);
            return false;
        }
        pending.dialogue.stop(&mut self.state);
        self.state.identity = uid;
        self.state.error.clear();
        true
    }

    /// Applies one frame of the helper's conversation `serial`. Success returns the reply that
    /// ends the held call; the controller has already forgotten the challenge.
    pub fn hear(&mut self, serial: u64, message: shared::PamMessage) -> Answer {
        let Some(pending) = self.pending.as_mut() else { return Answer::Stale };
        match pending.dialogue.hear(&mut self.state, serial, message) {
            Heard::Stale => Answer::Stale,
            Heard::Changed => Answer::Changed,
            Heard::Ended { outcome: shared::PamOutcome::Success, .. } => {
                info!("challenge {:?} authenticated successfully", pending.cookie);
                let Some(pending) = self.pending.take() else { return Answer::Stale };
                self.state = PolkitState::default();
                Answer::Succeeded { reply: pending.reply }
            }
            Heard::Ended { outcome, .. } => {
                error!("authentication for challenge {:?} failed: {outcome:?}", pending.cookie);
                self.state.error = super::lock::error_for_outcome(&outcome);
                Answer::Failed
            }
        }
    }
}

impl PamFields for PolkitState {
    fn pam(&mut self) -> (&mut Option<PamPrompt>, &mut Vec<PamNotice>, &mut bool) {
        (&mut self.prompt, &mut self.messages, &mut self.authenticating)
    }
}

/// `mantle.polkit` action dispatch (ADR-0037). Returns whether state changed.
pub fn dispatch(controller: &mut PolkitController, envelope: &shared::CommandEnvelope) -> bool {
    let Some(action) = crate::parse_action::<PolkitAction>(&envelope.params) else { return false };
    match action {
        PolkitAction::Cancel => controller.cancel(None),
        PolkitAction::SelectIdentity { uid } => controller.select_identity(uid),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedValue, Value};

    use super::*;

    fn challenge(cookie: &str) -> BeginAuthenticationCall {
        offering(cookie, &[1000])
    }

    fn offering(cookie: &str, uids: &[u32]) -> BeginAuthenticationCall {
        let user = |uid: u32| HashMap::from([("uid".to_string(), OwnedValue::try_from(Value::from(uid)).unwrap())]);
        BeginAuthenticationCall {
            action_id: "org.test.act".into(),
            message: "Authenticate to test".into(),
            icon_name: "dialog-password".into(),
            details: HashMap::new(),
            cookie: cookie.into(),
            identities: uids.iter().map(|&uid| ("unix-user".to_string(), user(uid))).collect(),
        }
    }

    fn begun(controller: &mut PolkitController, cookie: &str) -> oneshot::Receiver<Result<(), AgentError>> {
        let (reply, answer) = oneshot::channel();
        controller.begin(challenge(cookie), reply, 1000);
        answer
    }

    #[test]
    fn the_session_user_is_the_default_identity_else_the_first_offered() {
        for (me, want) in [(2000, 2000), (3000, 0)] {
            let mut c = PolkitController::default();
            c.begin(offering("a", &[0, 2000]), oneshot::channel().0, me);
            let state = c.snapshot();
            assert_eq!(state.identity, want, "session uid {me}");
            assert_eq!(state.identities.iter().map(|i| i.uid).collect::<Vec<_>>(), [0, 2000], "as offered");
            assert_eq!(state.identities[0].name, "root");
        }
    }

    /// Switching ends the running helper and starts nothing; the next submit runs as the new uid,
    /// and the old helper's late verdict is dropped.
    #[test]
    fn selecting_an_identity_restarts_for_it_and_drops_the_old_helper() {
        let mut c = PolkitController::default();
        let _answer = {
            let (reply, answer) = oneshot::channel();
            c.begin(offering("a", &[1000, 2000]), reply, 1000);
            answer
        };
        let select = |uid: u32| shared::CommandEnvelope {
            params: shared::CommandParams {
                generation_id: 0,
                capability: "polkit".into(),
                action: "select_identity".into(),
                arguments: vec![uid.into()],
                legacy_expected_revision: 0,
            },
            id: 1,
        };
        assert!(!dispatch(&mut c, &select(1000)), "already the identity");
        assert!(!c.select_identity(4000), "not offered");
        let mut old = None;
        assert!(submit(&mut c, &mut old));
        let old = old.unwrap();
        let prompt = shared::PamMessage::Prompt { text: "Code:".into(), echo: true };
        assert!(matches!(c.hear(old, prompt), Answer::Changed));
        assert!(c.snapshot().prompt.is_some());

        assert!(dispatch(&mut c, &select(2000)));
        let state = c.snapshot();
        assert_eq!((state.identity, state.prompt, state.authenticating), (2000, None, false));
        let late = shared::PamMessage::Outcome(shared::PamOutcome::Success);
        assert!(matches!(c.hear(old, late), Answer::Stale));
        let mut started = None;
        c.submit(shared::Zeroizing::new(b"x".to_vec()), |serial, uid, _| {
            started = Some((serial, uid));
            Conversation::detached().0
        });
        assert!(started.is_some_and(|(serial, uid)| serial != old && uid == 2000));
    }

    #[test]
    fn a_challenge_becomes_the_state_and_a_second_one_is_refused() {
        let mut c = PolkitController::default();
        let mut first = begun(&mut c, "a");
        assert!(c.snapshot().active);
        assert_eq!(c.snapshot().message, "Authenticate to test");
        let mut second = begun(&mut c, "b");
        assert!(matches!(second.try_recv(), Ok(Err(AgentError::Cancelled))));
        assert!(first.try_recv().is_err(), "the first is still waiting on the user");
    }

    /// Submits `secret`, recording the serial a started helper got.
    fn submit(c: &mut PolkitController, started: &mut Option<u64>) -> bool {
        c.submit(shared::Zeroizing::new(b"hunter2".to_vec()), |serial, uid, cookie| {
            assert_eq!((uid, cookie.as_str()), (1000, "a"));
            *started = Some(serial);
            Conversation::detached().0
        })
    }

    #[test]
    fn the_helper_starts_on_the_first_submit_and_a_failure_waits_for_the_next() {
        let mut c = PolkitController::default();
        let mut answer = begun(&mut c, "a");
        assert_eq!(c.snapshot().prompt, None, "no helper runs before anything is typed");
        let mut started = None;
        assert!(submit(&mut c, &mut started));
        let serial = started.expect("the first submit starts the helper");
        assert!(c.snapshot().authenticating);
        assert!(!submit(&mut c, &mut None), "not two at once");
        let prompt = shared::PamMessage::Prompt { text: "Password:".into(), echo: false };
        assert!(matches!(c.hear(serial, prompt), Answer::Changed), "the held answer goes to it");
        let failed = shared::PamMessage::Outcome(shared::PamOutcome::AuthFailed);
        assert!(matches!(c.hear(serial, failed), Answer::Failed));
        let state = c.snapshot();
        assert!(state.active && !state.authenticating);
        assert_eq!(state.error, "authentication failed");
        assert!(answer.try_recv().is_err(), "polkitd is still waiting");

        let mut restarted = None;
        assert!(submit(&mut c, &mut restarted));
        assert!(restarted.is_some_and(|next| next != serial), "a new helper, a new serial");
        let late = shared::PamMessage::Outcome(shared::PamOutcome::Success);
        assert!(matches!(c.hear(serial, late), Answer::Stale), "the old helper's late word is dropped");
    }

    #[test]
    fn success_hands_the_challenge_back_and_clears_the_state() {
        let mut c = PolkitController::default();
        let _answer = begun(&mut c, "a");
        let mut started = None;
        submit(&mut c, &mut started);
        let success = shared::PamMessage::Outcome(shared::PamOutcome::Success);
        assert!(matches!(c.hear(started.unwrap(), success), Answer::Succeeded { .. }));
        assert_eq!(c.snapshot(), PolkitState::default());
    }

    #[test]
    fn cancel_answers_polkitd_and_a_late_pam_answer_is_stale() {
        let mut c = PolkitController::default();
        let mut answer = begun(&mut c, "a");
        let mut started = None;
        submit(&mut c, &mut started);
        assert!(!c.cancel(Some("other")), "polkitd cancelling a different cookie changes nothing");
        assert!(c.cancel(None));
        assert!(matches!(answer.try_recv(), Ok(Err(AgentError::Cancelled))));
        assert!(!c.snapshot().active);
        let success = shared::PamMessage::Outcome(shared::PamOutcome::Success);
        assert!(matches!(c.hear(started.unwrap(), success), Answer::Stale));
        assert!(!c.cancel(None), "nothing left to cancel");
        assert!(!submit(&mut c, &mut None), "no challenge to answer");
    }
}

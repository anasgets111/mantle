//! One PAM conversation as `mantle.lock` and `mantle.polkit` show it (ADR-0381): the prompt PAM
//! waits on, its info and error lines, and the one answer typed before its prompt arrived.

use std::sync::atomic::{AtomicU64, Ordering};

use shared::state::lock::{PamNotice, PamPrompt};
use shared::{PamMessage, PamNoticeKind, PamOutcome, Zeroizing};

use crate::pam_worker::Conversation;

/// The part of a capability's state a [`Dialogue`] owns: `prompt`, `messages`, `authenticating`.
pub trait PamFields {
    fn pam(&mut self) -> (&mut Option<PamPrompt>, &mut Vec<PamNotice>, &mut bool);
}

/// ponytail: lines kept per conversation, oldest dropped first. pam_faillock and pam_fprintd send
/// two or three per attempt; raise it if a module sends more that matter.
const MAX_MESSAGES: usize = 8;

/// Serials and prompt ids, unique for the Supervisor's life, so a late frame from any earlier
/// conversation never matches and two identical prompts in a row still differ.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Default)]
pub struct Dialogue {
    serial: u64,
    live: Option<Conversation>,
    /// Submitted before PAM asked; it answers the next prompt if that prompt is masked.
    held: Option<Zeroizing<Vec<u8>>>,
    /// The running conversation showed a prompt. One that fails without asking anything is a stack
    /// problem (faillock's lockout, a broken module), and restarting it at once would spin.
    prompted: bool,
    /// An error line came before the first prompt: pam_faillock's preauth refusing a locked account.
    /// That `required` failure decides the verdict whatever is typed, so a submit restarts PAM
    /// rather than answer it; a restart at a prompt costs no attempt.
    refused_early: bool,
}

/// What a frame did to the state.
#[derive(Debug, PartialEq, Eq)]
pub enum Heard {
    /// From a conversation that is no longer the running one.
    Stale,
    Changed,
    Ended {
        outcome: PamOutcome,
        prompted: bool,
    },
}

impl Dialogue {
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// Starts a conversation under a fresh serial, ending any running one.
    pub fn start(&mut self, start: impl FnOnce(u64) -> Conversation) {
        self.serial = next_id();
        self.prompted = false;
        self.refused_early = false;
        self.live = Some(start(self.serial));
    }

    /// Ends the conversation and clears what it showed.
    pub fn stop(&mut self, state: &mut impl PamFields) {
        self.live = None;
        self.held = None;
        let (prompt, messages, authenticating) = state.pam();
        *prompt = None;
        messages.clear();
        *authenticating = false;
    }

    /// Answers the pending prompt, or holds the answer for the next one, calling `start` first if
    /// nothing runs. Refused while an answer is with PAM or held, so a held Enter starts no second
    /// attempt.
    pub fn submit(
        &mut self,
        state: &mut impl PamFields,
        secret: Zeroizing<Vec<u8>>,
        start: impl FnOnce(u64) -> Conversation,
    ) -> bool {
        if *state.pam().2 {
            return false;
        }
        if self.refused_early {
            self.stop(state);
        }
        if self.live.is_none() {
            self.start(start);
        }
        let (prompt, messages, authenticating) = state.pam();
        let Some(live) = &self.live else { return false };
        messages.clear();
        *authenticating = true;
        match prompt.take() {
            Some(_) => live.answer(secret),
            None => self.held = Some(secret),
        }
        true
    }

    /// Applies one frame from conversation `serial`.
    pub fn hear(&mut self, state: &mut impl PamFields, serial: u64, message: PamMessage) -> Heard {
        let Some(live) = self.live.as_ref().filter(|_| serial == self.serial) else { return Heard::Stale };
        let (prompt, messages, authenticating) = state.pam();
        match message {
            PamMessage::Prompt { text, echo } => {
                self.prompted = true;
                match self.held.take() {
                    Some(secret) if !echo => live.answer(secret),
                    // An answer typed early never goes to a prompt PAM shows: it may be the password.
                    _ => {
                        *authenticating = false;
                        *prompt = Some(PamPrompt { id: next_id(), text, echo });
                    }
                }
            }
            PamMessage::Message { kind, text } => {
                self.refused_early |= kind == PamNoticeKind::Error && !self.prompted;
                if messages.len() == MAX_MESSAGES {
                    messages.remove(0);
                }
                messages.push(PamNotice { kind, text });
            }
            PamMessage::Outcome(outcome) => {
                self.live = None;
                self.held = None;
                *prompt = None;
                *authenticating = false;
                return Heard::Ended { outcome, prompted: self.prompted };
            }
            // The tasks never forward one.
            PamMessage::Response { .. } => return Heard::Stale,
        }
        Heard::Changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::UnboundedReceiver;

    #[derive(Default)]
    struct Fields(Option<PamPrompt>, Vec<PamNotice>, bool);

    impl PamFields for Fields {
        fn pam(&mut self) -> (&mut Option<PamPrompt>, &mut Vec<PamNotice>, &mut bool) {
            (&mut self.0, &mut self.1, &mut self.2)
        }
    }

    fn started() -> (Dialogue, u64, UnboundedReceiver<Zeroizing<Vec<u8>>>) {
        let mut dialogue = Dialogue::default();
        let mut answers = None;
        dialogue.start(|_| {
            let (conversation, received) = Conversation::detached();
            answers = Some(received);
            conversation
        });
        let serial = dialogue.serial;
        (dialogue, serial, answers.unwrap())
    }

    fn prompt(text: &str, echo: bool) -> PamMessage {
        PamMessage::Prompt { text: text.into(), echo }
    }

    fn unused(_: u64) -> Conversation {
        panic!("a conversation already runs")
    }

    fn secret(bytes: &[u8]) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(bytes.to_vec())
    }

    #[test]
    fn a_prompt_is_shown_answered_once_and_a_second_submit_waits_for_the_next() {
        let (mut dialogue, serial, mut answers) = started();
        let mut fields = Fields::default();
        assert_eq!(dialogue.hear(&mut fields, serial, prompt("Password:", false)), Heard::Changed);
        let first = fields.0.clone().expect("the prompt is on screen");
        assert_eq!((first.text.as_str(), first.echo, fields.2), ("Password:", false, false));

        assert!(dialogue.submit(&mut fields, secret(b"hunter2"), unused));
        assert_eq!(answers.try_recv().unwrap().as_slice(), b"hunter2");
        assert!(fields.0.is_none() && fields.2, "with PAM now");
        assert!(!dialogue.submit(&mut fields, secret(b"again"), unused), "a held Enter starts no second attempt");
        assert!(answers.try_recv().is_err());

        dialogue.hear(&mut fields, serial, prompt("Password:", false));
        assert_ne!(fields.0.as_ref().unwrap().id, first.id, "the same text twice is two prompts");
    }

    #[test]
    fn an_early_answer_goes_to_the_next_masked_prompt_but_never_to_a_shown_one() {
        let (mut dialogue, serial, mut answers) = started();
        let mut fields = Fields::default();
        assert!(dialogue.submit(&mut fields, secret(b"hunter2"), unused));
        assert!(fields.2 && answers.try_recv().is_err(), "held until PAM asks");
        dialogue.hear(&mut fields, serial, prompt("Password:", false));
        assert_eq!(answers.try_recv().unwrap().as_slice(), b"hunter2");
        assert!(fields.0.is_none(), "an answered prompt is never shown");

        let (mut dialogue, serial, mut answers) = started();
        let mut fields = Fields::default();
        dialogue.submit(&mut fields, secret(b"hunter2"), unused);
        dialogue.hear(&mut fields, serial, prompt("Username:", true));
        assert!(answers.try_recv().is_err(), "a password typed early must not answer a visible prompt");
        assert!(fields.0.as_ref().is_some_and(|p| p.echo) && !fields.2);
        assert!(dialogue.held.is_none(), "and it is gone");
    }

    #[test]
    fn messages_queue_up_to_the_cap_and_an_answer_clears_them() {
        let (mut dialogue, serial, _answers) = started();
        let mut fields = Fields::default();
        for n in 0..=MAX_MESSAGES {
            dialogue.hear(&mut fields, serial, PamMessage::Message { kind: PamNoticeKind::Info, text: n.to_string() });
        }
        assert_eq!(fields.1.len(), MAX_MESSAGES);
        assert_eq!(fields.1[0].text, "1", "the oldest went first");
        dialogue.hear(&mut fields, serial, prompt("Password:", false));
        dialogue.submit(&mut fields, secret(b"x"), unused);
        assert!(fields.1.is_empty());
    }

    #[test]
    fn the_outcome_ends_it_and_a_stale_or_stopped_serial_is_ignored() {
        let (mut dialogue, serial, _answers) = started();
        let mut fields = Fields::default();
        assert_eq!(dialogue.hear(&mut fields, serial + 1000, prompt("Password:", false)), Heard::Stale);
        assert!(fields.0.is_none());
        let failed = PamMessage::Outcome(PamOutcome::AuthFailed);
        assert_eq!(
            dialogue.hear(&mut fields, serial, failed.clone()),
            Heard::Ended { outcome: PamOutcome::AuthFailed, prompted: false },
            "no prompt was shown, so the caller must not restart at once"
        );
        assert!(!dialogue.is_live());
        assert_eq!(dialogue.hear(&mut fields, serial, failed), Heard::Stale, "one verdict per conversation");
        let mut restarted = None;
        assert!(dialogue.submit(&mut fields, secret(b"x"), |serial| {
            restarted = Some(serial);
            Conversation::detached().0
        }));
        assert!(restarted.is_some_and(|new| new != serial), "a submit with nothing running starts one");
        assert!(fields.2 && dialogue.held.is_some(), "held for its first prompt");

        let (mut dialogue, serial, _answers) = started();
        dialogue.hear(&mut fields, serial, prompt("Password:", false));
        dialogue.stop(&mut fields);
        assert!(fields.0.is_none() && !fields.2);
        assert_eq!(dialogue.hear(&mut fields, serial, prompt("Password:", false)), Heard::Stale);
    }

    /// A lockout that ended while its worker waited: the restarted worker's preauth passes, so the
    /// correct password unlocks instead of failing once. An error after the first prompt is a later
    /// factor's, and restarting there would send the next answer to a fresh password prompt.
    #[test]
    fn a_submit_after_an_early_error_line_restarts_pam_and_holds_the_answer() {
        let (mut dialogue, serial, mut old) = started();
        let mut fields = Fields::default();
        let locked = PamMessage::Message { kind: PamNoticeKind::Error, text: "account locked, 5 minutes left".into() };
        dialogue.hear(&mut fields, serial, locked.clone());
        dialogue.hear(&mut fields, serial, prompt("Password:", false));
        let mut new = None;
        assert!(dialogue.submit(&mut fields, secret(b"hunter2"), |serial| {
            let (conversation, received) = Conversation::detached();
            new = Some((serial, received));
            conversation
        }));
        let (fresh, mut answers) = new.expect("the early-refused conversation was replaced");
        assert!(old.try_recv().is_err(), "the refused worker never sees the password");
        assert!(fields.1.is_empty() && fields.0.is_none(), "the frozen countdown is gone");
        assert_eq!(dialogue.hear(&mut fields, serial, prompt("Password:", false)), Heard::Stale);
        dialogue.hear(&mut fields, fresh, prompt("Password:", false));
        assert_eq!(answers.try_recv().unwrap().as_slice(), b"hunter2", "held for the new worker's prompt");

        let (mut dialogue, serial, mut answers) = started();
        let mut fields = Fields::default();
        dialogue.hear(&mut fields, serial, prompt("Password:", false));
        dialogue.submit(&mut fields, secret(b"hunter2"), unused);
        dialogue.hear(&mut fields, serial, locked);
        dialogue.hear(&mut fields, serial, prompt("Code:", false));
        assert!(dialogue.submit(&mut fields, secret(b"123456"), unused), "a later error line restarts nothing");
        answers.try_recv().unwrap();
        assert_eq!(answers.try_recv().unwrap().as_slice(), b"123456");
    }
}

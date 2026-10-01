//! The `mantle.idle` inhibit gate (ADR-0139): how a held logind inhibitor affects threshold events.
//!
//! Mantle is the idle daemon here: idleness comes from the compositor (`ext_idle_notifier_v1`), and
//! nothing sets logind's `IdleHint`, so logind never acts on it. This shell must honor inhibitors itself.
//!
//! ADR-0032 gave `mantle.idle` `inhibit`/`release_inhibit` writes but no read half. A config could
//! ask logind to prevent idle and still receive `on_idle`; external `systemd-inhibit
//! --what=idle mpv film.mkv` had the same problem.
//!
//! The gate closes both. `Manager.BlockInhibited` is a change-notified, colon-separated list, so
//! one property watch answers whether any holder, including this shell, blocks idle. While it
//! names `idle`, idle thresholds stop and `idle:inhibit(reason)` has its expected effect. An input
//! resume still reaches a threshold that was already idle when the hold began.
//!
//! The compositor's regular listener honours surface inhibitors; its input-only twin reports
//! input even when a hold suppresses ordinary idle notifications.

use std::collections::HashSet;

/// `Manager.BlockInhibited`'s colon-separated `what` entries, e.g. `"handle-power-key"` or
/// `"idle:sleep"`. Match `"idle"` as a whole entry; substring matching would misread
/// `"handle-lid-switch"`.
pub(crate) fn blocks_idle(block_inhibited: &str) -> bool {
    block_inhibited.split(':').any(|what| what == "idle")
}

/// Tracks which thresholds were told the session went idle, so a newly-held inhibitor can take
/// that back rather than leaving a config's `on_idle` unanswered.
#[derive(Debug, Default)]
pub(crate) struct IdleGate {
    blocked: bool,
    /// Gated listeners still idle, unless input already resumed that threshold.
    idled: HashSet<(u32, u64)>,
    /// Thresholds resumed for an inhibitor or compositor activity, but not yet for input. A later
    /// input still needs its own callback after either non-input resume.
    awaiting_input: HashSet<(u32, u64)>,
}

impl IdleGate {
    /// One raw threshold event, recorded even under a block so release knows what is still idle.
    /// `None` drops it.
    pub(crate) fn observe(&mut self, event: shared::IdleEvent) -> Option<shared::IdleEvent> {
        let key = (event.generation_id, event.threshold_sec);
        match event.state {
            shared::IdleState::Idled => {
                self.idled.insert(key);
                (!self.blocked).then_some(event)
            }
            shared::IdleState::Resumed { cause } => {
                let was_idled = self.idled.remove(&key);
                match cause {
                    shared::ResumeCause::Input => {
                        let awaiting_input = self.awaiting_input.remove(&key);
                        (awaiting_input || (was_idled && !self.blocked)).then_some(event)
                    }
                    shared::ResumeCause::Activity if was_idled && !self.blocked => {
                        self.awaiting_input.insert(key);
                        Some(event)
                    }
                    shared::ResumeCause::Activity => None,
                    shared::ResumeCause::Inhibitor => unreachable!("the gate creates inhibitor resumes"),
                }
            }
        }
    }

    /// Drops a departed generation's thresholds so release does not replay them.
    pub(crate) fn forget(&mut self, generation_id: u32) {
        self.idled.retain(|&(idled_generation, _)| idled_generation != generation_id);
        self.awaiting_input.retain(|&(idled_generation, _)| idled_generation != generation_id);
    }

    /// A cancelled last callback destroys its listener pair; a later registration starts fresh.
    pub(crate) fn forget_threshold(&mut self, generation_id: u32, threshold_sec: u64) {
        let key = (generation_id, threshold_sec);
        self.idled.remove(&key);
        self.awaiting_input.remove(&key);
    }

    /// A change in logind's idle-block answer; `None` if unchanged. A block takes back every idle
    /// threshold; release announces those still idle, since a notification never resends `idled`.
    ///
    /// Starts unblocked: an unblocked first observation is `None` and logs nothing; an inhibitor
    /// already held at startup is a real change and logs.
    pub(crate) fn set_blocked(&mut self, blocked: bool) -> Option<Vec<shared::IdleEvent>> {
        if blocked == self.blocked {
            return None;
        }
        self.blocked = blocked;
        let state = if blocked {
            self.awaiting_input.extend(self.idled.iter().copied());
            shared::IdleState::Resumed { cause: shared::ResumeCause::Inhibitor }
        } else {
            shared::IdleState::Idled
        };
        let mut owed: Vec<shared::IdleEvent> = self
            .idled
            .iter()
            .map(|&(generation_id, threshold_sec)| shared::IdleEvent { generation_id, threshold_sec, state })
            .collect();
        // HashSet order is arbitrary; sort before Lua callbacks for repeatable undims.
        owed.sort_by_key(|event| (event.generation_id, event.threshold_sec));
        Some(owed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{IdleEvent, IdleState, ResumeCause};

    fn event(generation_id: u32, threshold_sec: u64, state: IdleState) -> IdleEvent {
        IdleEvent { generation_id, threshold_sec, state }
    }

    fn resumed(cause: ResumeCause) -> IdleState {
        IdleState::Resumed { cause }
    }

    #[test]
    fn block_inhibited_names_idle_only_as_a_whole_entry() {
        assert!(blocks_idle("idle"));
        assert!(blocks_idle("idle:sleep"));
        assert!(blocks_idle("shutdown:idle:handle-power-key"));
        assert!(!blocks_idle(""));
        assert!(!blocks_idle("handle-power-key"));
        assert!(!blocks_idle("shutdown:sleep"));
    }

    /// Startup with no inhibitor: the gate already agrees, so do not announce a nonexistent
    /// release.
    #[test]
    fn observing_an_unblocked_system_at_startup_is_not_a_change() {
        assert_eq!(IdleGate::default().set_blocked(false), None);
    }

    #[test]
    fn an_unblocked_gate_forwards_everything_untouched() {
        let mut gate = IdleGate::default();
        assert_eq!(gate.observe(event(1, 30, IdleState::Idled)), Some(event(1, 30, IdleState::Idled)));
        assert_eq!(
            gate.observe(event(1, 30, resumed(ResumeCause::Input))),
            Some(event(1, 30, resumed(ResumeCause::Input)))
        );
    }

    #[test]
    fn a_blocked_gate_forwards_nothing() {
        let mut gate = IdleGate::default();
        gate.set_blocked(true);
        assert_eq!(gate.observe(event(1, 30, IdleState::Idled)), None);
        assert_eq!(gate.observe(event(1, 30, resumed(ResumeCause::Input))), None);
    }

    /// A config dimmed at 30s must get its undim when a film takes an inhibitor, not wait for
    /// input.
    #[test]
    fn an_arriving_inhibitor_takes_back_every_idle_it_had_announced() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(1, 300, IdleState::Idled));

        assert_eq!(
            gate.set_blocked(true),
            Some(vec![event(1, 30, resumed(ResumeCause::Inhibitor)), event(1, 300, resumed(ResumeCause::Inhibitor)),])
        );
    }

    #[test]
    fn a_threshold_that_already_resumed_is_not_resumed_again() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(1, 30, resumed(ResumeCause::Input)));

        assert_eq!(gate.set_blocked(true), Some(Vec::new()));
    }

    #[test]
    fn an_inhibitor_arriving_while_nothing_was_idle_owes_nothing() {
        let mut gate = IdleGate::default();
        assert_eq!(gate.set_blocked(true), Some(Vec::new()));
    }

    /// `BlockInhibited` changes for unrelated inhibitors too; the same idle answer must not
    /// re-announce a resume.
    #[test]
    fn repeating_the_same_block_state_owes_nothing() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        assert_eq!(gate.set_blocked(true).map(|owed| owed.len()), Some(1));
        assert_eq!(gate.set_blocked(true), None, "the same answer twice is not a change");
    }

    #[test]
    fn releasing_an_inhibitor_announces_the_thresholds_still_idle() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(1, 60, IdleState::Idled));
        gate.set_blocked(true);
        gate.observe(event(1, 60, resumed(ResumeCause::Input)));
        gate.observe(event(1, 300, IdleState::Idled));

        assert_eq!(
            gate.set_blocked(false),
            Some(vec![event(1, 30, IdleState::Idled), event(1, 300, IdleState::Idled)])
        );
    }

    #[test]
    fn a_forgotten_generation_is_not_announced_on_release() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(2, 30, IdleState::Idled));
        gate.set_blocked(true);
        gate.forget(1);

        assert_eq!(gate.set_blocked(false), Some(vec![event(2, 30, IdleState::Idled)]));
    }

    /// ADR-0299: idle -> inhibitor -> input -> release delivers an input resume during the hold
    /// and nothing on release, because activity already resumed the seat.
    #[test]
    fn idle_inhibitor_input_release_sequence_delivers_input_during_hold_and_nothing_on_release() {
        let mut gate = IdleGate::default();
        assert_eq!(gate.observe(event(1, 30, IdleState::Idled)), Some(event(1, 30, IdleState::Idled)));
        assert_eq!(gate.set_blocked(true), Some(vec![event(1, 30, resumed(ResumeCause::Inhibitor))]));
        assert_eq!(
            gate.observe(event(1, 30, resumed(ResumeCause::Input))),
            Some(event(1, 30, resumed(ResumeCause::Input)))
        );
        assert_eq!(gate.observe(event(1, 30, resumed(ResumeCause::Input))), None);
        assert_eq!(gate.set_blocked(false), Some(Vec::new()));
    }

    #[test]
    fn unblocked_twin_listeners_are_deduplicated_to_one_resume() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        assert_eq!(
            gate.observe(event(1, 30, resumed(ResumeCause::Input))),
            Some(event(1, 30, resumed(ResumeCause::Input)))
        );
        assert_eq!(gate.observe(event(1, 30, resumed(ResumeCause::Input))), None);
    }

    #[test]
    fn compositor_activity_stops_idle_without_consuming_a_later_input() {
        let mut gate = IdleGate::default();
        assert_eq!(gate.observe(event(1, 30, IdleState::Idled)), Some(event(1, 30, IdleState::Idled)));
        assert_eq!(
            gate.observe(event(1, 30, resumed(ResumeCause::Activity))),
            Some(event(1, 30, resumed(ResumeCause::Activity)))
        );
        assert_eq!(
            gate.observe(event(1, 30, resumed(ResumeCause::Input))),
            Some(event(1, 30, resumed(ResumeCause::Input)))
        );
        assert_eq!(gate.observe(event(1, 30, resumed(ResumeCause::Input))), None);
    }

    #[test]
    fn input_after_inhibitor_release_is_delivered_after_activity() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.set_blocked(true);
        assert_eq!(gate.observe(event(1, 30, resumed(ResumeCause::Activity))), None);
        assert_eq!(gate.set_blocked(false), Some(Vec::new()));
        assert_eq!(
            gate.observe(event(1, 30, resumed(ResumeCause::Input))),
            Some(event(1, 30, resumed(ResumeCause::Input)))
        );
    }

    #[test]
    fn cancelling_a_threshold_drops_its_pending_input_resume() {
        let mut gate = IdleGate::default();
        gate.observe(event(1, 30, IdleState::Idled));
        gate.observe(event(1, 30, resumed(ResumeCause::Activity)));
        gate.forget_threshold(1, 30);
        assert_eq!(gate.observe(event(1, 30, resumed(ResumeCause::Input))), None);
    }
}

//! Routes `mantle call` answers back to the control client that asked (ADR-0197).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// Most `mantle call`s that may be in flight at once (ADR-0197). A keybind makes one at a time; a
/// script could make more, and this bounds what a peer that never reads its answer can pin.
pub(super) const MAX_PENDING_CALLS: usize = 64;

/// Control clients waiting on an `mantle call` answer, keyed by the id this assigns.
///
/// Not the `GenerationRegistry`: that is keyed by generation, and every control client shares
/// [`shared::CONTROL_CLIENT_GENERATION`], so it cannot tell one waiting peer from another. The id
/// is assigned here rather than taken from the client for the same reason a generation id is not
/// believed from a handshake body -- a client-chosen id would let one peer collect another's answer.
#[derive(Clone, Default)]
pub struct CallRoutes(Arc<Mutex<Pending>>);

/// The waiting callers and the counter that names them. One lock covers both: every id is handed
/// out while the map is already held to check the cap against it.
#[derive(Default)]
struct Pending {
    waiting: HashMap<u64, Waiting>,
    last_id: u64,
}

/// One waiting caller: where its answer goes, and which generation was asked.
struct Waiting {
    reply: mpsc::Sender<Vec<u8>>,
    /// `None` until `main` forwards it. A result naming a different generation is stale, which a
    /// respawn mid-call can produce, and answering from it would report another config's outcome.
    dispatched_to: Option<u32>,
}

impl CallRoutes {
    /// Reserves an id for a caller, or `None` when too many are already pending.
    pub fn open(&self, reply: mpsc::Sender<Vec<u8>>) -> Option<u64> {
        let mut pending = self.0.lock().expect("call routes mutex poisoned");
        if pending.waiting.len() >= MAX_PENDING_CALLS {
            return None;
        }
        pending.last_id = pending.last_id.wrapping_add(1);
        let id = pending.last_id;
        pending.waiting.insert(id, Waiting { reply, dispatched_to: None });
        Some(id)
    }

    /// Records which generation was asked, so a later result can be checked against it.
    pub fn dispatched(&self, id: u64, generation_id: u32) {
        if let Some(entry) = self.0.lock().expect("call routes mutex poisoned").waiting.get_mut(&id) {
            entry.dispatched_to = Some(generation_id);
        }
    }

    /// Answers the caller and forgets it. Refuses a result from a generation that was not asked.
    pub fn answer(&self, from_generation: u32, result: &shared::CallResult) -> Result<(), String> {
        let entry = {
            let mut pending = self.0.lock().expect("call routes mutex poisoned");
            let Some(entry) = pending.waiting.remove(&result.id) else {
                return Err(format!("no caller is waiting on call {}", result.id));
            };
            match entry.dispatched_to {
                // Not yet forwarded, so no generation can have been asked. Refused without removing
                // the route: the call it belongs to has not been made, and consuming it here would
                // strand the caller that is about to make it.
                None => {
                    pending.waiting.insert(result.id, entry);
                    return Err(format!("call {} has not been dispatched yet", result.id));
                }
                Some(asked) if asked != from_generation => {
                    pending.waiting.insert(result.id, entry);
                    return Err(format!(
                        "call {} was dispatched to generation {asked}, so generation {from_generation} cannot answer it",
                        result.id
                    ));
                }
                Some(_) => entry,
            }
        };
        let payload = serde_json::to_vec(&shared::SupervisorFrame::CallResult(result.clone()))
            .map_err(|err| format!("could not encode the answer to call {}: {err}", result.id))?;
        entry.reply.try_send(payload).map_err(|err| format!("the caller of call {} is gone: {err}", result.id))
    }

    /// Whether this id still has a caller waiting, so a connection can forget the ones answered.
    pub fn is_pending(&self, id: u64) -> bool {
        self.0.lock().expect("call routes mutex poisoned").waiting.contains_key(&id)
    }

    /// Drops ids whose connection ended, so a caller that hung up before its answer leaves nothing.
    pub fn close(&self, ids: &[u64]) {
        let mut pending = self.0.lock().expect("call routes mutex poisoned");
        for id in ids {
            pending.waiting.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use shared::SupervisorFrame;

    use super::*;

    fn result(id: u64) -> shared::CallResult {
        shared::CallResult { id, outcome: shared::CallOutcome::Returned(serde_json::json!("recording")) }
    }

    #[tokio::test]
    async fn an_answer_reaches_the_caller_that_opened_the_call() {
        let routes = CallRoutes::default();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        let id = routes.open(tx).expect("the first call fits");
        routes.dispatched(id, 7);

        routes.answer(7, &result(id)).expect("the generation that was asked may answer");
        let payload = rx.try_recv().expect("the caller is waiting on exactly this");
        let frame: SupervisorFrame = serde_json::from_slice(&payload).unwrap();
        assert_eq!(frame, SupervisorFrame::CallResult(result(id)));
    }

    #[tokio::test]
    async fn a_generation_that_was_not_asked_cannot_answer() {
        let routes = CallRoutes::default();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        let id = routes.open(tx).unwrap();
        routes.dispatched(id, 7);

        // A respawn mid-call can leave the old generation's reply in flight.
        let refusal = routes.answer(8, &result(id)).expect_err("a stale generation must be refused");
        assert!(refusal.contains('7') && refusal.contains('8'), "the refusal names both: {refusal}");
        assert!(rx.try_recv().is_err(), "nothing may reach the caller");
    }

    #[tokio::test]
    async fn a_route_that_was_never_dispatched_is_refused_and_kept() {
        let routes = CallRoutes::default();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        let id = routes.open(tx).unwrap();

        // The id exists but `main` has not forwarded it yet, so no generation was asked. Consuming
        // it here would strand the caller whose call is still on its way.
        assert!(routes.answer(7, &result(id)).is_err());
        assert!(rx.try_recv().is_err());
        routes.dispatched(id, 7);
        routes.answer(7, &result(id)).expect("the route survived the refusal");
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn an_id_nobody_waits_on_is_refused_rather_than_panicking() {
        let routes = CallRoutes::default();
        assert!(routes.answer(7, &result(404)).is_err());
    }

    #[tokio::test]
    async fn a_caller_that_hung_up_leaves_nothing_behind() {
        let routes = CallRoutes::default();
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let id = routes.open(tx).unwrap();
        routes.close(&[id]);
        assert!(routes.answer(7, &result(id)).is_err(), "a closed route must not still accept an answer");
    }

    #[tokio::test]
    async fn pending_calls_are_bounded_so_a_peer_that_never_reads_cannot_pin_the_table() {
        let routes = CallRoutes::default();
        let mut held = Vec::new();
        for _ in 0..MAX_PENDING_CALLS {
            let (tx, rx) = mpsc::channel::<Vec<u8>>(1);
            held.push(rx);
            assert!(routes.open(tx).is_some());
        }
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(1);
        assert!(routes.open(tx).is_none(), "one past the cap is refused");
    }
}

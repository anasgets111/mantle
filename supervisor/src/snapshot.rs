//! Snapshot-push path (ADR-0037): serialize, send to the authoritative generation, and record in
//! `last_snapshots`, whose entry also carries the capability's revision.

use std::collections::HashMap;

use shared::{Capability, SupervisorFrame, warn};

use crate::{send_frame_logged, socket};

/// A capability's last sent snapshot, plus how many pushes since start were dropped as equal to
/// the one before them. The revision counts the ones sent.
pub(crate) struct Published {
    pub(crate) snapshot: shared::StateSnapshot,
    pub(crate) deduped: u32,
}

/// Sends `snapshot` and hands it back. `send_frame_logged` borrows, so the obvious spelling
/// deep-clones the whole `payload` tree, the largest thing on this path, per signal just to keep a
/// copy; moving it through the frame and back does not.
pub(crate) fn send_owned(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    snapshot: shared::StateSnapshot,
) -> shared::StateSnapshot {
    let frame = SupervisorFrame::StateSnapshot(snapshot);
    send_frame_logged(registry, generation_id, &frame);
    match frame {
        SupervisorFrame::StateSnapshot(snapshot) => snapshot,
        _ => unreachable!("the frame was built as a StateSnapshot"),
    }
}

/// Pushes `state` as a fresh `StateSnapshot` with the next revision, and records it in
/// `last_snapshots` (ADR-0029), which `Supervisor::hydrate` replays to a new generation. A payload
/// equal to the last one is dropped: every push re-resolves the Renderer's scene (ADR-0044).
///
/// Taking [`Capability`] makes off-roster names unrepresentable (ADR-0076).
pub(crate) fn push_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    last_snapshots: &mut HashMap<Capability, Published>,
    capability: Capability,
    state: &impl serde::Serialize,
) {
    match serde_json::to_value(state) {
        Ok(payload) => {
            // Tray and notification icons are rewritten in place at the same path, so an equal
            // payload can still mean new pixels for the Renderer to stat.
            let spools_icons = matches!(capability, Capability::Tray | Capability::Notifications);
            if let Some(last) = last_snapshots.get_mut(&capability)
                && !spools_icons
                && last.snapshot.payload == payload
            {
                last.deduped += 1;
                return;
            }
            // ADR-0004's state version; the first push is `1`.
            let (revision, deduped) =
                last_snapshots.get(&capability).map_or((1, 0), |last| (last.snapshot.revision + 1, last.deduped));
            let snapshot = send_owned(
                registry,
                generation_id,
                shared::StateSnapshot { capability: capability.to_string(), revision, payload },
            );
            last_snapshots.insert(capability, Published { snapshot, deduped });
        }
        Err(err) => warn!("failed to serialize {capability} StateSnapshot: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_equal_payload_is_not_pushed_again_except_for_icon_spools() {
        let registry = socket::GenerationRegistry::default();
        let mut last_snapshots = HashMap::new();
        let mut push = |capability, value: u32| {
            push_snapshot(&registry, 1, &mut last_snapshots, capability, &value);
            (last_snapshots[&capability].snapshot.revision, last_snapshots[&capability].deduped)
        };
        assert_eq!(push(Capability::Audio, 1), (1, 0));
        assert_eq!(push(Capability::Audio, 1), (1, 1), "an equal payload must not bump the revision");
        assert_eq!(push(Capability::Audio, 2), (2, 1));
        assert_eq!(push(Capability::Tray, 1), (1, 0));
        assert_eq!(push(Capability::Tray, 1), (2, 0), "a rewritten tray icon keeps its path and still needs a push");
    }
}

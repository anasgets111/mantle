//! Snapshot-push path (ADR-0037): serialize, send to the authoritative generation, and record in
//! `last_snapshots`, whose entry also carries the capability's revision.

use std::collections::HashMap;

use shared::{Capability, SupervisorFrame, warn};

use crate::{send_frame_logged, socket};

/// Bumps the revision, pushes `state` as a fresh `StateSnapshot`, and records it in
/// `last_snapshots` (ADR-0029), which `Supervisor::hydrate` replays to a new generation. A payload
/// equal to the last one is dropped: every push re-resolves the Renderer's scene (ADR-0044).
///
/// Taking [`Capability`] makes off-roster names unrepresentable (ADR-0076).
pub(crate) fn push_snapshot(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    last_snapshots: &mut HashMap<Capability, shared::StateSnapshot>,
    capability: Capability,
    state: &impl serde::Serialize,
) {
    match serde_json::to_value(state) {
        Ok(payload) => {
            // Tray and notification icons are rewritten in place at the same path, so an equal
            // payload can still mean new pixels for the Renderer to stat.
            let spools_icons = matches!(capability, Capability::Tray | Capability::Notifications);
            if !spools_icons && last_snapshots.get(&capability).is_some_and(|last| last.payload == payload) {
                return;
            }
            // ADR-0004's state version; the first push is `1`.
            let revision = last_snapshots.get(&capability).map_or(0, |last| last.revision) + 1;
            // Move the snapshot through the frame and take it back out. `send_frame_logged`
            // borrows, so the obvious spelling deep-clones the whole `payload` tree -- the largest
            // thing on this path -- on every signal, purely to keep a copy.
            let frame = SupervisorFrame::StateSnapshot(shared::StateSnapshot {
                capability: capability.to_string(),
                revision,
                payload,
            });
            send_frame_logged(registry, generation_id, &frame);
            if let SupervisorFrame::StateSnapshot(snapshot) = frame {
                last_snapshots.insert(capability, snapshot);
            }
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
            last_snapshots[&capability].revision
        };
        assert_eq!(push(Capability::Audio, 1), 1);
        assert_eq!(push(Capability::Audio, 1), 1, "an equal payload must not bump the revision");
        assert_eq!(push(Capability::Audio, 2), 2);
        assert_eq!(push(Capability::Tray, 1), 1);
        assert_eq!(push(Capability::Tray, 1), 2, "a rewritten tray icon keeps its path and still needs a push");
    }
}

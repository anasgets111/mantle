//! Snapshot-push path (ADR-0037): serialize, send to the authoritative generation, and record in
//! `last_snapshots`, whose entry also carries the capability's revision.

use std::collections::HashMap;

use shared::{Capability, SupervisorFrame, warn};

use crate::{send_frame_logged, socket};

/// A capability's last sent payload as JSON, plus how many pushes since start were dropped as equal
/// to the one before them. The revision counts the ones sent. Bytes, not a `Value`: the tree takes
/// about 7x its JSON on the heap, and every entry lives as long as the Supervisor.
pub(crate) struct Published {
    pub(crate) payload: Vec<u8>,
    pub(crate) revision: u32,
    pub(crate) deduped: u32,
}

/// Sends `published` as `capability`'s `StateSnapshot`, parsing it back for the frame.
pub(crate) fn send(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    capability: Capability,
    published: &Published,
) {
    match serde_json::from_slice(&published.payload) {
        Ok(payload) => {
            let snapshot = shared::StateSnapshot {
                capability: capability.as_str().to_owned(),
                revision: published.revision,
                payload,
            };
            send_frame_logged(registry, generation_id, &SupervisorFrame::StateSnapshot(snapshot));
        }
        Err(err) => warn!("failed to parse {capability}'s stored StateSnapshot: {err}"),
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
    match serde_json::to_vec(state) {
        Ok(payload) => {
            // Tray and notification icons are rewritten in place at the same path, so an equal
            // payload can still mean new pixels for the Renderer to stat.
            let spools_icons = matches!(capability, Capability::Tray | Capability::Notifications);
            if let Some(last) = last_snapshots.get_mut(&capability)
                && !spools_icons
                && last.payload == payload
            {
                last.deduped += 1;
                return;
            }
            // ADR-0004's state version; the first push is `1`.
            let (revision, deduped) =
                last_snapshots.get(&capability).map_or((1, 0), |last| (last.revision + 1, last.deduped));
            let published = Published { payload, revision, deduped };
            send(registry, generation_id, capability, &published);
            last_snapshots.insert(capability, published);
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
        let (tx, mut frames) = tokio::sync::mpsc::channel(8);
        registry.register(1, tx, Default::default());
        let mut last_snapshots = HashMap::new();
        let mut push = |capability, value: u32| {
            push_snapshot(&registry, 1, &mut last_snapshots, capability, &value);
            (last_snapshots[&capability].revision, last_snapshots[&capability].deduped)
        };
        assert_eq!(push(Capability::Audio, 1), (1, 0));
        assert_eq!(push(Capability::Audio, 1), (1, 1), "an equal payload must not bump the revision");
        assert_eq!(push(Capability::Audio, 2), (2, 1));
        assert_eq!(push(Capability::Tray, 1), (1, 0));
        assert_eq!(push(Capability::Tray, 1), (2, 0), "a rewritten tray icon keeps its path and still needs a push");
        let sent: Vec<_> = std::iter::from_fn(|| frames.try_recv().ok())
            .map(|bytes| match serde_json::from_slice(&bytes).unwrap() {
                SupervisorFrame::StateSnapshot(s) => format!("{} {} {}", s.capability, s.revision, s.payload),
                other => panic!("pushed {other:?}"),
            })
            .collect();
        assert_eq!(sent, ["audio 1 1", "audio 2 2", "tray 1 1", "tray 2 1"], "each frame carries the stored bytes");
    }
}

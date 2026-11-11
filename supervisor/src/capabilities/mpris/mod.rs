//! Media players (`mantle.mpris`, ADR-0036).
//!
//! Supervisor-owned session-bus MPRIS discovery and zero-polling progress state, so `mpris.players`
//! survives Renderer crash/reload like idle/lock authority (ADR-0010). Capture position once with
//! a monotonic timestamp and interpolate elapsed time client-side; read live position only when
//! seeking. Hand-written proxies (`proxies.rs`), a live `HashMap<bus_name, entry>` registry with
//! one forwarder per player (`player.rs`), controller dispatch (`controller.rs`), and pure
//! parsing/comparison helpers in `metadata.rs` and `position.rs`, unit-testable without a live
//! D-Bus connection, make the boundaries explicit.
//!
//! Players never register; discovery is active (`watcher.rs`): scan `ListNames` once, then watch
//! `NameOwnerChanged` for `org.mpris.MediaPlayer2.` arrivals and departures. ADR-0036 also fixes
//! `playerctld` exclusion, album-art trust checks, track-identity caching, and `SetPosition`'s
//! `TrackId` fallback.

pub mod collections;
pub mod controller;
pub mod metadata;
pub mod player;
mod position;
pub mod proxies;
pub mod watcher;

pub use controller::MprisController;
use shared::action::MprisAction;

/// Cap on each string a player supplies besides its URLs: metadata text, `Identity`,
/// `DesktopEntry`, track and playlist ids, playlist names and icons, orderings. Any session-bus
/// application can be a player, and this cap times every list's limit is what keeps `mantle.mpris`
/// within one frame (`the_largest_mpris_snapshot_fits_in_one_frame`). A tray title's cap; a
/// 100-character YouTube title fits unless it is mostly CJK.
const MAX_METADATA_TEXT_BYTES: usize = 256;

/// Cap on the current track's title, artists, album, album artists and genres: one of each per
/// player, so it can be looser than [`MAX_METADATA_TEXT_BYTES`], which lists multiply.
const MAX_TRACK_TEXT_BYTES: usize = 1024;

/// Cap on `mpris:artUrl` and `xesam:url`: `PATH_MAX`, room for any local `file://` art. A longer
/// URL is dropped, not cut: a cut one names a different file or page.
const MAX_METADATA_URL_BYTES: usize = 4096;

/// Whether `text` fits [`MAX_METADATA_TEXT_BYTES`]. An id or URI the player supplies is dropped past
/// it rather than cut: a cut one names nothing.
fn fits_text_cap(text: &str) -> bool {
    text.len() <= MAX_METADATA_TEXT_BYTES
}

/// ponytail: at most 8 players. A session client owning 8 player names (a flood of throwaway
/// names, or one per tab) keeps every other player out until one of those names leaves.
/// Upgrade: a per-owner limit.
const MAX_PLAYERS: usize = 8;

/// Playlist orderings kept. MPRIS defines five; the rest are a player's own.
const MAX_ORDERINGS: usize = 16;

/// `mantle.mpris` action dispatch (ADR-0037): queues each write action on its player's own task
/// (ADR-0036/ADR-0029). Seeks are unclamped here; each command clamps, or declines to, where it runs.
pub fn dispatch(controller: &MprisController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<MprisAction>(&envelope.params) else { return };
    // Every action's first argument is the player id.
    let Some(id) = envelope.params.arguments.first().and_then(serde_json::Value::as_str) else { return };
    let (queue, controller) = (controller, controller.clone());
    let command = async move {
        match action {
            MprisAction::Control { id, cmd } => controller.control(&id, cmd).await,
            MprisAction::Seek { id, position_us } => controller.seek(&id, position_us).await,
            MprisAction::SeekRelative { id, offset_us } => controller.seek_relative(&id, offset_us).await,
            MprisAction::Raise { id } => controller.raise(&id).await,
            MprisAction::Quit { id } => controller.quit(&id).await,
            MprisAction::OpenUri { id, uri } => controller.open_uri(&id, &uri).await,
            MprisAction::SetVolume { id, value } => controller.set_volume(&id, value).await,
            MprisAction::SetLoopStatus { id, value } => controller.set_loop_status(&id, value).await,
            MprisAction::SetShuffle { id, value } => controller.set_shuffle(&id, value).await,
            MprisAction::SetRate { id, value } => controller.set_rate(&id, value).await,
            MprisAction::TrackListAddTrack { id, uri, after_track, set_as_current } => {
                controller.track_list_add_track(&id, &uri, &after_track, set_as_current).await
            }
            MprisAction::TrackListRemoveTrack { id, track_id } => {
                controller.track_list_remove_track(&id, &track_id).await
            }
            MprisAction::TrackListGoTo { id, track_id } => controller.track_list_go_to(&id, &track_id).await,
            MprisAction::PlaylistsGet { id, index, count, order, reverse } => {
                controller.playlists_get(&id, index, count, &order, reverse).await
            }
            MprisAction::PlaylistsActivate { id, playlist_id } => {
                controller.playlists_activate(&id, &playlist_id).await
            }
        }
    };
    queue.enqueue(id, Box::pin(command));
}

#[cfg(test)]
mod tests {
    use shared::state::mpris::{
        LoopStatus, MprisState, PlayState, PlayerState, PlaylistSummary, PlaylistsState, TrackListState, TrackSummary,
    };

    use super::*;
    use crate::capabilities::test_support::{snapshot_frame_len, worst_case_text};

    /// The proof that no set of players can push `mantle.mpris` past the frame cap: every list at its
    /// limit, every string at its cap in characters JSON escapes to six bytes.
    #[test]
    fn the_largest_mpris_snapshot_fits_in_one_frame() {
        let text = || worst_case_text(MAX_METADATA_TEXT_BYTES);
        let url = || worst_case_text(MAX_METADATA_URL_BYTES);
        let playlist = || PlaylistSummary { id: text(), name: text(), icon: text() };
        let track = TrackSummary { id: text(), title: text(), artist: text(), length: Some(i64::MAX) };
        let current = || worst_case_text(MAX_TRACK_TEXT_BYTES);
        // Every field spelled out, so a field added later fails to compile here until it is sized.
        let player = PlayerState {
            // A bus name is at most 255 bytes, none of them escaped.
            id: "p".repeat(255),
            identity: text(),
            play_state: PlayState::Playing,
            can_go_next: true,
            can_go_previous: true,
            can_seek: true,
            can_play: true,
            can_pause: true,
            can_raise: true,
            can_quit: true,
            volume: f64::MAX,
            loop_status: Some(LoopStatus::Playlist),
            shuffle: true,
            rate: f64::MAX,
            minimum_rate: f64::MAX,
            maximum_rate: f64::MAX,
            title: current(),
            artist: current(),
            album: current(),
            album_artist: current(),
            genre: current(),
            // Canonical, so at most `PATH_MAX`.
            album_art_path: worst_case_text(4096),
            art_url: url(),
            position: Some(i64::MIN),
            position_updated_at: i64::MIN,
            length: Some(i64::MAX),
            url: url(),
            desktop_entry: text(),
            pid: Some(u32::MAX),
            track_list: TrackListState {
                tracks: vec![track; collections::TRACK_LIMIT],
                current_track: text(),
                can_edit: true,
            },
            playlists: PlaylistsState {
                count: u32::MAX,
                orderings: vec![text(); MAX_ORDERINGS],
                active: Some(playlist()),
                index: u32::MAX,
                page_size: collections::PLAYLIST_PAGE_LIMIT,
                order: text(),
                reverse: true,
                playlists: vec![playlist(); collections::PLAYLIST_PAGE_LIMIT as usize],
            },
        };

        let len = snapshot_frame_len(shared::Capability::Mpris, &MprisState { players: vec![player; MAX_PLAYERS] });

        assert!(len < shared::framing::MAX_FRAME_LEN, "{len} bytes");
    }
}

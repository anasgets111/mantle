//! Media players (`mantle.mpris`, ADR-0036).
//!
//! Supervisor-owned session-bus MPRIS discovery and zero-polling progress state, so `mpris.players`
//! survives Renderer crash/reload like idle/lock authority (ADR-0010). Capture position once with
//! a monotonic timestamp and interpolate elapsed time client-side; read live position only when
//! seeking. Hand-written proxies (`proxies.rs`), a live `HashMap<bus_name, entry>` registry with
//! one forwarder per player (`player.rs`), controller dispatch (`controller.rs`), and pure
//! parsing/comparison helpers in `metadata.rs`, unit-testable without a live D-Bus connection,
//! make the boundaries explicit.
//!
//! Players never register; discovery is active (`watcher.rs`): scan `ListNames` once, then watch
//! `NameOwnerChanged` for `org.mpris.MediaPlayer2.` arrivals and departures. ADR-0036 also fixes
//! `playerctld` exclusion, album-art trust checks, track-identity caching, and `SetPosition`'s
//! `TrackId` fallback.

pub mod collections;
pub mod controller;
pub mod metadata;
pub mod player;
pub mod proxies;
pub mod watcher;

pub use controller::{MprisController, MprisSignal, PlayerCommand};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MprisAction {
    /// Sends a playback command to `players[].id`.
    Control { id: String, cmd: PlayerCommand },
    /// Seeks to an absolute position in microseconds, clamped to `[0, length]` (only `>= 0` when `length` is `-1`).
    Seek { id: String, position_us: i64 },
    /// Seeks by a signed offset in microseconds, unclamped; past the end may skip to the next track.
    SeekRelative { id: String, offset_us: i64 },
    /// Calls Raise; check `players[].can_raise` before calling.
    Raise { id: String },
    /// Calls Quit; check `players[].can_quit` before calling.
    Quit { id: String },
    /// Opens an absolute URI in the player.
    OpenUri { id: String, uri: String },
    /// Sets MPRIS `Volume`; finite values at or above zero are accepted.
    SetVolume { id: String, value: f64 },
    /// Sets MPRIS `LoopStatus` to `None`, `Track`, or `Playlist`.
    SetLoopStatus { id: String, value: String },
    /// Sets MPRIS `Shuffle`.
    SetShuffle { id: String, value: bool },
    /// Sets a positive finite MPRIS playback rate.
    SetRate { id: String, value: f64 },
    /// Inserts a URI after a track id, or after `/org/mpris/MediaPlayer2/TrackList/NoTrack` to prepend.
    TrackListAddTrack { id: String, uri: String, after_track: String, set_as_current: bool },
    /// Removes a track by its TrackList object path.
    TrackListRemoveTrack { id: String, track_id: String },
    /// Starts the track identified by its TrackList object path.
    TrackListGoTo { id: String, track_id: String },
    /// Reads a bounded playlist page into `players[].playlists`.
    PlaylistsGet { id: String, index: u32, count: u32, order: String, reverse: bool },
    /// Activates a playlist by its object path.
    PlaylistsActivate { id: String, playlist_id: String },
}

/// `mantle.mpris` action dispatch (ADR-0037): `tokio::spawn`s each write action
/// (ADR-0036/ADR-0029). Seeks are unclamped here; each command clamps, or declines to, where it runs.
pub fn dispatch(controller: &MprisController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<MprisAction>(&envelope.params) else { return };
    let controller = controller.clone();
    tokio::spawn(async move {
        match action {
            MprisAction::Control { id, cmd } => controller.control(&id, cmd).await,
            MprisAction::Seek { id, position_us } => controller.seek(&id, position_us).await,
            MprisAction::SeekRelative { id, offset_us } => controller.seek_relative(&id, offset_us).await,
            MprisAction::Raise { id } => controller.raise(&id).await,
            MprisAction::Quit { id } => controller.quit(&id).await,
            MprisAction::OpenUri { id, uri } => controller.open_uri(&id, &uri).await,
            MprisAction::SetVolume { id, value } => controller.set_volume(&id, value).await,
            MprisAction::SetLoopStatus { id, value } => controller.set_loop_status(&id, &value).await,
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
    });
}

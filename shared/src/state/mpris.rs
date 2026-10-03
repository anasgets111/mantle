//! `mantle.mpris` snapshot payload.

use serde::Serialize;

use crate::action::LoopStatus;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MprisState {
    /// Every controllable MPRIS player except `playerctld`, longest-running first, so `players[1]`
    /// stays put; empty when none runs.
    pub players: Vec<PlayerState>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlayerState {
    /// Bus-name suffix after `org.mpris.MediaPlayer2.`, e.g. `"spotify"`; every action takes it.
    pub id: String,
    /// Display name, e.g. `"Spotify"`; empty if unanswered.
    pub identity: String,
    /// MPRIS `PlaybackStatus`; keeps the last value when a read fails, `"Stopped"` if none.
    pub play_state: PlayState,
    /// MPRIS `CanGoNext`.
    pub can_go_next: bool,
    /// MPRIS `CanGoPrevious`.
    pub can_go_previous: bool,
    /// MPRIS `CanSeek`.
    pub can_seek: bool,
    /// MPRIS `CanPlay`.
    pub can_play: bool,
    /// MPRIS `CanPause`.
    pub can_pause: bool,
    /// MPRIS `CanRaise` on the root interface.
    pub can_raise: bool,
    /// MPRIS `CanQuit` on the root interface.
    pub can_quit: bool,
    /// MPRIS volume in percent, `100` when unreported. The protocol permits amplification above `100`.
    pub volume: f64,
    /// MPRIS loop mode; `nil` when the player does not report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_status: Option<LoopStatus>,
    /// MPRIS shuffle setting.
    pub shuffle: bool,
    /// MPRIS playback rate.
    pub rate: f64,
    /// MPRIS minimum playback rate, or `0` when unavailable.
    pub minimum_rate: f64,
    /// MPRIS maximum playback rate, or `0` when unavailable.
    pub maximum_rate: f64,
    /// Track title; empty when unset, normal between tracks.
    pub title: String,
    /// Artists joined with `", "`; empty when unset.
    pub artist: String,
    /// Album title; empty when unset.
    pub album: String,
    /// Album artists joined with `", "`; empty when unset.
    pub album_artist: String,
    /// Genres joined with `", "`; empty when unset.
    pub genre: String,
    /// Cover art as an existing local path, or empty when unavailable.
    pub album_art_path: String,
    /// Playback offset in microseconds as of `position_updated_at`, not polled while playing: add
    /// elapsed time. `nil` when unknown (ADR-0036).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    /// `CLOCK_MONOTONIC` microseconds when `position` was read. No Lua clock shares this epoch
    /// (not `mantle.system.monotonic`); only compare it with itself.
    pub position_updated_at: i64,
    /// Track length in microseconds, or `nil` when unknown, as for a live stream (ADR-0036).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<i64>,
    /// `xesam:url` as sent, e.g. a `file://` path or an `https://` page; empty when unset (ADR-0137).
    pub url: String,
    /// The player's `.desktop` basename, e.g. `"firefox"`, for app matching; empty when unset.
    pub desktop_entry: String,
    /// Nearby tracks from the optional MPRIS TrackList interface.
    pub track_list: TrackListState,
    /// One bounded page from the optional MPRIS Playlists interface.
    pub playlists: PlaylistsState,
}

/// MPRIS `PlaybackStatus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PlayState {
    Playing,
    Paused,
    #[default]
    Stopped,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrackSummary {
    /// TrackList object path, used by `track_list_go_to` and `track_list_remove_track`.
    pub id: String,
    /// Track title, empty when the player has none.
    pub title: String,
    /// Track artists joined with `", "`.
    pub artist: String,
    /// Track length in microseconds, or `nil` when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrackListState {
    /// At most 100 tracks around the current track; the full playlist remains player-owned.
    pub tracks: Vec<TrackSummary>,
    /// Current TrackList object path, or empty if unknown.
    pub current_track: String,
    /// Whether the player permits add and remove calls.
    pub can_edit: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlaylistSummary {
    /// Stable playlist object path.
    pub id: String,
    /// User-facing playlist name.
    pub name: String,
    /// Icon URI, or empty if absent.
    pub icon: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlaylistsState {
    /// Total playlists reported by the player.
    pub count: u32,
    /// Ordering names accepted by `playlists_get`.
    pub orderings: Vec<String>,
    /// Active playlist, if the player reports one.
    pub active: Option<PlaylistSummary>,
    /// Index used for the current page.
    pub index: u32,
    /// Number of entries requested for the current page, at most 100.
    pub page_size: u32,
    /// Ordering used for the current page.
    pub order: String,
    /// Whether the page is reversed.
    pub reverse: bool,
    /// At most 100 playlist entries.
    pub playlists: Vec<PlaylistSummary>,
}

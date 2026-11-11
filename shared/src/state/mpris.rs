//! `mantle.mpris` snapshot payload.

use serde::{Deserialize, Serialize};

/// MPRIS `LoopStatus`, also the `set_loop_status` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LoopStatus {
    /// Plays through once.
    None,
    /// Repeats the current track.
    Track,
    /// Repeats the playlist.
    Playlist,
}

impl LoopStatus {
    /// The MPRIS wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Track => "Track",
            Self::Playlist => "Playlist",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MprisState {
    /// Controllable MPRIS players except `playerctld`, longest-running first, so `players[1]`
    /// stays put; empty when none runs. At most 8: one arriving past that is listed once another leaves.
    pub players: Vec<PlayerState>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlayerState {
    /// Bus-name suffix after `org.mpris.MediaPlayer2.`, e.g. `"spotify"`; every action takes it.
    pub id: String,
    /// Display name, e.g. `"Spotify"`; empty if unanswered. Cut at 256 bytes.
    pub identity: String,
    /// MPRIS `PlaybackStatus`; keeps the last value when a read fails, `"stopped"` if none.
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
    /// Track title; empty when unset, normal between tracks. Cut at 1024 bytes, like the four below.
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
    /// `mpris:artUrl` as sent, e.g. an `https://` image to fetch and cache; empty when unset.
    /// One over 4096 bytes is ignored: the same track keeps its previous one, a new track has none.
    pub art_url: String,
    /// Playback offset in microseconds as of `position_updated_at`, not polled while playing: add
    /// elapsed time. `nil` when unknown (ADR-0036).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    /// `CLOCK_MONOTONIC` microseconds when `position` was read: the clock of `mantle.clock:now()`
    /// in seconds, so `mantle.clock:now() - position_updated_at / 1e6` is the age.
    pub position_updated_at: i64,
    /// Track length in microseconds, or `nil` when unknown, as for a live stream (ADR-0036).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<i64>,
    /// `xesam:url` as sent, e.g. a `file://` path or an `https://` page; empty when unset (ADR-0137).
    pub url: String,
    /// The player's `.desktop` basename, e.g. `"firefox"`, for app matching; empty when unset. Cut at 256 bytes.
    pub desktop_entry: String,
    /// Process id of the bus name's owner, which `mantle.audio.apps[].pid` matches exactly; `nil`
    /// when the bus refuses. A sandboxed app's streams may report a pid from inside its sandbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Nearby tracks from the optional MPRIS TrackList interface.
    pub track_list: TrackListState,
    /// One bounded page from the optional MPRIS Playlists interface.
    pub playlists: PlaylistsState,
}

/// MPRIS `PlaybackStatus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PlayState {
    Playing,
    Paused,
    #[default]
    Stopped,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrackSummary {
    /// TrackList object path, used by `track_list_go_to` and `track_list_remove_track`. A track
    /// whose id is over 256 bytes is left out of `tracks`.
    pub id: String,
    /// Track title, empty when the player has none. Cut at 256 bytes, like `artist`.
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
    /// Current TrackList object path, or empty if unknown or over 256 bytes.
    pub current_track: String,
    /// Whether the player permits add and remove calls.
    pub can_edit: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlaylistSummary {
    /// Stable playlist object path.
    pub id: String,
    /// User-facing playlist name, cut at 256 bytes.
    pub name: String,
    /// Icon URI, or empty if absent or over 256 bytes.
    pub icon: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlaylistsState {
    /// Total playlists reported by the player.
    pub count: u32,
    /// Ordering names accepted by `playlists_get`: at most 16, names over 256 bytes left out.
    pub orderings: Vec<String>,
    /// Active playlist, if the player reports one; `nil` too when its id is over 256 bytes.
    pub active: Option<PlaylistSummary>,
    /// Index used for the current page.
    pub index: u32,
    /// Number of entries requested for the current page, at most 100.
    pub page_size: u32,
    /// Ordering used for the current page.
    pub order: String,
    /// Whether the page is reversed.
    pub reverse: bool,
    /// At most 100 playlist entries. One whose id is over 256 bytes is left out, so a page can be
    /// shorter than `page_size` while `count` includes it.
    pub playlists: Vec<PlaylistSummary>,
}

//! [`MprisController`]: `mantle.mpris`'s write dispatcher and state owner.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use shared::debug;
use tokio::sync::mpsc::UnboundedSender;

use super::collections::{PlaylistsState, read_playlists};
use super::metadata::clamp_seek_target;
use super::player::PlayerState;
use super::proxies::{MprisPlaylistsProxy, MprisTrackListProxy};
use super::watcher::{service_name_for_id, spawn_discovery};
use shared::action::PlayerCommand;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct MprisState {
    /// Every controllable MPRIS player except `playerctld`, longest-running first, so `players[1]`
    /// stays put; empty when none runs.
    pub players: Vec<PlayerState>,
}

/// Every command here fails the same one way, and only into a `debug!`. An enum with `Display`
/// and `Error` impls bought nothing a constant does not: nothing matches on it and nothing returns
/// it.
const UNKNOWN_PLAYER: &str = "no MPRIS player with that id is currently tracked";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
static NEXT_PLAYLIST_REQUEST: AtomicU64 = AtomicU64::new(1);

/// No `events` field, unlike `TrayController` (ADR-0031): `control`/`seek`/`seek_relative` issue
/// real D-Bus calls and never self-send a signal (ADR-0036); the next player event triggers the
/// push. The channel belongs to [`super::watcher::spawn_discovery`] and is consumed at construction.
#[derive(Clone)]
pub struct MprisController {
    registry: super::player::PlayerRegistry,
    events: UnboundedSender<()>,
}

impl MprisController {
    /// Spawns discovery (`ListNames`, then `NameOwnerChanged`) on the session bus. Returns
    /// immediately; discovery fills the registry in background tasks.
    pub fn new(connection: zbus::Connection, events: UnboundedSender<()>) -> Self {
        let registry: super::player::PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(spawn_discovery(connection, registry.clone(), events.clone()));
        Self { registry, events }
    }

    /// Inert controller with an empty registry and no discovery task, used when the session-bus
    /// connection cannot be established.
    pub fn inert() -> Self {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        Self { registry: Arc::new(Mutex::new(HashMap::new())), events }
    }

    /// Re-derives `mpris.players` from the full registry. Synchronous because forwarders update
    /// each entry's `last_known` before waking `main.rs`.
    pub fn snapshot(&self) -> MprisState {
        MprisState { players: super::player::ordered_players(&self.registry) }
    }

    pub async fn track_list_add_track(&self, id: &str, uri: &str, after: &str, set_as_current: bool) {
        let Some(proxy) = self.find_track_list(id) else {
            debug!("TrackList AddTrack for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        if !matches!(proxy.can_edit_tracks().await, Ok(true)) {
            debug!("TrackList AddTrack for {id:?} ignored because CanEditTracks is false or unavailable");
            return;
        }
        let Ok(after) = zbus::zvariant::ObjectPath::try_from(after) else {
            debug!("TrackList AddTrack for {id:?} ignored because after_track is not an object path");
            return;
        };
        if let Err(err) = proxy.add_track(uri, after, set_as_current).await {
            debug!("TrackList AddTrack for {id:?} failed: {err}");
        }
    }

    pub async fn track_list_remove_track(&self, id: &str, track_id: &str) {
        let Some(proxy) = self.find_track_list(id) else {
            debug!("TrackList RemoveTrack for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        if track_id == NO_TRACK {
            debug!("TrackList RemoveTrack for {id:?} ignored because NoTrack is not removable");
            return;
        }
        let Ok(track_id) = zbus::zvariant::ObjectPath::try_from(track_id) else {
            debug!("TrackList RemoveTrack for {id:?} ignored because track_id is not an object path");
            return;
        };
        if !matches!(proxy.can_edit_tracks().await, Ok(true)) {
            debug!("TrackList RemoveTrack for {id:?} ignored because CanEditTracks is false or unavailable");
            return;
        }
        if let Err(err) = proxy.remove_track(track_id).await {
            debug!("TrackList RemoveTrack for {id:?} failed: {err}");
        }
    }

    pub async fn track_list_go_to(&self, id: &str, track_id: &str) {
        let Some(proxy) = self.find_track_list(id) else {
            debug!("TrackList GoTo for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        if track_id == NO_TRACK {
            debug!("TrackList GoTo for {id:?} ignored because NoTrack is not a track");
            return;
        }
        let Ok(track_id) = zbus::zvariant::ObjectPath::try_from(track_id) else {
            debug!("TrackList GoTo for {id:?} ignored because track_id is not an object path");
            return;
        };
        if let Err(err) = proxy.go_to(track_id).await {
            debug!("TrackList GoTo for {id:?} failed: {err}");
        }
    }

    pub async fn playlists_get(&self, id: &str, index: u32, count: u32, order: &str, reverse: bool) {
        let bus_name = service_name_for_id(id);
        let (proxy, request) = {
            let mut guard = self.registry.lock().expect("mutex poisoned");
            let Some(entry) = guard.get_mut(&bus_name) else {
                debug!("GetPlaylists for {id:?} failed: {}", UNKNOWN_PLAYER);
                return;
            };
            let Some(proxy) = entry.playlists.clone() else {
                debug!("GetPlaylists for {id:?} failed: {}", UNKNOWN_PLAYER);
                return;
            };
            let request = NEXT_PLAYLIST_REQUEST.fetch_add(1, Ordering::Relaxed);
            entry.playlist_request = request;
            entry.playlist_revision += 1;
            (proxy, request)
        };
        let state = read_playlists(&proxy, index, count, order, reverse, false).await;
        if let Err(err) = &state {
            debug!("GetPlaylists for {id:?} failed: {err}");
        }
        self.store_playlists(&bus_name, request, state.ok());
    }

    pub async fn playlists_activate(&self, id: &str, playlist_id: &str) {
        let Some(proxy) = self.find_playlists(id) else {
            debug!("ActivatePlaylist for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        let Ok(playlist_id) = zbus::zvariant::ObjectPath::try_from(playlist_id) else {
            debug!("ActivatePlaylist for {id:?} ignored because playlist_id is not an object path");
            return;
        };
        if let Err(err) = proxy.activate_playlist(playlist_id).await {
            debug!("ActivatePlaylist for {id:?} failed: {err}");
        }
    }

    fn find_track_list(&self, id: &str) -> Option<MprisTrackListProxy<'static>> {
        let bus_name = service_name_for_id(id);
        self.registry.lock().expect("mutex poisoned").get(&bus_name).and_then(|entry| entry.track_list.clone())
    }

    fn find_playlists(&self, id: &str) -> Option<MprisPlaylistsProxy<'static>> {
        let bus_name = service_name_for_id(id);
        self.registry.lock().expect("mutex poisoned").get(&bus_name).and_then(|entry| entry.playlists.clone())
    }

    fn store_playlists(&self, bus_name: &str, request: u64, state: Option<PlaylistsState>) {
        let mut guard = self.registry.lock().expect("mutex poisoned");
        if let Some(entry) = guard.get_mut(bus_name)
            && entry.playlist_request == request
        {
            entry.playlist_request = 0;
            if let Some(state) = state
                && entry.last_known.playlists != state
            {
                entry.last_known.playlists = state;
                let _ = self.events.send(());
            }
        }
    }

    pub async fn control(&self, id: &str, cmd: PlayerCommand) {
        debug!("control: player={id:?} cmd={cmd:?}");
        let Some(player) = self.find_player(id) else {
            debug!("send_command({id:?}, {cmd:?}) failed: {}", UNKNOWN_PLAYER);
            return;
        };
        let result = match cmd {
            PlayerCommand::Play => player.play().await,
            PlayerCommand::Pause => player.pause().await,
            PlayerCommand::PlayPause => player.play_pause().await,
            PlayerCommand::Next => player.next().await,
            PlayerCommand::Previous => player.previous().await,
            PlayerCommand::Stop => player.stop().await,
        };
        if let Err(err) = result {
            debug!("send_command({id:?}, {cmd:?}) failed: {err}");
        }
    }

    pub async fn raise(&self, id: &str) {
        self.call_root(id, true).await;
    }
    pub async fn quit(&self, id: &str) {
        self.call_root(id, false).await;
    }

    async fn call_root(&self, id: &str, raise: bool) {
        let Some(root) = self.find_root(id) else {
            debug!("root action for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        let result = if raise { root.raise().await } else { root.quit().await };
        if let Err(err) = result {
            debug!("root action for {id:?} failed: {err}");
        }
    }

    pub async fn open_uri(&self, id: &str, uri: &str) {
        if !valid_uri(uri) {
            debug!("OpenUri rejected an invalid URI for {id:?}");
            return;
        }
        let Some(player) = self.find_player(id) else {
            debug!("OpenUri for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        if let Err(err) = player.open_uri(uri).await {
            debug!("OpenUri for {id:?} failed: {err}");
        }
    }

    pub async fn set_volume(&self, id: &str, value: f64) {
        if !value.is_finite() || value < 0.0 {
            debug!("invalid volume for {id:?}");
            return;
        }
        if let Some(p) = self.find_player(id)
            && let Err(err) = p.set_volume(value).await
        {
            debug!("set volume for {id:?} failed: {err}");
        }
    }
    pub async fn set_rate(&self, id: &str, value: f64) {
        let state = self.find_state(id);
        let within_limits = state.as_ref().is_none_or(|s| {
            let minimum = s.minimum_rate;
            let maximum = s.maximum_rate;
            (minimum <= 0.0 || value >= minimum) && (maximum <= 0.0 || value <= maximum)
        });
        if !value.is_finite() || value <= 0.0 || !within_limits {
            debug!("invalid or unsupported rate for {id:?}");
            return;
        }
        if let Some(p) = self.find_player(id)
            && let Err(err) = p.set_rate(value).await
        {
            debug!("set rate for {id:?} failed: {err}");
        }
    }
    pub async fn set_shuffle(&self, id: &str, value: bool) {
        if let Some(p) = self.find_player(id)
            && let Err(err) = p.set_shuffle(value).await
        {
            debug!("set shuffle for {id:?} failed: {err}");
        }
    }
    pub async fn set_loop_status(&self, id: &str, value: &str) {
        if !matches!(value, "None" | "Track" | "Playlist") {
            debug!("invalid LoopStatus for {id:?}");
            return;
        }
        if let Some(p) = self.find_player(id)
            && let Err(err) = p.set_loop_status(value).await
        {
            debug!("set LoopStatus for {id:?} failed: {err}");
        }
    }

    /// `mpris:seek_relative(id, off)`: MPRIS `Seek`, which is relative already.
    ///
    /// Never a live `Position` plus `SetPosition`: `resolve_position`'s protection does not reach
    /// that read, and Firefox answers `Position` with `0` for seconds after any seek, so "forward
    /// five seconds" would jump to the start of the track.
    ///
    /// No clamping: the player owns its own endpoints, and MPRIS lets a `Seek` past the end move
    /// to the next track. Clamping here would need a length we may not have (ADR-0036) and would
    /// silently differ from what every other MPRIS client does.
    pub async fn seek_relative(&self, id: &str, off: i64) {
        let Some(player) = self.find_player(id) else {
            debug!("seek_relative({id:?}, {off}) failed: {}", UNKNOWN_PLAYER);
            return;
        };
        if let Err(err) = player.seek(off).await {
            debug!("seek_relative({id:?}, {off}) failed: {err}");
        }
    }

    /// Converts an absolute `target` into the relative `Seek` a player without a usable trackid
    /// needs. Refuses rather than inventing an origin: an unknown position read as `0` seeks to
    /// `target` from the start, and a never-read `-1` overflows the subtraction for a large target.
    async fn seek_by_difference(
        &self,
        id: &str,
        player: &super::proxies::MprisPlayerProxy<'static>,
        target: i64,
    ) -> zbus::Result<()> {
        let Some(position) = self.live_position(id).await.filter(|position| *position >= 0) else {
            debug!("seek to {target} for {id:?} needs a position to convert against and has none");
            return Ok(());
        };
        let Some(offset) = target.checked_sub(position) else {
            debug!("seek to {target} for {id:?} does not fit an i64 offset from {position}");
            return Ok(());
        };
        player.seek(offset).await
    }

    /// A live `Position` read, not the cached snapshot.
    async fn live_position(&self, id: &str) -> Option<i64> {
        let player = self.find_player(id)?;
        match player.position().await {
            Ok(position) => Some(position),
            Err(err) => {
                debug!("live Position read failed for {id:?}, falling back to the cached value: {err}");
                self.cached_position(id)
            }
        }
    }

    /// `mpris:seek(id, pos_us)`: `SetPosition(cached trackid, clamped pos_us)` when available;
    /// otherwise relative `Seek` from the last known position because some players never report
    /// `mpris:trackid` (ADR-0036). State waits for real `Seeked`/`PropertiesChanged` signals.
    pub async fn seek(&self, id: &str, target_us: i64) {
        let Some(context) = self.find_seek_context(id) else {
            debug!("seek to {target_us} for {id:?} failed: {}", UNKNOWN_PLAYER);
            return;
        };
        let target = clamp_seek_target(target_us, context.length);
        let result = match context.trackid {
            Some(trackid) => match zbus::zvariant::ObjectPath::try_from(trackid.as_str()) {
                Ok(path) => context.player.set_position(path, target).await,
                Err(err) => {
                    debug!(
                        "cached trackid {trackid:?} for {} isn't a valid object path, falling back to relative Seek: {err}",
                        context.bus_name
                    );
                    self.seek_by_difference(id, &context.player, target).await
                }
            },
            // Some players never report `mpris:trackid`; `Player.Seek` takes a relative offset, so
            // the absolute target has to be converted against a live position.
            None => self.seek_by_difference(id, &context.player, target).await,
        };
        if let Err(err) = result {
            debug!("seek to {target} for {id:?} failed: {err}");
        }
    }

    fn find_player(&self, id: &str) -> Option<super::proxies::MprisPlayerProxy<'static>> {
        let bus_name = service_name_for_id(id);
        self.registry.lock().expect("mutex poisoned").get(&bus_name).map(|entry| entry.player.clone())
    }

    fn find_root(&self, id: &str) -> Option<super::proxies::MprisRootProxy<'static>> {
        let bus_name = service_name_for_id(id);
        let guard = self.registry.lock().expect("mutex poisoned");
        let entry = guard.get(&bus_name)?;
        Some(entry.root.clone())
    }

    fn cached_position(&self, id: &str) -> Option<i64> {
        let bus_name = service_name_for_id(id);
        self.registry.lock().expect("mutex poisoned").get(&bus_name).map(|entry| entry.last_known.position)
    }

    fn find_state(&self, id: &str) -> Option<PlayerState> {
        let bus_name = service_name_for_id(id);
        self.registry.lock().expect("mutex poisoned").get(&bus_name).map(|entry| entry.last_known.clone())
    }

    fn find_seek_context(&self, id: &str) -> Option<SeekContext> {
        let bus_name = service_name_for_id(id);
        let guard = self.registry.lock().expect("mutex poisoned");
        let entry = guard.get(&bus_name)?;
        Some(SeekContext {
            bus_name: bus_name.clone(),
            player: entry.player.clone(),
            trackid: entry.cached_trackid.clone(),
            length: entry.last_known.length,
        })
    }
}

fn valid_uri(uri: &str) -> bool {
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && !scheme.is_empty()
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        && uri.is_ascii()
        && !uri.bytes().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && uri.bytes().enumerate().all(|(i, byte)| {
            byte != b'%'
                || uri
                    .as_bytes()
                    .get(i + 1..i + 3)
                    .is_some_and(|escape| escape.len() == 2 && escape.iter().all(u8::is_ascii_hexdigit))
        })
}

/// [`MprisController::find_seek_context`]'s answer.
struct SeekContext {
    bus_name: String,
    player: super::proxies::MprisPlayerProxy<'static>,
    trackid: Option<String>,
    length: i64,
}

#[cfg(test)]
mod tests {
    use super::valid_uri;

    #[test]
    fn open_uri_requires_an_absolute_uri_without_control_characters() {
        for uri in ["https://example.test/track", "file:///tmp/song.flac", "spotify:track:abc"] {
            assert!(valid_uri(uri), "{uri:?}");
        }
        for uri in ["", "relative/path", ":missing-scheme", "1bad:value", "https://bad.test/\n", "https://bad.test/%Q0"]
        {
            assert!(!valid_uri(uri), "{uri:?}");
        }
    }
}

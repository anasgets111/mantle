//! Per-player registry: bind a discovered MPRIS name, hydrate one live [`PlayerState`] entry,
//! and maintain it in a resync loop.

pub use shared::state::mpris::PlayerState;

use crate::capabilities::scale::percent_from_fraction;
use shared::action::LoopStatus;
use shared::state::mpris::PlayState;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{FutureExt, StreamExt};
use shared::debug;
use tokio::task::JoinHandle;

use super::collections::{TrackListState, read_playlists, read_track_list, spawn_collections_forwarder};
use super::metadata::{TrackIdentity, parse_metadata, resolve_album_art_path};
use super::proxies::{
    MprisPlayerProxy, MprisPlaylistsProxy, MprisRootProxy, MprisTrackListProxy, bind_player, bind_playlists, bind_root,
    bind_track_list,
};
use super::watcher::player_id;
use tokio::sync::mpsc::UnboundedSender;

/// Allocates [`PlayerEntry::registered`] on the same terms as the tray counter.
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(0);

pub(super) struct PlayerEntry {
    pub(super) player: MprisPlayerProxy<'static>,
    pub(super) root: MprisRootProxy<'static>,
    pub(super) track_list: Option<MprisTrackListProxy<'static>>,
    pub(super) playlists: Option<MprisPlaylistsProxy<'static>>,
    pub(super) last_known: PlayerState,
    /// First-seen order used by [`ordered_players`]. HashMap order once leaked into
    /// `mpris.players`, letting `players[1]` swap on an unrelated tick.
    registered: u64,
    track_identity: TrackIdentity,
    /// Cached `mpris:trackid` for `SetPosition` (ADR-0036); internal, not pushed to Lua.
    pub(super) cached_trackid: Option<String>,
    pub(super) playlist_revision: u64,
    pub(super) playlist_request: u64,
    /// `None` only between insertion and forwarder spawn; the first resync can fire immediately on
    /// subscribe, so insertion always comes first.
    forwarder: Option<JoinHandle<()>>,
    collections_forwarder: Option<JoinHandle<()>>,
}

pub(super) type PlayerRegistry = Arc<Mutex<HashMap<String, PlayerEntry>>>;

/// `mpris.players`, longest-running first by appearance, not [`PlayerState::id`]: `players[1]`
/// means the player that has been there, not a newly playing tab after an alphabetical reorder.
pub(super) fn ordered_players(registry: &PlayerRegistry) -> Vec<PlayerState> {
    let guard = registry.lock().expect("mpris registry mutex poisoned");
    let mut entries: Vec<&PlayerEntry> = guard.values().collect();
    entries.sort_by_key(|entry| entry.registered);
    entries.into_iter().map(|entry| entry.last_known.clone()).collect()
}

/// How long after a `PlaybackStatus` change to read `Position` again.
const POSITION_RECHECK_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Cross-process-comparable `CLOCK_MONOTONIC` microseconds, matching the IDL's timestamp field;
/// opaque `std::time::Instant` would not. Known gap (ADR-0036): `system.time` is
/// 1Hz, too coarse for this resolution.
pub(super) fn monotonic_micros() -> i64 {
    let now: std::time::Duration = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(std::time::Duration::from)
        .unwrap_or_default();
    i64::try_from(now.as_micros()).unwrap_or(i64::MAX)
}

struct Resynced {
    state: PlayerState,
    identity: TrackIdentity,
    trackid: Option<String>,
}

fn resolved_player_identity(read: zbus::Result<String>, previous: Option<&PlayerState>, bus_name: &str) -> String {
    match read {
        Ok(identity) => identity,
        Err(err) => {
            debug!("Identity read failed for {bus_name}; keeping the last known value this round: {err}");
            previous.map(|state| state.identity.clone()).unwrap_or_default()
        }
    }
}

/// `resync`'s fallback fields: the three cached values relevant to degradation, not
/// `player`/`forwarder`, which it never changes. Owned, so a fallback moves rather than clones.
#[derive(Default)]
struct Previous {
    state: PlayerState,
    identity: TrackIdentity,
    trackid: Option<String>,
}

/// The `Position` to publish, and the `CLOCK_MONOTONIC` moment it was read.
///
/// Two answers must not be believed. A **failed** read means the player did not answer, not that the
/// track is at zero (ADR-0036). A **zero** on a track we were already minutes into means the same
/// thing: Firefox and zen answer `Position` with `0` for several seconds after any `SetPosition` or
/// `Seek`, then report the true offset again once they catch up. Measured by seeking to 340s and
/// reading `Position` back as 340s, then `0`, then the true 363s while `PlaybackStatus` stayed
/// `Playing` throughout. Publishing that zero restarts every progress bar at the start of the track
/// while the video plays on.
///
/// Either way the last real reading is kept **with its own timestamp**. A stale position under a
/// fresh stamp tells a client extrapolating from the pair that the track jumped backwards, which is
/// worse than either half alone. This is the retention `album_art_path` and `length` already do
/// across a same-track update, for the same reason: the player stopped describing something it had
/// not actually changed.
///
/// A zero on a track we were *not* already inside is published, because that is where a new one
/// begins.
///
/// An unchanged reading under an unchanged `play_state` keeps its stamp too, so a re-sent
/// `Metadata` from a paused player compares equal and is not pushed. A changed `play_state`
/// restamps: extrapolation restarts from the moment playback resumed.
fn resolve_position(
    read: zbus::Result<i64>,
    previous: Option<&PlayerState>,
    same_track: bool,
    play_state: PlayState,
    bus_name: &str,
) -> (Option<i64>, i64) {
    let last = previous.map(|p| (p.position, p.position_updated_at));
    let settled = previous.is_some_and(|p| p.play_state == play_state);
    match read {
        Ok(0) if same_track && matches!(last, Some((Some(position), _)) if position > 0) => last.unwrap_or((None, 0)),
        Ok(position) if same_track && settled && matches!(last, Some((known, _)) if known == Some(position)) => {
            last.unwrap_or((None, 0))
        }
        // A negative offset is a player bug, unknown like an unread one, as `length` treats it.
        Ok(position) => (Some(position).filter(|p| *p >= 0), monotonic_micros()),
        Err(err) => {
            debug!("Position read failed for {bus_name}; keeping the last known reading this round: {err}");
            // Only the track the reading belongs to. Publishing the previous track's offset under
            // the new one's metadata is worse than admitting we do not know: a 30-second track
            // would inherit a 5:40 position and every bar would draw it past its own end.
            if same_track { last.unwrap_or((None, 0)) } else { (None, 0) }
        }
    }
}

/// Re-reads every `PlayerState` field from `player`/`root`, folding in `previous` for track
/// identity caching (`album_art_path`/`length`, ADR-0036). Always succeeds; failed properties
/// retain their previous values.
async fn resync(
    bus_name: &str,
    player: &MprisPlayerProxy<'static>,
    root: &MprisRootProxy<'static>,
    previous: Option<Previous>,
) -> Resynced {
    // Concurrent: zbus answers most from its property cache, so the uncached ones (`Position`, any
    // property the player lacks) cost one round trip together rather than one each.
    let (
        status,
        can_go_next,
        can_go_previous,
        can_seek,
        can_play,
        can_pause,
        can_raise,
        can_quit,
        volume,
        loop_status,
        shuffle,
        rate,
        minimum_rate,
        maximum_rate,
        player_identity,
        desktop_entry,
        raw_position,
        metadata,
    ) = tokio::join!(
        player.playback_status(),
        player.can_go_next(),
        player.can_go_previous(),
        player.can_seek(),
        player.can_play(),
        player.can_pause(),
        root.can_raise(),
        root.can_quit(),
        player.volume(),
        player.loop_status(),
        player.shuffle(),
        player.rate(),
        player.minimum_rate(),
        player.maximum_rate(),
        root.identity(),
        root.desktop_entry(),
        player.position(),
        player.metadata(),
    );
    let previous_state = previous.as_ref().map(|p| &p.state);
    let play_state = match status.as_deref().map(parse_play_state) {
        Ok(Some(status)) => status,
        _ => {
            debug!("PlaybackStatus for {bus_name} is unreadable or unknown, {status:?}; keeping the last known value");
            previous_state.map(|s| s.play_state).unwrap_or_default()
        }
    };
    let can_go_next = can_go_next.unwrap_or_else(|_| previous_state.is_some_and(|s| s.can_go_next));
    let can_go_previous = can_go_previous.unwrap_or_else(|_| previous_state.is_some_and(|s| s.can_go_previous));
    let can_seek = can_seek.unwrap_or_else(|_| previous_state.is_some_and(|s| s.can_seek));
    let can_play = can_play.unwrap_or_else(|_| previous_state.is_some_and(|s| s.can_play));
    let can_pause = can_pause.unwrap_or_else(|_| previous_state.is_some_and(|s| s.can_pause));
    let can_raise = can_raise.unwrap_or_else(|_| previous_state.is_some_and(|s| s.can_raise));
    let can_quit = can_quit.unwrap_or_else(|_| previous_state.is_some_and(|s| s.can_quit));
    let volume = volume
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(percent_from_fraction)
        .unwrap_or_else(|| previous_state.map_or(100.0, |s| s.volume));
    let loop_status = match loop_status {
        Ok(status) => parse_loop_status(&status),
        Err(_) => previous_state.and_then(|s| s.loop_status),
    };
    let shuffle = shuffle.unwrap_or_else(|_| previous_state.is_some_and(|s| s.shuffle));
    let rate = rate
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_else(|| previous_state.map_or(1.0, |s| s.rate));
    let minimum_rate = minimum_rate
        .ok()
        .filter(|value| value.is_finite())
        .unwrap_or_else(|| previous_state.map_or(0.0, |s| s.minimum_rate));
    let maximum_rate = maximum_rate
        .ok()
        .filter(|value| value.is_finite())
        .unwrap_or_else(|| previous_state.map_or(0.0, |s| s.maximum_rate));
    // `player_identity` is MediaPlayer2.Identity, not the TrackIdentity key below.
    let player_identity = resolved_player_identity(player_identity, previous_state, bus_name);
    // Optional and absent on several players: an error means "I have none", not a stale value.
    let desktop_entry = desktop_entry.unwrap_or_default();

    // A full Metadata read failure keeps metadata-derived fields instead of resetting them and
    // causing a spurious track change.
    let Ok(metadata) = metadata else {
        debug!(
            "Metadata read failed for {bus_name}; keeping the last known title/artist/art/length/trackid this round"
        );
        // Keeping the previous track's fields is by definition the same-track case.
        let (position, position_updated_at) =
            resolve_position(raw_position, previous_state, true, play_state, bus_name);
        let Previous { state: kept, identity, trackid } = previous.unwrap_or_default();
        let state = PlayerState {
            id: player_id(bus_name).to_string(),
            identity: player_identity,
            play_state,
            can_go_next,
            can_go_previous,
            can_seek,
            can_play,
            can_pause,
            can_raise,
            can_quit,
            volume,
            loop_status,
            shuffle,
            rate,
            minimum_rate,
            maximum_rate,
            position,
            position_updated_at,
            desktop_entry,
            ..kept
        };
        return Resynced { state, identity, trackid };
    };
    let parsed = parse_metadata(&metadata);

    let new_identity = parsed.track_identity();
    let same_track = previous.as_ref().is_some_and(|p| p.identity == new_identity);
    let (position, position_updated_at) =
        resolve_position(raw_position, previous_state, same_track, play_state, bus_name);
    let kept = previous.map(|p| p.state).unwrap_or_default();

    let local_art_path = resolve_album_art_path(parsed.art_url.as_deref());
    let album_art_path = if !local_art_path.is_empty() {
        local_art_path
    } else if same_track {
        kept.album_art_path
    } else {
        String::new()
    };
    let length = match parsed.length_us {
        Some(length) if length >= 0 => Some(length),
        _ if same_track => kept.length,
        _ => None,
    };

    let trackid = parsed.trackid.clone();
    let state = PlayerState {
        id: player_id(bus_name).to_string(),
        identity: player_identity,
        play_state,
        can_go_next,
        can_go_previous,
        can_seek,
        can_play,
        can_pause,
        can_raise,
        can_quit,
        volume,
        loop_status,
        shuffle,
        rate,
        minimum_rate,
        maximum_rate,
        title: parsed.title,
        artist: parsed.artist,
        album: parsed.album,
        album_artist: parsed.album_artist,
        genre: parsed.genre,
        album_art_path,
        position,
        position_updated_at,
        length,
        // Keep artwork across same-track updates; players may drop metadata keys after a full
        // description.
        url: match parsed.url {
            Some(url) => url,
            None if same_track => kept.url,
            None => String::new(),
        },
        desktop_entry,
        track_list: kept.track_list,
        playlists: kept.playlists,
    };
    Resynced { state, identity: new_identity, trackid }
}

/// Binds `bus_name`, runs [`resync`], inserts the entry, then starts its forwarder. Logs and
/// returns on bind failure or `CanControl == false`; uncontrollable sources are not useful in a
/// status bar (ADR-0036).
///
/// Insert before spawn: zbus `receive_*_changed` streams replay cached values, so a forwarder
/// could otherwise run before insertion, find nothing to update, and freeze the player. Confirmed
/// live for the first player in a fresh session.
pub(super) async fn register_player(
    connection: &zbus::Connection,
    registry: &PlayerRegistry,
    events: &UnboundedSender<()>,
    bus_name: String,
) {
    let player = match bind_player(connection, &bus_name).await {
        Ok(player) => player,
        Err(err) => {
            debug!("failed to bind Player for {bus_name}: {err}");
            return;
        }
    };
    let root = match bind_root(connection, &bus_name).await {
        Ok(root) => root,
        Err(err) => {
            debug!("failed to bind MediaPlayer2 for {bus_name}: {err}");
            return;
        }
    };
    let track_list = bind_track_list(connection, &bus_name).await.ok();
    let playlists = bind_playlists(connection, &bus_name).await.ok();
    match player.can_control().await {
        Ok(true) => {}
        Ok(false) => {
            debug!("{bus_name} reports CanControl=false; not tracking it");
            return;
        }
        Err(err) => debug!("CanControl read failed for {bus_name} (tracking anyway): {err}"),
    }

    let Resynced { mut state, identity, trackid } = resync(&bus_name, &player, &root, None).await;
    if let Some(proxy) = &track_list
        && let Ok(tracks) = read_track_list(proxy, trackid.as_deref()).await
    {
        state.track_list = tracks;
    }
    if let Some(proxy) = &playlists
        && let Ok(playlists_state) =
            read_playlists(proxy, 0, super::collections::PLAYLIST_PAGE_LIMIT, "", false, false).await
    {
        state.playlists = playlists_state;
    }

    let mut entry = PlayerEntry {
        player: player.clone(),
        root: root.clone(),
        track_list: track_list.clone(),
        playlists: playlists.clone(),
        last_known: state,
        registered: 0,
        track_identity: identity,
        cached_trackid: trackid,
        playlist_revision: 0,
        playlist_request: 0,
        forwarder: None,
        collections_forwarder: None,
    };
    let previous = {
        let mut guard = registry.lock().expect("mutex poisoned");
        entry.registered = match guard.get(&bus_name) {
            // Same bus name, same player: holds its place; a restart gets a new unique name.
            Some(existing) => existing.registered,
            None => NEXT_REGISTRATION.fetch_add(1, Ordering::Relaxed),
        };
        guard.insert(bus_name.clone(), entry)
    };
    if let Some(previous) = previous {
        if let Some(handle) = previous.forwarder {
            handle.abort();
        }
        if let Some(handle) = previous.collections_forwarder {
            handle.abort();
        }
    }

    let (refresh_tx, refresh_rx) = tokio::sync::mpsc::unbounded_channel();
    let forwarder =
        spawn_player_forwarder(bus_name.clone(), player, root, registry.clone(), events.clone(), refresh_tx);
    let collections_forwarder = spawn_collections_forwarder(
        bus_name.clone(),
        track_list,
        playlists,
        registry.clone(),
        events.clone(),
        refresh_rx,
    );
    match registry.lock().expect("mutex poisoned").get_mut(&bus_name) {
        Some(entry) => entry.forwarder = Some(forwarder),
        // A real NameOwnerChanged departure raced insertion; abort the new forwarder rather than
        // leak an untracked, un-abortable task.
        None => forwarder.abort(),
    }
    match registry.lock().expect("mutex poisoned").get_mut(&bus_name) {
        Some(entry) => entry.collections_forwarder = Some(collections_forwarder),
        None => collections_forwarder.abort(),
    }
    let _ = events.send(());
}

/// Re-runs [`resync`] on each burst of property changes or `Seeked`, updating the entry in place
/// with no debounce or incremental patching. `Position` is excluded from `PropertiesChanged` as too
/// high-frequency, so only `Seeked` signals position changes.
fn spawn_player_forwarder(
    bus_name: String,
    player: MprisPlayerProxy<'static>,
    root: MprisRootProxy<'static>,
    registry: PlayerRegistry,
    events: UnboundedSender<()>,
    collections_refresh: UnboundedSender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seeked = match player.receive_seeked().await {
            Ok(stream) => Some(stream),
            Err(err) => {
                debug!("Seeked subscription failed for {bus_name}; retrying while forwarding properties: {err}");
                None
            }
        };
        let mut retry_seeked_at = seeked.is_none().then(tokio::time::Instant::now);
        // `true` for `PlaybackStatus`, which arms the position recheck.
        let mut properties = futures_util::stream::select_all([
            player.receive_playback_status_changed().await.map(|_| true).boxed(),
            player.receive_metadata_changed().await.map(|_| false).boxed(),
            player.receive_volume_changed().await.map(|_| false).boxed(),
            player.receive_loop_status_changed().await.map(|_| false).boxed(),
            player.receive_shuffle_changed().await.map(|_| false).boxed(),
            player.receive_rate_changed().await.map(|_| false).boxed(),
            player.receive_can_go_next_changed().await.map(|_| false).boxed(),
            player.receive_can_go_previous_changed().await.map(|_| false).boxed(),
            player.receive_can_seek_changed().await.map(|_| false).boxed(),
            player.receive_can_play_changed().await.map(|_| false).boxed(),
            player.receive_can_pause_changed().await.map(|_| false).boxed(),
            root.receive_can_raise_changed().await.map(|_| false).boxed(),
            root.receive_can_quit_changed().await.map(|_| false).boxed(),
        ]);

        // Several players update `Position` at an indeterminate time *after* `PlaybackStatus`, so
        // the read taken while handling that signal answers with whatever the player held
        // mid-transition -- for Firefox, sometimes zero. One late re-read fixes it. Anything the
        // player does tell us in the meantime still arrives on its own signal.
        let mut recheck_at: Option<tokio::time::Instant> = None;

        loop {
            let deadline = recheck_at;
            let retry_deadline = retry_seeked_at;
            let fired = tokio::select! {
                Some(status) = properties.next() => Some(Wake::Property { status }),
                signal = async { match seeked.as_mut() { Some(stream) => stream.next().await, None => std::future::pending().await } } => {
                    Some(seeked_wake(signal, &mut seeked, &mut retry_seeked_at))
                },
                () = async move {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => Some(Wake::Recheck),
                () = async move {
                    match retry_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => Some(Wake::RetrySeeked),
                else => None,
            };
            let Some(fired) = fired else {
                break;
            };
            if matches!(fired, Wake::RetrySeeked) {
                match player.receive_seeked().await {
                    Ok(stream) => {
                        seeked = Some(stream);
                        retry_seeked_at = None;
                    }
                    Err(err) => {
                        debug!(
                            "Seeked subscription failed for {bus_name}; retrying while forwarding properties: {err}"
                        );
                        retry_seeked_at = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1));
                    }
                }
                continue;
            }
            let mut status_changed = matches!(fired, Wake::Property { status: true });
            let mut seeked_position = match fired {
                Wake::Seeked(position) => Some(position),
                _ => None,
            };
            // One resync for every queued wake: a `PropertiesChanged` wakes one stream per property.
            while let Some(Some(status)) = properties.next().now_or_never() {
                status_changed |= status;
            }
            while let Some(signal) = seeked.as_mut().and_then(|stream| stream.next().now_or_never()) {
                if let Wake::Seeked(position) = seeked_wake(signal, &mut seeked, &mut retry_seeked_at) {
                    seeked_position = Some(position);
                }
            }
            // Only a status change arms it, and only the timer firing disarms it. Clearing on any
            // event let a `Metadata` change 20ms later cancel the correction, which is the one
            // case the delay exists for -- the player publishes the new state first and the
            // position that goes with it some indeterminate time after.
            if status_changed {
                recheck_at = Some(tokio::time::Instant::now() + POSITION_RECHECK_DELAY);
            } else if deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
                recheck_at = None;
            }

            let previous = registry.lock().expect("mutex poisoned").get(&bus_name).map(|entry| Previous {
                state: entry.last_known.clone(),
                identity: entry.track_identity.clone(),
                trackid: entry.cached_trackid.clone(),
            });
            let Resynced { mut state, identity, trackid } = resync(&bus_name, &player, &root, previous).await;

            let mut guard = registry.lock().expect("mutex poisoned");
            let Some(entry) = guard.get_mut(&bus_name) else { break };
            // A `Seeked` batched with a track change belongs to the old track.
            if let Some(position) = seeked_position
                && entry.track_identity == identity
            {
                publish_seeked_position(&mut state, position);
            }
            let track_changed = entry.cached_trackid != trackid;
            state.track_list = if track_changed {
                TrackListState { current_track: trackid.clone().unwrap_or_default(), ..TrackListState::default() }
            } else {
                entry.last_known.track_list.clone()
            };
            state.playlists = entry.last_known.playlists.clone();
            // Record what `resync` found either way: `identity` and `trackid` can move while the
            // state compares equal -- the same track re-queued gets a fresh `mpris:trackid`, and a
            // stale one misaddresses `SetPosition`. Only the notification is worth skipping.
            let unchanged = entry.last_known == state;
            entry.last_known = state;
            entry.track_identity = identity;
            entry.cached_trackid = trackid;
            drop(guard);
            if track_changed {
                let _ = collections_refresh.send(());
            }
            if unchanged {
                continue;
            }

            if events.send(()).is_err() {
                break;
            }
        }
    })
}

#[derive(Clone, Copy)]
enum Wake {
    /// `status` when `PlaybackStatus` was among the changes.
    Property {
        status: bool,
    },
    Seeked(i64),
    Recheck,
    RetrySeeked,
}

/// A `Seeked` stream's item as a [`Wake`]. The stream ending drops it and schedules a resubscribe.
fn seeked_wake(
    signal: Option<super::proxies::Seeked>,
    seeked: &mut Option<super::proxies::SeekedStream>,
    retry_seeked_at: &mut Option<tokio::time::Instant>,
) -> Wake {
    match signal {
        Some(message) => match message.args() {
            Ok(args) => Wake::Seeked(args.position_us),
            Err(_) => Wake::Property { status: false },
        },
        None => {
            *seeked = None;
            *retry_seeked_at = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1));
            Wake::Property { status: false }
        }
    }
}

/// `None` for a spelling outside the MPRIS spec.
fn parse_play_state(status: &str) -> Option<PlayState> {
    match status {
        "Playing" => Some(PlayState::Playing),
        "Paused" => Some(PlayState::Paused),
        "Stopped" => Some(PlayState::Stopped),
        _ => None,
    }
}

/// `None` for a spelling outside the MPRIS spec, as for a player that reports none.
fn parse_loop_status(status: &str) -> Option<LoopStatus> {
    [LoopStatus::None, LoopStatus::Track, LoopStatus::Playlist].into_iter().find(|known| known.as_str() == status)
}

fn publish_seeked_position(state: &mut PlayerState, position: i64) {
    state.position = Some(position).filter(|p| *p >= 0);
    state.position_updated_at = monotonic_micros();
}

/// Removes a departed `bus_name` (`NameOwnerChanged` with an empty new owner) and aborts its
/// forwarder. No-op if untracked, including skipped `playerctld` or uncontrollable sources.
pub(super) fn unregister_player(registry: &PlayerRegistry, bus_name: &str, events: &UnboundedSender<()>) {
    let removed = registry.lock().expect("mutex poisoned").remove(bus_name);
    if let Some(entry) = removed {
        debug!("MPRIS player departed: {bus_name}");
        if let Some(handle) = entry.forwarder {
            handle.abort();
        }
        if let Some(handle) = entry.collections_forwarder {
            handle.abort();
        }
        let _ = events.send(());
    }
}

#[cfg(test)]
mod position_tests {
    use super::*;

    fn previous_at(position: i64) -> PlayerState {
        PlayerState { position: Some(position), position_updated_at: 42, ..PlayerState::default() }
    }

    #[test]
    fn a_zero_mid_track_keeps_the_last_reading_and_its_timestamp() {
        // Firefox answers `0` for several seconds after a seek while playing on from the target.
        // Believing it restarts every progress bar at the beginning of the track.
        let state = previous_at(340_000_000);
        assert_eq!(resolve_position(Ok(0), Some(&state), true, PlayState::Stopped, "test"), (Some(340_000_000), 42));
    }

    #[test]
    fn a_zero_on_a_new_track_is_published() {
        // A track we were not already inside legitimately begins at zero, so the same reading is
        // the truth rather than a player that has not caught up.
        let state = previous_at(340_000_000);
        let (position, updated_at) = resolve_position(Ok(0), Some(&state), false, PlayState::Stopped, "test");
        assert_eq!(position, Some(0));
        assert_ne!(updated_at, 42, "a believed reading carries the moment it was taken");
    }

    #[test]
    fn a_real_reading_always_wins() {
        let state = previous_at(340_000_000);
        let (position, updated_at) = resolve_position(Ok(363_000_000), Some(&state), true, PlayState::Stopped, "test");
        assert_eq!(position, Some(363_000_000));
        assert_ne!(updated_at, 42);
    }

    #[test]
    fn an_unchanged_reading_keeps_its_stamp_until_the_play_state_moves() {
        let state = PlayerState { play_state: PlayState::Paused, ..previous_at(340_000_000) };
        assert_eq!(
            resolve_position(Ok(340_000_000), Some(&state), true, PlayState::Paused, "test"),
            (Some(340_000_000), 42)
        );
        let (_, resumed_at) = resolve_position(Ok(340_000_000), Some(&state), true, PlayState::Playing, "test");
        assert_ne!(resumed_at, 42, "resuming restarts extrapolation from now");
    }

    #[test]
    fn a_zero_with_nothing_to_fall_back_on_is_published() {
        // The first reading of a player that really is at the start has no previous to keep.
        let (position, _) = resolve_position(Ok(0), None, true, PlayState::Stopped, "test");
        assert_eq!(position, Some(0));
    }

    #[test]
    fn an_unread_position_is_nil_rather_than_a_fabricated_zero() {
        // ADR-0036: unavailable is not zero. Nothing has ever been read here.
        assert_eq!(resolve_position(Err(zbus::Error::InvalidReply), None, true, PlayState::Stopped, "test"), (None, 0));
    }

    #[test]
    fn a_seeked_signal_to_zero_overrides_the_transient_zero_filter() {
        let mut state = previous_at(340_000_000);
        publish_seeked_position(&mut state, 0);
        assert_eq!(state.position, Some(0));
        assert_ne!(state.position_updated_at, 42);
        publish_seeked_position(&mut state, -1);
        assert_eq!(state.position, None, "a negative offset is unknown, as on the property read");
    }

    #[test]
    fn a_failed_identity_read_keeps_the_previous_identity() {
        let previous = PlayerState { identity: "Player name".to_string(), ..PlayerState::default() };
        assert_eq!(resolved_player_identity(Err(zbus::Error::InvalidReply), Some(&previous), "test"), "Player name");
        assert_eq!(resolved_player_identity(Err(zbus::Error::InvalidReply), None, "test"), "");
        assert_eq!(resolved_player_identity(Ok("New name".to_string()), Some(&previous), "test"), "New name");
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;

    struct Root;

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        #[zbus(property)]
        fn identity(&self) -> zbus::fdo::Result<String> {
            Err(zbus::fdo::Error::Failed("Identity temporarily unavailable".to_string()))
        }
    }

    #[tokio::test]
    async fn a_failed_identity_property_read_uses_the_previous_state() {
        let (connection, _peer) = p2p_pair_serving(|builder| builder.serve_at("/org/mpris/MediaPlayer2", Root)).await;
        let root = bind_root(&connection, "org.mpris.MediaPlayer2.test").await.unwrap();
        let read = root.identity().await;
        assert!(read.is_err(), "fixture must return a D-Bus error");

        let previous = PlayerState { identity: "Player name".to_string(), ..PlayerState::default() };
        assert_eq!(resolved_player_identity(read, Some(&previous), "test"), "Player name");

        // No Player interface: a failed Metadata read moves the whole previous track across.
        let player = bind_player(&connection, "org.mpris.MediaPlayer2.test").await.unwrap();
        let state = PlayerState {
            id: "test".into(),
            title: "Song".into(),
            album_art_path: "/art.png".into(),
            length: Some(9),
            position: Some(5),
            position_updated_at: 42,
            track_list: TrackListState { current_track: "/t/1".into(), ..TrackListState::default() },
            ..previous
        };
        let identity = parse_metadata(&crate::capabilities::test_support::properties(&[(
            "xesam:title",
            zbus::zvariant::Value::from("Song"),
        )]))
        .track_identity();
        let previous = Previous { state: state.clone(), identity: identity.clone(), trackid: Some("/t/1".into()) };
        let after = resync("org.mpris.MediaPlayer2.test", &player, &root, Some(previous)).await;
        assert_eq!((after.state, after.identity, after.trackid.as_deref()), (state, identity, Some("/t/1")));
    }
}

#[cfg(test)]
mod resync_tests {
    use super::*;
    use crate::capabilities::test_support::{p2p_pair_serving, properties, within};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use zbus::zvariant::{ObjectPath, OwnedValue, Value};

    const NAME: &str = "org.mpris.MediaPlayer2.fixture";
    const PATH: &str = "/org/mpris/MediaPlayer2";
    const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

    /// Every flag differs from its default, so a read landing in another field shows.
    #[derive(Clone, Default)]
    struct Player {
        position_reads: Arc<AtomicUsize>,
        /// Held by a test to stall `Position` reads, and with them the resync awaiting one.
        gate: Arc<tokio::sync::Mutex<()>>,
    }

    fn track(trackid: &str, title: &str) -> HashMap<String, OwnedValue> {
        properties(&[
            ("mpris:trackid", Value::from(ObjectPath::try_from(trackid.to_string()).unwrap())),
            ("xesam:title", Value::from(title.to_string())),
            ("xesam:artist", Value::from(vec!["Artist"])),
            ("mpris:length", Value::from(240_000_000_i64)),
        ])
    }

    #[rustfmt::skip]
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        #[zbus(property)] fn playback_status(&self) -> String { "Paused".into() }
        #[zbus(property)] fn metadata(&self) -> HashMap<String, OwnedValue> { track("/t/1", "Song") }
        /// A new reading per call, so every resync changes the state and wakes `events`.
        #[zbus(property)]
        async fn position(&self) -> i64 {
            let _open = self.gate.lock().await;
            1_000 * i64::try_from(self.position_reads.fetch_add(1, Ordering::Relaxed)).unwrap()
        }
        #[zbus(property)] fn can_control(&self) -> bool { true }
        #[zbus(property)] fn can_go_next(&self) -> bool { true }
        #[zbus(property)] fn can_go_previous(&self) -> bool { false }
        #[zbus(property)] fn can_seek(&self) -> bool { true }
        #[zbus(property)] fn can_play(&self) -> bool { false }
        #[zbus(property)] fn can_pause(&self) -> bool { true }
        #[zbus(property)] fn volume(&self) -> f64 { 0.5 }
        #[zbus(property)] fn loop_status(&self) -> String { "Track".into() }
        #[zbus(property)] fn shuffle(&self) -> bool { true }
        #[zbus(property)] fn rate(&self) -> f64 { 1.5 }
        #[zbus(property)] fn minimum_rate(&self) -> f64 { 0.25 }
        #[zbus(property)] fn maximum_rate(&self) -> f64 { 2.0 }
    }

    struct Root;

    #[rustfmt::skip]
    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        #[zbus(property)] fn identity(&self) -> String { "Fixture".into() }
        #[zbus(property)] fn desktop_entry(&self) -> String { "fixture".into() }
        #[zbus(property)] fn can_raise(&self) -> bool { false }
        #[zbus(property)] fn can_quit(&self) -> bool { true }
    }

    /// `(connection, peer, the peer's player)`.
    async fn serve() -> (zbus::Connection, zbus::Connection, Player) {
        let player = Player::default();
        let served = player.clone();
        let (connection, peer) = p2p_pair_serving(|builder| builder.serve_at(PATH, served)?.serve_at(PATH, Root)).await;
        (connection, peer, player)
    }

    /// Long enough for queued wakes, their resyncs and the 100 ms recheck to finish.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    async fn properties_changed(peer: &zbus::Connection, changed: HashMap<&str, Value<'_>>) {
        let emitter = zbus::object_server::SignalEmitter::new(peer, PATH).unwrap();
        let interface = zbus::names::InterfaceName::from_static_str_unchecked(PLAYER);
        zbus::fdo::Properties::properties_changed(&emitter, interface, changed, (&[][..]).into()).await.unwrap();
    }

    async fn seeked(peer: &zbus::Connection, position: i64) {
        peer.emit_signal(None::<&str>, PATH, PLAYER, "Seeked", &(position,)).await.unwrap();
    }

    fn last_known(registry: &PlayerRegistry) -> PlayerState {
        registry.lock().unwrap().get(NAME).map(|entry| entry.last_known.clone()).unwrap()
    }

    #[tokio::test]
    async fn every_concurrent_read_lands_in_its_own_field() {
        let (connection, _peer, _) = serve().await;
        let player = bind_player(&connection, NAME).await.unwrap();
        let root = bind_root(&connection, NAME).await.unwrap();
        let Resynced { state, trackid, .. } = resync(NAME, &player, &root, None).await;
        let expected = PlayerState {
            id: "fixture".into(),
            identity: "Fixture".into(),
            play_state: PlayState::Paused,
            can_go_next: true,
            can_go_previous: false,
            can_seek: true,
            can_play: false,
            can_pause: true,
            can_raise: false,
            can_quit: true,
            volume: 50.0,
            loop_status: Some(LoopStatus::Track),
            shuffle: true,
            rate: 1.5,
            minimum_rate: 0.25,
            maximum_rate: 2.0,
            title: "Song".into(),
            artist: "Artist".into(),
            position: state.position,
            position_updated_at: state.position_updated_at,
            length: Some(240_000_000),
            desktop_entry: "fixture".into(),
            ..PlayerState::default()
        };
        assert_eq!(state, expected);
        assert_eq!(trackid.as_deref(), Some("/t/1"));
    }

    #[tokio::test]
    async fn queued_wakes_fold_into_one_resync() {
        let (connection, peer, player) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, mut woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        let reads = || player.position_reads.load(Ordering::Relaxed);
        let before = reads();
        within(woken.recv()).await;
        settle().await;
        // zbus replays all 13 cached properties as the forwarder subscribes, 13 resyncs undrained;
        // a second batch is legitimate on a multi-thread runtime. Plus the replayed status's recheck.
        assert!(reads() - before <= 3, "{} resyncs for the subscription replay", reads() - before);

        let before = reads();
        let changed =
            [("Volume", 0.25.into()), ("Rate", 1.25.into()), ("Shuffle", false.into()), ("LoopStatus", "None".into())];
        properties_changed(&peer, HashMap::from(changed)).await;
        seeked(&peer, 5_000_000).await;
        settle().await;
        assert!(reads() - before <= 3, "{} resyncs for one signal of 4 properties and a Seeked", reads() - before);
        let state = last_known(&registry);
        assert_eq!((state.position, state.volume, state.shuffle), (Some(5_000_000), 25.0, false));
    }

    #[tokio::test]
    async fn a_seeked_batched_with_a_track_change_leaves_the_new_track_live_position() {
        let (connection, peer, player) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        settle().await;
        let stalled = player.gate.lock().await;
        // A resync stuck on `Position` while the next batch queues up behind it.
        properties_changed(&peer, HashMap::from([("Volume", 0.75.into())])).await;
        settle().await;
        seeked(&peer, 300_000_000).await;
        properties_changed(&peer, HashMap::from([("Metadata", Value::from(track("/t/2", "Next")))])).await;
        settle().await;
        drop(stalled);
        settle().await;
        let state = last_known(&registry);
        assert_eq!(state.title, "Next");
        assert_ne!(state.position, Some(300_000_000), "the old track's seek target");
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair;

    /// An entry with only the two ordering fields. Binding a proxy makes no call, so a p2p pair
    /// with nobody answering is enough.
    async fn entry(connection: &zbus::Connection, id: &str, registered: u64) -> PlayerEntry {
        PlayerEntry {
            player: super::super::proxies::bind_player(connection, "org.mpris.MediaPlayer2.probe")
                .await
                .expect("binding makes no call"),
            root: super::super::proxies::bind_root(connection, "org.mpris.MediaPlayer2.probe")
                .await
                .expect("binding makes no call"),
            track_list: None,
            playlists: None,
            last_known: PlayerState { id: id.to_string(), ..PlayerState::default() },
            registered,
            track_identity: TrackIdentity::default(),
            cached_trackid: None,
            playlist_revision: 0,
            playlist_request: 0,
            forwarder: None,
            collections_forwarder: None,
        }
    }

    /// HashMap iteration is process-seeded; without ordering, `players[1]` changed between pushes
    /// over the same set.
    #[tokio::test]
    async fn the_list_is_in_appearance_order_whatever_the_map_says() {
        let (connection, _peer) = p2p_pair().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Build before locking; holding a guard across await triggers clippy's
        // `await_holding_lock` lint.
        let third = entry(&connection, "third", 2).await;
        let first = entry(&connection, "first", 0).await;
        let second = entry(&connection, "second", 1).await;
        {
            let mut guard = registry.lock().unwrap();
            guard.insert("zed".to_string(), third);
            guard.insert("alpha".to_string(), first);
            guard.insert("mid".to_string(), second);
        }
        let ids: Vec<String> = ordered_players(&registry).into_iter().map(|player| player.id).collect();
        assert_eq!(ids, ["first", "second", "third"], "an alphabetical sort would answer the other way");
    }

    /// Position updates must not move a player under a config holding its index.
    #[tokio::test]
    async fn a_player_that_resyncs_holds_its_place() {
        let (connection, _peer) = p2p_pair().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let spotify = entry(&connection, "spotify", 0).await;
        let firefox = entry(&connection, "firefox", 1).await;
        {
            let mut guard = registry.lock().unwrap();
            guard.insert("spotify".to_string(), spotify);
            guard.insert("firefox".to_string(), firefox);
        }
        let mut replacement = entry(&connection, "spotify-again", 999).await;
        // Re-registration of a live bus name keeps its sequence, as `register_player` does.
        replacement.registered = registry.lock().unwrap().get("spotify").expect("just inserted").registered;
        registry.lock().unwrap().insert("spotify".to_string(), replacement);

        let ids: Vec<String> = ordered_players(&registry).into_iter().map(|player| player.id).collect();
        assert_eq!(ids, ["spotify-again", "firefox"]);
    }
}

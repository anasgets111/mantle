//! Per-player registry: bind a discovered MPRIS name, hydrate one live [`PlayerState`] entry,
//! and maintain it in a resync loop.

pub use shared::state::mpris::PlayerState;

use crate::capabilities::scale::percent_from_fraction;
use crate::capabilities::{Dict, get_all, get_property, truncate_utf8_bytes};
use shared::state::mpris::LoopStatus;
use shared::state::mpris::PlayState;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{FutureExt, StreamExt};
use shared::{debug, warn_first};
use tokio::task::JoinHandle;

use super::changes::{Changes, Drain, Props, drain};
use super::collections::{
    TrackListState, collections_forwarder, next_optional, property, read_playlists, read_track_list,
};
use super::metadata::{TrackIdentity, parse_metadata, resolve_album_art_path};
use super::position::{publish_seeked_position, resolve_position};
use super::proxies::{
    MprisPlayerProxy, MprisPlaylistsProxy, MprisRootProxy, MprisTrackListProxy, bind_player, bind_playlists, bind_root,
    bind_track_list,
};
use super::watcher::player_id;
use super::{MAX_METADATA_TEXT_BYTES, MAX_PLAYERS};
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
    /// `None` only between insertion and forwarder spawn; the first resync fires on spawn, so
    /// insertion always comes first.
    forwarder: Option<JoinHandle<()>>,
    collections_forwarder: Option<JoinHandle<()>>,
    /// From the first command on: the player's queue and the task running it in order.
    pub(super) commands: Option<(tokio::sync::mpsc::Sender<Command>, JoinHandle<()>)>,
}

/// One dispatched `mantle.mpris` action.
pub(super) type Command = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

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

struct Resynced {
    state: PlayerState,
    identity: TrackIdentity,
    trackid: Option<String>,
}

fn resolved_player_identity(read: Option<String>, previous: Option<&PlayerState>, bus_name: &str) -> String {
    match read {
        Some(identity) => truncate_utf8_bytes(&identity, MAX_METADATA_TEXT_BYTES),
        None => {
            debug!("Identity unread for {bus_name}; keeping the last known value this round");
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

/// Builds every `PlayerState` field from what `changes` holds and a live `Position`, folding in
/// `previous` for track identity caching (`album_art_path`/`length`, ADR-0036). Reads both
/// interfaces whole first when `changes` asks for it. Always succeeds; a property never read
/// keeps its previous value.
async fn resync(
    bus_name: &str,
    player: &MprisPlayerProxy<'static>,
    root: &MprisRootProxy<'static>,
    changes: &Changes,
    previous: Option<Previous>,
) -> Resynced {
    if std::mem::take(&mut changes.lock().refetch) {
        let (player, root) = tokio::join!(get_all::<Props>(player.inner()), get_all::<Props>(root.inner()));
        let mut pending = changes.lock();
        // An invalidation during the read may postdate what it answered: leave the slot it
        // cleared for the reread it armed, since `fill` keeps whatever is already there.
        let invalidated = pending.refetch;
        for read in [player, root].into_iter().filter(|_| !invalidated) {
            match read {
                Ok(read) => pending.props.fill(read),
                Err(err) => {
                    debug!("properties unread for {bus_name}; retrying on the next change: {err}");
                    pending.refetch = true;
                }
            }
        }
    }
    // Everything but `Position` is taken before awaiting it, as zbus's cache answered: a change
    // landing during the call is the next resync's, with any `Seeked` that came before it.
    let props = changes.lock().props.clone();
    let Props {
        playback_status: status,
        can_go_next,
        can_go_previous,
        can_seek,
        can_play,
        can_pause,
        volume,
        loop_status,
        shuffle,
        rate,
        minimum_rate,
        maximum_rate,
        metadata,
        identity: player_identity,
        desktop_entry,
        can_raise,
        can_quit,
        ..
    } = props;
    let previous_state = previous.as_ref().map(|p| &p.state);
    let play_state = match status.as_deref().and_then(parse_play_state) {
        Some(status) => status,
        None => {
            debug!("PlaybackStatus for {bus_name} is unreadable or unknown, {status:?}; keeping the last known value");
            previous_state.map(|s| s.play_state).unwrap_or_default()
        }
    };
    let can_go_next = can_go_next.unwrap_or_else(|| previous_state.is_some_and(|s| s.can_go_next));
    let can_go_previous = can_go_previous.unwrap_or_else(|| previous_state.is_some_and(|s| s.can_go_previous));
    let can_seek = can_seek.unwrap_or_else(|| previous_state.is_some_and(|s| s.can_seek));
    let can_play = can_play.unwrap_or_else(|| previous_state.is_some_and(|s| s.can_play));
    let can_pause = can_pause.unwrap_or_else(|| previous_state.is_some_and(|s| s.can_pause));
    let can_raise = can_raise.unwrap_or_else(|| previous_state.is_some_and(|s| s.can_raise));
    let can_quit = can_quit.unwrap_or_else(|| previous_state.is_some_and(|s| s.can_quit));
    let volume = volume
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(percent_from_fraction)
        .unwrap_or_else(|| previous_state.map_or(100.0, |s| s.volume));
    let loop_status = match loop_status {
        Some(status) => parse_loop_status(&status),
        None => previous_state.and_then(|s| s.loop_status),
    };
    let shuffle = shuffle.unwrap_or_else(|| previous_state.is_some_and(|s| s.shuffle));
    let rate = rate
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_else(|| previous_state.map_or(1.0, |s| s.rate));
    let minimum_rate = minimum_rate
        .filter(|value| value.is_finite())
        .unwrap_or_else(|| previous_state.map_or(0.0, |s| s.minimum_rate));
    let maximum_rate = maximum_rate
        .filter(|value| value.is_finite())
        .unwrap_or_else(|| previous_state.map_or(0.0, |s| s.maximum_rate));
    // `player_identity` is MediaPlayer2.Identity, not the TrackIdentity key below.
    let player_identity = resolved_player_identity(player_identity, previous_state, bus_name);
    // Optional and absent on several players: unread means "I have none", not a stale value.
    let desktop_entry = truncate_utf8_bytes(&desktop_entry.unwrap_or_default(), MAX_METADATA_TEXT_BYTES);

    // A full Metadata read failure keeps metadata-derived fields instead of resetting them and
    // causing a spurious track change.
    let parsed = match &metadata {
        Some(Dict(metadata)) => Some(parse_metadata(metadata)),
        None => {
            debug!("Metadata unread for {bus_name}; keeping the last known title/artist/art/length/trackid this round");
            None
        }
    };
    let new_identity = parsed.as_ref().map(|parsed| parsed.track_identity());
    // Keeping the previous track's fields is by definition the same-track case.
    let same_track = new_identity.as_ref().is_none_or(|new| previous.as_ref().is_some_and(|p| &p.identity == new));
    let raw_position = property(player.inner(), "Position").await;
    let (position, position_updated_at) =
        resolve_position(raw_position, previous_state, same_track, play_state, bus_name);
    let Previous { state: kept, identity, trackid } = previous.unwrap_or_default();
    let mut state = PlayerState {
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
    let (Some(parsed), Some(new_identity)) = (parsed, new_identity) else {
        return Resynced { state, identity, trackid };
    };

    let local_art_path = resolve_album_art_path(parsed.art_url.as_deref());
    state.album_art_path = if !local_art_path.is_empty() {
        local_art_path
    } else if same_track {
        state.album_art_path
    } else {
        String::new()
    };
    state.art_url = match parsed.art_url {
        Some(url) => url,
        None if same_track => state.art_url,
        None => String::new(),
    };
    state.length = match parsed.length_us {
        Some(length) if length >= 0 => Some(length),
        _ if same_track => state.length,
        _ => None,
    };
    // Keep artwork across same-track updates; players may drop metadata keys after a full
    // description.
    state.url = match parsed.url {
        Some(url) => url,
        None if same_track => state.url,
        None => String::new(),
    };
    state.title = parsed.title;
    state.artist = parsed.artist;
    state.album = parsed.album;
    state.album_artist = parsed.album_artist;
    state.genre = parsed.genre;
    Resynced { state, identity: new_identity, trackid: parsed.trackid }
}

/// Binds `bus_name`, runs [`resync`], inserts the entry, then starts its forwarder. Logs and
/// returns on bind failure or `CanControl == false`; uncontrollable sources are not useful in a
/// status bar (ADR-0036).
///
/// Insert before spawn: the forwarder resyncs as soon as it starts, so it could otherwise run
/// before insertion, find nothing to update, and freeze the player. Confirmed live for the first
/// player in a fresh session.
///
/// A name left out at [`MAX_PLAYERS`] is picked up by discovery's re-scan once one leaves.
pub(super) async fn register_player(
    connection: &zbus::Connection,
    registry: &PlayerRegistry,
    events: &UnboundedSender<()>,
    bus_name: String,
) {
    // Checked before any round trip: each refused arrival would otherwise cost a bind, a resync
    // and the collections.
    let full = {
        let guard = registry.lock().expect("mutex poisoned");
        guard.len() >= MAX_PLAYERS && !guard.contains_key(&bus_name)
    };
    if full {
        let message = format!("not tracking {bus_name}: {MAX_PLAYERS} players already are");
        warn_first!(message, "{message}");
        return;
    }
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
    match get_property(player.inner(), "CanControl").await {
        Ok(Some(true)) => {}
        Ok(Some(false)) => {
            debug!("{bus_name} reports CanControl=false; not tracking it");
            return;
        }
        read => debug!("CanControl unread for {bus_name} (tracking anyway): {read:?}"),
    }
    // Before the first read, so a change during it is taken rather than lost or queued.
    let (changes, drain) = match drain(connection, &bus_name).await {
        Ok(drain) => drain,
        Err(err) => {
            debug!("cannot follow {bus_name}'s property changes: {err}");
            return;
        }
    };

    let Resynced { mut state, identity, trackid } = resync(&bus_name, &player, &root, &changes, None).await;
    state.pid = owner_pid(connection, &bus_name).await;
    let (refresh_tx, refresh_rx) = tokio::sync::mpsc::unbounded_channel();
    let collections_forwarder = collections_forwarder(
        bus_name.clone(),
        track_list.clone(),
        playlists.clone(),
        registry.clone(),
        events.clone(),
        refresh_rx,
        Arc::clone(&changes),
    )
    .await;
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
        track_list,
        playlists,
        last_known: state,
        registered: 0,
        track_identity: identity,
        cached_trackid: trackid,
        playlist_revision: 0,
        playlist_request: 0,
        forwarder: None,
        collections_forwarder: None,
        commands: None,
    };
    let previous = {
        let mut guard = registry.lock().expect("mutex poisoned");
        if guard.len() >= MAX_PLAYERS && !guard.contains_key(&bus_name) {
            // Lost a race with a concurrent registration; the check above warned.
            debug!("not tracking {bus_name}: {MAX_PLAYERS} players already are");
            return;
        }
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
        if let Some((_, handle)) = previous.commands {
            handle.abort();
        }
    }

    let forwarder = spawn_player_forwarder(
        bus_name.clone(),
        player,
        root,
        (changes, drain),
        registry.clone(),
        events.clone(),
        refresh_tx,
    );
    let collections_forwarder = tokio::spawn(collections_forwarder);
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

/// Read once per registration, since a new owner registers the name again; later resyncs keep it.
async fn owner_pid(connection: &zbus::Connection, bus_name: &str) -> Option<u32> {
    let name = zbus::names::BusName::try_from(bus_name).ok()?;
    let read = async { zbus::fdo::DBusProxy::new(connection).await?.get_connection_unix_process_id(name).await };
    read.await.inspect_err(|err| debug!("no owner pid for {bus_name}: {err}")).ok()
}

/// Re-runs [`resync`] on each burst of property changes or `Seeked`, updating the entry in place
/// with no debounce or incremental patching. `Position` is excluded from `PropertiesChanged` as too
/// high-frequency, so only `Seeked` signals position changes.
fn spawn_player_forwarder(
    bus_name: String,
    player: MprisPlayerProxy<'static>,
    root: MprisRootProxy<'static>,
    (changes, drain): (Arc<Changes>, Drain),
    registry: PlayerRegistry,
    events: UnboundedSender<()>,
    collections_refresh: UnboundedSender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Owned by this task, so the drain ends with it however it ends.
        let _drain = drain;
        // ponytail: a failed or ended Seeked subscription is not retried; upgrade: resubscribe on a timer.
        let mut seeked = player
            .receive_seeked()
            .await
            .inspect_err(|err| debug!("Seeked subscription failed for {bus_name}: {err}"))
            .ok();

        // Several players update `Position` at an indeterminate time *after* `PlaybackStatus`, so
        // the read taken while handling that signal answers with whatever the player held
        // mid-transition -- for Firefox, sometimes zero. One late re-read fixes it. Anything the
        // player does tell us in the meantime still arrives on its own signal.
        let mut recheck_at: Option<tokio::time::Instant> = None;

        loop {
            let deadline = recheck_at;
            let mut seeked_position = tokio::select! {
                () = changes.player.notified() => None,
                signal = next_optional(&mut seeked) => seeked_wake(signal, &mut seeked),
                () = sleep_until_opt(deadline) => None,
            };
            // One resync for every queued wake. The first wake is registration's, and counts as a
            // status change, so the `Position` registration read is checked again too.
            changes.player.notified().now_or_never();
            let status_changed = std::mem::take(&mut changes.lock().status);
            while let Some(signal) = seeked.as_mut().and_then(|stream| stream.next().now_or_never()) {
                seeked_position = seeked_wake(signal, &mut seeked).or(seeked_position);
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
            let Resynced { mut state, identity, trackid } = resync(&bus_name, &player, &root, &changes, previous).await;

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

/// A `Seeked` stream's item as the position it carries. The stream ending drops it, so it is not
/// polled again.
fn seeked_wake(
    signal: Option<super::proxies::Seeked>,
    seeked: &mut Option<super::proxies::SeekedStream>,
) -> Option<i64> {
    if signal.is_none() {
        *seeked = None;
    }
    signal?.args().ok().map(|args| args.position_us)
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
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

/// Removes a departed `bus_name` (`NameOwnerChanged` with an empty new owner) and aborts its
/// forwarder. No-op if untracked, including skipped `playerctld` or uncontrollable sources.
/// Returns whether it was tracked.
pub(super) fn unregister_player(registry: &PlayerRegistry, bus_name: &str, events: &UnboundedSender<()>) -> bool {
    let removed = registry.lock().expect("mutex poisoned").remove(bus_name);
    let tracked = removed.is_some();
    if let Some(entry) = removed {
        debug!("MPRIS player departed: {bus_name}");
        if let Some(handle) = entry.forwarder {
            handle.abort();
        }
        if let Some(handle) = entry.collections_forwarder {
            handle.abort();
        }
        if let Some((_, handle)) = entry.commands {
            handle.abort();
        }
        let _ = events.send(());
    }
    tracked
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;

    #[test]
    fn a_failed_identity_read_keeps_the_previous_identity() {
        let previous = PlayerState { identity: "Player name".to_string(), ..PlayerState::default() };
        assert_eq!(resolved_player_identity(None, Some(&previous), "test"), "Player name");
        assert_eq!(resolved_player_identity(None, None, "test"), "");
        assert_eq!(resolved_player_identity(Some("New name".to_string()), Some(&previous), "test"), "New name");
        let long = "n".repeat(super::super::MAX_METADATA_TEXT_BYTES + 1);
        assert_eq!(resolved_player_identity(Some(long), None, "test").len(), super::super::MAX_METADATA_TEXT_BYTES);
    }

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
        let read = get_property::<String>(root.inner(), "Identity").await;
        assert!(read.is_err(), "fixture must return a D-Bus error");

        let previous = PlayerState { identity: "Player name".to_string(), ..PlayerState::default() };
        assert_eq!(resolved_player_identity(read.ok().flatten(), Some(&previous), "test"), "Player name");

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
        let identity = parse_metadata(&super::super::metadata::decoded(
            &crate::capabilities::test_support::properties(&[("xesam:title", zbus::zvariant::Value::from("Song"))]),
        ))
        .track_identity();
        let previous = Previous { state: state.clone(), identity: identity.clone(), trackid: Some("/t/1".into()) };
        let after = resync("org.mpris.MediaPlayer2.test", &player, &root, &Changes::new(), Some(previous)).await;
        assert_eq!((after.state, after.identity, after.trackid.as_deref()), (state, identity, Some("/t/1")));
    }
}

#[cfg(test)]
mod resync_tests {
    use super::*;
    use crate::capabilities::test_support::{p2p_pair_serving, poll_until, private_bus, properties, within};
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
        /// `Position` reads begun, stalled ones included.
        asked: Arc<AtomicUsize>,
        /// Fails every `Position` read once set.
        failing: Arc<std::sync::atomic::AtomicBool>,
        /// `Volume` reads, each counted once it has its value and before it waits on `volume_gate`.
        volume_reads: Arc<AtomicUsize>,
        volume_gate: Arc<tokio::sync::Mutex<()>>,
        /// `Volume` is 0.75 rather than 0.5 once set.
        louder: Arc<std::sync::atomic::AtomicBool>,
    }

    fn track(trackid: &str, title: &str) -> HashMap<String, OwnedValue> {
        properties(&[
            ("mpris:trackid", Value::from(ObjectPath::try_from(trackid.to_string()).unwrap())),
            ("xesam:title", Value::from(title.to_string())),
            ("xesam:artist", Value::from(vec!["Artist"])),
            ("mpris:length", Value::from(240_000_000_i64)),
            ("mpris:artUrl", Value::from("https://example.com/art.jpg")),
        ])
    }

    #[rustfmt::skip]
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        #[zbus(property)] fn playback_status(&self) -> String { "Paused".into() }
        #[zbus(property)] fn metadata(&self) -> HashMap<String, OwnedValue> { track("/t/1", "Song") }
        /// A new reading per call, so every resync changes the state and wakes `events`.
        #[zbus(property)]
        async fn position(&self) -> zbus::fdo::Result<i64> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            let _open = self.gate.lock().await;
            if self.failing.load(Ordering::Relaxed) {
                return Err(zbus::fdo::Error::Failed("no position".into()));
            }
            Ok(1_000 * i64::try_from(self.position_reads.fetch_add(1, Ordering::Relaxed)).unwrap())
        }
        #[zbus(property)] fn can_control(&self) -> bool { true }
        #[zbus(property)] fn can_go_next(&self) -> bool { true }
        #[zbus(property)] fn can_go_previous(&self) -> bool { false }
        #[zbus(property)] fn can_seek(&self) -> bool { true }
        #[zbus(property)] fn can_play(&self) -> bool { false }
        #[zbus(property)] fn can_pause(&self) -> bool { true }
        #[zbus(property)]
        async fn volume(&self) -> f64 {
            let volume = if self.louder.load(Ordering::Relaxed) { 0.75 } else { 0.5 };
            self.volume_reads.fetch_add(1, Ordering::Relaxed);
            let _open = self.volume_gate.lock().await;
            volume
        }
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
        /// Past the text cap, so the capped read shows.
        #[zbus(property)] fn desktop_entry(&self) -> String { "f".repeat(super::super::MAX_METADATA_TEXT_BYTES + 1) }
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

    /// Returns once `Position` reads stop for 150 ms: queued wakes, their resyncs and the 100 ms recheck are done.
    async fn settle(player: &Player) {
        within(async {
            loop {
                let reads = player.position_reads.load(Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(150)).await;
                if player.position_reads.load(Ordering::Relaxed) == reads {
                    return;
                }
            }
        })
        .await;
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

    /// Polls until `last_known` satisfies `done`, so a test reads state only once it has landed.
    async fn until(registry: &PlayerRegistry, done: impl Fn(&PlayerState) -> bool) {
        poll_until(|| done(&last_known(registry)).then_some(())).await;
    }

    /// Position updates must not move a player under a config holding its index.
    #[tokio::test]
    async fn a_player_that_re_registers_holds_its_place() {
        let (connection, _peer, _) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        for name in [NAME, "org.mpris.MediaPlayer2.other", NAME] {
            register_player(&connection, &registry, &events, name.to_string()).await;
        }

        let ids: Vec<String> = ordered_players(&registry).into_iter().map(|player| player.id).collect();
        assert_eq!(ids, ["fixture", "other"]);
    }

    /// Past the limit a new player is not tracked; one already tracked still re-registers.
    #[tokio::test]
    async fn players_past_the_limit_are_not_tracked() {
        let (connection, _peer, _) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        for index in 0..=super::super::MAX_PLAYERS {
            register_player(&connection, &registry, &events, format!("{NAME}{index}")).await;
        }
        register_player(&connection, &registry, &events, format!("{NAME}0")).await;

        let ids: Vec<String> = ordered_players(&registry).into_iter().map(|player| player.id).collect();
        let expected: Vec<String> = (0..super::super::MAX_PLAYERS).map(|index| format!("fixture{index}")).collect();
        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn a_player_keeps_its_owner_pid_through_a_resync() {
        let bus = private_bus().await;
        let peer = bus.connection().await;
        peer.object_server().at(PATH, Player::default()).await.unwrap();
        peer.object_server().at(PATH, Root).await.unwrap();
        peer.request_name(NAME).await.unwrap();
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&bus.connection().await, &registry, &events, NAME.to_string()).await;
        assert_eq!(last_known(&registry).pid, Some(std::process::id()));

        properties_changed(&peer, HashMap::from([("Volume", 0.25.into())])).await;
        until(&registry, |state| state.volume == 25.0).await;
        assert_eq!(last_known(&registry).pid, Some(std::process::id()));
    }

    #[tokio::test]
    async fn every_concurrent_read_lands_in_its_own_field() {
        let (connection, _peer, _) = serve().await;
        let player = bind_player(&connection, NAME).await.unwrap();
        let root = bind_root(&connection, NAME).await.unwrap();
        // The flags the fixture answers `false` start `true`, so one left unread shows.
        let flags = PlayerState { can_go_previous: true, can_play: true, can_raise: true, ..PlayerState::default() };
        let previous = Previous { state: flags, ..Previous::default() };
        let Resynced { state, trackid, .. } = resync(NAME, &player, &root, &Changes::new(), Some(previous)).await;
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
            art_url: "https://example.com/art.jpg".into(),
            position: state.position,
            position_updated_at: state.position_updated_at,
            length: Some(240_000_000),
            desktop_entry: "f".repeat(super::super::MAX_METADATA_TEXT_BYTES),
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
        settle(&player).await;
        // The forwarder's first wake, and that wake's status recheck.
        assert!(reads() - before <= 2, "{} resyncs for the first wake", reads() - before);

        let before = reads();
        let changed =
            [("Volume", 0.25.into()), ("Rate", 1.25.into()), ("Shuffle", false.into()), ("LoopStatus", "None".into())];
        properties_changed(&peer, HashMap::from(changed)).await;
        seeked(&peer, 5_000_000).await;
        until(&registry, |state| state.volume == 25.0 && state.position == Some(5_000_000)).await;
        settle(&player).await; // the upper bound on resyncs needs the quiet period
        assert!(reads() - before <= 3, "{} resyncs for one signal of 4 properties and a Seeked", reads() - before);
        assert!(!last_known(&registry).shuffle);
    }

    /// A player's `ay` metadata entry, through `GetAll` and `PropertiesChanged` alike, is walked
    /// rather than built, and zbus's property cache, which builds it, stays empty.
    #[tokio::test]
    async fn a_megabyte_byte_array_in_metadata_is_skipped_and_never_cached() {
        let (connection, peer, _) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        let mut big = track("/t/2", "Big");
        big.insert("x-cover-bytes".into(), OwnedValue::try_from(Value::from(vec![7u8; 1024 * 1024])).unwrap());
        properties_changed(&peer, HashMap::from([("Metadata", Value::from(big))])).await;
        until(&registry, |state| state.title == "Big").await;
        let proxy = registry.lock().unwrap()[NAME].player.clone();
        assert!(proxy.inner().cached_property_raw("Metadata").is_none());
    }

    /// A negative volume or a zero rate is a player bug that keeps the last value, and only a
    /// `PlaybackStatus` change reads `Position` once more after its resync.
    #[tokio::test]
    async fn bad_values_keep_the_last_and_only_a_status_change_rechecks_the_position() {
        let (connection, peer, player) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        settle(&player).await;
        let reads = || player.position_reads.load(Ordering::Relaxed);

        let before = reads();
        properties_changed(&peer, HashMap::from([("Volume", (-0.5).into()), ("Rate", 0.0.into())])).await;
        // `settle` alone returns if the resync starts late.
        poll_until(|| (reads() > before).then_some(())).await;
        settle(&player).await;
        assert_eq!(reads() - before, 1, "one resync, no recheck");
        assert_eq!((last_known(&registry).volume, last_known(&registry).rate), (50.0, 1.5));

        let before = reads();
        properties_changed(&peer, HashMap::from([("PlaybackStatus", "Paused".into())])).await;
        poll_until(|| (reads() >= before + 2).then_some(())).await;
        settle(&player).await;
        assert_eq!(reads() - before, 2, "the resync and its recheck");

        let before = reads();
        properties_changed(&peer, HashMap::from([("Position", 5_i64.into())])).await;
        settle(&player).await;
        assert_eq!(reads(), before, "a change to nothing read wakes nothing");
    }

    #[tokio::test]
    async fn dropping_the_drain_ends_its_task() {
        let (connection, _peer, _) = serve().await;
        let (changes, drain) = within(drain(&connection, NAME)).await.unwrap();
        let held = Arc::downgrade(&changes);
        drop((changes, drain));
        poll_until(|| held.upgrade().is_none().then_some(())).await;
    }

    /// A `MediaPlayer2` change lands, and an invalidated property is read again, the value a
    /// signal set before it giving way.
    #[tokio::test]
    async fn a_root_change_lands_and_an_invalidated_property_is_read_again() {
        let (connection, peer, player) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        let emitter = zbus::object_server::SignalEmitter::new(&peer, PATH).unwrap();
        let root = zbus::names::InterfaceName::from_static_str_unchecked("org.mpris.MediaPlayer2");
        let changed = HashMap::from([("CanRaise", Value::from(true))]);
        zbus::fdo::Properties::properties_changed(&emitter, root, changed, (&[][..]).into()).await.unwrap();
        until(&registry, |state| state.can_raise).await;

        properties_changed(&peer, HashMap::from([("Volume", 0.25.into())])).await;
        until(&registry, |state| state.volume == 25.0).await;
        let invalidate = || async {
            let interface = zbus::names::InterfaceName::from_static_str_unchecked(PLAYER);
            let (changed, invalidated) = (HashMap::<&str, Value<'_>>::new(), ["Volume"]);
            let invalidated = (&invalidated[..]).into();
            zbus::fdo::Properties::properties_changed(&emitter, interface, changed, invalidated).await.unwrap();
        };
        invalidate().await;
        until(&registry, |state| state.volume == 50.0).await;

        // Invalidated again after the read it started took the old value: that reply must not
        // refill what the second invalidation cleared, or the reread that follows cannot.
        let stalled = player.volume_gate.lock().await;
        let read = player.volume_reads.load(Ordering::Relaxed);
        invalidate().await;
        poll_until(|| (player.volume_reads.load(Ordering::Relaxed) > read).then_some(())).await;
        player.louder.store(true, Ordering::Relaxed);
        invalidate().await;
        drop(stalled);
        until(&registry, |state| state.volume == 75.0).await;
    }

    /// zbus queues 64 messages per match rule, then stops reading the connection until one is
    /// taken: changes must keep being taken while a resync awaits a reply, or that reply never lands.
    #[tokio::test]
    async fn a_flood_of_changes_during_a_stalled_read_still_lands() {
        let (connection, peer, player) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        settle(&player).await;
        let stalled = player.gate.lock().await;
        let asked = player.asked.load(Ordering::Relaxed);
        properties_changed(&peer, HashMap::from([("Volume", 0.75.into())])).await;
        poll_until(|| (player.asked.load(Ordering::Relaxed) > asked).then_some(())).await;
        for step in 1..=300 {
            properties_changed(&peer, HashMap::from([("Volume", (f64::from(step) / 1000.0).into())])).await;
        }
        drop(stalled);
        until(&registry, |state| state.volume == 30.0).await;
    }

    /// The value a signal carries is the update: a failing read after it must not lose it.
    #[tokio::test]
    async fn a_status_carried_by_its_signal_lands_when_the_next_read_fails() {
        let (connection, peer, player) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        settle(&player).await;
        player.failing.store(true, Ordering::Relaxed);
        // The fixture still answers `Paused`: only the signal says `Playing`.
        properties_changed(&peer, HashMap::from([("PlaybackStatus", "Playing".into())])).await;
        until(&registry, |state| state.play_state == shared::state::mpris::PlayState::Playing).await;
    }

    #[tokio::test]
    async fn a_seeked_batched_with_a_track_change_leaves_the_new_track_live_position() {
        let (connection, peer, player) = serve().await;
        let registry: PlayerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        register_player(&connection, &registry, &events, NAME.to_string()).await;
        settle(&player).await;
        let stalled = player.gate.lock().await;
        // A resync stuck on `Position` while the next batch queues up behind it.
        properties_changed(&peer, HashMap::from([("Volume", 0.75.into())])).await;
        settle(&player).await;
        seeked(&peer, 300_000_000).await;
        properties_changed(&peer, HashMap::from([("Metadata", Value::from(track("/t/2", "Next")))])).await;
        settle(&player).await; // the batch must queue behind the stalled resync
        drop(stalled);
        until(&registry, |state| state.title == "Next").await;
        assert_ne!(last_known(&registry).position, Some(300_000_000), "the old track's seek target");
    }
}

#[cfg(test)]
pub(super) mod ordering_tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair;

    /// An entry with only the two ordering fields. Binding a proxy makes no call, so a p2p pair
    /// with nobody answering is enough.
    pub(in super::super) async fn entry(connection: &zbus::Connection, id: &str, registered: u64) -> PlayerEntry {
        PlayerEntry {
            player: bind_player(connection, "org.mpris.MediaPlayer2.probe").await.expect("binding makes no call"),
            root: bind_root(connection, "org.mpris.MediaPlayer2.probe").await.expect("binding makes no call"),
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
            commands: None,
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
}

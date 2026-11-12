//! [`MprisController`]: `mantle.mpris`'s write dispatcher and state owner.

pub use shared::state::mpris::MprisState;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use shared::debug;
use tokio::sync::mpsc::UnboundedSender;

use super::collections::{PlaylistsState, read_playlists};
use super::metadata::clamp_seek_target;
use super::proxies::{MprisPlaylistsProxy, MprisTrackListProxy};
use super::watcher::{service_name_for_id, spawn_discovery};
use shared::action::PlayerCommand;
use shared::state::mpris::LoopStatus;

/// Every command fails this one way, only into a `debug!`; nothing matches on it.
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

    /// Queues `command` behind `id`'s earlier ones: they land in dispatch order, and a player that
    /// never answers holds up only its own. ponytail: 32 queued, then the newest is refused, as an
    /// mpsc queue cannot drop its oldest; a deque that does if a slider on a slow player loses its end.
    pub(super) fn enqueue(&self, id: &str, command: super::player::Command) {
        let mut guard = self.registry.lock().expect("mutex poisoned");
        let Some(entry) = guard.get_mut(&service_name_for_id(id)) else {
            return debug!("command for {id:?} failed: {UNKNOWN_PLAYER}");
        };
        let (queue, _) = entry.commands.get_or_insert_with(|| {
            let (queue, mut commands) = tokio::sync::mpsc::channel::<super::player::Command>(32);
            (queue, tokio::spawn(futures_util::stream::poll_fn(move |cx| commands.poll_recv(cx)).for_each(|c| c)))
        });
        if queue.try_send(command).is_err() && shared::log::first_failure(&format!("mpris-queue-{id}")) {
            shared::warn!("{id:?} has 32 commands waiting on it; refusing more until it answers");
        }
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

    /// `value` is a percent; MPRIS `Volume` is a fraction.
    pub async fn set_volume(&self, id: &str, value: f64) {
        if !value.is_finite() || value < 0.0 {
            debug!("invalid volume for {id:?}");
            return;
        }
        if let Some(p) = self.find_player(id)
            && let Err(err) = p.set_volume(value / 100.0).await
        {
            debug!("set volume for {id:?} failed: {err}");
        }
    }
    pub async fn set_rate(&self, id: &str, value: f64) {
        let within_limits = self.find_rate_limits(id).is_none_or(|(minimum, maximum)| {
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
    pub async fn set_loop_status(&self, id: &str, value: LoopStatus) {
        if let Some(p) = self.find_player(id)
            && let Err(err) = p.set_loop_status(value.as_str()).await
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
    /// `target` from the start, and a negative reading can overflow the subtraction for a large target.
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

    /// A live `Position` read. Never the cached snapshot: it dates from the last signal, so it is
    /// stale by however long the track has played since.
    async fn live_position(&self, id: &str) -> Option<i64> {
        let player = self.find_player(id)?;
        player.position().await.inspect_err(|err| debug!("live Position read failed for {id:?}: {err}")).ok()
    }

    /// `mpris:seek(id, pos_us)`: `SetPosition(cached trackid, clamped pos_us)` when available;
    /// otherwise relative `Seek` from a live position because some players never report
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
        self.registry.lock().expect("mutex poisoned").get(&bus_name).map(|entry| entry.root.clone())
    }

    /// `(MinimumRate, MaximumRate)` as last read.
    fn find_rate_limits(&self, id: &str) -> Option<(f64, f64)> {
        let bus_name = service_name_for_id(id);
        let guard = self.registry.lock().expect("mutex poisoned");
        guard.get(&bus_name).map(|entry| (entry.last_known.minimum_rate, entry.last_known.maximum_rate))
    }

    fn find_seek_context(&self, id: &str) -> Option<SeekContext> {
        let bus_name = service_name_for_id(id);
        let guard = self.registry.lock().expect("mutex poisoned");
        let entry = guard.get(&bus_name)?;
        Some(SeekContext {
            bus_name,
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
                || uri.as_bytes().get(i + 1..i + 3).is_some_and(|escape| escape.iter().all(u8::is_ascii_hexdigit))
        })
}

/// [`MprisController::find_seek_context`]'s answer.
struct SeekContext {
    bus_name: String,
    player: super::proxies::MprisPlayerProxy<'static>,
    trackid: Option<String>,
    length: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::{p2p_pair_serving, poll_until};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A player with no `mpris:trackid`, so an absolute seek converts against `Position`, which
    /// fails once `refusing` is set. `seeks` records every `Seek` offset.
    #[derive(Clone, Default)]
    struct Player {
        refusing: Arc<AtomicBool>,
        seeks: Arc<Mutex<Vec<i64>>>,
        /// `Next` records `-1` in `seeks`, then never answers.
        hung: Arc<AtomicBool>,
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        fn seek(&self, offset: i64) {
            self.seeks.lock().unwrap().push(offset);
        }
        async fn next(&self) {
            self.seeks.lock().unwrap().push(-1);
            if self.hung.load(Ordering::Relaxed) {
                std::future::pending::<()>().await;
            }
        }
        #[zbus(property)]
        fn position(&self) -> zbus::fdo::Result<i64> {
            if self.refusing.load(Ordering::Relaxed) {
                return Err(zbus::fdo::Error::Failed("no position".into()));
            }
            Ok(5_000_000)
        }
        #[zbus(property)]
        fn can_seek(&self) -> bool {
            true
        }
    }

    struct Root;

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        #[zbus(property)]
        fn identity(&self) -> String {
            "Fixture".into()
        }
    }

    /// The cached position is a snapshot from the last signal, seconds old while playing: an
    /// offset from it lands somewhere else, so a failed live read refuses the seek instead.
    #[tokio::test]
    async fn an_absolute_seek_converts_against_a_live_position_and_without_one_sends_nothing() {
        let player = Player::default();
        let served = player.clone();
        let (connection, _peer) = p2p_pair_serving(|builder| {
            builder.serve_at("/org/mpris/MediaPlayer2", served)?.serve_at("/org/mpris/MediaPlayer2", Root)
        })
        .await;
        let controller = MprisController::inert();
        let name = "org.mpris.MediaPlayer2.fixture";
        super::super::player::register_player(&connection, &controller.registry, &controller.events, name.into()).await;
        assert_eq!(controller.registry.lock().unwrap()[name].last_known.position, Some(5_000_000));

        controller.seek("fixture", 60_000_000).await;
        player.refusing.store(true, Ordering::Relaxed);
        controller.seek("fixture", 60_000_000).await;

        assert_eq!(*player.seeks.lock().unwrap(), [55_000_000], "only the seek with a live position goes out");
    }

    /// A player served on its own p2p pair and tracked as `org.mpris.MediaPlayer2.<id>`.
    async fn tracked_player(controller: &MprisController, id: &str) -> (Player, zbus::Connection) {
        let player = Player::default();
        let served = player.clone();
        let (connection, peer) = p2p_pair_serving(|builder| {
            builder.serve_at("/org/mpris/MediaPlayer2", served)?.serve_at("/org/mpris/MediaPlayer2", Root)
        })
        .await;
        let name = format!("org.mpris.MediaPlayer2.{id}");
        super::super::player::register_player(&connection, &controller.registry, &controller.events, name).await;
        (player, peer)
    }

    fn command(action: &str, arguments: serde_json::Value) -> shared::CommandEnvelope {
        let serde_json::Value::Array(arguments) = arguments else { panic!("positional arguments") };
        let (capability, action) = ("mpris".into(), action.into());
        let params =
            shared::CommandParams { generation_id: 1, capability, action, arguments, legacy_expected_revision: 0 };
        shared::CommandEnvelope { params, id: 0 }
    }

    /// The seek reads `Position` first; spawned apart, the `Next` behind it overtook it and the seek
    /// landed on the next track.
    #[tokio::test]
    async fn one_players_commands_land_in_dispatch_order() {
        let controller = MprisController::inert();
        let (player, _peer) = tracked_player(&controller, "fixture").await;
        super::super::dispatch(&controller, &command("seek", serde_json::json!(["fixture", 60_000_000])));
        super::super::dispatch(&controller, &command("control", serde_json::json!(["fixture", "next"])));
        poll_until(|| (player.seeks.lock().unwrap().len() == 2).then_some(())).await;
        assert_eq!(*player.seeks.lock().unwrap(), [55_000_000, -1], "the seek was dispatched first");
    }

    /// The abort handle of `id`'s command queue task, which ends only by being aborted.
    fn queue_task(controller: &MprisController, id: &str) -> tokio::task::AbortHandle {
        let registry = controller.registry.lock().unwrap();
        registry[&format!("org.mpris.MediaPlayer2.{id}")]
            .commands
            .as_ref()
            .expect("a command was queued")
            .1
            .abort_handle()
    }

    /// Well inside the five-second call timeout that would otherwise end a hung call.
    async fn ends_promptly(task: &tokio::task::AbortHandle) {
        let ended = async {
            while !task.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), ended).await.expect("the hung player's queue ended");
    }

    /// Each player has its own queue: one that never answers holds up only its own commands, and
    /// leaving drops them, the one in flight included.
    #[tokio::test]
    async fn a_hung_player_holds_up_only_its_own_commands_until_it_leaves() {
        let controller = MprisController::inert();
        let (hung, _hung_peer) = tracked_player(&controller, "hung").await;
        let (other, _other_peer) = tracked_player(&controller, "other").await;
        hung.hung.store(true, Ordering::Relaxed);
        for id in ["hung", "hung", "other"] {
            super::super::dispatch(&controller, &command("control", serde_json::json!([id, "next"])));
        }
        poll_until(|| (other.seeks.lock().unwrap().len() == 1).then_some(())).await;
        assert_eq!(*hung.seeks.lock().unwrap(), [-1], "the second waits behind the first");
        let queue = queue_task(&controller, "hung");

        super::super::player::unregister_player(
            &controller.registry,
            "org.mpris.MediaPlayer2.hung",
            &controller.events,
        );
        ends_promptly(&queue).await;
    }

    /// A player that restarts under the same name is a new registration: the old queue, and the
    /// command it is stuck on, end with the old entry.
    #[tokio::test]
    async fn a_re_registered_player_drops_its_old_queue() {
        let controller = MprisController::inert();
        let (hung, _peer) = tracked_player(&controller, "hung").await;
        hung.hung.store(true, Ordering::Relaxed);
        super::super::dispatch(&controller, &command("control", serde_json::json!(["hung", "next"])));
        poll_until(|| (hung.seeks.lock().unwrap().len() == 1).then_some(())).await;
        let queue = queue_task(&controller, "hung");

        let _again = tracked_player(&controller, "hung").await;
        ends_promptly(&queue).await;
    }

    /// One command in flight and 32 waiting fit without a warning; the next is refused, and warns.
    #[tokio::test]
    async fn a_full_queue_refuses_the_newest_command_and_warns_once() {
        let controller = MprisController::inert();
        for (id, sent, warned) in [("fits", 33, false), ("overflows", 34, true)] {
            let (hung, _peer) = tracked_player(&controller, id).await;
            hung.hung.store(true, Ordering::Relaxed);
            let next = command("control", serde_json::json!([id, "next"]));
            super::super::dispatch(&controller, &next);
            poll_until(|| (hung.seeks.lock().unwrap().len() == 1).then_some(())).await;
            (1..sent).for_each(|_| super::super::dispatch(&controller, &next));
            // `first_failure` is true only for a key nothing has warned under yet.
            assert_eq!(!shared::log::first_failure(&format!("mpris-queue-{id}")), warned, "{id}");
        }
    }

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

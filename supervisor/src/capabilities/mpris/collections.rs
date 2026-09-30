//! Bounded public state for optional TrackList and Playlists interfaces.

use futures_util::{Stream, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use zbus::zvariant::OwnedObjectPath;

use super::metadata::parse_metadata;
use super::player::PlayerRegistry;
use super::proxies::{MprisPlaylistsProxy, MprisTrackListProxy};

const TRACK_LIMIT: usize = 100;
const MAX_TRACK_IDS: usize = 10_000;
pub(super) const PLAYLIST_PAGE_LIMIT: u32 = 100;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TrackSummary {
    /// TrackList object path, used by `track_list_go_to` and `track_list_remove_track`.
    pub id: String,
    /// Track title, empty when the player has none.
    pub title: String,
    /// Track artists joined with `", "`.
    pub artist: String,
    /// Track length in microseconds, or `-1` when unknown.
    pub length: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TrackListState {
    /// At most 100 tracks around the current track; the full playlist remains player-owned.
    pub tracks: Vec<TrackSummary>,
    /// Current TrackList object path, or empty if unknown.
    pub current_track: String,
    /// Whether the player permits add and remove calls.
    pub can_edit: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PlaylistSummary {
    /// Stable playlist object path.
    pub id: String,
    /// User-facing playlist name.
    pub name: String,
    /// Icon URI, or empty if absent.
    pub icon: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
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

pub(super) async fn read_track_list(
    proxy: &MprisTrackListProxy<'_>,
    current_track: Option<&str>,
) -> zbus::Result<TrackListState> {
    let ids = proxy.tracks().await?;
    // ponytail: TrackList exposes the full ID array, so D-Bus deserializes it before this cap.
    // A paged protocol or a decoder limit would bound the incoming allocation too.
    if ids.len() > MAX_TRACK_IDS {
        return Err(zbus::Error::Failure("TrackList exceeds the supported ID count".into()));
    }
    let current_index = current_track.and_then(|current| ids.iter().position(|id| id.as_str() == current));
    let start = current_index.map_or(0, |index| index.saturating_sub(TRACK_LIMIT / 2));
    let start = start.min(ids.len().saturating_sub(TRACK_LIMIT));
    let window = &ids[start..ids.len().min(start + TRACK_LIMIT)];
    let metadata = proxy.get_tracks_metadata(window.to_vec()).await?;
    if metadata.len() != window.len() {
        return Err(zbus::Error::Failure("GetTracksMetadata returned the wrong number of tracks".into()));
    }
    let tracks = window
        .iter()
        .zip(metadata)
        .map(|(id, metadata)| {
            let parsed = parse_metadata(&metadata);
            TrackSummary {
                id: id.to_string(),
                title: parsed.title,
                artist: parsed.artist,
                length: parsed.length_us.unwrap_or(-1),
            }
        })
        .collect();
    Ok(TrackListState {
        tracks,
        current_track: current_track.unwrap_or_default().to_string(),
        can_edit: proxy.can_edit_tracks().await?,
    })
}

pub(super) async fn read_playlists(
    proxy: &MprisPlaylistsProxy<'_>,
    index: u32,
    requested_count: u32,
    order: &str,
    reverse: bool,
    fallback_order: bool,
) -> zbus::Result<PlaylistsState> {
    let orderings = proxy.orderings().await?;
    let selected_order =
        if order.is_empty() || (fallback_order && !orderings.iter().any(|supported| supported == order)) {
            orderings.first().cloned().unwrap_or_else(|| "Alphabetical".to_string())
        } else {
            order.to_string()
        };
    let count = proxy.playlist_count().await?;
    if !orderings.iter().any(|supported| supported == &selected_order) {
        return Err(zbus::Error::Failure(format!("unsupported playlist ordering {selected_order:?}")));
    }
    let (active_valid, active) = proxy.active_playlist().await?;
    let active = active_valid.then(|| active.into());
    let page_size = requested_count.min(PLAYLIST_PAGE_LIMIT);
    let items = proxy.get_playlists(index, page_size, &selected_order, reverse).await?;
    if items.len() > page_size as usize {
        return Err(zbus::Error::Failure("GetPlaylists returned more than the requested page".into()));
    }
    Ok(PlaylistsState {
        count,
        orderings,
        active,
        index,
        page_size,
        order: selected_order,
        reverse,
        playlists: items.into_iter().map(Into::into).collect(),
    })
}

impl From<(OwnedObjectPath, String, String)> for PlaylistSummary {
    fn from((id, name, icon): (OwnedObjectPath, String, String)) -> Self {
        Self { id: id.to_string(), name, icon }
    }
}

async fn next_optional<S: Stream + Unpin>(stream: &mut Option<S>) -> Option<S::Item> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

pub(super) fn spawn_collections_forwarder(
    bus_name: String,
    track_list: Option<MprisTrackListProxy<'static>>,
    playlists: Option<MprisPlaylistsProxy<'static>>,
    registry: PlayerRegistry,
    events: UnboundedSender<()>,
    mut refresh: UnboundedReceiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut replaced = match &track_list {
            Some(proxy) => proxy.receive_track_list_replaced().await.ok(),
            None => None,
        };
        let mut added = match &track_list {
            Some(proxy) => proxy.receive_track_added().await.ok(),
            None => None,
        };
        let mut removed = match &track_list {
            Some(proxy) => proxy.receive_track_removed().await.ok(),
            None => None,
        };
        let mut metadata_changed = match &track_list {
            Some(proxy) => proxy.receive_track_metadata_changed().await.ok(),
            None => None,
        };
        let mut can_edit = match &track_list {
            Some(proxy) => Some(proxy.receive_can_edit_tracks_changed().await),
            None => None,
        };
        let mut playlist_changed = match &playlists {
            Some(proxy) => proxy.receive_playlist_changed().await.ok(),
            None => None,
        };
        let mut playlist_count = match &playlists {
            Some(proxy) => Some(proxy.receive_playlist_count_changed().await),
            None => None,
        };
        let mut orderings = match &playlists {
            Some(proxy) => Some(proxy.receive_orderings_changed().await),
            None => None,
        };
        let mut active_playlist = match &playlists {
            Some(proxy) => Some(proxy.receive_active_playlist_changed().await),
            None => None,
        };

        loop {
            let changed = tokio::select! {
                Some(_) = refresh.recv() => true,
                Some(_) = next_optional(&mut replaced) => true,
                Some(_) = next_optional(&mut added) => true,
                Some(_) = next_optional(&mut removed) => true,
                Some(_) = next_optional(&mut metadata_changed) => true,
                Some(_) = next_optional(&mut can_edit) => true,
                Some(_) = next_optional(&mut playlist_changed) => true,
                Some(_) = next_optional(&mut playlist_count) => true,
                Some(_) = next_optional(&mut orderings) => true,
                Some(_) = next_optional(&mut active_playlist) => true,
                else => false,
            };
            if !changed {
                return;
            }
            let (track_id, old_playlists, playlist_revision) = {
                let guard = registry.lock().expect("mpris registry mutex poisoned");
                let Some(entry) = guard.get(&bus_name) else { return };
                (entry.cached_trackid.clone(), entry.last_known.playlists.clone(), entry.playlist_revision)
            };
            let new_track_list = match &track_list {
                Some(proxy) => read_track_list(proxy, track_id.as_deref()).await.ok(),
                None => None,
            };
            let new_playlists = match &playlists {
                Some(proxy) => read_playlists(
                    proxy,
                    old_playlists.index,
                    if old_playlists.order.is_empty() { PLAYLIST_PAGE_LIMIT } else { old_playlists.page_size },
                    &old_playlists.order,
                    old_playlists.reverse,
                    true,
                )
                .await
                .ok(),
                None => None,
            };
            let mut guard = registry.lock().expect("mpris registry mutex poisoned");
            let Some(entry) = guard.get_mut(&bus_name) else { return };
            let mut changed = false;
            if entry.cached_trackid == track_id
                && let Some(new_track_list) = new_track_list
                && entry.last_known.track_list != new_track_list
            {
                entry.last_known.track_list = new_track_list;
                changed = true;
            }
            if entry.playlist_revision == playlist_revision
                && entry.playlist_request == 0
                && let Some(new_playlists) = new_playlists
                && entry.last_known.playlists != new_playlists
            {
                entry.last_known.playlists = new_playlists;
                entry.playlist_revision += 1;
                changed = true;
            }
            drop(guard);
            if changed && events.send(()).is_err() {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::mpris::proxies::{MprisPlaylistsProxy, MprisTrackListProxy};
    use crate::capabilities::test_support::{p2p_pair_serving, within};
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use zbus::zvariant::{ObjectPath, OwnedValue};

    struct TrackPeer {
        edits: Arc<Mutex<Vec<String>>>,
        metadata_requests: Arc<Mutex<Vec<usize>>>,
        track_count: Arc<AtomicUsize>,
        short_metadata: Arc<AtomicBool>,
    }

    struct PlaylistPeer {
        edits: Arc<Mutex<Vec<String>>>,
    }

    fn path(value: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(value).unwrap()
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.TrackList")]
    impl TrackPeer {
        async fn get_tracks_metadata(&self, ids: Vec<OwnedObjectPath>) -> Vec<HashMap<String, OwnedValue>> {
            self.metadata_requests.lock().unwrap().push(ids.len());
            let count = ids.len() - usize::from(self.short_metadata.load(Ordering::Relaxed));
            (0..count).map(|_| HashMap::new()).collect()
        }

        async fn add_track(&self, uri: &str, after_track: ObjectPath<'_>, set_as_current: bool) {
            self.edits.lock().unwrap().push(format!("add:{uri}:{}:{set_as_current}", after_track.as_str()));
        }

        async fn remove_track(&self, track_id: ObjectPath<'_>) {
            self.edits.lock().unwrap().push(format!("remove:{}", track_id.as_str()));
        }

        async fn go_to(&self, track_id: ObjectPath<'_>) {
            self.edits.lock().unwrap().push(format!("goto:{}", track_id.as_str()));
        }

        #[zbus(property)]
        async fn tracks(&self) -> Vec<OwnedObjectPath> {
            (0..self.track_count.load(Ordering::Relaxed)).map(|index| path(&format!("/track/{index}"))).collect()
        }

        #[zbus(property)]
        async fn can_edit_tracks(&self) -> bool {
            true
        }

        #[zbus(signal)]
        async fn track_list_replaced(
            signal_emitter: &zbus::object_server::SignalEmitter<'_>,
            tracks: Vec<OwnedObjectPath>,
            current_track: OwnedObjectPath,
        ) -> zbus::Result<()>;
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Playlists")]
    impl PlaylistPeer {
        async fn activate_playlist(&self, id: ObjectPath<'_>) {
            self.edits.lock().unwrap().push(format!("activate:{}", id.as_str()));
        }

        async fn get_playlists(
            &self,
            index: u32,
            max_count: u32,
            order: &str,
            reverse_order: bool,
        ) -> Vec<(OwnedObjectPath, String, String)> {
            self.edits.lock().unwrap().push(format!("get:{index}:{max_count}:{order}:{reverse_order}"));
            (0..max_count.min(2))
                .map(|index| (path(&format!("/playlist/{index}")), format!("List {index}"), String::new()))
                .collect()
        }

        #[zbus(property)]
        async fn playlist_count(&self) -> u32 {
            1000
        }

        #[zbus(property)]
        async fn orderings(&self) -> Vec<String> {
            vec!["Alphabetical".to_string()]
        }

        #[zbus(property)]
        async fn active_playlist(&self) -> (bool, (OwnedObjectPath, String, String)) {
            (true, (path("/playlist/0"), "List 0".into(), String::new()))
        }

        #[zbus(signal)]
        async fn playlist_changed(
            signal_emitter: &zbus::object_server::SignalEmitter<'_>,
            playlist: (OwnedObjectPath, String, String),
        ) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn collection_proxies_read_bounded_state_and_send_track_and_playlist_actions() {
        let edits = Arc::new(Mutex::new(Vec::new()));
        let metadata_requests = Arc::new(Mutex::new(Vec::new()));
        let track_count = Arc::new(AtomicUsize::new(300));
        let short_metadata = Arc::new(AtomicBool::new(false));
        let track_edits = edits.clone();
        let playlist_edits = edits.clone();
        let requests_for_peer = metadata_requests.clone();
        let count_for_peer = track_count.clone();
        let short_for_peer = short_metadata.clone();
        let (client, server) = p2p_pair_serving(|builder| {
            builder
                .serve_at(
                    "/tracklist",
                    TrackPeer {
                        edits: track_edits,
                        metadata_requests: requests_for_peer,
                        track_count: count_for_peer,
                        short_metadata: short_for_peer,
                    },
                )?
                .serve_at("/playlists", PlaylistPeer { edits: playlist_edits })
        })
        .await;
        let tracks = MprisTrackListProxy::builder(&client).path("/tracklist").unwrap().build().await.unwrap();
        let playlists = MprisPlaylistsProxy::builder(&client).path("/playlists").unwrap().build().await.unwrap();

        let track_state = read_track_list(&tracks, Some("/track/250")).await.unwrap();
        assert_eq!(track_state.current_track, "/track/250");
        assert_eq!(track_state.tracks.len(), TRACK_LIMIT);
        assert_eq!(track_state.tracks.first().unwrap().id, "/track/200");
        assert_eq!(*metadata_requests.lock().unwrap(), [TRACK_LIMIT]);
        assert!(track_state.can_edit);
        short_metadata.store(true, Ordering::Relaxed);
        assert!(read_track_list(&tracks, Some("/track/250")).await.is_err());
        short_metadata.store(false, Ordering::Relaxed);
        track_count.store(MAX_TRACK_IDS + 1, Ordering::Relaxed);
        let oversized_tracks = MprisTrackListProxy::builder(&client).path("/tracklist").unwrap().build().await.unwrap();
        assert!(read_track_list(&oversized_tracks, Some("/track/250")).await.is_err());
        track_count.store(300, Ordering::Relaxed);
        let mut replaced = tracks.receive_track_list_replaced().await.unwrap();
        TrackPeer::track_list_replaced(
            &zbus::object_server::SignalEmitter::new(&server, "/tracklist").unwrap(),
            vec![path("/track/new")],
            path("/track/new"),
        )
        .await
        .unwrap();
        assert!(within(replaced.next()).await.is_some());
        tracks
            .add_track(
                "file:///tmp/a.ogg",
                ObjectPath::try_from("/org/mpris/MediaPlayer2/TrackList/NoTrack").unwrap(),
                true,
            )
            .await
            .unwrap();
        tracks.remove_track(ObjectPath::try_from("/track/0").unwrap()).await.unwrap();
        tracks.go_to(ObjectPath::try_from("/track/1").unwrap()).await.unwrap();

        let list_state = read_playlists(&playlists, 4, u32::MAX, "", false, false).await.unwrap();
        assert_eq!(list_state.count, 1000);
        assert_eq!(list_state.page_size, 100);
        assert_eq!(list_state.order, "Alphabetical");
        assert_eq!(list_state.playlists.len(), 2);
        assert_eq!(list_state.active.as_ref().map(|active| active.id.as_str()), Some("/playlist/0"));
        let mut playlist_changes = playlists.receive_playlist_changed().await.unwrap();
        PlaylistPeer::playlist_changed(
            &zbus::object_server::SignalEmitter::new(&server, "/playlists").unwrap(),
            (path("/playlist/0"), "Renamed".into(), String::new()),
        )
        .await
        .unwrap();
        assert!(within(playlist_changes.next()).await.is_some());
        playlists.activate_playlist(ObjectPath::try_from("/playlist/0").unwrap()).await.unwrap();
        assert_eq!(
            *edits.lock().unwrap(),
            [
                "add:file:///tmp/a.ogg:/org/mpris/MediaPlayer2/TrackList/NoTrack:true",
                "remove:/track/0",
                "goto:/track/1",
                "get:4:100:Alphabetical:false",
                "activate:/playlist/0",
            ]
        );
        drop(server);
    }
}

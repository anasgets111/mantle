//! Bounded public state for optional TrackList and Playlists interfaces.

pub use shared::state::mpris::{PlaylistSummary, PlaylistsState, TrackListState, TrackSummary};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zbus::zvariant::{OwnedObjectPath, Type};

use super::changes::Changes;
use super::metadata::{Metadata, parse_metadata};
use super::player::PlayerRegistry;
use super::proxies::{MprisPlaylistsProxy, MprisTrackListProxy};
use super::{MAX_METADATA_TEXT_BYTES, MAX_ORDERINGS, fits_text_cap};
use crate::capabilities::{Dict, get_property, truncate_utf8_bytes};

pub(super) const TRACK_LIMIT: usize = 100;
const MAX_TRACK_IDS: usize = 10_000;
pub(super) const PLAYLIST_PAGE_LIMIT: u32 = 100;

/// One property read typed; a mistyped one is an error, as zbus's generated getter made it.
pub(super) async fn property<T: serde::de::DeserializeOwned + Type>(
    proxy: &zbus::Proxy<'_>,
    name: &str,
) -> zbus::Result<T> {
    get_property(proxy, name).await?.ok_or_else(|| zbus::Error::Failure(format!("{name} has another type")))
}

pub(super) async fn read_track_list(
    proxy: &MprisTrackListProxy<'_>,
    current_track: Option<&str>,
) -> zbus::Result<TrackListState> {
    let ids: Vec<OwnedObjectPath> = property(proxy.inner(), "Tracks").await?;
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
    let tracks = window.iter().zip(metadata).filter_map(|(id, Dict(metadata))| track_summary(id, &metadata)).collect();
    Ok(TrackListState {
        tracks,
        current_track: current_track.unwrap_or_default().to_string(),
        can_edit: property(proxy.inner(), "CanEditTracks").await?,
    })
}

/// `None` for an id past the cap; see [`fits_text_cap`]. Title and artist take the list's tighter
/// cap, since a hundred tracks multiply them.
fn track_summary(id: &OwnedObjectPath, metadata: &Metadata) -> Option<TrackSummary> {
    if !fits_text_cap(id.as_str()) {
        return None;
    }
    let parsed = parse_metadata(metadata);
    Some(TrackSummary {
        id: id.to_string(),
        title: truncate_utf8_bytes(&parsed.title, MAX_METADATA_TEXT_BYTES),
        artist: truncate_utf8_bytes(&parsed.artist, MAX_METADATA_TEXT_BYTES),
        length: parsed.length_us.filter(|length| *length >= 0),
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
    let orderings: Vec<String> = property::<Vec<String>>(proxy.inner(), "Orderings")
        .await?
        .into_iter()
        .filter(|ordering| fits_text_cap(ordering))
        .take(MAX_ORDERINGS)
        .collect();
    let selected_order =
        if order.is_empty() || (fallback_order && !orderings.iter().any(|supported| supported == order)) {
            orderings.first().cloned().unwrap_or_else(|| "Alphabetical".to_string())
        } else {
            order.to_string()
        };
    let count = property(proxy.inner(), "PlaylistCount").await?;
    if !orderings.iter().any(|supported| supported == &selected_order) {
        return Err(zbus::Error::Failure(format!("unsupported playlist ordering {selected_order:?}")));
    }
    let (active_valid, active): (bool, _) = property(proxy.inner(), "ActivePlaylist").await?;
    let active = active_valid.then(|| playlist_summary(active)).flatten();
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
        playlists: items.into_iter().filter_map(playlist_summary).collect(),
    })
}

/// `None` for an id past the cap; an icon URI past it reads as none, since a cut one names nothing.
fn playlist_summary((id, name, icon): (OwnedObjectPath, String, String)) -> Option<PlaylistSummary> {
    let icon = if fits_text_cap(&icon) { icon } else { String::new() };
    fits_text_cap(id.as_str()).then(|| PlaylistSummary {
        id: id.to_string(),
        name: truncate_utf8_bytes(&name, MAX_METADATA_TEXT_BYTES),
        icon,
    })
}

/// Rereads the collections on each change [`Changes`] takes for them and on each track change
/// the player forwarder sends on `refresh`.
pub(super) async fn collections_forwarder(
    bus_name: String,
    track_list: Option<MprisTrackListProxy<'static>>,
    playlists: Option<MprisPlaylistsProxy<'static>>,
    registry: PlayerRegistry,
    events: UnboundedSender<()>,
    mut refresh: UnboundedReceiver<()>,
    changes: std::sync::Arc<Changes>,
) {
    loop {
        // Never all disabled: `notified` has no pattern to fail.
        tokio::select! {
            Some(()) = refresh.recv() => {}
            () = changes.collections.notified() => {}
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
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::capabilities::mpris::proxies::{MprisPlaylistsProxy, MprisTrackListProxy};
    use crate::capabilities::test_support::p2p_pair_serving;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use zbus::zvariant::{ObjectPath, OwnedValue, Value};

    pub(in crate::capabilities::mpris) struct TrackPeer {
        edits: Arc<Mutex<Vec<String>>>,
        track_count: Arc<AtomicUsize>,
        short_metadata: Arc<AtomicBool>,
    }

    impl TrackPeer {
        /// `tracks` tracks, each edit logged to `edits`: the controller's tests share one log.
        pub(in crate::capabilities::mpris) fn new(edits: Arc<Mutex<Vec<String>>>, tracks: usize) -> Self {
            let track_count = Arc::new(AtomicUsize::new(tracks));
            Self { edits, track_count, short_metadata: Arc::default() }
        }
    }

    pub(in crate::capabilities::mpris) struct PlaylistPeer {
        pub(in crate::capabilities::mpris) edits: Arc<Mutex<Vec<String>>>,
        /// Sent as a `PlaylistCount` change by the next `GetPlaylists`, before it answers.
        pub(in crate::capabilities::mpris) count_on_get: Mutex<Option<u32>>,
        /// `PlaylistCount`, 1000 until [`playlist_count_changed`] moves it.
        pub(in crate::capabilities::mpris) count: Arc<AtomicU32>,
    }

    impl PlaylistPeer {
        pub(in crate::capabilities::mpris) fn new(edits: Arc<Mutex<Vec<String>>>, count_on_get: Option<u32>) -> Self {
            Self { edits, count_on_get: Mutex::new(count_on_get), count: Arc::new(AtomicU32::new(1000)) }
        }
    }

    /// Sets `PlaylistCount` to `count`, then announces it.
    pub(in crate::capabilities::mpris) async fn playlist_count_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        playlist_count: &AtomicU32,
        count: u32,
    ) {
        playlist_count.store(count, Ordering::Relaxed);
        let interface = zbus::names::InterfaceName::from_static_str_unchecked("org.mpris.MediaPlayer2.Playlists");
        let changed = HashMap::from([("PlaylistCount", Value::from(count))]);
        zbus::fdo::Properties::properties_changed(emitter, interface, changed, (&[][..]).into()).await.unwrap();
    }

    fn path(value: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(value).unwrap()
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.TrackList")]
    impl TrackPeer {
        async fn get_tracks_metadata(&self, ids: Vec<OwnedObjectPath>) -> Vec<HashMap<String, OwnedValue>> {
            self.edits.lock().unwrap().push(format!("metadata:{}", ids.len()));
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
            #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        ) -> Vec<(OwnedObjectPath, String, String)> {
            self.edits.lock().unwrap().push(format!("get:{index}:{max_count}:{order}:{reverse_order}"));
            let count = self.count_on_get.lock().unwrap().take();
            if let Some(count) = count {
                playlist_count_changed(&emitter, &self.count, count).await;
            }
            (0..max_count.min(2))
                .map(|index| (path(&format!("/playlist/{index}")), format!("List {index}"), String::new()))
                .collect()
        }

        #[zbus(property)]
        async fn playlist_count(&self) -> u32 {
            self.count.load(Ordering::Relaxed)
        }

        /// More than [`MAX_ORDERINGS`], one of them past the text cap.
        #[zbus(property)]
        async fn orderings(&self) -> Vec<String> {
            let custom = (0..MAX_ORDERINGS).map(|index| format!("Custom{index}"));
            ["Alphabetical".to_string(), "o".repeat(MAX_METADATA_TEXT_BYTES + 1)].into_iter().chain(custom).collect()
        }

        #[zbus(property)]
        async fn active_playlist(&self) -> (bool, (OwnedObjectPath, String, String)) {
            (true, (path("/playlist/0"), "List 0".into(), String::new()))
        }
    }

    /// The id goes back to the player on an action, so one past the cap drops the entry rather than
    /// naming nothing; text is cut, and an icon URI past the cap reads as none.
    #[test]
    fn an_entry_with_an_overlong_id_is_dropped_and_its_text_capped() {
        let long = "x".repeat(MAX_METADATA_TEXT_BYTES + 1);
        let long_path = path(&format!("/{long}"));
        // Past the current track's cap too, so the track list's tighter one shows.
        let longer = "x".repeat(super::super::MAX_TRACK_TEXT_BYTES + 1);
        let title = crate::capabilities::test_support::properties(&[
            ("xesam:title", Value::from(longer.clone())),
            ("xesam:artist", Value::from(longer)),
            ("mpris:length", Value::from(0_i64)),
        ]);

        assert_eq!(track_summary(&long_path, &Metadata::default()), None);
        let track = track_summary(&path("/track/0"), &super::super::metadata::decoded(&title)).unwrap();
        assert_eq!((track.title.len(), track.artist.len()), (MAX_METADATA_TEXT_BYTES, MAX_METADATA_TEXT_BYTES));
        assert_eq!(track.length, Some(0), "zero is a length; only a negative one is unknown");

        assert_eq!(playlist_summary((long_path, "List".into(), String::new())), None);
        let playlist = playlist_summary((path("/playlist/0"), long.clone(), long)).unwrap();
        assert_eq!(playlist.name.len(), MAX_METADATA_TEXT_BYTES);
        assert_eq!(playlist.icon, "");
    }

    #[tokio::test]
    async fn collection_proxies_read_bounded_state_and_send_track_and_playlist_actions() {
        let edits = Arc::new(Mutex::new(Vec::new()));
        let track_count = Arc::new(AtomicUsize::new(300));
        let short_metadata = Arc::new(AtomicBool::new(false));
        let track_edits = edits.clone();
        let playlist_edits = edits.clone();
        let count_for_peer = track_count.clone();
        let short_for_peer = short_metadata.clone();
        let (client, server) = p2p_pair_serving(|builder| {
            builder
                .serve_at(
                    "/tracklist",
                    TrackPeer { edits: track_edits, track_count: count_for_peer, short_metadata: short_for_peer },
                )?
                .serve_at("/playlists", PlaylistPeer::new(playlist_edits, None))
        })
        .await;
        let tracks = MprisTrackListProxy::builder(&client).path("/tracklist").unwrap().build().await.unwrap();
        let playlists = MprisPlaylistsProxy::builder(&client).path("/playlists").unwrap().build().await.unwrap();

        let track_state = read_track_list(&tracks, Some("/track/250")).await.unwrap();
        assert_eq!(track_state.current_track, "/track/250");
        assert_eq!(track_state.tracks.len(), TRACK_LIMIT);
        assert_eq!(track_state.tracks.first().unwrap().id, "/track/200");
        assert!(track_state.can_edit);
        short_metadata.store(true, Ordering::Relaxed);
        assert!(read_track_list(&tracks, Some("/track/250")).await.is_err());
        short_metadata.store(false, Ordering::Relaxed);
        track_count.store(MAX_TRACK_IDS + 1, Ordering::Relaxed);
        assert!(read_track_list(&tracks, Some("/track/250")).await.is_err());
        track_count.store(300, Ordering::Relaxed);
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
        assert_eq!(list_state.orderings.len(), MAX_ORDERINGS);
        assert!(list_state.orderings.iter().all(|ordering| ordering.len() <= MAX_METADATA_TEXT_BYTES));
        assert_eq!(list_state.playlists.len(), 2);
        assert_eq!(list_state.active.as_ref().map(|active| active.id.as_str()), Some("/playlist/0"));
        playlists.activate_playlist(ObjectPath::try_from("/playlist/0").unwrap()).await.unwrap();
        assert_eq!(
            *edits.lock().unwrap(),
            [
                // The full window, then the read refused for its short answer.
                "metadata:100",
                "metadata:100",
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

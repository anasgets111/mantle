//! Hand-written proxies for `org.mpris.MediaPlayer2`/`Player`; no maintained zbus MPRIS crate.
//!
//! Every player uses fixed `/org/mpris/MediaPlayer2` (the freedesktop spec disallows otherwise),
//! while `destination` varies. Declare `default_path` only; binders take the bus name explicitly.

use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

#[zbus::proxy(interface = "org.mpris.MediaPlayer2", default_path = "/org/mpris/MediaPlayer2")]
pub(super) trait MprisRoot {
    #[zbus(name = "Raise")]
    fn raise(&self) -> zbus::Result<()>;
    #[zbus(name = "Quit")]
    fn quit(&self) -> zbus::Result<()>;
    #[zbus(property, name = "Identity")]
    fn identity(&self) -> zbus::Result<String>;
    /// Optional in the spec; several players publish no `.desktop` name, so absence is an answer
    /// rather than a failure (ADR-0137).
    #[zbus(property, name = "DesktopEntry")]
    fn desktop_entry(&self) -> zbus::Result<String>;
    #[zbus(property, name = "CanRaise")]
    fn can_raise(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanQuit")]
    fn can_quit(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(interface = "org.mpris.MediaPlayer2.Player", default_path = "/org/mpris/MediaPlayer2")]
pub(super) trait MprisPlayer {
    #[zbus(name = "Play")]
    fn play(&self) -> zbus::Result<()>;
    #[zbus(name = "Pause")]
    fn pause(&self) -> zbus::Result<()>;
    #[zbus(name = "PlayPause")]
    fn play_pause(&self) -> zbus::Result<()>;
    #[zbus(name = "Next")]
    fn next(&self) -> zbus::Result<()>;
    #[zbus(name = "Previous")]
    fn previous(&self) -> zbus::Result<()>;
    #[zbus(name = "Stop")]
    fn stop(&self) -> zbus::Result<()>;
    #[zbus(name = "OpenUri")]
    fn open_uri(&self, uri: &str) -> zbus::Result<()>;
    /// Relative seek in microseconds; also the `mpris:trackid` fallback when uncached (ADR-0036).
    #[zbus(name = "Seek")]
    fn seek(&self, offset_us: i64) -> zbus::Result<()>;
    /// Absolute seek. Per freedesktop, `track_id` must be the currently playing track's
    /// `mpris:trackid` or the call is a no-op. Real players do not enforce this reliably, so the
    /// cache stays fresh (ADR-0036).
    #[zbus(name = "SetPosition")]
    fn set_position(&self, track_id: ObjectPath<'_>, position_us: i64) -> zbus::Result<()>;

    #[zbus(property, name = "PlaybackStatus")]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Metadata")]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    /// Freedesktop excludes `Position` from `PropertiesChanged` because it changes too often.
    /// zbus otherwise caches this getter until that absent signal; live `busctl` showed a player
    /// advancing while the getter stayed at stale `0`. Disable caching with
    /// `emits_changed_signal = "false"`.
    #[zbus(property(emits_changed_signal = "false"), name = "Position")]
    fn position(&self) -> zbus::Result<i64>;
    #[zbus(property, name = "CanControl")]
    fn can_control(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanGoNext")]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanGoPrevious")]
    fn can_go_previous(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanSeek")]
    fn can_seek(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanPlay")]
    fn can_play(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "CanPause")]
    fn can_pause(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "MinimumRate")]
    fn minimum_rate(&self) -> zbus::Result<f64>;
    #[zbus(property, name = "MaximumRate")]
    fn maximum_rate(&self) -> zbus::Result<f64>;
    #[zbus(property, name = "Volume")]
    fn volume(&self) -> zbus::Result<f64>;
    #[zbus(property, name = "Volume")]
    fn set_volume(&self, value: f64) -> zbus::Result<()>;
    #[zbus(property, name = "LoopStatus")]
    fn loop_status(&self) -> zbus::Result<String>;
    #[zbus(property, name = "LoopStatus")]
    fn set_loop_status(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property, name = "Shuffle")]
    fn shuffle(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "Shuffle")]
    fn set_shuffle(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property, name = "Rate")]
    fn rate(&self) -> zbus::Result<f64>;
    #[zbus(property, name = "Rate")]
    fn set_rate(&self, value: f64) -> zbus::Result<()>;

    /// Freedesktop excludes `Position` from `PropertiesChanged`; this signal marks discontinuous
    /// jumps. `player.rs` waits on it for position-only changes beside `PlaybackStatus`/`Metadata`.
    #[zbus(signal, name = "Seeked")]
    fn seeked(&self, position_us: i64) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.TrackList",
    default_service = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub(super) trait MprisTrackList {
    #[zbus(name = "GetTracksMetadata")]
    fn get_tracks_metadata(&self, track_ids: Vec<OwnedObjectPath>) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    #[zbus(name = "AddTrack")]
    fn add_track(&self, uri: &str, after_track: ObjectPath<'_>, set_as_current: bool) -> zbus::Result<()>;
    #[zbus(name = "RemoveTrack")]
    fn remove_track(&self, track_id: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(name = "GoTo")]
    fn go_to(&self, track_id: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(property, name = "Tracks")]
    fn tracks(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property, name = "CanEditTracks")]
    fn can_edit_tracks(&self) -> zbus::Result<bool>;
    #[zbus(signal, name = "TrackListReplaced")]
    fn track_list_replaced(&self, tracks: Vec<OwnedObjectPath>, current_track: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal, name = "TrackAdded")]
    fn track_added(&self, metadata: HashMap<String, OwnedValue>, after_track: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal, name = "TrackRemoved")]
    fn track_removed(&self, track_id: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal, name = "TrackMetadataChanged")]
    fn track_metadata_changed(
        &self,
        track_id: OwnedObjectPath,
        metadata: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Playlists",
    default_service = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub(super) trait MprisPlaylists {
    #[zbus(name = "ActivatePlaylist")]
    fn activate_playlist(&self, playlist_id: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(name = "GetPlaylists")]
    fn get_playlists(
        &self,
        index: u32,
        max_count: u32,
        order: &str,
        reverse_order: bool,
    ) -> zbus::Result<Vec<(OwnedObjectPath, String, String)>>;
    #[zbus(property, name = "PlaylistCount")]
    fn playlist_count(&self) -> zbus::Result<u32>;
    #[zbus(property, name = "Orderings")]
    fn orderings(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property, name = "ActivePlaylist")]
    fn active_playlist(&self) -> zbus::Result<(bool, (OwnedObjectPath, String, String))>;
    #[zbus(signal, name = "PlaylistChanged")]
    fn playlist_changed(&self, playlist: (OwnedObjectPath, String, String)) -> zbus::Result<()>;
}

pub(super) async fn bind_root(connection: &zbus::Connection, bus_name: &str) -> zbus::Result<MprisRootProxy<'static>> {
    MprisRootProxy::builder(connection).destination(bus_name.to_string())?.build().await
}

pub(super) async fn bind_player(
    connection: &zbus::Connection,
    bus_name: &str,
) -> zbus::Result<MprisPlayerProxy<'static>> {
    MprisPlayerProxy::builder(connection).destination(bus_name.to_string())?.build().await
}

pub(super) async fn bind_track_list(
    connection: &zbus::Connection,
    bus_name: &str,
) -> zbus::Result<MprisTrackListProxy<'static>> {
    MprisTrackListProxy::builder(connection).destination(bus_name.to_string())?.build().await
}

pub(super) async fn bind_playlists(
    connection: &zbus::Connection,
    bus_name: &str,
) -> zbus::Result<MprisPlaylistsProxy<'static>> {
    MprisPlaylistsProxy::builder(connection).destination(bus_name.to_string())?.build().await
}

#[cfg(test)]
mod tests {
    use super::{bind_player, bind_root};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::capabilities::test_support::p2p_pair_serving;

    struct PlayerCalls(UnboundedSender<String>);

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl PlayerCalls {
        async fn stop(&self) {
            let _ = self.0.send("Stop".into());
        }
        async fn open_uri(&self, uri: &str) {
            let _ = self.0.send(format!("OpenUri:{uri}"));
        }
    }

    #[tokio::test]
    async fn stop_and_open_uri_use_the_mpris_player_methods() {
        const PATH: &str = "/org/mpris/MediaPlayer2";
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let (client, _server) = p2p_pair_serving(|peer| peer.serve_at(PATH, PlayerCalls(calls_tx))).await;
        let proxy = bind_player(&client, "org.mpris.MediaPlayer2.probe").await.unwrap();

        proxy.stop().await.unwrap();
        proxy.open_uri("file:///tmp/track.ogg").await.unwrap();

        assert_eq!(calls_rx.recv().await.as_deref(), Some("Stop"));
        assert_eq!(calls_rx.recv().await.as_deref(), Some("OpenUri:file:///tmp/track.ogg"));
    }

    struct RootCalls(UnboundedSender<String>);

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl RootCalls {
        async fn raise(&self) {
            let _ = self.0.send("Raise".into());
        }
        async fn quit(&self) {
            let _ = self.0.send("Quit".into());
        }
        #[zbus(property)]
        fn can_raise(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_quit(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn root_flags_and_methods_use_the_mpris_root_interface() {
        const PATH: &str = "/org/mpris/MediaPlayer2";
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let (client, _server) = p2p_pair_serving(|peer| peer.serve_at(PATH, RootCalls(calls_tx))).await;
        let proxy = bind_root(&client, "org.mpris.MediaPlayer2.probe").await.unwrap();

        assert!(proxy.can_raise().await.unwrap());
        assert!(proxy.can_quit().await.unwrap());
        proxy.raise().await.unwrap();
        proxy.quit().await.unwrap();

        assert_eq!(calls_rx.recv().await.as_deref(), Some("Raise"));
        assert_eq!(calls_rx.recv().await.as_deref(), Some("Quit"));
    }
}

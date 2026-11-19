//! Hand-written proxies for `org.mpris.MediaPlayer2`/`Player`; no maintained zbus MPRIS crate.
//!
//! Every player uses fixed `/org/mpris/MediaPlayer2` (the freedesktop spec disallows otherwise),
//! while `destination` varies. Declare `default_path` only; binders take the bus name explicitly.

use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::metadata::Metadata;
use crate::capabilities::Dict;

// Properties go through `capabilities::get_all` and `get_property`, and changes through
// `changes.rs`: zbus's property cache, its generated getters and `PropertiesChanged::args` decode
// values to `OwnedValue`, one `Value` per byte of an array, and any player picks the types.

pub(super) const ROOT: &str = "org.mpris.MediaPlayer2";
pub(super) const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
pub(super) const TRACK_LIST: &str = "org.mpris.MediaPlayer2.TrackList";
pub(super) const PLAYLISTS: &str = "org.mpris.MediaPlayer2.Playlists";
pub(super) const PATH: &str = "/org/mpris/MediaPlayer2";

#[zbus::proxy(interface = "org.mpris.MediaPlayer2", default_path = "/org/mpris/MediaPlayer2")]
pub(super) trait MprisRoot {
    fn raise(&self) -> zbus::Result<()>;
    fn quit(&self) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.mpris.MediaPlayer2.Player", default_path = "/org/mpris/MediaPlayer2")]
pub(super) trait MprisPlayer {
    fn play(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn play_pause(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn open_uri(&self, uri: &str) -> zbus::Result<()>;
    /// Relative seek in microseconds; also the `mpris:trackid` fallback when uncached (ADR-0036).
    fn seek(&self, offset_us: i64) -> zbus::Result<()>;
    /// Absolute seek. Per freedesktop, `track_id` must be the currently playing track's
    /// `mpris:trackid` or the call is a no-op. Real players do not enforce this reliably, so the
    /// cache stays fresh (ADR-0036).
    fn set_position(&self, track_id: ObjectPath<'_>, position_us: i64) -> zbus::Result<()>;

    #[zbus(property, name = "Volume")]
    fn set_volume(&self, value: f64) -> zbus::Result<()>;
    #[zbus(property, name = "LoopStatus")]
    fn set_loop_status(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property, name = "Shuffle")]
    fn set_shuffle(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property, name = "Rate")]
    fn set_rate(&self, value: f64) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.TrackList",
    default_service = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub(super) trait MprisTrackList {
    fn get_tracks_metadata(&self, track_ids: Vec<OwnedObjectPath>) -> zbus::Result<Vec<Dict<Metadata>>>;
    fn add_track(&self, uri: &str, after_track: ObjectPath<'_>, set_as_current: bool) -> zbus::Result<()>;
    fn remove_track(&self, track_id: ObjectPath<'_>) -> zbus::Result<()>;
    fn go_to(&self, track_id: ObjectPath<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Playlists",
    default_service = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub(super) trait MprisPlaylists {
    fn activate_playlist(&self, playlist_id: ObjectPath<'_>) -> zbus::Result<()>;
    fn get_playlists(
        &self,
        index: u32,
        max_count: u32,
        order: &str,
        reverse_order: bool,
    ) -> zbus::Result<Vec<(OwnedObjectPath, String, String)>>;
}

pub(super) async fn bind_root(connection: &zbus::Connection, bus_name: &str) -> zbus::Result<MprisRootProxy<'static>> {
    MprisRootProxy::builder(connection)
        .destination(bus_name.to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub(super) async fn bind_player(
    connection: &zbus::Connection,
    bus_name: &str,
) -> zbus::Result<MprisPlayerProxy<'static>> {
    MprisPlayerProxy::builder(connection)
        .destination(bus_name.to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub(super) async fn bind_track_list(
    connection: &zbus::Connection,
    bus_name: &str,
) -> zbus::Result<MprisTrackListProxy<'static>> {
    MprisTrackListProxy::builder(connection)
        .destination(bus_name.to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub(super) async fn bind_playlists(
    connection: &zbus::Connection,
    bus_name: &str,
) -> zbus::Result<MprisPlaylistsProxy<'static>> {
    MprisPlaylistsProxy::builder(connection)
        .destination(bus_name.to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

//! Every signal a player sends, taken by a task of its own into typed values the forwarders read.
//!
//! zbus queues 64 messages per match rule, then stops reading the connection until one is taken:
//! a forwarder awaiting a reply must not be what takes them, or a flood holds back the reply.
//! zbus's property cache drains in a task too, but into `OwnedValue`, one `Value` per array byte.

use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use serde::de::MapAccess;
use tokio::sync::Notify;

use super::metadata::{Metadata, TrackIdentity, parse_metadata};
use super::proxies::{PATH, PLAYER, PLAYLISTS, ROOT, TRACK_LIST};
use crate::capabilities::{Dict, DictFields, Hint};
use zbus::zvariant::OwnedObjectPath;

const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Declares [`Props`] from one list, so decoding, merging and forgetting cannot disagree on a name.
macro_rules! props {
    ($($field:ident: $type:ty = $name:literal,)*) => {
        /// The properties the forwarders read, each `None` until a signal or a read gives a value
        /// of its type. `Position` is not one: no signal carries it.
        #[derive(Clone, Default)]
        pub(super) struct Props {
            $(pub(super) $field: Option<$type>,)*
        }

        impl Props {
            /// Takes every value `newer` holds; `false` when it holds none.
            fn update(&mut self, newer: Props) -> bool {
                let mut any = false;
                $(if newer.$field.is_some() {
                    self.$field = newer.$field;
                    any = true;
                })*
                any
            }

            /// Clears `name`'s value; `false` for a property this does not hold.
            fn forget(&mut self, name: &str) -> bool {
                match name {
                    $($name => self.$field = None,)*
                    _ => return false,
                }
                true
            }
        }

        impl DictFields for Props {
            fn read<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error> {
                match key {
                    $($name => self.$field = map.next_value_seed(Hint::new())?,)*
                    _ => return Ok(false),
                }
                Ok(true)
            }
        }
    };
}

props! {
    playback_status: String = "PlaybackStatus",
    loop_status: String = "LoopStatus",
    shuffle: bool = "Shuffle",
    can_go_next: bool = "CanGoNext",
    can_go_previous: bool = "CanGoPrevious",
    can_seek: bool = "CanSeek",
    can_play: bool = "CanPlay",
    can_pause: bool = "CanPause",
    volume: f64 = "Volume",
    rate: f64 = "Rate",
    minimum_rate: f64 = "MinimumRate",
    maximum_rate: f64 = "MaximumRate",
    metadata: Dict<Metadata> = "Metadata",
    identity: String = "Identity",
    desktop_entry: String = "DesktopEntry",
    can_raise: bool = "CanRaise",
    can_quit: bool = "CanQuit",
    can_edit_tracks: bool = "CanEditTracks",
    playlist_count: u32 = "PlaylistCount",
    orderings: Vec<String> = "Orderings",
    active_playlist: (bool, (OwnedObjectPath, String, String)) = "ActivePlaylist",
}

impl Props {
    /// Fills only what `self` lacks from `read`: a signal taken while the read was in flight may
    /// be the newer, and one that is older is followed by the signal that corrects it.
    pub(super) fn fill(&mut self, read: Props) {
        let signalled = std::mem::replace(self, read);
        self.update(signalled);
    }
}

pub(super) struct Pending {
    pub(super) props: Props,
    /// `PlaybackStatus` changed since the forwarder last looked.
    pub(super) status: bool,
    /// A property was invalidated, or not read yet: the next resync reads both interfaces whole.
    pub(super) refetch: bool,
    /// The last `Seeked` offset since the forwarder last looked, and the track it was sent on.
    pub(super) seeked: Option<(i64, TrackIdentity)>,
}

pub(super) struct Changes {
    pending: Mutex<Pending>,
    /// Holds a permit from the start, so the forwarder's first wake checks what registration read.
    pub(super) player: Notify,
    pub(super) collections: Notify,
}

impl Changes {
    pub(super) fn new() -> Self {
        let player = Notify::new();
        player.notify_one();
        let pending = Pending { props: Props::default(), status: true, refetch: true, seeked: None };
        Self { pending: Mutex::new(pending), player, collections: Notify::new() }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().expect("mpris changes mutex poisoned")
    }

    /// Only `PropertiesChanged` and `Seeked` bodies are read; the collections reread on any
    /// signal of theirs, so a `TrackAdded`'s `a{sv}` goes undecoded.
    fn take(&self, signal: &zbus::Message) {
        let header = signal.header();
        let (Some(interface), Some(member)) = (header.interface(), header.member()) else { return };
        match (interface.as_str(), member.as_str()) {
            (PROPERTIES, "PropertiesChanged") => self.changed(&signal.body()),
            (PLAYER, "Seeked") => {
                // A malformed one still resyncs, as it did when each signal resynced.
                if let Ok(position) = signal.body().deserialize::<i64>() {
                    let mut pending = self.lock();
                    // ponytail: a queued `Seeked` can inherit a newer `GetAll` seed's track until the next
                    // update. Upgrade: keep the seed's `recv_position()` and drop seeks received before it.
                    // What `resync` reports for the same metadata, none included.
                    let track = pending.props.metadata.as_ref().map(|Dict(metadata)| parse_metadata(metadata));
                    pending.seeked = Some((position, track.unwrap_or_default().track_identity()));
                }
                self.player.notify_one();
            }
            (TRACK_LIST | PLAYLISTS, _) => self.collections.notify_one(),
            _ => {}
        }
    }

    fn changed(&self, body: &zbus::message::Body) {
        let Ok((interface, Dict(changed), invalidated)) = body.deserialize::<(&str, Dict<Props>, Vec<&str>)>() else {
            return;
        };
        if interface == TRACK_LIST || interface == PLAYLISTS {
            // The collections read their own values; only whether one of theirs moved matters.
            let mut named = Props::default();
            if named.update(changed) || invalidated.iter().any(|name| named.forget(name)) {
                self.collections.notify_one();
            }
            return;
        }
        if interface != PLAYER && interface != ROOT {
            return;
        }
        let mut pending = self.lock();
        pending.status |= changed.playback_status.is_some() || invalidated.contains(&"PlaybackStatus");
        let mut woken = pending.props.update(changed);
        for name in invalidated {
            if pending.props.forget(name) {
                pending.refetch = true;
                woken = true;
            }
        }
        drop(pending);
        if woken {
            self.player.notify_one();
        }
    }
}

/// The task [`drain`] spawned, ended when this is dropped.
pub(super) struct Drain(tokio::task::JoinHandle<()>);

impl Drop for Drain {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Subscribes to every signal `bus_name`'s owner sends on the MPRIS object, in one stream so
/// they stay in order, and takes each in a spawned task that awaits nothing else. Nothing is
/// awaited between subscribing and spawning: a full queue nobody takes stalls the connection.
pub(super) async fn drain(connection: &zbus::Connection, bus_name: &str) -> zbus::Result<(Arc<Changes>, Drain)> {
    // A bus matches senders by unique name only; a peer is the only sender there is.
    let owner = if connection.is_bus() {
        Some(zbus::fdo::DBusProxy::new(connection).await?.get_name_owner(bus_name.try_into()?).await?)
    } else {
        None
    };
    let mut rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).path(PATH)?;
    if let Some(owner) = &owner {
        rule = rule.sender(owner.as_ref())?;
    }
    let mut signals = zbus::MessageStream::for_match_rule(rule.build(), connection, None).await?;
    let changes = Arc::new(Changes::new());
    let taken = Arc::clone(&changes);
    let task = tokio::spawn(async move {
        while let Some(signal) = signals.next().await {
            if let Ok(signal) = signal {
                taken.take(&signal);
            }
        }
    });
    Ok((changes, Drain(task)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use std::collections::HashMap;
    use zbus::zvariant::{ObjectPath, Value};

    /// Whether `signal` woke the player and the collections, and the seek it left.
    fn taken(signal: zbus::Message) -> (bool, bool, Option<i64>) {
        let (player, collections, seeked) = taken_by(&Changes::new(), signal);
        (player, collections, seeked.map(|(position, _)| position))
    }

    fn taken_by(changes: &Changes, signal: zbus::Message) -> (bool, bool, Option<(i64, TrackIdentity)>) {
        changes.player.notified().now_or_never();
        changes.take(&signal);
        let woken = |notify: &Notify| notify.notified().now_or_never().is_some();
        (woken(&changes.player), woken(&changes.collections), changes.lock().seeked.take())
    }

    fn signal(
        interface: &str,
        member: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> zbus::Message {
        zbus::Message::signal(PATH, interface, member).unwrap().build(body).unwrap()
    }

    /// Whether `changed` and `invalidated` on `interface` woke the player and the collections.
    fn woken(interface: &str, changed: &[(&str, Value<'_>)], invalidated: &[&str]) -> (bool, bool) {
        let changed: HashMap<&str, &Value<'_>> = changed.iter().map(|(name, value)| (*name, value)).collect();
        let (player, collections, _) =
            taken(signal(PROPERTIES, "PropertiesChanged", &(interface, changed, invalidated)));
        (player, collections)
    }

    #[test]
    fn a_seek_wakes_the_player_with_its_offset_and_any_collections_signal_the_collections() {
        assert_eq!(taken(signal(PLAYER, "Seeked", &(5_i64,))), (true, false, Some(5)));
        assert_eq!(taken(signal(PLAYER, "Seeked", &("malformed",))), (true, false, None));
        let added = (HashMap::<&str, Value<'_>>::new(), ObjectPath::try_from("/t/1").unwrap());
        assert_eq!(taken(signal(TRACK_LIST, "TrackAdded", &added)), (false, true, None));
        assert_eq!(taken(signal(PLAYLISTS, "PlaylistChanged", &("/p",))), (false, true, None));
        assert_eq!(taken(signal(ROOT, "Seeked", &(5_i64,))), (false, false, None));
        assert_eq!(taken(signal(PROPERTIES, "Other", &(PLAYER,))), (false, false, None));

        // A seek carries the track it was sent on, the default before any metadata.
        let changes = Changes::new();
        let seek = || signal(PLAYER, "Seeked", &(5_i64,));
        assert_eq!(taken_by(&changes, seek()), (true, false, Some((5, TrackIdentity::default()))));
        let fields = crate::capabilities::test_support::properties(&[("xesam:title", "Song".into())]);
        let metadata = HashMap::from([("Metadata", Value::from(fields.clone()))]);
        taken_by(&changes, signal(PROPERTIES, "PropertiesChanged", &(PLAYER, metadata, Vec::<&str>::new())));
        let song = parse_metadata(&super::super::metadata::decoded(&fields)).track_identity();
        assert_eq!(taken_by(&changes, seek()), (true, false, Some((5, song))));
    }

    #[test]
    fn each_property_wakes_its_own_forwarder_and_an_unread_one_wakes_neither() {
        let active = Value::from((true, (ObjectPath::try_from("/p").unwrap(), "Mix", "")));
        let collections = [
            ("CanEditTracks", true.into()),
            ("PlaylistCount", 3_u32.into()),
            ("Orderings", vec!["Alphabetical"].into()),
        ];
        for (name, value) in collections.into_iter().chain([("ActivePlaylist", active)]) {
            assert_eq!(woken(TRACK_LIST, &[(name, value)], &[]), (false, true), "{name}");
            assert_eq!(woken(PLAYLISTS, &[], &[name]), (false, true), "{name} invalidated");
        }
        assert_eq!(woken(PLAYER, &[("Volume", 0.5.into())], &[]), (true, false));
        assert_eq!(woken(ROOT, &[], &["Identity"]), (true, false));
        assert_eq!(woken(PLAYER, &[("Position", 5_i64.into())], &["Position"]), (false, false));
        assert_eq!(woken("org.example", &[("Volume", 0.5.into())], &[]), (false, false));
        assert_eq!(woken(PLAYLISTS, &[("Volume", "mistyped".into())], &[]), (false, false));
    }

    fn volume(volume: f64) -> Props {
        Props { volume: Some(volume), ..Props::default() }
    }

    #[test]
    fn a_read_fills_only_what_no_signal_set_and_a_forgotten_value_refills() {
        let mut props = volume(0.25);
        let read = || Props { shuffle: Some(true), ..volume(0.5) };
        props.fill(read());
        assert_eq!((props.volume, props.shuffle), (Some(0.25), Some(true)));

        assert!(props.forget("Volume"));
        assert!(!props.forget("Position"));
        props.fill(read());
        assert_eq!(props.volume, Some(0.5));

        assert!(props.update(volume(0.75)));
        assert!(!props.update(Props::default()));
        assert_eq!((props.volume, props.shuffle), (Some(0.75), Some(true)));
    }
}

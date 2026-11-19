//! A player's `PropertiesChanged`, taken by a task of its own into typed values the forwarders read.
//!
//! zbus queues 64 messages per match rule, then stops reading the connection until one is taken:
//! a forwarder awaiting a reply must not be what takes them, or a flood holds back the reply.
//! zbus's property cache drains in a task too, but into `OwnedValue`, one `Value` per array byte.

use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use serde::de::MapAccess;
use tokio::sync::Notify;

use super::metadata::Metadata;
use super::proxies::{PATH, PLAYER, PLAYLISTS, ROOT, TRACK_LIST};
use crate::capabilities::{Dict, DictFields, Hint, properties_proxy};
use zbus::zvariant::OwnedObjectPath;

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
        let pending = Pending { props: Props::default(), status: true, refetch: true };
        Self { pending: Mutex::new(pending), player, collections: Notify::new() }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().expect("mpris changes mutex poisoned")
    }

    fn take(&self, body: &zbus::message::Body) {
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

/// Subscribes to `bus_name`'s `PropertiesChanged` and takes each signal in a spawned task that
/// awaits nothing else.
pub(super) async fn drain(connection: &zbus::Connection, bus_name: &str) -> zbus::Result<(Arc<Changes>, Drain)> {
    let mut signals =
        properties_proxy(connection, bus_name.to_string(), PATH).await?.receive_properties_changed().await?;
    let changes = Arc::new(Changes::new());
    let taken = Arc::clone(&changes);
    let task = tokio::spawn(async move {
        while let Some(signal) = signals.next().await {
            taken.take(&signal.message().body());
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

    /// Whether `changed` and `invalidated` on `interface` woke the player and the collections.
    fn woken(interface: &str, changed: &[(&str, Value<'_>)], invalidated: &[&str]) -> (bool, bool) {
        let changes = Changes::new();
        changes.player.notified().now_or_never();
        let changed: HashMap<&str, &Value<'_>> = changed.iter().map(|(name, value)| (*name, value)).collect();
        let signal = zbus::Message::signal(PATH, "org.freedesktop.DBus.Properties", "PropertiesChanged").unwrap();
        changes.take(&signal.build(&(interface, changed, invalidated)).unwrap().body());
        (changes.player.notified().now_or_never().is_some(), changes.collections.notified().now_or_never().is_some())
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

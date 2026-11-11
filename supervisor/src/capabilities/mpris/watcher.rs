//! Discovery: which session-bus names `mantle.mpris` tracks.
//!
//! Players never register; scan `ListNames` once, then watch `NameOwnerChanged` for arrivals and
//! departures under the same prefix (ADR-0036).

use futures_util::StreamExt;
use shared::{error, warn};
use tokio::sync::mpsc::UnboundedSender;

use super::player::{PlayerRegistry, register_player, unregister_player};

pub(super) const MPRIS_SERVICE_PREFIX: &str = "org.mpris.MediaPlayer2.";
const EXCLUDED_SUFFIX: &str = "playerctld";

/// True for a trackable MPRIS bus name: the discovery prefix, except `playerctld`, `playerctl`'s
/// aggregator. It mirrors the active player, including `Identity`/`DesktopEntry`; live
/// introspection found that exclusion signal reliable (ADR-0036). This only affects this list.
pub(super) fn is_trackable_player(bus_name: &str) -> bool {
    bus_name.strip_prefix(MPRIS_SERVICE_PREFIX).is_some_and(|suffix| !suffix.is_empty() && suffix != EXCLUDED_SUFFIX)
}

/// IDL `id`: bus-name suffix after the discovery prefix, reversible with
/// [`service_name_for_id`] (ADR-0036). Meaningful only for names accepted by
/// [`is_trackable_player`].
pub(super) fn player_id(bus_name: &str) -> &str {
    bus_name.strip_prefix(MPRIS_SERVICE_PREFIX).unwrap_or(bus_name)
}

/// Reconstructs the bus name from write argument `id`, the inverse of [`player_id`] (ADR-0036);
/// writes recompute it instead of keeping a lookup table.
pub(super) fn service_name_for_id(id: &str) -> String {
    format!("{MPRIS_SERVICE_PREFIX}{id}")
}

/// Scans `ListNames`, filters with [`is_trackable_player`], and registers each match not already
/// tracked. Returns whether any was left out at the player limit.
async fn discover_existing(
    connection: &zbus::Connection,
    dbus_proxy: &zbus::fdo::DBusProxy<'static>,
    registry: &PlayerRegistry,
    events: &UnboundedSender<()>,
) -> bool {
    let names = match dbus_proxy.list_names().await {
        Ok(names) => names,
        Err(err) => {
            warn!("ListNames failed; no players discovered this scan: {err}");
            return false;
        }
    };
    let mut left_out = false;
    for name in names {
        let name = name.to_string();
        let tracked = registry.lock().expect("mpris registry mutex poisoned").contains_key(&name);
        if is_trackable_player(&name) && !tracked {
            left_out |= register_player(connection, registry, events, name).await;
        }
    }
    left_out
}

/// `NameOwnerChanged` for player names only; `arg0namespace` keeps the bus from waking discovery
/// for every unrelated name on the session.
fn player_owner_rule() -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns(MPRIS_SERVICE_PREFIX.trim_end_matches('.'))?
        .build())
}

/// Binds `org.freedesktop.DBus`, subscribes to `NameOwnerChanged`, scans with
/// [`discover_existing`], then starts the forwarder over that live subscription. Bind or
/// subscribe failure logs and disables discovery.
///
/// Subscribe before scanning: scan-first can permanently miss a player appearing or disappearing
/// between the `ListNames` reply and subscription. A signal arriving before the scan reaches its
/// name only double-registers it, handled by `register_player`'s replace-and-abort logic.
pub(super) async fn spawn_discovery(
    connection: zbus::Connection,
    registry: PlayerRegistry,
    events: UnboundedSender<()>,
) {
    let dbus_proxy = match zbus::fdo::DBusProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(err) => {
            error!("failed to bind org.freedesktop.DBus; player discovery disabled for this run: {err}");
            return;
        }
    };
    let stream = match player_owner_rule() {
        Ok(rule) => zbus::MessageStream::for_match_rule(rule, &connection, None).await,
        Err(err) => Err(err),
    };
    let mut stream = match stream {
        Ok(stream) => stream,
        Err(err) => {
            error!("failed to subscribe to NameOwnerChanged; player discovery disabled for this run: {err}");
            return;
        }
    };

    let mut left_out = discover_existing(&connection, &dbus_proxy, &registry, &events).await;

    tokio::spawn(async move {
        while let Some(message) = stream.next().await {
            let Some(signal) = message.ok().and_then(zbus::fdo::NameOwnerChanged::from_message) else { continue };
            let Ok(args) = signal.args() else { continue };
            let name = args.name.to_string();
            if !is_trackable_player(&name) {
                continue;
            }
            shared::debug!("MPRIS player signal: {name} owner={}", args.new_owner.is_some());
            if args.new_owner.is_some() {
                left_out |= register_player(&connection, &registry, &events, name).await;
            } else {
                unregister_player(&registry, &name, &events);
                // A slot may have freed: pick up the players left out at the limit.
                if left_out {
                    left_out = discover_existing(&connection, &dbus_proxy, &registry, &events).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serving any object starts the peer's object server, so every call this test's peers do not
    /// answer is refused at once instead of timing out.
    struct Root;

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        #[zbus(property)]
        fn identity(&self) -> String {
            "Fixture".into()
        }
    }

    /// A player left out while every slot was taken, at the first scan or on arriving later, is
    /// picked up once one frees, rather than staying untracked until it or the shell restarts.
    #[tokio::test]
    async fn a_player_left_out_at_the_limit_is_tracked_once_a_slot_frees() {
        use crate::capabilities::test_support::{poll_until, private_bus};
        let bus = private_bus().await;
        let player = |index: usize| {
            let name = format!("{MPRIS_SERVICE_PREFIX}p{index}");
            let peer = bus.builder().name(name.clone()).unwrap().serve_at("/org/mpris/MediaPlayer2", Root).unwrap();
            async move { (name, peer.build().await.unwrap()) }
        };
        let mut players = Vec::new();
        for index in 0..=super::super::MAX_PLAYERS {
            players.push(player(index).await);
        }
        let registry = PlayerRegistry::default();
        let (events, _woken) = tokio::sync::mpsc::unbounded_channel();
        spawn_discovery(bus.builder().build().await.unwrap(), registry.clone(), events).await;
        let tracked = |name: &str| registry.lock().unwrap().contains_key(name);
        let untracked = |players: &[(String, zbus::Connection)]| {
            players.iter().map(|(name, _)| name.clone()).find(|name| !tracked(name))
        };

        let free_a_slot = |players: &mut Vec<(String, zbus::Connection)>| {
            let freed = players.iter().position(|(name, _)| tracked(name)).unwrap();
            drop(players.remove(freed));
        };

        let left_out = untracked(&players).expect("the scan leaves one name out");
        free_a_slot(&mut players);
        poll_until(|| tracked(&left_out).then_some(())).await;

        // Now one arrives after startup, while every slot is taken.
        players.push(player(100).await);
        let left_out = poll_until(|| untracked(&players)).await;
        free_a_slot(&mut players);
        poll_until(|| tracked(&left_out).then_some(())).await;
        assert_eq!(registry.lock().unwrap().len(), super::super::MAX_PLAYERS);
    }

    #[test]
    fn player_owner_rule_matches_player_names_only() {
        let rule = player_owner_rule().unwrap();
        let owner_changed = |name: &str| {
            zbus::Message::signal("/org/freedesktop/DBus", "org.freedesktop.DBus", "NameOwnerChanged")
                .unwrap()
                .sender("org.freedesktop.DBus")
                .unwrap()
                .build(&(name, "", ":1.7"))
                .unwrap()
        };
        assert!(rule.matches(&owner_changed("org.mpris.MediaPlayer2.firefox")).unwrap());
        assert!(!rule.matches(&owner_changed(":1.42")).unwrap());
        assert!(!rule.matches(&owner_changed("org.mpris.MediaPlayer2Extra")).unwrap());
    }

    #[test]
    fn is_trackable_player_accepts_real_players_and_rejects_playerctld_and_foreign_names() {
        for (name, expected) in [
            ("org.mpris.MediaPlayer2.firefox.instance_1_59239", true),
            ("org.mpris.MediaPlayer2.playerctld", false),
            ("org.freedesktop.DBus", false),
            ("org.mpris.MediaPlayer2", false),
        ] {
            assert_eq!(is_trackable_player(name), expected, "{name}");
        }
    }

    #[test]
    fn player_id_strips_the_discovery_prefix_and_service_name_for_id_restores_it() {
        let bus_name = "org.mpris.MediaPlayer2.mpv.instance-abc123";
        let id = player_id(bus_name);
        assert_eq!(id, "mpv.instance-abc123");
        assert_eq!(service_name_for_id(id), bus_name);
    }
}

//! Discovery: which session-bus names `mantle.mpris` tracks.
//!
//! Players never register; scan `ListNames` once, then watch `NameOwnerChanged` for arrivals and
//! departures under the same prefix (ADR-0036).

use std::collections::HashMap;

use futures_util::StreamExt;
use shared::{error, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

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

/// Scans `ListNames` for the names [`is_trackable_player`] accepts that are not already tracked.
async fn discover_existing(dbus_proxy: &zbus::fdo::DBusProxy<'static>, registry: &PlayerRegistry) -> Vec<String> {
    let names = match dbus_proxy.list_names().await {
        Ok(names) => names,
        Err(err) => {
            warn!("ListNames failed; no players discovered this scan: {err}");
            return Vec::new();
        }
    };
    let mut found = Vec::new();
    for name in names {
        let name = name.to_string();
        let tracked = registry.lock().expect("mpris registry mutex poisoned").contains_key(&name);
        if is_trackable_player(&name) && !tracked {
            found.push(name);
        }
    }
    found
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

/// Forgets registrations that have run to the end, so `pending` holds only those in flight and not
/// one entry per player name seen.
fn drop_finished(pending: &mut HashMap<String, JoinHandle<()>>) {
    pending.retain(|_, task| !task.is_finished());
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

    let mut found = discover_existing(&dbus_proxy, &registry).await;
    // Registrations in flight, each its own task: one player that never answers takes about 100 s
    // of call timeouts. Its departure cancels it.
    let mut pending = HashMap::new();
    // ponytail: at most MAX_PLAYERS registrations run at once, however many player names a client
    // owns; 8 that never answer stall the rest until they time out or leave, as 8 names held by one
    // client do. A waiting name is never refused for the ones in flight. Upgrade: a per-owner limit.
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(super::MAX_PLAYERS));
    tokio::spawn(async move {
        loop {
            drop_finished(&mut pending);
            for name in found.drain(..) {
                let (connection, registry, events) = (connection.clone(), registry.clone(), events.clone());
                let (bus_name, permits) = (name.clone(), permits.clone());
                let task = tokio::spawn(async move {
                    let _permit = permits.acquire().await;
                    register_player(&connection, &registry, &events, bus_name).await
                });
                pending.insert(name, task).inspect(tokio::task::JoinHandle::abort);
            }
            let Some(message) = stream.next().await else { break };
            let Some(signal) = message.ok().and_then(zbus::fdo::NameOwnerChanged::from_message) else { continue };
            let Ok(args) = signal.args() else { continue };
            let name = args.name.to_string();
            if !is_trackable_player(&name) {
                continue;
            }
            shared::debug!("MPRIS player signal: {name} owner={}", args.new_owner.is_some());
            if args.new_owner.is_some() {
                found.push(name);
            } else {
                // Awaited, so a cancelled registration cannot insert after the removal below.
                if let Some(task) = pending.remove(&name) {
                    task.abort();
                    let _ = task.await;
                }
                let was_full = registry.lock().expect("mpris registry mutex poisoned").len() >= super::MAX_PLAYERS;
                unregister_player(&registry, &name, &events);
                // A slot may have freed: pick up the players left out at the limit.
                if was_full {
                    found = discover_existing(&dbus_proxy, &registry).await;
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

    #[tokio::test]
    async fn a_finished_registration_leaves_the_pending_map() {
        let finished = tokio::spawn(async {});
        let running = tokio::spawn(std::future::pending());
        tokio::task::yield_now().await;
        let mut pending = HashMap::from([("finished".to_string(), finished), ("running".to_string(), running)]);

        drop_finished(&mut pending);

        assert_eq!(pending.keys().collect::<Vec<_>>(), ["running"]);
        pending["running"].abort();
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

        poll_until(|| (registry.lock().unwrap().len() == super::super::MAX_PLAYERS).then_some(())).await;
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

    /// A player whose `CanControl` answers only once `gate` closes; `entered` counts the players
    /// asked.
    struct Gated {
        gate: std::sync::Arc<tokio::sync::Semaphore>,
        entered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        asked: std::sync::atomic::AtomicBool,
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Gated {
        #[zbus(property)]
        async fn can_control(&self) -> bool {
            use std::sync::atomic::Ordering::Relaxed;
            if !self.asked.swap(true, Relaxed) {
                self.entered.fetch_add(1, Relaxed);
            }
            let _ = self.gate.acquire().await;
            true
        }
    }

    /// Shared by every [`Gated`] player one test serves.
    #[derive(Clone)]
    struct Gate {
        gate: std::sync::Arc<tokio::sync::Semaphore>,
        entered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Gate {
        fn closed() -> Self {
            Gate { gate: std::sync::Arc::new(tokio::sync::Semaphore::new(0)), entered: Default::default() }
        }

        fn player(&self) -> Gated {
            Gated { gate: self.gate.clone(), entered: self.entered.clone(), asked: Default::default() }
        }

        fn entered(&self) -> usize {
            self.entered.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// Discovery with a call timeout far past [`poll_until`]'s bound, so a registration waiting on a
    /// [`Gated`] player cannot end by timing out within a test.
    async fn discovery(
        bus: &crate::capabilities::test_support::PrivateBus,
    ) -> (PlayerRegistry, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let registry = PlayerRegistry::default();
        let (events, woken) = tokio::sync::mpsc::unbounded_channel();
        let connection = bus.builder().method_timeout(std::time::Duration::from_secs(60)).build().await.unwrap();
        spawn_discovery(connection, registry.clone(), events).await;
        (registry, woken)
    }

    async fn serve(
        bus: &crate::capabilities::test_support::PrivateBus,
        name: &str,
        gated: Option<Gated>,
    ) -> zbus::Connection {
        let builder = bus.builder().name(format!("{MPRIS_SERVICE_PREFIX}{name}")).unwrap();
        let builder = builder.serve_at("/org/mpris/MediaPlayer2", Root).unwrap();
        let builder = match gated {
            Some(gated) => builder.serve_at("/org/mpris/MediaPlayer2", gated).unwrap(),
            None => builder,
        };
        builder.build().await.unwrap()
    }

    /// A player that never answers holds up only its own registration, and leaving cancels it.
    #[tokio::test]
    async fn a_hung_player_holds_up_only_itself() {
        use crate::capabilities::test_support::{poll_until, private_bus};
        let bus = private_bus().await;
        let (registry, _woken) = discovery(&bus).await;
        let tracked = |name: &str| registry.lock().unwrap().contains_key(&format!("{MPRIS_SERVICE_PREFIX}{name}"));
        let gate = Gate::closed();

        let hung = serve(&bus, "hung", Some(gate.player())).await;
        poll_until(|| (gate.entered() == 1).then_some(())).await;
        let _next = serve(&bus, "next", None).await;
        poll_until(|| tracked("next").then_some(())).await;

        hung.release_name(format!("{MPRIS_SERVICE_PREFIX}hung")).await.unwrap();
        let _last = serve(&bus, "last", None).await;
        poll_until(|| tracked("last").then_some(())).await;
        // The hung player now answers: a registration still running would finish within
        // milliseconds, its remaining calls refused at once since the name has no owner.
        gate.gate.close();
        let ghost =
            tokio::time::timeout(std::time::Duration::from_millis(500), poll_until(|| tracked("hung").then_some(())));
        assert!(ghost.await.is_err(), "a departed player's registration ran on");
    }

    /// Before any player is tracked, registrations in flight are what the limit bounds: a client
    /// owning many player names gets at most `MAX_PLAYERS` registered at once.
    #[tokio::test]
    async fn at_most_max_players_registrations_run_at_once() {
        use crate::capabilities::test_support::{poll_until, private_bus};
        let bus = private_bus().await;
        let gate = Gate::closed();
        let mut players = Vec::new();
        for index in 0..3 * super::super::MAX_PLAYERS {
            players.push(serve(&bus, &format!("p{index}"), Some(gate.player())).await);
        }
        let (registry, _woken) = discovery(&bus).await;

        poll_until(|| (gate.entered() >= super::super::MAX_PLAYERS).then_some(())).await;
        let more = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            poll_until(|| (gate.entered() > super::super::MAX_PLAYERS).then_some(())),
        );
        assert!(more.await.is_err(), "{} registrations ran at once", gate.entered());

        gate.gate.close();
        poll_until(|| (registry.lock().unwrap().len() == super::super::MAX_PLAYERS).then_some(())).await;
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

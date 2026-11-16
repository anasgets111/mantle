//! [`TrayController`]: the `mantle.tray` write-action dispatcher and state owner.

use std::time::SystemTime;

use enumflags2::BitFlags;
use futures_util::StreamExt;
use shared::action::ScrollOrientation;
use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;
use zbus::fdo::RequestNameFlags;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::capabilities::epoch_seconds;
use crate::capabilities::name_departures;

use super::icon::icon_filename_stem;
use super::menu::fetch_menu_via;
use super::proxies::StatusNotifierWatcherClientProxy;
use super::registration::{ResolvedRegistration, item_id, resolve_registration};
use super::registry::{
    ItemEntry, ItemKey, ItemRegistry, RegisterOutcome, enforce_menu_budget, ordered_items, register_item,
    remaining_menu_budget, spawn_name_owner_changed_forwarder, store_menu,
};
use super::watcher::StatusNotifierWatcher;
use super::{DEFAULT_ITEM_OBJECT_PATH, TrayState, WATCHER_BUS_NAME, WATCHER_OBJECT_PATH};

#[derive(Clone)]
pub struct TrayController {
    registry: ItemRegistry,
    events: UnboundedSender<()>,
}

impl TrayController {
    /// Requests `org.kde.StatusNotifierWatcher` without `ReplaceExisting`/`DoNotQueue` flags
    /// (ADR-0031). With `DoNotQueue` unset, zbus queues and returns `Ok(InQueue)`, so only a hard
    /// `Err` is failure, and it does not stop construction.
    ///
    /// Exports [`WATCHER_OBJECT_PATH`] before claiming the name, so a call routed to the new owner
    /// finds the object. Awaits no app (ADR-0031 amendment). Subscribes to departures before the
    /// export, so an item that registers and exits at once is not listed forever.
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<()>) -> Self {
        let dbus_proxy = zbus::fdo::DBusProxy::new(&connection)
            .await
            .inspect_err(|err| error!("failed to bind org.freedesktop.DBus for NameOwnerChanged tracking: {err}"))
            .ok();
        // `acquired` before RequestName: winning the name, now or once another watcher exits, announces the host.
        let (departures, acquired) = match &dbus_proxy {
            Some(dbus_proxy) => (
                name_departures(dbus_proxy, "vanished tray items stay listed").await,
                dbus_proxy.receive_name_acquired().await.ok(),
            ),
            None => (None, None),
        };
        let registry = ItemRegistry::default();
        let watcher = StatusNotifierWatcher {
            connection: connection.clone(),
            registry: registry.clone(),
            events: events.clone(),
        };
        // Continue after export failure; tray setup must not abort the Supervisor.
        if let Err(err) = connection.object_server().at(WATCHER_OBJECT_PATH, watcher).await {
            warn!("failed to export StatusNotifierWatcher at {WATCHER_OBJECT_PATH}: {err}");
        }
        match connection.request_name_with_flags(WATCHER_BUS_NAME, BitFlags::<RequestNameFlags>::empty()).await {
            Ok(reply) => debug!("RequestName({WATCHER_BUS_NAME}) -> {reply}"),
            Err(err) => error!("RequestName({WATCHER_BUS_NAME}) failed: {err}"),
        }
        if let Some(mut acquired) = acquired {
            let connection = connection.clone();
            tokio::spawn(async move {
                while let Some(signal) = acquired.next().await {
                    if signal.args().is_ok_and(|args| args.name.as_str() == WATCHER_BUS_NAME)
                        && let Ok(emitter) = SignalEmitter::new(&connection, WATCHER_OBJECT_PATH)
                    {
                        let _ = StatusNotifierWatcher::status_notifier_host_registered(&emitter).await;
                    }
                }
            });
        }

        let task = (connection, registry.clone(), events.clone());
        tokio::spawn(async move {
            let (connection, registry, events) = task;
            match StatusNotifierWatcherClientProxy::new(&connection).await {
                Ok(watcher_client) => {
                    let our_unique_name = connection.unique_name().map(|name| name.to_string()).unwrap_or_default();
                    if let Err(err) = watcher_client.register_status_notifier_host(&our_unique_name).await {
                        debug!("RegisterStatusNotifierHost failed: {err}");
                    }
                }
                Err(err) => {
                    debug!(
                        "failed to bind the StatusNotifierWatcher client proxy for RegisterStatusNotifierHost: {err}"
                    )
                }
            }
            let Some(dbus_proxy) = dbus_proxy else { return };
            // Subscribed, then scan, then forward: a departure during the scan is applied after the insert.
            adopt_existing_items(&connection, &dbus_proxy, &registry, &events).await;
            if let Some(departures) = departures {
                spawn_name_owner_changed_forwarder(connection.clone(), departures, registry, events);
            }
        });

        Self { registry, events }
    }

    /// Empty registry, no forwarders, and nothing exported. Used when the tray session-bus
    /// connection cannot be established; actions behave like a live controller with no items.
    pub fn inert(events: UnboundedSender<()>) -> Self {
        Self { registry: ItemRegistry::default(), events }
    }

    /// Re-derives `tray.items` from the registry. Synchronous because forwarders update
    /// `last_known` before waking `main.rs`.
    pub fn snapshot(&self) -> TrayState {
        TrayState { items: ordered_items(&self.registry) }
    }

    #[cfg(test)]
    pub(super) fn waiting_len(&self) -> usize {
        self.registry.lock().expect("mutex poisoned").waiting.len()
    }

    /// What `pick` clones out of the entry `id` names, under one lock that no D-Bus call outlives.
    /// An unknown id is logged under `action`.
    fn find<T>(&self, action: &str, id: &str, pick: impl FnOnce(&ItemKey, &ItemEntry) -> T) -> Option<T> {
        let guard = self.registry.lock().expect("mutex poisoned");
        let found = guard.items.iter().find(|(key, _)| item_id(key.0.as_str(), key.1.as_str()) == id);
        if found.is_none() {
            debug!("{action}({id:?}) failed: no tray item with that id has been registered");
        }
        found.map(|(key, entry)| pick(key, entry))
    }

    /// `tray:activate(id, x, y)`. Skips `Activate` when `ItemIsMenu` is true, per SNI semantics,
    /// here rather than in every config (ADR-0031).
    pub async fn activate(&self, id: &str, x: i32, y: i32) {
        let Some((item_is_menu, item)) =
            self.find("activate", id, |_, entry| (entry.last_known.item_is_menu, entry.item.clone()))
        else {
            return;
        };
        if item_is_menu {
            return;
        }
        if let Err(err) = item.activate(x, y).await {
            debug!("activate({id:?}) failed: {err}");
        }
    }

    /// `tray:context_menu(id, x, y)`: right-click context menu for items that export no DBusMenu.
    pub async fn context_menu(&self, id: &str, x: i32, y: i32) {
        let Some(item) = self.find("context_menu", id, |_, entry| entry.item.clone()) else { return };
        if let Err(err) = item.context_menu(x, y).await {
            debug!("context_menu({id:?}) failed: {err}");
        }
    }

    /// `tray:secondary_activate(id, x, y)`: middle-click (ADR-0074). No `ItemIsMenu` gate: it
    /// constrains primary clicks only.
    pub async fn secondary_activate(&self, id: &str, x: i32, y: i32) {
        let Some(item) = self.find("secondary_activate", id, |_, entry| entry.item.clone()) else { return };
        if let Err(err) = item.secondary_activate(x, y).await {
            debug!("secondary_activate({id:?}) failed: {err}");
        }
    }

    /// `tray:scroll(id, delta, orientation)`: icon scroll (ADR-0074).
    pub async fn scroll(&self, id: &str, delta: i32, orientation: ScrollOrientation) {
        let Some(item) = self.find("scroll", id, |_, entry| entry.item.clone()) else { return };
        let orientation = match orientation {
            ScrollOrientation::Vertical => "vertical",
            ScrollOrientation::Horizontal => "horizontal",
        };
        if let Err(err) = item.scroll(delta, orientation).await {
            debug!("scroll({id:?}) failed: {err}");
        }
    }

    /// `tray:activate_menu_item(id, menu_item_id)` sends `DBusMenu.Event(id, "clicked", 0,
    /// timestamp)` (ADR-0031).
    pub async fn activate_menu_item(&self, id: &str, menu_item_id: i32) {
        let Some(menu) = self.find("activate_menu_item", id, |_, entry| entry.menu.clone()) else { return };
        let Some(menu) = menu else {
            debug!("activate_menu_item({id:?}, {menu_item_id}) failed: that tray item has no registered dbusmenu");
            return;
        };
        let data = Value::I32(0);
        if let Err(err) = menu.event(menu_item_id, "clicked", &data, epoch_seconds(SystemTime::now()) as u32).await {
            debug!("activate_menu_item({id:?}, {menu_item_id}) failed: {err}");
        }
    }

    /// `tray:menu_will_show(id, submenu_id)` calls DBusMenu `AboutToShow(submenu_id)`, its
    /// lazy-population signal, then re-fetches and pushes the entire menu tree unless the item
    /// answers, or the refetch shows, that nothing changed; a later change arrives as `LayoutUpdated` (ADR-0031). Full
    /// refetch is adequate for human-scale trees.
    pub async fn menu_will_show(&self, id: &str, submenu_id: i32) {
        let Some((key, menu)) = self.find("menu_will_show", id, |key, entry| (key.clone(), entry.menu.clone())) else {
            return;
        };
        let Some(menu) = menu else {
            debug!("menu_will_show({id:?}, {submenu_id}) failed: that tray item has no registered dbusmenu");
            return;
        };
        // An error still refetches: some items never implement `AboutToShow`.
        match menu.about_to_show(submenu_id).await {
            Ok(false) => return,
            Ok(true) => {}
            Err(err) => debug!("menu_will_show({id:?}, {submenu_id}) AboutToShow failed: {err}"),
        }
        let max_nodes = remaining_menu_budget(&self.registry.lock().expect("mutex poisoned"), &key);
        match fetch_menu_via(&menu, &icon_filename_stem(id), max_nodes).await {
            Ok(mut items) => {
                let mut guard = self.registry.lock().expect("mutex poisoned");
                let budget = remaining_menu_budget(&guard, &key);
                // A `NewMenu` rebind during the fetch made this menu stale.
                let current = |entry: &&mut ItemEntry| {
                    entry.menu.as_ref().map(|bound| bound.inner().path()) == Some(menu.inner().path())
                };
                let changed = guard.items.get_mut(&key).filter(current).is_some_and(|entry| {
                    enforce_menu_budget(&entry.last_known.id, &mut items, budget);
                    store_menu(entry, items)
                });
                drop(guard);
                if changed {
                    let _ = self.events.send(());
                }
            }
            Err(err) => debug!("menu_will_show({id:?}, {submenu_id}) GetLayout failed: {err}"),
        }
    }
}

/// Whether `name` is an item's well-known `org.{kde,freedesktop}.StatusNotifierItem-PID-N` name
/// claimed before `RegisterStatusNotifierItem` (ADR-0073).
///
/// Accepts both KDE and Chromium spellings. The trailing `-` excludes the watcher and names that
/// merely share the prefix.
fn is_item_bus_name(name: &str) -> bool {
    ["org.kde.StatusNotifierItem-", "org.freedesktop.StatusNotifierItem-"].iter().any(|prefix| name.starts_with(prefix))
}

/// Object paths an item that never named one might be exporting at, tried in order until one
/// answers (ADR-0171).
///
/// Adoption has no `RegisterStatusNotifierItem` argument to read, so ADR-0031's default is a guess,
/// and it is the wrong guess for every Chromium application, which exports at
/// `/StatusNotifierItem/1`.
///
/// The list is the conventions a real session shows. An item exporting anywhere else still needs
/// introspection, which ADR-0073 declined; the ayatana shape is deliberately absent, because its
/// path ends in an application-chosen id no list can hold, and those clients re-register on
/// `StatusNotifierHostRegistered` anyway.
const ADOPTION_OBJECT_PATHS: [&str; 3] =
    [DEFAULT_ITEM_OBJECT_PATH, "/StatusNotifierItem/1", "/org/chromium/StatusNotifierItem/1"];

/// Registers tray items already on the bus when this host starts (ADR-0073). The spec expects
/// clients to re-register after `StatusNotifierHostRegistered`, but Slack does not; without this
/// bus scan, restarting the shell lost Slack until Slack restarted.
///
/// One task per name, so an app that answers nothing delays only its own adoption. Duplicates are
/// harmless: `(unique_name, object_path)` is the key, so re-registration overwrites the entry.
///
/// ponytail: finds only items that claimed a well-known name. An item registering only
/// `RegisterStatusNotifierItem("/some/object/path")` is invisible without introspecting every
/// session-bus connection. Vesktop has that shape but re-registers on the signal. Upgrade path:
/// introspection, costing dozens of startup round trips for a rare case.
pub(super) async fn adopt_existing_items(
    connection: &zbus::Connection,
    dbus_proxy: &zbus::fdo::DBusProxy<'_>,
    registry: &ItemRegistry,
    events: &UnboundedSender<()>,
) {
    let names = match dbus_proxy.list_names().await {
        Ok(names) => names,
        Err(err) => {
            warn!("ListNames failed, so no already-running item is adopted this run: {err}");
            return;
        }
    };
    let mut adoptions = tokio::task::JoinSet::new();
    for name in names.into_iter().filter(|name| is_item_bus_name(name.as_str())) {
        let (connection, registry, events) = (connection.clone(), registry.clone(), events.clone());
        adoptions.spawn(async move {
            // No sender: this well-known branch does not need one, and no call supplies it.
            let resolved = match resolve_registration(&connection, name.as_str(), None).await {
                Ok(resolved) => resolved,
                Err(err) => {
                    debug!(2; "{name} looks like an item but could not be resolved: {err}");
                    return;
                }
            };
            {
                let guard = registry.lock().expect("mutex poisoned");
                if ADOPTION_OBJECT_PATHS.iter().any(|candidate| {
                    OwnedObjectPath::try_from(*candidate)
                        .is_ok_and(|path| guard.items.contains_key(&(resolved.unique_name.clone(), path)))
                }) {
                    return;
                }
            }
            let mut refusals = Vec::new();
            for candidate in ADOPTION_OBJECT_PATHS {
                let object_path = match OwnedObjectPath::try_from(candidate) {
                    Ok(path) => path,
                    Err(err) => {
                        debug!(2; "{candidate} is not an object path: {err}");
                        continue;
                    }
                };
                let attempt = ResolvedRegistration { object_path, ..resolved.clone() };
                match register_item(&connection, &registry, &events, attempt, false).await {
                    RegisterOutcome::Registered => {
                        debug!("adopted {name} at {candidate}, registered before this host started");
                        return;
                    }
                    RegisterOutcome::Deferred => return,
                    RegisterOutcome::Failed(err) => refusals.push(err),
                }
            }
            if !refusals.is_empty() {
                debug!(2; "failed to adopt {name}: {}", refusals.join("; "));
            }
        });
    }
    while adoptions.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::watcher::tests::{StubDBusDaemon, StubStatusNotifierItem};
    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;

    /// Adoption has no registration argument to read, so it guessed the spec default and stopped
    /// there, which is no path at all for a Chromium application, and cost Slack its icon on
    /// every restart of the shell (ADR-0171).
    ///
    /// Stubs go through `p2p_pair_serving` (see `watcher::tests::register_status_notifier_item_accepts_a_unique_name_matching_the_real_sender`):
    /// adoption calls the peer and waits for the verdict. A throwaway `ListNames` to spend the first call hid the dropped-call race.
    #[tokio::test]
    async fn adoption_finds_an_item_that_exports_only_the_chromium_path() {
        let (connection, _peer) = p2p_pair_serving(|peer| {
            peer.serve_at("/StatusNotifierItem/1", StubStatusNotifierItem)?
                .serve_at("/org/freedesktop/DBus", StubDBusDaemon)
        })
        .await;

        let registry = ItemRegistry::default();
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        let dbus_proxy = zbus::fdo::DBusProxy::new(&connection).await.expect("failed to bind the stub daemon");

        adopt_existing_items(&connection, &dbus_proxy, &registry, &events).await;

        let paths: Vec<String> =
            registry.lock().unwrap().items.keys().map(|(_, object_path)| object_path.to_string()).collect();
        assert_eq!(paths, vec!["/StatusNotifierItem/1".to_string()]);
    }

    /// A `NewMenu` rebind while `menu_will_show` fetches makes that fetch stale: it is not stored.
    #[tokio::test]
    async fn menu_will_show_stores_a_fetch_only_from_the_menu_still_bound() {
        use super::super::proxies::bind_dbusmenu;
        use super::super::registry::tests::{CountingMenu, entry, key};
        use crate::capabilities::test_support::{poll_until, private_bus, within};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let bus = private_bus().await;
        let (fetches, gate) = (Arc::new(AtomicUsize::new(0)), Arc::new(tokio::sync::Semaphore::new(1)));
        let menu = || CountingMenu { fetches: fetches.clone(), gate: gate.clone() };
        let app = bus.builder().serve_at("/MenuBar", menu()).unwrap().serve_at("/MenuBar2", menu()).unwrap();
        let app = app.build().await.unwrap();
        let connection = bus.connection().await;
        let destination: zbus::names::OwnedBusName =
            zbus::names::BusName::from(app.unique_name().unwrap().clone()).into();
        let bind = |path: &'static str| {
            let (connection, destination) = (connection.clone(), destination.clone());
            async move { bind_dbusmenu(&connection, &destination, &path.try_into().unwrap()).await.unwrap() }
        };
        let mut item = entry(&connection, "item", 0).await;
        item.menu = Some(bind("/MenuBar").await);
        let registry = ItemRegistry::default();
        registry.lock().unwrap().items.insert(key(":1.10"), item);
        let (events, _pushed) = tokio::sync::mpsc::unbounded_channel();
        let controller = TrayController { registry: registry.clone(), events };
        let id = item_id(":1.10", "/StatusNotifierItem");
        let stored = || registry.lock().unwrap().items.get_mut(&key(":1.10")).unwrap().last_known.menu.take().is_some();

        controller.menu_will_show(&id, 0).await;
        assert!(stored(), "a fetch from the bound menu is stored");

        let pending = tokio::spawn({
            let (controller, id) = (controller.clone(), id.clone());
            async move { controller.menu_will_show(&id, 0).await }
        });
        poll_until(|| (fetches.load(Ordering::Relaxed) == 2).then_some(())).await;
        let rebound = bind("/MenuBar2").await;
        registry.lock().unwrap().items.get_mut(&key(":1.10")).unwrap().menu = Some(rebound);
        gate.add_permits(1);
        within(pending).await.unwrap();
        assert!(!stored(), "the replaced menu's reply must not be stored");
    }

    /// Records each click and scroll it receives, tagged with which item it is.
    struct RecordingItem(&'static str, UnboundedSender<String>);

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl RecordingItem {
        fn activate(&self, x: i32, y: i32) {
            let _ = self.1.send(format!("{} Activate {x} {y}", self.0));
        }
        fn context_menu(&self, x: i32, y: i32) {
            let _ = self.1.send(format!("{} ContextMenu {x} {y}", self.0));
        }
        fn secondary_activate(&self, x: i32, y: i32) {
            let _ = self.1.send(format!("{} SecondaryActivate {x} {y}", self.0));
        }
        fn scroll(&self, delta: i32, orientation: String) {
            let _ = self.1.send(format!("{} Scroll {delta} {orientation}", self.0));
        }
    }

    /// Records menu events; answers `AboutToShow` with "nothing changed", so no refetch follows.
    struct RecordingMenu(UnboundedSender<String>);

    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl RecordingMenu {
        fn event(&self, id: i32, event_id: String, _data: zbus::zvariant::OwnedValue, _timestamp: u32) {
            let _ = self.0.send(format!("Event {id} {event_id}"));
        }
        fn about_to_show(&self, id: i32) -> bool {
            let _ = self.0.send(format!("AboutToShow {id}"));
            false
        }
    }

    /// Lua's six actions reach the item or its menu through `dispatch`; an unknown id, `activate`
    /// on an `ItemIsMenu` item, a menu action on an item with no menu, malformed arguments and an
    /// unknown action reach nothing.
    #[tokio::test]
    async fn dispatch_routes_each_action_to_the_item_and_drops_what_it_cannot_resolve() {
        use super::super::proxies::{bind_dbusmenu, bind_item};
        use super::super::registry::tests::{entry, key};
        use crate::capabilities::test_support::{envelope, p2p_pair_serving, within};
        use serde_json::json;
        let (calls, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (connection, _peer) = p2p_pair_serving(|peer| {
            peer.serve_at("/Plain", RecordingItem("plain", calls.clone()))?
                .serve_at("/MenuOnly", RecordingItem("menu-only", calls.clone()))?
                .serve_at("/MenuBar", RecordingMenu(calls))
        })
        .await;
        // A p2p peer ignores the destination; only the path picks the fake.
        let destination = zbus::names::OwnedBusName::try_from("org.example.Item").unwrap();
        let path = |path: &str| OwnedObjectPath::try_from(path).unwrap();
        // `entry` binds `/StatusNotifierItem`; point each at its fake.
        let mut plain = entry(&connection, "plain", 0).await;
        plain.item = bind_item(&connection, &destination, &path("/Plain")).await.unwrap();
        plain.menu = Some(bind_dbusmenu(&connection, &destination, &path("/MenuBar")).await.unwrap());
        let mut menu_only = entry(&connection, "menu-only", 1).await;
        menu_only.item = bind_item(&connection, &destination, &path("/MenuOnly")).await.unwrap();
        menu_only.last_known.item_is_menu = true;
        let registry = ItemRegistry::default();
        registry.lock().unwrap().items.extend([(key(":1.10"), plain), (key(":1.20"), menu_only)]);
        let (events, _pushed) = tokio::sync::mpsc::unbounded_channel();
        let controller = TrayController { registry, events };
        let plain = item_id(":1.10", DEFAULT_ITEM_OBJECT_PATH);
        let menu_only = item_id(":1.20", DEFAULT_ITEM_OBJECT_PATH);
        let dispatch = |action: &str, arguments: serde_json::Value| {
            let arguments = arguments.as_array().expect("arguments are a list").clone();
            super::super::dispatch(&controller, &envelope(shared::Capability::Tray, action, arguments));
        };
        let mut next = async || within(received.recv()).await.expect("the app is alive");

        dispatch("activate", json!([plain, 1, 2]));
        assert_eq!(next().await, "plain Activate 1 2");
        dispatch("context_menu", json!([plain, 3, 4]));
        assert_eq!(next().await, "plain ContextMenu 3 4");
        dispatch("secondary_activate", json!([plain, 5, 6]));
        assert_eq!(next().await, "plain SecondaryActivate 5 6");
        dispatch("scroll", json!([plain, -2, "horizontal"]));
        assert_eq!(next().await, "plain Scroll -2 horizontal");
        dispatch("scroll", json!([plain, 3, "vertical"]));
        assert_eq!(next().await, "plain Scroll 3 vertical");
        dispatch("activate_menu_item", json!([plain, 7]));
        assert_eq!(next().await, "Event 7 clicked");
        dispatch("menu_will_show", json!([plain, 9]));
        assert_eq!(next().await, "AboutToShow 9");

        // Spawned in order on this single-threaded runtime, so a call they wrongly made would
        // arrive before the one that follows them.
        dispatch("activate", json!([menu_only, 1, 2]));
        dispatch("activate_menu_item", json!([menu_only, 7]));
        dispatch("menu_will_show", json!([menu_only, 9]));
        dispatch("activate", json!(["9.99/StatusNotifierItem", 1, 2]));
        dispatch("context_menu", json!([plain]));
        dispatch("secondary_activate", json!([plain, "5", 6]));
        dispatch("scroll", json!([plain, 1, "diagonal"]));
        dispatch("nonsense", json!([plain, 1, 2]));
        dispatch("context_menu", json!([menu_only, 8, 9]));
        assert_eq!(next().await, "menu-only ContextMenu 8 9", "an ItemIsMenu item still takes other clicks");
    }

    /// Another watcher, counting the hosts that register with it.
    struct RivalWatcher(tokio::sync::mpsc::UnboundedSender<()>);

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl RivalWatcher {
        fn register_status_notifier_host(&self, _service: String) {
            let _ = self.0.send(());
        }
    }

    /// Queued behind another watcher, this host registered with that one; winning the name later
    /// has to announce it here, or items waiting for a host never register.
    #[tokio::test]
    async fn winning_the_watcher_name_from_another_watcher_announces_the_host() {
        use crate::capabilities::test_support::{private_bus, within};
        use futures_util::StreamExt;

        let bus = private_bus().await;
        let (hosts, mut rival_hosts) = tokio::sync::mpsc::unbounded_channel();
        let rival = bus.builder().serve_at(WATCHER_OBJECT_PATH, RivalWatcher(hosts)).unwrap();
        let rival = rival.name(WATCHER_BUS_NAME).unwrap().build().await.unwrap();
        let listener = bus.connection().await;
        let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal);
        let rule = rule.member("StatusNotifierHostRegistered").unwrap().build();
        let mut announced = zbus::MessageStream::for_match_rule(rule, &listener, None).await.unwrap();
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        let _tray = TrayController::new(bus.connection().await, events).await;
        within(rival_hosts.recv()).await.expect("this host registers with the rival");
        rival.release_name(WATCHER_BUS_NAME).await.unwrap();

        within(announced.next()).await.expect("stream open").unwrap();
        let watcher =
            zbus::Proxy::new(&listener, WATCHER_BUS_NAME, WATCHER_OBJECT_PATH, "org.kde.StatusNotifierWatcher");
        let registered: bool = watcher.await.unwrap().get_property("IsStatusNotifierHostRegistered").await.unwrap();
        assert!(registered, "this host is registered once its watcher owns the name");
    }

    #[test]
    fn only_a_dash_suffixed_item_name_is_an_item_bus_name() {
        // The trailing `-` excludes prefix-only names; this Supervisor's own watcher name must never be adopted.
        for (name, expected) in [
            ("org.kde.StatusNotifierItem-1240273-1", true),
            ("org.freedesktop.StatusNotifierItem-1240273-1", true),
            ("org.kde.StatusNotifierWatcher", false),
            ("org.kde.StatusNotifierHost-1234", false),
            ("org.kde.StatusNotifierItemRegistry", false),
            ("org.kde.StatusNotifierItem", false),
            ("com.example.org.kde.StatusNotifierItem-1-1", false),
        ] {
            assert_eq!(is_item_bus_name(name), expected, "{name}");
        }
    }
}

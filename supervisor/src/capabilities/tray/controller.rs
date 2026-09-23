//! [`TrayController`]: the `mantle.tray` write-action dispatcher and state owner.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use enumflags2::BitFlags;
use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::{OwnedObjectPath, Value};

use super::menu::fetch_menu_via;
use super::proxies::StatusNotifierWatcherClientProxy;
use super::registration::{ResolvedRegistration, item_id, resolve_registration};
use super::registry::{
    ItemEntry, ItemKey, ItemRegistry, ordered_items, register_item, spawn_name_owner_changed_forwarder,
};
use super::watcher::StatusNotifierWatcher;
use super::{
    DEFAULT_ITEM_OBJECT_PATH, TraySignal, TrayState, WATCHER_BUS_NAME, WATCHER_OBJECT_PATH, unix_timestamp_u32,
};

#[derive(Clone)]
pub struct TrayController {
    registry: ItemRegistry,
    events: UnboundedSender<TraySignal>,
}

impl TrayController {
    /// Requests `org.kde.StatusNotifierWatcher` without `ReplaceExisting`/`DoNotQueue` flags
    /// (ADR-0031). With `DoNotQueue` unset, zbus queues and returns `Ok(InQueue)`, so only a hard
    /// `Err` is failure, and it does not stop construction.
    ///
    /// Exports [`WATCHER_OBJECT_PATH`] before claiming the name, so a call routed to the new owner
    /// finds the object. Awaits no app (ADR-0031 amendment).
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<TraySignal>) -> Self {
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let host_registered = Arc::new(Mutex::new(false));
        let watcher = StatusNotifierWatcher {
            connection: connection.clone(),
            registry: registry.clone(),
            host_registered,
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
            match zbus::fdo::DBusProxy::new(&connection).await {
                Ok(dbus_proxy) => {
                    spawn_name_owner_changed_forwarder(
                        connection.clone(),
                        dbus_proxy.clone(),
                        registry.clone(),
                        events.clone(),
                    );
                    adopt_existing_items(&connection, &dbus_proxy, &registry, &events).await;
                }
                Err(err) => error!("failed to bind org.freedesktop.DBus for NameOwnerChanged tracking: {err}"),
            }
        });

        Self { registry, events }
    }

    /// Empty registry, no forwarders, and nothing exported. Used when the tray session-bus
    /// connection cannot be established; actions behave like a live controller with no items.
    pub fn inert(events: UnboundedSender<TraySignal>) -> Self {
        Self { registry: Arc::new(Mutex::new(HashMap::new())), events }
    }

    /// Re-derives `tray.items` from the registry. Synchronous because forwarders update
    /// `last_known` before sending [`TraySignal`].
    pub fn build_state(&self) -> TrayState {
        TrayState { items: ordered_items(&self.registry) }
    }

    /// What `pick` clones out of the entry `id` names, under one lock that no D-Bus call outlives.
    /// An unknown id is logged under `action`.
    fn find<T>(&self, action: &str, id: &str, pick: impl FnOnce(&ItemKey, &ItemEntry) -> T) -> Option<T> {
        let guard = self.registry.lock().expect("mutex poisoned");
        let found = guard.iter().find(|(key, _)| item_id(key.0.as_str(), key.1.as_str()) == id);
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

    /// `tray:secondary_activate(id, x, y)`: middle-click (ADR-0074). No `ItemIsMenu` gate: it
    /// constrains primary clicks only.
    pub async fn secondary_activate(&self, id: &str, x: i32, y: i32) {
        let Some(item) = self.find("secondary_activate", id, |_, entry| entry.item.clone()) else { return };
        if let Err(err) = item.secondary_activate(x, y).await {
            debug!("secondary_activate({id:?}) failed: {err}");
        }
    }

    /// `tray:scroll(id, delta, orientation)`: icon scroll (ADR-0074). Passes `orientation`
    /// verbatim; the application interprets it, including values beyond the two named orientations.
    pub async fn scroll(&self, id: &str, delta: i32, orientation: &str) {
        let Some(item) = self.find("scroll", id, |_, entry| entry.item.clone()) else { return };
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
        if let Err(err) = menu.event(menu_item_id, "clicked", &data, unix_timestamp_u32()).await {
            debug!("activate_menu_item({id:?}, {menu_item_id}) failed: {err}");
        }
    }

    /// `tray:menu_will_show(id, submenu_id)` calls DBusMenu `AboutToShow(submenu_id)`, its
    /// lazy-population signal, then re-fetches and pushes the entire menu tree unless the item
    /// answers that nothing changed; a later change arrives as `LayoutUpdated` (ADR-0031). Full
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
        match fetch_menu_via(&menu).await {
            Ok(items) => {
                let mut guard = self.registry.lock().expect("mutex poisoned");
                if let Some(entry) = guard.get_mut(&key) {
                    entry.last_known.menu = Some(items);
                }
                drop(guard);
                let _ = self.events.send(TraySignal::RegistryChanged);
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
async fn adopt_existing_items(
    connection: &zbus::Connection,
    dbus_proxy: &zbus::fdo::DBusProxy<'_>,
    registry: &ItemRegistry,
    events: &UnboundedSender<TraySignal>,
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
                match register_item(&connection, &registry, &events, attempt).await {
                    Ok(()) => {
                        debug!("adopted {name} at {candidate}, registered before this host started");
                        return;
                    }
                    Err(err) => refusals.push(err),
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
    use super::super::watcher::tests::{StubDBusDaemon, StubStatusNotifierItem};
    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;

    /// Adoption has no registration argument to read, so it guessed the spec default and stopped
    /// there, which is no path at all for a Chromium application, and cost Slack its icon on
    /// every restart of the shell (ADR-0171).
    ///
    /// Stubs through `p2p_pair_serving` for the same reason as
    /// `watcher::tests::register_status_notifier_item_accepts_a_unique_name_matching_the_real_sender`:
    /// adoption calls the peer and waits for the verdict. This was one thread plus a throwaway
    /// `ListNames` to spend the first call, which hid the dropped-call race rather than closing it.
    #[tokio::test]
    async fn adoption_finds_an_item_that_exports_only_the_chromium_path() {
        let (connection, _peer) = p2p_pair_serving(|peer| {
            peer.serve_at("/StatusNotifierItem/1", StubStatusNotifierItem)?
                .serve_at("/org/freedesktop/DBus", StubDBusDaemon)
        })
        .await;

        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        let dbus_proxy = zbus::fdo::DBusProxy::new(&connection).await.expect("failed to bind the stub daemon");

        adopt_existing_items(&connection, &dbus_proxy, &registry, &events).await;

        let paths: Vec<String> =
            registry.lock().unwrap().keys().map(|(_, object_path)| object_path.to_string()).collect();
        assert_eq!(paths, vec!["/StatusNotifierItem/1".to_string()]);
    }

    #[test]
    fn an_item_name_is_recognized_in_both_spellings() {
        assert!(is_item_bus_name("org.kde.StatusNotifierItem-1240273-1"));
        assert!(is_item_bus_name("org.freedesktop.StatusNotifierItem-1240273-1"));
    }

    #[test]
    fn the_watcher_is_not_an_item() {
        // This Supervisor's own name; adopting it would register the watcher as an icon.
        assert!(!is_item_bus_name("org.kde.StatusNotifierWatcher"));
        assert!(!is_item_bus_name("org.kde.StatusNotifierHost-1234"));
    }

    #[test]
    fn a_name_that_only_starts_the_same_way_is_not_an_item() {
        // The trailing `-` excludes these prefix-only names.
        assert!(!is_item_bus_name("org.kde.StatusNotifierItemRegistry"));
        assert!(!is_item_bus_name("org.kde.StatusNotifierItem"));
        assert!(!is_item_bus_name("com.example.org.kde.StatusNotifierItem-1-1"));
    }
}

//! Per-item registry: hydration + signal-forwarder tasks that keep each tracked
//! `StatusNotifierItem`'s [`super::item::TrayItem`] snapshot live.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::stream::{BoxStream, SelectAll};
use futures_util::{FutureExt, StreamExt};
use shared::debug;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use zbus::names::{BusName, OwnedUniqueName};
use zbus::zvariant::OwnedObjectPath;

use crate::capabilities::shm_icons;

use super::icon::{SPOOL_SUBDIR, icon_filename_stem};
use super::item::{TrayItem, fetch_tray_item_base};
use super::menu::{MenuItem, fetch_menu_via};
use super::proxies::{DBusMenuProxy, NewMenuStream, StatusNotifierItemProxy, bind_dbusmenu, bind_item};
use super::registration::{ResolvedRegistration, item_id};

/// Process-wide sequence for [`ItemEntry::registered`]. It only needs to increase; each registry
/// still sees a monotonic order in tests.
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(0);

pub(super) struct ItemEntry {
    pub(super) item: StatusNotifierItemProxy<'static>,
    pub(super) menu: Option<DBusMenuProxy<'static>>,
    pub(super) last_known: TrayItem,
    /// First-registration sequence used by [`ordered_items`], so `HashMap` iteration order never
    /// reshuffles the tray.
    registered: u64,
    properties_forwarder: JoinHandle<()>,
    menu_forwarder: JoinHandle<()>,
}

/// `tray.items` in registration order. Sorting D-Bus ids lexicographically puts `1.100` before
/// `1.20` and inserts a new app mid-strip; registration order also appends.
pub(super) fn ordered_items(registry: &ItemRegistry) -> Vec<TrayItem> {
    let guard = registry.lock().expect("tray registry mutex poisoned");
    let mut entries: Vec<&ItemEntry> = guard.values().collect();
    entries.sort_by_key(|entry| entry.registered);
    entries.into_iter().map(|entry| entry.last_known.clone()).collect()
}

pub(super) type ItemKey = (OwnedUniqueName, OwnedObjectPath);
pub(super) type ItemRegistry = Arc<Mutex<HashMap<ItemKey, ItemEntry>>>;

/// Binds and hydrates a `StatusNotifierItem` and its menu (ADR-0031 eager fetch), spawns
/// forwarders, and inserts it into `registry`. Replaces the same key and aborts its old forwarders.
pub(super) async fn register_item(
    connection: &zbus::Connection,
    registry: &ItemRegistry,
    events: &UnboundedSender<()>,
    resolved: ResolvedRegistration,
) -> Result<(), String> {
    let ResolvedRegistration { unique_name, destination, object_path } = resolved;
    let item = bind_item(connection, &destination, &object_path)
        .await
        .map_err(|err| format!("failed to bind StatusNotifierItem: {err}"))?;

    // An object that answers nothing is not an item: `bind_item` does no I/O and
    // `fetch_tray_item_base` defaults every property, so an adoption guess at a path nobody exports
    // would insert a blank phantom (ADR-0168). SNI makes `Status` mandatory, so a live item answers.
    if let Err(err) = item.status().await {
        return Err(format!("{destination} exports no StatusNotifierItem at {object_path}: {err}"));
    }

    // Subscribed before each first read, so a change signalled during registration is refetched.
    let (signals, new_menu) = (item_signals(&item).await, item.receive_new_menu().await);
    let mut tray_item = fetch_tray_item_base(&item, &unique_name, &object_path, &TrayItem::default(), false).await;

    let menu = bind_item_menu(&item).await.inspect_err(|err| debug!("no DBusMenu for {unique_name}: {err}"));
    let menu = menu.ok().flatten();
    let updates = menu_updates(menu.as_ref()).await;
    if let Some(menu) = &menu {
        match fetch_menu_via(menu, &icon_filename_stem(&tray_item.id)).await {
            Ok(items) => tray_item.menu = Some(items),
            Err(err) => debug!("GetLayout failed for {unique_name}: {err}"),
        }
    }

    let key: ItemKey = (unique_name.clone(), object_path.clone());

    // TOCTOU guard: the property/GetLayout awaits can outlive the connection, while
    // NameOwnerChanged only removes entries that already exist. Check liveness immediately before
    // insertion, with no await after it. Best-effort failures proceed; this narrows, not
    // eliminates, the race.
    if let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(connection).await {
        match dbus_proxy.name_has_owner(BusName::from(unique_name.clone())).await {
            Ok(false) => return Err(format!("{unique_name} disconnected during registration")),
            Ok(true) => {}
            Err(err) => {
                debug!("pre-insert liveness check for {unique_name} failed (proceeding anyway): {err}")
            }
        }
    }

    let previous = {
        let mut guard = registry.lock().expect("mutex poisoned");
        // Spawned under the lock, so a signal already queued finds the entry rather than ending its forwarder.
        let (k, r, e) = (key.clone(), registry.clone(), events.clone());
        let properties_forwarder = spawn_item_signal_forwarder(item.clone(), signals, k, r, e);
        let (k, r, e) = (key.clone(), registry.clone(), events.clone());
        let menu_forwarder = spawn_menu_signal_forwarder(&item, new_menu, menu.clone(), updates, k, r, e);
        let mut entry =
            ItemEntry { item, menu, last_known: tray_item, registered: 0, properties_forwarder, menu_forwarder };
        entry.registered = match guard.get(&key) {
            // Same key means re-registration, so keep its place. A restart gets a new unique name
            // and key, so it is a new item.
            Some(existing) => existing.registered,
            None => NEXT_REGISTRATION.fetch_add(1, Ordering::Relaxed),
        };
        guard.insert(key, entry)
    };
    if let Some(previous) = previous {
        previous.properties_forwarder.abort();
        previous.menu_forwarder.abort();
    }
    let _ = events.send(());
    Ok(())
}

/// Stores refreshed SNI properties under the menu the entry already holds.
///
/// `fetch_tray_item_base` reads properties only and leaves `menu` unset, so the menu has to move
/// across; assigning `refreshed` on its own blanks the menu on every title or icon change.
///
/// Deletes a spooled PNG the refreshed item no longer names; departure only reaps current paths.
///
/// Returns whether the item changed, new pixels at an unchanged PNG path included.
fn keep_menu_across(entry: &mut ItemEntry, mut refreshed: TrayItem) -> bool {
    let old = &entry.last_known;
    for (old, new) in [
        (&old.icon_path, &refreshed.icon_path),
        (&old.attention_icon_path, &refreshed.attention_icon_path),
        (&old.overlay_icon_path, &refreshed.overlay_icon_path),
    ] {
        if let Some(old) = old
            && Some(old) != new.as_ref()
        {
            shm_icons::remove_png(SPOOL_SUBDIR, old);
        }
    }
    let menu = entry.last_known.menu.take();
    let changed = refreshed != entry.last_known;
    refreshed.menu = menu;
    entry.last_known = refreshed;
    changed
}

/// Waits for a signal, then drains the ones already queued (an app sends NewIcon+NewToolTip+NewTitle together).
/// `true` when any was an icon signal, which needs the full fetch.
async fn next_burst(signals: &mut (impl futures_util::Stream<Item = bool> + Unpin)) -> Option<bool> {
    let mut icon = signals.next().await?;
    while let Some(next) = signals.next().now_or_never().flatten() {
        icon |= next;
    }
    Some(icon)
}

/// Every `NewX` signal but `NewMenu`, `true` for one that changes the icon.
async fn item_signals(item: &StatusNotifierItemProxy<'static>) -> zbus::Result<SelectAll<BoxStream<'static, bool>>> {
    Ok(futures_util::stream::select_all([
        item.receive_new_title().await?.map(|_| false).boxed(),
        item.receive_new_icon().await?.map(|_| true).boxed(),
        item.receive_new_attention_icon().await?.map(|_| true).boxed(),
        item.receive_new_overlay_icon().await?.map(|_| true).boxed(),
        item.receive_new_tool_tip().await?.map(|_| false).boxed(),
        item.receive_new_status().await?.map(|_| true).boxed(),
    ]))
}

/// Re-fetches the [`TrayItem`] properties on every burst of `NewX` signals (all of them for an icon or status signal,
/// else just the text ones) and updates the entry in place without debounce. One task per item; its handle lives in
/// [`ItemEntry`] and is aborted on unregistration.
///
/// The menu is carried across rather than refetched: `spawn_menu_signal_forwarder` refreshes it on
/// `NewMenu` and `LayoutUpdated`, and `controller::menu_will_show` refreshes it on open. Fetching it here too
/// would put a full `GetLayout` round trip behind every frame of an animated icon.
fn spawn_item_signal_forwarder(
    item: StatusNotifierItemProxy<'static>,
    signals: zbus::Result<SelectAll<BoxStream<'static, bool>>>,
    key: ItemKey,
    registry: ItemRegistry,
    events: UnboundedSender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut signals) = signals else { return };
        while let Some(icon) = next_burst(&mut signals).await {
            let Some(previous) = registry.lock().expect("mutex poisoned").get(&key).map(|e| e.last_known.clone())
            else {
                break;
            };
            let refreshed = fetch_tray_item_base(&item, &key.0, &key.1, &previous, !icon).await;

            let mut guard = registry.lock().expect("mutex poisoned");
            let Some(entry) = guard.get_mut(&key) else { break };
            let changed = keep_menu_across(entry, refreshed);
            drop(guard);

            if changed && events.send(()).is_err() {
                break;
            }
        }
    })
}

/// Stores a refetched menu and returns whether it differs from the stored one. Tray skips
/// equal-snapshot dedupe for its icon spool, so this is what keeps an app's no-op `LayoutUpdated`
/// from pushing the whole tray. Menus carry no pixmaps, so equal means nothing to redraw.
pub(super) fn store_menu(entry: &mut ItemEntry, items: Vec<MenuItem>) -> bool {
    let changed = entry.last_known.menu.as_ref() != Some(&items);
    if let Some(old) = entry.last_known.menu.replace(items) {
        reap_menu_icons(&entry.last_known.id, &old, entry.last_known.menu.as_deref().unwrap_or_default());
    }
    changed
}

/// Deletes the item's own `{stem}_menu_{id}.png` files that `old` names and `kept` no longer does;
/// an app-supplied `icon_name` cannot reap another item's file.
fn reap_menu_icons(item_id: &str, old: &[MenuItem], kept: &[MenuItem]) {
    let prefix = format!("{}_menu_", icon_filename_stem(item_id));
    let own = |path: &str| {
        let name = std::path::Path::new(path).file_name().and_then(|name| name.to_str());
        name.and_then(|name| name.strip_prefix(&prefix)?.strip_suffix(".png"))
            .is_some_and(|id| id.parse::<i32>().is_ok())
    };
    fn paths<'a>(items: &'a [MenuItem], out: &mut Vec<&'a str>) {
        for item in items {
            out.extend(item.icon_name.as_deref());
            paths(&item.children, out);
        }
    }
    let (mut gone, mut live) = (Vec::new(), Vec::new());
    paths(old, &mut gone);
    paths(kept, &mut live);
    for path in gone.into_iter().filter(|path| own(path) && !live.contains(path)) {
        shm_icons::remove_png(SPOOL_SUBDIR, path);
    }
}

/// Refetch 100 ms after the last menu signal: apps send one per changed submenu, and each refetch
/// is a whole `GetLayout`.
const MENU_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// The item's `Menu`, bound; `Ok(None)` for none, including the `/NO_DBUSMENU` Qt names until the app sets one.
async fn bind_item_menu(item: &StatusNotifierItemProxy<'static>) -> zbus::Result<Option<DBusMenuProxy<'static>>> {
    let path = item.menu().await?;
    if matches!(path.as_str(), "/" | "/NO_DBUSMENU") {
        return Ok(None);
    }
    let destination = item.inner().destination().to_owned().into();
    bind_dbusmenu(item.inner().connection(), &destination, &path).await.map(Some)
}

/// `LayoutUpdated` and `ItemsPropertiesUpdated` from `menu`; none without one.
async fn menu_updates(menu: Option<&DBusMenuProxy<'static>>) -> BoxStream<'static, bool> {
    let Some(menu) = menu else { return futures_util::stream::pending().boxed() };
    match (menu.receive_layout_updated().await, menu.receive_items_properties_updated().await) {
        (Ok(layout), Ok(props)) => futures_util::stream::select(layout.map(|_| false), props.map(|_| false)).boxed(),
        _ => futures_util::stream::pending().boxed(),
    }
}

/// Refetches the full menu after each settled burst of `NewMenu`, `LayoutUpdated` or
/// `ItemsPropertiesUpdated`, rebinding `Menu` on `NewMenu`, and updates `menu` in place (ADR-0031). One task per item,
/// aborted with the item task on unregistration.
fn spawn_menu_signal_forwarder(
    item: &StatusNotifierItemProxy<'static>,
    new_menu: zbus::Result<NewMenuStream>,
    mut menu: Option<DBusMenuProxy<'static>>,
    mut updates: BoxStream<'static, bool>,
    key: ItemKey,
    registry: ItemRegistry,
    events: UnboundedSender<()>,
) -> JoinHandle<()> {
    let item = item.clone();
    tokio::spawn(async move {
        let Ok(mut new_menu) = new_menu else { return };
        let item_stem = icon_filename_stem(&item_id(key.0.as_str(), key.1.as_str()));
        loop {
            let mut changes = futures_util::stream::select(new_menu.by_ref().map(|_| true), updates.by_ref());
            let Some(mut rebind) = changes.next().await else { break };
            while let Ok(Some(next)) = tokio::time::timeout(MENU_SETTLE, changes.next()).await {
                rebind |= next;
            }
            // A failed read is no `Menu` change: keep the bound menu rather than wipe a working one.
            if rebind && let Some(bound) = bind_item_menu(&item).await.inspect_err(|err| debug!("NewMenu: {err}")).ok()
            {
                menu = bound;
                updates = menu_updates(menu.as_ref()).await;
                let mut guard = registry.lock().expect("mutex poisoned");
                let Some(entry) = guard.get_mut(&key) else { break };
                entry.menu = menu.clone();
                let cleared = menu.is_none()
                    && entry.last_known.menu.take().is_some_and(|old| {
                        reap_menu_icons(&entry.last_known.id, &old, &[]);
                        true
                    });
                drop(guard);
                if cleared && events.send(()).is_err() {
                    break;
                }
            }
            let Some(menu) = &menu else { continue };
            match fetch_menu_via(menu, &item_stem).await {
                Ok(items) => {
                    let mut guard = registry.lock().expect("mutex poisoned");
                    let Some(entry) = guard.get_mut(&key) else { break };
                    let changed = store_menu(entry, items);
                    drop(guard);
                    if changed && events.send(()).is_err() {
                        break;
                    }
                }
                Err(err) => debug!(2; "GetLayout (menu signal refresh) failed: {err}"),
            }
        }
    })
}

/// Removes entries whose unique name drops off the bus (`new_owner` empty). SNI has no
/// `UnregisterStatusNotifierItem` signal, so this one global subscription supplies liveness
/// (ADR-0031). `connection` emits `StatusNotifierItemUnregistered` for other hosts on the bus;
/// Mantle's own tray reads this registry instead.
pub(super) fn spawn_name_owner_changed_forwarder(
    connection: zbus::Connection,
    mut stream: zbus::fdo::NameOwnerChangedStream,
    registry: ItemRegistry,
    events: UnboundedSender<()>,
) {
    tokio::spawn(async move {
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };
            let dropped_name = args.name.to_string();

            let removed: Vec<(ItemKey, ItemEntry)> = registry
                .lock()
                .expect("mutex poisoned")
                .extract_if(|(unique_name, _), _| unique_name.as_str() == dropped_name)
                .collect();
            if removed.is_empty() {
                continue;
            }
            let mut departed = Vec::with_capacity(removed.len());
            for ((unique_name, object_path), entry) in removed {
                // The same `service + path` spelling `register_item` announces (ADR-0172).
                let item_id = format!("{}{}", unique_name.as_str(), object_path.as_str());
                debug!("tray item {} disconnected", item_id);
                departed.push(item_id);
                entry.properties_forwarder.abort();
                entry.menu_forwarder.abort();
                // Remove all three variant files (ADR-0074). Reconnecting apps get a new unique
                // name, so stale PNGs otherwise pile up for the rest of the session (logind clears
                // $XDG_RUNTIME_DIR only when the user's last session ends).
                for path in [
                    &entry.last_known.icon_path,
                    &entry.last_known.attention_icon_path,
                    &entry.last_known.overlay_icon_path,
                ]
                .into_iter()
                .flatten()
                {
                    shm_icons::remove_png(SPOOL_SUBDIR, path);
                }
                reap_menu_icons(&entry.last_known.id, entry.last_known.menu.as_deref().unwrap_or_default(), &[]);
            }
            if events.send(()).is_err() {
                break;
            }
            // Emitted last: a stalled D-Bus write must not hold up the cleanup above.
            if let Ok(emitter) = zbus::object_server::SignalEmitter::new(&connection, super::WATCHER_OBJECT_PATH) {
                for id in departed {
                    let _ =
                        super::watcher::StatusNotifierWatcher::status_notifier_item_unregistered(&emitter, &id).await;
                }
            }
        }
    });
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::capabilities::test_support::p2p_pair;
    use zbus::object_server::SignalEmitter;

    /// Minimal entry for ordering tests. A p2p proxy bind makes no call, so no answering peer is
    /// needed.
    pub(in super::super) async fn entry(connection: &zbus::Connection, id: &str, registered: u64) -> ItemEntry {
        let destination = zbus::names::OwnedBusName::try_from("org.example.Item").expect("a valid bus name");
        let path = OwnedObjectPath::try_from("/StatusNotifierItem").expect("a valid object path");
        ItemEntry {
            item: bind_item(connection, &destination, &path).await.expect("binding makes no call"),
            menu: None,
            last_known: TrayItem { id: id.to_string(), ..TrayItem::default() },
            registered,
            properties_forwarder: tokio::spawn(std::future::ready(())),
            menu_forwarder: tokio::spawn(std::future::ready(())),
        }
    }

    #[tokio::test]
    async fn queued_text_signals_are_one_text_fetch_and_an_icon_signal_forces_the_full_one() {
        use futures_util::stream::{self, StreamExt};
        let mut text = stream::iter([false, false, false]).chain(stream::pending());
        assert_eq!(next_burst(&mut text).await, Some(false));
        let mut mixed = stream::iter([false, true, false]).chain(stream::pending());
        assert_eq!(next_burst(&mut mixed).await, Some(true));
        assert_eq!(next_burst(&mut stream::empty()).await, None);
    }

    pub(in super::super) fn key(unique: &str) -> ItemKey {
        (
            OwnedUniqueName::try_from(unique).expect("a valid unique name"),
            OwnedObjectPath::try_from("/StatusNotifierItem").expect("a valid object path"),
        )
    }

    /// `HashMap` iteration is process-seeded; without registration order, an unrelated update
    /// reshuffled the tray.
    #[tokio::test]
    async fn the_strip_is_in_registration_order_whatever_the_map_says() {
        let (connection, _peer) = p2p_pair().await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Build entries before locking; the helper awaits and a std mutex guard must not cross it.
        let third = entry(&connection, "third", 2).await;
        let first = entry(&connection, "first", 0).await;
        let second = entry(&connection, "second", 1).await;
        // Insert out of registration order; a `HashMap` may return that order.
        {
            let mut guard = registry.lock().unwrap();
            guard.insert(key(":1.30"), third);
            guard.insert(key(":1.10"), first);
            guard.insert(key(":1.20"), second);
        }
        let ids: Vec<String> = ordered_items(&registry).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, ["first", "second", "third"]);
    }

    /// A property signal carries no menu, so assigning the refreshed item on its own would blank a
    /// menu that only `LayoutUpdated` and opening the menu ever refill.
    #[tokio::test]
    async fn refreshing_properties_keeps_the_menu_and_reports_only_real_changes() {
        let (connection, _peer) = p2p_pair().await;
        let mut entry = entry(&connection, "item", 0).await;
        entry.last_known.menu = Some(vec![MenuItem { label: Some("Quit".to_string()), ..MenuItem::default() }]);

        let renamed = TrayItem { id: "item".to_string(), name: "renamed".to_string(), ..TrayItem::default() };
        assert!(keep_menu_across(&mut entry, renamed.clone()));

        assert_eq!(entry.last_known.name, "renamed", "the refreshed properties must land");
        let menu = entry.last_known.menu.as_ref().expect("a property refresh must not blank the menu");
        assert_eq!(menu[0].label.as_deref(), Some("Quit"));

        assert!(!keep_menu_across(&mut entry, renamed.clone()), "an identical refresh is no change");
        assert!(entry.last_known.menu.is_some(), "an unchanged refresh must not blank the menu either");

        let pixmap = TrayItem {
            icon_path: Some("/spool/item.png".to_string()),
            pixmap_digests: [Some(1), None, None],
            ..renamed
        };
        assert!(keep_menu_across(&mut entry, pixmap.clone()));
        assert!(!keep_menu_across(&mut entry, pixmap.clone()), "the same pixels are no change");
        let repainted = TrayItem { pixmap_digests: [Some(2), None, None], ..pixmap };
        assert!(keep_menu_across(&mut entry, repainted), "the same PNG path can hold new pixels");
    }

    /// Counts `GetLayout` calls, each held until the test adds a permit to `gate`.
    pub(in super::super) struct CountingMenu {
        pub(in super::super) fetches: Arc<std::sync::atomic::AtomicUsize>,
        pub(in super::super) gate: Arc<tokio::sync::Semaphore>,
    }

    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl CountingMenu {
        async fn get_layout(
            &self,
            _parent: i32,
            _depth: i32,
            _names: Vec<String>,
        ) -> (u32, super::super::proxies::RawMenuLayout) {
            self.fetches.fetch_add(1, Ordering::Relaxed);
            self.gate.acquire().await.unwrap().forget();
            let child = zbus::zvariant::Value::from((
                1,
                HashMap::<String, zbus::zvariant::Value>::new(),
                Vec::<zbus::zvariant::Value>::new(),
            ));
            (1, (0, HashMap::new(), vec![child.try_into().unwrap()]))
        }

        #[zbus(signal)]
        async fn layout_updated(emitter: &SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;
    }

    struct FakeItem;

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl FakeItem {
        #[zbus(property, name = "Menu")]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from("/MenuBar").unwrap()
        }
        #[zbus(signal)]
        async fn new_menu(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    }

    /// The burst queues while a fetch is held, so the debounce sees it whole however slow the run.
    #[tokio::test]
    async fn a_burst_of_menu_signals_is_one_fetch() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (item_path, menu_path) = ("/StatusNotifierItem", "/MenuBar");
        let app = bus.builder().serve_at(menu_path, CountingMenu { fetches: fetches.clone(), gate: gate.clone() });
        let app = app.unwrap().serve_at(item_path, FakeItem).unwrap().build().await.unwrap();
        let connection = bus.connection().await;
        // Starts dispatch, so the `Ping` below is answered.
        connection.object_server();
        let destination = BusName::from(app.unique_name().unwrap().clone()).into();
        let item = bind_item(&connection, &destination, &OwnedObjectPath::try_from(item_path).unwrap()).await.unwrap();
        let menu = bind_dbusmenu(&connection, &destination, &OwnedObjectPath::try_from(menu_path).unwrap()).await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let entry = entry(&connection, "item", 0).await;
        registry.lock().unwrap().insert(key(":1.10"), entry);
        let (events, _pushed) = tokio::sync::mpsc::unbounded_channel();
        let (new_menu, menu) = (item.receive_new_menu().await, menu.ok());
        let updates = menu_updates(menu.as_ref()).await;
        let _forwarder = spawn_menu_signal_forwarder(&item, new_menu, menu, updates, key(":1.10"), registry, events);
        let menu_signals = SignalEmitter::new(&app, menu_path).unwrap();
        let item_signals = SignalEmitter::new(&app, item_path).unwrap();
        // Holds the first fetch at the gate, so the burst below queues behind it.
        while fetches.load(Ordering::Relaxed) == 0 {
            CountingMenu::layout_updated(&menu_signals, 0, 0).await.unwrap();
            tokio::time::sleep(MENU_SETTLE).await;
        }

        for revision in 1..=10 {
            CountingMenu::layout_updated(&menu_signals, revision, 0).await.unwrap();
            FakeItem::new_menu(&item_signals).await.unwrap();
        }
        // The bus keeps one sender's order, so this reply means the burst reached the forwarder.
        let ping = zbus::fdo::PeerProxy::builder(&app).destination(connection.unique_name().unwrap().clone());
        ping.unwrap().path("/").unwrap().build().await.unwrap().ping().await.unwrap();
        gate.add_permits(100);
        crate::capabilities::test_support::within(async {
            while fetches.load(Ordering::Relaxed) < 2 {
                tokio::time::sleep(MENU_SETTLE / 10).await;
            }
        })
        .await;
        tokio::time::sleep(MENU_SETTLE * 3).await;
        assert_eq!(fetches.load(Ordering::Relaxed), 2, "the held fetch, then one for the whole burst");
    }

    #[tokio::test]
    async fn a_refetched_menu_is_a_change_only_when_it_differs() {
        let (connection, _peer) = p2p_pair().await;
        let mut entry = entry(&connection, "item", 0).await;
        let quit = vec![MenuItem { label: Some("Quit".to_string()), ..MenuItem::default() }];
        assert!(store_menu(&mut entry, quit.clone()), "the first menu is a change");
        assert!(!store_menu(&mut entry, quit.clone()), "a no-op LayoutUpdated refetch is no change");
        let toggled = vec![MenuItem { toggle_state: Some(1), ..quit[0].clone() }];
        assert!(store_menu(&mut entry, toggled.clone()));
        assert_eq!(entry.last_known.menu, Some(toggled));
    }

    #[tokio::test]
    async fn menu_icon_files_are_deleted_when_a_refetch_drops_the_node() {
        shm_icons::init_test_spool();
        let spool = |name: &str| shm_icons::write_png(SPOOL_SUBDIR, name, b"\x89PNG").unwrap();
        let (kept, dropped, nested) = (spool("reap_menu_1.png"), spool("reap_menu_2.png"), spool("reap_menu_3.png"));
        let foreign = spool("other_menu_4.png");
        let node = |icon: &str, children| MenuItem { icon_name: Some(icon.into()), children, ..MenuItem::default() };
        let (connection, _peer) = p2p_pair().await;
        let mut entry = entry(&connection, "reap", 0).await;
        let child = node(&nested, vec![]);
        store_menu(&mut entry, vec![node(&kept, vec![]), node(&dropped, vec![child]), node(&foreign, vec![])]);

        store_menu(&mut entry, vec![node(&kept, vec![]), node("folder", vec![])]);
        assert!(std::path::Path::new(&kept).exists(), "a node that survives keeps its file");
        assert!(!std::path::Path::new(&dropped).exists() && !std::path::Path::new(&nested).exists());
        assert!(std::path::Path::new(&foreign).exists(), "another item's file is not this menu's to delete");

        reap_menu_icons("reap", entry.last_known.menu.as_deref().unwrap(), &[]);
        assert!(!std::path::Path::new(&kept).exists(), "departure removes the rest");
    }

    /// The departure lands while no forwarder runs yet (as during the startup scan); the
    /// subscription buffers it, so the item inserted meanwhile is still removed.
    #[tokio::test]
    async fn an_item_whose_connection_drops_is_removed_once_the_forwarder_runs() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let host = bus.connection().await;
        let dbus = zbus::fdo::DBusProxy::new(&host).await.unwrap();
        let departures = crate::capabilities::name_departures(&dbus, "test").await.unwrap();
        let app = bus.connection().await;
        let unique = app.unique_name().unwrap().to_owned();
        drop(app);
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let gone = entry(&host, "gone", 0).await;
        registry.lock().unwrap().insert(key(unique.as_str()), gone);
        let (events, mut pushed) = tokio::sync::mpsc::unbounded_channel();

        spawn_name_owner_changed_forwarder(host.clone(), departures, registry.clone(), events);

        crate::capabilities::test_support::within(pushed.recv()).await.expect("the forwarder announces the removal");
        assert!(registry.lock().unwrap().is_empty());
    }

    /// Qt names `/NO_DBUSMENU` until the app sets a menu, then sends `NewMenu`.
    struct LateMenuItem {
        menu: Arc<Mutex<&'static str>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl LateMenuItem {
        #[zbus(property, name = "Status")]
        fn status(&self) -> String {
            "Active".to_string()
        }
        /// `""` fails the read, as an app answering late or erroring once does.
        #[zbus(property, name = "Menu")]
        fn menu(&self) -> zbus::fdo::Result<OwnedObjectPath> {
            let path = *self.menu.lock().unwrap();
            if path.is_empty() {
                *self.menu.lock().unwrap() = "/MenuBar2";
                return Err(zbus::fdo::Error::Failed("not now".into()));
            }
            Ok(OwnedObjectPath::try_from(path).unwrap())
        }
        #[zbus(signal)]
        async fn new_menu(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn a_menu_set_after_show_replaces_qt_no_dbusmenu() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let menu_path = Arc::new(Mutex::new("/NO_DBUSMENU"));
        let gate = Arc::new(tokio::sync::Semaphore::new(100));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = bus.builder().serve_at("/MenuBar", CountingMenu { fetches: fetches.clone(), gate: gate.clone() });
        let app = app.unwrap().serve_at("/MenuBar2", CountingMenu { fetches, gate });
        let app = app.unwrap().serve_at("/StatusNotifierItem", LateMenuItem { menu: menu_path.clone() });
        let app = app.unwrap().build().await.unwrap();
        let connection = bus.connection().await;
        let unique = app.unique_name().unwrap().to_owned();
        let resolved = ResolvedRegistration {
            unique_name: unique.clone(),
            destination: BusName::from(unique.clone()).into(),
            object_path: OwnedObjectPath::try_from("/StatusNotifierItem").unwrap(),
        };
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _pushed) = tokio::sync::mpsc::unbounded_channel();
        register_item(&connection, &registry, &events, resolved).await.unwrap();
        let key: ItemKey = (unique, OwnedObjectPath::try_from("/StatusNotifierItem").unwrap());
        assert!(registry.lock().unwrap()[&key].menu.is_none(), "/NO_DBUSMENU is no menu");

        *menu_path.lock().unwrap() = "/MenuBar";
        let emitter = SignalEmitter::new(&app, "/StatusNotifierItem").unwrap();
        crate::capabilities::test_support::within(async {
            loop {
                LateMenuItem::new_menu(&emitter).await.unwrap();
                tokio::time::sleep(MENU_SETTLE * 2).await;
                let guard = registry.lock().unwrap();
                let entry = &guard[&key];
                if entry.last_known.menu.is_some() {
                    assert_eq!(entry.menu.as_ref().map(|menu| menu.inner().path().as_str()), Some("/MenuBar"));
                    break;
                }
            }
        })
        .await;

        // A `LayoutUpdated` from the old menu in the same burst must not cancel the rebind.
        *menu_path.lock().unwrap() = "/MenuBar2";
        let old_menu = SignalEmitter::new(&app, "/MenuBar").unwrap();
        crate::capabilities::test_support::within(async {
            loop {
                LateMenuItem::new_menu(&emitter).await.unwrap();
                CountingMenu::layout_updated(&old_menu, 1, 0).await.unwrap();
                tokio::time::sleep(MENU_SETTLE * 2).await;
                let guard = registry.lock().unwrap();
                if guard[&key].menu.as_ref().map(|menu| menu.inner().path().as_str()) == Some("/MenuBar2") {
                    break;
                }
            }
        })
        .await;

        // A failed `Menu` read is no menu change: the bound menu and its entries stay.
        *menu_path.lock().unwrap() = "";
        LateMenuItem::new_menu(&emitter).await.unwrap();
        crate::capabilities::test_support::poll_until(|| (*menu_path.lock().unwrap() == "/MenuBar2").then_some(()))
            .await;
        tokio::time::sleep(MENU_SETTLE * 2).await;
        {
            let guard = registry.lock().unwrap();
            assert_eq!(guard[&key].menu.as_ref().map(|menu| menu.inner().path().as_str()), Some("/MenuBar2"));
            assert!(
                guard[&key].last_known.menu.as_ref().is_some_and(|menu| !menu.is_empty()),
                "a failed read wiped the menu"
            );
        }

        // A menu taken away is no menu, `nil` like one never set: its entries would click a proxy that is gone.
        *menu_path.lock().unwrap() = "/";
        crate::capabilities::test_support::within(async {
            loop {
                LateMenuItem::new_menu(&emitter).await.unwrap();
                tokio::time::sleep(MENU_SETTLE * 2).await;
                if ordered_items(&registry)[0].menu.is_none() {
                    break;
                }
            }
        })
        .await;
        assert!(registry.lock().unwrap()[&key].menu.is_none());

        // The forwarder outlives a cleared menu: one set again shows.
        *menu_path.lock().unwrap() = "/MenuBar";
        crate::capabilities::test_support::within(async {
            loop {
                LateMenuItem::new_menu(&emitter).await.unwrap();
                tokio::time::sleep(MENU_SETTLE * 2).await;
                if ordered_items(&registry)[0].menu.as_ref().is_some_and(|menu| !menu.is_empty()) {
                    break;
                }
            }
        })
        .await;
    }

    /// Renames itself while the host registers it: `Menu` is read after the properties.
    struct RenamedDuringRegistration {
        title: Arc<Mutex<&'static str>>,
        connection: Arc<std::sync::OnceLock<zbus::Connection>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl RenamedDuringRegistration {
        #[zbus(property, name = "Status")]
        fn status(&self) -> String {
            "Active".to_string()
        }
        #[zbus(property, name = "Id")]
        fn id(&self) -> String {
            "renaming".to_string()
        }
        #[zbus(property, name = "Title")]
        fn title(&self) -> String {
            self.title.lock().unwrap().to_string()
        }
        #[zbus(property, name = "IconName")]
        fn icon_name(&self) -> String {
            self.title.lock().unwrap().to_string()
        }
        #[zbus(property, name = "Menu")]
        async fn menu(&self) -> OwnedObjectPath {
            *self.title.lock().unwrap() = "renamed";
            let connection = self.connection.get().unwrap();
            let interface = "org.kde.StatusNotifierItem";
            connection
                .emit_signal(None::<BusName<'_>>, "/StatusNotifierItem", interface, "NewTitle", &())
                .await
                .unwrap();
            OwnedObjectPath::try_from("/").unwrap()
        }
    }

    #[tokio::test]
    async fn a_change_signalled_during_registration_is_refetched() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let title = Arc::new(Mutex::new("first"));
        let app_connection = Arc::new(std::sync::OnceLock::new());
        let item = RenamedDuringRegistration { title: title.clone(), connection: app_connection.clone() };
        let app = bus.builder().serve_at("/StatusNotifierItem", item).unwrap().build().await.unwrap();
        app_connection.set(app.clone()).unwrap();
        let connection = bus.connection().await;
        let unique = app.unique_name().unwrap().to_owned();
        let resolved = ResolvedRegistration {
            unique_name: unique.clone(),
            destination: BusName::from(unique.clone()).into(),
            object_path: OwnedObjectPath::try_from("/StatusNotifierItem").unwrap(),
        };
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _pushed) = tokio::sync::mpsc::unbounded_channel();
        register_item(&connection, &registry, &events, resolved).await.unwrap();
        assert_eq!(*title.lock().unwrap(), "renamed", "the rename landed during registration");

        let icon = crate::capabilities::test_support::poll_until(|| {
            let items = ordered_items(&registry);
            items.iter().any(|item| item.name == "renamed").then(|| items[0].icon_name.clone())
        })
        .await;
        assert_eq!(icon.as_deref(), Some("first"), "a title signal reads only the text properties");
    }

    /// In-place updates must not move an item.
    #[tokio::test]
    async fn an_item_that_re_registers_holds_its_place() {
        use super::super::watcher::tests::{StubDBusDaemon, StubStatusNotifierItem};
        use crate::capabilities::test_support::p2p_pair_serving;

        let (connection, _peer) = p2p_pair_serving(|peer| {
            peer.serve_at("/A", StubStatusNotifierItem)?
                .serve_at("/B", StubStatusNotifierItem)?
                .serve_at("/org/freedesktop/DBus", StubDBusDaemon)
        })
        .await;
        let registry: ItemRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        for (unique, path) in [(":1.10", "/A"), (":1.20", "/B"), (":1.10", "/A")] {
            let destination = zbus::names::OwnedBusName::try_from(unique).unwrap();
            let resolved = ResolvedRegistration {
                unique_name: OwnedUniqueName::try_from(unique).unwrap(),
                destination,
                object_path: OwnedObjectPath::try_from(path).unwrap(),
            };
            register_item(&connection, &registry, &events, resolved).await.unwrap();
        }

        let ids: Vec<String> = ordered_items(&registry).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, ["1.10/A", "1.20/B"], "a re-registered item must not jump to the end");
    }
}

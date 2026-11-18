//! Tray caps: the item limit with its waiting queue and deferred adoption, and the shared menu budget.

use shared::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;

use super::menu::{MenuItem, count_menu_nodes, truncate_menu_nodes};
use super::registration::{ResolvedRegistration, item_id};
use super::registry::{ItemKey, ItemRegistry, RegisterOutcome, Registry, reap_menu_icons};
use super::watcher::register_and_announce;
use super::{MAX_ITEMS, MAX_MENU_NODES, MAX_TOTAL_MENU_NODES, MAX_WAITING_ITEMS};

/// Remaining shared menu-node allowance for `key` (excluding `key`'s own existing nodes, so a
/// menu refresh does not starve itself), capped by [`MAX_MENU_NODES`] - 1.
pub(super) fn remaining_menu_budget(registry: &Registry, key: &ItemKey) -> usize {
    let used: usize = registry
        .items
        .iter()
        .filter(|(k, _)| *k != key)
        .filter_map(|(_, entry)| entry.last_known.menu.as_deref())
        .map(count_menu_nodes)
        .sum();
    MAX_TOTAL_MENU_NODES.saturating_sub(used).min(MAX_MENU_NODES - 1)
}

/// Enforces `max_nodes` on `items`, deleting any spooled menu PNGs for nodes that were cut.
pub(super) fn enforce_menu_budget(item_id: &str, items: &mut Vec<MenuItem>, max_nodes: usize) {
    let budget = max_nodes.min(MAX_MENU_NODES - 1);
    let mut remaining = budget;
    let mut dropped = Vec::new();
    truncate_menu_nodes(items, &mut remaining, &mut dropped);
    if !dropped.is_empty() {
        reap_menu_icons(item_id, &dropped, items);
        debug!("menu for {item_id} truncated to {budget} nodes under shared tray budget");
    }
}

pub(super) fn record_at_cap(guard: &mut Registry, resolved: &ResolvedRegistration, defer_on_full: bool) {
    let id = item_id(resolved.unique_name.as_str(), resolved.object_path.as_str());
    let message = format!("not tracking {id}: {} tray items already are; waiting for a slot", guard.items.len());
    if shared::log::first_failure(&format!("tray item limit {}", resolved.unique_name)) {
        warn!("{message}");
    } else {
        debug!("{message}");
    }
    if !defer_on_full {
        guard.adoption_deferred = true;
        return;
    }
    if guard.waiting.iter().any(|w| w.unique_name == resolved.unique_name && w.object_path == resolved.object_path) {
        return;
    }
    if guard.waiting.len() < MAX_WAITING_ITEMS {
        guard.waiting.push_back(resolved.clone());
    } else {
        debug!("tray waiting queue full ({MAX_WAITING_ITEMS}); dropping {id}");
    }
}

/// Admits queued registrations and deferred startup adoptions after a departure frees slot(s).
/// Runs in its own task so a slow or silent waiting item never stalls `NameOwnerChanged`.
pub(super) fn admit_waiting(connection: zbus::Connection, registry: ItemRegistry, events: UnboundedSender<()>) {
    tokio::spawn(async move {
        loop {
            loop {
                let next = {
                    let mut guard = registry.lock().expect("mutex poisoned");
                    if guard.items.len() >= MAX_ITEMS {
                        guard.admitting = false;
                        return;
                    }
                    guard.waiting.pop_front()
                };
                let Some(resolved) = next else { break };
                match register_and_announce(&connection, &registry, &events, resolved).await {
                    (_, RegisterOutcome::Registered) => {}
                    (_, RegisterOutcome::Deferred) => {
                        registry.lock().expect("mutex poisoned").admitting = false;
                        return;
                    }
                    (registered_id, RegisterOutcome::Failed(err)) => {
                        debug!("admitting waiting tray item {registered_id} failed: {err}");
                    }
                }
            }
            let need_adopt = {
                let mut guard = registry.lock().expect("mutex poisoned");
                if guard.items.len() < MAX_ITEMS && guard.adoption_deferred {
                    guard.adoption_deferred = false;
                    true
                } else if guard.items.len() < MAX_ITEMS && !guard.waiting.is_empty() {
                    continue;
                } else {
                    guard.admitting = false;
                    return;
                }
            };
            if need_adopt && let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(&connection).await {
                super::controller::adopt_existing_items(&connection, &dbus_proxy, &registry, &events).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::StreamExt;
    use zbus::zvariant::OwnedObjectPath;

    use super::super::icon::{SPOOL_SUBDIR, icon_filename_stem};
    use super::super::registry::spawn_name_owner_changed_forwarder;
    use super::super::registry::tests::{entry, key, resolved};
    use super::*;
    use crate::capabilities::shm_icons;
    use crate::capabilities::test_support::{p2p_pair, within};

    struct MutableTitleItem {
        title: Arc<Mutex<&'static str>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl MutableTitleItem {
        #[zbus(property, name = "Id")]
        fn id(&self) -> String {
            "mutable-item".to_string()
        }
        #[zbus(property, name = "Title")]
        fn title(&self) -> String {
            self.title.lock().unwrap().to_string()
        }
        #[zbus(property, name = "Status")]
        fn status(&self) -> String {
            "Active".to_string()
        }
    }

    #[tokio::test]
    async fn an_item_past_max_items_waits_and_enters_when_one_leaves() {
        use super::super::TrayController;
        use super::super::watcher::tests::StubStatusNotifierItem;
        use super::super::{WATCHER_BUS_NAME, WATCHER_OBJECT_PATH};
        use crate::capabilities::test_support::{poll_until, private_bus};

        let bus = private_bus().await;
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = TrayController::new(bus.connection().await, events).await;

        async fn watcher_proxy(conn: &zbus::Connection) -> zbus::Proxy<'_> {
            zbus::Proxy::new(conn, WATCHER_BUS_NAME, WATCHER_OBJECT_PATH, "org.kde.StatusNotifierWatcher")
                .await
                .unwrap()
        }
        async fn register(conn: &zbus::Connection) {
            let item = ("/StatusNotifierItem",);
            watcher_proxy(conn).await.call::<_, _, ()>("RegisterStatusNotifierItem", &item).await.unwrap();
        }

        // Register the first item synchronously so `snapshot().items[0]` is known to be this mutable peer.
        let first_title = Arc::new(Mutex::new("initial"));
        let first_app = bus
            .builder()
            .serve_at("/StatusNotifierItem", MutableTitleItem { title: first_title.clone() })
            .unwrap()
            .build()
            .await
            .unwrap();
        let first_unique = first_app.unique_name().unwrap().to_owned();
        let first_id = item_id(first_unique.as_str(), "/StatusNotifierItem");
        register(&first_app).await;
        poll_until(|| (controller.snapshot().items.len() == 1).then_some(())).await;

        let spawn_item = || async {
            let app =
                bus.builder().serve_at("/StatusNotifierItem", StubStatusNotifierItem).unwrap().build().await.unwrap();
            let unique = app.unique_name().unwrap().to_owned();
            register(&app).await;
            (app, item_id(unique.as_str(), "/StatusNotifierItem"))
        };

        let mut apps = futures_util::future::join_all((1..MAX_ITEMS).map(|_| spawn_item())).await;
        poll_until(|| (controller.snapshot().items.len() == MAX_ITEMS).then_some(())).await;

        assert_eq!(controller.snapshot().items[0].id, first_id);

        // Re-registering an already-tracked item at capacity must succeed, refresh, and keep its place.
        *first_title.lock().unwrap() = "re-registered";
        register(&first_app).await;
        poll_until(|| (controller.snapshot().items[0].name == "re-registered").then_some(())).await;
        assert_eq!(controller.snapshot().items[0].id, first_id, "re-registration kept its order");

        // Register three extra items past MAX_ITEMS: one departs while waiting, two stay to enter in order.
        let (extra_departing, extra_departing_id) = spawn_item().await;
        let (extra_waiting1, extra_waiting1_id) = spawn_item().await;
        let (_extra_waiting2, extra_waiting2_id) = spawn_item().await;
        poll_until(|| (controller.waiting_len() == 3).then_some(())).await;

        let host_conn = bus.connection().await;
        let watcher_client = watcher_proxy(&host_conn).await;
        assert_eq!(controller.snapshot().items.len(), MAX_ITEMS);
        assert!(!controller.snapshot().items.iter().any(|item| item.id == extra_waiting1_id));
        assert!(!controller.snapshot().items.iter().any(|item| item.id == extra_waiting2_id));
        assert!(!controller.snapshot().items.iter().any(|item| item.id == extra_departing_id));
        let registered: Vec<String> = watcher_client.get_property("RegisteredStatusNotifierItems").await.unwrap();
        assert_eq!(registered.len(), MAX_ITEMS);
        assert!(registered.contains(&format!("{first_unique}/StatusNotifierItem")));

        // Drop the departing waiting item before any slot frees: waiting queue shrinks by one.
        drop(extra_departing);
        poll_until(|| (controller.waiting_len() == 2).then_some(())).await;

        // Free one tracked slot: waiting1 enters, announced under `service + path`, while waiting2 remains queued.
        let mut announced = watcher_client.receive_signal("StatusNotifierItemRegistered").await.unwrap();
        let waiting1_service = format!("{}/StatusNotifierItem", extra_waiting1.unique_name().unwrap());
        drop(first_app);
        within(async {
            while announced.next().await.unwrap().body().deserialize::<String>().unwrap() != waiting1_service {}
        })
        .await;
        let after_first = poll_until(|| {
            let snap = controller.snapshot();
            snap.items.iter().any(|item| item.id == extra_waiting1_id).then_some(snap)
        })
        .await;
        assert_eq!(after_first.items.len(), MAX_ITEMS);
        assert!(!after_first.items.iter().any(|item| item.id == first_id));
        assert_eq!(controller.waiting_len(), 1, "admitting waiting1 must keep waiting2 queued");

        // Free a second tracked slot: waiting2 enters too.
        let (_, dropped2_id) = apps.remove(0);
        let after_second = poll_until(|| {
            let snap = controller.snapshot();
            snap.items.iter().any(|item| item.id == extra_waiting2_id).then_some(snap)
        })
        .await;
        assert_eq!(after_second.items.len(), MAX_ITEMS);
        assert!(!after_second.items.iter().any(|item| item.id == dropped2_id));
        assert_eq!(controller.waiting_len(), 0);
    }

    #[tokio::test]
    async fn shared_menu_budget_truncates_across_items_and_reaps_dropped_icons() {
        shm_icons::init_test_spool();
        let (connection, _peer) = p2p_pair().await;
        let registry = ItemRegistry::default();
        let per_item = MAX_MENU_NODES - 1;
        let filler_count = MAX_TOTAL_MENU_NODES / per_item;

        for idx in 0..filler_count {
            let mut item_entry = entry(&connection, &format!("1.{idx}/StatusNotifierItem"), idx as u64).await;
            item_entry.last_known.menu = Some(vec![MenuItem::default(); per_item]);
            registry.lock().unwrap().items.insert(key(&format!(":1.{idx}")), item_entry);
        }

        let remaining_for_new = remaining_menu_budget(&registry.lock().unwrap(), &key(":1.99"));
        assert_eq!(remaining_for_new, MAX_TOTAL_MENU_NODES - filler_count * per_item);
        assert_eq!(
            remaining_menu_budget(&registry.lock().unwrap(), &key(":1.0")),
            MAX_MENU_NODES - 1,
            "an existing item refreshing its menu excludes its own nodes from the shared sum"
        );

        let item_id_str = "1.99/StatusNotifierItem";
        let stem = icon_filename_stem(item_id_str);
        let kept_icon = shm_icons::write_png(SPOOL_SUBDIR, &format!("{stem}_menu_1.png"), b"\x89PNG").unwrap();
        let dropped_icon = shm_icons::write_png(SPOOL_SUBDIR, &format!("{stem}_menu_5.png"), b"\x89PNG").unwrap();
        let mut new_menu = vec![
            MenuItem { id: 1, icon_name: Some(kept_icon.clone()), ..MenuItem::default() },
            MenuItem { id: 2, ..MenuItem::default() },
            MenuItem { id: 3, ..MenuItem::default() },
            MenuItem { id: 4, ..MenuItem::default() },
            MenuItem { id: 5, icon_name: Some(dropped_icon.clone()), ..MenuItem::default() },
            MenuItem { id: 6, ..MenuItem::default() },
        ];
        enforce_menu_budget(item_id_str, &mut new_menu, remaining_for_new);

        assert_eq!(count_menu_nodes(&new_menu), remaining_for_new);
        assert!(std::path::Path::new(&kept_icon).exists(), "kept node keeps its spooled icon");
        assert!(!std::path::Path::new(&dropped_icon).exists(), "truncated node's spooled icon is reaped");

        let mut single_cap = vec![MenuItem::default(); MAX_MENU_NODES + 5];
        enforce_menu_budget(item_id_str, &mut single_cap, usize::MAX);
        assert_eq!(count_menu_nodes(&single_cap), MAX_MENU_NODES - 1);
    }

    #[test]
    fn record_at_cap_deduplicates_same_path_allows_second_path_and_caps_queue_at_max_waiting() {
        let mut reg = Registry::default();
        let first = resolved(":1.1", "/A");
        record_at_cap(&mut reg, &first, true);
        record_at_cap(&mut reg, &first, true);
        assert_eq!(reg.waiting.len(), 1, "identical (unique, path) must deduplicate");

        record_at_cap(&mut reg, &resolved(":1.1", "/B"), true);
        assert_eq!(reg.waiting.len(), 2, "distinct object path on the same peer must also queue");

        for i in 2..(MAX_WAITING_ITEMS + 10) {
            record_at_cap(&mut reg, &resolved(&format!(":1.{i}"), "/StatusNotifierItem"), true);
        }
        assert_eq!(reg.waiting.len(), MAX_WAITING_ITEMS, "waiting queue must cap at MAX_WAITING_ITEMS");
    }

    #[tokio::test]
    async fn deferred_startup_adoption_runs_after_a_tracked_item_leaves() {
        use super::super::watcher::tests::StubStatusNotifierItem;
        use crate::capabilities::test_support::{poll_until, private_bus};

        let bus = private_bus().await;
        let host = bus.connection().await;
        let dbus = zbus::fdo::DBusProxy::new(&host).await.unwrap();
        let departures = crate::capabilities::name_departures(&dbus, "test").await.unwrap();

        let existing =
            bus.builder().serve_at("/StatusNotifierItem", StubStatusNotifierItem).unwrap().build().await.unwrap();
        existing.request_name("org.freedesktop.StatusNotifierItem-9999-1").await.unwrap();
        let existing_unique = existing.unique_name().unwrap().to_owned();

        let tracked_app = bus.connection().await;
        let tracked_unique = tracked_app.unique_name().unwrap().to_owned();

        let registry = ItemRegistry::default();
        {
            let tracked_entry = entry(&host, &item_id(tracked_unique.as_str(), "/StatusNotifierItem"), 0).await;
            let mut fillers = Vec::with_capacity(MAX_ITEMS - 1);
            for idx in 1..MAX_ITEMS {
                fillers.push((
                    key(&format!(":1.8{idx}")),
                    entry(&host, &format!("1.8{idx}/StatusNotifierItem"), idx as u64).await,
                ));
            }
            let mut guard = registry.lock().unwrap();
            guard.items.insert(key(tracked_unique.as_str()), tracked_entry);
            for (k, v) in fillers {
                guard.items.insert(k, v);
            }
        }
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();

        super::super::controller::adopt_existing_items(&host, &dbus, &registry, &events).await;
        assert!(registry.lock().unwrap().adoption_deferred, "adoption at MAX_ITEMS must set adoption_deferred");
        assert!(
            !registry
                .lock()
                .unwrap()
                .items
                .contains_key(&(existing_unique.clone(), OwnedObjectPath::try_from("/StatusNotifierItem").unwrap(),))
        );

        spawn_name_owner_changed_forwarder(host.clone(), departures, registry.clone(), events);
        drop(tracked_app);

        poll_until(|| {
            let guard = registry.lock().unwrap();
            (!guard.adoption_deferred
                && guard.items.contains_key(&(
                    existing_unique.clone(),
                    OwnedObjectPath::try_from("/StatusNotifierItem").unwrap(),
                )))
            .then_some(())
        })
        .await;
        assert_eq!(registry.lock().unwrap().items.len(), MAX_ITEMS);
    }
}

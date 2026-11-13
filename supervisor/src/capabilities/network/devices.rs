//! NetworkManager devices for `mantle.network`: resolving the Wi-Fi and wired devices, and the
//! forwarder tasks that turn their signals, and the manager's, into [`NetworkSignal`]s.

use std::time::Duration;

use futures_util::{FutureExt, Stream, StreamExt, stream, stream_select};
use tokio::sync::mpsc::UnboundedSender;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

use super::proxies::{
    AccessPointProxy, DEVICE_TYPE_ETHERNET, DEVICE_TYPE_WIFI, DeviceProxy, NetworkManagerProxy, WiredProxy,
    WirelessProxy,
};
use super::{NetworkController, NetworkSignal};
use crate::capabilities::{bind, forward_until_closed};
use shared::{debug, error, warn};

/// One resolved Wi-Fi device: its path for `AddAndActivateConnection2`, plus the `Device` and
/// `Device.Wireless` proxies. Association state and APs use those two interfaces on one object.
#[derive(Clone)]
pub(super) struct WifiDevice {
    pub(super) id: String,
    pub(super) device_path: OwnedObjectPath,
    pub(super) device: DeviceProxy<'static>,
    pub(super) wireless: WirelessProxy<'static>,
}

/// One resolved Ethernet device. Its path feeds `ActivateConnection`, its `Device` proxy feeds
/// `ethernet_enabled` and the address, and its `Device.Wired` proxy feeds the link speed.
#[derive(Clone)]
pub(super) struct EthernetDevice {
    pub(super) path: OwnedObjectPath,
    pub(super) device: DeviceProxy<'static>,
    pub(super) wired: WiredProxy<'static>,
}

/// The Wi-Fi and wired devices NetworkManager has now, with the tasks watching them. Replaced whole
/// on [`NetworkSignal::DevicesChanged`]; callers clone devices out, so no await holds the lock.
#[derive(Default)]
pub(super) struct Devices {
    pub(super) wifi: Vec<WifiDevice>,
    pub(super) ethernet: Vec<EthernetDevice>,
    /// Aborted when the set is replaced, so a removed device's watchers stop with it.
    pub(super) watchers: Vec<tokio::task::AbortHandle>,
}

/// Binds every Wi-Fi and wired device from `GetAllDevices`. Unreadable devices are logged and
/// skipped.
pub(super) async fn resolve_devices(
    connection: &zbus::Connection,
    nm: &NetworkManagerProxy<'static>,
) -> zbus::Result<(Vec<WifiDevice>, Vec<EthernetDevice>)> {
    let mut wifi = Vec::new();
    let mut ethernet = Vec::new();
    for path in nm.get_all_devices().await? {
        // Probe uncached: a cached proxy's first read triggers `GetAll`, match rules and
        // `GetNameOwner`, paid for every unrelated device on each `DevicesChanged`.
        let probe = match DeviceProxy::builder(connection).path(path.clone()) {
            Ok(builder) => builder.cache_properties(CacheProperties::No).build().await,
            Err(err) => Err(err),
        };
        let probe = match probe {
            Ok(probe) => probe,
            Err(err) => {
                debug!("failed to bind device {path}: {err}");
                continue;
            }
        };
        let device_type = match probe.device_type().await {
            Ok(device_type) => device_type,
            Err(err) => {
                debug!("failed to read device_type for {path}: {err}");
                continue;
            }
        };
        match device_type {
            DEVICE_TYPE_ETHERNET => {
                match tokio::try_join!(
                    bind::<DeviceProxy>(connection, path.clone()),
                    bind::<WiredProxy>(connection, path.clone()),
                ) {
                    Ok((device, wired)) => ethernet.push(EthernetDevice { path, device, wired }),
                    Err(err) => debug!("failed to bind wired device {path}: {err}"),
                }
            }
            DEVICE_TYPE_WIFI => {
                let id = match probe.interface().await {
                    Ok(id) => id,
                    Err(err) => {
                        debug!("failed to read interface for {path}: {err}");
                        continue;
                    }
                };
                if id.is_empty() || wifi.iter().any(|other: &WifiDevice| other.id == id) {
                    warn!("skipping Wi-Fi device {path} with empty or duplicate interface {id:?}");
                    continue;
                }
                match tokio::try_join!(
                    bind::<DeviceProxy>(connection, path.clone()),
                    bind::<WirelessProxy>(connection, path.clone()),
                ) {
                    Ok((device, wireless)) => wifi.push(WifiDevice { id, device_path: path, device, wireless }),
                    Err(err) => debug!("failed to bind wireless device {path}: {err}"),
                }
            }
            _ => {}
        }
    }
    Ok((wifi, ethernet))
}

/// Starts the per-device watchers for one device set and returns their abort handles.
pub(super) fn watch_devices(
    connection: &zbus::Connection,
    wifi: &[WifiDevice],
    ethernet: &[EthernetDevice],
    events: &UnboundedSender<NetworkSignal>,
) -> Vec<tokio::task::AbortHandle> {
    let mut watchers = Vec::new();
    for wifi in wifi {
        watchers.push(
            spawn_wifi_forwarder(connection.clone(), wifi.id.clone(), wifi.wireless.clone(), events.clone())
                .abort_handle(),
        );
        watchers.push(spawn_device_state_forwarder(wifi.device.clone(), events.clone()).abort_handle());
    }
    if wifi.is_empty() {
        debug!("no Wi-Fi device found; scan and access-point events wait for one to appear");
    }
    for device in ethernet {
        watchers.push(spawn_device_state_forwarder(device.device.clone(), events.clone()).abort_handle());
    }
    watchers
}

/// Forwards `org.freedesktop.NetworkManager.Device.Wireless` events to `events` until the
/// connection drops. Spawned once by [`NetworkController::new`] so borrow-heavy streams stay out
/// of `main.rs`'s top-level `select!`; a dropped receiver ends it on the next send.
///
/// `ActiveAccessPoint` is the only watched property that moves when the radio joins or leaves.
/// Watching only the AP set and `LastScan` left `connected` frozen, so an association after bar
/// startup read offline until the next scan, minutes later.
///
/// It owns the associated AP's strength watch, retargeted and aborted with each association.
/// Otherwise an orphan would rebuild for an AP nothing is connected to (ADR-0082).
fn spawn_wifi_forwarder(
    connection: zbus::Connection,
    id: String,
    wireless: WirelessProxy<'static>,
    events: UnboundedSender<NetworkSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ap_set = tokio::try_join!(wireless.receive_access_point_added(), wireless.receive_access_point_removed());
        let mut ap_set_changed = match ap_set {
            Ok((added, removed)) => stream::select(added.map(drop), removed.map(drop)),
            Err(err) => {
                warn!("failed to subscribe to AccessPointAdded/AccessPointRemoved: {err}");
                return;
            }
        };
        let mut last_scan_changed = wireless.receive_last_scan_changed().await;
        let mut active_ap_changed = wireless.receive_active_access_point_changed().await;
        // The first `active_ap_changed` emission fills the property cache, so an existing
        // association is watched without a startup read. A `JoinSet` aborts its task when dropped,
        // so aborting this watcher on a device rescan stops the strength watch too.
        let mut strength = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                Some(()) = ap_set_changed.next() => {
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(change) = active_ap_changed.next() => {
                    strength.shutdown().await;
                    if let Ok(path) = change.get().await {
                        strength.extend(strength_forwarder(&connection, path, events.clone()));
                    }
                    if events.send(NetworkSignal::Changed).is_err() { break; }
                }
                Some(_) = last_scan_changed.next() => {
                    if events.send(NetworkSignal::ScanCompleted(id.clone())).is_err() { break; }
                }
                else => break,
            }
        }
    })
}

/// Forwards the associated AP's `Strength` as [`NetworkSignal::Changed`], keeping bars current
/// between scans. `None` for NetworkManager's `/` path means no association.
///
/// Watch only the associated AP. On real hardware over 180s it emitted 26 times, a quiet 6-second
/// poll, versus 76 events across 17 APs, one every 2.4s indefinitely. The others are read after
/// each scan, which is when NetworkManager moves them; ADR-0029 item 6 required this measured
/// choice before adding debounce.
fn strength_forwarder(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
    events: UnboundedSender<NetworkSignal>,
) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
    if path.as_str() == "/" {
        return None;
    }
    let connection = connection.clone();
    Some(async move {
        let access_point = match bind::<AccessPointProxy>(&connection, path.clone()).await {
            Ok(access_point) => access_point,
            Err(err) => {
                debug!("failed to bind the associated access point {path}: {err}");
                return;
            }
        };
        forward_throttled(access_point.receive_strength_changed().await, &events, STRENGTH_INTERVAL).await;
    })
}

/// Longest a burst of signal-strength changes holds off a rebuild; each one costs per-device D-Bus reads.
const STRENGTH_INTERVAL: Duration = Duration::from_secs(3);

/// Sends [`NetworkSignal::Changed`] for the first item, then at most once per `interval`, with a
/// trailing send when items arrived meanwhile, so the last value always lands.
async fn forward_throttled<T>(
    mut items: impl Stream<Item = T> + Unpin,
    events: &UnboundedSender<NetworkSignal>,
    interval: Duration,
) {
    while items.next().await.is_some() {
        loop {
            if events.send(NetworkSignal::Changed).is_err() {
                return;
            }
            tokio::time::sleep(interval).await;
            let mut more = false;
            while items.next().now_or_never().flatten().is_some() {
                more = true;
            }
            if !more {
                break;
            }
        }
    }
}

/// Forwards each device's `State` as [`NetworkSignal::Changed`]. Per-device tasks cover
/// `ethernet_enabled` and announce Wi-Fi disconnects before `ActiveAccessPoint` catches up.
/// zbus emits the cached current value once, priming the first snapshot without a startup read.
fn spawn_device_state_forwarder(
    device: DeviceProxy<'static>,
    events: UnboundedSender<NetworkSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        forward_until_closed(device.receive_state_changed().await, events, NetworkSignal::Changed).await;
    })
}

/// Forwards manager-wide properties used by `NetworkState`: radio switches and the default route.
/// Device tasks cannot cover them; networking can switch off while devices stay put, and the route
/// can move between two activated devices.
pub(super) fn spawn_manager_forwarder(nm: NetworkManagerProxy<'static>, events: UnboundedSender<NetworkSignal>) {
    tokio::spawn(async move {
        // `stream_select!` can re-poll a stream that already ended, so each input is fused.
        let changes = stream_select!(
            nm.receive_wireless_enabled_changed().await.map(drop).fuse(),
            nm.receive_networking_enabled_changed().await.map(drop).fuse(),
            nm.receive_primary_connection_changed().await.map(drop).fuse(),
            nm.receive_active_connections_changed().await.map(drop).fuse()
        );
        forward_until_closed(changes, events, NetworkSignal::Changed).await;
    });
}

/// Sends `signal` for each item of an added/removed subscription pair, such as `DeviceAdded` and
/// `DeviceRemoved`, until both end or the receiver does. A failed subscription is logged: changes
/// it would have reported need a restart.
pub(super) fn forward<A, B>(
    subscribed: zbus::Result<(A, B)>,
    signal: NetworkSignal,
    events: UnboundedSender<NetworkSignal>,
) where
    A: Stream + Unpin + Send + 'static,
    B: Stream + Unpin + Send + 'static,
{
    let (added, removed) = match subscribed {
        Ok(streams) => streams,
        Err(err) => {
            return error!("failed to subscribe for {signal:?}; those changes need a restart: {err}");
        }
    };
    tokio::spawn(forward_until_closed(stream::select(added.map(drop), removed.map(drop)), events, signal));
}

impl NetworkController {
    /// Rescans devices and restarts their watchers, new before old so no change falls in a gap.
    /// Skips the restart when the Wi-Fi and wired paths are unchanged, as for most veth, bridge and
    /// VPN additions. `restarted` rebinds anyway: the old proxies cache a dead daemon's properties.
    pub(super) async fn refresh_devices(&self, restarted: bool) {
        let (wifi, ethernet) = match resolve_devices(&self.connection, &self.nm).await {
            Ok(found) => found,
            Err(err) => {
                warn!("failed to rescan devices: {err}");
                return;
            }
        };
        let unchanged = {
            let current = self.devices.lock().expect("mutex poisoned");
            current.wifi.iter().map(|wifi| &wifi.device_path).eq(wifi.iter().map(|wifi| &wifi.device_path))
                && current.ethernet.iter().map(|device| &device.path).eq(ethernet.iter().map(|device| &device.path))
        };
        if unchanged && !restarted {
            return;
        }
        debug!("device set changed: wifi={} ethernet={}", wifi.len(), ethernet.len());
        let watchers = watch_devices(&self.connection, &wifi, &ethernet, &self.events);
        let live: Vec<String> = wifi.iter().map(|device| device.id.clone()).collect();
        let previous =
            std::mem::replace(&mut *self.devices.lock().expect("mutex poisoned"), Devices { wifi, ethernet, watchers });
        self.access_points.lock().expect("mutex poisoned").retain(|id, _| live.contains(id));
        for watcher in previous.watchers {
            watcher.abort();
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::capabilities::test_support::{PrivateBus, p2p_pair_serving, private_bus, within};

    struct FakeNm;

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl FakeNm {
        #[zbus(property)]
        fn wireless_enabled(&self) -> bool {
            true
        }
    }

    pub(crate) async fn method_call_monitor(bus: &PrivateBus) -> zbus::MessageStream {
        let monitor = bus.connection().await;
        let dbus = "org.freedesktop.DBus";
        monitor
            .call_method(
                Some(dbus),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Monitoring"),
                "BecomeMonitor",
                &(&["type='method_call'"][..], 0u32),
            )
            .await
            .unwrap();
        zbus::MessageStream::from(monitor)
    }

    pub(crate) async fn collect_method_calls(
        monitor: &mut zbus::MessageStream,
        barrier: &zbus::Connection,
    ) -> HashMap<String, usize> {
        tokio::task::yield_now().await;
        zbus::fdo::DBusProxy::new(barrier).await.unwrap().get_id().await.unwrap();
        let mut counts = HashMap::new();
        within(async {
            while let Some(Ok(message)) = monitor.next().await {
                let header = message.header();
                if header.message_type() != zbus::message::Type::MethodCall {
                    continue;
                }
                let member = header.member().map(|m| m.as_str().to_owned()).unwrap_or_default();
                if member == "GetId" && header.path().map(|p| p.as_str()) == Some("/org/freedesktop/DBus") {
                    break;
                }
                *counts.entry(member).or_insert(0) += 1;
            }
        })
        .await;
        counts
    }

    struct FakeNmDevices(std::sync::Mutex<Vec<OwnedObjectPath>>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl FakeNmDevices {
        #[zbus(property)]
        fn wireless_enabled(&self) -> bool {
            true
        }

        fn get_all_devices(&self) -> Vec<OwnedObjectPath> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeWireless;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
    impl FakeWireless {
        #[zbus(property)]
        fn active_access_point(&self) -> OwnedObjectPath {
            crate::capabilities::network::root_object_path().into()
        }

        #[zbus(property)]
        fn last_scan(&self) -> i64 {
            1
        }
    }

    struct FakeDevice(u32, String, AtomicUsize, Arc<AtomicUsize>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl FakeDevice {
        #[zbus(property)]
        fn device_type(&self) -> u32 {
            self.0
        }

        #[zbus(property)]
        fn interface(&self) -> String {
            self.1.clone()
        }

        #[zbus(property)]
        fn state(&self) -> u32 {
            self.3.fetch_add(1, Ordering::SeqCst);
            self.2.load(Ordering::SeqCst) as u32
        }
    }

    fn dev_path(name: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(format!("/org/freedesktop/NetworkManager/Devices/{name}")).unwrap()
    }

    async fn drain_signals(
        signals: &mut tokio::sync::mpsc::UnboundedReceiver<NetworkSignal>,
        mut changed: usize,
        mut scans: usize,
    ) {
        while changed > 0 || scans > 0 {
            match within(signals.recv()).await {
                Some(NetworkSignal::Changed) if changed > 0 => changed -= 1,
                Some(NetworkSignal::ScanCompleted(_)) if scans > 0 => scans -= 1,
                other => panic!("unexpected signal while draining: {other:?}"),
            }
        }
    }

    async fn serve_devices(
        bus: &PrivateBus,
        specs: &[(&str, u32, &str)],
    ) -> (zbus::Connection, Vec<OwnedObjectPath>, Vec<Arc<AtomicUsize>>) {
        let paths: Vec<_> = specs.iter().map(|(name, ..)| dev_path(name)).collect();
        let nm_devices = FakeNmDevices(std::sync::Mutex::new(paths.clone()));
        let mut builder = bus.builder().serve_at("/org/freedesktop/NetworkManager", nm_devices).unwrap();
        let mut reads = Vec::new();
        for ((name, kind, iface), path) in specs.iter().zip(&paths) {
            let count = Arc::new(AtomicUsize::new(0));
            reads.push(Arc::clone(&count));
            if *name != "gone" {
                builder = builder
                    .serve_at(path.clone(), FakeDevice(*kind, (*iface).into(), AtomicUsize::new(30), count))
                    .unwrap();
                if *kind == DEVICE_TYPE_WIFI {
                    builder = builder.serve_at(path.clone(), FakeWireless).unwrap();
                }
            }
        }
        let nm_conn = builder.name("org.freedesktop.NetworkManager").unwrap().build().await.unwrap();
        (nm_conn, paths, reads)
    }

    #[tokio::test]
    async fn resolve_devices_probes_uncached_preserves_order_and_defers_retained_cache() {
        let bus = private_bus().await;
        let specs = [
            ("veth0", 14, "v0"),
            ("eth0", DEVICE_TYPE_ETHERNET, "eth0"),
            ("wlan_empty", DEVICE_TYPE_WIFI, ""),
            ("wlan0", DEVICE_TYPE_WIFI, "wlan0"),
            ("wlan_dup", DEVICE_TYPE_WIFI, "wlan0"),
            ("gone", DEVICE_TYPE_ETHERNET, "gone"),
            ("eth1", DEVICE_TYPE_ETHERNET, "eth1"),
            ("veth1", 32, "lo"),
        ];
        let (nm_conn, paths, reads) = serve_devices(&bus, &specs).await;
        let conn = bus.connection().await;
        let nm = NetworkManagerProxy::new(&conn).await.unwrap();
        let mut monitor = method_call_monitor(&bus).await;

        let (wifi, ethernet) = resolve_devices(&conn, &nm).await.unwrap();
        let calls = collect_method_calls(&mut monitor, &conn).await;

        assert_eq!(wifi.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["wlan0"]);
        assert_eq!(
            ethernet.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(),
            [paths[1].as_str(), paths[6].as_str()]
        );
        assert_eq!(calls.get("GetAll").copied().unwrap_or(0), 0, "{calls:?}");
        assert_eq!(calls.get("AddMatch").copied().unwrap_or(0), 0, "{calls:?}");
        for count in &reads {
            assert_eq!(count.load(Ordering::SeqCst), 0, "lazy proxy must not read State before use");
        }

        // Watching retained devices initializes each retained proxy once and delivers updates.
        let (events, mut signals) = tokio::sync::mpsc::unbounded_channel();
        let _watchers = watch_devices(&conn, &wifi, &ethernet, &events);
        drain_signals(&mut signals, 4, 1).await;
        let watch_calls = collect_method_calls(&mut monitor, &conn).await;
        assert_eq!(watch_calls.get("GetAll").copied().unwrap_or(0), 4, "{watch_calls:?}");
        for idx in [0, 2, 4, 5, 7] {
            assert_eq!(reads[idx].load(Ordering::SeqCst), 0);
        }
        for idx in [1, 3, 6] {
            assert_eq!(reads[idx].load(Ordering::SeqCst), 1);
        }

        let iface = nm_conn.object_server().interface::<_, FakeDevice>(paths[1].clone()).await.unwrap();
        iface.get().await.2.store(100, Ordering::SeqCst);
        iface.get().await.state_changed(iface.signal_emitter()).await.unwrap();
        assert!(matches!(within(signals.recv()).await, Some(NetworkSignal::Changed)));
        assert_eq!(ethernet[0].device.state().await.unwrap(), 100);
    }

    #[tokio::test]
    async fn devices_changed_on_unrelated_device_skips_getall_and_match_rules() {
        let bus = private_bus().await;
        let specs = [
            ("eth0", DEVICE_TYPE_ETHERNET, "eth0"),
            ("wlan0", DEVICE_TYPE_WIFI, "wlan0"),
            ("veth0", 14, "v0"),
            ("veth1", 14, "v1"),
        ];
        let (nm_conn, paths, reads) = serve_devices(&bus, &specs[..3]).await;
        nm_conn
            .object_server()
            .at(dev_path("veth1"), FakeDevice(14, "v1".into(), AtomicUsize::new(0), Arc::default()))
            .await
            .unwrap();
        let conn = bus.connection().await;
        let (events, mut signals) = tokio::sync::mpsc::unbounded_channel();
        let controller = NetworkController::new(conn.clone(), events).await.unwrap();
        drain_signals(&mut signals, 4, 1).await;
        let mut monitor = method_call_monitor(&bus).await;
        let _ = collect_method_calls(&mut monitor, &conn).await;

        let nm_iface =
            nm_conn.object_server().interface::<_, FakeNmDevices>("/org/freedesktop/NetworkManager").await.unwrap();
        *nm_iface.get().await.0.lock().unwrap() = vec![paths[0].clone(), paths[1].clone(), dev_path("veth1")];
        controller.refresh_devices(false).await;
        let calls = collect_method_calls(&mut monitor, &conn).await;

        assert_eq!(calls.get("GetAll").copied().unwrap_or(0), 0, "{calls:?}");
        assert_eq!(calls.get("AddMatch").copied().unwrap_or(0), 0, "{calls:?}");
        assert_eq!(reads[0].load(Ordering::SeqCst), 1);
        assert_eq!(reads[1].load(Ordering::SeqCst), 1);
        assert_eq!(reads[2].load(Ordering::SeqCst), 0);
        assert_eq!((controller.ethernet().len(), controller.devices.lock().unwrap().wifi.len()), (1, 1));
    }

    /// zbus primes a property stream with the cached value, so an answering manager reports once.
    #[tokio::test]
    async fn the_manager_forwarder_reports_networkmanagers_properties() {
        let (connection, _nm) = p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/NetworkManager", FakeNm)).await;
        let (events, mut signals) = tokio::sync::mpsc::unbounded_channel();
        spawn_manager_forwarder(NetworkManagerProxy::new(&connection).await.unwrap(), events);
        assert!(matches!(within(signals.recv()).await, Some(NetworkSignal::Changed)));
    }

    #[tokio::test]
    async fn each_added_or_removed_item_sends_the_signal() {
        let (events, mut signals) = tokio::sync::mpsc::unbounded_channel();
        // Pending, not ended: a forwarder that spins on an ended stream would hang the test.
        let one = || stream::iter([()]).chain(stream::pending());
        forward(Ok((one(), one())), NetworkSignal::DevicesChanged, events);
        for _ in 0..2 {
            assert!(matches!(within(signals.recv()).await, Some(NetworkSignal::DevicesChanged)));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_strength_changes_rebuilds_twice_and_the_last_one_lands() {
        let (items, rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
        let (events, mut rebuilds) = tokio::sync::mpsc::unbounded_channel();
        let stream = stream::unfold(rx, |mut rx| async { rx.recv().await.map(|item| (item, rx)) }).boxed();
        let task = tokio::spawn(async move { forward_throttled(stream, &events, Duration::from_secs(3)).await });
        items.send(0).unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        for strength in 1..50 {
            items.send(strength).unwrap();
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(items);
        task.await.unwrap();
        let mut count = 0;
        while rebuilds.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 2, "one leading rebuild and one trailing rebuild for the whole burst");
    }
}

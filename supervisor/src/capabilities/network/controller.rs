//! [`NetworkController`]: `mantle.network`'s proxies and push state, rebuilt from NetworkManager on
//! every signal, and the networking, radio and wired switches.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use shared::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::OwnedObjectPath;

use super::devices::{
    Devices, EthernetDevice, WifiDevice, forward, resolve_devices, spawn_manager_forwarder, watch_devices,
};
use super::join::JoinState;
use super::proxies::{DEVICE_STATE_ACTIVATED, DeviceProxy, IP4ConfigProxy, NetworkManagerProxy, SettingsProxy};
use super::{NetworkSignal, NetworkState};
use crate::capabilities::bind_uncached;
use shared::state::network::WifiDeviceInfo;
use shared::state::network_profiles::NetworkProfilesState;

/// Shared network proxies and state. Clones can move into spawned writes.
#[derive(Clone)]
pub struct NetworkController {
    pub(super) connection: zbus::Connection,
    pub(super) nm: NetworkManagerProxy<'static>,
    pub(super) settings: SettingsProxy<'static>,
    /// The devices NetworkManager has now; see [`Devices`].
    pub(super) devices: Arc<Mutex<Devices>>,
    /// AP readings retained between scans, pruned against live paths.
    pub(super) access_points: Arc<Mutex<HashMap<String, HashMap<OwnedObjectPath, super::scan::ApReading>>>>,
    /// Saved Wi-Fi SSIDs for [`AccessPointInfo::saved`](super::AccessPointInfo::saved), refreshed on
    /// [`NetworkSignal::SavedChanged`] and [`NetworkSignal::ProfileUpdated`].
    pub(super) saved_ssids: Arc<Mutex<HashMap<OwnedObjectPath, Vec<u8>>>>,
    /// Push state, mutated by [`handle_signal`](Self::handle_signal).
    pub(super) state: Arc<Mutex<NetworkState>>,
    /// The pending intent, prompt and accepted attempt share one lock.
    pub(super) join: Arc<Mutex<JoinState>>,
    /// The last VPN failure, the pending secret request and `connect_vpn`'s waits.
    pub(super) vpn: Arc<Mutex<super::vpn::VpnState>>,
    /// Saved profiles for `network_profiles`, their active states and the last `_profile` failure.
    pub(super) profiles: Arc<Mutex<super::profiles::Profiles>>,
    /// Set by the active-connection forwarders: the next rebuild re-reads the active states.
    pub(super) activity: Arc<AtomicBool>,
    /// NetworkManager's unique name on the bus, `None` off-bus.
    pub(super) nm_owner: Arc<Mutex<Option<OwnedUniqueName>>>,
    /// The last queued `_profile` action's write; see `queue_profile_action`.
    pub(super) profile_writes: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    /// `network_profiles` pushes, sent apart from the rebuilds `handle_signal` returns.
    pub(super) profile_states: UnboundedSender<NetworkProfilesState>,
    /// Signal sender, including scan's FIFO event.
    pub(super) events: UnboundedSender<NetworkSignal>,
}

fn carry_scan(next: &mut NetworkState, previous: Option<&NetworkState>, signal: &NetworkSignal) {
    for device in &mut next.wifi_devices {
        // `None`: `next` is the stored state itself, so its own flag is the previous one.
        let was_scanning = previous.map_or(device.scanning, |previous| {
            previous.wifi_devices.iter().find(|old| old.id == device.id).is_some_and(|old| old.scanning)
        });
        device.scanning = matches!(signal, NetworkSignal::ScanStarted(id) if id == &device.id)
            || (was_scanning && !matches!(signal, NetworkSignal::ScanCompleted(id) if id == &device.id));
    }
}

/// `device`'s first IPv4 address without its prefix, read uncached because `Ip4Config`'s path
/// changes per activation. ponytail: a DHCP renewal without a state change shows the old address
/// until the next rebuild. Upgrade path: watch `AddressData`.
async fn read_ipv4(connection: &zbus::Connection, device: Option<&DeviceProxy<'static>>) -> Option<String> {
    let path = device?.ip4_config().await.ok().filter(|path| path.as_str() != "/")?;
    let config = bind_uncached::<IP4ConfigProxy>(connection, path).await.ok()?;
    let addresses = config.address_data().await.ok()?;
    String::try_from(addresses.first()?.get("address")?.clone()).ok()
}

fn primary_wifi_index(
    route: Option<&OwnedObjectPath>,
    links: &[(Option<OwnedObjectPath>, Option<u32>)],
) -> Option<usize> {
    links
        .iter()
        .enumerate()
        .max_by_key(|(index, (active, state))| {
            let route = route.is_some_and(|route| route.as_str() != "/" && Some(route) == active.as_ref());
            (u8::from(route) * 2 + u8::from(*state == Some(DEVICE_STATE_ACTIVATED)), std::cmp::Reverse(*index))
        })
        .map(|(index, _)| index)
}

fn display_ssid(wired: bool, primary: Option<&WifiDeviceInfo>) -> Option<String> {
    if wired { Some("Ethernet".into()) } else { primary.and_then(|device| device.ssid.clone()) }
}

fn promote_primary(devices: &mut [WifiDeviceInfo], index: usize) {
    devices[..=index].rotate_right(1);
}

impl NetworkController {
    /// Resolves devices and starts their event forwarders.
    pub async fn new(
        connection: zbus::Connection,
        events: UnboundedSender<NetworkSignal>,
        profile_states: UnboundedSender<NetworkProfilesState>,
    ) -> zbus::Result<Self> {
        let nm = NetworkManagerProxy::new(&connection).await?;
        let settings = SettingsProxy::new(&connection).await?;

        // Subscribed before the first device scan, so an adapter plugged in during it is not missed.
        let device_list = tokio::try_join!(nm.receive_device_added(), nm.receive_device_removed());
        forward(device_list, NetworkSignal::DevicesChanged, events.clone());
        let (wifi, ethernet) = resolve_devices(&connection, &nm).await?;
        let watchers = watch_devices(&connection, &wifi, &ethernet, &events);
        let activity = Arc::default();
        spawn_manager_forwarder(nm.clone(), events.clone(), Arc::clone(&activity));
        // Subscribed before the first fill below, so a profile saved in between is not missed.
        let profiles = tokio::try_join!(settings.receive_new_connection(), settings.receive_connection_removed());
        forward(profiles, NetworkSignal::SavedChanged, events.clone());
        let vpn = Arc::<Mutex<super::vpn::VpnState>>::default();
        super::secret_agent::export(&connection, Arc::clone(&vpn), events.clone()).await;
        let nm_owner = Arc::default();
        super::vpn::forward_active_changes(&connection, Arc::clone(&nm_owner), events.clone(), Arc::clone(&activity))
            .await;
        let updated = "type='signal',sender='org.freedesktop.NetworkManager',interface='org.freedesktop.NetworkManager.Settings.Connection',member='Updated'";
        super::vpn::forward_nm_signal(&connection, Arc::clone(&nm_owner), updated, events.clone(), |message| {
            Some(NetworkSignal::ProfileUpdated(message.header().path()?.to_owned().into()))
        })
        .await;
        let mut owners = settings.inner().receive_owner_changed().await?;
        // Read after subscribing, so a change in between lands in `owners` and overwrites it.
        let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
        *nm_owner.lock().expect("mutex poisoned") =
            dbus.get_name_owner(settings.inner().destination().clone()).await.ok();
        let (owner_events, owner_vpn, owner_name) = (events.clone(), Arc::clone(&vpn), Arc::clone(&nm_owner));
        let owner_connection = connection.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            super::secret_agent::register(&owner_connection).await;
            // ponytail: `nm`'s cached manager properties outlive a restart until NM changes them; upgrade: rebuild `nm`.
            while let Some(owner) = owners.next().await
                && owner_events.send(NetworkSignal::SavedChanged).is_ok()
            {
                *owner_name.lock().expect("mutex poisoned") = owner.clone().map(Into::into);
                // Its secret request died with the NetworkManager that asked; a prompt would block the next.
                super::secret_agent::drop_request(&owner_vpn, &owner_events, false, |_| true);
                // A restarted NetworkManager forgot the agent. With no owner, a call would D-Bus-activate it.
                if owner.is_some() {
                    let _ = owner_events.send(NetworkSignal::Restarted);
                    super::secret_agent::register(&owner_connection).await;
                }
            }
        });

        let controller = Self {
            connection,
            nm,
            settings,
            devices: Arc::new(Mutex::new(Devices { wifi, ethernet, watchers })),
            access_points: Arc::new(Mutex::new(HashMap::new())),
            saved_ssids: Arc::default(),
            state: Arc::new(Mutex::new(NetworkState::default())),
            join: Arc::default(),
            vpn,
            profiles: Arc::default(),
            profile_writes: Arc::default(),
            nm_owner,
            activity,
            profile_states,
            events,
        };
        controller.refresh_profiles().await;
        // Pushed now, so a config reading only `network_profiles` waits on no device signal.
        controller.read_profile_activity().await;
        controller.push_profiles();
        Ok(controller)
    }

    /// Rebuilds and returns push state for one signal.
    pub async fn handle_signal(&self, signal: NetworkSignal) -> NetworkState {
        let mut next = match &signal {
            NetworkSignal::ScanStarted(_) => {
                let mut next = self.state.lock().expect("mutex poisoned").clone();
                if next.wifi_devices.is_empty() {
                    next = self.build_state(&signal).await;
                    carry_scan(&mut next, Some(&NetworkState::default()), &signal);
                } else {
                    carry_scan(&mut next, None, &signal);
                }
                next
            }
            _ => {
                let mut profiles_changed =
                    matches!(signal, NetworkSignal::SavedChanged | NetworkSignal::ProfileUpdated(_));
                match &signal {
                    NetworkSignal::SavedChanged => self.refresh_profiles().await,
                    NetworkSignal::ProfileUpdated(path) => self.refresh_profile(path.clone()).await,
                    NetworkSignal::DevicesChanged | NetworkSignal::Restarted => {
                        self.refresh_devices(signal == NetworkSignal::Restarted).await;
                        if let Some(pending) = self.pending_intent()
                            && self.pending_wifi(&pending).is_none()
                        {
                            self.resolve_connect_intent().await;
                        }
                    }
                    _ => {}
                }
                // A restarted NetworkManager's activations were never announced to us.
                if self.activity.swap(false, Ordering::Relaxed) || signal == NetworkSignal::Restarted {
                    self.read_profile_activity().await;
                    profiles_changed = true;
                }
                if profiles_changed {
                    self.push_profiles();
                }
                // Read D-Bus before taking the plain mutex; never hold it across an await.
                let mut next = self.build_state(&signal).await;
                carry_scan(&mut next, Some(&self.state.lock().expect("mutex poisoned")), &signal);
                next
            }
        };
        self.join.lock().expect("mutex poisoned").overlay(&mut next);
        self.vpn.lock().expect("mutex poisoned").overlay(&mut next);
        *self.state.lock().expect("mutex poisoned") = next.clone();
        next
    }

    /// Reads NM state, defaulting fields whose properties fail.
    async fn build_state(&self, signal: &NetworkSignal) -> NetworkState {
        // `PrimaryConnection` is `/` without a default route.
        let route = self.nm.primary_connection().await.ok();
        let connected = route.as_ref().is_some_and(|path| path.as_str() != "/");
        let wired = connected && self.nm.primary_connection_type().await.is_ok_and(|kind| kind == "802-3-ethernet");
        let wifi = self.devices.lock().expect("mutex poisoned").wifi.clone();
        let mut wifi_devices = Vec::with_capacity(wifi.len());
        let mut links = Vec::with_capacity(wifi.len());
        for device in &wifi {
            let active = device.device.active_connection().await.ok();
            let state = device.device.state().await.ok();
            links.push((active, state));
            let available_networks = self
                .build_available_networks(
                    device,
                    matches!(signal, NetworkSignal::ScanCompleted(id) if id == &device.id),
                )
                .await;
            let associated = available_networks.iter().find(|ap| ap.active);
            wifi_devices.push(WifiDeviceInfo {
                id: device.id.clone(),
                connected: state == Some(DEVICE_STATE_ACTIVATED),
                ssid: associated.map(|ap| ap.ssid.clone()),
                strength: associated.map_or(0, |ap| ap.strength),
                wifi_ip: read_ipv4(&self.connection, Some(&device.device)).await,
                available_networks,
                ..WifiDeviceInfo::default()
            });
        }
        if let Some(index) = primary_wifi_index(route.as_ref(), &links) {
            promote_primary(&mut wifi_devices, index);
        }
        let ethernet = self.activated_ethernet().await;
        let ethernet_ip = read_ipv4(&self.connection, ethernet.as_ref().map(|ethernet| &ethernet.device)).await;
        NetworkState {
            connected,
            ssid: display_ssid(wired, wifi_devices.first()),
            wifi_enabled: self.nm.wireless_enabled().await.unwrap_or_default(),
            wifi_present: !wifi.is_empty(),
            // `ethernet()` clones out, so no guard lives across the awaits below.
            ethernet_present: !self.ethernet().is_empty(),
            networking_enabled: self.nm.networking_enabled().await.unwrap_or_default(),
            ethernet_enabled: ethernet.is_some(),
            ethernet_ip,
            // NetworkManager reports an unknown speed as `0`.
            ethernet_speed: match &ethernet {
                Some(ethernet) => ethernet.wired.speed().await.ok().filter(|speed| *speed > 0),
                None => None,
            },
            // The join state overlays these fields after NM facts are read.
            connecting_ssid: None,
            connect_error: None,
            password_ssid: None,
            wifi_devices,
            vpns: self.build_vpns(),
            // Overlaid with the VPN state.
            vpn_error: None,
            vpn_secret: None,
        }
    }

    /// First activated wired device.
    async fn activated_ethernet(&self) -> Option<EthernetDevice> {
        for ethernet in self.ethernet() {
            match ethernet.device.state().await {
                Ok(DEVICE_STATE_ACTIVATED) => return Some(ethernet),
                Ok(_) => {}
                Err(err) => debug!("failed to read state for ethernet device {}: {err}", ethernet.path),
            }
        }
        None
    }

    pub(super) fn wifi(&self, id: Option<&str>) -> Option<WifiDevice> {
        let primary = self.state.lock().expect("mutex poisoned").wifi_devices.first().map(|wifi| wifi.id.clone());
        let devices = self.devices.lock().expect("mutex poisoned");
        match id {
            Some(id) => devices.wifi.iter().find(|wifi| wifi.id == id),
            None => {
                primary.and_then(|id| devices.wifi.iter().find(|wifi| wifi.id == id)).or_else(|| devices.wifi.first())
            }
        }
        .cloned()
    }

    pub(super) fn access_point(&self, id: &str, ssid: &str) -> Option<super::AccessPointInfo> {
        self.state
            .lock()
            .expect("mutex poisoned")
            .wifi_devices
            .iter()
            .find(|device| device.id == id)?
            .available_networks
            .iter()
            .find(|ap| ap.ssid == ssid)
            .cloned()
    }

    /// The wired devices now, cloned out like [`wifi`](Self::wifi).
    pub(super) fn ethernet(&self) -> Vec<EthernetDevice> {
        self.devices.lock().expect("mutex poisoned").ethernet.clone()
    }

    /// `NetworkingEnabled` is read-only; only `WirelessEnabled`/`WwanEnabled`/`WimaxEnabled`
    /// have setters. Toggle it with `Enable(bool)`, not a direct property write.
    pub async fn set_networking_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.enable(enabled).await {
            debug!("failed to set networking_enabled={enabled}: {err}");
        }
    }

    /// `WirelessEnabled` is read-write.
    pub async fn set_wifi_enabled(&self, enabled: bool) {
        if let Err(err) = self.nm.set_wireless_enabled(enabled).await {
            debug!("failed to set wifi_enabled={enabled}: {err}");
        }
    }

    /// ADR-0029: `false` disconnects every wired device; `true` activates each existing
    /// autoconnect profile. A device with none is a no-op; NM cannot fabricate a connection.
    pub async fn set_ethernet_enabled(&self, enabled: bool) {
        for ethernet in self.ethernet() {
            if enabled {
                self.activate_autoconnect_profile(&ethernet.device, &ethernet.path).await;
            } else if let Err(err) = ethernet.device.disconnect().await {
                warn!("failed to disconnect ethernet device {}: {err}", ethernet.path);
            }
        }
    }

    /// `Device.Disconnect` on the selected Wi-Fi device. NetworkManager also stops
    /// autoconnect there until the user joins again, so the radio does not rejoin behind the click.
    pub async fn disconnect_wifi_device(&self, id: Option<&str>) {
        let Some(wifi) = self.wifi(id) else {
            match id {
                Some(id) => warn!("disconnect_wifi_device({id:?}): Wi-Fi device is unavailable"),
                None => debug!("disconnect_wifi: no Wi-Fi device is available"),
            }
            return;
        };
        if let Err(err) = wifi.device.disconnect().await {
            warn!("failed to disconnect Wi-Fi device {}: {err}", wifi.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::{PrivateBus, poll_until, private_bus, within};

    /// A device whose `Ip4Config` is at the path it holds.
    struct Ip4Device(&'static str);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl Ip4Device {
        #[zbus(property)]
        fn ip4_config(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from(self.0).unwrap()
        }
    }

    struct Ip4Config;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.IP4Config")]
    impl Ip4Config {
        #[zbus(property)]
        fn address_data(&self) -> Vec<HashMap<String, zbus::zvariant::OwnedValue>> {
            use super::super::tests::owned;
            let address = |ip: &str| HashMap::from([("address".into(), owned(ip)), ("prefix".into(), owned(24u32))]);
            vec![address("192.0.2.7"), address("192.0.2.8")]
        }
    }

    /// A device's address is its current config's first, without the prefix; `/` is no config.
    #[tokio::test]
    async fn ipv4_is_the_first_address_of_the_current_config() {
        let (configured, unconfigured) =
            ("/org/freedesktop/NetworkManager/Devices/1", "/org/freedesktop/NetworkManager/Devices/2");
        let config = "/org/freedesktop/NetworkManager/IP4Config/3";
        let (controller, _events, _peer) = super::super::connect::tests::attempting(|peer| {
            peer.serve_at(configured, Ip4Device(config))?
                .serve_at(unconfigured, Ip4Device("/"))?
                .serve_at(config, Ip4Config)
        })
        .await;
        for (path, expected) in [(configured, Some("192.0.2.7")), (unconfigured, None)] {
            let path = OwnedObjectPath::try_from(path).unwrap();
            let device = crate::capabilities::bind::<DeviceProxy>(&controller.connection, path.clone()).await.unwrap();
            assert_eq!(read_ipv4(&controller.connection, Some(&device)).await.as_deref(), expected, "{path}");
        }
    }

    struct FakeNm(OwnedObjectPath);

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl FakeNm {
        fn get_all_devices(&self) -> Vec<OwnedObjectPath> {
            vec![self.0.clone()]
        }
    }

    struct FakeWired(u32);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl FakeWired {
        #[zbus(property)]
        fn device_type(&self) -> u32 {
            super::super::proxies::DEVICE_TYPE_ETHERNET
        }

        #[zbus(property)]
        fn state(&self) -> u32 {
            self.0
        }
    }

    /// A NetworkManager with one wired device at `path`, in `state`.
    async fn serve_nm(bus: &PrivateBus, path: &str, state: u32) -> zbus::Connection {
        let path = OwnedObjectPath::try_from(path).unwrap();
        let builder = bus.builder().serve_at("/org/freedesktop/NetworkManager", FakeNm(path.clone())).unwrap();
        let builder = builder.serve_at(path, FakeWired(state)).unwrap();
        builder.name("org.freedesktop.NetworkManager").unwrap().build().await.unwrap()
    }

    /// A restarted NetworkManager can reuse a device path, and announces nothing for objects it
    /// exported before taking its name, so only the owner change says the devices are new.
    #[tokio::test]
    async fn devices_recover_after_networkmanager_restarts() {
        let bus = private_bus().await;
        let device = "/org/freedesktop/NetworkManager/Devices/2";
        let first = serve_nm(&bus, device, DEVICE_STATE_ACTIVATED).await;
        let (events, mut signals) = tokio::sync::mpsc::unbounded_channel();
        let network = NetworkController::new(bus.connection().await, events, tokio::sync::mpsc::unbounded_channel().0)
            .await
            .unwrap();
        let state = network.handle_signal(NetworkSignal::Changed).await;
        assert!(state.ethernet_present && state.ethernet_enabled);

        // The old NetworkManager asks for a VPN secret, then exits before it is answered.
        let vpn = HashMap::from([("connection", HashMap::from([("type", super::super::tests::owned("vpn"))]))]);
        let path = OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Settings/5").unwrap();
        let ask = (vpn, path, "vpn", vec!["password"], 1u32);
        let (asking, agent) = (first.clone(), network.connection.unique_name().unwrap().to_string());
        tokio::spawn(async move {
            let interface = Some("org.freedesktop.NetworkManager.SecretAgent");
            let path = "/org/freedesktop/NetworkManager/SecretAgent";
            asking.call_method(Some(agent.as_str()), path, interface, "GetSecrets", &ask).await
        });
        poll_until(|| network.vpn.lock().unwrap().secret.is_some().then_some(())).await;
        first.release_name("org.freedesktop.NetworkManager").await.unwrap();
        drop(first);
        for (path, state) in [(device, 30), ("/org/freedesktop/NetworkManager/Devices/3", DEVICE_STATE_ACTIVATED)] {
            let _restarted = serve_nm(&bus, path, state).await;
            while network.ethernet().first().map(|wired| wired.path.as_str()) != Some(path)
                || network.state.lock().unwrap().ethernet_enabled != (state == DEVICE_STATE_ACTIVATED)
            {
                network.handle_signal(within(signals.recv()).await.unwrap()).await;
            }
            assert!(network.vpn.lock().unwrap().secret.is_none(), "the prompt left with the NetworkManager that asked");
        }
    }

    #[test]
    fn a_rebuild_keeps_a_scan_until_it_completes() {
        let previous = NetworkState {
            wifi_devices: ["wlan0", "wlan1"]
                .map(|id| WifiDeviceInfo { id: id.into(), scanning: true, ..WifiDeviceInfo::default() })
                .to_vec(),
            ..NetworkState::default()
        };
        let mut next = previous.clone();
        carry_scan(&mut next, Some(&previous), &NetworkSignal::ScanCompleted("wlan1".into()));
        assert!(next.wifi_devices[0].scanning && !next.wifi_devices[1].scanning);
        let mut own = previous.clone();
        own.wifi_devices[0].scanning = false;
        carry_scan(&mut own, None, &NetworkSignal::ScanStarted("wlan0".into()));
        assert!(own.wifi_devices[0].scanning && own.wifi_devices[1].scanning, "None carries the state's own flags");
    }

    #[test]
    fn primary_prefers_the_default_route_then_an_activated_interface() {
        let route = OwnedObjectPath::try_from("/active/2").unwrap();
        let other = OwnedObjectPath::try_from("/active/1").unwrap();
        let links = vec![
            (None, Some(30)),
            (Some(other.clone()), Some(DEVICE_STATE_ACTIVATED)),
            (Some(route.clone()), Some(DEVICE_STATE_ACTIVATED)),
        ];
        assert_eq!(primary_wifi_index(Some(&route), &links), Some(2));
        assert_eq!(primary_wifi_index(None, &links), Some(1));
        assert_eq!(primary_wifi_index(None, &links[..1]), Some(0));
        let tie = [(Some(other.clone()), Some(DEVICE_STATE_ACTIVATED)), (Some(other), Some(DEVICE_STATE_ACTIVATED))];
        assert_eq!(primary_wifi_index(None, &tie), Some(0));
        assert_eq!(primary_wifi_index(None, &[]), None);
        let mut devices =
            ["wlan0", "wlan1", "wlan2", "wlan3"].map(|id| WifiDeviceInfo { id: id.into(), ..Default::default() });
        promote_primary(&mut devices, 2);
        assert_eq!(devices.map(|device| device.id), ["wlan2", "wlan0", "wlan1", "wlan3"]);
    }

    #[test]
    fn ethernet_wins_while_wifi_stays_associated() {
        let wifi = WifiDeviceInfo { ssid: Some("home".into()), ..Default::default() };
        assert_eq!(display_ssid(true, Some(&wifi)).as_deref(), Some("Ethernet"));
        assert_eq!(display_ssid(false, Some(&wifi)).as_deref(), Some("home"));
        assert_eq!(display_ssid(false, None), None);
    }

    #[tokio::test]
    async fn startup_and_saved_changed_walk_profiles_once_and_clear_on_error() {
        use super::super::connect::tests::ListedProfiles;
        use super::super::devices::tests::{collect_method_calls, method_call_monitor};
        use super::super::tests::SavedProfile;

        use super::super::tests::owned as ov;
        let vpn = |k: &str, i: &str| {
            let m = [("type".into(), ov(k)), ("id".into(), ov(i)), ("uuid".into(), ov(i))];
            HashMap::from([("connection".into(), HashMap::from(m))])
        };
        let wifi = |s: &[u8]| HashMap::from([("802-11-wireless".into(), HashMap::from([("ssid".into(), ov(s))]))]);
        let p = |n: u8| OwnedObjectPath::try_from(format!("/org/freedesktop/NetworkManager/Settings/{n}")).unwrap();
        let (bus, sp) = (private_bus().await, "/org/freedesktop/NetworkManager/Settings");
        let mut builder = bus
            .builder()
            .serve_at("/org/freedesktop/NetworkManager", FakeNm(super::super::root_object_path().into()))
            .unwrap()
            .serve_at(sp, ListedProfiles(vec![p(1), p(2), p(3), p(4), p(9)]))
            .unwrap();
        let profiles = [
            (1, wifi(b"lab-\xff")),
            (2, vpn("wireguard", "wg0")),
            (3, vpn("vpn", "alpha")),
            (4, vpn("802-3-ethernet", "eth")),
            (5, wifi(b"cafe")),
            (6, vpn("vpn", "beta")),
        ];
        for (n, map) in profiles {
            builder = builder
                .serve_at(p(n), SavedProfile(tokio::sync::mpsc::unbounded_channel().0, map, 0, Vec::new()))
                .unwrap();
        }
        let nm_conn = builder.name("org.freedesktop.NetworkManager").unwrap().build().await.unwrap();
        let conn = bus.connection().await;
        let mut monitor = method_call_monitor(&bus).await;
        let (events, mut signals) = tokio::sync::mpsc::unbounded_channel();
        let (states, mut pushed) = tokio::sync::mpsc::unbounded_channel();
        let controller = NetworkController::new(conn.clone(), events, states).await.unwrap();
        let startup = collect_method_calls(&mut monitor, &conn).await;
        assert_eq!((startup.get("ListConnections"), startup.get("GetSettings")), (Some(&1), Some(&5)));
        let ssids = || controller.saved_ssids.lock().unwrap().clone();
        let vpn_ids = || controller.build_vpns().into_iter().map(|vpn| vpn.id).collect::<Vec<_>>();
        assert_eq!(ssids(), HashMap::from([(p(1), b"lab-\xff".to_vec())]));
        assert_eq!(vpn_ids(), ["alpha", "wg0"]);
        let listed = |state: NetworkProfilesState| state.profiles.into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(listed(pushed.try_recv().unwrap()), ["alpha", "eth", "wg0"], "pushed once at startup");

        let iface = nm_conn.object_server().interface::<_, ListedProfiles>(sp).await.unwrap();
        iface.get_mut().await.0 = vec![p(5), p(6), p(3), p(9)];
        drop(iface);
        controller.handle_signal(NetworkSignal::SavedChanged).await;
        let reload = collect_method_calls(&mut monitor, &conn).await;
        assert_eq!((reload.get("ListConnections"), reload.get("GetSettings")), (Some(&1), Some(&4)));
        assert_eq!(ssids(), HashMap::from([(p(5), b"cafe".to_vec())]));
        assert_eq!(vpn_ids(), ["alpha", "beta"]);
        assert_eq!(listed(pushed.try_recv().unwrap()), ["alpha", "beta"]);

        // NetworkManager's `Updated` re-reads only the profile it names.
        let iface = nm_conn.object_server().interface::<_, SavedProfile>(p(6)).await.unwrap();
        iface.get_mut().await.1 = vpn("vpn", "gamma");
        drop(iface);
        let interface = "org.freedesktop.NetworkManager.Settings.Connection";
        nm_conn.emit_signal(None::<&str>, p(6), interface, "Updated", &()).await.unwrap();
        while within(signals.recv()).await != Some(NetworkSignal::ProfileUpdated(p(6))) {}
        controller.handle_signal(NetworkSignal::ProfileUpdated(p(6))).await;
        let updated = collect_method_calls(&mut monitor, &conn).await;
        assert_eq!((updated.get("ListConnections"), updated.get("GetSettings")), (None, Some(&1)));
        assert_eq!(vpn_ids(), ["alpha", "gamma"]);
        assert_eq!(listed(pushed.try_recv().unwrap()), ["alpha", "gamma"]);

        nm_conn.object_server().remove::<ListedProfiles, _>(sp).await.unwrap();
        controller.handle_signal(NetworkSignal::SavedChanged).await;
        assert!(ssids().is_empty() && vpn_ids().is_empty());
    }
}

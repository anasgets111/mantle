//! [`NetworkController`]: `mantle.network`'s proxies and push state, rebuilt from NetworkManager on
//! every signal, and the networking, radio and wired switches.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use shared::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedObjectPath;

use super::devices::{
    Devices, EthernetDevice, WifiDevice, forward, resolve_devices, spawn_manager_forwarder, watch_devices,
};
use super::join::JoinState;
use super::proxies::{DEVICE_STATE_ACTIVATED, DeviceProxy, IP4ConfigProxy, NetworkManagerProxy, SettingsProxy};
use super::{NetworkSignal, NetworkState};
use shared::state::network::WifiDeviceInfo;

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
    /// [`NetworkSignal::SavedChanged`]. ponytail: an edited SSID stays stale until another profile
    /// change or NM restart. Upgrade path: watch each profile's `Updated`.
    pub(super) saved_ssids: Arc<Mutex<HashMap<OwnedObjectPath, Vec<u8>>>>,
    /// Push state, mutated by [`handle_signal`](Self::handle_signal).
    pub(super) state: Arc<Mutex<NetworkState>>,
    /// The pending intent, prompt and accepted attempt share one lock.
    pub(super) join: Arc<Mutex<JoinState>>,
    /// Signal sender, including scan's FIFO event.
    pub(super) events: UnboundedSender<NetworkSignal>,
}

fn carry_scan(next: &mut NetworkState, previous: &NetworkState, signal: &NetworkSignal) {
    for device in &mut next.wifi_devices {
        device.scanning = matches!(signal, NetworkSignal::ScanStarted(id) if id == &device.id)
            || (previous.wifi_devices.iter().find(|old| old.id == device.id).is_some_and(|old| old.scanning)
                && !matches!(signal, NetworkSignal::ScanCompleted(id) if id == &device.id));
    }
    next.scanning = next.wifi_devices.first().is_some_and(|wifi| wifi.scanning);
}

/// `device`'s first IPv4 address without its prefix, read uncached because `Ip4Config`'s path
/// changes per activation. ponytail: a DHCP renewal without a state change shows the old address
/// until the next rebuild. Upgrade path: watch `AddressData`.
async fn read_ipv4(connection: &zbus::Connection, device: Option<&DeviceProxy<'static>>) -> Option<String> {
    let path = device?.ip4_config().await.ok().filter(|path| path.as_str() != "/")?;
    let config = IP4ConfigProxy::builder(connection)
        .path(path)
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
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
    pub async fn new(connection: zbus::Connection, events: UnboundedSender<NetworkSignal>) -> zbus::Result<Self> {
        let nm = NetworkManagerProxy::new(&connection).await?;
        let settings = SettingsProxy::new(&connection).await?;

        // Subscribed before the first device scan, so an adapter plugged in during it is not missed.
        let device_list = tokio::try_join!(nm.receive_device_added(), nm.receive_device_removed());
        forward(device_list, NetworkSignal::DevicesChanged, events.clone());
        let (wifi, ethernet) = resolve_devices(&connection, &nm).await?;
        let watchers = watch_devices(&connection, &wifi, &ethernet, &events);
        spawn_manager_forwarder(nm.clone(), events.clone());
        // Subscribed before the first fill below, so a profile saved in between is not missed.
        let profiles = tokio::try_join!(settings.receive_new_connection(), settings.receive_connection_removed());
        forward(profiles, NetworkSignal::SavedChanged, events.clone());
        let mut owners = settings.inner().receive_owner_changed().await?;
        let owner_events = events.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while owners.next().await.is_some() && owner_events.send(NetworkSignal::SavedChanged).is_ok() {}
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
            events,
        };
        controller.refresh_saved_ssids().await;
        Ok(controller)
    }

    /// Rebuilds and returns push state for one signal.
    pub async fn handle_signal(&self, signal: NetworkSignal) -> NetworkState {
        let mut next = match &signal {
            NetworkSignal::ScanStarted(_) => {
                let current = self.state.lock().expect("mutex poisoned").clone();
                let mut next =
                    if current.wifi_devices.is_empty() { self.build_state(&signal).await } else { current.clone() };
                carry_scan(&mut next, &current, &signal);
                next
            }
            _ => {
                match signal {
                    NetworkSignal::SavedChanged => self.refresh_saved_ssids().await,
                    NetworkSignal::DevicesChanged => {
                        self.refresh_devices().await;
                        if let Some(pending) = self.pending_intent()
                            && self.pending_wifi(&pending).is_none()
                        {
                            self.resolve_connect_intent().await;
                        }
                    }
                    _ => {}
                }
                // Read D-Bus before taking the plain mutex; never hold it across an await.
                let mut next = self.build_state(&signal).await;
                {
                    let state = self.state.lock().expect("mutex poisoned");
                    carry_scan(&mut next, &state, &signal);
                }
                next
            }
        };
        self.join.lock().expect("mutex poisoned").overlay(&mut next);
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
        let primary = wifi_devices.first();
        let ethernet = self.activated_ethernet().await;
        let ethernet_ip = read_ipv4(&self.connection, ethernet.as_ref().map(|ethernet| &ethernet.device)).await;
        NetworkState {
            scanning: false,
            connected,
            ssid: display_ssid(wired, primary),
            strength: primary.map_or(0, |device| device.strength),
            wifi_enabled: self.nm.wireless_enabled().await.unwrap_or_default(),
            wifi_present: !wifi.is_empty(),
            // `ethernet()` clones out, so no guard lives across the awaits below.
            ethernet_present: !self.ethernet().is_empty(),
            networking_enabled: self.nm.networking_enabled().await.unwrap_or_default(),
            ethernet_enabled: ethernet.is_some(),
            wifi_ip: primary.and_then(|device| device.wifi_ip.clone()),
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
            available_networks: primary.map_or_else(Vec::new, |device| device.available_networks.clone()),
            wifi_devices,
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

    #[test]
    fn a_rebuild_keeps_a_scan_until_it_completes_or_its_device_disappears() {
        let previous = NetworkState {
            wifi_devices: ["wlan0", "wlan1"]
                .map(|id| WifiDeviceInfo { id: id.into(), scanning: true, ..WifiDeviceInfo::default() })
                .to_vec(),
            ..NetworkState::default()
        };
        let mut next = previous.clone();
        carry_scan(&mut next, &previous, &NetworkSignal::ScanCompleted("wlan1".into()));
        assert!(next.scanning && next.wifi_devices[0].scanning && !next.wifi_devices[1].scanning);
        next.wifi_devices.clear();
        carry_scan(&mut next, &previous, &NetworkSignal::DevicesChanged);
        assert!(!next.scanning);
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
}

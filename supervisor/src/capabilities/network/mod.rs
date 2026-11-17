//! NetworkManager D-Bus controller (`mantle.network`; ADR-0029). Its hand-written proxies'
//! (`proxies.rs`, ADR-0212) signal streams feed its worker task (`capabilities::spawn_worker`), which
//! rebuilds state and sends it to `main.rs`.
//!
//! Forwarder tasks feed one channel: wireless APs/association, each device's state, the manager's
//! radio switches/default route, its device list, and saved-profile changes. ADR-0082: scan-only
//! watching left connected machines reading offline for minutes.
//!
//! A device added or removed after startup, such as a USB adapter, rescans the device set and
//! restarts its watchers ([`NetworkSignal::DevicesChanged`]).
pub use shared::state::network::{AccessPointInfo, JoinError, NetworkState};

use zbus::zvariant::ObjectPath;

mod connect;
mod controller;
mod devices;
mod intent;
mod join;
mod profiles;
mod proxies;
mod scan;
mod secret_agent;
mod vpn;

pub use controller::NetworkController;
use shared::action::NetworkAction;

/// A pending `network:connect(ssid, hidden)` intent, stashed in the controller (ADR-0037) with
/// the same single-slot semantics the PAM one-shot protocol uses (ADR-0028), until paired
/// `secure_submit(network, connect)` supplies password bytes (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingNetworkConnect {
    pub(super) attempt: u64,
    pub ssid: String,
    /// The raw SSID `ssid` names, resolved once when stashed; the error is the join's.
    pub ssid_bytes: Result<Vec<u8>, &'static str>,
    pub hidden: bool,
    pub device_id: String,
    pub device_path: Option<zbus::zvariant::OwnedObjectPath>,
}

/// What forwarders report to the network worker; `build_state` makes the payload with a fresh D-Bus
/// round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkSignal {
    /// Any non-`scanning` field change: AP set, association, device state, or radio. All trigger
    /// the same full re-derive (ADR-0029), so one variant is enough.
    Changed,
    /// `LastScan` changed, or NetworkManager refused `RequestScan`. Either way no scan is in flight.
    ScanCompleted(String),
    /// Sent by [`NetworkController::scan_device`] before `RequestScan` completes, through
    /// the same channel for FIFO ordering.
    ScanStarted(String),
    /// A saved profile was added or removed, so the saved-SSID cache is stale.
    SavedChanged,
    /// NetworkManager added or removed a device, so the device set is stale.
    DevicesChanged,
    /// NetworkManager took its name again: its devices are stale even at reused paths.
    Restarted,
}

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::from_static_str_unchecked("/")
}

/// `mantle.network` dispatch (ADR-0037). Writes spawn rather than await inline (ADR-0029);
/// `connect` stashes its intent until paired `secure_submit(network, connect)`.
pub fn dispatch(controller: &NetworkController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<NetworkAction>(&envelope.params) else { return };
    let controller = controller.clone();
    match action {
        NetworkAction::SetNetworkingEnabled { enabled } => {
            tokio::spawn(async move { controller.set_networking_enabled(enabled).await });
        }
        NetworkAction::SetWifiEnabled { enabled } => {
            tokio::spawn(async move { controller.set_wifi_enabled(enabled).await });
        }
        NetworkAction::SetEthernetEnabled { enabled } => {
            tokio::spawn(async move { controller.set_ethernet_enabled(enabled).await });
        }
        NetworkAction::Scan => {
            tokio::spawn(async move { controller.scan_device(None).await });
        }
        NetworkAction::ScanDevice { id } => {
            tokio::spawn(async move { controller.scan_device(Some(&id)).await });
        }
        NetworkAction::Connect { ssid, hidden } => controller.stash_connect_intent(ssid, hidden, None),
        NetworkAction::ConnectDevice { ssid, hidden, id } => controller.stash_connect_intent(ssid, hidden, Some(&id)),
        // Not spawned: it touches no D-Bus, and a late cancel would resurrect the prompt.
        NetworkAction::CancelConnect => controller.cancel_connect(),
        NetworkAction::AbortConnect => controller.abort_connect(),
        NetworkAction::Forget { ssid } => {
            tokio::spawn(async move { controller.forget(&ssid).await });
        }
        NetworkAction::DisconnectWifi => {
            tokio::spawn(async move { controller.disconnect_wifi_device(None).await });
        }
        NetworkAction::DisconnectWifiDevice { id } => {
            tokio::spawn(async move { controller.disconnect_wifi_device(Some(&id)).await });
        }
        NetworkAction::ConnectVpn { uuid } => {
            tokio::spawn(async move { controller.connect_vpn(&uuid).await });
        }
        NetworkAction::DisconnectVpn { uuid } => {
            tokio::spawn(async move { controller.disconnect_vpn(&uuid).await });
        }
        // Not spawned, like `cancel_connect`: it only answers a waiting agent call.
        NetworkAction::CancelVpnSecret => controller.cancel_vpn_secret(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use shared::Capability;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use zbus::message::Header;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    use super::connect::tests::{ListedProfiles, attempting, begun, home_on, wifi};
    use super::join::tests::joined;
    use super::vpn::tests::{ACTIVE, FakeActive, settings};
    use super::*;
    use crate::capabilities::bind;
    use crate::capabilities::test_support::{dispatcher, poll_until, within};

    const NM: &str = "/org/freedesktop/NetworkManager";
    const WLAN0: &str = "/org/freedesktop/NetworkManager/Devices/1";
    const WLAN1: &str = "/org/freedesktop/NetworkManager/Devices/2";
    const ETH0: &str = "/org/freedesktop/NetworkManager/Devices/3";
    const HOME: &str = "/org/freedesktop/NetworkManager/Settings/1";
    const VPN: &str = "/org/freedesktop/NetworkManager/Settings/2";

    /// Each fake sends `<object path> <call>` for every call it answers, to a test that may not listen.
    fn record(calls: &UnboundedSender<String>, header: &Header<'_>, call: String) {
        let path = header.path().map_or("", |path| path.as_str());
        let _ = calls.send(format!("{path} {call}"));
    }

    /// NetworkManager's manager object, for any `network` test that checks what it was asked.
    pub(crate) struct Nm(pub(crate) UnboundedSender<String>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl Nm {
        fn enable(&self, #[zbus(header)] header: Header<'_>, enable: bool) {
            record(&self.0, &header, format!("Enable({enable})"));
        }

        #[zbus(property)]
        fn wireless_enabled(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn set_wireless_enabled(&self, enabled: bool) {
            let _ = self.0.send(format!("{NM} WirelessEnabled={enabled}"));
        }

        #[zbus(property)]
        fn active_connections(&self) -> Vec<OwnedObjectPath> {
            vec![OwnedObjectPath::try_from(ACTIVE).unwrap()]
        }

        fn activate_connection(
            &self,
            #[zbus(header)] header: Header<'_>,
            profile: OwnedObjectPath,
            device: OwnedObjectPath,
            ap: OwnedObjectPath,
        ) -> OwnedObjectPath {
            record(&self.0, &header, format!("ActivateConnection({profile}, {device}, {ap})"));
            OwnedObjectPath::try_from(ACTIVE).unwrap()
        }

        fn deactivate_connection(&self, #[zbus(header)] header: Header<'_>, active: OwnedObjectPath) {
            record(&self.0, &header, format!("DeactivateConnection({active})"));
        }
    }

    struct Device(UnboundedSender<String>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl Device {
        fn disconnect(&self, #[zbus(header)] header: Header<'_>) {
            record(&self.0, &header, "Disconnect".into());
        }
    }

    struct Wireless(UnboundedSender<String>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
    impl Wireless {
        fn request_scan(&self, #[zbus(header)] header: Header<'_>, _options: HashMap<String, OwnedValue>) {
            record(&self.0, &header, "RequestScan".into());
        }
    }

    pub(crate) struct SavedProfile(
        pub(crate) UnboundedSender<String>,
        pub(crate) HashMap<String, HashMap<String, OwnedValue>>,
    );

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl SavedProfile {
        fn get_settings(&self) -> HashMap<String, HashMap<String, OwnedValue>> {
            self.1.clone()
        }

        fn delete(&self, #[zbus(header)] header: Header<'_>) {
            record(&self.0, &header, "Delete".into());
        }
    }

    /// A controller with Wi-Fi devices wlan0 and wlan1, one wired device, a saved `home` network
    /// and a VPN `u-1` whose activation reads as activated; then what its NetworkManager was asked.
    async fn fixture()
    -> (NetworkController, UnboundedReceiver<String>, UnboundedReceiver<NetworkSignal>, zbus::Connection) {
        let (calls, asked) = unbounded_channel();
        let ssid = OwnedValue::try_from(Value::from(&b"home"[..])).unwrap();
        let home = HashMap::from([("802-11-wireless".into(), HashMap::from([("ssid".into(), ssid)]))]);
        let (controller, events, peer) = attempting(|mut peer| {
            for path in [WLAN0, WLAN1, ETH0] {
                peer = peer.serve_at(path, Device(calls.clone()))?;
            }
            peer.serve_at(NM, Nm(calls.clone()))?
                .serve_at(WLAN0, Wireless(calls.clone()))?
                .serve_at(WLAN1, Wireless(calls.clone()))?
                .serve_at(HOME, SavedProfile(calls.clone(), home))?
                .serve_at(VPN, SavedProfile(calls.clone(), settings("vpn")))?
                .serve_at(
                    "/org/freedesktop/NetworkManager/Settings",
                    ListedProfiles([HOME, VPN].map(|path| OwnedObjectPath::try_from(path).unwrap()).to_vec()),
                )?
                .serve_at(ACTIVE, FakeActive { state: proxies::ACTIVE_STATE_ACTIVATED, then: None })
        })
        .await;
        for (id, path) in [("wlan0", WLAN0), ("wlan1", WLAN1)] {
            let device = wifi(&controller, id, OwnedObjectPath::try_from(path).unwrap()).await;
            controller.devices.lock().unwrap().wifi.push(device);
        }
        let path = OwnedObjectPath::try_from(ETH0).unwrap();
        let device = bind(&controller.connection, path.clone()).await.unwrap();
        let wired = bind(&controller.connection, path.clone()).await.unwrap();
        controller.devices.lock().unwrap().ethernet.push(devices::EthernetDevice { device, wired, path });
        controller.refresh_profiles().await;
        (controller, asked, events, peer)
    }

    /// Every arm that writes to NetworkManager reaches its one call, on the device or profile named.
    #[tokio::test]
    async fn each_write_reaches_its_networkmanager_call() {
        let cases: [(&str, Vec<serde_json::Value>, String); 10] = [
            ("set_networking_enabled", vec![false.into()], format!("{NM} Enable(false)")),
            ("set_wifi_enabled", vec![true.into()], format!("{NM} WirelessEnabled=true")),
            ("set_ethernet_enabled", vec![false.into()], format!("{ETH0} Disconnect")),
            ("scan", vec![], format!("{WLAN0} RequestScan")),
            ("scan_device", vec!["wlan1".into()], format!("{WLAN1} RequestScan")),
            ("forget", vec!["home".into()], format!("{HOME} Delete")),
            ("disconnect_wifi", vec![], format!("{WLAN0} Disconnect")),
            ("disconnect_wifi_device", vec!["wlan1".into()], format!("{WLAN1} Disconnect")),
            ("connect_vpn", vec!["u-1".into()], format!("{NM} ActivateConnection({VPN}, /, /)")),
            ("disconnect_vpn", vec!["u-1".into()], format!("{NM} DeactivateConnection({ACTIVE})")),
        ];
        let mut wrong = Vec::new();
        for (action, arguments, expected) in cases {
            let (controller, mut asked, _events, _peer) = fixture().await;
            dispatcher(Capability::Network, dispatch, &controller)(action, arguments);
            let call = within(asked.recv()).await;
            if call.as_deref() != Some(expected.as_str()) {
                wrong.push(format!("{action}: expected {expected:?}, got {call:?}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn connect_stashes_its_intent_on_the_primary_or_named_device() {
        for (action, arguments, hidden, id, path) in [
            ("connect", vec!["cafe".into(), false.into()], false, "wlan0", WLAN0),
            ("connect_device", vec!["cafe".into(), true.into(), "wlan1".into()], true, "wlan1", WLAN1),
        ] {
            let (controller, _asked, _events, _peer) = fixture().await;
            dispatcher(Capability::Network, dispatch, &controller)(action, arguments);
            // Stashed before `dispatch` returns; its resolution is spawned.
            let pending = controller.pending_intent().unwrap_or_else(|| panic!("{action} stashed nothing"));
            let device_path = pending.device_path.as_ref().map(|path| path.as_str());
            assert_eq!(
                (pending.ssid.as_str(), pending.hidden, pending.device_id.as_str(), device_path),
                ("cafe", hidden, id, Some(path)),
                "{action}"
            );
        }
    }

    #[tokio::test]
    async fn cancel_drops_only_a_prompt_and_abort_stops_an_accepted_join() {
        let (controller, mut asked, _events, _peer) = fixture().await;
        let dispatch = dispatcher(Capability::Network, dispatch, &controller);
        let join = || {
            let mut state = NetworkState::default();
            controller.join.lock().unwrap().overlay(&mut state);
            (state.password_ssid, state.connecting_ssid)
        };
        {
            let mut join = controller.join.lock().unwrap();
            join.stash(home_on(WLAN0));
            let pending = join.pending().unwrap();
            assert!(join.request_password(&pending));
        }
        dispatch("cancel_connect", vec![]);
        assert_eq!(join(), (None, None), "cancel takes the prompt down");

        let (_, attempt) = begun(&controller, home_on(WLAN0));
        assert!(controller.accept(attempt, &joined(1)));
        dispatch("cancel_connect", vec![]);
        assert_eq!(join(), (None, Some("home".into())), "cancel leaves a running join");
        dispatch("abort_connect", vec![]);
        assert_eq!(join(), (None, None), "abort ends the join");
        let stopped = within(asked.recv()).await;
        assert_eq!(stopped, Some(format!("{NM} DeactivateConnection({})", joined(1).active)));
    }

    #[tokio::test]
    async fn cancel_vpn_secret_declines_the_pending_request() {
        let (controller, _asked, _events, _peer) = fixture().await;
        let declined = secret_agent::tests::asking(Arc::clone(&controller.vpn), controller.events.clone());
        poll_until(|| controller.vpn.lock().unwrap().secret.is_some().then_some(())).await;

        dispatcher(Capability::Network, dispatch, &controller)("cancel_vpn_secret", vec![]);

        assert!(controller.vpn.lock().unwrap().secret.is_none(), "the request is taken down at once");
        assert!(within(declined).await.unwrap(), "NetworkManager hears the user declined");
    }

    /// [`fixture`] with a password prompt up for `home` on wlan0 and a VPN secret request
    /// pending, for `lifecycle`'s tests; the handle resolves to whether the user declined that
    /// request.
    pub(crate) async fn prompted()
    -> (NetworkController, UnboundedReceiver<NetworkSignal>, zbus::Connection, tokio::task::JoinHandle<bool>) {
        let (controller, _asked, events, peer) = fixture().await;
        {
            let mut join = controller.join.lock().unwrap();
            join.stash(home_on(WLAN0));
            let pending = join.pending().unwrap();
            assert!(join.request_password(&pending));
        }
        let vpn = secret_agent::tests::asking(Arc::clone(&controller.vpn), controller.events.clone());
        poll_until(|| controller.vpn.lock().unwrap().secret.is_some().then_some(())).await;
        (controller, events, peer, vpn)
    }
}

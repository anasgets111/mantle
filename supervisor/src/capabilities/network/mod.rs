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
mod edit;
mod intent;
mod join;
mod profile_actions;
mod profiles;
mod proxies;
mod scan;
mod secret_agent;
mod vpn;

pub use controller::NetworkController;
use shared::action::NetworkAction;
use shared::warn;

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
    /// NetworkManager changed the saved profile at this path.
    ProfileUpdated(zbus::zvariant::OwnedObjectPath),
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
            tokio::spawn(async move { controller.connect_vpn(&uuid, tokio::sync::oneshot::channel().0).await });
        }
        NetworkAction::DisconnectVpn { uuid } => {
            tokio::spawn(async move {
                if let Err(err) = controller.disconnect_vpn(&uuid).await {
                    warn!("failed to deactivate VPN {uuid}: {err}");
                }
            });
        }
        // Not spawned, like `cancel_connect`: it only answers a waiting agent call.
        NetworkAction::CancelVpnSecret => controller.cancel_vpn_secret(),
        profile @ (NetworkAction::UpdateProfile { .. }
        | NetworkAction::DeleteProfile { .. }
        | NetworkAction::ReapplyProfile { .. }
        | NetworkAction::ActivateProfile { .. }
        | NetworkAction::DeactivateProfile { .. }) => controller.queue_profile_action(profile),
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

    /// A test's settings value.
    pub(crate) fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).unwrap()
    }
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

    /// NetworkManager's manager object, for any `network` test that checks what it was asked. An
    /// activation sends wlan0's failure for the device reason given, ahead of its reply.
    pub(crate) struct Nm(pub(crate) UnboundedSender<String>, pub(crate) Option<u32>);

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

        async fn activate_connection(
            &self,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
            profile: OwnedObjectPath,
            device: OwnedObjectPath,
            ap: OwnedObjectPath,
        ) -> OwnedObjectPath {
            record(&self.0, &header, format!("ActivateConnection({profile}, {device}, {ap})"));
            if let Some(reason) = self.1 {
                let failed = (120u32, 50u32, reason);
                let device = "org.freedesktop.NetworkManager.Device";
                connection.emit_signal(None::<&str>, WLAN0, device, "StateChanged", &failed).await.unwrap();
            }
            OwnedObjectPath::try_from(ACTIVE).unwrap()
        }

        fn deactivate_connection(&self, #[zbus(header)] header: Header<'_>, active: OwnedObjectPath) {
            record(&self.0, &header, format!("DeactivateConnection({active})"));
        }

        fn get_device_by_ip_iface(&self, iface: &str) -> Result<OwnedObjectPath, NmRefused> {
            if iface == "polkit" {
                return Err(NmRefused::PermissionDenied(iface.into()));
            }
            let path = [("wlan0", WLAN0), ("wlan1", WLAN1), ("eth0", ETH0)].into_iter().find(|(id, _)| *id == iface);
            Ok(OwnedObjectPath::try_from(path.ok_or_else(|| NmRefused::UnknownDevice(iface.into()))?.1).unwrap())
        }

        #[zbus(property)]
        fn version(&self) -> String {
            "1.58.1".into()
        }
    }

    /// A device with `home` applied at version 7, under the UUID it holds.
    struct Device(UnboundedSender<String>, &'static str);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl Device {
        fn disconnect(&self, #[zbus(header)] header: Header<'_>) {
            record(&self.0, &header, "Disconnect".into());
        }

        fn get_applied_connection(&self, _flags: u32) -> (HashMap<String, HashMap<String, OwnedValue>>, u64) {
            let mut applied = home();
            applied.get_mut("connection").unwrap().insert("uuid".into(), owned(self.1));
            (applied, 7)
        }

        fn reapply(
            &self,
            #[zbus(header)] header: Header<'_>,
            settings: HashMap<String, HashMap<String, OwnedValue>>,
            version_id: u64,
            _flags: u32,
        ) {
            record(&self.0, &header, format!("Reapply({}, {version_id})", settings.len()));
        }
    }

    struct Wireless(UnboundedSender<String>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
    impl Wireless {
        fn request_scan(&self, #[zbus(header)] header: Header<'_>, _options: HashMap<String, OwnedValue>) {
            record(&self.0, &header, "RequestScan".into());
        }
    }

    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.freedesktop.NetworkManager.Settings")]
    pub(crate) enum Refused {
        PermissionDenied(String),
        VersionIdMismatch(String),
        InvalidProperty(String),
        InvalidConnection(String),
    }

    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.freedesktop.NetworkManager")]
    enum NmRefused {
        UnknownDevice(String),
        PermissionDenied(String),
    }

    /// A saved profile with its settings and `Flags`, at version 7, refusing the next `Update2`s
    /// in order. A version mismatch comes with a concurrent edit that turns autoconnect off.
    pub(crate) struct SavedProfile(
        pub(crate) UnboundedSender<String>,
        pub(crate) HashMap<String, HashMap<String, OwnedValue>>,
        pub(crate) u32,
        pub(crate) Vec<Refused>,
    );

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl SavedProfile {
        fn get_settings(&self) -> HashMap<String, HashMap<String, OwnedValue>> {
            self.1.clone()
        }

        fn delete(&self, #[zbus(header)] header: Header<'_>) {
            record(&self.0, &header, "Delete".into());
        }

        fn update2(
            &mut self,
            #[zbus(header)] header: Header<'_>,
            settings: HashMap<String, HashMap<String, OwnedValue>>,
            flags: u32,
            args: HashMap<String, OwnedValue>,
        ) -> Result<HashMap<String, OwnedValue>, Refused> {
            let version = args.get("version-id").and_then(|version| u64::try_from(version.clone()).ok());
            let autoconnect = settings.get("connection").and_then(|section| section.get("autoconnect"));
            let autoconnect = autoconnect.and_then(|value| bool::try_from(value.clone()).ok());
            record(&self.0, &header, format!("Update2({flags}, {version:?}, autoconnect {autoconnect:?})"));
            if self.3.is_empty() {
                return Ok(HashMap::new());
            }
            let refused = self.3.remove(0);
            if matches!(refused, Refused::VersionIdMismatch(_)) {
                self.1.entry("connection".into()).or_default().insert("autoconnect".into(), owned(false));
            }
            Err(refused)
        }

        #[zbus(property)]
        fn flags(&self) -> u32 {
            self.2
        }

        #[zbus(property)]
        fn version_id(&self) -> u64 {
            7
        }
    }

    /// The saved `home` Wi-Fi profile, UUID `h-1`.
    fn home() -> HashMap<String, HashMap<String, OwnedValue>> {
        let connection = [("type", owned("802-11-wireless")), ("id", owned("home")), ("uuid", owned("h-1"))];
        HashMap::from([
            ("802-11-wireless".into(), HashMap::from([("ssid".into(), owned(&b"home"[..]))])),
            ("connection".into(), connection.into_iter().map(|(key, value)| (key.to_string(), value)).collect()),
        ])
    }

    /// A controller with Wi-Fi devices wlan0 and wlan1, one wired device, a saved `home` network
    /// and a VPN `u-1` whose activation reads as activated; then what its NetworkManager was asked.
    async fn fixture()
    -> (NetworkController, UnboundedReceiver<String>, UnboundedReceiver<NetworkSignal>, zbus::Connection) {
        let (calls, asked) = unbounded_channel();
        let (controller, events, peer) = attempting(|mut peer| {
            for path in [WLAN0, WLAN1, ETH0] {
                peer = peer.serve_at(path, Device(calls.clone(), "h-1"))?;
            }
            peer.serve_at(NM, Nm(calls.clone(), None))?
                .serve_at(WLAN0, Wireless(calls.clone()))?
                .serve_at(WLAN1, Wireless(calls.clone()))?
                .serve_at(HOME, SavedProfile(calls.clone(), home(), 0, Vec::new()))?
                .serve_at(VPN, SavedProfile(calls.clone(), settings("vpn"), 0, Vec::new()))?
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
        let cases: [(&str, Vec<serde_json::Value>, String); 17] = [
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
            (
                "update_profile",
                vec!["h-1".into(), serde_json::json!({"autoconnect": false})],
                format!("{HOME} Update2(65, Some(7), autoconnect Some(false))"),
            ),
            ("delete_profile", vec!["h-1".into()], format!("{HOME} Delete")),
            ("reapply_profile", vec!["h-1".into(), "wlan1".into()], format!("{WLAN1} Reapply(0, 7)")),
            // A profile without an interface goes where NetworkManager picks; a VPN, through `connect_vpn`.
            ("activate_profile", vec!["h-1".into()], format!("{NM} ActivateConnection({HOME}, /, /)")),
            (
                "activate_profile",
                vec!["h-1".into(), "wlan1".into()],
                format!("{NM} ActivateConnection({HOME}, {WLAN1}, /)"),
            ),
            ("activate_profile", vec!["u-1".into(), "wlan1".into()], format!("{NM} ActivateConnection({VPN}, /, /)")),
            ("deactivate_profile", vec!["u-1".into()], format!("{NM} DeactivateConnection({ACTIVE})")),
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

    /// A join's staged key keeps a profile off the disk; a generated profile is unsaved by nature.
    #[tokio::test]
    async fn an_update_refuses_a_staged_key_but_saves_a_generated_profile() {
        use shared::state::network_profiles::ProfileKind::{Ethernet, Wifi};
        let (controller, mut asked, _events, peer) = fixture().await;
        let off = shared::action::ProfileEdit { autoconnect: Some(false), ..Default::default() };
        let typed = "a key typed in connect is not saved yet; join with the right key, or restart NetworkManager";
        let unsaved = "the profile has unsaved changes; save them, or restart NetworkManager";
        for (kind, flags, expected) in [(Wifi, 0x1, Err(typed)), (Ethernet, 0x1, Err(unsaved)), (Wifi, 0x3, Ok(()))] {
            controller.profiles.lock().unwrap().list[0].1.kind = kind;
            peer.object_server().interface::<_, SavedProfile>(HOME).await.unwrap().get_mut().await.2 = flags;
            let result = controller.update_profile("h-1", &off).await;
            assert_eq!(result, expected.map_err(String::from), "{kind:?} flags {flags}");
        }
        assert!(within(asked.recv()).await.unwrap().starts_with(&format!("{HOME} Update2")));
        assert!(asked.try_recv().is_err(), "only the generated profile was written");
    }

    /// A version mismatch reads the profile again once; NetworkManager's message names a bad setting.
    #[tokio::test]
    async fn a_refused_update_says_why_and_a_concurrent_edit_is_retried_once() {
        let mismatch = || Refused::VersionIdMismatch("changed".into());
        let raced = format!("{HOME} Update2(65, Some(7), autoconnect Some(false))");
        let first = format!("{HOME} Update2(65, Some(7), autoconnect None)");
        let cases = [
            (vec![mismatch()], Ok(()), vec![first.clone(), raced.clone()]),
            (vec![mismatch(), mismatch()], Err("the profile changed meanwhile; try again"), vec![first.clone(), raced]),
            (vec![Refused::PermissionDenied("polkit".into())], Err("not authorized"), vec![first.clone()]),
            (vec![Refused::InvalidProperty("ipv4.dns: bad".into())], Err("ipv4.dns: bad"), vec![first]),
        ];
        let priority = shared::action::ProfileEdit { autoconnect_priority: Some(5), ..Default::default() };
        for (refusals, expected, calls) in cases {
            let (controller, mut asked, _events, peer) = fixture().await;
            peer.object_server().interface::<_, SavedProfile>(HOME).await.unwrap().get_mut().await.3 = refusals;
            let result = controller.update_profile("h-1", &priority).await;
            assert_eq!(result, expected.map_err(String::from));
            assert_eq!(std::iter::from_fn(|| asked.try_recv().ok()).collect::<Vec<_>>(), calls);
        }
        let (controller, mut asked, _events, _peer) = fixture().await;
        assert_eq!(controller.update_profile("h-1", &Default::default()).await, Ok(()));
        assert!(asked.try_recv().is_err(), "an empty edit writes nothing");
    }

    /// A plain rebuild reads no activations; a changed activation or a restarted NetworkManager does.
    #[tokio::test]
    async fn profiles_reread_activations_only_after_one_changed() {
        let (mut controller, _asked, _events, peer) = fixture().await;
        let (states, mut pushed) = unbounded_channel();
        controller.profile_states = states;
        let flags = |state: shared::state::network_profiles::NetworkProfilesState| {
            state.profiles.into_iter().map(|p| (p.uuid, p.active, p.activating)).collect::<Vec<_>>()
        };
        let read = vec![("h-1".to_string(), false, false), ("u-1".to_string(), true, false)];
        controller.handle_signal(NetworkSignal::Changed).await;
        assert!(pushed.try_recv().is_err(), "a strength tick pushes no profiles");
        controller.handle_signal(NetworkSignal::Restarted).await;
        assert_eq!(flags(pushed.try_recv().unwrap()), read);
        controller.profiles.lock().unwrap().active.clear();
        controller.activity.store(true, std::sync::atomic::Ordering::Relaxed);
        controller.handle_signal(NetworkSignal::Changed).await;
        assert_eq!(flags(pushed.try_recv().unwrap()), read);

        peer.object_server().remove::<FakeActive, _>(ACTIVE).await.unwrap();
        let reconnecting = FakeActive { state: proxies::ACTIVE_STATE_ACTIVATING, then: None };
        peer.object_server().at(ACTIVE, reconnecting).await.unwrap();
        let tick = controller.handle_signal(NetworkSignal::Changed).await;
        assert!(tick.vpns[0].active && !tick.vpns[0].activating, "a strength tick reads no activations");

        let mut profiles = controller.profiles.lock().unwrap();
        profiles.active.push(("h-1".into(), proxies::ACTIVE_STATE_ACTIVATING));
        assert_eq!(flags(profiles.snapshot())[0], ("h-1".to_string(), false, true));
    }

    /// A gone activation is a settled one, and a Wi-Fi device's reason still says why at once:
    /// NetworkManager unexports a failed activation before its reply arrives. Only
    /// NetworkManager's own error names for a missing profile or interface are rephrased.
    #[tokio::test]
    async fn a_vanished_activation_says_why_at_once() {
        let (controller, _asked, _events, peer) = fixture().await;
        peer.object_server().remove::<FakeActive, _>(ACTIVE).await.unwrap();
        peer.object_server().interface::<_, Nm>(NM).await.unwrap().get_mut().await.1 = Some(proxies::REASON_NO_SECRETS);
        let activate = || within(controller.activate_profile("h-1", None, tokio::sync::oneshot::channel().0));
        assert_eq!(activate().await, Err("the saved password is missing or wrong; join with connect".into()));
        controller.profiles.lock().unwrap().list[0].1.kind = shared::state::network_profiles::ProfileKind::Ethernet;
        assert_eq!(activate().await, Err("connection failed".into()), "a Wi-Fi device's reason is not ethernet's");

        assert_eq!(controller.reapply_profile("h-1", Some("wlan9")).await, Err("no interface wlan9".into()));
        assert_eq!(controller.reapply_profile("h-1", Some("polkit")).await, Err("not authorized".into()));
        controller.profiles.lock().unwrap().list[0].1.uuid = "gone".into();
        assert_eq!(controller.delete_profile("gone").await, Err("no such profile".into()));
    }

    /// A missing key sends a Wi-Fi profile to `connect`; other kinds get the plain verdict, and a
    /// deactivation meanwhile is none. A stuck activation is stopped at the ceiling. Only a
    /// profile's own activations stop, and only where it is applied does it reapply.
    #[tokio::test]
    async fn an_activation_says_why_it_failed_and_stops_what_it_started() {
        use shared::state::network_profiles::ProfileKind;
        let (controller, mut asked, _events, peer) = fixture().await;
        peer.object_server().remove::<FakeActive, _>(ACTIVE).await.unwrap();
        let failed = FakeActive { state: proxies::ACTIVE_STATE_DEACTIVATED, then: None };
        peer.object_server().at(ACTIVE, failed).await.unwrap();
        let nm = peer.object_server().interface::<_, Nm>(NM).await.unwrap();
        nm.get_mut().await.1 = Some(proxies::REASON_NO_SECRETS);
        let activate = || controller.activate_profile("h-1", None, tokio::sync::oneshot::channel().0);
        let password = "the saved password is missing or wrong; join with connect";
        assert_eq!(activate().await, Err(password.into()));
        controller.profiles.lock().unwrap().list[0].1.kind = ProfileKind::Ethernet;
        assert_eq!(activate().await, Err("wrong password".into()));
        nm.get_mut().await.1 = Some(proxies::REASON_USER_REQUESTED);
        assert_eq!(activate().await, Ok(()));
        assert_eq!(controller.deactivate_profile("h-1").await, Ok(()));
        let reapplied = controller.reapply_profile("u-1", Some("wlan0")).await;
        assert_eq!(reapplied, Err("the profile is not active on wlan0".into()));
        // Without an interface: where `u-1`'s activation runs, and only if it is applied there.
        assert_eq!(controller.reapply_profile("h-1", None).await, Err("the profile is not active".into()));
        assert_eq!(controller.reapply_profile("u-1", None).await, Err("the profile is not active".into()));
        let calls: Vec<_> = std::iter::from_fn(|| asked.try_recv().ok()).collect();
        assert_eq!(calls, vec![format!("{NM} ActivateConnection({HOME}, /, /)"); 3], "u-1 alone is active");
        peer.object_server().interface::<_, Device>(WLAN0).await.unwrap().get_mut().await.1 = "u-1";
        assert_eq!(controller.reapply_profile("u-1", None).await, Ok(()));
        assert_eq!(asked.try_recv().ok(), Some(format!("{WLAN0} Reapply(0, 7)")));

        nm.get_mut().await.1 = None;
        peer.object_server().interface::<_, FakeActive>(ACTIVE).await.unwrap().get_mut().await.state =
            proxies::ACTIVE_STATE_ACTIVATING;
        tokio::time::pause();
        assert_eq!(activate().await, Err("connection timed out".into()));
        let calls: Vec<_> = std::iter::from_fn(|| asked.try_recv().ok()).collect();
        assert_eq!(calls.last(), Some(&format!("{NM} DeactivateConnection({ACTIVE})")));
    }

    /// An activation, VPN or not, ends its turn once NetworkManager takes the request: the next
    /// `_profile` action runs while the first still waits for a verdict.
    #[tokio::test]
    async fn an_activation_ends_its_turn_after_networkmanager_replies() {
        let (controller, mut asked, _events, peer) = fixture().await;
        peer.object_server().interface::<_, FakeActive>(ACTIVE).await.unwrap().get_mut().await.state =
            proxies::ACTIVE_STATE_ACTIVATING;
        let dispatch = dispatcher(Capability::Network, dispatch, &controller);
        for (uuid, profile) in [("h-1", HOME), ("u-1", VPN)] {
            dispatch("activate_profile", vec![uuid.into()]);
            dispatch("delete_profile", vec![uuid.into()]);
            assert_eq!(within(asked.recv()).await, Some(format!("{NM} ActivateConnection({profile}, /, /)")));
            assert_eq!(within(asked.recv()).await, Some(format!("{profile} Delete")));
        }
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
        let (controller, _events, _peer, declined) = prompted().await;

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

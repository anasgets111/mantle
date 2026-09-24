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
//!
//! ponytail: only the first Wi-Fi device from `GetAllDevices` is tracked. Multiple adapters need a
//! device selector in `available_networks`/`scan`/`connect`; none exists.

use serde::Serialize;
use zbus::zvariant::ObjectPath;

mod connect;
mod controller;
mod devices;
mod intent;
mod profiles;
mod proxies;
mod scan;

pub use controller::NetworkController;

/// One scanned network in `available_networks`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AccessPointInfo {
    /// Network name, `""` for hidden networks; one entry per SSID, from its strongest access point.
    pub ssid: String,
    /// Signal strength, `0` to `100`.
    pub strength: u8,
    /// Needs a key: WEP, WPA or RSN.
    pub secure: bool,
    /// `"2.4 GHz"`, `"5 GHz"`, `"6 GHz"`, or empty for a frequency outside those bands.
    pub band: String,
    /// The Wi-Fi device is associated with this SSID.
    pub active: bool,
    /// A saved NetworkManager profile names this SSID, so `connect` asks for no password.
    pub saved: bool,
}

/// A failed join, as `connect_error`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct JoinError {
    /// The network the join was for.
    pub ssid: String,
    /// Display text, such as `"wrong password"` or `"network not found"`.
    pub message: String,
}

/// `mantle.network`'s payload (ADR-0037).
// Re-derived from NetworkManager on each `NetworkSignal` (ADR-0029).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NetworkState {
    /// A scan is in flight, from the moment `scan` is accepted.
    pub scanning: bool,
    /// A connection carries the default route; `false` means offline.
    pub connected: bool,
    /// `"Ethernet"` when the default route is wired, else the associated SSID, else `nil`. An
    /// association still getting an address has an `ssid` while `connected` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// The associated network's `strength`, `0` to `100`; `0` without a Wi-Fi association.
    pub strength: u8,
    /// Wi-Fi radio power (`WirelessEnabled`); can be `true` with no Wi-Fi hardware, see `wifi_present`.
    pub wifi_enabled: bool,
    /// A Wi-Fi device exists.
    pub wifi_present: bool,
    /// At least one wired device exists, cable or not.
    pub ethernet_present: bool,
    /// NetworkManager networking is on (`NetworkingEnabled`).
    pub networking_enabled: bool,
    /// A wired device is activated; `set_ethernet_enabled`'s read-back, unlike carrier.
    pub ethernet_enabled: bool,
    /// The Wi-Fi device's IPv4 address without prefix, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_ip: Option<String>,
    /// The first activated wired device's IPv4 address without prefix, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethernet_ip: Option<String>,
    /// That wired device's link speed in Mb/s; `0` when unknown or none is activated.
    pub ethernet_speed: u32,
    /// The SSID `connect` is joining, or `nil`; clears on a verdict or `abort_connect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connecting_ssid: Option<String>,
    /// The last failed `connect`, or `nil` before any or after a success. Kept until the next
    /// `connect`, `cancel_connect` or `abort_connect`; check its `ssid` before showing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<JoinError>,
    /// The SSID whose `connect` waits for a password from a `network`/`connect` secure field, or
    /// `nil`. Also set after a rejected key; cleared when a join starts or by `cancel_connect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_ssid: Option<String>,
    /// NetworkManager's visible networks, re-read on every change: one per SSID, at most 20, ordered
    /// associated, then saved, then strongest. `{}` without Wi-Fi hardware.
    pub available_networks: Vec<AccessPointInfo>,
}

/// A pending `network:connect(ssid, hidden)` intent, stashed in the controller (ADR-0037) with
/// the same single-slot semantics the PAM one-shot protocol uses (ADR-0028), until paired
/// `secure_submit(network, connect)` supplies password bytes (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingNetworkConnect {
    pub ssid: String,
    pub hidden: bool,
}

/// What forwarders report to the network worker; `build_state` makes the payload with a fresh D-Bus
/// round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSignal {
    /// Any non-`scanning` field change: AP set, association, device state, or radio. All trigger
    /// the same full re-derive (ADR-0029), so one variant is enough.
    Changed,
    /// `LastScan` changed, or NetworkManager refused `RequestScan`. Either way no scan is in flight.
    ScanCompleted,
    /// Sent by [`NetworkController::mark_scanning`] before `RequestScan` completes, through
    /// the same channel for FIFO ordering.
    ScanStarted,
    /// A saved profile was added or removed, so the saved-SSID cache is stale.
    SavedChanged,
    /// NetworkManager added or removed a device, so the device set is stale.
    DevicesChanged,
}

fn root_object_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("\"/\" is always a valid D-Bus object path")
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NetworkAction {
    /// Turns NetworkManager networking on or off.
    SetNetworkingEnabled { enabled: bool },
    /// Powers the Wi-Fi radio.
    SetWifiEnabled { enabled: bool },
    /// `false` disconnects every wired device; `true` activates each one's autoconnect profile, and a
    /// device without one stays down.
    SetEthernetEnabled { enabled: bool },
    /// Requests a Wi-Fi scan; a no-op without Wi-Fi hardware.
    Scan,
    /// Joins a network. Without a saved profile, a secured, `hidden` or out-of-range one sets
    /// `password_ssid` and waits for a key.
    Connect { ssid: String, hidden: bool },
    /// Drops the password request `password_ssid` names; a join already running continues.
    CancelConnect,
    /// Stops the join `connecting_ssid` names, deleting a profile the join created.
    AbortConnect,
    /// Deletes every saved profile for this SSID.
    Forget { ssid: String },
    /// Disconnects Wi-Fi; NetworkManager does not autoconnect it again until the next join.
    DisconnectWifi,
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
            controller.mark_scanning();
            tokio::spawn(async move { controller.scan().await });
        }
        NetworkAction::Connect { ssid, hidden } => {
            controller.stash_connect_intent(PendingNetworkConnect { ssid, hidden });
            tokio::spawn(async move { controller.resolve_connect_intent().await });
        }
        // Not spawned: it touches no D-Bus, and a late cancel would resurrect the prompt.
        NetworkAction::CancelConnect => controller.cancel_connect(),
        NetworkAction::AbortConnect => controller.abort_connect(),
        NetworkAction::Forget { ssid } => {
            tokio::spawn(async move { controller.forget(&ssid).await });
        }
        NetworkAction::DisconnectWifi => {
            tokio::spawn(async move { controller.disconnect_wifi().await });
        }
    }
}

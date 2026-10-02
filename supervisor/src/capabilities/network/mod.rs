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

pub use controller::NetworkController;
use shared::action::NetworkAction;

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

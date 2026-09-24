//! BlueZ Bluetooth D-Bus controller (`mantle.bluetooth`; ADR-0030).
//!
//! Proxies follow BlueZ's D-Bus API docs; `org.freedesktop.DBus.ObjectManager` reuses
//! `zbus::fdo::ObjectManagerProxy` (ADR-0030: no maintained BlueZ proxy crate).
//!
//! ponytail: [`BluetoothController::new`] degrades instead of failing like `NetworkController::new`
//! (`zbus::Result<Self>`). NetworkManager is assumed present for `mantle.network`; BlueZ may be
//! absent with no hardware or no `bluetoothd`, so binding, adapter lookup, and agent registration
//! log and produce an inert controller: `enabled`/`discovering` are `false`, lists are empty, and
//! writes log and no-op.
//!
//! ponytail: an adapter BlueZ adds later is picked up, but a `bluetoothd` that starts after the
//! Supervisor is not, because the `ObjectManager` binding and agent registration happen once.
//! Upgrade path: watch `org.bluez`'s `NameOwnerChanged` and rebuild.
//!
//! ponytail: After `start_discovery` clears the list and pushes a fresh snapshot, matching the
//! `last_snapshots` bookkeeping used by every capability, any
//! `DeviceRegistryChanged` rebuilds both lists from the entire registry, not only devices newly
//! seen this session, matching `NetworkController::build_available_networks`'s no-debounce
//! discipline. A prior-session device can reappear after any registry change because BlueZ
//! `Device1` objects persist and this controller does not track `RSSI`. This follows ADR-0030's
//! deferred mechanics; a strict session-scoped list is the upgrade if hardware shows it wrong.

use serde::Serialize;

/// Object path where this Supervisor exports `org.bluez.Agent1` on its unique connection name.
pub const AGENT_OBJECT_PATH: &str = "/org/mantle/Bluez/Agent1";

pub mod agent;
pub mod controller;
pub mod proxies;
pub mod registry;

pub use controller::BluetoothController;

// State shape pushed as `mantle.bluetooth`'s StateSnapshot.
// ---------------------------------------------------------------------------------------------

/// What the shell is doing to a device, as its `busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeviceAction {
    Pairing,
    Connecting,
    Disconnecting,
}

/// What a `pairing_request` asks; see `PairingRequest.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PairingKind {
    Confirm,
    Authorize,
    Service,
    Display,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ConnectedDevice {
    /// MAC address, e.g. `"00:1A:7D:DA:71:11"`; every `bluetooth` action takes it.
    pub mac: String,
    /// The device's advertised name, or empty.
    pub name: String,
    /// Battery percentage, or `-1` when the device reports none.
    pub battery: i32,
    /// From the class of device: `"keyboard"`, `"mouse"`, `"headphones"`, `"headset"`, `"phone"`,
    /// `"computer"` or `"generic"`.
    pub category: String,
    /// Same as [`DiscoveredDevice::busy`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<DeviceAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PairedDevice {
    /// MAC address, the argument of `connect` and `forget`.
    pub mac: String,
    /// The device's advertised name, or empty.
    pub name: String,
    /// Same set as `ConnectedDevice.category`.
    pub category: String,
    /// BlueZ refuses every connection to or from the device until it is unblocked.
    pub blocked: bool,
    /// Same as [`DiscoveredDevice::busy`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<DeviceAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiscoveredDevice {
    /// MAC address, the argument of `pair`.
    pub mac: String,
    /// Advertised name, often empty when the device broadcasts only an address.
    pub name: String,
    /// Always `false`.
    pub paired: bool,
    /// BlueZ refuses to pair with or connect to the device until it is unblocked.
    pub blocked: bool,
    /// The action this shell is running on the device, or `nil`; another client's never shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<DeviceAction>,
}

/// What the pairing agent is asking the user.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PairingRequest {
    /// `"confirm"`: does the device show `code`? `"authorize"`: may it pair? `"service"`: may a
    /// paired, untrusted device connect? `"display"`: type `code` on the device; nothing to answer.
    pub kind: PairingKind,
    /// The device's MAC address.
    pub mac: String,
    /// The device's advertised name, or empty.
    pub name: String,
    /// Six-digit passkey for `"confirm"`, passkey or PIN for `"display"`, else `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct BluetoothState {
    /// BlueZ has an adapter; `false` without one or without `bluetoothd`.
    pub available: bool,
    /// The adapter is powered.
    pub enabled: bool,
    /// The adapter is scanning, whichever client started it.
    pub discovering: bool,
    /// Other devices can find this adapter. BlueZ turns it off after `DiscoverableTimeout` (180 s by default).
    pub discoverable: bool,
    /// Paired, connected devices. Unordered and may reshuffle on any push: sort before drawing.
    pub connected_devices: Vec<ConnectedDevice>,
    /// Paired devices that are not connected. Unordered like `connected_devices`.
    pub paired_devices: Vec<PairedDevice>,
    /// Unpaired devices BlueZ knows, unordered. Kept after `stop_discovery`; BlueZ expires unseen
    /// temporary ones after `TemporaryTimeout` (30 s by default).
    pub discovered_devices: Vec<DiscoveredDevice>,
    /// The pairing question to show, or `nil`. Answer with `answer_pairing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_request: Option<PairingRequest>,
}

/// What signal forwarders report to the bluetooth worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothSignal {
    /// The adapter's own `Powered`, `Discovering` or `Discoverable` property changed.
    AdapterChanged,
    /// A device was added/removed, or its `Connected`/`Paired`/`Name`/`Blocked`/
    /// `Battery1.Percentage` changed.
    DeviceRegistryChanged,
    /// Sent by [`BluetoothController::clear_discovered`], not a forwarder, when
    /// `bluetooth:start_discovery()` begins. It clears `discovered_devices` before
    /// `StartDiscovery` returns (ADR-0030); a distinct variant prevents registry re-derivation
    /// from immediately undoing the clear.
    DiscoveryCleared,
    /// The agent put up or took down a pairing request.
    PairingChanged,
}

/// Maps BlueZ `Class` bits 8-12 (Major) and 2-7 (Minor) (ADR-0030). Parses them instead of
/// trusting `Icon`, which is empty when `Class == 0`, common for BLE peripherals before GAP data.
fn class_to_category(class: u32) -> &'static str {
    let major = (class >> 8) & 0x1F;
    let minor = (class >> 2) & 0x3F;
    match major {
        0x01 => "computer",
        0x02 => "phone",
        0x04 => match minor {
            0x01 | 0x02 => "headset",
            0x06 => "headphones",
            _ => "generic",
        },
        0x05 => match (minor >> 4) & 0x3 {
            0b01 => "keyboard",
            0b10 => "mouse",
            0b11 => "keyboard",
            _ => "generic",
        },
        _ => "generic",
    }
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BluetoothAction {
    /// Powers the adapter on or off.
    SetEnabled { enabled: bool },
    /// Makes the adapter findable by other devices, or not.
    SetDiscoverable { discoverable: bool },
    /// Clears `discovered_devices` and scans. The request holds, so a scan starts once the adapter
    /// powers on and pauses while a `pair` runs.
    StartDiscovery,
    /// Stops discovery; `discovered_devices` stays.
    StopDiscovery,
    /// Pairs a discovered device, then trusts and connects it.
    Pair { mac: String },
    /// Trusts and connects a paired device.
    Connect { mac: String },
    /// Disconnects a connected device.
    Disconnect { mac: String },
    /// Removes a device from BlueZ, unpairing it.
    Forget { mac: String },
    /// Accepts or rejects the `pairing_request` for `mac`; a yes within 750 ms of it appearing is
    /// ignored.
    AnswerPairing { mac: String, accept: bool },
}

/// `mantle.bluetooth` action dispatch (ADR-0037): `tokio::spawn`s each write action rather than
/// awaiting inline (ADR-0030). `stop_discovery` leaves the last `discovered_devices` snapshot.
pub fn dispatch(controller: &BluetoothController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<BluetoothAction>(&envelope.params) else { return };
    let controller = controller.clone();
    match action {
        BluetoothAction::SetEnabled { enabled } => {
            tokio::spawn(async move { controller.set_enabled(enabled).await });
        }
        BluetoothAction::SetDiscoverable { discoverable } => {
            tokio::spawn(async move { controller.set_discoverable(discoverable).await });
        }
        // Not spawned: the intent is stored in dispatch order, so a quick start then stop ends
        // wanting none. The reconcile they trigger is spawned.
        BluetoothAction::StartDiscovery => {
            controller.clear_discovered();
            controller.set_discovery(true);
        }
        BluetoothAction::StopDiscovery => controller.set_discovery(false),
        // Not spawned: it only answers a waiting agent call, and a late answer could land on the
        // next prompt.
        BluetoothAction::AnswerPairing { mac, accept } => controller.answer_pairing(&mac, accept),
        BluetoothAction::Pair { mac } => {
            tokio::spawn(async move { controller.pair(&mac).await });
        }
        BluetoothAction::Connect { mac } => {
            tokio::spawn(async move { controller.connect(&mac).await });
        }
        BluetoothAction::Disconnect { mac } => {
            tokio::spawn(async move { controller.disconnect(&mac).await });
        }
        BluetoothAction::Forget { mac } => {
            tokio::spawn(async move { controller.forget(&mac).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- class_to_category ----

    #[test]
    fn class_to_category_reads_only_the_major_and_minor_bits() {
        for (class, category, case) in [
            (0x00_2540, "keyboard", "a real keyboard"),
            (0x00_2580, "mouse", "a real mouse"),
            ((0x05 << 8) | (0b11_0000 << 2), "keyboard", "a combo keyboard and pointing device"),
            (0x05 << 8, "generic", "a peripheral the spec leaves uncategorized"),
            (0x01 << 8, "computer", "major 0x01"),
            (0x02 << 8, "phone", "major 0x02"),
            ((0x04 << 8) | (0x01 << 2), "headset", "audio/video minor 0x01"),
            ((0x04 << 8) | (0x02 << 2), "headset", "audio/video minor 0x02"),
            ((0x04 << 8) | (0x06 << 2), "headphones", "audio/video minor 0x06"),
            ((0x04 << 8) | (0x03 << 2), "generic", "another audio/video minor"),
            (0x03 << 8, "generic", "LAN/Network Access Point, not a drawn category"),
            (0x24_0404, "headset", "a real headset, whose Service Class bits sit above bit 12"),
        ] {
            assert_eq!(class_to_category(class), category, "{case}");
        }
    }
}

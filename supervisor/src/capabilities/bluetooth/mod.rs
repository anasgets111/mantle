//! BlueZ Bluetooth D-Bus controller (`mantle.bluetooth`; ADR-0030).
//!
//! Proxies follow BlueZ's D-Bus API docs; `org.freedesktop.DBus.ObjectManager` reuses
//! `zbus::fdo::ObjectManagerProxy` (ADR-0030: no maintained BlueZ proxy crate).
//!
//! ponytail: [`BluetoothController::new`] degrades to an inert controller (logs, empty lists, no-op
//! writes) when BlueZ is absent, unlike `NetworkController::new`, which assumes NetworkManager.
//!
//! ponytail: any `DeviceRegistryChanged` rebuilds the lists from the whole registry, so a
//! prior-session device can reappear after `start_discovery` clears them, because BlueZ `Device1`
//! objects persist and RSSI is not tracked (ADR-0030); upgrade to a session-scoped list if hardware
//! shows it wrong.

pub use shared::state::bluetooth::{
    BluetoothState, ConnectedDevice, DeviceAction, DeviceCategory, DiscoveredDevice, PairedDevice, PairingKind,
    PairingRequest,
};

use shared::action::BluetoothAction;

/// Object path where this Supervisor exports `org.bluez.Agent1` on its unique connection name.
pub const AGENT_OBJECT_PATH: &str = "/org/mantle/Bluez/Agent1";

pub mod agent;
pub mod controller;
pub mod proxies;
pub mod registry;

pub use controller::BluetoothController;

/// What signal forwarders report to the bluetooth worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothSignal {
    /// The adapter's own `Powered`, `Discovering` or `Discoverable` property changed.
    AdapterChanged,
    /// A device was added/removed, or its `Connected`/`Paired`/`Alias`/`Class`/`Blocked`/
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
fn class_to_category(class: u32) -> DeviceCategory {
    let major = (class >> 8) & 0x1F;
    let minor = (class >> 2) & 0x3F;
    match major {
        0x01 => DeviceCategory::Computer,
        0x02 => DeviceCategory::Phone,
        0x04 => match minor {
            0x01 | 0x02 => DeviceCategory::Headset,
            0x06 => DeviceCategory::Headphones,
            _ => DeviceCategory::Generic,
        },
        0x05 => match (minor >> 4) & 0x3 {
            0b01 | 0b11 => DeviceCategory::Keyboard,
            0b10 => DeviceCategory::Mouse,
            _ => DeviceCategory::Generic,
        },
        _ => DeviceCategory::Generic,
    }
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

    #[test]
    fn class_to_category_reads_only_the_major_and_minor_bits() {
        for (class, category, case) in [
            (0x00_2540, DeviceCategory::Keyboard, "a real keyboard"),
            (0x00_2580, DeviceCategory::Mouse, "a real mouse"),
            ((0x05 << 8) | (0b11_0000 << 2), DeviceCategory::Keyboard, "a combo keyboard and pointing device"),
            (0x05 << 8, DeviceCategory::Generic, "a peripheral the spec leaves uncategorized"),
            (0x01 << 8, DeviceCategory::Computer, "major 0x01"),
            (0x02 << 8, DeviceCategory::Phone, "major 0x02"),
            ((0x04 << 8) | (0x01 << 2), DeviceCategory::Headset, "audio/video minor 0x01"),
            ((0x04 << 8) | (0x02 << 2), DeviceCategory::Headset, "audio/video minor 0x02"),
            ((0x04 << 8) | (0x06 << 2), DeviceCategory::Headphones, "audio/video minor 0x06"),
            ((0x04 << 8) | (0x03 << 2), DeviceCategory::Generic, "another audio/video minor"),
            (0x03 << 8, DeviceCategory::Generic, "LAN/Network Access Point, not a drawn category"),
            (0x24_0404, DeviceCategory::Headset, "a real headset, whose Service Class bits sit above bit 12"),
        ] {
            assert_eq!(class_to_category(class), category, "{case}");
        }
    }
}

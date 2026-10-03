//! `mantle.bluetooth` snapshot payload.

use serde::Serialize;

/// What the shell is doing to a device, as its `busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeviceAction {
    Pairing,
    Connecting,
    Disconnecting,
}

/// What a `pairing_request` asks; see `PairingRequest.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PairingKind {
    Confirm,
    Authorize,
    Service,
    Display,
    PinEntry,
    PasskeyEntry,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConnectedDevice {
    /// MAC address, e.g. `"00:1A:7D:DA:71:11"`; every `bluetooth` action takes it.
    pub mac: String,
    /// The device's advertised name, or empty.
    pub name: String,
    /// Battery percentage, or `nil` when the device reports none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<u8>,
    /// From the class of device: `"keyboard"`, `"mouse"`, `"headphones"`, `"headset"`, `"phone"`,
    /// `"computer"` or `"generic"`.
    pub category: String,
    /// Same as `DiscoveredDevice.busy`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<DeviceAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PairedDevice {
    /// MAC address, the argument of `connect` and `forget`.
    pub mac: String,
    /// The device's advertised name, or empty.
    pub name: String,
    /// Same set as `ConnectedDevice.category`.
    pub category: String,
    /// BlueZ refuses every connection to or from the device until it is unblocked.
    pub blocked: bool,
    /// Same as `DiscoveredDevice.busy`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<DeviceAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiscoveredDevice {
    /// MAC address, the argument of `pair`.
    pub mac: String,
    /// Advertised name, often empty when the device broadcasts only an address.
    pub name: String,
    /// BlueZ refuses to pair with or connect to the device until it is unblocked.
    pub blocked: bool,
    /// The action this shell is running on the device, or `nil`; another client's never shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<DeviceAction>,
}

/// What the pairing agent is asking the user.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PairingRequest {
    /// `"confirm"`: does the device show `code`? `"authorize"`: may it pair? `"service"`: may a
    /// paired, untrusted device connect? `"display"`: type `code` on the device; nothing to answer.
    /// `"pin_entry"` and `"passkey_entry"`: type a secret in a Bluetooth `secure_submit` field.
    pub kind: PairingKind,
    /// Unique for this Supervisor session. Include it with the device MAC in a secure entry target.
    pub id: String,
    /// The device's MAC address.
    pub mac: String,
    /// The device's advertised name, or empty.
    pub name: String,
    /// Six-digit passkey for `"confirm"`, passkey or PIN for `"display"`, else `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// The pairing question to show, or `nil`. Entry requests use `secure_submit`;
    /// confirmation, authorization and service requests use `answer_pairing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_request: Option<PairingRequest>,
}

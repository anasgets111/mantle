//! `mantle.network` snapshot payload.

use serde::Serialize;

/// One scanned network in `available_networks`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// A saved NetworkManager profile compatible with this device names this SSID.
    pub saved: bool,
}

/// A failed join, as `connect_error`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JoinError {
    /// The network the join was for.
    pub ssid: String,
    /// Display text, such as `"wrong password"` or `"network not found"`.
    pub message: String,
}

/// One Wi-Fi interface, with its own scan, join, address and access points. `connected` means
/// activated here, while the flat `connected` means a default route; `ssid` never uses `"Ethernet"`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WifiDeviceInfo {
    /// Interface name, such as `"wlan0"`; pass it to actions ending in `_device`.
    pub id: String,
    /// A scan is in flight on this interface.
    pub scanning: bool,
    /// This interface has an activated connection.
    pub connected: bool,
    /// Associated SSID, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// Associated access point strength, `0` to `100`.
    pub strength: u8,
    /// IPv4 address without prefix, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_ip: Option<String>,
    /// SSID of this interface's current join, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connecting_ssid: Option<String>,
    /// Failed join on this interface, or `nil`; cleared by the next join or `cancel_connect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<JoinError>,
    /// SSID awaiting a password on this interface, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_ssid: Option<String>,
    /// Visible networks on this interface: one per SSID, at most 20, ordered associated,
    /// then saved, then strongest.
    pub available_networks: Vec<AccessPointInfo>,
}

/// `mantle.network`'s payload (ADR-0037).
// Re-derived from NetworkManager on each `NetworkSignal` (ADR-0029).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetworkState {
    /// Every Wi-Fi interface, primary first. IDs are interface names, never NetworkManager object paths.
    pub wifi_devices: Vec<WifiDeviceInfo>,
    /// A scan is in flight on the primary Wi-Fi device.
    pub scanning: bool,
    /// A connection carries the default route; `false` means offline.
    pub connected: bool,
    /// `"Ethernet"` when the default route is wired, else the associated SSID, else `nil`. An
    /// association still getting an address has an `ssid` while `connected` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// The primary Wi-Fi device's associated network strength, `0` to `100`.
    pub strength: u8,
    /// Wi-Fi radio power (`WirelessEnabled`); can be `true` with no Wi-Fi hardware, see `wifi_present`.
    pub wifi_enabled: bool,
    /// At least one Wi-Fi device exists.
    pub wifi_present: bool,
    /// At least one wired device exists, cable or not.
    pub ethernet_present: bool,
    /// NetworkManager networking is on (`NetworkingEnabled`).
    pub networking_enabled: bool,
    /// A wired device is activated; `set_ethernet_enabled`'s read-back, unlike carrier.
    pub ethernet_enabled: bool,
    /// The primary Wi-Fi device's IPv4 address without prefix, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_ip: Option<String>,
    /// The first activated wired device's IPv4 address without prefix, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethernet_ip: Option<String>,
    /// That wired device's link speed in Mb/s; `nil` when unknown or none is activated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethernet_speed: Option<u32>,
    /// The current join's SSID on any Wi-Fi device, or `nil`; clears on a verdict or `abort_connect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connecting_ssid: Option<String>,
    /// The last failed join on any Wi-Fi device, or `nil` before any or after a success. Kept until the next
    /// `connect`, `cancel_connect` or `abort_connect`; check its `ssid` before showing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<JoinError>,
    /// The join SSID awaiting a password from a `network`/`connect` secure field on any device, or
    /// `nil`. Also set after a rejected key; cleared when a join starts or by `cancel_connect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_ssid: Option<String>,
    /// The primary Wi-Fi device's visible networks: one per SSID, at most 20, ordered associated,
    /// then saved, then strongest. `{}` without Wi-Fi hardware.
    pub available_networks: Vec<AccessPointInfo>,
}

//! `mantle.network_profiles` snapshot payload.

use serde::{Deserialize, Serialize};

/// `profiles[].kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Ethernet,
    Wifi,
    /// A plugin VPN.
    Vpn,
    Wireguard,
    /// Any other type, such as a bridge or a mobile broadband profile.
    Other,
}

/// `connection.metered`, also `update_profile`'s `metered`.
// The discriminants are NetworkManager's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Metered {
    /// NetworkManager guesses from the device.
    Unknown = 0,
    Yes = 1,
    No = 2,
}

/// A profile's `ipv4` or `ipv6` settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IpSettings {
    /// NetworkManager's method: `"auto"`, `"manual"`, `"link-local"`, `"shared"`, `"disabled"`, and for IPv6
    /// also `"dhcp"` and `"ignore"`.
    pub method: String,
    /// Static addresses as `"address/prefix"`.
    pub addresses: Vec<String>,
    /// Static gateway, or `nil`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// Static DNS servers, in order. A DNS-over-TLS server reads as NetworkManager writes it, such as
    /// `"dns+tls://1.1.1.1#one.one.one.one"`.
    pub dns: Vec<String>,
    /// Search domains.
    pub dns_search: Vec<String>,
    /// Ignore DNS servers from DHCP or router advertisements and use only `dns`.
    pub ignore_auto_dns: bool,
}

/// One saved NetworkManager profile in `profiles`. Secrets never appear.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileInfo {
    /// Profile UUID; pass it to the `_profile` actions.
    pub uuid: String,
    /// Profile name.
    pub id: String,
    pub kind: ProfileKind,
    /// The Wi-Fi network name, or `nil`; `�` stands for bytes that are not UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// The interface the profile is bound to, or `nil` for any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    pub autoconnect: bool,
    /// Higher wins among profiles that can autoconnect on one device.
    pub autoconnect_priority: i32,
    pub metered: Metered,
    /// The profile is activated on some device.
    pub active: bool,
    /// The profile is being activated.
    pub activating: bool,
    /// `nil` when the profile has no IPv4 settings, as a VPN may not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<IpSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<IpSettings>,
}

/// `error.action`: the `_profile` action that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProfileAction {
    Update,
    Delete,
    Reapply,
    Activate,
    Deactivate,
}

/// A failed `_profile` action, as `error`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileError {
    /// The profile the action was for.
    pub uuid: String,
    pub action: ProfileAction,
    /// Display text, such as `"not authorized"`.
    pub message: String,
}

/// `mantle.network_profiles`' payload: saved NetworkManager profiles, pushed apart from `network` so a
/// signal-strength change does not resend them. `mantle.network`'s `_profile` actions edit them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetworkProfilesState {
    /// Every saved profile except loopback, ordered by name.
    pub profiles: Vec<ProfileInfo>,
    /// The last failed `_profile` action, or `nil`; a later success of the same action on the same
    /// profile clears it. VPN activation failures land in `network`'s `vpn_error` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProfileError>,
}

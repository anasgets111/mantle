//! `mantle.power` snapshot payload.

use serde::Serialize;

/// `mantle.power`'s payload. Profile fields are `nil` without power-profiles-daemon, the rest without UPower;
/// a failed read is also `nil`. With neither service the payload is an empty table.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PowerState {
    /// Active platform profile, e.g. `"balanced"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    /// Available profiles in daemon order, e.g. `{"power-saver", "balanced", "performance"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    /// UPower's `OnBattery`: running on battery rather than mains.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<bool>,
    /// UPower's display-device `EnergyRate` in watts; direction is `mantle.battery.state`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_rate: Option<f64>,
}

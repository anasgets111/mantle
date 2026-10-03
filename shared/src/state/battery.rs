//! `mantle.battery` snapshot payload.

use serde::Serialize;

/// `battery.state`: UPower's `Device.State` by name, e.g. `b.state == "pending_charge"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BatteryStatus {
    /// No answer: UPower unreachable, an unknown state number, or a display device that is not a battery.
    #[default]
    Unknown,
    /// Taking current from an adapter.
    Charging,
    /// Draining.
    Discharging,
    /// Flat.
    Empty,
    /// Charged and holding.
    FullyCharged,
    /// On mains, neither draining nor taking current: a charge limit, weak charger or thermal pause.
    PendingCharge,
    /// Waiting to discharge.
    PendingDischarge,
}

/// `mantle.battery`'s payload. No battery, or no UPower, reads `present = false` and defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatteryState {
    /// UPower's display device is a present battery. Check it before drawing the other fields.
    pub present: bool,
    /// UPower's `Percentage`, rounded to `0` to `100`; a spurious `0` while not draining keeps the last value.
    pub percent: u8,
    /// What the battery is doing; see `BatteryStatus`.
    pub state: BatteryStatus,
    /// Seconds until flat, or `nil` while UPower has no estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_empty: Option<u32>,
    /// Seconds until full, or `nil` while UPower has no estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_full: Option<u32>,
}

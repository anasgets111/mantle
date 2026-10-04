//! `mantle.battery` snapshot payload.

use serde::Serialize;

/// `battery.state`: UPower's `Device.State` by name, e.g. `b.state == "pending_charge"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BatteryStatus {
    /// No answer: UPower unreachable, an unknown state number, or a device without a battery state.
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

/// `mantle.battery`'s payload. Without a system battery, `present = false`; peripherals may remain.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatteryState {
    /// UPower's display device is a present battery. Check it before drawing system charge, state or time estimates.
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
    /// Battery health: full charge as a percent of design capacity, `0` to `100`, combined over the system
    /// batteries. `nil` when UPower reports no design capacity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u8>,
    /// UPower batteries outside the system supply, ordered by object path. This may include devices
    /// also shown by `mantle.bluetooth`.
    pub peripherals: Vec<PeripheralBattery>,
}

/// One UPower battery outside the system supply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PeripheralBattery {
    /// UPower object path; stable only while this UPower owner holds the device.
    pub id: String,
    /// UPower model, vendor, or object name.
    pub name: String,
    /// UPower device type, e.g. `"mouse"`, `"keyboard"`, `"headset"`.
    pub kind: String,
    /// Charge percentage, or `nil` when UPower reports a coarse level or has no usable reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    /// Coarse charge level, including `"unknown"`, or `nil` when UPower says to use `percent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// Charge state; `"unknown"` when UPower has no recognized value.
    pub state: BatteryStatus,
    /// UPower icon name, or `nil` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

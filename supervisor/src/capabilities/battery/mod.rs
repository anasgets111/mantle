//! `mantle.battery` reports presence, percentage, charge state, and time estimates from UPower's
//! `DisplayDevice`. Read-only, with no `dispatch`.
//!
//! Not a `/sys/class/power_supply` udev watch: it misses capacity changes the kernel does not
//! announce and cannot tell a reached charge limit from running on battery (ADR-0080).

pub mod controller;

pub use controller::{BatteryController, BatterySignal};

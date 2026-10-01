//! `mantle.brightness` snapshot payload.

use serde::Serialize;

/// `mantle.brightness`'s payload; the capability stays `nil` on a machine with no backlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BrightnessState {
    /// Screen backlight, `0` to `100`: the last requested level (sysfs `brightness`), not the mid-fade one.
    pub percent: u8,
}

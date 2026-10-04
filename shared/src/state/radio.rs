//! `mantle.radio` snapshot payload.

use serde::{Deserialize, Serialize};

/// A radio class, named as the kernel's rfkill type; discriminants are the kernel type ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum RadioKind {
    Wlan = 1,
    Bluetooth = 2,
    Uwb = 3,
    Wimax = 4,
    Wwan = 5,
    Gps = 6,
    Fm = 7,
    Nfc = 8,
}

impl RadioKind {
    /// The kind for a kernel rfkill type id; `None` for `0` (all) and types this list lacks.
    pub fn from_type(ty: u8) -> Option<Self> {
        Some(match ty {
            1 => Self::Wlan,
            2 => Self::Bluetooth,
            3 => Self::Uwb,
            4 => Self::Wimax,
            5 => Self::Wwan,
            6 => Self::Gps,
            7 => Self::Fm,
            8 => Self::Nfc,
            _ => return None,
        })
    }
}

/// One radio class and the rfkill block state of its devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Radio {
    pub kind: RadioKind,
    /// Software block, which `set_blocked` changes: any device of this kind is blocked.
    pub soft_blocked: bool,
    /// Hardware switch block, which software cannot clear: any device of this kind is blocked.
    pub hard_blocked: bool,
}

/// `mantle.radio`'s payload; the capability stays `nil` without `/dev/rfkill` or until a radio exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RadioState {
    /// One entry per kind with at least one device, ordered by kernel type id.
    pub radios: Vec<Radio>,
}

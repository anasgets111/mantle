//! `mantle.android_connect` snapshot payload.

use serde::{Deserialize, Serialize};

/// The device category reported by KDE Connect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    Phone,
    Tablet,
    Desktop,
    Laptop,
    Tv,
    #[default]
    Unknown,
}

impl DeviceType {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "phone" => Self::Phone,
            "tablet" => Self::Tablet,
            "desktop" => Self::Desktop,
            "laptop" => Self::Laptop,
            "tv" => Self::Tv,
            _ => Self::Unknown,
        }
    }
}

/// A remote Android or paired device known to the daemon.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AndroidDevice {
    /// Unique KDE Connect device identifier.
    pub id: String,
    /// Device's advertised or configured friendly name.
    pub name: String,
    /// Device form-factor / category.
    pub device_type: DeviceType,
    /// Whether the device has established mutual pairing.
    pub paired: bool,
    /// Whether the device is currently reachable on the network.
    pub reachable: bool,
    /// Whether the device's SFTP storage is currently mounted.
    pub mounted: bool,
    /// Why the last `browse` failed, such as missing `sshfs`. Clears on the next `browse` or a mount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browse_error: Option<String>,
    /// Plugins switched off for this device, by id: `battery`, `clipboard`, `connectivity_report`,
    /// `contacts`, `findmyphone`, `mpris`, `notification`, `ping`, `runcommand`, `sftp`, `share`,
    /// `sms`, `systemvolume`, `telephony`, or the daemon's default `mousepad` and `presenter`.
    pub disabled_plugins: Vec<String>,
    /// Whether every desktop clipboard change goes to this device. On unless switched off.
    pub clipboard_auto_share: bool,
    /// Whether an incoming pairing request from this device is awaiting confirmation. Clears on
    /// accept, reject or disconnect, or after 30 s: the daemon reports no cancel from the device.
    pub pairing_requested: bool,
    /// Whether a pairing request this PC sent is awaiting the device's answer. Clears when the
    /// device accepts, on `unpair`, on disconnect, or after 30 s: the daemon reports no rejection.
    pub pairing_sent: bool,
    /// Whether the device is reconnecting to apply a plugin switch. Clears when it connects again,
    /// or after 30 s.
    pub reconnecting: bool,
    /// Battery level percentage (0 to 100), or `nil` if unknown or unannounced. With two devices
    /// connected, the daemon can report one's value as the other's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<u8>,
    /// Whether the battery is charging, or `nil` if unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charging: Option<bool>,
    /// Cellular network signal strength (0 to 4 bars), or `nil` if not reported. With two devices
    /// connected, the daemon can report one's value as the other's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_strength: Option<u8>,
}

/// A command a paired device can run on this PC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AndroidCommand {
    /// Unique identifier, the key `remove_command` takes.
    pub id: String,
    /// Label the device shows.
    pub name: String,
    /// Shell command line this PC runs with `sh -c` when a device asks.
    pub command: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AndroidConnectState {
    /// Whether the KDE Connect daemon (`io.github.hepp3n.kdeconnect`) is running on the session bus.
    pub available: bool,
    /// Known devices. Unordered; sort before drawing.
    pub devices: Vec<AndroidDevice>,
    /// Commands paired devices can run on this PC with `sh -c`, from
    /// `~/.config/kdeconnect/runcommand.json`.
    pub commands: Vec<AndroidCommand>,
    /// Percent sent of the file transfer in progress, or `nil` when none runs. The daemon does not
    /// say which device it goes to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer_progress: Option<u8>,
}

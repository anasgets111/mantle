//! `mantle.audio` snapshot payload.

use serde::Serialize;

/// `mantle.audio`'s payload (ADR-0053).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioState {
    /// Default output volume in percent, `0` to `150`, loudest channel; louder writes by other clients
    /// are pulled back to `150`. `nil` with no sink or before its first volume report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Default output mute; `false` with no default sink or before its first report.
    pub muted: bool,
    /// Default output balance, `-1.0` (left) to `1.0` (right); `nil` with no sink, for mono or an unknown
    /// channel map.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f32>,
    /// Default input volume in percent; `set_source_volume` caps at `100`, another client may not.
    /// `nil` with no source or before its first volume report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_volume: Option<f32>,
    /// Default input (microphone) mute; `false` with no default source or before its first report.
    pub source_muted: bool,
    /// Every output device.
    pub sinks: Vec<AudioDevice>,
    /// Every input device.
    pub sources: Vec<AudioDevice>,
    /// Apps playing or recording audio, excluding pid-less streams, notification sounds, meters and monitor captures.
    pub apps: Vec<AppStream>,
    /// BlueZ audio devices PipeWire knows, with their codecs, ordered by `device`.
    pub bluetooth: Vec<BluetoothCodecs>,
}

/// One `sinks` or `sources` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioDevice {
    /// PipeWire node id, the argument of `set_default_sink`/`set_default_source`; not reboot-stable.
    pub id: u32,
    /// `node.description`, e.g. `"Built-in Audio Analog Stereo"`, else `node.nick`, else `node.name`.
    pub name: String,
    /// This is the default output or input; with no default known, or one not in this list, the lowest
    /// `id` is.
    pub active: bool,
    /// `device.icon-name` theme name, e.g. `"audio-card-analog"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The active card route's `port.type`, e.g. `"headphones"`, `"hdmi"`, `"mic"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    /// `device.bus`, e.g. `"pci"`, `"usb"`, `"bluetooth"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bus: Option<String>,
    /// `device.form-factor`, e.g. `"headset"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form_factor: Option<String>,
}

/// One BlueZ audio device's codec choices, joined to `mantle.bluetooth` by MAC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BluetoothCodecs {
    /// PipeWire device id, the first argument of `set_bluetooth_profile`.
    pub device: u32,
    /// MAC address from the `bluez_card.*` name, `_` turned to `:`.
    pub mac: String,
    /// Available profiles that name a codec, ordered by `index`.
    pub codecs: Vec<CodecProfile>,
    /// `index` of the active profile; `nil` before PipeWire reports it or when it is not in `codecs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<i32>,
}

/// One entry of `BluetoothCodecs.codecs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CodecProfile {
    /// Profile index, the second argument of `set_bluetooth_profile`.
    pub index: i32,
    /// Codec from the profile name, else its English description, e.g. `"AAC"`, `"LDAC"`, `"mSBC"`.
    pub codec: String,
    /// PipeWire's description, e.g. `"High Fidelity Playback (A2DP Sink, codec AAC)"`.
    pub description: String,
}

/// One app's playback or recording stream (ADR-0053). Streams without a pid are left out.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppStream {
    /// PipeWire node id, the first argument of `set_app_volume` and `set_app_muted`.
    pub id: u32,
    /// Owning process id, from `application.process.id`.
    pub pid: i32,
    /// `application.name`, if the client set one.
    pub name: Option<String>,
    /// `/proc/<pid>/comm`, or `nil` if it was unreadable when the stream's properties were read.
    pub process_name: Option<String>,
    /// `application.process.binary`, e.g. `"firefox"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// XDG icon name from `application.icon-name`, else `media.icon-name`, e.g. `"firefox"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// A capture stream, such as a call's microphone, rather than playback.
    pub recording: bool,
    /// Stream volume in percent; `nil` until PipeWire reports the stream's `Props`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Stream mute; `false` until `volume` is known.
    pub muted: bool,
}

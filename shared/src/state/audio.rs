//! `mantle.audio` snapshot payload.

use serde::Serialize;

/// `mantle.audio`'s payload (ADR-0053).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioState {
    /// Every output device; the `active` one is the default.
    pub sinks: Vec<AudioDevice>,
    /// Every input device; the `active` one is the default.
    pub sources: Vec<AudioDevice>,
    /// Apps playing or recording audio, excluding pid-less streams, notification sounds, meters and monitor captures.
    pub apps: Vec<AppStream>,
    /// ALSA and BlueZ cards with their profiles, ordered by `device`. A card makes sinks and sources
    /// only for its active profile: analog or HDMI on one Intel HDA card, a codec on a headset.
    pub cards: Vec<AudioCard>,
}

/// One `sinks` or `sources` entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioDevice {
    /// PipeWire node id, the first argument of every device action; not reboot-stable.
    pub id: u32,
    /// `node.name`, e.g. `"alsa_output.pci-0000_00_1f.3.analog-stereo"`: stable across reboots, so
    /// a config keys its own labels on it.
    pub node_name: String,
    /// `node.description`, e.g. `"Built-in Audio Analog Stereo"`, else `node.nick`, else `node.name`.
    pub name: String,
    /// This is the default output or input; with no default known, or one not in this list, the lowest
    /// `id` is.
    pub active: bool,
    /// The `cards[].device` this comes from; `nil` for a virtual device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<u32>,
    /// Volume in percent, the loudest channel; `nil` before its first report. A sink's default is
    /// pulled back to `150` when another client sets it louder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    /// `false` before its first report.
    pub muted: bool,
    /// `-1.0` (left) to `1.0` (right); `nil` for mono, an unknown channel map, or before its first report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f32>,
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
    /// Channel levels in percent, or `nil` before valid channel volumes arrive. Indices name the
    /// current channel order; read a fresh snapshot after a device changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<AudioChannel>>,
}

/// One sink or source channel. `position` is `nil` if PipeWire omits or does not name its channel map.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioChannel {
    /// Zero-based index for `set_channel_volume`.
    pub index: u32,
    /// Speaker position, e.g. `"front_left"` or `"front_right"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    /// Reported level in percent. Sink writes stop at `150`; only the default output is corrected above it.
    pub volume: f64,
}

/// One ALSA or BlueZ card and every profile it offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioCard {
    /// PipeWire device id, the first argument of `set_card_profile`.
    pub device: u32,
    /// `device.description`, e.g. `"Built-in Audio"`, else `device.nick`, else `device.name`.
    pub name: String,
    /// A BlueZ card's MAC address, from its `bluez_card.*` name with `_` turned to `:`; joins
    /// `mantle.bluetooth`. `nil` on an ALSA card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Every profile, unavailable ones included, ordered by `index`.
    pub profiles: Vec<CardProfile>,
    /// `index` of the active profile; `nil` before PipeWire reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<i32>,
    /// Every port, unplugged ones included, ordered by `index`. A device's `port` is its active one.
    pub ports: Vec<CardPort>,
}

/// One entry of `AudioCard.ports`: a place a card routes sound, such as speakers or headphones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CardPort {
    /// Port index, the second argument of `set_port`.
    pub index: i32,
    /// PipeWire's untranslated id, e.g. `"analog-output-headphones"`.
    pub name: String,
    /// PipeWire's description, e.g. `"Headphones"`.
    pub description: String,
    /// `port.type`, e.g. `"speaker"`, `"headphones"`, `"hdmi"`, `"mic"`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub port_type: Option<String>,
    /// `"output"` for a sink's port, `"input"` for a source's.
    pub direction: PortDirection,
    /// `false` when PipeWire reports the port unavailable, such as an unplugged jack.
    pub available: bool,
    /// The card routes sound through it now: an active route of the active profile names it, and
    /// its `type` is that sink's or source's `port`.
    pub active: bool,
    /// `profiles[].index` values the port works under.
    pub profiles: Vec<i32>,
}

/// Whether a port plays or records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PortDirection {
    /// A sink's port, such as speakers.
    Output,
    /// A source's port, such as a microphone.
    Input,
}

/// One entry of `AudioCard.profiles`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CardProfile {
    /// Profile index, the second argument of `set_card_profile`.
    pub index: i32,
    /// PipeWire's untranslated id, e.g. `"output:hdmi-stereo"` or `"off"`.
    pub name: String,
    /// PipeWire's description, e.g. `"Digital Stereo (HDMI) Output"`.
    pub description: String,
    /// The Bluetooth codec the profile names, e.g. `"AAC"`, `"LDAC"`, `"mSBC"`; `nil` when it names
    /// none, as `off` and every ALSA profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// `false` when PipeWire reports the profile unavailable, such as HDMI with no display plugged in.
    pub available: bool,
    /// Sinks the profile makes; `0` for an input-only profile or `off`.
    pub outputs: u32,
    /// Sources the profile makes; `0` for an output-only profile or `off`.
    pub inputs: u32,
}

/// One app's playback or recording stream (ADR-0053). Streams without a pid are left out.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppStream {
    /// PipeWire node id, the first argument of `set_app_volume`, `set_app_muted` and `set_app_target`.
    pub id: u32,
    /// Owning process id, from `application.process.id`.
    pub pid: i32,
    /// `application.name`, if the client set one.
    pub name: Option<String>,
    /// `media.name`, which tells one app's streams apart, e.g. a browser tab's `"YouTube Music"`.
    /// Follows the stream as the client renames it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_name: Option<String>,
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
    pub volume: Option<f64>,
    /// Stream mute; `false` until `volume` is known.
    pub muted: bool,
    /// The `sinks[].id` a playback stream plays into, or the `sources[].id` a recording reads; `nil`
    /// while it is linked to neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<u32>,
    /// The `sinks[].id` or `sources[].id` the stream is pinned to: the device its `target.object`
    /// in the `default` metadata names, by `node.name` or `object.serial`, as `set_app_target`
    /// writes. `nil` when unset, or when it names no current device of the stream's direction,
    /// such as an unplugged headset; WirePlumber then plays it on the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<u32>,
}

//! What the mixer tracks from PipeWire, as plain maps testable without a live proxy; `publish`
//! turns them into payloads.

pub use shared::state::audio::AudioState;

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use pipewire as pw;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use super::devices::{Card, DeviceEntry};
use super::graph::LinkGraph;
use super::streams::{AppStream, CaptureApp};
use crate::capabilities::audio::{master, route};

/// All PipeWire inputs to `mantle.privacy` in one snapshot (ADR-0137). One channel keeps the three
/// lists from arriving out of order when a config draws them together. All three lists change on
/// the same registry events; separate senders would add orderings where one list is a push behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivacySources {
    /// Running `Stream/Input/Video` nodes linked to a camera source (ADR-0034).
    pub cameras: Vec<CaptureApp>,
    /// The PipeWire daemon, which holds the camera for its clients; the kernel scan skips it.
    pub pipewire_pid: Option<u32>,
    pub microphones: Vec<CaptureApp>,
    pub screencasts: Vec<CaptureApp>,
}

/// One audio write crossing from tokio to the PipeWire loop. A channel is required because the
/// proxies are `!Send`; `pipewire::channel` gives the loop an eventfd, so commands apply between
/// PipeWire events. Array ids resolve against live maps; stale ids are logged and dropped because
/// PipeWire recycles them. Volumes are linear fractions. `SetNullSinks` replaces the declared null
/// sinks (ADR-0383); `SetLevelMeters` the metered sinks and sources.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCommand {
    SetVolume { id: u32, volume: f32 },
    SetMuted { id: u32, muted: bool },
    ToggleMute(u32),
    SetBalance { id: u32, balance: f32 },
    SetDefaultSink(u32),
    SetDefaultSource(u32),
    SetChannelVolume { id: u32, index: u32, volume: f32 },
    SetAppVolume { id: u32, volume: f32 },
    SetAppMuted { id: u32, muted: bool },
    SetAppTarget { id: u32, device: Option<u32> },
    SetCardProfile { device: u32, index: i32 },
    SetPort { card: u32, index: i32 },
    SetNullSinks(Vec<shared::action::VirtualDevice>),
    SetLevelMeters(Vec<u32>),
}

/// Listener-owned state and snapshot channels. Held for the thread lifetime. `sink_*`/`metadata*`
/// fields track master volume (ADR-0053 decision 3); sink proxies stay separate because
/// they use `param`, while `nodes` handles stream/video `info`.
pub(super) struct MixerState {
    /// False until PipeWire has answered for everything the listener asked for at startup, which
    /// `registry::run_inner` decides with a `core.sync` barrier re-armed behind each new global. Both publishers build their
    /// payload from the maps and send nothing while it is false, so the first snapshot a config
    /// ever sees is a complete one rather than a default it will watch get corrected.
    ///
    /// The maps keep updating throughout. This gates sending, not tracking.
    pub(super) hydrated: bool,
    pub(super) last_sent: Option<AudioState>,
    /// Maps are keyed by node id; `BTreeMap` publishes them in id order.
    pub(super) apps: BTreeMap<u32, AppStream>,
    pub(super) graph: LinkGraph,
    /// `Stream/Input/Video` nodes, idle ones included; [`LinkGraph`] says which read a camera.
    pub(super) camera_streams: BTreeMap<u32, CaptureApp>,
    /// `Stream/Input/Audio` nodes, idle ones included (ADR-0137).
    pub(super) microphones: BTreeMap<u32, CaptureApp>,
    /// `Stream/Output/Video` nodes, idle ones included (ADR-0137).
    pub(super) screencasts: BTreeMap<u32, CaptureApp>,
    pub(super) nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    pub(super) updates: UnboundedSender<AudioState>,
    pub(super) privacy_updates: watch::Sender<PrivacySources>,
    /// `Audio/Sink` id -> tracked sink data; its `param` listener replaces stream `info`.
    pub(super) sinks: HashMap<u32, DeviceEntry>,
    /// Bound sink proxies/listeners, separate from `sinks` so state stays plain test data.
    pub(super) sink_nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    /// `Audio/Source` id -> the same `Props`-backed entry used for master volume.
    pub(super) sources: HashMap<u32, DeviceEntry>,
    pub(super) source_nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    /// Bound ALSA and BlueZ `Device` proxies/listeners for writes.
    pub(super) devices: HashMap<u32, (Rc<pw::device::Device>, pw::device::DeviceListener)>,
    /// `(device global id, card.profile.device)` -> active ALSA or BlueZ `Route`.
    pub(super) device_routes: HashMap<(u32, i32), route::ActiveRoute>,
    /// `Device` id -> its profiles, for [`AudioState::cards`].
    pub(super) cards: HashMap<u32, Card>,
    /// Audio stream id -> raw `Props`, using the sink's pod shape and parser.
    pub(super) app_props: HashMap<u32, master::RawSinkProps>,
    /// Names selected by `default.audio.sink`/`default.audio.source`, or `None` before arrival.
    pub(super) default_sink_name: Option<String>,
    pub(super) default_source_name: Option<String>,
    /// Bound `default` metadata and its registry id; `Metadata` has no id accessor, so removal
    /// needs this. `None` until `metadata.name == "default"` is found.
    pub(super) metadata: Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    pub(super) metadata_id: Option<u32>,
    /// Stream id -> its `target.object` in the `default` metadata, for `apps[].pinned_to`.
    pub(super) app_pins: HashMap<u32, String>,
    /// `node.name` -> a declared null sink and the `adapter` proxy this connection created for it.
    pub(super) null_sinks: HashMap<String, (shared::action::VirtualDevice, pw::node::Node)>,
    /// Metered sink or source id -> its capture stream.
    pub(super) meters: HashMap<u32, super::meters::Meter>,
}

impl MixerState {
    /// Empty maps, not yet hydrated.
    pub(super) fn new(updates: UnboundedSender<AudioState>, privacy_updates: watch::Sender<PrivacySources>) -> Self {
        Self {
            hydrated: false,
            last_sent: None,
            apps: BTreeMap::new(),
            graph: LinkGraph::default(),
            camera_streams: BTreeMap::new(),
            microphones: BTreeMap::new(),
            screencasts: BTreeMap::new(),
            nodes: HashMap::new(),
            updates,
            privacy_updates,
            sinks: HashMap::new(),
            sink_nodes: HashMap::new(),
            sources: HashMap::new(),
            source_nodes: HashMap::new(),
            devices: HashMap::new(),
            device_routes: HashMap::new(),
            cards: HashMap::new(),
            app_props: HashMap::new(),
            default_sink_name: None,
            default_source_name: None,
            metadata: None,
            metadata_id: None,
            app_pins: HashMap::new(),
            null_sinks: HashMap::new(),
            meters: HashMap::new(),
        }
    }

    /// Drops every bound proxy. Their listeners hold this state, so a lost connection that kept
    /// them would leak itself, sockets included.
    pub(super) fn release_proxies(&mut self) {
        self.nodes.clear();
        self.sink_nodes.clear();
        self.source_nodes.clear();
        self.devices.clear();
        self.metadata = None;
        self.null_sinks.clear();
        self.meters.clear();
    }

    /// Entry map for one direction, shared by binding and writing.
    pub(super) fn device_entries_mut(&mut self, kind: DefaultDevice) -> &mut HashMap<u32, DeviceEntry> {
        match kind {
            DefaultDevice::Sink => &mut self.sinks,
            DefaultDevice::Source => &mut self.sources,
        }
    }

    pub(super) fn device_entries(&self, kind: DefaultDevice) -> &HashMap<u32, DeviceEntry> {
        match kind {
            DefaultDevice::Sink => &self.sinks,
            DefaultDevice::Source => &self.sources,
        }
    }

    /// Whether node `id` is a tracked sink or source.
    pub(super) fn kind_of(&self, id: u32) -> Option<DefaultDevice> {
        [DefaultDevice::Sink, DefaultDevice::Source]
            .into_iter()
            .find(|&kind| self.device_entries(kind).contains_key(&id))
    }

    /// The node `default` metadata routes one direction to, per [`master::resolve_default_device`].
    pub(super) fn default_node(&self, kind: DefaultDevice) -> Option<u32> {
        let name = match kind {
            DefaultDevice::Sink => self.default_sink_name.as_deref(),
            DefaultDevice::Source => self.default_source_name.as_deref(),
        };
        let entries = self.device_entries(kind);
        master::resolve_default_device(name, entries.iter().map(|(&id, entry)| (id, entry.names.node_name.as_str())))
    }

    /// A routed device's card Props are authoritative when the node only mirrors them.
    pub(super) fn device_props(&self, kind: DefaultDevice, id: u32) -> Option<&master::RawSinkProps> {
        let entry = self.device_entries(kind).get(&id)?;
        entry.reported_props(&self.device_routes)
    }

    /// Drops everything tracked under a removed global. Audio publishes only if a map that feeds
    /// it lost an entry or a link went (ports are most removals); privacy only for camera links and sources and its capture streams.
    pub(super) fn remove_global(&mut self, id: u32) {
        self.nodes.remove(&id);
        self.sink_nodes.remove(&id);
        self.source_nodes.remove(&id);
        // Evaluate every removal: `||` would leave a later map's entry behind.
        let mut audio = self.sinks.remove(&id).is_some();
        audio |= self.sources.remove(&id).is_some();
        audio |= self.app_props.remove(&id).is_some();
        audio |= self.cards.remove(&id).is_some();
        audio |= self.apps.remove(&id).is_some();
        self.app_pins.remove(&id);
        self.meters.remove(&id);
        // ALSA and BlueZ devices both record routes; drop this id's so a reused id inherits none.
        let routes = self.device_routes.len();
        self.device_routes.retain(|&(device_id, _), _| device_id != id);
        audio |= self.devices.remove(&id).is_some();
        audio |= self.device_routes.len() != routes;
        if self.metadata_id == Some(id) {
            audio = true;
            self.metadata = None;
            self.metadata_id = None;
            self.default_sink_name = None;
            self.default_source_name = None;
            self.app_pins.clear();
        }
        let graph = self.graph.remove(id);
        // A link that went may have carried an app to its device.
        if audio || graph.links {
            self.publish_audio();
        }
        let mut privacy = graph.camera;
        privacy |= self.camera_streams.remove(&id).is_some();
        privacy |= self.microphones.remove(&id).is_some();
        privacy |= self.screencasts.remove(&id).is_some();
        if privacy {
            self.publish_privacy();
        }
    }
}

/// Which default-device name a metadata `property` event addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DefaultDevice {
    Sink,
    Source,
}

impl DefaultDevice {
    /// The loudest volume fraction a write may set: the sink's cap, 100% for a source.
    pub(super) fn max_volume(self) -> f32 {
        if self == Self::Sink { master::SINK_MAX_VOLUME } else { 1.0 }
    }
}

/// Metadata key naming the master output device, confirmed in `pw-metadata`.
pub(super) const DEFAULT_AUDIO_SINK_KEY: &str = "default.audio.sink";

/// The `default` metadata key WirePlumber reads a stream's chosen device from.
pub(super) const TARGET_OBJECT_KEY: &str = "target.object";

/// Metadata key naming the default input device; same JSON shape as the sink key.
pub(super) const DEFAULT_AUDIO_SOURCE_KEY: &str = "default.audio.source";

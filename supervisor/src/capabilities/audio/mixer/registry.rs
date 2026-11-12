//! PipeWire registry plumbing for stream/video nodes, `Audio/Sink`/`Source`, default metadata,
//! and ALSA and BlueZ devices' profiles and routes. `run` is the thread entry point; callbacks run
//! in its blocking `main_loop.run()`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::capabilities::{RETRY_FIRST, STABLE, next_delay};

use pipewire as pw;
use pw::keys;
use pw::registry::GlobalObject;
use pw::spa::pod::Value;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;
use shared::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::capabilities::audio::{master, profile, route, virtual_devices};
use shared::state::audio_levels::AudioLevelsState;

use super::PropsLookup;
use super::devices::{Card, DeviceEntry, DeviceRoute, device_names};
use super::state::{
    AudioCommand, AudioState, DEFAULT_AUDIO_SINK_KEY, DEFAULT_AUDIO_SOURCE_KEY, DefaultDevice, MixerState,
    PrivacySources, TARGET_OBJECT_KEY,
};
use super::streams::{NodeKind, apply_capture_info_event, apply_info_event, classify, prop_u32};
use super::write::{apply_command, cap_default_sink};
use super::{meters, null_sinks};

/// Runs the listener until process exit. Sends [`AudioState`] on every relevant
/// node-added/-properties-changed/-removed/param-changed event, and [`PrivacySources`] on the
/// equivalent camera/microphone/screencast events (ADR-0034, ADR-0137). One PipeWire connection
/// serves two capabilities, each publishing only on its own changes. The `pipewire-rs` loop and
/// `Rc` state are single-threaded and non-`Send`: call from
/// `std::thread::spawn`, never async.
///
/// `updates` currently reaches only a `main()` log (ADR-0017); `privacy_updates` feeds
/// `privacy::PrivacyController`, describing cameras (ADR-0034), microphones and screencasts
/// (ADR-0137). ponytail: no shutdown path; `main_loop.run()` ends at process exit
/// until Phase 7/8's reload orchestrator needs a `quit()` trigger.
///
/// An unreachable or lost PipeWire is retried, so a restart or a late start at login recovers. A
/// lost connection first publishes empty state, the snapshot of a machine with no audio hardware.
pub fn run(
    updates: UnboundedSender<AudioState>,
    privacy_updates: watch::Sender<PrivacySources>,
    level_updates: UnboundedSender<AudioLevelsState>,
    commands: AudioCommandReceiver,
    virtual_devices: UnboundedSender<virtual_devices::Request>,
) {
    reconnect(None, &updates, &privacy_updates, &level_updates, commands, &virtual_devices);
}

/// The declared null sinks, outliving each connection so the next one creates them again.
type NullSinks = Rc<RefCell<Vec<shared::action::VirtualDevice>>>;

/// [`run`]'s loop; `remote` names a socket other than the default, for tests.
fn reconnect(
    remote: Option<&str>,
    updates: &UnboundedSender<AudioState>,
    privacy_updates: &watch::Sender<PrivacySources>,
    level_updates: &UnboundedSender<AudioLevelsState>,
    commands: AudioCommandReceiver,
    virtual_devices: &UnboundedSender<virtual_devices::Request>,
) {
    let mut commands = Some(commands);
    let null_sinks = NullSinks::default();
    let (mut delay, mut lost) = (RETRY_FIRST, false);
    loop {
        let started = std::time::Instant::now();
        let levels = Rc::new(RefCell::new(meters::Levels::new(level_updates.clone())));
        // The virtual devices' child outlives a lost daemon with none of its modules.
        let restart = lost.then(|| virtual_devices.clone());
        match run_inner(remote, updates, privacy_updates, &mut commands, &null_sinks, &levels, restart) {
            Ok(true) => {
                lost = true;
                warn!("lost the PipeWire connection; reconnecting");
                let _ = updates.send(AudioState::default());
                privacy_updates.send_replace(PrivacySources::default());
                if started.elapsed() >= STABLE {
                    delay = RETRY_FIRST;
                }
            }
            Ok(false) => debug!("PipeWire closed the connection before its first snapshot"),
            Err(err) if delay == RETRY_FIRST => warn!("cannot reach PipeWire ({err}); retrying"),
            Err(err) => debug!("cannot reach PipeWire ({err}); retrying in {delay:?}"),
        }
        std::thread::sleep(delay);
        delay = next_delay(delay);
    }
}

/// Channel types for audio writes; `run` attaches the receiver to the PipeWire loop.
pub type AudioCommandSender = pw::channel::Sender<AudioCommand>;
pub type AudioCommandReceiver = pw::channel::Receiver<AudioCommand>;

/// Built here, not in `main.rs`, so nothing outside this module needs to name `pipewire::channel`.
pub fn command_channel() -> (AudioCommandSender, AudioCommandReceiver) {
    pw::channel::channel()
}

/// One connection until PipeWire drops it; `Ok` says whether it published, `commands` comes back.
fn run_inner(
    remote: Option<&str>,
    updates: &UnboundedSender<AudioState>,
    privacy_updates: &watch::Sender<PrivacySources>,
    commands: &mut Option<AudioCommandReceiver>,
    declared: &NullSinks,
    levels: &Rc<RefCell<meters::Levels>>,
    restart: Option<UnboundedSender<virtual_devices::Request>>,
) -> Result<bool, pw::Error> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(remote.map(|remote| pw::properties::properties! { *keys::REMOTE_NAME => remote }))?;
    let registry = core.get_registry_rc()?;

    let state = Rc::new(RefCell::new(MixerState::new(updates.clone(), privacy_updates.clone())));
    // `done` answers once per barrier, and only the last opens the gate; take, so one restart.
    let restart = Cell::new(restart);

    // Weak: the listener is stored on registry's C object, so a captured strong RegistryRc would
    // keep itself alive forever.
    let registry_weak = registry.downgrade();
    let state_for_global = Rc::clone(&state);
    let state_for_remove = Rc::clone(&state);

    // Publication is held until PipeWire has answered for everything this listener asked for.
    // `core.sync`'s `done` is a barrier: it means every method issued before it, and every event
    // those produced, has been handled.
    //
    // One sync cannot be enough, because the requests that matter are made *by* the events it is
    // waiting on: `on_global` binds a node and subscribes to its params from inside the `global`
    // callback, and a sink's volume arrives on the `param` event that follows. So every global
    // handled while the gate is shut pushes the barrier back behind whatever it just asked for,
    // and only the newest barrier opens the gate. A fixed pair of syncs would leave a sink
    // discovered between the two publishing at volume zero, which is the bug this exists to close.
    //
    // Matched on the returned sequence, never on any `done`: `apply_command`'s writes are methods
    // too, and their completion must not be read as hydration.
    let barrier: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    let core_weak = core.downgrade();

    /// Re-arms the barrier behind the requests just issued. Opening early is the whole failure
    /// mode, so a `sync` that cannot be issued opens the gate rather than shutting it forever.
    fn rearm(core: &pw::core::CoreWeak, barrier: &Cell<Option<i32>>, state: &RefCell<MixerState>) {
        match core.upgrade().map(|core| core.sync(0)) {
            Some(Ok(seq)) => barrier.set(Some(seq.seq())),
            other => {
                if let Some(Err(err)) = other {
                    warn!("core.sync failed ({err}); publishing without waiting for PipeWire to settle");
                }
                let mut state = state.borrow_mut();
                state.hydrated = true;
                state.publish_audio();
                state.publish_privacy();
            }
        }
    }

    let barrier_for_global = Rc::clone(&barrier);
    let core_for_global = core.downgrade();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if let Some(registry) = registry_weak.upgrade() {
                on_global(&state_for_global, &registry, obj);
            }
            // Bind the read before the call: `rearm` takes the state mutably on its failure path,
            // and a borrow held across it would panic inside a PipeWire callback.
            let gated = !state_for_global.borrow().hydrated;
            if gated {
                rearm(&core_for_global, &barrier_for_global, &state_for_global);
            }
        })
        .global_remove(move |id| state_for_remove.borrow_mut().remove_global(id))
        .register();

    // The gate itself. `error` opens it too: a core error means no further `done` is coming, and a
    // shell whose audio never publishes at all is worse than one that publishes early. A lost
    // connection (`EPIPE`, as `pw-cat` reads it) ends the loop for `reconnect` instead.
    let main_loop_for_error = main_loop.downgrade();
    let state_for_done = Rc::clone(&state);
    let barrier_for_done = Rc::clone(&barrier);
    let state_for_error = Rc::clone(&state);
    let state_for_info = Rc::clone(&state);
    let _core_listener = core
        .add_listener_local()
        // The daemon's pid is in its core props only, not its registry global.
        // ponytail: only the daemon is excluded, not the client that registered a camera source;
        // a setup hosting cameras in wireplumber shows "wireplumber", visible rather than hidden.
        .info(move |info| {
            let pid = info.props().and_then(|props| prop_u32(props, *keys::APP_PROCESS_ID));
            let mut state = state_for_info.borrow_mut();
            if pid.is_some() && state.graph.core_pid != pid {
                state.graph.core_pid = pid;
                state.publish_privacy();
            }
        })
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE || barrier_for_done.get() != Some(seq.seq()) {
                return;
            }
            if let Some(restart) = restart.take() {
                let _ = restart.send(virtual_devices::Request::Restart);
            }
            // Publish once here even with no sink at all: a machine with no audio hardware still
            // owes the config an answer, and waiting for a nonempty map would never end.
            let mut state = state_for_done.borrow_mut();
            state.hydrated = true;
            state.publish_audio();
            state.publish_privacy();
        })
        .error(move |id, _seq, res, message| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            if res == -libc::EPIPE {
                if let Some(main_loop) = main_loop_for_error.upgrade() {
                    main_loop.quit();
                }
                return;
            }
            let mut state = state_for_error.borrow_mut();
            if !state.hydrated {
                warn!("PipeWire core error before the first snapshot ({message}); publishing what arrived");
                state.hydrated = true;
                state.publish_audio();
                state.publish_privacy();
            }
        })
        .register();

    // Arm the first barrier. Without a single global this is the one that opens the gate, which is
    // what gives a machine with no audio hardware its empty first snapshot.
    rearm(&core_weak, &barrier, &state);

    // Hold it for the loop lifetime; dropping AttachedReceiver detaches the eventfd source and
    // every later command is silently discarded.
    null_sinks::sync(&state, &core, &declared.borrow());
    let (state_for_command, core_for_command, declared) = (Rc::clone(&state), core.clone(), Rc::clone(declared));
    let levels = Rc::clone(levels);
    let Some(receiver) = commands.take() else { return Ok(false) };
    let attached = receiver.attach(main_loop.loop_(), move |command| {
        // Names, not ids: applied before hydration too.
        if let AudioCommand::SetNullSinks(devices) = command {
            *declared.borrow_mut() = devices;
            return null_sinks::sync(&state_for_command, &core_for_command, &declared.borrow());
        }
        // Sent before this connection's first snapshot, as during an outage: its ids name nothing live.
        if !state_for_command.borrow().hydrated {
            return debug!("dropped {command:?}: PipeWire has not answered yet");
        }
        if let AudioCommand::SetLevelMeters(ids) = command {
            return meters::sync(&state_for_command, &core_for_command, &levels, &ids);
        }
        apply_command(&state_for_command, command)
    });

    main_loop.run();
    *commands = Some(attached.deattach());
    let hydrated = state.borrow().hydrated;
    state.borrow_mut().release_proxies();
    drop((_registry_listener, _core_listener));
    debug_assert_eq!(Rc::strong_count(&state), 1, "a listener still holds the mixer state");
    Ok(hydrated)
}

/// A `param` event's pod as a value tree; `None` for an absent or undecodable one.
fn decode(param: Option<&pw::spa::pod::Pod>) -> Option<Value> {
    PodDeserializer::deserialize_from::<Value>(param?.as_bytes()).ok().map(|(_, value)| value)
}

fn record_route(state: &Rc<RefCell<MixerState>>, device_id: u32, value: &Value) {
    let mut current = state.borrow_mut();
    let Some((profile_device, route)) = route::extract_route_target(value, &current.device_routes, device_id) else {
        return;
    };
    let previous = current.device_routes.insert((device_id, profile_device), route.clone());
    if previous.as_ref() == Some(&route) {
        return;
    }
    // ponytail: the first push can lack `port` until Route answers; upgrade: fold Route into hydration.
    current.publish_audio();
    drop(current);
    // Node Props can arrive before their Route, when the cap has no route to write through.
    cap_default_sink(state);
}

pub(super) fn record_node_props(
    state: &Rc<RefCell<MixerState>>,
    kind: DefaultDevice,
    node_id: u32,
    raw: master::RawSinkProps,
) -> bool {
    let mut current = state.borrow_mut();
    let Some(entry) = current.device_entries_mut(kind).get_mut(&node_id) else { return false };
    let changed = entry.props.as_ref() != Some(&raw);
    entry.props = Some(raw);
    if changed {
        current.publish_audio();
    }
    changed
}

/// Routes `Node`, `Device`, and `default` `Metadata` globals to their binders and records the
/// `Link` globals in the link graph; ignores the rest.
fn on_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.type_ {
        ObjectType::Node => on_node_global(state, registry, obj),
        ObjectType::Device => bind_device(state, registry, obj),
        ObjectType::Metadata => bind_default_metadata(state, registry, obj),
        ObjectType::Link => {
            let mut state = state.borrow_mut();
            let Some(props) = obj.props else { return };
            let changed = state.graph.add_link(obj.id, props);
            if changed.camera {
                state.publish_privacy();
            }
            // Deduplicated: a link between two nodes no app owns publishes nothing new.
            if changed.links {
                state.publish_audio();
            }
        }
        _ => {}
    }
}

/// `media.class` for output devices. Master volume lives here, not on streams.
const AUDIO_SINK: &str = "Audio/Sink";

/// `media.class` for input devices, tracked like sinks for source volume/mute. No monitor
/// filter: PulseAudio synthesizes `.monitor` sources, native PipeWire does not, and this `pw-dump`
/// lists one `Audio/Source` beside one `Audio/Sink`.
const AUDIO_SOURCE: &str = "Audio/Source";

/// Metadata name key. A machine advertises several names (`"settings"`, `"default"`,
/// `"route-settings"`); it is absent from `pipewire-rs::keys`. Live `pw-mon` showed the needed
/// global as `metadata.name = "default"`.
const METADATA_NAME: &str = "metadata.name";

/// Classifies `Node` globals. `Audio/Sink`/`Source` use [`bind_device_node`] for `param` volume
/// events; streams/video use `info`, gated to props changes. Stream pids may arrive after `global`.
fn on_node_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.props.and_then(|props| props.get_prop(*keys::MEDIA_CLASS)) {
        Some(AUDIO_SINK) => return bind_device_node(state, registry, obj, DefaultDevice::Sink),
        Some(AUDIO_SOURCE) => return bind_device_node(state, registry, obj, DefaultDevice::Source),
        _ => {}
    }
    let Some(kind) = obj.props.and_then(classify) else {
        return;
    };

    // A camera source is read from its globals; its consumers are the nodes that get bound.
    if kind == NodeKind::Video {
        let mut state = state.borrow_mut();
        if obj.props.is_some_and(|props| state.graph.add_camera(obj.id, props)) {
            state.publish_privacy();
        }
        return;
    }
    let Ok(node): Result<pw::node::Node, _> = registry.bind(obj) else { return };
    let node_id = obj.id;
    let state_for_info = Rc::clone(state);
    let state_for_param = Rc::clone(state);
    let listener = node
        .add_listener_local()
        .info(move |info| {
            let mut state_mut = state_for_info.borrow_mut();
            let has_props_change = info.change_mask().contains(pw::node::NodeChangeMask::PROPS);
            if matches!(kind, NodeKind::Audio | NodeKind::Microphone)
                && apply_info_event(
                    std::path::Path::new("/proc"),
                    &mut state_mut.apps,
                    node_id,
                    has_props_change,
                    info.props(),
                )
            {
                state_mut.publish_audio();
            }
            let apps = match kind {
                NodeKind::Audio | NodeKind::Video => return,
                NodeKind::CameraStream => &mut state_mut.camera_streams,
                NodeKind::Microphone => &mut state_mut.microphones,
                NodeKind::Screencast => &mut state_mut.screencasts,
            };
            let running = matches!(info.state(), pw::node::NodeState::Running);
            if apply_capture_info_event(apps, node_id, kind, has_props_change, info.props(), running) {
                state_mut.publish_privacy();
            }
        })
        // Per-app streams carry the same channelVolumes/mute pod, parser, and cube-root conversion
        // as master sinks (verified live).
        // Only audio streams carry a Props pod.
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(value) = decode(param) else { return };
            // As for master sinks, a node can advertise multiple Props objects; missing
            // channelVolumes means this is not a mixer update, not zero volume.
            let mut state_mut = state_for_param.borrow_mut();
            let muted = state_mut.app_props.get(&node_id).is_some_and(|props| props.mute);
            let Some(raw) = master::extract_sink_props(&value, muted) else {
                return;
            };
            if state_mut.app_props.insert(node_id, raw.clone()) != Some(raw) {
                state_mut.publish_audio();
            }
        })
        .register();

    if matches!(kind, NodeKind::Audio | NodeKind::Microphone) {
        node.subscribe_params(&[pw::spa::param::ParamType::Props]);
    }

    state.borrow_mut().nodes.insert(node_id, (node, listener));
}

/// Binds an `Audio/Sink` or `Audio/Source` and subscribes to its `Props` param, where PipeWire
/// keeps volume/mute. `node.name` is present in the first `global` props, unlike a stream pid.
///
/// `extract_sink_props` rejects the second of a sink's two `Props` objects; treating its missing
/// mixer fields as zero was a shipped bug.
fn bind_device_node(
    state: &Rc<RefCell<MixerState>>,
    registry: &pw::registry::RegistryRc,
    obj: &GlobalObject<&DictRef>,
    kind: DefaultDevice,
) {
    let node_id = obj.id;
    let Some(names) = obj.props.and_then(device_names) else { return };

    // Bind before recording: a failed bind never gets Props, so its volume would stay `None`.
    let node: pw::node::Node = match registry.bind(obj) {
        Ok(node) => node,
        Err(err) => {
            debug!("failed to bind {kind:?} node {node_id}: {err}");
            return;
        }
    };
    debug!("bound {kind:?} device node {node_id}");

    let state_for_param = Rc::clone(state);
    let state_for_info = Rc::clone(state);
    let listener = node
        .add_listener_local()
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(value) = decode(param) else { return };
            let muted = state_for_param.borrow().device_props(kind, node_id).is_some_and(|props| props.mute);
            let Some(raw) = master::extract_sink_props(&value, muted) else {
                return;
            };
            if !record_node_props(&state_for_param, kind, node_id, raw) {
                return;
            }
            if kind == DefaultDevice::Sink {
                cap_default_sink(&state_for_param);
            }
        })
        // Only info props carry device.id, card.profile.device and the device hints; global ones never do.
        .info(move |info| {
            if !info.change_mask().contains(pw::node::NodeChangeMask::PROPS) {
                return;
            }
            let Some(props) = info.props() else { return };
            let (route, Some(names)) = (parse_device_route(props), device_names(props)) else { return };
            let mut state_mut = state_for_info.borrow_mut();
            let Some(entry) = state_mut.device_entries_mut(kind).get_mut(&node_id) else { return };
            if entry.names != names || entry.route != route {
                entry.names = names;
                entry.route = route;
                state_mut.publish_audio();
            }
        })
        .register();

    // Subscribe after listener registration; neither call dispatches until main_loop.run(), so
    // order doesn't matter.
    node.subscribe_params(&[pw::spa::param::ParamType::Props]);

    let mut state_mut = state.borrow_mut();
    state_mut.device_entries_mut(kind).insert(node_id, DeviceEntry { names, ..DeviceEntry::default() });
    match kind {
        DefaultDevice::Sink => state_mut.sink_nodes.insert(node_id, (node, listener)),
        DefaultDevice::Source => state_mut.source_nodes.insert(node_id, (node, listener)),
    };
    state_mut.publish_audio();
}

/// Hardware route from node props. `device.id` and `card.profile.device` appear together or not
/// at all: this machine's analog output has `51`/`7`, its mic `51`/`0`. `None` means node-owned.
fn parse_device_route(props: &impl PropsLookup) -> Option<DeviceRoute> {
    let device_id = props.get_prop(*keys::DEVICE_ID)?.parse().ok()?;
    let profile_device = props.get_prop(CARD_PROFILE_DEVICE)?.parse().ok()?;
    Some(DeviceRoute { device_id, profile_device })
}

/// Sink property naming its device route. Absent from `pipewire-rs::keys`; live `pw-dump` shows
/// this output's `card.profile.device = 7`, matching `device: 7` on device 49's Route.
const CARD_PROFILE_DEVICE: &str = "card.profile.device";

/// Binds the `Metadata` global named `"default"`, which publishes `default.audio.sink` and
/// `default.audio.source`; other metadata globals stay unbound.
fn bind_default_metadata(
    state: &Rc<RefCell<MixerState>>,
    registry: &pw::registry::RegistryRc,
    obj: &GlobalObject<&DictRef>,
) {
    if obj.props.and_then(|props| props.get_prop(METADATA_NAME)) != Some("default") {
        return;
    }

    let Ok(metadata): Result<pw::metadata::Metadata, _> = registry.bind(obj) else { return };

    let state_for_property = Rc::clone(state);
    let listener = metadata
        .add_listener_local()
        .property(move |subject, key, _type_, value| {
            // Ignore other keys, including default.configured.* (which can name a disconnected
            // Bluetooth device); default.audio.* names the real devices.
            let field = match key {
                Some(DEFAULT_AUDIO_SINK_KEY) => DefaultDevice::Sink,
                Some(DEFAULT_AUDIO_SOURCE_KEY) => DefaultDevice::Source,
                // Keyed by the stream's id; publish_audio drops a pin that changed nothing.
                Some(TARGET_OBJECT_KEY) => {
                    let mut state_mut = state_for_property.borrow_mut();
                    match value {
                        Some(pin) => state_mut.app_pins.insert(subject, pin.to_string()),
                        None => state_mut.app_pins.remove(&subject),
                    };
                    state_mut.publish_audio();
                    return 0;
                }
                // `pw-metadata -d`: no key clears everything the subject set.
                None => {
                    let mut state_mut = state_for_property.borrow_mut();
                    if subject == 0 {
                        (state_mut.default_sink_name, state_mut.default_source_name) = (None, None);
                    } else {
                        state_mut.app_pins.remove(&subject);
                    }
                    state_mut.publish_audio();
                    drop(state_mut);
                    cap_default_sink(&state_for_property);
                    return 0;
                }
                _ => return 0,
            };
            let mut state_mut = state_for_property.borrow_mut();
            // A cleared value means no default; resolve_default_device already falls back.
            let name = value.and_then(master::parse_default_device_name);
            match field {
                DefaultDevice::Sink => state_mut.default_sink_name = name,
                DefaultDevice::Source => state_mut.default_source_name = name,
            }
            state_mut.publish_audio();
            drop(state_mut);
            if field == DefaultDevice::Sink {
                cap_default_sink(&state_for_property);
            }
            0
        })
        .register();

    let mut state_mut = state.borrow_mut();
    state_mut.metadata_id = Some(obj.id);
    state_mut.metadata = Some((metadata, listener));
}

/// The params a device `info` asks for again, from each `(id, flags)` it lists: PipeWire flips a
/// param's `SPA_PARAM_INFO_SERIAL` flag bit when its value changes, so a flag set unlike `seen`'s
/// marks a change. `Profile`'s answer ends the `EnumProfile` and `EnumRoute` enumerations
/// ([`Card::finish_enumeration`]), so a change to any of the three asks all three, in that order.
fn params_to_ask(
    seen: &mut HashMap<u32, u32>,
    params: impl Iterator<Item = (u32, u32)>,
) -> Vec<pw::spa::param::ParamType> {
    use pw::spa::param::ParamType;
    let changed: Vec<u32> =
        params.filter(|&(id, flags)| seen.insert(id, flags) != Some(flags)).map(|(id, _)| id).collect();
    let card = [ParamType::EnumProfile, ParamType::EnumRoute, ParamType::Profile];
    let mut ask = Vec::new();
    if card.iter().any(|param| changed.contains(&param.as_raw())) {
        ask.extend(card);
    }
    if changed.contains(&ParamType::Route.as_raw()) {
        ask.push(ParamType::Route);
    }
    ask
}

/// Binds ALSA and BlueZ `Device` globals for their profiles and active `Route`: its index for
/// writes, its port for device glyphs. Without the `Route`, hardware-sink `set_volume` is silently
/// dropped. A BlueZ card is keyed by MAC for a Bluetooth join. `v4l2`/`libcamera` devices have no
/// audio routes.
///
/// Enumerated from `info`, not subscribed to: `subscribe_params` delivers only what exists when it
/// is called, and a `Route` appearing later is pushed to nobody. A shell started before its card
/// settles, which is every login, then dropped every write for the session (ADR-0200).
///
/// `Profile` is asked after `EnumProfile` and `EnumRoute`, so its answer ends both enumerations and
/// publishes only on a change. Answers are matched by that order, not by seq: protocol-native's
/// `device_marshal_enum_params` sends `SPA_RESULT_RETURN_ASYNC(msg->seq)` and ignores the caller's
/// (a live `enum_params(7, ..)` came back as 1073741828).
fn bind_device(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    let Some(props) = obj.props else { return };
    let device_name = props.get_prop(*keys::DEVICE_NAME);
    let mac = match props.get_prop(*keys::DEVICE_API) {
        Some("alsa") => None,
        Some("bluez5") => {
            let Some(mac) = device_name.and_then(profile::mac_from_card_name) else {
                debug!("Bluetooth device {} names no address; its codecs are not tracked", obj.id);
                return;
            };
            Some(mac)
        }
        _ => return,
    };
    let name = props
        .get_prop(*keys::DEVICE_DESCRIPTION)
        .or_else(|| props.get_prop(*keys::DEVICE_NICK))
        .or(device_name)
        .unwrap_or_default()
        .to_string();
    let device_id = obj.id;
    let device: pw::device::Device = match registry.bind(obj) {
        Ok(device) => device,
        Err(err) => {
            debug!("failed to bind audio device {device_id} ({name}): {err}");
            return;
        }
    };

    // `Rc` so the `info` handler below can reach the proxy it is registered on; `Weak` there,
    // because a strong one is a cycle through the listener the device owns.
    let device = Rc::new(device);
    let device_for_info = Rc::downgrade(&device);
    let param_flags = RefCell::new(HashMap::new());
    let state_for_param = Rc::clone(state);
    let listener = device
        .add_listener_local()
        .param(move |_seq, param_type, _index, _next, param| {
            let Some(value) = decode(param) else { return };
            match param_type {
                pw::spa::param::ParamType::Route => return record_route(&state_for_param, device_id, &value),
                pw::spa::param::ParamType::EnumRoute => {
                    let Some(port) = route::extract_port(&value) else { return };
                    if let Some(card) = state_for_param.borrow_mut().cards.get_mut(&device_id) {
                        card.enumerated_port(port);
                    }
                    return;
                }
                _ => {}
            }
            let Some(profile) = profile::extract_profile(&value) else { return };
            let mut state = state_for_param.borrow_mut();
            let Some(card) = state.cards.get_mut(&device_id) else { return };
            match param_type {
                pw::spa::param::ParamType::EnumProfile => card.enumerated(profile),
                pw::spa::param::ParamType::Profile if card.finish_enumeration(profile.index) => {
                    state.publish_audio();
                }
                _ => {}
            }
        })
        // Re-asks the params that changed, every one on the first `info` a bind always answers
        // with. A profile switch or a plugged headset moves the active route; a volume step
        // changes only the Route.
        .info(move |info| {
            if !info.change_mask().contains(pw::device::DeviceChangeMask::PARAMS) {
                return;
            }
            let params = info.params().iter().map(|param| (param.id().as_raw(), param.flags().bits()));
            let ask = params_to_ask(&mut param_flags.borrow_mut(), params);
            if let Some(device) = device_for_info.upgrade() {
                for param in ask {
                    device.enum_params(0, Some(param), 0, u32::MAX);
                }
            }
        })
        .register();

    let mut state = state.borrow_mut();
    state.cards.insert(device_id, Card { mac, name, ..Card::default() });
    state.devices.insert(device_id, (device, listener));
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    /// Killed on drop, so a failing test leaves no daemon behind.
    struct Daemon(Child);

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A private PipeWire with one null sink and no session manager, listening at `dir/pipewire-0`.
    fn daemon(dir: &std::path::Path) -> Daemon {
        daemon_with(dir, "")
    }

    /// [`daemon`] with `extra` `context.objects` entries beside the sink.
    fn daemon_with(dir: &std::path::Path, extra: &str) -> Daemon {
        let config = dir.join("pipewire.conf");
        std::fs::write(
            &config,
            "context.properties = { core.daemon = true core.name = pipewire-0 support.dbus = false }
            context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }
            context.modules = [
                { name = libpipewire-module-protocol-native } { name = libpipewire-module-access }
                { name = libpipewire-module-adapter } { name = libpipewire-module-client-node }
                { name = libpipewire-module-metadata }
            ]
            context.objects = [ { factory = adapter args = {
                factory.name = support.null-audio-sink node.name = test-sink media.class = Audio/Sink
                audio.position = [ FL FR ]
            } } EXTRA ]"
                .replace("EXTRA", extra),
        )
        .unwrap();
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&config)
            .env("PIPEWIRE_RUNTIME_DIR", dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pipewire must be installed: the reconnect test runs a private daemon");
        Daemon(child)
    }

    fn until(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<AudioState>,
        done: impl Fn(&AudioState) -> bool,
    ) -> AudioState {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match updates.try_recv() {
                Ok(state) if done(&state) => return state,
                Ok(_) => {}
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(err) => panic!("no matching audio state: {err}"),
            }
        }
    }

    /// The ids and `node.name`s of the `class` nodes `pw-dump` lists on the private daemon at `dir`.
    fn nodes(dir: &std::path::Path, class: &str) -> Vec<(u64, String)> {
        let output = Command::new("pw-dump").env("PIPEWIRE_RUNTIME_DIR", dir).output().expect("pw-dump runs");
        let dump: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap_or_default();
        dump.iter()
            .filter(|object| object["info"]["props"]["media.class"] == class)
            .map(|object| {
                let name = object["info"]["props"]["node.name"].as_str().unwrap_or_default().to_owned();
                (object["id"].as_u64().unwrap_or_default(), name)
            })
            .collect()
    }

    /// Polls [`nodes`] until `done` accepts them, within 10 s.
    fn until_nodes(dir: &std::path::Path, class: &str, done: impl Fn(&[(u64, String)]) -> bool) -> Vec<(u64, String)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = nodes(dir, class);
            if done(&found) {
                return found;
            }
            assert!(Instant::now() < deadline, "the daemon never held the {class} nodes wanted: {found:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A mixer thread on the private daemon at `dir`, and the far ends of its channels.
    struct Mixer {
        published: tokio::sync::mpsc::UnboundedReceiver<AudioState>,
        commands: AudioCommandSender,
        levels: tokio::sync::mpsc::UnboundedReceiver<AudioLevelsState>,
        restarts: tokio::sync::mpsc::UnboundedReceiver<virtual_devices::Request>,
    }

    fn mixer(dir: &std::path::Path) -> Mixer {
        let remote = dir.join("pipewire-0").to_str().unwrap().to_owned();
        let (updates, published) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy) = watch::channel(PrivacySources::default());
        let (commands, receiver) = command_channel();
        let (level_updates, levels) = tokio::sync::mpsc::unbounded_channel();
        let (restart, restarts) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            reconnect(Some(&remote), &updates, &privacy_updates, &level_updates, receiver, &restart);
        });
        Mixer { published, commands, levels, restarts }
    }

    /// No id, no stream: metering the sink opens one capture stream, a repeat call keeps that
    /// stream, an unknown id opens none, and the empty list closes it and pushes an empty list.
    #[test]
    fn a_metered_sink_has_one_capture_stream_until_the_list_empties() {
        let dir = tempfile::tempdir().unwrap();
        let _pipewire = daemon(dir.path());
        let mut mixer = mixer(dir.path());
        let sink = until(&mut mixer.published, |audio| audio.sinks.len() == 1).sinks[0].id;
        assert!(nodes(dir.path(), "Stream/Input/Audio").is_empty());

        mixer.commands.send(AudioCommand::SetLevelMeters(vec![sink, sink + 1000])).unwrap();
        let stream = until_nodes(dir.path(), "Stream/Input/Audio", |found| found.len() == 1);
        while mixer.levels.try_recv().is_ok() {}
        mixer.commands.send(AudioCommand::SetLevelMeters(vec![sink])).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while mixer.levels.try_recv().is_err() {
            assert!(Instant::now() < deadline, "the repeat call pushed nothing");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(nodes(dir.path(), "Stream/Input/Audio"), stream, "the repeat call kept the same stream");

        mixer.commands.send(AudioCommand::SetLevelMeters(Vec::new())).unwrap();
        until_nodes(dir.path(), "Stream/Input/Audio", <[_]>::is_empty);
        let mut last = None;
        while let Ok(state) = mixer.levels.try_recv() {
            last = Some(state);
        }
        assert_eq!(last, Some(AudioLevelsState::default()), "the emptied list is pushed");
        let mut audio = None;
        while let Ok(state) = mixer.published.try_recv() {
            audio = Some(state);
        }
        assert!(audio.is_none_or(|audio| audio.apps.is_empty()), "the meter is not an app");
    }

    /// A declared null sink appears, comes back after a daemon restart, which also asks the
    /// virtual devices' child to restart, and goes with an empty list.
    #[test]
    fn a_null_sink_is_made_again_after_a_restart_and_goes_with_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let pipewire = daemon(dir.path());
        let mut mixer = mixer(dir.path());
        let device = shared::action::VirtualDevice {
            kind: shared::action::VirtualKind::NullSink,
            name: "mantle-test-null".to_string(),
            description: None,
            args: serde_json::Map::new(),
        };
        let declared = |audio: &AudioState| audio.sinks.iter().any(|sink| sink.node_name == "mantle-test-null");
        until(&mut mixer.published, |audio| audio.sinks.len() == 1);
        mixer.commands.send(AudioCommand::SetNullSinks(vec![device])).unwrap();
        until(&mut mixer.published, declared);
        assert!(mixer.restarts.try_recv().is_err(), "the first connection restarts nothing");

        drop(pipewire);
        until(&mut mixer.published, |audio| *audio == AudioState::default());
        let _pipewire = daemon(dir.path());
        until(&mut mixer.published, declared);
        assert!(matches!(mixer.restarts.try_recv(), Ok(virtual_devices::Request::Restart)));

        mixer.commands.send(AudioCommand::SetNullSinks(Vec::new())).unwrap();
        until(&mut mixer.published, |audio| audio.sinks.len() == 1 && !declared(audio));
    }

    /// `pw-metadata -d 0` clears every key the subject set, with no key in the event, so the
    /// default sink falls back to the lowest id instead of staying on the name cleared.
    #[test]
    fn clearing_the_default_metadata_falls_back_to_the_lowest_sink() {
        let dir = tempfile::tempdir().unwrap();
        let _pipewire = daemon_with(
            dir.path(),
            "{ factory = adapter args = { factory.name = support.null-audio-sink node.name = test-sink-2
                media.class = Audio/Sink audio.position = [ FL FR ] } }
            { factory = metadata args = { metadata.name = default } }",
        );
        let mut mixer = mixer(dir.path());
        let pw_metadata = |args: &[&str]| {
            let status = Command::new("pw-metadata")
                .args(["-n", "default"])
                .args(args)
                .env("PIPEWIRE_RUNTIME_DIR", dir.path())
                .status()
                .expect("pw-metadata runs");
            assert!(status.success(), "pw-metadata {args:?}");
        };
        let active = |audio: &AudioState| {
            audio.sinks.iter().find(|sink| sink.active).map(|sink| sink.node_name.clone()).unwrap_or_default()
        };
        until(&mut mixer.published, |audio| audio.sinks.len() == 2 && active(audio) == "test-sink");

        pw_metadata(&["0", "default.audio.sink", r#"{"name":"test-sink-2"}"#, "Spa:String:JSON"]);
        until(&mut mixer.published, |audio| active(audio) == "test-sink-2");
        pw_metadata(&["-d", "0"]);
        until(&mut mixer.published, |audio| active(audio) == "test-sink");
    }

    /// The conf `set_virtual_devices` writes loads in `pipewire -c`: a loopback is a sink by its name.
    #[test]
    fn a_loopback_in_the_generated_conf_loads_as_a_sink() {
        let dir = tempfile::tempdir().unwrap();
        let _pipewire = daemon(dir.path());
        let device = shared::action::VirtualDevice {
            kind: shared::action::VirtualKind::Loopback,
            name: "mantle-test-loop".to_string(),
            description: Some("Test loop".to_string()),
            args: serde_json::Map::new(),
        };
        let conf = dir.path().join("devices.conf");
        std::fs::write(&conf, virtual_devices::conf_text(&[device])).unwrap();
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&conf)
            .env("PIPEWIRE_RUNTIME_DIR", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _child = Daemon(child);
        until_nodes(dir.path(), "Audio/Sink", |found| found.iter().any(|(_, name)| name == "mantle-test-loop"));
    }

    #[test]
    fn a_route_change_asks_only_the_route_and_a_profile_change_the_card_params() {
        use pw::spa::param::ParamType;
        let raw = |param: ParamType| param.as_raw();
        let mut seen = HashMap::new();
        let all =
            [raw(ParamType::EnumProfile), raw(ParamType::EnumRoute), raw(ParamType::Profile), raw(ParamType::Route)];
        let info = |flipped: &[u32]| all.map(|id| (id, if flipped.contains(&id) { 0b11 } else { 0b10 }));
        let card = vec![ParamType::EnumProfile, ParamType::EnumRoute, ParamType::Profile];

        assert_eq!(params_to_ask(&mut seen, info(&[]).into_iter()), [card.clone(), vec![ParamType::Route]].concat());
        assert_eq!(params_to_ask(&mut seen, info(&[]).into_iter()), [], "nothing changed");
        assert_eq!(params_to_ask(&mut seen, info(&[raw(ParamType::Route)]).into_iter()), [ParamType::Route]);
        let profile = [raw(ParamType::Route), raw(ParamType::Profile)];
        assert_eq!(params_to_ask(&mut seen, info(&profile).into_iter()), card, "the route's flag is unchanged");
    }

    /// Absent at start, then up, lost, and back, as a PipeWire started late at login and then
    /// restarted. A write queued during the outage is dropped; one sent after it lands, and a mute
    /// toggle flips the sink.
    #[test]
    fn a_missing_or_lost_pipewire_is_reconnected() {
        let dir = tempfile::tempdir().unwrap();
        let Mixer { mut published, commands, .. } = mixer(dir.path());

        std::thread::sleep(Duration::from_millis(200));
        let pipewire = daemon(dir.path());
        let first = until(&mut published, |audio| audio.sinks.len() == 1).sinks[0].id;
        drop(pipewire);
        until(&mut published, |audio| *audio == AudioState::default());

        commands.send(AudioCommand::SetVolume { id: first, volume: 0.9 }).unwrap();
        let _pipewire = daemon(dir.path());
        let sink = until(&mut published, |audio| audio.sinks.len() == 1).sinks[0].id;
        commands.send(AudioCommand::SetVolume { id: sink, volume: 0.5 }).unwrap();
        until(&mut published, |audio| {
            let volume = audio.sinks.first().and_then(|sink| sink.volume);
            let near = |percent: f64| volume.is_some_and(|volume| (volume - percent).abs() < 1.0);
            assert!(!near(90.0), "the write queued during the outage was replayed");
            near(50.0)
        });
        commands.send(AudioCommand::ToggleMute(sink)).unwrap();
        until(&mut published, |audio| audio.sinks.first().is_some_and(|sink| sink.muted));
    }
}

//! Applies `AudioCommand`s against live `MixerState` maps. Writes are not optimistic:
//! success returns through `registry`'s `param`/`property` listener, like `wpctl` or a headset
//! button change.

use std::cell::RefCell;
use std::rc::Rc;

use pipewire as pw;
use shared::debug;

use crate::capabilities::audio::{master, profile, route};

use super::devices::DeviceRoute;
use super::state::{AudioCommand, DEFAULT_AUDIO_SINK_KEY, DEFAULT_AUDIO_SOURCE_KEY, DefaultDevice, MixerState};

/// The `type_` a `default.audio.*` metadata value carries. Read straight off `pw-metadata`'s own
/// dump (`type:'Spa:String:JSON'`) rather than inferred from the value looking like JSON.
const METADATA_JSON_TYPE: &str = "Spa:String:JSON";

/// The `default` metadata key WirePlumber reads a stream's chosen device from.
const TARGET_OBJECT_KEY: &str = "target.object";

/// Applies one [`AudioCommand`] on the PipeWire thread. Every arm resolves its target against live
/// maps first and logs rather than guessing when it cannot. Stale ids matter because PipeWire
/// recycles them. Results return through the same-loop event.
pub(super) fn apply_command(state: &Rc<RefCell<MixerState>>, command: AudioCommand) {
    debug!(2; "applying command: {command:?}");
    match command {
        AudioCommand::SetVolume { id, volume } => {
            set_levels(state, id, format_args!("volume {volume}"), |props, max| {
                master::cubed_channel_volumes(volume, &props.channel_volumes, max)
            })
        }
        AudioCommand::SetChannelVolume { id, index, volume } => {
            set_levels(state, id, format_args!("channel {index} volume {volume}"), |props, max| {
                master::channel_volume(props, index, volume, max)
            });
        }
        AudioCommand::SetBalance { id, balance } => {
            set_levels(state, id, format_args!("balance {balance}"), |props, _| {
                master::balanced_channel_volumes(props, balance)
            });
        }
        AudioCommand::SetMuted { id, muted } => set_muted(state, id, Some(muted)),
        AudioCommand::ToggleMute(id) => set_muted(state, id, None),
        AudioCommand::SetAppVolume { id, volume } => {
            let Some(current) = state.borrow().app_props.get(&id).cloned() else {
                debug!("set_app_volume({id}, {volume}) names a stream with no known Props; ignored");
                return;
            };
            let Some(channel_volumes) = master::cubed_channel_volumes(volume, &current.channel_volumes, 1.0) else {
                debug!("set_app_volume({id}, {volume}) names a stream that reports no channels; ignored");
                return;
            };
            write_node_props(state, id, Some(channel_volumes), None);
        }
        // Mute needs no channel count, but `write_node_props` also binds sinks and sources: a device
        // id must not mute a whole sink.
        AudioCommand::SetAppMuted { id, muted } => {
            if !state.borrow().apps.contains_key(&id) {
                debug!("set_app_muted({id}, {muted}) names no tracked stream; ignored");
                return;
            }
            write_node_props(state, id, None, Some(muted));
        }
        AudioCommand::SetAppTarget { id, device } => write_app_target(state, id, device),
        AudioCommand::SetDefaultSink(id) => write_default_device(state, DefaultDevice::Sink, id),
        AudioCommand::SetDefaultSource(id) => write_default_device(state, DefaultDevice::Source, id),
        AudioCommand::SetCardProfile { device, index } => write_card_profile(state, device, index),
        AudioCommand::SetPort { card, index } => write_port(state, card, index),
        // `registry` applies it, since creating a node needs the core.
        AudioCommand::SetNullSinks(_) => {}
    }
}

/// Writes the channel volumes `levels` derives from sink or source `id`'s current `Props` and its
/// direction's cap; every volume write shares it.
fn set_levels(
    state: &Rc<RefCell<MixerState>>,
    id: u32,
    what: std::fmt::Arguments,
    levels: impl FnOnce(&master::RawSinkProps, f32) -> Option<Vec<f32>>,
) {
    // ponytail: two writes within one PipeWire round trip can share stale Props; upgrade with write acknowledgments.
    let target = {
        let state = state.borrow();
        state.kind_of(id).and_then(|kind| Some((kind, levels(state.device_props(kind, id)?, kind.max_volume())?)))
    };
    let Some((kind, channels)) = target else {
        debug!("{what} for device {id} has no tracked device or channels to write; ignored");
        return;
    };
    write_device_volume(state, kind, id, Some(channels), None);
}

/// Pulls the default sink back to the cap when another client (`wpctl set-volume 5%+`) raised it past.
pub(super) fn cap_default_sink(state: &Rc<RefCell<MixerState>>) {
    let correction = {
        let state = state.borrow();
        state.default_node(DefaultDevice::Sink).and_then(|id| {
            let props = state.device_props(DefaultDevice::Sink, id)?;
            // The slack stops a loop: 1.5 written as 3.375 reads back as 1.4999999.
            if master::master_volume_from_props(props).volume <= master::SINK_MAX_VOLUME + 1e-3 {
                return None;
            }
            Some((
                id,
                master::cubed_channel_volumes(
                    master::SINK_MAX_VOLUME,
                    &props.channel_volumes,
                    master::SINK_MAX_VOLUME,
                )?,
            ))
        })
    };
    if let Some((id, channels)) = correction {
        write_device_volume(state, DefaultDevice::Sink, id, Some(channels), None);
    }
}

/// Sets or toggles a sink's or source's mute. `None` toggles using resolved `Props`; a set needs no
/// channel count because mute carries no `channelVolumes`.
fn set_muted(state: &Rc<RefCell<MixerState>>, id: u32, muted: Option<bool>) {
    let Some(kind) = state.borrow().kind_of(id) else {
        debug!("a mute names no tracked device {id}; ignored");
        return;
    };
    // A toggle needs the current value; an explicit set does not, so only the toggle waits for
    // the first `Props`. Requiring it for both dropped a mute keypress during startup.
    //
    // A routed device still needs its Route index before the write can be sent.
    let current = || state.borrow().device_props(kind, id).map(|props| !props.mute);
    let Some(muted) = muted.or_else(current) else {
        debug!("a mute toggle has no Props on device {id} to read; ignored");
        return;
    };
    write_device_volume(state, kind, id, None, Some(muted));
}

/// Writes through the owning object. A hardware sink's node `Props` accepts
/// `pw-cli set-param 59 Props '{ mute: true }'` but does nothing, while a stream write works;
/// the volume lives on the `Device` `Route`, and node `channelVolumes` only mirrors it.
/// Live `pw-cli set-param 49 Route '{ index: 2, device: 7, props: { channelVolumes: [...] },
/// save: true }'` moved the volume. Use a known route, or node `Props` for virtual/null sinks.
fn write_device_volume(
    state: &Rc<RefCell<MixerState>>,
    kind: DefaultDevice,
    node_id: u32,
    channel_volumes: Option<Vec<f32>>,
    muted: Option<bool>,
) {
    let route = state.borrow().device_entries(kind).get(&node_id).and_then(|entry| entry.route);
    match route {
        Some(route) => write_device_route(state, node_id, route, channel_volumes, muted),
        None => write_node_props(state, node_id, channel_volumes, muted),
    }
}

/// Sends `SPA_PARAM_Route` with nested `props`; `save: true` survives re-plug. The `index` is not
/// guessable: cards publish several routes, and only `Route` reports the active one for a given
/// `card.profile.device`; without it the write goes nowhere or to the wrong route.
fn write_device_route(
    state: &Rc<RefCell<MixerState>>,
    node_id: u32,
    route: DeviceRoute,
    channel_volumes: Option<Vec<f32>>,
    muted: Option<bool>,
) {
    let Some(index) =
        state.borrow().device_routes.get(&(route.device_id, route.profile_device)).map(|active| active.index)
    else {
        debug!(
            "node {node_id} routes through device {} port {}, whose active Route index has not been seen; ignored",
            route.device_id, route.profile_device
        );
        return;
    };
    let object = route::route_object(index, route.profile_device, channel_volumes, muted);
    set_device_param(state, route.device_id, pw::spa::param::ParamType::Route, &object);
}

/// Sends `SPA_PARAM_Profile`, which switches an ALSA card's outputs or a BlueZ device's codec; the
/// new profile returns through `bind_device`'s listener.
fn write_card_profile(state: &Rc<RefCell<MixerState>>, device_id: u32, index: i32) {
    set_device_param(state, device_id, pw::spa::param::ParamType::Profile, &profile::profile_object(index));
}

/// Routes card `card_id` to port `index` on [`port_device`]; the new route returns through
/// `bind_device`'s listener.
fn write_port(state: &Rc<RefCell<MixerState>>, card_id: u32, index: i32) {
    let Some(device) = port_device(&state.borrow(), card_id, index) else {
        debug!("set_port({card_id}, {index}) names no port of that card's active profile; ignored");
        return;
    };
    let object = route::route_object(index, device, None, None);
    set_device_param(state, card_id, pw::spa::param::ParamType::Route, &object);
}

/// The `card.profile.device` port `index` of `card_id` is routed on: one of its devices that a live
/// sink or source routes through. Those are the active profile's; `device_routes` still holds the
/// routes of a profile left behind.
fn port_device(state: &MixerState, card_id: u32, index: i32) -> Option<i32> {
    let port = state.cards.get(&card_id)?.ports.get(&index)?;
    let live = |device| {
        let route = Some(DeviceRoute { device_id: card_id, profile_device: device });
        state.sinks.values().chain(state.sources.values()).any(|entry| entry.route == route)
    };
    port.devices.iter().copied().find(|&device| live(device))
}

/// Sends `object` as `param` to bound device `device_id`, a card's `Route` or `Profile`.
fn set_device_param(
    state: &Rc<RefCell<MixerState>>,
    device_id: u32,
    param: pw::spa::param::ParamType,
    object: &pw::spa::pod::Value,
) {
    with_pod(object, format_args!("a {param:?} object for device {device_id}"), |pod| {
        let state = state.borrow();
        let Some((device, _listener)) = state.devices.get(&device_id) else {
            debug!("device {device_id} is not bound; its {param:?} write is dropped");
            return;
        };
        device.set_param(param, 0, pod);
    })
}

/// Sends `SPA_PARAM_Props` to a stream or a node-owned sink/source.
fn write_node_props(
    state: &Rc<RefCell<MixerState>>,
    node_id: u32,
    channel_volumes: Option<Vec<f32>>,
    muted: Option<bool>,
) {
    let object = master::props_object(channel_volumes, muted);
    with_pod(&object, format_args!("a Props object for node {node_id}"), |pod| {
        let state = state.borrow();
        let Some((node, _listener)) = state
            .sink_nodes
            .get(&node_id)
            .or_else(|| state.source_nodes.get(&node_id))
            .or_else(|| state.nodes.get(&node_id))
        else {
            debug!("no bound node {node_id} to write Props to; ignored");
            return;
        };
        node.set_param(pw::spa::param::ParamType::Props, 0, pod);
    })
}

/// Serializes `object` and hands the pod to `send`, logging `what` when either step fails. The
/// bytes live only in this frame: `Pod::from_bytes` borrows them and `set_param` reads that borrow
/// into C, so dropping the `Vec` before the send would hand PipeWire a dangling pointer.
fn with_pod(object: &pw::spa::pod::Value, what: std::fmt::Arguments, send: impl FnOnce(&pw::spa::pod::Pod)) {
    let Some(bytes) = master::serialize_props(object) else {
        debug!(2; "failed to serialize {what}; ignored");
        return;
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&bytes) else {
        debug!(2; "serialized {what} did not read back as a pod; ignored");
        return;
    };
    send(pod)
}

/// Writes `target.object` for stream `id` in the `default` metadata, how WirePlumber moves a
/// stream; `None` deletes it and the stream follows the default again. Named by `node.name`, which
/// WirePlumber matches as it does `object.serial`.
fn write_app_target(state: &Rc<RefCell<MixerState>>, id: u32, device: Option<u32>) {
    let state = state.borrow();
    let Some(app) = state.apps.get(&id) else {
        debug!("set_app_target({id}, {device:?}) names no tracked stream; ignored");
        return;
    };
    let kind = if app.recording { DefaultDevice::Source } else { DefaultDevice::Sink };
    let name = match device.map(|device| state.device_entries(kind).get(&device)) {
        None => None,
        Some(Some(entry)) => Some(entry.names.node_name.as_str()),
        Some(None) => {
            debug!("set_app_target({id}, {device:?}) names no {kind:?} for that stream; ignored");
            return;
        }
    };
    let Some((metadata, _listener)) = state.metadata.as_ref() else {
        debug!("the `default` metadata object is not bound; cannot move stream {id}");
        return;
    };
    metadata.set_property(id, TARGET_OBJECT_KEY, None, name);
}

/// Writes `audio:set_default_sink/source(id)`. Metadata names devices by `node.name`, so an id
/// read from the device array is translated through its tracked entry. System-wide defaults use `0`
/// (`pw-metadata` shows `update: id:0 key:'default.audio.sink'`).
fn write_default_device(state: &Rc<RefCell<MixerState>>, kind: DefaultDevice, id: u32) {
    let state = state.borrow();
    let node_name = state.device_entries(kind).get(&id).map(|entry| entry.names.node_name.clone());
    let Some(node_name) = node_name else {
        debug!("no tracked {kind:?} with registry id {id}; ignored");
        return;
    };
    let Some((metadata, _listener)) = state.metadata.as_ref() else {
        debug!("the `default` metadata object is not bound; cannot set the default {kind:?}");
        return;
    };
    let key = match kind {
        DefaultDevice::Sink => DEFAULT_AUDIO_SINK_KEY,
        DefaultDevice::Source => DEFAULT_AUDIO_SOURCE_KEY,
    };
    let value = serde_json::json!({ "name": node_name }).to_string();
    metadata.set_property(0, key, Some(METADATA_JSON_TYPE), Some(&value));
}

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    use super::super::devices::{Card, DeviceEntry};
    use super::super::state::PrivacySources;
    use super::*;

    #[test]
    fn a_port_is_routed_on_the_profile_device_the_active_profile_uses() {
        let (updates, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = MixerState::new(updates, privacy_updates);
        let port = route::Port {
            index: 3,
            name: "analog-output-headphones".to_string(),
            description: String::new(),
            port_type: None,
            output: true,
            available: true,
            profiles: vec![1, 2],
            devices: vec![4, 7],
        };
        state.cards.insert(51, Card { ports: [(3, port)].into_iter().collect(), ..Card::default() });
        state.device_routes.insert((51, 4), route::ActiveRoute { index: 2, port: None, props: None });
        assert_eq!(port_device(&state, 51, 3), None, "a route the previous profile left, with no node on it");

        let routed = |profile_device| DeviceEntry {
            route: Some(DeviceRoute { device_id: 51, profile_device }),
            ..DeviceEntry::default()
        };
        state.sinks.insert(60, routed(7));
        assert_eq!(port_device(&state, 51, 3), Some(7));
        assert_eq!(port_device(&state, 51, 9), None, "an unknown port");
        assert_eq!(port_device(&state, 52, 3), None, "another card's route does not count");
    }
}

//! Declared null sinks (ADR-0383): `adapter` objects this connection creates, so they go with it
//! and are made again on the next.

use std::cell::RefCell;

use pipewire as pw;
use serde_json::{Value, json};
use shared::action::VirtualDevice;
use shared::warn;

use super::state::MixerState;

/// Creates the null sinks in `declared` that this connection lacks, and destroys the ones it no
/// longer declares or declares differently.
pub(super) fn sync(state: &RefCell<MixerState>, core: &pw::core::CoreRc, declared: &[VirtualDevice]) {
    let mut state = state.borrow_mut();
    let stale: Vec<String> = state
        .null_sinks
        .iter()
        .filter(|(_, (device, _))| !declared.contains(device))
        .map(|(name, _)| name.clone())
        .collect();
    for name in stale {
        if let Some((_, node)) = state.null_sinks.remove(&name)
            && let Err(err) = core.destroy_object(node)
        {
            warn!("cannot remove null sink {name}: {err}");
        }
    }
    for device in declared {
        if state.null_sinks.contains_key(&device.name) {
            continue;
        }
        let mut props = pw::properties::PropertiesBox::new();
        for (key, value) in properties(device) {
            props.insert(key, value);
        }
        match core.create_object::<pw::node::Node>("adapter", &props) {
            Ok(node) => {
                state.null_sinks.insert(device.name.clone(), (device.clone(), node));
            }
            Err(err) => warn!("cannot create null sink {}: {err}", device.name),
        }
    }
}

/// The adapter's properties: Mantle's defaults, then the config's `args`, as PipeWire strings.
fn properties(device: &VirtualDevice) -> Vec<(String, String)> {
    let Value::Object(mut props) = json!({
        "factory.name": "support.null-audio-sink",
        "media.class": "Audio/Sink",
        "audio.position": "FL,FR",
        "monitor.channel-volumes": true,
        "node.name": device.name,
        "node.description": device.description.as_deref().unwrap_or(&device.name),
    }) else {
        return Vec::new();
    };
    props.extend(device.args.clone());
    // Last, so no `args` can keep the sink past the session.
    props.insert("object.linger".to_string(), false.into());
    props
        .into_iter()
        .map(|(key, value)| match value {
            Value::String(text) => (key, text),
            other => (key, other.to_string()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use shared::action::VirtualKind;

    use super::*;

    #[test]
    fn a_null_sink_is_a_non_lingering_adapter_with_the_configs_args_over_the_defaults() {
        let Value::Object(args) = json!({ "audio.position": ["MONO"], "object.linger": true }) else { unreachable!() };
        let device = VirtualDevice { kind: VirtualKind::NullSink, name: "silent".to_string(), description: None, args };
        let props: std::collections::HashMap<String, String> = properties(&device).into_iter().collect();
        assert_eq!(props["factory.name"], "support.null-audio-sink");
        assert_eq!(props["node.name"], "silent");
        assert_eq!(props["node.description"], "silent", "the name stands in for a missing description");
        assert_eq!(props["audio.position"], r#"["MONO"]"#, "PipeWire reads a JSON array from a string");
        assert_eq!(props["object.linger"], "false", "no config keeps a sink past the session");
    }
}

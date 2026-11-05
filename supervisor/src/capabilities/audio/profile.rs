//! Card profile parsing, for ALSA and BlueZ devices alike, and BlueZ codec names.

use pipewire::spa::pod::{Object, Property, Value};
use pipewire::spa::sys as spa_sys;
use pipewire::spa::utils::Id;

/// One card profile from `EnumProfile` or `Profile`, e.g. BlueZ index 2, `a2dp-sink-aac`, described
/// as `"High Fidelity Playback (A2DP Sink, codec AAC)"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Profile {
    pub index: i32,
    /// PipeWire's untranslated id, empty when the object carries none.
    pub name: String,
    pub description: String,
    /// `false` only when PipeWire reports the profile unavailable; unknown counts as available.
    pub available: bool,
    /// `Audio/Sink` and `Audio/Source` nodes the profile makes, from its `classes`.
    pub outputs: u32,
    pub inputs: u32,
}

/// Pulls a [`Profile`] from an `EnumProfile` or `Profile` object. `None` skips an object missing
/// its index or description.
pub(super) fn extract_profile(value: &Value) -> Option<Profile> {
    let Value::Object(object) = value else { return None };
    let mut index = None;
    let mut name = String::new();
    let mut description = None;
    let mut available = true;
    let (mut outputs, mut inputs) = (0, 0);
    for property in &object.properties {
        match (property.key, &property.value) {
            (spa_sys::SPA_PARAM_PROFILE_index, Value::Int(value)) => index = Some(*value),
            (spa_sys::SPA_PARAM_PROFILE_name, Value::String(value)) => name = value.clone(),
            (spa_sys::SPA_PARAM_PROFILE_description, Value::String(value)) => description = Some(value.clone()),
            (spa_sys::SPA_PARAM_PROFILE_available, Value::Id(Id(value))) => {
                available = *value != spa_sys::SPA_PARAM_AVAILABILITY_no;
            }
            // `[count, [class, nodes, "card.profile.devices", [device, ...]], ...]`, as `pw-dump` shows.
            (spa_sys::SPA_PARAM_PROFILE_classes, Value::Struct(classes)) => {
                for class in classes.iter().skip(1) {
                    let Value::Struct(class) = class else { continue };
                    let [Value::String(class), Value::Int(nodes), ..] = class.as_slice() else { continue };
                    let nodes = u32::try_from(*nodes).unwrap_or(0);
                    match class.as_str() {
                        "Audio/Sink" => outputs += nodes,
                        "Audio/Source" => inputs += nodes,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(Profile { index: index?, name, description: description?, available, outputs, inputs })
}

/// The codec after a BlueZ profile name's role, e.g. `"AAC"` from `a2dp-sink-aac`, or `None` for
/// `off` or a bare `a2dp-sink`. Read before the description because the name is never translated.
pub(super) fn codec_from_name(name: &str) -> Option<String> {
    let codec = ["a2dp-sink-", "a2dp-duplex-", "headset-head-unit-", "bap-sink-", "bap-duplex-"]
        .iter()
        .find_map(|role| name.strip_prefix(role))?;
    let spelled = match codec {
        "aptx" => "aptX".to_string(),
        "aptx_hd" => "aptX HD".to_string(),
        "aptx_ll" => "aptX LL".to_string(),
        "msbc" => "mSBC".to_string(),
        "faststream" => "FastStream".to_string(),
        other => other.replace('_', "-").to_uppercase(),
    };
    (!spelled.is_empty()).then_some(spelled)
}

/// The codec a BlueZ profile description names, e.g. `"AAC"` from
/// `"High Fidelity Playback (A2DP Sink, codec AAC)"`, or `None` for a profile without one, such as
/// `Off`. The fallback for a name [`codec_from_name`] cannot read; it matches only an English
/// description.
pub(super) fn codec_of(description: &str) -> Option<String> {
    let rest = &description[description.find("codec ")? + "codec ".len()..];
    let codec = rest.split([')', ',']).next()?.trim();
    (!codec.is_empty()).then(|| codec.to_string())
}

/// Builds the `SPA_PARAM_Profile` object that switches a device to profile `index`. `save: true`
/// keeps the choice for the device's next connection, as the other mixer writes do.
pub(super) fn profile_object(index: i32) -> Value {
    Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamProfile,
        id: spa_sys::SPA_PARAM_Profile,
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_PROFILE_index, Value::Int(index)),
            Property::new(spa_sys::SPA_PARAM_PROFILE_save, Value::Bool(true)),
        ],
    })
}

/// The MAC in WirePlumber's BlueZ card name, `bluez_card.AA_BB_CC_DD_EE_FF` to
/// `AA:BB:CC:DD:EE:FF`. The card's global props never carry `api.bluez5.address`: `impl-device.c`
/// copies only its `global_keys` (`device.name` among them) onto a device global.
pub(super) fn mac_from_card_name(name: &str) -> Option<String> {
    let address = name.strip_prefix("bluez_card.")?;
    (address.len() == 17).then(|| address.replace('_', ":"))
}

#[cfg(test)]
mod tests {
    use super::super::master::serialize_props;
    use super::*;

    #[test]
    fn a_profile_object_names_the_index_and_asks_to_be_saved() {
        // Round-tripped through bytes, because those bytes are what `set_param` hands to C.
        let bytes = serialize_props(&profile_object(3)).expect("a Profile object must serialize");
        let (_, value) = pipewire::spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(&bytes)
            .expect("the bytes must read back as a pod");
        let Value::Object(object) = value else { panic!("a Profile must be an object") };
        assert_eq!(object.id, spa_sys::SPA_PARAM_Profile);
        let has = |key, expected: Value| object.properties.iter().any(|p| p.key == key && p.value == expected);
        assert!(has(spa_sys::SPA_PARAM_PROFILE_index, Value::Int(3)));
        assert!(has(spa_sys::SPA_PARAM_PROFILE_save, Value::Bool(true)));
    }

    fn enum_profile(index: i32, description: &str, availability: u32) -> Value {
        Value::Object(Object {
            type_: spa_sys::SPA_TYPE_OBJECT_ParamProfile,
            id: spa_sys::SPA_PARAM_EnumProfile,
            properties: vec![
                Property::new(spa_sys::SPA_PARAM_PROFILE_index, Value::Int(index)),
                Property::new(spa_sys::SPA_PARAM_PROFILE_name, Value::String("a2dp-sink-aac".to_string())),
                Property::new(spa_sys::SPA_PARAM_PROFILE_description, Value::String(description.to_string())),
                Property::new(spa_sys::SPA_PARAM_PROFILE_available, Value::Id(Id(availability))),
            ],
        })
    }

    #[test]
    fn extract_profile_reads_index_description_and_availability() {
        let aac = "High Fidelity Playback (A2DP Sink, codec AAC)";
        assert_eq!(
            extract_profile(&enum_profile(2, aac, spa_sys::SPA_PARAM_AVAILABILITY_yes)),
            Some(Profile {
                index: 2,
                name: "a2dp-sink-aac".to_string(),
                description: aac.to_string(),
                available: true,
                outputs: 0,
                inputs: 0,
            })
        );
        let gone = enum_profile(3, aac, spa_sys::SPA_PARAM_AVAILABILITY_no);
        assert_eq!(extract_profile(&gone).map(|profile| profile.available), Some(false));
        assert_eq!(extract_profile(&Value::Bool(true)), None);
    }

    /// `output:analog-stereo+input:analog-stereo` on an Intel HDA card, as `pw-dump` recorded it:
    /// `[2, ["Audio/Source", 1, "card.profile.devices", [0]], ["Audio/Sink", 1, "card.profile.devices", [7]]]`.
    #[test]
    fn extract_profile_counts_the_sinks_and_sources_its_classes_name() {
        let class = |name: &str, device| {
            Value::Struct(vec![
                Value::String(name.to_string()),
                Value::Int(1),
                Value::String("card.profile.devices".to_string()),
                Value::ValueArray(pipewire::spa::pod::ValueArray::Int(vec![device])),
            ])
        };
        let Value::Object(mut duplex) = enum_profile(1, "Analog Stereo Duplex", spa_sys::SPA_PARAM_AVAILABILITY_yes)
        else {
            unreachable!()
        };
        let classes = vec![Value::Int(2), class("Audio/Source", 0), class("Audio/Sink", 7)];
        duplex.properties.push(Property::new(spa_sys::SPA_PARAM_PROFILE_classes, Value::Struct(classes)));
        let off = Value::Struct(vec![Value::Int(0)]);

        let counts = |profile: Option<Profile>| profile.map(|profile| (profile.outputs, profile.inputs));
        assert_eq!(counts(extract_profile(&Value::Object(duplex.clone()))), Some((1, 1)));
        duplex.properties.last_mut().unwrap().value = off;
        assert_eq!(counts(extract_profile(&Value::Object(duplex))), Some((0, 0)), "`off` makes no node");
    }

    #[test]
    fn codec_of_reads_the_codec_out_of_a_bluez_profile_description() {
        assert_eq!(codec_of("High Fidelity Playback (A2DP Sink, codec AAC)"), Some("AAC".to_string()));
        assert_eq!(codec_of("High Fidelity Playback (A2DP Sink, codec SBC-XQ)"), Some("SBC-XQ".to_string()));
        assert_eq!(codec_of("Headset Head Unit (HSP/HFP, codec mSBC)"), Some("mSBC".to_string()));
        assert_eq!(codec_of("Off"), None);
    }

    #[test]
    fn codec_from_name_reads_the_codec_after_the_profile_role() {
        assert_eq!(codec_from_name("a2dp-sink-aac"), Some("AAC".to_string()));
        assert_eq!(codec_from_name("a2dp-sink-sbc_xq"), Some("SBC-XQ".to_string()));
        assert_eq!(codec_from_name("a2dp-sink-aptx_hd"), Some("aptX HD".to_string()));
        assert_eq!(codec_from_name("headset-head-unit-msbc"), Some("mSBC".to_string()));
        assert_eq!(codec_from_name("a2dp-sink"), None, "a bare role names no codec");
        assert_eq!(codec_from_name("off"), None);
    }
}

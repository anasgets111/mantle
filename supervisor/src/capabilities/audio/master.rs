//! Master output volume/mute from the default sink's `SPA_PARAM_Props` pod and the
//! metadata lookup that selects that sink.
//!
//! It is absent from `info().props()`: after `Node::subscribe_params(&[ParamType::Props])`, it
//! arrives only in `param`. Live sink 59 reported `SPA_PROP_volume` 65539, `SPA_PROP_mute` 65540,
//! and `SPA_PROP_channelVolumes` 65544. `libspa-sys` bindgen gets these from `props.h`'s wrong
//! enum ordering, and `libspa` has no typed wrapper, so parsing uses raw `spa_sys` constants.
//!
//! **Linear vs cubic:** at 30%, scalar `SPA_PROP_volume` stayed `1.0`, while
//! `SPA_PROP_channelVolumes` was `[0.027004944, 0.027004944]` (`0.3^3 = 0.027`; `wpctl`/`pactl`
//! show `0.30`). Live `wpctl set-mute` confirmed `SPA_PROP_mute` flips independently. We
//! max and cube-root the channels, rather than using scalar `volume` or raw `channelVolumes`,
//! because stereo channels may differ.

use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Property, Value, ValueArray};
use pipewire::spa::sys as spa_sys;
use pipewire::spa::utils::Id;
use shared::state::audio::AudioChannel;

use crate::capabilities::scale::percent_from_fraction;

/// Master output volume, mute and balance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasterVolume {
    pub volume: f32,
    pub muted: bool,
    pub balance: Option<f32>,
}

/// The `SPA_PROP_mute`/`SPA_PROP_channelVolumes`/`SPA_PROP_channelMap` values the mixer pulls from
/// a sink `Props` pod, stripped of pod machinery so [`master_volume_from_props`] stays testable.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSinkProps {
    pub mute: bool,
    pub channel_volumes: Vec<f32>,
    pub channel_map: Vec<u32>,
}

/// Pulls [`RawSinkProps`] from `libspa`'s generic [`Value`] (always an object here, checked live).
/// Sinks emit two `Props` objects back to back on every subscription and mixer change (confirmed
/// live): `index=0` has mixer keys; `index=1` has unrelated ALSA `device`/`deviceName`/`cardName`/
/// `params`. A real bug treated the latter's missing
/// `channelVolumes` as empty and overwrote the value just set by `index=0`; absence now returns
/// `None`. A `Props` without a `mute` key (a volume-only update) keeps `previous_mute`.
pub fn extract_sink_props(value: &Value, previous_mute: bool) -> Option<RawSinkProps> {
    let Value::Object(object) = value else { return None };

    let mut mute = previous_mute;
    let mut channel_volumes = None;
    let mut channel_map = Vec::new();
    for property in &object.properties {
        match (property.key, &property.value) {
            (key, Value::Bool(value)) if key == spa_sys::SPA_PROP_mute => mute = *value,
            (key, Value::ValueArray(ValueArray::Float(values))) if key == spa_sys::SPA_PROP_channelVolumes => {
                channel_volumes = Some(values.clone());
            }
            (key, Value::ValueArray(ValueArray::Id(ids))) if key == spa_sys::SPA_PROP_channelMap => {
                channel_map = ids.iter().map(|Id(position)| *position).collect();
            }
            _ => {}
        }
    }
    Some(RawSinkProps { mute, channel_volumes: channel_volumes?, channel_map })
}

/// ponytail: a fixed output ceiling; upgrade path is a config-set one through an audio action.
pub const SINK_MAX_VOLUME: f32 = 1.5;

/// Converts raw `Props` to the reported value: the cube root of the loudest channel. Empty channels
/// report `0.0` instead of panicking.
pub fn master_volume_from_props(props: &RawSinkProps) -> MasterVolume {
    let peak_linear = props.channel_volumes.iter().copied().fold(0.0_f32, f32::max);
    MasterVolume { volume: peak_linear.cbrt(), muted: props.mute, balance: balance(props) }
}

fn position_name(position: u32) -> Option<&'static str> {
    match position {
        spa_sys::SPA_AUDIO_CHANNEL_MONO => Some("mono"),
        spa_sys::SPA_AUDIO_CHANNEL_FL => Some("front_left"),
        spa_sys::SPA_AUDIO_CHANNEL_FR => Some("front_right"),
        spa_sys::SPA_AUDIO_CHANNEL_FC => Some("front_center"),
        spa_sys::SPA_AUDIO_CHANNEL_LFE => Some("lfe"),
        spa_sys::SPA_AUDIO_CHANNEL_RL => Some("rear_left"),
        spa_sys::SPA_AUDIO_CHANNEL_RR => Some("rear_right"),
        spa_sys::SPA_AUDIO_CHANNEL_SL => Some("side_left"),
        spa_sys::SPA_AUDIO_CHANNEL_SR => Some("side_right"),
        spa_sys::SPA_AUDIO_CHANNEL_RC => Some("rear_center"),
        spa_sys::SPA_AUDIO_CHANNEL_FLC => Some("front_left_center"),
        spa_sys::SPA_AUDIO_CHANNEL_FRC => Some("front_right_center"),
        spa_sys::SPA_AUDIO_CHANNEL_TC => Some("top_center"),
        spa_sys::SPA_AUDIO_CHANNEL_TFL => Some("top_front_left"),
        spa_sys::SPA_AUDIO_CHANNEL_TFC => Some("top_front_center"),
        spa_sys::SPA_AUDIO_CHANNEL_TFR => Some("top_front_right"),
        spa_sys::SPA_AUDIO_CHANNEL_TRL => Some("top_rear_left"),
        spa_sys::SPA_AUDIO_CHANNEL_TRC => Some("top_rear_center"),
        spa_sys::SPA_AUDIO_CHANNEL_TRR => Some("top_rear_right"),
        _ => None,
    }
}

pub fn channel_levels(props: &RawSinkProps) -> Option<Vec<AudioChannel>> {
    if props.channel_volumes.is_empty() {
        return None;
    }
    let mapped = props.channel_map.len() == props.channel_volumes.len();
    props
        .channel_volumes
        .iter()
        .enumerate()
        .map(|(index, &volume)| {
            if !volume.is_finite() || volume < 0.0 {
                return None;
            }
            Some(AudioChannel {
                index: u32::try_from(index).ok()?,
                position: mapped.then(|| position_name(props.channel_map[index])).flatten().map(str::to_string),
                volume: percent_from_fraction(f64::from(volume.cbrt())),
            })
        })
        .collect()
}

pub fn channel_volume(props: &RawSinkProps, index: u32, volume: f32, max: f32) -> Option<Vec<f32>> {
    if !volume.is_finite() {
        return None;
    }
    let mut channels = props.channel_volumes.clone();
    *channels.get_mut(usize::try_from(index).ok()?)? = volume.clamp(0.0, max).powi(3);
    Some(channels)
}

/// Parses either `default.audio.sink` or `default.audio.source`, whose live `pw-metadata` shape is
/// `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}`, into `node.name`. `None` means no
/// default is known yet.
pub fn parse_default_device_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("name")?.as_str().map(str::to_string)
}

/// Resolves the metadata name against tracked `Audio/Sink` or `Audio/Source` `node.name` values,
/// not registry ids, for the `sinks`/`sources` active flag.
///
/// ponytail: startup metadata and `global` events have no ordering guarantee, and removal may
/// have no fresh `default.audio.sink` update. An untracked name falls back to the lowest id,
/// correct only with one sink; upgrade path: track the previous resolved id.
pub fn resolve_default_device<'a>(
    default_name: Option<&str>,
    names: impl Iterator<Item = (u32, &'a str)>,
) -> Option<u32> {
    let mut matched: Option<u32> = None;
    let mut lowest: Option<u32> = None;
    for (id, name) in names {
        if default_name == Some(name) {
            // Lowest id wins a tie, not map iteration order: HashMap order isn't stable, and
            // without this the active device would flip between publishes of an unchanged map.
            matched = Some(matched.map_or(id, |current| current.min(id)));
        }
        lowest = Some(lowest.map_or(id, |low| low.min(id)));
    }
    matched.or(lowest)
}

/// Inverse of [`master_volume_from_props`]'s cube root: scales `current` so its loudest channel
/// reads `target`, clamped to `[0.0, max]`; all-silent channels spread evenly, having no balance to
/// keep. PipeWire stores `channelVolumes` cubed, so skipping this wrote 30% as 67%, the read mistake
/// (ADR-0053).
pub fn cubed_channel_volumes(target: f32, current: &[f32], max: f32) -> Option<Vec<f32>> {
    if current.is_empty() {
        // PipeWire accepts and ignores an empty channelVolumes array, so set_volume would report
        // success without changing anything. Refuse it so the caller can report failure.
        return None;
    }
    let cubed = target.clamp(0.0, max).powi(3);
    let peak = current.iter().copied().fold(0.0_f32, f32::max);
    Some(current.iter().map(|c| if peak > 0.0 { c * cubed / peak } else { cubed }).collect())
}

/// PulseAudio's balance side (`channelmap.c` `on_left`/`on_right`): `0` left, `1` right.
fn side(position: u32) -> Option<usize> {
    match position {
        spa_sys::SPA_AUDIO_CHANNEL_FL
        | spa_sys::SPA_AUDIO_CHANNEL_RL
        | spa_sys::SPA_AUDIO_CHANNEL_SL
        | spa_sys::SPA_AUDIO_CHANNEL_FLC
        | spa_sys::SPA_AUDIO_CHANNEL_TFL
        | spa_sys::SPA_AUDIO_CHANNEL_TRL => Some(0),
        spa_sys::SPA_AUDIO_CHANNEL_FR
        | spa_sys::SPA_AUDIO_CHANNEL_RR
        | spa_sys::SPA_AUDIO_CHANNEL_SR
        | spa_sys::SPA_AUDIO_CHANNEL_FRC
        | spa_sys::SPA_AUDIO_CHANNEL_TFR
        | spa_sys::SPA_AUDIO_CHANNEL_TRR => Some(1),
        _ => None,
    }
}

/// Mean cube root of the left and the right channels; `None` without both or with a mismatched map.
fn sides(props: &RawSinkProps) -> Option<[f32; 2]> {
    if props.channel_volumes.len() != props.channel_map.len() {
        return None;
    }
    let (mut sum, mut count) = ([0.0_f32; 2], [0.0_f32; 2]);
    for (volume, &position) in props.channel_volumes.iter().zip(&props.channel_map) {
        if let Some(index) = side(position) {
            sum[index] += volume.cbrt();
            count[index] += 1.0;
        }
    }
    (count[0] > 0.0 && count[1] > 0.0).then(|| [sum[0] / count[0], sum[1] / count[1]])
}

/// PulseAudio's `pa_cvolume_get_balance`: `-1.0` fully left, `1.0` fully right.
fn balance(props: &RawSinkProps) -> Option<f32> {
    let [left, right] = sides(props)?;
    // Silence reads centered.
    Some((right - left) / left.max(right).max(f32::MIN_POSITIVE))
}

/// PulseAudio's `pa_cvolume_set_balance` on cubed channels. Side averages can push one channel past
/// the cap, so each is clamped.
pub fn balanced_channel_volumes(props: &RawSinkProps, target: f32) -> Option<Vec<f32>> {
    let old = sides(props)?;
    let (loud, target) = (old[0].max(old[1]), target.clamp(-1.0, 1.0));
    let new = [loud * (1.0 - target.max(0.0)), loud * (1.0 + target.min(0.0))];
    let scaled = props.channel_volumes.iter().zip(&props.channel_map).map(|(&volume, &position)| {
        let volume = match side(position) {
            Some(index) if old[index] > 0.0 => volume * (new[index] / old[index]).powi(3),
            Some(index) => new[index].powi(3),
            None => volume,
        };
        volume.min(SINK_MAX_VOLUME.powi(3))
    });
    Some(scaled.collect())
}

/// Builds the partial `SPA_PARAM_Props` object accepted by `Node::set_param`. Live
/// `pw-cli set-param <stream> Props '{ mute: true }'` left volume alone; sending both fields loses
/// a same-tick write because state is not optimistic (`set_app_volume(0.42)` then mute sent the
/// pre-write volume).
pub fn props_object(channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    props_object_with_id(spa_sys::SPA_PARAM_Props, channel_volumes, muted)
}

/// The same object under a caller-chosen id. `Props` nested in a `Route` must use
/// `SPA_PARAM_Route`, not `SPA_PARAM_Props`; PipeWire accepts the standalone id and ignores it
/// (confirmed with `pw-cli enum-params <device> Route`).
pub(super) fn props_object_with_id(id: u32, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    let mut properties = Vec::new();
    if let Some(muted) = muted {
        properties.push(Property::new(spa_sys::SPA_PROP_mute, Value::Bool(muted)));
    }
    if let Some(channel_volumes) = channel_volumes {
        properties.push(Property::new(
            spa_sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(channel_volumes)),
        ));
    }
    Value::Object(Object { type_: spa_sys::SPA_TYPE_OBJECT_Props, id, properties })
}

/// Serializes [`props_object`]'s output into bytes for `Pod::from_bytes`; callers keep those
/// bytes alive while the borrowed `&Pod` is in use.
pub fn serialize_props(object: &Value) -> Option<Vec<u8>> {
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), object).ok()?;
    Some(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn names(map: &HashMap<u32, String>) -> impl Iterator<Item = (u32, &str)> {
        map.iter().map(|(&id, name)| (id, name.as_str()))
    }

    fn raw(channel_volumes: &[f32], channel_map: &[u32]) -> RawSinkProps {
        RawSinkProps { mute: false, channel_volumes: channel_volumes.to_vec(), channel_map: channel_map.to_vec() }
    }

    fn sample_props_object() -> Value {
        Value::Object(pipewire::spa::pod::Object {
            type_: 262146,
            id: 2,
            properties: vec![
                pipewire::spa::pod::Property::new(spa_sys::SPA_PROP_volume, Value::Float(1.0)),
                pipewire::spa::pod::Property::new(spa_sys::SPA_PROP_mute, Value::Bool(false)),
                pipewire::spa::pod::Property::new(
                    spa_sys::SPA_PROP_channelVolumes,
                    Value::ValueArray(pipewire::spa::pod::ValueArray::Float(vec![0.027004944, 0.027004944])),
                ),
            ],
        })
    }

    #[test]
    fn extract_sink_props_reads_mute_and_channel_volumes_from_a_real_props_object_and_rejects_a_non_object_pod() {
        let extracted = extract_sink_props(&sample_props_object(), false).expect("should parse as sink props");
        assert!(!extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.027004944, 0.027004944]);
        assert_eq!(extract_sink_props(&Value::Float(1.0), false), None, "a non-object pod is rejected");
    }

    #[test]
    fn extract_sink_props_keeps_the_previous_mute_when_the_key_is_absent() {
        let value = Value::Object(pipewire::spa::pod::Object {
            type_: 262146,
            id: 2,
            properties: vec![pipewire::spa::pod::Property::new(
                spa_sys::SPA_PROP_channelVolumes,
                Value::ValueArray(pipewire::spa::pod::ValueArray::Float(vec![1.0])),
            )],
        });
        assert!(extract_sink_props(&value, true).unwrap().mute);
        assert!(!extract_sink_props(&value, false).unwrap().mute);
    }

    /// Regression test: `wpctl get-volume` said 0.45, but the next `mantle.audio` push reported
    /// `0.0` with no real change in between. Root cause, found with a live probe: a sink's Props
    /// param isn't one object, it's two, delivered back to back -- `index=0` (modeled by
    /// [`sample_props_object`]) carries `volume`/`mute`/`channelVolumes`; `index=1` carries
    /// unrelated ALSA device settings with none of those keys. This is `index=1`'s real shape,
    /// keys taken verbatim from that probe (257/258/261 are `device`/`deviceName`/`cardName`,
    /// 65550 is `SPA_PROP_latencyOffsetNsec`, 524289 a nested `params` struct). Before the fix
    /// this returned a "parsed" zero that clobbered the value `index=0` had just set; it must
    /// return `None` so the caller leaves the previously tracked volume alone.
    #[test]
    fn extract_sink_props_ignores_the_alsa_device_settings_props_object() {
        let device_settings_object = Value::Object(pipewire::spa::pod::Object {
            type_: 262146,
            id: 2,
            properties: vec![
                pipewire::spa::pod::Property::new(257, Value::String("front:0".to_string())),
                pipewire::spa::pod::Property::new(258, Value::String(String::new())),
                pipewire::spa::pod::Property::new(261, Value::String(String::new())),
                pipewire::spa::pod::Property::new(65550, Value::Long(0)),
                pipewire::spa::pod::Property::new(524289, Value::Struct(vec![])),
            ],
        });
        assert_eq!(
            extract_sink_props(&device_settings_object, false),
            None,
            "a Props object with no channelVolumes key is not a mixer update and must not report a fabricated zero volume"
        );
    }

    #[test]
    fn master_volume_from_props_cube_roots_the_loudest_channel_and_reports_mute_independently() {
        // (volumes, mute, expected volume): wpctl showed 30% while channelVolumes carried 0.3^3.
        for (volumes, mute, expected) in [
            (&[0.027004944, 0.027004944][..], false, 0.3),
            (&[0.0, 1.0], false, 1.0),
            (&[0.027004944, 0.027004944], true, 0.3),
        ] {
            let master = master_volume_from_props(&RawSinkProps { mute, ..raw(volumes, &[]) });
            assert!((master.volume - expected).abs() < 0.001, "{volumes:?}: got {}", master.volume);
            assert_eq!(master.muted, mute, "{volumes:?}");
        }
    }

    #[test]
    fn master_volume_from_props_handles_an_empty_channel_array_without_panicking() {
        let props = raw(&[], &[]);
        assert_eq!(master_volume_from_props(&props).volume, 0.0);
        assert_eq!(channel_levels(&props), None);
    }

    #[test]
    fn channel_write_changes_one_reported_channel_and_clamps_the_target() {
        let props = raw(&[0.125, 0.125], &[]);
        assert_eq!(channel_volume(&props, 1, 1.5, 1.0), Some(vec![0.125, 1.0]));
        assert_eq!(channel_volume(&props, 0, 0.2, 1.5), Some(vec![0.2f32.powi(3), 0.125]));
        assert_eq!(channel_volume(&props, 2, 0.2, 1.0), None);
        assert_eq!(channel_volume(&props, 0, f32::NAN, 1.0), None);
    }

    #[test]
    fn parse_default_device_name_reads_the_name_key_and_rejects_malformed_json_or_a_missing_key() {
        let json = r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#;
        assert_eq!(parse_default_device_name(json), Some("alsa_output.pci-0000_00_1f.3.analog-stereo".to_string()));
        assert_eq!(parse_default_device_name("not json"), None);
        assert_eq!(parse_default_device_name("{}"), None);
    }

    #[test]
    fn resolve_default_device_matches_by_name_else_falls_back_to_the_lowest_id() {
        let both = HashMap::from([(70, "bluez_output.headset".to_string()), (59, "alsa_output.analog".to_string())]);
        let alsa = HashMap::from([(59, "alsa_output.analog".to_string())]);
        for (case, default_name, sinks, expected) in [
            ("by name", Some("bluez_output.headset"), &both, Some(70)),
            ("no default name", None, &both, Some(59)),
            ("named sink not tracked yet", Some("bluez_output.not-seen-yet"), &alsa, Some(59)),
            ("no sinks", None, &HashMap::new(), None),
        ] {
            assert_eq!(resolve_default_device(default_name, names(sinks)), expected, "{case}");
        }
    }

    #[test]
    fn cubed_channel_volumes_is_the_exact_inverse_of_the_read_direction() {
        let volumes = cubed_channel_volumes(0.3, &[1.0, 1.0], 1.0).expect("two channels is not zero");
        let read_back = master_volume_from_props(&raw(&volumes, &[]));
        assert!((read_back.volume - 0.3).abs() < 1e-6, "expected ~0.3, got {}", read_back.volume);
    }

    #[test]
    fn cubed_channel_volumes_keeps_balance_and_spreads_evenly_over_silent_channels() {
        assert_eq!(cubed_channel_volumes(0.5, &[0.125, 1.0], 1.0), Some(vec![0.015625, 0.125]));
        assert_eq!(cubed_channel_volumes(0.5, &[0.0; 6], 1.0), Some(vec![0.125; 6]));
    }

    #[test]
    fn cubed_channel_volumes_clamps_a_value_outside_the_specified_range() {
        assert_eq!(cubed_channel_volumes(2.0, &[1.0], 1.0), Some(vec![1.0]));
        assert_eq!(cubed_channel_volumes(-0.5, &[1.0], 1.0), Some(vec![0.0]));
        let corrected = cubed_channel_volumes(SINK_MAX_VOLUME, &[8.0, 1.0], SINK_MAX_VOLUME);
        assert_eq!(corrected, Some(vec![3.375, 0.421875]));
        assert_eq!(cubed_channel_volumes(0.5, &[], 1.0), None, "a device reporting no channels is refused");
    }

    const STEREO: [u32; 2] = [spa_sys::SPA_AUDIO_CHANNEL_FL, spa_sys::SPA_AUDIO_CHANNEL_FR];

    #[test]
    fn a_set_balance_reads_back_keeping_the_louder_side_under_the_cap() {
        let volumes = balanced_channel_volumes(&raw(&[1.0, 1.0], &STEREO), -0.36).unwrap();
        assert_eq!(volumes[0], 1.0);
        let read_back = balance(&raw(&volumes, &STEREO)).unwrap();
        assert!((read_back + 0.36).abs() < 1e-5, "expected ~-0.36, got {read_back}");
        assert_eq!(balanced_channel_volumes(&raw(&[0.125, 1.0], &STEREO), 0.0), Some(vec![1.0, 1.0]));
        let quad = [STEREO[0], STEREO[1], spa_sys::SPA_AUDIO_CHANNEL_RL, spa_sys::SPA_AUDIO_CHANNEL_RR];
        assert_eq!(
            balanced_channel_volumes(&raw(&[1.0, 3.375, 1.0, 0.0], &quad), 0.0),
            Some(vec![1.0, 3.375, 1.0, 0.0])
        );
    }

    #[test]
    fn balance_is_none_without_both_sides_and_centered_for_silence() {
        let mono = [spa_sys::SPA_AUDIO_CHANNEL_MONO];
        assert_eq!(balance(&raw(&[1.0], &mono)), None);
        assert_eq!(balanced_channel_volumes(&raw(&[1.0], &mono), 0.5), None);
        assert_eq!(balance(&raw(&[1.0, 1.0], &[])), None);
        assert_eq!(balance(&raw(&[0.0, 0.0], &STEREO)), Some(0.0));
        assert_eq!(balanced_channel_volumes(&raw(&[0.0, 0.0], &STEREO), 0.5), Some(vec![0.0, 0.0]));
    }

    #[test]
    fn a_props_object_carries_only_the_field_the_caller_is_changing() {
        // Sending both loses a write; see props_object's doc comment.
        let Value::Object(volume_only) = props_object(Some(vec![0.5]), None) else {
            panic!("props_object must build an object")
        };
        assert_eq!(volume_only.properties.len(), 1);
        assert_eq!(volume_only.properties[0].key, spa_sys::SPA_PROP_channelVolumes);

        let Value::Object(mute_only) = props_object(None, Some(true)) else {
            panic!("props_object must build an object")
        };
        assert_eq!(mute_only.properties.len(), 1);
        assert_eq!(mute_only.properties[0].key, spa_sys::SPA_PROP_mute);
    }

    #[test]
    fn a_serialized_props_object_reads_back_as_the_same_values() {
        // The write path hands these bytes to C; disagreement would make an accepted write no-op.
        let bytes = serialize_props(&props_object(Some(vec![0.027, 0.027]), Some(true)))
            .expect("a Props object must serialize");
        let (_, value) = pipewire::spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(&bytes)
            .expect("the bytes must read back as a pod");
        let extracted = extract_sink_props(&value, false).expect("the round trip must still parse as sink props");
        assert!(extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.027, 0.027]);
    }
}

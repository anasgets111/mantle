//! Assembles the `mantle.audio` and `mantle.privacy` payloads from [`MixerState`]'s maps.

use super::devices::{card_list, device_list};
use super::state::{AudioState, DefaultDevice, MixerState, PrivacySources};
use super::streams::{AppStream, running};
use crate::capabilities::audio::master;
use crate::capabilities::scale::percent_from_fraction;

impl MixerState {
    /// Publishes even if the receiver is absent; that is startup or shutdown, not a tracking error.
    ///
    /// Silent until [`MixerState::hydrated`], so the first snapshot is complete: volumes, device and privacy lists.
    pub(super) fn publish_audio(&mut self) {
        if !self.hydrated {
            return;
        }
        let (sink, source) = (self.default_node(DefaultDevice::Sink), self.default_node(DefaultDevice::Source));
        // Join here: identity and volume arrive on unordered PipeWire `info` and `param` events;
        // folding volume in at info time could overwrite a reading already landed.
        let apps = self
            .apps
            .values()
            .cloned()
            .map(|app| match self.app_props.get(&app.id).map(master::master_volume_from_props) {
                Some(measured) => AppStream {
                    volume: Some(percent_from_fraction(f64::from(measured.volume))),
                    muted: measured.muted,
                    ..app
                },
                None => app,
            })
            .map(|app| {
                let devices = if app.recording { &self.sources } else { &self.sinks };
                let target = self.graph.linked(app.id, !app.recording).find(|node| devices.contains_key(node));
                AppStream { target, ..app }
            })
            .collect();
        let next = AudioState {
            sinks: device_list(&self.sinks, &self.device_routes, sink),
            sources: device_list(&self.sources, &self.device_routes, source),
            apps,
            cards: card_list(&self.cards, &self.device_routes),
        };
        if Some(&next) == self.last_sent.as_ref() {
            return;
        }
        let _ = self.updates.send(next.clone());
        self.last_sent = Some(next);
    }

    /// Publishes all three privacy lists together (ADR-0137), even when only one changed.
    /// Recomputing the two it did not touch is three map walks.
    pub(super) fn publish_privacy(&self) {
        if !self.hydrated {
            return;
        }
        let _ = self.privacy_updates.send(PrivacySources {
            cameras: self.graph.camera_consumers(&self.camera_streams),
            pipewire_pid: self.graph.core_pid,
            microphones: running(&self.microphones),
            screencasts: running(&self.screencasts),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use tokio::sync::mpsc::UnboundedSender;
    use tokio::sync::watch;

    use super::super::devices::{
        AudioCard, AudioDevice, Card, CardPort, CardProfile, DeviceEntry, DeviceNames, DeviceRoute, PortDirection,
    };
    use super::super::streams::CaptureApp;
    use super::*;
    use crate::capabilities::audio::{profile, route};
    use shared::state::audio::AudioChannel;

    fn sample_stream(node_id: u32) -> AppStream {
        AppStream {
            id: node_id,
            pid: 100 + node_id as i32,
            name: Some(format!("app-{node_id}")),
            media_name: None,
            process_name: None,
            binary: None,
            icon: None,
            recording: false,
            volume: None,
            muted: false,
            target: None,
        }
    }

    fn sink_at(node_name: &str, description: Option<&str>, props: Option<master::RawSinkProps>) -> DeviceEntry {
        DeviceEntry {
            names: DeviceNames {
                node_name: node_name.to_string(),
                description: description.map(str::to_string),
                ..DeviceNames::default()
            },
            props,
            ..DeviceEntry::default()
        }
    }

    #[test]
    fn audio_state_serializes_as_the_flat_shape_docs_adr_0053_specifies() {
        // 50 avoids serde_json's long widened-f32 tail, keeping this about field shape.
        let stream = sample_stream(1);
        let state = AudioState {
            sinks: vec![AudioDevice {
                id: 59,
                node_name: "alsa_output.analog-stereo".to_string(),
                name: "Built-in Audio Analog Stereo".to_string(),
                active: true,
                card: Some(51),
                volume: Some(50.0),
                icon: Some("audio-card-analog".to_string()),
                ..AudioDevice::default()
            }],
            sources: vec![AudioDevice {
                id: 60,
                node_name: "alsa_input.analog-stereo".to_string(),
                name: "Built-in Audio Analog Stereo".to_string(),
                active: true,
                volume: Some(25.0),
                muted: true,
                ..AudioDevice::default()
            }],
            apps: vec![stream.clone()],
            cards: vec![
                AudioCard {
                    device: 51,
                    name: "Built-in Audio".to_string(),
                    mac: None,
                    profiles: vec![CardProfile {
                        index: 3,
                        name: "output:hdmi-stereo".to_string(),
                        description: "Digital Stereo (HDMI) Output".to_string(),
                        codec: None,
                        available: false,
                    }],
                    active: None,
                    ports: vec![CardPort {
                        index: 2,
                        name: "analog-output-headphones".to_string(),
                        description: "Headphones".to_string(),
                        port_type: Some("headphones".to_string()),
                        direction: PortDirection::Output,
                        available: false,
                        active: false,
                        profiles: vec![1],
                    }],
                },
                AudioCard {
                    device: 80,
                    name: "WH-1000XM4".to_string(),
                    mac: Some("AA:BB:CC:DD:EE:FF".to_string()),
                    profiles: vec![CardProfile {
                        index: 2,
                        name: "a2dp-sink-aac".to_string(),
                        description: "High Fidelity Playback (A2DP Sink, codec AAC)".to_string(),
                        codec: Some("AAC".to_string()),
                        available: true,
                    }],
                    active: Some(2),
                    ports: vec![],
                },
            ],
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "sinks": [{
                    "id": 59,
                    "node_name": "alsa_output.analog-stereo",
                    "name": "Built-in Audio Analog Stereo",
                    "active": true,
                    "card": 51,
                    "volume": 50.0,
                    "muted": false,
                    "icon": "audio-card-analog",
                }],
                "sources": [{
                    "id": 60,
                    "node_name": "alsa_input.analog-stereo",
                    "name": "Built-in Audio Analog Stereo",
                    "active": true,
                    "volume": 25.0,
                    "muted": true,
                }],
                "apps": [{
                    "id": stream.id,
                    "pid": stream.pid,
                    "name": stream.name,
                    "process_name": stream.process_name,
                    "recording": false,
                    "muted": stream.muted,
                }],
                "cards": [
                    {
                        "device": 51,
                        "name": "Built-in Audio",
                        "profiles": [{
                            "index": 3,
                            "name": "output:hdmi-stereo",
                            "description": "Digital Stereo (HDMI) Output",
                            "available": false,
                        }],
                        "ports": [{
                            "index": 2,
                            "name": "analog-output-headphones",
                            "description": "Headphones",
                            "type": "headphones",
                            "direction": "output",
                            "available": false,
                            "active": false,
                            "profiles": [1],
                        }],
                    },
                    {
                        "device": 80,
                        "name": "WH-1000XM4",
                        "mac": "AA:BB:CC:DD:EE:FF",
                        "profiles": [{
                            "index": 2,
                            "name": "a2dp-sink-aac",
                            "description": "High Fidelity Playback (A2DP Sink, codec AAC)",
                            "codec": "AAC",
                            "available": true,
                        }],
                        "active": 2,
                        "ports": [],
                    },
                ],
            })
        );
    }

    /// Raw `Props` at a linear volume, so tests name the linear value rather than its cube.
    fn props_at(linear: f32, muted: bool) -> master::RawSinkProps {
        master::RawSinkProps { mute: muted, channel_volumes: vec![linear.powi(3); 2], channel_map: vec![] }
    }

    /// `publish_audio` fixture with empty proxy maps; tests fill only fields they exercise.
    ///
    /// Hydrated, because every test below asserts on what a publish carries. The gate itself is
    /// pinned by `a_state_that_has_not_hydrated_publishes_nothing`.
    fn mixer_state(updates: UnboundedSender<AudioState>, privacy_updates: watch::Sender<PrivacySources>) -> MixerState {
        MixerState { hydrated: true, ..MixerState::new(updates, privacy_updates) }
    }

    #[test]
    fn a_state_that_has_not_hydrated_publishes_nothing() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        state.hydrated = false;
        // A bound sink with no `Props` yet: exactly the shape the first `global` burst leaves.
        state.sinks = HashMap::from([(59, sink_at("alsa_output.pci-...analog-stereo", None, None))]);
        state.default_sink_name = Some("alsa_output.pci-...analog-stereo".to_string());

        state.publish_audio();
        state.publish_privacy();

        assert!(rx.try_recv().is_err(), "an unhydrated state must not publish its unknown master volume");
        assert!(!privacy_rx.has_changed().unwrap(), "an unhydrated state must not publish its empty privacy lists");

        state.hydrated = true;
        state.publish_audio();
        state.publish_privacy();
        assert!(rx.try_recv().is_ok(), "opening the gate must publish audio on the next call");
        assert!(privacy_rx.has_changed().unwrap(), "opening the gate must publish privacy on the next call");
    }

    #[test]
    fn publish_audio_reports_each_devices_volume_and_the_app_snapshot() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        state.sinks =
            HashMap::from([(59, sink_at("alsa_output.pci-...analog-stereo", None, Some(props_at(0.3, false))))]);
        state.default_sink_name = Some("alsa_output.pci-...analog-stereo".to_string());
        state.apps.insert(1, sample_stream(1));

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.sinks[0].volume, Some(30.0), "a whole percent, not f32 noise");
        assert!(!published.sinks[0].muted);
        assert_eq!(published.apps, vec![sample_stream(1)]);
    }

    #[test]
    fn publish_audio_lists_alsa_and_bluez_cards_with_every_profile_and_its_codec() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        let profile = |index: i32, name: &str, description: &str, available: bool| {
            let profile =
                profile::Profile { index, name: name.to_string(), description: description.to_string(), available };
            (index, profile)
        };
        state.cards = HashMap::from([
            (
                80,
                Card {
                    mac: Some("AA:BB:CC:DD:EE:FF".to_string()),
                    profiles: [
                        profile(0, "off", "Off", true),
                        profile(1, "a2dp-sink-sbc", "High Fidelity Playback (A2DP Sink, codec SBC)", true),
                        // A translated description names no codec in English; the name still does.
                        profile(2, "a2dp-sink-aac", "High-Fidelity-Wiedergabe (A2DP-Senke, Codec AAC)", true),
                        profile(3, "a2dp-sink-ldac", "High Fidelity Playback (A2DP Sink, codec LDAC)", false),
                    ]
                    .into_iter()
                    .collect(),
                    active: Some(2),
                    ..Card::default()
                },
            ),
            (
                51,
                Card {
                    name: "Built-in Audio".to_string(),
                    profiles: [
                        profile(0, "off", "Off", true),
                        profile(1, "output:analog-stereo", "Analog Stereo Output", true),
                        profile(4, "output:hdmi-stereo", "Digital Stereo (HDMI) Output", false),
                    ]
                    .into_iter()
                    .collect(),
                    active: Some(1),
                    ..Card::default()
                },
            ),
        ]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        let cards: Vec<(u32, Option<&str>, Option<i32>)> =
            published.cards.iter().map(|card| (card.device, card.mac.as_deref(), card.active)).collect();
        assert_eq!(cards, [(51, None, Some(1)), (80, Some("AA:BB:CC:DD:EE:FF"), Some(2))], "both kinds, by device");
        let profiles = |card: &AudioCard| -> Vec<(i32, Option<String>, bool)> {
            card.profiles.iter().map(|profile| (profile.index, profile.codec.clone(), profile.available)).collect()
        };
        assert_eq!(
            profiles(&published.cards[0]),
            [(0, None, true), (1, None, true), (4, None, false)],
            "unplugged HDMI stays listed, flagged unavailable, and no ALSA profile names a codec"
        );
        let codec = |name: &str| Some(name.to_string());
        assert_eq!(
            profiles(&published.cards[1]),
            [(0, None, true), (1, codec("SBC"), true), (2, codec("AAC"), true), (3, codec("LDAC"), false)],
            "off names no codec, a translated description still names one, and LDAC stays flagged"
        );
        assert_eq!(published.cards[0].name, "Built-in Audio");
    }

    #[test]
    fn publish_privacy_carries_camera_consumers_and_the_daemon_pid_until_the_stream_goes() {
        let (updates, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        let camera = HashMap::from([("device.id".to_string(), "54".to_string())]);
        let link = HashMap::from([
            ("link.output.node".to_string(), "63".to_string()),
            ("link.input.node".to_string(), "94".to_string()),
        ]);
        state.graph.core_pid = Some(7842);
        state.graph.add_camera(63, &camera);
        state.graph.add_link(70, &link);
        state.camera_streams.insert(94, CaptureApp { node_id: 94, pid: None, app_name: None, running: true });

        state.publish_privacy();
        assert_eq!((privacy_rx.borrow().cameras.len(), privacy_rx.borrow().pipewire_pid), (1, Some(7842)));

        state.remove_global(94);
        assert!(privacy_rx.borrow().cameras.is_empty(), "a removed stream must leave the camera list");
    }

    /// A config drawing volume but not privacy starts the mixer and never reads this channel.
    #[test]
    fn privacy_publishes_nobody_reads_keep_only_the_latest() {
        let (updates, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);

        for node_id in 1..=3 {
            state.microphones =
                BTreeMap::from([(node_id, CaptureApp { node_id, pid: None, app_name: None, running: true })]);
            state.publish_privacy();
        }

        let held = privacy_rx.borrow();
        assert_eq!(held.microphones.len(), 1, "a late reader must find one snapshot, not three");
        assert_eq!(held.microphones[0].node_id, 3, "and it must be the newest");
    }

    #[test]
    fn publish_audio_overlays_a_streams_own_props_reading_and_no_other_onto_its_entry() {
        // Identity arrives in `info`, volume in `param`; this is their join point.
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        state.apps.insert(1, sample_stream(1));
        state.app_props = HashMap::from([(1, props_at(0.42, true))]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.apps[0].volume, Some(42.0));
        assert!(published.apps[0].muted);

        state.app_props = HashMap::from([(99, props_at(0.42, true))]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.apps[0].volume, None, "another stream's reading must not leak onto this one");
        assert!(!published.apps[0].muted);
    }

    #[test]
    fn an_app_reports_the_device_its_links_reach() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        state.sinks = HashMap::from([(59, sink_at("alsa_output.analog", None, None))]);
        state.sources = HashMap::from([(60, sink_at("alsa_input.analog", None, None))]);
        state.apps.insert(101, sample_stream(101));
        state.apps.insert(102, AppStream { recording: true, ..sample_stream(102) });
        let link = |output: &str, input: &str| {
            HashMap::from([
                ("link.output.node".to_string(), output.to_string()),
                ("link.input.node".to_string(), input.to_string()),
            ])
        };
        state.graph.add_link(80, &link("101", "59"));
        state.graph.add_link(81, &link("60", "102"));
        // A recording reads from the source, not from the sink a playback link also names.
        state.graph.add_link(82, &link("59", "102"));

        state.publish_audio();
        let targets = |published: &AudioState| published.apps.iter().map(|app| app.target).collect::<Vec<_>>();
        assert_eq!(targets(&rx.try_recv().unwrap()), [Some(59), Some(60)]);

        state.remove_global(80);
        assert_eq!(targets(&rx.try_recv().expect("an unlinked app republishes")), [None, Some(60)]);
    }

    #[test]
    fn publish_audio_reports_sinks_and_sources_with_their_own_active_flags() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        state.sinks = HashMap::from([
            (59, sink_at("alsa_output.analog", Some("Built-in Audio Analog Stereo"), Some(props_at(2.0, false)))),
            (
                70,
                sink_at(
                    "bluez_output.headset",
                    Some("WH-1000XM4"),
                    Some(master::RawSinkProps { mute: false, channel_volumes: vec![], channel_map: vec![] }),
                ),
            ),
        ]);
        state.default_sink_name = Some("bluez_output.headset".to_string());
        state.sources =
            HashMap::from([(60, sink_at("alsa_input.analog", Some("Built-in Microphone"), Some(props_at(0.6, true))))]);
        state.default_source_name = Some("alsa_input.analog".to_string());

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.sinks.iter().map(|sink| sink.active).collect::<Vec<_>>(), [false, true]);
        assert_eq!(published.sinks[1].name, "WH-1000XM4");
        assert_eq!(published.sinks[0].channels.as_ref().unwrap()[0].volume, 200.0);
        assert_eq!(published.sinks[0].volume, Some(200.0), "a sink that is not the default is reported uncorrected");
        assert_eq!(published.sinks[1].channels, None);
        // The source's Props use the sink's cube-root conversion.
        assert_eq!(
            published.sources,
            vec![AudioDevice {
                id: 60,
                node_name: "alsa_input.analog".to_string(),
                name: "Built-in Microphone".to_string(),
                active: true,
                volume: Some(60.0),
                muted: true,
                channels: Some(vec![
                    AudioChannel { index: 0, position: None, volume: 60.0 },
                    AudioChannel { index: 1, position: None, volume: 60.0 },
                ]),
                ..AudioDevice::default()
            }]
        );
    }

    #[test]
    fn a_routed_source_uses_card_props_and_falls_back_to_node_props_before_the_route_arrives() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        let mut source = sink_at("bluez_input.headset", None, Some(props_at(1.0, false)));
        source.props.as_mut().unwrap().channel_map =
            vec![pipewire::spa::sys::SPA_AUDIO_CHANNEL_FL, pipewire::spa::sys::SPA_AUDIO_CHANNEL_FR];
        source.route = Some(DeviceRoute { device_id: 80, profile_device: 0 });
        state.sources.insert(91, source);
        state.default_source_name = Some("bluez_input.headset".to_string());

        state.publish_audio();
        let before_route = rx.try_recv().unwrap();
        assert!(!before_route.sources[0].muted);
        assert_eq!(before_route.sources[0].balance, Some(0.0), "a front pair reads centred");
        assert_eq!(before_route.sources[0].card, Some(80));
        assert_eq!(before_route.sources[0].channels.as_ref().unwrap()[0].position.as_deref(), Some("front_left"));

        state
            .device_routes
            .insert((80, 0), route::ActiveRoute { index: 0, port: None, props: Some(props_at(0.4, true)) });
        state.publish_audio();
        let published = rx.try_recv().unwrap();
        assert!(published.sources[0].muted);
        assert_eq!(published.sources[0].volume, Some(40.0));
        assert_eq!(
            published.sources[0].channels.as_ref().unwrap().iter().map(|channel| channel.volume).collect::<Vec<_>>(),
            [40.0, 40.0]
        );
        assert!(state.device_props(DefaultDevice::Source, 91).unwrap().mute);
    }

    #[test]
    fn publish_audio_of_a_fresh_state_reports_empty_lists_and_deduplicates_identical_state() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        state.publish_audio();
        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert!(published.apps.is_empty());
        assert!(published.sinks.is_empty());
        assert!(published.sources.is_empty());
        state.publish_audio();
        assert!(rx.try_recv().is_err(), "second publish with identical state must be skipped");
    }

    #[test]
    fn removing_a_global_publishes_audio_only_when_a_tracked_entry_went() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = watch::channel(PrivacySources::default());
        let mut state = mixer_state(updates, privacy_updates);
        state.apps.insert(5, sample_stream(5));
        state.publish_audio();
        rx.try_recv().unwrap();
        // Changed behind the publisher's back, so any publish would now differ from `last_sent`.
        state.apps.insert(6, sample_stream(6));
        state.remove_global(99);
        assert!(rx.try_recv().is_err(), "an untracked global (a port or link) must not publish");
        state.remove_global(5);
        assert_eq!(rx.try_recv().unwrap().apps.len(), 1);

        state.device_routes.insert((80, 0), route::ActiveRoute { index: 0, port: None, props: None });
        state.remove_global(80);
        assert!(state.device_routes.is_empty(), "a removed BlueZ device must not leave routes for a reused id");
    }
}

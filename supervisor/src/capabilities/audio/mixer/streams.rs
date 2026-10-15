//! PipeWire stream nodes for `mantle.audio` and `mantle.privacy`: classifying a node by
//! `media.class`, and tracking audio streams, cameras, and microphone and screen captures from
//! their `info` events.

pub use shared::state::audio::AppStream;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use pipewire::keys;
use serde::Serialize;

use super::PropsLookup;

/// `media.class` for playback streams, verified in `pw-dump`.
const STREAM_OUTPUT_AUDIO: &str = "Stream/Output/Audio";

/// `media.class` for camera and portal screencast sources (ADR-0034).
const VIDEO_SOURCE: &str = "Video/Source";

/// `media.class` for a video reader: a camera consumer when linked to a camera source (ADR-0034).
const STREAM_INPUT_VIDEO: &str = "Stream/Input/Video";

/// `media.class` for app audio capture (ADR-0137), distinct from `source_volume`, a device
/// setting that says nothing about whether anything is listening.
const STREAM_INPUT_AUDIO: &str = "Stream/Input/Audio";

/// `media.class` for screen-capture producers (ADR-0137). Most compositors and portals advertise
/// `Stream/Output/Video`; portals advertising `Video/Source` (e.g. `xdg-desktop-portal-hyprland`)
/// are classified through [`is_screencast_source`].
///
/// ponytail: `wf-recorder`, `grim`, and other wlr-screencopy clients never reach PipeWire. Catching
/// them needs compositor-reported screencopy clients, which niri-ipc does not provide.
const STREAM_OUTPUT_VIDEO: &str = "Stream/Output/Video";

/// A capture reading a sink monitor, as cava and every visualiser does; the property beats a name list.
fn is_monitor_capture(props: &impl PropsLookup) -> bool {
    props.get_prop(*keys::STREAM_CAPTURE_SINK) == Some("true")
}

/// Screen-capture producers from portals that advertise `Video/Source` (e.g. `xdg-desktop-portal-hyprland`)
/// rather than `Stream/Output/Video`. A physical webcam is a hardware device (`device.api = "v4l2"`
/// or `media.role = "Camera"`).
fn is_screencast_source(props: &impl PropsLookup) -> bool {
    if props.get_prop(*keys::MEDIA_ROLE) == Some("Camera")
        || props.get_prop(*keys::DEVICE_API).is_some()
        || props.get_prop("api.v4l2.path").is_some()
    {
        return false;
    }
    let matches_screencast = |val: &str| {
        val.starts_with("xdph-")
            || val.starts_with("xdpw-")
            || val.contains("portal")
            || val.contains("screencast")
            || val.contains("screencopy")
    };
    props.get_prop(*keys::NODE_NAME).is_some_and(matches_screencast)
        || props.get_prop(*keys::MEDIA_NAME).is_some_and(matches_screencast)
        || props.get_prop(*keys::APP_NAME).is_some_and(matches_screencast)
        || props.get_prop(*keys::APP_PROCESS_BINARY).is_some_and(matches_screencast)
}

/// Node kind chosen at `global` time and carried into `info`; state-only `info` props can be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeKind {
    Audio,
    Video,
    CameraStream,
    Microphone,
    Screencast,
}

/// Classifies a `global` event by `media.class`; `None` covers sinks, sources, and other nodes.
pub(super) fn classify(props: &impl PropsLookup) -> Option<NodeKind> {
    match props.get_prop(*keys::MEDIA_CLASS) {
        Some(STREAM_OUTPUT_AUDIO) => Some(NodeKind::Audio),
        Some(VIDEO_SOURCE) => Some(if is_screencast_source(props) { NodeKind::Screencast } else { NodeKind::Video }),
        Some(STREAM_INPUT_AUDIO) => Some(NodeKind::Microphone),
        Some(STREAM_INPUT_VIDEO) => Some(NodeKind::CameraStream),
        Some(STREAM_OUTPUT_VIDEO) => Some(NodeKind::Screencast),
        _ => None,
    }
}

/// Parses an audio stream with a valid `application.process.id` and resolves its process name.
/// `None` also covers a node PipeWire has not finished populating (see [`apply_info_event`]), a
/// monitor capture, a peak meter (`stream.monitor`), and a notification sound, which pipewire-pulse
/// maps the `event` role to.
fn parse_stream_props(
    proc_root: &Path,
    node_id: u32,
    props: &impl PropsLookup,
    known: Option<&AppStream>,
) -> Option<AppStream> {
    let recording = match classify(props)? {
        NodeKind::Audio => false,
        NodeKind::Microphone => true,
        _ => return None,
    };
    if is_monitor_capture(props)
        || props.get_prop(*keys::STREAM_MONITOR) == Some("true")
        || props.get_prop(*keys::MEDIA_ROLE) == Some("Notification")
    {
        return None;
    }
    // ponytail: pid-less (portal-owned) streams stay out; `portal.app_id` on their Client is the upgrade.
    let pid = props.get_prop(*keys::APP_PROCESS_ID)?.parse().ok()?;
    let prop = |key: &str| props.get_prop(key).map(str::to_string);
    Some(AppStream {
        id: node_id,
        pid,
        name: prop(*keys::APP_NAME),
        // The node's earlier name stands until it is removed; its `comm` is not reread on every PROPS event.
        process_name: known
            .filter(|app| app.pid == pid)
            .and_then(|app| app.process_name.clone())
            .or_else(|| resolve_process_name(proc_root, pid)),
        binary: prop(*keys::APP_PROCESS_BINARY),
        icon: prop(*keys::APP_ICON_NAME).or_else(|| prop(*keys::MEDIA_ICON_NAME)),
        recording,
        volume: None,
        muted: false,
    })
}

/// Reads `{proc_root}/{pid}/comm`; `None` if the process exited or procfs is unreadable.
///
/// Takes the root rather than hard-coding `/proc`, which is the repository rule for every procfs
/// reader in `capabilities/` -- it is what lets the tests below point at a tempdir. Shares
/// `privacy::video::read_comm`, which was already built that way.
fn resolve_process_name(proc_root: &Path, pid: i32) -> Option<String> {
    crate::capabilities::privacy::video::read_comm(proc_root, pid.try_into().ok()?)
}

/// Applies a bound node's `info`. Gate upsert/remove on `NodeChangeMask::PROPS`: state-only events
/// carry empty props and must not drop a live stream. `global_bind`'s first `info` has PROPS.
/// Returns whether `apps` changed, as do the video and capture variants.
pub(super) fn apply_info_event(
    proc_root: &Path,
    apps: &mut BTreeMap<u32, AppStream>,
    node_id: u32,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
) -> bool {
    if !has_props_change {
        return false;
    }
    let parsed = props.and_then(|props| parse_stream_props(proc_root, node_id, props, apps.get(&node_id)));
    upsert_or_remove(apps, node_id, parsed)
}

/// Stores `parsed` under `node_id`, or removes the entry when it is `None`; whether the map changed.
fn upsert_or_remove<V: PartialEq>(map: &mut BTreeMap<u32, V>, node_id: u32, parsed: Option<V>) -> bool {
    match parsed {
        Some(value) if map.get(&node_id) == Some(&value) => false,
        Some(value) => {
            map.insert(node_id, value);
            true
        }
        None => map.remove(&node_id).is_some(),
    }
}

/// Which `Stream/Input/Video` nodes read a camera (ADR-0034), from registry globals alone. A link
/// from a camera source to a stream makes that stream a camera consumer; a viewer of a portal
/// screencast stays out.
#[derive(Debug, Default)]
pub(super) struct CameraGraph {
    /// Camera source nodes.
    sources: BTreeSet<u32>,
    /// `Link` id -> (output node, input node), kept only for links leaving a camera source.
    links: BTreeMap<u32, (u32, u32)>,
    /// The daemon's `application.process.id`, from the core `info` event.
    pub(super) core_pid: Option<u32>,
}

pub(super) fn prop_u32(props: &impl PropsLookup, key: &str) -> Option<u32> {
    props.get_prop(key)?.parse().ok()
}

impl CameraGraph {
    /// Records a `Video/Source` that is a camera: the device-backed node has `device.id` and
    /// `media.role = "Camera"`, a portal screencast has neither. Whether it was one.
    pub(super) fn add_source(&mut self, id: u32, props: &impl PropsLookup) -> bool {
        let camera = props.get_prop(*keys::MEDIA_ROLE) == Some("Camera") || props.get_prop(*keys::DEVICE_ID).is_some();
        camera && self.sources.insert(id)
    }

    /// Whether the link leaves a camera source, so the consumers changed. A node's global precedes
    /// the links that name it.
    pub(super) fn add_link(&mut self, id: u32, props: &impl PropsLookup) -> bool {
        let (Some(output), Some(input)) =
            (prop_u32(props, *keys::LINK_OUTPUT_NODE), prop_u32(props, *keys::LINK_INPUT_NODE))
        else {
            return false;
        };
        self.sources.contains(&output) && self.links.insert(id, (output, input)) != Some((output, input))
    }

    /// Forgets a removed global of any kind, and the links that name it so a reused id inherits
    /// none; whether the consumers changed.
    pub(super) fn remove(&mut self, id: u32) -> bool {
        let source = self.sources.remove(&id);
        let links = self.links.len();
        self.links.retain(|&link, &mut (output, input)| link != id && output != id && input != id);
        source || self.links.len() != links
    }

    /// Running streams linked to a camera source.
    pub(super) fn consumers(&self, streams: &BTreeMap<u32, CaptureApp>) -> Vec<CaptureApp> {
        running(streams).into_iter().filter(|app| self.links.values().any(|&(_, input)| input == app.node_id)).collect()
    }
}

/// One microphone or screen-capture stream (ADR-0137); the list it is in supplies the kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureApp {
    /// PipeWire registry id, its list's key.
    pub node_id: u32,
    /// `application.process.id`, which portal-created streams may omit because the owner is the
    /// portal. A naming hint, never an identity.
    pub pid: Option<u32>,
    /// `application.name`, if the client set one.
    pub app_name: Option<String>,
    /// Whether PipeWire reports `Running`. `Idle`/`Suspended` means the app holds the device open
    /// without reading, as a browser tab does between calls, and is not capture.
    pub running: bool,
}

/// Parses `props` for `kind`. `pid` is optional: these lists have only PipeWire, so dropping a
/// pid-less node would drop the capture.
fn parse_capture_props(node_id: u32, kind: NodeKind, props: &impl PropsLookup) -> Option<CaptureApp> {
    if classify(props) != Some(kind) {
        return None;
    }
    if kind == NodeKind::Microphone && is_monitor_capture(props) {
        return None;
    }
    Some(CaptureApp {
        node_id,
        pid: prop_u32(props, *keys::APP_PROCESS_ID),
        app_name: props.get_prop(*keys::APP_NAME).or_else(|| props.get_prop(*keys::NODE_NAME)).map(str::to_string),
        // Set by the caller from this `info` event.
        running: false,
    })
}

/// Applies one capture `info`. Props are gated on PROPS because other events carry a non-null
/// empty dict; `pw_node_info::state` is complete on every event, so `running` is not gated. The
/// STATE bit only says whether it moved since the last one.
pub(super) fn apply_capture_info_event(
    apps: &mut BTreeMap<u32, CaptureApp>,
    node_id: u32,
    kind: NodeKind,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
    running: bool,
) -> bool {
    if has_props_change {
        let parsed = props.and_then(|props| parse_capture_props(node_id, kind, props));
        return upsert_or_remove(apps, node_id, parsed.map(|app| CaptureApp { running, ..app }));
    }
    // An unknown node stays unknown: a state event must not resurrect a rejected monitor capture.
    match apps.get_mut(&node_id) {
        Some(app) if app.running != running => {
            app.running = running;
            true
        }
        _ => false,
    }
}

/// Running entries only. Idle streams stay tracked so a later `Running` needs no re-parse.
pub(super) fn running(apps: &BTreeMap<u32, CaptureApp>) -> Vec<CaptureApp> {
    apps.values().filter(|app| app.running).cloned().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn camera_stream_props() -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), "Video/Source".to_string()),
            ("application.name".to_string(), "Firefox".to_string()),
            ("application.process.id".to_string(), "42".to_string()),
            ("node.name".to_string(), "Firefox".to_string()),
        ])
    }

    fn capture_props(media_class: &str) -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), media_class.to_string()),
            ("application.name".to_string(), "Firefox".to_string()),
            ("application.process.id".to_string(), "42".to_string()),
        ])
    }

    #[test]
    fn classify_tells_the_node_kinds_apart_by_media_class() {
        for (media_class, expected) in [
            ("Stream/Output/Audio", NodeKind::Audio),
            ("Video/Source", NodeKind::Video),
            ("Stream/Input/Audio", NodeKind::Microphone),
            ("Stream/Output/Video", NodeKind::Screencast),
            ("Stream/Input/Video", NodeKind::CameraStream),
        ] {
            let props = HashMap::from([("media.class".to_string(), media_class.to_string())]);
            assert_eq!(classify(&props), Some(expected), "{media_class}");
        }
        let props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        assert_eq!(classify(&props), None, "a sink is bound by the device path, not this one");
    }

    #[test]
    fn a_portal_screencast_node_is_classified_and_tracked_with_or_without_client_props() {
        let props = |extra: &[(&str, &str)]| -> HashMap<String, String> {
            let base = [("media.class", "Video/Source")];
            base.iter().chain(extra).map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let hyprland = "xdg-desktop-portal-hyprland";
        // (props, node id, expected pid)
        for (props, node_id, pid) in [
            (
                props(&[
                    ("node.name", "xdph-streaming-1234"),
                    ("application.name", hyprland),
                    ("application.process.binary", hyprland),
                    ("application.process.id", "42"),
                ]),
                1,
                Some(42),
            ),
            (props(&[("media.name", "xdph-streaming-628754"), ("node.name", hyprland)]), 104, None),
        ] {
            assert_eq!(classify(&props), Some(NodeKind::Screencast));
            let mut apps = BTreeMap::new();
            apply_capture_info_event(&mut apps, node_id, NodeKind::Screencast, true, Some(&props), true);
            let expected = CaptureApp { node_id, pid, app_name: Some(hyprland.to_string()), running: true };
            assert_eq!(running(&apps), vec![expected]);
        }
    }

    #[test]
    fn classify_leaves_a_camera_with_camera_role_or_device_api_as_video() {
        let mut props = camera_stream_props();
        props.insert("media.role".to_string(), "Camera".to_string());
        props.insert("node.name".to_string(), "xdph-streaming-1".to_string());
        assert_eq!(classify(&props), Some(NodeKind::Video));

        let mut v4l2_props = camera_stream_props();
        v4l2_props.insert("device.api".to_string(), "v4l2".to_string());
        assert_eq!(classify(&v4l2_props), Some(NodeKind::Video));
    }

    /// A browser tab holds capture open between calls; publishing idle would light privacy forever.
    #[test]
    fn an_idle_capture_stream_is_tracked_but_not_published() {
        let mut apps = BTreeMap::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Microphone,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            false,
        );
        assert!(running(&apps).is_empty(), "an idle stream is not capture");

        let empty: HashMap<String, String> = HashMap::new();
        assert!(apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true));
        assert_eq!(running(&apps).len(), 1, "the same node going Running must publish without a re-parse");
    }

    /// The following state-only event has empty props; re-parsing would drop the entry.
    #[test]
    fn a_running_capture_stream_survives_a_state_only_event() {
        let mut apps = BTreeMap::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Microphone,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            true,
        );
        let empty: HashMap<String, String> = HashMap::new();
        let changed = apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true);

        assert_eq!(running(&apps).len(), 1);
        assert!(!changed, "an unchanged capture must not republish privacy");
    }

    /// A later state event cannot resurrect a node whose props were rejected.
    #[test]
    fn a_state_event_cannot_resurrect_a_node_whose_props_were_rejected() {
        let mut props = capture_props("Stream/Input/Audio");
        props.insert("stream.capture.sink".to_string(), "true".to_string());

        let mut apps = BTreeMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, true, Some(&props), true);
        let empty: HashMap<String, String> = HashMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true);

        assert!(running(&apps).is_empty());
    }

    /// Portal streams may omit pid; dropping one would drop the screencast, unlike the pid-matched
    /// camera path.
    #[test]
    fn a_capture_stream_without_a_pid_is_still_tracked() {
        let props = HashMap::from([("media.class".to_string(), "Stream/Output/Video".to_string())]);

        let mut apps = BTreeMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Screencast, true, Some(&props), true);

        assert_eq!(running(&apps), vec![CaptureApp { node_id: 1, pid: None, app_name: None, running: true }]);
    }

    #[test]
    fn a_node_of_the_wrong_class_for_its_kind_is_dropped() {
        let mut apps = BTreeMap::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Screencast,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            true,
        );

        assert!(running(&apps).is_empty());
    }

    fn zen_browser_stream_props() -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.name".to_string(), "Zen".to_string()),
            ("application.process.id".to_string(), "1538319".to_string()),
            ("application.process.binary".to_string(), "zen-bin".to_string()),
            ("client.id".to_string(), "97".to_string()),
            ("node.name".to_string(), "Zen".to_string()),
        ])
    }

    #[test]
    fn parse_stream_props_builds_an_app_stream_from_a_real_node_a_capture_a_nameless_pid_and_a_comm_file() {
        let proc = tempfile::tempdir().unwrap();
        let parsed = parse_stream_props(proc.path(), 1, &zen_browser_stream_props(), None)
            .expect("should parse as an audio stream");
        assert_eq!(parsed.pid, 1538319);
        assert_eq!(parsed.name, Some("Zen".to_string()));
        assert_eq!(parsed.binary, Some("zen-bin".to_string()));
        assert!(!parsed.recording);

        let parsed = parse_stream_props(proc.path(), 1, &capture_props("Stream/Input/Audio"), None)
            .expect("a call's mic parses");
        assert!(parsed.recording, "an input stream is recording");

        let props = HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.process.id".to_string(), "1234".to_string()),
        ]);
        let parsed = parse_stream_props(proc.path(), 1, &props, None).expect("pid alone is enough to parse");
        assert_eq!(parsed.name, None, "a missing app name is allowed");

        let pid = 1234;
        std::fs::create_dir(proc.path().join("1234")).unwrap();
        std::fs::write(proc.path().join("1234").join("comm"), "mpv\n").unwrap();

        let props = HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.process.id".to_string(), pid.to_string()),
            ("application.name".to_string(), "Test App".to_string()),
        ]);

        let app = parse_stream_props(proc.path(), 42, &props, None).expect("should build an AppStream");
        assert_eq!(app.id, 42);
        assert_eq!(app.pid, pid);
        assert_eq!(app.name, Some("Test App".to_string()));
        assert_eq!(app.process_name, Some("mpv".to_string()));
        // Identity comes from properties; volume lives on a param and joins in publish_audio.
        assert_eq!(app.volume, None);
        assert!(!app.muted);
    }

    #[test]
    fn parse_stream_props_rejects_a_peak_meter_and_a_notification_sound() {
        let proc = tempfile::tempdir().unwrap();
        let mut meter = capture_props("Stream/Input/Audio");
        meter.insert("stream.monitor".to_string(), "true".to_string());
        assert!(parse_stream_props(proc.path(), 1, &meter, None).is_none(), "a level meter is not a mixer row");

        let mut notification = zen_browser_stream_props();
        notification.insert("media.role".to_string(), "Notification".to_string());
        assert!(parse_stream_props(proc.path(), 1, &notification, None).is_none());
    }

    #[test]
    fn parse_stream_props_rejects_a_non_stream_class_and_a_missing_or_unparseable_pid() {
        let proc = tempfile::tempdir().unwrap();
        let props = |class: &str, pid: Option<&str>| -> HashMap<String, String> {
            let mut props = HashMap::from([("media.class".to_string(), class.to_string())]);
            if let Some(pid) = pid {
                props.insert("application.process.id".to_string(), pid.to_string());
            }
            props
        };
        for (case, props) in [
            ("non-stream media class", props("Audio/Sink", Some("1234"))),
            ("missing pid", props("Stream/Output/Audio", None)),
            ("unparseable pid", props("Stream/Output/Audio", Some("not-a-pid"))),
        ] {
            assert!(parse_stream_props(proc.path(), 1, &props, None).is_none(), "{case}");
        }
    }

    #[test]
    fn apply_info_event_keeps_a_tracked_stream_through_a_state_only_info_event() {
        let proc = tempfile::tempdir().unwrap();
        let mut apps = BTreeMap::new();
        // Bind-time global_bind guarantees the first info event carries PROPS and full props.
        apply_info_event(proc.path(), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.len(), 1, "the initial props-bearing info event should track the stream");

        // A state-only transition (e.g. RUNNING -> IDLE) sends empty props, not stream removal.
        let state_only_props: HashMap<String, String> = HashMap::new();
        apply_info_event(proc.path(), &mut apps, 1, false, Some(&state_only_props));

        assert_eq!(apps.len(), 1, "a state-only info event (no PROPS change) must not drop an already-tracked stream");
    }

    #[test]
    fn apply_info_event_upserts_on_a_props_bearing_event() {
        let proc = tempfile::tempdir().unwrap();
        let mut apps = BTreeMap::new();
        apply_info_event(proc.path(), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(
            apps.values().cloned().collect::<Vec<_>>(),
            vec![parse_stream_props(proc.path(), 1, &zen_browser_stream_props(), None).unwrap()]
        );
        assert!(!apply_info_event(proc.path(), &mut apps, 1, true, Some(&zen_browser_stream_props())));
    }

    #[test]
    fn apply_info_event_removes_when_a_props_bearing_event_no_longer_parses() {
        let proc = tempfile::tempdir().unwrap();
        let mut apps = BTreeMap::new();
        apply_info_event(proc.path(), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.len(), 1);

        let non_stream_props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        apply_info_event(proc.path(), &mut apps, 1, true, Some(&non_stream_props));

        assert!(apps.is_empty(), "a PROPS-bearing event that no longer parses as a stream should remove it");
    }

    #[test]
    fn a_second_props_event_reuses_the_tracked_process_name() {
        let proc = tempfile::tempdir().unwrap();
        let props = zen_browser_stream_props();
        let pid = props["application.process.id"].clone();
        std::fs::create_dir(proc.path().join(&pid)).unwrap();
        std::fs::write(proc.path().join(&pid).join("comm"), "first\n").unwrap();
        let mut apps = BTreeMap::new();
        apply_info_event(proc.path(), &mut apps, 1, true, Some(&props));
        std::fs::write(proc.path().join(&pid).join("comm"), "second\n").unwrap();
        apply_info_event(proc.path(), &mut apps, 1, true, Some(&props));
        assert_eq!(apps[&1].process_name.as_deref(), Some("first"));
    }

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
    }

    /// Global props of the real UVC camera node (`pw-cli ls Node`).
    fn camera_source() -> HashMap<String, String> {
        props(&[
            ("media.class", "Video/Source"),
            ("media.role", "Camera"),
            ("device.id", "54"),
            ("object.path", "v4l2:/dev/video0"),
        ])
    }

    fn camera_graph() -> CameraGraph {
        let mut graph = CameraGraph::default();
        assert!(graph.add_source(63, &camera_source()));
        graph
    }

    fn link(output: &str, input: &str) -> HashMap<String, String> {
        props(&[("link.output.node", output), ("link.input.node", input)])
    }

    fn video_stream(node_id: u32, running: bool) -> BTreeMap<u32, CaptureApp> {
        let app = CaptureApp { node_id, pid: None, app_name: Some("ffmpeg".to_string()), running };
        BTreeMap::from([(node_id, app)])
    }

    #[test]
    fn a_stream_linked_to_a_camera_source_is_a_consumer_until_it_stops_or_unlinks() {
        let mut graph = camera_graph();
        assert!(graph.add_link(70, &link("63", "94")));
        let consumers = |graph: &CameraGraph, running| graph.consumers(&video_stream(94, running));
        assert_eq!(consumers(&graph, true).len(), 1);
        assert!(consumers(&graph, false).is_empty(), "an idle stream holds the camera without reading");
        assert!(graph.remove(70));
        assert!(consumers(&graph, true).is_empty());
    }

    #[test]
    fn only_a_device_backed_or_camera_role_source_is_a_camera() {
        let mut graph = CameraGraph::default();
        let portal = props(&[("media.class", "Video/Source"), ("node.name", "unlisted-portal-source")]);
        assert!(!graph.add_source(80, &portal), "a screencast with an unlisted name is not a camera");
        assert!(graph.add_source(81, &props(&[("media.role", "Camera")])));
        assert!(graph.add_source(82, &props(&[("device.id", "54")])));
        assert!(!graph.add_link(71, &link("80", "94")), "a viewer of that source is no camera user");
        assert!(graph.consumers(&video_stream(94, true)).is_empty());
    }

    #[test]
    fn removing_a_node_or_link_id_drops_every_link_naming_it() {
        for (removed, why) in
            [(63, "the camera source"), (94, "the consumer, so a reused id inherits no link"), (70, "the link")]
        {
            let mut graph = camera_graph();
            graph.add_link(70, &link("63", "94"));
            assert!(graph.remove(removed), "{why}");
            assert!(graph.consumers(&video_stream(94, true)).is_empty(), "{why}");
        }
    }

    #[test]
    fn a_pid_prop_parses_once_as_u32() {
        let pid = |value: &str| prop_u32(&props(&[("application.process.id", value)]), "application.process.id");
        assert_eq!(pid("7842"), Some(7842));
        assert_eq!(pid("-1"), None);
        assert_eq!(pid("daemon"), None);
    }
}

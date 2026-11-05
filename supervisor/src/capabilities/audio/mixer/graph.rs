//! The PipeWire link graph: every `Link` global's two ends, and which `Video/Source` nodes are
//! cameras, from registry globals alone. Camera consumers (ADR-0034) and an app's device are
//! queries on it.

use std::collections::{BTreeMap, BTreeSet};

use pipewire::keys;

use super::PropsLookup;
use super::streams::{CaptureApp, prop_u32, running};

/// What a graph change touched: camera consumers feed `mantle.privacy`, any link `apps[].target`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Changed {
    pub(super) camera: bool,
    pub(super) links: bool,
}

#[derive(Debug, Default)]
pub(super) struct LinkGraph {
    /// Camera source nodes.
    cameras: BTreeSet<u32>,
    /// `Link` id -> (output node, input node), for every link.
    links: BTreeMap<u32, (u32, u32)>,
    /// The daemon's `application.process.id`, from the core `info` event.
    pub(super) core_pid: Option<u32>,
}

impl LinkGraph {
    /// Records a `Video/Source` that is a camera: the device-backed node has `device.id` and
    /// `media.role = "Camera"`, a portal screencast has neither. Whether it was one.
    pub(super) fn add_camera(&mut self, id: u32, props: &impl PropsLookup) -> bool {
        let camera = props.get_prop(*keys::MEDIA_ROLE) == Some("Camera") || props.get_prop(*keys::DEVICE_ID).is_some();
        camera && self.cameras.insert(id)
    }

    /// Records a link; what changed, if anything. A node's global precedes the links that name it.
    pub(super) fn add_link(&mut self, id: u32, props: &impl PropsLookup) -> Changed {
        let (Some(output), Some(input)) =
            (prop_u32(props, *keys::LINK_OUTPUT_NODE), prop_u32(props, *keys::LINK_INPUT_NODE))
        else {
            return Changed::default();
        };
        let new = self.links.insert(id, (output, input)) != Some((output, input));
        Changed { camera: new && self.cameras.contains(&output), links: new }
    }

    /// Forgets a removed global of any kind, and the links that name it so a reused id inherits
    /// none; what changed.
    pub(super) fn remove(&mut self, id: u32) -> Changed {
        let mut changed = Changed { camera: self.cameras.contains(&id), links: false };
        self.links.retain(|&link, &mut (output, input)| {
            let keep = link != id && output != id && input != id;
            changed.camera |= !keep && self.cameras.contains(&output);
            changed.links |= !keep;
            keep
        });
        self.cameras.remove(&id);
        changed
    }

    /// The nodes linked to `node`: what it plays into for a playback stream, what it records from
    /// otherwise.
    pub(super) fn linked(&self, node: u32, playback: bool) -> impl Iterator<Item = u32> + '_ {
        self.links.values().filter_map(move |&(output, input)| match playback {
            true => (output == node).then_some(input),
            false => (input == node).then_some(output),
        })
    }

    /// Running streams linked to a camera source.
    pub(super) fn camera_consumers(&self, streams: &BTreeMap<u32, CaptureApp>) -> Vec<CaptureApp> {
        running(streams)
            .into_iter()
            .filter(|app| {
                self.links.values().any(|&(output, input)| input == app.node_id && self.cameras.contains(&output))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

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

    fn camera_graph() -> LinkGraph {
        let mut graph = LinkGraph::default();
        assert!(graph.add_camera(63, &camera_source()));
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
        assert!(graph.add_link(70, &link("63", "94")).camera);
        let consumers = |graph: &LinkGraph, running| graph.camera_consumers(&video_stream(94, running));
        assert_eq!(consumers(&graph, true).len(), 1);
        assert!(consumers(&graph, false).is_empty(), "an idle stream holds the camera without reading");
        assert!(graph.remove(70).camera);
        assert!(consumers(&graph, true).is_empty());
    }

    #[test]
    fn only_a_device_backed_or_camera_role_source_is_a_camera() {
        let mut graph = LinkGraph::default();
        let portal = props(&[("media.class", "Video/Source"), ("node.name", "unlisted-portal-source")]);
        assert!(!graph.add_camera(80, &portal), "a screencast with an unlisted name is not a camera");
        assert!(graph.add_camera(81, &props(&[("media.role", "Camera")])));
        assert!(graph.add_camera(82, &props(&[("device.id", "54")])));
        assert_eq!(
            graph.add_link(71, &link("80", "94")),
            Changed { camera: false, links: true },
            "a viewer of that source is no camera user, but its link is tracked"
        );
        assert!(graph.camera_consumers(&video_stream(94, true)).is_empty());
        assert_eq!(graph.add_link(71, &link("80", "94")), Changed::default(), "a repeat changes nothing");
        assert_eq!(graph.remove(71), Changed { camera: false, links: true });
    }

    #[test]
    fn removing_a_node_or_link_id_drops_every_link_naming_it() {
        for (removed, why) in
            [(63, "the camera source"), (94, "the consumer, so a reused id inherits no link"), (70, "the link")]
        {
            let mut graph = camera_graph();
            graph.add_link(70, &link("63", "94"));
            graph.add_link(71, &link("40", "41"));
            assert_eq!(graph.remove(removed), Changed { camera: true, links: true }, "{why}");
            assert!(graph.camera_consumers(&video_stream(94, true)).is_empty(), "{why}");
            assert_eq!(graph.links.len(), 1, "{why}: the unrelated link stays");
        }
    }

    #[test]
    fn a_stream_is_linked_to_the_device_it_plays_into_or_records_from() {
        let mut graph = LinkGraph::default();
        // Two channels, two links, as `pw-link -l` shows for a stereo stream.
        graph.add_link(80, &link("101", "59"));
        graph.add_link(81, &link("101", "59"));
        graph.add_link(82, &link("60", "102"));
        assert_eq!(graph.linked(101, true).collect::<Vec<_>>(), [59, 59]);
        assert_eq!(graph.linked(102, false).collect::<Vec<_>>(), [60]);
        assert_eq!(graph.linked(102, true).count(), 0, "a recording stream plays into nothing");
    }
}

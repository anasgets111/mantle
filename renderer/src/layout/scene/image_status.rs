use std::collections::{HashMap, HashSet};

use mlua::Lua;
use shared::debug;

use super::{NodeId, ResolvedNode, Scene};
use crate::image::{Loaded, Natural};
use crate::layout::node::{self, LayoutError, PaintStyle};
use crate::layout::paint::DrawnImage;
use crate::lua::signal::{Signal, image_status};

/// What paint last found for each bound image: the source it drew and the outcome.
pub(super) type Loads = HashMap<NodeId, (String, Loaded)>;

impl Scene {
    /// Refuses two shown images bound to one name in one surface instance, naming both.
    pub(crate) fn check_image_status_names(&self, lua: &Lua) -> Result<(), LayoutError> {
        if !image_status::any_registered(lua) {
            return Ok(());
        }
        for (key, tree) in &self.surfaces {
            let (mut seen, mut clash) = (HashMap::new(), None);
            each_bound(tree, true, &mut |node, _, shown| {
                let Some(cell) = node::signal_at(&node.properties, "status").as_ref().and_then(image_status::cell)
                else {
                    return;
                };
                if shown {
                    let id = node::fields::common::id.read(&node.properties).ok().flatten();
                    let id = id.unwrap_or_else(|| "(no id)".to_string());
                    if let Some(first) = seen.insert(cell, id.clone()) {
                        clash.get_or_insert((first, id));
                    }
                }
            });
            if let Some((a, b)) = clash {
                let detail = format!("{key}: images `{a}` and `{b}` share one image_status name");
                return Err(node::invalid("status", detail));
            }
        }
        Ok(())
    }

    /// Writes each name from the first shown image bound to it, by instance id, whose state is
    /// known: empty, or what paint last found for its `source`. With none known, `source` reads
    /// loading, so a source change never leaves the old one's status up while paint skips the node.
    pub(crate) fn publish_image_status(&mut self, lua: &Lua) {
        if !image_status::any_registered(lua) {
            self.image_loads.clear();
            return;
        }
        let (mut bound, mut cells) = (HashSet::new(), HashSet::new());
        let mut chosen: HashMap<_, (Signal, Option<State>)> = HashMap::new();
        let mut surfaces: Vec<_> = self.surfaces.iter().collect();
        surfaces.sort_unstable_by_key(|(key, _)| key.as_str());
        for (_, tree) in surfaces {
            each_bound(tree, true, &mut |node, source, shown| {
                let Some(signal) = node::signal_at(&node.properties, "status") else { return };
                let Some(cell) = image_status::cell(&signal) else { return };
                bound.insert(node.id);
                cells.insert(cell);
                if !shown {
                    return;
                }
                let state = match self.image_loads.get(&node.id) {
                    _ if source.is_empty() => Some(("empty", None)),
                    Some((drawn, Loaded::Ready(size))) if drawn == source => Some(("ready", *size)),
                    Some((drawn, Loaded::Failed)) if drawn == source => Some(("failed", None)),
                    _ => None,
                };
                let first = chosen.entry(cell).or_insert_with(|| (signal.clone(), state));
                if first.1.is_none() && state.is_some() {
                    *first = (signal, state);
                }
            });
        }
        for (signal, state) in chosen.into_values() {
            let (state, size) = state.unwrap_or(("loading", None));
            if let Err(err) = image_status::write(lua, &signal, state, size) {
                debug!("writing an image_status signal failed: {err}");
            }
        }
        self.image_loads.retain(|node, _| bound.contains(node));
        image_status::note_bound(lua, cells);
    }

    /// Records what `instance_id`'s paint found decoded or failed for each bound image still
    /// naming that `source`, and rewrites the names when that changed. Mid-dissolve paint draws the
    /// run's destination; a newer `source` stays loading.
    pub fn note_image_loads(&mut self, instance_id: &str, drawn: &[DrawnImage], lua: &Lua) {
        let Some(tree) = self.surfaces.get(instance_id) else { return };
        let (loads, mut changed) = (&mut self.image_loads, false);
        each_bound(tree, false, &mut |node, source, _| {
            if node.properties.contains_key("status")
                && let Some(found) = drawn.iter().find(|image| image.node == node.id && image.source == source)
            {
                let record = (found.source.clone(), found.loaded);
                changed |= loads.insert(node.id, record.clone()).is_none_or(|old| old != record);
            }
        });
        if changed {
            self.publish_image_status(lua);
        }
    }
}

/// One image's state for its name: the word and the size.
type State = (&'static str, Option<Natural>);

/// Calls `visit` for every `image` under `node` with its `source` and whether it and its
/// ancestors are in flow; only shown ones when `hidden` is false. A hidden or leaving image keeps
/// its record but never speaks for its name.
pub(super) fn each_bound(node: &ResolvedNode, hidden: bool, visit: &mut dyn FnMut(&ResolvedNode, &str, bool)) {
    fn walk(node: &ResolvedNode, shown: bool, hidden: bool, visit: &mut dyn FnMut(&ResolvedNode, &str, bool)) {
        let shown = shown && node.in_flow();
        if !shown && !hidden {
            return;
        }
        if let Some(PaintStyle::Image { source, .. }) = &node.paint {
            visit(node, source, shown);
        }
        for child in &node.children {
            walk(child, shown, hidden, visit);
        }
    }
    walk(node, true, hidden, visit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::scene::tests::{apply_at, full, surface_from};
    use crate::text::shaping::ShapingHandle;

    fn status(lua: &Lua) -> String {
        lua.load(r#"local s = loaded:get(); return s.state .. (s.width and (" " .. s.width .. "x" .. s.height) or "")"#)
            .eval()
            .unwrap()
    }

    #[test]
    fn status_follows_the_current_source_through_loading_ready_failed_and_empty() {
        let (lua, surface) = surface_from(
            r#"loaded = image_status("wall")
            path = state("path", "/w/good.png")
            inline = state("inline", false)
            shown = state("shown", true)
            return panel { id = "bar", child = image { width = 40, height = 20, source = path, visible = shown,
                async = inline:map(function(on) return not on end), retain = true, status = loaded } }"#,
        );
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        let pass = |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let paint = |scene: &mut Scene, source: &str, loaded| {
            let id = scene.surface("bar@TEST").unwrap().children[0].id;
            scene.note_image_loads("bar@TEST", &[DrawnImage { node: id, source: source.into(), loaded }], &lua);
        };
        let good = Loaded::Ready(Some((4000, 3000)));
        assert_eq!(status(&lua), "empty", "before the first pass");
        pass(&mut scene);
        assert_eq!(status(&lua), "loading");
        assert!(!crate::lua::signal::take_layout_changed(&lua).is_empty(), "readers owe a follow-up pass");
        paint(&mut scene, "/w/good.png", good);
        assert_eq!(status(&lua), "ready 4000x3000", "the file's own size");
        pass(&mut scene);
        assert_eq!(status(&lua), "ready 4000x3000", "a pass over the same source does not reload it");
        paint(&mut scene, "/w/good.png", Loaded::Ready(None));
        assert_eq!(status(&lua), "ready", "a size nothing could read is left out");

        lua.load(r#"path:set("/w/missing.png")"#).exec().unwrap();
        pass(&mut scene);
        assert_eq!(status(&lua), "loading", "a changed source, while `retain` still draws the old one");
        paint(&mut scene, "/w/good.png", good);
        assert_eq!(status(&lua), "loading", "the old source's draw says nothing about the new one");
        paint(&mut scene, "/w/missing.png", Loaded::Failed);
        assert_eq!(status(&lua), "failed", "though `retain` keeps drawing the old picture");

        lua.load(r#"shown:set(false); path:set("")"#).exec().unwrap();
        pass(&mut scene);
        assert_eq!(status(&lua), "failed", "a hidden image keeps its last status");
        lua.load("shown:set(true)").exec().unwrap();
        pass(&mut scene);
        assert_eq!(status(&lua), "empty");

        lua.load(r#"inline:set(true); path:set("/w/good.png")"#).exec().unwrap();
        pass(&mut scene);
        assert_eq!(status(&lua), "loading");
        paint(&mut scene, "/w/good.png", good);
        assert_eq!(status(&lua), "ready 4000x3000");
        lua.load(r#"path:set("/w/other.png")"#).exec().unwrap();
        pass(&mut scene);
        assert_eq!(status(&lua), "loading", "a source paint never reaches does not keep the old one's status");
    }

    #[test]
    fn copies_across_outputs_share_a_name_and_two_in_one_instance_clash() {
        let (lua, surface) = surface_from(
            r#"loaded = image_status("wall")
            both = state("both", false)
            return panel { id = "bar", child = function(output)
                local kids = {}
                if output == "RIGHT" then kids[1] = rect { width = 1, height = 1 } end
                kids[#kids + 1] = image { id = "a", width = 4, height = 4, source = "/w/" .. output .. ".png",
                    async = true, status = loaded }
                kids[#kids + 1] = image { id = "b", width = 4, height = 4, source = "/w/b.png", status = loaded,
                    visible = both }
                return column { children = kids }
            end }"#,
        );
        let mut left = crate::layout::scene::tests::instance_at(&surface, full());
        (left.instance_id, left.output) = ("bar@LEFT".into(), "LEFT".into());
        let mut right = left.clone();
        (right.instance_id, right.output) = ("bar@RIGHT".into(), "RIGHT".into());
        let (shaping, mut scene) = (ShapingHandle::spawn(), Scene::new());
        let pass = |scene: &mut Scene| {
            scene.apply(std::slice::from_ref(&surface), &[left.clone(), right.clone()], &shaping, &lua)
        };
        pass(&mut scene).expect("copies on outputs with different siblings");
        assert_eq!(status(&lua), "loading");
        let paint = |scene: &mut Scene, instance: &str, size| {
            let images = &scene.surface(instance).unwrap().children[0].children;
            let node = images.iter().find(|child| matches!(child.paint, Some(PaintStyle::Image { .. }))).unwrap().id;
            let source = format!("/w/{}.png", &instance[4..]);
            let drawn = DrawnImage { node, source, loaded: Loaded::Ready(Some((size, size))) };
            scene.note_image_loads(instance, &[drawn], &lua);
        };
        paint(&mut scene, "bar@RIGHT", 2);
        assert_eq!(status(&lua), "ready 2x2", "LEFT not painted yet: a copy that knows speaks");
        paint(&mut scene, "bar@LEFT", 1);
        assert_eq!(status(&lua), "ready 1x1", "the first instance by id drives once it knows");
        scene.forget("bar@LEFT", &lua);
        assert_eq!(status(&lua), "ready 2x2", "its output gone, the other copy's own state takes over");
        lua.load("both:set(true)").exec().unwrap();
        let err = pass(&mut scene).unwrap_err().to_string();
        assert!(err.contains("images `a` and `b`"), "{err}");
    }
}

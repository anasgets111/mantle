use std::collections::HashSet;

use mlua::Lua;

use super::{ResolvedNode, Scene};
use crate::layout::node::{self, PaintStyle};
use crate::lua::signal::{self, CellId};

impl Scene {
    /// Writes every `elided` and `settled` name from the retained scene.
    pub(crate) fn publish_marks(&self, lua: &Lua) {
        if !signal::marks::any_registered(lua) {
            return;
        }
        let (mut elided, mut settled) = (HashSet::new(), HashSet::new());
        for tree in self.surfaces.values() {
            collect(tree, &mut elided, &mut settled);
        }
        signal::marks::publish(lua, &elided, &settled);
    }
}

/// A hidden or leaving node is skipped with its subtree: it adds no mark, and a frozen tween asks
/// for no frames, so it reads settled.
fn collect(node: &ResolvedNode, elided: &mut HashSet<CellId>, settled: &mut HashSet<CellId>) {
    if !node.in_flow() {
        return;
    }
    if let Some(PaintStyle::Text { elided: true, .. }) = &node.paint
        && let Some(id) = node::signal_at(&node.properties, "elided").and_then(|s| s.elided_id())
    {
        elided.insert(id);
    }
    if node.tweening()
        && let Some(id) = node::signal_at(&node.properties, "settled").and_then(|s| s.settled_id())
    {
        settled.insert(id);
    }
    for child in &node.children {
        collect(child, elided, settled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::scene::tests::{apply_at, full, instance_at, surface_from};
    use crate::text::shaping::ShapingHandle;

    #[test]
    fn shared_elided_names_follow_committed_visible_bindings_across_outputs() {
        let (lua, surface) = surface_from(
            r#"cut = elided("body")
            alias = elided("body")
            narrow = state("narrow", 40)
            shown = state("shown", true)
            height = state("height", 10)
            return panel { id = "bar", child = function(output)
                return text { content = "A long label", visible = shown,
                    width = output == "LEFT" and narrow or 500,
                    height = output == "RIGHT" and height or 10, elide = "end", elided = cut }
            end }"#,
        );
        let mut left = instance_at(&surface, full());
        left.instance_id = "bar@LEFT".into();
        left.output = "LEFT".into();
        let mut right = left.clone();
        right.instance_id = "bar@RIGHT".into();
        right.output = "RIGHT".into();
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        let cut = || lua.load("return cut:get()").eval::<bool>().unwrap();
        scene.apply(std::slice::from_ref(&surface), &[left.clone(), right.clone()], &shaping, &lua).unwrap();
        assert!(cut(), "the wide output cannot overwrite the narrow output's result");
        assert!(lua.load("return alias:get()").eval::<bool>().unwrap(), "the name shares a cell");
        scene.apply(std::slice::from_ref(&surface), &[right.clone()], &shaping, &lua).unwrap();
        assert!(cut(), "a narrowed pass keeps the other output's measurement");
        lua.load("narrow:set(500); height:set(-1)").exec().unwrap();
        let err = scene.apply(std::slice::from_ref(&surface), &[left.clone(), right.clone()], &shaping, &lua);
        assert!(err.unwrap_err().to_string().contains("height"));
        assert!(cut(), "LEFT's successful fit rolled back with RIGHT");
        lua.load("height:set(10)").exec().unwrap();
        scene.apply(std::slice::from_ref(&surface), &[left.clone(), right.clone()], &shaping, &lua).unwrap();
        assert!(!cut(), "the successful retry commits the new measurement");
        lua.load("narrow:set(40); shown:set(false)").exec().unwrap();
        scene.apply(std::slice::from_ref(&surface), &[left.clone(), right.clone()], &shaping, &lua).unwrap();
        assert!(!cut(), "hidden bindings do not leave a stale result");
        lua.load("shown:set(true)").exec().unwrap();
        scene.apply(std::slice::from_ref(&surface), &[left.clone(), right], &shaping, &lua).unwrap();
        assert!(cut());
        scene.forget(&left.instance_id, &lua);
        assert!(!cut(), "only the wide binding remains");
    }

    #[test]
    fn removing_a_list_item_clears_its_elision_and_other_signal_kinds_stay_unwritten() {
        let (lua, surface) = surface_from(
            r#"cut = elided("item")
            items = state("items", { 1 })
            other = state("other", "unchanged")
            return panel { id = "bar", child = column { children = {
                text { width = 20, content = "Long label", elide = "end", elided = other },
                list { items = items, each = function()
                    return text { width = 20, content = "Long label", elide = "end", elided = cut }
                end },
            } } }"#,
        );
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(lua.load("return cut:get()").eval::<bool>().unwrap());
        lua.load("items:set({})").exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(!lua.load("return cut:get()").eval::<bool>().unwrap());
        assert_eq!(lua.load("return other:get()").eval::<String>().unwrap(), "unchanged");
    }
}

#[cfg(test)]
mod settled_tests {
    use std::time::Duration;

    use crate::layout::scene::tests::{applied, apply_at, full, instance_at, surface_from};
    use crate::layout::scene::*;
    use crate::text::shaping::ShapingHandle;

    fn settled(lua: &mlua::Lua, name: &str) -> bool {
        lua.load(format!(r#"return settled("{name}"):get()"#)).eval().unwrap()
    }

    fn started(scene: &Scene) -> std::time::Instant {
        scene.surface("bar@TEST").unwrap().children[0].tweens[0].started
    }

    const WIDTH: &str = r#"panel { id = "bar", child = rect { width = state("w", 40), height = 20,
        settled = settled("s"), animate = { width = { duration = 100, easing = "linear" } } } }"#;

    /// True at rest, false from the pass that starts a tween (and through a retarget, which writes
    /// nothing), true on the tick that ends it, owing readers one pass. Idle frames write nothing.
    #[test]
    fn a_tweens_end_flips_settled_once_and_nothing_else_writes() {
        let (mut scene, lua, shaping, surface) = applied(WIDTH);
        let instances = [instance_at(&surface, full())];
        let edges = || crate::lua::signal::take_layout_changed(&lua).len();
        assert!(settled(&lua, "s"), "nothing has moved yet");

        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(!settled(&lua, "s"), "the pass that starts a tween clears it");
        assert_eq!(edges(), 1);
        let first = started(&scene);
        scene.tick(&instances, &shaping, &lua, first + Duration::from_millis(50));
        assert!(!settled(&lua, "s") && edges() == 0, "a frame mid-run writes nothing");

        lua.load(r#"state("w", 40):set(60)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(!settled(&lua, "s") && edges() == 0, "a retarget keeps it false");
        let began = started(&scene);
        scene.tick(&instances, &shaping, &lua, began + Duration::from_millis(100));
        assert!(settled(&lua, "s"));
        assert_eq!(edges(), 1, "the edge owes its readers a pass");
        scene.tick(&instances, &shaping, &lua, began + Duration::from_millis(200));
        assert_eq!(edges(), 0, "idle frames write nothing");
    }

    /// A keyframe run is one motion, a hold included; an endless one never ends. A hidden subtree
    /// is frozen and asks for no frames, so it reads settled like `animating` does.
    #[test]
    fn a_keyframe_run_settles_at_its_end_a_loop_never_and_a_hidden_node_always() {
        let (mut scene, lua, shaping, surface) = applied(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10, opacity = 1, settled = settled("s"), animate = {
                    opacity = { duration = 100, easing = "linear", keyframes = { 1, { value = 1, duration = 3600 }, 0 } } } },
                rect { width = 10, height = 10, opacity = 1, settled = settled("spin"), animate = {
                    opacity = { duration = 100, loops = "infinite", keyframes = { 1, 0 } } } },
                row { visible = state("open", true), children = { rect { width = state("w", 40), height = 10,
                    settled = settled("hid"), animate = { width = 100 } } } } } } }"#,
        );
        let instances = [instance_at(&surface, full())];
        let began = scene.surface("bar@TEST").unwrap().children[0].children[0].tweens[0].started;
        let at = |ms| began + Duration::from_millis(ms);
        assert!(!settled(&lua, "s") && !settled(&lua, "spin") && settled(&lua, "hid"));

        scene.tick(&instances, &shaping, &lua, at(1000));
        assert!(!settled(&lua, "s"), "holding still inside a run is not settled");
        scene.tick(&instances, &shaping, &lua, at(3700));
        assert!(settled(&lua, "s"), "the run's last frame");
        scene.tick(&instances, &shaping, &lua, at(3_600_000));
        assert!(!settled(&lua, "spin"), "a loop has no end");

        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(!settled(&lua, "hid"));
        lua.load(r#"state("open", true):set(false)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(settled(&lua, "hid"));
    }

    /// A `move` is the node's own motion, and a name two nodes share reads false until neither
    /// moves, also when one of them is removed mid-run.
    #[test]
    fn a_move_counts_and_a_shared_name_outlives_a_removed_node() {
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local move = { move = { duration = 100, easing = "linear" } }
               local a = rect { id = "a", width = 10, height = 10 }
               local b = rect { id = "b", width = 10, height = 10, settled = settled("m"), animate = move }
               local c = rect { id = "c", width = 10, height = 10, settled = settled("m"), animate = move }
               return panel { id = "bar", child = row { width = 100, height = 10,
                   children = state("kids", { a, b, c }) } }"##,
        );
        let mut scene = Scene::new();
        let instances = [instance_at(&surface, full())];
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"local k = state("kids", {}):get(); state("kids", {}):set({ k[2], k[3] })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(!settled(&lua, "m"), "both siblings slid left");
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let began = row.children[0].movement.as_ref().unwrap().started;

        lua.load(r#"local k = state("kids", {}):get(); state("kids", {}):set({ k[1] })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(!settled(&lua, "m"), "the node that stays is still moving");
        scene.tick(&instances, &shaping, &lua, began + Duration::from_millis(500));
        assert!(settled(&lua, "m"));
    }
}

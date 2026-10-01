use std::collections::HashSet;

use mlua::Lua;

use super::{ResolvedNode, Scene};
use crate::layout::node::{self, PaintStyle};
use crate::lua::signal::{self, CellId};

impl Scene {
    pub(crate) fn publish_elision(&self, lua: &Lua) {
        if !signal::elision::any_registered(lua) {
            return;
        }
        let mut truncated = HashSet::new();
        for tree in self.surfaces.values() {
            collect(tree, &mut truncated);
        }
        signal::elision::publish(lua, &truncated);
    }
}

fn collect(node: &ResolvedNode, truncated: &mut HashSet<CellId>) {
    if !node.in_flow() {
        return;
    }
    if let Some(PaintStyle::Text { elided: true, .. }) = &node.paint
        && let Some(id) = node::signal_at(&node.properties, "elided").and_then(|s| s.elided_id())
    {
        truncated.insert(id);
    }
    for child in &node.children {
        collect(child, truncated);
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
                    height = output == "RIGHT" and height or 10, elide = "End", elided = cut }
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
        assert!(scene.apply(std::slice::from_ref(&surface), &[left.clone(), right.clone()], &shaping, &lua).is_err());
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
                text { width = 20, content = "Long label", elide = "End", elided = other },
                list { source = items, itemfn = function()
                    return text { width = 20, content = "Long label", elide = "End", elided = cut }
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

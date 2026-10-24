//! Child selection, the `children` table cache, and reconciliation by `id` then position.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mlua::{Lua, Value};

use super::ResolvedNode;
use crate::layout::node::{self, LayoutError, PropMap, fields};
use crate::lua::location::Site;
use crate::lua::nodes::VirtualNode;

/// Selects `child`, `children`, generated list children, or no children. Surface roles share one
/// `child`; `textfield` is a leaf. Its callbacks and secure-submit fields remain in
/// `ResolvedNode.properties`; the keyboard path reads the latter from the scene while the secret
/// buffer stays on `App` (ADR-0005).
fn children_of(kind: &str, properties: &PropMap) -> Result<Vec<VirtualNode>, LayoutError> {
    match kind {
        "panel" | "lock" => Ok(fields::root::child.read(properties)?.into_iter().collect()),
        "window" | "popup" => Ok(fields::toplevel::child.read(properties)?.into_iter().collect()),
        "rect" => fields::stack::children.read(properties),
        "row" | "column" => fields::flow::children.read(properties),
        "list" => unreachable!("`prepare` builds a list's items through `node::list_children`"),
        "text" | "icon" | "image" | "capture" | "shader" | "path" | "textfield" => Ok(Vec::new()),
        other => unreachable!("ensure_supported_kind already rejected `{other}`"),
    }
}

/// The nodes a node last read out of its `children` or `child` table. Read again only
/// once the node holds a different table: a signal's new value, a function `child`'s new call or a
/// rebuilt `list` item. Blind to a table changed in place, the contract `docs/nodes/index.md` states.
#[derive(Clone)]
pub struct ChildTable {
    /// Kept to keep it alive: compared by address, so a collected table cannot hand its address to
    /// a new one.
    table: mlua::Table,
    /// Checked first: a table of a dropped VM panics on any read, and its address can come back in
    /// the next VM. A scene outlives its VM only in tests.
    lua: mlua::WeakLua,
    /// Shared, so the rollback copy each pass takes of the retained tree does not copy them.
    nodes: Rc<[VirtualNode]>,
}

/// By hand: `WeakLua` has no `Debug`.
impl std::fmt::Debug for ChildTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildTable").field("nodes", &self.nodes.len()).finish_non_exhaustive()
    }
}

/// [`children_of`], or the nodes `kept` read from the same table.
pub(super) fn children_kept(
    kind: &str,
    properties: &PropMap,
    kept: &mut Option<ChildTable>,
    lua: &Lua,
) -> Result<Vec<VirtualNode>, LayoutError> {
    // A kind accepts one of the two keys, so at most one is present.
    let Some(Value::Table(table)) = properties.get("children").or_else(|| properties.get("child")) else {
        return children_of(kind, properties);
    };
    let lua = lua.weak();
    if let Some(kept) = kept.as_ref().filter(|kept| kept.lua == lua && kept.table.to_pointer() == table.to_pointer()) {
        return Ok(kept.nodes.to_vec());
    }
    let nodes = children_of(kind, properties)?;
    *kept = Some(ChildTable { table: table.clone(), lua, nodes: nodes.as_slice().into() });
    Ok(nodes)
}

/// `child = function(output)` on a `panel`/`lock` (ADR-0121) runs per instance, with the output
/// name, before the ordinary child walk. It is part of its root's resolve, so it runs again only
/// when the root resolves again (ADR-0270): a signal it or the root's properties read was written,
/// or a reload. Repeat calls preserve registry-stable state such as `state("wallpaper_" .. output)`.
/// `window`/`popup` and an `output = "active"` panel (ADR-0246) have no output name, so function
/// children are refused rather than called with `""`.
pub(super) fn build_child_for_output(
    mut properties: PropMap,
    kind: &str,
    output: &str,
) -> Result<PropMap, LayoutError> {
    let Some(Value::Function(builder)) = properties.get("child") else {
        return Ok(properties);
    };
    if !matches!(kind, "panel" | "lock") || output.is_empty() {
        return Err(node::invalid(
            "child",
            format!(
                "a function child is for a `panel` or `lock`, which have one instance per output to hand it; \
                 this `{kind}` has one instance wherever the compositor places it"
            ),
        ));
    }
    let built = builder.call::<Value>(output).map_err(|e| node::invalid("child", crate::lua::describe(&e)))?;
    match built {
        Value::Table(_) => {
            properties.insert("child", built);
        }
        // A nil result maps this output empty, like an absent `child`.
        Value::Nil => {
            properties.remove("child");
        }
        other => {
            return Err(node::invalid(
                "child",
                format!("expected the function to return a node table, got {}", node::preview_for_error(&other)),
            ));
        }
    }
    Ok(properties)
}

/// Pairs children by identity (ADR-0045 decisions 1-2): an `id` matches only the same `id`, while
/// id-less children match positionally among other id-less children. An id miss is new, never a
/// positional fallback, so it cannot inherit an unrelated `NodeId`/subtree. Unclaimed retained
/// nodes come back second, for `prepare` to see off. The linear match uses one `HashMap<&str,
/// usize>` per parent; sibling count is unbounded (`1..10000` is legal Lua), and this runs on the
/// Wayland dispatch thread at capability-push cadence (ADR-0044 decision 2).
pub(super) fn pair_children_by_id_then_position(
    fresh_children: &[VirtualNode],
    old_children: Vec<ResolvedNode>,
) -> Result<(Vec<Option<ResolvedNode>>, Vec<ResolvedNode>), LayoutError> {
    if !fresh_children.iter().any(|c| c.properties.contains_key("id"))
        && !old_children.iter().any(|c| c.properties.contains_key("id"))
    {
        let mut old_iter = old_children.into_iter();
        let matched = (0..fresh_children.len()).map(|_| old_iter.next()).collect();
        return Ok((matched, old_iter.collect()));
    }

    // Not `collect`: a `Result` collect drops the size hint, and a list is as long as its data.
    let mut fresh_ids: Vec<Option<String>> = Vec::with_capacity(fresh_children.len());
    for child in fresh_children {
        fresh_ids.push(node::fields::common::id.read(&child.properties)?);
    }

    // Decision 1: reject duplicate sibling ids before matching `old_children`.
    let mut seen: HashSet<&str> = HashSet::with_capacity(fresh_ids.len());
    for id in fresh_ids.iter().flatten() {
        if !seen.insert(id.as_str()) {
            return Err(node::invalid("id", format!("duplicate id `{id}` among siblings")));
        }
    }

    // Retained ids were already validated and cannot hold signals, so `.ok().flatten()` safely
    // treats absent and validated-no-id alike.
    let old_ids: Vec<Option<String>> =
        old_children.iter().map(|c| node::fields::common::id.read(&c.properties).ok().flatten()).collect();
    let mut old_slots: Vec<Option<ResolvedNode>> = old_children.into_iter().map(Some).collect();

    let mut retained_by_id: HashMap<&str, usize> = HashMap::with_capacity(old_ids.len());
    for (index, id) in old_ids.iter().enumerate() {
        if let Some(id) = id {
            retained_by_id.insert(id.as_str(), index);
        }
    }

    // Identified subsequence: a miss stays `None`, not a positional slot.
    let mut matched: Vec<Option<ResolvedNode>> = Vec::with_capacity(fresh_children.len());
    for fresh_id in &fresh_ids {
        let claimed = fresh_id
            .as_deref()
            .and_then(|id| retained_by_id.get(id).copied())
            .and_then(|index| old_slots[index].take());
        matched.push(claimed);
    }

    // Id-less subsequences zip in order (ADR-0023); identified retained children are excluded.
    let mut unidentified_old = old_ids.iter().enumerate().filter(|(_, id)| id.is_none()).map(|(index, _)| index);
    for (slot, fresh_id) in matched.iter_mut().zip(&fresh_ids) {
        if fresh_id.is_none()
            && let Some(index) = unidentified_old.next()
        {
            *slot = old_slots[index].take();
        }
    }

    Ok((matched, old_slots.into_iter().flatten().collect()))
}

/// The (old, new) build sites when an id-less, visible `old` with an `animate.exit` is paired by
/// position with a node of its kind built on another line, so the exit never plays. A keyed
/// `list` row carries its key as `id` and is excluded with the rest.
///
/// ponytail: the site is the constructor's call line, so two calls of one helper share it and a
/// swap between them goes unflagged. Upgrade: record the first config frame outside helpers, or
/// compare props.
pub(super) fn replaced_in_place(
    old: &ResolvedNode,
    kind: &str,
    fresh: &PropMap,
    site: Option<Site>,
) -> Option<(Site, Site)> {
    let (old_site, site) = (old.site?, site?);
    let exits = || {
        let animate = fields::common::animate.read(&old.properties).ok().flatten()?;
        animate.get::<Value>("exit").ok().filter(|exit| !exit.is_nil())
    };
    (old_site != site
        && old.kind == kind
        && old.visible
        && !old.leaving
        && !(old.properties.contains_key("id") || fresh.contains_key("id"))
        && exits().is_some())
    .then_some((old_site, site))
}

pub(super) fn swap_message((old, new): (Site, Site)) -> String {
    format!("{new} replaced {old} in place, so its animate.exit did not run; give the siblings distinct ids")
}

/// The previous child at this position, or `None` when its kind changed and it leaves instead.
/// Split from `pass::prepare` to keep its frame small.
pub(super) fn reusable_for(
    candidate: &mut Option<ResolvedNode>,
    kind: &str,
    unclaimed: &mut Vec<ResolvedNode>,
) -> Option<ResolvedNode> {
    match candidate.take() {
        Some(candidate) if candidate.kind != kind => {
            unclaimed.push(candidate);
            None
        }
        candidate => candidate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::scene_lua;
    use crate::layout::scene::tests::{applied, apply_at, apply_on_two_outputs, full, surface_from};
    use crate::layout::scene::*;
    use crate::lua::nodes::deserialize_lua_table;

    /// A `children` table is read once per table: an edit in place is not seen, a new table is.
    #[test]
    fn a_children_table_is_read_again_only_once_the_node_holds_a_new_one() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"first = { rect { width = 4, height = 4 } }
            kids = state("kids", first)
            return panel { id = "bar", child = row { children = kids } }"#,
        );
        let count = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].children.len();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();

        lua.load("first[2] = rect { width = 4, height = 4 }").exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(count(&scene), 1, "the same table keeps what it read");

        lua.load("kids:set({ first[1], first[2], rect {} })").exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(count(&scene), 3, "a new table is read");
    }

    #[test]
    fn a_function_child_is_built_once_per_output_with_that_outputs_name() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel {
                    id = "wall",
                    child = function(output)
                        return rect { width = output == "LEFT" and 10 or 20, height = 5 }
                    end,
                }"#,
        );

        apply_on_two_outputs(&mut scene, &surface, &shaping, &lua).unwrap();

        assert_eq!(scene.surface("wall@LEFT").unwrap().children[0].rect.width, 10.0);
        assert_eq!(scene.surface("wall@RIGHT").unwrap().children[0].rect.width, 20.0);
    }

    #[test]
    fn a_function_child_returning_nil_maps_the_instance_empty() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel {
                    id = "wall",
                    child = function(output)
                        if output == "LEFT" then return rect { width = 10, height = 5 } end
                    end,
                }"#,
        );

        apply_on_two_outputs(&mut scene, &surface, &shaping, &lua).unwrap();

        assert_eq!(scene.surface("wall@LEFT").unwrap().children.len(), 1);
        assert!(scene.surface("wall@RIGHT").unwrap().children.is_empty());
    }

    #[test]
    fn a_function_child_on_a_window_is_refused_and_a_non_node_return_names_child() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = scene_lua();
        let table: mlua::Table =
            lua.load(r#"return window { id = "w", child = function() return rect {} end }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err().to_string();
        assert!(err.contains("child") && err.contains("window"), "{err}");

        let table: mlua::Table =
            lua.load(r#"return panel { id = "p", child = function() return 4 end }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err().to_string();
        assert!(err.contains("child") && err.contains("node table"), "{err}");
    }

    const EXIT: &str = "animate = { exit = { duration = 100, opacity = 0 } }";

    /// Builds `kids = { sheet() }` (`sheet` is its props), runs `prelude`, then swaps in `other()`,
    /// a global constructor of `bubble` props, over a second pass.
    fn swapped(sheet: &str, bubble: &str, prelude: &str, other: &str) -> Scene {
        let (mut scene, lua, shaping, surface) = applied(&format!(
            r#"function sheet() return rect {{ width = 10, height = 10, {sheet} }} end
               function bubble() return rect {{ width = 20, height = 10, {bubble} }} end
               return panel {{ id = "bar", child = row {{ children = state("kids", {{ sheet() }}) }} }}"#
        ));
        let surfaces = std::slice::from_ref(&surface);
        lua.load(prelude).exec().unwrap();
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        lua.load(format!(r#"state("kids", {{}}):set({{ {other}() }})"#)).exec().unwrap();
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        scene
    }

    #[test]
    fn an_id_less_child_replaced_by_another_site_with_an_exit_is_flagged_once() {
        let scene = swapped(EXIT, "", "", "bubble");
        assert_eq!(scene.swap_warnings.len(), 1, "a repeated pass adds nothing");
        let pair = *scene.swap_warnings.iter().next().unwrap();
        assert_eq!(
            swap_message(pair),
            "shell.lua:2 replaced shell.lua:1 in place, so its animate.exit did not run; give the siblings distinct ids"
        );
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(row.children.iter().all(|c| !c.leaving), "the sheet's node was reused, not left");
    }

    #[test]
    fn a_swap_that_loses_no_exit_is_not_flagged() {
        let quiet = |scene: Scene| assert!(scene.swap_warnings.is_empty());
        quiet(swapped(EXIT, "", "", "sheet")); // same line, same node again
        quiet(swapped("", "", "", "bubble")); // nothing to skip
        let hidden = format!(r#"visible = state("shown", true), {EXIT}"#);
        quiet(swapped(&hidden, "", r#"state("shown", true):set(false)"#, "bubble"));
        let scene = swapped(&format!(r#"id = "a", {EXIT}"#), r#"id = "b""#, "", "bubble");
        assert!(scene.swap_warnings.is_empty());
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(row.children.iter().any(|c| c.leaving), "distinct ids let the exit run");
    }

    #[test]
    fn a_keyed_list_row_is_not_flagged() {
        let (mut scene, lua, shaping, surface) = applied(&format!(
            r#"function row_for(n) return rect {{ width = 10, height = 10, {EXIT} }} end
               return panel {{ id = "bar", child = row {{ children = {{ list {{
                   items = state("items", {{ "a" }}), key = function(n) return n end, each = row_for }} }} }} }}"#
        ));
        lua.load(r#"state("items", {}):set({ "b" })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(scene.swap_warnings.is_empty());
    }

    #[test]
    fn a_kind_change_at_the_same_index_allocates_a_new_identity() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let old_id = scene.surface("bar@TEST").unwrap().children[0].id;

        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = text { content = "hi" } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].kind, "text");
        assert_ne!(scene.surface("bar@TEST").unwrap().children[0].id, old_id);
    }

    #[test]
    fn an_identified_child_keeps_its_node_id_across_applies_when_a_sibling_is_inserted_above_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "keep", width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let keep_id_before = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 5, height = 5 },
                rect { id = "keep", width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0];

        assert_eq!(
            row.children[1].id, keep_id_before,
            "the id-matched sibling must keep its NodeId despite the insertion above it"
        );
    }

    #[test]
    fn an_unidentified_child_list_keeps_node_ids_across_applies_by_position_only() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let original_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 5, height = 5 },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0];

        assert_eq!(
            row.children[0].id, original_id,
            "position 0 still reuses the retained identity, since matching stays positional"
        );
        assert_ne!(
            row.children[1].id, original_id,
            "position 1 is a fresh allocation, not a reused identity -- matching today's rule with no ids present"
        );
    }

    #[test]
    fn a_mixed_child_list_keeps_identified_node_ids_across_applies_and_the_rest_only_by_position() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { id = "anchor", width = 2, height = 2 },
                rect { width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (b_id_before, anchor_id_before, c_id_before) = {
            let root = scene.surfaces.get("bar@TEST").unwrap();
            let row = &root.children[0].children;
            (row[0].id, row[1].id, row[2].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { id = "anchor", width = 2, height = 2 },
                rect { width = 9, height = 9 },
                rect { width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let root = scene.surfaces.get("bar@TEST").unwrap();
        let row2 = &root.children[0].children;

        assert_eq!(row2.len(), 4);
        assert_eq!(row2[0].id, b_id_before, "B sits ahead of the insertion, so it keeps its slot either way");
        assert_eq!(row2[1].id, anchor_id_before, "the identified sibling keeps its id regardless of position");
        assert_ne!(
            row2[3].id, c_id_before,
            "C is unidentified, so the inserted node claims its retained slot positionally -- same rule an id-less list already had"
        );
    }

    #[test]
    fn duplicate_sibling_ids_are_rejected_as_a_layout_error() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "dup", width = 1, height = 1 },
                rect { id = "dup", width = 2, height = 2 },
            } } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "id" && detail.contains("dup")),
            "duplicate sibling ids must be rejected, naming the offending id: {err:?}"
        );
    }

    #[test]
    fn the_same_id_under_two_different_parents_does_not_collide() {
        let (scene, _lua, _shaping, _surface) = applied(
            r#"panel { id = "bar", child = row { children = {
                column { children = { rect { id = "inner", width = 1, height = 1 } } },
                column { children = { rect { id = "inner", width = 2, height = 2 } } },
            } } }"#,
        );
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(
            row.children.len(),
            2,
            "two columns, each with its own 'inner' child, must not collide across parents"
        );
        assert_eq!(row.children[0].children[0].rect.width, 1.0);
        assert_eq!(row.children[1].children[0].rect.width, 2.0);
    }

    #[test]
    fn a_signal_valued_id_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = scene_lua();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("x").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("sig", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { id = sig, width = 1, height = 1 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::UnsupportedSignalProperty(p) if p == "id"),
            "a Signal-valued id must be rejected outright, not resolved: {err:?}"
        );
    }

    #[test]
    fn a_fresh_id_gets_a_new_node_id_and_the_vanished_id_is_removed() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "a", width = 1, height = 1 },
                rect { id = "b", width = 2, height = 2 },
                rect { id = "c", width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (a_id, b_id, c_id) = {
            let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (row[0].id, row[1].id, row[2].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "b", width = 2, height = 2 },
                rect { id = "c", width = 3, height = 3 },
                rect { id = "d", width = 4, height = 4 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(row[0].id, b_id, "b keeps its identity across the apply");
        assert_eq!(row[1].id, c_id, "c keeps its identity across the apply");
        assert!(
            row[2].id != a_id && row[2].id != b_id && row[2].id != c_id,
            "d declared a new id, so it must get a fresh NodeId rather than adopt a retained one"
        );
        assert!(row.iter().all(|child| child.id != a_id));
    }

    #[test]
    fn an_anonymous_fresh_child_never_inherits_an_identified_retained_node() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "x", width = 1, height = 1 },
                rect { width = 2, height = 2 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (x_id, anon_id) = {
            let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (row[0].id, row[1].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { width = 2, height = 2 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(
            row[0].id, anon_id,
            "the unidentified subsequence pairs among itself, so the retained anonymous node lands in slot 0"
        );
        assert!(row[1].id != x_id && row[1].id != anon_id, "slot 1 has no unidentified counterpart left, so it is new");
        assert!(row.iter().all(|child| child.id != x_id));
    }

    #[test]
    fn a_vanished_id_removes_its_subtree_even_when_fresh_slots_are_unmatched() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "gone", width = 1, height = 1, children = { rect { width = 1, height = 1 } } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (outer_id, inner_id) = {
            let outer = &scene.surfaces.get("bar@TEST").unwrap().children[0].children[0];
            (outer.id, outer.children[0].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "other", width = 1, height = 1 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_ne!(row[0].id, outer_id, "`other` must not adopt `gone`'s retained node");
        assert_ne!(row[0].id, inner_id);
        assert!(row[0].children.is_empty());
        assert_eq!(scene.census().1, 3);
    }

    #[test]
    fn many_identified_siblings_reconcile_in_reversed_order_without_a_quadratic_scan() {
        const N: usize = 2000;
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();

        let mut v1_children = String::new();
        for i in 0..N {
            v1_children.push_str(&format!("rect {{ id = \"n{i}\", width = 1, height = 1 }},\n"));
        }
        let (_lua1, surface_v1) =
            surface_from(&format!("panel {{ id = \"bar\", child = row {{ children = {{ {v1_children} }} }} }}"));
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let before: Vec<NodeId> =
            scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        let mut v2_children = String::new();
        v2_children.push_str("rect { id = \"fresh\", width = 1, height = 1 },\n");
        for i in (1..N).rev() {
            v2_children.push_str(&format!("rect {{ id = \"n{i}\", width = 1, height = 1 }},\n"));
        }
        let (_lua2, surface_v2) =
            surface_from(&format!("panel {{ id = \"bar\", child = row {{ children = {{ {v2_children} }} }} }}"));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let after: Vec<NodeId> =
            scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        assert_eq!(after.len(), N);
        for (slot, i) in (1..N).rev().enumerate() {
            assert_eq!(after[slot + 1], before[i], "n{i} must keep its NodeId across the reversal");
        }
        assert!(!before.contains(&after[0]), "the never-seen `fresh` id must allocate rather than inherit n0's node");
        assert!(!after.contains(&before[0]), "n0 left the config");
    }
}

use std::rc::Rc;

use mlua::{Lua, Value};

use crate::layout::node::{self, LayoutError, PropMap, fields};
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

#[cfg(test)]
mod tests {
    use crate::layout::node::scene_lua;
    use crate::layout::scene::tests::{apply_at, apply_on_two_outputs, full, surface_from};
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
}

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};

use super::ResolvedNode;
use crate::layout::node;
use crate::lua::signal::CellId;

/// Prior state of one `geometry(name)` cell modified during a speculative pass, for rollback.
pub(super) struct GeometryWrite {
    id: CellId,
    cell: Rc<RefCell<Value>>,
    prev_value: Value,
    prev_stamp: Option<u64>,
}

/// Restores `geometry` cells written from `log[from..]` in reverse order, along with their prior
/// write stamps, returning the restored cell ids.
pub(super) fn rollback_geometry(log: &mut Vec<GeometryWrite>, from: usize) -> Vec<CellId> {
    let mut rolled = Vec::with_capacity(log.len().saturating_sub(from));
    let mut stamps = Vec::with_capacity(log.len().saturating_sub(from));
    while log.len() > from {
        let entry = log.pop().expect("len checked above");
        *entry.cell.borrow_mut() = entry.prev_value;
        rolled.push(entry.id);
        stamps.push((entry.id, entry.prev_stamp));
    }
    if !stamps.is_empty() {
        crate::lua::signal::restore_write_stamps(stamps);
    }
    rolled
}

/// Writes every visible `geometry(name)` node's absolute rect into its signal (ADR-0147), the same
/// space `hover_writes` reports. No dirty flag: a pass write that changed a rect notes it for
/// `RendererClient` to turn into one follow-up pass, and a `quiet` tick write notes nothing, so no
/// frame ever schedules a pass. Hidden nodes keep their last rect, the way a hidden subtree keeps
/// everything else.
pub(super) fn publish_geometry(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    lua: &Lua,
    quiet: bool,
    mut undo: Option<&mut Vec<GeometryWrite>>,
) -> mlua::Result<()> {
    if !node.in_flow() {
        return Ok(());
    }
    let rect = node.at(origin_x, origin_y);
    if let Some((id, cell)) = node::signal_at(&node.properties, "geometry").and_then(|signal| signal.geometry_cell()) {
        const KEYS: [&str; 4] = ["x", "y", "width", "height"];
        let fresh = [rect.x, rect.y, rect.width, rect.height];
        let same = match &*cell.borrow() {
            Value::Table(old) => KEYS.into_iter().zip(fresh).all(|(key, v)| old.get::<f32>(key).ok() == Some(v)),
            _ => false,
        };
        if !same {
            let table = Value::Table(crate::lua::marshal::rect_table(lua, rect)?);
            let prev_value = cell.replace(table);
            if let Some(log) = undo.as_deref_mut() {
                let prev_stamp = crate::lua::signal::note_write_prev(id);
                log.push(GeometryWrite { id, cell, prev_value, prev_stamp });
            } else {
                crate::lua::signal::note_write(id);
            }
            if !quiet {
                crate::lua::signal::note_layout_changed(lua, id);
            }
        }
    }
    for child in &node.children {
        publish_geometry(child, rect.x, rect.y, lua, quiet, undo.as_deref_mut())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::layout::scene::tests::{apply_at, full, surface_from};
    use crate::layout::scene::*;

    #[test]
    fn a_geometry_signal_reads_the_nodes_absolute_rect_after_the_pass_without_dirtying_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"g = geometry("card")
            return panel { id = "bar", child = column { padding = { left = 10, top = 5 }, children = {
                rect { width = 30, height = 20, geometry = g } } } }"#,
        );
        let dirty = crate::lua::signal::DirtyFlag::new();
        crate::lua::signal::register(&lua, dirty.clone()).unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let (x, y, h): (f32, f32, f32) = lua.load("local r = g:get() return r.x, r.y, r.height").eval().unwrap();
        assert_eq!((x, y, h), (10.0, 5.0, 20.0));
        assert!(!dirty.take(), "the pass itself does not dirty; the client decides on one follow-up");
        assert!(!crate::lua::signal::take_layout_changed(&lua).is_empty(), "the first measurement is a change");
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(crate::lua::signal::take_layout_changed(&lua).is_empty(), "an unchanged rect notes nothing");
        let err = lua.load("g:set(1)").exec().unwrap_err().to_string();
        assert!(err.contains("a geometry"), "{err}");
    }

    #[test]
    fn vetoed_or_failed_pass_restores_geometry_stamp_and_layout_changed_notes() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, v1) = surface_from(
            r#"g = geometry("card")
            other = state("other", 1)
            return panel { id = "bar", child = rect { width = 30, height = 20, geometry = g } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&v1), full(), &shaping, &lua).unwrap();
        let g_id = crate::lua::signal::from_userdata(&lua.globals().get("g").unwrap()).unwrap().cell_id().unwrap();
        let other_id =
            crate::lua::signal::from_userdata(&lua.globals().get("other").unwrap()).unwrap().cell_id().unwrap();
        assert_eq!(crate::lua::signal::take_layout_changed(&lua), vec![g_id]);
        let pre_stamp = crate::lua::signal::write_clock(&lua);

        // Pre-existing LayoutChanged notes stay intact while the vetoed pass's notes roll back.
        crate::lua::signal::note_layout_changed(&lua, other_id);
        let v2 = crate::lua::nodes::deserialize_lua_table(
            &lua.load(r#"return panel { id = "bar", child = rect { width = 99, height = 20, geometry = g } }"#)
                .eval::<mlua::Table>()
                .unwrap(),
        )
        .unwrap();
        let instances = [crate::layout::scene::tests::instance_at(&v2, full())];
        let err = scene
            .apply_admitting(&[v2], &instances, &shaping, &lua, |_, _| Err(node::invalid("child", "vetoed")))
            .unwrap_err();
        assert!(err.to_string().contains("vetoed"), "{err}");
        let width: f32 = lua.load("return g:get().width").eval().unwrap();
        assert_eq!(width, 30.0);
        assert_eq!(crate::lua::signal::take_layout_changed(&lua), vec![other_id]);
        assert!(!crate::lua::signal::written_since(pre_stamp, &[g_id]));

        // A post-visiting failure (`check_image_status_names`) also rolls back earlier instances' geometry.
        let dup_tables: Vec<mlua::Table> = lua
            .load(
                r#"s = image_status("dup")
                return {
                    panel { id = "bar", child = rect { width = 77, height = 20, geometry = g } },
                    panel { id = "imgs", child = row { children = {
                        image { id = "a", source = "/tmp/a.png", status = s },
                        image { id = "b", source = "/tmp/b.png", status = s },
                    } } },
                }"#,
            )
            .eval()
            .unwrap();
        let dup_surfaces: Vec<crate::lua::nodes::VirtualNode> =
            dup_tables.iter().map(|t| crate::lua::nodes::deserialize_lua_table(t).unwrap()).collect();
        let dup_instances: Vec<_> =
            dup_surfaces.iter().map(|s| crate::layout::scene::tests::instance_at(s, full())).collect();
        let err = scene.apply_locked(&dup_surfaces, &dup_instances, &shaping, &lua, false, false).unwrap_err();
        assert!(err.to_string().contains("image_status"), "{err}");
        let width: f32 = lua.load("return g:get().width").eval().unwrap();
        assert_eq!(width, 30.0);
        assert!(crate::lua::signal::take_layout_changed(&lua).is_empty());
        assert!(!crate::lua::signal::written_since(pre_stamp, &[g_id]));
    }

    #[test]
    fn rollback_geometry_from_slice_preserves_earlier_writes_and_unwinds_same_cell_in_reverse() {
        let lua = crate::layout::node::scene_lua();
        lua.load(r#"g1 = geometry("g1") g2 = geometry("g2")"#).exec().unwrap();
        let mut node1 = ResolvedNode::test("rect", (0.0, 0.0, 25.0, 10.0), vec![]);
        node1.properties =
            std::rc::Rc::new(crate::layout::node::PropMap::from_iter([("geometry", lua.globals().get("g1").unwrap())]));
        let mut node2_a = ResolvedNode::test("rect", (0.0, 0.0, 40.0, 10.0), vec![]);
        node2_a.properties =
            std::rc::Rc::new(crate::layout::node::PropMap::from_iter([("geometry", lua.globals().get("g2").unwrap())]));
        let mut node2_b = ResolvedNode::test("rect", (0.0, 0.0, 80.0, 10.0), vec![]);
        node2_b.properties =
            std::rc::Rc::new(crate::layout::node::PropMap::from_iter([("geometry", lua.globals().get("g2").unwrap())]));

        let g1_id = crate::lua::signal::from_userdata(&lua.globals().get("g1").unwrap()).unwrap().cell_id().unwrap();
        let g2_id = crate::lua::signal::from_userdata(&lua.globals().get("g2").unwrap()).unwrap().cell_id().unwrap();
        let before_g1 = crate::lua::signal::write_clock(&lua);
        let mut undo = Vec::new();
        publish_geometry(&node1, 0.0, 0.0, &lua, false, Some(&mut undo)).unwrap();
        let after_g1 = crate::lua::signal::write_clock(&lua);
        let undo_len = undo.len();
        publish_geometry(&node2_a, 0.0, 0.0, &lua, false, Some(&mut undo)).unwrap();
        publish_geometry(&node2_b, 0.0, 0.0, &lua, false, Some(&mut undo)).unwrap();
        assert_eq!(lua.load("return g2:get().width").eval::<f32>().unwrap(), 80.0);

        let rect = |name: &str| {
            lua.load(format!("local r = {name}:get() return r.x, r.y, r.width, r.height"))
                .eval::<(f32, f32, f32, f32)>()
                .unwrap()
        };
        let rolled = rollback_geometry(&mut undo, undo_len);
        assert_eq!(rolled, vec![g2_id, g2_id]);
        assert_eq!(undo.len(), undo_len, "earlier instance writes remain in undo log");
        assert_eq!(lua.load("return g1:get().width").eval::<f32>().unwrap(), 25.0);
        assert_eq!(rect("g2"), (0.0, 0.0, 0.0, 0.0));
        assert!(!crate::lua::signal::written_since(after_g1, &[g2_id]));

        // Full rollback also restores `g1` cleanly.
        let rolled = rollback_geometry(&mut undo, 0);
        assert_eq!(rolled, vec![g1_id]);
        assert_eq!(rect("g1"), (0.0, 0.0, 0.0, 0.0));
        assert!(!crate::lua::signal::written_since(before_g1, &[g1_id]));
        let _ = crate::lua::signal::take_layout_changed(&lua);
    }
}

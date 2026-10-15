use mlua::Lua;
use mlua::Value;

use super::ResolvedNode;
use crate::layout::node;

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
            *cell.borrow_mut() = Value::Table(crate::lua::marshal::rect_table(lua, rect)?);
            crate::lua::signal::note_write(id);
            if !quiet {
                crate::lua::signal::note_layout_changed(lua, id);
            }
        }
    }
    for child in &node.children {
        publish_geometry(child, rect.x, rect.y, lua, quiet)?;
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
}

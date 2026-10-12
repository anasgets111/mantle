use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mlua::{Lua, Value};

use super::{CellId, Signal, SignalKind, next_cell_id, note_layout_changed, note_write, same_value};
use crate::lua::luacats::{SignalOf, lua_fn, spelled};

/// The value `image_status(name)` holds.
pub(crate) struct ImageStatus;
spelled!(ImageStatus => r#"{ state: "empty"|"loading"|"ready"|"failed", width: integer?, height: integer? }"#);

/// Names, and the cells the last successful pass bound: a reload keeps those names' values and
/// drops the rest.
#[derive(Default)]
struct Registry {
    names: HashMap<String, Signal>,
    bound: HashSet<CellId>,
}

pub(super) fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// Read-only, initially `{ state = "empty" }`. The load of the `source` of the image bound
        /// through `status`: `"loading"` until its decode lands, then `"ready"` with the file's own
        /// `width` and `height` in pixels, or `"failed"`.
        /// [docs](https://anasgets111.github.io/mantle/guide/signals.html#image_status-read-an-images-load)
        fn image_status(
            lua,
            /// One name, one read-only signal, across reloads. Bind it as one image's `status`.
            name: String,
        ) -> SignalOf<ImageStatus> {
            let empty = Value::Table(lua.create_table_from([("state", "empty")])?);
            let signal = crate::lua::app_data_or_default::<Registry>(lua)
                .names
                .entry(name)
                .or_insert_with(|| Signal(SignalKind::ImageStatus(next_cell_id(), Rc::new(RefCell::new(empty)))))
                .clone();
            Ok(SignalOf::new(signal))
        }
    )
}

pub(crate) fn any_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<Registry>().is_some_and(|registry| !registry.names.is_empty())
}

/// Notes the cells the scene binds after a successful pass, which the next evaluation keeps.
pub(crate) fn note_bound(lua: &Lua, cells: HashSet<CellId>) {
    if let Some(mut registry) = lua.app_data_mut::<Registry>() {
        registry.bound = cells;
    }
}

/// Drops the names the last successful pass bound no image to; called as an evaluation begins,
/// so a reload that fails keeps them all.
pub(super) fn shrink(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<Registry>() {
        let Registry { names, bound } = &mut *registry;
        names.retain(|_, signal| cell(signal).is_some_and(|cell| bound.contains(&cell)));
    }
}

/// `signal`'s cell, if it is an `image_status`.
pub(crate) fn cell(signal: &Signal) -> Option<CellId> {
    if let SignalKind::ImageStatus(id, _) = &signal.0 { Some(*id) } else { None }
}

/// Sets an `image_status` `signal` to `state`; other kinds are left alone. A change earns its
/// readers the one follow-up pass every layout measurement gets.
pub(crate) fn write(lua: &Lua, signal: &Signal, state: &str, size: Option<crate::image::Natural>) -> mlua::Result<()> {
    let SignalKind::ImageStatus(id, cell) = &signal.0 else { return Ok(()) };
    let value = lua.create_table_from([("state", state)])?;
    if let Some((width, height)) = size {
        value.set("width", width)?;
        value.set("height", height)?;
    }
    let value = Value::Table(value);
    if !same_value(&cell.borrow(), &value) {
        *cell.borrow_mut() = value;
        note_write(*id);
        note_layout_changed(lua, *id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_evaluation_keeps_the_names_an_image_bound_and_drops_the_rest() {
        let lua = crate::layout::node::scene_lua();
        lua.load(r#"kept = image_status("kept"); image_status("gone")"#).exec().unwrap();
        let kept = super::super::from_userdata(&lua.globals().get("kept").unwrap()).unwrap();
        note_bound(&lua, cell(&kept).into_iter().collect());
        shrink(&lua);
        let names: Vec<String> = lua.app_data_ref::<Registry>().unwrap().names.keys().cloned().collect();
        assert_eq!(names, ["kept"]);
    }
}

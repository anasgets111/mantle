use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mlua::{Lua, Value};

use super::{CellId, Signal, SignalKind, next_cell_id, note_layout_changed, note_write};
use crate::lua::luacats::{SignalOf, lua_fn};

#[derive(Default)]
struct Registry(HashMap<String, Signal>);

pub(super) fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// Read-only, initially false. Whether any visible text bound through `elided` lost content
        /// to `elide` or `max_lines` after fitting.
        /// [docs](https://anasgets111.github.io/mantle/guide/signals.html#elided-read-text-truncation)
        fn elided(
            lua,
            /// One name, one read-only signal, across reloads. Bind it as a text node's `elided`.
            name: String,
        ) -> SignalOf<bool> {
            let signal = crate::lua::app_data_or_default::<Registry>(lua)
                .0.entry(name)
                .or_insert_with(|| Signal(SignalKind::Elided(next_cell_id(), Rc::new(RefCell::new(Value::Boolean(false))))))
                .clone();
            Ok(SignalOf::new(signal))
        }
    )
}

pub(crate) fn any_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<Registry>().is_some_and(|registry| !registry.0.is_empty())
}

/// Publish the whole retained scene together: shared handles use any truncated binding, and a
/// hidden or removed final binding clears its value. Narrowed passes still include other outputs.
pub(crate) fn publish(lua: &Lua, truncated: &HashSet<CellId>) {
    let Some(registry) = lua.app_data_ref::<Registry>() else { return };
    for signal in registry.0.values() {
        let SignalKind::Elided(id, cell) = &signal.0 else { unreachable!() };
        let value = Value::Boolean(truncated.contains(id));
        if *cell.borrow() != value {
            *cell.borrow_mut() = value;
            note_write(*id);
            note_layout_changed(lua, *id);
        }
    }
}

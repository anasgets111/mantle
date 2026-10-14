use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mlua::{Lua, Value};

use super::{CellId, Signal, SignalKind, next_cell_id, note_layout_changed, note_write};
use crate::lua::luacats::{SignalOf, lua_fn};

/// A read-only boolean the scene writes: marked by a visible binding (text cut short, a tween running).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Mark {
    Elided,
    Settled,
}

impl Mark {
    /// What a name reads with no visible binding marked.
    fn unmarked(self) -> bool {
        self == Self::Settled
    }

    fn signal(self, id: CellId, cell: Rc<RefCell<Value>>) -> SignalKind {
        match self {
            Self::Elided => SignalKind::Elided(id, cell),
            Self::Settled => SignalKind::Settled(id, cell),
        }
    }
}

#[derive(Default)]
struct Registry(HashMap<(Mark, String), Signal>);

fn named(lua: &Lua, mark: Mark, name: String) -> Signal {
    crate::lua::app_data_or_default::<Registry>(lua)
        .0
        .entry((mark, name))
        .or_insert_with(|| Signal(mark.signal(next_cell_id(), Rc::new(RefCell::new(Value::Boolean(mark.unmarked()))))))
        .clone()
}

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
            Ok(SignalOf::new(named(lua, Mark::Elided, name)))
        }
    )?;
    lua_fn!(
        lua,
        /// Read-only, initially true. Whether every `animate` tween on the node bound through
        /// `settled` is at rest; false from the pass that starts one until the frame that ends the
        /// last.
        /// [docs](https://anasgets111.github.io/mantle/guide/animation.html#settled-know-when-a-node-stops-moving)
        fn settled(
            lua,
            /// One name, one read-only signal, across reloads. Bind it as a node's `settled`.
            name: String,
        ) -> SignalOf<bool> {
            Ok(SignalOf::new(named(lua, Mark::Settled, name)))
        }
    )
}

pub(crate) fn any_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<Registry>().is_some_and(|registry| !registry.0.is_empty())
}

/// Publish the whole retained scene together: shared handles use any marked binding, and a hidden
/// or removed final binding clears its value. `elided` and `settled` are the marked cells. Only a
/// changed value is written, and only that earns its readers the follow-up pass.
pub(crate) fn publish(lua: &Lua, elided: &HashSet<CellId>, settled: &HashSet<CellId>) {
    if let Some(mut registry) = lua.app_data_mut::<Registry>() {
        // An entry only the registry holds has no reader or binding left; asking again makes a new one.
        registry.0.retain(|_, signal| match &signal.0 {
            SignalKind::Elided(_, cell) | SignalKind::Settled(_, cell) => Rc::strong_count(cell) > 1,
            _ => true,
        });
    }
    let Some(registry) = lua.app_data_ref::<Registry>() else { return };
    for ((mark, _), signal) in &registry.0 {
        let (SignalKind::Elided(id, cell) | SignalKind::Settled(id, cell)) = &signal.0 else { unreachable!() };
        let marked = match mark {
            Mark::Elided => elided,
            Mark::Settled => settled,
        };
        let value = Value::Boolean(marked.contains(id) != mark.unmarked());
        if *cell.borrow() != value {
            *cell.borrow_mut() = value;
            note_write(*id);
            note_layout_changed(lua, *id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_nothing_holds_any_more_leaves_the_registry() {
        let lua = Lua::new();
        register(&lua).unwrap();
        lua.load(r#"kept = settled("kept"); for i = 1, 3 do settled("toast-" .. i) end"#).exec().unwrap();
        let names = |lua: &Lua| lua.app_data_ref::<Registry>().map(|r| r.0.len());
        assert_eq!(names(&lua), Some(4));
        lua.gc_collect().unwrap();
        publish(&lua, &HashSet::new(), &HashSet::new());
        assert_eq!(names(&lua), Some(1), "only the held name stays");
    }
}

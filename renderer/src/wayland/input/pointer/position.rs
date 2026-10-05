//! `pointer(name)`: the pointer's node-local position, written from the same hit path as hover.

use mlua::Value;

use super::*;
use crate::lua::signal::{any_pointer_registered, is_read};

/// Writes each signal its position, `nil` off the node. One that nothing reads gets no table and no
/// dirty mark, only a `nil` where a stale position was left, so motion over it costs the walk alone.
pub(crate) fn apply_pointer_writes(
    lua: &Lua,
    writes: Vec<(crate::lua::signal::Signal, Option<layout::hit::LogicalPoint>)>,
) {
    for (signal, local) in writes {
        let (Some(handle), Some(id)) = (signal.pointer_handle(), signal.cell_id()) else { continue };
        let held = handle.get();
        let local = local.filter(|_| is_read(lua, id));
        let same = match (&held, local) {
            (Value::Nil, None) => true,
            (Value::Table(table), Some(at)) => {
                table.get::<f32>("x").ok() == Some(at.x) && table.get::<f32>("y").ok() == Some(at.y)
            }
            _ => false,
        };
        if same {
            continue;
        }
        match local.map(|at| local_pointer_table(lua, at)).transpose() {
            Ok(table) => handle.set(table.map_or(Value::Nil, Value::Table)),
            Err(err) => warn!("could not build a pointer position: {err}"),
        }
    }
}

impl App {
    /// Rewrites every `pointer` signal on surface `index` from where the pointer is now. Once per
    /// motion batch, not per `wl_pointer` event: only the last position of a batch is observable.
    pub(in crate::wayland) fn sync_pointer(&self, index: usize) {
        let lua = self.client.lua();
        if !any_pointer_registered(lua) {
            return;
        }
        let surface_id = &self.surfaces[index].surface_id;
        let Some(tree) = self.client.scene().surface(surface_id) else { return };
        let point = (self.pointer_at.as_ref())
            .filter(|(at, _)| at == surface_id)
            .map(|(_, position)| layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 });
        let path = point.map(|point| layout::hit::hit_path(tree, point)).unwrap_or_default();
        apply_pointer_writes(lua, layout::hover::pointer_writes(tree, &path, point));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::signal::{
        DirtyFlag, DirtyScope, begin_instance_resolve, end_instance_resolve, from_userdata, register,
    };

    fn at(x: f32, y: f32) -> Option<layout::hit::LogicalPoint> {
        Some(layout::hit::LogicalPoint { x, y })
    }

    /// `pointer("t")` with `bar` having resolved with `reads` its value or not.
    fn setup(reads: bool) -> (Lua, DirtyFlag, crate::lua::signal::Signal) {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        lua.load(r#"p = pointer("t")"#).exec().unwrap();
        let signal = from_userdata(&lua.globals().get("p").unwrap()).unwrap();
        begin_instance_resolve(&lua, "bar");
        if reads {
            lua.load("p:get()").exec().unwrap();
        }
        end_instance_resolve(&lua);
        (lua, dirty, signal)
    }

    fn held(lua: &Lua) -> Option<(f32, f32)> {
        let table: Option<mlua::Table> = lua.load("return p:get()").eval().unwrap();
        table.map(|table| (table.get("x").unwrap(), table.get("y").unwrap()))
    }

    #[test]
    fn a_read_pointer_holds_the_position_inside_and_nil_after_leaving_and_dirties_only_its_reader() {
        let (lua, dirty, signal) = setup(true);
        assert_eq!(held(&lua), None, "nil until the pointer is over the node");

        apply_pointer_writes(&lua, vec![(signal.clone(), at(30.0, 10.0))]);
        assert_eq!(held(&lua), Some((30.0, 10.0)));
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Instances(vec!["bar".into()]));

        apply_pointer_writes(&lua, vec![(signal.clone(), at(30.0, 10.0))]);
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Clean, "the same position is no change");

        apply_pointer_writes(&lua, vec![(signal, None)]);
        assert_eq!(held(&lua), None, "a leave clears it");
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Instances(vec!["bar".into()]));
    }

    #[test]
    fn an_unread_pointer_is_never_written_and_a_stale_position_is_cleared() {
        let (lua, dirty, signal) = setup(false);
        apply_pointer_writes(&lua, vec![(signal.clone(), at(5.0, 5.0))]);
        assert_eq!(held(&lua), None, "nothing reads it, so no table is built");
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Clean);

        // A reader that came and went leaves its last position behind until the next write.
        signal
            .pointer_handle()
            .unwrap()
            .set(Value::Table(local_pointer_table(&lua, layout::hit::LogicalPoint { x: 1.0, y: 2.0 }).unwrap()));
        apply_pointer_writes(&lua, vec![(signal, at(5.0, 5.0))]);
        assert_eq!(held(&lua), None);
    }

    #[test]
    fn a_reader_that_appears_under_a_still_pointer_gets_the_position_on_the_post_pass_refresh() {
        let (lua, dirty, signal) = setup(false);
        apply_pointer_writes(&lua, vec![(signal.clone(), at(30.0, 10.0))]);
        assert_eq!(held(&lua), None, "unread while the pointer moved");

        // The reader's first resolve, then the refresh `re_resolve` is followed by: no new motion.
        begin_instance_resolve(&lua, "bar");
        lua.load("p:get()").exec().unwrap();
        end_instance_resolve(&lua);
        apply_pointer_writes(&lua, vec![(signal.clone(), at(30.0, 10.0))]);
        assert_eq!(held(&lua), Some((30.0, 10.0)));
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Instances(vec!["bar".into()]), "one follow-up pass");

        apply_pointer_writes(&lua, vec![(signal, at(30.0, 10.0))]);
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Clean, "and no loop");
    }
}

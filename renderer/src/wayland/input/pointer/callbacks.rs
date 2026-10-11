//! Marshalling for the pointer's Lua callbacks: `on_click`, `on_press`, `on_drag` and `on_hover`.

use super::*;

/// The node-local `{ x, y }` table `on_drag` and `on_click` pass.
pub(super) fn local_pointer_table(lua: &Lua, local: layout::hit::LogicalPoint) -> mlua::Result<mlua::Table> {
    let pointer = lua.create_table()?;
    pointer.set("x", local.x)?;
    pointer.set("y", local.y)?;
    Ok(pointer)
}

/// Call `on_click` with its node rect in surface logical coordinates (ADR-0050 decision 3), the
/// button, and `position` as a node-local pointer like `on_drag`'s. The rect round-trips to popup
/// `anchor_rect` through Lua. Error labels distinguish building the engine's argument from a
/// raised config handler.
pub(in crate::wayland::input) fn call_on_click(
    lua: &Lua,
    on_click: &Function,
    rect: LogicalRect,
    button: &str,
    local: layout::hit::LogicalPoint,
    modifiers: [bool; 4],
) -> Result<(), (&'static str, mlua::Error)> {
    let argument = rect_table(lua, rect).map_err(|e| ("could not build on_click's rect argument", e))?;
    let pointer = local_pointer_table(lua, local).map_err(|e| ("could not build on_click's pointer argument", e))?;
    let modifiers =
        modifiers_table(lua, modifiers).map_err(|e| ("could not build on_click's modifiers argument", e))?;
    on_click.call::<()>((argument, button, pointer, modifiers)).map_err(|e| ("on_click raised, ignoring it", e))
}

/// Call `on_press` like `on_click`: rect, button name and node-local pointer.
pub(super) fn call_on_press(
    lua: &Lua,
    on_press: &Function,
    target: &PressTarget,
    button: &str,
    modifiers: [bool; 4],
) -> Result<(), (&'static str, mlua::Error)> {
    let rect = rect_table(lua, target.rect).map_err(|e| ("could not build on_press's rect argument", e))?;
    let pointer =
        local_pointer_table(lua, target.at).map_err(|e| ("could not build on_press's pointer argument", e))?;
    let modifiers =
        modifiers_table(lua, modifiers).map_err(|e| ("could not build on_press's modifiers argument", e))?;
    on_press.call::<()>((rect, button, pointer, modifiers)).map_err(|e| ("on_press raised, ignoring it", e))
}

/// Call `on_drag` with the rect, pointer in node-local coordinates, gesture phase (ADR-0116
/// decision 1), modifiers and surface-logical velocity in px/s. Local coordinates avoid repeated
/// subtraction in handlers; unclamped coordinates let each config apply its own `min`/`max`.
pub(super) fn call_on_drag(
    lua: &Lua,
    on_drag: &Function,
    rect: LogicalRect,
    local: layout::hit::LogicalPoint,
    phase: DragPhase,
    modifiers: [bool; 4],
    (vx, vy): (f64, f64),
) -> Result<(), (&'static str, mlua::Error)> {
    let rect_argument = rect_table(lua, rect).map_err(|e| ("could not build on_drag's rect argument", e))?;
    let pointer = local_pointer_table(lua, local).map_err(|e| ("could not build on_drag's pointer argument", e))?;
    let modifiers = modifiers_table(lua, modifiers).map_err(|e| ("could not build on_drag's modifiers argument", e))?;
    let velocity = local_pointer_table(lua, layout::hit::LogicalPoint { x: vx as f32, y: vy as f32 })
        .map_err(|e| ("could not build on_drag's velocity argument", e))?;
    on_drag
        .call::<()>((rect_argument, pointer, phase.name(), modifiers, velocity))
        .map_err(|e| ("on_drag raised, ignoring it", e))
}

/// Writes one node's hover answer: the boolean, the rect it was crossed at, then its `on_hover`.
///
/// The rect precedes the callback because a handler is documented to read `hover_rect(name)` for
/// the crossing it was called for; writing it after would hand a per-item tooltip the rect of the
/// item left behind, or the startup placeholder on a first hover.
pub(crate) fn apply_hover_write(lua: &Lua, write: layout::hover::HoverWrite, fires_on_hover: bool, surface_id: &str) {
    // Non-hover signals stay untouched, so `hover = mantle.network` cannot overwrite a
    // capability snapshot (ADR-0062 decision 2).
    let Some(handle) = write.signal.hover_handle() else {
        return;
    };
    // Rects are edge-only, not merely an optimization: mlua table equality is identity, so
    // a fresh equal table would undo decision 4 on every motion.
    if handle.set_changed(mlua::Value::Boolean(write.hovered))
        && let Some(rect) = write.rect
        && let Some(rect_handle) = write.signal.hover_rect_handle()
    {
        match rect_table(lua, rect) {
            Ok(table) => rect_handle.set(mlua::Value::Table(table)),
            // The boolean landed; keep the last tooltip position on table-build failure.
            Err(err) => warn!("{surface_id}: could not build a hover rect: {err}"),
        }
    }
    // Fire only on the pointer's own edges (ADR-0095): device-rate motion could call a handler
    // hundreds of times across one node. Swallow handler errors like `fire_on_click`.
    if fires_on_hover
        && write.signal.pointer_crossed(write.hovered)
        && let Some(on_hover) = &write.on_hover
    {
        call_logged(on_hover, write.hovered, format_args!("{surface_id}: on_hover handler"));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::pt;
    use super::*;
    use mlua::Table;

    /// A per-item tooltip reads the hovered item's rect in `on_hover` to place itself, so the
    /// crossing's rect has to be there when the handler runs.
    #[test]
    fn on_hover_reads_the_rect_of_the_crossing_it_was_called_for() {
        let lua = Lua::new();
        let (signal, rect_signal) =
            crate::lua::signal::Signal::new_hover(crate::lua::signal::DirtyFlag::new(), mlua::Value::Boolean(false));
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        let sink = std::rc::Rc::clone(&seen);
        let read_rect = rect_signal.clone();
        let on_hover = lua
            .create_function(move |lua, _hovered: bool| {
                let mlua::Value::Table(rect) = read_rect.get_value(lua).unwrap() else { panic!("a rect table") };
                *sink.borrow_mut() = Some(rect.get::<f32>("x").unwrap());
                Ok(())
            })
            .unwrap();

        let write = layout::hover::HoverWrite {
            signal,
            hovered: true,
            rect: Some(LogicalRect { x: 140.0, y: 0.0, width: 24.0, height: 24.0 }),
            on_hover: Some(on_hover),
        };
        apply_hover_write(&lua, write, true, "bar");

        assert_eq!(*seen.borrow(), Some(140.0), "the handler saw this crossing, not the one before it");
    }

    /// Layout writes `hover` silently under a still pointer (ADR-0112 decision 8), so `on_hover`
    /// judges an edge by what the last pointer event said, not by the signal: the first motion
    /// after layout slid the node in or out still fires, and a still pointer never does.
    #[test]
    fn on_hover_fires_on_the_first_motion_after_layout_moved_the_node_under_or_off_the_pointer() {
        let lua = Lua::new();
        let (signal, rect_signal) =
            crate::lua::signal::Signal::new_hover(crate::lua::signal::DirtyFlag::new(), mlua::Value::Boolean(false));
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&seen);
        let on_hover = lua
            .create_function(move |lua, hovered: bool| {
                let mlua::Value::Table(rect) = rect_signal.get_value(lua).unwrap() else { panic!("a rect table") };
                sink.borrow_mut().push((hovered, rect.get::<f32>("x").unwrap()));
                Ok(())
            })
            .unwrap();
        let write = |hovered, x, pointer| {
            let write = layout::hover::HoverWrite {
                signal: signal.clone(),
                hovered,
                // `hover_writes` reports a rect only for a hovered node.
                rect: hovered.then_some(LogicalRect { x, y: 0.0, width: 24.0, height: 24.0 }),
                on_hover: Some(on_hover.clone()),
            };
            apply_hover_write(&lua, write, pointer, "bar");
            std::mem::take(&mut *seen.borrow_mut())
        };
        let none = Vec::<(bool, f32)>::new();

        assert_eq!(write(true, 10.0, false), none, "layout sliding the node under a still pointer is silent");
        assert_eq!(write(false, 0.0, true), none, "a pointer that never entered does not leave");
        assert_eq!(write(true, 10.0, false), none);
        assert_eq!(write(true, 20.0, true), [(true, 10.0)], "the next motion fires, with the layout-time rect");
        assert_eq!(write(true, 20.0, true), none, "motion that stays inside fires nothing");
        assert_eq!(write(false, 0.0, false), none, "layout sliding it off is silent");
        assert_eq!(write(false, 0.0, true), [(false, 10.0)], "the next motion outside it is the pointer's leave");
    }

    #[test]
    fn on_drag_is_handed_the_pointer_in_the_nodes_own_coordinates() {
        let lua = Lua::new();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&seen);
        let handler = lua
            .create_function(move |_, (rect, pointer, phase, _, velocity): (Table, Table, String, Table, Table)| {
                sink.borrow_mut().push((
                    rect.get::<f32>("width").unwrap(),
                    pointer.get::<f32>("x").unwrap(),
                    pointer.get::<f32>("y").unwrap(),
                    phase,
                    (velocity.get::<f32>("x").unwrap(), velocity.get::<f32>("y").unwrap()),
                ));
                Ok(())
            })
            .unwrap();
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        call_on_drag(&lua, &handler, rect, pt(20.0, 6.0), DragPhase::Start, [false; 4], (0.0, 0.0)).unwrap();
        // Past the right edge: unclamped, so the config's own clamp is what pins the slider.
        call_on_drag(&lua, &handler, rect, pt(50.0, 6.0), DragPhase::End, [false; 4], (1200.0, -40.0)).unwrap();
        let start = (40.0, 20.0, 6.0, "start".to_string(), (0.0, 0.0));
        assert_eq!(*seen.borrow(), vec![start, (40.0, 50.0, 6.0, "end".to_string(), (1200.0, -40.0))]);
    }

    #[test]
    fn on_click_takes_the_button_name_as_a_second_argument_beside_the_rect() {
        // Second argument, not a fifth field on the rect table: every handler written against the
        // one-argument form keeps working, since Lua drops arguments a function does not declare.
        let lua = Lua::new();
        let seen: Function = lua
            .load(r#"seen = {} return function(rect, button, pointer) seen.x, seen.y, seen.w, seen.h, seen.button, seen.px, seen.py = rect.x, rect.y, rect.width, rect.height, button, pointer.x, pointer.y end"#)
            .eval()
            .unwrap();
        call_on_click(
            &lua,
            &seen,
            LogicalRect { x: 12.0, y: 4.0, width: 40.0, height: 24.0 },
            "right",
            pt(8.0, 6.0),
            [false; 4],
        )
        .unwrap();

        let recorded: Table = lua.globals().get("seen").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 12.0);
        assert_eq!(recorded.get::<f32>("y").unwrap(), 4.0);
        assert_eq!(recorded.get::<f32>("w").unwrap(), 40.0);
        assert_eq!(recorded.get::<f32>("h").unwrap(), 24.0);
        assert_eq!(recorded.get::<String>("button").unwrap(), "right");
        assert_eq!((recorded.get::<f32>("px").unwrap(), recorded.get::<f32>("py").unwrap()), (8.0, 6.0), "node-local");

        // ADR-0050 decision 3's worked example, which every config in the tree uses: a one-argument handler.
        let anchor: Function = lua.load(r#"return function(rect) anchor = rect end"#).eval().unwrap();
        let rect = LogicalRect { x: 40.0, y: 0.0, width: 86.0, height: 24.0 };
        call_on_click(&lua, &anchor, rect, "left", pt(1.0, 1.0), [false; 4]).unwrap();
        let anchor: Table = lua.globals().get("anchor").unwrap();
        assert_eq!((anchor.get::<f32>("x").unwrap(), anchor.get::<f32>("height").unwrap()), (40.0, 24.0));
    }

    #[test]
    fn the_pointer_handlers_take_the_held_modifiers_fourth() {
        let lua = Lua::new();
        let note: Function = lua
            .load("seen = {} return function(name, m) seen[name] = (m.ctrl and 'c' or '-') .. (m.shift and 's' or '-') end")
            .eval()
            .unwrap();
        let handler = |name: &'static str| {
            let note = note.clone();
            lua.create_function(move |_, args: mlua::MultiValue| {
                // Fourth for every handler; `on_drag`'s velocity follows it.
                let held = args.into_iter().nth(3).expect("a modifiers table");
                note.call::<()>((name, held))
            })
            .unwrap()
        };
        let rect = LogicalRect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 };
        let target =
            PressTarget { rect, at: pt(1.0, 1.0), handler: handler("press"), id: layout::scene::NodeId::test(1) };
        call_on_press(&lua, &target.handler, &target, "left", [true, false, false, false]).unwrap();
        call_on_click(&lua, &handler("click"), rect, "left", pt(1.0, 1.0), [true, true, false, false]).unwrap();
        call_on_drag(
            &lua,
            &handler("drag"),
            rect,
            pt(1.0, 1.0),
            DragPhase::Move,
            [false, true, false, false],
            (0.0, 0.0),
        )
        .unwrap();
        call_on_drag(&lua, &handler("idle"), rect, pt(1.0, 1.0), DragPhase::End, [false; 4], (0.0, 0.0)).unwrap();
        let seen: Table = lua.globals().get("seen").unwrap();
        let read = |name: &str| seen.get::<String>(name).unwrap();
        assert_eq!([read("press"), read("click"), read("drag"), read("idle")], ["c-", "cs", "-s", "--"]);
    }
}

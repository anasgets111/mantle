use mlua::{Lua, Value};
use shared::debug;

use super::pass::publish_geometry;
use super::solver::{MainAxis, main_axis_of};
use super::{LayoutStyle, LogicalSize, ResolvedNode, Scene};
use crate::layout::node::{self, PropMap};
use crate::lua::signal::{CellId, Signal};

impl Scene {
    /// A wheel's `asked` offset for `signal`, applied to the retained trees without a pass
    /// (ADR-0274): each visible container it scrolls moves its children, clamped against the room
    /// they already take, and the instances whose children moved come back with their `geometry`
    /// published. `None`, with nothing written, when only a pass shows the offset: a getter, `:map`
    /// or `list` build read it, or nothing on screen scrolls by it.
    pub fn scroll_in_place(&mut self, signal: &Signal, asked: f32, lua: &Lua) -> Option<Vec<String>> {
        let cell = signal.cell_id()?;
        let handle = signal.scroll_handle()?;
        let mut holders = 0;
        let read = crate::lua::signal::with_derived(cell);
        if !self.surfaces.values().all(|tree| read_only_as_scroll(tree, cell, &read, &mut holders)) || holders == 0 {
            return None;
        }
        handle.set_quiet(Value::Number(f64::from(asked)));
        let mut moved = Vec::new();
        for (key, tree) in &mut self.surfaces {
            if scroll_retained(tree, cell) {
                if let Err(err) = publish_geometry(tree, 0.0, 0.0, lua, false) {
                    debug!("{key}: writing a geometry signal failed: {err}");
                }
                moved.push(key.clone());
            }
        }
        Some(moved)
    }
}

/// The axis `node` scrolls along by `cell`, if it does.
fn scrolls_by(node: &ResolvedNode, cell: CellId) -> Option<MainAxis> {
    let holds = node::signal_at(&node.properties, "scroll").and_then(|signal| signal.cell_id()) == Some(cell);
    if holds { main_axis_of(node.kind, &node.properties).ok().flatten() } else { None }
}

/// Counts the visible containers `cell` scrolls into `holders`; false when a node a pass resolves
/// read it or a computed over it (`read`). Below a hidden node is frozen (ADR-0124): a pass would not show it the offset either.
///
/// ponytail: a scroll slot inside a `list` item is that item's read, so that list falls back to a
/// pass. Upgrade: note a slot to the instance alone, outside every frame.
fn read_only_as_scroll(
    node: &ResolvedNode,
    cell: CellId,
    read: &rustc_hash::FxHashSet<CellId>,
    holders: &mut usize,
) -> bool {
    if node.leaving {
        return true;
    }
    if read.iter().any(|&cell| {
        node.resolve_memo.as_ref().is_some_and(|memo| memo.read(cell))
            || node.list_memo.as_ref().is_some_and(|memo| memo.read(cell))
    }) {
        return false;
    }
    if !node.visible {
        return true;
    }
    *holders += usize::from(scrolls_by(node, cell).is_some());
    node.children.iter().all(|child| read_only_as_scroll(child, cell, read, holders))
}

/// Moves the children of every visible container `cell` scrolls to its offset; whether any moved.
fn scroll_retained(node: &mut ResolvedNode, cell: CellId) -> bool {
    if !node.in_flow() {
        return false;
    }
    let mut moved = false;
    if let Some(axis) = scrolls_by(node, cell) {
        let size = LogicalSize { width: node.rect.width, height: node.rect.height };
        let from = node.scrolled;
        node.scrolled = scroll_children(&node.properties, &node.layout_style, size, axis, &mut node.children, from);
        moved = node.scrolled != from;
    }
    for child in &mut node.children {
        moved |= scroll_retained(child, cell);
    }
    moved
}

/// Clamps this container's asked offset against the room its children take and moves them to it
/// from `from`, the offset they carry now. Returns the offset they carry after.
///
/// Subtracted from every child's main coordinate, so a scrolled child sits before the content box
/// and the clip `layout::paint` computes per node cuts it. Summed here rather than read off
/// taffy's `scrollable_overflow_rect`, since the two disagree: CSS scrollable overflow is the union
/// of the children's border boxes, while this engine's `spacing`-and-margin footprint is what
/// `Fill` was sized against, which the tests pin.
pub(super) fn scroll_children(
    properties: &PropMap,
    style: &LayoutStyle,
    size: LogicalSize,
    axis: MainAxis,
    children: &mut [ResolvedNode],
    from: f32,
) -> f32 {
    let padding = style.padding;
    let (content_main, padding_start) = match axis {
        MainAxis::Horizontal => ((size.width - padding.horizontal()).max(0.0), padding.left),
        MainAxis::Vertical => ((size.height - padding.vertical()).max(0.0), padding.top),
    };
    reveal_child(properties, children, axis, padding_start - from, content_main);
    let offset = scroll_offset(properties, content_main, extent_along(children, axis, style.spacing));
    let by = offset - from;
    if by != 0.0 {
        // A leaver keeps the offset it was dropped at (`finish`).
        for child in children.iter_mut().filter(|child| !child.leaving) {
            match axis {
                MainAxis::Horizontal => child.rect.x -= by,
                MainAxis::Vertical => child.rect.y -= by,
            }
        }
    }
    offset
}

/// How much room this container's visible children take along `axis`, margins and gaps included:
/// the number a scroll offset is clamped against. The same footprint the sizing pass used, which
/// keeps a scroll limit and the layout it scrolls in agreement.
pub(super) fn extent_along(children: &[ResolvedNode], axis: MainAxis, spacing: f32) -> f32 {
    let (extents, visible) = children.iter().filter(|c| c.in_flow()).fold((0.0f32, 0usize), |(sum, count), c| {
        let extent = match axis {
            MainAxis::Horizontal => c.rect.width,
            MainAxis::Vertical => c.rect.height,
        };
        (sum + extent + c.margin_on(axis), count + 1)
    });
    extents + spacing * visible.saturating_sub(1) as f32
}

/// Honours a pending `signal:reveal(index)` on this container's scroll signal (ADR-0112): moves the
/// asked offset the least distance that puts the `index`-th visible child's border box inside the
/// viewport, or leaves it alone when the child is already in view. Written quietly, ahead of
/// [`scroll_offset`], which then clamps it like any wheel ask -- so a reveal past the end lands on
/// the end, and a reveal of a child that does not exist changes nothing. Children are still at
/// their unscrolled positions here, which is what makes `rect` minus the leading padding the
/// child's place in the content.
fn reveal_child(
    properties: &PropMap,
    children: &[ResolvedNode],
    axis: MainAxis,
    padding_start: f32,
    content_main: f32,
) {
    let Some(signal) = node::fields::flow::scroll.read(properties).ok().flatten() else {
        return;
    };
    let Some(index) = signal.take_reveal() else {
        return;
    };
    let Some(child) = children.iter().filter(|c| c.in_flow()).nth(index - 1) else {
        return;
    };
    let (start, extent) = match axis {
        MainAxis::Horizontal => (child.rect.x - padding_start, child.rect.width),
        MainAxis::Vertical => (child.rect.y - padding_start, child.rect.height),
    };
    let asked = signal.scroll_offset().unwrap_or(0.0);
    let wanted = if start < asked {
        start
    } else if start + extent > asked + content_main {
        start + extent - content_main
    } else {
        return;
    };
    if let Some(handle) = signal.scroll_handle() {
        handle.set_quiet(Value::Number(f64::from(wanted)));
    }
}

/// How far this container is scrolled along its main axis, clamped to what there is to scroll, and
/// written back so the signal holds the offset actually used (ADR-0069 decision 4).
///
/// `content_main` is the viewport and `total_main` the content, both already computed by the
/// caller for its own alignment arithmetic, so no new parameter is threaded through the recursion.
///
/// A container with nothing to scroll returns 0 rather than erroring, so a `Content`-sized column
/// (content and viewport the same number by construction) is a no-op, the same answer `Fill` gives
/// in a `Content` parent for the same reason: no remainder (decision 5).
fn scroll_offset(properties: &PropMap, content_main: f32, total_main: f32) -> f32 {
    let Some(signal) = node::fields::flow::scroll.read(properties).ok().flatten() else {
        return 0.0;
    };
    let Some(asked) = signal.scroll_offset() else {
        return 0.0;
    };
    let limit = (total_main - content_main).max(0.0);
    let used = asked.clamp(0.0, limit);
    if used != asked
        && let Some(handle) = signal.scroll_handle()
    {
        // Quiet: this number is derived from the geometry of the pass that is running, so marking
        // the scene dirty would schedule another pass to observe what this one already used.
        handle.set_quiet(Value::Number(f64::from(used)));
    }
    used
}

#[cfg(test)]
mod tests {
    use crate::layout::scene::tests::{apply_at, full, surface_from};
    use crate::layout::scene::*;
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};

    /// A leaving child sits at the rect it was dropped at, scroll offset and all. The pass that
    /// removed it re-solves the scroll for the children that are left, and the leaver does not
    /// travel with them: it fades where the reader last saw it.
    #[test]
    fn a_leaving_child_keeps_the_scroll_offset_it_was_dropped_at() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local function tile(name) return rect { id = name, width = 10, height = 40,
                   background = "#ff0000", animate = { exit = { duration = 100, opacity = 0 } } } end
               return panel { id = "bar", child = column { width = 100, height = 100, spacing = 0,
                   scroll = scroll("s"), children = state("kids", { tile("a"), tile("b"), tile("c") }) } }"##,
        );
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(20.0));
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let ys: Vec<f32> = scene.surface("bar@TEST").unwrap().children[0].children.iter().map(|c| c.rect.y).collect();
        assert_eq!(ys, [-20.0, 20.0, 60.0], "120 px of tiles in a 100 px column, scrolled 20");

        // Dropping the first leaves 80 px, which is less than the column: the offset clamps to 0
        // and the two that stay move up. The leaver holds the -20 it was showing.
        lua.load(r#"local k = state("kids", {}):get(); state("kids", {}):set({ k[2], k[3] })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        let ys: Vec<f32> = column.children.iter().map(|c| c.rect.y).collect();
        assert_eq!(ys, [0.0, 40.0, -20.0], "b and c reflow, a fades where it was");
        assert!(column.children[2].leaving);
    }

    /// ADR-0069. Scrolling moves children within a viewport the clip already cuts them to.
    fn scrolled(lua_src: &str, offset: f32) -> (mlua::Lua, Vec<f32>, f32) {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(f64::from(offset)));

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let container = &scene.surface("bar@TEST").unwrap().children[0];
        let ys = container.children.iter().map(|c| c.rect.y).collect();
        (lua, ys, signal.scroll_offset().unwrap())
    }

    const SCROLLED_COLUMN: &str = r#"panel { id = "bar", child = column { width = 100, height = 100, scroll = scroll("s"), children = {
        rect { width = 10, height = 100 }, rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
    } } }"#;

    fn revealed(index: usize, offset: f32) -> (Vec<f32>, f32) {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(SCROLLED_COLUMN).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(f64::from(offset)));
        assert!(signal.request_reveal(index));

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let container = &scene.surface("bar@TEST").unwrap().children[0];
        let ys = container.children.iter().map(|c| c.rect.y).collect();
        assert!(signal.take_reveal().is_none(), "the pass consumed the ask");
        (ys, signal.scroll_offset().unwrap())
    }

    /// ADR-0112: `signal:reveal(index)` moves the least distance that shows the child, and only
    /// when it is out of view -- so a keyboard selection walking down a list scrolls it a row at a
    /// time and a selection already on screen leaves the wheel's offset alone.
    #[test]
    fn a_reveal_scrolls_the_least_distance_that_shows_the_child() {
        let (ys, used) = revealed(3, 0.0);
        assert_eq!(used, 200.0, "the third 100px child in a 100px viewport: its bottom lands on the viewport's");
        assert_eq!(ys, vec![-200.0, -100.0, 0.0]);

        let (_, used) = revealed(1, 150.0);
        assert_eq!(used, 0.0, "revealing upward puts the child's top at the viewport's top");

        let (_, used) = revealed(2, 100.0);
        assert_eq!(used, 100.0, "a child already in view moves nothing");

        let (_, used) = revealed(9, 20.0);
        assert_eq!(used, 20.0, "a child that does not exist reveals nothing");
    }

    #[test]
    fn a_scroll_offset_moves_children_up_within_the_viewport() {
        let (_lua, ys, used) = scrolled(SCROLLED_COLUMN, 120.0);
        assert_eq!(ys, vec![-120.0, -20.0, 80.0], "every child shifts by the offset, first one out of the box");
        assert_eq!(used, 120.0, "an in-range offset is used as asked");
    }

    #[test]
    fn an_unscrolled_container_places_children_exactly_as_before() {
        let (_lua, ys, _) = scrolled(SCROLLED_COLUMN, 0.0);
        assert_eq!(ys, vec![0.0, 100.0, 200.0]);
    }

    /// The bound is content minus viewport: 300 of children in a 100 box leaves 200 to scroll.
    #[test]
    fn an_offset_past_the_end_is_clamped_and_written_back() {
        let (_lua, ys, used) = scrolled(SCROLLED_COLUMN, 5_000.0);
        assert_eq!(used, 200.0, "the signal holds what was used, not what the wheel asked for");
        assert_eq!(ys, vec![-200.0, -100.0, 0.0], "so the last child sits at the top and nothing scrolls past it");
    }

    #[test]
    fn a_negative_offset_is_clamped_to_the_top() {
        let (_lua, ys, used) = scrolled(SCROLLED_COLUMN, -50.0);
        assert_eq!(used, 0.0);
        assert_eq!(ys[0], 0.0);
    }

    /// Decision 5: a container whose content and viewport are the same number by construction.
    #[test]
    fn a_content_sized_container_has_nothing_to_scroll() {
        let (_lua, ys, used) = scrolled(
            r#"panel { id = "bar", child = column { width = 100, scroll = scroll("s"), children = {
                rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
            } } }"#,
            80.0,
        );
        assert_eq!(used, 0.0, "no remainder, so the offset is clamped away rather than erroring");
        assert_eq!(ys, vec![0.0, 100.0]);
    }

    /// `max_height` is what makes a content-sized container scrollable: below the cap it is exactly
    /// its children, at the cap it stops and the rest is remainder.
    #[test]
    fn a_max_height_caps_a_content_sized_column_and_leaves_the_rest_to_scroll() {
        let capped = r#"panel { id = "bar", child = column { width = 100, max_height = 150, scroll = scroll("s"), children = {
            rect { width = 10, height = 100 }, rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
        } } }"#;
        let (lua, ys, used) = scrolled(capped, 5_000.0);
        assert_eq!(used, 150.0, "300 of children in a box capped at 150 leaves 150 to scroll");
        assert_eq!(ys, vec![-150.0, -50.0, 50.0]);
        drop(lua);

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { width = 100, max_height = 150, children = {
                rect { width = 10, height = 40 }, rect { width = 10, height = 40 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.height, 80.0, "under the cap the box is the content, as with no cap at all");
    }

    /// Spacing counts toward the content extent, because a gap advances the cursor by
    /// it. A bound computed without it would let the list scroll one gap short of its end.
    #[test]
    fn spacing_counts_toward_what_there_is_to_scroll() {
        let (_lua, _ys, used) = scrolled(
            r#"panel { id = "bar", child = column { width = 100, height = 100, spacing = 10, scroll = scroll("s"), children = {
                rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
            } } }"#,
            9_999.0,
        );
        assert_eq!(used, 110.0, "200 of children plus one 10px gap, less the 100 viewport");
    }

    #[test]
    fn a_row_scrolls_horizontally() {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return panel { id = "bar", child = row { width = 100, height = 50, scroll = scroll("s"), children = {
                    rect { width = 100, height = 10 }, rect { width = 100, height = 10 },
                } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(60.0));
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, -60.0, "a row takes the offset on x, not y");
        assert_eq!(row.children[0].rect.y, 0.0);
    }

    /// A wheel must not be able to write a capability snapshot. `scroll_handle` refuses every kind
    /// but its own, so binding the wrong signal scrolls nothing instead.
    /// Alignment and scrolling cannot both be in play; this pins why rather than trusting it.
    ///
    /// `spare` is `(content - total).max(0)` and the scroll limit is `(total - content).max(0)`, so
    /// one is zero whenever the other is not. Content that underfills its box aligns and cannot
    /// scroll; overflowing content has no spare to align with. A `Center` column with a scroll
    /// offset is therefore still centred, not centred-then-shifted.
    #[test]
    fn alignment_and_scrolling_are_mutually_exclusive_by_construction() {
        let (_lua, ys, used) = scrolled(
            r#"panel { id = "bar", child = column { width = 100, height = 300, align_v = "Center", scroll = scroll("s"), children = {
                rect { width = 10, height = 50 }, rect { width = 10, height = 50 },
            } } }"#,
            999.0,
        );
        assert_eq!(used, 0.0, "100 of content in a 300 box leaves nothing to scroll");
        assert_eq!(ys, vec![100.0, 150.0], "so the pair stays centred rather than being dragged off the top");
    }

    #[test]
    fn a_scroll_property_naming_something_that_is_not_a_scroll_signal_is_inert() {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return panel { id = "bar", child = column { width = 100, height = 100, scroll = state("s", 120), children = {
                    rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
                } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        let ys: Vec<f32> = column.children.iter().map(|c| c.rect.y).collect();
        assert_eq!(ys, vec![0.0, 100.0], "a `state()` signal holding 120 scrolls nothing");
    }

    /// A wheel over a container nothing but its `scroll` slot reads moves the kept children where
    /// a pass would put them, clamped, and schedules no pass. A readout of the offset owes one.
    #[test]
    fn a_wheel_scrolls_in_place_unless_lua_reads_the_offset() {
        let ys = |scene: &Scene| -> Vec<f32> {
            scene.surface("bar@TEST").unwrap().children[0].children[0].children.iter().map(|c| c.rect.y).collect()
        };
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        let dirty = crate::lua::signal::DirtyFlag::new();
        crate::lua::signal::register(&lua, dirty.clone()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"s = scroll("s")
                shown = state("shown", false)
                local tile = rect { width = 10, height = 100 }
                return panel { id = "bar", child = row { children = {
                    column { height = 100, padding = 5, spacing = 4, scroll = s, children = { tile, tile, tile } },
                    row { visible = shown, children = { text { content = s:map(function(v) return tostring(v) end) } } },
                } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let signal = crate::lua::signal::from_userdata(&lua.globals().get("s").unwrap()).unwrap();
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let pass = |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        pass(&mut scene);
        dirty.take();

        assert_eq!(scene.scroll_in_place(&signal, 150.0, &lua), Some(vec!["bar@TEST".to_string()]));
        assert_eq!(ys(&scene), [-145.0, -41.0, 63.0]);
        assert!(!dirty.take(), "no pass is scheduled");
        assert_eq!(scene.scroll_in_place(&signal, 900.0, &lua).map(|moved| moved.len()), Some(1));
        assert_eq!(signal.scroll_offset(), Some(218.0), "308 px of tiles in a 90 px viewport");
        let kept = ys(&scene);
        pass(&mut scene);
        assert_eq!(ys(&scene), kept, "a pass puts them where the wheel did");

        lua.load("shown:set(true)").exec().unwrap();
        pass(&mut scene);
        assert_eq!(scene.scroll_in_place(&signal, 0.0, &lua), None, "the readout owes a pass");
        assert_eq!(signal.scroll_offset(), Some(218.0), "and nothing is written");
    }
}

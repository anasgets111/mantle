//! One node's properties for a pass: resolved from its declaration, or kept from the last pass
//! when nothing that resolve read has been written since (ADR-0270).

use std::rc::Rc;
use std::time::Instant;

use mlua::{Lua, Value, WeakLua};

use super::{LayoutStyle, ResolvedNode};
use crate::layout::node::{self, LayoutError, PaintStyle, PropMap, Tween};
use crate::lua::signal::{self, CellId, ComputedFrame};

/// What a node's properties were last resolved from: its raw declaration, the write clock taken
/// before the resolve read anything, and every cell it read. With the same declaration, a resolve
/// can be skipped when those cells still hold after derived property outputs have settled.
///
/// Exact for signals and blind to anything else a getter reads: `os.time()`, an upvalue, a file.
/// That is the contract `docs/guide/signals.md` states.
#[derive(Clone)]
pub struct ResolveMemo {
    /// Kept to keep its values alive: tables, functions and signals compare by address, so a
    /// collected one cannot hand its address to a new one.
    raw: PropMap,
    /// Checked first: a value of a dropped VM panics on any read. A scene outlives its VM only in
    /// tests.
    lua: WeakLua,
    stamp: u64,
    cells: Vec<CellId>,
    /// A value was dropped: never kept, so every pass reports it until it is fixed.
    dropped: bool,
    /// No table property held a signal when `raw` was declared ([`node::tables_plain`]).
    tables_plain: bool,
}

/// One node's properties for this pass, displayed values and parse included, and the memo they
/// came from.
pub(super) struct Resolved {
    pub properties: Rc<PropMap>,
    pub style: LayoutStyle,
    pub paint: Option<PaintStyle>,
    pub tweens: Vec<Tween>,
    pub movement: Option<Box<node::MoveSpec>>,
    pub memo: Rc<ResolveMemo>,
}

/// `raw` resolved, `build` applied (a surface root's function `child`), and its tweens reconciled
/// against `retained`'s. Or, when `retained`'s memo still holds, `retained`'s own properties with
/// its tweens advanced to `now`, as a tick would, and no Lua run; its cells are noted as this
/// pass's reads so the next write to one still dirties the surface.
pub(super) fn resolve(
    kind: &'static str,
    raw: PropMap,
    mut retained: Option<&mut ResolvedNode>,
    now: Instant,
    lua: &Lua,
    build: impl FnOnce(PropMap) -> Result<PropMap, LayoutError>,
) -> Result<Resolved, LayoutError> {
    // The surface's read, not the node's: `finish` applies the offset to whatever it keeps, and a
    // wheel scrolls in place while no getter reads it (ADR-0274).
    if let Some(slot) = node::signal_at(&raw, "scroll").and_then(|signal| signal.cell_id()) {
        signal::note_reads(lua, &[slot]);
    }
    let same = retained
        .as_deref()
        .and_then(|r| r.resolve_memo.as_deref())
        .filter(|memo| memo.lua == lua.weak() && same_declaration(&memo.raw, &raw));
    let tables_plain = same.map_or_else(|| node::tables_plain(&raw), |memo| memo.tables_plain);
    let keep = if let Some(memo) = same.filter(|memo| !memo.dropped) {
        if signal::written_since(memo.stamp, &memo.cells) {
            node::settle_property_signals(&raw, kind, tables_plain, lua)?;
        }
        !signal::written_since(memo.stamp, &memo.cells)
    } else {
        false
    };
    if let Some(r) = retained.as_deref_mut()
        && keep
    {
        let memo = r.resolve_memo.take().expect("keep requires a resolve memo");
        signal::note_reads(lua, &memo.cells);
        let mut properties = std::mem::take(&mut r.properties);
        let mut tweens = std::mem::take(&mut r.tweens);
        // A resting tween moves nothing, so what the last pass or tick parsed still holds. A `text`
        // parses again: `finish` fitted its kept paint to last pass's box.
        let moving = tweens.iter().any(|tween| !tween.resting);
        // Only a moving tween writes the map, so a still node keeps sharing it with the rollback.
        if moving {
            node::advance(&mut tweens, Rc::make_mut(&mut properties), now, lua)?;
        }
        let injected = drop_injected_sizes(&mut properties, &memo.raw);
        let style = if moving || injected { LayoutStyle::parse(&properties)? } else { *r.layout_style };
        let paint = if moving || kind == "text" { node::paint_style(kind, &properties)? } else { r.paint.take() };
        return Ok(Resolved { properties, style, paint, tweens, movement: r.move_spec.take(), memo });
    }
    let stamp = signal::write_clock(lua);
    let frame = ComputedFrame::enter(lua);
    let mut properties = build(node::resolve_declared(raw.clone(), kind, tables_plain, lua)?)?;
    let dropped = drop_refused_values(kind, &mut properties, retained.as_deref().map(|r| &*r.properties), lua)?;
    let (tweens, movement) = node::retarget(kind, retained.as_deref().map(tween_state), &mut properties, now, lua)?;
    let properties = Rc::new(properties);
    let memo = Rc::new(ResolveMemo { raw, lua: lua.weak(), stamp, cells: frame.finish(), dropped, tables_plain });
    let paint = node::paint_style(kind, &properties)?;
    let style = LayoutStyle::parse(&properties)?;
    Ok(Resolved { properties, style, paint, tweens, movement: movement.map(Box::new), memo })
}

/// The values a pass dropped, each named down to its node as the walk returns. Present only
/// while a startup or dirty pass runs: a reload and `mantle check` refuse a bad value instead.
#[derive(Default)]
pub(super) struct DroppedValues(Vec<LayoutError>);

impl DroppedValues {
    pub(super) fn count(lua: &Lua) -> usize {
        lua.app_data_ref::<Self>().map_or(0, |dropped| dropped.0.len())
    }

    /// Names the child the values dropped since `from` sat in; whether there were any.
    pub(super) fn under_child(lua: &Lua, from: usize, here: impl Fn(LayoutError) -> LayoutError) -> bool {
        let Some(mut dropped) = lua.app_data_mut::<Self>() else { return false };
        let tail = dropped.0.split_off(from);
        let any = !tail.is_empty();
        dropped.0.extend(tail.into_iter().map(here));
        any
    }

    pub(super) fn take(lua: &Lua) -> Vec<LayoutError> {
        lua.app_data_mut::<Self>().map(|mut dropped| std::mem::take(&mut dropped.0)).unwrap_or_default()
    }
}

/// Drops each value a row of `kind` refuses, as CSS drops an invalid declaration: the node reads
/// the default and nothing animates toward the value. Errors where the row has no default, for
/// `child`/`children`, whose default would empty the node, and for `secure_submit`, whose default
/// would hand a password to `on_change`. A value the retained node holds already passed.
fn drop_refused_values(
    kind: &str,
    properties: &mut PropMap,
    retained: Option<&PropMap>,
    lua: &Lua,
) -> Result<bool, LayoutError> {
    if lua.app_data_ref::<DroppedValues>().is_none() {
        return Ok(false);
    }
    let bit = crate::lua::nodes::properties::kind_bit(kind).unwrap_or(0);
    // Sorted, so which values a report names comes from the config, not the hasher.
    let mut keys: Vec<&'static str> = properties.keys().copied().collect();
    keys.sort_unstable();
    let mut refused = Vec::new();
    for name in keys {
        let value = properties.get(name);
        if matches!(name, "child" | "children" | "secure_submit")
            || retained.is_some_and(|kept| kept.get(name) == value)
        {
            continue;
        }
        let rows = crate::lua::nodes::properties::properties()
            .filter(|row| row.kinds & bit != 0 && row.name == name && !row.raw && !row.refused);
        // Every row: a root's own row and the common one both parse its `width`.
        for row in rows {
            if let Err(err) = (row.check)(row, value) {
                if (row.check)(row, None).is_err() {
                    return Err(err);
                }
                refused.push((name, err));
                break;
            }
        }
    }
    let dropped = !refused.is_empty();
    for (name, err) in refused {
        properties.remove(name);
        lua.app_data_mut::<DroppedValues>().expect("checked above").0.push(err);
    }
    Ok(dropped)
}

/// Drops a `width`/`height` a layout injected for a content-sized axis (`pass::measure_content_sizes`)
/// and the declaration `raw` lacks, so the next solve measures the content again.
pub(super) fn drop_injected_sizes(properties: &mut Rc<PropMap>, raw: &PropMap) -> bool {
    let mut dropped = false;
    for key in ["width", "height"] {
        if properties.contains_key(key) && !raw.contains_key(key) {
            Rc::make_mut(properties).remove(key);
            dropped = true;
        }
    }
    dropped
}

/// What `node::retarget` reads off a retained node.
pub(super) fn tween_state(node: &ResolvedNode) -> (&[Tween], &PropMap) {
    (&node.tweens, &node.properties)
}

/// The same Lua values under the same keys: by address for what has one, by value otherwise.
fn same_declaration(kept: &PropMap, fresh: &PropMap) -> bool {
    kept.len() == fresh.len()
        && kept.iter().all(|(key, a)| {
            fresh.get(key).is_some_and(|b| match (a, b) {
                (Value::String(a), Value::String(b)) => a.as_bytes() == b.as_bytes(),
                (Value::Table(_) | Value::Function(_) | Value::UserData(_) | Value::Thread(_), _) => {
                    node::same_lua_value(a, b)
                }
                (a, b) => a == b,
            })
        })
}

impl ResolveMemo {
    /// The declaration this resolve read.
    pub(super) fn raw(&self) -> &PropMap {
        &self.raw
    }

    /// Whether this resolve read `cell`: a getter, a `:map` or a function `child` did.
    pub(super) fn read(&self, cell: CellId) -> bool {
        self.cells.contains(&cell)
    }
}

/// By hand: `WeakLua` has no `Debug`.
impl std::fmt::Debug for ResolveMemo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolveMemo").field("stamp", &self.stamp).field("cells", &self.cells).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{apply_at, full, instance_at, surface_from};
    use super::super::*;
    use crate::lua::signal::from_userdata;

    /// A `clock` text beside a `rect` whose `background` maps `accent`, counting the map's runs in
    /// `runs`, under `hover` and `scroll` slots a test can write.
    const CLOCK_BESIDE_A_CHIP: &str = r##"runs = 0
        clock = state("clock", "0")
        accent = state("accent", "#101010")
        over = hover("chip")
        offset = scroll("list")
        return panel { id = "bar", child = column { width = 100, height = 40, scroll = offset, children = {
            text { content = clock },
            rect { width = 10, height = 100, hover = over,
                background = accent:map(function(c) runs = runs + 1 return c end),
                border_color = over:map(function(on) return on and "#ffffff" or "#000000" end) },
        } } }"##;

    struct Fixture {
        lua: mlua::Lua,
        surface: VirtualNode,
        scene: Scene,
        shaping: ShapingHandle,
    }

    impl Fixture {
        fn new(source: &str) -> Self {
            let (lua, surface) = surface_from(source);
            let mut fixture = Self { lua, surface, scene: Scene::new(), shaping: ShapingHandle::spawn() };
            fixture.apply().unwrap();
            fixture
        }

        fn apply(&mut self) -> Result<(), LayoutError> {
            apply_at(&mut self.scene, std::slice::from_ref(&self.surface), full(), &self.shaping, &self.lua)
        }

        fn run(&mut self, lua: &str) {
            self.lua.load(lua).exec().unwrap();
            self.apply().unwrap();
        }

        fn runs(&self) -> i64 {
            self.lua.globals().get("runs").unwrap()
        }

        /// The `column`'s child at `index`.
        fn child(&self, index: usize) -> &ResolvedNode {
            &self.scene.surface("bar@TEST").unwrap().children[0].children[index]
        }

        fn signal(&self, name: &str) -> crate::lua::signal::Signal {
            from_userdata(&self.lua.globals().get::<mlua::AnyUserData>(name).unwrap()).unwrap()
        }
    }

    fn string(node: &ResolvedNode, property: &str) -> String {
        node.properties[property].as_string().unwrap().to_string_lossy()
    }

    #[test]
    fn a_write_a_node_never_read_runs_none_of_its_getters_and_one_it_read_runs_them_again() {
        let mut bar = Fixture::new(CLOCK_BESIDE_A_CHIP);
        assert_eq!(bar.runs(), 1);

        bar.run(r#"clock:set("1")"#);
        assert_eq!(string(bar.child(0), "content"), "1");
        assert_eq!(bar.runs(), 1, "the clock is not the chip's");

        bar.run(r##"accent:set("#202020")"##);
        assert_eq!(bar.runs(), 2);
        assert_eq!(string(bar.child(1), "background"), "#202020");
    }

    #[test]
    fn a_node_keeps_its_parse_when_a_mapped_value_stays_equal() {
        let mut bar = Fixture::new(
            r#"snapshot = state("snapshot", { width = 10, net = 0 })
            return panel { id = "bar", child = column { children = {
                rect { width = snapshot:map(function(s)
                    if s.net == "bad" then error("bad width") end
                    return s.width
                end):map(function(width) return width end), height = 10 },
            } } }"#,
        );
        let before = bar.child(0).resolve_memo.as_ref().unwrap().clone();
        bar.run(r#"snapshot:set({ width = 10, net = 2 })"#);
        assert!(std::rc::Rc::ptr_eq(&before, bar.child(0).resolve_memo.as_ref().unwrap()));
        bar.run(r#"snapshot:set({ width = 20, net = 2 })"#);
        assert_eq!(bar.child(0).rect.width, 20.0);
        bar.lua.load(r#"snapshot:set({ width = 20, net = "bad" })"#).exec().unwrap();
        assert!(bar.apply().unwrap_err().to_string().contains("bad width"));
        bar.run(r#"snapshot:set({ width = 20, net = 3 })"#);
        assert_eq!(bar.child(0).rect.width, 20.0);
    }

    /// `hover` and `scroll` are copied raw and read off the retained node, so a kept node has to
    /// hold the same slots and still count as their reader.
    #[test]
    fn a_hover_or_scroll_write_still_reaches_a_kept_node() {
        let mut bar = Fixture::new(CLOCK_BESIDE_A_CHIP);
        bar.run(r#"clock:set("1")"#);

        bar.signal("over").hover_handle().unwrap().set_changed(Value::Boolean(true));
        bar.apply().unwrap();
        assert_eq!(string(bar.child(1), "border_color"), "#ffffff");
        assert_eq!(bar.runs(), 2, "the node holding a `hover` slot counts as reading it");

        bar.signal("offset").scroll_handle().unwrap().set_changed(Value::Number(30.0));
        bar.apply().unwrap();
        assert_eq!(bar.child(0).rect.y, -30.0, "the kept column scrolls its children");
        assert_eq!(bar.runs(), 2, "the scroll is not the chip's");
    }

    /// A nested computed settles like a top-level one: its source's write re-resolves the node only
    /// when the output changed.
    #[test]
    fn a_nested_computed_resolves_again_only_when_its_output_changes() {
        let mut bar = Fixture::new(
            r#"gap = state("gap", 2)
            return panel { id = "bar", child = column { children = {
                rect { width = 10, height = 10, margin = { left = gap:map(function(g) return g // 2 * 2 end) } } } } }"#,
        );
        assert_eq!(bar.child(0).margin.left, 2.0);
        let before = bar.child(0).resolve_memo.as_ref().unwrap().clone();
        bar.run("gap:set(3)");
        assert!(std::rc::Rc::ptr_eq(&before, bar.child(0).resolve_memo.as_ref().unwrap()), "kept");
        bar.run("gap:set(4)");
        assert_eq!(bar.child(0).margin.left, 4.0);
    }

    /// A same-declaration re-resolve keeps the verdict that its tables held no signal, so a signal put
    /// into one in place is refused by the parser instead of read; a new declaration is scanned again.
    #[test]
    fn a_re_resolve_of_one_declaration_does_not_scan_its_tables_again() {
        let source = |margin: &str| {
            format!(
                r##"gap = state("gap", 7)
                accent = state("accent", "#101010")
                m = {{ left = 2 }}
                return panel {{ id = "bar", child = column {{ children = {{
                    rect {{ width = 10, height = 10, background = accent, margin = {margin} }} }} }} }}"##
            )
        };
        let mut bar = Fixture::new(&source("m"));
        assert_eq!(bar.child(0).margin.left, 2.0);
        bar.lua.load(r##"m.left = gap; accent:set("#303030")"##).exec().unwrap();
        let err = bar.apply().unwrap_err().to_string();
        assert!(err.contains("margin"), "the in-place signal is refused loudly: {err}");

        let mut bar = Fixture::new(&source("{ left = gap }"));
        assert_eq!(bar.child(0).margin.left, 7.0);
        bar.run(r##"accent:set("#303030")"##);
        assert_eq!(bar.child(0).margin.left, 7.0, "a new declaration with a signal resolves it");
    }

    /// A kept node keeps its parse too: the edge table's `__index` runs on the first pass only.
    #[test]
    fn a_kept_node_does_not_parse_its_properties_again() {
        let mut bar = Fixture::new(
            r#"reads = 0
            clock = state("clock", "0")
            local m = setmetatable({}, { __index = function() reads = reads + 1 return 2 end })
            return panel { id = "bar", child = column { children = {
                text { content = clock }, rect { width = 10, height = 10, margin = m } } } }"#,
        );
        let reads: i64 = bar.lua.globals().get("reads").unwrap();
        bar.run(r#"clock:set("1")"#);
        assert_eq!(bar.lua.globals().get::<i64>("reads").unwrap(), reads);
        assert_eq!(bar.child(1).margin.left, 2.0);
    }

    /// ADR-0270: a function `child` is part of its root's resolve, so it runs again only when a
    /// signal that resolve read changes.
    #[test]
    fn a_function_child_runs_again_only_when_a_signal_it_read_changes() {
        let mut bar = Fixture::new(
            r#"runs = 0
            clock = state("clock", "0")
            label = state("label", "a")
            return panel { id = "bar", child = function(output)
                runs = runs + 1
                return column { children = { text { content = clock }, text { content = label:get() } } }
            end }"#,
        );
        bar.run(r#"clock:set("1")"#);
        assert_eq!((bar.runs(), string(bar.child(0), "content")), (1, "1".into()));

        bar.run(r#"label:set("b")"#);
        assert_eq!((bar.runs(), string(bar.child(1), "content")), (2, "b".into()));
    }

    /// The rollback keeps the memo of the last good pass, which predates the write that broke the
    /// node, so the node resolves again rather than keeping what it had.
    #[test]
    fn a_node_whose_getter_failed_resolves_again_once_its_input_changes() {
        let mut bar = Fixture::new(
            r##"clock = state("clock", "0")
            accent = state("accent", "#101010")
            return panel { id = "bar", child = column { children = {
                text { content = clock },
                rect { width = 10, height = 10,
                    background = accent:map(function(c) assert(c ~= "bad", "bad accent") return c end) },
            } } }"##,
        );
        bar.lua.load(r#"accent:set("bad")"#).exec().unwrap();
        assert!(bar.apply().unwrap_err().to_string().contains("bad accent"));
        bar.lua.load(r#"clock:set("1")"#).exec().unwrap();
        assert!(bar.apply().is_err(), "an unrelated write does not hide the broken node");

        bar.run(r##"accent:set("#303030")"##);
        assert_eq!(string(bar.child(1), "background"), "#303030");
    }

    /// A pass serves one answer per computed (ADR-0157), so a node resolved after the first
    /// instance's scroll clamp still reads the pre-clamp offset. Its memo has to count that write,
    /// or the second output shows the stale offset until the next scroll.
    #[test]
    fn a_readout_served_a_value_from_before_a_mid_pass_write_resolves_again_next_pass() {
        let mut bar = Fixture::new(
            r#"offset = scroll("list")
            return panel { id = "bar", child = row { children = {
                column { width = 10, height = 40, scroll = offset, children = { rect { width = 10, height = 10 } } },
                text { content = offset:map(function(v) return tostring(math.floor(v)) end) },
            } } }"#,
        );
        let on = |output: &str| SurfaceInstance {
            output: output.into(),
            instance_id: format!("bar@{output}"),
            ..instance_at(&bar.surface, full())
        };
        let instances = [on("A"), on("B")];
        let apply = |bar: &mut Fixture| {
            bar.scene.apply(std::slice::from_ref(&bar.surface), &instances, &bar.shaping, &bar.lua).unwrap();
        };
        apply(&mut bar);
        // Past the end of 10 px of content in 40 px: the first instance's pass clamps it to 0.
        bar.signal("offset").scroll_handle().unwrap().set_changed(Value::Number(30.0));
        apply(&mut bar);
        apply(&mut bar);
        for output in ["A", "B"] {
            let text = &bar.scene.surface(&format!("bar@{output}")).unwrap().children[0].children[1];
            assert_eq!(string(text, "content"), "0", "on {output}");
        }
    }

    /// The memos a list and its items keep hold values of the VM that built them; a scene handed a
    /// new VM must not read them.
    #[test]
    fn a_scene_carried_to_a_new_vm_builds_again_rather_than_reading_the_old_ones_memos() {
        const LIST: &str = r#"return panel { id = "bar", child = list { source = { "a" },
            itemfn = function() return rect { width = 10, height = 10 } end } }"#;
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(LIST);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        drop(lua);
        let (lua, surface) = surface_from(LIST);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children[0].rect.width, 10.0);
    }

    /// A computed over a computed hands its cells up through the frame it runs in, so a write to
    /// the innermost cell still reaches the node.
    #[test]
    fn a_write_under_a_computed_of_a_computed_reaches_the_node() {
        let mut bar = Fixture::new(
            r##"accent = state("accent", "#101010")
            local inner = accent:map(function(c) return c end)
            return panel { id = "bar", child = column { children = {
                rect { width = 10, height = 10, background = inner:map(function(c) return c end) },
            } } }"##,
        );
        bar.run(r##"accent:set("#202020")"##);
        assert_eq!(string(bar.child(0), "background"), "#202020");
    }

    /// A hidden subtree reads nothing (ADR-0124); shown again, a child whose input was written
    /// meanwhile resolves again, and one whose input was not is kept.
    #[test]
    fn a_child_shown_again_resolves_only_if_its_input_was_written_while_hidden() {
        let mut bar = Fixture::new(
            r#"runs = 0
            shown = state("shown", true)
            label = state("label", "a")
            other = state("other", "x")
            return panel { id = "bar", child = column { children = {
                row { visible = shown, children = {
                    text { content = label },
                    text { content = other:map(function(v) runs = runs + 1 return v end) },
                } },
            } } }"#,
        );
        bar.run(r#"shown:set(false)"#);
        bar.run(r#"label:set("b")"#);
        bar.run(r#"shown:set(true)"#);
        let row = bar.child(0);
        assert_eq!(string(&row.children[0], "content"), "b");
        assert_eq!((string(&row.children[1], "content"), bar.runs()), ("x".into(), 1));
    }

    /// A signal bound in an item is read by the item's resolve (ADR-0293), so a write to one
    /// resolves the item again and it shows the new value.
    #[test]
    fn a_write_an_item_read_reaches_it_through_the_list() {
        let mut bar = Fixture::new(
            r##"accent = state("accent", "#101010")
            items = state("items", { "a", "b" })
            return panel { id = "bar", child = column { children = {
                list { source = items, itemfn = function(name)
                    return text { content = name, foreground = accent }
                end },
            } } }"##,
        );
        bar.run(r##"accent:set("#202020")"##);
        let list = bar.child(0);
        assert_eq!(string(&list.children[1], "foreground"), "#202020");

        bar.run(r#"items:set({ "c", "b" })"#);
        let list = bar.child(0);
        assert_eq!(
            (string(&list.children[0], "content"), string(&list.children[1], "content")),
            ("c".into(), "b".into())
        );
    }

    /// Paired by id, a node takes its own memo with it, so reordering keeps each one's answer.
    /// Paired by position, a sibling's removal hands a node a different declaration, which misses.
    #[test]
    fn reordered_or_removed_siblings_each_show_their_own_declaration() {
        let mut bar = Fixture::new(
            r#"swap = state("swap", false)
            local a = text { id = "a", content = "A" }
            local b = text { id = "b", content = "B" }
            return panel { id = "bar", child = column { children = swap:map(function(s)
                if s then return { b, a, text { content = "2" } } end
                return { a, b, text { content = "1" }, text { content = "2" } }
            end) } }"#,
        );
        bar.run(r#"swap:set(true)"#);
        let contents: Vec<String> = (0..3).map(|i| string(bar.child(i), "content")).collect();
        assert_eq!(contents, ["B", "A", "2"]);
    }

    /// A getter that writes a cell it read answers from before its own write, so the node resolves
    /// again next pass rather than keeping that answer.
    #[test]
    fn a_node_that_writes_what_it_read_resolves_again_until_it_settles() {
        let mut bar = Fixture::new(
            r#"n = state("n", 0)
            return panel { id = "bar", child = column { children = {
                text { content = n:map(function(v) if v < 2 then n:set(v + 1) end return tostring(v) end) },
            } } }"#,
        );
        for expected in ["1", "2", "2"] {
            bar.apply().unwrap();
            assert_eq!(string(bar.child(0), "content"), expected);
        }
    }

    /// A new evaluation may change what any getter reads without writing a cell.
    #[test]
    fn a_new_evaluation_resolves_every_node_again() {
        let mut bar = Fixture::new(CLOCK_BESIDE_A_CHIP);
        crate::lua::signal::begin_evaluation(&bar.lua);
        bar.apply().unwrap();
        assert_eq!(bar.runs(), 2);
    }
}

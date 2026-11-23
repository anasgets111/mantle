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
    raw: Rc<PropMap>,
    /// Checked first: a value of a dropped VM panics on any read. A scene outlives its VM only in
    /// tests.
    lua: WeakLua,
    stamp: u64,
    pub(super) cells: Rc<[CellId]>,
    /// A value was dropped: never kept, so every pass reports it until it is fixed.
    pub(super) dropped: bool,
    /// No table property held a signal when `raw` was declared ([`node::tables_plain`]).
    tables_plain: bool,
    /// `[width, height]` a signal in `raw` resolved to nothing: [`drop_injected_sizes`] treats them as undeclared.
    omitted: [bool; 2],
    /// What each resolve of `raw` reads: `raw`, or a `path`'s with its literal `commands` replaced
    /// by their parse ([`node::tweened`]) at its second resolve. ponytail: `path` `commands` only;
    /// `shape` and long `text` parse every literal on each resolve. Upgrade: a parse per row.
    read: Rc<PropMap>,
    /// `raw` was resolved again, which decided `read`: a declaration resolved once never pays for it.
    decided: bool,
}

/// One node's properties for this pass, displayed values and parse included, and the memo they
/// came from.
pub(super) struct Resolved {
    pub properties: Rc<PropMap>,
    pub style: Rc<LayoutStyle>,
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
    raw: Rc<PropMap>,
    mut retained: Option<&mut ResolvedNode>,
    now: Instant,
    lua: &Lua,
    reentry: bool,
    build: impl FnOnce(PropMap) -> Result<PropMap, LayoutError>,
) -> Result<Resolved, LayoutError> {
    // The surface's read, not the node's: `finish` applies the offset to whatever it keeps, and a
    // wheel scrolls in place while no getter reads it (ADR-0274).
    if let Some(slot) = node::signal_at(&raw, "scroll").and_then(|signal| signal.cell_id()) {
        signal::note_reads(lua, &[slot]);
    }
    let same = retained.as_deref().and_then(|r| r.resolve_memo.as_deref()).filter(|memo| memo.same_raw(&raw, lua));
    let (tables_plain, mut read) = same
        .map_or_else(|| (node::tables_plain(&raw), Rc::clone(&raw)), |memo| (memo.tables_plain, Rc::clone(&memo.read)));
    let (decide, decided) = (same.is_some_and(|memo| !memo.decided), same.is_some());
    let keep = if let Some(memo) = same.filter(|memo| !memo.dropped) {
        if signal::written_since(memo.stamp, &memo.cells) {
            node::settle_property_signals(&read, kind, tables_plain, lua)?;
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
        let mut tweens = std::mem::take(&mut r.tweens);
        // A resting tween moves nothing, so what the last pass or tick parsed still holds. A `text`
        // `finish` wrapped or cut parses again: its kept paint holds the fitted string (as a tick's).
        let moving = tweens.iter().any(|tween| !tween.resting);
        // Advanced in place and shared, not taken: `prepare` reads the retained node's flow axis.
        node::advance(&mut tweens, &mut r.properties, now, lua)?;
        let injected = drop_injected_sizes(&mut r.properties, &memo);
        let properties = Rc::clone(&r.properties);
        let style =
            if moving || injected { Rc::new(LayoutStyle::parse(&properties)?) } else { Rc::clone(&r.layout_style) };
        let refit = matches!(
            &r.paint,
            Some(PaintStyle::Text { wrap: node::Wrap::Word, .. } | PaintStyle::Text { elided: true, .. })
        );
        let paint = if moving || refit { node::paint_style(kind, &properties)? } else { r.paint.take() };
        return Ok(Resolved { properties, style, paint, tweens, movement: r.move_spec.take(), memo });
    }
    let stamp = signal::write_clock(lua);
    let frame = ComputedFrame::enter(lua);
    let mut properties = build(node::resolve_declared(&read, kind, tables_plain, lua)?)?;
    // A literal `commands` whose parse runs no Lua is checked before anything parses it (#157).
    let literal = raw.get("commands").filter(|literal| {
        decide && kind == "path" && matches!(literal, Value::Table(table) if node::plain_commands(table))
    });
    let (dropped, params) =
        drop_refused_values(kind, &mut properties, retained.as_deref().map(|r| &*r.properties), lua)?;
    let shown = retained.as_deref().map(|r| (r.tweens.as_slice(), &*r.properties));
    let (tweens, movement) = node::retarget(kind, shown, &mut properties, now, lua, reentry && shown.is_none())?;
    let omitted = ["width", "height"].map(|key| raw.contains_key(key) && !properties.contains_key(key));
    let cells = Rc::from(frame.finish());
    // A tween or sequence on `params` replaces the value the check parsed with its frame.
    let params = params.filter(|(read, _)| properties.get("params") == Some(read)).map(|(_, parsed)| parsed);
    let paint = node::paint_style_with(kind, &properties, params)?;
    // Paint's parse of that literal, still in the map (no signal, refusal or tween replaced it),
    // stands in for it for every reader, this declaration's later resolves included.
    if let (Some(literal), Some(PaintStyle::Path(path))) = (literal, &paint)
        && properties.get("commands") == Some(literal)
        && let Ok(parsed) = node::tweened(lua, &path.commands)
    {
        properties.insert("commands", parsed.clone());
        let mut copy = (*raw).clone();
        copy.insert("commands", parsed);
        read = Rc::new(copy);
    }
    // A declaration that resolved to itself shares the map the memo keeps rather than a copy.
    let properties = if same_declaration(&read, &properties) { Rc::clone(&read) } else { Rc::new(properties) };
    let memo =
        Rc::new(ResolveMemo { raw, read, lua: lua.weak(), stamp, cells, dropped, tables_plain, omitted, decided });
    let style = Rc::new(LayoutStyle::parse(&properties)?);
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

/// A `shader`'s `params` value and the check's parse of it.
type CheckedParams = (Value, Vec<node::ShaderParam>);

/// Drops each value a row of `kind` refuses, as CSS drops an invalid declaration: the node reads
/// the default and nothing animates toward the value. Errors where the row has no default, for
/// `child`/`children`, whose default would empty the node, and for `secure_submit`, whose default
/// would hand a password to `on_change`. A value the retained node holds already passed. Returns
/// whether it dropped any, and a `shader`'s `params` value with the check's parse, so paint reads it once.
fn drop_refused_values(
    kind: &str,
    properties: &mut PropMap,
    retained: Option<&PropMap>,
    lua: &Lua,
) -> Result<(bool, Option<CheckedParams>), LayoutError> {
    if lua.app_data_ref::<DroppedValues>().is_none() {
        return Ok((false, None));
    }
    let bit = crate::layout::node::properties::kind_bit(kind).unwrap_or(0);
    // Sorted, so which values a report names comes from the config, not the hasher.
    let mut keys: Vec<&'static str> = properties.keys().copied().collect();
    keys.sort_unstable();
    let mut refused = Vec::new();
    let mut params = None;
    for name in keys {
        let value = properties.get(name);
        if matches!(name, "child" | "children" | "secure_submit")
            || retained.is_some_and(|kept| kept.get(name) == value)
        {
            continue;
        }
        let rows = crate::layout::node::properties::rows_named(name)
            .iter()
            .filter(|row| row.kinds & bit != 0 && !row.raw && !row.refused);
        // Every row: a root's own row and the common one both parse its `width`.
        for row in rows {
            let checked = if name == "params" {
                node::fields::shader::params.read(properties).map(|read| params = value.cloned().map(|v| (v, read)))
            } else {
                (row.check)(row, value)
            };
            if let Err(err) = checked {
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
    Ok((dropped, params))
}

/// Drops a `width`/`height` a layout injected for a content-sized axis (`pass::measure_content_sizes`)
/// and the declaration `memo` lacks or resolved to nothing, so the next solve measures the content again.
pub(super) fn drop_injected_sizes(properties: &mut Rc<PropMap>, memo: &ResolveMemo) -> bool {
    let mut dropped = false;
    for (key, omitted) in ["width", "height"].into_iter().zip(memo.omitted) {
        if properties.contains_key(key) && (omitted || !memo.raw.contains_key(key)) {
            Rc::make_mut(properties).remove(key);
            dropped = true;
        }
    }
    dropped
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
    pub(super) fn raw(&self) -> &Rc<PropMap> {
        &self.raw
    }

    /// Whether this resolve read `cell`: a getter, a `:map` or a function `child` did.
    pub(super) fn read(&self, cell: CellId) -> bool {
        self.cells.contains(&cell)
    }

    pub(super) fn same_raw(&self, raw: &Rc<PropMap>, lua: &Lua) -> bool {
        self.lua == lua.weak() && (Rc::ptr_eq(&self.raw, raw) || same_declaration(&self.raw, raw))
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
    use super::super::tests::{applied, apply_at, full, instance_at, surface_from};
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

        fn with_dirty(source: &str) -> (Self, crate::lua::signal::DirtyFlag, mlua::Table) {
            let dirty = crate::lua::signal::DirtyFlag::new();
            let lua = crate::layout::node::scene_lua();
            crate::lua::signal::register(&lua, dirty.clone()).unwrap();
            let table: mlua::Table = lua.load(source).eval().unwrap();
            let mut fixture = Self {
                lua,
                surface: crate::lua::nodes::deserialize_lua_table(&table).unwrap(),
                scene: Scene::new(),
                shaping: ShapingHandle::spawn(),
            };
            fixture.apply().unwrap();
            let _ = dirty.take_scope(&fixture.lua);
            (fixture, dirty, table)
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

    /// #157: past the first, a bound write on a `path` of 200 literal commands neither walks them for
    /// signals nor parses them again: the first such resolve's parse is the literal.
    /// A signal inside a literal `commands`, or a getter behind a metatable, is still read.
    #[test]
    fn a_bound_write_keeps_a_paths_literal_commands_parse() {
        let mut bar = Fixture::new(
            r##"shift, y = state("shift", 0), state("y", 2)
            local moved = shift:map(function(x) return { x = x, y = 0 } end)
            commands = {}
            for i = 1, 200 do commands[i] = { op = i == 1 and "M" or "L", points = { i, 2 } } end
            local getter = { __index = function(_, k) if k == "points" then return { 1, y:get() } end end }
            return panel { id = "bar", child = column { children = {
                path { width = 300, height = 8, commands = commands, shift = moved },
                path { width = 300, height = 8, commands = { { op = "M", points = { 1, y } } } },
                path { width = 300, height = 8, commands = { setmetatable({ op = "M" }, getter) }, shift = moved } } } }"##,
        );
        let path = |bar: &Fixture, i| match &bar.child(i).paint {
            Some(node::PaintStyle::Path(path)) => (std::rc::Rc::clone(&path.commands), path.shift),
            other => panic!("a path paints, got {other:?}"),
        };
        bar.run("shift:set(1)");
        let (before, _) = path(&bar, 0);
        bar.run("commands[2].points[2] = 5; shift:set(3); y:set(4)");
        let (after, shift) = path(&bar, 0);
        assert_eq!(shift, (3.0, 0.0));
        assert!(std::rc::Rc::ptr_eq(&before, &after), "parsed once; the in-place edit is not seen");
        assert_eq!(path(&bar, 1).0.points, [1.0, 4.0], "a signal in a literal `commands` is read again");
        assert_eq!(path(&bar, 2).0.points, [1.0, 4.0], "so is a getter's");
    }

    /// #157: a literal `commands` whose getter fails fails the pass, as a malformed one does, even
    /// when a second read of the getter would pass; so do one holding itself, and one another getter
    /// breaks before paint reads it. 4096 commands sharing one bad command are checked in linear time.
    #[test]
    fn a_paths_failing_commands_fails_the_pass() {
        let (lua, surface) = surface_from(
            "local p, c, l = {}, {}, {} for i = 1, 4096 do p[i], c[i], l[i] = i, p, c end
            return panel { id = 'bar', child = path { width = 9, height = 9, commands = l } }",
        );
        let (mut scene, instance, shaping) = (Scene::new(), [instance_at(&surface, full())], ShapingHandle::spawn());
        for pass in 0..2 {
            let dropped = scene.apply_locked(std::slice::from_ref(&surface), &instance, &shaping, &lua, false, false);
            assert!(dropped.unwrap().unwrap().to_string().contains("commands[1]"), "pass {pass} drops it");
        }
        let getter = "{ __index = function(_, k) if k == 'points' then calls = calls + 1 assert(calls % 2 == 0) return { 1, 2 } end end }";
        for commands in [
            format!("{{ setmetatable({{ op = 'M' }}, {getter}) }}"),
            "{ { op = 'M', points = { 1 } } }".into(),
            "(function() local p = { 1, 2 } p[3] = p return { { op = 'M', points = p } } end)()".into(),
            "(function() local c = { op = 'M', points = { 1, 2 } } animate = { opacity = setmetatable({}, \
                { __index = function(_, k) c.points = { 9 } return k == 'duration' and 100 or nil end }) } return { c } end)()"
                .into(),
        ] {
            let (lua, surface) = surface_from(&format!(
                "calls = 0 return panel {{ id = 'bar', child = path {{ width = 9, height = 9, commands = {commands}, animate = animate }} }}"
            ));
            let err = apply_at(&mut Scene::new(), &[surface], full(), &ShapingHandle::spawn(), &lua).unwrap_err();
            assert!(err.to_string().contains("commands[1]"), "{commands}: {err}");
        }
    }

    /// #157: a node other than a `path` walks a literal past the scan's 256 tables on every resolve, as
    /// before, so a signal put into it in place is read when another bound value is written.
    #[test]
    fn a_large_literal_off_a_path_is_walked_again() {
        let mut bar = Fixture::new(
            r##"opacity, x = state("opacity", 1), state("x", 3)
            shape = { commands = {} }
            for i = 1, 140 do shape.commands[i] = { op = i == 1 and "M" or "L", points = { i % 40, i % 7 } } end
            shape.commands[141] = { op = "Z", points = {} }
            return panel { id = "bar", child = column { children = {
                rect { width = 40, height = 10, background = "#ffffff", shape = shape, opacity = opacity } } } }"##,
        );
        bar.run("shape.commands[2].points[1] = x; opacity:set(0.5)");
        assert_eq!(bar.child(0).properties["opacity"], mlua::Value::Number(0.5));
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
        const LIST: &str = r#"return panel { id = "bar", child = list { items = { "a" },
            each = function() return rect { width = 10, height = 10 } end } }"#;
        let (mut scene, lua, shaping, _surface) = applied(LIST);
        drop(lua);
        let (lua, surface) = surface_from(LIST);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children[0].rect.width, 10.0);
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
                list { items = items, each = function(name)
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

    const TEXT_FIT_AND_ANIMATE: &str = r##"tick = state("tick", "0")
        box_w = state("box_w", 300)
        fg = state("fg", "#000000")
        return panel { id = "bar", child = column { width = box_w, children = {
            text { content = tick },
            text { content = "plain label" },
            text { content = "fit-then-elide", width = "fill", elide = "end" },
            text { content = "alpha omega", width = "fill", wrap = "word" },
            text { content = { { text = "hi ", bold = true }, { text = "long tail here" } },
                width = "fill", wrap = "word", max_lines = 1, elide = "end" },
            text { content = "glow", foreground = fg, animate = { foreground = 100 } },
            text { content = "plain fill", width = "fill", text_align = "center" },
        } } }"##;

    #[test]
    fn kept_plain_text_reuses_paint_while_wrapped_elided_and_animated_text_refits() {
        let mut bar = Fixture::new(TEXT_FIT_AND_ANIMATE);
        let text_paint = |bar: &Fixture, i: usize| match &bar.child(i).paint {
            Some(PaintStyle::Text { content, runs, color, elided, .. }) => {
                (content.clone(), runs.clone(), *color, *elided)
            }
            other => panic!("expected text paint, got {other:?}"),
        };
        let (plain0, _, _, _) = text_paint(&bar, 1);
        let (elidable0, _, _, elided0) = text_paint(&bar, 2);
        assert!(!elided0);

        // Kept plain and non-elided texts keep the exact `Arc<str>` allocation.
        bar.run(r#"tick:set("1")"#);
        let (plain1, _, _, _) = text_paint(&bar, 1);
        let (elidable1, _, _, _) = text_paint(&bar, 2);
        assert!(std::sync::Arc::ptr_eq(&plain0, &plain1));
        assert!(std::sync::Arc::ptr_eq(&elidable0, &elidable1));

        // Shrinking the parent wraps and elides the kept texts; widening recovers full content and runs.
        bar.run("box_w:set(36)");
        let (cut, _, _, was_elided) = text_paint(&bar, 2);
        let (wrapped, _, _, wrap_elided) = text_paint(&bar, 3);
        let (capped, capped_runs, _, cap_elided) = text_paint(&bar, 4);
        assert!(was_elided && cut.ends_with('\u{2026}') && cut.as_ref() != "fit-then-elide");
        assert!(!wrap_elided && wrapped.contains('\n'), "word wrap without elide: {wrapped:?}");
        assert!(cap_elided && capped.ends_with('\u{2026}') && capped_runs.iter().any(|r| r.bold));
        // Paint measures a kept plain text's line against the box it gets now (#169).
        let fits = |bar: &Fixture| match &bar.child(6).paint {
            Some(PaintStyle::Text { widest_line, .. }) => *widest_line <= bar.child(6).rect.width,
            other => panic!("expected text paint, got {other:?}"),
        };
        assert!(!fits(&bar), "a shrunk box cuts the line");

        bar.run("box_w:set(400)");
        let (restored_elide, _, _, elided_now) = text_paint(&bar, 2);
        let (restored_wrap, _, _, _) = text_paint(&bar, 3);
        let (restored_cap, restored_runs, _, cap_elided_now) = text_paint(&bar, 4);
        assert_eq!((restored_elide.as_ref(), elided_now), ("fit-then-elide", false));
        assert_eq!(restored_wrap.as_ref(), "alpha omega");
        assert_eq!((restored_cap.as_ref(), cap_elided_now), ("hi long tail here", false));
        assert_eq!(restored_runs[0].range, 0..3);
        assert!(fits(&bar), "a widened box keeps the ink");

        // A moving tween on a kept plain text still re-parses its animated paint properties.
        let t0 = Instant::now();
        bar.lua.set_app_data(crate::lua::timer::SimClock(t0));
        bar.run(r##"fg:set("#ffffff")"##);
        bar.lua.set_app_data(crate::lua::timer::SimClock(t0 + Duration::from_millis(50)));
        bar.run(r#"tick:set("2")"#);
        let (_, _, mid, _) = text_paint(&bar, 5);
        assert!(mid.r > 0.1 && mid.r < 0.9, "kept moving text updates color: {mid:?}");
    }

    #[test]
    fn unchanged_declaration_shares_memo_rc_while_resolved_tweens_allocate_distinct() {
        let mut bar = Fixture::new(
            r##"w = state("w", 20)
            nested = state("nested", 4)
            return panel { id = "bar", child = column { width = 200, height = 200, children = {
                rect { width = 10, height = 10, background = "#101010" },
                rect { width = w, height = 10 },
                rect { width = 10, height = 10, margin = { left = nested } },
                rect { width = w, height = 10, animate = { width = 100 } },
                row { animate = { width = 100 }, children = { rect { width = w, height = 10 } } },
            } } }"##,
        );
        let shared_rect = bar.child(0);
        let shared_memo = shared_rect.resolve_memo.as_ref().unwrap();
        assert!(std::rc::Rc::ptr_eq(&shared_rect.properties, shared_memo.raw()));

        // Top-level signal, nested-table signal, and running tween each alter `properties` and
        // must not share `memo.raw()`.
        let sig_rect = bar.child(1);
        assert!(!std::rc::Rc::ptr_eq(&sig_rect.properties, sig_rect.resolve_memo.as_ref().unwrap().raw()));
        let nested_rect = bar.child(2);
        assert!(!std::rc::Rc::ptr_eq(&nested_rect.properties, nested_rect.resolve_memo.as_ref().unwrap().raw()));

        let t0 = Instant::now();
        bar.lua.set_app_data(crate::lua::timer::SimClock(t0));
        bar.run("w:set(60)");
        bar.lua.set_app_data(crate::lua::timer::SimClock(t0 + Duration::from_millis(50)));
        bar.apply().unwrap();
        let tweening = bar.child(3);
        assert!(!std::rc::Rc::ptr_eq(&tweening.properties, tweening.resolve_memo.as_ref().unwrap().raw()));

        // A content-sized `row` with no `width` in `raw` initially shares `memo.raw()`, then
        // `measure_content_sizes` calls `Rc::make_mut(&mut node.properties)` to inject `width`:
        // copy-on-write detaches `properties` without mutating `memo.raw()`.
        let measured = bar.child(4);
        let measured_memo = measured.resolve_memo.as_ref().unwrap();
        assert!(measured.properties.contains_key("width"));
        assert!(!measured_memo.raw().contains_key("width"), "Rc::make_mut must not mutate ResolveMemo.raw");
        assert!(!std::rc::Rc::ptr_eq(&measured.properties, measured_memo.raw()));
    }

    const DEEP_SKIPPED_SUBTREE: &str = r##"runs = 0
        tick, box_w, deep_w, deep_bg = state("tick", "0"), state("box_w", 200), state("deep_w", 10), state("deep_bg", "#101010")
        local doubled = deep_w:map(function(w) runs = runs + 1 return w * 2 end)
        return panel { id = "bar", child = column { width = box_w, children = {
            text { content = tick },
            row { width = "fill", children = { column { width = "fill", children = {
                rect { width = doubled, height = 10, background = deep_bg },
                rect { width = "fill", height = 10, background = "#202020", geometry = geometry("fill") },
            } } } },
        } } }"##;

    /// #155: after unrelated writes skip a deep subtree, read-tracking, isolated paint writes, and
    /// computed layout writes deep inside still fire, and parent resizes still update descendants.
    #[test]
    fn a_written_cell_deep_in_a_skipped_subtree_still_reaches_layout_and_paint() {
        let (mut bar, dirty, table) = Fixture::with_dirty(DEEP_SKIPPED_SUBTREE);
        fn deep(bar: &Fixture) -> &ResolvedNode {
            &bar.child(1).children[0].children[0]
        }
        let fill_w = |bar: &Fixture| bar.child(1).children[0].children[1].rect.width;
        let (stamp0, cells0, _) = bar.child(1).subtree.clone().expect("deep subtree is skippable");
        // Unrelated write skips the deep subtree (`Rc::ptr_eq` fails if `build_subtree` re-ran).
        bar.run(r#"tick:set("1")"#);
        let _ = dirty.take_scope(&bar.lua);
        let sub1 = bar.child(1).subtree.as_ref().unwrap();
        assert!(std::rc::Rc::ptr_eq(&cells0, &sub1.1) && sub1.0 > stamp0);
        assert_eq!((deep(&bar).rect.width, fill_w(&bar), bar.runs()), (20.0, 200.0, 1));
        // Parent resize still updates descendants while keeping the subtree skipped.
        bar.run("box_w:set(120)");
        let _ = dirty.take_scope(&bar.lua);
        assert!(std::rc::Rc::ptr_eq(&cells0, &bar.child(1).subtree.as_ref().unwrap().1));
        assert_eq!((fill_w(&bar), bar.runs()), (120.0, 1));
        // Isolated paint-only write deep inside dirties the instance and updates target color.
        let paint_before = crate::layout::paint::build(bar.scene.surface("bar@TEST").unwrap(), 1.0, None);
        let target_paint_before = deep(&bar).paint.clone();
        bar.lua.load(r##"deep_bg:set("#ff0000")"##).exec().unwrap();
        assert_eq!(dirty.take_scope(&bar.lua), crate::lua::signal::DirtyScope::Instances(vec!["bar@TEST".into()]));
        bar.apply().unwrap();
        assert_eq!(string(deep(&bar), "background"), "#ff0000");
        assert_ne!(deep(&bar).paint, target_paint_before);
        let paint_after = crate::layout::paint::build(bar.scene.surface("bar@TEST").unwrap(), 1.0, None);
        assert_ne!(paint_before, paint_after);
        assert!(!paint_after.damage_since(&paint_before, true).is_empty());
        // A second skip with a freshly deserialized equal declaration proves `same_raw`,
        // `ReadTracker`, and `stamp` stayed live.
        let root_memo = bar.scene.surface("bar@TEST").unwrap().resolve_memo.as_ref().unwrap().clone();
        let cells2 = bar.child(1).subtree.as_ref().unwrap().1.clone();
        bar.surface = crate::lua::nodes::deserialize_lua_table(&table).unwrap();
        bar.run(r#"tick:set("2")"#);
        let _ = dirty.take_scope(&bar.lua);
        assert!(std::rc::Rc::ptr_eq(&root_memo, bar.scene.surface("bar@TEST").unwrap().resolve_memo.as_ref().unwrap()));
        assert!(std::rc::Rc::ptr_eq(&cells2, &bar.child(1).subtree.as_ref().unwrap().1));
        assert_eq!(bar.runs(), 2);
        bar.lua.load("deep_w:set(45)").exec().unwrap();
        assert_eq!(dirty.take_scope(&bar.lua), crate::lua::signal::DirtyScope::Instances(vec!["bar@TEST".into()]));
        bar.apply().unwrap();
        assert_eq!((deep(&bar).rect.width, bar.runs()), (90.0, 4));
    }

    const WRAPPING_SKIPPED_SUBTREE: &str = r#"w = state("w", nil)
        return panel { id = "bar", child = column { children = {
            rect { width = w, children = {
                row { max_width = 100, height = 40, wrap = true, children = {
                    rect { width = "50%", height = 10 },
                    rect { width = "50%", height = 10 },
                    rect { width = 50, height = 10, geometry = geometry("wrap") },
                } },
            } },
        } } }"#;

    /// #155: a skipped wrapping container whose outer size holds still reads back moved descendants.
    #[test]
    fn a_skipped_wrapping_subtree_reads_back_moved_descendants_when_its_size_holds() {
        let mut bar = Fixture::new(WRAPPING_SKIPPED_SUBTREE);
        fn row(bar: &Fixture) -> &ResolvedNode {
            &bar.child(0).children[0]
        }
        let cells0 = row(&bar).subtree.as_ref().unwrap().1.clone();
        assert_eq!((row(&bar).rect.width, row(&bar).rect.height, row(&bar).children[2].rect.y), (50.0, 40.0, 0.0));
        bar.run("w:set(40)");
        assert!(std::rc::Rc::ptr_eq(&cells0, &row(&bar).subtree.as_ref().unwrap().1));
        assert_eq!((row(&bar).rect.width, row(&bar).rect.height, row(&bar).children[2].rect.y), (50.0, 40.0, 10.0));
    }

    const KEPT_LIST_ITEMS: &str = r#"a, b, c, items = state("a", "a1"), state("b", "b1"), state("c", "c1"), state("items", { 1 })
        return panel { id = "bar", child = column { children = {
            list { items = { 1, 2 }, each = function(i)
                return row { children = { text { content = i == 1 and a or b } } }
            end },
            list { items = items, each = function() return text { content = c } end },
        } } }"#;

    /// #155: `keep_item` and `prepare` list-item skips reuse subtrees while keeping reads live.
    #[test]
    fn a_kept_list_item_reuses_its_subtree_and_still_updates_when_written() {
        let (mut bar, dirty, _) = Fixture::with_dirty(KEPT_LIST_ITEMS);
        fn item(bar: &Fixture, list: usize, i: usize) -> &ResolvedNode {
            &bar.child(list).children[i]
        }
        let cells1 = item(&bar, 0, 1).subtree.as_ref().unwrap().1.clone();
        bar.run(r#"a:set("a2")"#);
        let _ = dirty.take_scope(&bar.lua);
        assert!(std::rc::Rc::ptr_eq(&cells1, &item(&bar, 0, 1).subtree.as_ref().unwrap().1));
        assert_eq!(string(&item(&bar, 0, 0).children[0], "content"), "a2");
        bar.lua.load(r#"b:set("b2")"#).exec().unwrap();
        assert_eq!(dirty.take_scope(&bar.lua), crate::lua::signal::DirtyScope::Instances(vec!["bar@TEST".into()]));
        bar.apply().unwrap();
        assert_eq!(string(&item(&bar, 0, 1).children[0], "content"), "b2");
        // List rebuild whose fresh leaf item matches the retained one hits `prepare`'s Kept path
        // and must still record its reads in `list_memo` (`memo.item_read`).
        let leaf_cells = item(&bar, 1, 0).subtree.as_ref().unwrap().1.clone();
        bar.run("items:set({ 1 })");
        let _ = dirty.take_scope(&bar.lua);
        assert!(std::rc::Rc::ptr_eq(&leaf_cells, &item(&bar, 1, 0).subtree.as_ref().unwrap().1));
        bar.lua.load(r#"c:set("c2")"#).exec().unwrap();
        assert_eq!(dirty.take_scope(&bar.lua), crate::lua::signal::DirtyScope::Instances(vec!["bar@TEST".into()]));
        bar.apply().unwrap();
        assert_eq!(string(item(&bar, 1, 0), "content"), "c2");
    }

    /// #167: every Kept pass matches a forced-Built pass in rects, published geometry, and display
    /// list, and passes after a rollback or font landing take the Built path.
    #[test]
    fn kept_subtrees_match_forced_built_across_writes_rollback_and_fonts() {
        use super::super::pass::tests::{KEPT_SUBTREES, with_forced_built};

        fn walk(node: &ResolvedNode, rects: &mut Vec<(&'static str, LogicalRect, bool)>, geoms: &mut Vec<[f32; 4]>) {
            rects.push((node.kind, node.rect, node.visible));
            if let Some((_, cell)) = node::signal_at(&node.properties, "geometry").and_then(|s| s.geometry_cell())
                && let Value::Table(t) = &*cell.borrow()
            {
                geoms.push(["x", "y", "width", "height"].map(|k| t.get::<f32>(k).unwrap()));
            }
            node.children.iter().for_each(|c| walk(c, rects, geoms));
        }
        fn assert_same(label: &str, kept: &Scene, built: &Scene) {
            let (k, b) = (kept.surface("bar@TEST").unwrap(), built.surface("bar@TEST").unwrap());
            assert!(k.subtree.is_none(), "{label}: surface root never builds a subtree memo");
            let ((mut kr, mut kg), (mut br, mut bg)) = ((Vec::new(), Vec::new()), (Vec::new(), Vec::new()));
            walk(k, &mut kr, &mut kg);
            walk(b, &mut br, &mut bg);
            assert_eq!(kr, br, "{label}: rects");
            assert_eq!(kg, bg, "{label}: published geometry");
            assert_eq!(
                crate::layout::paint::build(k, 1.0, None),
                crate::layout::paint::build(b, 1.0, None),
                "{label}: display list"
            );
        }
        let kept_in = |f: &mut dyn FnMut()| {
            KEPT_SUBTREES.set(0);
            f();
            KEPT_SUBTREES.get()
        };

        const NESTED_LIST: &str = r#"a, inner = state("a", "a1"), state("inner", "i1")
            return panel { id = "bar", child = list { items = { 1, 2 }, each = function(i)
                return i == 1 and text { content = a } or list { items = { "x" }, each = function(k)
                    return text { content = k .. inner:get() }
                end }
            end } }"#;

        let (t0, shaping) = (Instant::now(), ShapingHandle::spawn());
        let make = |src: &str, sh: &ShapingHandle, force: bool| {
            let (lua, surface) = surface_from(src);
            lua.set_app_data(crate::lua::timer::SimClock(t0));
            let mut f = Fixture { lua, surface, scene: Scene::new(), shaping: sh.clone() };
            if force {
                with_forced_built(|| f.apply().unwrap())
            } else {
                f.apply().unwrap()
            }
            f
        };
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "clock_chip",
                CLOCK_BESIDE_A_CHIP,
                &[r#"clock:set("1")"#, "offset:scroll_to(30)", r##"accent:set("#202020")"##],
            ),
            (
                "text_fit_animate",
                TEXT_FIT_AND_ANIMATE,
                &[r#"tick:set("1")"#, "box_w:set(36)", "box_w:set(400)", r##"fg:set("#ffffff")"##, r#"tick:set("2")"#],
            ),
            (
                "deep_skipped",
                DEEP_SKIPPED_SUBTREE,
                &[r#"tick:set("1")"#, "box_w:set(120)", r##"deep_bg:set("#ff0000")"##, "deep_w:set(45)"],
            ),
            ("wrapping_skipped", WRAPPING_SKIPPED_SUBTREE, &["w:set(40)"]),
            (
                "kept_list_items",
                KEPT_LIST_ITEMS,
                &[r#"a:set("a2")"#, r#"b:set("b2")"#, "items:set({ 1 })", r#"c:set("c2")"#],
            ),
            ("nested_unwindowed_list", NESTED_LIST, &[r#"a:set("a2")"#, r#"inner:set("i2")"#]),
        ];
        for &(name, src, steps) in cases {
            let (mut kept, mut built) = (make(src, &shaping, false), make(src, &shaping, true));
            assert_same(name, &kept.scene, &built.scene);
            let mut kept_total = 0;
            for (i, &script) in steps.iter().enumerate() {
                let at = t0 + Duration::from_millis((i as u64 + 1) * 50);
                kept.lua.set_app_data(crate::lua::timer::SimClock(at));
                built.lua.set_app_data(crate::lua::timer::SimClock(at));
                kept_total += kept_in(&mut || kept.run(script));
                assert_eq!(
                    kept_in(&mut || with_forced_built(|| built.run(script))),
                    0,
                    "{name}:{script}: forced Built"
                );
                assert_same(&format!("{name}:{script}"), &kept.scene, &built.scene);
            }
            assert!(kept_total > 0, "{name}: never hit the Kept path");
        }

        // Rollback drops solver trees, so the next pass takes the Built path; the one after keeps again.
        let (mut kept, built) =
            (make(DEEP_SKIPPED_SUBTREE, &shaping, false), make(DEEP_SKIPPED_SUBTREE, &shaping, true));
        let inst = [instance_at(&kept.surface, full())];
        kept.scene
            .inspect_then_roll_back(std::slice::from_ref(&kept.surface), &inst, &kept.shaping, &kept.lua, |_| {})
            .unwrap();
        assert_eq!(kept_in(&mut || kept.apply().unwrap()), 0, "pass after rollback must take Built path");
        assert_same("after_rollback", &kept.scene, &built.scene);
        assert!(kept_in(&mut || kept.apply().unwrap()) > 0, "pass after rebuilt solver tree keeps subtrees again");

        // A font landing drops solver trees, so the next pass takes the Built path.
        if let Some((font_shaping, open)) =
            crate::text::shaping::tests::gated_shots_fixture(crate::text::fonts::covering_file)
        {
            const GLYPH: &str = r#"return panel { id = "bar", child = row { children = { text { content = "a" }, text { content = "●" } } } }"#;
            let mut k = make(GLYPH, &font_shaping, false);
            assert!(kept_in(&mut || k.apply().unwrap()) > 0, "before font landing, subtree is kept");
            crate::text::shaping::tests::land(&font_shaping, open);
            assert_eq!(kept_in(&mut || k.apply().unwrap()), 0, "pass after font landing must take Built path");
            assert_same("after_font_landing", &k.scene, &make(GLYPH, &font_shaping, true).scene);
        }
    }
}

//! One node's properties for a pass: resolved from its declaration, or kept from the last pass
//! when nothing that resolve read has been written since (ADR-0270).

use std::rc::Rc;
use std::time::Instant;

use mlua::{Lua, Value, WeakLua};

use super::solver::{text_measure_matches, text_measure_tweening};
use super::{LayoutStyle, ResolvedNode};
use crate::layout::node::{self, LayoutError, PropMap, Tween};
use crate::lua::signal::{self, CellId, ComputedFrame};

/// What a node's properties were last resolved from: its raw declaration, the write clock taken
/// before the resolve read anything, and every cell it read. While the declaration is the same and
/// none of the cells has been written, a resolve now would answer what the last one did.
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
}

/// One node's properties for this pass, displayed values and parse included, and the memo they
/// came from.
pub(super) struct Resolved {
    pub properties: PropMap,
    pub style: LayoutStyle,
    pub tweens: Vec<Tween>,
    pub memo: Rc<ResolveMemo>,
    /// The retained measurement, when this `text` measures from what it measured from last pass.
    pub text_memo: Option<(Option<f32>, taffy::Size<f32>)>,
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
    if let Some(r) = retained.as_deref_mut()
        && let Some(memo) = r.resolve_memo.take_if(|memo| {
            memo.lua == lua.weak()
                && !signal::written_since(memo.stamp, &memo.cells)
                && same_declaration(&memo.raw, &raw)
        })
    {
        signal::note_reads(lua, &memo.cells);
        let text_memo = if text_measure_tweening(kind, &r.tweens) { None } else { r.text_memo };
        let mut properties = std::mem::take(&mut r.properties);
        let mut tweens = std::mem::take(&mut r.tweens);
        node::advance(&mut tweens, &mut properties, now, lua)?;
        let style = LayoutStyle::parse(&properties)?;
        return Ok(Resolved { properties, style, tweens, memo, text_memo });
    }
    let stamp = signal::write_clock(lua);
    let frame = ComputedFrame::enter(lua);
    let mut properties = build(node::resolve_properties(raw.clone(), kind, lua)?)?;
    let tweens = node::retarget(kind, retained.as_deref().map(tween_state), &mut properties, now, lua)?;
    let memo = Rc::new(ResolveMemo { raw, lua: lua.weak(), stamp, cells: frame.finish() });
    let text_memo = retained
        .filter(|r| kind == "text" && text_measure_matches(&properties, &r.properties))
        .and_then(|r| r.text_memo);
    let style = LayoutStyle::parse(&properties)?;
    Ok(Resolved { properties, style, tweens, memo, text_memo })
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
                    a.type_name() == b.type_name() && a.to_pointer() == b.to_pointer()
                }
                (a, b) => a == b,
            })
        })
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

    /// A new evaluation may change what any getter reads without writing a cell.
    #[test]
    fn a_new_evaluation_resolves_every_node_again() {
        let mut bar = Fixture::new(CLOCK_BESIDE_A_CHIP);
        crate::lua::signal::begin_evaluation(&bar.lua);
        bar.apply().unwrap();
        assert_eq!(bar.runs(), 2);
    }
}

use std::rc::Rc;

use mlua::{Lua, Value};
use rustc_hash::FxHashMap;

use super::CellId;
use super::budget::PassDeadline;

/// One `Computed`'s identity for [`EvaluationMemo`], counted rather than derived from an address.
/// The memo keeps nothing alive, and a pass builds and discards computeds constantly, so an
/// address freed by one would serve its value to the next computed allocated there (ADR-0170).
/// A counter cannot be recycled. `Signal::clone` copies the id because a clone is the same computed
/// with the same `func`, which is the one case that must share a memo entry.
pub(super) type MemoKey = u64;

/// Next unused [`MemoKey`]. `Relaxed` is enough: ids need only differ, and the Loader is one thread
/// (ADR-0039).
pub(super) fn next_computed_id() -> MemoKey {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(super) struct MemoEntry {
    value: Value,
    cells: Vec<CellId>,
}

/// Values already produced during the current outermost [`Signal::get_value`](super::Signal::get_value).
#[derive(Default)]
pub(super) struct MemoTable {
    pub(super) map: FxHashMap<MemoKey, MemoEntry>,
    depth: usize,
    pub(super) eval_stack: Vec<Vec<CellId>>,
    /// The write clock when the open pass began, the oldest write a value it serves can predate.
    pub(super) pass_opened: Option<u64>,
}

/// The cells read between `enter` and `finish`, including through a memo hit or a nested frame,
/// which hands its own up on close. A `computed` evaluation is one; a `list`'s build is another
/// (ADR-0269).
pub(crate) struct ComputedFrame<'lua>(&'lua Lua);

impl<'lua> ComputedFrame<'lua> {
    pub(crate) fn enter(lua: &'lua Lua) -> Self {
        EvaluationMemo::push_frame(lua);
        Self(lua)
    }

    pub(crate) fn finish(self) -> Vec<CellId> {
        let cells = EvaluationMemo::pop_frame(self.0);
        std::mem::forget(self);
        cells
    }
}

impl Drop for ComputedFrame<'_> {
    fn drop(&mut self) {
        EvaluationMemo::pop_frame(self.0);
    }
}

/// Maps cell reads to surface instances for targeted invalidation (ADR-0044).
#[derive(Default)]
pub(super) struct ReadTracker {
    active_instance: Option<Rc<str>>,
    pub(super) cell_readers: rustc_hash::FxHashMap<CellId, rustc_hash::FxHashSet<Rc<str>>>,
    instance_cells: rustc_hash::FxHashMap<Rc<str>, rustc_hash::FxHashSet<CellId>>,
}

/// Marks the beginning of an instance's layout resolution, clearing its prior reads.
pub(crate) fn begin_instance_resolve(lua: &Lua, instance_id: &str) {
    forget_instance(lua, instance_id);
    crate::lua::app_data_or_default::<ReadTracker>(lua).active_instance = Some(Rc::from(instance_id));
}

/// Closes the active instance layout resolution scope.
pub(crate) fn end_instance_resolve(lua: &Lua) {
    if let Some(mut tracker) = lua.app_data_mut::<ReadTracker>() {
        tracker.active_instance = None;
    }
}

/// Drops an instance and its cell associations when a surface is destroyed.
pub(crate) fn forget_instance(lua: &Lua, instance_id: &str) {
    if let Some(mut tracker) = lua.app_data_mut::<ReadTracker>() {
        let instance_rc: Rc<str> = Rc::from(instance_id);
        if let Some(old_cells) = tracker.instance_cells.remove(&instance_rc) {
            for cell_id in old_cells {
                if let std::collections::hash_map::Entry::Occupied(mut e) = tracker.cell_readers.entry(cell_id) {
                    e.get_mut().remove(&instance_rc);
                    if e.get().is_empty() {
                        e.remove();
                    }
                }
            }
        }
    }
}

/// Resets all tracked cell-instance mappings (e.g. after a failed pass or on config reload).
pub(crate) fn reset_read_tracker(lua: &Lua) {
    if let Some(mut tracker) = lua.app_data_mut::<ReadTracker>() {
        *tracker = ReadTracker::default();
    }
}

/// Records that the active instance (and any enclosing computed evaluation) read `cell_id`.
pub(crate) fn note_read(lua: &Lua, cell_id: CellId) {
    note_instance_reads(lua, &[cell_id]);
    EvaluationMemo::record_dependency(lua, cell_id);
}

/// [`note_read`] for every cell a skipped build read last time, so the instance and any enclosing
/// frame still depend on them.
pub(crate) fn note_reads(lua: &Lua, cells: &[CellId]) {
    note_instance_reads(lua, cells);
    if let Some(mut table) = lua.app_data_mut::<MemoTable>()
        && let Some(frame) = table.eval_stack.last_mut()
    {
        add_unique(frame, cells);
    }
}

/// Marks the enclosing frame as reading a clock: a `delay` holding a pending value or a `pulse`
/// whose window is open answers differently later with no cell written.
pub(super) fn note_unsettled(lua: &Lua) {
    EvaluationMemo::record_dependency(lua, UNSETTLED);
}

/// Stands for the clock in a read set; never allocated, and always written.
const UNSETTLED: CellId = CellId(0);

/// When each cell was last written, on one counter. Per thread rather than per `Lua`: a capability
/// push writes through a `LiveSignalHandle`, which holds no `Lua`, and cells are `Rc`s, so every
/// write lands on the thread that reads them. `CellId`s are process-unique, so two VMs on one
/// thread cannot collide.
#[derive(Default)]
struct WriteLog {
    clock: u64,
    /// Every cell counts as written at this tick: a new evaluation may have changed what a build
    /// reads without writing a cell.
    everything: u64,
    last: FxHashMap<CellId, u64>,
}

thread_local! {
    static WRITES: std::cell::RefCell<WriteLog> = std::cell::RefCell::new(WriteLog::default());
}

/// Stamps a write to `cell`, whether or not it dirties the scene.
pub(crate) fn note_write(cell: CellId) {
    WRITES.with_borrow_mut(|log| {
        log.clock += 1;
        let at = log.clock;
        log.last.insert(cell, at);
    });
}

/// Counts every cell as written now, and forgets the per-cell stamps this subsumes.
pub(crate) fn note_everything_written() {
    WRITES.with_borrow_mut(|log| {
        log.clock += 1;
        log.everything = log.clock;
        log.last.clear();
    });
}

/// The stamp a build takes before it reads anything. Inside a pass, the clock when the pass began:
/// the pass serves one answer per computed (ADR-0157), so a build may read a value from before a
/// write the pass itself made, a scroll clamp or a getter's `set`.
pub(crate) fn write_clock(lua: &Lua) -> u64 {
    let opened = lua.app_data_ref::<MemoTable>().and_then(|table| table.pass_opened);
    opened.unwrap_or_else(current_clock)
}

pub(super) fn current_clock() -> u64 {
    WRITES.with_borrow(|log| log.clock)
}

/// Whether any of `cells` was written after `stamp`.
pub(crate) fn written_since(stamp: u64, cells: &[CellId]) -> bool {
    WRITES.with_borrow(|log| {
        log.everything > stamp
            || cells.iter().any(|cell| *cell == UNSETTLED || log.last.get(cell).is_some_and(|at| *at > stamp))
    })
}

/// [`note_read`]'s instance half.
fn note_instance_reads(lua: &Lua, cells: &[CellId]) {
    if let Some(mut tracker) = lua.app_data_mut::<ReadTracker>()
        && let Some(instance_id) = tracker.active_instance.as_ref().map(Rc::clone)
    {
        for &cell_id in cells {
            tracker.cell_readers.entry(cell_id).or_default().insert(Rc::clone(&instance_id));
            tracker.instance_cells.entry(Rc::clone(&instance_id)).or_default().insert(cell_id);
        }
    }
}

/// Appends each of `cells` that `frame` does not hold yet.
fn add_unique(frame: &mut Vec<CellId>, cells: &[CellId]) {
    for &cell in cells {
        if !frame.contains(&cell) {
            frame.push(cell);
        }
    }
}

/// One evaluation's memo, closing ADR-0044 decision 3's ceiling: without it a shared dependency is
/// re-run once per path that reaches it: a filter read by every row's `background` directly and
/// through a computed runs twice per row, and a diamond of depth N evaluates its root 2^N times.
///
/// Scoped to one layout pass, never across them: between two passes a `state`/`Live` cell may have
/// changed, and nothing here observes that. Within the scope the memo also makes an impure closure
/// (`os.clock()`, `math.random`) answer consistently on every path instead of differing by which
/// dependency edge reached it.
///
/// [`LayoutPassBudget`](super::LayoutPassBudget) opens the table, so one pass is the scope whenever a pass is running
/// (ADR-0157): a shared computed answers once for the pass rather than once for
/// `background`, once for `border_color`, and once for the label colour of every row. Outside a
/// pass -- startup evaluation, a `capability::CapabilityHandle::notify_change` handler -- the
/// outermost `Computed` still owns it, which is what keeps a handler that `:set()`s between its
/// own `:get()`s observing its own writes.
///
/// The cost is that `layout::scene` writes two cells mid-pass, and a derived readout of either now
/// holds the value it had when the pass started rather than depending on where in the tree the
/// reader sits: the `Scroll` clamp ([`LiveSignalHandle::set_quiet`](super::LiveSignalHandle::set_quiet), whose own contract already
/// says a derived readout sees the clamp next pass) and the `geometry(name)` publish (whose move
/// schedules the follow-up pass `Scene::settle_geometry` runs). Both settle on the next pass, and
/// both were previously answered one way above the writer and another way below it.
pub(super) struct EvaluationMemo<'lua> {
    lua: &'lua Lua,
    owner: bool,
}

impl<'lua> EvaluationMemo<'lua> {
    pub(super) fn enter(lua: &'lua Lua) -> Self {
        let in_pass = lua.app_data_ref::<PassDeadline>().is_some_and(|slot| slot.0.is_some());
        let owner = if in_pass {
            false
        } else {
            let mut table = crate::lua::app_data_or_default::<MemoTable>(lua);
            let owner = table.depth == 0;
            table.depth += 1;
            owner
        };
        Self { lua, owner }
    }

    /// A value already produced, with its cells noted as read by the active instance and the
    /// enclosing frame. `None` outside an evaluation, which is the outermost `Computed`'s own
    /// first look.
    pub(super) fn get(lua: &Lua, key: MemoKey) -> Option<Value> {
        let mut table = lua.app_data_mut::<MemoTable>()?;
        let MemoTable { map, eval_stack, .. } = &mut *table;
        let entry = map.get(&key)?;
        note_instance_reads(lua, &entry.cells);
        if let Some(frame) = eval_stack.last_mut() {
            add_unique(frame, &entry.cells);
        }
        Some(entry.value.clone())
    }

    pub(super) fn insert(lua: &Lua, key: MemoKey, value: &Value, cells: Vec<CellId>) {
        if let Some(mut table) = lua.app_data_mut::<MemoTable>() {
            table.map.insert(key, MemoEntry { value: value.clone(), cells });
        }
    }

    /// Creates the table, so a frame opened outside any evaluation still collects its reads.
    fn push_frame(lua: &Lua) {
        crate::lua::app_data_or_default::<MemoTable>(lua).eval_stack.push(Vec::new());
    }

    fn pop_frame(lua: &Lua) -> Vec<CellId> {
        let Some(mut table) = lua.app_data_mut::<MemoTable>() else {
            return Vec::new();
        };
        let cells = table.eval_stack.pop().unwrap_or_default();
        if let Some(parent) = table.eval_stack.last_mut() {
            add_unique(parent, &cells);
        }
        cells
    }

    fn record_dependency(lua: &Lua, cell_id: CellId) {
        if let Some(mut table) = lua.app_data_mut::<MemoTable>()
            && let Some(frame) = table.eval_stack.last_mut()
            && !frame.contains(&cell_id)
        {
            frame.push(cell_id);
        }
    }
}

impl Drop for EvaluationMemo<'_> {
    fn drop(&mut self) {
        if let Ok(Some(mut table)) = self.lua.try_app_data_mut::<MemoTable>() {
            table.depth = table.depth.saturating_sub(1);
            if self.owner {
                table.map.clear();
                table.eval_stack.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{lua_with_signal, lua_with_state};
    use super::super::*;
    use super::*;

    /// A launcher shape that once spent its whole 5ms budget in the field:
    /// `results` filters every application, `web_shown` reads `results`, `effective_selected` reads
    /// both, and each row's `background` reads that. One read of `background` used to run the
    /// filter twice; the memo makes the second reach `results` a lookup.
    #[test]
    fn a_dependency_reached_by_two_paths_runs_once_per_evaluation() {
        let (lua, _dirty) = lua_with_state();
        let runs: bool = lua
            .load(
                r#"
                local runs = 0
                local query = state("q", "fi")
                local results = computed({ query }, function(text)
                    runs = runs + 1
                    return { text }
                end)
                local web_shown = computed({ results }, function(found) return #found == 0 end)
                local selected = computed({ results, web_shown }, function(found, web)
                    return (web and "web") or found[1]
                end)
                local background = computed({ selected }, function(id) return id end)
                background:get()
                return runs == 1
                "#,
            )
            .eval()
            .unwrap();
        assert!(runs, "`results` is reached by two paths and has to run once, not twice");
    }

    /// The memo is one evaluation wide, not a cache. A `state` written between two `get`s has to
    /// show through, or a config would read its own writes stale.
    #[test]
    fn a_write_between_two_gets_is_not_served_from_the_previous_evaluation() {
        let (lua, _dirty) = lua_with_state();
        let (before, after): (i64, i64) = lua
            .load(
                r#"
                local n = state("n", 1)
                local doubled = computed({ n }, function(v) return v * 2 end)
                local before = doubled:get()
                n:set(21)
                return before, doubled:get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!((before, after), (2, 42));
    }

    /// ADR-0157. `node::resolve_properties` reads one property at a time, so before the pass owned
    /// the memo this shared computed ran once for every property of every node that reached it --
    /// measured live at 1.35ms of CPU for a single cold getter, against a 5ms cap.
    #[test]
    fn a_computed_read_by_two_properties_in_one_pass_runs_its_body_once() {
        let (lua, _dirty) = lua_with_state();
        let runs: i64 = {
            let _pass = LayoutPassBudget::enter(&lua).expect("a pass budget");
            lua.load(
                r#"
                runs = 0
                local n = state("n", 1)
                shared = computed({ n }, function(v) runs = runs + 1 return v end)
                "#,
            )
            .exec()
            .unwrap();
            // Two reads the way two properties of two nodes reach one signal, not one Lua chunk:
            // each is its own outermost `get_value`, which is what used to open its own memo.
            for _ in 0..4 {
                lua.load("shared:get()").exec().unwrap();
            }
            lua.globals().get("runs").unwrap()
        };
        assert_eq!(runs, 1, "four outermost reads inside one pass are one evaluation");
    }

    /// The pass is the scope, not a cache across passes: the next pass has to see a `state` written
    /// since, or a config would read its own writes one frame stale forever.
    #[test]
    fn the_next_pass_evaluates_again_rather_than_serving_the_last_ones_answer() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            runs = 0
            n = state("n", 1)
            shared = computed({ n }, function(v) runs = runs + 1 return v end)
            "#,
        )
        .exec()
        .unwrap();

        let first: i64 = {
            let _pass = LayoutPassBudget::enter(&lua).expect("a pass budget");
            lua.load("shared:get() shared:get()").exec().unwrap();
            lua.globals().get("runs").unwrap()
        };
        lua.load("n:set(21)").exec().unwrap();
        let second: i64 = {
            let _pass = LayoutPassBudget::enter(&lua).expect("a pass budget");
            lua.load("return shared:get()").eval().unwrap()
        };

        assert_eq!(first, 1, "the first pass evaluates once");
        assert_eq!(second, 21, "the second pass sees the write, rather than the memo from the first");
    }

    /// ADR-0157's cost, stated as a test. `layout::scene` clamps a `Scroll` cell mid-pass with
    /// `set_quiet`, and a derived readout evaluated before the clamp now keeps the pre-clamp answer
    /// for the whole pass instead of depending on where in the tree the reader sits.
    /// `LiveSignalHandle::set_quiet`'s own contract already says the clamp lands on a derived
    /// readout next pass; this is what makes that true of every reader rather than some.
    #[test]
    fn a_cell_written_mid_pass_reaches_a_derived_readout_on_the_next_pass_not_this_one() {
        let (lua, _dirty) = lua_with_state();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let scroll = from_userdata(&signal).unwrap();
        lua.globals().set("offset", signal.clone()).unwrap();
        lua.load(r#"readout = computed({ offset }, function(v) return v end)"#).exec().unwrap();

        let during: f64 = {
            let _pass = LayoutPassBudget::enter(&lua).expect("a pass budget");
            lua.load("return readout:get()").eval::<f64>().unwrap();
            // The clamp: derived from geometry this pass measured, so it does not dirty the scene.
            scroll.scroll_handle().unwrap().set_quiet(Value::Number(120.0));
            lua.load("return readout:get()").eval().unwrap()
        };
        let next: f64 = {
            let _pass = LayoutPassBudget::enter(&lua).expect("a pass budget");
            lua.load("return readout:get()").eval().unwrap()
        };

        assert_eq!(during, 0.0, "the reader that came first sets the pass's answer, wherever it sits in the tree");
        assert_eq!(next, 120.0, "and the clamp lands on the next pass, which is what `set_quiet` promises");
    }

    /// The memo was keyed on the address of a computed's dependency vector, and a computed dropped
    /// mid-pass hands that address straight back to the allocator. This is the shape that broke the
    /// lock screen on 2026-09-08: the second computed cannot return an integer, and with the
    /// address as the key it returned the first one's `0`.
    #[test]
    fn a_computed_built_where_a_dead_one_stood_gets_its_own_value() {
        let (lua, _dirty) = lua_with_state();
        let _pass = LayoutPassBudget::enter(&lua).expect("a pass budget");
        let answer: String = lua
            .load(
                r#"
                local leaf = state("leaf", 1)
                local doomed = computed({ leaf }, function() return 0 end)
                doomed:get()
                doomed = nil
                collectgarbage("collect")
                collectgarbage("collect")
                local fresh = computed({ leaf }, function() return "mine" end)
                return fresh:get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(answer, "mine", "a recycled address must not carry a memo entry with it");
    }

    /// A wide diamond must not spend nesting levels on cache hits: the memo is checked before
    /// `CpuBudget::enter`, so 40 readers of one dependency stay under `MAX_SIGNAL_NESTING_DEPTH`
    /// even though 40 > 32.
    #[test]
    fn cache_hits_do_not_consume_signal_nesting_depth() {
        let (lua, _dirty) = lua_with_state();
        let total: i64 = lua
            .load(
                r#"
                local leaf = state("leaf", 1)
                local shared = computed({ leaf }, function(v) return v end)
                local deps = {}
                for _ = 1, 40 do deps[#deps + 1] = shared end
                local wide = computed(deps, function(...)
                    local sum = 0
                    for _, v in ipairs({ ... }) do sum = sum + v end
                    return sum
                end)
                return wide:get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(total, 40);
    }

    #[test]
    fn a_diamond_dependency_graph_costs_one_call_per_level_not_two_to_the_level() {
        // History, because this assertion inverted twice. 20 diamond levels first made 2^20-1 calls
        // and returned `Ok(1048576)` in 1.9s; budgeting resolution then cut it off around 3,200
        // calls, and this test asserted the cut-off. [`EvaluationMemo`] removes the blow-up
        // instead: the second edge into each level is a lookup, so the graph is 20 calls, finishes,
        // and still answers 2^20 -- the same number the slow version reached the long way.
        //
        // Counts calls rather than timing the evaluation. A wall-clock bound flakes under the
        // parallel suite for the reason the next test's own comment records: a descheduled thread
        // blows the deadline without doing any more work. The call count is what the memo actually
        // changes -- 20 against 2^20-1, five orders of magnitude apart -- so it fails the blow-up
        // this exists to catch on any machine, at any load.
        let lua = lua_with_signal("a", Value::Integer(1));
        let calls = Rc::new(Cell::new(0u32));
        let counter = Rc::clone(&calls);
        lua.globals()
            .set(
                "add",
                lua.create_function(move |_, (x, y): (i64, i64)| {
                    counter.set(counter.get() + 1);
                    Ok(x + y)
                })
                .unwrap(),
            )
            .unwrap();
        lua.load("for _ = 1, 20 do a = computed({a, a}, add) end").exec().unwrap();

        let result: mlua::Result<i64> = lua.load("return a:get()").eval();

        assert_eq!(result.unwrap(), 1_048_576, "a shared dependency must still be summed once per edge");
        assert_eq!(calls.get(), 20, "one call per level; the un-memoized graph would make 2^20-1");
    }

    #[test]
    fn computed_memo_hit_records_dependencies_for_subsequent_surface_readers() {
        let (lua, dirty) = lua_with_state();
        lua.load(
            r#"
            q = state("q", "hello")
            c = computed({q}, function(text) return text .. " world" end)
            "#,
        )
        .exec()
        .unwrap();

        {
            let _pass = LayoutPassBudget::enter(&lua).unwrap();
            begin_instance_resolve(&lua, "surface_1");
            let v1: String = lua.load("return c:get()").eval().unwrap();
            assert_eq!(v1, "hello world");
            end_instance_resolve(&lua);

            begin_instance_resolve(&lua, "surface_2");
            let v2: String = lua.load("return c:get()").eval().unwrap();
            assert_eq!(v2, "hello world");
            end_instance_resolve(&lua);
        }

        lua.load("q:set('bye')").exec().unwrap();
        match dirty.take_scope(&lua) {
            DirtyScope::Instances(mut instances) => {
                instances.sort();
                assert_eq!(instances, vec!["surface_1".to_string(), "surface_2".to_string()]);
            }
            other => panic!("expected DirtyScope::Instances with both surfaces, got {other:?}"),
        }
    }
}

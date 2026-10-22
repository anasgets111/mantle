//! `s:on_change(fn(current, previous))` on named `state` and on a `:map` or `computed`. A write
//! only queues the change, and a derived signal is read again only in the drain, which runs before
//! the next layout pass, so no handler runs inside its writer's stack or a map's body.

use mlua::{AnyUserData, Function, Lua, Value};
use rustc_hash::FxHashMap;
use shared::error;

use super::{CellId, CpuBudget, FUNCTION_SLOT, Signal, SignalKind, same_value, tracking};
use crate::lua::location::{Site, report_raised};
use crate::lua::warn_raised;

/// Rounds per [`run`] (ADR-0288). A handler's write queues the next round, so a chain takes one
/// round per link; eight is deeper than a hand-written chain, and a loop stops after eight rounds
/// of handler budgets instead of hanging the turn.
const MAX_ROUNDS: usize = 8;

/// Handlers by state cell, and the writes since the last [`run`]: each written cell once, with
/// the value it held before its first write.
#[derive(Default)]
struct StateHandlers {
    handlers: FxHashMap<CellId, Vec<Function>>,
    pending: Vec<(CellId, Signal, Value)>,
    derived: Vec<Watched>,
}

/// A change [`run`] calls handlers for: their label, the handlers, `current` and `previous`.
type Change = (String, Vec<Function>, Value, Value);

/// A `:map` or `computed` with handlers. Held strongly: the computed registry is weak, and a
/// collected computed's [`tracking`] entry goes with it, leaving nothing to say it was written.
struct Watched {
    signal: AnyUserData,
    cell: CellId,
    name: String,
    /// The write clock before the last read; `None` after a raise, so every drain reads again.
    stamp: Option<u64>,
    /// `None` until a read succeeds; the first success is the baseline and fires nothing.
    last: Option<Value>,
    handlers: Vec<Function>,
}

impl Watched {
    fn new(lua: &Lua, signal: AnyUserData, cell: CellId, site: Option<Site>, handler: Function) -> Self {
        let name = site.map_or_else(|| "a derived signal".to_string(), |site| format!("the signal created at {site}"));
        let mut watched = Self { signal, cell, name, stamp: None, last: None, handlers: vec![handler] };
        watched.last = watched.read(lua);
        watched
    }

    /// The current value, or `None` after logging a raise; [`report_raised`] folds repeats.
    fn read(&mut self, lua: &Lua) -> Option<Value> {
        let stamp = tracking::write_clock(lua);
        match super::read(lua, &self.signal) {
            Ok(value) => {
                self.stamp = Some(stamp);
                Some(value)
            }
            Err(err) => {
                self.stamp = None;
                if let Ok(func) = self.signal.nth_user_value::<Function>(FUNCTION_SLOT) {
                    report_raised(&func, format!("{}, read for on_change,", self.name), &err);
                }
                None
            }
        }
    }
}

/// `s:on_change(f)` on a state or, reading its baseline and firing nothing, a `:map` or `computed`.
/// ponytail: none can be removed, so a call inside a callback adds one per run until [`clear`];
/// upgrade path: skip a handler already registered.
pub(super) fn add(lua: &Lua, signal: &AnyUserData, handler: Function) -> mlua::Result<()> {
    let (cell, site) = match &signal.borrow::<Signal>()?.0 {
        SignalKind::State { id, .. } => {
            crate::lua::app_data_or_default::<StateHandlers>(lua).handlers.entry(*id).or_default().push(handler);
            return Ok(());
        }
        SignalKind::Computed { out, site, .. } => (out.cell, *site),
        _ => return Ok(()),
    };
    if let Some(watched) =
        crate::lua::app_data_or_default::<StateHandlers>(lua).derived.iter_mut().find(|watched| watched.cell == cell)
    {
        watched.handlers.push(handler);
        return Ok(());
    }
    let watched = Watched::new(lua, signal.clone(), cell, site, handler);
    crate::lua::app_data_or_default::<StateHandlers>(lua).derived.push(watched);
    Ok(())
}

/// One round's changes: each queued state whose value differs from before its first write, read at
/// its turn so it sees an earlier handler's write to it, then the watched signals that changed.
fn changes(lua: &Lua) -> impl Iterator<Item = Change> + '_ {
    let pending = lua.app_data_mut::<StateHandlers>().map(|mut registry| std::mem::take(&mut registry.pending));
    let derived = changed_derived(lua);
    let states = pending.unwrap_or_default().into_iter().filter_map(|(id, signal, previous)| {
        let current = signal.get_value(lua).ok().filter(|current| !same_value(current, &previous))?;
        let handlers = lua.app_data_ref::<StateHandlers>()?.handlers.get(&id)?.clone();
        Some((format!("state({:?})", super::globals::state_name(lua, id)), handlers, current, previous))
    });
    states.chain(derived)
}

/// Reads each watched signal again whose cell was written since its last read, and returns those
/// whose value differs from the last, adopting the new value as the baseline.
fn changed_derived(lua: &Lua) -> Vec<Change> {
    // Taken out: a read runs Lua, which may register another watcher.
    let Some(mut watched) =
        lua.app_data_mut::<StateHandlers>().map(|mut registry| std::mem::take(&mut registry.derived))
    else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    for watched in &mut watched {
        if watched.stamp.is_some_and(|stamp| !tracking::written_since(stamp, &[watched.cell])) {
            continue;
        }
        let Some(current) = watched.read(lua) else { continue };
        if let Some(previous) = watched.last.replace(current.clone()).filter(|previous| !same_value(&current, previous))
        {
            changed.push((watched.name.clone(), watched.handlers.clone(), current, previous));
        }
    }
    let mut registry = crate::lua::app_data_or_default::<StateHandlers>(lua);
    watched.append(&mut registry.derived);
    registry.derived = watched;
    changed
}

/// Queues `signal`'s write for [`run`] when it has a handler. A cell already queued keeps its
/// first previous value, so `run` compares against what the handlers last saw.
pub(super) fn note_write(lua: &Lua, signal: &Signal, previous: Value) {
    let Some(id) = signal.cell_id() else { return };
    let Some(mut registry) = lua.app_data_mut::<StateHandlers>() else { return };
    if registry.handlers.contains_key(&id) && !registry.pending.iter().any(|(queued, ..)| *queued == id) {
        registry.pending.push((id, signal.clone(), previous));
    }
}

/// Runs the handlers of every state written since the last call whose value differs from before
/// its first write, and of every watched derived signal whose value differs from the last read,
/// each under its own CPU budget. A raising handler is logged; the value stays. Called once per
/// turn before the layout pass, like a capability push's `on_change`.
pub fn run(lua: &Lua) {
    for _ in 0..MAX_ROUNDS {
        let mut round = changes(lua).peekable();
        if round.peek().is_none() {
            return;
        }
        for (name, handlers, current, previous) in round {
            for handler in handlers {
                warn_raised(
                    &handler,
                    CpuBudget::call(lua, &handler, (current.clone(), previous.clone())),
                    format_args!("{name}:on_change handler"),
                );
            }
        }
    }
    // A change left at the cap is dropped: a state's queue empties, a derived adopts its value.
    let left: Vec<String> = changes(lua).map(|(name, ..)| name).collect();
    if !left.is_empty() {
        error!(
            "on_change handlers were still changing signals after {MAX_ROUNDS} rounds, so the changes to {} run no handler; one handler's write probably undoes another's",
            left.join(", ")
        );
    }
}

/// Before each evaluation and after a failed one, with the capability handlers: the next
/// evaluation registers its own, and a queued write belongs to the handlers it drops.
pub fn clear(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<StateHandlers>() {
        registry.handlers.clear();
        registry.pending.clear();
        registry.derived.clear();
    }
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    use super::super::tests::lua_with_state;
    use super::super::{begin_evaluation, promote_states, write_state};
    use super::*;

    fn count(lua: &Lua, name: &str) -> i64 {
        lua.globals().get::<i64>(name).unwrap()
    }

    #[test]
    fn a_lua_write_runs_the_handler_after_it_with_the_current_and_previous_value() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            s = state("count", 1)
            seen = {}
            s:on_change(function(now, before) seen[#seen + 1] = before .. "->" .. now .. "@" .. s:get() end)
            s:set(2)
            during = #seen
            "#,
        )
        .exec()
        .unwrap();
        assert_eq!(count(&lua, "during"), 0, "the writer's stack runs no handler");

        run(&lua);

        let seen: Vec<String> = lua.load("return seen").eval().unwrap();
        assert_eq!(seen, ["1->2@2"]);
    }

    #[test]
    fn a_socket_write_runs_the_handler() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"open = state("open", false) seen = "" open:on_change(function(now) seen = tostring(now) end)"#)
            .exec()
            .unwrap();
        promote_states(&lua);

        write_state(&lua, &shared::SetState { name: "open".into(), write: shared::StateWrite::Toggle }).unwrap();
        run(&lua);

        assert_eq!(lua.globals().get::<String>("seen").unwrap(), "true");
    }

    #[test]
    fn a_write_that_changes_nothing_runs_nothing() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            s = state("kind", "none")
            t = state("tags", { "a" })
            runs = 0
            s:on_change(function() runs = runs + 1 end)
            t:on_change(function() runs = runs + 1 end)
            s:set("none")
            t:set({ "a" })
            s:set("launcher") s:set("none")
            "#,
        )
        .exec()
        .unwrap();
        promote_states(&lua);
        let to = shared::StateWrite::Set(serde_json::json!("none"));
        write_state(&lua, &shared::SetState { name: "kind".into(), write: to }).unwrap();

        run(&lua);

        assert_eq!(count(&lua, "runs"), 0, "an equal write, and a change undone before the queue ran, are no change");
    }

    #[test]
    fn declaring_or_redeclaring_a_state_runs_nothing() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"runs = 0 state("open", false):on_change(function() runs = runs + 1 end)"#).exec().unwrap();
        run(&lua);
        begin_evaluation(&lua);
        lua.load(r#"state("open", true)"#).exec().unwrap();
        run(&lua);
        assert_eq!(count(&lua, "runs"), 0, "a reseed from an edited initial is a declaration, not a write");
    }

    #[test]
    fn a_handler_that_writes_state_runs_that_states_handlers_in_the_same_drain() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            open = state("open", true)
            query = state("query", "fire")
            cleared = false
            open:on_change(function(now) if not now then query:set("") end end)
            query:on_change(function(now) cleared = now == "" end)
            open:set(false)
            "#,
        )
        .exec()
        .unwrap();

        run(&lua);

        assert!(lua.globals().get::<bool>("cleared").unwrap());
    }

    #[test]
    fn a_state_written_by_an_earlier_handler_in_its_round_shows_its_new_value() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            x = state("x", 0)
            y = state("y", 0)
            seen = {}
            x:on_change(function() y:set(2) end)
            y:on_change(function(now, before) seen[#seen + 1] = before .. "->" .. now end)
            x:set(1) y:set(1)
            "#,
        )
        .exec()
        .unwrap();

        run(&lua);

        let seen: Vec<String> = lua.load("return seen").eval().unwrap();
        assert_eq!(seen, ["0->2", "1->2"]);
    }

    #[test]
    fn a_handler_that_keeps_changing_its_own_state_stops_after_the_round_cap() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            flip = state("flip", false)
            runs = 0
            flip:on_change(function(now) runs = runs + 1 flip:set(not now) end)
            flip:set(true)
            "#,
        )
        .exec()
        .unwrap();

        run(&lua);
        assert_eq!(count(&lua, "runs"), MAX_ROUNDS as i64);
        run(&lua);
        assert_eq!(count(&lua, "runs"), MAX_ROUNDS as i64, "the write left over at the cap is dropped, not retried");
    }

    #[test]
    fn a_raising_handler_keeps_the_value_and_the_next_handler_still_runs() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            s = state("count", 0)
            ran = false
            s:on_change(function() error("broke") end)
            s:on_change(function() ran = true end)
            s:set(5)
            "#,
        )
        .exec()
        .unwrap();

        run(&lua);

        assert!(lua.globals().get::<bool>("ran").unwrap());
        assert_eq!(lua.load("return s:get()").eval::<i64>().unwrap(), 5);
    }

    #[test]
    fn clear_forgets_the_handlers_and_the_queued_writes() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"s = state("count", 0) runs = 0 s:on_change(function() runs = runs + 1 end) s:set(1)"#)
            .exec()
            .unwrap();
        clear(&lua);
        run(&lua);
        lua.load("s:set(2)").exec().unwrap();
        run(&lua);
        assert_eq!(count(&lua, "runs"), 0);
    }

    #[test]
    fn a_derived_signal_runs_its_handlers_once_per_change_until_clear() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            n = state("n", 1)
            parity = computed({ n }, function(v) return v % 2 end)
            seen = {}
            parity:on_change(function(now, before) seen[#seen + 1] = before .. "->" .. now end)
            -- Nothing else holds this map, so only the watcher keeps it from collection.
            n:map(function(v) return v * 10 end):on_change(function(now) seen[#seen + 1] = "x" .. now end)
            parity:on_change(function() seen[#seen + 1] = "second" end)
            collectgarbage() collectgarbage()
            "#,
        )
        .exec()
        .unwrap();
        run(&lua);
        assert_eq!(lua.load("return #seen").eval::<i64>().unwrap(), 0, "registration reads a baseline, fires nothing");

        lua.load("n:set(3)").exec().unwrap();
        run(&lua);
        lua.load("n:set(4)").exec().unwrap();
        run(&lua);
        run(&lua);
        clear(&lua);
        lua.load("n:set(5)").exec().unwrap();
        run(&lua);

        let seen: Vec<String> = lua.load("return seen").eval().unwrap();
        assert_eq!(seen, ["x30", "1->0", "second", "x40"], "an unchanged result fires nothing; clear drops all");
    }

    #[test]
    fn a_computed_that_switches_its_source_fires_on_the_new_one() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            use_b = state("use_b", false)
            a = state("a", "a1")
            b = state("b", "b1")
            pick = computed({ use_b }, function(flag) return flag and b:get() or a:get() end)
            seen = {}
            pick:on_change(function(now) seen[#seen + 1] = now end)
            use_b:set(true)
            "#,
        )
        .exec()
        .unwrap();
        run(&lua);
        lua.load(r#"b:set("b2")"#).exec().unwrap();
        run(&lua);

        let seen: Vec<String> = lua.load("return seen").eval().unwrap();
        assert_eq!(seen, ["b1", "b2"]);
    }

    #[test]
    fn a_loop_through_a_derived_signal_stops_at_the_round_cap() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            flip = state("flip", false)
            runs = 0
            flip:map(function(v) return not v end):on_change(function(now) runs = runs + 1 flip:set(now) end)
            flip:set(true)
            "#,
        )
        .exec()
        .unwrap();

        run(&lua);
        assert_eq!(count(&lua, "runs"), MAX_ROUNDS as i64);
        run(&lua);
        assert_eq!(count(&lua, "runs"), MAX_ROUNDS as i64, "the change left at the cap is the new baseline");
    }

    #[test]
    fn a_raising_map_keeps_its_baseline_and_a_first_success_fires_nothing() {
        let (lua, _dirty) = lua_with_state();
        lua.load(
            r#"
            s = state("s", 0)
            seen = {}
            s:map(function(v) if v % 2 == 0 then error("even") end return v end)
                :on_change(function(now, before) seen[#seen + 1] = tostring(before) .. "->" .. now end)
            "#,
        )
        .exec()
        .unwrap();
        for write in ["s:set(1)", "s:set(2)", "s:set(3)"] {
            lua.load(write).exec().unwrap();
            run(&lua);
        }
        let seen: Vec<String> = lua.load("return seen").eval().unwrap();
        assert_eq!(seen, ["1->3"], "the first success is the baseline; a later raise keeps it");
    }

    #[test]
    fn a_map_over_delay_or_pulse_fires_when_the_deadline_writes_it() {
        let (lua, _dirty) = lua_with_state();
        let t0 = std::time::Instant::now();
        lua.set_app_data(crate::lua::timer::SimClock(t0));
        lua.load(
            r#"
            s = state("s", 0)
            seen = {}
            delay(s, 10):map(function(v) return v end):on_change(function(now) seen[#seen + 1] = "delay " .. now end)
            pulse(s, 10):map(function(v) return v end):on_change(function(now) seen[#seen + 1] = "pulse " .. tostring(now) end)
            s:set(1)
            "#,
        )
        .exec()
        .unwrap();
        run(&lua);
        let due = t0 + std::time::Duration::from_millis(10);
        lua.set_app_data(crate::lua::timer::SimClock(due));
        super::super::take_due_wake(&lua, due).into_iter().for_each(super::super::note_write);
        run(&lua);

        let seen: Vec<String> = lua.load("return seen").eval().unwrap();
        assert_eq!(seen, ["pulse true", "delay 1", "pulse false"]);
    }

    #[test]
    fn a_state_map_or_computed_has_the_hook_and_other_kinds_do_not() {
        let (lua, _dirty) = lua_with_state();
        for derived in [r#"hover("h")"#, r#"delay(state("d", 1), 10)"#, r#"pulse(state("p", 1), 10)"#] {
            assert!(lua.load(format!("return ({derived}).on_change == nil")).eval::<bool>().unwrap(), "{derived}");
        }
        for watchable in [
            r#"state("t", 0)"#,
            r#"state("m", 1):map(function(v) return v end)"#,
            r#"computed({ 1 }, function(v) return v end)"#,
        ] {
            assert!(lua.load(format!("return ({watchable}).on_change ~= nil")).eval::<bool>().unwrap(), "{watchable}");
        }
    }
}

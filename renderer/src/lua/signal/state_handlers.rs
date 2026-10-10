//! `s:on_change(fn(current, previous))` on named `state`. A write only queues the change; the
//! queue runs before the next layout pass, so no handler runs inside its writer's stack.

use mlua::{Function, Lua, Value};
use rustc_hash::FxHashMap;
use shared::error;

use super::{CellId, CpuBudget, Signal, same_value};
use crate::lua::warn_raised;

/// Rounds per [`run`] (ADR-0288). A handler's write queues the next round, so a chain takes one
/// round per link; eight is deeper than a hand-written chain, and a loop stops after eight rounds
/// of 2.5 ms handler budgets instead of hanging the turn.
const MAX_ROUNDS: usize = 8;

/// Handlers by state cell, and the writes since the last [`run`]: each written cell once, with
/// the value it held before its first write.
#[derive(Default)]
struct StateHandlers {
    handlers: FxHashMap<CellId, Vec<Function>>,
    pending: Vec<(CellId, Signal, Value)>,
}

/// `s:on_change(f)`: several per state, all dropped by [`clear`].
pub(super) fn add(lua: &Lua, id: CellId, handler: Function) {
    crate::lua::app_data_or_default::<StateHandlers>(lua).handlers.entry(id).or_default().push(handler);
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
/// its first write, each under its own CPU budget. A raising handler is logged; the value stays.
/// Called once per turn before the layout pass, like a capability push's `on_change`.
pub fn run(lua: &Lua) {
    for _ in 0..MAX_ROUNDS {
        let pending = match lua.app_data_mut::<StateHandlers>() {
            Some(mut registry) if !registry.pending.is_empty() => std::mem::take(&mut registry.pending),
            _ => return,
        };
        for (id, signal, previous) in pending {
            let Ok(current) = signal.get_value(lua) else { continue };
            if same_value(&current, &previous) {
                continue;
            }
            // Cloned out: a handler may register another or write state, both of which borrow.
            let handlers = lua.app_data_ref::<StateHandlers>().and_then(|registry| registry.handlers.get(&id).cloned());
            for handler in handlers.unwrap_or_default() {
                warn_raised(
                    &handler,
                    CpuBudget::call(lua, &handler, (current.clone(), previous.clone())),
                    format_args!("state({:?}):on_change handler", super::globals::state_name(lua, id)),
                );
            }
        }
    }
    let Some(mut registry) = lua.app_data_mut::<StateHandlers>() else { return };
    let left: Vec<CellId> = registry.pending.drain(..).map(|(id, ..)| id).collect();
    drop(registry);
    if !left.is_empty() {
        let names: Vec<String> = left.into_iter().map(|id| super::globals::state_name(lua, id)).collect();
        error!(
            "state handlers were still changing state after {MAX_ROUNDS} rounds, so the changes to {} run no handler; one handler's write probably undoes another's",
            names.join(", ")
        );
    }
}

/// Before each evaluation and after a failed one, with the capability handlers: the next
/// evaluation registers its own, and a queued write belongs to the handlers it drops.
pub fn clear(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<StateHandlers>() {
        registry.handlers.clear();
        registry.pending.clear();
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
    fn only_a_state_signal_has_the_hook_so_a_config_can_test_for_it() {
        let (lua, _dirty) = lua_with_state();
        for derived in [r#"state("s", 1):map(function(v) return v end)"#, r#"hover("h")"#] {
            assert!(lua.load(format!("return ({derived}).on_change == nil")).eval::<bool>().unwrap(), "{derived}");
        }
        assert!(lua.load(r#"return state("t", 0).on_change ~= nil"#).eval::<bool>().unwrap());
    }
}

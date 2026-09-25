use std::time::{Duration, Instant};

use mlua::Lua;

use super::tracking::MemoTable;

/// CPU runtime is capped at 5ms per evaluation.
const CPU_CAP: Duration = Duration::from_millis(5);

/// Whole-`Scene::apply` cap, not per getter. It must exceed legitimate passes that run every getter
/// and block on shaping per text measurement. A 2000-sibling row (up to 4000) measured release
/// 202ms/298ms and debug 550ms/1.10s; 2s is ~2x worst and ~300x ADR-0069's 6.14ms 500-row list.
///
/// Bounds damage, not performance: a spinning `margin` `__index` ran one `Scene::apply` for 26.10s,
/// returned `Ok(())`, and blocked the thread that answers `configure` and runs Lua (ADR-0039). Now
/// it is a 2s stall and reportable `LayoutError`. ponytail: 200ms for 2000 siblings still drops
/// frames at ADR-0044's push cadence. Upgrade to a cost-sized per-pass budget.
const LAYOUT_PASS_CAP: Duration = Duration::from_secs(2);

/// One evaluation's wall pre-filter and thread-CPU deadline. CPU is authoritative; an unexpired
/// wall deadline proves CPU is unexpired, avoiding a syscall. `Instant::now()` costs 23.6ns versus
/// `CLOCK_THREAD_CPUTIME_ID`'s 170.4ns. Wall alone charged descheduled work: on 12 threads it fired
/// 5 times in 53 suite runs for configs a quiet machine evaluates in microseconds. A parked thread
/// fires no hook, and ADR-0048 removes blocking calls (`io` absent; four `os` calls never wait).
#[derive(Clone, Copy)]
pub(super) struct Deadline {
    wall: Instant,
    /// `None` when the CPU clock is unreadable; wall remains authoritative.
    cpu: Option<Duration>,
}

impl Deadline {
    /// `cap` from now on both clocks. Anchoring CPU here and not at wall expiry is what keeps the
    /// cap a cap: read later, the deadline would be `cap` of CPU *after* the wall cap, doubling it.
    fn lasting(cap: Duration) -> Self {
        Self { wall: Instant::now() + cap, cpu: thread_cpu_time().map(|used| used + cap) }
    }

    fn expired(&self) -> bool {
        // Past wall pre-filter, so CPU decides. An unreadable clock expires; an unmeasurable cap
        // must fire rather than disappear.
        Instant::now() > self.wall
            && self.cpu.is_none_or(|deadline| thread_cpu_time().is_none_or(|used| used > deadline))
    }
}

/// CPU used by the calling thread. Per thread, not process: Lua runs start to finish on the
/// entering Wayland thread (ADR-0039); process-wide time would charge shaping.
/// `crate::wayland::idle_profile` charges blocks of its loop against the same per-thread scope.
pub(crate) fn thread_cpu_time() -> Option<Duration> {
    let spent = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    Some(Duration::new(spent.tv_sec().try_into().ok()?, spent.tv_nsec().try_into().ok()?))
}

/// VM instructions between checks. `HookTriggers` warns low values have high overhead; 1000 stays
/// cheap and catches a runaway closure within roughly one batch of 5ms, not seconds later.
const CHECK_EVERY_N_INSTRUCTIONS: u32 = 1000;

/// Max live [`Signal::get_value`](super::Signal::get_value) calls before `mlua::Error`, matching `layout::scene`'s
/// `MAX_TREE_DEPTH`. [`CpuBudget::enter`] wraps dependency resolution, so it bounds body recursion
/// and chains (`s:map(f):map(g):...`). A 200-link chain reaches depth 200 with Rust depth 1 if the
/// deadline wraps only the closure; 5000 links abort with `fatal runtime error: stack overflow`,
/// beyond the 5ms hook because the chain does no Lua work. 32 leaves headroom; scene measurements
/// set both constants.
const MAX_SIGNAL_NESTING_DEPTH: usize = 32;

/// Shared error for hook and [`CpuBudget::check_not_exceeded`] gates.
///
/// Deliberately names no construct: [`CpuBudget`] also wraps
/// `capability::CapabilityHandle::notify_change`'s handlers, and the hook cannot tell which it
/// interrupted. Each call site already prefixes what it was doing ("Signal getter failed: ...",
/// "`on_change` handler raised, ignoring it: ...").
const CPU_CAP_EXCEEDED: &str = "exceeded the 5ms CPU budget for one evaluation";

/// Distinct pass-budget error so config knows which limit it hit. Plain `__index` without a signal
/// reaches it, the hole this budget closes.
const LAYOUT_PASS_CAP_EXCEEDED: &str = "the layout pass exceeded its 2s CPU budget";

/// RAII claim on the 5ms budget for dependency resolution plus closure call. Deadlines stack in
/// `app_data` because computed dependencies, body reads, and self/mutual cycles re-enter; a single
/// mlua hook removed by an inner call would strip the outer cap. Install on 0->1 holders, remove on
/// 1->0. `stack[0]`, not `last()`, is the outer evaluation's deadline and the minimum: LIFO pushes
/// are non-decreasing, while `last()` is freshly reset and misses expiry. `first()` is O(1) versus
/// scanning up to 32 entries per hook.
pub(crate) struct CpuBudget<'lua> {
    lua: &'lua Lua,
}

/// Installs the instruction hook for the life of the VM, once, before any config code runs.
///
/// `set_global_hook`, not per-thread `set_hook`: coroutines otherwise ran unhooked, measured 5.75s
/// of Lua in `coroutine.create`/`resume` returning `Ok`.
///
/// Installed permanently rather than around each budget, because installing a hook does not
/// retrofit coroutines that already exist -- a new Lua thread inherits its creator's hook, and a
/// creator running while nothing was budgeted had none to pass on. `shell.lua`'s own top level is
/// exactly that moment, so a `coroutine.wrap` stored there and resumed from a getter spun with
/// nothing to stop it. Refcounting when to remove it was what left those windows open.
///
/// The cost is the callback itself, every [`CHECK_EVERY_N_INSTRUCTIONS`]: with no budget live
/// [`expired_budget`] finds no deadline stack and returns on the first lookup.
pub(crate) fn install_hook(lua: &Lua) -> mlua::Result<()> {
    lua.set_global_hook(
        mlua::HookTriggers { every_nth_instruction: Some(CHECK_EVERY_N_INSTRUCTIONS), ..mlua::HookTriggers::new() },
        |lua, _| match expired_budget(lua) {
            Some(message) => Err(mlua::Error::runtime(message)),
            None => Ok(mlua::VmState::Continue),
        },
    )
}

/// Whole-`Scene::apply` deadline, when a pass is in flight.
#[derive(Default)]
pub(super) struct PassDeadline(pub(super) Option<Deadline>);

/// RAII claim on [`LAYOUT_PASS_CAP`] for the whole pass. It covers metamethod-aware `Table::get`
/// after [`CpuBudget`] drops its hook (a `while true` `margin.__index` once hung Wayland), and
/// stops a margined tree buying one 5ms budget per `get_value` under ADR-0021. Runs beside
/// [`CpuBudget`]; the earlier
/// [`expired_budget`] wins, preserving the 5ms cap and adding a pass ceiling.
pub(crate) struct LayoutPassBudget<'lua> {
    lua: &'lua Lua,
}

impl<'lua> LayoutPassBudget<'lua> {
    /// Starts the pass clock and holds the hook across it, putting `__index` under a budget. Also
    /// opens the [`EvaluationMemo`](super::tracking::EvaluationMemo) for the pass: a computed then answers once for every node and
    /// property that reads it, instead of once per property (ADR-0157).
    pub(crate) fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        crate::lua::app_data_or_default::<PassDeadline>(lua).0 = Some(Deadline::lasting(LAYOUT_PASS_CAP));
        // Starts with or retains the memo table across passes, clearing it at pass end.
        crate::lua::app_data_or_default::<MemoTable>(lua).pass_opened = Some(super::tracking::current_clock());
        Ok(Self { lua })
    }

    /// Rust-boundary gate: config `pcall` can swallow the hook's ordinary Lua error in `__index` or
    /// a getter, but cannot swallow this check.
    pub(crate) fn exceeded(&self) -> bool {
        self.lua.app_data_ref::<PassDeadline>().and_then(|slot| slot.0).is_some_and(|d| d.expired())
    }
}

impl Drop for LayoutPassBudget<'_> {
    fn drop(&mut self) {
        if let Ok(Some(mut slot)) = self.lua.try_app_data_mut::<PassDeadline>() {
            slot.0 = None;
        }
        if let Ok(Some(mut table)) = self.lua.try_app_data_mut::<MemoTable>() {
            table.map.clear();
            table.eval_stack.clear();
            table.pass_opened = None;
        }
    }
}

impl<'lua> CpuBudget<'lua> {
    /// Claims one nesting level, refusing past [`MAX_SIGNAL_NESTING_DEPTH`]. Install hook before
    /// pushing so early return cannot strand a deadline and disable the VM's cap; no Lua runs
    /// between the two, and the hook tolerates an empty stack.
    pub(crate) fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        let mut stack = crate::lua::app_data_or_default::<Vec<Deadline>>(lua);
        if stack.len() >= MAX_SIGNAL_NESTING_DEPTH {
            return Err(mlua::Error::runtime(format!(
                "signal nesting exceeded its maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels -- a computed/map chain recursing into itself, or a dependency chain that long?"
            )));
        }
        let deadline = stack.first().copied().unwrap_or_else(|| Deadline::lasting(CPU_CAP));
        stack.push(deadline);
        Ok(Self { lua })
    }

    /// Second 5ms gate at Rust boundary. A `pcall` can catch the hook and return a partial `Ok`,
    /// measured at 7.5x the cap; this check turns it into `Err`. ponytail: a body that swallows the
    /// hook and never returns still spins. VM lacks preemption; upgrade path: evaluate in a separate
    /// process (ADR-0039).
    pub(crate) fn check_not_exceeded(&self) -> mlua::Result<()> {
        match expired_budget(self.lua) {
            Some(message) => Err(mlua::Error::runtime(message)),
            None => Ok(()),
        }
    }
}

impl Drop for CpuBudget<'_> {
    fn drop(&mut self) {
        if let Ok(Some(mut stack)) = self.lua.try_app_data_mut::<Vec<Deadline>>() {
            stack.pop();
        }
    }
}

/// Returns the earlier expired signal/pass deadline. Signal uses outermost `first()`; pass stays
/// separate so that lookup remains O(1). No deadline means no expiry, allowing hook installation.
fn expired_budget(lua: &Lua) -> Option<&'static str> {
    let signal = lua.app_data_ref::<Vec<Deadline>>().and_then(|stack| stack.first().copied());
    if signal.is_some_and(|deadline| deadline.expired()) {
        return Some(CPU_CAP_EXCEEDED);
    }
    let pass = lua.app_data_ref::<PassDeadline>().and_then(|slot| slot.0);
    pass.filter(Deadline::expired).map(|_| LAYOUT_PASS_CAP_EXCEEDED)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{lua_with_signal, lua_with_state};
    use super::super::*;
    use super::*;

    #[test]
    fn computed_aborts_a_runaway_closure_instead_of_hanging_or_returning_a_wrong_value() {
        let lua = lua_with_signal("a", Value::Integer(1));

        let start = Instant::now();
        let result: mlua::Result<i64> =
            lua.load("return computed({a}, function(x) while true do end end):get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a busy-loop computed must error, not return a value");
        assert!(elapsed < Duration::from_secs(1), "the 5ms cap must abort well under a second, took {elapsed:?}");
    }

    #[test]
    fn a_nested_get_call_inside_a_computed_body_does_not_strip_the_outer_calls_cap() {
        // A body reading a second Signal re-enters the budget; inner return must preserve the outer
        // cap.
        let lua = lua_with_signal("a", Value::Integer(1));
        lua.globals().set("other", Signal::new_state(Value::Integer(2), DirtyFlag::new()).unwrap()).unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> =
            lua.load("return computed({a}, function(x) local y = other:get(); while true do end end):get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "the outer computed must still abort even though its body read a second Signal");
        assert!(elapsed < Duration::from_secs(1), "the cap must still fire near 5ms, took {elapsed:?}");
    }

    #[test]
    fn a_self_referential_computed_is_rejected_with_a_nesting_depth_error_not_an_abort() {
        // Self-reference recurses through `get_value` beyond what CPU cap can stop; before this cap
        // the exact case ended in `fatal runtime error: stack overflow`.
        let lua = lua_with_state().0;
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                local loop
                loop = computed({}, function() return loop:get() end)
                return loop:get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a self-referential computed must error, not abort the process");
        assert!(elapsed < Duration::from_secs(1), "the depth cap must trip well under a second, took {elapsed:?}");
    }

    #[test]
    fn a_mutually_recursive_computed_pair_is_rejected_with_a_nesting_depth_error_not_an_abort() {
        let lua = lua_with_state().0;
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                local a, b
                a = computed({}, function() return b:get() end)
                b = computed({}, function() return a:get() end)
                return a:get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a mutually recursive computed pair must error, not abort the process");
        assert!(elapsed < Duration::from_secs(1), "the depth cap must trip well under a second, took {elapsed:?}");
    }

    /// `s:map(f):map(f):...` `links` deep over signal `a`.
    fn map_chain_source(links: usize) -> String {
        format!(
            r#"
            local s = a
            for _ = 1, {links} do s = s:map(function(v) return v end) end
            return s:get()
            "#
        )
    }

    #[test]
    fn a_long_map_dependency_chain_is_rejected_by_the_nesting_cap_not_a_stack_overflow() {
        // Dependency chains nest `get_value` without Lua calls. Before the deadline wrapped
        // resolution, this chain ended in `fatal runtime error: stack overflow`; cap saw depth 1.
        let lua = lua_with_signal("a", Value::Integer(1));
        let err = lua.load(map_chain_source(200)).eval::<i64>().unwrap_err();
        assert!(
            err.to_string().contains("signal nesting exceeded"),
            "a 200-link map chain must trip the nesting cap: {err}"
        );
    }

    /// `delay(delay(...))` / `pulse(pulse(...))` `links` deep over signal `a`.
    fn hold_chain_source(builder: &str, links: usize) -> String {
        format!(
            r#"
            local s = a
            for _ = 1, {links} do s = {builder}(s, 1) end
            return s:get()
            "#
        )
    }

    #[test]
    fn a_long_delay_or_pulse_chain_is_rejected_by_the_nesting_cap_not_a_stack_overflow() {
        // A delay or pulse chain nests `get_value` the way a map chain does. Unguarded, 10,000
        // links ended `mantle check` in `fatal runtime error: stack overflow`: SIGABRT, which no
        // config author can read.
        for builder in ["delay", "pulse"] {
            let lua = lua_with_signal("a", Value::Integer(1));
            let err = lua.load(hold_chain_source(builder, 200)).eval::<Value>().unwrap_err();
            assert!(
                err.to_string().contains("signal nesting exceeded"),
                "a 200-link {builder} chain must trip the nesting cap: {err}"
            );
        }
    }

    #[test]
    fn a_map_chain_at_the_nesting_cap_is_accepted_and_one_link_past_it_is_rejected() {
        // At most N levels are admitted, N+1 rejected. This distinguishes gates: CPU measures this
        // thread, not descheduled wait, so a busy machine may hit 5ms at the admitted depth;
        // nesting must not reject a depth it promises.
        let lua = lua_with_signal("a", Value::Integer(7));
        match lua.load(map_chain_source(MAX_SIGNAL_NESTING_DEPTH)).eval::<i64>() {
            Ok(value) => assert_eq!(value, 7, "exactly MAX_SIGNAL_NESTING_DEPTH nested levels must be admitted"),
            Err(err) => assert!(
                err.to_string().contains(CPU_CAP_EXCEEDED),
                "at the limit only the CPU budget may fire, never the nesting cap: {err}"
            ),
        }

        let err = lua.load(map_chain_source(MAX_SIGNAL_NESTING_DEPTH + 1)).eval::<i64>().unwrap_err();
        assert!(
            err.to_string().contains(&format!("maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels")),
            "one level past the cap must be rejected, naming the limit actually enforced: {err}"
        );
    }

    #[test]
    fn a_computed_descheduled_past_its_deadline_is_not_charged_for_time_it_did_not_run() {
        // Before the fix: 5 failures in 53 renderer-suite runs, a different test each time, across
        // 630 tests/12 threads, each falsely raising the 5ms error while a quiet-machine config
        // was descheduled. `park` burns no CPU, so the CPU cap must not fire; the later loop
        // exercises both hook and return gates.
        let lua = lua_with_signal("a", Value::Integer(7));
        let park = lua
            .create_function(|_, ()| {
                std::thread::sleep(Duration::from_millis(40));
                Ok(())
            })
            .unwrap();
        lua.globals().set("park", park).unwrap();

        let value: i64 = lua
            .load("return a:map(function(v) park() local n = 0 for i = 1, 5000 do n = n + i end return v end):get()")
            .eval()
            .unwrap();

        assert_eq!(value, 7);
    }

    #[test]
    fn a_pcall_swallowing_the_hook_error_still_fails_at_the_rust_boundary() {
        // `pcall` catches the hook's ordinary Lua error and could return partial data. Bounded loop
        // keeps a regression slow rather than hanging the runner.
        let lua = lua_with_signal("a", Value::Integer(1));
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                return computed({a}, function(x)
                    local n = 0
                    for _ = 1, 400 do
                        pcall(function() for _ = 1, 20000 do n = n + 1 end end)
                    end
                    return n
                end):get()
                "#,
            )
            .eval();

        assert!(
            result.is_err(),
            "a body that swallows the hook error must not yield a partially computed value: {result:?}"
        );
    }

    #[test]
    fn a_coroutine_made_before_any_budget_is_still_covered_by_the_cpu_cap() {
        // The sibling below creates its coroutine inside a budgeted body, so it inherits the hook
        // that body installed. One made while nothing is budgeted -- at the top level of
        // `shell.lua`, before any getter runs -- inherited no hook, because installing one does not
        // retrofit threads that already exist. Resuming it later from inside a budget then spun
        // with nothing to stop it, and `mantle call` made that reachable from outside the process.
        let lua = lua_with_signal("a", Value::Integer(1));
        lua.load(
            r#"
            spin = coroutine.wrap(function()
                local n = 0
                for _ = 1, 500000000 do n = n + 1 end
                return n
            end)
            "#,
        )
        .exec()
        .unwrap();

        let start = Instant::now();
        let result: mlua::Result<i64> = lua.load("return computed({a}, function(x) return spin() end):get()").eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a coroutine made before the budget must still hit the cap: {result:?}");
        assert!(elapsed < Duration::from_secs(1), "the hook must reach it, took {elapsed:?}");
    }

    #[test]
    fn a_coroutine_body_is_covered_by_the_cpu_cap() {
        // Per-thread `Lua::set_hook` left coroutine work unhooked: measured 5.75s returning `Ok`.
        // Elapsed assertion catches the escape; Rust-boundary gate would error either way.
        let lua = lua_with_signal("a", Value::Integer(1));
        let start = Instant::now();
        let result: mlua::Result<i64> = lua
            .load(
                r#"
                return computed({a}, function(x)
                    local step = coroutine.wrap(function()
                        local n = 0
                        for _ = 1, 500000000 do n = n + 1 end
                        return n
                    end)
                    return step()
                end):get()
                "#,
            )
            .eval();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "work inside a coroutine must still hit the CPU cap: {result:?}");
        assert!(elapsed < Duration::from_secs(1), "the hook must reach the coroutine, took {elapsed:?}");
    }

    #[test]
    fn a_cap_abort_does_not_leave_the_hook_installed_for_later_unrelated_evaluation() {
        let lua = lua_with_signal("a", Value::Integer(1));
        let _: mlua::Result<i64> = lua.load("return computed({a}, function(x) while true do end end):get()").eval();

        // A legitimate top-level script slower than 5ms must not inherit an aborted hook.
        let result: i64 = lua
            .load(
                r#"
                local sum = 0
                for i = 1, 2000000 do sum = sum + i end
                return sum
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 2_000_001_000_000);
    }
}

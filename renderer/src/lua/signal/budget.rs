use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::time::{Duration, Instant};

use mlua::{FromLuaMulti, Function, IntoLuaMulti, Lua};
use shared::warn;

use super::tracking::MemoTable;
use crate::lua::location::Site;

/// Runaway cap on the CPU one outermost evaluation may spend; overrunning it is an error. Sized
/// 20x the frame target so a slow machine running a legitimate chain never reaches it (ADR-0369).
const RUNAWAY_CAP: Duration = Duration::from_millis(50);

/// Frame-time target for one outermost evaluation. Missing it logs a warning; it never fails.
const FRAME_TARGET: Duration = Duration::from_micros(2500);

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
///
/// A signal deadline anchors its CPU clock lazily, at the first hook tick ([`Deadline::expired`]
/// with `anchor`), not at entry: a getter that ends inside one 1000-instruction batch never reads
/// the CPU clock, and the anchor lands at most one batch (microseconds) late.
#[derive(Clone)]
pub(super) struct Deadline {
    wall: Instant,
    cap: Duration,
    /// Outer `None`: not anchored yet. Inner `None`: the CPU clock is unreadable; wall remains
    /// authoritative.
    cpu: Cell<Option<Option<Duration>>>,
    /// The computed this stack level is evaluating; `None` for a handler, a held read or a pass.
    site: Option<Site>,
}

impl Deadline {
    /// `cap` from now on wall; CPU anchors on the first tick. Anchoring CPU at wall expiry instead
    /// would make the deadline `cap` of CPU *after* the wall cap, doubling it.
    fn lazy(cap: Duration) -> Self {
        Self { wall: Instant::now() + cap, cap, cpu: Cell::new(None), site: None }
    }

    /// `cap` from now on both clocks.
    fn lasting(cap: Duration) -> Self {
        let deadline = Self::lazy(cap);
        deadline.start_cpu();
        deadline
    }

    fn anchor(&self) {
        if self.cpu.get().is_none() {
            self.start_cpu();
        }
    }

    fn start_cpu(&self) {
        self.cpu.set(Some(thread_cpu_time().map(|used| used + self.cap)));
    }

    /// CPU spent since the clock anchored; `None` if it never did, which costs no clock read.
    fn cpu_spent(&self) -> Option<Duration> {
        let end = self.cpu.get()??;
        Some(thread_cpu_time()?.saturating_sub(end - self.cap))
    }

    /// `anchor` is true from the hook, which starts the CPU clock; the Rust-boundary gate passes
    /// false so a getter that never ticked stays syscall-free. Native calls that run long between
    /// ticks call [`anchor_cpu_budget`] at entry. ponytail: a getter whose only long call is Lua's
    /// own C library (a huge `string.rep`) overruns unseen when wall has expired too, since no
    /// baseline exists; the pass cap still bounds it. Upgrade: anchor at entry.
    fn expired(&self, anchor: bool) -> bool {
        let Some(cpu) = self.cpu.get() else {
            if anchor {
                self.start_cpu();
            }
            return false;
        };
        // Past wall pre-filter, so CPU decides. An unreadable clock expires; an unmeasurable cap
        // must fire rather than disappear.
        Instant::now() > self.wall && cpu.is_none_or(|deadline| thread_cpu_time().is_none_or(|used| used > deadline))
    }
}

/// CPU used by the calling thread. Per thread, not process: Lua runs start to finish on the
/// entering Wayland thread (ADR-0039); process-wide time would charge shaping.
/// `crate::wayland::idle_profile` charges blocks of its loop against the same per-thread scope.
pub(crate) fn thread_cpu_time() -> Option<Duration> {
    #[cfg(test)]
    CLOCK_READS.with(|reads| reads.set(reads.get() + 1));
    let spent = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    Some(Duration::new(spent.tv_sec().try_into().ok()?, spent.tv_nsec().try_into().ok()?))
}

#[cfg(test)]
thread_local!(static CLOCK_READS: Cell<usize> = const { Cell::new(0) });

/// VM instructions between checks. `HookTriggers` warns low values have high overhead; 1000 stays
/// cheap and catches a runaway closure within roughly one batch of the 50ms cap, not seconds later.
const CHECK_EVERY_N_INSTRUCTIONS: u32 = 1000;

/// Max live [`Signal::get_value`](super::Signal::get_value) calls before `mlua::Error`, matching `layout::scene`'s
/// `MAX_TREE_DEPTH`. [`CpuBudget::enter`] wraps dependency resolution, so it bounds body recursion
/// and chains (`s:map(f):map(g):...`). A 200-link chain reaches depth 200 with Rust depth 1 if the
/// deadline wraps only the closure; 5000 links abort with `fatal runtime error: stack overflow`,
/// beyond the CPU hook because the chain does no Lua work. 32 leaves headroom; scene measurements
/// set both constants.
const MAX_SIGNAL_NESTING_DEPTH: usize = 32;

/// Shared error for hook and [`CpuBudget::check_not_exceeded`] gates.
///
/// Deliberately names no construct: [`CpuBudget`] also wraps
/// `capability::CapabilityHandle::notify_change`'s handlers, and the hook cannot tell which it
/// interrupted. Each call site already prefixes what it was doing ("Signal getter failed: ...",
/// "`on_change` handler raised, ignoring it: ...").
const CPU_CAP_EXCEEDED: &str = "exceeded the 50ms CPU limit for one evaluation; a runaway loop?";

/// Distinct pass-budget error so config knows which limit it hit. Plain `__index` without a signal
/// reaches it, the hole this budget closes.
const LAYOUT_PASS_CAP_EXCEEDED: &str = "the layout pass exceeded its 2s CPU budget";

/// RAII claim on the runaway cap for dependency resolution plus closure call. Deadlines stack in
/// `app_data` because computed dependencies, body reads, and self/mutual cycles re-enter; a single
/// mlua hook removed by an inner call would strip the outer cap. Install on 0->1 holders, remove on
/// 1->0. `stack[0]`, not `last()`, is the outer evaluation's deadline and the minimum: LIFO pushes
/// are non-decreasing, while `last()` is freshly reset and misses expiry. `first()` is O(1) versus
/// scanning up to 32 entries per hook.
pub(crate) struct CpuBudget<'lua> {
    lua: &'lua Lua,
    /// No outer evaluation had started its CPU clock, so an overrun is this evaluation's own.
    pub(super) fresh: bool,
    /// The outermost evaluation lent the collector [`GC_CREDIT_KIB`] to return on drop.
    gc_deferred: bool,
}

/// Allocation credit lent to the collector for one outermost evaluation, far past the 2.5 ms budget.
const GC_CREDIT_KIB: i32 = 1 << 20;

/// `collectgarbage("step", kib)`: adds `kib` to the collector's debt, stepping only if it is then positive.
fn gc_shift_debt(lua: &Lua, kib: i32) -> bool {
    // SAFETY: `lua_gc` with `LUA_GCSTEP` takes one int and touches no stack slot; `exec_raw` runs it protected.
    unsafe { lua.exec_raw::<()>((), |state| _ = mlua::ffi::lua_gc(state, mlua::ffi::LUA_GCSTEP, kib)) }.is_ok()
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
        |lua, _| match expired_budget(lua, true) {
            Some(message) => Err(mlua::Error::runtime(message)),
            None => Ok(mlua::VmState::Continue),
        },
    )
}

/// Starts the outer evaluation's CPU clock, if it has not started. A no-op with no evaluation live
/// or after the first tick; call it at entry of a native function that can run past one hook batch
/// (`fuzzy`, `json.decode`), where no tick would anchor the budget before the work is spent.
pub(crate) fn anchor_cpu_budget(lua: &Lua) {
    if let Some(deadline) = lua.app_data_ref::<Budgets>().as_ref().and_then(|budgets| budgets.stack.first()) {
        deadline.anchor();
    }
}

/// Live evaluations plus the frame-target bookkeeping, in one `app_data` value so the instruction
/// hook pays one lookup for both. Deadlines stack because computed dependencies, body reads and
/// cycles re-enter.
#[derive(Default)]
struct Budgets {
    stack: Vec<Deadline>,
    /// Hook ticks of the running outermost evaluation by the innermost site at the tick. Sampling
    /// reads no clock; it sees only Lua instructions, so native time is never attributed.
    samples: RefCell<Vec<(Option<Site>, u32)>>,
    /// Slowest sites already warned about. [`forget_frame_warnings`] clears it per evaluation.
    /// ponytail: every overrun with no site (a handler, timer or action) shares the `None` key, so
    /// only the first such overrun per reload warns. Upgrade: key by the callback's own site.
    warned: HashSet<Option<Site>>,
}

impl Budgets {
    fn sample(&self) {
        let Some(site) = self.stack.last().map(|top| top.site) else { return };
        let mut samples = self.samples.borrow_mut();
        match samples.iter_mut().find(|(seen, _)| *seen == site) {
            Some((_, ticks)) => *ticks += 1,
            None => samples.push((site, 1)),
        }
    }

    /// Ends the outermost evaluation: one warning per slowest site when its CPU, measured only if
    /// the clock anchored, missed [`FRAME_TARGET`]. A runaway already raised an error.
    fn report_frame_overrun(&mut self) {
        let samples = std::mem::take(self.samples.get_mut());
        let Some(spent) = self.stack[0].cpu_spent().filter(|spent| (FRAME_TARGET..RUNAWAY_CAP).contains(spent)) else {
            return;
        };
        let slowest = samples.iter().max_by_key(|(_, ticks)| *ticks).map(|(site, _)| *site);
        if !self.warned.insert(slowest.flatten()) {
            return;
        }
        let culprit = match slowest {
            Some(Some(site)) => format!("most of it in the signal created at {site}"),
            Some(None) => "most of it outside any signal (handler body or native call)".to_string(),
            None => "none of it in Lua instructions (native work)".to_string(),
        };
        let message =
            format!("an evaluation took {spent:.1?} of CPU, over the {FRAME_TARGET:?} frame target; {culprit}");
        #[cfg(test)]
        FRAME_WARNINGS.with_borrow_mut(|seen| seen.push(message.clone()));
        warn!("{message}");
    }
}

/// Lets a re-evaluated config's slow signal warn again. Called before each evaluation.
pub(super) fn forget_frame_warnings(lua: &Lua) {
    if let Ok(Some(mut budgets)) = lua.try_app_data_mut::<Budgets>() {
        budgets.warned.clear();
    }
}

/// Whole-`Scene::apply` deadline, when a pass is in flight.
#[derive(Default)]
pub(super) struct PassDeadline(pub(super) Option<Deadline>);

/// RAII claim on [`LAYOUT_PASS_CAP`] for the whole pass. It covers metamethod-aware `Table::get`
/// after [`CpuBudget`] drops its hook (a `while true` `margin.__index` once hung Wayland), and
/// stops a margined tree buying one evaluation budget per `get_value` under ADR-0021. Runs beside
/// [`CpuBudget`]; the earlier
/// [`expired_budget`] wins, preserving the runaway cap and adding a pass ceiling.
pub(crate) struct LayoutPassBudget<'lua> {
    lua: &'lua Lua,
}

impl<'lua> LayoutPassBudget<'lua> {
    /// Starts the pass clock and holds the hook across it, putting `__index` under a budget. Also
    /// opens the [`EvaluationMemo`](super::tracking::EvaluationMemo) for the pass: a computed then answers once for every node and
    /// property that reads it, instead of once per property (ADR-0157).
    pub(crate) fn enter(lua: &'lua Lua) -> Self {
        crate::lua::app_data_or_default::<PassDeadline>(lua).0 = Some(Deadline::lasting(LAYOUT_PASS_CAP));
        // Starts with or retains the memo table across passes, clearing it at pass end.
        crate::lua::app_data_or_default::<MemoTable>(lua).pass_opened = Some(super::tracking::current_clock());
        Self { lua }
    }

    /// Rust-boundary gate: config `pcall` can swallow the hook's ordinary Lua error in `__index` or
    /// a getter, but cannot swallow this check.
    pub(crate) fn exceeded(&self) -> bool {
        self.lua.app_data_ref::<PassDeadline>().and_then(|slot| slot.0.clone()).is_some_and(|d| d.expired(true))
    }
}

impl Drop for LayoutPassBudget<'_> {
    fn drop(&mut self) {
        if let Ok(Some(mut slot)) = self.lua.try_app_data_mut::<PassDeadline>() {
            slot.0 = None;
        }
        if let Ok(Some(mut table)) = self.lua.try_app_data_mut::<MemoTable>() {
            // An enclosing memo owns the answers past the pass: `EvaluationMemo::resume` carries them.
            if table.depth == 0 {
                table.map.clear();
            }
            table.eval_stack.clear();
            table.pass_opened = None;
        }
    }
}

impl<'lua> CpuBudget<'lua> {
    /// Claims one nesting level, refusing past [`MAX_SIGNAL_NESTING_DEPTH`]. Install hook before
    /// pushing so early return cannot strand a deadline and disable the VM's cap; no Lua runs
    /// between the two, and the hook tolerates an empty stack.
    pub(crate) fn enter(lua: &'lua Lua, site: Option<Site>) -> mlua::Result<Self> {
        let mut budgets = crate::lua::app_data_or_default::<Budgets>(lua);
        let stack = &mut budgets.stack;
        if stack.len() >= MAX_SIGNAL_NESTING_DEPTH {
            return Err(mlua::Error::runtime(format!(
                "signal nesting exceeded its maximum depth of {MAX_SIGNAL_NESTING_DEPTH} levels -- a computed/map chain recursing into itself, or a dependency chain that long?"
            )));
        }
        let fresh = stack.first().is_none_or(|outer| outer.cpu.get().is_none());
        // Garbage collection runs inside whichever allocation pays the heap's debt, so a getter
        // was charged for collecting what a capability push or another getter left: 6 budget
        // errors in 8 runs of a 56-image palette scan, none with the collector stopped. Deferred
        // here, the evaluation's own allocation is paid for on exit, outside the budget (ADR-0372).
        let gc_deferred = stack.is_empty() && lua.gc_is_running() && gc_shift_debt(lua, -GC_CREDIT_KIB);
        let mut deadline = stack.first().cloned().unwrap_or_else(|| Deadline::lazy(RUNAWAY_CAP));
        deadline.site = site;
        stack.push(deadline);
        Ok(Self { lua, fresh, gc_deferred })
    }

    /// Calls `f` under its own budget, erroring when the call raised or ran past the runaway cap: the one
    /// way an `on_change` handler, timer or `mantle call` handler reaches config code.
    pub(crate) fn call<A: IntoLuaMulti, R: FromLuaMulti>(lua: &Lua, f: &Function, args: A) -> mlua::Result<R> {
        let budget = CpuBudget::enter(lua, None)?;
        let value = f.call(args)?;
        budget.check_not_exceeded()?;
        Ok(value)
    }

    /// Second runaway gate at Rust boundary. A `pcall` can catch the hook and return a partial `Ok`,
    /// measured at 7.5x the old 2.5ms cap; this check turns it into `Err`. ponytail: a body that swallows the
    /// hook and never returns still spins. VM lacks preemption; upgrade path: evaluate in a separate
    /// process (ADR-0039).
    pub(crate) fn check_not_exceeded(&self) -> mlua::Result<()> {
        match expired_budget(self.lua, false) {
            Some(message) => Err(mlua::Error::runtime(message)),
            None => Ok(()),
        }
    }
}

impl Drop for CpuBudget<'_> {
    fn drop(&mut self) {
        if let Ok(Some(mut budgets)) = self.lua.try_app_data_mut::<Budgets>() {
            if budgets.stack.len() == 1 {
                budgets.report_frame_overrun();
            }
            budgets.stack.pop();
        }
        // After the pop: a finalizer reading a signal opens a budget of its own.
        if self.gc_deferred {
            gc_shift_debt(self.lua, GC_CREDIT_KIB);
        }
    }
}

#[cfg(test)]
thread_local!(static FRAME_WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) });

/// Returns the earlier expired signal/pass deadline. Signal uses outermost `first()`; pass stays
/// separate so that lookup remains O(1). No deadline means no expiry, allowing hook installation.
fn expired_budget(lua: &Lua, anchor: bool) -> Option<&'static str> {
    if let Some(budgets) = lua.app_data_ref::<Budgets>() {
        // `anchor` is true only from the hook: one sample per tick.
        if anchor {
            budgets.sample();
        }
        if budgets.stack.first().is_some_and(|d| d.expired(anchor)) {
            return Some(CPU_CAP_EXCEEDED);
        }
    }
    let pass = lua.app_data_ref::<PassDeadline>()?;
    pass.0.as_ref().filter(|d| d.expired(anchor)).map(|_| LAYOUT_PASS_CAP_EXCEEDED)
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
        assert!(elapsed < Duration::from_secs(1), "the 50ms cap must abort well under a second, took {elapsed:?}");
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
        assert!(elapsed < Duration::from_secs(1), "the cap must still fire near 50ms, took {elapsed:?}");
    }

    #[test]
    fn a_recursive_computed_is_rejected_with_a_nesting_depth_error_not_an_abort() {
        // Self-reference recurses through `get_value` beyond what the CPU cap can stop; before the
        // depth cap the exact case ended in `fatal runtime error: stack overflow`.
        for (source, case) in [
            ("local loop loop = computed({}, function() return loop:get() end) return loop:get()", "self-referential"),
            (
                "local a, b a = computed({}, function() return b:get() end) b = computed({}, function() return a:get() end) return a:get()",
                "mutually recursive",
            ),
        ] {
            let lua = lua_with_state().0;
            let start = Instant::now();
            let result: mlua::Result<i64> = lua.load(source).eval();
            let elapsed = start.elapsed();

            assert!(result.is_err(), "a {case} computed must error, not abort the process");
            assert!(
                elapsed < Duration::from_secs(1),
                "{case}: the depth cap must trip well under a second, took {elapsed:?}"
            );
        }
    }

    /// `step` applied to `s` `links` deep over signal `a`.
    fn chain_source(step: &str, links: usize) -> String {
        format!("local s = a for _ = 1, {links} do s = {step} end return s:get()")
    }

    fn map_chain_source(links: usize) -> String {
        chain_source("s:map(function(v) return v end)", links)
    }

    #[test]
    fn a_long_dependency_chain_is_rejected_by_the_nesting_cap_not_a_stack_overflow() {
        // Chains nest `get_value` without Lua calls; unguarded, 10,000 links aborted with a stack overflow.
        for (kind, step) in
            [("map", "s:map(function(v) return v end)"), ("delay", "delay(s, 1)"), ("pulse", "pulse(s, 1)")]
        {
            let lua = lua_with_signal("a", Value::Integer(1));
            let err = lua.load(chain_source(step, 200)).eval::<Value>().unwrap_err();
            assert!(
                err.to_string().contains("signal nesting exceeded"),
                "a 200-link {kind} chain must trip the nesting cap: {err}"
            );
        }
    }

    #[test]
    fn a_map_chain_at_the_nesting_cap_is_accepted_and_one_link_past_it_is_rejected() {
        // At most N levels are admitted, N+1 rejected. This distinguishes gates: CPU measures this
        // thread, not descheduled wait, so a busy machine may hit the cap at the admitted depth;
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
        // 630 tests/12 threads, each falsely raising the cap error while a quiet-machine config
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

        let before = clock_reads();
        let value: i64 = lua
            .load("return a:map(function(v) local n = 0 for i = 1, 5000 do n = n + i end park() for i = 1, 5000 do n = n + i end return v end):get()")
            .eval()
            .unwrap();

        assert_eq!(value, 7);
        assert!(
            clock_reads() - before > 1,
            "after the pause, the hook must recheck CPU time without charging the pause"
        );
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
                    for _ = 1, 4000 do
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
    fn a_coroutine_is_covered_by_the_cpu_cap_whether_made_before_any_budget_or_inside_one() {
        let lua = lua_with_signal("a", Value::Integer(1));
        // Before: created while nothing is budgeted -- at the top level of `shell.lua`, before any
        // getter runs -- so it inherited no hook, because installing one does not retrofit threads
        // that already exist. Resuming it later from inside a budget then spun with nothing to
        // stop it, and `mantle call` made that reachable from outside the process.
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
        for (row, body) in [
            ("made before any budget", "return computed({a}, function(x) return spin() end):get()"),
            // Inside: it inherits the hook that body installed. Per-thread `Lua::set_hook` left
            // coroutine work unhooked: measured 5.75s returning `Ok`. Elapsed assertion catches the
            // escape; Rust-boundary gate would error either way.
            (
                "made inside the body",
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
            ),
        ] {
            let start = Instant::now();
            let result: mlua::Result<i64> = lua.load(body).eval();
            let elapsed = start.elapsed();

            assert!(result.is_err(), "{row}: work inside a coroutine must still hit the CPU cap: {result:?}");
            assert!(elapsed < Duration::from_secs(1), "{row}: the hook must reach the coroutine, took {elapsed:?}");
        }
    }

    #[test]
    fn a_cap_abort_does_not_leave_the_hook_installed_for_later_unrelated_evaluation() {
        let lua = lua_with_signal("a", Value::Integer(1));
        let _: mlua::Result<i64> = lua.load("return computed({a}, function(x) while true do end end):get()").eval();

        // A legitimate top-level script slower than the cap must not inherit an aborted hook.
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

    /// `lua_with_signal` plus two ways to spend `ms` of thread CPU, which a loaded machine cannot
    /// shorten or lengthen: `spin(ms)` in Lua instructions, which the hook samples, and `burn(ms)`
    /// natively, which it cannot see (it anchors the budget first, as any long native call does).
    fn lua_with_burners() -> Lua {
        let lua = lua_with_signal("a", Value::Integer(1));
        let cpu = || thread_cpu_time().unwrap_or_default();
        let burn = lua
            .create_function(move |lua, ms: u64| {
                anchor_cpu_budget(lua);
                let start = cpu();
                while cpu() - start < Duration::from_millis(ms) {}
                Ok(())
            })
            .unwrap();
        let cpu_ms = lua.create_function(move |_, ()| Ok(cpu().as_secs_f64() * 1000.0)).unwrap();
        lua.globals().set("burn", burn).unwrap();
        lua.globals().set("cpu_ms", cpu_ms).unwrap();
        lua.load("function spin(ms) local t = cpu_ms() while cpu_ms() - t < ms do end end").exec().unwrap();
        lua
    }

    fn frame_warnings() -> Vec<String> {
        FRAME_WARNINGS.with_borrow_mut(std::mem::take)
    }

    /// A getter that spins 8ms (3x the frame target, far under the cap), created `line` lines down.
    fn slow_getter(line: usize) -> String {
        format!("{}return computed({{a}}, function(x) spin(8) return x end):get()", "\n".repeat(line - 1))
    }

    fn eval_slow(lua: &Lua, source: &str) -> i64 {
        lua.load(source).set_name("slow").eval().unwrap()
    }

    #[test]
    fn a_getter_over_the_frame_target_returns_its_value_and_warns_once_per_site() {
        let lua = lua_with_burners();

        assert_eq!(eval_slow(&lua, &slow_getter(1)), 1);
        let first = frame_warnings();
        assert_eq!(first.len(), 1, "one warning for the slow evaluation: {first:?}");
        assert!(
            first[0].contains("signal created at [string \"slow\"]:1"),
            "it names the computed's creation line: {}",
            first[0]
        );

        eval_slow(&lua, &slow_getter(1));
        assert!(frame_warnings().is_empty(), "the same site does not warn twice");

        eval_slow(&lua, &slow_getter(2));
        let other = frame_warnings();
        assert!(other.len() == 1 && other[0].contains("\"slow\"]:2"), "a different site warns on its own: {other:?}");
    }

    #[test]
    fn a_reload_lets_the_same_slow_site_warn_again() {
        let lua = lua_with_burners();
        eval_slow(&lua, &slow_getter(1));
        assert_eq!(frame_warnings().len(), 1);

        begin_evaluation(&lua);
        eval_slow(&lua, &slow_getter(1));
        assert_eq!(frame_warnings().len(), 1, "an edited config's slow getter must not stay silenced");
    }

    #[test]
    fn a_cheap_outer_computed_over_an_expensive_inner_one_blames_the_inner() {
        let lua = lua_with_burners();
        lua.load(
            "local inner = computed({a}, function(x) spin(8) return x end)\n\
             local outer = computed({inner}, function(v) return v end)\n\
             return outer:get()",
        )
        .set_name("nested")
        .eval::<i64>()
        .unwrap();

        let warned = frame_warnings();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("\"nested\"]:1"), "the inner computed, line 1, spent the time: {}", warned[0]);
    }

    #[test]
    fn a_handler_that_spins_in_its_own_body_blames_no_signal() {
        let lua = lua_with_burners();
        let handler: Function = lua
            .load("local cheap = computed({a}, function(x) return x end) return function() spin(8) return cheap:get() end")
            .eval()
            .unwrap();

        CpuBudget::call::<_, i64>(&lua, &handler, ()).unwrap();

        let warned = frame_warnings();
        assert!(warned.len() == 1 && warned[0].contains("outside any signal"), "{warned:?}");
    }

    #[test]
    fn native_time_has_no_instructions_to_sample_and_says_so() {
        let lua = lua_with_burners();
        lua.load("return computed({a}, function(x) burn(8) return x end):get()").eval::<i64>().unwrap();

        let warned = frame_warnings();
        assert!(warned.len() == 1 && warned[0].contains("native work"), "{warned:?}");
    }

    #[test]
    fn a_getter_under_the_frame_target_does_not_warn() {
        let lua = lua_with_burners();
        lua.load("return computed({a}, function(x) spin(1) return x end):get()").eval::<i64>().unwrap();
        assert!(frame_warnings().is_empty());
    }

    #[test]
    fn a_runaway_raises_and_logs_no_frame_warning() {
        let lua = lua_with_burners();
        lua.load("return computed({a}, function(x) while true do end end):get()").eval::<i64>().unwrap_err();
        assert!(frame_warnings().is_empty(), "one fault, one message");
    }

    fn clock_reads() -> usize {
        CLOCK_READS.with(Cell::get)
    }

    #[test]
    fn short_getters_read_the_cpu_clock_zero_times() {
        // Short getters end inside one hook batch, so none reads the CPU clock.
        const GETTERS: usize = 500;
        let lua = lua_with_signal("a", Value::Integer(7));
        let before = clock_reads();
        let sum: i64 = lua
            .load(format!(
                "local n = 0 for _ = 1, {GETTERS} do n = n + a:map(function(v) return v end):get() end return n"
            ))
            .eval()
            .unwrap();
        // The loop itself ticks the hook, but only inside no budget: those ticks find no deadline.
        assert_eq!(sum, 7 * GETTERS as i64);
        assert_eq!(clock_reads() - before, 0, "getters ending before a hook tick must not read the CPU clock");
    }

    #[test]
    fn a_long_getter_anchors_the_cpu_clock_and_a_runaway_still_names_its_error() {
        let lua = lua_with_signal("a", Value::Integer(1));
        let before = clock_reads();
        let n: i64 = lua
            // About 40 hook batches, and a tenth of the cap: a debug build under a loaded suite ran
            // 100000 past it.
            .load("return a:map(function(v) local n = 0 for i = 1, 10000 do n = n + 1 end return n end):get()")
            .eval()
            .unwrap();
        assert_eq!(n, 10_000);
        assert!(clock_reads() - before >= 1, "the first hook tick must anchor the CPU clock");

        let err = lua.load("return computed({a}, function(x) while true do end end):get()").eval::<i64>().unwrap_err();
        assert!(err.to_string().contains(CPU_CAP_EXCEEDED), "the runaway must name the CPU budget: {err}");
    }

    #[test]
    fn one_long_native_call_in_a_getter_is_still_stopped_at_the_budget() {
        // No hook tick runs before or during the call, so only the call's own entry can anchor.
        let lua = lua_with_signal("a", Value::Integer(1));
        crate::lua::fuzzy::register(&lua).unwrap();
        let call = r#"return fuzzy(haystack, "bbbbbbbbbbbbbbbbbbbbbbbb")"#;
        // Sized by measurement so it outlasts the cap at any optimisation level: double until one
        // call costs 2x the cap in thread CPU.
        let mut len = 50_000;
        loop {
            lua.globals().set("haystack", "ab".repeat(len)).unwrap();
            let start = thread_cpu_time().unwrap_or_default();
            lua.load(call).eval::<Value>().unwrap();
            if thread_cpu_time().unwrap_or_default() - start >= 2 * RUNAWAY_CAP {
                break;
            }
            len *= 2;
        }
        let err =
            lua.load(format!("return computed({{a}}, function(x) {call} end):get()")).eval::<Value>().unwrap_err();
        assert!(err.to_string().contains(CPU_CAP_EXCEEDED), "the long call must be stopped and named: {err}");
    }

    #[test]
    fn a_getter_runs_with_the_collection_deferred_and_pays_for_its_garbage_after() {
        let lua = lua_with_signal("a", Value::Integer(1));
        let finalizer = "setmetatable({}, { __gc = function() collected = true end })";
        let inside: bool = lua
            .load(format!("return computed({{a}}, function() {finalizer} for i = 1, 64 do local _ = string.rep(i, 1 << 16) end return collected == true end):get()"))
            .eval()
            .unwrap();
        assert!(!inside, "collection inside a getter is charged to its budget");

        // From a finished cycle, small getters must not start the next one (ADR-0372).
        let collected: bool = lua
            .load(format!(
                "pad = string.rep('x', 1 << 20) collected = nil collectgarbage() {finalizer}
                for i = 1, 100 do computed({{a}}, function() return {{ i }} end):get() end return collected == true"
            ))
            .eval()
            .unwrap();
        assert!(!collected, "a small getter started a collection");

        // From a finished cycle, one getter drops ~8 MiB. Paid on exit, a whole cycle runs and
        // frees it; unpaid, it waits for later allocations to collect.
        lua.gc_collect().unwrap();
        let before = lua.used_memory();
        lua.load("computed({a}, function() local g = {} for i = 1, 8 do g[i] = string.rep(i, 1 << 20) end return #g end):get()")
            .exec()
            .unwrap();
        let grown = lua.used_memory().saturating_sub(before);
        assert!(grown < 1 << 20, "the getter's garbage must be collected as it returns, {grown} bytes left");
    }

    #[test]
    fn a_config_that_stopped_the_collector_keeps_it_stopped_across_a_getter() {
        let lua = lua_with_signal("a", Value::Integer(1));
        lua.load("collectgarbage('stop') computed({a}, function() return 1 end):get()").exec().unwrap();
        assert!(!lua.gc_is_running());
    }
}

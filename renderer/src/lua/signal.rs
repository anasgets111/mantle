//! `Signal`: `get`, `map`, `set`, `computed(dependencies, fn)`, and `state(name, initial)`
//! (ADR-0044 decision 5). Rust owns the userdata; `computed` calls `fn` with dependency values, not
//! handles, so its body does not call `:get()` on declared deps.
//!
//! ponytail: `computed`/`map` recompute on every layout pass, with no invalidation graph across
//! passes. [`EvaluationMemo`] collapses repeats *within* one pass; nothing caches *between* them,
//! so the Watcher still decides when a value goes stale.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;

use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use crate::lua::marshal;

/// CPU runtime is capped at 5ms per evaluation.
const CPU_CAP: Duration = Duration::from_millis(5);

/// Whole-`Scene::apply` cap, not per getter. It must exceed legitimate passes that run every getter
/// and block on shaping per text measurement. A 2000-sibling row (up to 4000) measured release
/// 202ms/298ms and debug 550ms/1.10s; 2s is ~2x worst and ~300x ADR-0069's 6.14ms 500-row list.
/// A 250ms cap refused that real config.
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
struct Deadline {
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

/// Max live [`Signal::get_value`] calls before `mlua::Error`, matching `layout::scene`'s
/// `MAX_TREE_DEPTH`. [`CpuBudget::enter`] wraps dependency resolution, so it bounds body recursion
/// and chains (`s:map(f):map(g):...`). A 200-link chain reaches depth 200 with Rust depth 1 if the
/// deadline wraps only the closure; 5000 links abort with `fatal runtime error: stack overflow`,
/// beyond the 5ms hook because the chain does no Lua work. 32 leaves headroom; scene measurements
/// set both constants.
const MAX_SIGNAL_NESTING_DEPTH: usize = 32;

/// Shared error for hook and [`CpuBudget::check_not_exceeded`] gates.
///
/// Deliberately names no construct. [`CpuBudget`] also wraps
/// `capability::CapabilityHandle::notify_change`'s handlers, so the old "computed/map exceeded ..."
/// wording made an `on_change` handler report itself as a `map` it never called. Each call site
/// already prefixes what it was doing ("Signal getter failed: ...", "`on_change` handler raised,
/// ignoring it: ..."), and the hook cannot tell which of them it interrupted.
const CPU_CAP_EXCEEDED: &str = "exceeded the 5ms CPU budget for one evaluation";

/// Distinct pass-budget error so config knows which limit it hit. Plain `__index` without a signal
/// reaches it, the hole this budget closes.
const LAYOUT_PASS_CAP_EXCEEDED: &str = "the layout pass exceeded its 2s CPU budget";

/// Globally unique identifier for a reactive cell, avoiding pointer recycling issues (ADR-0170).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct CellId(u64);

fn next_cell_id() -> CellId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    CellId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

#[derive(Clone)]
enum SignalKind {
    /// `computed`/`map`. Function and sources are user values, not fields, so a cycle through a
    /// config table stays collectable (ADR-0221); `Delayed`/`Pulse` keep their source and values the
    /// same way, leaving only their clocks in Rust.
    Computed { id: MemoKey, arity: usize },
    /// A `Computed`, `Delayed` or `Pulse` read out of its userdata by [`from_userdata`], carrying
    /// the handle to its user values. Lives only as long as the read that made it.
    Derived(mlua::AnyUserData),
    /// Rust-overwritable value (`Signal::new_live`/`LiveSignalHandle`). `Rc<RefCell<_>>` because
    /// the Loader stays on one Wayland dispatch thread (ADR-0039).
    Live { id: CellId, cell: Rc<RefCell<Value>> },
    /// Engine-written, config-read boolean from `hover(name)` (ADR-0062), separate from `Live` so
    /// only `hover_handle` can write it and `hover = mantle.network` gets no writer. `paired_rect`
    /// links the boolean to `hover_rect(name)`'s cell; the rect half has `None` and is not a
    /// trigger.
    Hover { id: CellId, cell: Rc<RefCell<Value>>, paired_rect: Option<(CellId, Rc<RefCell<Value>>)>, dirty: DirtyFlag },
    /// Scroll offset in logical pixels (ADR-0069), written by the wheel handler and layout clamp.
    /// Separate from `Hover` so only `scroll_handle` writes it; `scroll = mantle.network` cannot
    /// overwrite a capability snapshot.
    Scroll {
        id: CellId,
        cell: Rc<RefCell<Value>>,
        dirty: DirtyFlag,
        /// One-shot 1-based child request from `signal:reveal(index)` (ADR-0112), consumed by the
        /// next viewport positioning pass. Separate from offset because only that pass knows child
        /// position and viewport height.
        reveal: Rc<Cell<Option<usize>>>,
    },
    /// Lua-authored writable state (ADR-0044 decision 5), built by `state`. Separate from `Live`
    /// even with identical storage: accepting `set` on `Live` would let config overwrite a pushed
    /// network SSID. The kind makes read-only capabilities a type-system fact. Carries the shared
    /// dirty flag because `set` has no `RendererClient` in reach.
    State { id: CellId, cell: Rc<RefCell<Value>>, dirty: DirtyFlag },
    /// `geometry(name)` (ADR-0147): the laid-out `{ x, y, width, height }` of the node declaring
    /// `geometry = geometry(name)`, in its surface's logical coordinates, the same space `on_click`
    /// and `hover_rect` report. Written by the layout pass and by a tween tick, never by Lua, and
    /// written quietly: a read sees the last layout, and a binding on it settles one pass later
    /// rather than dirtying the scene it was measured in.
    Geometry(CellId, Rc<RefCell<Value>>),
    /// `delay(signal, ms)` (ADR-0146): follows `source` once it has held a new value for `hold`.
    /// Pull-based like everything else here: a read notes the pending value and its due time,
    /// arms the poll loop's one timeout through [`WakeDeadline`], and keeps answering the held
    /// value until a read after the due time adopts the new one. A source that returns to the
    /// held value before then cancels the change, which makes this a trailing debounce as well
    /// as a close-hold.
    Delayed { hold: Duration, due: Rc<Cell<Option<Instant>>> },
    /// `pulse(signal, ms)` (ADR-0153): `true` for `ms` after `source` changes value, `false`
    /// otherwise. The other half of [`SignalKind::Delayed`]'s shape and the same machinery -- that
    /// one answers the old value until a change settles, this one says a change just happened --
    /// and it is what fires a one-shot animation, which a config has no way to call `restart()` on
    /// (ADR-0152). Pull-based: a read compares against the value it last saw, arms the wake, and
    /// falls back to `false` on the read after the window closes.
    Pulse { hold: Duration, until: Rc<Cell<Option<Instant>>> },
}

struct DelayCell {
    held: Value,
    pending: Option<(Value, Instant)>,
}

struct PulseCell {
    /// The source value this signal last read. Seeded at construction, so a pulse starts low and
    /// fires on the first change rather than on the pass that built it.
    seen: Value,
    /// When the window closes, while one is open.
    until: Option<Instant>,
}

/// The earliest moment a clock-driven signal has to be re-read -- a `delay`'s hold coming due or
/// a `pulse`'s window closing -- read by the poll loop as its timeout; `None` keeps the loop
/// timeout-free (ADR-0124). One slot, not a list: a due wake dirties the scene, the pass re-reads
/// every such signal, and each one still pending re-arms itself.
#[derive(Default)]
struct WakeDeadline(Option<Instant>);

fn arm_wake(lua: &Lua, due: Instant) {
    let mut slot = super::app_data_or_default::<WakeDeadline>(lua);
    slot.0 = Some(slot.0.map_or(due, |current| current.min(due)));
}

/// When the poll loop has to wake for a pending `delay` or `pulse`, if any.
pub fn next_wake_deadline(lua: &Lua) -> Option<Instant> {
    lua.app_data_ref::<WakeDeadline>().and_then(|slot| slot.0)
}

/// Clears a due deadline and says so; the caller dirties the scene. Not due, or none, is `false`.
pub fn take_due_wake(lua: &Lua, now: Instant) -> bool {
    let Some(mut slot) = lua.app_data_mut::<WakeDeadline>() else { return false };
    if slot.0.is_some_and(|due| due <= now) {
        slot.0 = None;
        return true;
    }
    false
}

impl SignalKind {
    /// Name used in [`Signal::set`] refusal messages.
    fn describe(&self) -> &'static str {
        match self {
            SignalKind::Computed { .. } | SignalKind::Derived(_) => "a computed",
            SignalKind::Live { .. } => "a capability",
            SignalKind::Hover { .. } => "a hover",
            SignalKind::Scroll { .. } => "a scroll",
            SignalKind::State { .. } => "a state",
            SignalKind::Delayed { .. } => "a delayed",
            SignalKind::Pulse { .. } => "a pulse",
            SignalKind::Geometry(..) => "a geometry",
        }
    }
}

/// Applies `marshal.rs` checks to Lua-authored `Number`/`Integer`/`String`; other shapes pass
/// unchanged. Shared by `new_state` and `set`; `new_live` receives Rust data.
fn check_lua_authored(value: &Value) -> Result<(), marshal::MarshalError> {
    match value {
        Value::Number(n) => {
            marshal::check_number(*n)?;
        }
        Value::Integer(i) => {
            marshal::check_integer(*i)?;
        }
        Value::String(s) => {
            let len = s.as_bytes().len();
            if len > marshal::MAX_STRING_BYTES {
                return Err(marshal::MarshalError::StringTooLong { len });
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether a `state` literal is comparable across evaluations. Tables, functions, and userdata use
/// pointer identity; fresh values would always look edited.
fn is_comparable_literal(value: &Value) -> bool {
    matches!(value, Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_) | Value::String(_))
}

/// Whether the `state` literal changed. `None` means no edit per ADR-0044's amendment. This keeps
/// a table literal, such as a popup's anchor rect, from looking edited on every reload and snapping
/// the popup to the corner. `Value`'s own `PartialEq` compares an `Integer` against a `Number` the way
/// Lua `==` does, so `0` and `0.0` match; scalar types differing do not.
fn literal_was_edited(current: &Value, seeded: &Value) -> Option<bool> {
    if !is_comparable_literal(current) || !is_comparable_literal(seeded) {
        return None;
    }
    Some(current != seeded)
}

/// Read-only reactive value: plain (`State`/Rust-pushed) or recomputed Lua closure (`Computed`).
#[derive(Clone)]
pub struct Signal(SignalKind);

impl Signal {
    /// Signal behind `state(name, initial)` (ADR-0044 decision 5), writable through `set`, which
    /// marks `dirty`; `initial` is Lua-authored and marshal-checked, unlike `new_live`.
    pub fn new_state(initial: Value, dirty: DirtyFlag) -> Result<Self, marshal::MarshalError> {
        check_lua_authored(&initial)?;
        Ok(Signal(SignalKind::State { id: next_cell_id(), cell: Rc::new(RefCell::new(initial)), dirty }))
    }

    /// Replaces a state value when the config changed its literal (ADR-0044 amendment), so the file
    /// wins over prior `set`; marks the same dirty flag. Only `State` belongs to the registry; any
    /// other kind is a caller bug, not config error.
    pub fn reseed(&self, value: Value) -> Result<(), marshal::MarshalError> {
        check_lua_authored(&value)?;
        let SignalKind::State { id, cell, dirty } = &self.0 else {
            debug_assert!(false, "reseed on {} signal, which the state registry cannot hold", self.0.describe());
            return Ok(());
        };
        *cell.borrow_mut() = value;
        dirty.mark_cell(*id);
        Ok(())
    }

    /// Rust-pushed signal via [`LiveSignalHandle`]. Values are serde-serialized Rust data, so Lua
    /// marshalling checks cannot find NaN/Inf/oversized strings. Every live signal shares one
    /// generation dirty flag, whose clone `renderer/src/socket/client.rs`'s `RendererClient` drains.
    pub fn new_live(initial: Value, dirty: DirtyFlag) -> (Self, LiveSignalHandle) {
        let id = next_cell_id();
        let cell = Rc::new(RefCell::new(initial));
        (Signal(SignalKind::Live { id, cell: Rc::clone(&cell) }), LiveSignalHandle(id, cell, dirty))
    }

    /// Boolean written by `wl_pointer`, read-only to Lua (ADR-0062 decision 2). Starts `false`, not
    /// nil, because `visible` treats nil as absent (ADR-0044 decision 1 amendment). `initial_rect`
    /// must be a real non-zero 1x1 table: tooltips require a non-zero `anchor_rect` before any
    /// pointer event, and this constructor lacks a Lua to build the table.
    pub fn new_hover(dirty: DirtyFlag, initial_rect: Value) -> (Self, Self) {
        let over_id = next_cell_id();
        let rect_id = next_cell_id();
        let over = Rc::new(RefCell::new(Value::Boolean(false)));
        let rect = Rc::new(RefCell::new(initial_rect));
        (
            Signal(SignalKind::Hover {
                id: over_id,
                cell: Rc::clone(&over),
                paired_rect: Some((rect_id, Rc::clone(&rect))),
                dirty: dirty.clone(),
            }),
            Signal(SignalKind::Hover { id: rect_id, cell: rect, paired_rect: None, dirty }),
        )
    }

    /// Scroll offset starting at top (ADR-0069 decision 2). Plain number, not a hover-like pair: no
    /// scrollbar uses content extent yet, so the first such config can define its shape.
    pub fn new_scroll(dirty: DirtyFlag) -> Self {
        Signal(SignalKind::Scroll {
            id: next_cell_id(),
            cell: Rc::new(RefCell::new(Value::Number(0.0))),
            dirty,
            reveal: Rc::new(Cell::new(None)),
        })
    }

    /// Requests the next positioning pass scroll visible child `index` (1-based) into view, marking
    /// dirty (ADR-0112). Other kinds return false for `signal:reveal()`'s named refusal.
    pub(crate) fn request_reveal(&self, index: usize) -> bool {
        let SignalKind::Scroll { id, reveal, dirty, .. } = &self.0 else { return false };
        reveal.set(Some(index));
        dirty.mark_cell(*id);
        true
    }

    /// Consumes the reveal in `layout::scene`'s positioning pass, so a later wheel event does not
    /// fight an already honored request.
    pub(crate) fn take_reveal(&self) -> Option<usize> {
        if let SignalKind::Scroll { reveal, .. } = &self.0 { reveal.take() } else { None }
    }

    /// Scroll write end for wheel and positioning clamp; `None` for other kinds keeps wheels off
    /// capability signals.
    pub(crate) fn scroll_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Scroll { id, cell, dirty, .. } = &self.0 else { return None };
        Some(LiveSignalHandle(*id, Rc::clone(cell), dirty.clone()))
    }

    /// Scroll offset without `Lua`: `layout::scene` clamps deep in a pass holding no VM reference,
    /// and threading one through every layout frame just to read a `RefCell` would add a parameter.
    pub(crate) fn scroll_offset(&self) -> Option<f32> {
        let SignalKind::Scroll { cell, .. } = &self.0 else { return None };
        Some(match *cell.borrow() {
            Value::Number(n) => n as f32,
            Value::Integer(n) => n as f32,
            _ => 0.0,
        })
    }

    /// Geometry write end for `layout::scene`; `None` for other kinds, so `geometry = hover(...)`
    /// or a state signal is inert rather than overwritten.
    pub(crate) fn geometry_cell(&self) -> Option<(CellId, Rc<RefCell<Value>>)> {
        if let SignalKind::Geometry(id, cell) = &self.0 { Some((*id, Rc::clone(cell))) } else { None }
    }

    /// Hover write end for `crate::wayland`; `None` for other kinds by design.
    pub(crate) fn hover_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Hover { id, cell, dirty, .. } = &self.0 else { return None };
        Some(LiveSignalHandle(*id, Rc::clone(cell), dirty.clone()))
    }

    /// Rect write end for the boolean hover half: last node position in surface logical
    /// coordinates, consumed by tooltip `popup.anchor_rect`. `None` for other kinds and the rect
    /// half itself.
    pub(crate) fn hover_rect_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Hover { paired_rect: Some((rect_id, rect)), dirty, .. } = &self.0 else { return None };
        Some(LiveSignalHandle(*rect_id, Rc::clone(rect), dirty.clone()))
    }

    /// Reactive cell identifier for targeted invalidation tracking, if this signal is backed by a cell.
    pub(crate) fn cell_id(&self) -> Option<CellId> {
        match &self.0 {
            SignalKind::Live { id, .. }
            | SignalKind::Hover { id, .. }
            | SignalKind::Scroll { id, .. }
            | SignalKind::State { id, .. }
            | SignalKind::Geometry(id, _) => Some(*id),
            _ => None,
        }
    }

    /// `map(f)` as a one-dependency `Computed`, recomputed on every read (ADR-0044 decision 3).
    /// Shared by
    /// Lua and Rust so `lua::capability::Capability` makes `mantle.lock` read like bare
    /// capabilities.
    pub(crate) fn mapped(lua: &Lua, source: mlua::AnyUserData, func: Function) -> mlua::Result<mlua::AnyUserData> {
        new_derived(lua, SignalKind::Computed { id: next_computed_id(), arity: 1 }, Some(func), vec![source])
    }

    /// Reads current value (ADR-0044 decision 1). `layout::node` uses it to resolve signal
    /// userdata; `&Lua`
    /// is threaded because `Computed` needs it for [`CpuBudget`] and mlua 0.12 cannot recover Lua
    /// from `AnyUserData`/`Value`.
    pub(crate) fn get_value(&self, lua: &Lua) -> mlua::Result<Value> {
        match &self.0 {
            SignalKind::Live { id, cell }
            | SignalKind::Hover { id, cell, .. }
            | SignalKind::Scroll { id, cell, .. }
            | SignalKind::State { id, cell, .. }
            | SignalKind::Geometry(id, cell) => {
                note_read(lua, *id);
                Ok(cell.borrow().clone())
            }
            SignalKind::Computed { .. } | SignalKind::Delayed { .. } | SignalKind::Pulse { .. } => Err(
                mlua::Error::runtime("a derived signal was read without the userdata holding its function and sources"),
            ),
            SignalKind::Derived(ud) => read_derived(lua, ud),
        }
    }
}

/// User value holding a `Computed`'s function; its sources, or a `Delayed`/`Pulse` source, follow.
const FUNCTION_SLOT: usize = 1;
const FIRST_SOURCE_SLOT: usize = 2;
/// A `Delayed`'s held value or a `Pulse`'s last-seen one, after the source; then a pending value.
const HELD_SLOT: usize = FIRST_SOURCE_SLOT + 1;
const PENDING_SLOT: usize = HELD_SLOT + 1;

/// A derived signal's userdata, with `func` and `sources` as user values rather than Rust fields
/// (see [`SignalKind::Computed`]).
fn new_derived(
    lua: &Lua,
    kind: SignalKind,
    func: Option<Function>,
    sources: Vec<mlua::AnyUserData>,
) -> mlua::Result<mlua::AnyUserData> {
    let ud = lua.create_userdata(Signal(kind))?;
    if let Some(func) = func {
        ud.set_nth_user_value(FUNCTION_SLOT, func)?;
    }
    for (offset, source) in sources.into_iter().enumerate() {
        ud.set_nth_user_value(FIRST_SOURCE_SLOT + offset, source)?;
    }
    Ok(ud)
}

fn source_at(ud: &mlua::AnyUserData, slot: usize) -> mlua::Result<Signal> {
    let source: mlua::AnyUserData = ud.nth_user_value(slot)?;
    from_userdata(&source).ok_or_else(|| mlua::Error::runtime("a derived signal's source is not a signal"))
}

fn read_derived(lua: &Lua, ud: &mlua::AnyUserData) -> mlua::Result<Value> {
    let kind = ud.borrow::<Signal>()?.0.clone();
    match kind {
        // Both recurse into their source, so both claim a nesting level for the reason
        // `Computed` does. Unguarded, a long enough chain exhausted the Rust stack and
        // aborted `mantle check` before any cap could answer.
        SignalKind::Delayed { hold, due } => {
            let _budget = CpuBudget::enter(lua)?;
            let fresh = source_at(ud, FIRST_SOURCE_SLOT)?.get_value(lua)?;
            let pending = due.get().map(|at| mlua::Result::Ok((ud.nth_user_value(PENDING_SLOT)?, at))).transpose()?;
            let mut cell = DelayCell { held: ud.nth_user_value(HELD_SLOT)?, pending };
            let answer = cell.follow(fresh, hold, Instant::now(), |at| arm_wake(lua, at));
            let (pending, at) = cell.pending.unzip();
            due.set(at);
            ud.set_nth_user_value(HELD_SLOT, cell.held)?;
            ud.set_nth_user_value(PENDING_SLOT, pending)?;
            Ok(answer)
        }
        SignalKind::Pulse { hold, until } => {
            let _budget = CpuBudget::enter(lua)?;
            let fresh = source_at(ud, FIRST_SOURCE_SLOT)?.get_value(lua)?;
            let mut cell = PulseCell { seen: ud.nth_user_value(HELD_SLOT)?, until: until.get() };
            let open = cell.fire(fresh, hold, Instant::now(), |at| arm_wake(lua, at));
            until.set(cell.until);
            ud.set_nth_user_value(HELD_SLOT, cell.seen)?;
            Ok(Value::Boolean(open))
        }
        SignalKind::Computed { id, arity } => {
            // A repeat within this evaluation costs one hash lookup and no Lua. Checked before
            // `CpuBudget::enter` on purpose: a hit does no work, so it must not spend a nesting
            // level either, or a wide diamond would hit `MAX_SIGNAL_NESTING_DEPTH` on cache
            // hits alone.
            if let Some(hit) = EvaluationMemo::get(lua, id) {
                for &cell in &hit.cells {
                    note_read(lua, cell);
                }
                return Ok(hit.value);
            }

            // Enter before dependency resolution, not only `func.call`, so nesting depth also
            // bounds dependency chains.
            let budget = CpuBudget::enter(lua)?;
            // Opened by whichever `Computed` is outermost and dropped when it returns, so the
            // memo spans exactly one evaluation. It deliberately does not span a
            // `capability::CapabilityHandle::notify_change` handler: that handler may `:set()`
            // between its own `:get()` calls and has to observe its own writes.
            let _memo = EvaluationMemo::enter(lua);
            let frame = ComputedFrame::enter(lua);

            let mut args = Vec::with_capacity(arity);
            for slot in FIRST_SOURCE_SLOT..FIRST_SOURCE_SLOT + arity {
                args.push(source_at(ud, slot)?.get_value(lua)?);
            }
            let func: Function = ud.nth_user_value(FUNCTION_SLOT)?;
            let value = func.call::<Value>(MultiValue::from_vec(args))?;
            budget.check_not_exceeded()?;
            let cells = frame.finish();
            EvaluationMemo::insert(lua, id, &value, cells);
            Ok(value)
        }
        other => Signal(other).get_value(lua),
    }
}

impl DelayCell {
    /// One read: the value to answer now, and whether to arm a wake for later. Split from the
    /// signal so the clock is a parameter.
    fn follow(&mut self, fresh: Value, hold: Duration, now: Instant, arm: impl FnOnce(Instant)) -> Value {
        if fresh == self.held {
            self.pending = None;
            return self.held.clone();
        }
        let due = match &self.pending {
            Some((pending, due)) if *pending == fresh => *due,
            _ => now + hold,
        };
        if now >= due {
            self.held = fresh;
            self.pending = None;
        } else {
            self.pending = Some((fresh, due));
            arm(due);
        }
        self.held.clone()
    }
}

impl PulseCell {
    /// One read: whether the window is open now, and whether to arm a wake for its close. Split
    /// from the signal so the clock is a parameter, the way [`DelayCell::follow`] is.
    ///
    /// A change while a window is already open restarts it rather than extending the old one,
    /// which is what `restart()` does to a running `SequentialAnimation`. The window is not
    /// re-armed once it has closed, so a source that holds its new value pulses once.
    fn fire(&mut self, fresh: Value, hold: Duration, now: Instant, arm: impl FnOnce(Instant)) -> bool {
        if fresh != self.seen {
            self.seen = fresh;
            self.until = Some(now + hold);
        }
        if let Some(until) = self.until.filter(|until| now < *until) {
            arm(until);
            true
        } else {
            self.until = None;
            false
        }
    }
}

/// Rust handle for [`Signal::new_live`] storage, used for `StateSnapshot` pushes. Lua reads the
/// latest value, with no memoization.
#[derive(Clone)]
pub struct LiveSignalHandle(CellId, Rc<RefCell<Value>>, DirtyFlag);

impl LiveSignalHandle {
    /// Last value, for `CapabilityHandle::hydrate` to pass as `on_change`'s replaced value.
    pub fn get(&self) -> Value {
        self.1.borrow().clone()
    }

    /// Writes and marks the cell dirty.
    pub fn set(&self, value: Value) {
        *self.1.borrow_mut() = value;
        self.2.mark_cell(self.0);
    }

    /// Writes without dirtying for `layout::scene`'s clamp, which derives the value from geometry
    /// just measured. Another pass would observe the same idempotent clamp; cost is one frame of
    /// staleness only when clamping: same-pass `scroll("x")` sees wheel input, derived readouts see
    /// the clamped value next pass. Positioning itself uses the clamped value immediately.
    pub(crate) fn set_quiet(&self, value: Value) {
        *self.1.borrow_mut() = value;
    }

    /// [`Self::set`] with equality deduplication. ADR-0062 decision 4 calls it for every
    /// device-rate `wl_pointer` motion; compare first to re-resolve only on boundary crossings.
    pub fn set_changed(&self, value: Value) -> bool {
        let unchanged = *self.1.borrow() == value;
        if unchanged {
            return false;
        }
        self.set(value);
        true
    }
}

/// Which surfaces an invalidation marks dirty.
#[derive(Debug, PartialEq, Eq)]
pub enum DirtyScope {
    /// No change since the last take.
    Clean,
    /// Scene-wide change or unknown cell dependency; every surface must re-resolve.
    All,
    /// Targeted set of instance IDs whose nodes actually read the modified cells.
    Instances(Vec<String>),
}

#[derive(Default)]
struct DirtyState {
    all: bool,
    cells: rustc_hash::FxHashSet<CellId>,
}

/// Shared invalidation flag tracking scene-wide or cell-targeted dirty marks.
#[derive(Clone, Default)]
pub struct DirtyFlag(Rc<RefCell<DirtyState>>);

impl DirtyFlag {
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(DirtyState::default())))
    }

    /// `configure` changing one surface's size invalidates resolved geometry like a capability
    /// push;
    /// `RendererClient::set_instance_size` marks this flag rather than adding a second mechanism
    /// (ADR-0044 decision 2).
    pub(crate) fn mark(&self) {
        self.0.borrow_mut().all = true;
    }

    /// Marks a specific reactive cell dirty.
    pub(crate) fn mark_cell(&self, id: CellId) {
        self.0.borrow_mut().cells.insert(id);
    }

    /// Reads and clears atomically: drain inbound frames, then re-resolve once
    /// (ADR-0044 decision 2).
    pub fn take(&self) -> bool {
        let mut state = self.0.borrow_mut();
        if state.all || !state.cells.is_empty() {
            *state = DirtyState::default();
            true
        } else {
            false
        }
    }

    /// Takes the invalidation scope: Clean, All, or targeted Instances based on ReadTracker.
    pub fn take_scope(&self, lua: &Lua) -> DirtyScope {
        let mut state = self.0.borrow_mut();
        if !state.all && state.cells.is_empty() {
            return DirtyScope::Clean;
        }
        if state.all {
            *state = DirtyState::default();
            return DirtyScope::All;
        }
        let cells = std::mem::take(&mut state.cells);
        let tracker = lua.app_data_ref::<ReadTracker>();
        let mut instances = rustc_hash::FxHashSet::default();
        if let Some(tracker) = tracker {
            for cell_id in cells {
                if let Some(readers) = tracker.cell_readers.get(&cell_id) {
                    for reader in readers {
                        instances.insert(reader.to_string());
                    }
                }
            }
        }
        if instances.is_empty() { DirtyScope::Clean } else { DirtyScope::Instances(instances.into_iter().collect()) }
    }
}

impl UserData for Signal {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Functions, not methods: a derived signal's read needs the userdata its user values hang
        // off, which a method's `&Self` has lost.
        methods.add_function("get", |lua, ud: mlua::AnyUserData| read(lua, &ud));
        methods.add_function("map", |lua, (ud, f): (mlua::AnyUserData, Function)| Signal::mapped(lua, ud, f));
        // ADR-0112: config requests a child, not a pixel offset; the pass owns pixels
        // (ADR-0069 decision 2).
        methods.add_method("reveal", |_, this, index: i64| {
            let Some(index) = usize::try_from(index).ok().filter(|index| *index >= 1) else {
                return Err(mlua::Error::runtime(format!(
                    "signal:reveal() takes a 1-based child index, and {index} is not one"
                )));
            };
            if !this.request_reveal(index) {
                return Err(mlua::Error::runtime(format!(
                    "signal:reveal() is only valid on a scroll(name) signal, and this is {} signal",
                    this.0.describe()
                )));
            }
            Ok(())
        });
        // ADR-0044 decision 5's only Lua write path. Other kinds refuse by name, so
        // `network:set(...)`
        // says why.
        methods.add_method("set", |_, this, value: Value| {
            let SignalKind::State { id, cell, dirty } = &this.0 else {
                return Err(mlua::Error::runtime(format!(
                    "signal:set() is only valid on a state(name, initial) signal, and this is {} signal: every other signal kind is read-only to Lua (ADR-0044 decision 5)",
                    this.0.describe()
                )));
            };
            // Check before writing; refusal preserves the value and dirty flag, matching
            // `new_state`.
            check_lua_authored(&value).map_err(|err| {
                mlua::Error::runtime(format!("signal:set() refused its value at the marshalling boundary: {err}"))
            })?;
            *cell.borrow_mut() = value;
            dirty.mark_cell(*id);
            Ok(())
        });
    }
}

/// Applies an `mantle set`/`toggle` to named `state` (ADR-0112), using `set`'s marshalling and
/// dirty checks. Refuses missing state or non-boolean toggle values, the two keybind/config
/// mismatches.
pub fn write_state(lua: &Lua, set: &shared::SetState) -> Result<(), String> {
    let (signal, initial) = lua
        .app_data_ref::<StateRegistry>()
        .and_then(|registry| registry.0.get(&set.name).cloned())
        .ok_or_else(|| format!("this config declares no state({:?}, ...)", set.name))?;
    let value = match &set.write {
        shared::StateWrite::Set(json) => {
            crate::lua::json::to_lua(lua, json).map_err(|err| format!("the value does not convert to Lua: {err}"))?
        }
        shared::StateWrite::Toggle => match signal.get_value(lua) {
            Ok(Value::Boolean(current)) => Value::Boolean(!current),
            Ok(other) => return Err(format!("it holds {}, and only a boolean toggles", other.type_name())),
            Err(err) => return Err(format!("its value could not be read: {err}")),
        },
        // Back to the declared initial when it already holds the value: the scalar comparison
        // `literal_was_edited` makes, so `1` and `1.0` are the same value and a table never is.
        shared::StateWrite::ToggleTo(json) => {
            let wanted = crate::lua::json::to_lua(lua, json)
                .map_err(|err| format!("the value does not convert to Lua: {err}"))?;
            let current = signal.get_value(lua).map_err(|err| format!("its value could not be read: {err}"))?;
            if literal_was_edited(&current, &wanted) == Some(false) { initial } else { wanted }
        }
    };
    signal.reseed(value).map_err(|err| format!("refused at the marshalling boundary: {err}"))
}

/// Whether config called `hover(name)`. `crate::wayland` checks first, so configs without tooltip
/// or hover expansion pay no tree clone, walk, or signal writes at pointer-report rate.
pub fn any_hover_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<HoverRegistry>().is_some_and(|registry| !registry.0.is_empty())
}

/// ADR-0044 decision 5 state registry: name preserves last-click values across in-place reloads;
/// the stored literal detects an edited initial, which wins over live state (the wallpaper case).
/// In `Lua::set_app_data`, so ADR-0044 decision 4's persistent VM preserves it and a replaced
/// Renderer starts without it.
#[derive(Default)]
struct StateRegistry(HashMap<String, (Signal, Value)>);

/// `hover(name)` registry (ADR-0062 decision 2), name-keyed across reloads so a tooltip stays open
/// through
/// config edits. Separate from [`StateRegistry`], or `state("volume", 0)` and
/// `hover("volume")` would collide and confuse `signal:set()`.
#[derive(Default)]
struct HoverRegistry(HashMap<String, (Signal, Signal)>);

/// Name-keyed `scroll(name)` registry; reload preserves the user's offset and avoids jumping an
/// open panel to top (ADR-0069 decision 2).
#[derive(Default)]
struct ScrollRegistry(HashMap<String, Signal>);

/// Name-keyed `geometry(name)` registry, so a reload keeps the last measured rect instead of
/// answering zero until the next pass.
#[derive(Default)]
struct GeometryRegistry(HashMap<String, Signal>);

/// The cells a pass's geometry write changed (ADR-0147 amendment); the client turns them into one
/// follow-up pass over their readers so a binding on the measurement settles, and only one, so a
/// binding that feeds its own measurement cannot spin the loop.
#[derive(Default)]
struct GeometryMoved(Vec<CellId>);

pub(crate) fn note_geometry_moved(lua: &Lua, id: CellId) {
    if let Some(mut moved) = lua.app_data_mut::<GeometryMoved>() {
        moved.0.push(id);
        return;
    }
    lua.set_app_data(GeometryMoved(vec![id]));
}

/// The cells a pass write moved since the last take.
pub fn take_geometry_moved(lua: &Lua) -> Vec<CellId> {
    lua.app_data_mut::<GeometryMoved>().map(|mut moved| std::mem::take(&mut moved.0)).unwrap_or_default()
}

/// One `Computed`'s identity for [`EvaluationMemo`], counted rather than derived from where its
/// dependencies happen to sit in memory.
///
/// The address of the `Rc<Vec<Signal>>` was the first key, on the reasoning that a fresh `Rc` per
/// `computed()` and per [`Signal::mapped`] separates every distinct computed. It does, until one is
/// dropped: the allocator hands the next same-sized `Rc` the address just freed, and the memo --
/// which holds a raw pointer and so keeps nothing alive -- serves the dead computed's value to the
/// live one. The memo's scope is a whole layout pass, and a pass builds and discards computeds
/// constantly: every `:map` in a `list`'s `itemfn`, every one in a surface that rebuilds its tree.
/// On 2026-09-08 that is what put `Integer(0)` into the lock screen's keyboard label and its
/// wallpaper path, from two Lua functions that cannot return an integer at all (ADR-0170).
///
/// A counter cannot be recycled. `Signal::clone` copies the id because a clone is the same computed
/// with the same `func`, which is the one case that must share a memo entry.
type MemoKey = u64;

/// Next unused [`MemoKey`]. `Relaxed` is enough: ids need only differ, and the Loader is one thread
/// (ADR-0039).
fn next_computed_id() -> MemoKey {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone)]
struct MemoEntry {
    value: Value,
    cells: Vec<CellId>,
}

/// Values already produced during the current outermost [`Signal::get_value`].
#[derive(Default)]
struct MemoTable {
    map: FxHashMap<MemoKey, MemoEntry>,
    depth: usize,
    eval_stack: Vec<Vec<CellId>>,
}

struct ComputedFrame<'lua>(&'lua Lua);

impl<'lua> ComputedFrame<'lua> {
    fn enter(lua: &'lua Lua) -> Self {
        EvaluationMemo::push_frame(lua);
        Self(lua)
    }

    fn finish(self) -> Vec<CellId> {
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
struct ReadTracker {
    active_instance: Option<Rc<str>>,
    cell_readers: rustc_hash::FxHashMap<CellId, rustc_hash::FxHashSet<Rc<str>>>,
    instance_cells: rustc_hash::FxHashMap<Rc<str>, rustc_hash::FxHashSet<CellId>>,
}

/// Marks the beginning of an instance's layout resolution, clearing its prior reads.
pub(crate) fn begin_instance_resolve(lua: &Lua, instance_id: &str) {
    let mut tracker = super::app_data_or_default::<ReadTracker>(lua);
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
    tracker.active_instance = Some(instance_rc);
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
    if let Some(mut tracker) = lua.app_data_mut::<ReadTracker>()
        && let Some(instance_id) = tracker.active_instance.as_ref().map(Rc::clone)
    {
        tracker.cell_readers.entry(cell_id).or_default().insert(Rc::clone(&instance_id));
        tracker.instance_cells.entry(instance_id).or_default().insert(cell_id);
    }
    EvaluationMemo::record_dependency(lua, cell_id);
}

/// One evaluation's memo, closing ADR-0044 decision 3's ceiling: without it a shared dependency is
/// re-run once per path that reaches it, so a launcher ran its whole application filter twice for
/// every row's `background` (once directly, once through a computed reading it), and a diamond of
/// depth N evaluated its root 2^N times.
///
/// Scoped to one layout pass, never across them: between two passes a `state`/`Live` cell may have
/// changed, and nothing here observes that. Within the scope the memo also makes an impure closure
/// (`os.clock()`, `math.random`) answer consistently on every path instead of differing by which
/// dependency edge reached it.
///
/// [`LayoutPassBudget`] opens the table, so one pass is the scope whenever a pass is running
/// (ADR-0157): a shared `results` answers once for the pass rather than once for
/// `background`, once for `border_color`, and once for the label colour of every row. Outside a
/// pass -- startup evaluation, a `capability::CapabilityHandle::notify_change` handler -- the
/// outermost `Computed` still owns it, which is what keeps a handler that `:set()`s between its
/// own `:get()`s observing its own writes.
///
/// The cost is that `layout::scene` writes two cells mid-pass, and a derived readout of either now
/// holds the value it had when the pass started rather than depending on where in the tree the
/// reader sits: the `Scroll` clamp ([`LiveSignalHandle::set_quiet`], whose own contract already
/// says a derived readout sees the clamp next pass) and the `geometry(name)` publish (whose move
/// schedules the follow-up pass `Scene::settle_geometry` runs). Both settle on the next pass, and
/// both were previously answered one way above the writer and another way below it.
struct EvaluationMemo<'lua> {
    lua: &'lua Lua,
    owner: bool,
}

impl<'lua> EvaluationMemo<'lua> {
    fn enter(lua: &'lua Lua) -> Self {
        let in_pass = lua.app_data_ref::<PassDeadline>().is_some_and(|slot| slot.0.is_some());
        let owner = if in_pass {
            false
        } else {
            let mut table = super::app_data_or_default::<MemoTable>(lua);
            let owner = table.depth == 0;
            table.depth += 1;
            owner
        };
        Self { lua, owner }
    }

    /// `None` outside an evaluation, which is the outermost `Computed`'s own first look.
    fn get(lua: &Lua, key: MemoKey) -> Option<MemoEntry> {
        lua.app_data_ref::<MemoTable>()?.map.get(&key).cloned()
    }

    fn insert(lua: &Lua, key: MemoKey, value: &Value, cells: Vec<CellId>) {
        if let Some(mut table) = lua.app_data_mut::<MemoTable>() {
            table.map.insert(key, MemoEntry { value: value.clone(), cells });
        }
    }

    fn push_frame(lua: &Lua) {
        if let Some(mut table) = lua.app_data_mut::<MemoTable>() {
            table.eval_stack.push(Vec::new());
        }
    }

    fn pop_frame(lua: &Lua) -> Vec<CellId> {
        let Some(mut table) = lua.app_data_mut::<MemoTable>() else {
            return Vec::new();
        };
        let cells = table.eval_stack.pop().unwrap_or_default();
        if let Some(parent) = table.eval_stack.last_mut() {
            for &c in &cells {
                if !parent.contains(&c) {
                    parent.push(c);
                }
            }
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
struct PassDeadline(Option<Deadline>);

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
    /// opens the [`EvaluationMemo`] for the pass: a computed then answers once for every node and
    /// property that reads it, instead of once per property (ADR-0157).
    pub(crate) fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        super::app_data_or_default::<PassDeadline>(lua).0 = Some(Deadline::lasting(LAYOUT_PASS_CAP));
        // Starts with or retains the memo table across passes, clearing it at pass end.
        super::app_data_or_default::<MemoTable>(lua);
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
        }
    }
}

impl<'lua> CpuBudget<'lua> {
    /// Claims one nesting level, refusing past [`MAX_SIGNAL_NESTING_DEPTH`]. Install hook before
    /// pushing so early return cannot strand a deadline and disable the VM's cap; no Lua runs
    /// between the two, and the hook tolerates an empty stack.
    pub(crate) fn enter(lua: &'lua Lua) -> mlua::Result<Self> {
        let mut stack = super::app_data_or_default::<Vec<Deadline>>(lua);
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

/// Shared answer for signal-like userdata and the `Signal` to resolve. It accepts [`Signal`],
/// `capability::Capability`, and wrapped `IdleMember`; every capability uses one, so live
/// bindings stay live instead of becoming literals.
///
/// This runs for every signal-valued property of every node, on every whole-scene resolve, so a
/// derived signal comes back as a [`SignalKind::Derived`] handle and its sources are read only when
/// it is.
pub fn from_userdata(ud: &mlua::AnyUserData) -> Option<Signal> {
    if let Ok(signal) = ud.borrow::<Signal>() {
        return Some(match signal.0 {
            SignalKind::Computed { .. } | SignalKind::Delayed { .. } | SignalKind::Pulse { .. } => {
                Signal(SignalKind::Derived(ud.clone()))
            }
            _ => signal.clone(),
        });
    }
    if let Ok(capability) = ud.borrow::<crate::lua::capability::Capability>() {
        return Some(capability.signal());
    }
    // `IdleMember` wraps a capability beside its three threshold methods (ADR-0141); without this
    // arm `visible = mantle.idle` is the one unbindable capability.
    Some(ud.borrow::<crate::lua::idle::IdleMember>().ok()?.signal())
}

/// `signal:get()` on any signal-like userdata.
pub(crate) fn read(lua: &Lua, ud: &mlua::AnyUserData) -> mlua::Result<Value> {
    from_userdata(ud).ok_or_else(|| mlua::Error::runtime("not a signal"))?.get_value(lua)
}

/// [`from_userdata`] without cloning; both must agree on signal types, tested by
/// `from_userdata_and_is_signal_agree`.
pub fn is_signal(ud: &mlua::AnyUserData) -> bool {
    ud.is::<Signal>() || ud.is::<crate::lua::capability::Capability>() || ud.is::<crate::lua::idle::IdleMember>()
}

/// Registers `computed`, `delay` and `pulse` (ADR-0146, ADR-0153), `state` (ADR-0044 decision 5),
/// `hover`, `hover_rect`, and `scroll`. Dependencies are signal-like userdata. Pass the shared
/// dirty flag explicitly, not via `app_data`: a hidden coupling failing inside a config author's
/// `state()` call is worse than threading one argument through. `set` marks the same flag
/// `new_live` returns and The `ms` a `delay` or a `pulse` is given, as whole milliseconds.
///
/// Bounded on what the caller actually gets rather than on the number it wrote: `0.1` clears a
/// bound written in floats and then rounds to nothing, leaving a `delay` that holds for no time
/// and a `pulse` that is never true, both of them silently.
fn parse_hold(what: &str, millis: f64) -> Result<Duration, mlua::Error> {
    let rounded = millis.round() as u64;
    if !(millis > 0.0 && millis <= 60_000.0) || rounded == 0 {
        return Err(mlua::Error::runtime(format!("{what} must be within [1, 60000] ms, got {millis}")));
    }
    Ok(Duration::from_millis(rounded))
}

/// `RendererClient` drains.
pub fn register(lua: &Lua, dirty: DirtyFlag) -> mlua::Result<()> {
    // Before any config code runs, so every coroutine it ever creates inherits the hook.
    install_hook(lua)?;
    let hover_dirty = dirty.clone();
    let rect_dirty = dirty.clone();
    let scroll_dirty = dirty.clone();
    lua.globals().set(
        "computed",
        lua.create_function(|lua, (deps, func): (Table, Function)| {
            let collected = deps
                .sequence_values::<mlua::AnyUserData>()
                .map(|dep| {
                    let dep = dep?;
                    // Name the expected type; `borrow`'s error does not.
                    if !is_signal(&dep) {
                        return Err(mlua::Error::runtime(
                            "computed() dependencies must be Signals or `mantle` capabilities",
                        ));
                    }
                    Ok(dep)
                })
                .collect::<mlua::Result<Vec<_>>>()?;
            let kind = SignalKind::Computed { id: next_computed_id(), arity: collected.len() };
            new_derived(lua, kind, Some(func), collected)
        })?,
    )?;
    lua.globals().set(
        "delay",
        lua.create_function(|lua, (source_ud, millis): (mlua::AnyUserData, f64)| {
            let source = from_userdata(&source_ud)
                .ok_or_else(|| mlua::Error::runtime("delay() takes a Signal or an `mantle` capability first"))?;
            let hold = parse_hold("delay() hold", millis)?;
            let held = source.get_value(lua)?;
            let ud = new_derived(lua, SignalKind::Delayed { hold, due: Rc::default() }, None, vec![source_ud])?;
            ud.set_nth_user_value(HELD_SLOT, held)?;
            Ok(ud)
        })?,
    )?;
    lua.globals().set(
        "pulse",
        lua.create_function(|lua, (source_ud, millis): (mlua::AnyUserData, f64)| {
            let source = from_userdata(&source_ud)
                .ok_or_else(|| mlua::Error::runtime("pulse() takes a Signal or an `mantle` capability first"))?;
            let hold = parse_hold("pulse() window", millis)?;
            let seen = source.get_value(lua)?;
            let ud = new_derived(lua, SignalKind::Pulse { hold, until: Rc::default() }, None, vec![source_ud])?;
            ud.set_nth_user_value(HELD_SLOT, seen)?;
            Ok(ud)
        })?,
    )?;
    lua.globals().set(
        "state",
        lua.create_function(move |lua, (name, initial): (String, Value)| {
            let existing = super::app_data_or_default::<StateRegistry>(lua).0.get(&name).cloned();
            if let Some((signal, seeded)) = existing {
                // Existing name wins across reload; an edited `initial` is later than `set` and
                // reseeds it (ADR-0044 decision 5 amendment).
                if literal_was_edited(&initial, &seeded) == Some(true) {
                    signal.reseed(initial.clone()).map_err(|err| {
                        mlua::Error::runtime(format!(
                            "state(\"{name}\", ...) refused its new initial value at the marshalling boundary: {err}"
                        ))
                    })?;
                    super::app_data_or_default::<StateRegistry>(lua).0.insert(name, (signal.clone(), initial));
                }
                return Ok(signal);
            }
            let signal = Signal::new_state(initial.clone(), dirty.clone()).map_err(|err| {
                mlua::Error::runtime(format!(
                    "state(\"{name}\", ...) refused its initial value at the marshalling boundary: {err}"
                ))
            })?;
            super::app_data_or_default::<StateRegistry>(lua).0.insert(name, (signal.clone(), initial));
            Ok(signal)
        })?,
    )?;
    lua.globals()
        .set("hover", lua.create_function(move |lua, name: String| Ok(hover_slot(lua, &hover_dirty, name)?.0))?)?;
    lua.globals()
        .set("hover_rect", lua.create_function(move |lua, name: String| Ok(hover_slot(lua, &rect_dirty, name)?.1))?)?;
    lua.globals().set(
        "geometry",
        lua.create_function(|lua, name: String| {
            let existing = super::app_data_or_default::<GeometryRegistry>(lua).0.get(&name).cloned();
            if let Some(signal) = existing {
                return Ok(signal);
            }
            let zero = lua.create_table()?;
            for key in ["x", "y", "width", "height"] {
                zero.set(key, 0.0)?;
            }
            let signal = Signal(SignalKind::Geometry(next_cell_id(), Rc::new(RefCell::new(Value::Table(zero)))));
            super::app_data_or_default::<GeometryRegistry>(lua).0.insert(name, signal.clone());
            Ok(signal)
        })?,
    )?;
    lua.globals().set(
        "scroll",
        lua.create_function(move |lua, name: String| {
            Ok(super::app_data_or_default::<ScrollRegistry>(lua)
                .0
                .entry(name)
                .or_insert_with(|| Signal::new_scroll(scroll_dirty.clone()))
                .clone())
        })?,
    )
}

/// Name-keyed hover slot: boolean from `hover(name)`, rect from `hover_rect(name)`
/// (ADR-0062 decision 2).
/// Either global creates the pair, and reloads share it. No marshalling: pointer handler owns both
/// values, not Lua.
fn hover_slot(lua: &Lua, dirty: &DirtyFlag, name: String) -> mlua::Result<(Signal, Signal)> {
    let existing = super::app_data_or_default::<HoverRegistry>(lua).0.get(&name).cloned();
    if let Some(slot) = existing {
        return Ok(slot);
    }
    let slot = Signal::new_hover(dirty.clone(), Value::Table(unhovered_rect(lua)?));
    super::app_data_or_default::<HoverRegistry>(lua).0.insert(name, slot.clone());
    Ok(slot)
}

/// Pre-pointer `hover_rect(name)`: real 1x1 origin table. Non-zero because zero `anchor_rect` is
/// rejected; `visible = hover(name)` stays false, so a tooltip waits invisibly at origin until
/// the pointer event supplies the real rect.
fn unhovered_rect(lua: &Lua) -> mlua::Result<mlua::Table> {
    let rect = lua.create_table()?;
    rect.set("x", 0.0)?;
    rect.set("y", 0.0)?;
    rect.set("width", 1.0)?;
    rect.set("height", 1.0)?;
    Ok(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_signal(name: &str, value: Value) -> Lua {
        let lua = lua_with_state().0;
        let signal = Signal::new_state(value, DirtyFlag::new()).unwrap();
        lua.globals().set(name, signal).unwrap();
        lua
    }

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

    /// VM whose `state` marks the flag `RendererClient` drains.
    #[test]
    fn a_delayed_signal_answers_the_old_value_until_the_source_has_held_the_new_one() {
        let mut cell = DelayCell { held: Value::Boolean(true), pending: None };
        let t0 = Instant::now();
        let hold = Duration::from_millis(147);
        let mut armed = None;
        assert_eq!(cell.follow(Value::Boolean(false), hold, t0, |due| armed = Some(due)), Value::Boolean(true));
        assert_eq!(armed, Some(t0 + hold), "the first read of a change arms the wake");
        assert_eq!(cell.follow(Value::Boolean(false), hold, t0 + hold / 2, |_| ()), Value::Boolean(true));
        assert_eq!(cell.follow(Value::Boolean(false), hold, t0 + hold, |_| ()), Value::Boolean(false));
        // A change that returns before its hold elapses is cancelled outright.
        let later = t0 + hold + Duration::from_millis(1);
        cell.follow(Value::Boolean(true), hold, later, |_| ());
        assert_eq!(
            cell.follow(Value::Boolean(false), hold, later + Duration::from_millis(1), |_| ()),
            Value::Boolean(false)
        );
        assert!(cell.pending.is_none());
        assert_eq!(cell.follow(Value::Boolean(false), hold, later + hold, |_| ()), Value::Boolean(false));
    }

    #[test]
    fn delay_is_a_global_that_holds_a_state_write_and_arms_the_poll_deadline() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"open = state("open", false) held = delay(open, 1)"#).exec().unwrap();
        lua.load("open:set(true)").exec().unwrap();
        assert!(!lua.load("return held:get()").eval::<bool>().unwrap());
        assert!(next_wake_deadline(&lua).is_some());
        std::thread::sleep(Duration::from_millis(5));
        assert!(take_due_wake(&lua, Instant::now()));
        assert!(lua.load("return held:get()").eval::<bool>().unwrap());
        assert!(next_wake_deadline(&lua).is_none(), "an adopted value leaves nothing armed");
        for refused in ["delay(open, 0)", "delay(open, 0.1)"] {
            // 0.1 ms rounds to no milliseconds at all, so a hold that reads as positive would
            // adopt on the very next poll and never hold anything.
            let err = lua.load(refused).exec().unwrap_err().to_string();
            assert!(err.contains("[1, 60000]"), "{refused}: {err}");
        }
    }

    /// The clock is a parameter, so the window is exercised without sleeping through it.
    #[test]
    fn a_pulse_opens_on_a_change_restarts_on_the_next_one_and_closes_by_itself() {
        let hold = Duration::from_millis(100);
        let start = Instant::now();
        let mut cell = PulseCell { seen: Value::Integer(0), until: None };

        assert!(!cell.fire(Value::Integer(0), hold, start, |_| ()), "an unchanged source never fires");
        let mut armed = None;
        assert!(cell.fire(Value::Integer(1), hold, start, |due| armed = Some(due)));
        assert_eq!(armed, Some(start + hold), "an open window arms the close");
        // The source holds its new value: the window stays open on its own, then shuts once.
        assert!(cell.fire(Value::Integer(1), hold, start + Duration::from_millis(50), |_| ()));
        assert!(!cell.fire(Value::Integer(1), hold, start + hold, |_| ()), "the window closes at its due time");
        assert!(!cell.fire(Value::Integer(1), hold, start + hold * 2, |_| ()), "and does not reopen");

        // A second change mid-window restarts it rather than extending the first, which is what
        // `restart()` does to a running animation.
        let reopened = start + hold * 2;
        assert!(cell.fire(Value::Integer(2), hold, reopened, |_| ()));
        let mut armed = None;
        assert!(cell.fire(Value::Integer(3), hold, reopened + Duration::from_millis(60), |due| armed = Some(due)));
        assert_eq!(armed, Some(reopened + Duration::from_millis(60) + hold));
    }

    #[test]
    fn pulse_is_a_global_that_starts_low_fires_on_a_write_and_arms_the_poll_deadline() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"clicks = state("clicks", 0) flashing = pulse(clicks, 50)"#).exec().unwrap();
        assert!(!lua.load("return flashing:get()").eval::<bool>().unwrap(), "a pulse starts low");
        assert!(next_wake_deadline(&lua).is_none(), "and arms nothing until something changes");

        lua.load("clicks:set(1)").exec().unwrap();
        assert!(lua.load("return flashing:get()").eval::<bool>().unwrap());
        assert!(next_wake_deadline(&lua).is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(take_due_wake(&lua, Instant::now()));
        assert!(!lua.load("return flashing:get()").eval::<bool>().unwrap(), "the window closed");

        for refused in ["pulse(clicks, 0)", "pulse(clicks, 0.1)"] {
            let err = lua.load(refused).exec().unwrap_err().to_string();
            assert!(err.contains("[1, 60000]"), "{refused}: {err}");
        }
        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();
        let err = lua.load("pulse(handle, 50)").exec().unwrap_err().to_string();
        assert!(err.contains("Signal"), "{err}");

        // Read-only for the same reason every other engine-written signal is: the only thing that
        // may open the window is the source changing.
        let err = lua.load("flashing:set(true)").exec().unwrap_err().to_string();
        assert!(err.contains("a pulse"), "{err}");
    }

    fn lua_with_state() -> (Lua, DirtyFlag) {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        (lua, dirty)
    }

    #[test]
    fn hover_returns_a_read_only_boolean_signal_that_starts_false() {
        // ADR-0062 decision 2: engine-written hover starts false, not nil; `visible` treats nil as
        // absent
        // (ADR-0044 decision 1 amendment).
        let (lua, _dirty) = lua_with_state();
        let started: bool = lua.load(r#"return hover("volume"):get()"#).eval().unwrap();
        assert!(!started);
    }

    #[test]
    fn hover_hands_the_same_name_the_same_signal_so_an_in_place_reload_keeps_it_open() {
        // Name is identity across reload (ADR-0044 decision 5, ADR-0062 decision 2), so the signal
        // is reused, not reset false. Check storage, not userdata `==`, which compares object
        // identity.
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"first = hover("volume") second = hover("volume") other = hover("battery")"#).exec().unwrap();

        let first: mlua::AnyUserData = lua.globals().get("first").unwrap();
        from_userdata(&first).unwrap().hover_handle().unwrap().set(Value::Boolean(true));

        assert!(lua.load("return second:get()").eval::<bool>().unwrap(), "one name is one slot");
        assert!(!lua.load("return other:get()").eval::<bool>().unwrap(), "a different name is a different slot");
    }

    #[test]
    fn hover_rect_reads_a_real_non_zero_rect_before_anything_has_been_hovered() {
        // Live-session bug: nil rect means absent (ADR-0044 decision 1 amendment), while tooltip
        // `anchor_rect` must be non-zero, so from the first frame each capability push made
        // every resolve refuse the popup until something hovered.
        let (lua, _dirty) = lua_with_state();
        let rect: mlua::Table = lua.load(r#"return hover_rect("volume"):get()"#).eval().unwrap();

        assert!(rect.get::<f32>("width").unwrap() > 0.0, "a zero-width anchor_rect is refused by the protocol");
        assert!(rect.get::<f32>("height").unwrap() > 0.0, "and so is a zero-height one");
        assert_eq!(rect.get::<f32>("x").unwrap(), 0.0);
        assert_eq!(rect.get::<f32>("y").unwrap(), 0.0);
    }

    #[test]
    fn a_config_cannot_write_a_hover_signal_and_the_refusal_names_it_a_hover() {
        // Own `SignalKind`, not capability kind, so refusal names hover (ADR-0062 decision 2).
        let (lua, dirty) = lua_with_state();
        let err = lua.load(r#"hover("volume"):set(true)"#).exec().unwrap_err().to_string();
        assert!(err.contains("state(name, initial)"), "the refusal points at the one writable kind: {err}");
        assert!(err.contains("a hover signal"), "the refusal has to name what it is holding: {err}");
        assert!(!dirty.take(), "a refused write marks nothing");
    }

    #[test]
    fn the_engine_writes_a_hover_signal_through_its_handle_and_only_a_hover_signal() {
        // Decision 2's other half: only hover has a writer, so `hover = mantle.network` cannot let
        // pointer input overwrite a capability snapshot.
        let dirty = DirtyFlag::new();
        let (hovered, hovered_rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let (capability, _capability_handle) = Signal::new_live(Value::Boolean(false), dirty.clone());

        assert!(hovered.hover_handle().is_some(), "a hover signal has a write end");
        assert!(hovered.hover_rect_handle().is_some(), "and a write end for where the node was");
        assert!(capability.hover_handle().is_none(), "a capability signal must not be writable as a hover");
        assert!(
            hovered_rect.hover_rect_handle().is_none(),
            "the rect half is not itself a trigger, so it hands out no second rect"
        );
    }

    #[test]
    fn writing_the_value_already_stored_marks_nothing() {
        // ADR-0062 decision 4: pointer motion writes at device rate, and one mark re-resolves every
        // surface
        // (ADR-0044 decision 2); a stationary pointer must cause no re-resolves.
        let dirty = DirtyFlag::new();
        let (hovered, _rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let handle = hovered.hover_handle().unwrap();

        assert!(handle.set_changed(Value::Boolean(true)), "the first crossing is a real change");
        assert!(dirty.take());

        assert!(!handle.set_changed(Value::Boolean(true)), "the same value again is not a change");
        assert!(!dirty.take(), "an unchanged hover must not re-resolve the scene");

        assert!(handle.set_changed(Value::Boolean(false)), "leaving is a change again");
        assert!(dirty.take());
    }

    #[test]
    fn set_changed_cannot_dedupe_a_table_because_table_equality_is_identity() {
        // `crate::wayland::input` builds a fresh rect table per event; mlua table `PartialEq` is
        // identity, not contents. It can never dedupe identical tables, so rect writes stay on the
        // entry edge, not every motion (ADR-0062 decision 4).
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        let (_over, rect) = Signal::new_hover(dirty.clone(), Value::Nil);
        let handle = rect.hover_handle().unwrap();

        let build = || {
            let table = lua.create_table().unwrap();
            table.set("x", 1.0).unwrap();
            table.set("y", 2.0).unwrap();
            Value::Table(table)
        };
        assert!(handle.set_changed(build()));
        assert!(dirty.take());
        assert!(handle.set_changed(build()), "an identical table is still a different table");
        assert!(dirty.take(), "which is exactly the per-motion dirty mark the writer must avoid");
    }

    #[test]
    fn state_returns_a_signal_reading_back_the_initial_value_it_was_given() {
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua.load(r#"return state("count", 7):get()"#).eval().unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn set_replaces_what_a_later_get_returns() {
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                local s = state("count", 7)
                s:set(41)
                return s:get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 41, "a state signal must read back what Lua last wrote, not its initial value");
    }

    #[test]
    fn set_marks_the_scene_dirty_flag_and_a_plain_get_does_not() {
        // Reads must not re-dirty the scene during layout.
        let (lua, dirty) = lua_with_state();
        lua.load(r#"s = state("count", 0)"#).exec().unwrap();
        assert!(!dirty.take(), "constructing a state signal changes nothing that is painted");

        let _: i64 = lua.load("return s:get()").eval().unwrap();
        assert!(!dirty.take(), "reading a state signal must not mark the scene dirty");

        lua.load("s:set(1)").exec().unwrap();
        assert!(dirty.take(), "writing a state signal must mark the shared scene-dirty flag");
    }

    #[test]
    fn the_same_state_name_and_the_same_initial_keeps_the_value_written_since() {
        // ADR-0044 decision 5: reload preserves the user's last click, not the literal.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "an unedited literal must keep the current value, not reset to the initial");
    }

    #[test]
    fn a_changed_initial_re_seeds_the_signal_and_marks_dirty() {
        // D5 amendment: changed literal is a later write than `set`; this is the wallpaper path.
        let (lua, dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 99):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 99, "an edited literal must win over the value `:set()` left behind");
        assert!(dirty.take(), "a re-seed must mark the scene dirty, or nothing repaints from it");
    }

    #[test]
    fn re_seeding_twice_from_the_same_edited_literal_only_happens_once() {
        // Remember the new literal; the old one would re-seed every later evaluation and clobber
        // `set` forever.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0)
                state("open", 99)
                state("open", 99):set(7)
                return state("open", 99):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 7, "the second evaluation of an already-adopted literal is not another edit");
    }

    #[test]
    fn a_table_initial_never_counts_as_edited() {
        // A popup's anchor rect: fresh table pointers would make every reload an edit and snap the
        // popup to the corner.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("anchor", { x = 0 }):set(5)
                return state("anchor", { x = 0 }):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "a table literal must keep the live value, since it cannot be compared");
    }

    #[test]
    fn rewriting_an_integer_literal_as_a_float_is_not_an_edit() {
        // Lua `==` says `0 == 0.0`; reformatting a number is not an edit.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("count", 0):set(5)
                return state("count", 0.0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "0 and 0.0 are the same literal");
    }

    #[test]
    fn changing_a_literals_type_is_an_edit() {
        let (lua, _dirty) = lua_with_state();
        let result: String = lua
            .load(
                r#"
                state("kind", false):set("clicked")
                return state("kind", "waiting"):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "waiting", "two scalars of different types are a different literal");
    }

    #[test]
    fn two_state_names_are_two_independent_signals() {
        let (lua, _dirty) = lua_with_state();
        let (a, b): (i64, i64) = lua
            .load(
                r#"
                state("a", 1):set(10)
                return state("a", 1):get(), state("b", 2):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!((a, b), (10, 2), "the map is keyed by name, so a write to one name must not reach another");
    }

    #[test]
    fn set_on_a_live_capability_signal_is_refused_because_capability_values_are_read_only_to_lua() {
        // Security contract: accepting `set` on `Live` would let config overwrite the Supervisor's
        // network SSID while every downstream reader believed it.
        let (lua, dirty) = lua_with_state();
        let (signal, _handle) = Signal::new_live(Value::Integer(1), dirty.clone());
        lua.globals().set("network", signal).unwrap();

        let err = lua.load(r#"network:set(2)"#).exec().unwrap_err();
        assert!(err.to_string().contains("read-only"), "the refusal must name the read-only rule: {err}");
        assert!(!dirty.take(), "a refused write must not mark the scene dirty either");

        let unchanged: i64 = lua.load("return network:get()").eval().unwrap();
        assert_eq!(unchanged, 1, "the pushed value must survive the attempt");
    }

    #[test]
    fn set_on_a_computed_signal_is_refused() {
        let (lua, _dirty) = lua_with_state();
        let err = lua
            .load(
                r#"
                local s = state("count", 1)
                s:map(function(v) return v end):set(9)
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "the refusal must name the read-only rule: {err}");
    }

    /// ADR-0112: keybind writes target config state; refuse missing names and non-boolean toggles.
    #[test]
    fn a_control_clients_write_reaches_a_declared_state_and_is_refused_otherwise() {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        lua.load(r#"OPEN = state("launcher_open", false); KIND = state("panel_kind", "none")"#).exec().unwrap();
        dirty.take();

        write_state(&lua, &shared::SetState { name: "launcher_open".into(), write: shared::StateWrite::Toggle })
            .unwrap();
        assert!(dirty.take(), "a write from outside re-resolves the scene like any other");
        assert!(lua.load("return OPEN:get()").eval::<bool>().unwrap());

        let set = shared::StateWrite::Set(serde_json::json!("notifications"));
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: set }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "notifications");
        dirty.take();

        let missing = write_state(&lua, &shared::SetState { name: "nope".into(), write: shared::StateWrite::Toggle });
        assert!(missing.unwrap_err().contains("declares no state"));
        let not_bool =
            write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: shared::StateWrite::Toggle });
        assert!(not_bool.unwrap_err().contains("only a boolean toggles"));
        assert!(!dirty.take(), "a refused write changes nothing");

        // `toggle <name> <value>`: to the value, then back to the declared initial.
        let to_launcher = || shared::StateWrite::ToggleTo(serde_json::json!("launcher"));
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: to_launcher() }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "launcher");
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: to_launcher() }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "none", "already it: back to the initial");
        assert!(dirty.take());
    }

    #[test]
    fn state_refuses_an_initial_value_that_fails_the_marshalling_boundary() {
        // Lua-authored `state` initial crosses `marshal.rs`.
        let (lua, _dirty) = lua_with_state();
        let err = lua.load(r#"return state("bad", 0/0)"#).eval::<Value>().unwrap_err();
        assert!(err.to_string().contains("finite"), "a NaN initial must be refused by name: {err}");
    }

    #[test]
    fn set_refuses_a_value_that_fails_the_marshalling_boundary() {
        let (lua, dirty) = lua_with_state();
        let err = lua
            .load(
                r#"
                s = state("count", 0)
                s:set(0/0)
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("finite"), "a NaN write must be refused by name: {err}");
        assert!(!dirty.take(), "a refused write must not mark the scene dirty");

        let unchanged: i64 = lua.load("return s:get()").eval().unwrap();
        assert_eq!(unchanged, 0, "a refused write must leave the stored value alone");
    }

    #[test]
    fn get_returns_the_wrapped_value() {
        let lua = lua_with_signal("s", Value::Number(0.75));
        let result: f64 = lua.load("return s:get()").eval().unwrap();
        assert_eq!(result, 0.75);
    }

    #[test]
    fn map_recomputes_against_the_parents_current_value() {
        let lua = lua_with_signal("s", Value::Integer(10));
        let result: i64 = lua.load("return s:map(function(v) return v * 2 end):get()").eval().unwrap();
        assert_eq!(result, 20);
    }

    #[test]
    fn computed_combines_multiple_dependencies_current_values() {
        let lua = lua_with_state().0;
        lua.globals().set("a", Signal::new_state(Value::Integer(3), DirtyFlag::new()).unwrap()).unwrap();
        lua.globals().set("b", Signal::new_state(Value::Integer(4), DirtyFlag::new()).unwrap()).unwrap();

        let result: i64 = lua.load("return computed({a, b}, function(x, y) return x + y end):get()").eval().unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn computed_reflects_a_later_signal_reconstruction_not_a_stale_cache() {
        // No memoization: later `get` sees a rebuilt dependency, not a cached first read.
        let lua = lua_with_signal("a", Value::Integer(1));
        lua.load("doubled = computed({a}, function(x) return x * 2 end)").exec().unwrap();

        let first: i64 = lua.load("return doubled:get()").eval().unwrap();
        assert_eq!(first, 2);

        lua.globals().set("a", Signal::new_state(Value::Integer(5), DirtyFlag::new()).unwrap()).unwrap();
        lua.load("doubled = computed({a}, function(x) return x * 2 end)").exec().unwrap();
        let second: i64 = lua.load("return doubled:get()").eval().unwrap();
        assert_eq!(second, 10);
    }

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
    fn a_live_signal_reflects_a_value_pushed_after_construction_not_a_frozen_snapshot() {
        let lua = lua_with_state().0;
        let (signal, handle) = Signal::new_live(Value::Integer(1), DirtyFlag::new());
        lua.globals().set("live", signal).unwrap();

        let first: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(first, 1, "must read the value passed to new_live before any push");

        handle.set(Value::Integer(42));
        let second: i64 = lua.load("return live:get()").eval().unwrap();
        assert_eq!(second, 42, "must reflect the pushed value without re-registering the global");
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
    fn a_derived_signal_stored_in_a_table_its_function_captures_is_collected() {
        // `M.x = computed(..., function() ... M ... end)` is how a module exports a signal. Held
        // from Rust, the function rooted `M`, and every reload leaked the whole module.
        for builder in [
            "t.x = a:map(function() return t and 1 end)",
            "t.x = computed({ a }, function() return t and 1 end)",
            "t.x = delay(a:map(function() return t and 1 end), 1)",
            "t.x = pulse(a:map(function() return t and 1 end), 1)",
            // The held, pending and last-seen values, not only the source.
            "t.x = delay(a:map(function() return t end), 1)",
            "t.x = pulse(a:map(function() return t end), 1)",
            "t.x = delay(a:map(function() return { m = t } end), 1)",
        ] {
            let lua = lua_with_signal("a", Value::Integer(1));
            let collected: bool = lua
                .load(format!(
                    r#"
                    local weak = setmetatable({{}}, {{ __mode = "v" }})
                    do
                        local t = {{}}
                        {builder}
                        t.x:get()
                        weak[1] = t
                    end
                    collectgarbage()
                    collectgarbage()
                    return weak[1] == nil
                    "#
                ))
                .eval()
                .unwrap();
            assert!(collected, "`{builder}` must not keep its table alive");
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

    /// Resolver must see through `Capability`, or live `mantle.<name>` becomes a literal.
    #[test]
    fn from_userdata_sees_through_a_capability_to_its_read_signal() {
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        handle.hydrate(Value::Integer(42), 1);
        lua.globals().set("probe", capability).unwrap();

        let ud: mlua::AnyUserData = lua.load("return probe").eval().unwrap();
        let signal = from_userdata(&ud).expect("a capability must resolve like the signal it wraps");
        assert_eq!(signal.get_value(&lua).unwrap().as_i64(), Some(42));
    }

    #[test]
    fn from_userdata_and_is_signal_agree() {
        // Prevents the two checks from drifting to different type sets.
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, _handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        lua.globals().set("probe", capability).unwrap();
        lua.globals().set("plain", Signal::new_state(Value::Integer(1), DirtyFlag::new()).unwrap()).unwrap();
        // Neither type: both checks must say no.
        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();

        for name in ["probe", "plain", "handle"] {
            let ud: mlua::AnyUserData = lua.load(format!("return {name}")).eval().unwrap();
            assert_eq!(from_userdata(&ud).is_some(), is_signal(&ud), "{name}");
        }
    }

    #[test]
    fn computed_accepts_a_capability_as_a_dependency_and_names_what_it_rejects() {
        use crate::lua::capability::{Capability, CommandSender};

        let lua = lua_with_state().0;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        handle.hydrate(Value::Integer(3), 1);
        lua.globals().set("probe", capability).unwrap();

        let doubled: i64 = lua.load("return computed({probe}, function(n) return n * 2 end):get()").eval().unwrap();
        assert_eq!(doubled, 6);

        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();
        let err = lua.load("return computed({handle}, function(n) return n end)").exec().unwrap_err().to_string();
        assert!(
            err.contains("must be Signals or `mantle` capabilities"),
            "the error must say what was expected: {err}"
        );
    }

    #[test]
    fn targeted_dirty_flag_isolates_surfaces_by_cell_reads() {
        let (lua, dirty) = lua_with_state();
        lua.load(
            r#"
            q = state("q", "search")
            clock = state("clock", "12:00")
            "#,
        )
        .exec()
        .unwrap();

        begin_instance_resolve(&lua, "modal_host@DP-1");
        lua.load("q:get()").exec().unwrap();
        end_instance_resolve(&lua);

        begin_instance_resolve(&lua, "bar@DP-1");
        lua.load("clock:get()").exec().unwrap();
        end_instance_resolve(&lua);

        // Mutating `q` marks only modal_host@DP-1 dirty
        lua.load("q:set('new_search')").exec().unwrap();
        match dirty.take_scope(&lua) {
            DirtyScope::Instances(instances) => {
                assert_eq!(instances, vec!["modal_host@DP-1".to_string()]);
            }
            other => panic!("expected DirtyScope::Instances, got {other:?}"),
        }
    }

    #[test]
    fn unread_cell_write_is_clean_and_mark_falls_back_to_all() {
        let (lua, dirty) = lua_with_state();
        // A cell with no registered readers in the scene owes no work.
        lua.load("unused = state('unused', 1)").exec().unwrap();
        lua.load("unused:set(2)").exec().unwrap();
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Clean);

        // An explicit unscoped mark forces whole-scene resolve.
        dirty.mark();
        assert_eq!(dirty.take_scope(&lua), DirtyScope::All);
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

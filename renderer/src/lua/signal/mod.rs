//! `Signal`: `get`, `map`, `set`, `computed(dependencies, fn)`, and `state(name, initial)`
//! (ADR-0044 decision 5). Rust owns the userdata; `computed` calls `fn` with dependency values, not
//! handles, so its body does not call `:get()` on declared deps.
//!
//! `computed`/`map` keep only their last scalar output, to tell their readers whether it changed.
//! [`EvaluationMemo`] collapses repeats *within* one pass; across passes a node keeps its resolved
//! properties until a cell it read is written (ADR-0270), so only signal reads invalidate.

mod budget;
mod dirty;
pub(crate) mod elision;
mod globals;
mod held;
mod state_handlers;
mod tracking;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, UserData, UserDataMethods, Value};

use crate::lua::location::Site;
use crate::lua::marshal;

pub(crate) use budget::{CpuBudget, LayoutPassBudget, anchor_cpu_budget, thread_cpu_time};
pub use dirty::{DirtyFlag, DirtyScope, LiveSignalHandle};
pub use globals::{
    any_focused_registered, any_hover_registered, any_pointer_registered, begin_evaluation, declared_states,
    promote_states, register, take_layout_changed, write_selection, write_state,
};
pub(crate) use globals::{note_layout_changed, reset, reset_target};
pub use held::{next_wake_deadline, take_due_wake};
pub use state_handlers::{clear as clear_state_handlers, run as run_state_handlers};
#[cfg(test)]
pub(crate) use tracking::MemoTable;
pub(crate) use tracking::{
    ComputedFrame, EvaluationMemo, begin_instance_resolve, end_instance_resolve, forget_instance, is_read,
    note_everything_written, note_read, note_reads, note_write, reset_read_tracker, untracked, with_derived,
    write_clock, written_since,
};
use tracking::{Evaluation, Output};

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
    Computed { out: Rc<Output>, arity: usize },
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
    /// Engine-written boolean from `focused(name)`; its own kind so the pointer and keyboard
    /// writers never write each other's signals.
    Focused { id: CellId, cell: Rc<RefCell<Value>>, dirty: DirtyFlag },
    /// Engine-written `{ x, y }` from `pointer(name)`, `nil` while the pointer is off the node; its
    /// own kind so `hover_handle` cannot write it.
    Pointer { id: CellId, cell: Rc<RefCell<Value>>, dirty: DirtyFlag },
    /// Scroll offset in logical pixels (ADR-0069), written by the wheel handler and layout clamp.
    /// Separate from `Hover` so only `scroll_handle` writes it; `scroll = mantle.network` cannot
    /// overwrite a capability snapshot.
    Scroll {
        id: CellId,
        cell: Rc<RefCell<Value>>,
        dirty: DirtyFlag,
        /// One-shot request from `:reveal`, `:scroll_to` or `:scroll_by` (ADR-0112), consumed by
        /// the next viewport positioning pass. Separate from offset because only that pass knows
        /// child position and the room to clamp to.
        request: Rc<Cell<Option<ScrollRequest>>>,
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
    /// Text fitting result, published only after a successful retained-scene transaction.
    Elided(CellId, Rc<RefCell<Value>>),
    /// `delay(signal, ms)` (ADR-0146): follows `source` once it has held a new value for `hold`.
    /// A read notes the pending value and its due time, arms a wake through `held::WakeDeadline`, and
    /// keeps answering the held value until a read after the due time adopts the new one; the
    /// wake writes `cell`, so its readers make that read (ADR-0275). A source that returns to the
    /// held value before then cancels the change, which makes this a trailing debounce as well
    /// as a close-hold.
    Delayed { hold: Duration, due: Rc<Cell<Option<Instant>>>, cell: CellId },
    /// `pulse(signal, ms)` (ADR-0153): `true` for `ms` after `source` changes value, `false`
    /// otherwise. The other half of [`SignalKind::Delayed`]'s shape and the same machinery -- that
    /// one answers the old value until a change settles, this one says a change just happened --
    /// and it is what fires a one-shot animation, which a config has no way to call `restart()` on
    /// (ADR-0152). A read compares against the value it last saw, arms the wake, and falls back to
    /// `false` on the read after the window closes, which the wake writing `cell` prompts.
    Pulse { hold: Duration, until: Rc<Cell<Option<Instant>>>, cell: CellId },
}

/// A move Lua asks of a `scroll` signal's containers; the pass turns it into an offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollRequest {
    /// The least move that shows this 1-based visible child.
    Reveal(usize),
    To(f32),
    /// Added to the run's target, like a wheel notch, so repeated calls mid-run accumulate.
    By(f32),
}

/// `offset` as a pixel request for `method`, refusing NaN and infinity.
fn finite(method: &str, offset: f64) -> mlua::Result<f32> {
    let pixels = offset as f32;
    if !pixels.is_finite() {
        return Err(mlua::Error::runtime(format!(
            "signal:{method}() takes a finite number of pixels, and {offset} is not one"
        )));
    }
    Ok(pixels)
}

impl SignalKind {
    /// Name used in `:set()` refusal messages.
    fn describe(&self) -> &'static str {
        match self {
            SignalKind::Computed { .. } | SignalKind::Derived(_) => "a computed",
            SignalKind::Live { .. } => "a capability",
            SignalKind::Hover { .. } => "a hover",
            SignalKind::Focused { .. } => "a focused",
            SignalKind::Pointer { .. } => "a pointer",
            SignalKind::Scroll { .. } => "a scroll",
            SignalKind::State { .. } => "a state",
            SignalKind::Delayed { .. } => "a delayed",
            SignalKind::Pulse { .. } => "a pulse",
            SignalKind::Geometry(..) => "a geometry",
            SignalKind::Elided(..) => "an elided",
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

/// Whether writing `b` over `a` changes nothing a reader sees. Strict by variant, since Lua 5.4
/// prints `1` and `1.0` differently, and floats by bits, so `-0.0` differs from `0.0` and NaN matches itself.
fn same_scalar(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Boolean(a), Value::Boolean(b)) => a == b,
        (Value::Integer(a), Value::Integer(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a.to_bits() == b.to_bits(),
        (Value::String(a), Value::String(b)) => a.as_bytes() == b.as_bytes(),
        _ => false,
    }
}

/// Entries [`same_value`] walks before calling two tables different.
pub(super) const SAME_TABLE_ENTRIES: usize = 256;

/// [`same_scalar`], plus two different plain-data tables (no metatable, scalar keys, scalar or
/// plain-table values) equal entry for entry. The same table, anywhere in either, is a change: a
/// writer that mutated it in place and wrote it again is signalling one.
fn same_value(a: &Value, b: &Value) -> bool {
    fn same_table(a: &mlua::Table, b: &mlua::Table, budget: &mut usize) -> bool {
        if a == b || a.metatable().is_some() || b.metatable().is_some() {
            return false;
        }
        let mut entries = 0;
        for pair in a.pairs::<Value, Value>() {
            let Ok((key, value)) = pair else { return false };
            if *budget == 0 || !is_comparable_literal(&key) {
                return false;
            }
            *budget -= 1;
            entries += 1;
            let Ok(other) = b.raw_get::<Value>(key) else { return false };
            let same = match (&value, &other) {
                (Value::Table(value), Value::Table(other)) => same_table(value, other, budget),
                _ => same_scalar(&value, &other),
            };
            if !same {
                return false;
            }
        }
        b.pairs::<Value, Value>().count() == entries
    }
    match (a, b) {
        (Value::Table(a), Value::Table(b)) => same_table(a, b, &mut { SAME_TABLE_ENTRIES }),
        _ => same_scalar(a, b),
    }
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
    /// wins over prior `set`; marks the same dirty flag and returns the replaced value. Only
    /// `State` belongs to the registry; any other kind is a caller bug, not config error.
    pub fn reseed(&self, value: Value) -> Result<Value, marshal::MarshalError> {
        check_lua_authored(&value)?;
        let SignalKind::State { id, cell, dirty } = &self.0 else {
            debug_assert!(false, "reseed on {} signal, which the state registry cannot hold", self.0.describe());
            return Ok(Value::Nil);
        };
        let previous = cell.replace(value);
        dirty.mark_cell(*id);
        Ok(previous)
    }

    /// Rust-pushed signal via [`LiveSignalHandle`]. Values are serde-serialized Rust data, so Lua
    /// marshalling checks cannot find NaN/Inf/oversized strings. Every live signal shares one
    /// generation dirty flag, whose clone `renderer/src/socket/client/mod.rs`'s `RendererClient` drains.
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

    /// Starts `false` for the same reason as [`Self::new_hover`].
    pub fn new_focused(dirty: DirtyFlag) -> Self {
        Signal(SignalKind::Focused { id: next_cell_id(), cell: Rc::new(RefCell::new(Value::Boolean(false))), dirty })
    }

    /// Starts `nil`: the pointer is not over the node until a motion says so.
    pub fn new_pointer(dirty: DirtyFlag) -> Self {
        Signal(SignalKind::Pointer { id: next_cell_id(), cell: Rc::new(RefCell::new(Value::Nil)), dirty })
    }

    /// Scroll offset starting at top (ADR-0069 decision 2). Plain number, not a hover-like pair: no
    /// scrollbar uses content extent yet, so the first such config can define its shape.
    pub fn new_scroll(dirty: DirtyFlag) -> Self {
        Signal(SignalKind::Scroll {
            id: next_cell_id(),
            cell: Rc::new(RefCell::new(Value::Number(0.0))),
            dirty,
            request: Rc::new(Cell::new(None)),
        })
    }

    /// Asks the next positioning pass for `asked`, marking dirty (ADR-0112). The last request
    /// wins, except that a `By` adds to a pending `To` or `By`. Other kinds return false for the
    /// method's named refusal.
    fn request_scroll(&self, asked: ScrollRequest) -> bool {
        let SignalKind::Scroll { id, request, dirty, .. } = &self.0 else { return false };
        request.set(Some(match (request.get(), asked) {
            (Some(ScrollRequest::To(to)), ScrollRequest::By(by)) => ScrollRequest::To(to + by),
            (Some(ScrollRequest::By(first)), ScrollRequest::By(by)) => ScrollRequest::By(first + by),
            _ => asked,
        }));
        dirty.note_scroll_request(*id, request);
        dirty.mark_cell(*id);
        true
    }

    /// The request waiting for the next positioning pass, left in place.
    pub(crate) fn pending_scroll(&self) -> Option<ScrollRequest> {
        if let SignalKind::Scroll { request, .. } = &self.0 { request.get() } else { None }
    }

    /// Consumes the request in `layout::scene`'s positioning pass, so a later wheel event does not
    /// fight an already honored one.
    pub(crate) fn take_scroll(&self) -> Option<ScrollRequest> {
        if let SignalKind::Scroll { request, .. } = &self.0 { request.take() } else { None }
    }

    fn scroll_request(&self, method: &str, asked: ScrollRequest) -> mlua::Result<()> {
        if self.request_scroll(asked) {
            return Ok(());
        }
        Err(mlua::Error::runtime(format!(
            "signal:{method}() is only valid on a scroll(name) signal, and this is {} signal",
            self.0.describe()
        )))
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

    pub(crate) fn elided_id(&self) -> Option<CellId> {
        if let SignalKind::Elided(id, _) = &self.0 { Some(*id) } else { None }
    }

    /// Hover write end for `crate::wayland`; `None` for other kinds by design.
    pub(crate) fn hover_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Hover { id, cell, dirty, .. } = &self.0 else { return None };
        Some(LiveSignalHandle(*id, Rc::clone(cell), dirty.clone()))
    }

    /// Focus-within write end for `crate::wayland`; `None` for other kinds.
    pub(crate) fn focused_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Focused { id, cell, dirty } = &self.0 else { return None };
        Some(LiveSignalHandle(*id, Rc::clone(cell), dirty.clone()))
    }

    /// Pointer write end for `crate::wayland`; `None` for other kinds.
    pub(crate) fn pointer_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Pointer { id, cell, dirty } = &self.0 else { return None };
        Some(LiveSignalHandle(*id, Rc::clone(cell), dirty.clone()))
    }

    /// Rect write end for the boolean hover half: last node position in surface logical
    /// coordinates, consumed by tooltip `popup.anchor_rect`. `None` for other kinds and the rect
    /// half itself.
    pub(crate) fn hover_rect_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Hover { paired_rect: Some((rect_id, rect)), dirty, .. } = &self.0 else { return None };
        Some(LiveSignalHandle(*rect_id, Rc::clone(rect), dirty.clone()))
    }

    /// The stored value and its id, for every kind that holds one; `None` for the derived kinds.
    fn cell(&self) -> Option<(CellId, &Rc<RefCell<Value>>)> {
        match &self.0 {
            SignalKind::Live { id, cell }
            | SignalKind::Hover { id, cell, .. }
            | SignalKind::Focused { id, cell, .. }
            | SignalKind::Pointer { id, cell, .. }
            | SignalKind::Scroll { id, cell, .. }
            | SignalKind::State { id, cell, .. }
            | SignalKind::Geometry(id, cell)
            | SignalKind::Elided(id, cell) => Some((*id, cell)),
            _ => None,
        }
    }

    /// Reactive cell identifier for targeted invalidation tracking, if this signal is backed by a cell.
    pub(crate) fn cell_id(&self) -> Option<CellId> {
        self.cell().map(|(id, _)| id)
    }

    /// `map(f)` as a one-dependency `Computed`, memoized per layout pass like any `Computed`.
    /// Shared by Lua and Rust so `lua::capability::Capability` makes `mantle.lock` read like bare
    /// capabilities.
    pub(crate) fn mapped(lua: &Lua, source: mlua::AnyUserData, func: Function) -> mlua::Result<mlua::AnyUserData> {
        new_derived(lua, SignalKind::Computed { out: Output::new(), arity: 1 }, Some(func), vec![source])
    }

    /// Reads current value (ADR-0044 decision 1). `layout::node` uses it to resolve signal
    /// userdata; `Computed` needs `lua` for [`CpuBudget`], and every caller already holds one.
    pub(crate) fn get_value(&self, lua: &Lua) -> mlua::Result<Value> {
        if let Some((id, cell)) = self.cell() {
            note_read(lua, id);
            return Ok(cell.borrow().clone());
        }
        match &self.0 {
            SignalKind::Derived(ud) => read_derived(lua, ud),
            SignalKind::Computed { .. } | SignalKind::Delayed { .. } | SignalKind::Pulse { .. } => Err(
                mlua::Error::runtime("a derived signal was read without the userdata holding its function and sources"),
            ),
            _ => unreachable!("`cell` holds every stored kind"),
        }
    }
}

/// User value holding where the signal was created ([`Site`]), named when its
/// function raises: that happens during a later layout pass, and the traceback shows only the
/// function's own lines, not the binding that made it.
const SITE_SLOT: usize = 1;
/// User value holding a `Computed`'s function; its sources, or a `Delayed`/`Pulse` source, follow.
const FUNCTION_SLOT: usize = 2;
const FIRST_SOURCE_SLOT: usize = 3;
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
    let out = if let SignalKind::Computed { out, .. } = &kind { Some(out.cell) } else { None };
    let ud = lua.create_userdata(Signal(kind))?;
    if let Some(out) = out {
        computeds(lua)?.raw_set(out.0, &ud)?;
    }
    ud.set_nth_user_value(SITE_SLOT, Site::to_lua(Site::of_caller(lua)))?;
    if let Some(func) = func {
        ud.set_nth_user_value(FUNCTION_SLOT, func)?;
    }
    for (offset, source) in sources.into_iter().enumerate() {
        ud.set_nth_user_value(FIRST_SOURCE_SLOT + offset, source)?;
    }
    Ok(ud)
}

/// Every live computed by its output cell, weakly, for [`DirtyFlag::take_scope`] to run again.
fn computeds(lua: &Lua) -> mlua::Result<mlua::Table> {
    const KEY: &str = "mantle.signal.computeds";
    if let Some(table) = lua.named_registry_value::<Option<mlua::Table>>(KEY)? {
        return Ok(table);
    }
    let table = lua.create_table()?;
    let weak = lua.create_table()?;
    weak.raw_set("__mode", "v")?;
    table.set_metatable(Some(weak))?;
    lua.set_named_registry_value(KEY, &table)?;
    Ok(table)
}

fn source_at(ud: &mlua::AnyUserData, slot: usize) -> mlua::Result<Signal> {
    let source: mlua::AnyUserData = ud.nth_user_value(slot)?;
    from_userdata(&source).ok_or_else(|| mlua::Error::runtime("a derived signal's source is not a signal"))
}

fn read_derived(lua: &Lua, ud: &mlua::AnyUserData) -> mlua::Result<Value> {
    let kind = ud.borrow::<Signal>()?.0.clone();
    match kind {
        SignalKind::Delayed { hold, due, cell } => held::read_delayed(lua, ud, hold, &due, cell),
        SignalKind::Pulse { hold, until, cell } => held::read_pulse(lua, ud, hold, &until, cell),
        SignalKind::Computed { out, arity } => {
            // A repeat within this evaluation costs one hash lookup and no Lua. Checked before
            // `CpuBudget::enter` on purpose: a hit does no work, so it must not spend a nesting
            // level either, or a wide diamond would hit `MAX_SIGNAL_NESTING_DEPTH` on cache
            // hits alone.
            if let Some(value) = EvaluationMemo::get(lua, out.cell) {
                return Ok(value);
            }

            // Enter before dependency resolution, not only `func.call`, so nesting depth also
            // bounds dependency chains.
            let budget = CpuBudget::enter(lua)?;
            // Opened by whichever `Computed` is outermost and dropped when it returns, so the
            // memo spans exactly one evaluation. It deliberately does not span a
            // `capability::CapabilityHandle::notify_change` handler: that handler may `:set()`
            // between its own `:get()` calls and has to observe its own writes.
            let _memo = EvaluationMemo::enter(lua);
            let at = write_clock(lua);
            let evaluation = Evaluation::enter(lua);

            let mut args = Vec::with_capacity(arity);
            for slot in FIRST_SOURCE_SLOT..FIRST_SOURCE_SLOT + arity {
                args.push(source_at(ud, slot)?.get_value(lua)?);
            }
            let func: Function = ud.nth_user_value(FUNCTION_SLOT)?;
            let value = func.call::<Value>(MultiValue::from_vec(args)).and_then(|value| {
                budget.check_not_exceeded()?;
                Ok(value)
            });
            let value = match (value, Site::from_lua(&ud.nth_user_value(SITE_SLOT)?)) {
                (Err(err), Some(site)) => {
                    let err = super::describe(&err);
                    return Err(mlua::Error::runtime(format!("signal created at {site}: {err}")));
                }
                (value, _) => value?,
            };
            out.settle(lua, &value, at, evaluation.finish());
            EvaluationMemo::insert(lua, out.cell, &value);
            note_reads(lua, &[out.cell]);
            Ok(value)
        }
        other => Signal(other).get_value(lua),
    }
}

impl UserData for Signal {
    // Named state's side-effect hook, the counterpart of a capability's (ADR-0115). A field, not a
    // method: a derived signal reads `nil`, so a config can test `signal.on_change` for either kind.
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("on_change", |lua, this| {
            let SignalKind::State { id, .. } = &this.0 else {
                return Ok(Value::Nil);
            };
            let id = *id;
            lua.create_function(move |lua, (_, handler): (mlua::AnyUserData, Function)| {
                state_handlers::add(lua, id, handler);
                Ok(())
            })
            .map(Value::Function)
        });
    }

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
            this.scroll_request("reveal", ScrollRequest::Reveal(index))
        });
        methods.add_method("scroll_to", |_, this, offset: f64| {
            this.scroll_request("scroll_to", ScrollRequest::To(finite("scroll_to", offset)?))
        });
        methods.add_method("scroll_by", |_, this, delta: f64| {
            this.scroll_request("scroll_by", ScrollRequest::By(finite("scroll_by", delta)?))
        });
        // ADR-0044 decision 5's only Lua write path. Other kinds refuse by name, so
        // `network:set(...)`
        // says why.
        methods.add_method("set", |lua, this, value: Value| {
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
            if !same_value(&cell.borrow(), &value) {
                let previous = cell.replace(value);
                dirty.mark_cell(*id);
                state_handlers::note_write(lua, this, previous);
            }
            Ok(())
        });
    }
}

/// Shared answer for signal-like userdata and the `Signal` to resolve. It accepts [`Signal`],
/// `capability::Capability`, and wrapped `IdleMember`; every capability uses one, so live
/// bindings stay live instead of becoming literals.
///
/// This runs for every signal-valued property of every node that resolves, so a
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

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn lua_with_signal(name: &str, value: Value) -> Lua {
        let lua = lua_with_state().0;
        let signal = Signal::new_state(value, DirtyFlag::new()).unwrap();
        lua.globals().set(name, signal).unwrap();
        lua
    }

    /// VM whose `state` marks the flag `RendererClient` drains.
    pub(super) fn lua_with_state() -> (Lua, DirtyFlag) {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        (lua, dirty)
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
        // surface that read the slot (ADR-0244); a stationary pointer must cause no re-resolves.
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

        lua.load("s:set(1)").exec().unwrap();
        assert!(!dirty.take(), "writing the value it holds changes nothing");
        lua.load("s:set(1.0)").exec().unwrap();
        assert!(dirty.take(), "1.0 prints differently from 1");

        lua.load("s:set({ a = 1, b = { 'x' } })").exec().unwrap();
        assert!(dirty.take());
        lua.load("s:set({ a = 1, b = { 'x' } })").exec().unwrap();
        assert!(!dirty.take(), "a fresh table equal entry for entry changes nothing");
        lua.load("s:set({ a = 1, b = { 'x' }, c = 2 })").exec().unwrap();
        assert!(dirty.take(), "an extra key is a change");
        lua.load("t = s:get() t.a = 2 s:set(t)").exec().unwrap();
        assert!(dirty.take(), "the same table mutated in place is a change");
        lua.load("s:set({ f = print }) s:set({ f = print })").exec().unwrap();
        assert!(dirty.take(), "a function inside is not plain data");
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

    /// A hole is an entry, not the end of the list, so a dependency after it is never dropped.
    #[test]
    fn computed_names_the_dependency_that_is_not_a_signal() {
        let lua = lua_with_state().0;
        lua.globals().set("a", Signal::new_state(Value::Integer(3), DirtyFlag::new()).unwrap()).unwrap();
        let err = |source: &str| lua.load(source).exec().unwrap_err().to_string();

        let hole = err("computed({a, nil, a}, function(x, y, z) return x end)");
        assert!(hole.contains("computed() dependency 2 is nil"), "{hole}");
        let number = err("computed({a, 5}, function(x, y) return x end)");
        assert!(number.contains("computed() dependency 2 is an integer"), "{number}");
        let named = err("computed({a, b = a}, function(x) return x end)");
        assert!(named.contains("`b`"), "{named}");
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

    /// Resolver must see through `Capability`, or live `mantle.<name>` becomes a literal.
    #[test]
    fn from_userdata_sees_through_a_capability_to_its_read_signal() {
        use crate::lua::capability::{Capability, CommandSender};

        let lua = Lua::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("probe", DirtyFlag::new(), CommandSender::new(0, tx));
        handle.hydrate(Value::Integer(42));
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
        handle.hydrate(Value::Integer(3));
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

        // A re-resolve that stops reading `clock` and reads `q` moves bar@DP-1's dependency over.
        begin_instance_resolve(&lua, "bar@DP-1");
        lua.load("q:get()").exec().unwrap();
        end_instance_resolve(&lua);
        lua.load("clock:set('12:01')").exec().unwrap();
        assert_eq!(dirty.take_scope(&lua), DirtyScope::Clean, "the dropped read no longer dirties it");
        lua.load("q:set('again')").exec().unwrap();
        match dirty.take_scope(&lua) {
            DirtyScope::Instances(mut instances) => {
                instances.sort();
                assert_eq!(instances, ["bar@DP-1", "modal_host@DP-1"]);
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
}

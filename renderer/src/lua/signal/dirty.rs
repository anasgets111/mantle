//! What changed since the scene last resolved: the shared [`DirtyFlag`], its [`DirtyScope`], and the
//! write end Rust holds on a live signal.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};

use super::tracking::{EvaluationMemo, ReadTracker, current_clock, downstream, outputs_written_since};
use super::{CellId, computeds, note_write, read};

/// Rust handle for [`super::Signal::new_live`] storage, used for `StateSnapshot` pushes. Lua reads the
/// latest value, with no memoization.
#[derive(Clone)]
pub struct LiveSignalHandle(pub(super) CellId, pub(super) Rc<RefCell<Value>>, pub(super) DirtyFlag);

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
    /// just measured. Positioning uses the clamped value immediately; a getter that read the wheel's
    /// value gets one follow-up pass in the same turn (`Scene::scroll_settled_since`).
    pub(crate) fn set_quiet(&self, value: Value) {
        *self.1.borrow_mut() = value;
        note_write(self.0);
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
    /// Instances whose own inputs changed with no cell written, such as their configured size.
    instances: rustc_hash::FxHashSet<String>,
    /// The write clock at the last take: a computed stamped since then has readers to re-resolve.
    taken_at: u64,
}

/// Shared invalidation flag tracking scene-wide or cell-targeted dirty marks.
#[derive(Clone, Default)]
pub struct DirtyFlag(Rc<RefCell<DirtyState>>);

impl DirtyFlag {
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(DirtyState::default())))
    }

    /// Every surface must re-resolve: a reload, or a change no instance can be named for.
    pub(crate) fn mark(&self) {
        self.0.borrow_mut().all = true;
    }

    /// One instance must re-resolve, for a change that is its own and written no cell: `configure`
    /// changing its size (ADR-0044 decision 2).
    pub(crate) fn mark_instance(&self, instance_id: &str) {
        self.0.borrow_mut().instances.insert(instance_id.to_string());
    }

    /// Marks a specific reactive cell dirty.
    pub(crate) fn mark_cell(&self, id: CellId) {
        note_write(id);
        self.0.borrow_mut().cells.insert(id);
    }

    /// Reads and clears atomically: drain inbound frames, then re-resolve once
    /// (ADR-0044 decision 2).
    pub fn take(&self) -> bool {
        let mut state = self.0.borrow_mut();
        if state.all || !state.cells.is_empty() || !state.instances.is_empty() {
            *state = DirtyState { taken_at: current_clock(), ..DirtyState::default() };
            true
        } else {
            false
        }
    }

    /// Takes the invalidation scope: Clean, All, or targeted Instances based on ReadTracker.
    ///
    /// A computed reading a written cell runs again here, and its readers count only if its output
    /// changed. When called inside an enclosing [`EvaluationMemo`] (e.g. during re-resolution),
    /// computed values evaluated here are retained in the memo table and handed over into the
    /// subsequent layout pass, preventing double-evaluation. Standalone calls evaluate under
    /// their own memo and drop it immediately.
    pub fn take_scope(&self, lua: &Lua) -> DirtyScope {
        let (mut cells, marked, taken_at) = {
            let mut state = self.0.borrow_mut();
            if !state.all && state.cells.is_empty() && state.instances.is_empty() {
                return DirtyScope::Clean;
            }
            if state.all {
                *state = DirtyState { taken_at: current_clock(), ..DirtyState::default() };
                return DirtyScope::All;
            }
            (std::mem::take(&mut state.cells), std::mem::take(&mut state.instances), state.taken_at)
        };
        // Unborrowed: a computed may `set` a state, which marks this flag.
        rerun_computeds(lua, &cells);
        cells.extend(outputs_written_since(taken_at));
        self.0.borrow_mut().taken_at = current_clock();
        let tracker = lua.app_data_ref::<ReadTracker>();
        let mut instances = marked;
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

/// Runs every computed downstream of `cells` once, which stamps each whose output changed. One that
/// fails counts as changed, so the pass meets the same error and reports it.
fn rerun_computeds(lua: &Lua, cells: &rustc_hash::FxHashSet<CellId>) {
    let outs = downstream(lua, cells);
    if outs.is_empty() {
        return;
    }
    let Ok(table) = computeds(lua) else { return };
    let _memo = EvaluationMemo::enter(lua);
    for out in outs {
        if let Ok(Some(ud)) = table.raw_get::<Option<mlua::AnyUserData>>(out.0)
            && read(lua, &ud).is_err()
        {
            note_write(out);
        }
    }
}

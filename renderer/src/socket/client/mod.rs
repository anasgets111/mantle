//! The Wayland thread's half: [`RendererClient`], which services each frame [`pump`](super::pump)
//! decodes.

mod frame;
mod reset;
mod resolve;
mod scroll;

pub use frame::FrameOutcome;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use shared::{RendererFrame, debug, error, warn};
use tokio::sync::mpsc;

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::SurfaceSpec;
use crate::layout::{self, Scene};
use crate::lua::capability::{Capability, CapabilityHandle, CommandSender};
use crate::lua::file::FileRegistry;
use crate::lua::palette::PaletteRegistry;
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::DirtyFlag;
use crate::lua::surfaces::evaluate_and_specs;
use crate::lua::{self, Loader};
use crate::text::shaping::ShapingHandle;

/// The registries whose calls finish on another thread.
struct OffThread {
    palette: PaletteRegistry,
    file: FileRegistry,
}

/// Reload state. `applied_specs` is the applied declaration set, empty before any evaluation.
/// `pending` holds an evaluation until the Wayland loop applies it. `applied_output` is ADR-0044 decision 2's re-resolve
/// target, retained so pushes skip `shell.lua`. mlua 0.12's `ValueRef` holds `WeakLua`: a retained
/// `mlua::Value` does not keep Lua alive and `ValueRef::to_pointer` panics after death, so Rust
/// field-drop order must keep `Lua` last (see [`RendererClient`]).
struct ReloadState {
    applied_specs: Vec<SurfaceSpec>,
    applied_output: Option<lua::LoadOutput>,
    pending: Option<(lua::LoadOutput, Vec<SurfaceSpec>)>,
}

/// One generation's Lua side: VM, retained scene, live signals, and reload state. Deliberately
/// `!Send`; `crate::wayland::App` owns it directly (ADR-0039), so closures, reconcile, and EGL
/// need no channel hop. **Field order is load-bearing: `loader` must stay last**. Most fields hold
/// `mlua::Value`s whose `WeakLua` does not keep the VM alive; moving `loader` first drops Lua
/// before them and panics (see [`ReloadState`]).
pub struct RendererClient {
    shell_lua_path: PathBuf,
    scene: Scene,
    /// `(surface, output)` pairs resolved by `expand_instances`, shared by
    /// [`Self::apply_instances`], [`Self::handle_apply_pending`], and
    /// [`Self::re_resolve_if_dirty`].
    instances: Vec<SurfaceInstance>,
    /// Whether this process holds or requested a lock. Arms
    /// [`crate::layout::secure_submit::lock_stays_authenticatable`]: `SurfaceFingerprint::Lock`
    /// carries only `id`, so editing `child` rebuilds nothing and
    /// reloads in place; deleting the password while locked would leave only a VT switch. Written
    /// by `crate::wayland::App::set_session_lock` and teardown. **A `bool`, not instance ids**:
    /// hotplug replaces instances, so the veto reads [`Self::instances`].
    holds_session_lock: bool,
    /// Whether the last pass was the one follow-up a layout measurement earns. A changed
    /// measurement in that pass gets no second follow-up, so a binding fed by its own
    /// measurement settles or stops, never spins.
    layout_follow_up: bool,
    /// Clone of `crate::wayland::App`'s `ShapingHandle`: one worker and font set per process
    /// (ADR-0023).
    shaping: ShapingHandle,
    /// Capability handles keyed by `StateSnapshot.capability` (ADR-0029), seeded from
    /// `shared::Capability::ALL` (ADR-0037). `RefCell` permits access through `&self`.
    capabilities: RefCell<HashMap<String, CapabilityHandle>>,
    /// Cloned into every [`Capability`], including lazy ones.
    /// Also this client's outbound half: [`CommandSender::frames`] hands out the Renderer ->
    /// Supervisor sender that socket-thread [`pump`](super::pump) drains to the wire.
    ///
    /// ponytail: unbounded, and deliberately so for now. `send` is called from synchronous Wayland
    /// dispatch callbacks, which cannot await, so backpressure is not available here; and
    /// `try_send` dropping a `SecureSubmit` would lose a password mid-unlock rather than delay it.
    /// The ceiling is therefore a config that produces frames faster than the socket drains.
    /// Upgrade path: give the callbacks a non-blocking handoff to an async sender that can park,
    /// so the bound lands on the queue instead of on the callback. Bounding this channel as it
    /// stands would trade a memory bug for a correctness one.
    commands: CommandSender,
    rescue_handle: CapabilityHandle,
    /// Renderer-sourced `mantle.screens` handle (ADR-0041 decision 2), not in `capabilities`.
    screens_handle: CapabilityHandle,
    /// `screens_handle`'s JSON mirror for [`Self::set_screens`] change detection.
    screens_payload: serde_json::Value,
    /// `rescue_handle`'s mirror for [`Self::set_rescue_state`] no-op detection.
    rescue_state: (bool, String),
    /// The rescue is `applied_output`'s own apply failure, so a re-resolve that applies clears it.
    /// Any other rescue waits for the next applied evaluation.
    rescue_is_applied_output: bool,
    process_registry: ProcessRegistry,
    off_thread: OffThread,
    /// Renderer-sourced `mantle.idle` threshold callbacks (ADR-0032).
    idle_registry: crate::lua::idle::IdleRegistry,
    /// Scene-dirty flag (ADR-0044 decision 2), cloned into every handed-out `LiveSignalHandle`.
    dirty: DirtyFlag,
    /// Instance IDs narrowed during the last dirty re-resolve, or `None` if full resolve.
    last_resolved: Option<Vec<String>>,
    /// The last dirty re-resolve's failure and its unlogged repeats; `None` once a pass applies.
    re_resolve_failure: Option<(String, u32)>,
    state: ReloadState,
    /// Each shown declared surface's `reset_on_close` list, from the evaluation that showed it, so
    /// a reload that removes the surface still knows what to write back (ADR-0289).
    shown_resets: HashMap<String, Vec<lua::signal::Signal>>,
    /// A reset or a moved layout measurement wrote after this turn's pass, so the loop owes another
    /// before it sleeps.
    owes_pass: bool,
    /// `mantle` table for lazy members. Above `loader` for drop order.
    mantle: mlua::Table,
    /// Last, load-bearing; see the struct docs.
    loader: Loader,
}

impl RendererClient {
    /// Builds one generation's VM, rescue signal, `process` registry, and seeded capability signals
    /// on the calling thread. `crate::wayland::run` calls it, never the socket thread: `mlua::Lua`
    /// is `!Send` and must be built where it runs (ADR-0039). Failure is fatal; no VM means no
    /// `shell.lua` evaluation or screen output.
    pub fn start(
        shaping: ShapingHandle,
        outbound_tx: mpsc::UnboundedSender<RendererFrame>,
        generation_id: u32,
        waker: crate::wake::Waker,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let shell_lua_path =
            shared::shell_lua_path().map_err(|err| format!("failed to resolve shell.lua's path: {err}"))?;
        // One flag per generation (ADR-0044 decision 2), shared by rescue.
        let dirty = DirtyFlag::new();
        // `require` searches here and nowhere else (ADR-0047 decision 1); derive it once.
        let config_dir = shell_lua_path.parent().ok_or("shell.lua's path has no parent directory")?.to_path_buf();
        let loader =
            Loader::new(dirty.clone(), &config_dir).map_err(|err| format!("failed to start the Lua loader: {err}"))?;
        // Capability commands and `process` all use this write path.
        let commands = CommandSender::new(generation_id, outbound_tx);
        let process_registry = ProcessRegistry::new(commands.clone());
        lua::process::register(loader.lua(), process_registry.clone())
            .map_err(|err| format!("failed to register the process global: {err}"))?;
        let palette_registry = PaletteRegistry::new(Some(waker.clone()));
        lua::palette::register(loader.lua(), palette_registry.clone())
            .map_err(|err| format!("failed to register the palette global: {err}"))?;
        let file_registry = FileRegistry::new(Some(waker));
        lua::file::register(loader.lua(), file_registry.clone())
            .map_err(|err| format!("failed to register the file global: {err}"))?;
        let client = Self::new(
            loader,
            shell_lua_path,
            shaping,
            commands,
            process_registry,
            OffThread { palette: palette_registry, file: file_registry },
            dirty,
        )
        .map_err(|err| format!("failed to build the `mantle` namespace: {err}"))?;
        Ok(client)
    }

    /// [`lua::namespace::build`] owns the `mantle` namespace; construction is separate from frame
    /// handling. [`Self::capability_handle`] lazily handles unrostered names from snapshots.
    fn new(
        loader: Loader,
        shell_lua_path: PathBuf,
        shaping: ShapingHandle,
        commands: CommandSender,
        process_registry: ProcessRegistry,
        off_thread: OffThread,
        dirty: DirtyFlag,
    ) -> mlua::Result<Self> {
        // Take it from `commands`, avoiding a drifting clone.
        let namespace = lua::namespace::build(&loader, &dirty, &commands, &shell_lua_path)?;
        Ok(Self {
            shell_lua_path,
            scene: Scene::new(),
            instances: Vec::new(),
            holds_session_lock: false,
            layout_follow_up: false,
            shaping,
            capabilities: RefCell::new(namespace.capabilities),
            commands,
            rescue_handle: namespace.rescue,
            screens_handle: namespace.screens,
            screens_payload: namespace.screens_payload,
            // Matches `lua::namespace::build`'s initial rescue signal.
            rescue_state: (false, String::new()),
            rescue_is_applied_output: false,
            process_registry,
            off_thread,
            idle_registry: namespace.idle,
            dirty,
            last_resolved: None,
            re_resolve_failure: None,
            state: ReloadState { applied_specs: Vec::new(), applied_output: None, pending: None },
            shown_resets: HashMap::new(),
            owes_pass: false,
            mantle: namespace.table,
            loader,
        })
    }

    /// Runs the callbacks of `palette` and `file` calls that finished off the Lua thread.
    pub fn poll_off_thread(&self) {
        self.off_thread.palette.poll();
        self.off_thread.file.poll();
    }

    /// Returns and clears the instance IDs narrowed during the last dirty re-resolve, if any.
    pub fn take_last_resolved(&mut self) -> Option<Vec<String>> {
        self.last_resolved.take()
    }

    /// Writes `rescue`'s `{ is_rescue, error_log }` only when changed. A write marks the shared
    /// `DirtyFlag` (ADR-0044 decision 2); a no-op rewrite could dirty a generation that must not
    /// mutate, unlike the genuine transition rejected by ADR-0044 decision 3. `pub` for
    /// `crate::wayland::App`'s `SessionLockHandler` (ADR-0052 decision 4), used for refused locks
    /// and both `finished` cases, the only user-facing path there.
    pub fn set_rescue_state(&mut self, is_rescue: bool, error_log: &str) {
        self.rescue_is_applied_output = false;
        if self.rescue_state.0 == is_rescue && self.rescue_state.1 == error_log {
            return;
        }
        match lua::namespace::rescue_table(&self.loader, is_rescue, error_log) {
            Ok(table) => {
                let previous = self.rescue_handle.hydrate(mlua::Value::Table(table));
                self.rescue_handle.notify_change(self.loader.lua(), previous);
                self.rescue_state = (is_rescue, error_log.to_string());
            }
            Err(err) => warn!("failed to build rescue state: {err}"),
        }
    }

    /// Before each `shell.lua` evaluation, clear what the last one registered: `process.run` children,
    /// whose `on_exit(nil)` runs first so anything it arms is cleared too, `palette.quantize`
    /// callbacks, `on_change` handlers (ADR-0115, ADR-0288) and `action` exports (ADR-0197).
    /// Evaluation registers them afresh; retaining them
    /// doubles side effects after a config save, and all are closures over locals that evaluation
    /// is about to replace.
    ///
    /// Run again when an evaluation *fails*, because clearing first is not enough: `shell.lua` may
    /// register several before it raises, and those belong to a config that was rejected. Half a
    /// config answering `mantle call` is worse than none, and the scene still on screen is the
    /// previous evaluation's, whose registrations this already dropped.
    fn clear_change_handlers(&self) {
        self.process_registry.kill_all();
        self.off_thread.palette.clear();
        self.off_thread.file.clear();
        for handle in self.capabilities.borrow().values().chain([&self.rescue_handle, &self.screens_handle]) {
            handle.clear_handlers();
        }
        lua::signal::clear_state_handlers(self.loader.lua());
        lua::action::clear(self.loader.lua());
        lua::timer::begin_evaluation(self.loader.lua());
        lua::forget_raised();
    }

    /// Returns a handle, lazily adding `mantle.<capability>` as `nil` (ADR-0029).
    /// Debug builds reject off-roster pushes first. **Refuses held names**: `Table::set` is silent,
    /// and an off-roster `rescue` push would replace the config-failure signal (ADR-0052 decision
    /// 1's bug).
    fn capability_handle(&self, capability: &str) -> mlua::Result<CapabilityHandle> {
        if let Some(handle) = self.capabilities.borrow().get(capability) {
            return Ok(handle.clone());
        }
        if self.mantle.contains_key(capability)? {
            return Err(mlua::Error::runtime(format!(
                "a StateSnapshot named the unrostered capability {capability:?}, and `mantle.{capability}` is already something else; refusing to replace it"
            )));
        }
        let (member, handle) = Capability::new(capability, self.dirty.clone(), self.commands.clone());
        self.mantle.set(capability, member)?;
        self.capabilities.borrow_mut().insert(capability.to_string(), handle.clone());
        Ok(handle)
    }

    /// Evaluates `shell.lua` once at startup. `applied_specs` stays empty only on *evaluation*
    /// failure; a failed *apply* keeps it because surfaces are already bound. Split so the caller expands returned specs via
    /// [`Self::apply_instances`].
    pub fn run_startup_evaluation(&mut self) -> Option<Vec<SurfaceSpec>> {
        self.clear_change_handlers();
        match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                self.state.applied_specs = specs.clone();
                // ADR-0044 decision 2 target: later pushes skip `shell.lua`.
                self.state.applied_output = Some(output);
                lua::timer::promote(self.loader.lua());
                lua::signal::promote_states(self.loader.lua());
                Some(specs)
            }
            Err(err) => {
                error!("startup shell.lua evaluation failed: {err}");
                // Whatever it registered before raising goes with it. `discard` after the clear
                // because that arms the next evaluation; nothing is running one now.
                self.clear_change_handlers();
                lua::timer::discard(self.loader.lua());
                self.set_rescue_state(true, &err.to_string());
                None
            }
        }
    }

    /// Replaces `(surface, output)` pairs, from `crate::wayland::run` before first apply and
    /// `OutputHandler` on each hotplug (ADR-0038 decision 3).
    pub fn set_instances(&mut self, instances: Vec<SurfaceInstance>) {
        self.instances = instances;
    }

    /// Arms the per-`Scene::apply` lock-authentication veto (ADR-0052 decision 3) when lock is
    /// requested, not when `locked` arrives; a reload in between could strip the field about to be
    /// shown.
    pub fn set_session_locked(&mut self, locked: bool) {
        self.holds_session_lock = locked;
    }

    /// The set last stored by [`Self::set_instances`], for `crate::wayland::App` to diff without a
    /// second copy that could drift.
    pub fn instances(&self) -> &[SurfaceInstance] {
        &self.instances
    }

    /// Declared surfaces from the applied evaluation, never `shell.lua`. Hotplug expands against
    /// this (ADR-0038 decision 3). Empty before any apply.
    pub fn applied_surface_specs(&self) -> Vec<SurfaceSpec> {
        self.state.applied_specs.clone()
    }

    /// Pushes `screens` (ADR-0041 decision 2) and reports change. As with `set_rescue_state`, the
    /// caller gates a reload on it, so a duplicate must not trigger one.
    pub fn set_screens(&mut self, payload: serde_json::Value) -> bool {
        if self.screens_payload == payload {
            return false;
        }
        match crate::lua::json::to_lua(self.loader.lua(), &payload) {
            Ok(value) => {
                let previous = self.screens_handle.hydrate(value);
                self.screens_handle.notify_change(self.loader.lua(), previous);
                self.screens_payload = payload;
                true
            }
            Err(err) => {
                warn!("failed to convert the screen list to a Lua value: {err}");
                false
            }
        }
    }

    /// Records which of an instance's axes are measured from its own tree rather than allocated to
    /// it, for [`Self::set_instance_size`] to honour, and puts `ceiling` on those axes.
    /// `crate::wayland::layer` pushes both from the *resolved* panel spec, on creation and on every
    /// later pass: the socket parser cannot tell a genuinely omitted extent from a signal-bound
    /// one, since `Bound` defers both to `SizeMode::Content`, and pinning `available` for
    /// a signal-bound extent would strand the surface at its output's size.
    ///
    /// The ceiling is written, not merely permitted, because on a measured axis `available` *is*
    /// the ceiling and nothing else may set it -- `set_instance_size` declines that axis outright.
    /// An axis that becomes measured therefore has to be given one here, or it would keep whatever
    /// the last configure left: a panel reloaded from `width = 200` to a measured width would solve
    /// its content against 200 for the rest of the generation, and no later configure could widen
    /// it. Idempotent, and it dirties only when the ceiling actually moved.
    ///
    /// Unknown ids are ignored, like `set_instance_size`'s.
    pub fn set_measured_axes(&mut self, instance_id: &str, axes: (bool, bool), ceiling: layout::LogicalSize) {
        let Some(instance) = self.instances.iter_mut().find(|i| i.instance_id == instance_id) else {
            return;
        };
        instance.measured_axes = axes;
        let bounded = layout::LogicalSize {
            width: if axes.0 { ceiling.width } else { instance.available.width },
            height: if axes.1 { ceiling.height } else { instance.available.height },
        };
        if instance.available == bounded {
            return;
        }
        instance.available = bounded;
        self.dirty.mark_instance(instance_id);
    }

    /// Forces a whole-scene pass, for a surface state change no signal carries.
    pub fn mark_dirty(&self) {
        self.dirty.mark();
    }

    /// Replaces one instance's compositor-configured `available` size and marks that instance
    /// dirty ([`DirtyFlag::mark_instance`], ADR-0023). Ignore unknown ids instead of dirtying a
    /// nonexistent surface.
    ///
    /// A measured axis is left alone (`SurfaceInstance::measured_axes`). The compositor's answer
    /// there is the size this surface asked for after measuring its own content, so writing it back
    /// would turn the measurement into its own ceiling and the content could never outgrow the size
    /// it happened to open at.
    pub fn set_instance_size(&mut self, instance_id: &str, size: layout::LogicalSize) {
        let Some(instance) = self.instances.iter_mut().find(|i| i.instance_id == instance_id) else {
            return;
        };
        let size = layout::LogicalSize {
            width: if instance.measured_axes.0 { instance.available.width } else { size.width },
            height: if instance.measured_axes.1 { instance.available.height } else { size.height },
        };
        if instance.available == size {
            // Same no-op rule as `set_rescue_state`.
            return;
        }
        instance.available = size;
        self.dirty.mark_instance(instance_id);
    }

    /// Retained scene; `paint_surface` looks up the resolved tree by the `"{id}@{output}"` id in
    /// its `TrackedSurface`.
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// [`Scene::set_field_draft`], marking `instance_id` for the pass that resizes the field.
    pub fn set_field_draft(&mut self, instance_id: &str, id: layout::NodeId, text: &str) -> bool {
        let resized = self.scene.set_field_draft(instance_id, id, text, &self.shaping);
        if resized {
            self.dirty.mark_instance(instance_id);
        }
        resized
    }

    /// [`Scene::set_field_scroll`].
    pub fn set_field_scroll(&mut self, instance_id: &str, id: layout::NodeId, scroll: f32) -> bool {
        self.scene.set_field_scroll(instance_id, id, scroll)
    }

    /// `initial_text` of the fields created since the last call; see [`Scene::take_seeds`].
    pub fn take_seeds(&mut self) -> Vec<(crate::layout::scene::NodeId, String)> {
        self.scene.take_seeds()
    }

    /// This turn's resolve split, for the idle report; see [`Scene::take_resolve_split`].
    pub fn take_resolve_split(&mut self) -> crate::layout::scene::ResolveSplit {
        self.scene.take_resolve_split()
    }

    /// This turn's tick split; see [`Scene::take_tick_split`].
    pub fn take_tick_split(&mut self) -> crate::layout::scene::TickSplit {
        self.scene.take_tick_split()
    }

    /// Relays what a surface's paint actually drew into the retained scene, where a `retain`ing
    /// `image` stops covering the gap and a `transition` starts (ADR-0183), and `image_status`
    /// learns a decode landed or failed. Narrow on purpose: the
    /// scene is not handed out mutably for a caller to walk itself.
    pub fn note_drawn_images(
        &mut self,
        instance_id: &str,
        drawn: &[crate::layout::paint::DrawnImage],
        now: std::time::Instant,
    ) {
        self.scene.note_drawn_images(instance_id, drawn, now);
        self.scene.note_image_loads(instance_id, drawn, self.loader.lua());
        self.mark_layout_readers();
    }

    /// Marks the readers of each layout measurement written since the last take. Written after
    /// this turn's pass, so their pass is owed now rather than at the next unrelated wake.
    fn mark_layout_readers(&mut self) {
        for id in crate::lua::signal::take_layout_changed(self.loader.lua()) {
            self.dirty.mark_cell(id);
            self.owes_pass = true;
        }
    }

    /// Drops a departed instance's retained tree and its `(surface, output)` pair, whenever its
    /// surface is destroyed. Both, because an instance left here reconciles as unchanged, so
    /// nothing rebuilds its surface and the scene re-grows a tree with no `wl_surface` behind it.
    pub fn forget_surface(&mut self, instance_id: &str) {
        crate::lua::signal::forget_instance(self.loader.lua(), instance_id);
        self.scene.forget(instance_id, self.loader.lua());
        self.instances.retain(|instance| instance.instance_id != instance_id);
        self.mark_layout_readers();
    }

    /// This generation's `Lua` builds the `on_click` argument (ADR-0050 decision 3):
    /// `crate::wayland::App` holds the `mlua::Function`, not a VM. Do not hold the borrow across
    /// the call; see [`crate::wayland::App::fire_on_click`].
    pub fn lua(&self) -> &mlua::Lua {
        self.loader.lua()
    }

    #[cfg(test)]
    pub(crate) fn loader(&self) -> &Loader {
        &self.loader
    }

    /// Evaluates `shell.lua` into `pending`, or sets rescue. `true` means there is something to
    /// apply; [`Self::pending_surfaces`] names what it rebuilds (ADR-0216).
    pub fn reevaluate(&mut self) -> bool {
        self.clear_change_handlers();
        match evaluate_and_specs(&self.loader, &self.shell_lua_path) {
            Ok((output, specs)) => {
                debug!("shell.lua evaluated; applying it");
                // Finish the collector's cycle here, outside every budget: otherwise the apply's
                // getters pay for sweeping the old evaluation's garbage (ADR-0290).
                let _ = self.loader.lua().gc_collect();
                self.state.pending = Some((output, specs));
                true
            }
            Err(err) => {
                // Whatever it registered before raising goes with it. `discard` after the clear
                // because that arms the next evaluation; nothing is running one now.
                self.clear_change_handlers();
                lua::timer::discard(self.loader.lua());
                error!("shell.lua re-evaluation failed: {err}");
                self.set_rescue_state(true, &err.to_string());
                false
            }
        }
    }

    /// The poll loop's only timeout: when the earliest pending `delay(signal, ms)` is due
    /// (ADR-0146) or the earliest open `pulse(signal, ms)` window closes (ADR-0153). `None` while
    /// nothing is pending, which is the idle case ADR-0124 keeps timeout-free.
    pub fn next_wake_deadline(&self) -> Option<std::time::Instant> {
        let signals = crate::lua::signal::next_wake_deadline(self.loader.lua());
        let timers = crate::lua::timer::next_deadline(self.loader.lua());
        let reset = self.owes_pass.then(std::time::Instant::now);
        // Scheduled apart because only one of them is answered by re-reading the scene: a due
        // signal dirties the tree, a due timer runs config code (ADR-0203).
        signals.into_iter().chain(timers).chain(reset).min()
    }

    /// Runs the config callbacks whose deadline has passed, before this turn's dirty-tree
    /// re-resolve. A `state:set` inside one therefore reaches this turn's pass, which resolves the
    /// *applied* tree.
    pub fn fire_due_timers(&mut self) {
        crate::lua::timer::dispatch_due(self.loader.lua(), std::time::Instant::now());
    }

    /// Writes each `delay` come due and `pulse` window closed, so this turn's re-resolve adopts
    /// the new value in their readers.
    pub fn wake_due_signals(&mut self) {
        let now = crate::lua::timer::now(self.loader.lua());
        for cell in crate::lua::signal::take_due_wake(self.loader.lua(), now) {
            self.dirty.mark_cell(cell);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::layout::instance::{OutputGeometry, expand_instances};
    use crate::lua::surfaces::surface_specs;
    use crate::lua::tests::probe;
    use shared::{ProcessExited, ProcessOutputLine, StateSnapshot, SupervisorFrame};

    pub(crate) fn write_shell_lua(dir: &std::path::Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("shell.lua");
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Reads `rescue:get()` by probe script; `CapabilityHandle` has no getter, so this is the
    /// only way to observe `set_rescue_state`'s stored value.
    pub(super) fn rescue_state(loader: &Loader) -> (bool, String) {
        let setup = "is_rescue, error_log = mantle.rescue:get().is_rescue, mantle.rescue:get().error_log";
        (probe(loader, setup, "is_rescue"), probe(loader, setup, "error_log"))
    }

    /// Pushes `payload` for `capability`; `workspace` is off the roster, so any payload hydrates it.
    pub(crate) fn push(client: &RendererClient, capability: &str, revision: u32, payload: serde_json::Value) {
        let snapshot = StateSnapshot { capability: capability.to_string(), revision, payload };
        client.apply_state_snapshot(snapshot).unwrap();
    }

    /// Client with a real outbound channel; return its receiver for queued socket frames.
    pub(crate) fn test_client(
        shell_lua_path: &std::path::Path,
    ) -> (RendererClient, mpsc::UnboundedReceiver<RendererFrame>) {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let dirty = DirtyFlag::new();
        let loader = Loader::new(dirty.clone(), shell_lua_path.parent().unwrap()).unwrap();
        let commands = CommandSender::new(0, outbound_tx);
        let process_registry = ProcessRegistry::new(commands.clone());
        lua::process::register(loader.lua(), process_registry.clone()).unwrap();
        let palette_registry = PaletteRegistry::new(None);
        lua::palette::register(loader.lua(), palette_registry.clone()).unwrap();
        let file_registry = FileRegistry::checking();
        lua::file::register(loader.lua(), file_registry.clone()).unwrap();
        let client = RendererClient::new(
            loader,
            shell_lua_path.to_path_buf(),
            ShapingHandle::spawn(),
            commands,
            process_registry,
            OffThread { palette: palette_registry, file: file_registry },
            dirty,
        )
        .unwrap();
        (client, outbound_rx)
    }

    /// A client over a fresh `shell.lua` holding `source`; the dir must outlive the test.
    pub(super) fn client_for(
        source: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf, RendererClient, mpsc::UnboundedReceiver<RendererFrame>) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), source);
        let (client, outbound_rx) = test_client(&path);
        (dir, path, client, outbound_rx)
    }

    /// A client over a config path that does not exist, for tests that push state or run Lua directly.
    pub(crate) fn bare_client() -> (RendererClient, mpsc::UnboundedReceiver<RendererFrame>) {
        test_client(std::path::Path::new("/no/such/shell.lua"))
    }

    /// One 1920x1080 `"TEST"` output keeps fixture ids readable (`"bar@TEST"`).
    fn test_outputs() -> Vec<OutputGeometry> {
        vec![OutputGeometry { name: "TEST".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } }]
    }

    /// `crate::wayland::run` startup order: evaluate, expand, store instances, apply.
    pub(crate) fn run_startup(client: &mut RendererClient) -> bool {
        let Some(specs) = client.run_startup_evaluation() else {
            return false;
        };
        let instances = expand_instances(&specs, &test_outputs());
        client.set_instances(instances);
        client.apply_instances()
    }

    /// Reloads `path` with `source` the way `App::apply_pending` does, keeping the one output.
    pub(super) fn reload(client: &mut RendererClient, path: &std::path::Path, source: &str) -> bool {
        std::fs::write(path, source).unwrap();
        if !client.reevaluate() {
            return false;
        }
        let (specs, _) = client.pending_surfaces().unwrap();
        client.set_instances(expand_instances(&specs, &test_outputs()));
        client.handle_apply_pending()
    }

    /// Instances for exactly `ids`, for tests that seed `state.pending` instead of [`run_startup`].
    pub(super) fn instances_for(ids: &[&str]) -> Vec<SurfaceInstance> {
        ids.iter()
            .map(|id| SurfaceInstance {
                instance_id: format!("{id}@TEST"),
                declared_id: (*id).to_string(),
                output: "TEST".to_string(),
                available: layout::LogicalSize { width: 1920.0, height: 1080.0 },
                measured_axes: (false, false),
            })
            .collect()
    }

    /// Next queued non-start frame, or a panic naming what was missing. Reading `mantle.lock`
    /// queues a start (ADR-0070 decision 1), so tests would otherwise step over it; the dedicated
    /// `capability_reads_start_only_rostered_names_once` test accounts for starts.
    pub(crate) fn queued_frame(outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> RendererFrame {
        loop {
            match outbound_rx.try_recv().expect("a frame must have been queued for the socket thread") {
                RendererFrame::StartCapability { .. } => continue,
                frame => return frame,
            }
        }
    }

    /// All queued capability starts that `queued_frame` skips.
    pub(super) fn queued_starts(outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Vec<String> {
        let mut started = Vec::new();
        while let Ok(frame) = outbound_rx.try_recv() {
            if let RendererFrame::StartCapability { capability } = frame {
                started.push(capability.to_string());
            }
        }
        started
    }

    /// ADR-0070 decision 1: reading starts it, an evaluation touching no capability costs nothing, and
    /// `__index` fires once per name because it moves the member onto the table (a `computed` in a
    /// `list` `each` reads `mantle.audio` once per row per layout pass; starting per read would be
    /// a frame per row per frame). A typo stays ordinary nil so the config line gets named; any other
    /// metamethod result would make `mantle.audioo:get()` fail inside the engine.
    #[test]
    fn capability_reads_start_only_rostered_names_once() {
        for (script, expected, case) in [
            ("local _ = mantle.audio", vec!["audio"], "a read starts it"),
            ("local _ = mantle.version.major", vec![], "`version` is off the roster and has nothing behind it"),
            ("for _ = 1, 50 do local _ = mantle.audio end", vec!["audio"], "a re-read queues no second start"),
            ("assert(mantle.audioo == nil)", vec![], "a name no capability owns reads nil and starts nothing"),
        ] {
            let (client, mut outbound_rx) = bare_client();
            client.loader.lua().load(script).exec().unwrap();
            assert_eq!(queued_starts(&mut outbound_rx), expected, "{case}");
        }
    }

    #[test]
    fn apply_state_snapshot_updates_the_live_signal_without_evaluating_shell_lua() {
        // ADR-0029: the first off-roster snapshot creates the Lua global, not an error.
        assert!(shared::Capability::from_name("workspace").is_none(), "this test needs a genuinely unrostered name");
        let (client, _outbound_rx) = bare_client();
        for (capability, payload, probe_expr, expected) in [
            ("audio", serde_json::json!({ "app_name": "Zen" }), "mantle.audio:get().app_name", "Zen"),
            ("workspace", serde_json::json!({ "active": 2 }), "mantle.workspace:get().active", "2"),
        ] {
            push(&client, capability, 1, payload);
            let read = probe::<mlua::Value>(&client.loader, &format!("seen = {probe_expr}"), "seen");
            assert_eq!(read.to_string().unwrap(), expected, "{capability}");
        }
    }

    #[test]
    fn a_hover_bound_by_a_config_survives_resolution_and_drives_what_it_is_bound_to() {
        // The untested half of ADR-0062: a config-authored `hover` property remains a handle in the
        // resolved tree (decision 3), and writing it moves a second bound node. `pointer_frame`
        // needs a compositor, so drive `layout::hover` on a real resolved tree instead.
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r#"
            local hovered = hover("pill")
            return { panel {
                id = "bar", layer = "top", width = "fill", height = 40,
                child = row {
                    width = 100, height = 20, hover = hovered,
                    children = { rect { id = "tip", width = 10, height = 10, visible = hovered } },
                },
            } }
            "#,
        );
        assert!(run_startup(&mut client), "the config must resolve");

        let hover_row =
            |client: &RendererClient| client.scene.surface("bar@TEST").expect("the bar resolves").children[0].clone();
        assert!(!hover_row(&client).children[0].visible, "nothing is hovered before the pointer arrives");

        // `hover_writes` receives the same tree as `App::sync_hover` when the pointer enters.
        let tree = client.scene.surface("bar@TEST").unwrap();
        let writes = layout::hover::hover_writes_at(tree, Some(layout::hit::LogicalPoint { x: 50.0, y: 10.0 }));
        assert_eq!(writes.len(), 1, "one node declared a hover, so there is one write");
        assert!(writes[0].hovered, "the pointer is inside the row that declared it");
        assert!(writes[0].rect.is_some(), "and it reports where it is, for a tooltip to anchor to");

        for write in &writes {
            write
                .signal
                .hover_handle()
                .expect("a config-authored hover is writable by the engine")
                .set_changed(mlua::Value::Boolean(write.hovered));
        }
        assert!(client.re_resolve_if_dirty(), "a hover that changed has to re-resolve the scene");
        assert!(hover_row(&client).children[0].visible, "the node bound to the hover is showing now");

        // Leave again, the edge callback designs lose when reevaluation replaces the node
        // (ADR-0062 decision 1).
        let tree = client.scene.surface("bar@TEST").unwrap();
        for write in layout::hover::hover_writes_at(tree, None) {
            write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(write.hovered));
        }
        assert!(client.re_resolve_if_dirty());
        assert!(!hover_row(&client).children[0].visible, "the pointer left, so it is hidden again");
    }

    /// Absolute centres of `hover` nodes, accumulating parent-relative origins as `layout::hit`
    /// does.
    fn hover_region_centres(
        node: &crate::layout::ResolvedNode,
        x: f32,
        y: f32,
        out: &mut Vec<layout::hit::LogicalPoint>,
    ) {
        let (x, y) = (x + node.rect.x, y + node.rect.y);
        // A zero-size node has no centre anything can point at.
        if node.properties.contains_key("hover") && node.rect.width > 0.0 && node.rect.height > 0.0 {
            out.push(layout::hit::LogicalPoint { x: x + node.rect.width / 2.0, y: y + node.rect.height / 2.0 });
        }
        for child in &node.children {
            hover_region_centres(child, x, y, out);
        }
    }

    #[test]
    fn a_popup_bound_to_a_hover_slot_opens_for_that_slot_alone_and_closes_when_the_pointer_leaves() {
        // The other half of ADR-0062: `hover(name)` -> pill `hover`, `hover_rect(name)` ->
        // popup `anchor_rect`, and the popup's `visible`.
        //
        // Two slots, because the contract is that a point inside one region is outside the other:
        // one region alone could not tell "opens when hovered" from "always open".
        let (_dir, path, mut client, _outbound_rx) = client_for(
            r##"
            local left, right = hover("left"), hover("right")
            return {
                panel {
                    id = "bar", layer = "top", width = "fill", height = 40,
                    child = row {
                        width = "fill", height = "fill", spacing = 0,
                        children = {
                            row { id = "left_pill", width = 60, height = 20, hover = left },
                            row { id = "right_pill", width = 60, height = 20, hover = right },
                        },
                    },
                },
                popup {
                    id = "tip", parent = "bar",
                    anchor_rect = hover_rect("left"),
                    visible = left,
                    width = 120, height = 40,
                    grab = false,
                    anchor = "bottom_left", gravity = "bottom_right",
                    child = rect { width = 120, height = 40, background = "#000000ff" },
                },
            }
            "##,
        );
        assert!(run_startup(&mut client), "the fixture must resolve into a scene");

        let tip_is_up = |client: &RendererClient| client.scene.surface("tip").expect("the popup resolves").visible;
        assert!(!tip_is_up(&client), "a hover popup is not up before the pointer has been anywhere");

        // Two bugs that happen before the first hover. `grab` defaults true, but a grabbing
        // popup needs an input serial hover cannot produce, so the compositor refused `visible =
        // true` on every re-resolve. And `anchor_rect` is required non-zero, while its rect signal
        // begins nil, which reads as absent rather than as a rect.
        let (_output, specs) = evaluate_and_specs(&client.loader, &path).expect("the fixture evaluates");
        let tip = specs
            .iter()
            .find_map(|spec| match spec {
                SurfaceSpec::Popup(popup) if popup.id == "tip" => Some(popup),
                _ => None,
            })
            .expect("the fixture declares the popup");
        assert!(!tip.grab, "a hover-opened popup must not ask for a grab; there is no click to arm its serial");
        assert!(
            tip.anchor_rect.width > 0.0 && tip.anchor_rect.height > 0.0,
            "anchor_rect has to be a real non-zero rect before anything has been hovered, got {:?}",
            tip.anchor_rect
        );

        // Cloned: the loop re-resolves through `client`, and the centres are the bar as it stood
        // before any of that.
        let bar = client.scene.surface("bar@TEST").unwrap().clone();
        let mut centres = Vec::new();
        hover_region_centres(&bar, 0.0, 0.0, &mut centres);
        assert_eq!(centres.len(), 2, "the fixture declares one hover region per pill");

        let mut opened_by = 0;
        for centre in centres {
            let writes = layout::hover::hover_writes_at(&bar, Some(centre));
            // Applied as `App::sync_hover` does: a point inside one region is outside the others,
            // and turning those *off* is half the walk.
            for write in &writes {
                assert_eq!(
                    write.hovered,
                    write.rect.is_some_and(|rect| rect.contains(centre)),
                    "a region lights exactly when it contains the point"
                );
            }
            for write in writes {
                write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(write.hovered));
                let Some(rect) = write.rect else {
                    continue;
                };
                // The popup's `anchor_rect`; a wrong shape fails the re-resolve below.
                let table = crate::lua::marshal::rect_table(client.lua(), rect).unwrap();
                write.signal.hover_rect_handle().unwrap().set_changed(mlua::Value::Table(table));
            }
            client.re_resolve_if_dirty();
            if tip_is_up(&client) {
                opened_by += 1;
            }
        }
        assert_eq!(opened_by, 1, "the slot the popup names opens it, and the other one does not");

        // Closing again. `anchor_rect` keeps its last rect rather than clearing, which is what
        // preserves the non-zero rule on the way out.
        let bar = client.scene.surface("bar@TEST").unwrap();
        for write in layout::hover::hover_writes_at(bar, None) {
            write.signal.hover_handle().unwrap().set_changed(mlua::Value::Boolean(false));
        }
        assert!(client.re_resolve_if_dirty());
        assert!(!tip_is_up(&client), "the pointer left the bar, so the popup closed");
    }

    #[test]
    fn a_surface_whose_visible_reads_a_capability_goes_up_and_down_with_the_payload() {
        // Regression: a card with no `visible` binding sat in the corner saying "no notifications"
        // for ever. The engine contract under it is that a surface's `visible` may be a signal over
        // a capability, and that a snapshot re-resolve moves the surface.
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r##"
            return { panel {
                id = "notification_area", layer = "top", anchor = { top = true, right = true },
                width = 200, height = 60,
                -- `false` until the first push, as at real boot (nil would leave `visible` at its default, true).
                visible = mantle.notifications:map(function(n) return #n.feed > 0 end, false),
                child = rect { width = 200, height = 60, background = "#111111ff" },
            } }
            "##,
        );
        assert!(run_startup(&mut client), "the fixture must resolve into a scene");

        let card_is_up = |client: &RendererClient| {
            client.scene.surface("notification_area@TEST").expect("the card resolves").visible
        };
        assert!(!card_is_up(&client), "nothing has been received, so there is nothing to show");

        push(
            &client,
            "notifications",
            1,
            serde_json::json!({ "feed": [{ "id": 7, "app_name": "Zen", "summary": "a thing happened" }] }),
        );
        assert!(client.re_resolve_if_dirty());
        assert!(card_is_up(&client), "a notification in the feed puts the card up");

        // The Supervisor expires it from the feed (ADR-0033); auto-hide is only this empty list,
        // with no timer here.
        push(&client, "notifications", 2, serde_json::json!({ "feed": [] }));
        assert!(client.re_resolve_if_dirty());
        assert!(!card_is_up(&client), "an expired feed takes the card with it");
    }

    #[test]
    fn every_rostered_capability_rescue_and_screens_are_on_the_mantle_table_and_read_nil_or_empty_at_boot() {
        // ADR-0037: every rostered capability is `mantle.<name>`, a live signal reading nil at
        // boot, never an index-into-nil rescue error.
        let (client, _outbound_rx) = bare_client();

        for capability in shared::Capability::ALL.iter().map(|c| c.as_str()) {
            let setup = format!("is_nil = mantle.{capability}:get() == nil");
            assert!(
                probe::<bool>(&client.loader, &setup, "is_nil"),
                "mantle.{capability} should read nil before its first snapshot"
            );
        }

        // `rescue` and `screens` both live under `mantle.*`, like capabilities.
        let setup = r#"
            rescued = mantle.rescue:get().is_rescue
            screen_count = #mantle.screens:get()
            bare_rescue_gone = rescue == nil
            bare_screens_gone = screens == nil
        "#;
        assert!(!probe::<bool>(&client.loader, setup, "rescued"));
        assert_eq!(probe::<i64>(&client.loader, setup, "screen_count"), 0);
        assert!(probe::<bool>(&client.loader, setup, "bare_rescue_gone"));
        assert!(probe::<bool>(&client.loader, setup, "bare_screens_gone"));
    }

    #[test]
    fn no_rostered_capability_is_left_as_a_bare_global() {
        // A global, once set, is never removed; a bare seed would work until colliding with a node
        // constructor as `lock` did (ADR-0052 decision 1).
        let (client, _outbound_rx) = bare_client();

        for capability in shared::Capability::ALL.iter().map(|c| c.as_str()) {
            // `lock` is the legitimate node constructor.
            if capability == "lock" {
                continue;
            }
            let setup = format!("is_nil = {capability} == nil");
            assert!(
                probe::<bool>(&client.loader, &setup, "is_nil"),
                "{capability} is still a bare global; it belongs on mantle.{capability}"
            );
        }
    }

    #[test]
    fn an_unrostered_push_refuses_to_replace_a_name_the_mantle_table_already_holds() {
        // `rescue` reports config failures; replacing it with an empty capability hides the shell's
        // breakage, as in ADR-0052 decision 1's `lock` bug.
        let (client, _outbound_rx) = bare_client();

        let snapshot = StateSnapshot { capability: "rescue".to_string(), revision: 1, payload: serde_json::json!({}) };
        let err = client.apply_state_snapshot(snapshot).unwrap_err().to_string();
        assert!(err.contains("already something else"), "the refusal must say why: {err}");

        // Real `rescue` still reads its own table, not an empty capability.
        assert!(probe::<bool>(&client.loader, "intact = mantle.rescue:get().is_rescue == false", "intact"));
    }

    #[test]
    fn mantle_version_is_three_integers_a_config_can_compare() {
        // Shape matters: configs compare `mantle.version.major > 0` or `minor >= 2`, so all three
        // fields must be numeric.
        let (client, _outbound_rx) = bare_client();

        let setup = "major, minor, patch = mantle.version.major, mantle.version.minor, mantle.version.patch";
        // Read through Lua against Cargo's string, not a second builder call: assert what config
        // sees. This also reaches `lua::namespace::version_table`'s `expect`, so it needs no
        // narrower test.
        let field = |name: &str| probe::<i64>(&client.loader, setup, name);
        assert_eq!(format!("{}.{}.{}", field("major"), field("minor"), field("patch")), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn mantle_config_dir_is_the_directory_shell_lua_was_loaded_from() {
        // Derive from the loaded path, not a fresh resolution, so explicit `shell.lua` cannot
        // report an unread directory.
        let (client, _outbound_rx) = test_client(std::path::Path::new("/opt/mantle-config/shell.lua"));
        let dir = probe::<String>(&client.loader, "dir = mantle.config_dir", "dir");
        assert_eq!(dir, "/opt/mantle-config");
    }

    /// `RendererClient::new` seeds after `Loader::new` registers the root-role constructors. A
    /// bare `lock` seed would silently overwrite the constructor; `set` over an existing global
    /// is silent, so `lock { ... }` would report "attempt to call a userdata value" against the
    /// config rather than the seed. Assert after a full generation, because that registration
    /// order is the contract.
    #[test]
    fn a_full_generation_keeps_lock_as_the_node_constructor_and_puts_the_capability_on_mantle() {
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r##"return {
                panel { id = "bar", layer = "top" },
                lock {
                    id = "screen",
                    child = text { content = mantle.lock:map(function(s) return (s and s.error) or "" end) },
                },
            }"##,
        );
        push(
            &client,
            "lock",
            1,
            serde_json::json!({ "active": false, "authenticating": false, "attempts": 2, "error": "authentication failed" }),
        );

        assert!(run_startup(&mut client), "a config declaring a lock screen must build a scene");

        let setup = r#"
            lock_kind = lock { id = "screen" }.kind
            capability_type = type(mantle.lock)
            attempts = mantle.lock:get().attempts
        "#;
        assert_eq!(
            probe::<String>(&client.loader, setup, "lock_kind"),
            "lock",
            "the global `lock` must still be the node constructor"
        );
        assert_eq!(probe::<String>(&client.loader, setup, "capability_type"), "userdata");
        assert_eq!(
            probe::<i64>(&client.loader, setup, "attempts"),
            2,
            "the `lock` StateSnapshot must reach `mantle.lock`, not a bare global nothing registered"
        );
    }

    #[test]
    fn a_declared_session_process_declares_the_name_and_the_stop_signal_the_config_chose() {
        let (client, mut outbound_rx) = bare_client();

        client
            .loader
            .lua()
            .load(r#"rec = session_process { name = "screen-recorder", stop_signal = "INT" }"#)
            .exec()
            .unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("declaring a session process must queue a RendererFrame::Command");
        };
        assert_eq!(envelope.params.capability, "processes");
        assert_eq!(envelope.params.action, "declare");
        assert_eq!(envelope.params.arguments, vec![serde_json::json!("screen-recorder"), serde_json::json!("INT")]);
    }

    #[test]
    fn a_session_process_with_an_unknown_stop_signal_raises_and_declares_nothing() {
        let (client, mut outbound_rx) = bare_client();

        let err =
            client.loader.lua().load(r#"session_process { name = "rec", stop_signal = "TEM" }"#).exec().unwrap_err();
        let err = crate::lua::describe(&err);

        assert!(
            err.contains(
                "session_process: stop_signal: expected a signal name, got string \"TEM\"; did you mean `TERM`?"
            ),
            "got {err}"
        );
        assert!(
            crate::lua::capability::tests::queued_command(&mut outbound_rx).is_none(),
            "the invalid declaration must not reach the Supervisor"
        );
    }

    #[test]
    fn a_session_processs_fields_read_what_the_supervisor_pushed_for_that_name() {
        let (client, _outbound_rx) = bare_client();
        client.loader.lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();

        push(
            &client,
            "processes",
            1,
            serde_json::json!({
                "sessions": {
                    "recorder": { "running": true, "pid": 4321, "started_at": 1700000000,
                                  "exit_code": null, "start_error": "" }
                }
            }),
        );

        let setup = r#"
            running = rec.running:get()
            pid = rec.pid:get()
            started = rec.started_at:get()
            code = rec.exit_code:get()
        "#;
        assert!(probe::<bool>(&client.loader, setup, "running"));
        assert_eq!(probe::<i64>(&client.loader, setup, "pid"), 4321);
        assert_eq!(probe::<i64>(&client.loader, setup, "started"), 1_700_000_000);
        assert_eq!(
            probe::<Option<i64>>(&client.loader, setup, "code"),
            None,
            "a run still going has no exit status, and nil is what a config must be able to test"
        );
    }

    #[test]
    fn an_undeclared_names_fields_read_nil_rather_than_a_stopped_program() {
        let (client, _outbound_rx) = bare_client();
        client.loader.lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();

        push(&client, "processes", 1, serde_json::json!({ "sessions": {} }));

        assert_eq!(
            probe::<Option<bool>>(&client.loader, "running = rec.running:get()", "running"),
            None,
            "before the Supervisor has answered, `running` is unknown rather than false"
        );
    }

    #[test]
    fn a_session_processs_methods_name_the_program_the_declaration_did() {
        let (client, mut outbound_rx) = bare_client();
        client.loader.lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();
        let _declare = queued_frame(&mut outbound_rx);

        client
            .loader
            .lua()
            .load(
                r#"
                rec:start("gpu-screen-recorder", { "-w", "DP-1" })
                rec:signal("USR2")
                rec:stop()
            "#,
            )
            .exec()
            .unwrap();

        let actions: Vec<(String, Vec<serde_json::Value>)> = (0..3)
            .map(|_| match queued_frame(&mut outbound_rx) {
                RendererFrame::Command(envelope) => (envelope.params.action, envelope.params.arguments),
                other => panic!("a session-process method must queue a Command, got {other:?}"),
            })
            .collect();
        assert_eq!(actions[0].0, "start");
        assert_eq!(
            actions[0].1,
            vec![
                serde_json::json!("recorder"),
                serde_json::json!("gpu-screen-recorder"),
                serde_json::json!(["-w", "DP-1"])
            ]
        );
        assert_eq!(actions[1], ("signal".to_string(), vec![serde_json::json!("recorder"), serde_json::json!("USR2")]));
        assert_eq!(actions[2], ("stop".to_string(), vec![serde_json::json!("recorder")]));
    }

    #[test]
    fn two_declarations_of_one_program_are_one_handle() {
        // The same reason `persistent_table` caches: a module and `shell.lua` may both declare it,
        // and in-place reload reruns both. Two handles would mean two sets of field signals, one of
        // them stale.
        let (client, _outbound_rx) = bare_client();

        let setup = r#"
            first = session_process { name = "recorder" }
            second = session_process { name = "recorder", stop_signal = "INT" }
            same = rawequal(first, second)
        "#;
        assert!(probe::<bool>(&client.loader, setup, "same"), "the declared name is the identity");
    }

    /// A config `on_click` invoking lock queues a real command envelope.
    #[test]
    fn a_config_calling_the_lock_action_queues_a_command_for_the_supervisor() {
        let (client, mut outbound_rx) = bare_client();

        client.loader.lua().load(r#"mantle.lock:lock()"#).exec().unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("a capability write must be queued as RendererFrame::Command");
        };
        assert_eq!(envelope.params.capability, "lock");
        assert_eq!(
            envelope.params.action, "lock",
            "the action `supervisor/src/capabilities/lock/controller.rs`'s dispatch answers to"
        );
    }

    /// A `window` and `popup` alongside panels.
    fn three_roles_config() -> &'static str {
        r#"return {
            panel { id = "bar", layer = "top" },
            window { id = "settings", title = "Settings", min_size = { width = 320, height = 240 } },
            popup { id = "menu", parent = "bar", width = 200, height = 120,
                    anchor_rect = { x = 12, y = 32, width = 86, height = 24 } },
        }"#
    }

    #[test]
    fn every_declared_role_comes_back_from_the_startup_evaluation_tagged_with_its_own_spec() {
        // All roles matter: `expand_instances` and `create_surfaces` branch on the variant; an
        // untagged role could bind only as a layer surface.
        let (_dir, _, mut client, _outbound_rx) = client_for(three_roles_config());

        let specs = client.run_startup_evaluation().expect("all three roles must evaluate");

        assert!(matches!(specs.as_slice(), [SurfaceSpec::Panel(_), SurfaceSpec::Window(_), SurfaceSpec::Popup(_)]));
        assert_eq!(specs.iter().map(SurfaceSpec::declared_id).collect::<Vec<_>>(), ["bar", "settings", "menu"]);
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    #[test]
    fn a_destroyed_surface_leaves_the_reconcile_set_with_its_tree() {
        let (_dir, _, mut client, _outbound_rx) = client_for(r#"return panel { id = "bar", layer = "top" }"#);
        client.set_instances(instances_for(&["bar", "menu"]));

        client.forget_surface("bar@TEST");

        assert_eq!(
            client.instances().iter().map(|instance| instance.instance_id.as_str()).collect::<Vec<_>>(),
            ["menu@TEST"]
        );
    }

    #[test]
    fn the_fingerprint_carries_every_role_so_adding_a_window_is_a_topology_change() {
        // ADR-0049 decision 3: a window object may come and go within a generation, but its
        // *declaration* is fixed, so adding one changes topology.
        let (dir, path, mut client, _outbound_rx) = client_for(r#"return panel { id = "bar", layer = "top" }"#);
        client.state.applied_specs = surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap();

        write_shell_lua(dir.path(), three_roles_config());
        assert!(client.reevaluate());
        let (specs, rebuilt) = client.pending_surfaces().expect("staged");
        assert_eq!(specs.len(), 3);
        assert_eq!(rebuilt.len(), 2, "only the added window and popup are new; the panel keeps its layer surface");
    }

    /// Lock screen whose `child` holds the `secure_submit` field.
    fn lock_config(background: &str) -> String {
        format!(
            r##"return {{
                panel {{ id = "bar", layer = "top" }},
                lock {{ id = "screen", child = column {{ background = "{background}", children = {{
                    textfield {{ mask_character = "*", secure_submit = {{ capability = "lock", action = "authenticate" }} }},
                }} }} }},
            }}"##
        )
    }

    #[test]
    fn an_in_place_reload_may_restyle_a_live_lock_screen_but_not_remove_its_way_out() {
        // `SurfaceFingerprint::Lock` carries only `id`, so edits inside the lock rebuild nothing. Restyles must land (ADR-0052 decision 2), but
        // removing the way out must be refused; `Scene::apply` rollback preserves the live tree.
        let (dir, _, mut client, _outbound_rx) = client_for(&lock_config("#101010FF"));
        assert!(run_startup(&mut client));
        client.set_session_locked(true);

        // Restyle: same surfaces and field, different colour.
        write_shell_lua(dir.path(), &lock_config("#204080FF"));
        assert!(client.reevaluate());
        client.handle_apply_pending();
        let restyled = client.scene.surface("screen@TEST").expect("the lock instance is still resolved");
        assert_eq!(
            restyled.children[0].properties.get("background").unwrap().as_string().unwrap().to_string_lossy(),
            "#204080FF",
            "restyling a live lock screen is the reason it is painted from Lua at all"
        );

        // Must refuse: same lock, no `textfield`.
        write_shell_lua(
            dir.path(),
            r##"return {
            panel { id = "bar", layer = "top" },
            lock { id = "screen", child = column { background = "#204080FF", children = {} } },
        }"##,
        );
        assert!(client.reevaluate());
        client.handle_apply_pending();

        let still_up = client.scene.surface("screen@TEST").expect("a refused apply leaves the prior scene standing");
        assert_eq!(
            still_up.children[0].children.len(),
            1,
            "the password field must still be there: a refused apply rolls the whole scene back to what was on screen"
        );
        assert_eq!(still_up.children[0].children[0].kind, "textfield");
    }

    #[test]
    fn an_unlocked_session_may_still_delete_its_lock_screens_password_field() {
        // With no lock held, the lock screen may become empty like any surface.
        let (dir, _, mut client, _outbound_rx) = client_for(&lock_config("#101010FF"));
        assert!(run_startup(&mut client));

        write_shell_lua(
            dir.path(),
            r##"return {
            panel { id = "bar", layer = "top" },
            lock { id = "screen", child = column { background = "#101010FF", children = {} } },
        }"##,
        );
        assert!(client.reevaluate());
        client.handle_apply_pending();

        assert!(
            client.scene.surface("screen@TEST").unwrap().children[0].children.is_empty(),
            "an unlocked session's lock screen is ordinary"
        );
    }

    #[test]
    fn the_lock_veto_follows_the_outputs_rather_than_the_instances_it_was_armed_with() {
        // The veto used to store `lock` instance ids from `LockCommand::Acquire`. Each hotplug's
        // `handle_output_change` replaces instances without revisiting that list; a lid closing on
        // a dock left the veto checking an unpaintable fossil while the live lock screen was open.
        // This apply mirrors `handle_output_change`: re-expand applied specs against current
        // outputs, store, resolve.
        let (dir, _, mut client, _outbound_rx) = client_for(&lock_config("#101010FF"));
        assert!(run_startup(&mut client));
        client.set_session_locked(true);

        let specs = client.applied_surface_specs();
        let docked = vec![OutputGeometry {
            name: "DP-1".to_string(),
            size: layout::LogicalSize { width: 2560.0, height: 1440.0 },
        }];
        client.set_instances(expand_instances(&specs, &docked));
        assert!(client.apply_instances(), "the freshly plugged output resolves its own lock surface");

        write_shell_lua(
            dir.path(),
            r##"return {
            panel { id = "bar", layer = "top" },
            lock { id = "screen", child = column { background = "#101010FF", children = {} } },
        }"##,
        );
        assert!(client.reevaluate());
        client.handle_apply_pending();

        let live =
            client.scene.surface("screen@DP-1").expect("the lock instance on the output that is actually plugged in");
        assert_eq!(
            live.children[0].children.len(),
            1,
            "the veto must ask what is on the glass now, not what was there when the lock was taken"
        );
    }

    #[test]
    fn a_second_lock_declaration_is_refused_at_evaluation() {
        // `expand_instances` makes one instance per lock spec/output. Two declarations make
        // `ensure_lock_surfaces` send two `get_lock_surface`s for one `wl_output`;
        // `ext-session-lock-v1` calls that `duplicate_output`, and the compositor kills the
        // connection after lock, leaving only a VT switch. Both entry points reject it: startup
        // returns no specs and `Reevaluate` fails, never staging it.
        let dir = tempfile::tempdir().unwrap();
        let two_locks = r#"return { lock { id = "first" }, lock { id = "second" } }"#;
        let path = write_shell_lua(dir.path(), two_locks);
        let (mut client, _outbound_rx) = test_client(&path);

        assert!(!run_startup(&mut client), "a config with two `lock` surfaces must not produce a generation");
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue, "the refusal has to be visible somewhere, and rescue is where an evaluation failure goes");
        assert!(error_log.contains("`lock` surfaces"), "the message must name what was refused: {error_log}");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        client.state.applied_specs = surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap();
        write_shell_lua(dir.path(), two_locks);
        assert!(
            !client.reevaluate(),
            "an edit that adds a second lock must fail the reevaluation rather than be staged"
        );
        assert!(client.state.pending.is_none());
    }

    /// Which declarations a reevaluation rebuilds, from what was last applied. `xdg-shell.xml`
    /// permits `set_title`/`set_app_id` after mapping, so a title edit must not respawn the process.
    /// ADR-0038 decision 2: layer-shell accepts `margin`, exclusive zone, `keyboard_interactivity`
    /// and size on a live surface; comparing whole specs would remap a bar just to move it 4px.
    /// `get_layer_surface` fixes the namespace at creation, so that edit needs a new layer surface.
    #[test]
    fn reevaluate_names_the_declarations_to_rebuild() {
        for (applied, edited, rebuilt, case) in [
            (
                r#"return window { id = "settings", title = "Settings" }"#,
                r#"return window { id = "settings", title = "Mantle settings", app_id = "mantle.settings" }"#,
                vec![],
                "a window's title is an in-place field",
            ),
            (
                r#"return panel { id = "bar", layer = "top", margin = { left = 4 }, keyboard_interactivity = "none", exclusive_zone = false, height = 32 }"#,
                r#"return panel { id = "bar", layer = "top", margin = { left = 40 }, keyboard_interactivity = "exclusive", exclusive_zone = true, height = 48 }"#,
                vec![],
                "margin, keyboard_interactivity, exclusive_zone and size are in-place fields",
            ),
            (
                r#"return panel { id = "bar", layer = "top" }"#,
                r#"return panel { id = "bar", layer = "top", namespace = "my-bar" }"#,
                vec!["bar"],
                "the namespace rebuilds that panel",
            ),
            (
                r#"return panel { id = "other", layer = "top" }"#,
                r#"return panel { id = "bar", layer = "top" }"#,
                vec!["bar"],
                "`bar` is new; `other` goes because no fresh instance names it",
            ),
        ] {
            let (dir, path, mut client, _outbound_rx) = client_for(applied);
            client.state.applied_specs = surface_specs(&client.loader.evaluate_file(&path).unwrap()).unwrap();

            write_shell_lua(dir.path(), edited);
            assert!(client.reevaluate(), "{case}");
            let (specs, names) = client.pending_surfaces().expect("staged");
            assert_eq!(specs.len(), 1, "{case}");
            assert_eq!(names, rebuilt, "{case}");
        }
    }

    /// `anchor_rect` feeds `xdg_positioner::set_anchor_rect`; zero size leaves it incomplete, so
    /// `get_popup` raises `invalid_positioner` and kills Wayland. Likewise `set_max_size` raises
    /// `invalid_size` when max < min. A config typo must instead be an evaluation `LayoutError` in
    /// `rescue.error_log`, naming the property.
    #[test]
    fn a_malformed_surface_fails_the_evaluation_and_names_the_property_in_rescue() {
        for (src, property) in [
            (
                r#"return {
                panel { id = "bar", layer = "top" },
                popup { id = "menu", parent = "bar", width = 200, height = 120,
                        anchor_rect = { x = 0, y = 0, width = 0, height = 0 } },
            }"#,
                "anchor_rect",
            ),
            (
                r#"return window { id = "settings", min_size = { width = 800, height = 600 }, max_size = { width = 320, height = 240 } }"#,
                "max_size",
            ),
        ] {
            let (_dir, _, mut client, _outbound_rx) = client_for(src);

            assert!(client.run_startup_evaluation().is_none(), "{property}: the whole evaluation must fail");

            let (is_rescue, error_log) = rescue_state(&client.loader);
            assert!(is_rescue, "{property}");
            assert!(error_log.contains(property), "rescue's error_log needs {property} named: {error_log}");
        }
    }

    #[test]
    fn run_startup_evaluation_on_a_missing_file_sets_rescue_and_leaves_scene_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let (mut client, _outbound_rx) = test_client(&missing);

        run_startup(&mut client);

        assert!(client.state.applied_specs.is_empty());
        assert!(client.scene.surface("bar@TEST").is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(error_log.contains("shell.lua"), "the log names the file it could not read: {error_log}");
    }

    #[test]
    fn a_successful_reevaluate_after_a_startup_failure_creates_every_surface() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let (mut client, _outbound_rx) = test_client(&missing);
        run_startup(&mut client);
        assert!(client.state.applied_specs.is_empty(), "startup must have failed (no file yet)");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        assert!(client.reevaluate(), "the first successful evaluation after a startup failure must be applied");
        assert_eq!(
            client.pending_surfaces().map(|(_, rebuilt)| rebuilt),
            Some(vec!["bar".to_string()]),
            "nothing was applied, so every declaration gets its surfaces"
        );
    }

    #[test]
    fn a_removed_declaration_applies_in_place_once_its_instance_is_gone_too() {
        let (dir, _, mut client, _outbound_rx) =
            client_for(r#"return { panel { id = "bar", layer = "top" }, panel { id = "dock", layer = "top" } }"#);
        assert!(run_startup(&mut client));

        write_shell_lua(dir.path(), r#"return { panel { id = "bar", layer = "top" } }"#);
        assert!(client.reevaluate());
        let (_, rebuilt) = client.pending_surfaces().expect("staged");
        assert!(rebuilt.is_empty(), "removing `dock` rebuilds nothing; reconcile drops its instance");

        // What `App::apply_pending` does: the scene resolves against the fresh instance set.
        client.set_instances(instances_for(&["bar"]));
        assert!(client.handle_apply_pending());
        assert!(client.scene.surface("bar@TEST").is_some());
    }

    #[test]
    fn a_measured_axis_keeps_its_ceiling_when_the_compositor_configures_the_size_it_asked_for() {
        // The conflation this guards against: `available` is the box a tree is solved against, and
        // on a measured axis that box is a *ceiling*, not an allocation. Writing the granted size
        // back would make the measurement its own cap.
        //
        // It only bites where natural size depends on the bound, which is exactly what wrapping
        // does. Measured against 1000 this paragraph is one line wide; against 180 it is 180 wide
        // and two lines tall. Grant it the 342 it asked for, feed that back as the ceiling, and it
        // can never grow wider again -- it wraps taller inside the width it happened to open at.
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", width = "fill", height = "fill" }"#);
        assert!(run_startup(&mut client), "startup must have applied");

        let ceiling = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap().available;
        client.set_measured_axes("bar@TEST", (true, false), ceiling);
        client.dirty.take();

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 342.0, height: 32.0 });

        let after = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap().available;
        assert_eq!(after.width, ceiling.width, "the measured axis keeps the ceiling it was seeded with");
        assert_eq!(after.height, 32.0, "the allocated axis takes the configured size, as before");
    }

    #[test]
    fn an_axis_that_becomes_measured_is_given_a_ceiling_instead_of_keeping_its_last_allocation() {
        // A reload may drop `width = 200` and leave the axis measured. Nothing else can set
        // `available` there afterwards -- `set_instance_size` declines a measured axis outright --
        // so without this the content would be solved against 200 for the rest of the generation
        // and no configure could ever widen it.
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", width = "fill", height = "fill" }"#);
        assert!(run_startup(&mut client), "startup must have applied");
        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 200.0, height: 39.0 });
        client.dirty.take();

        let ceiling = layout::LogicalSize { width: 1920.0, height: 1080.0 };
        client.set_measured_axes("bar@TEST", (true, false), ceiling);

        let instance = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap();
        assert_eq!(instance.measured_axes, (true, false));
        assert_eq!(instance.available.width, 1920.0, "the newly measured axis takes the ceiling it may grow into");
        assert_eq!(instance.available.height, 39.0, "the allocated axis keeps what the compositor configured");
        assert!(client.dirty.take(), "the tree must be solved again against the room it actually has");
    }

    #[test]
    fn recording_the_same_measurement_again_changes_nothing_and_names_no_instance_it_lacks() {
        // It runs on every resolved pass, so a pass that re-derived the same panel spec must not
        // cost a re-resolve of every surface in the scene.
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", width = "fill", height = "fill" }"#);
        assert!(run_startup(&mut client), "startup must have applied");
        assert!(!client.dirty.take(), "a clean startup leaves the flag clear");
        let ceiling = client.instances.iter().find(|i| i.instance_id == "bar@TEST").unwrap().available;

        client.set_measured_axes("bar@TEST", (false, true), ceiling);
        assert!(!client.dirty.take(), "the ceiling it already had is not a change to any tree");

        client.set_measured_axes("no-such-surface@TEST", (true, true), ceiling);
        assert!(!client.dirty.take(), "an unknown id is ignored, the way an unknown configure is");
    }

    #[test]
    fn set_instance_size_replaces_one_instances_available_size_and_marks_the_scene_dirty() {
        // `configure` reuses ADR-0044 decision 2's one dirty flag, not a second change mechanism.
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", width = "fill", height = "fill" }"#);
        assert!(run_startup(&mut client), "startup must have applied");
        assert!(!client.dirty.take(), "a clean startup leaves the flag clear");
        assert_eq!(
            client.scene.surface("bar@TEST").unwrap().rect.height,
            1080.0,
            "the first resolve uses the output's own size"
        );

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 1920.0, height: 32.0 });

        assert!(client.re_resolve_if_dirty(), "a new configured size must mark the scene dirty and re-resolve");
        assert_eq!(
            client.scene.surface("bar@TEST").unwrap().rect.height,
            32.0,
            "the surface must now resolve against the size the compositor actually configured, not the whole output"
        );
    }

    #[test]
    fn a_configure_for_one_surface_re_resolves_only_that_instance() {
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r#"return {
                panel { id = "bar", layer = "top", width = "fill", height = "fill" },
                panel { id = "b", layer = "top" },
                panel { id = "c", layer = "top" },
                panel { id = "d", layer = "top" },
            }"#,
        );
        assert!(run_startup(&mut client));
        client.take_last_resolved();

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 1920.0, height: 32.0 });
        assert!(client.re_resolve_if_dirty());

        assert_eq!(client.take_last_resolved(), Some(vec!["bar@TEST".to_string()]), "None means all four instances");
    }

    #[test]
    fn set_instance_size_repeating_the_current_size_does_not_mark_the_scene_dirty() {
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", width = "fill", height = "fill" }"#);
        assert!(run_startup(&mut client));

        client.set_instance_size("bar@TEST", layout::LogicalSize { width: 1920.0, height: 1080.0 });
        assert!(
            !client.dirty.take(),
            "a duplicate configure carrying the size already resolved against changes nothing"
        );

        client.set_instance_size("no-such-surface@TEST", layout::LogicalSize { width: 10.0, height: 10.0 });
        assert!(!client.dirty.take(), "a configure for a surface no instance names must not dirty the whole scene");
    }

    #[test]
    fn one_surface_on_two_outputs_resolves_two_trees_against_two_different_sizes() {
        // `output = "all"` across laptop and 4K external means two configured sizes; one declared
        // surface tree cannot serve both.
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", width = "fill", height = "fill" }"#);

        let specs = client.run_startup_evaluation().expect("the fixture evaluates");
        let outputs = vec![
            OutputGeometry { name: "eDP-1".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } },
            OutputGeometry { name: "DP-1".to_string(), size: layout::LogicalSize { width: 3840.0, height: 2160.0 } },
        ];
        client.set_instances(expand_instances(&specs, &outputs));
        assert!(client.apply_instances());

        assert_eq!(client.scene.surface("bar@eDP-1").unwrap().rect.width, 1920.0);
        assert_eq!(client.scene.surface("bar@DP-1").unwrap().rect.width, 3840.0);
    }

    #[test]
    fn a_panel_naming_an_unplugged_monitor_gets_no_instance_and_no_resolved_tree() {
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", output = "HDMI-A-9" }"#);

        assert!(
            run_startup(&mut client),
            "an unmatched monitor is not an apply failure -- there is simply nothing to resolve"
        );
        assert!(client.scene.surface("bar@TEST").is_none());
        assert!(client.instances.is_empty());
    }

    #[test]
    fn a_failed_reevaluation_stages_nothing_and_sets_rescue() {
        let (_dir, _, mut client, _outbound_rx) = client_for("this is not lua");

        assert!(!client.reevaluate());
        assert!(client.state.pending.is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(error_log.contains("syntax error"), "the log carries the Lua error: {error_log}");
    }

    #[test]
    fn a_topology_field_error_reads_distinctly_from_a_top_level_return_error() {
        // A topology-field type error such as `anchor.top` not boolean used to become
        // `InvalidTopLevelReturn`'s fixed "must be a `panel` node or an array of them" message.
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", anchor = { top = "yes" } }"#);

        assert!(!client.reevaluate());
        let (_, error) = rescue_state(&client.loader);
        assert!(error.contains("panel: invalid value for `anchor`"), "expected a field-specific message, got: {error}");
        assert!(
            !error.contains("top-level return"),
            "must not reuse the unrelated top-level-return message, got: {error}"
        );
    }

    #[test]
    fn a_push_arriving_before_the_first_successful_apply_is_not_consumed_and_lost() {
        let (mut client, _outbound_rx) = bare_client();
        run_startup(&mut client);
        assert!(client.state.applied_output.is_none(), "startup must have failed (no file)");
        push(&client, "audio", 1, serde_json::json!(1));
        client.re_resolve_if_dirty();
        assert!(client.dirty.take(), "the push stays pending for whatever applies next");
    }

    #[test]
    fn a_burst_of_pushes_before_one_check_marks_the_flag_only_once() {
        // ADR-0044 decision 2: "drain first, then re-resolve once"; several pushes before one read
        // coalesce into one dirty read.
        let (client, _outbound_rx) = bare_client();
        assert!(!client.dirty.take(), "a fresh client must not start dirty");

        for revision in 1..=5 {
            push(&client, "audio", revision, serde_json::json!({ "n": revision }));
        }

        assert!(client.dirty.take(), "a burst of five pushes must have marked the flag");
        assert!(
            !client.dirty.take(),
            "the flag records only whether a push happened since the last check, not how many, so the burst coalesces into one turn's work"
        );
    }

    #[test]
    fn a_push_that_makes_a_property_invalid_drops_it_in_rescue_until_it_is_fixed() {
        let (_dir, _, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", visible = mantle.workspace }"#);
        push(&client, "workspace", 1, serde_json::json!(false));
        run_startup(&mut client);
        assert_eq!(rescue_state(&client.loader), (false, String::new()));

        // `visible` requires a boolean; a table is dropped for its default, `true`.
        push(&client, "workspace", 2, serde_json::json!({ "not": "a boolean" }));
        assert!(client.re_resolve_if_dirty());
        assert!(client.scene.surface("bar@TEST").unwrap().visible, "the default, not the prior tree");
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue && error_log.contains("visible"), "a dropped value must reach rescue: {error_log}");

        push(&client, "workspace", 3, serde_json::json!(false));
        assert!(client.re_resolve_if_dirty());
        assert!(!client.scene.surface("bar@TEST").unwrap().visible);
        assert_eq!(rescue_state(&client.loader), (false, String::new()), "the fixed value clears it");
    }

    #[test]
    fn a_config_binding_a_bare_rostered_signal_applies_at_startup_with_no_push_at_all() {
        // ADR-0044 decision 1: startup runs before an inbound frame drains, so rostered
        // capabilities read `nil`; bare bindings still apply using each parser's absent-property
        // default.
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r#"return panel { id = "bar", layer = "top", visible = mantle.audio, child = row { children = { rect { width = mantle.network, height = 10, children = mantle.tray }, text { content = mantle.audio } } } }"#,
        );

        run_startup(&mut client);

        let bar = client.scene.surface("bar@TEST").expect("a bare rostered signal must not stop the config applying");
        assert!(bar.visible, "`visible = audio` with audio still nil must take `visible`'s default");
        assert!(
            bar.children[0].children[0].children.is_empty(),
            "`children = tray` with tray still nil must take `children`'s default"
        );
        // ADR-0044 example: `text { content = mantle.mpris.title }` applies at boot with `title` still nil.
        assert_eq!(
            layout::node::fields::text::content.read(&bar.children[0].children[1].properties).unwrap().0,
            "",
            "`content = audio` with audio still nil must take `content`'s default"
        );
        assert_eq!(
            rescue_state(&client.loader),
            (false, String::new()),
            "a startup that applies must not be in rescue"
        );
        // `set_rescue_state` writes the shared `DirtyFlag`; startup used to dirty it while clearing an
        // already-clear rescue, costing a redundant first-poll `Scene::apply`.
        assert!(!client.dirty.take(), "an apply that succeeded resolved every signal at its current value");
    }

    /// `screens_payload` shape, hand-written so tests need no live compositor via `crate::wayland`.
    fn screens_json(names: &[&str]) -> serde_json::Value {
        serde_json::Value::Array(
            names
                .iter()
                .map(|name| serde_json::json!({ "name": name, "width": 1920, "height": 1080, "scale": 1, "refresh": 60.0 }))
                .collect(),
        )
    }

    #[test]
    fn a_config_looping_over_screens_declares_one_panel_per_connected_output() {
        // ADR-0041 decision 1: no `variants`; Lua already has `for`. Without the pre-evaluation
        // seed, this loop would run zero times.
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r#"
            local panels = {}
            for _, screen in ipairs(mantle.screens:get()) do
                panels[#panels + 1] = panel { id = "bar@" .. screen.name, layer = "top", output = screen.name }
            end
            return panels
            "#,
        );

        assert!(client.set_screens(screens_json(&["eDP-1", "DP-1"])));
        let specs = client.run_startup_evaluation().expect("the config must evaluate");

        assert_eq!(specs.iter().map(SurfaceSpec::declared_id).collect::<Vec<_>>(), ["bar@eDP-1", "bar@DP-1"]);
        assert!(matches!(&specs[1], SurfaceSpec::Panel(panel) if panel.topology.output == "DP-1"));
    }

    #[test]
    fn set_screens_repeating_the_same_list_reports_no_change_and_leaves_the_scene_clean() {
        // `update_output` handles changes absent from `screens`; an unchanged push buys neither
        // `Scene::apply` nor a re-evaluation.
        let (_dir, _, mut client, _outbound_rx) = client_for(r#"return panel { id = "bar", layer = "top" }"#);
        client.set_screens(screens_json(&["eDP-1"]));
        run_startup(&mut client);

        assert!(!client.set_screens(screens_json(&["eDP-1"])), "the same list is not an output change");
        assert!(!client.dirty.take(), "and must not mark the scene dirty");
    }

    #[test]
    fn a_real_output_change_marks_the_scene_dirty_so_a_config_reading_screens_re_resolves() {
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r#"return panel { id = "bar", layer = "top", child = text { content = computed({mantle.screens}, function(list) return "n=" .. #list end) } }"#,
        );
        client.set_screens(screens_json(&["eDP-1"]));
        run_startup(&mut client);
        let content = |client: &RendererClient| {
            client.scene.surface("bar@TEST").unwrap().children[0]
                .properties
                .get("content")
                .unwrap()
                .as_string()
                .unwrap()
                .to_string_lossy()
        };
        assert_eq!(content(&client), "n=1");

        assert!(client.set_screens(screens_json(&["eDP-1", "DP-1"])), "a monitor appearing is an output change");
        assert!(client.re_resolve_if_dirty(), "and the scene must re-resolve against it without re-reading shell.lua");
        assert_eq!(content(&client), "n=2");
    }

    /// ADR-0115 on a Renderer-sourced member: `screens` runs the handler with both lists, and a
    /// re-evaluation drops it like a capability's.
    #[test]
    fn screens_on_change_sees_both_lists_and_a_re_evaluation_drops_it() {
        let (_dir, path, mut client, _outbound_rx) = client_for(
            r#"
            mantle.screens:on_change(function(now, before) seen = #before .. "->" .. #now end)
            return panel { id = "bar", layer = "top" }
            "#,
        );
        client.set_screens(screens_json(&["eDP-1"]));
        run_startup(&mut client);

        client.set_screens(screens_json(&["eDP-1", "DP-1"]));
        assert_eq!(client.loader.lua().globals().get::<String>("seen").unwrap(), "1->2");

        std::fs::write(&path, r#"return panel { id = "bar", layer = "top" }"#).unwrap();
        assert!(client.reevaluate());
        client.set_screens(screens_json(&["eDP-1"]));
        assert_eq!(client.loader.lua().globals().get::<String>("seen").unwrap(), "1->2", "the dropped handler ran");
    }

    /// ADR-0288: a `mantle set` runs the state's handler before the pass that paints it, and a
    /// re-evaluation, applied or failed, drops what the last one registered.
    #[test]
    fn a_state_write_over_the_socket_runs_on_change_before_the_pass_and_a_re_evaluation_drops_it() {
        let (_dir, path, mut client, _outbound_rx) = client_for(
            r#"
            local open = state("open", false)
            local hint = state("hint", "idle")
            open:on_change(function(now) hint:set(now and "open" or "closed") end)
            return panel { id = "bar", layer = "top", child = text { content = hint } }
            "#,
        );
        assert!(run_startup(&mut client));
        let content = |client: &RendererClient| {
            client.scene.surface("bar@TEST").unwrap().children[0]
                .properties
                .get("content")
                .unwrap()
                .as_string()
                .unwrap()
                .to_string_lossy()
        };
        let toggle = |client: &mut RendererClient| {
            let set = shared::SetState { name: "open".into(), write: shared::StateWrite::Toggle };
            assert_eq!(client.handle_frame(SupervisorFrame::SetState { id: 1, set }), FrameOutcome::Handled);
        };

        toggle(&mut client);
        assert!(client.re_resolve_if_dirty());
        assert_eq!(content(&client), "open", "the handler's write lands in the same pass");

        std::fs::write(&path, r#"error("half a config") state("open", false):on_change(function() end)"#).unwrap();
        assert!(!client.reevaluate());
        toggle(&mut client);
        client.re_resolve_if_dirty();
        assert_eq!(content(&client), "open", "neither the old handler nor the failed one ran");
    }

    #[test]
    fn applied_surface_specs_returns_the_applied_declarations_without_reading_shell_lua_again_and_none_before_any_apply()
     {
        // Hotplug expansion source (ADR-0038 decision 3). Delete the file to prove it is untouched.
        let (_dir, path, mut client, _outbound_rx) =
            client_for(r#"return panel { id = "bar", layer = "top", output = "all" }"#);
        run_startup(&mut client);
        std::fs::remove_file(&path).unwrap();

        let specs = client.applied_surface_specs();
        assert_eq!(specs.len(), 1);
        assert!(
            matches!(&specs[0], SurfaceSpec::Panel(panel) if panel.topology.id == "bar" && panel.topology.output == "all")
        );

        // Failed startup declared nothing, so hotplug adds no instance.
        let (client, _outbound_rx) = bare_client();
        assert!(client.applied_surface_specs().is_empty());
    }

    /// A reload kills what the last evaluation started and runs each `on_exit(nil)` once, before the
    /// new top level; the Supervisor's output and exit frames still in flight reach no callback.
    #[test]
    fn a_reload_kills_the_last_evaluations_process_run_children() {
        use crate::lua::capability::tests::queued_command;
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"lines, exits = lines or 0, exits or 0
            exits_at_top = exits
            process.run("tail", { "-f", "log" }, function() lines = lines + 1 end,
                function(code) exits, last_code = exits + 1, code end)
            return panel { id = "bar", layer = "top" }"#,
        );
        let (mut client, mut rx) = test_client(&path);
        assert!(run_startup(&mut client));
        let run = queued_command(&mut rx).expect("the top level ran a child");
        let output = |id| {
            SupervisorFrame::ProcessOutput(ProcessOutputLine {
                id,
                stream: shared::ProcessStream::Stdout,
                line: "x".into(),
            })
        };
        let _ = client.handle_frame(output(run.id));

        assert_eq!(client.handle_frame(SupervisorFrame::Reevaluate), FrameOutcome::ApplyPending);
        let kill = queued_command(&mut rx).expect("the reload killed the child");
        assert_eq!((kill.id, kill.params.action.as_str()), (run.id, "kill"));
        assert_eq!(queued_command(&mut rx).expect("the new evaluation ran its own").params.action, "run");
        let _ = client.handle_frame(output(run.id));
        let _ = client.handle_frame(SupervisorFrame::ProcessExited(ProcessExited { id: run.id, code: Some(0) }));

        let globals = client.lua().globals();
        assert_eq!(globals.get::<i64>("lines").unwrap(), 1, "no on_line after the kill");
        assert_eq!(globals.get::<i64>("exits").unwrap(), 1, "on_exit fires once");
        assert_eq!(globals.get::<i64>("exits_at_top").unwrap(), 1, "and before the new top level");
        assert!(globals.get::<mlua::Value>("last_code").unwrap().is_nil(), "with nil");
    }

    #[test]
    fn a_due_timer_fires_through_the_client() {
        let (_dir, _, mut client, _outbound_rx) = client_for(
            r#"fired = false
            timer(1, function() fired = true end)
            return panel { id = "bar", layer = "top" }"#,
        );
        assert!(run_startup(&mut client));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !client.loader.lua().globals().get::<bool>("fired").unwrap() {
            assert!(std::time::Instant::now() < deadline, "the timer never fired");
            client.fire_due_timers();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A reload forgets folded raises, so the failing handler of the new config logs in full again.
    #[test]
    fn a_reload_logs_a_repeating_raise_in_full_again() {
        const SHELL: &str = r#"
            timer(1, function() error("boom") end)
            return panel { id = "bar", layer = "top" }
        "#;
        let (_dir, path, mut client, _outbound_rx) = client_for(SHELL);
        assert!(run_startup(&mut client));
        let fire = |client: &RendererClient| {
            let day = std::time::Instant::now() + std::time::Duration::from_secs(86_400);
            crate::lua::timer::dispatch_due(client.loader.lua(), day);
            crate::lua::location::LOGGED.with_borrow(|lines| lines.last().cloned().unwrap_or_default())
        };

        assert!(fire(&client).contains("defined at"));
        assert!(reload(&mut client, &path, SHELL));
        let logged = fire(&client);
        assert!(logged.contains("defined at"), "the reload forgot the fold: {logged}");
    }
}

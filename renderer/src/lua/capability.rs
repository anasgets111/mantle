//! Capability and [`CapabilityHandle`] share the live signal, revision, and write path
//! (ADR-0052 decision 1). [`CommandSender::send`] queues `{capability, action, arguments}` as a
//! `RendererFrame::Command` on the channel drained by the socket thread's `pump` (ADR-0039), since
//! Lua runs on the Wayland dispatch thread and has no socket in scope. `Rc`, not `Arc`, is correct
//! because this state stays on that thread.
//!
//! One userdata owns both halves. Each action is a method its `__index` resolves from
//! `shared::Capability::actions` (ADR-0264), and ADR-0052 decision 4 reads lock state through the
//! name it locks, so `mantle.lock:get().attempts` and `mantle.lock:lock()` use the same object;
//! [`Capability`] delegates `get`/`map` to its [`Signal`].

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, MetaMethod, MultiValue, UserData, UserDataMethods, Value};
use shared::{CommandEnvelope, CommandParams, RendererFrame, error, warn};
use tokio::sync::mpsc::UnboundedSender;

use crate::lua::fuzzy::closest;
use crate::lua::signal::{CpuBudget, DirtyFlag, LiveSignalHandle, Signal};

/// Builds the generation-guarded envelope and queues it for the socket thread. One sender per
/// generation is cloned into every [`Capability`] on `mantle`.
#[derive(Clone)]
pub struct CommandSender {
    generation_id: u32,
    /// `CommandEnvelope.id` (e.g. `"id": 105`), shared across clones so capabilities never reuse
    /// an id. `Rc<Cell<_>>` is safe here because the sender is single-threaded.
    next_id: Rc<Cell<u64>>,
    /// Capabilities this generation already asked the Supervisor to start. Shared across clones;
    /// `lua::namespace` and `secure_submit`'s sweep both use it (ADR-0070 decisions 1, 5), so a
    /// second `mantle.audio` reader costs nothing.
    started: Rc<RefCell<HashSet<String>>>,
    outbound_tx: UnboundedSender<RendererFrame>,
}

impl CommandSender {
    /// `generation_id` comes from `socket::generation_id_from_env`.
    pub fn new(generation_id: u32, outbound_tx: UnboundedSender<RendererFrame>) -> Self {
        CommandSender {
            generation_id,
            next_id: Rc::new(Cell::new(0)),
            started: Rc::new(RefCell::new(HashSet::new())),
            outbound_tx,
        }
    }

    /// Asks the Supervisor to construct `capability`'s controller once per generation. The
    /// Supervisor drops repeats (ADR-0070 decision 3); the local set also stops a `map` over
    /// `mantle.audio` from writing a frame on every layout pass.
    pub(crate) fn start_capability(&self, capability: &str) {
        if self.started.borrow().contains(capability) {
            return;
        }
        // Every caller passes a roster name: `mantle`'s index, `idle`, and `secure_submit`, whose
        // parser admits only its three targets.
        let known = shared::Capability::from_name(capability).expect("a roster name");
        self.started.borrow_mut().insert(capability.to_string());
        let frame = RendererFrame::StartCapability { capability: known };
        if self.outbound_tx.send(frame).is_err() {
            error!("mantle.{known}: failed to queue the start request, the control-socket writer is gone");
        }
    }

    /// The channel where `RendererClient` queues non-command frames (`CallResult`, `LockReport`)
    /// alongside commands.
    pub fn frames(&self) -> UnboundedSender<RendererFrame> {
        self.outbound_tx.clone()
    }

    /// Queues the command envelope. `expected_revision` is the last hydrated `StateSnapshot`
    /// revision, kept current by [`CapabilityHandle`]. `0` means "never hydrated": `bump_revision`
    /// starts at `1`, and state-less `lock` capabilities send it forever (ADR-0052 decision 1,
    /// `process.rs`).
    pub(crate) fn send(
        &self,
        capability: &str,
        action: &str,
        arguments: Vec<serde_json::Value>,
        expected_revision: u32,
    ) {
        self.send_as(self.next_id(), capability, action, arguments, expected_revision);
    }

    /// A fresh JSON-RPC request id, for a caller that names a later command by it (`process`).
    pub(crate) fn next_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    /// [`Self::send`] under an id the caller already holds.
    pub(crate) fn send_as(
        &self,
        id: u64,
        capability: &str,
        action: &str,
        arguments: Vec<serde_json::Value>,
        expected_revision: u32,
    ) {
        let envelope = CommandEnvelope {
            params: CommandParams {
                generation_id: self.generation_id,
                capability: capability.to_string(),
                action: action.to_string(),
                arguments,
                expected_revision,
            },
            id,
        };
        if self.outbound_tx.send(RendererFrame::Command(envelope)).is_err() {
            error!("mantle.{capability}:{action}: failed to queue the command, the control-socket writer is gone");
        }
    }
}

/// One `mantle` member: its live signal and write path. `name` is the
/// `shared::Capability::ALL` roster name, the Lua field and every envelope's `capability` value.
///
/// Only `lua::idle` clones it, wrapping `mantle.idle` so three threshold methods share userdata
/// with `get`/`map`/`on_change` (ADR-0141). The signal and handler list remain shared.
#[derive(Clone)]
pub struct Capability {
    name: String,
    signal: Signal,
    /// Shared with the paired [`CapabilityHandle`], so an action stamps the last snapshot revision
    /// the config could have read; see [`CommandSender::send`].
    revision: Rc<Cell<u32>>,
    commands: CommandSender,
    /// `on_change` handlers run by [`CapabilityHandle::notify_change`] after each push (ADR-0115).
    handlers: Rc<RefCell<Vec<Function>>>,
}

impl Capability {
    /// Builds an `mantle.<name>` member and the handle `socket::RendererClient` hydrates. Return
    /// them together: value and revision must move as one, because ordinary dispatch does not
    /// enforce envelope revision claims. Pairing them here is the only guard against a `set` that
    /// stamps a stale read onto the current write.
    pub fn new(name: &str, dirty: DirtyFlag, commands: CommandSender) -> (Self, CapabilityHandle) {
        // `nil` until the Supervisor's first push (ADR-0037), paired with revision `0`, which no
        // push can produce.
        Self::seeded(name, Value::Nil, dirty, commands)
    }

    /// A member holding `initial` before its first write, for the Renderer-sourced `screens` and
    /// `rescue`. Off the roster, they have no actions.
    pub fn seeded(name: &str, initial: Value, dirty: DirtyFlag, commands: CommandSender) -> (Self, CapabilityHandle) {
        let (signal, signal_handle) = Signal::new_live(initial, dirty);
        let revision = Rc::new(Cell::new(0));
        let handlers = Rc::new(RefCell::new(Vec::new()));
        let capability = Capability {
            name: name.to_string(),
            signal,
            revision: Rc::clone(&revision),
            commands,
            handlers: Rc::clone(&handlers),
        };
        (capability, CapabilityHandle { name: name.to_string(), signal: signal_handle, revision, handlers })
    }

    /// Registers an `on_change` handler, including for wrappers such as `lua::idle` that re-export
    /// this capability's read half.
    pub fn add_handler(&self, handler: Function) {
        self.handlers.borrow_mut().push(handler);
    }

    /// Roster name for a wrapper that must send the `start_capability` request hidden by
    /// `mantle`'s `__index` path.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Command sender, so a wrapper can announce a read hidden by `__index`.
    pub fn commands(&self) -> &CommandSender {
        &self.commands
    }

    /// Wrapped read signal for `signal::from_userdata`, allowing live forms (`content =
    /// mantle.mpris`, `computed({mantle.audio}, f)`) through a wrapper the engine otherwise
    /// cannot see past.
    pub fn signal(&self) -> Signal {
        self.signal.clone()
    }
}

/// Rust-side half of an `mantle.<name>` member, where `StateSnapshot` writes. One handle carries
/// the `LiveSignalHandle` and revision because `socket::RendererClient` holds one per capability.
#[derive(Clone)]
pub struct CapabilityHandle {
    name: String,
    signal: LiveSignalHandle,
    revision: Rc<Cell<u32>>,
    handlers: Rc<RefCell<Vec<Function>>>,
}

impl CapabilityHandle {
    /// Writes a `StateSnapshot` revision before its value. The value write marks the scene dirty
    /// (ADR-0044 decision 2), so it must go last. Returns the replaced value for
    /// [`Self::notify_change`].
    pub fn hydrate(&self, value: Value, revision: u32) -> Value {
        self.revision.set(revision);
        let previous = if self.handlers.borrow().is_empty() { Value::Nil } else { self.signal.get() };
        self.signal.set(value);
        previous
    }

    /// Runs each `on_change` handler with `(current, previous)` (ADR-0115), under its own 5ms CPU
    /// budget, the same budget as `map`. A raising handler is logged and skipped after the push;
    /// handlers run from a copied list, so one can register another without borrowing the
    /// `RefCell` recursively.
    pub fn notify_change(&self, lua: &Lua, previous: Value) {
        let handlers = self.handlers.borrow().clone();
        if handlers.is_empty() {
            return;
        }
        let current = self.signal.get();
        for handler in handlers {
            let outcome = CpuBudget::enter(lua).and_then(|budget| {
                handler.call::<()>((current.clone(), previous.clone()))?;
                budget.check_not_exceeded()
            });
            if let Err(err) = outcome {
                warn!("mantle.{}:on_change handler raised, ignoring it: {err}", self.name);
            }
        }
    }

    /// Clears handlers before the client re-evaluates `shell.lua`; re-registering without this
    /// would double every reload side effect.
    pub fn clear_handlers(&self) {
        self.handlers.borrow_mut().clear();
    }
}

impl UserData for Capability {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Delegate `get`/`map` so capabilities read like bare `Signal` globals.
        methods.add_method("get", |lua, this, ()| this.signal.get_value(lua));
        methods.add_function("map", |lua, (ud, f): (mlua::AnyUserData, Function)| Signal::mapped(lua, ud, f));
        // The one non-rendering push reaction (ADR-0115): once per `StateSnapshot`, outside layout,
        // with new and old payloads, and input-callback powers (actions, `process.run`, state).
        methods.add_method("on_change", |_, this, f: Function| {
            this.handlers.borrow_mut().push(f);
            Ok(())
        });
        // Each action is a method (ADR-0264). mlua looks up `get`/`map`/`on_change` before
        // `__index`; `every_roster_action_is_a_method_no_builtin_shadows` keeps names clear of them.
        // An unknown key raises rather than reading `nil`, so `mantle.audio.volume` names the fix.
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            let roster = shared::Capability::from_name(&this.name);
            let actions = roster.map_or(&[][..], shared::Capability::actions);
            let Some(&action) = actions.iter().find(|action| **action == key) else {
                let name = &this.name;
                let fields = roster.map_or(&[][..], shared::Capability::state_fields);
                let hint = match closest(&key, actions.iter().chain(fields).copied()) {
                    Some(near) if actions.contains(&near) => format!("did you mean mantle.{name}:{near}(...)?"),
                    Some(near) => format!("did you mean mantle.{name}:get().{near}?"),
                    None if actions.is_empty() => {
                        format!("it is read-only, with no actions; read it with mantle.{name}:get()")
                    }
                    None => format!("its actions are {}; :get() reads its state", actions.join(", ")),
                };
                return Err(mlua::Error::runtime(format!("mantle.{name} has no `{key}`: {hint}")));
            };
            let capability = this.clone();
            lua.create_function(move |lua, (receiver, args): (Value, MultiValue)| {
                capability.send_action(lua, &receiver, action, args)
            })
        });
    }
}

impl Capability {
    /// Marshals `args` and queues `action`. State itself has no `set` (ADR-0044 decision 5).
    fn send_action(&self, lua: &Lua, receiver: &Value, action: &str, args: MultiValue) -> mlua::Result<()> {
        // A dot call would send its first argument as the receiver's slot.
        let bound =
            matches!(receiver, Value::UserData(ud) if ud.borrow::<Capability>().is_ok_and(|c| c.name == self.name));
        if !bound {
            return Err(mlua::Error::runtime(format!(
                "mantle.{0}.{action} needs a colon: mantle.{0}:{action}(...)",
                self.name
            )));
        }
        let mut arguments = Vec::with_capacity(args.len());
        for (index, value) in args.into_iter().enumerate() {
            // Names only here: the Supervisor's serde enums own argument checks
            // (`supervisor/src/action.rs`). Rejecting an unmarshallable slot names argument 3
            // instead of sending a malformed command.
            let json = lua.from_value::<serde_json::Value>(value).map_err(|err| {
                mlua::Error::runtime(format!(
                    "mantle.{}:{action} could not marshal argument {}: {err}",
                    self.name,
                    index + 1
                ))
            })?;
            arguments.push(json);
        }
        self.commands.send(&self.name, action, arguments, self.revision.get());
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use mlua::{Lua, Value};
    use tokio::sync::mpsc;

    use super::*;

    /// Test VM with `audio` bound at `mantle.probe`; its methods are the roster name's actions.
    fn lua_with_capability(generation_id: u32) -> (Lua, CapabilityHandle, mpsc::UnboundedReceiver<RendererFrame>) {
        let lua = Lua::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let (capability, handle) = Capability::new("audio", DirtyFlag::new(), CommandSender::new(generation_id, tx));
        let table = lua.create_table().unwrap();
        table.set("probe", capability).unwrap();
        lua.globals().set("mantle", table).unwrap();
        (lua, handle, rx)
    }

    /// The next queued command, skipping the capability starts that precede it.
    pub(crate) fn queued_command(rx: &mut mpsc::UnboundedReceiver<RendererFrame>) -> Option<CommandEnvelope> {
        loop {
            match rx.try_recv().ok()? {
                RendererFrame::Command(envelope) => return Some(envelope),
                RendererFrame::StartCapability { .. } => continue,
                other => panic!("a command must be queued as RendererFrame::Command, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_action_method_queues_the_generation_guarded_envelope() {
        let (lua, _handle, mut rx) = lua_with_capability(4);

        lua.load(r#"mantle.probe:set_volume(0.75)"#).exec().unwrap();

        let envelope = queued_command(&mut rx).expect("an action must queue a command");
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "audio");
        assert_eq!(envelope.params.action, "set_volume");
        assert_eq!(envelope.params.arguments, vec![serde_json::json!(0.75)]);
        // No snapshot is hydrated; `bump_revision` starts at 1, so `0` is correct.
        assert_eq!(envelope.params.expected_revision, 0);
    }

    #[test]
    fn an_action_stamps_the_revision_of_the_snapshot_the_config_could_last_have_read() {
        let (lua, handle, mut rx) = lua_with_capability(0);

        handle.hydrate(Value::Nil, 7);
        lua.load(r#"mantle.probe:set_volume(0.5)"#).exec().unwrap();
        assert_eq!(queued_command(&mut rx).unwrap().params.expected_revision, 7);

        handle.hydrate(Value::Nil, 8);
        lua.load(r#"mantle.probe:set_volume(0.6)"#).exec().unwrap();
        assert_eq!(queued_command(&mut rx).unwrap().params.expected_revision, 8);
    }

    #[test]
    fn an_action_with_no_arguments_sends_an_empty_array_not_a_missing_field() {
        // `null` would not deserialize into `Vec<serde_json::Value>`.
        let (lua, _handle, mut rx) = lua_with_capability(0);

        lua.load(r#"mantle.probe:toggle_mute()"#).exec().unwrap();

        assert_eq!(queued_command(&mut rx).unwrap().params.arguments, Vec::<serde_json::Value>::new());
    }

    #[test]
    fn each_action_gets_a_distinct_json_rpc_id() {
        let (lua, _handle, mut rx) = lua_with_capability(0);

        lua.load(r#"mantle.probe:toggle_mute(); mantle.probe:toggle_mute()"#).exec().unwrap();

        assert_eq!(queued_command(&mut rx).unwrap().id, 0);
        assert_eq!(queued_command(&mut rx).unwrap().id, 1);
    }

    #[test]
    fn an_unmarshallable_argument_is_a_config_error_naming_its_slot_and_queues_nothing() {
        let (lua, _handle, mut rx) = lua_with_capability(0);

        let err = lua.load(r#"mantle.probe:set_app_volume("firefox", function() end)"#).exec().unwrap_err();

        assert!(err.to_string().contains("argument 2"), "the error must name the offending slot: {err}");
        assert!(rx.try_recv().is_err(), "a refused argument must not queue a half-built command");
    }

    #[test]
    fn an_unknown_or_read_only_action_is_a_config_error_and_queues_nothing() {
        let (lua, _handle, mut rx) = lua_with_capability(0);
        let err = lua.load(r#"mantle.probe:set_volumee(1)"#).exec().unwrap_err();
        assert!(err.to_string().contains("did you mean mantle.audio:set_volume(...)?"), "{err}");
        let err = lua.load(r#"mantle.probe:rename()"#).exec().unwrap_err();
        assert!(err.to_string().contains("its actions are set_volume,"), "{err}");

        let (tx, _rx) = mpsc::unbounded_channel();
        let (battery, _) = Capability::new("battery", DirtyFlag::new(), CommandSender::new(0, tx));
        lua.globals().get::<mlua::Table>("mantle").unwrap().set("battery", battery).unwrap();
        let err = lua.load(r#"mantle.battery:refresh()"#).exec().unwrap_err();
        assert!(err.to_string().contains("it is read-only"), "{err}");
        assert!(queued_command(&mut rx).is_none(), "a refused name must not reach the Supervisor");
    }

    /// Before the first snapshot too: the path comes from the roster, not from the pushed payload.
    #[test]
    fn reading_a_state_field_off_the_capability_names_its_get_path() {
        let (lua, _handle, _rx) = lua_with_capability(0);

        let err = lua.load("return mantle.probe.volume").exec().unwrap_err();
        assert!(
            err.to_string().contains("mantle.audio has no `volume`: did you mean mantle.audio:get().volume?"),
            "{err}"
        );
        let err = lua.load("return mantle.probe.volme").exec().unwrap_err();
        assert!(err.to_string().contains("did you mean mantle.audio:get().volume?"), "{err}");

        let (tx, _rx) = mpsc::unbounded_channel();
        let (battery, _) = Capability::new("battery", DirtyFlag::new(), CommandSender::new(0, tx));
        lua.globals().get::<mlua::Table>("mantle").unwrap().set("battery", battery).unwrap();
        let err = lua.load("return mantle.battery.percent").exec().unwrap_err();
        assert!(err.to_string().contains("did you mean mantle.battery:get().percent?"), "{err}");
    }

    #[test]
    fn a_dot_call_is_a_config_error_naming_the_colon_form_and_queues_nothing() {
        let (lua, _handle, mut rx) = lua_with_capability(0);

        let err = lua.load("mantle.probe.set_volume(0.5)").exec().unwrap_err();

        assert!(err.to_string().contains("mantle.audio:set_volume("), "{err}");
        assert!(queued_command(&mut rx).is_none(), "a dot call must not send 0.5 as the receiver");
    }

    /// mlua resolves `get`/`map`/`on_change` before `__index`, so an action by one of those names
    /// would never reach the Supervisor.
    #[test]
    fn every_roster_action_is_a_method_no_builtin_shadows() {
        let lua = Lua::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let commands = CommandSender::new(0, tx);
        for capability in shared::Capability::ALL {
            let (member, _) = Capability::new(capability.as_str(), DirtyFlag::new(), commands.clone());
            lua.globals().set("cap", member).unwrap();
            for action in capability.actions() {
                lua.load(format!("cap:{action}()")).exec().unwrap();
                let sent = queued_command(&mut rx).expect("an action method must queue a command");
                assert_eq!(sent.params.action, *action, "mantle.{capability}:{action} is shadowed");
            }
        }
    }

    #[test]
    fn on_change_runs_after_a_push_with_the_new_and_the_replaced_payload() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load(
            r#"
            seen = {}
            mantle.probe:on_change(function(current, previous)
                seen[#seen + 1] = { current = current, previous = previous }
            end)
        "#,
        )
        .exec()
        .unwrap();

        let previous = handle.hydrate(Value::Integer(1), 1);
        handle.notify_change(&lua, previous);
        let previous = handle.hydrate(Value::Integer(2), 2);
        handle.notify_change(&lua, previous);

        let seen: mlua::Table = lua.globals().get("seen").unwrap();
        assert_eq!(seen.len().unwrap(), 2);
        let first: mlua::Table = seen.get(1).unwrap();
        assert_eq!(first.get::<i64>("current").unwrap(), 1);
        assert_eq!(first.get::<Value>("previous").unwrap(), Value::Nil, "the first push replaces nil");
        let second: mlua::Table = seen.get(2).unwrap();
        assert_eq!(second.get::<i64>("current").unwrap(), 2);
        assert_eq!(second.get::<i64>("previous").unwrap(), 1);
    }

    #[test]
    fn a_raising_handler_does_not_stop_the_next_one_or_the_push() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load(
            r#"
            ran = false
            mantle.probe:on_change(function() error("first handler broke") end)
            mantle.probe:on_change(function() ran = true end)
        "#,
        )
        .exec()
        .unwrap();

        let previous = handle.hydrate(Value::Integer(5), 1);
        handle.notify_change(&lua, previous);

        assert!(lua.globals().get::<bool>("ran").unwrap());
        let read: i64 = lua.load("return mantle.probe:get()").eval().unwrap();
        assert_eq!(read, 5);
    }

    #[test]
    fn clear_handlers_forgets_what_the_last_evaluation_registered() {
        let (lua, handle, _rx) = lua_with_capability(1);
        lua.load("count = 0; mantle.probe:on_change(function() count = count + 1 end)").exec().unwrap();
        handle.clear_handlers();
        let previous = handle.hydrate(Value::Integer(1), 1);
        handle.notify_change(&lua, previous);
        assert_eq!(lua.globals().get::<i64>("count").unwrap(), 0);
    }

    #[test]
    fn get_reads_the_same_live_value_a_bare_capability_global_would() {
        let (lua, handle, _rx) = lua_with_capability(0);

        let before: bool = lua.load("return mantle.probe:get() == nil").eval().unwrap();
        assert!(before, "a capability reads nil until its first snapshot (ADR-0037)");

        let pushed = lua.create_table().unwrap();
        pushed.set("attempts", 2).unwrap();
        handle.hydrate(Value::Table(pushed), 1);

        let attempts: i64 = lua.load("return mantle.probe:get().attempts").eval().unwrap();
        assert_eq!(attempts, 2);
    }

    #[test]
    fn map_returns_a_signal_that_tracks_later_pushes() {
        // The lock screen's failure text maps this handle; freezing the map at registration would
        // paint the first snapshot forever.
        let (lua, handle, _rx) = lua_with_capability(0);
        lua.load(r#"mapped = mantle.probe:map(function(s) return (s and s.error) or "" end)"#).exec().unwrap();

        let first: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(first, "");

        let pushed = lua.create_table().unwrap();
        pushed.set("error", "authentication failed").unwrap();
        handle.hydrate(Value::Table(pushed), 2);

        let second: String = lua.load("return mapped:get()").eval().unwrap();
        assert_eq!(second, "authentication failed");
    }
}

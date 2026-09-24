//! `process` global table and `ProcessHandle` userdata (ADR-0026).
//!
//! `process.run(cmd, args, out_cb, exit_cb)` runs on the Wayland dispatch thread during Lua
//! evaluation, with no socket in scope. [`ProcessRegistry`] queues the outbound `"process"`/`"run"`
//! envelope through the generation's [`CommandSender`] (ADR-0039).
//!
//! `Rc<RefCell<_>>` is correct because the registry and Lua closures stay on that thread.
//!
//! Callback convention, unspecified by the docs, is fixed here (ADR-0026):
//! `out_cb(line, stream)` uses Lua strings `"stdout"`/`"stderr"` while the wire keeps
//! `shared::ProcessStream`; `exit_cb(code)` receives an integer or `nil` via `Option<i32>`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, UserData, UserDataMethods};
use shared::{ProcessStream, warn};

use super::capability::CommandSender;

/// One `process.run` callback pair, retained until matching `SupervisorFrame::ProcessExited`.
struct PendingProcess {
    out_cb: Function,
    exit_cb: Function,
}

/// Retains callbacks, assigns `CommandEnvelope.id` in the Renderer (ADR-0026), and queues
/// outbound `"process"` commands. Renderer assignment permits synchronous `ProcessHandle` return.
#[derive(Clone)]
pub struct ProcessRegistry {
    pending: Rc<RefCell<HashMap<u64, PendingProcess>>>,
    commands: CommandSender,
}

impl ProcessRegistry {
    pub fn new(commands: CommandSender) -> Self {
        ProcessRegistry { pending: Rc::default(), commands }
    }

    fn send(&self, action: &str, arguments: Vec<serde_json::Value>, id: u64) {
        self.commands.send_as(id, "process", action, arguments, 0);
    }

    fn run(&self, cmd: String, args: Vec<String>, out_cb: Function, exit_cb: Function) -> ProcessHandle {
        let id = self.commands.next_id();
        self.pending.borrow_mut().insert(id, PendingProcess { out_cb, exit_cb });
        self.send("run", vec![serde_json::json!(cmd), serde_json::json!(args)], id);
        ProcessHandle { id, registry: self.clone() }
    }

    /// Queues a `"detach"` with no callbacks retained: a detached program has no handle, no
    /// output and no exit code to deliver, so there is no pending pair to leak (ADR-0188). The
    /// envelope still carries an id because every command does; nothing ever answers it.
    fn detach(&self, cmd: String, args: Vec<String>) {
        self.commands.send("process", "detach", vec![serde_json::json!(cmd), serde_json::json!(args)], 0);
    }

    fn kill(&self, id: u64) {
        self.send("kill", Vec::new(), id);
    }

    /// Dispatches `SupervisorFrame::ProcessOutput` to `id`'s `out_cb`. Stale/unknown ids, including
    /// forgotten generations or wire desyncs, are ignored.
    pub fn dispatch_output(&self, id: u64, stream: ProcessStream, line: String) {
        let out_cb = self.pending.borrow().get(&id).map(|p| p.out_cb.clone());
        let Some(out_cb) = out_cb else { return };
        if let Err(err) = out_cb.call::<()>((line, stream_name(stream))) {
            warn!("process.run(id={id}): out_cb raised an error: {err}");
        }
    }

    /// Dispatches `SupervisorFrame::ProcessExited`, invokes `exit_cb`, then forgets the id
    /// (ADR-0026).
    pub fn dispatch_exit(&self, id: u64, code: Option<i32>) {
        let exit_cb = self.pending.borrow_mut().remove(&id).map(|p| p.exit_cb);
        let Some(exit_cb) = exit_cb else { return };
        if let Err(err) = exit_cb.call::<()>(code) {
            warn!("process.run(id={id}): exit_cb raised an error: {err}");
        }
    }
}

fn stream_name(stream: ProcessStream) -> &'static str {
    match stream {
        ProcessStream::Stdout => "stdout",
        ProcessStream::Stderr => "stderr",
    }
}

/// Opaque userdata returned to Lua.
pub struct ProcessHandle {
    id: u64,
    registry: ProcessRegistry,
}

impl UserData for ProcessHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("kill", |_, this, ()| {
            this.registry.kill(this.id);
            Ok(())
        });
    }
}

/// Registers `process.run(cmd, args, out_cb, exit_cb)` and `process.detach(cmd, args)`; mlua's
/// closure signature supplies argument validation.
pub fn register(lua: &Lua, registry: ProcessRegistry) -> mlua::Result<()> {
    let detach_registry = registry.clone();
    super::define(lua, "process", "", lua.create_table()?)?;
    super::define(
        lua,
        "process.run",
        r#"---@class ProcessHandle
local ProcessHandle = {}

---`SIGTERM` to its process group, `SIGKILL` 100 ms later; `exit_cb` still fires. A no-op after exit.
function ProcessHandle:kill() end

---Spawns `cmd` with stdout and stderr piped and stdin on `/dev/null`, without blocking (ADR-0026).
---The process belongs to the generation: its group is reaped when the Renderer is replaced.
---Callbacks run unbudgeted; a raise is logged as a warning.
---[docs](https://anasgets111.github.io/mantle/guide/processes.html#processrun)
---@param cmd string Looked up on `PATH`; no shell, so no globbing, pipes or quoting.
---@param args string[] Already split: `"a b"` is one argument.
---@param out_cb fun(line: string, stream: "stdout"|"stderr") Once per line, newline stripped, cut at 64 KiB. Accumulate here and decode in `exit_cb`.
---@param exit_cb fun(code: integer?) `nil` when a signal ended it or it failed to spawn.
---@return ProcessHandle
"#,
        lua.create_function(move |_, (cmd, args, out_cb, exit_cb): (String, Vec<String>, Function, Function)| {
            Ok(registry.run(cmd, args, out_cb, exit_cb))
        })?,
    )?;
    super::define(
        lua,
        "process.detach",
        r#"---Spawns `cmd` in its own session with stdio on `/dev/null`; it outlives every reload and the
---shell. No handle, output or exit code, and a spawn failure is only logged (ADR-0188).
---[docs](https://anasgets111.github.io/mantle/guide/processes.html#processdetach)
---@param cmd string Looked up on `PATH`; no shell, so no globbing, pipes or quoting.
---@param args string[] Already split: `"a b"` is one argument.
"#,
        lua.create_function(move |_, (cmd, args): (String, Vec<String>)| {
            detach_registry.detach(cmd, args);
            Ok(())
        })?,
    )
}

#[cfg(test)]
mod tests {
    use shared::RendererFrame;
    use tokio::sync::mpsc;

    use super::*;
    use crate::lua::capability::tests::queued_command;

    fn lua_with_process(generation_id: u32) -> (Lua, ProcessRegistry, mpsc::UnboundedReceiver<RendererFrame>) {
        let lua = Lua::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let registry = ProcessRegistry::new(CommandSender::new(generation_id, tx));
        register(&lua, registry.clone()).unwrap();
        (lua, registry, rx)
    }

    #[test]
    fn process_run_returns_a_handle_and_queues_a_well_formed_run_command() {
        let (lua, _registry, mut rx) = lua_with_process(4);

        lua.load(r#"handle = process.run("echo", {"hi"}, function() end, function() end)"#).exec().unwrap();

        let envelope = queued_command(&mut rx).expect("a run command must have been queued");
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "process");
        assert_eq!(envelope.params.action, "run");
        assert_eq!(envelope.params.arguments, vec![serde_json::json!("echo"), serde_json::json!(["hi"])]);
        assert_eq!(envelope.id, 0, "the first process.run call on a fresh registry gets id 0");

        let is_userdata: bool = lua.load("return type(handle) == \"userdata\"").eval().unwrap();
        assert!(is_userdata, "process.run must return a userdata ProcessHandle");
    }

    /// ADR-0188. `detach` returns nothing and retains nothing: there is no handle to hold and no
    /// callback pair to leak, because a program this shell has let go of reports nothing back.
    #[test]
    fn process_detach_queues_a_command_and_retains_no_callbacks() {
        let (lua, registry, mut rx) = lua_with_process(7);

        let returned: mlua::Value = lua.load(r#"return process.detach("kate", {"notes.md"})"#).eval().unwrap();
        assert!(returned.is_nil(), "a detached program has no handle to give back");

        let envelope = queued_command(&mut rx).expect("a detach command must have been queued");
        assert_eq!(envelope.params.capability, "process");
        assert_eq!(envelope.params.action, "detach");
        assert_eq!(envelope.params.generation_id, 7);
        assert_eq!(envelope.params.arguments, vec![serde_json::json!("kate"), serde_json::json!(["notes.md"])]);
        assert!(
            registry.pending.borrow().is_empty(),
            "nothing may be retained for a process that will never report an exit"
        );
    }

    #[test]
    fn each_process_run_call_gets_a_distinct_monotonic_id() {
        let (lua, _registry, mut rx) = lua_with_process(0);

        lua.load(r#"process.run("a", {}, function() end, function() end)"#).exec().unwrap();
        lua.load(r#"process.run("b", {}, function() end, function() end)"#).exec().unwrap();

        assert_eq!(queued_command(&mut rx).unwrap().id, 0);
        assert_eq!(queued_command(&mut rx).unwrap().id, 1);
    }

    #[test]
    fn process_handle_kill_queues_a_kill_command_with_no_arguments_carrying_the_same_id() {
        let (lua, _registry, mut rx) = lua_with_process(4);

        lua.load(r#"handle = process.run("sleep", {"5"}, function() end, function() end); handle:kill()"#)
            .exec()
            .unwrap();

        let run_envelope = queued_command(&mut rx).unwrap();
        let kill_envelope = queued_command(&mut rx).expect("a kill command must have been queued");
        assert_eq!(kill_envelope.params.capability, "process");
        assert_eq!(kill_envelope.params.action, "kill");
        assert_eq!(kill_envelope.params.arguments, Vec::<serde_json::Value>::new());
        assert_eq!(kill_envelope.id, run_envelope.id);
    }

    #[test]
    fn process_run_rejects_a_non_function_callback() {
        let (lua, _registry, _rx) = lua_with_process(0);

        let err = lua.load(r#"process.run("echo", {}, "not a function", function() end)"#).exec().unwrap_err();
        assert!(err.to_string().contains("function"), "expected a type error mentioning function, got: {err}");
    }

    #[test]
    fn process_run_rejects_a_non_string_cmd() {
        let (lua, _registry, _rx) = lua_with_process(0);

        // `String` extraction coerces `42` to `"42"`; a table has no such coercion and must reject.
        let err = lua.load(r#"process.run({}, {}, function() end, function() end)"#).exec().unwrap_err();
        assert!(err.to_string().contains("string"), "expected a type error mentioning string, got: {err}");
    }

    /// Reads the global a callback updated; callbacks have no other observable side channel.
    fn probe_table(lua: &Lua) -> mlua::Table {
        lua.load("probe = probe or {}; return probe").eval().unwrap()
    }

    #[test]
    fn dispatch_output_invokes_the_registered_out_cb_with_line_and_stream() {
        let (lua, registry, _rx) = lua_with_process(0);
        lua.load(r#"process.run("cmd", {}, function(line, stream) probe = { line = line, stream = stream } end, function() end)"#)
            .exec()
            .unwrap();

        registry.dispatch_output(0, ProcessStream::Stdout, "hello".to_string());

        let probe = probe_table(&lua);
        assert_eq!(probe.get::<String>("line").unwrap(), "hello");
        assert_eq!(probe.get::<String>("stream").unwrap(), "stdout");

        registry.dispatch_output(0, ProcessStream::Stderr, "oops".to_string());
        let probe = probe_table(&lua);
        assert_eq!(probe.get::<String>("stream").unwrap(), "stderr");
    }

    #[test]
    fn dispatch_output_on_an_unknown_id_is_silently_ignored() {
        let (_lua, registry, _rx) = lua_with_process(0);
        registry.dispatch_output(99, ProcessStream::Stdout, "unheard".to_string());
    }

    #[test]
    fn dispatch_exit_invokes_the_registered_exit_cb_with_the_code_then_forgets_the_id() {
        let (lua, registry, _rx) = lua_with_process(0);
        lua.load(r#"process.run("cmd", {}, function() end, function(code) probe = { code = code } end)"#)
            .exec()
            .unwrap();

        registry.dispatch_exit(0, Some(3));
        let probe = probe_table(&lua);
        assert_eq!(probe.get::<i64>("code").unwrap(), 3);

        lua.load("probe = nil").exec().unwrap();
        registry.dispatch_exit(0, Some(99));
        let is_nil: bool = lua.load("return probe == nil").eval().unwrap();
        assert!(is_nil, "a second dispatch_exit for an already-dispatched id must be a no-op");
    }

    #[test]
    fn dispatch_exit_passes_nil_for_an_absent_code() {
        let (lua, registry, _rx) = lua_with_process(0);
        lua.load(r#"process.run("cmd", {}, function() end, function(code) probe = { is_nil = code == nil } end)"#)
            .exec()
            .unwrap();

        registry.dispatch_exit(0, None);
        let probe = probe_table(&lua);
        assert!(probe.get::<bool>("is_nil").unwrap(), "a killed-by-signal exit must pass nil, not a synthesized code");
    }
}

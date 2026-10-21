//! `process` global table and `ProcessHandle` userdata (ADR-0026).
//!
//! `process.run(cmd, args, on_line, on_exit)` runs on the Wayland dispatch thread during Lua
//! evaluation, with no socket in scope. [`ProcessRegistry`] queues the outbound `"process"`/`"run"`
//! envelope through the generation's [`CommandSender`] (ADR-0039).
//!
//! `Rc<RefCell<_>>` is correct because the registry and Lua closures stay on that thread.
//!
//! Callback convention, unspecified by the docs, is fixed here (ADR-0026):
//! `on_line(line, stream)` uses Lua strings `"stdout"`/`"stderr"` while the wire keeps
//! `shared::ProcessStream`; `on_exit(code)` receives an integer or `nil` via `Option<i32>`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use mlua::{Function, Lua};
use shared::ProcessStream;

use super::luacats::{lua_class, lua_fn};
use crate::layout::node::prop::{Keyword, keywords};

use super::capability::CommandSender;
use crate::lua::call_logged;

/// One `process.run` callback pair, retained until matching `SupervisorFrame::ProcessExited`.
struct PendingProcess {
    on_line: Function,
    on_exit: Function,
}

/// Retains callbacks, assigns `CommandEnvelope.id` in the Renderer (ADR-0026), and queues
/// outbound `"process"` commands. Renderer assignment permits synchronous `ProcessHandle` return.
#[derive(Clone)]
pub struct ProcessRegistry {
    pending: Rc<RefCell<BTreeMap<u64, PendingProcess>>>,
    commands: CommandSender,
}

impl ProcessRegistry {
    pub fn new(commands: CommandSender) -> Self {
        ProcessRegistry { pending: Rc::default(), commands }
    }

    fn send(&self, action: &str, arguments: Vec<serde_json::Value>, id: u64) {
        self.commands.send_as(id, "process", action, arguments);
    }

    /// `process.capture`: the Supervisor answers a `"capture"` with at most one stdout frame carrying
    /// the whole text, so the callbacks are plain `run` ones: the line is stashed, `on_exit` delivers it.
    fn capture(&self, lua: &Lua, cmd: String, args: Vec<String>, on_done: Function) -> mlua::Result<ProcessHandle> {
        let stdout = Rc::new(RefCell::new(String::new()));
        let stash = stdout.clone();
        let on_line = lua.create_function(move |_, (text, _stream): (String, String)| {
            *stash.borrow_mut() = text;
            Ok(())
        })?;
        // A killed or unspawned child has nothing worth handing over: stdout is `nil` with the code.
        let on_exit = lua.create_function(move |_, code: Option<i32>| {
            on_done.call::<()>((code.map(|_| std::mem::take(&mut *stdout.borrow_mut())), code))
        })?;
        Ok(self.start("capture", cmd, args, on_line, on_exit))
    }

    fn start(
        &self,
        action: &str,
        cmd: String,
        args: Vec<String>,
        on_line: Function,
        on_exit: Function,
    ) -> ProcessHandle {
        let id = self.commands.next_id();
        self.pending.borrow_mut().insert(id, PendingProcess { on_line, on_exit });
        self.send(action, vec![serde_json::json!(cmd), serde_json::json!(args)], id);
        ProcessHandle { id, registry: self.clone() }
    }

    /// Queues a `"detach"` with no callbacks retained: a detached program has no handle, no
    /// output and no exit code to deliver, so there is no pending pair to leak (ADR-0188). The
    /// envelope still carries an id because every command does; nothing ever answers it.
    fn detach(&self, cmd: String, args: Vec<String>) {
        self.commands.send("process", "detach", vec![serde_json::json!(cmd), serde_json::json!(args)]);
    }

    fn kill(&self, id: u64) {
        self.send("kill", Vec::new(), id);
    }

    /// Kills every child the last evaluation started and runs each `on_exit(nil)` now, in start
    /// order, so a guard an exit resets is clear before the next evaluation's top level reads it.
    /// Forgetting the ids drops the output still in flight and the Supervisor's later exit frames.
    /// One pass: a child an `on_exit` starts here outlives this reload, so a retry chain cannot spin.
    pub fn kill_all(&self) {
        let ids: Vec<u64> = self.pending.borrow().keys().copied().collect();
        for id in ids {
            self.kill(id);
            self.dispatch_exit(id, None);
        }
    }

    /// Dispatches `SupervisorFrame::ProcessOutput` to `id`'s `on_line`. Stale/unknown ids, including
    /// forgotten generations or wire desyncs, are ignored.
    pub fn dispatch_output(&self, id: u64, stream: ProcessStream, line: String) {
        let on_line = self.pending.borrow().get(&id).map(|p| p.on_line.clone());
        let Some(on_line) = on_line else { return };
        call_logged(&on_line, (line, stream_name(stream)), format_args!("process.run(id={id}): on_line"));
    }

    /// Dispatches `SupervisorFrame::ProcessExited`, invokes `on_exit`, then forgets the id
    /// (ADR-0026).
    pub fn dispatch_exit(&self, id: u64, code: Option<i32>) {
        let on_exit = self.pending.borrow_mut().remove(&id).map(|p| p.on_exit);
        let Some(on_exit) = on_exit else { return };
        call_logged(&on_exit, code, format_args!("process.run(id={id}): on_exit"));
    }
}

keywords! {
    /// Which pipe an `on_line` line came from; the wire keeps `shared::ProcessStream`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Stream {
        Stdout,
        Stderr,
    }
}

fn stream_name(stream: ProcessStream) -> &'static str {
    match stream {
        ProcessStream::Stdout => Stream::Stdout,
        ProcessStream::Stderr => Stream::Stderr,
    }
    .name()
}

/// Opaque userdata returned to Lua.
pub struct ProcessHandle {
    id: u64,
    registry: ProcessRegistry,
}

lua_class! {
    impl ProcessHandle {
        /// `SIGTERM` to its process group, `SIGKILL` 100 ms later; `on_exit` still fires. A no-op after exit.
        fn kill(_lua, this) {
            this.registry.kill(this.id);
            Ok(())
        }
    }
}

/// Registers `process.run(cmd, args, on_line, on_exit)` and `process.detach(cmd, args)`; mlua's
/// closure signature supplies argument validation.
pub fn register(lua: &Lua, registry: ProcessRegistry) -> mlua::Result<()> {
    let detach_registry = registry.clone();
    let capture_registry = registry.clone();
    super::luacats::lua_table!(lua, process)?;
    lua_fn!(
        lua,
        /// Spawns `cmd` with stdout and stderr piped and stdin on `/dev/null`, without blocking (ADR-0026).
        /// The process belongs to the evaluation: a reload kills its group and calls `on_exit(nil)`.
        /// Callbacks run unbudgeted; a raise is logged as a warning.
        /// [docs](https://anasgets111.github.io/mantle/guide/processes.html#processrun)
        fn process.run(
            _lua,
            /// Looked up on `PATH`; no shell, so no globbing, pipes or quoting.
            cmd: String,
            /// Already split: `"a b"` is one argument. `nil` is none.
            args: Option<Vec<String>>,
            /// Once per line, newline stripped, cut at 64 KiB. Accumulate here and decode in `on_exit`.
            on_line: fn(line: String, stream: Stream),
            /// `nil` when a signal ended it or it failed to spawn.
            on_exit: fn(code: Option<i32>),
        ) -> ProcessHandle {
            Ok(registry.start("run", cmd, args.unwrap_or_default(), on_line.0, on_exit.0))
        }
    )?;
    lua_fn!(
        lua,
        /// Runs `cmd` like `process.run`, then hands `on_done` its whole stdout as one string, newlines
        /// kept, and its exit code. stderr is dropped. Only the first 1 MiB of stdout is kept.
        /// A reload kills it and calls `on_done(nil, nil)`.
        /// [docs](https://anasgets111.github.io/mantle/guide/processes.html#processcapture)
        fn process.capture(
            lua,
            /// Looked up on `PATH`; no shell, so no globbing, pipes or quoting.
            cmd: String,
            /// Already split: `"a b"` is one argument. `nil` is none.
            args: Option<Vec<String>>,
            /// `stdout` and `code` are both `nil` when a signal ended it or it failed to spawn.
            on_done: fn(stdout: Option<String>, code: Option<i32>),
        ) -> ProcessHandle {
            capture_registry.capture(lua, cmd, args.unwrap_or_default(), on_done.0)
        }
    )?;
    lua_fn!(
        lua,
        /// Spawns `cmd` in its own session with stdio on `/dev/null`; it outlives every reload and the
        /// shell. No handle, output or exit code, and a spawn failure is only logged (ADR-0188).
        /// [docs](https://anasgets111.github.io/mantle/guide/processes.html#processdetach)
        fn process.detach(
            _lua,
            /// Looked up on `PATH`; no shell, so no globbing, pipes or quoting.
            cmd: String,
            /// Already split: `"a b"` is one argument. `nil` is none.
            args: Option<Vec<String>>,
        ) {
            detach_registry.detach(cmd, args.unwrap_or_default());
            Ok(())
        }
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
    fn process_run_queues_a_well_formed_run_command_with_distinct_ids_and_its_handle_kills_under_the_same_id() {
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

        // Ids are distinct and monotonic per call.
        lua.load(r#"process.run("b", nil, function() end, function() end)"#).exec().unwrap();
        let nil_args = queued_command(&mut rx).unwrap();
        assert_eq!(nil_args.id, 1);
        assert_eq!(nil_args.params.arguments, vec![serde_json::json!("b"), serde_json::json!([])], "nil args is none");

        lua.load(r#"handle = process.run("sleep", {"5"}, function() end, function() end); handle:kill()"#)
            .exec()
            .unwrap();

        let run_envelope = queued_command(&mut rx).unwrap();
        assert_eq!(run_envelope.id, 2);
        let kill_envelope = queued_command(&mut rx).expect("a kill command must have been queued");
        assert_eq!(kill_envelope.params.capability, "process");
        assert_eq!(kill_envelope.params.action, "kill");
        assert_eq!(kill_envelope.params.arguments, Vec::<serde_json::Value>::new());
        assert_eq!(kill_envelope.id, run_envelope.id);
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

        lua.load(r#"process.detach("kate")"#).exec().unwrap();
        let bare = queued_command(&mut rx).unwrap();
        assert_eq!(bare.params.arguments, vec![serde_json::json!("kate"), serde_json::json!([])], "args are optional");
    }

    /// Reads the global a callback updated; callbacks have no other observable side channel.
    fn probe_table(lua: &Lua) -> mlua::Table {
        lua.load("probe = probe or {}; return probe").eval().unwrap()
    }

    #[test]
    fn dispatch_output_invokes_the_registered_on_line_with_line_and_stream() {
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
        let (lua, registry, _rx) = lua_with_process(0);
        lua.load(r#"process.run("cmd", {}, function() heard = true end, function() end)"#).exec().unwrap();

        registry.dispatch_output(99, ProcessStream::Stdout, "unheard".to_string());

        assert!(lua.load("return heard").eval::<Option<bool>>().unwrap().is_none(), "no other id's callback ran");
        assert_eq!(registry.pending.borrow().len(), 1, "and the registered process is untouched");
    }

    #[test]
    fn dispatch_exit_passes_the_code_or_nil_then_forgets_the_id() {
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

        lua.load(r#"process.run("cmd", {}, function() end, function(code) probe = { is_nil = code == nil } end)"#)
            .exec()
            .unwrap();
        registry.dispatch_exit(1, None);
        assert!(
            probe_table(&lua).get::<bool>("is_nil").unwrap(),
            "a killed-by-signal exit must pass nil, not a synthesized code"
        );
    }

    #[test]
    fn capture_queues_a_capture_and_delivers_the_whole_stdout_with_the_code_or_nils() {
        let (lua, registry, mut rx) = lua_with_process(2);
        lua.load(r#"process.capture("cmd", nil, function(out, code) probe = { out = out, code = code } end)"#)
            .exec()
            .unwrap();
        assert_eq!(queued_command(&mut rx).expect("queued").params.action, "capture");

        registry.dispatch_output(0, ProcessStream::Stdout, "a\nb\n".to_string());
        registry.dispatch_exit(0, Some(3));
        let probe = probe_table(&lua);
        assert_eq!((probe.get::<String>("out").unwrap(), probe.get::<i64>("code").unwrap()), ("a\nb\n".into(), 3));

        lua.load("probe = nil").exec().unwrap();
        lua.load(r#"process.capture("cmd", nil, function(out, code) probe = { n = out == nil and code == nil } end)"#)
            .exec()
            .unwrap();
        registry.dispatch_output(1, ProcessStream::Stdout, "partial".to_string());
        registry.kill_all();
        assert!(probe_table(&lua).get::<bool>("n").unwrap(), "a killed capture hands over nil, nil");

        lua.load(r#"process.capture("cmd", nil, function(out, code) probe = { out = out } end)"#).exec().unwrap();
        registry.dispatch_exit(2, Some(0));
        assert_eq!(probe_table(&lua).get::<String>("out").unwrap(), "", "no output is an empty string");
    }
}

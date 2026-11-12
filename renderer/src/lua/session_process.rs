//! `session_process { name, stop_signal }`: one long-running program, declared by name, read as
//! signals and driven with methods.
//!
//! Sibling of `lua::store`'s `persistent_table` and built the same way -- a plain Lua table whose
//! real fields are the methods and whose `__index` answers everything else with a signal mapped
//! over the owning capability, cached with `rawset` so later reads are ordinary.
//!
//! The reason it exists is lifetime. `process.run`'s child belongs to the generation that spawned
//! it and its group is reaped when that Renderer is replaced, which is right for a helper that answers a question
//! and exits. A program the config wants to keep -- a recorder, a stream a widget reads -- has to
//! outlive the VM that started it, and the only thing here that does is the Supervisor. So the
//! config names the program and the Supervisor holds it; what comes back is state, like every
//! other capability, rather than a handle that would be stale by the next reload.

use std::collections::HashMap;

use mlua::{IntoLua, Lua, LuaSerdeExt, ObjectLike, Table, Value};

use super::luacats::{As, LuaType, lua_fn, spelled};
use super::store::{capability, index_entry_signals};
use crate::layout::node::preview_for_error;

/// Handles keyed by declared name, so two declarations of one program share a table and its
/// signals. Survives VM re-evaluation (ADR-0044 decision 4), so a reload hands back the same one.
#[derive(Default)]
struct SessionRegistry(HashMap<String, Table>);

/// Registers `session_process`. `mantle.processes` is resolved at call time: registration runs in
/// `Loader::new`, before `lua::namespace::build` creates `mantle`.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// Declares a program that lives for the session: the Supervisor holds it across reloads and stops
        /// it at shutdown. Re-declaring a name returns the same handle and re-reads only `stop_signal`, so
        /// declare at a module's top level. Use `process.run` when you need its output.
        /// [docs](https://anasgets111.github.io/mantle/guide/processes.html#session_process)
        fn session_process(
            lua,
            /// `name` keys it in `mantle.processes`; empty raises. `stop_signal` defaults to `"TERM"`.
            spec: As<Table, Spec>,
        ) -> SessionProcessHandle {
            let spec = spec.0;
            super::marshal::only_keys(&spec, &["name", "stop_signal"])
                .map_err(|detail| mlua::Error::runtime(format!("session_process: {detail}")))?;
            let name: String = spec.get("name")?;
            if name.is_empty() {
                return Err(mlua::Error::runtime(
                    "session_process: name is how the Supervisor keys this program and how the config reads it back; it cannot be empty",
                ));
            }
            let stop_signal: Value = spec.get("stop_signal")?;

            lua.from_value::<Option<shared::action::SignalName>>(stop_signal.clone()).map_err(|_| {
                let given = stop_signal.as_string().map(|name| name.to_string_lossy()).unwrap_or_default();
                let got = super::marshal::expected_one_of(shared::action::SignalName::NAMES, &given, &preview_for_error(&stop_signal));
                mlua::Error::runtime(format!("session_process: stop_signal: expected a signal name, {got}"))
            })?;
            let processes = capability(lua, "session_process", "processes")?;
            // Sent every evaluation, like `storage:open`: the Supervisor keeps the entry it has
            // and takes the newer stop signal, so editing that lands on reload without disturbing
            // a program already up.
            processes.call_method::<()>("declare", (name.clone(), stop_signal))?;

            if let Some(existing) = super::app_data_or_default::<SessionRegistry>(lua).0.get(&name).cloned() {
                return Ok(SessionProcessHandle(existing));
            }

            let handle = build_handle(lua, &name, processes)?;
            super::app_data_or_default::<SessionRegistry>(lua).0.insert(name, handle.clone());
            Ok(SessionProcessHandle(handle))
        }
    )
}

/// `session_process`'s `spec`, checked key by key with messages naming the call.
struct Spec;

spelled!(Spec => format!("{{ name: {}, stop_signal?: SignalName, [string]: \"no such property\" }}", String::lua()));

/// What `session_process` returns: [`build_handle`]'s table, whose real fields are the methods and
/// whose other keys are signals over the program's `mantle.processes` entry. The capability's entry
/// type, not a Rust signature here, fixes those fields, so the class block is written beside it.
struct SessionProcessHandle(Table);

impl IntoLua for SessionProcessHandle {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

impl LuaType for SessionProcessHandle {
    fn lua() -> String {
        "SessionProcessHandle".to_string()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        out.push(
            r#"---@class SessionProcessHandle
---A program declared with `session_process`. Each field is a signal over its `mantle.processes`
---entry, `nil` before the first push; while `running` is false they describe the finished run.
---@field running Signal<boolean?> Whether it is up.
---@field pid Signal<integer?> Also its process group id. Kept after exit; `nil` before a spawn or after a failed `start`.
---@field started_at Signal<integer?> Unix seconds the current or last run began; `nil` before a spawn or after a failed `start`.
---@field exit_code Signal<integer?> The last finished run's exit status; `nil` while running, before the first run, or after a signal ended it.
---@field start_error Signal<string?> Why the last `start` spawned nothing, usually a command not on `PATH`; `""` when it spawned.
local SessionProcessHandle = {}

---Starts the program unless it is already running, with stdio inherited. The outcome arrives as
---state: `running`, or `start_error`.
---@param cmd string Looked up on `PATH`; no shell, so no globbing, pipes or quoting.
---@param args? string[] Already split: `"a b"` is one argument.
function SessionProcessHandle:start(cmd, args) end

---Sends `signal` to the program itself, not its group. A no-op when it is not running.
---@param signal SignalName
function SessionProcessHandle:signal(signal) end

---Sends the declared `stop_signal` to the program's group, then `SIGKILL` 5 s later. Shell
---shutdown does this to every session process.
function SessionProcessHandle:stop() end
"#
            .to_string(),
        );
    }
}

/// Config table: real `start`/`signal`/`stop` fields; `__index` answers other keys with signals.
fn build_handle(lua: &Lua, name: &str, processes: mlua::AnyUserData) -> mlua::Result<Table> {
    let handle = lua.create_table()?;

    let owner = processes.clone();
    let program = name.to_string();
    handle.set(
        "start",
        lua.create_function(move |_, (_handle, cmd, args): (Table, String, Option<Vec<String>>)| {
            owner.call_method::<()>("start", (program.clone(), cmd, args.unwrap_or_default()))
        })?,
    )?;

    let owner = processes.clone();
    let program = name.to_string();
    handle.set(
        "signal",
        lua.create_function(move |_, (_handle, signal): (Table, String)| {
            owner.call_method::<()>("signal", (program.clone(), signal))
        })?,
    )?;

    let owner = processes.clone();
    let program = name.to_string();
    handle
        .set("stop", lua.create_function(move |_, _handle: Table| owner.call_method::<()>("stop", program.clone()))?)?;

    index_entry_signals(lua, &handle, &processes, "sessions", name, None)?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use crate::lua::tests::probe;
    use crate::socket::client::tests::{bare_client, push, queued_frame};
    use shared::RendererFrame;

    #[test]
    fn a_declared_session_process_declares_the_name_and_the_stop_signal_the_config_chose() {
        let (client, mut outbound_rx) = bare_client();

        client
            .loader()
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
            client.loader().lua().load(r#"session_process { name = "rec", stop_signal = "TEM" }"#).exec().unwrap_err();
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
        client.loader().lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();

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
        assert!(probe::<bool>(client.loader(), setup, "running"));
        assert_eq!(probe::<i64>(client.loader(), setup, "pid"), 4321);
        assert_eq!(probe::<i64>(client.loader(), setup, "started"), 1_700_000_000);
        assert_eq!(
            probe::<Option<i64>>(client.loader(), setup, "code"),
            None,
            "a run still going has no exit status, and nil is what a config must be able to test"
        );
    }

    #[test]
    fn an_undeclared_names_fields_read_nil_rather_than_a_stopped_program() {
        let (client, _outbound_rx) = bare_client();
        client.loader().lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();

        push(&client, "processes", 1, serde_json::json!({ "sessions": {} }));

        assert_eq!(
            probe::<Option<bool>>(client.loader(), "running = rec.running:get()", "running"),
            None,
            "before the Supervisor has answered, `running` is unknown rather than false"
        );
    }

    #[test]
    fn a_session_processs_methods_name_the_program_the_declaration_did() {
        let (client, mut outbound_rx) = bare_client();
        client.loader().lua().load(r#"rec = session_process { name = "recorder" }"#).exec().unwrap();
        let _declare = queued_frame(&mut outbound_rx);

        client
            .loader()
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
        assert!(probe::<bool>(client.loader(), setup, "same"), "the declared name is the identity");
    }
}

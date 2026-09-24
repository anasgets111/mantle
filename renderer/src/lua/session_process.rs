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

use mlua::{IntoLua, Lua, ObjectLike, Table, Value};

use super::luacats::{As, LuaType, lua_fn, spelled};
use super::store::{capability, index_entry_signals};

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

            let processes = capability(lua, "session_process", "processes")?;
            // Sent every evaluation, like `storage:open`: the Supervisor keeps the entry it has
            // and takes the newer stop signal, so editing that lands on reload without disturbing
            // a program already up.
            processes.call_method::<()>("invoke", ("declare", name.clone(), stop_signal))?;

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

spelled!(Spec => format!("{{ name: {}, stop_signal?: SignalName }}", String::lua()));

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
            owner.call_method::<()>("invoke", ("start", program.clone(), cmd, args.unwrap_or_default()))
        })?,
    )?;

    let owner = processes.clone();
    let program = name.to_string();
    handle.set(
        "signal",
        lua.create_function(move |_, (_handle, signal): (Table, String)| {
            owner.call_method::<()>("invoke", ("signal", program.clone(), signal))
        })?,
    )?;

    let owner = processes.clone();
    let program = name.to_string();
    handle.set(
        "stop",
        lua.create_function(move |_, _handle: Table| owner.call_method::<()>("invoke", ("stop", program.clone())))?,
    )?;

    index_entry_signals(lua, &handle, &processes, "sessions", name)?;
    Ok(handle)
}

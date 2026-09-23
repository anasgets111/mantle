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

use mlua::{Lua, ObjectLike, Table, Value};

use super::store::{capability, index_entry_signals};

/// Handles keyed by declared name, so two declarations of one program share a table and its
/// signals. Survives VM re-evaluation (ADR-0044 decision 4), so a reload hands back the same one.
#[derive(Default)]
struct SessionRegistry(HashMap<String, Table>);

/// Registers `session_process`. `mantle.processes` is resolved at call time: registration runs in
/// `Loader::new`, before `lua::namespace::build` creates `mantle`.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "session_process",
        lua.create_function(|lua, spec: Table| {
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
                return Ok(existing);
            }

            let handle = build_handle(lua, &name, processes)?;
            super::app_data_or_default::<SessionRegistry>(lua).0.insert(name, handle.clone());
            Ok(handle)
        })?,
    )
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

//! `persistent_table { path, name, defaults }` (ADR-0136): named JSON file, read as signals and
//! written one key at a time.
//!
//! Config supplies `path` and `name`, usually from `mantle.config_dir` and `os.getenv` (one of
//! ADR-0048's four calls), so `$XDG_STATE_HOME`, `$XDG_CACHE_HOME`, a file beside `shell.lua`, and
//! three simultaneous files are all the same call with different inputs.
//!
//! Plain Lua table, not userdata: a missing `store.theme` falls through `__index`, gets a signal,
//! and is `rawset` so later reads are ordinary. `set` is a real field, so configs cannot store that
//! key.

use std::collections::HashMap;

use mlua::{AnyUserData, Lua, ObjectLike, Table, Value};

use crate::lua::signal::{Signal, from_userdata};

/// Stores keyed by joined absolute path: two calls naming one file share one table/signals. It
/// survives VM re-evaluation (ADR-0044 decision 4), so reload does not hand back a second table.
#[derive(Default)]
struct StoreRegistry(HashMap<String, Table>);

/// Registers `persistent_table`. Resolve `mantle.storage` at call time; registration runs in
/// `Loader::new`, before `lua::namespace::build` creates `mantle`.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "persistent_table",
        lua.create_function(|lua, spec: Table| {
            super::marshal::only_keys(&spec, &["path", "name", "defaults"])
                .map_err(|detail| mlua::Error::runtime(format!("persistent_table: {detail}")))?;
            let path: String = spec.get("path")?;
            let name: String = spec.get("name")?;
            let defaults: Value = spec.get("defaults")?;
            let file = join(&path, &name)?;

            let storage = capability(lua, "persistent_table", "storage")?;
            // Send every evaluation: the Supervisor merges defaults (ADR-0136 decision 4), so
            // edited defaults land on reload without reverting user values.
            let defaults = match defaults {
                Value::Nil => Value::Table(lua.create_table()?),
                defaults => defaults,
            };
            storage.call_method::<()>("invoke", ("open", file.clone(), defaults))?;

            if let Some(existing) = super::app_data_or_default::<StoreRegistry>(lua).0.get(&file).cloned() {
                return Ok(existing);
            }

            let store = build_store(lua, &file, storage)?;
            super::app_data_or_default::<StoreRegistry>(lua).0.insert(file, store.clone());
            Ok(store)
        })?,
    )
}

/// Config file as one absolute path.
///
/// Keep `path` and `name` separate because config computes the directory and writes the filename.
/// Join here so config and Supervisor use the same key by construction.
fn join(path: &str, name: &str) -> mlua::Result<String> {
    if !path.starts_with('/') {
        return Err(mlua::Error::runtime(format!(
            "persistent_table: path must be absolute, got {path:?}. A relative path resolves against the Supervisor's working directory, which nothing sets"
        )));
    }
    if name.is_empty() || name.contains('/') {
        return Err(mlua::Error::runtime(format!("persistent_table: name is one file name, not a path, got {name:?}")));
    }
    Ok(format!("{}/{name}", path.trim_end_matches('/')))
}

/// `mantle.<name>` through namespace `__index`, so the read starts the capability
/// (ADR-0070 decision 1). `caller` names the global in the error.
pub(super) fn capability(lua: &Lua, caller: &str, name: &str) -> mlua::Result<AnyUserData> {
    let mantle: Table = lua.globals().get("mantle").map_err(|_| {
        mlua::Error::runtime(format!("{caller}: the `mantle` namespace is not built yet on this Lua state"))
    })?;
    mantle.get(name)
}

/// Answers every key `table` lacks with `payload[section][entry][key]` mapped over `capability`,
/// cached with `rawset` so later reads are plain and each key has one signal. `nil` before the
/// first push and for an absent entry or key, matching the property's documented default.
pub(super) fn index_entry_signals(
    lua: &Lua,
    table: &Table,
    capability: &AnyUserData,
    section: &'static str,
    entry: &str,
) -> mlua::Result<()> {
    let signal = from_userdata(capability)
        .ok_or_else(|| mlua::Error::runtime(format!("the `{section}` owner is not a signal")))?;
    let entry = entry.to_string();
    let metatable = lua.create_table()?;
    metatable.set(
        "__index",
        lua.create_function(move |lua, (table, key): (Table, String)| {
            let entry = entry.clone();
            let field = key.clone();
            let read = lua.create_function(move |_, payload: Value| {
                let Value::Table(payload) = payload else { return Ok(Value::Nil) };
                let Value::Table(entries) = payload.get::<Value>(section)? else { return Ok(Value::Nil) };
                let Value::Table(stored) = entries.get::<Value>(entry.as_str())? else { return Ok(Value::Nil) };
                stored.get::<Value>(field.as_str())
            })?;
            let key_signal = Signal::mapped(lua, lua.create_userdata(signal.clone())?, read)?;
            table.raw_set(key, key_signal.clone())?;
            Ok(key_signal)
        })?,
    )?;
    table.set_metatable(Some(metatable))
}

/// Config table: real `set` field; `__index` answers other keys with per-file signals.
fn build_store(lua: &Lua, file: &str, storage: mlua::AnyUserData) -> mlua::Result<Table> {
    let store = lua.create_table()?;
    index_entry_signals(lua, &store, &storage, "files", file)?;
    let path = file.to_string();
    store.set(
        "set",
        lua.create_function(move |_, (_store, key, value): (Table, String, Value)| {
            storage.call_method::<()>("invoke", ("set", path.clone(), key, value))
        })?,
    )?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_and_a_name_join_into_one_absolute_file() {
        assert_eq!(join("/home/u/.config/bar", "settings.json").unwrap(), "/home/u/.config/bar/settings.json");
        assert_eq!(join("/home/u/.config/bar/", "settings.json").unwrap(), "/home/u/.config/bar/settings.json");
    }

    #[test]
    fn a_relative_path_is_refused_at_the_call_rather_than_on_the_wire() {
        let err = join(".config/bar", "settings.json").unwrap_err().to_string();
        assert!(err.contains("must be absolute"), "the message has to say what to fix: {err}");
    }

    #[test]
    fn a_name_carrying_a_separator_is_refused() {
        assert!(join("/home/u", "nested/settings.json").is_err());
        assert!(join("/home/u", "").is_err());
    }
}

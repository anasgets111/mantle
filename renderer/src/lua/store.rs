//! `persistent_table { path, defaults }` (ADR-0136): JSON file, read as signals and written one
//! key at a time.
//!
//! Config supplies the file `path`, usually from `mantle.config_dir` and `os.getenv` (one of
//! ADR-0048's four calls), so `$XDG_STATE_HOME`, `$XDG_CACHE_HOME`, a file beside `shell.lua`, and
//! three simultaneous files are all the same call with different inputs.
//!
//! Plain Lua table, not userdata: a missing `store.theme` falls through `__index`, gets a signal,
//! and is `rawset` so later reads are ordinary. `set` is a real field, so configs cannot store that
//! key.

use std::collections::HashMap;

use mlua::{AnyUserData, IntoLua, Lua, ObjectLike, Table, Value};

use crate::layout::node::preview_str;
use crate::lua::luacats::{As, LuaType, lua_fn, spelled};
use crate::lua::signal::{Signal, from_userdata};

/// Stores keyed by absolute file path: two calls naming one file share one table/signals. It
/// survives VM re-evaluation (ADR-0044 decision 4), so reload does not hand back a second table.
#[derive(Default)]
struct StoreRegistry(HashMap<String, Table>);

/// Registers `persistent_table`. Resolve `mantle.storage` at call time; registration runs in
/// `Loader::new`, before `lua::namespace::build` creates `mantle`.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// A JSON file read as signals (ADR-0136). The same file returns the same table across reloads.
        /// `defaults` fills only missing keys, so adding one keeps the user's values.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#persistent_table)
        fn persistent_table(
            lua,
            /// `path` is a file path, absolute or `~/` under `$HOME`, not ending in `/`; otherwise raises.
            /// The key in `mantle.storage`'s `contents` is the expanded absolute path.
            spec: As<Table, Spec>,
        ) -> PersistentTable {
            let spec = spec.0;
            super::marshal::only_keys(&spec, &["path", "defaults"])
                .map_err(|detail| mlua::Error::runtime(format!("persistent_table: {detail}")))?;
            let path: String = spec.get("path")?;
            let defaults: Value = spec.get("defaults")?;
            let file = file_path(&path)?;

            let storage = capability(lua, "persistent_table", "storage")?;
            // Send every evaluation: the Supervisor merges defaults (ADR-0136 decision 4), so
            // edited defaults land on reload without reverting user values.
            let defaults = match defaults {
                Value::Nil => lua.create_table()?,
                Value::Table(defaults) => defaults,
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "persistent_table: defaults must be a table, got {}",
                        crate::lua::marshal::a_type(&other)
                    )));
                }
            };
            storage.call_method::<()>("open", (file.clone(), defaults.clone()))?;

            if let Some(existing) = super::app_data_or_default::<StoreRegistry>(lua).0.get(&file).cloned() {
                return Ok(PersistentTable(existing));
            }

            let store = build_store(lua, &file, storage, defaults)?;
            super::app_data_or_default::<StoreRegistry>(lua).0.insert(file, store.clone());
            Ok(PersistentTable(store))
        }
    )
}

/// `persistent_table`'s `spec`, checked key by key with messages naming the call.
struct Spec;

spelled!(Spec => r#"{ path: string, defaults?: table, [string]: "no such property" }"#);

/// What `persistent_table` returns: [`build_store`]'s table, whose `set` is real and whose other
/// keys are signals of the file's JSON values, whatever keys it holds; so the class block is written
/// here rather than read off a Rust signature.
struct PersistentTable(Table);

impl IntoLua for PersistentTable {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

impl LuaType for PersistentTable {
    fn lua() -> String {
        "PersistentTable".to_string()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        out.push(
            r#"---@class PersistentTable
---One JSON file. Every key but `set` reads as a signal of its stored value: the declared default until
---this file's first push, and again for a key the file lacks, such as one `set(key, nil)` deleted; `nil` if it has no default.
---@field set fun(self: PersistentTable, key: string, value: any) Stores one JSON value; `nil` deletes the key. The file is saved 1 s after the last write.
---@field [string] Signal<any>
"#
            .to_string(),
        );
    }
}

/// The config's `path` as the key config and Supervisor share: `~/` expanded, absolute and naming a file.
fn file_path(path: &str) -> mlua::Result<String> {
    let path = &*super::marshal::expand_path("persistent_table", path)?;
    if !path.starts_with('/') {
        return Err(mlua::Error::runtime(format!(
            "persistent_table: path must be absolute or start with ~/, got {}; a relative one resolves against the \
             Supervisor's working directory, which nothing sets",
            preview_str(path)
        )));
    }
    if std::path::Path::new(path).file_name().is_none() || path.ends_with('/') {
        return Err(mlua::Error::runtime(format!(
            "persistent_table: path must name a file, got {}",
            preview_str(path)
        )));
    }
    // One file, one key: `/a/./x` and `/a//x` would otherwise make two stores for it.
    if path.split('/').skip(1).any(|part| matches!(part, "" | "." | "..")) {
        return Err(mlua::Error::runtime(format!(
            "persistent_table: path must not contain empty, `.` or `..` components, got {}",
            preview_str(path)
        )));
    }
    Ok(path.to_string())
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
/// cached with `rawset` so later reads are plain and each key has one signal. A store's `defaults`
/// answer for every entry the payload lacks and for every key a present entry lacks; without one a
/// missing key is `nil`, matching the property's documented default.
pub(super) fn index_entry_signals(
    lua: &Lua,
    table: &Table,
    capability: &AnyUserData,
    section: &'static str,
    entry: &str,
    defaults: Option<Table>,
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
            let defaults = defaults.clone();
            let read = lua.create_function(move |_, payload: Value| {
                let stored = match payload {
                    Value::Table(payload) => match payload.get::<Value>(section)? {
                        Value::Table(entries) => entries.get::<Value>(entry.as_str())?,
                        _ => Value::Nil,
                    },
                    _ => Value::Nil,
                };
                let value = match stored {
                    Value::Table(stored) => stored.get::<Value>(field.as_str())?,
                    _ => Value::Nil,
                };
                // A key the file lacks reads its default, as after `set(key, nil)`; the Supervisor's `open` refills the file.
                if !value.is_nil() {
                    return Ok(value);
                }
                defaults.as_ref().map_or(Ok(Value::Nil), |defaults| defaults.get(field.as_str()))
            })?;
            let key_signal = Signal::mapped(lua, lua.create_userdata(signal.clone())?, read, Value::Nil)?;
            table.raw_set(key, key_signal.clone())?;
            Ok(key_signal)
        })?,
    )?;
    table.set_metatable(Some(metatable))
}

/// Config table: real `set` field; `__index` answers other keys with per-file signals.
fn build_store(lua: &Lua, file: &str, storage: mlua::AnyUserData, defaults: Table) -> mlua::Result<Table> {
    let store = lua.create_table()?;
    index_entry_signals(lua, &store, &storage, "contents", file, Some(defaults))?;
    let path = file.to_string();
    store.set(
        "set",
        lua.create_function(move |_, (_store, key, value): (Table, String, Value)| {
            storage.call_method::<()>("set", (path.clone(), key, value))
        })?,
    )?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_file_path_is_its_own_key() {
        assert_eq!(file_path("/home/u/.config/bar/settings.json").unwrap(), "/home/u/.config/bar/settings.json");
        match shared::expand_tilde("~/bar/settings.json") {
            Ok(expanded) => assert_eq!(file_path("~/bar/settings.json").unwrap(), expanded),
            Err(_) => assert!(file_path("~/bar/settings.json").is_err(), "no $HOME, no expansion"),
        }
    }

    #[test]
    fn a_relative_path_or_a_directory_is_refused_at_the_call_rather_than_on_the_wire() {
        for (path, needle) in [
            (".config/bar/settings.json", "must be absolute"),
            ("/home/u/bar/", "must name a file"),
            ("/", "must name a file"),
            ("/home/u/..", "must name a file"),
            ("/home/u/./x.json", "empty, `.` or `..`"),
            ("/home/u//x.json", "empty, `.` or `..`"),
            ("/home/u/../x.json", "empty, `.` or `..`"),
        ] {
            let err = file_path(path).unwrap_err().to_string();
            assert!(err.contains(needle), "{path:?}: the message has to say what to fix: {err}");
        }
    }
}

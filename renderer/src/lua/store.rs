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
struct StoreRegistry(HashMap<String, (Table, Table)>);

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

            // The store's signals read this table, not the one passed, so a reload's edited defaults
            // land in the table the first call built.
            let existing = super::app_data_or_default::<StoreRegistry>(lua).0.get(&file).cloned();
            let (store, kept) = match existing {
                Some(entry) => entry,
                None => {
                    let kept = lua.create_table()?;
                    (build_store(lua, &file, storage, kept.clone())?, kept)
                }
            };
            kept.clear()?;
            defaults.for_each(|key: Value, value: Value| kept.raw_set(key, value))?;
            super::app_data_or_default::<StoreRegistry>(lua).0.insert(file, (store.clone(), kept));
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
    let path = super::marshal::absolute_path("persistent_table", path)?;
    let path = &*path.to_string_lossy();
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
    use crate::lua;
    use crate::lua::tests::probe;
    use crate::socket::client::tests::{bare_client, push, queued_frame};
    use shared::RendererFrame;

    /// A reload's `persistent_table` call hands back the same table, so its edited defaults have to reach it.
    #[test]
    fn a_second_declaration_of_one_file_answers_its_new_defaults() {
        use crate::lua::capability::Capability;
        use crate::lua::signal::DirtyFlag;
        let lua = Lua::new();
        crate::lua::signal::register(&lua, DirtyFlag::new()).unwrap();
        register(&lua).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (storage, _handle) =
            Capability::new("storage", DirtyFlag::new(), crate::lua::capability::CommandSender::new(0, tx));
        let mantle = lua.create_table().unwrap();
        mantle.set("storage", storage).unwrap();
        lua.globals().set("mantle", mantle).unwrap();
        let read = |defaults: &str| {
            lua.load(format!(r#"return persistent_table({{ path = "/s/x.json", defaults = {defaults} }}).theme:get()"#))
                .eval::<String>()
                .unwrap()
        };
        assert_eq!(read(r#"{ theme = "dark" }"#), "dark");
        crate::lua::signal::begin_evaluation(&lua);
        assert_eq!(read(r#"{ theme = "light" }"#), "light");
    }

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

    #[test]
    fn a_declared_store_opens_the_file_the_config_named_and_nothing_else() {
        // ADR-0136: path, file name, and defaults belong to config; the envelope must carry exactly
        // `shell.lua`'s values, not a directory chosen here.
        let (client, mut outbound_rx) = bare_client();

        client
            .loader()
            .lua()
            .load(r#"store = persistent_table { path = "/tmp/bar/settings.json", defaults = { theme = "mocha" } }"#)
            .exec()
            .unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("declaring a store must queue a RendererFrame::Command");
        };
        assert_eq!(envelope.params.capability, "storage");
        assert_eq!(envelope.params.action, "open");
        assert_eq!(
            envelope.params.arguments,
            vec![serde_json::json!("/tmp/bar/settings.json"), serde_json::json!({ "theme": "mocha" })],
            "the joined path is the key both sides use, so it is built once, here"
        );
    }

    #[test]
    fn a_stores_key_reads_the_value_the_supervisor_pushed_for_that_file() {
        let (client, _outbound_rx) = bare_client();
        client.loader().lua().load(r#"store = persistent_table { path = "/tmp/bar/state.json" }"#).exec().unwrap();

        push(
            &client,
            "storage",
            1,
            serde_json::json!({
                "contents": { "/tmp/bar/state.json": { "theme": "latte", "wallpaper": { "fit": "cover" } } }
            }),
        );

        let setup = r#"
            theme = store.theme:get()
            fit = store.wallpaper:get().fit
            unset = store.nothing_here:get()
        "#;
        assert_eq!(probe::<String>(client.loader(), setup, "theme"), "latte");
        assert_eq!(probe::<String>(client.loader(), setup, "fit"), "cover", "a table value is stored whole");
        assert_eq!(
            probe::<Option<String>>(client.loader(), setup, "unset"),
            None,
            "a key the file does not have reads nil, so a property falls back to its documented default"
        );
    }

    #[test]
    fn a_stores_declared_default_reads_before_the_first_push_and_whenever_the_file_lacks_the_key() {
        let (client, _outbound_rx) = bare_client();
        client
            .loader()
            .lua()
            .load(r#"store = persistent_table { path = "/tmp/bar/state.json", defaults = { theme = "mocha" } }"#)
            .exec()
            .unwrap();

        let setup = r#"theme = store.theme:get() other = store.other:get()"#;
        assert_eq!(probe::<String>(client.loader(), setup, "theme"), "mocha");
        assert_eq!(probe::<Option<String>>(client.loader(), setup, "other"), None, "no default, no value");

        push(&client, "storage", 1, serde_json::json!({ "contents": { "/tmp/bar/state.json": { "theme": "latte" } } }));
        assert_eq!(probe::<String>(client.loader(), setup, "theme"), "latte");

        // `set("theme", nil)` deletes the key; the next push lacks it and the default shows again.
        push(&client, "storage", 2, serde_json::json!({ "contents": { "/tmp/bar/state.json": {} } }));
        assert_eq!(probe::<String>(client.loader(), setup, "theme"), "mocha");
        assert_eq!(probe::<Option<String>>(client.loader(), setup, "other"), None);
    }

    #[test]
    fn a_stores_key_runs_on_change_for_a_write_to_that_key_only() {
        let (client, _outbound_rx) = bare_client();
        let lua = client.loader().lua();
        lua.load(
            r#"
            store = persistent_table { path = "/tmp/bar/state.json", defaults = { theme = "mocha" } }
            seen = {}
            store.theme:on_change(function(now, before) seen[#seen + 1] = before .. "->" .. now end)
            "#,
        )
        .exec()
        .unwrap();
        let push = |revision, payload| {
            push(&client, "storage", revision, payload);
            lua::signal::run_state_handlers(lua);
        };

        push(1, serde_json::json!({ "contents": { "/tmp/bar/state.json": { "theme": "latte" } } }));
        push(2, serde_json::json!({ "contents": { "/tmp/bar/state.json": { "theme": "latte", "font": "Inter" } } }));

        let seen: Vec<String> = lua.load("return seen").eval().unwrap();
        assert_eq!(seen, ["mocha->latte"], "a write to another key fires nothing");
    }

    #[test]
    fn a_store_the_first_push_omits_still_reads_its_defaults() {
        let (client, _outbound_rx) = bare_client();
        client
            .loader()
            .lua()
            .load(
                r#"
                a = persistent_table { path = "/tmp/bar/a.json", defaults = { theme = "mocha" } }
                b = persistent_table { path = "/tmp/bar/b.json", defaults = { theme = "frappe" } }"#,
            )
            .exec()
            .unwrap();
        push(&client, "storage", 1, serde_json::json!({ "contents": { "/tmp/bar/a.json": { "theme": "latte" } } }));

        let setup = r#"a_theme = a.theme:get() b_theme = b.theme:get() b_other = b.other:get()"#;
        assert_eq!(probe::<String>(client.loader(), setup, "a_theme"), "latte");
        assert_eq!(probe::<String>(client.loader(), setup, "b_theme"), "frappe", "B has not been pushed yet");
        assert_eq!(probe::<Option<String>>(client.loader(), setup, "b_other"), None);
    }

    #[test]
    fn a_stores_write_names_the_same_file_the_declaration_did() {
        let (client, mut outbound_rx) = bare_client();
        client.loader().lua().load(r#"store = persistent_table { path = "/tmp/bar/state.json" }"#).exec().unwrap();
        let _open = queued_frame(&mut outbound_rx);

        client.loader().lua().load(r#"store:set("theme", "latte")"#).exec().unwrap();

        let RendererFrame::Command(envelope) = queued_frame(&mut outbound_rx) else {
            panic!("a store write must be queued as RendererFrame::Command");
        };
        assert_eq!(envelope.params.action, "set");
        assert_eq!(
            envelope.params.arguments,
            vec![serde_json::json!("/tmp/bar/state.json"), serde_json::json!("theme"), serde_json::json!("latte")]
        );
    }

    #[test]
    fn two_declarations_of_one_file_are_one_table() {
        // A module and `shell.lua` may redeclare a store, and in-place reload reruns all
        // declarations: all three executions land on one table and signal per key, or one `:set()`
        // leaves the other stale.
        let (client, _outbound_rx) = bare_client();

        let setup = r#"
            first = persistent_table { path = "/tmp/bar/state.json" }
            second = persistent_table { path = "/tmp/bar/state.json" }
            same = rawequal(first, second)
        "#;
        assert!(probe::<bool>(client.loader(), setup, "same"), "the file path is the identity");
    }

    #[test]
    fn a_store_path_must_be_an_absolute_file_and_name_is_gone() {
        let (client, _outbound_rx) = bare_client();
        for (spec, needle) in [
            (r#"{ path = "bar/state.json" }"#, "must be absolute"),
            (r#"{ path = "/tmp/bar/" }"#, "must name a file"),
            (r#"{ path = "/tmp/bar", name = "state.json" }"#, "name"),
        ] {
            let err = client.loader().lua().load(format!("persistent_table {spec}")).exec().unwrap_err().to_string();
            assert!(err.contains(needle), "{spec}: {err}");
        }
    }
}

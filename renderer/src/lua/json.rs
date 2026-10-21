//! `json` global table (ADR-0057): the config's reader for structured subprocess output and its
//! writer for JSON arguments.
//!
//! `process.run`'s `on_line` fires once per newline-stripped line, so a config polling
//! `lsblk --json` accumulates and decodes the buffer. Without a decoder, such subprocesses are
//! fire-and-forget.

use mlua::{Lua, LuaSerdeExt, Table, Value};

/// The sole JSON-to-Lua mapping, used by pushed capability payloads and `json.decode`, so `null` maps
/// one way everywhere.
///
/// mlua's serde bridge defaults map `null` to truthy lightuserdata, so `if payload.field` would treat
/// null as data. Both `*_to_null` options are off: `null` becomes `nil` and erases the table key.
/// Cost: a JSON-array `null` leaves a hole and stops `ipairs` (`[1, null, 3]` yields one element, `#` is
/// 3). Null fields (tray `icon_path`, `toggle_state`) matter; no capability produces null elements.
pub fn to_lua(lua: &Lua, json: &serde_json::Value) -> mlua::Result<Value> {
    let options = mlua::serde::ser::Options::new().serialize_none_to_null(false).serialize_unit_to_null(false);
    lua.to_value_with(json, options)
}

/// Parse and conversion failures become one message for `json.decode`; a convention that requires
/// `pcall` is a raise with extra steps. The conversion error may be unreachable (`from_slice`'s
/// recursion limit trips first) but [`register`]'s contract promises the path.
fn decode(lua: &Lua, bytes: &[u8]) -> Result<Value, String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    to_lua(lua, &json).map_err(|err| format!("decoded, but could not be converted to a Lua value: {err}"))
}

/// The reverse of [`to_lua`]: a hole inside an array's length writes `null`, as a `null` element
/// decodes to a hole; `nil` alone writes `null`. A table whose keys are all positive integers is an
/// array, so `{}` writes `[]`, while at most half its slots are holes; any other key makes it an
/// object, whose keys must be strings.
///
/// mlua refuses functions, userdata and threads, and keeps its own cycle check as a backstop.
/// [`Walk`] first refuses what would pass silently or never end: NaN and infinity, which
/// serde_json writes as `null`, nesting past what decode reads, a shared table expanding past
/// [`MAX_VALUES`], a sparse array, and a decoded array given a named key.
fn encode(lua: &Lua, value: Value) -> Result<String, String> {
    Walk { array_mt: lua.array_metatable(), path: Vec::new(), left: MAX_VALUES }.reject_unencodable(&value)?;
    let options = mlua::serde::de::Options::new().detect_mixed_tables(true).sort_keys(true);
    let json: serde_json::Value = lua.from_value_with(value, options).map_err(|err| err.to_string())?;
    serde_json::to_string(&json).map_err(|err| err.to_string())
}

/// The deepest nesting `decode` reads back: serde_json's limit of 128 stops at the 128th container.
const MAX_DEPTH: usize = 127;

/// Values one encode may visit. A table shared n levels deep is walked 2^n times, and native code
/// runs no instruction hook, so this is the only stop.
/// ponytail: a 1Mi-value cap; raise it if a config legitimately encodes more.
const MAX_VALUES: usize = 1 << 20;

/// The pre-walk [`encode`] runs before handing a value to mlua.
struct Walk {
    array_mt: Table,
    /// The tables from the root to the current value, so a repeat is a cycle, not a shared table.
    path: Vec<*const std::ffi::c_void>,
    left: usize,
}

impl Walk {
    fn reject_unencodable(&mut self, value: &Value) -> Result<(), String> {
        self.left = self.left.checked_sub(1).ok_or(format!("more than {MAX_VALUES} values"))?;
        match value {
            Value::Number(n) if !n.is_finite() => Err(format!("{n} has no JSON form")),
            Value::Table(table) => {
                let at = table.to_pointer();
                if self.path.contains(&at) {
                    return Err("a table contains itself".into());
                }
                if self.path.len() == MAX_DEPTH {
                    return Err(format!("tables nest deeper than {MAX_DEPTH}"));
                }
                self.path.push(at);
                // mlua writes an all-integer-key table up to its highest index, so `{ [2^40] = 1 }`
                // would allocate that many nulls. An integral float key past `i64` is an index to mlua too.
                let (mut count, mut highest, mut array) = (0i64, 0i64, true);
                for pair in table.pairs::<Value, Value>() {
                    let (key, value) = pair.map_err(|err| err.to_string())?;
                    match key {
                        Value::Integer(index) if index >= 1 => (count, highest) = (count + 1, highest.max(index)),
                        Value::Number(n) if n >= 1.0 && n.fract() == 0.0 => (count, highest) = (count + 1, i64::MAX),
                        _ => array = false,
                    }
                    self.reject_unencodable(&key)?;
                    self.reject_unencodable(&value)?;
                }
                self.path.pop();
                // mlua writes a table carrying decode's array metatable by `#` alone, dropping named keys.
                if !array && table.metatable().as_ref() == Some(&self.array_mt) {
                    return Err("a decoded array has a named key, which a JSON array cannot hold".into());
                }
                // ponytail: refuses arrays over half holes; stream the serialization to accept any density.
                if array && highest > 2 * count {
                    return Err(format!("an array with {count} values up to index {highest} is more than half holes"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Registers `json.decode(text)` and `json.encode(value)`.
///
/// Decode returns one value, or `nil` plus a message, matching `io.open`. Failure is routine (`on_line`
/// supplies partial buffers and non-JSON stdout), so raising would force `pcall` everywhere. No trailing
/// `nil`: `table.insert(t, decoded, nil)` would take the table as a position and raise.
///
/// A bare top-level `null` succeeds as `nil`, indistinguishable from failure to `if t then` (pinned by
/// `decode_of_a_bare_top_level_null_is_indistinguishable_from_a_decode_failure`); a success sentinel
/// would bring back the truthy-lightuserdata bug [`to_lua`] avoids. Takes `mlua::LuaString` so
/// non-UTF-8 bytes reach serde as a decode error, not an argument-conversion raise.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    super::luacats::lua_table!(lua, json)?;
    super::luacats::lua_fn!(
        lua,
        /// Decodes JSON and never raises: failure returns `nil, message`. JSON `null` also decodes to `nil`,
        /// and a `null` array element leaves a hole that stops `ipairs` (ADR-0057).
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#jsondecode)
        fn json.decode(lua, text: mlua::LuaString) -> (value: Value, error: Option<String>) as Result<Value, String> {
            super::signal::anchor_cpu_budget(lua);
            Ok(decode(lua, &text.as_bytes()))
        }
    )?;
    super::luacats::lua_fn!(
        lua,
        /// Encodes a value as compact JSON with sorted keys. A table with only positive integer keys is an
        /// array, so `{}` is `[]`, and a hole inside it is `null`; other tables need string keys. Raises
        /// on a function, userdata, cycle, NaN, infinity, nesting past 127, over 2^20 values, an array over
        /// half holes or a decoded array given a named key.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#jsonencode)
        fn json.encode(
            lua,
            /// Tables, strings, numbers, booleans and `nil`.
            value: Value,
        ) -> String {
            super::signal::anchor_cpu_budget(lua);
            encode(lua, value).map_err(|err| mlua::Error::runtime(format!("json.encode: {err}")))
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::tests::lua_registered;

    fn encoded(lua: &Lua, chunk: &str) -> Result<String, String> {
        lua.load(chunk).eval::<String>().map_err(|err| err.to_string())
    }

    #[test]
    fn encode_writes_compact_sorted_json_that_decode_reads_back() {
        let lua = lua_registered(register);
        let text = encoded(&lua, r#"return json.encode({ b = { 1, 2.5, "x" }, a = true, c = { n = "é" } })"#);
        assert_eq!(text.as_deref(), Ok(r#"{"a":true,"b":[1,2.5,"x"],"c":{"n":"é"}}"#));
        let same: bool = lua
            .load(r#"local t = json.decode(json.encode({ 1, 2, { k = "v" } })) return t[2] == 2 and t[3].k == "v""#)
            .eval()
            .unwrap();
        assert!(same);
    }

    #[test]
    fn encode_mirrors_decodes_null_mapping() {
        let lua = lua_registered(register);
        assert_eq!(encoded(&lua, "return json.encode({ 1, nil, 3 })").as_deref(), Ok("[1,null,3]"), "a hole is null");
        assert_eq!(encoded(&lua, "return json.encode(nil)").as_deref(), Ok("null"));
        assert_eq!(encoded(&lua, "return json.encode({})").as_deref(), Ok("[]"));
        assert_eq!(encoded(&lua, r#"return json.encode("a\"b\n")"#).as_deref(), Ok(r#""a\"b\n""#));
        assert_eq!(encoded(&lua, "return json.encode(3.0)").as_deref(), Ok("3.0"), "a float stays a float");
    }

    #[test]
    fn encode_refuses_what_json_cannot_hold_instead_of_writing_null() {
        let lua = lua_registered(register);
        for (chunk, needle) in [
            ("return json.encode({ f = print })", "function"),
            ("return json.encode({ 0/0 })", "no JSON form"),
            ("return json.encode({ x = math.huge })", "no JSON form"),
            ("local t = {} t.self = t return json.encode(t)", "contains itself"),
            ("return json.encode({ 1, 2, x = 3 })", "json.encode"),
            ("local t = {} for _ = 1, 200 do t = { t } end return json.encode(t)", "deeper than 127"),
            ("return json.encode({ [2^40] = 1 })", "more than half holes"),
            ("local t = json.decode('[1]') t.name = 'x' return json.encode(t)", "named key"),
            ("return json.encode({ [2^63] = 1 })", "more than half holes"),
            ("local t = {} for _ = 1, 60 do t = { t, t } end return json.encode(t)", "values"),
        ] {
            let err = encoded(&lua, chunk).expect_err(chunk);
            assert!(err.contains(needle), "{chunk}: {err}");
        }
        let shared: String = lua
            .load("local s = { 1 } return json.encode({ a = s, b = s })")
            .eval()
            .expect("a shared table is no cycle");
        assert_eq!(shared, r#"{"a":[1],"b":[1]}"#);
    }

    #[test]
    fn the_deepest_table_encode_accepts_decodes_again() {
        let lua = lua_registered(register);
        let chunk = format!(
            "local t = {{}} for _ = 2, {MAX_DEPTH} do t = {{ t }} end return json.decode(json.encode(t)) ~= nil"
        );
        assert!(lua.load(&chunk).eval::<bool>().unwrap(), "encode wrote JSON decode refuses");
    }

    /// A config polling an untrusted subprocess must get nil plus a message, never a raised error,
    /// a stack overflow, or a Lua argument error past its error check.
    #[test]
    fn decode_failure_returns_nil_and_a_message() {
        let lua = lua_registered(register);
        let deep = format!("{}1{}", "[".repeat(10_000), "]".repeat(10_000));
        for (input, case) in [
            (br#"{"volume":"#.to_vec(), "malformed input"),
            (deep.into_bytes(), "nesting deep enough to overflow a recursive walk"),
            (b"{\"name\":\"\xff\xfe\"}".to_vec(), "bytes that are not utf8"),
        ] {
            lua.globals().set("raw", lua.create_string(&input).unwrap()).unwrap();
            let (value, message): (Value, Option<String>) = lua.load("return json.decode(raw)").eval().unwrap();
            assert_eq!(value, Value::Nil, "{case}");
            assert!(
                message.is_some_and(|m| !m.is_empty()),
                "{case}: the second return has to carry something loggable"
            );
        }
    }

    #[test]
    fn decode_maps_a_null_field_to_an_absent_key_exactly_as_a_pushed_capability_payload_does() {
        let lua = lua_registered(register);
        let (key_count, absent): (i64, bool) = lua
            .load(
                r#"
                local t = json.decode('{"icon_name":"a","icon_path":null}')
                local n = 0
                for _ in pairs(t) do n = n + 1 end
                return n, t.icon_path == nil
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(key_count, 1, "a null field is erased, not left as a truthy sentinel");
        assert!(absent);
    }

    #[test]
    fn decode_returns_one_value_on_success_so_it_can_be_forwarded_straight_into_another_call() {
        let lua = lua_registered(register);
        // A trailing nil would make three-argument `table.insert` treat the value as a position.
        let count: i64 = lua.load(r#"local t = {} table.insert(t, json.decode('{"a":1}')) return #t"#).eval().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn decode_of_a_bare_top_level_null_is_indistinguishable_from_a_decode_failure() {
        let lua = lua_registered(register);
        let (value, message): (Value, Option<String>) = lua.load(r#"return json.decode('null')"#).eval().unwrap();
        assert_eq!(value, Value::Nil);
        assert_eq!(
            message, None,
            "it succeeded, so there is no message -- but `if t then` cannot tell that apart from a parse error"
        );
    }

    /// Real captured `niri msg -j focused-window` input, preserving its non-ASCII title and
    /// `layout` null nested two levels deep.
    #[test]
    fn decode_reads_a_real_niri_focused_window_line_including_its_nested_nulls() {
        let lua = lua_registered(register);
        lua.globals()
            .set(
                "line",
                r#"{"id":2,"title":"◐ Docs/build-steps.md next step","app_id":"kitty","pid":1481,"workspace_id":11,"is_focused":true,"layout":{"pos_in_scrolling_layout":null,"tile_size":[1536.0,960.0],"window_size":[1536,960],"tile_pos_in_workspace_view":[192.0,120.0]},"focus_timestamp":{"secs":64419,"nanos":188252639}}"#,
            )
            .unwrap();
        let (title, scrolling_absent, width): (String, bool, i64) = lua
            .load(
                r#"
                local w = json.decode(line)
                return w.title, w.layout.pos_in_scrolling_layout == nil, w.layout.window_size[1]
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            title, "\u{25d0} Docs/build-steps.md next step",
            "a \\uXXXX escape has to come back out as the character it names"
        );
        assert!(scrolling_absent, "a null two levels down has to erase its key like a top-level one");
        assert_eq!(width, 1536, "an integer array element stays an integer, not a float");
    }
}

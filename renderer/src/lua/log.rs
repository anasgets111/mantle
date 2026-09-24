//! `log` global table (ADR-0245): config diagnostics through `shared::log`.

use mlua::{Lua, Value, Variadic};
use shared::log::Level;

/// `print`'s join, so a `print` becomes `log.info` unchanged.
fn message(args: Variadic<Value>) -> mlua::Result<String> {
    Ok(args.iter().map(Value::to_string).collect::<mlua::Result<Vec<_>>>()?.join("\t"))
}

/// The stub of `log.warn`, `log.info` and `log.debug`; `log.error`'s carries the doc they share.
const LEVEL: &str = "---@param ... any\n---[docs](https://anasgets111.github.io/mantle/guide/scripting.html#log)\n";

pub fn register(lua: &Lua) -> mlua::Result<()> {
    super::define(lua, "log", "", lua.create_table()?)?;
    for (name, level, stub) in [
        (
            "error",
            Level::Error,
            r#"---Writes a stamped line at this level, arguments joined like `print`'s (ADR-0245). Every level
---prints by default; filter with `MANTLE_LOG=config=warn` or `config=off`.
---@param ... any
---[docs](https://anasgets111.github.io/mantle/guide/scripting.html#log)
"#,
        ),
        ("warn", Level::Warn, LEVEL),
        ("info", Level::Info, LEVEL),
        ("debug", Level::Debug(1), LEVEL),
    ] {
        super::define(
            lua,
            &format!("log.{name}"),
            stub,
            lua.create_function(move |_, args: Variadic<Value>| {
                shared::log::emit(level, "config", format_args!("{}", message(args)?));
                Ok(())
            })?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_joins_its_arguments_like_print() {
        let lua = Lua::new();
        let args: Variadic<Value> = lua
            .load(r#"return "volume", 0.5, nil, setmetatable({}, { __tostring = function() return "sink" end })"#)
            .eval()
            .unwrap();
        assert_eq!(message(args).unwrap(), "volume\t0.5\tnil\tsink");
    }
}

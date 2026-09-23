//! `log` global table (ADR-0245): config diagnostics through `shared::log`.

use mlua::{Lua, Value, Variadic};
use shared::log::Level;

/// `print`'s join, so a `print` becomes `log.info` unchanged.
fn message(args: Variadic<Value>) -> mlua::Result<String> {
    Ok(args.iter().map(Value::to_string).collect::<mlua::Result<Vec<_>>>()?.join("\t"))
}

pub fn register(lua: &Lua) -> mlua::Result<()> {
    let table = lua.create_table()?;
    for (name, level) in
        [("error", Level::Error), ("warn", Level::Warn), ("info", Level::Info), ("debug", Level::Debug(1))]
    {
        table.set(
            name,
            lua.create_function(move |_, args: Variadic<Value>| {
                shared::log::emit(level, "config", format_args!("{}", message(args)?));
                Ok(())
            })?,
        )?;
    }
    lua.globals().set("log", table)
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

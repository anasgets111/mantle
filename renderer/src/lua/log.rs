//! `log` global table (ADR-0245): config diagnostics through `shared::log`.

use mlua::{Lua, Value, Variadic};
use shared::log::Level;

/// `print`'s join, so a `print` becomes `log.info` unchanged.
fn message(args: Variadic<Value>) -> mlua::Result<String> {
    Ok(args.iter().map(Value::to_string).collect::<mlua::Result<Vec<_>>>()?.join("\t"))
}

/// `log.<name>` at `level`; `log.error`'s doc is the one they share.
macro_rules! level {
    ($lua:expr, $name:ident, $level:expr, $(#[doc = $doc:literal])*) => {
        super::luacats::lua_fn!(
            $lua,
            $(#[doc = $doc])*
            /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#log)
            fn log.$name(_lua, args: Variadic<Value>) {
                shared::log::emit($level, "config", format_args!("{}", message(args)?));
                Ok(())
            }
        )
    };
}

/// Lua's `warn` pieces joined into one line. On by default, unlike stock Lua: a config that calls
/// `warn` wants it read. `@on`/`@off` still toggle it; other `@` controls are ignored, as in Lua.
#[derive(Default)]
struct Warnings {
    off: bool,
    line: String,
}

impl Warnings {
    /// The line to log once `piece` completes a message.
    fn take(&mut self, piece: &str, incomplete: bool) -> Option<String> {
        self.line.push_str(piece);
        if incomplete {
            return None;
        }
        let line = std::mem::take(&mut self.line);
        match line.as_str() {
            "@on" => self.off = false,
            "@off" => self.off = true,
            _ if line.starts_with('@') || self.off => {}
            _ => return Some(line),
        }
        None
    }
}

pub fn register(lua: &Lua) -> mlua::Result<()> {
    // mlua's state has no warn function (`luaL_newstate` would install one), so `warn` went nowhere.
    let warnings = std::cell::RefCell::new(Warnings::default());
    lua.set_warning_function(move |_, piece, incomplete| {
        if let Some(line) = warnings.borrow_mut().take(piece, incomplete) {
            shared::log::emit(Level::Warn, "config", format_args!("{line}"));
        }
        Ok(())
    });
    super::luacats::lua_table!(lua, log)?;
    level!(
        lua,
        error,
        Level::Error,
        /// Writes a stamped line at this level, arguments joined like `print`'s (ADR-0245). Every level
        /// prints by default; filter with `MANTLE_LOG=config=warn` or `config=off`.
    )?;
    level!(lua, warn, Level::Warn,)?;
    level!(lua, info, Level::Info,)?;
    level!(lua, debug, Level::Debug(1),)
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

    #[test]
    fn warn_joins_its_pieces_and_honours_on_and_off() {
        let mut warnings = Warnings::default();
        assert_eq!(warnings.take("low ", true), None);
        assert_eq!(warnings.take("battery", false).as_deref(), Some("low battery"));
        assert_eq!(warnings.take("@off", false), None);
        assert_eq!(warnings.take("hidden", false), None);
        assert_eq!(warnings.take("@on", false), None);
        assert_eq!(warnings.take("@other", false), None);
        assert_eq!(warnings.take("shown", false).as_deref(), Some("shown"));
    }
}

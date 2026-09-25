//! Where config code is, as errors print it: chunks named relative to the config directory, the
//! construction site a node or derived signal records, and tracebacks without the engine's frames.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use mlua::{Lua, Table, Value};

/// Replaces Lua's file searcher, `package.searchers[2]`, with one that names the chunk as
/// [`chunk_name`] does. Lua's names it by the absolute path, which `LUA_IDSIZE` (60 bytes) cuts to
/// `...2b41-2949-.../widgets/bar.lua:4` in every error and traceback. Same search, same two
/// results, so `package.loaded` and ADR-0047's forgetting see no difference.
pub(super) fn name_required_chunks_relatively(lua: &Lua, config_dir: &Path) -> mlua::Result<()> {
    let root = config_dir.to_path_buf();
    let searcher = lua.create_function(move |lua, name: mlua::LuaString| {
        let package: Table = lua.globals().get("package")?;
        let search: mlua::Function = package.get("searchpath")?;
        let (found, tried): (Option<String>, Value) = search.call((name, package.get::<Value>("path")?))?;
        let Some(path) = found else { return Ok((tried, Value::Nil)) };
        let source = std::fs::read(&path).map_err(mlua::Error::external)?;
        let chunk = format!("@{}", chunk_name(&root, Path::new(&path)));
        let loader = lua.load(source).set_name(chunk).into_function()?;
        Ok((Value::Function(loader), Value::String(lua.create_string(&path)?)))
    })?;
    lua.globals().get::<Table>("package")?.get::<Table>("searchers")?.set(2, searcher)
}

/// `path` relative to the config directory, the name every error and traceback shows for it.
pub(super) fn chunk_name(config_dir: &Path, path: &Path) -> String {
    path.strip_prefix(config_dir).unwrap_or(path).display().to_string()
}

/// A Lua error as a config author reads it: without mlua's `runtime error: ` prefix, and without
/// the traceback frames that are the engine's glue rather than config code. A `[C]` metamethod,
/// `__index` upvalue or unnamed `[C]: in ?` is mlua's error handler or userdata dispatch, and
/// `__mlua_*` chunks are mlua's own Lua; none of them names a line the author wrote. An error that
/// crossed a Rust callback carries a second traceback, the tail of the first, so only the first is
/// kept.
pub(crate) fn describe(err: &mlua::Error) -> String {
    let text = err.to_string();
    let text = text.strip_prefix("runtime error: ").unwrap_or(&text);
    let text = match text.match_indices("stack traceback:").nth(1) {
        Some((second, _)) => &text[..second],
        None => text,
    };
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            let frame = line.trim_start_matches('\t');
            frame.len() == line.len()
                || !(frame.starts_with("[C]: in metamethod ")
                    || frame.starts_with("[C]: in upvalue '__")
                    || frame == "[C]: in ?"
                    || frame.starts_with("__mlua"))
        })
        .collect();
    if lines.last() == Some(&"stack traceback:") {
        lines.pop();
    }
    lines.join("\n")
}

/// A line of config code, `widgets/bar.lua:12` once displayed: where a node or derived signal was
/// constructed, for an error that surfaces in a later layout pass, when that line is no longer on
/// the stack (ADR-0268). One Lua integer, the chunk's index in [`CHUNKS`] above the line, so
/// recording one makes no string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site(i64);

thread_local! {
    /// Every chunk name a [`Site`] has recorded, interned. Thread-local rather than per VM because
    /// a site is displayed where no `Lua` is at hand (a refused child in `layout::node::spec`); the
    /// VM runs on one thread (ADR-0039), so the thread that records a site is the one that
    /// displays it. Append-only, one entry per distinct file, each numbered by its arrival.
    static CHUNKS: RefCell<HashMap<Box<str>, i64>> = RefCell::default();
}

impl Site {
    /// The Lua code calling the running Rust function. `None` when that caller is not Lua, as in
    /// `pcall(text, props)`.
    pub(crate) fn of_caller(lua: &Lua) -> Option<Site> {
        lua.inspect_stack(1, |frame| {
            let line = i64::try_from(frame.current_line()?).ok()?;
            let chunk = frame.source().short_src?;
            let index = CHUNKS.with_borrow_mut(|chunks| match chunks.get(chunk.as_ref()) {
                Some(&index) => index,
                None => {
                    let index = chunks.len() as i64;
                    chunks.insert(chunk.as_ref().into(), index);
                    index
                }
            });
            Some(Site(index << 32 | (line & 0xffff_ffff)))
        })
        .flatten()
    }

    /// The Lua value a table or user value stores it as.
    pub(crate) fn to_lua(site: Option<Site>) -> Option<i64> {
        site.map(|site| site.0)
    }

    /// Back from [`Self::to_lua`]. A non-integer, or an integer naming no recorded chunk, is no
    /// site; a config's own integer `__site` can still pass for one.
    pub(crate) fn from_lua(value: &Value) -> Option<Site> {
        let Value::Integer(packed) = value else { return None };
        let known = CHUNKS.with_borrow(|chunks| (0..chunks.len() as i64).contains(&(packed >> 32)));
        known.then_some(Site(*packed))
    }
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let line = self.0 & 0xffff_ffff;
        // A scan, but only an error displays a site.
        CHUNKS.with_borrow(|chunks| match chunks.iter().find(|(_, index)| **index == self.0 >> 32) {
            Some((chunk, _)) => write!(f, "{chunk}:{line}"),
            None => write!(f, "?:{line}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    /// `error` raised in the main chunk, and a failing `computed` read through `:get()`, which
    /// crosses a Rust callback and so gathers a second traceback.
    #[test]
    fn describe_keeps_one_traceback_of_config_frames() {
        let lua = Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let run = |source: &str| super::describe(&lua.load(source).set_name("@shell.lua").exec().unwrap_err());

        assert_eq!(
            run("error('broken')"),
            "shell.lua:1: broken\nstack traceback:\n\t[C]: in function 'error'\n\tshell.lua:1: in main chunk"
        );
        let read = run("local s = state('a', 1)\nreturn computed({s}, function(v) return v.x end):get()");
        assert_eq!(read.matches("stack traceback:").count(), 1, "{read}");
        assert!(read.ends_with("\tshell.lua:2: in main chunk"), "{read}");
    }
}

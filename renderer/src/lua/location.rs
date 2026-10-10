//! Where config code is, as errors print it: chunks named relative to the config directory, the
//! construction site a node or derived signal records, and tracebacks without the engine's frames.

use rustc_hash::FxHashMap as HashMap;
use std::cell::RefCell;
use std::path::Path;

use mlua::{IntoLuaMulti, Lua, Table, Value};
use shared::warn;

/// Replaces Lua's file searcher, `package.searchers[2]`, with one that names the chunk as
/// [`chunk_name`] does. Lua's names it by the absolute path, which `LUA_IDSIZE` (60 bytes) cuts to
/// `...2b41-2949-.../widgets/bar.lua:4` in every error and traceback. Same search, same two
/// results, so `package.loaded` and ADR-0047's forgetting see no difference.
pub(super) fn name_required_chunks_relatively(lua: &Lua, config_dir: &Path) -> mlua::Result<()> {
    let root = config_dir.to_path_buf();
    let prefix = format!("{}/", root.display());
    let searcher = lua.create_function(move |lua, name: mlua::LuaString| {
        let package: Table = lua.globals().get("package")?;
        let search: mlua::Function = package.get("searchpath")?;
        let (found, tried): (Option<String>, Value) = search.call((name, package.get::<Value>("path")?))?;
        let Some(path) = found else {
            // The config-relative form every other error uses, not the absolute search paths.
            let tried = match tried {
                Value::String(tried) => Value::String(lua.create_string(tried.to_str()?.replace(&prefix, ""))?),
                other => other,
            };
            return Ok((tried, Value::Nil));
        };
        let source = std::fs::read(&path).map_err(mlua::Error::external)?;
        let chunk = format!("@{}", chunk_name(&root, Path::new(&path)));
        let loader = lua.load(source).set_name(chunk).into_function()?;
        Ok((Value::Function(loader), Value::String(lua.create_string(&path)?)))
    })?;
    let searchers: Table = lua.globals().get::<Table>("package")?.get("searchers")?;
    searchers.set(2, searcher)?;
    // Safe mode loads no C modules; these two only add a "can't load C modules" line to the error.
    searchers.set(3, Value::Nil)?;
    searchers.set(4, Value::Nil)
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
    // `error({...})`: Lua stringifies a non-string error value as `table: 0x...`.
    let first = text.lines().next().unwrap_or_default();
    let object;
    let text = match first.split_once(": 0x") {
        Some((kind, address))
            if matches!(kind, "table" | "function" | "thread" | "userdata")
                && address.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            object = format!("error object is a {kind}, not a string{}", &text[first.len()..]);
            &object
        }
        _ => text,
    };
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

/// Logs a config callback's raise as `{what} raised, ignoring it: ... (defined at file:N)`; `Ok` is
/// silent. A callback nothing waits on must not take the turn with it.
pub(crate) fn warn_raised(handler: &mlua::Function, outcome: mlua::Result<()>, what: impl std::fmt::Display) {
    if let Err(err) = outcome {
        report_raised(handler, format!("{what} raised, ignoring it"), &err);
    }
}

/// Calls `f` for its side effects and logs a raise through [`warn_raised`]. Unbudgeted: for the
/// callbacks that run no CPU cap, see [`CpuBudget::call`](super::signal::CpuBudget::call) for those that do.
pub(crate) fn call_logged(f: &mlua::Function, args: impl IntoLuaMulti, what: impl std::fmt::Display) {
    warn_raised(f, f.call(args), what);
}

/// Most distinct `(label, message)` pairs [`report_raised`] remembers. ponytail: past it the table
/// resets and the next raises log in full again; upgrade path: an LRU.
const FOLD_CAP: usize = 64;

thread_local! {
    static RAISED: RefCell<HashMap<(String, String), u32>> = RefCell::default();
}

/// Forgets every folded raise, so a re-evaluated config's first raise logs in full.
pub(crate) fn forget_raised() {
    RAISED.with_borrow_mut(HashMap::clear);
}

/// Logs `head: message (defined at ...)` for a raise of `handler`. A handler that keeps raising the
/// same message (per pointer motion, per timer tick) logs the first and then only the 2nd, 4th,
/// 8th... as `raised again (N times)`, so a hot handler cannot flood the log.
pub(crate) fn report_raised(handler: &mlua::Function, head: String, err: &mlua::Error) {
    if let Some(line) = raised_line(handler, head, err) {
        warn!("{line}");
    }
}

fn raised_line(handler: &mlua::Function, head: String, err: &mlua::Error) -> Option<String> {
    let message = describe(err);
    let times = RAISED.with_borrow_mut(|seen| {
        if seen.len() >= FOLD_CAP && !seen.contains_key(&(head.clone(), message.clone())) {
            seen.clear();
        }
        let times = seen.entry((head.clone(), message.clone())).or_default();
        *times += 1;
        *times
    });
    if times == 1 {
        let info = handler.info();
        let at = match (info.short_src, info.line_defined) {
            (Some(src), Some(line)) => format!(" (defined at {src}:{line})"),
            _ => String::new(),
        };
        Some(format!("{head}: {message}{at}"))
    } else if times.is_power_of_two() {
        Some(format!("{head}: raised again ({times} times): {}", message.lines().next().unwrap_or_default()))
    } else {
        None
    }
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

    fn handler(lua: &Lua, source: &str) -> (mlua::Function, mlua::Error) {
        let f: mlua::Function = lua.load(source).set_name("@widgets/bar.lua").eval().unwrap();
        let err = f.call::<()>(()).unwrap_err();
        (f, err)
    }

    #[test]
    fn a_raise_names_where_the_handler_was_defined_and_repeats_fold() {
        let lua = Lua::new();
        let (f, err) = handler(&lua, "\n\nreturn function() error('boom') end");
        let line = |head: &str| super::raised_line(&f, head.into(), &err);

        let first = line("bar: on_hover raised, ignoring it").unwrap();
        assert!(first.contains("widgets/bar.lua:3: boom"), "{first}");
        assert!(first.ends_with("(defined at widgets/bar.lua:3)"), "{first}");
        let rest: Vec<_> = (2..=8).map(|_| line("bar: on_hover raised, ignoring it")).collect();
        assert!(rest[0].as_ref().unwrap().contains("raised again (2 times)"), "{rest:?}");
        assert_eq!(rest.iter().flatten().count(), 3, "2nd, 4th and 8th only: {rest:?}");
        assert!(line("other: on_hover raised, ignoring it").is_some(), "another label logs afresh");
        super::forget_raised();
        assert!(line("bar: on_hover raised, ignoring it").unwrap().contains("(defined at"), "reload logs in full");
    }

    #[test]
    fn a_non_string_error_value_is_named_by_type() {
        let lua = Lua::new();
        let (_, err) = handler(&lua, "return function() error({ code = 1 }) end");
        let text = super::describe(&err);
        assert!(text.starts_with("error object is a table, not a string"), "{text}");
    }

    #[test]
    fn a_missing_module_lists_config_relative_paths_and_no_c_loader() {
        let dir = tempfile::tempdir().unwrap();
        let loader = crate::lua::Loader::new(crate::lua::signal::DirtyFlag::new(), dir.path()).unwrap();
        let err = loader.lua().load("require('nope.missing')").exec().unwrap_err();
        let text = super::describe(&err);
        assert!(text.contains("no file 'nope/missing.lua'"), "{text}");
        assert!(!text.contains(&dir.path().display().to_string()), "{text}");
        assert!(!text.contains("C modules"), "{text}");
    }

    #[test]
    fn a_syntax_error_in_a_required_module_names_its_file_and_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mod.lua"), "local x = 1\nlocal = \n").unwrap();
        let loader = crate::lua::Loader::new(crate::lua::signal::DirtyFlag::new(), dir.path()).unwrap();
        let err = loader.lua().load("require('mod')").exec().unwrap_err();
        let text = super::describe(&err);
        assert!(text.contains("mod.lua:2:"), "{text}");
        assert!(!text.contains(&dir.path().display().to_string()), "{text}");
    }

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

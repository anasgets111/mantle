//! `file` global (ADR-0048 amendment): whole-file read and write off the Lua thread, results by
//! callback, the same shape as `palette.quantize`. The config VM itself still has no blocking IO.

use std::cell::OnceCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use mlua::{FromLua, Function, Lua, Table, Value};

use super::calls::{Calls, Job};
use super::luacats::{As, lua_class, lua_fn, spelled};
use crate::lua::call_logged;
use shared::warn;

/// ponytail: 8 MiB bounds the string a read hands the VM; a larger file errors. Stream or chunk if a use needs more.
const READ_CAP: u64 = 8 << 20;

/// What a call produced; the variant picks the callback's argument shape.
enum Outcome {
    Read(Result<String, String>),
    Write(Result<bool, String>),
}

/// `file.write`'s optional callback.
struct WriteDone;

spelled!(WriteDone => "fun(changed: boolean?, err: string?)");

/// `file.write`'s `opts`.
struct Options;

spelled!(Options => r#"{ in_place?: boolean, [string]: "no such property" }"#);

#[derive(Clone)]
pub struct FileRegistry {
    calls: Calls<Outcome>,
    /// One thread for all writes, so two writes to a path land in call order. Started by the first write.
    writer: Rc<OnceCell<Sender<Job>>>,
}

impl FileRegistry {
    pub fn new(waker: Option<crate::wake::Waker>) -> Self {
        Self { calls: Calls::new(waker, false), writer: Rc::default() }
    }

    /// For a config check: evaluating `shell.lua` must not write the user's files.
    pub fn checking() -> Self {
        Self { calls: Calls::new(None, true), writer: Rc::default() }
    }

    /// A write that outlives a cancel or reload still lands; only its callback goes.
    fn spawn(
        &self,
        ordered: bool,
        on_done: Option<Function>,
        work: impl FnOnce() -> Outcome + Send + 'static,
    ) -> FileHandle {
        let (id, job) = self.calls.submit(on_done, work);
        if let Some(job) = job {
            if ordered {
                let writer = self.writer.get_or_init(|| {
                    let (tx, rx) = std::sync::mpsc::channel::<Job>();
                    std::thread::spawn(move || rx.into_iter().for_each(|job| job()));
                    tx
                });
                let _ = writer.send(job);
            } else {
                // ponytail: a thread per read; a config reading hundreds of files at once wants a small pool.
                std::thread::spawn(job);
            }
        }
        FileHandle { id, calls: self.calls.clone() }
    }

    /// Drops every pending callback at an evaluation boundary.
    pub fn clear(&self) {
        self.calls.clear();
    }

    /// Runs each finished call's callback once; a cancelled or cleared id is dropped.
    pub fn poll(&self) {
        self.calls.poll(|id, on_done, outcome| {
            let what = format_args!("file(id={id}): callback");
            match outcome {
                Outcome::Read(Ok(text)) => call_logged(on_done, (text, None::<String>), what),
                Outcome::Read(Err(err)) => call_logged(on_done, (None::<String>, err), what),
                Outcome::Write(Ok(changed)) => call_logged(on_done, (changed, None::<String>), what),
                Outcome::Write(Err(err)) => call_logged(on_done, (None::<bool>, err), what),
            }
        });
    }
}

pub struct FileHandle {
    id: u64,
    calls: Calls<Outcome>,
}

lua_class! {
    impl FileHandle {
        /// Drops the callback. A write still lands.
        fn cancel(_lua, this) {
            this.calls.cancel(this.id);
            Ok(())
        }
    }
}

fn resolve(call: &str, path: &str) -> mlua::Result<PathBuf> {
    shared::expand_home(path)
        .map_err(|err| mlua::Error::runtime(format!("{call}: {err}, got {}", crate::layout::node::preview_str(path))))
}

fn read(path: &PathBuf) -> Result<String, String> {
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    file.take(READ_CAP + 1).read_to_end(&mut bytes).map_err(|err| err.to_string())?;
    if bytes.len() as u64 > READ_CAP {
        return Err(format!("file is over {} MiB", READ_CAP >> 20));
    }
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8".to_string())
}

fn write(path: &PathBuf, text: &str, in_place: bool) -> Result<bool, String> {
    // Opening a FIFO blocks until the other end appears and writes run one at a time, so only a regular file is read or written in place.
    let existing = std::fs::metadata(path).ok();
    if in_place && existing.as_ref().is_some_and(|meta| !meta.is_file()) {
        return Err("in_place needs a regular file".into());
    }
    if existing.is_some_and(|meta| meta.is_file())
        && std::fs::read(path).is_ok_and(|current| current == text.as_bytes())
    {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    }
    if in_place {
        // Truncates the existing inode, following a symlink; a new file gets 0666 minus umask.
        let mut file = std::fs::File::create(path).map_err(|err| err.to_string())?;
        file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).map_err(|err| err.to_string())?;
    } else {
        shared::write_atomic(path, text.as_bytes()).map_err(|err| err.to_string())?;
    }
    Ok(true)
}

pub fn register(lua: &Lua, registry: FileRegistry) -> mlua::Result<()> {
    super::luacats::lua_table!(lua, file)?;
    let reads = registry.clone();
    lua_fn!(
        lua,
        /// Reads a whole file off the Lua thread. `on_done` gets the text, or `nil` and an error message
        /// (missing, unreadable, not UTF-8, or over 8 MiB). A raise in `on_done` is logged as a warning.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#file)
        fn file.read(
            _lua,
            /// Absolute, or `~/` under `$HOME`; anything else raises.
            path: String,
            on_done: fn(text: Option<String>, err: Option<String>),
        ) -> FileHandle {
            let path = resolve("file.read", &path)?;
            Ok(reads.spawn(false, Some(on_done.0), move || {
                Outcome::Read(read(&path))
            }))
        }
    )?;
    lua_fn!(
        lua,
        /// Writes `text` to a file off the Lua thread, creating parent directories. When the file already
        /// holds exactly `text`, nothing is written and `changed` is `false`; otherwise the file is replaced atomically.
        /// `opts.in_place = true` rewrites the same inode instead, for files another program watches by path;
        /// a crash mid-write leaves a truncated file, and a watcher can read it empty or partial meanwhile.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#file)
        fn file.write(
            lua,
            /// Absolute, or `~/` under `$HOME`; anything else raises.
            path: String,
            text: String,
            /// `in_place` (default `false`): truncate and rewrite the file instead of replacing it. Pass `nil` to skip
            /// it before a callback.
            opts: As<Option<Value>, Option<Options>>,
            /// Runs once with `changed`, or `nil` and an error message. Cancelling or reloading drops it; the write still lands.
            on_done: As<Option<Function>, Option<WriteDone>>,
        ) -> FileHandle {
            let path = resolve("file.write", &path)?;
            if matches!(opts.0, Some(Value::Function(_))) {
                return Err(mlua::Error::runtime("file.write: the callback is the fourth argument; pass `nil` for opts before it"));
            }
            let mut in_place = false;
            if let Some(opts) = opts.0.map(|opts| Table::from_lua(opts, lua)).transpose()? {
                super::marshal::only_keys(&opts, &["in_place"])
                    .map_err(|detail| mlua::Error::runtime(format!("file.write: opts: {detail}")))?;
                in_place = match opts.get::<Value>("in_place")? {
                    Value::Nil => false,
                    Value::Boolean(on) => on,
                    other => return Err(mlua::Error::runtime(format!("file.write: opts.in_place must be a boolean, got {}", other.type_name()))),
                };
            }
            Ok(registry.spawn(true, on_done.0, move || {
                Outcome::Write(write(&path, &text, in_place).inspect_err(|err| warn!("file.write({}): {err}", path.display())))
            }))
        }
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::super::calls::poll_until;
    use super::*;

    fn lua_with_file() -> (Lua, FileRegistry) {
        let lua = Lua::new();
        let registry = FileRegistry::new(None);
        register(&lua, registry.clone()).unwrap();
        (lua, registry)
    }

    /// Runs `code` (which must set `done = true` in its callback) and returns once it fires.
    fn run(lua: &Lua, registry: &FileRegistry, code: &str) {
        lua.load(code).exec().unwrap();
        poll_until(|| registry.poll(), || lua.load("return done == true").eval().unwrap());
    }

    #[test]
    fn read_round_trips_and_a_missing_or_binary_file_errs() {
        let (lua, registry) = lua_with_file();
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("a.txt");
        std::fs::write(&good, "héllo\n").unwrap();
        std::fs::write(dir.path().join("bin"), [0xff, 0xfe]).unwrap();
        let d = dir.path().display();
        run(&lua, &registry, &format!(r#"file.read("{d}/a.txt", function(t, e) text, done = t, true end)"#));
        assert_eq!(lua.load("return text").eval::<String>().unwrap(), "héllo\n");
        for name in ["missing", "bin"] {
            run(
                &lua,
                &registry,
                &format!(r#"done = false; file.read("{d}/{name}", function(t, e) text, err, done = t, e, true end)"#),
            );
            let (text, err): (Option<String>, Option<String>) = lua.load("return text, err").eval().unwrap();
            assert!(text.is_none() && err.is_some(), "{name}: {text:?} {err:?}");
        }
    }

    #[test]
    fn a_read_over_the_cap_errs() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::File::create(&big).unwrap().set_len(READ_CAP + 1).unwrap();
        assert!(read(&big).unwrap_err().contains("MiB"));
    }

    #[test]
    fn write_creates_parents_skips_equal_bytes_and_replaces_changed_ones() {
        let (lua, registry) = lua_with_file();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("deep/er/out.txt");
        let call = |text: &str| {
            run(
                &lua,
                &registry,
                &format!(
                    r#"done = false; file.write("{}", "{text}", nil, function(c, e) changed, err, done = c, e, true end)"#,
                    target.display()
                ),
            );
            lua.load("return changed").eval::<Option<bool>>().unwrap()
        };
        assert_eq!(call("one"), Some(true));
        let before = std::fs::metadata(&target).unwrap().ino();
        assert_eq!(call("one"), Some(false));
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), before, "equal bytes must not replace the file");
        assert_eq!(call("two"), Some(true));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "two");
        let leftovers = std::fs::read_dir(target.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "the temporary must be renamed away");
    }

    #[test]
    fn in_place_keeps_the_inode_and_a_symlink_while_the_default_replaces_it() {
        let (lua, registry) = lua_with_file();
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&real, "a").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let call = |path: &std::path::Path, opts: &str, text: &str| {
            run(
                &lua,
                &registry,
                &format!(
                    r#"done = false; file.write("{}", "{text}", {opts}, function() done = true end)"#,
                    path.display()
                ),
            );
        };
        let ino = || std::fs::metadata(&real).unwrap().ino();
        let before = ino();
        call(&real, "{ in_place = true }", "b");
        assert_eq!((std::fs::read_to_string(&real).unwrap(), ino()), ("b".into(), before));
        call(&link, "{ in_place = true }", "c");
        assert_eq!((std::fs::read_to_string(&real).unwrap(), ino()), ("c".into(), before));
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        call(&real, "nil", "d");
        assert_ne!(ino(), before, "the default must replace the inode");
        call(&dir.path().join("new/x"), "{ in_place = true }", "e");
        assert_eq!(std::fs::read_to_string(dir.path().join("new/x")).unwrap(), "e");
    }

    #[test]
    fn in_place_onto_a_fifo_errs_instead_of_blocking_the_writer() {
        let (lua, registry) = lua_with_file();
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let call = |opts: &str| {
            run(
                &lua,
                &registry,
                &format!(
                    r#"done = false; file.write("{}", "x", {opts}, function(c, e) err, done = e, true end)"#,
                    fifo.display()
                ),
            );
            lua.load("return err").eval::<Option<String>>().unwrap()
        };
        assert!(call("{ in_place = true }").unwrap().contains("regular file"));
        assert_eq!(call("nil"), None);
        assert_eq!(std::fs::read_to_string(&fifo).unwrap(), "x", "the default replaces the FIFO with a regular file");
    }

    #[test]
    fn bad_opts_raise_at_the_call_and_write_nothing() {
        let (lua, _) = lua_with_file();
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        for (opts, needle) in [
            ("function() end", "pass `nil`"),
            ("{ inplace = true }", "unknown key `inplace`"),
            ("3", "to table"),
            ("{ in_place = 1 }", "must be a boolean"),
            (r#"{ in_place = "false" }"#, "must be a boolean"),
        ] {
            let err = lua.load(format!(r#"file.write("{d}/out", "x", {opts})"#)).exec().unwrap_err().to_string();
            assert!(err.contains(needle), "{opts}: {err}");
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn clear_drops_the_callback_but_the_write_lands() {
        let (lua, registry) = lua_with_file();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        lua.load(format!(
            r#"fired = false; file.write("{}", "x", nil, function() fired = true end)"#,
            target.display()
        ))
        .exec()
        .unwrap();
        registry.calls.replay(|| registry.clear());
        registry.poll();
        assert!(!lua.load("return fired").eval::<bool>().unwrap());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }

    #[test]
    fn writes_to_one_path_land_in_call_order() {
        let (lua, registry) = lua_with_file();
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        run(
            &lua,
            &registry,
            &format!(
                r#"done, left = false, 40
                for i = 1, 40 do
                    local p = "{d}/f" .. i
                    file.write(p, "first")
                    file.write(p, "second", nil, function() left = left - 1; done = left == 0 end)
                end"#
            ),
        );
        for i in 1..=40 {
            assert_eq!(std::fs::read_to_string(dir.path().join(format!("f{i}"))).unwrap(), "second");
        }
    }

    #[test]
    fn a_checking_registry_writes_nothing() {
        let lua = Lua::new();
        register(&lua, FileRegistry::checking()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        lua.load(format!(r#"file.write("{}/out", "x")"#, dir.path().display())).exec().unwrap();
        assert!(!dir.path().join("out").exists());
    }

    #[test]
    fn a_relative_path_raises_at_the_call() {
        let (lua, _) = lua_with_file();
        for code in [r#"file.read("rel/x", function() end)"#, r#"file.write("rel/x", "t")"#] {
            let err = lua.load(code).exec().unwrap_err().to_string();
            assert!(err.contains("path must be absolute or start with ~/"), "{err}");
        }
    }
}

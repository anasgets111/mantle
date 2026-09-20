//! Levelled, stamped diagnostics for both binaries (ADR-0199, amended).
//!
//! The level macros beside [`crate::eprintln!`] expand here carrying the caller's `module_path!()`,
//! so a line's subsystem is the module it was written in rather than a prefix every call site
//! repeats. Nothing reads the log to add this: `emit` runs in the process that had the thought, so a
//! terminal run and a detached one are stamped by the same code.

use std::fmt::{Arguments, Write as _};
use std::io::Write as _;
use std::sync::OnceLock;

/// Names the level a line was written at. `MANTLE_LOG` spells these, plus `off`. `Debug` carries a
/// verbosity (1-3, `-v`/`-vv`/`-vvv`-style): `debug!(2; ...)` writes at `Debug(2)`, and a threshold
/// of `debug2` admits `Debug(1)` and `Debug(2)` but not `Debug(3)`.
///
/// Ordered least to most frequent, so a threshold admits everything at or below it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug(u8),
}

impl Level {
    /// Padded, so the subsystem column does not move between levels. The verbosity does not widen
    /// it: it is a filter, not something a reader needs to see per line.
    fn name(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug(_) => "DEBUG",
        }
    }

    fn colour(self) -> &'static str {
        match self {
            Self::Error => "\x1b[31m",
            Self::Warn => "\x1b[33m",
            Self::Info => "\x1b[32m",
            Self::Debug(_) => "\x1b[2m",
        }
    }
}

/// One `MANTLE_LOG` level name. The outer `None` is "not a level at all", the inner one is `off`.
fn threshold(text: &str) -> Option<Option<Level>> {
    match text.trim() {
        "off" => Some(None),
        "error" => Some(Some(Level::Error)),
        "warn" => Some(Some(Level::Warn)),
        "info" => Some(Some(Level::Info)),
        "debug" => Some(Some(Level::Debug(1))),
        other => Some(Some(Level::Debug(match other.strip_prefix("debug")?.parse().ok()? {
            n @ 1..=3 => n,
            _ => return None,
        }))),
    }
}

/// Everything [`emit`] needs, resolved once so no line pays for an env read.
struct Config {
    /// Prepended to the subsystem, to tell two processes writing one log apart. Empty for the
    /// Supervisor, which is the majority of lines and needs no marking.
    tag: &'static str,
    default: Option<Level>,
    overrides: Vec<(String, Option<Level>)>,
}

impl Config {
    fn threshold(&self, target: &str) -> Option<Level> {
        self.overrides.iter().find(|(name, _)| name == target).map_or(self.default, |(_, level)| *level)
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// `-v` count to default level: nothing without a flag but `Error`, `-v` adds `Warn`/`Info`
/// together, `-vv`/`-vvv` step through `Debug`'s own verbosity, `-vvvv` and past it hold at
/// `Debug(3)`. `MANTLE_LOG`'s own bare level, if set, still overrides this.
fn verbosity_level(count: u8) -> Level {
    match count {
        0 => Level::Error,
        1 => Level::Info,
        2 => Level::Debug(1),
        3 => Level::Debug(2),
        _ => Level::Debug(3),
    }
}

/// Resolves the filter and `tag`, then routes panics through the same format.
///
/// Called before anything else in `main`. A diagnostic that beats it still prints, unstamped; see
/// [`write_to`]. `verbose` is the Supervisor's own `-v` count; the Renderer has no argv of its own
/// to parse one from, so it reads what the Supervisor forwarded through [`crate::VERBOSE_ENV`] at
/// spawn (ADR-0243) and passes that instead.
pub fn init(tag: &'static str, verbose: u8) {
    let raw = std::env::var("MANTLE_LOG").unwrap_or_default();
    let (mut default, mut overrides, mut rejected) = (Some(verbosity_level(verbose)), Vec::new(), Vec::new());
    for item in raw.split(',').map(str::trim).filter(|item| !item.is_empty()) {
        match item.split_once('=') {
            Some((name, level)) => match threshold(level) {
                Some(level) => overrides.push((name.trim().to_string(), level)),
                None => rejected.push(item.to_string()),
            },
            None => match threshold(item) {
                Some(level) => default = level,
                None => rejected.push(item.to_string()),
            },
        }
    }

    let _ = CONFIG.set(Config { tag, default, overrides });

    if !rejected.is_empty() {
        // Named rather than dropped: a typo in `MANTLE_LOG` otherwise looks like a subsystem that
        // has gone quiet.
        write_to(Level::Warn, "log", format_args!("MANTLE_LOG: ignoring {}", rejected.join(", ")));
    }

    std::panic::set_hook(Box::new(|panic| {
        write_to(Level::Error, "panic", format_args!("{panic}"));
        if std::env::var_os("RUST_BACKTRACE").is_some_and(|value| value != "0") {
            write_to(Level::Error, "panic", format_args!("{}", std::backtrace::Backtrace::force_capture()));
        }
    }));
}

/// The level macros' entry point: takes the caller's `module_path!()` and names its subsystem.
pub fn emit(level: Level, module: &str, args: Arguments<'_>) {
    write_to(level, subsystem(module), args);
}

/// Filters, formats and writes one line, or writes nothing.
fn write_to(level: Level, target: &str, args: Arguments<'_>) {
    let Some(config) = CONFIG.get() else {
        // Before `init`, print unfiltered and unstamped rather than drop it: a diagnostic from
        // startup is the one least able to wait for the config that would have formatted it.
        let _ = writeln!(std::io::stderr(), "{target}: {args}");
        return;
    };
    if config.threshold(target).is_none_or(|threshold| level > threshold) {
        return;
    }
    let line = format_line(config.tag, level, target, args);
    // One `write_all` of the whole line: two processes share this descriptor, and a line assembled
    // in several writes is a line the other one can cut in half.
    let _ = std::io::stderr().write_all(line.as_bytes());
}

/// Split from [`write_to`] so the format is testable without owning the process-wide [`CONFIG`].
///
/// Always plain: `capture` leaves stderr on the log or a pipe onto it (ADR-0199), so nothing this
/// writes is ever going straight to a terminal. Colour is [`colourise`]'s, which is how `mantle log`
/// reaches it: the file holds these bytes, and whoever prints them to a terminal paints them.
fn format_line(tag: &str, level: Level, target: &str, args: Arguments<'_>) -> String {
    let mut line = String::with_capacity(96);
    let mut clock_buf = [0u8; 8];
    let _ = write!(line, "{} {} ", clock(&mut clock_buf), level.name());
    if !tag.is_empty() {
        let _ = write!(line, "{tag}/");
    }
    let _ = writeln!(line, "{target}: {args}");
    line
}

/// Paints one line of [`format_line`]'s output: dim clock, the level in its own colour, cyan
/// subsystem.
///
/// Parses the line back rather than taking the parts, because the other caller is `mantle log`
/// reading a finished file. Anything that does not match the shape is returned untouched, which
/// covers a pre-`init` line and the second and later lines of a multi-line message.
pub fn colourise(line: &str) -> String {
    let Some((clock, rest)) = line.split_once(' ') else { return line.to_string() };
    // Taken by width, not to the next space: every `Level::name` is padded to five.
    let (Some(name), Some(rest)) = (rest.get(..5), rest.get(5..)) else { return line.to_string() };
    // The verbosity is irrelevant here: `colour` and `name` don't read it, and the line itself
    // doesn't carry it back.
    let Some(level) = [Level::Error, Level::Warn, Level::Info, Level::Debug(1)].into_iter().find(|l| l.name() == name)
    else {
        return line.to_string();
    };
    let Some((target, message)) = rest.trim_start_matches(' ').split_once(": ") else { return line.to_string() };
    format!("\x1b[2m{clock}\x1b[0m {}{name}\x1b[0m \x1b[36m{target}\x1b[0m: {message}", level.colour())
}

/// The subsystem a module belongs to: the capability under `capabilities::`, else whatever sits
/// directly under the crate root.
///
/// Public for the guard in `shared/tests` that holds messages to it, so the rule and the name it is
/// checked against cannot drift apart.
pub fn subsystem(module: &str) -> &str {
    match module.split_once("capabilities::") {
        Some((_, rest)) => rest.split("::").next().unwrap_or(rest),
        None => {
            let mut parts = module.splitn(3, "::");
            let root = parts.next().unwrap_or(module);
            parts.next().unwrap_or(root)
        }
    }
}

/// The local wall clock as `HH:MM:SS`, to be read beside `date`'s output and journalctl's.
///
/// Per line rather than an offset resolved once, so a session running across a DST change keeps
/// telling the truth.
fn clock(buf: &mut [u8; 8]) -> &str {
    let now = std::time::SystemTime::now();
    let secs = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as libc::time_t;
    // SAFETY: `libc::tm` is a C struct of integers and pointers, so an all-zero value is a valid one
    // to hand `localtime_r`, which overwrites it. The reentrant form writes only there and shares no
    // static buffer, and the result is read only once the null check has passed.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return "--:--:--";
        }
        tm
    };
    if write!(&mut buf[..], "{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec).is_err() {
        return "--:--:--";
    }
    std::str::from_utf8(buf).unwrap_or("--:--:--")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(tag: &'static str, raw: &str) -> Config {
        let (mut default, mut overrides) = (Some(Level::Info), Vec::new());
        for item in raw.split(',').map(str::trim).filter(|item| !item.is_empty()) {
            match item.split_once('=') {
                Some((name, level)) => overrides.push((name.trim().to_string(), threshold(level).unwrap())),
                None => default = threshold(item).unwrap(),
            }
        }
        Config { tag, default, overrides }
    }

    #[test]
    fn a_named_subsystem_overrides_the_default_level_without_moving_any_other() {
        let config = config("", "warn,tray=debug,network=off");

        assert_eq!(config.threshold("tray"), Some(Level::Debug(1)), "the named subsystem is raised");
        assert_eq!(config.threshold("network"), None, "`off` silences one subsystem outright");
        assert_eq!(config.threshold("audio"), Some(Level::Warn), "everything unnamed takes the default");
    }

    #[test]
    fn a_level_is_admitted_when_it_is_no_more_frequent_than_the_threshold() {
        let config = config("", "warn");

        assert!(Level::Error <= config.threshold("audio").unwrap(), "a rarer level than the threshold prints");
        assert!(Level::Warn <= config.threshold("audio").unwrap(), "the threshold itself prints");
        assert!(Level::Info > config.threshold("audio").unwrap(), "a more frequent one does not");
    }

    #[test]
    fn a_v_count_climbs_one_level_at_a_time_then_holds_at_the_loudest_debug() {
        assert_eq!(verbosity_level(0), Level::Error, "no -v: only what would break something");
        assert_eq!(verbosity_level(1), Level::Info, "one -v: warn and info both, in one step");
        assert_eq!(verbosity_level(2), Level::Debug(1));
        assert_eq!(verbosity_level(3), Level::Debug(2));
        assert_eq!(verbosity_level(4), Level::Debug(3));
        assert_eq!(verbosity_level(9), Level::Debug(3), "past 4, it holds rather than erroring");
    }

    #[test]
    fn debug_verbosity_admits_its_own_number_and_everything_quieter() {
        let config = config("", "debug2");

        assert!(Level::Debug(1) <= config.threshold("audio").unwrap(), "a quieter debug level prints");
        assert!(Level::Debug(2) <= config.threshold("audio").unwrap(), "the threshold itself prints");
        assert!(Level::Debug(3) > config.threshold("audio").unwrap(), "a louder debug level does not");
    }

    #[test]
    fn a_debug_verbosity_outside_one_to_three_is_rejected_like_a_typo() {
        assert_eq!(threshold("debug0"), None, "0 is not a verbosity");
        assert_eq!(threshold("debug4"), None, "verbosity tops out at 3, like -vvv");
        assert_eq!(threshold("debugger"), None, "not a number at all");
    }

    #[test]
    fn a_module_is_named_by_its_capability_or_by_what_sits_under_the_crate_root() {
        assert_eq!(subsystem("supervisor::capabilities::tray::controller"), "tray");
        assert_eq!(subsystem("supervisor::capabilities::updates::pacman::check"), "updates");
        assert_eq!(subsystem("renderer::wayland::surface"), "wayland");
        assert_eq!(subsystem("supervisor::watcher"), "watcher");
        assert_eq!(subsystem("renderer"), "renderer", "a crate root has nothing under it to name");
    }

    #[test]
    fn a_line_written_somewhere_that_is_not_a_terminal_carries_no_escape_sequence() {
        let line = format_line("", Level::Warn, "tray", format_args!("RequestName failed"));

        assert!(!line.contains('\x1b'), "the log file has to stay greppable: {line:?}");
        assert!(line.ends_with(" WARN  tray: RequestName failed\n"), "{line:?}");
    }

    #[test]
    fn a_tagged_process_names_itself_before_the_subsystem_so_one_log_can_hold_both() {
        let line = format_line("renderer", Level::Info, "image", format_args!("decoded"));

        assert!(line.ends_with(" INFO  renderer/image: decoded\n"), "{line:?}");
    }

    #[test]
    fn painting_a_line_keeps_every_word_of_it_and_leaves_anything_else_alone() {
        let plain = format_line("renderer", Level::Warn, "wayland", format_args!("bind failed: {}", 7));
        let painted = colourise(&plain);

        assert!(painted.contains("WARN "), "the level survives, since the paint is what parses it back");
        assert!(painted.ends_with("renderer/wayland\x1b[0m: bind failed: 7\n"), "{painted:?}");
        assert!(painted.starts_with("\x1b[2m"), "the clock is dimmed: {painted:?}");

        // `mantle log` paints a whole file, which holds pre-`init` lines and the tail of multi-line
        // messages. Neither has the shape, and mangling them would be worse than leaving them grey.
        for pass_through in ["tray: something before init\n", "    at src/main.rs:1\n", "\n"] {
            assert_eq!(colourise(pass_through), pass_through, "an unshaped line is not touched");
        }
    }

    #[test]
    fn the_clock_reads_as_a_wall_clock_time() {
        let mut buf = [0u8; 8];
        let clock = clock(&mut buf);

        let parts: Vec<&str> = clock.split(':').collect();
        assert_eq!(parts.len(), 3, "{clock:?}");
        assert!(parts.iter().all(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_digit())), "{clock:?}");
    }
}

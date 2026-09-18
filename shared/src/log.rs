//! Levelled, stamped diagnostics for both binaries (ADR-0199, amended).
//!
//! The level macros beside [`crate::eprintln!`] expand here carrying the caller's `module_path!()`,
//! so a line's subsystem is the module it was written in rather than a prefix every call site
//! repeats. Nothing reads the log to add this: `emit` runs in the process that had the thought, so a
//! terminal run and a detached one are stamped by the same code.

use std::fmt::{Arguments, Write as _};
use std::io::{IsTerminal, Write as _};
use std::sync::OnceLock;

/// Names the level a line was written at. `OBELISK_LOG` spells these, plus `off`.
///
/// Ordered least to most frequent, so a threshold admits everything at or below it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    /// Padded, so the subsystem column does not move between levels.
    fn name(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
        }
    }

    fn colour(self) -> &'static str {
        match self {
            Self::Error => "\x1b[31m",
            Self::Warn => "\x1b[33m",
            Self::Info => "\x1b[32m",
            Self::Debug => "\x1b[2m",
        }
    }
}

/// One `OBELISK_LOG` level name. The outer `None` is "not a level at all", the inner one is `off`.
fn threshold(text: &str) -> Option<Option<Level>> {
    Some(match text.trim() {
        "off" => None,
        "error" => Some(Level::Error),
        "warn" => Some(Level::Warn),
        "info" => Some(Level::Info),
        "debug" => Some(Level::Debug),
        _ => return None,
    })
}

/// Everything [`emit`] needs, resolved once so no line pays for an env read or an `isatty`.
struct Config {
    /// Prepended to the subsystem, to tell two processes writing one log apart. Empty for the
    /// Supervisor, which is the majority of lines and needs no marking.
    tag: &'static str,
    colour: bool,
    default: Option<Level>,
    overrides: Vec<(String, Option<Level>)>,
}

impl Config {
    fn threshold(&self, target: &str) -> Option<Level> {
        self.overrides.iter().find(|(name, _)| name == target).map_or(self.default, |(_, level)| *level)
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Resolves the filter, the colour rule and `tag`, then routes panics through the same format.
///
/// Called before anything else in `main`. A diagnostic that beats it still prints, unstamped; see
/// [`write_to`].
pub fn init(tag: &'static str) {
    let raw = std::env::var("OBELISK_LOG").unwrap_or_default();
    let (mut default, mut overrides, mut rejected) = (Some(Level::Info), Vec::new(), Vec::new());
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

    // `colour` asks about stderr alone. A detached run's stderr is the log file (ADR-0199), so the
    // file holds plain bytes without the question being asked twice.
    let colour = std::io::stderr().is_terminal();
    let _ = CONFIG.set(Config { tag, colour, default, overrides });

    if !rejected.is_empty() {
        // Named rather than dropped: a typo in `OBELISK_LOG` otherwise looks like a subsystem that
        // has gone quiet.
        write_to(Level::Warn, "log", format_args!("OBELISK_LOG: ignoring {}", rejected.join(", ")));
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
    // One `write_all` of the whole line: two processes share this descriptor, and a line assembled
    // in several writes is a line the other one can cut in half.
    let _ = std::io::stderr().write_all(format_line(config, level, target, args).as_bytes());
}

/// Split from [`write_to`] so the format is testable without owning the process-wide [`CONFIG`].
fn format_line(config: &Config, level: Level, target: &str, args: Arguments<'_>) -> String {
    let [dim, reset, cyan] = if config.colour { ["\x1b[2m", "\x1b[0m", "\x1b[36m"] } else { [""; 3] };
    let level_colour = if config.colour { level.colour() } else { "" };

    let mut line = String::with_capacity(96);
    let _ = write!(line, "{dim}{}{reset} {level_colour}{}{reset} {cyan}", clock(), level.name());
    if !config.tag.is_empty() {
        let _ = write!(line, "{}/", config.tag);
    }
    let _ = writeln!(line, "{target}{reset}: {args}");
    line
}

/// The subsystem a module belongs to: the capability under `capabilities::`, else whatever sits
/// directly under the crate root.
fn subsystem(module: &str) -> &str {
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
fn clock() -> String {
    let now = std::time::SystemTime::now();
    let secs = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as libc::time_t;
    // SAFETY: `libc::tm` is a C struct of integers and pointers, so an all-zero value is a valid one
    // to hand `localtime_r`, which overwrites it. The reentrant form writes only there and shares no
    // static buffer, and the result is read only once the null check has passed.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return "--:--:--".to_string();
        }
        tm
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
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
        Config { tag, colour: false, default, overrides }
    }

    #[test]
    fn a_named_subsystem_overrides_the_default_level_without_moving_any_other() {
        let config = config("", "warn,tray=debug,network=off");

        assert_eq!(config.threshold("tray"), Some(Level::Debug), "the named subsystem is raised");
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
    fn a_module_is_named_by_its_capability_or_by_what_sits_under_the_crate_root() {
        assert_eq!(subsystem("supervisor::capabilities::tray::controller"), "tray");
        assert_eq!(subsystem("supervisor::capabilities::updates::pacman::check"), "updates");
        assert_eq!(subsystem("renderer::wayland::surface"), "wayland");
        assert_eq!(subsystem("supervisor::watcher"), "watcher");
        assert_eq!(subsystem("renderer"), "renderer", "a crate root has nothing under it to name");
    }

    #[test]
    fn a_line_written_somewhere_that_is_not_a_terminal_carries_no_escape_sequence() {
        let line = format_line(&config("", ""), Level::Warn, "tray", format_args!("RequestName failed"));

        assert!(!line.contains('\x1b'), "the log file has to stay greppable: {line:?}");
        assert!(line.ends_with(" WARN  tray: RequestName failed\n"), "{line:?}");
    }

    #[test]
    fn a_tagged_process_names_itself_before_the_subsystem_so_one_log_can_hold_both() {
        let line = format_line(&config("renderer", ""), Level::Info, "image", format_args!("decoded"));

        assert!(line.ends_with(" INFO  renderer/image: decoded\n"), "{line:?}");
    }

    #[test]
    fn the_clock_reads_as_a_wall_clock_time() {
        let clock = clock();

        let parts: Vec<&str> = clock.split(':').collect();
        assert_eq!(parts.len(), 3, "{clock:?}");
        assert!(parts.iter().all(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_digit())), "{clock:?}");
    }
}

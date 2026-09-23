//! Argument parsing for the `mantle` binary.
//!
//! Hand-rolled: a handful of flags, a handful of subcommands, and one non-obvious rule (`-c` may
//! name a file). `clap` would be the workspace's largest dependency; the rule needs custom code
//! either way.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum Command {
    /// Start the shell. The default with no arguments.
    Run,
    /// Write `.luarc.json` and the language-server stubs into the config directory.
    Init {
        force: bool,
    },
    /// Evaluate the config and report what it says, without taking a Wayland surface.
    Check,
    /// `set <name> <value>`, `toggle <name>` or `toggle <name> <value>` writes a running config's
    /// `state` signal from outside for a compositor keybind (ADR-0112).
    SetState(shared::SetState),
    /// `call <name> [args...]` runs a config's `action(name, fn)` and prints what it returned
    /// (ADR-0197). `name` is one opaque string: `rec.toggle` groups for a reader, and nothing
    /// splits on the dot.
    Call {
        name: String,
        arguments: Vec<serde_json::Value>,
    },
    /// `log [-f]` prints what a run wrote to stdout and stderr, which a shell with no terminal
    /// parks in a file (ADR-0199).
    Log {
        follow: bool,
    },
    /// `list` prints the running Supervisors (ADR-0222).
    List,
    Version,
    Help,
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub command: Command,
    /// Absolute `-c` directory. `None` leaves `shared::config_dir()`'s own order in charge.
    pub config_dir: Option<PathBuf>,
    /// `-d`: re-exec detached and give the caller's shell its prompt back.
    pub detach: bool,
    /// `--profile[=SECS]`: seconds between profile reports.
    pub profile: Option<u64>,
    /// `--pid`: the Supervisor `set`, `toggle`, `call` and `log` address.
    pub pid: Option<u32>,
    /// `-v`'s count, `-vv` and repetition both counted (ADR-0243).
    pub verbose: u8,
}

pub const HELP: &str = "\
mantle -- a Wayland desktop shell configured in Lua

USAGE:
    mantle [OPTIONS]            start the shell
    mantle init [OPTIONS]       set up a config directory for editing
    mantle check [OPTIONS]      evaluate the config and exit
    mantle set <NAME> <VALUE>   write the running config's state(NAME) signal
    mantle toggle <NAME>        flip it, when it holds a boolean
    mantle toggle <NAME> <VALUE>
                                set it to VALUE, or back to its declared
                                initial when it already is VALUE
    mantle call <NAME> [ARGS]   run the config's action(NAME) and print what
                                 it returned
    mantle log [-f]             print the shell's stdout and stderr
    mantle list                 show running shells: PID UPTIME DIR CONFIG

OPTIONS:
    -c, --config <DIR>   the config directory, holding shell.lua. Overrides
                         $MANTLE_CONFIG_DIR and $XDG_CONFIG_HOME.
    -d, --detach         run only: start the shell in its own session and
                         return, sending its output to `mantle log`
        --force          init only: overwrite files that already exist
    -f, --follow         log only: keep printing until the shell exits
        --pid <PID>      set, toggle, call and log: the shell `list` shows,
                         not with -c
        --profile[=SECS] run only: log idle, heap and PSS/GPU reports every
                         SECS seconds, 60 by default. Implies -v, which is
                         the level the reports print at
    -v, --verbose        run only: repeat to raise the log level. None:
                         Error, Warn, and start/reload/respawn/stop. -v:
                         also Info. -vv/-vvv: Debug, itself levelled; past
                         -vvv it holds. Overridden by a MANTLE_LOG default
                         level. The config's own log.* always prints.
    -V, --version
    -h, --help

The config is a directory, not a file: `require` resolves inside it, and the
shell reloads when any .lua file in it changes.

`set` and `toggle` are how a compositor keybind reaches a running config:
bind `mantle toggle launcher_open` and the config's `state(\"launcher_open\",
false)` flips; bind `mantle toggle modal launcher` and `state(\"modal\", \"\")`
becomes \"launcher\", or \"\" again when it already was. VALUE is read as JSON
(true, 3, \"text\", [1,2]); anything that is not JSON is taken as a string, so
quoting `notifications` is optional.

`log` prints a shell's log from its runtime directory, and `-f` keeps reading
until that shell exits. Every run writes it; a terminal, a redirect or a pipe
also gets a copy.
`-d` starts a shell that way deliberately and prints its pid, so `mantle -d`
then `mantle log -f` runs one from a terminal without tying the terminal up.

`call` is for what a keybind wants the shell to *do* rather than look like:
the config declares `action(\"rec.toggle\", function() ... end)` and the bind is
`mantle call rec.toggle`. NAME is one opaque string -- the dot groups it for a
reader, nothing splits on it. Arguments are read as JSON like VALUE above. It
waits for the answer, prints it, and exits non-zero when the action failed or
does not exist.
";

/// `-c` names a directory, but accepts a path to `shell.lua` because that is what someone reaches
/// for after editing it. Report the substitution: `require` and the watcher use the directory.
fn config_dir_from(raw: &str) -> Result<PathBuf, String> {
    let given = Path::new(raw);
    let dir = if given.is_file() {
        let parent = given
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| format!("--config {raw} is a file with no parent directory"))?;
        eprintln!("mantle: --config takes a directory; using {} because {raw} is a file", parent.display());
        parent.to_path_buf()
    } else {
        given.to_path_buf()
    };
    // Resolve before handing it to Renderer through the environment; spawned processes need not
    // share this process's working directory.
    std::path::absolute(&dir).map_err(|err| format!("--config {}: {err}", dir.display()))
}

/// `-v`'s count: `--verbose` is 1, `-v` is 1, and `-vv`/`-vvv` (grouped, the common shape) count
/// their own `v`s. `None` for anything else, `-` bare included.
fn verbose_count(arg: &str) -> Option<u8> {
    if arg == "--verbose" {
        return Some(1);
    }
    let vees = arg.strip_prefix('-')?;
    (!vees.is_empty() && vees.bytes().all(|b| b == b'v')).then_some(vees.len() as u8)
}

/// Whether `arg` is one of the options this parser knows, rather than a value that merely begins
/// with a dash. `-1` is a `set` value; `-c` is an option even where a value is expected.
fn is_option(arg: &str) -> bool {
    matches!(
        arg,
        "-c" | "--config" | "-d" | "--detach" | "--force" | "-f" | "--follow" | "-V" | "--version" | "-h" | "--help"
    ) || arg == "--pid"
        || arg.starts_with("--config=")
        || arg.starts_with("--profile")
        || verbose_count(arg).is_some()
}

pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> Result<Args, String> {
    let mut args = argv.into_iter().skip(1);
    let mut command = None;
    let mut config_dir = None;
    let mut force = false;
    let mut follow = false;
    let mut detach = false;
    let mut profile = None;
    let mut pid = None;
    let mut verbose: u8 = 0;
    let mut positional = Vec::new();

    while let Some(arg) = args.next() {
        if let Some(count) = verbose_count(&arg) {
            verbose = verbose.saturating_add(count);
            continue;
        }
        match arg.as_str() {
            "init" | "check" | "set" | "toggle" | "call" | "log" | "list" if command.is_none() => {
                command = Some(match arg.as_str() {
                    "init" => "init",
                    "check" => "check",
                    "set" => "set",
                    "call" => "call",
                    "log" => "log",
                    "list" => "list",
                    _ => "toggle",
                });
            }
            // Take the state name and `set` value before flags; a value may begin with a dash
            // (`-1`). An option the parser knows is still an option in that slot, or
            // `mantle toggle open -c /dir` would store the flag as the value and then choke on the
            // directory.
            _ if matches!(command, Some("set" | "toggle")) && positional.len() < 2 && !is_option(&arg) => {
                positional.push(arg);
            }
            // `call` takes a name and however many arguments the action declares, so no two-slot
            // cap. A JSON argument beginning with a dash is still a value, as above.
            _ if matches!(command, Some("call")) && !is_option(&arg) => {
                positional.push(arg);
            }
            "-c" | "--config" => {
                let value = args.next().ok_or_else(|| "--config needs a directory".to_string())?;
                config_dir = Some(config_dir_from(&value)?);
            }
            "-d" | "--detach" => detach = true,
            // `detach_self`'s marker for the child it re-execs; it runs in the foreground.
            "--detached" => {}
            "--force" => force = true,
            "-f" | "--follow" => follow = true,
            "--profile" => profile = Some(60),
            "--pid" => pid = Some(pid_from(args.next().as_deref().unwrap_or_default())?),
            "-V" | "--version" => {
                return Ok(Args { command: Command::Version, config_dir, detach, profile, pid, verbose });
            }
            "-h" | "--help" => {
                return Ok(Args { command: Command::Help, config_dir, detach, profile, pid, verbose });
            }
            other => {
                if let Some(value) = other.strip_prefix("--config=") {
                    config_dir = Some(config_dir_from(value)?);
                } else if let Some(value) = other.strip_prefix("--pid=") {
                    pid = Some(pid_from(value)?);
                } else if let Some(value) = other.strip_prefix("--profile=") {
                    let secs = value.parse::<u64>().ok().filter(|secs| *secs > 0);
                    profile =
                        Some(secs.ok_or_else(|| format!("--profile takes a positive number of seconds, got {value}"))?);
                } else {
                    return Err(format!("unknown argument {other}"));
                }
            }
        }
    }

    let command = match command {
        Some("init") => Command::Init { force },
        Some("check") => Command::Check,
        Some("set") => {
            let [name, value] = <[String; 2]>::try_from(positional)
                .map_err(|_| "set takes a state name and a value: `mantle set launcher_open true`".to_string())?;
            // Parse JSON when possible; bare words stay strings, so keybinds need no extra quotes.
            let value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
            Command::SetState(shared::SetState { name, write: shared::StateWrite::Set(value) })
        }
        Some("toggle") => {
            let mut positional = positional.into_iter();
            let name = positional
                .next()
                .ok_or_else(|| "toggle takes a state name: `mantle toggle launcher_open`".to_string())?;
            let write = match positional.next() {
                // The same reading as `set`: JSON when it parses, a string otherwise.
                Some(value) => shared::StateWrite::ToggleTo(
                    serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value)),
                ),
                None => shared::StateWrite::Toggle,
            };
            Command::SetState(shared::SetState { name, write })
        }
        Some("call") => {
            let mut positional = positional.into_iter();
            let name =
                positional.next().ok_or_else(|| "call takes an action name: `mantle call rec.toggle`".to_string())?;
            // The same reading as `set`: JSON when it parses, a string otherwise, so a keybind
            // passing a word needs no shell quoting.
            let arguments =
                positional.map(|arg| serde_json::from_str(&arg).unwrap_or(serde_json::Value::String(arg))).collect();
            Command::Call { name, arguments }
        }
        Some("log") => Command::Log { follow },
        Some("list") => Command::List,
        _ => Command::Run,
    };
    if force && !matches!(command, Command::Init { .. }) {
        return Err("--force is only meaningful with `init`".to_string());
    }
    if follow && !matches!(command, Command::Log { .. }) {
        return Err("--follow is only meaningful with `log`".to_string());
    }
    if detach && !matches!(command, Command::Run) {
        return Err("--detach is only meaningful when starting the shell".to_string());
    }
    if profile.is_some() && !matches!(command, Command::Run) {
        return Err("--profile is only meaningful when starting the shell".to_string());
    }
    if verbose > 0 && !matches!(command, Command::Run) {
        return Err("-v is only meaningful when starting the shell".to_string());
    }
    if pid.is_some() && !matches!(command, Command::SetState(_) | Command::Call { .. } | Command::Log { .. }) {
        return Err("--pid is only meaningful with `set`, `toggle`, `call` and `log`".to_string());
    }
    if config_dir.is_some() && command == Command::List {
        return Err("`list` shows every config's shells".to_string());
    }
    if pid.is_some() && config_dir.is_some() {
        return Err("--pid names one shell; drop -c".to_string());
    }
    // The reports print at Info, and asking for them is the whole point of the flag: without this
    // `--profile` alone logs that profiling is on and then nothing at all. A louder `-v` still wins.
    if profile.is_some() {
        verbose = verbose.max(1);
    }
    Ok(Args { command, config_dir, detach, profile, pid, verbose })
}

fn pid_from(raw: &str) -> Result<u32, String> {
    raw.parse().map_err(|_| format!("--pid takes a process id, got {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Args, String> {
        parse(std::iter::once("mantle".to_string()).chain(args.iter().map(|a| (*a).to_string())))
    }

    /// The state name and value are taken before flags so a value like `-5` is not read as one,
    /// but an option the parser knows is still an option in that slot. Taking `-c` as the toggle
    /// value stored a flag in the state and then failed on the directory behind it.
    #[test]
    fn an_option_after_a_state_name_is_still_an_option() {
        use shared::{SetState, StateWrite};
        let dir = std::env::temp_dir();
        let path = dir.to_str().unwrap();
        let joined = format!("--config={path}");
        for args in [
            vec!["toggle", "launcher_open", "-c", path],
            vec!["toggle", "launcher_open", "--config", path],
            // The joined form is one argument, so it is the one a bare "starts with a dash" test
            // would wave through as the toggle's value.
            vec!["toggle", "launcher_open", joined.as_str()],
        ] {
            let parsed = parse_args(&args).unwrap();
            assert_eq!(
                parsed.command,
                Command::SetState(SetState { name: "launcher_open".into(), write: StateWrite::Toggle }),
                "{args:?}"
            );
            assert_eq!(parsed.config_dir.as_deref(), Some(std::path::Path::new(path)), "{args:?}");
        }
        // A value that merely begins with a dash is still a value.
        let parsed = parse_args(&["set", "volume_step", "-5"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::SetState(SetState { name: "volume_step".into(), write: StateWrite::Set(serde_json::json!(-5)) })
        );
    }

    #[test]
    fn no_arguments_runs_the_shell_against_the_default_config() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            Args { command: Command::Run, config_dir: None, detach: false, profile: None, pid: None, verbose: 0 }
        );
    }

    #[test]
    fn profile_defaults_its_interval_and_refuses_one_that_would_never_report() {
        assert_eq!(parse_args(&["--profile"]).unwrap().profile, Some(60));
        assert_eq!(parse_args(&["--profile=5"]).unwrap().profile, Some(5));
        assert!(parse_args(&["--profile=0"]).is_err());
        assert!(parse_args(&["--profile=soon"]).is_err());
        assert!(parse_args(&["check", "--profile"]).is_err(), "nothing runs long enough to report");
        assert_eq!(
            parse_args(&["--profile"]).unwrap().verbose,
            1,
            "the reports print at Info, so the flag asks for it"
        );
        assert_eq!(parse_args(&["--profile", "-vvv"]).unwrap().verbose, 3, "an explicit -v still wins");
    }

    #[test]
    fn verbose_counts_repetition_and_grouping_the_same_way() {
        assert_eq!(parse_args(&[]).unwrap().verbose, 0);
        assert_eq!(parse_args(&["-v"]).unwrap().verbose, 1);
        assert_eq!(parse_args(&["-v", "-v"]).unwrap().verbose, 2, "repeated, like -v twice on the shell");
        assert_eq!(parse_args(&["-vv"]).unwrap().verbose, 2, "grouped, the common shape");
        assert_eq!(parse_args(&["-vvv"]).unwrap().verbose, 3);
        assert_eq!(parse_args(&["--verbose", "-v"]).unwrap().verbose, 2, "long and short form add up");
        assert!(parse_args(&["check", "-v"]).is_err(), "nothing runs long enough to log anything");
    }

    #[test]
    fn a_config_directory_is_made_absolute() {
        let args = parse_args(&["-c", "some/config"]).unwrap();
        let dir = args.config_dir.expect("-c sets a directory");
        assert!(dir.is_absolute(), "a relative -c must be resolved before any Renderer inherits it");
        assert!(dir.ends_with("some/config"));
    }

    #[test]
    fn the_long_form_and_the_equals_form_agree() {
        let a = parse_args(&["--config", "some/config"]).unwrap();
        let b = parse_args(&["--config=some/config"]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_path_to_shell_lua_resolves_to_its_directory() {
        // Accommodate the common `-c ~/.config/mantle/shell.lua` after editing that file.
        //
        // The rule under test is "a path to a file resolves to its parent".
        // Absolute, because `is_file()` has to see it and tests run from the crate root.
        let dir = tempfile::tempdir().unwrap();
        let shell_lua = dir.path().join("shell.lua");
        std::fs::write(&shell_lua, "return {}").unwrap();
        let args = parse_args(&["-c", shell_lua.to_str().unwrap()]).unwrap();
        assert_eq!(args.config_dir.as_deref(), Some(dir.path()));
    }

    #[test]
    fn subcommands_parse_with_their_own_options() {
        assert_eq!(parse_args(&["init"]).unwrap().command, Command::Init { force: false });
        assert_eq!(parse_args(&["init", "--force"]).unwrap().command, Command::Init { force: true });
        assert_eq!(parse_args(&["check"]).unwrap().command, Command::Check);
        assert_eq!(
            parse_args(&["call", "rec.toggle"]).unwrap().command,
            Command::Call { name: "rec.toggle".into(), arguments: Vec::new() }
        );
        // The dot is not split: one opaque name, so no delimiter rule can surprise a config.
        assert_eq!(
            parse_args(&["call", "a.b.c", "7", "hello"]).unwrap().command,
            Command::Call {
                name: "a.b.c".into(),
                // JSON where it parses, a string otherwise, as `set` reads its value.
                arguments: vec![serde_json::json!(7), serde_json::json!("hello")]
            }
        );
        assert!(parse_args(&["call"]).is_err(), "call without a name has nothing to ask for");
        assert_eq!(parse_args(&["log"]).unwrap().command, Command::Log { follow: false });
        assert_eq!(parse_args(&["log", "-f"]).unwrap().command, Command::Log { follow: true });
    }

    /// ADR-0112: keybind verbs. Values parse as JSON when possible; bare words need no quotes.
    #[test]
    fn set_and_toggle_name_a_state_and_read_the_value_as_json_or_a_bare_string() {
        use shared::{SetState, StateWrite};
        assert_eq!(
            parse_args(&["set", "launcher_open", "true"]).unwrap().command,
            Command::SetState(SetState {
                name: "launcher_open".into(),
                write: StateWrite::Set(serde_json::json!(true))
            })
        );
        assert_eq!(
            parse_args(&["set", "panel_kind", "notifications"]).unwrap().command,
            Command::SetState(SetState {
                name: "panel_kind".into(),
                write: StateWrite::Set(serde_json::json!("notifications"))
            })
        );
        assert_eq!(
            parse_args(&["set", "volume_step", "-5"]).unwrap().command,
            Command::SetState(SetState { name: "volume_step".into(), write: StateWrite::Set(serde_json::json!(-5)) }),
            "a negative number is a value, not a flag"
        );
        assert_eq!(
            parse_args(&["toggle", "launcher_open"]).unwrap().command,
            Command::SetState(SetState { name: "launcher_open".into(), write: StateWrite::Toggle })
        );
        assert_eq!(
            parse_args(&["toggle", "modal", "launcher"]).unwrap().command,
            Command::SetState(SetState {
                name: "modal".into(),
                write: StateWrite::ToggleTo(serde_json::json!("launcher"))
            }),
            "a toggle with a value is a toggle to it"
        );
        assert!(parse_args(&["set", "launcher_open"]).is_err(), "set without a value");
        assert!(parse_args(&["toggle"]).is_err(), "toggle without a name");
    }

    #[test]
    fn force_without_init_is_refused_rather_than_ignored() {
        assert!(parse_args(&["--force"]).is_err(), "a flag that does nothing is worse than an error");
        assert!(parse_args(&["-f"]).is_err(), "--follow has nothing to follow without `log`");
        assert!(parse_args(&["log", "-d"]).is_err(), "--detach has nothing to detach without a run");
        assert!(parse_args(&["-d"]).unwrap().detach, "a bare run takes it");
        assert!(!parse_args(&["--detached"]).unwrap().detach, "the re-exec'd child must not detach again");
    }

    #[test]
    fn pid_parses_for_client_commands_and_is_refused_elsewhere() {
        let parsed = parse_args(&["toggle", "open", "--pid", "5"]).unwrap();
        assert_eq!(parsed.pid, Some(5));
        assert!(matches!(
            parsed.command,
            Command::SetState(shared::SetState { write: shared::StateWrite::Toggle, .. })
        ));
        assert_eq!(parse_args(&["log", "--pid=7"]).unwrap().pid, Some(7));
        assert_eq!(parse_args(&["list"]).unwrap().command, Command::List);
        for refused in [
            &["-d", "--pid", "5"][..],
            &["list", "--pid", "5"],
            &["init", "--pid", "5"],
            &["list", "-c", "/"],
            &["log", "-c", "/", "--pid", "5"],
            &["call", "rec.toggle", "--pid", "5", "-c", "/"],
            &["log", "--pid", "x"],
        ] {
            assert!(parse_args(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn config_needs_a_value_and_an_unknown_flag_is_an_error() {
        assert!(parse_args(&["-c"]).is_err());
        assert!(parse_args(&["--colour"]).is_err());
    }

    #[test]
    fn version_and_help_win_over_anything_after_them() {
        assert_eq!(parse_args(&["--version", "init"]).unwrap().command, Command::Version);
        assert_eq!(parse_args(&["-h"]).unwrap().command, Command::Help);
    }
}

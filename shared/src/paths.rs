//! XDG paths shared by `supervisor` and `renderer`, so both resolve the control socket, session
//! lock flag and config directory identically.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// `$XDG_RUNTIME_DIR/obelisk`: per-login state, and one directory per running Supervisor (ADR-0222).
pub fn runtime_root() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("obelisk"))
}

/// The Supervisor's `runtime_root()/<pid>-<start ms>`, handed to its Renderers through the environment.
pub const INSTANCE_DIR_ENV: &str = "OBELISK_INSTANCE_DIR";

/// This shell's socket, log and icon spools (ADR-0222).
pub fn instance_dir() -> io::Result<PathBuf> {
    std::env::var_os(INSTANCE_DIR_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "OBELISK_INSTANCE_DIR is not set"))
}

/// Control socket, shared by the `supervisor` listener and `renderer` client. Not `/tmp`: it is
/// world-writable and unsuitable for secure textfield submissions (ADR-0005).
pub fn control_socket_path() -> io::Result<PathBuf> {
    Ok(instance_dir()?.join("control.sock"))
}

/// The "compositor is locked and nothing of ours holds it" marker (ADR-0060), per login like the
/// compositor lock. Only `supervisor` reads or writes it; the Renderer holds the protocol object but
/// never the decision (ADR-0042).
pub fn session_locked_flag_path() -> io::Result<PathBuf> {
    Ok(runtime_root()?.join("session-locked"))
}

/// A config directory named by the session. `-c` overwrites it in the Supervisor.
pub const CONFIG_DIR_ENV: &str = "OBELISK_CONFIG_DIR";

/// Generation id stamped on every spawned Renderer.
///
/// Shared because both binaries read it. If absent, the Renderer treats that as "nobody spawned
/// me" and refuses to start; the Supervisor sets it on boot and every respawn.
pub const GENERATION_ID_ENV: &str = "OBELISK_GENERATION_ID";

/// Set when `obelisk check` re-execs the Renderer to evaluate a config without a display.
pub const CHECK_ENV: &str = "OBELISK_CHECK";

/// `obelisk --profile[=SECS]`, set by the Supervisor so every Renderer generation inherits it. One
/// switch for the idle, heap and PSS/GPU reports, so their lines share a clock.
pub const PROFILE_ENV: &str = "OBELISK_PROFILE";

/// The report interval [`PROFILE_ENV`] carries; the CLI already refused a bad value.
pub fn profile_interval() -> Option<Duration> {
    let secs = std::env::var(PROFILE_ENV).ok()?.parse::<u64>().ok().filter(|secs| *secs > 0)?;
    Some(Duration::from_secs(secs))
}

/// Renderer exit code for a Wayland connection that is gone: a log out, a reboot, or a compositor
/// crash. Shared because the Supervisor reads it as "the session is over" and stops rather than
/// respawning into a compositor that is not there.
///
/// Distinct from `0` (clean), `1` (a `?` failure) and `101` (a panic), and from the Renderer's `70`
/// for a gone Supervisor.
pub const EXIT_COMPOSITOR_GONE: i32 = 71;

/// `~/.config/obelisk/` by precedence: `$OBELISK_CONFIG_DIR`, `$XDG_CONFIG_HOME/obelisk`, then
/// `$HOME/.config/obelisk`.
///
/// Both binaries call this and agree through the environment. `-c` therefore sets
/// [`CONFIG_DIR_ENV`] in the Supervisor: every spawned Renderer, including a replacement,
/// inherits it. Passing a path through the handshake would require re-passing it on every
/// respawn; a missed pass would silently load a different config than the watched one.
pub fn config_dir() -> io::Result<PathBuf> {
    let var = std::env::var_os;
    config_dir_from(var(CONFIG_DIR_ENV), var("XDG_CONFIG_HOME"), var("HOME"))
}

/// [`config_dir`]'s precedence with its lookups passed as parameters.
///
/// Tests pass values instead of calling `set_var`: `setenv` rewrites process-wide `environ` and
/// races every concurrent `getenv`, regardless of which variable each call names.
fn config_dir_from(
    explicit: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> io::Result<PathBuf> {
    // It names the config directory itself; `$XDG_CONFIG_HOME` names its parent.
    if let Some(dir) = explicit {
        return Ok(PathBuf::from(dir));
    }

    if let Some(xdg_config_home) = xdg_config_home {
        return Ok(PathBuf::from(xdg_config_home).join("obelisk"));
    }

    let home =
        home.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".config").join("obelisk"))
}

/// `config_dir()` joined with the real config entry point, `shell.lua`.
pub fn shell_lua_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("shell.lua"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No lock is needed: precedence tests use [`config_dir_from`] and no test writes the
    /// process-wide environment anymore.
    #[test]
    fn shell_lua_path_is_config_dir_joined_with_shell_lua() {
        let path = shell_lua_path().unwrap();
        assert_eq!(path, config_dir().unwrap().join("shell.lua"));
        assert!(path.ends_with("shell.lua"));
    }

    /// The named directory is the config itself, not a parent to join with `obelisk`.
    #[test]
    fn the_named_directory_wins_over_xdg_config_home() {
        let resolved = config_dir_from(Some("/tmp/env".into()), Some("/tmp/xdg".into()), None).unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/env"));
    }

    /// The last rung, which old `set_var` tests could not reach without unsetting the developer's
    /// `$HOME`.
    #[test]
    fn home_is_the_last_resort_and_is_joined_with_dot_config() {
        assert_eq!(
            config_dir_from(None, None, Some("/home/someone".into())).unwrap(),
            PathBuf::from("/home/someone/.config/obelisk")
        );
    }

    /// No variables means an error rather than a guess.
    #[test]
    fn no_variable_at_all_is_an_error() {
        assert_eq!(config_dir_from(None, None, None).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}

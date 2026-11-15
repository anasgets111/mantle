//! XDG paths shared by `supervisor` and `renderer`, so both resolve the control socket, session
//! lock flag and config directory identically.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `$XDG_RUNTIME_DIR/mantle`: per-login state, and one directory per running Supervisor (ADR-0222).
pub fn runtime_root() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("mantle"))
}

/// The Supervisor's `runtime_root()/<pid>-<start ms>`, handed to its Renderers through the environment.
pub const INSTANCE_DIR_ENV: &str = "MANTLE_INSTANCE_DIR";

/// This shell's socket, log and icon spools (ADR-0222).
pub fn instance_dir() -> io::Result<PathBuf> {
    std::env::var_os(INSTANCE_DIR_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "MANTLE_INSTANCE_DIR is not set"))
}

/// Control socket, shared by the `supervisor` listener and `renderer` client. Not `/tmp`: it is
/// world-writable and unsuitable for secure textfield submissions (ADR-0005).
pub fn control_socket_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("control.sock")
}

/// The "compositor is locked and nothing of ours holds it" marker (ADR-0060), per compositor like
/// the lock it records: keyed by `$WAYLAND_DISPLAY`, so a shell in a nested compositor cannot set or
/// clear the outer shell's. Only `supervisor` reads or writes it; the Renderer holds the protocol
/// object but never the decision (ADR-0042).
pub fn session_locked_flag_path() -> io::Result<PathBuf> {
    Ok(session_locked_flag_path_from(&runtime_root()?, std::env::var_os("WAYLAND_DISPLAY").as_deref()))
}

/// [`session_locked_flag_path`] with its lookups passed as parameters; `root` is [`runtime_root`],
/// whose parent is `$XDG_RUNTIME_DIR`. An unset or empty display keeps the bare name: the Renderer
/// cannot connect without one, so nothing ever locks under it. ponytail: a Renderer handed an
/// inherited `WAYLAND_SOCKET` ignores the display, so this key and the length bound below do not
/// hold for it; that fd is single-use, so such a shell cannot respawn anyway.
fn session_locked_flag_path_from(root: &Path, display: Option<&OsStr>) -> PathBuf {
    let Some(display) = display.filter(|display| !display.is_empty()).map(Path::new) else {
        return root.join("session-locked");
    };
    // libwayland resolves a relative name against `$XDG_RUNTIME_DIR`, so `wayland-1` and
    // `$XDG_RUNTIME_DIR/wayland-1` are one socket and share one marker.
    let display = match display.file_name() {
        Some(name) if display.is_absolute() && display.parent() == root.parent() => name,
        _ => display.as_os_str(),
    };
    // `/` becomes `%_` and `%` becomes `%%`, so the name is one component and distinct displays
    // stay distinct. Two-byte escapes keep it under NAME_MAX: a connectable display fits `sun_path`
    // (std's `UnixStream::connect` refuses 108 bytes or more), so at most 15 + 2 x 107 = 229 bytes.
    let mut name = b"session-locked-".to_vec();
    for &byte in display.as_bytes() {
        match byte {
            b'/' => name.extend_from_slice(b"%_"),
            b'%' => name.extend_from_slice(b"%%"),
            _ => name.push(byte),
        }
    }
    root.join(OsString::from_vec(name))
}

/// A config directory named by the session. The Supervisor passes every Renderer its resolved one.
pub const CONFIG_DIR_ENV: &str = "MANTLE_CONFIG_DIR";

/// Generation id stamped on every spawned Renderer.
///
/// Shared because both binaries read it. If absent, the Renderer treats that as "nobody spawned
/// me" and refuses to start; the Supervisor sets it on boot and every respawn.
pub const GENERATION_ID_ENV: &str = "MANTLE_GENERATION_ID";

/// Set when `mantle check` re-execs the Renderer to evaluate a config without a display.
pub const CHECK_ENV: &str = "MANTLE_CHECK";

/// `mantle --profile[=SECS]`, passed by the Supervisor to every Renderer generation. One
/// switch for the idle, heap and PSS/GPU reports, so their lines share a clock.
pub const PROFILE_ENV: &str = "MANTLE_PROFILE";

/// The report interval [`PROFILE_ENV`] carries; the CLI already refused a bad value.
pub fn profile_interval() -> Option<Duration> {
    let secs = std::env::var(PROFILE_ENV).ok()?.parse::<u64>().ok().filter(|secs| *secs > 0)?;
    Some(Duration::from_secs(secs))
}

/// `mantle -v`'s count, passed by the Supervisor to every Renderer generation (ADR-0243): the
/// Renderer takes no argv of its own to parse a repeated flag from.
pub const VERBOSE_ENV: &str = "MANTLE_VERBOSE";

/// Renderer exit code for a Wayland connection that is gone: a log out, a reboot, or a compositor
/// crash. Shared because the Supervisor reads it as "the session is over" and stops rather than
/// respawning into a compositor that is not there.
///
/// Distinct from `0` (clean), `1` (a `?` failure) and `101` (a panic), and from the Renderer's `70`
/// for a gone Supervisor.
pub const EXIT_COMPOSITOR_GONE: i32 = 71;

/// Renderer exit code for a compositor missing a global the Renderer cannot start without, such as
/// `zwlr_layer_shell_v1`. The Supervisor stops rather than respawning: every replacement would find
/// the same compositor. `69` is sysexits' `EX_UNAVAILABLE`, distinct from the codes above.
pub const EXIT_COMPOSITOR_UNSUPPORTED: i32 = 69;

/// `~/.config/mantle/` by precedence: `$MANTLE_CONFIG_DIR`, `$XDG_CONFIG_HOME/mantle`, then
/// `$HOME/.config/mantle`.
///
/// Both binaries agree through the environment: the Supervisor resolves it, `-c` included, and sets
/// [`CONFIG_DIR_ENV`] on every Renderer it spawns, a replacement included. Passing a path through
/// the handshake would require re-passing it on every respawn; a missed pass would silently load a
/// different config than the watched one.
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

    xdg_dir_from(xdg_config_home, home, ".config")
        .map(|dir| dir.join("mantle"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME is set"))
}

/// An XDG base directory: `$var`, else `$HOME/<home_relative>`. The spec says to ignore an empty or
/// relative `$var`.
pub fn xdg_dir(var: &str, home_relative: &str) -> Option<PathBuf> {
    xdg_dir_from(std::env::var_os(var), std::env::var_os("HOME"), home_relative)
}

/// `$XDG_DATA_HOME` then each absolute `$XDG_DATA_DIRS` entry. An empty or unset dirs value is
/// `/usr/local/share:/usr/share`: a login shell often leaves the variable unset, and skipping
/// `/usr/share` then finds nothing.
pub fn xdg_data_dirs(data_home: Option<PathBuf>, data_dirs: Option<&str>) -> Vec<PathBuf> {
    let dirs = data_dirs.filter(|value| !value.is_empty()).unwrap_or("/usr/local/share:/usr/share");
    data_home
        .into_iter()
        .chain(dirs.split(':').filter(|entry| Path::new(entry).is_absolute()).map(PathBuf::from))
        .collect()
}

/// [`xdg_dir`] with its lookups passed as parameters.
fn xdg_dir_from(value: Option<OsString>, home: Option<OsString>, home_relative: &str) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| home.filter(|home| !home.is_empty()).map(|home| PathBuf::from(home).join(home_relative)))
}

/// Why [`expand_home`] refused a path.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExpandHomeError {
    #[error("`~/` needs $HOME, which is unset or empty")]
    NoHome,
    #[error("path must be absolute or start with ~/")]
    Relative,
}

/// `path` with a leading `~/` expanded under `$HOME`; anything else, relative included, as given, for
/// callers whose own rules decide what a bare name means.
pub fn expand_tilde(path: &str) -> Result<Cow<'_, str>, ExpandHomeError> {
    if !path.starts_with("~/") {
        return Ok(Cow::Borrowed(path));
    }
    expand_home(path).map(|expanded| Cow::Owned(expanded.to_string_lossy().into_owned()))
}

/// An absolute path, or `~/rest` under `$HOME`; an error for anything else, so a config's relative path
/// never silently resolves against the renderer's working directory.
pub fn expand_home(path: &str) -> Result<PathBuf, ExpandHomeError> {
    expand_home_from(path, std::env::var_os("HOME"))
}

fn expand_home_from(path: &str, home: Option<OsString>) -> Result<PathBuf, ExpandHomeError> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = home.filter(|home| !home.is_empty()).ok_or(ExpandHomeError::NoHome)?;
        return Ok(PathBuf::from(home).join(rest));
    }
    if Path::new(path).is_absolute() { Ok(PathBuf::from(path)) } else { Err(ExpandHomeError::Relative) }
}

/// Strips trailing slashes for `mantle.folders` payload keys, making `/walls/` and `/walls` one
/// watch while keeping `/` as `/`.
pub fn folder_key(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() }
}

/// `config_dir()` joined with the real config entry point, `shell.lua`.
pub fn shell_lua_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("shell.lua"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_wayland_display_gets_its_own_session_locked_marker_inside_the_runtime_dir() {
        let root = Path::new("/run/user/1000/mantle");
        let marker = |display: &[u8]| session_locked_flag_path_from(root, Some(OsStr::from_bytes(display)));
        assert_eq!(marker(b"wayland-1"), root.join("session-locked-wayland-1"));
        assert_ne!(
            marker(b"wayland-1"),
            marker(b"wayland-2"),
            "a nested shell's lock must not read as the outer one's"
        );
        assert_eq!(marker(b"/run/user/1000/wayland-1"), marker(b"wayland-1"), "one socket, one marker");

        // An absolute socket path is legal in WAYLAND_DISPLAY; it must not escape the root, nor
        // collide with a name that spells its encoding.
        let absolute = marker(b"/tmp/../etc/wayland-1");
        assert_eq!(absolute, root.join("session-locked-%_tmp%_..%_etc%_wayland-1"));
        assert_eq!(absolute.parent(), Some(root));
        assert_ne!(marker(b"/tmp/x"), marker(b"%_tmp%_x"));
        assert_eq!(marker(b"w\xff_%").as_os_str().as_bytes(), b"/run/user/1000/mantle/session-locked-w\xff_%%");
        let longest = marker(&[b'/'; 107]);
        assert_eq!(longest.parent(), Some(root));
        assert_eq!(longest.file_name().unwrap().len(), 229, "a connectable display stays under NAME_MAX");

        // Only an absolute display takes the shortcut, even under a relative runtime dir.
        let relative = Path::new("x/mantle");
        let display = |name: &str| session_locked_flag_path_from(relative, Some(OsStr::new(name)));
        assert_ne!(display("x/wayland-1"), display("wayland-1"));
        assert_eq!(session_locked_flag_path_from(root, None), root.join("session-locked"));
        assert_eq!(marker(b""), root.join("session-locked"));
    }

    #[test]
    fn expand_home_takes_absolute_and_tilde_slash_only() {
        let home = Some(OsString::from("/home/u"));
        assert_eq!(expand_home_from("~/a/b", home.clone()), Ok("/home/u/a/b".into()));
        assert_eq!(expand_home_from("/etc/x", home.clone()), Ok("/etc/x".into()));
        assert_eq!(expand_home_from("rel/x", home.clone()), Err(ExpandHomeError::Relative));
        assert_eq!(expand_home_from("~x", home), Err(ExpandHomeError::Relative));
        assert_eq!(expand_home_from("~/a", Some("".into())), Err(ExpandHomeError::NoHome));
        assert_eq!(expand_home_from("~/a", None), Err(ExpandHomeError::NoHome));
    }

    #[test]
    fn expand_tilde_passes_everything_but_a_leading_tilde_slash_through() {
        for same in ["/etc/x", "rel/x", "~x", "a/~/b", "icon-name"] {
            assert_eq!(expand_tilde(same).unwrap(), same);
        }
    }

    #[test]
    fn config_dir_prefers_the_named_directory_then_xdg_then_home_and_errors_without_any() {
        // (label, explicit, xdg_config_home, home, expected; None = NotFound)
        let cases = [
            (
                "the named directory wins over xdg_config_home",
                Some("/tmp/env"),
                Some("/tmp/xdg"),
                None,
                Some("/tmp/env"),
            ),
            // The last rung, which old `set_var` tests could not reach without unsetting the
            // developer's `$HOME`.
            (
                "home is the last resort, joined with .config",
                None,
                None,
                Some("/home/someone"),
                Some("/home/someone/.config/mantle"),
            ),
            // No variables means an error rather than a guess.
            ("no variable at all is an error", None, None, None, None),
        ];
        for (label, explicit, xdg, home, expected) in cases {
            let resolved = config_dir_from(explicit.map(Into::into), xdg.map(Into::into), home.map(Into::into));
            match expected {
                Some(dir) => assert_eq!(resolved.unwrap(), PathBuf::from(dir), "{label}"),
                None => assert_eq!(resolved.unwrap_err().kind(), io::ErrorKind::NotFound, "{label}"),
            }
        }
    }

    #[test]
    fn an_empty_or_relative_xdg_value_falls_back_to_home() {
        let home = || Some("/home/someone".into());
        let resolve = |value: &str| xdg_dir_from(Some(value.into()), home(), ".cache");
        assert_eq!(resolve("/var/cache"), Some(PathBuf::from("/var/cache")));
        assert_eq!(resolve(""), Some(PathBuf::from("/home/someone/.cache")));
        assert_eq!(resolve("cache"), Some(PathBuf::from("/home/someone/.cache")));
        assert_eq!(xdg_dir_from(None, Some("".into()), ".cache"), None);
    }

    #[test]
    fn folder_key_strips_trailing_slashes_and_keeps_root() {
        assert_eq!(folder_key("/walls/"), "/walls");
        assert_eq!(folder_key("/walls//"), "/walls");
        assert_eq!(folder_key("/walls"), "/walls");
        assert_eq!(folder_key("/"), "/");
    }
}

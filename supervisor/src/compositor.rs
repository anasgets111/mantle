//! Which compositor this session is running, and the probe that answers it.
//!
//! Top-level because compositor identity belongs to the session, not a capability: `keyboard`
//! and `workspaces` both need it, and **Compositor link** (`CONTEXT.md`) is scoped to keyboard
//! layout.
//!
//! This owns detection and compositor IPC plumbing, with no adaptor. ADR-0056 decision 1 says
//! `workspaces` gets no trait and `CompositorLink` does not grow one. The two capabilities share
//! only this probe, the IPC connections and, since `workspaces::hyprland` (ADR-0118), the two
//! socket locations.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use shared::debug;

/// A compositor implemented here, narrower than "a compositor that exists". Other sessions yield
/// [`detect_compositor`]'s `None`; dependent capabilities degrade rather than guess (ADR-0056
/// decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorKind {
    Hyprland,
    Niri,
}

impl CompositorKind {
    /// Lowercase payload name (`mantle.workspaces.compositor`, ADR-0119), so config can choose
    /// display policy without detecting the compositor again.
    pub fn name(self) -> &'static str {
        match self {
            CompositorKind::Hyprland => "hyprland",
            CompositorKind::Niri => "niri",
        }
    }
}

/// Env vars each compositor sets for every process in its session, in probe order.
///
/// A table, so a third compositor is one data line and precedence is explicit. Order breaks ties if two vars are set; that case is unlikely and harmless because
/// real sessions run one compositor.
///
/// Entries are vars set *because the compositor is running*. `$XDG_CURRENT_DESKTOP` is only a name
/// written by the launcher and remains set if the compositor never starts. It is useful to report
/// via [`unsupported_session_report`], not to dispatch on.
const PROBES: &[(CompositorKind, &str)] =
    &[(CompositorKind::Hyprland, "HYPRLAND_INSTANCE_SIGNATURE"), (CompositorKind::Niri, "NIRI_SOCKET")];

/// The first [`PROBES`] entry whose var this session has set, or `None` for a compositor with no
/// implementor here.
pub fn detect_compositor() -> Option<CompositorKind> {
    PROBES.iter().find(|(_, var)| std::env::var_os(var).is_some()).map(|(kind, _)| *kind)
}

/// The "disabled for this run" line `keyboard` and `workspaces` share when [`detect_compositor`]
/// returns `None`.
///
/// If set, names `$XDG_CURRENT_DESKTOP`, because "this session is sway, which has no implementor"
/// is actionable. This is not a second detection path; an unrecognised name still yields no
/// implementor.
pub fn unsupported_session_report() -> String {
    match session_desktop() {
        Some(desktop) => format!("this session is {desktop}, which has no implementor"),
        None => "no supported compositor was detected".to_string(),
    }
}

/// `$HYPRLAND_INSTANCE_SIGNATURE`, or `None` when it is unset or empty.
///
/// [`detect_compositor`] probes with `var_os`, which accepts bytes `var` rejects, so a session
/// detected as Hyprland can still have no usable signature. An empty one builds
/// `$XDG_RUNTIME_DIR/hypr//.socket.sock`, which resolves and never connects.
pub fn hyprland_signature() -> Option<String> {
    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|signature| !signature.is_empty())
}

/// A socket in Hyprland's per-instance directory,
/// `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`. `"socket2.sock"` pushes
/// newline-terminated `event>>payload` lines; `"socket.sock"` answers one plain-text command per
/// connection (`j/workspaces` for JSON, `dispatch ...` for a write). Shared because `keyboard` and
/// `workspaces` both open these files, and their path is a session-level fact.
pub fn hyprland_socket_path(signature: &str, name: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    hyprland_socket_path_in(&runtime_dir, signature, name)
}

/// The `join` half of [`hyprland_socket_path`], split from `$XDG_RUNTIME_DIR` lookup so tests avoid
/// `set_var`. `setenv` rewrites process-wide `environ` and races every concurrent `getenv` in the
/// test binary, even for unrelated variables.
fn hyprland_socket_path_in(runtime_dir: &str, signature: &str, name: &str) -> PathBuf {
    PathBuf::from(runtime_dir).join("hypr").join(signature).join(name)
}

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// One `.socket.sock` command. Hyprland answers once per connection and closes it: `j/<what>`
/// returns the `hyprctl -j` JSON, a write returns `ok` or the reason it refused (ADR-0118).
pub fn hyprland_request(socket_path: &Path, command: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    // A stalled Hyprland must not wedge the reader or the shared action thread.
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.write_all(command.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

/// A write and its reply check, blocking. `capability` prefixes the log line, the only thing the
/// two callers differ in. An unread reply makes a refusal silent: a bad device, an index out of
/// range.
pub fn hyprland_command(socket_path: &Path, command: &str, capability: &str) {
    match hyprland_request(socket_path, command) {
        Ok(reply) if reply.trim() == "ok" => {
            debug!(2; "{capability}: Hyprland command `{command}` succeeded");
        }
        Ok(reply) => debug!("{capability}: Hyprland refused `{command}`: {}", reply.trim()),
        Err(err) => debug!("{capability}: Hyprland `{command}` request failed: {err}"),
    }
}

/// `$NIRI_SOCKET` with its event stream requested, ready for `read_events`. Blocks on niri's
/// reply, so callers run it off the main task.
pub fn niri_event_stream() -> std::io::Result<niri_ipc::socket::Socket> {
    let mut socket = niri_ipc::socket::Socket::connect()?;
    match socket.send(niri_ipc::Request::EventStream)? {
        Ok(niri_ipc::Response::Handled) => Ok(socket),
        Ok(other) => Err(std::io::Error::other(format!("unexpected reply to the niri EventStream request: {other:?}"))),
        Err(msg) => Err(std::io::Error::other(format!("niri refused the EventStream request: {msg}"))),
    }
}

/// Runs `job` after every earlier one on a single shared thread, so rapid write actions reach the
/// compositor in the order they were sent. Each job blocks for at most one IPC round trip.
pub fn run_in_order(job: impl FnOnce() + Send + 'static) {
    type Job = Box<dyn FnOnce() + Send>;
    static QUEUE: std::sync::OnceLock<std::sync::mpsc::Sender<Job>> = std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (queue, jobs) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || jobs.into_iter().for_each(|job| job()));
        queue
    });
    let _ = queue.send(Box::new(job));
}

/// One niri request on a fresh connection (`read_events` shuts down the event-stream socket's
/// write half), bounded by [`REQUEST_TIMEOUT`]: `niri_ipc::socket::Socket` has no timeout API, so a
/// stalled niri would wedge [`run_in_order`]'s thread. The connect itself cannot stall on a unix
/// socket short of a full backlog.
fn niri_request(path: &Path, request: &niri_ipc::Request) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let mut line = serde_json::to_string(request).map_err(std::io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut String::new())?;
    Ok(())
}

/// One niri action, fire-and-forget through [`run_in_order`].
pub fn niri_action(action: niri_ipc::Action, capability: &'static str) {
    run_in_order(move || {
        let label = format!("{action:?}");
        let Some(path) = std::env::var_os(niri_ipc::socket::SOCKET_PATH_ENV) else {
            return debug!("{capability}: {} is unset; {label} ignored", niri_ipc::socket::SOCKET_PATH_ENV);
        };
        if let Err(err) = niri_request(Path::new(&path), &niri_ipc::Request::Action(action)) {
            debug!("{capability}: niri {label} request failed: {err}");
        }
    });
}

fn session_desktop() -> Option<String> {
    let value = std::env::var("XDG_CURRENT_DESKTOP").ok()?;
    desktop_name(&value).map(str::to_string)
}

/// `$XDG_CURRENT_DESKTOP`'s first entry. The spec orders its colon-separated list most to least
/// specific, so `"niri:wlroots"` names niri. Separate from [`session_desktop`] to test parsing
/// without writing a process-global env var from a test thread.
fn desktop_name(value: &str) -> Option<&str> {
    let first = value.split(':').next()?.trim();
    (!first.is_empty()).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compositor_kind_is_detectable_by_the_var_that_compositor_sets() {
        // The exhaustive `match` forces each new kind to name its env var. `find` and the length
        // then force that pair into `PROBES`; a missing entry is undetectable.
        for kind in [CompositorKind::Hyprland, CompositorKind::Niri] {
            let var = match kind {
                CompositorKind::Hyprland => "HYPRLAND_INSTANCE_SIGNATURE",
                CompositorKind::Niri => "NIRI_SOCKET",
            };
            assert_eq!(PROBES.iter().find(|(probe, _)| *probe == kind).map(|(_, v)| *v), Some(var), "{kind:?}");
        }
        assert_eq!(PROBES.len(), 2, "a PROBES entry for a kind the loop above does not list");
    }

    #[test]
    fn hyprland_request_gives_up_on_a_socket_that_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".socket.sock");
        let _silent = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let started = std::time::Instant::now();
        let err = hyprland_request(&path, "j/clients").unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn run_in_order_keeps_send_order_when_the_first_job_is_slow() {
        let (seen, arrived) = std::sync::mpsc::channel();
        let first = seen.clone();
        run_in_order(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            first.send(1).unwrap();
        });
        run_in_order(move || seen.send(2).unwrap());

        let wait = std::time::Duration::from_secs(5);
        let order = [arrived.recv_timeout(wait), arrived.recv_timeout(wait)];
        assert_eq!(order, [Ok(1), Ok(2)]);
    }

    #[test]
    fn niri_request_gives_up_on_a_socket_that_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("niri.sock");
        let _silent = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let started = std::time::Instant::now();
        let err = niri_request(&path, &niri_ipc::Request::Version).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn hyprland_socket_path_joins_runtime_dir_hypr_signature_and_name() {
        assert_eq!(
            hyprland_socket_path_in("/run/user/1000", "abc123", "socket2.sock"),
            PathBuf::from("/run/user/1000/hypr/abc123/socket2.sock")
        );
    }

    #[test]
    fn a_missing_runtime_dir_falls_back_to_tmp() {
        assert_eq!(
            hyprland_socket_path_in("/tmp", "abc123", "socket2.sock"),
            PathBuf::from("/tmp/hypr/abc123/socket2.sock")
        );
    }

    #[test]
    fn desktop_name_takes_the_most_specific_entry_of_a_colon_separated_list() {
        assert_eq!(desktop_name("niri:wlroots"), Some("niri"));
        assert_eq!(desktop_name("Hyprland"), Some("Hyprland"));
        assert_eq!(desktop_name(" sway : wlroots "), Some("sway"));
    }

    #[test]
    fn desktop_name_is_none_when_the_variable_is_set_but_empty() {
        // Bare `weston`/`cage` can leave `XDG_CURRENT_DESKTOP` set but empty; reporting
        // "this session is , which has no implementor" is worse than the generic line.
        assert_eq!(desktop_name(""), None);
        assert_eq!(desktop_name("   "), None);
        assert_eq!(desktop_name(":wlroots"), None);
    }
}

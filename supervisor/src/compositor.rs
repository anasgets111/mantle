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

use std::io::{BufRead, Read, Write};
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
    Sway,
    Mango,
}

impl CompositorKind {
    /// Lowercase payload name (`mantle.workspaces.compositor`, ADR-0119), so config can choose
    /// display policy without detecting the compositor again.
    pub fn name(self) -> &'static str {
        match self {
            CompositorKind::Hyprland => "hyprland",
            CompositorKind::Niri => "niri",
            CompositorKind::Sway => "sway",
            CompositorKind::Mango => "mango",
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
const PROBES: &[(CompositorKind, &str)] = &[
    (CompositorKind::Hyprland, "HYPRLAND_INSTANCE_SIGNATURE"),
    (CompositorKind::Niri, "NIRI_SOCKET"),
    (CompositorKind::Sway, "SWAYSOCK"),
    (CompositorKind::Mango, "MANGO_INSTANCE_SIGNATURE"),
];

/// The first [`PROBES`] entry whose var this session has set, or `None` for a compositor with no
/// implementor here.
pub fn detect_compositor() -> Option<CompositorKind> {
    PROBES.iter().find(|(_, var)| std::env::var_os(var).is_some()).map(|(kind, _)| *kind)
}

/// The "disabled for this run" line `keyboard` and `workspaces` share when [`detect_compositor`]
/// returns `None`.
///
/// If set, names `$XDG_CURRENT_DESKTOP`, because "this session is river, which has no implementor"
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

/// `$SWAYSOCK`, or `None` when unset or empty.
pub fn sway_socket() -> Option<PathBuf> {
    std::env::var_os("SWAYSOCK").filter(|path| !path.is_empty()).map(PathBuf::from)
}

pub const SWAY_RUN_COMMAND: u32 = 0;
pub const SWAY_GET_WORKSPACES: u32 = 1;
pub const SWAY_SUBSCRIBE: u32 = 2;
pub const SWAY_GET_TREE: u32 = 4;
pub const SWAY_GET_INPUTS: u32 = 100;
const SWAY_MAGIC: &[u8; 6] = b"i3-ipc";
/// Far above a real `GET_TREE`; a corrupt length must not allocate gigabytes.
const SWAY_MAX_PAYLOAD: usize = 64 << 20;

/// Writes one i3-ipc frame.
pub fn sway_write(stream: &mut impl Write, kind: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(SWAY_MAGIC);
    frame.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    frame.extend_from_slice(&kind.to_ne_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

/// Reads one frame as `(type, payload)`.
pub fn sway_read(stream: &mut impl Read) -> std::io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 14];
    stream.read_exact(&mut header)?;
    if &header[..6] != SWAY_MAGIC {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "sway frame lacks the i3-ipc magic"));
    }
    let len = u32::from_ne_bytes(header[6..10].try_into().expect("4 bytes")) as usize;
    let kind = u32::from_ne_bytes(header[10..14].try_into().expect("4 bytes"));
    if len > SWAY_MAX_PAYLOAD {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("sway frame of {len} bytes")));
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload)?;
    Ok((kind, payload))
}

/// A connection to sway whose reads and writes give up after [`REQUEST_TIMEOUT`].
pub fn sway_connect(socket_path: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    Ok(stream)
}

/// One request on a fresh connection; the first frame back is the reply.
pub fn sway_request(socket_path: &Path, kind: u32, payload: &str) -> std::io::Result<Vec<u8>> {
    let mut stream = sway_connect(socket_path)?;
    sway_write(&mut stream, kind, payload.as_bytes())?;
    Ok(sway_read(&mut stream)?.1)
}

/// One `RUN_COMMAND` on the shared action thread; sway answers `[{"success":bool,"error":..}]`.
pub fn sway_command(command: String, capability: &'static str) {
    run_in_order(move || {
        let Some(path) = sway_socket() else {
            return debug!("{capability}: SWAYSOCK is unset; sway `{command}` ignored");
        };
        match sway_request(&path, SWAY_RUN_COMMAND, &command) {
            Ok(reply) => match serde_json::from_slice::<Vec<serde_json::Value>>(&reply) {
                Ok(results) if results.iter().all(|r| r["success"] == true) => {
                    debug!(2; "{capability}: sway command `{command}` succeeded");
                }
                _ => debug!("{capability}: sway refused `{command}`: {}", String::from_utf8_lossy(&reply)),
            },
            Err(err) => debug!("{capability}: sway `{command}` request failed: {err}"),
        }
    });
}

/// `$MANGO_INSTANCE_SIGNATURE`, the path of mango's IPC socket, or `None` when unset or empty.
/// mango exports it only while its socket is bound and unsets it on exit.
pub fn mango_socket() -> Option<PathBuf> {
    std::env::var_os("MANGO_INSTANCE_SIGNATURE").filter(|path| !path.is_empty()).map(PathBuf::from)
}

/// Longest mango line read, matching the other compositor readers; a longer one is a lost stream.
pub const MANGO_LINE_MAX: u64 = 64 << 20;

/// Connects and sends one newline-terminated mango command. A newline inside `command` would
/// smuggle a second command, so it is refused. A `watch` keeps the stream open for more lines.
pub fn mango_send(socket_path: &Path, command: &str) -> std::io::Result<UnixStream> {
    if command.contains(['\n', '\r']) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "newline in a mango command"));
    }
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    stream.write_all(format!("{command}\n").as_bytes())?;
    Ok(stream)
}

/// One line of at most `max` bytes, `None` at end of stream. Lossy: mango copies window titles
/// into its JSON raw, and a strict `read_line` would fail the stream on one that is not UTF-8.
pub fn mango_read_line(reader: &mut impl BufRead, max: u64) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = reader.take(max).read_until(b'\n', &mut line)?;
    if read as u64 == max && line.last() != Some(&b'\n') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "mango line over the size limit"));
    }
    Ok((read > 0).then(|| String::from_utf8_lossy(&line).into_owned()))
}

/// One mango `get` or `dispatch`: a single JSON line back, then mango closes the connection.
pub fn mango_request(socket_path: &Path, command: &str) -> std::io::Result<String> {
    let stream = mango_send(socket_path, command)?;
    // A stalled mango must not wedge the reader or the shared action thread.
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    Ok(mango_read_line(&mut std::io::BufReader::new(stream), MANGO_LINE_MAX)?.unwrap_or_default())
}

/// A mango `dispatch` through [`run_in_order`], logging a refusal (`{"error":...}`) at debug level.
pub fn mango_dispatch(command: String, capability: &'static str) {
    run_in_order(move || {
        let Some(path) = mango_socket() else {
            return debug!("{capability}: MANGO_INSTANCE_SIGNATURE is unset; `{command}` ignored");
        };
        match mango_request(&path, &format!("dispatch {command}")) {
            Ok(reply) if reply.contains("\"success\":true") => debug!(2; "{capability}: mango `{command}` succeeded"),
            Ok(reply) => debug!("{capability}: mango refused `{command}`: {}", reply.trim()),
            Err(err) => debug!("{capability}: mango `{command}` request failed: {err}"),
        }
    });
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
        for kind in [CompositorKind::Hyprland, CompositorKind::Niri, CompositorKind::Sway, CompositorKind::Mango] {
            let var = match kind {
                CompositorKind::Hyprland => "HYPRLAND_INSTANCE_SIGNATURE",
                CompositorKind::Niri => "NIRI_SOCKET",
                CompositorKind::Sway => "SWAYSOCK",
                CompositorKind::Mango => "MANGO_INSTANCE_SIGNATURE",
            };
            assert_eq!(PROBES.iter().find(|(probe, _)| *probe == kind).map(|(_, v)| *v), Some(var), "{kind:?}");
        }
        assert_eq!(PROBES.len(), 4, "a PROBES entry for a kind the loop above does not list");
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
    fn a_request_gives_up_on_a_socket_that_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent.sock");
        let _silent = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let started = std::time::Instant::now();
        let hypr = hyprland_request(&path, "j/clients").unwrap_err();
        let niri = niri_request(&path, &niri_ipc::Request::Version).unwrap_err();

        assert_eq!([hypr.kind(), niri.kind()], [std::io::ErrorKind::WouldBlock; 2]);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn a_sway_frame_round_trips_over_a_socket_and_a_bad_magic_is_rejected() {
        let (mut ours, mut theirs) = UnixStream::pair().unwrap();
        sway_write(&mut ours, SWAY_GET_TREE, b"{\"x\":1}").unwrap();
        assert_eq!(sway_read(&mut theirs).unwrap(), (SWAY_GET_TREE, b"{\"x\":1}".to_vec()));

        ours.write_all(b"i3-ipx\0\0\0\0\0\0\0\0").unwrap();
        assert_eq!(sway_read(&mut theirs).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_sway_frame_announcing_more_than_the_cap_is_rejected_before_allocating() {
        let mut frame = b"i3-ipc".to_vec();
        frame.extend_from_slice(&u32::MAX.to_ne_bytes());
        frame.extend_from_slice(&4u32.to_ne_bytes());

        assert_eq!(sway_read(&mut frame.as_slice()).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_sway_request_reads_the_first_frame_the_server_answers_with() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sway.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let asked = sway_read(&mut conn).unwrap();
            sway_write(&mut conn, asked.0, b"[]").unwrap();
            asked
        });

        let reply = sway_request(&path, SWAY_GET_WORKSPACES, "").unwrap();

        assert_eq!(reply, b"[]");
        assert_eq!(server.join().unwrap(), (SWAY_GET_WORKSPACES, Vec::new()));
    }

    #[test]
    fn a_mango_line_over_the_limit_is_an_error_and_bad_utf8_is_replaced() {
        let read = |bytes: &[u8]| mango_read_line(&mut std::io::BufReader::new(bytes), 8);
        assert_eq!(read(b"0123456\n").unwrap().as_deref(), Some("0123456\n"));
        assert_eq!(read(b"01234567\n").unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(read(b"a\xffb").unwrap().as_deref(), Some("a\u{fffd}b"));
        assert_eq!(read(b"").unwrap(), None);
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
        assert_eq!(desktop_name(" river : wlroots "), Some("river"));
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

//! mango, reached over `$MANGO_INSTANCE_SIGNATURE` (a Unix socket path).
//!
//! mango is dwl-style: each output has a fixed set of tags (1..=`tag_num`) and shows any subset of
//! them. Each tag becomes one workspace with id `"<output>:<tag>"`. `watch all-monitors` pushes a
//! full snapshot per change; clients come from a `get all-clients` after each one. Fixture shapes
//! come from `build_monitor_json`, `build_tags_json` and `build_client_json` in mango's
//! `src/ipc/ipc.c`.

use std::io::{BufRead, Write};
use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use shared::{debug, warn};

use super::{Compositor, End, REQUEST_TIMEOUT, keep_following, run_in_order, toggle_needed};
use crate::capabilities::RETRY_FIRST;
use crate::capabilities::keyboard::layout::LayoutSink;
use crate::capabilities::windows::controller::{StatePublisher as WindowsPublisher, WindowEntry};
use crate::capabilities::workspaces::controller::{FocusedWindow, StatePublisher, WorkspaceRow};

#[derive(Deserialize)]
struct Monitors {
    monitors: Vec<Monitor>,
}

#[derive(Deserialize)]
struct Monitor {
    name: String,
    /// The monitor holding keyboard focus.
    #[serde(default)]
    active: bool,
    #[serde(default)]
    tags: Vec<Tag>,
    #[serde(default)]
    keyboardlayout: String,
}

#[derive(Deserialize)]
struct Tag {
    index: u32,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    is_urgent: bool,
    #[serde(default)]
    client_count: u32,
}

#[derive(Deserialize)]
struct Clients {
    clients: Vec<Client>,
}

/// `tags` holds 1-based tag numbers, or 0 for mango's special tag.
#[derive(Deserialize)]
struct Client {
    id: u32,
    #[serde(default)]
    title: String,
    #[serde(default)]
    appid: String,
    #[serde(default)]
    monitor: String,
    #[serde(default)]
    tags: Vec<u32>,
    #[serde(default)]
    is_focused: bool,
    #[serde(default)]
    is_fullscreen: bool,
    #[serde(default)]
    is_floating: bool,
    #[serde(default)]
    is_maximized: bool,
    #[serde(default)]
    is_minimized: bool,
    #[serde(default)]
    is_urgent: bool,
}

fn workspace_id(output: &str, tag: u32) -> String {
    format!("{output}:{tag}")
}

/// Every tag of every output. mango may show several tags at once but a payload output has one
/// active workspace, so only the lowest shown tag is `is_active`.
fn workspace_rows(monitors: &[Monitor], clients: &[Client]) -> Vec<WorkspaceRow> {
    let mut rows = Vec::new();
    for monitor in monitors {
        let active = monitor.tags.iter().filter(|tag| tag.is_active).map(|tag| tag.index).min();
        for tag in &monitor.tags {
            let standing = clients
                .iter()
                .filter(|client| client.monitor == monitor.name && client.tags.contains(&tag.index))
                .min_by_key(|client| (!client.is_focused, client.id));
            let is_active = active == Some(tag.index);
            rows.push(WorkspaceRow {
                id: workspace_id(&monitor.name, tag.index),
                number: Some(tag.index),
                name: None,
                output: Some(monitor.name.clone()),
                is_active,
                is_focused: is_active && monitor.active,
                populated: tag.client_count > 0,
                app_id: standing.map(|client| client.appid.clone()).filter(|id| !id.is_empty()),
                window_id: standing.map(|client| client.id.to_string()),
                urgent: tag.is_urgent,
            });
        }
    }
    rows
}

fn window_rows(clients: &[Client]) -> Vec<WindowEntry> {
    let mut ordered: Vec<&Client> = clients.iter().collect();
    ordered.sort_by_key(|client| client.id);
    ordered
        .into_iter()
        .map(|client| {
            let output = Some(client.monitor.clone()).filter(|name| !name.is_empty());
            let tag = client.tags.iter().copied().filter(|&tag| tag > 0).min();
            WindowEntry {
                id: client.id.to_string(),
                title: client.title.clone(),
                app_id: client.appid.clone(),
                workspace_id: output.as_deref().zip(tag).map(|(output, tag)| workspace_id(output, tag)),
                output,
                focused: client.is_focused,
                floating: Some(client.is_floating),
                fullscreen: Some(client.is_fullscreen),
                minimized: Some(client.is_minimized),
                maximized: Some(client.is_maximized),
                urgent: client.is_urgent,
            }
        })
        .collect()
}

fn focused_window(clients: &[Client]) -> Option<FocusedWindow> {
    clients.iter().find(|client| client.is_focused).map(|client| FocusedWindow {
        title: client.title.clone(),
        app_id: client.appid.clone(),
        floating: client.is_floating,
        fullscreen: Some(client.is_fullscreen),
    })
}

/// What one read of the watch stream holds.
enum Burst {
    End,
    /// Lines arrived but none was a monitor snapshot.
    Junk,
    Snapshot(Vec<Monitor>),
}

/// Blocks for one line, then takes every complete line already buffered and keeps the last
/// snapshot: mango pushes one on every arrange, title changes included, and drops a watcher that
/// falls behind.
fn read_burst<R: Read>(reader: &mut BufReader<R>) -> std::io::Result<Burst> {
    let mut burst = Burst::End;
    loop {
        let Some(line) = mango_read_line(reader, MANGO_LINE_MAX)? else { return Ok(burst) };
        burst = match serde_json::from_str(&line) {
            Ok(Monitors { monitors }) => Burst::Snapshot(monitors),
            Err(_) => {
                debug!("skipped a mango line that is not a monitor snapshot: {line}");
                match burst {
                    Burst::End => Burst::Junk,
                    kept => kept,
                }
            }
        };
        if !reader.buffer().contains(&b'\n') {
            return Ok(burst);
        }
    }
}

/// `get all-clients`, tried twice: a failure would otherwise leave the windows stale until the
/// next change.
fn read_clients(path: &Path) -> Option<Vec<Client>> {
    let read = || -> std::io::Result<Vec<Client>> {
        let reply = mango_request(path, "get all-clients")?;
        serde_json::from_str::<Clients>(&reply).map(|c| c.clients).map_err(std::io::Error::other)
    };
    read().or_else(|_| read()).map_err(|err| debug!("mango clients unreadable; skipped a round: {err}")).ok()
}

/// Folds one `watch all-monitors` stream into the published state.
/// ponytail: one `get all-clients` per burst, since mango has no client delta; a second
/// `watch all-clients` stream would save the round trip.
fn follow<R: Read>(
    mut stream: BufReader<R>,
    clients_path: &Path,
    publisher: &mut StatePublisher,
    windows_publisher: &mut WindowsPublisher,
    keyboard: &LayoutSink,
) -> End {
    let mut last_layout = None;
    loop {
        let monitors = match read_burst(&mut stream) {
            Ok(Burst::Snapshot(monitors)) => monitors,
            Ok(Burst::Junk) => continue,
            Ok(Burst::End) => break warn!("mango event stream ended"),
            Err(err) => break warn!("mango event stream failed: {err}"),
        };
        let Some(clients) = read_clients(clients_path) else { continue };
        let layout = monitors.iter().find(|monitor| monitor.active).map(|monitor| monitor.keyboardlayout.clone());
        if layout.is_some() && layout != last_layout {
            keyboard.write(layout.clone().unwrap_or_default(), 0, 0);
            last_layout = layout;
        }
        let rows = workspace_rows(&monitors, &clients);
        let focused = focused_window(&clients);
        let workspaces_alive = publisher.publish(&rows, focused.as_ref(), None, None);
        let windows_alive = windows_publisher.publish(window_rows(&clients));
        if !workspaces_alive && !windows_alive {
            return End::Unwanted;
        }
    }
    keyboard.write(String::new(), 0, 0);
    let workspaces_alive = publisher.publish(&[], None, None, None);
    let windows_alive = windows_publisher.publish(Vec::new());
    if workspaces_alive || windows_alive { End::Lost } else { End::Unwanted }
}

/// Connector names mango's monitor selector can carry. The selector is an unanchored PCRE2
/// pattern, so the name is anchored and its dots escaped; anything else (a comma or colon would
/// split the dispatch arguments) is refused rather than escaped.
fn monitor_selector(output: &str) -> Option<String> {
    let plain = !output.is_empty() && output.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    plain.then(|| format!("^{}$", output.replace('.', "\\.")))
}

/// `"<output>:<tag>"` back into its parts; the tag is the text after the last colon.
fn parse_workspace_id(id: &str) -> Option<(&str, u32)> {
    let (output, tag) = id.rsplit_once(':')?;
    let tag = tag.parse().ok().filter(|tag| (1..=31).contains(tag))?;
    Some((output, tag))
}

fn focus_command(id: &str) -> Option<String> {
    let (output, tag) = parse_workspace_id(id)?;
    Some(format!("viewcrossmon,{tag},{}", monitor_selector(output)?))
}

/// Windows are addressed by mango's numeric client id through the `client,<id>` suffix.
fn window_command(function: &str, id: &str) -> Option<String> {
    // mango reads the id as a C int and, for anything outside 1..=i32::MAX, acts on the focused window.
    Some(format!("{function} client,{}", id.parse::<i32>().ok().filter(|id| *id > 0)?))
}

fn dispatch_window(function: &str, id: &str) {
    match window_command(function, id) {
        Some(command) => mango_dispatch(command, "windows"),
        None => debug!("{function}({id:?}) is not a mango window id; ignored"),
    }
}

/// `$MANGO_INSTANCE_SIGNATURE`, the path of mango's IPC socket, or `None` when unset or empty.
/// mango exports it only while its socket is bound and unsets it on exit.
fn mango_socket() -> Option<PathBuf> {
    std::env::var_os("MANGO_INSTANCE_SIGNATURE").filter(|path| !path.is_empty()).map(PathBuf::from)
}

/// Longest mango line read, matching the other compositor readers; a longer one is a lost stream.
const MANGO_LINE_MAX: u64 = 64 << 20;

/// Connects and sends one newline-terminated mango command. A newline inside `command` would
/// smuggle a second command, so it is refused. A `watch` keeps the stream open for more lines.
fn mango_send(socket_path: &Path, command: &str) -> std::io::Result<UnixStream> {
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
fn mango_read_line(reader: &mut impl BufRead, max: u64) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = reader.take(max).read_until(b'\n', &mut line)?;
    if read as u64 == max && line.last() != Some(&b'\n') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "mango line over the size limit"));
    }
    Ok((read > 0).then(|| String::from_utf8_lossy(&line).into_owned()))
}

/// One mango `get` or `dispatch`: a single JSON line back, then mango closes the connection.
fn mango_request(socket_path: &Path, command: &str) -> std::io::Result<String> {
    let stream = mango_send(socket_path, command)?;
    // A stalled mango must not wedge the reader or the shared action thread.
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    Ok(mango_read_line(&mut std::io::BufReader::new(stream), MANGO_LINE_MAX)?.unwrap_or_default())
}

/// A mango `dispatch` through [`run_in_order`], logging a refusal (`{"error":...}`) at debug level.
fn mango_dispatch(command: String, capability: &'static str) {
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

/// mango over `$MANGO_INSTANCE_SIGNATURE`'s socket.
pub struct Mango;

impl Compositor for Mango {
    /// On an OS thread, so a stalled mango blocks the reader and not the caller. Also drives
    /// `mantle.windows` and `keyboard`'s layout name from the same stream.
    fn spawn_reader(
        &self,
        mut publisher: StatePublisher,
        mut windows_publisher: WindowsPublisher,
        keyboard: LayoutSink,
    ) {
        let Some(path) = mango_socket() else {
            debug!("MANGO_INSTANCE_SIGNATURE is unset or empty; workspace and window reporting disabled for this run");
            return;
        };
        std::thread::spawn(move || {
            keep_following(
                "mango",
                RETRY_FIRST,
                || mango_send(&path, "watch all-monitors").map(BufReader::new),
                |stream| follow(stream, &path, &mut publisher, &mut windows_publisher, &keyboard),
            );
        });
    }

    /// `workspaces:focus(id)`: `viewcrossmon` focuses the output, then shows only that tag.
    fn focus_workspace(&self, id: &str) {
        match focus_command(id) {
            Some(command) => mango_dispatch(command, "workspaces"),
            None => warn!("{id:?} is not a mango workspace id; ignored"),
        }
    }

    fn focus_window(&self, id: &str) {
        dispatch_window("focusid", id);
    }

    fn close_window(&self, id: &str) {
        dispatch_window("killclient", id);
    }

    fn set_fullscreen(&self, id: &str, fullscreen: bool, current: Option<bool>) {
        if toggle_needed(current, fullscreen) {
            dispatch_window("togglefullscreen", id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;

    // Trimmed from `get all-monitors` (`build_monitor_json`); tags 1 and 2 shown together on eDP-1.
    const MONITORS: &str = r#"{"monitors":[
        {"name":"DP-9","active":false,"keyboardlayout":"English (US)","tags":[
            {"index":1,"is_active":true,"is_urgent":false,"layout":"T","client_count":2},
            {"index":2,"is_active":false,"is_urgent":true,"layout":"T","client_count":1}]},
        {"name":"eDP-1","active":true,"keyboardlayout":"Arabic (Egypt)","tags":[
            {"index":1,"is_active":true,"is_urgent":false,"layout":"T","client_count":0},
            {"index":2,"is_active":true,"is_urgent":false,"layout":"T","client_count":1},
            {"index":3,"is_active":false,"is_urgent":false,"layout":"T","client_count":0}]}]}"#;

    // Trimmed from `get all-clients` (`build_client_json`).
    const CLIENTS: &str = r#"{"clients":[
        {"id":7,"title":"vim","appid":"kitty","monitor":"DP-9","tags":[1],"is_focused":false,"is_fullscreen":false,
         "is_floating":false,"is_maximized":false,"is_minimized":false,"is_urgent":false},
        {"id":4,"title":"Docs","appid":"firefox","monitor":"DP-9","tags":[1,2],"is_focused":false,"is_fullscreen":false,
         "is_floating":false,"is_maximized":false,"is_minimized":false,"is_urgent":true},
        {"id":9,"title":"Player","appid":"","monitor":"eDP-1","tags":[2],"is_focused":true,"is_fullscreen":true,
         "is_floating":true,"is_maximized":false,"is_minimized":true,"is_urgent":false},
        {"id":2,"title":"Hidden","appid":"x","monitor":"eDP-1","tags":[0],"is_focused":false}]}"#;

    fn monitors() -> Vec<Monitor> {
        serde_json::from_str::<Monitors>(MONITORS).unwrap().monitors
    }

    fn clients() -> Vec<Client> {
        serde_json::from_str::<Clients>(CLIENTS).unwrap().clients
    }

    #[test]
    fn every_tag_is_a_workspace_and_the_lowest_shown_tag_is_active() {
        let rows = workspace_rows(&monitors(), &clients());
        let ids: Vec<_> = rows.iter().map(|row| (row.id.as_str(), row.is_active, row.is_focused)).collect();
        assert_eq!(
            ids,
            [
                ("DP-9:1", true, false),
                ("DP-9:2", false, false),
                ("eDP-1:1", true, true),
                ("eDP-1:2", false, false),
                ("eDP-1:3", false, false),
            ]
        );
    }

    #[test]
    fn a_tag_reports_its_population_urgency_and_standing_window() {
        let rows = workspace_rows(&monitors(), &clients());
        let tag = |id: &str| rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!((tag("DP-9:1").app_id.as_deref(), tag("DP-9:1").window_id.as_deref()), (Some("firefox"), Some("4")));
        assert!(tag("DP-9:2").urgent && tag("DP-9:2").populated);
        assert_eq!((tag("eDP-1:2").app_id.as_deref(), tag("eDP-1:2").window_id.as_deref()), (None, Some("9")));
        assert!(!tag("eDP-1:3").populated && tag("eDP-1:3").window_id.is_none());
    }

    #[test]
    fn windows_map_their_lowest_tag_and_skip_the_special_tag() {
        let windows = window_rows(&clients());
        let ids: Vec<_> = windows.iter().map(|w| (w.id.as_str(), w.workspace_id.as_deref())).collect();
        assert_eq!(ids, [("2", None), ("4", Some("DP-9:1")), ("7", Some("DP-9:1")), ("9", Some("eDP-1:2"))]);
        let player = windows.iter().find(|w| w.id == "9").unwrap();
        assert_eq!(
            (player.fullscreen, player.floating, player.minimized, player.focused),
            (Some(true), Some(true), Some(true), true)
        );
        assert_eq!(focused_window(&clients()).map(|w| w.title), Some("Player".to_string()));
    }

    #[test]
    fn focus_targets_the_tag_on_its_own_output() {
        assert_eq!(focus_command("DP-9:2").as_deref(), Some("viewcrossmon,2,^DP-9$"));
        assert_eq!(focus_command("Virtual.1:3").as_deref(), Some("viewcrossmon,3,^Virtual\\.1$"));
    }

    #[test]
    fn ids_and_names_that_could_change_the_command_are_refused() {
        for id in ["", "7", "DP-1:0", "DP-1:32", "DP-1:x", "DP-1,2:1", "DP:1:1", "DP-1 :1", "a|b:1", "DP-1\n:1", ":1"] {
            assert_eq!(focus_command(id), None, "{id:?}");
        }
        assert_eq!(window_command("focusid", "12").as_deref(), Some("focusid client,12"));
        assert_eq!(window_command("focusid", "2147483647").as_deref(), Some("focusid client,2147483647"));
        for id in ["", "0", "-1", "1,2", "1 client,2", "2147483648", "3000000000", "1\n"] {
            assert_eq!(window_command("killclient", id), None, "{id:?}");
        }
    }

    /// Stands in for mango: one connection per reply, which is dropped unanswered when `None`.
    /// Returns the listener so a test can see whether another request arrived.
    fn serve(listener: UnixListener, replies: Vec<Option<Vec<u8>>>) -> std::thread::JoinHandle<UnixListener> {
        std::thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut command = String::new();
                BufReader::new(stream.try_clone().unwrap()).read_line(&mut command).unwrap();
                assert_eq!(command, "get all-clients\n");
                if let Some(reply) = reply {
                    stream.write_all(&reply).unwrap();
                    stream.write_all(b"\n").unwrap();
                }
            }
            listener
        })
    }

    struct Run {
        end: End,
        state: shared::state::workspaces::WorkspacesState,
        windows: shared::state::windows::WindowsState,
        layout: String,
        extra_request: bool,
    }

    /// Runs `follow` over `lines` against a fake mango answering `replies`. `listening` keeps the
    /// state receivers alive: dropped, `follow` stops at the first publish and leaves the state to
    /// inspect; kept, it runs to the end of `lines` and reports the loss.
    fn run(lines: &[u8], buffer: usize, replies: Vec<Option<Vec<u8>>>, listening: bool) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mango.sock");
        let server = serve(UnixListener::bind(&path).unwrap(), replies);
        let (ws_tx, ws_rx) = tokio::sync::mpsc::unbounded_channel();
        let (win_tx, win_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));
        let windows = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));
        let mut publisher = StatePublisher::new(state.clone(), ws_tx, crate::compositor::CompositorKind::Mango);
        let mut windows_publisher = WindowsPublisher::new(windows.clone(), win_tx, "mango");
        let keyboard = LayoutSink::default();
        let _receivers = listening.then_some((ws_rx, win_rx));

        let end =
            follow(BufReader::with_capacity(buffer, lines), &path, &mut publisher, &mut windows_publisher, &keyboard);
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (state, windows) = (state.lock().unwrap().clone(), windows.lock().unwrap().clone());
        let layout = keyboard.state.lock().unwrap().active_layout.clone();
        Run { end, state, windows, layout, extra_request: listener.accept().is_ok() }
    }

    fn monitors_with_layout(layout: &str) -> String {
        MONITORS.replace('\n', "").replace("Arabic (Egypt)", layout)
    }

    fn clients_line() -> Vec<u8> {
        CLIENTS.replace('\n', "").into_bytes()
    }

    #[test]
    fn junk_lines_are_skipped_without_a_clients_request_and_a_lost_stream_clears_everything() {
        let lines = format!("{0}\n{{\"error\":\"unknown command\"}}\n{0}\n", monitors_with_layout("Arabic (Egypt)"));
        // A one-byte buffer makes every line its own burst.
        let ran = run(lines.as_bytes(), 1, vec![Some(clients_line()), Some(clients_line())], true);

        assert_eq!(ran.end, End::Lost);
        assert!(!ran.extra_request);
        assert!(ran.state.outputs.is_empty() && ran.windows.windows.is_empty());
        assert_eq!(ran.layout, "");
    }

    #[test]
    fn a_burst_of_snapshots_costs_one_clients_request_and_publishes_the_last() {
        let lines = ["First", "Second", "Third"].map(|name| monitors_with_layout(name) + "\n").concat();
        let ran = run(lines.as_bytes(), 1 << 16, vec![Some(clients_line())], false);

        assert_eq!(ran.end, End::Unwanted);
        assert!(!ran.extra_request);
        assert_eq!(ran.layout, "Third");
        assert_eq!(ran.state.outputs.len(), 2);
    }

    #[test]
    fn a_failed_clients_request_is_retried_once() {
        let line = monitors_with_layout("Arabic (Egypt)") + "\n";
        let ran = run(line.as_bytes(), 1 << 16, vec![None, Some(clients_line())], false);

        assert_eq!(ran.end, End::Unwanted);
        assert_eq!(ran.windows.windows.len(), 4);
    }

    #[test]
    fn a_title_that_is_not_utf8_is_replaced_not_fatal() {
        let mut clients = clients_line();
        let at = clients.windows(3).position(|w| w == b"vim").unwrap();
        clients[at] = 0xff;
        let mut lines = monitors_with_layout("Arabic (Egypt)").into_bytes();
        lines.extend(b"\n");
        let ran = run(&lines, 1 << 16, vec![Some(clients)], false);

        assert_eq!(ran.end, End::Unwanted);
        assert!(ran.windows.windows.iter().any(|w| w.title == "\u{fffd}im"));
    }

    #[test]
    fn no_monitors_means_no_workspaces() {
        let Monitors { monitors } = serde_json::from_str(r#"{"monitors":[]}"#).unwrap();
        assert!(workspace_rows(&monitors, &clients()).is_empty());
    }

    #[test]
    fn the_watch_command_reaches_the_socket_as_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mango.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut stream = BufReader::new(mango_send(&path, "watch all-monitors").unwrap());
        let (mut peer, _) = listener.accept().unwrap();
        let mut command = String::new();
        BufReader::new(peer.try_clone().unwrap()).read_line(&mut command).unwrap();
        assert_eq!(command, "watch all-monitors\n");

        peer.write_all(b"{\"monitors\":[]}\n").unwrap();
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"monitors\":[]}\n");
        assert!(mango_send(&path, "watch x\nget version").is_err());
    }

    #[test]
    fn a_mango_line_over_the_limit_is_an_error_and_bad_utf8_is_replaced() {
        let read = |bytes: &[u8]| mango_read_line(&mut std::io::BufReader::new(bytes), 8);
        assert_eq!(read(b"0123456\n").unwrap().as_deref(), Some("0123456\n"));
        assert_eq!(read(b"01234567\n").unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(read(b"a\xffb").unwrap().as_deref(), Some("a\u{fffd}b"));
        assert_eq!(read(b"").unwrap(), None);
    }
}

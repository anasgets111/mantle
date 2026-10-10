//! `workspaces`' sway implementor, reached over `$SWAYSOCK` (i3-compatible IPC).
//!
//! The only file that knows sway's JSON for workspaces and the tree. `controller.rs` owns payload,
//! reduction and publish in terms of `WorkspaceRow`/`FocusedWindow`; this maps sway's replies and
//! drives the loop. A workspace's `id` is its name: sway focuses and moves by name, and a numeric
//! id would not survive a rename or reorder.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::Deserialize;
use shared::{debug, warn};

use super::controller::{FocusedWindow, StatePublisher, WorkspaceRow};
use super::niri::{End, keep_following};
use crate::capabilities::RETRY_FIRST;
use crate::capabilities::keyboard::layout::LayoutSink;
use crate::capabilities::windows::controller::{StatePublisher as WindowsPublisher, WindowEntry};
use crate::compositor::{
    SWAY_GET_TREE, SWAY_GET_WORKSPACES, SWAY_SUBSCRIBE, sway_command, sway_connect, sway_read, sway_request,
    sway_socket, sway_write,
};

const EVENT_INPUT: u32 = (1 << 31) | 21;

/// `GET_WORKSPACES` entry (`ipc_json_describe_workspace`, `ipc-server.c`).
#[derive(Debug, Deserialize)]
struct SwayWorkspace {
    name: String,
    /// `-1` when the name does not start with a number.
    num: i64,
    output: Option<String>,
    #[serde(default)]
    visible: bool,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    urgent: bool,
}

/// `GET_TREE` node (`ipc_json_describe_node`); only the fields windows need.
#[derive(Debug, Deserialize)]
struct Node {
    id: u64,
    name: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    urgent: bool,
    app_id: Option<String>,
    window_properties: Option<WindowProperties>,
    #[serde(default)]
    fullscreen_mode: u8,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    floating_nodes: Vec<Node>,
}

/// Xwayland windows have no `app_id`; their class stands in.
#[derive(Debug, Deserialize)]
struct WindowProperties {
    class: Option<String>,
}

/// Scratchpad windows live here, on the `__i3` output.
const SCRATCH: &str = "__i3_scratch";

/// Every view in tree order; floating is inherited by a floating container's children.
fn collect_windows(
    node: &Node,
    output: Option<&str>,
    workspace: Option<&str>,
    floating: bool,
    out: &mut Vec<WindowEntry>,
) {
    let (mut output, mut workspace) = (output, workspace);
    match node.kind.as_str() {
        "output" => output = node.name.as_deref().filter(|name| *name != "__i3"),
        "workspace" => workspace = node.name.as_deref().filter(|name| *name != SCRATCH),
        _ => {}
    }
    let floating = floating || node.kind == "floating_con";
    let is_view =
        matches!(node.kind.as_str(), "con" | "floating_con") && node.nodes.is_empty() && node.floating_nodes.is_empty();
    if is_view {
        let app_id = node
            .app_id
            .clone()
            .or_else(|| node.window_properties.as_ref().and_then(|props| props.class.clone()))
            .unwrap_or_default();
        out.push(WindowEntry {
            id: node.id.to_string(),
            title: node.name.clone().unwrap_or_default(),
            app_id,
            workspace_id: workspace.map(str::to_string),
            output: workspace.and(output).map(str::to_string),
            focused: node.focused,
            floating: Some(floating),
            fullscreen: Some(node.fullscreen_mode != 0),
            minimized: None,
            maximized: None,
            urgent: node.urgent,
        });
    }
    for child in node.nodes.iter().chain(&node.floating_nodes) {
        collect_windows(child, output, workspace, floating, out);
    }
}

fn parse_windows(json: &[u8]) -> serde_json::Result<Vec<WindowEntry>> {
    let root: Node = serde_json::from_slice(json)?;
    let mut windows = Vec::new();
    collect_windows(&root, None, None, false, &mut windows);
    Ok(windows)
}

/// `populated`/`app_id` (ADR-0117): the focused window on the workspace, else the first in tree order.
fn workspace_rows(workspaces: &[SwayWorkspace], windows: &[WindowEntry]) -> Vec<WorkspaceRow> {
    workspaces
        .iter()
        .map(|workspace| {
            let standing = windows
                .iter()
                .filter(|window| window.workspace_id.as_deref() == Some(workspace.name.as_str()))
                .min_by_key(|window| !window.focused);
            WorkspaceRow {
                id: workspace.name.clone(),
                number: u32::try_from(workspace.num).ok(),
                name: Some(workspace.name.clone()),
                output: workspace.output.clone().filter(|output| !output.is_empty()),
                is_active: workspace.visible,
                is_focused: workspace.focused,
                populated: standing.is_some(),
                app_id: standing.map(|w| w.app_id.clone()).filter(|id| !id.is_empty()),
                window_id: standing.map(|w| w.id.clone()),
                urgent: workspace.urgent,
            }
        })
        .collect()
}

fn focused_window(windows: &[WindowEntry]) -> Option<FocusedWindow> {
    windows.iter().find(|window| window.focused).map(|window| FocusedWindow {
        title: window.title.clone(),
        app_id: window.app_id.clone(),
        floating: window.floating.unwrap_or(false),
        fullscreen: window.fullscreen,
    })
}

/// Re-reads workspaces and the tree on fresh connections and publishes. `false` once nobody listens.
fn refresh(
    path: &Path,
    publisher: &mut StatePublisher,
    windows_publisher: &mut WindowsPublisher,
) -> std::io::Result<bool> {
    let workspaces: Vec<SwayWorkspace> =
        serde_json::from_slice(&sway_request(path, SWAY_GET_WORKSPACES, "")?).map_err(std::io::Error::other)?;
    let windows = parse_windows(&sway_request(path, SWAY_GET_TREE, "")?).map_err(std::io::Error::other)?;
    let rows = workspace_rows(&workspaces, &windows);
    let workspaces_alive = publisher.publish(&rows, focused_window(&windows).as_ref(), None, None);
    let windows_alive = windows_publisher.publish(windows);
    Ok(workspaces_alive || windows_alive)
}

/// Connects and subscribes; a refusal is an error so [`keep_following`] retries.
fn connect(path: &Path) -> std::io::Result<UnixStream> {
    let mut stream = sway_connect(path)?;
    sway_write(&mut stream, SWAY_SUBSCRIBE, br#"["workspace","window","input"]"#)?;
    let (_, reply) = sway_read(&mut stream)?;
    if serde_json::from_slice::<serde_json::Value>(&reply).map_err(std::io::Error::other)?["success"] != true {
        return Err(std::io::Error::other("sway refused the subscription"));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// What one burst of events asks for.
#[derive(Debug, PartialEq)]
struct Burst {
    rows: bool,
    layout: bool,
}

/// Whether `buffered` holds a whole frame (14-byte header, then the payload it announces).
fn frame_buffered(buffered: &[u8]) -> bool {
    buffered.len() >= 14
        && buffered.len() - 14 >= u32::from_ne_bytes(buffered[6..10].try_into().expect("4 bytes")) as usize
}

/// Blocks for one event, then takes every whole frame already buffered.
fn read_burst<R: std::io::Read>(reader: &mut BufReader<R>) -> std::io::Result<Burst> {
    let mut burst = Burst { rows: false, layout: false };
    loop {
        match sway_read(reader)?.0 {
            EVENT_INPUT => burst.layout = true,
            _ => burst.rows = true,
        }
        if !frame_buffered(reader.buffer()) {
            return Ok(burst);
        }
    }
}

fn follow(
    stream: UnixStream,
    path: &Path,
    publisher: &mut StatePublisher,
    windows_publisher: &mut WindowsPublisher,
    keyboard: &LayoutSink,
) -> End {
    let mut reader = BufReader::new(stream);
    keyboard.read_sway(path);
    let mut burst = Burst { rows: true, layout: false };
    let err = loop {
        if burst.rows {
            match refresh(path, publisher, windows_publisher) {
                Ok(true) => {}
                Ok(false) => return End::Unwanted,
                Err(err) => break err,
            }
        }
        if burst.layout {
            keyboard.read_sway(path);
        }
        match read_burst(&mut reader) {
            Ok(next) => burst = next,
            Err(err) => break err,
        }
    };
    warn!("sway connection lost: {err}");
    let workspaces_alive = publisher.publish(&[], None, None, None);
    let windows_alive = windows_publisher.publish(Vec::new());
    if workspaces_alive || windows_alive { End::Lost } else { End::Unwanted }
}

/// Runs [`keep_following`] on an OS thread; also drives `mantle.windows` and `keyboard`'s layout.
pub fn spawn_reader(mut publisher: StatePublisher, mut windows_publisher: WindowsPublisher, keyboard: LayoutSink) {
    let Some(path) = sway_socket() else {
        debug!("SWAYSOCK is unset; workspace and window reporting disabled for this run");
        return;
    };
    std::thread::spawn(move || {
        keep_following(
            "sway",
            RETRY_FIRST,
            || connect(&path),
            |stream| follow(stream, &path, &mut publisher, &mut windows_publisher, &keyboard),
        );
    });
}

/// Words sway's `workspace` and `move` read as relative targets even when quoted (`strcasecmp`).
const KEYWORDS: &[&str] =
    &["number", "next", "prev", "next_on_output", "prev_on_output", "back_and_forth", "current", "output", "gaps"];

/// `"name"` for a command line. `execute_command` strips quotes without unescaping, and expands
/// `$var`, so a name holding `"`, `\` or `$` cannot be sent faithfully.
/// ponytail: such names, sway's keywords and `--flags` cannot be targeted; sway offers no escape.
fn quoted_workspace(name: &str) -> Option<String> {
    let keyword = KEYWORDS.iter().any(|word| name.eq_ignore_ascii_case(word));
    if name.is_empty() || keyword || name.starts_with("--") || name.contains(['"', '\\', '$']) {
        warn!("{name:?} cannot be addressed in a sway command; ignored");
        return None;
    }
    Some(format!("\"{name}\""))
}

/// `[con_id=N]` criteria; the id is parsed so a caller's string cannot add commands.
fn criteria(id: &str) -> Option<String> {
    let parsed = id.parse::<u64>().ok().map(|id| format!("[con_id={id}]"));
    if parsed.is_none() {
        debug!("{id:?} is not a sway window id; ignored");
    }
    parsed
}

/// `--no-auto-back-and-forth` keeps a user's `workspace_auto_back_and_forth` from undoing a focus.
fn focus_workspace_command(id: &str) -> Option<String> {
    Some(format!("workspace --no-auto-back-and-forth {}", quoted_workspace(id)?))
}

fn window_command(id: &str, action: &str) -> Option<String> {
    Some(format!("{} {action}", criteria(id)?))
}

fn move_command(id: &str, workspace_id: &str) -> Option<String> {
    window_command(
        id,
        &format!("move --no-auto-back-and-forth container to workspace {}", quoted_workspace(workspace_id)?),
    )
}

fn send(command: Option<String>, capability: &'static str) {
    if let Some(command) = command {
        sway_command(command, capability);
    }
}

/// `workspaces:focus(id)`; sway creates a workspace it does not have.
pub fn focus(id: &str) {
    send(focus_workspace_command(id), "workspaces");
}

pub fn focus_window(id: &str) {
    send(window_command(id, "focus"), "windows");
}

pub fn close_window(id: &str) {
    send(window_command(id, "kill"), "windows");
}

/// Explicit `enable`/`disable`, so unlike a toggle it needs no read of the current state.
pub fn set_fullscreen(id: &str, fullscreen: bool) {
    send(window_command(id, if fullscreen { "fullscreen enable" } else { "fullscreen disable" }), "windows");
}

pub fn move_window_to_workspace(id: &str, workspace_id: &str) {
    send(move_command(id, workspace_id), "windows");
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_WINDOW: u32 = (1 << 31) | 3;
    use crate::compositor::SWAY_GET_INPUTS;
    use std::sync::{Arc, Mutex};

    // Shapes follow `ipc_json_describe_workspace`/`_node`/`_view` and `ipc-server.c` get_workspaces.
    const WORKSPACES: &str = r#"[
        {"id":3,"type":"workspace","name":"1:web","num":1,"output":"eDP-1","visible":true,"focused":true,"urgent":false,"nodes":[],"floating_nodes":[]},
        {"id":4,"type":"workspace","name":"scratch \"x\"","num":-1,"output":"eDP-1","visible":false,"focused":false,"urgent":true},
        {"id":5,"type":"workspace","name":"2","num":2,"output":"DP-2","visible":true,"focused":false,"urgent":false}
    ]"#;

    const TREE: &str = r#"{"id":1,"type":"root","name":"root","nodes":[
      {"id":9,"type":"output","name":"__i3","nodes":[{"id":10,"type":"workspace","name":"__i3_scratch","nodes":[],
        "floating_nodes":[{"id":40,"type":"floating_con","name":"hidden","app_id":"pavucontrol","fullscreen_mode":0,"focused":false,"urgent":false,"nodes":[],"floating_nodes":[]}]}]},
      {"id":2,"type":"output","name":"eDP-1","nodes":[
        {"id":3,"type":"workspace","name":"1:web","nodes":[
          {"id":20,"type":"con","name":null,"nodes":[
            {"id":21,"type":"con","name":"Docs","app_id":"firefox","fullscreen_mode":0,"focused":false,"urgent":false,"nodes":[],"floating_nodes":[]},
            {"id":22,"type":"con","name":"term","app_id":"kitty","fullscreen_mode":0,"focused":true,"urgent":false,"nodes":[],"floating_nodes":[]}
          ],"floating_nodes":[]}],
         "floating_nodes":[
          {"id":23,"type":"floating_con","name":"calc","app_id":null,"window_properties":{"class":"Qalculate","instance":"q"},"fullscreen_mode":0,"focused":false,"urgent":true,"nodes":[],"floating_nodes":[]}]},
        {"id":4,"type":"workspace","name":"scratch \"x\"","nodes":[],"floating_nodes":[]}]},
      {"id":6,"type":"output","name":"DP-2","nodes":[
        {"id":5,"type":"workspace","name":"2","nodes":[
          {"id":30,"type":"con","name":"movie","app_id":"mpv","fullscreen_mode":1,"focused":false,"urgent":false,"nodes":[],"floating_nodes":[]}],
         "floating_nodes":[]}]}
    ],"floating_nodes":[]}"#;

    #[test]
    fn the_tree_yields_every_view_with_its_workspace_output_floating_and_fullscreen() {
        let windows = parse_windows(TREE.as_bytes()).unwrap();

        let summary: Vec<_> = windows
            .iter()
            .map(|w| {
                (
                    w.id.as_str(),
                    w.app_id.as_str(),
                    w.workspace_id.as_deref(),
                    w.output.as_deref(),
                    w.floating,
                    w.fullscreen,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("40", "pavucontrol", None, None, Some(true), Some(false)),
                ("21", "firefox", Some("1:web"), Some("eDP-1"), Some(false), Some(false)),
                ("22", "kitty", Some("1:web"), Some("eDP-1"), Some(false), Some(false)),
                ("23", "Qalculate", Some("1:web"), Some("eDP-1"), Some(true), Some(false)),
                ("30", "mpv", Some("2"), Some("DP-2"), Some(false), Some(true)),
            ]
        );
        assert!(windows[3].urgent && windows[2].focused);
        assert_eq!(focused_window(&windows).unwrap().app_id, "kitty");
    }

    #[test]
    fn workspace_rows_use_the_name_as_id_and_pick_the_focused_window_as_standing() {
        let workspaces: Vec<SwayWorkspace> = serde_json::from_str(WORKSPACES).unwrap();
        let rows = workspace_rows(&workspaces, &parse_windows(TREE.as_bytes()).unwrap());

        assert_eq!(rows[0].id, "1:web");
        assert_eq!((rows[0].number, rows[1].number), (Some(1), None));
        assert!(rows[0].is_active && rows[0].is_focused && rows[0].populated);
        assert_eq!((rows[0].app_id.as_deref(), rows[0].window_id.as_deref()), (Some("kitty"), Some("22")));
        assert!(!rows[1].populated && rows[1].urgent);
        assert_eq!(rows[2].output.as_deref(), Some("DP-2"));
    }

    #[test]
    fn commands_carry_the_no_auto_back_and_forth_flag_and_a_quoted_name() {
        assert_eq!(focus_workspace_command("1:web").unwrap(), r#"workspace --no-auto-back-and-forth "1:web""#);
        assert_eq!(window_command("17", "focus").unwrap(), "[con_id=17] focus");
        assert_eq!(window_command("17", "kill").unwrap(), "[con_id=17] kill");
        assert_eq!(
            move_command("17", "web").unwrap(),
            r#"[con_id=17] move --no-auto-back-and-forth container to workspace "web""#
        );
        assert_eq!(window_command("5; exec x", "kill"), None);
    }

    #[test]
    fn ids_sway_cannot_take_literally_are_refused() {
        for id in [r#"a"b"#, r"a\b", "$x", "Current", "NEXT", "back_and_forth", "gaps", "--x", ""] {
            assert_eq!(focus_workspace_command(id), None, "{id:?}");
            assert_eq!(move_command("1", id), None, "{id:?}");
        }
    }

    #[test]
    fn a_burst_of_frames_already_buffered_asks_for_one_refresh() {
        let mut queued = Vec::new();
        for kind in [EVENT_WINDOW, EVENT_WINDOW, EVENT_INPUT] {
            sway_write(&mut queued, kind, br#"{"change":"title"}"#).unwrap();
        }
        let mut reader = BufReader::new(std::io::Cursor::new(queued));

        assert_eq!(read_burst(&mut reader).unwrap(), Burst { rows: true, layout: true });
        assert_eq!(read_burst(&mut reader).unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn an_input_only_burst_leaves_the_rows_alone() {
        let mut queued = Vec::new();
        sway_write(&mut queued, EVENT_INPUT, b"{}").unwrap();

        let burst = read_burst(&mut BufReader::new(std::io::Cursor::new(queued))).unwrap();

        assert_eq!(burst, Burst { rows: false, layout: true });
    }

    #[test]
    fn follow_subscribes_publishes_from_replies_and_refreshes_on_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sway.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let mut subscription = None;
            let mut subscribed_to = Vec::new();
            // connect, inputs, workspaces, tree, then the same two refreshes after the event.
            for _ in 0..6 {
                let (mut conn, _) = listener.accept().unwrap();
                let (kind, payload) = sway_read(&mut conn).unwrap();
                let reply: &[u8] = match kind {
                    SWAY_SUBSCRIBE => {
                        subscribed_to = payload;
                        br#"{"success":true}"#
                    }
                    SWAY_GET_WORKSPACES => WORKSPACES.as_bytes(),
                    SWAY_GET_TREE => TREE.as_bytes(),
                    SWAY_GET_INPUTS => br#"[{"name":"kbd","type":"keyboard","xkb_layout_names":["English (US)"],"xkb_active_layout_index":0,"xkb_active_layout_name":"English (US)"}]"#,
                    other => panic!("unexpected request {other}"),
                };
                sway_write(&mut conn, kind, reply).unwrap();
                if kind == SWAY_SUBSCRIBE {
                    sway_write(&mut conn, EVENT_WINDOW, br#"{"change":"title"}"#).unwrap();
                    subscription = Some(conn);
                }
            }
            drop(subscription);
            subscribed_to
        });

        let (ws_tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel();
        let (win_tx, _win_rx) = tokio::sync::mpsc::unbounded_channel();
        let ws_state = Arc::new(Mutex::new(Default::default()));
        let mut publisher = StatePublisher::new(Arc::clone(&ws_state), ws_tx, crate::compositor::CompositorKind::Sway);
        let mut windows_publisher = WindowsPublisher::new(Arc::default(), win_tx, "sway");
        let keyboard = LayoutSink::default();

        let stream = connect(&path).unwrap();
        let end = follow(stream, &path, &mut publisher, &mut windows_publisher, &keyboard);

        assert_eq!(end, End::Lost);
        let subscribed = server.join().unwrap();
        assert_eq!(subscribed, br#"["workspace","window","input"]"#);
        assert!(ws_rx.try_recv().is_ok(), "the first read published workspaces");
        let layout = keyboard.state.lock().unwrap().clone();
        assert_eq!((layout.active_layout.as_str(), layout.layout_count), ("English (US)", 1));
    }
}

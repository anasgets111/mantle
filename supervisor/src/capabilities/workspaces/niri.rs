//! `workspaces`' niri implementor, reached over `$NIRI_SOCKET`.
//!
//! The only file naming `niri_ipc`. `controller.rs` owns payload, reduction, and publish in terms
//! of `WorkspaceRow`/`FocusedWindow`; this maps niri types and drives the loop.

use std::collections::HashMap;
use std::time::Duration;

use shared::{debug, warn};

use super::controller::{FocusedWindow, StatePublisher, WorkspaceRow};
use crate::capabilities::keyboard::layout::LayoutSink;
use crate::capabilities::windows::controller::{StatePublisher as WindowsPublisher, WindowEntry};
use crate::capabilities::{RETRY_FIRST, RETRY_MAX, STABLE};

/// niri workspaces reduced to the common input. Clone `name` and `output` per event; a session has
/// only a handful of workspaces.
///
/// `populated`/`app_id` (ADR-0117) use `Window.workspace_id`: focused window id when focused
/// there, otherwise the lowest id because the map has no order and the compositor's first tile is
/// not on the wire. Empty `app_id` is `None`.
fn workspace_rows(
    workspaces: &HashMap<u64, niri_ipc::Workspace>,
    windows: &HashMap<u64, niri_ipc::Window>,
) -> Vec<WorkspaceRow> {
    workspaces
        .values()
        .map(|workspace| {
            let standing = windows
                .values()
                .filter(|window| window.workspace_id == Some(workspace.id))
                .min_by_key(|window| (!window.is_focused, window.id));
            WorkspaceRow {
                id: workspace.id.to_string(),
                number: Some(u32::from(workspace.idx)),
                name: workspace.name.clone(),
                output: workspace.output.clone(),
                is_active: workspace.is_active,
                is_focused: workspace.is_focused,
                populated: standing.is_some(),
                app_id: standing.and_then(|window| window.app_id.clone()).filter(|id| !id.is_empty()),
                window_id: standing.map(|w| w.id.to_string()),
                urgent: workspace.is_urgent,
            }
        })
        .collect()
}

/// Every niri window, for `windows`. `output` joins through `workspace_id`: niri's `Window` has no
/// output field of its own. Sorted by ascending window id, since niri's ids climb monotonically
/// and the source map has no order.
fn window_rows(
    windows: &HashMap<u64, niri_ipc::Window>,
    workspaces: &HashMap<u64, niri_ipc::Workspace>,
) -> Vec<WindowEntry> {
    let mut ordered: Vec<&niri_ipc::Window> = windows.values().collect();
    ordered.sort_by_key(|window| window.id);
    ordered
        .into_iter()
        .map(|window| WindowEntry {
            id: window.id.to_string(),
            title: window.title.clone().unwrap_or_default(),
            app_id: window.app_id.clone().unwrap_or_default(),
            workspace_id: window.workspace_id.map(|id| id.to_string()),
            output: window.workspace_id.and_then(|id| workspaces.get(&id)).and_then(|ws| ws.output.clone()),
            focused: window.is_focused,
            floating: Some(window.is_floating),
            fullscreen: None,
            minimized: None,
            maximized: None,
            urgent: window.is_urgent,
        })
        .collect()
}

/// niri flags focus on each window, so search here rather than in `derive_state`. Clone only the
/// winner; even a fifty-window session builds one `FocusedWindow` per event.
///
/// Wire `title`/`app_id` are `Option` but the payload makes them non-nullable, so default to empty.
/// A window reporting neither is still a real toplevel.
fn focused_window(windows: &HashMap<u64, niri_ipc::Window>) -> Option<FocusedWindow> {
    windows.values().find(|window| window.is_focused).map(|window| FocusedWindow {
        title: window.title.clone().unwrap_or_default(),
        app_id: window.app_id.clone().unwrap_or_default(),
        floating: window.is_floating,
        // niri-ipc 26.4.0 has no fullscreen field (ADR-0056 decision 5); absent, not `false`.
        fullscreen: None,
    })
}

/// niri's three state parts, reset together.
#[derive(Default)]
struct Parts {
    workspaces: niri_ipc::state::WorkspacesState,
    windows: niri_ipc::state::WindowsState,
    overview: niri_ipc::state::OverviewState,
}

impl Parts {
    /// Whether `niri_ipc::state` would `.expect`-panic on `event`: it names a workspace or window
    /// these parts never saw. Release builds abort on panic, so this is checked, not caught.
    ///
    /// ponytail: matches the `.expect` sites of niri_ipc 26.4.0; re-audit `niri_ipc::state` on upgrade.
    fn desynced(&self, event: &niri_ipc::Event) -> bool {
        use niri_ipc::Event::*;
        match event {
            WorkspaceActivated { id, .. } => !self.workspaces.workspaces.contains_key(id),
            WorkspaceActiveWindowChanged { workspace_id, .. } => !self.workspaces.workspaces.contains_key(workspace_id),
            WindowClosed { id } => !self.windows.windows.contains_key(id),
            WindowLayoutsChanged { changes } => changes.iter().any(|(id, _)| !self.windows.windows.contains_key(id)),
            _ => false,
        }
    }

    /// `EventStreamStatePart::apply` returns ignored events, so one `let` chain passes each event
    /// down the parts that did not want it. `false` leaves the parts untouched when
    /// [`Self::desynced`]; the caller starts over on a fresh stream.
    fn apply(&mut self, event: niri_ipc::Event) -> bool {
        use niri_ipc::state::EventStreamStatePart;
        if self.desynced(&event) {
            return false;
        }
        if let Some(event) = self.workspaces.apply(event)
            && let Some(event) = self.windows.apply(event)
        {
            self.overview.apply(event);
        }
        true
    }
}

/// Why [`follow`] stopped.
#[derive(Debug, PartialEq)]
enum End {
    /// The stream ended or its state desynced; the published state is cleared and a fresh stream's
    /// replay rebuilds every part.
    Lost,
    /// Nobody listens to either publisher.
    Unwanted,
}

/// Folds one event stream into the published state.
fn follow(
    socket: niri_ipc::socket::Socket,
    publisher: &mut StatePublisher,
    windows_publisher: &mut WindowsPublisher,
    keyboard: &LayoutSink,
) -> End {
    let mut read_event = socket.read_events();
    let mut parts = Parts::default();
    let mut layout_names = Vec::new();
    let lost = |publisher: &mut StatePublisher, windows_publisher: &mut WindowsPublisher| {
        let workspaces_alive = publisher.publish(&[], None, None, Some(false));
        let windows_alive = windows_publisher.publish(Vec::new());
        if workspaces_alive || windows_alive { End::Lost } else { End::Unwanted }
    };
    loop {
        let event = match read_event() {
            Ok(event) => event,
            Err(err) if undecodable(&err) => {
                warn!("skipped a niri event this niri-ipc cannot decode: {err}");
                continue;
            }
            Err(err) => {
                warn!("niri event stream ended: {err}");
                return lost(publisher, windows_publisher);
            }
        };
        if keyboard.apply_niri(&mut layout_names, &event) {
            continue;
        }
        // No published row reads layouts or focus timestamps.
        let moves_rows = !matches!(
            event,
            niri_ipc::Event::WindowLayoutsChanged { .. } | niri_ipc::Event::WindowFocusTimestampChanged { .. }
        );
        if !parts.apply(event) {
            warn!("niri event state went out of sync; restarting the event stream");
            return lost(publisher, windows_publisher);
        }
        if !moves_rows {
            continue;
        }

        let rows = workspace_rows(&parts.workspaces.workspaces, &parts.windows.windows);
        let focused = focused_window(&parts.windows.windows);
        let workspaces_alive = publisher.publish(&rows, focused.as_ref(), None, Some(parts.overview.is_open));
        let windows_alive =
            windows_publisher.publish(window_rows(&parts.windows.windows, &parts.workspaces.workspaces));
        if !workspaces_alive && !windows_alive {
            return End::Unwanted;
        }
    }
}

/// Connects, runs `follow`, and reconnects after each loss with [`RETRY_FIRST`] doubling to
/// [`RETRY_MAX`] like the audio mixer, until it reports [`End::Unwanted`]. The first connect retries
/// too. `first_delay` is a parameter for tests.
/// ponytail: a compositor that never comes up keeps one thread retrying every 30 s until exit; stop once the publishers are gone.
fn keep_following<S>(
    first_delay: Duration,
    mut connect: impl FnMut() -> std::io::Result<S>,
    mut follow: impl FnMut(S) -> End,
) {
    let mut delay = first_delay;
    loop {
        let mut failures = 0;
        let stream = loop {
            match connect() {
                Ok(stream) => break stream,
                Err(err) if failures == 0 => warn!("cannot reach niri ({err}); retrying"),
                Err(err) => debug!("cannot reach niri ({err}); retrying in {delay:?}"),
            }
            failures += 1;
            std::thread::sleep(delay);
            delay = (delay * 2).min(RETRY_MAX);
        };
        let started = std::time::Instant::now();
        if follow(stream) == End::Unwanted {
            return;
        }
        if started.elapsed() >= STABLE {
            delay = first_delay;
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(RETRY_MAX);
    }
}

/// On an OS thread, connects, requests the event stream and runs [`keep_following`], so a stalled
/// niri blocks the reader and not the caller. Also drives `mantle.windows` and `keyboard`'s layout
/// from the same stream, rather than a second connection.
pub fn spawn_reader(mut publisher: StatePublisher, mut windows_publisher: WindowsPublisher, keyboard: LayoutSink) {
    std::thread::spawn(move || {
        keep_following(RETRY_FIRST, crate::compositor::niri_event_stream, |socket| {
            follow(socket, &mut publisher, &mut windows_publisher, &keyboard)
        });
    });
}

/// A line niri sent but niri-ipc can't decode (a newer niri's event kind or shape), already
/// consumed from the stream; socket end reads as `UnexpectedEof`, so it stays fatal.
fn undecodable(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::InvalidData
}

/// `workspaces:focus(id)`. `WorkspaceReferenceArg::Id`, not `Index`: `number` shifts on reorder and
/// could focus the wrong workspace.
pub fn focus(id: &str) {
    let Some(reference) = workspace_reference(id) else { return };
    crate::compositor::niri_action(niri_ipc::Action::FocusWorkspace { reference }, "workspaces");
}

fn workspace_reference(id: &str) -> Option<niri_ipc::WorkspaceReferenceArg> {
    let parsed = id.parse::<u64>().ok().map(niri_ipc::WorkspaceReferenceArg::Id);
    if parsed.is_none() {
        warn!("{id:?} is not a niri workspace id; ignored");
    }
    parsed
}

pub fn focus_window(id: &str) {
    let Ok(id) = id.parse::<u64>() else {
        debug!("focus({id:?}) is not a niri window id; ignored");
        return;
    };
    crate::compositor::niri_action(niri_ipc::Action::FocusWindow { id }, "windows");
}

pub fn close_window(id: &str) {
    let Ok(id) = id.parse::<u64>() else {
        debug!("close({id:?}) is not a niri window id; ignored");
        return;
    };
    crate::compositor::niri_action(niri_ipc::Action::CloseWindow { id: Some(id) }, "windows");
}

pub fn move_window_to_workspace(id: &str, workspace_id: &str) {
    let Some(reference) = workspace_reference(workspace_id) else { return };
    let Ok(id) = id.parse::<u64>() else {
        debug!("move_window_to_workspace({id:?}, {workspace_id}) is not a niri window id; ignored");
        return;
    };
    crate::compositor::niri_action(
        niri_ipc::Action::MoveWindowToWorkspace { window_id: Some(id), reference, focus: false },
        "windows",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures deserialize niri wire JSON, copied from live `niri msg -j workspaces`/`-j windows`
    /// rather than struct literals. A renamed field breaks them; the wire contract is the niri
    /// upgrade boundary, so tests stay with the adaptor.
    fn workspace(id: u64, idx: u8, output: &str, is_active: bool, is_focused: bool) -> niri_ipc::Workspace {
        serde_json::from_value(serde_json::json!({
            "id": id, "idx": idx, "name": null, "output": output,
            "is_urgent": false, "is_active": is_active, "is_focused": is_focused, "active_window_id": null
        }))
        .unwrap()
    }

    fn window(id: u64, title: &str, app_id: &str, is_focused: bool, is_floating: bool) -> niri_ipc::Window {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": title, "app_id": app_id, "pid": 1481, "workspace_id": 5,
            "is_focused": is_focused, "is_floating": is_floating, "is_urgent": false,
            "layout": {
                "pos_in_scrolling_layout": [3, 1], "tile_size": [1920.0, 1200.0], "window_size": [1920, 1200],
                "tile_pos_in_workspace_view": null, "window_offset_in_tile": [0.0, 0.0]
            },
            "focus_timestamp": null
        }))
        .unwrap()
    }

    fn map<T>(items: Vec<(u64, T)>) -> HashMap<u64, T> {
        items.into_iter().collect()
    }

    #[test]
    fn workspace_rows_carry_every_field_the_reduction_reads() {
        let rows = workspace_rows(
            &map(vec![(5, workspace(5, 2, "eDP-1", true, true))]),
            &map(vec![(2, window(2, "src/main.rs - Neovim", "kitty", true, true))]),
        );

        assert_eq!(
            rows,
            vec![WorkspaceRow {
                id: "5".to_string(),
                number: Some(2),
                name: None,
                output: Some("eDP-1".to_string()),
                is_active: true,
                is_focused: true,
                populated: true,
                app_id: Some("kitty".to_string()),
                window_id: Some("2".to_string()),
                urgent: false,
            }]
        );
    }

    #[test]
    fn workspace_rows_stand_a_workspace_in_by_its_focused_window_else_its_lowest_id() {
        let workspaces =
            map(vec![(5, workspace(5, 1, "eDP-1", true, true)), (6, workspace(6, 2, "eDP-1", false, false))]);
        let mut elsewhere = window(30, "Sign in | Slack", "slack", false, false);
        elsewhere.workspace_id = Some(6);
        let windows = map(vec![
            (14, window(14, "Inbox", "thunderbird", false, false)),
            (2, window(2, "src/main.rs - Neovim", "kitty", true, true)),
            (30, elsewhere),
        ]);

        let rows = workspace_rows(&workspaces, &windows);
        let app_of = |id: u64| rows.iter().find(|row| row.id == id.to_string()).unwrap().app_id.clone();
        let window_of = |id: u64| rows.iter().find(|row| row.id == id.to_string()).unwrap().window_id.clone();

        assert_eq!(app_of(5).as_deref(), Some("kitty"), "focus wins over a lower id");
        assert_eq!(window_of(5).as_deref(), Some("2"));
        assert_eq!(app_of(6).as_deref(), Some("slack"));
        assert_eq!(window_of(6).as_deref(), Some("30"));

        let mut unfocused = windows.clone();
        unfocused.get_mut(&2).unwrap().is_focused = false;
        let rows = workspace_rows(&workspaces, &unfocused);
        assert_eq!(rows.iter().find(|row| row.id == "5").unwrap().app_id.as_deref(), Some("kitty"), "lowest id");
        assert_eq!(rows.iter().find(|row| row.id == "5").unwrap().window_id.as_deref(), Some("2"));
    }

    #[test]
    fn urgency_passes_through_from_niri_workspaces_and_windows() {
        let mut ws = workspace(5, 1, "eDP-1", true, true);
        ws.is_urgent = true;
        let mut win = window(2, "Inbox", "thunderbird", false, false);
        win.is_urgent = true;
        let (workspaces, windows) = (map(vec![(5, ws)]), map(vec![(2, win)]));

        assert!(workspace_rows(&workspaces, &windows)[0].urgent);
        assert!(window_rows(&windows, &workspaces)[0].urgent);
    }

    #[test]
    fn workspace_rows_mark_an_empty_workspace_unpopulated_with_no_app_id() {
        let mut nameless = window(2, "", "", false, false);
        nameless.app_id = None;
        let rows = workspace_rows(
            &map(vec![(5, workspace(5, 1, "eDP-1", true, true)), (6, workspace(6, 2, "eDP-1", false, false))]),
            &map(vec![(2, nameless)]),
        );
        let row_of = |id: u64| rows.iter().find(|row| row.id == id.to_string()).unwrap();

        assert_eq!(
            (row_of(5).populated, row_of(5).app_id.as_deref(), row_of(5).window_id.as_deref()),
            (true, None, Some("2")),
            "a window with no id still populates"
        );
        assert_eq!(
            (row_of(6).populated, row_of(6).app_id.as_deref(), row_of(6).window_id.as_deref()),
            (false, None, None)
        );
    }

    #[test]
    fn workspace_rows_keep_a_workspace_niri_reports_no_output_for() {
        // `derive_state` drops it; the adaptor reports niri's value. niri sets `output: null` with
        // no connected outputs.
        let mut orphan = workspace(1, 1, "eDP-1", true, true);
        orphan.output = None;

        assert_eq!(workspace_rows(&map(vec![(1, orphan)]), &HashMap::new())[0].output, None);
    }

    #[test]
    fn window_rows_carries_every_window_ordered_by_id_and_joins_output_through_workspace() {
        let workspaces = map(vec![(5, workspace(5, 1, "eDP-1", true, true))]);
        let windows = map(vec![
            (14, window(14, "Sign in | Slack", "slack", false, false)),
            (2, window(2, "src/main.rs - Neovim", "kitty", true, true)),
        ]);

        let rows = window_rows(&windows, &workspaces);

        assert_eq!(rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["2", "14"], "ascending window id");
        let kitty = &rows[0];
        assert_eq!(kitty.title, "src/main.rs - Neovim");
        assert_eq!(kitty.app_id, "kitty");
        assert_eq!(kitty.workspace_id, Some("5".to_string()));
        assert_eq!(kitty.output.as_deref(), Some("eDP-1"));
        assert!(kitty.focused);
        assert_eq!(kitty.floating, Some(true));
        assert_eq!((kitty.fullscreen, kitty.minimized, kitty.maximized), (None, None, None));
    }

    #[test]
    fn window_rows_leaves_output_absent_for_a_window_off_every_known_workspace() {
        let mut orphan = window(9, "orphan", "orphan", false, false);
        orphan.workspace_id = Some(99);

        let rows = window_rows(&map(vec![(9, orphan)]), &HashMap::new());

        assert_eq!(rows[0].workspace_id, Some("99".to_string()));
        assert_eq!(rows[0].output, None, "workspace 99 is unknown, so no output can be joined");
    }

    #[test]
    fn focused_window_picks_the_window_niri_flags() {
        let windows = map(vec![
            (2, window(2, "src/main.rs - Neovim", "kitty", true, true)),
            (14, window(14, "Sign in | Slack", "slack", false, false)),
        ]);

        let focused = focused_window(&windows).expect("a flagged window is the focused one");

        assert_eq!(focused.title, "src/main.rs - Neovim");
        assert_eq!(focused.app_id, "kitty");
        assert!(focused.floating);
    }

    #[test]
    fn focused_window_is_none_when_niri_flags_nothing() {
        // Real: focusing a layer-shell surface leaves every toplevel unfocused.
        let windows = map(vec![(14, window(14, "Sign in | Slack", "slack", false, false))]);

        assert_eq!(focused_window(&windows), None);
    }

    #[test]
    fn focused_window_defaults_a_null_title_or_app_id_to_empty_rather_than_dropping_the_window() {
        // The payload declares both non-nullable, while niri's wire uses `Option` for both.
        let mut bare = window(2, "", "", true, false);
        bare.title = None;
        bare.app_id = None;

        let focused = focused_window(&map(vec![(2, bare)])).expect("a titleless window is still focused");

        assert_eq!((focused.title.as_str(), focused.app_id.as_str()), ("", ""));
    }

    #[test]
    fn an_event_naming_an_unseen_window_is_refused_not_applied() {
        let mut parts = Parts::default();
        let closed: niri_ipc::Event = serde_json::from_str(r#"{"WindowClosed":{"id":9}}"#).unwrap();

        assert!(!parts.apply(closed), "niri-ipc would panic on a window it never saw");
    }

    #[test]
    fn a_failed_first_connect_and_a_lost_stream_both_retry_until_nobody_listens() {
        let (mut connects, mut follows) = (0, 0);
        keep_following(
            Duration::ZERO,
            || {
                connects += 1;
                if connects == 1 { Err(std::io::ErrorKind::NotFound.into()) } else { Ok(connects) }
            },
            |_| {
                follows += 1;
                if follows < 3 { End::Lost } else { End::Unwanted }
            },
        );

        assert_eq!((follows, connects), (3, 4), "the first connect failed once and was retried");
    }

    /// niri-ipc's `read_events` turns a serde error into an `io::Error`; skipping relies on an
    /// unknown event kind and socket end landing on different kinds.
    #[test]
    fn an_unknown_event_is_skippable_and_socket_end_is_not() {
        let decode = |line: &str| std::io::Error::from(serde_json::from_str::<niri_ipc::Event>(line).unwrap_err());

        assert!(undecodable(&decode(r#"{"ScreencastStarted":{"id":1}}"#)));
        assert!(!undecodable(&decode("")), "read_line gives an empty line at socket end");
    }
}

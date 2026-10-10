//! [`WorkspacesController`]: `mantle.workspaces` state owner, write action, and compositor-neutral
//! reduction. See `workspaces/mod.rs`.
//!
//! [`derive_state`] knows no compositor type. It consumes [`WorkspaceRow`]s and a
//! [`FocusedWindow`]; the `crate::compositor` modules reduce their IPC into those rows.

pub use shared::state::workspaces::{
    ActiveClient, OutputWorkspaces, SpecialWorkspace, WorkspaceEntry, WorkspacesState,
};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use shared::debug;
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::publish;

use crate::compositor::{CompositorKind, unsupported_session_report};

/// One compositor workspace reduced to [`derive_state`]'s input fields; owned by neither adaptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: String,
    pub number: Option<u32>,
    pub name: Option<String>,
    /// Connector, or `None` when no output exists (niri reports this with no monitor); dropped.
    pub output: Option<String>,
    /// Workspace shown on its own output; every output has exactly one.
    pub is_active: bool,
    /// Workspace holding keyboard focus. Exactly one is global, making
    /// `OutputWorkspaces::focused_workspace` optional (ADR-0056 decision 4).
    pub is_focused: bool,
    /// Whether a window sits here and which `app_id` represents it (ADR-0117). The adaptor chooses
    /// it from its private window list; an empty id is `None`.
    pub populated: bool,
    pub app_id: Option<String>,
    pub window_id: Option<String>,
    pub urgent: bool,
}

/// The focused toplevel reduced to the `active_client` fields.
///
/// The adaptor decides which window is focused (niri flags each one); [`derive_state`] maps the
/// winner into the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedWindow {
    pub title: String,
    pub app_id: String,
    pub floating: bool,
    /// `None` when unreported; it stays absent in the payload.
    pub fullscreen: Option<bool>,
}

/// Folds rows into the payload. Pure and unit-tested without a compositor. Sorts outputs by
/// connector and workspaces by `number`, unnumbered last by name; omits an output with no active
/// workspace rather than fabricating an id (should be unreachable).
pub fn derive_state(workspaces: &[WorkspaceRow], focused: Option<&FocusedWindow>) -> WorkspacesState {
    let mut by_output: HashMap<&str, Vec<&WorkspaceRow>> = HashMap::new();
    for workspace in workspaces {
        let Some(output) = workspace.output.as_deref() else { continue };
        by_output.entry(output).or_default().push(workspace);
    }

    let mut outputs: Vec<OutputWorkspaces> = by_output
        .into_iter()
        .filter_map(|(name, mut group)| {
            group.sort_by_key(|workspace| (workspace.number.is_none(), workspace.number, workspace.name.clone()));
            let active_workspace = group.iter().find(|workspace| workspace.is_active)?.id.clone();
            let focused_workspace =
                group.iter().find(|workspace| workspace.is_focused).map(|workspace| workspace.id.clone());
            let workspaces = group
                .into_iter()
                .map(|workspace| WorkspaceEntry {
                    id: workspace.id.clone(),
                    number: workspace.number,
                    name: workspace.name.clone(),
                    populated: workspace.populated,
                    app_id: workspace.app_id.clone(),
                    window_id: workspace.window_id.clone(),
                    urgent: workspace.urgent,
                })
                .collect();
            Some(OutputWorkspaces { name: name.to_string(), active_workspace, focused_workspace, workspaces })
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));

    let active_client = focused.map(|window| ActiveClient {
        title: window.title.clone(),
        app_id: window.app_id.clone(),
        floating: window.floating,
        fullscreen: window.fullscreen,
    });

    WorkspacesState { compositor: String::new(), outputs, active_client, special: None, overview_open: None }
}

/// Compositor-neutral reader half: reduce, drop equal updates, store, and wake `main.rs`, shared
/// through [`StatePublisher::publish`].
pub struct StatePublisher {
    state: Arc<Mutex<WorkspacesState>>,
    events: UnboundedSender<()>,
    compositor: CompositorKind,
}

impl StatePublisher {
    pub fn new(state: Arc<Mutex<WorkspacesState>>, events: UnboundedSender<()>, compositor: CompositorKind) -> Self {
        Self { state, events, compositor }
    }

    /// `false` once no one listens, ending the reader loop. Not debounced: startup replays are
    /// separate real events (niri sends workspaces and windows separately, so the first push has
    /// no known window).
    ///
    /// `special == None` means unsupported; `Some` including empty means supported (ADR-0119).
    /// Sort by name because the adaptor's list is wire-ordered.
    pub fn publish(
        &mut self,
        workspaces: &[WorkspaceRow],
        focused: Option<&FocusedWindow>,
        special: Option<&[SpecialWorkspace]>,
        overview_open: Option<bool>,
    ) -> bool {
        let mut current = derive_state(workspaces, focused);
        current.compositor = self.compositor.name().to_string();
        current.overview_open = overview_open;
        current.special = special.map(|list| {
            let mut list = list.to_vec();
            list.sort_by(|a, b| a.name.cmp(&b.name));
            list
        });
        publish(&self.state, &self.events, current)
    }
}

/// No `Clone`: the reader owns an OS thread and moves only what it needs because niri's socket is
/// a blocking `std::net::UnixStream`, not tokio-aware.
pub struct WorkspacesController {
    state: Arc<Mutex<WorkspacesState>>,
    compositor: Option<CompositorKind>,
}

impl WorkspacesController {
    /// `state` and `compositor` come from `Capabilities::ensure_compositor_reader` (ADR-0247
    /// decision 2), which spawns the niri/Hyprland reader at most once and shares it with
    /// `windows`. `None` means no compositor implements this session, so nothing ever pushes
    /// (ADR-0056).
    ///
    /// A reader that was already running wrote `state` and signalled before this controller
    /// existed to catch it, so a non-default `state` here needs its own signal: otherwise this
    /// generation reads `nil` until the next real compositor event.
    pub fn new(
        state: Arc<Mutex<WorkspacesState>>,
        compositor: Option<CompositorKind>,
        events: UnboundedSender<()>,
    ) -> Self {
        if compositor.is_none() {
            debug!("{}; workspace reporting disabled for this run", unsupported_session_report());
        } else if *state.lock().expect("workspaces state mutex poisoned") != WorkspacesState::default() {
            let _ = events.send(());
        }
        Self { state, compositor }
    }

    pub fn snapshot(&self) -> WorkspacesState {
        self.state.lock().expect("workspaces state mutex poisoned").clone()
    }

    /// `workspaces:focus(id)`.
    pub fn focus(&self, id: &str) {
        match self.compositor {
            Some(kind) => kind.backend().focus_workspace(id),
            None => debug!("focus({id:?}) called but this session has no workspace implementor; ignored"),
        }
    }

    /// `workspaces:toggle_special(name)`. Only Hyprland has specials; niri lacks the `special` key
    /// so configs can feature-test it.
    pub fn toggle_special(&self, name: &str) {
        match self.compositor {
            Some(kind) => kind.backend().toggle_special(name),
            None => {
                debug!(
                    "toggle_special({name:?}) called but this session's compositor has no special workspaces; ignored"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: u64, number: u32, output: &str, is_active: bool, is_focused: bool) -> WorkspaceRow {
        WorkspaceRow {
            id: id.to_string(),
            number: Some(number),
            name: None,
            output: Some(output.to_string()),
            is_active,
            is_focused,
            populated: false,
            app_id: None,
            window_id: None,
            urgent: false,
        }
    }

    fn window(title: &str, app_id: &str, floating: bool) -> FocusedWindow {
        FocusedWindow { title: title.to_string(), app_id: app_id.to_string(), floating, fullscreen: None }
    }

    // ---- derive_state: grouping and ordering ----

    #[test]
    fn derive_state_groups_by_output_and_orders_outputs_and_workspaces_deterministically() {
        // Deliberately out of order: adaptors fold maps, whose iteration is unordered.
        let workspaces = [
            workspace(9, 3, "eDP-1", false, false),
            workspace(2, 1, "DP-2", true, false),
            workspace(5, 1, "eDP-1", true, true),
            workspace(7, 2, "eDP-1", false, false),
        ];

        let state = derive_state(&workspaces, None);

        assert_eq!(state.outputs.iter().map(|out| out.name.as_str()).collect::<Vec<_>>(), ["DP-2", "eDP-1"]);
        let edp = &state.outputs[1];
        assert_eq!(edp.workspaces.iter().map(|entry| entry.number).collect::<Vec<_>>(), [Some(1), Some(2), Some(3)]);
        assert_eq!(edp.workspaces.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), ["5", "7", "9"]);
    }

    #[test]
    fn derive_state_lists_numbered_workspaces_before_named_ones_and_names_in_order() {
        let named = |id: &str, name: &str| WorkspaceRow {
            id: id.to_string(),
            number: None,
            name: Some(name.to_string()),
            ..workspace(1, 1, "eDP-1", false, false)
        };
        let workspaces = [
            named("-1338", "web"),
            workspace(10, 10, "eDP-1", false, false),
            named("-1337", "chat"),
            workspace(2, 2, "eDP-1", true, true),
        ];

        let ids = derive_state(&workspaces, None).outputs[0]
            .workspaces
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();

        assert_eq!(ids, ["2", "10", "-1337", "-1338"]);
    }

    #[test]
    fn derive_state_carries_populated_and_app_id_through_and_omits_an_absent_app_id() {
        let mut busy = workspace(5, 1, "eDP-1", true, true);
        busy.populated = true;
        busy.app_id = Some("firefox".to_string());
        busy.window_id = Some("42".to_string());
        let workspaces = [busy, workspace(7, 2, "eDP-1", false, false)];

        let state = derive_state(&workspaces, None);
        let entries = &state.outputs[0].workspaces;

        assert!(entries[0].populated);
        assert_eq!(entries[0].app_id.as_deref(), Some("firefox"));
        assert_eq!(entries[0].window_id.as_deref(), Some("42"));
        assert!(!entries[1].populated);
        let json = serde_json::to_value(&entries[1]).unwrap();
        assert!(json.get("app_id").is_none(), "an empty workspace has no app_id key: {json}");
        assert!(json.get("window_id").is_none(), "an empty workspace has no window_id key: {json}");
        assert_eq!(json["populated"], false);
    }

    #[test]
    fn derive_state_reports_the_active_workspace_by_id_not_by_index() {
        // `id` and `number` disagree: reorder moves `number` but not `id`, catching swapped fields.
        let state = derive_state(&[workspace(42, 1, "eDP-1", true, true)], None);

        assert_eq!(state.outputs[0].active_workspace, "42");
        assert_eq!(state.outputs[0].workspaces[0].number, Some(1));
    }

    #[test]
    fn derive_state_ignores_a_workspace_that_has_no_output() {
        // Real: a compositor reports no output when none are connected.
        let orphan = WorkspaceRow { output: None, ..workspace(1, 1, "eDP-1", true, true) };

        assert_eq!(derive_state(&[orphan], None).outputs, Vec::new());
    }

    #[test]
    fn derive_state_omits_an_output_with_no_active_workspace_rather_than_inventing_one() {
        let workspaces = [workspace(1, 1, "eDP-1", false, false), workspace(2, 1, "DP-2", true, false)];

        let state = derive_state(&workspaces, None);

        assert_eq!(state.outputs.len(), 1, "an output with no active workspace reported is not listed");
        assert_eq!(state.outputs[0].name, "DP-2");
    }

    // ---- derive_state: focus (ADR-0056 decision 4) ----

    #[test]
    fn derive_state_puts_focused_workspace_only_on_the_output_that_holds_focus() {
        let workspaces = [workspace(1, 1, "eDP-1", true, false), workspace(2, 1, "DP-2", true, true)];

        let state = derive_state(&workspaces, None);

        let dp = state.outputs.iter().find(|out| out.name == "DP-2").unwrap();
        let edp = state.outputs.iter().find(|out| out.name == "eDP-1").unwrap();
        assert_eq!(dp.focused_workspace.as_deref(), Some("2"), "the focused output reports the id it is focused on");
        assert_eq!(edp.focused_workspace, None, "an output that does not hold focus must not claim it does");
    }

    #[test]
    fn an_unfocused_output_omits_focused_workspace_from_its_json_entirely() {
        let json = serde_json::to_value(derive_state(&[workspace(1, 1, "eDP-1", true, false)], None)).unwrap();

        let output = &json["outputs"][0];
        assert!(
            output.get("focused_workspace").is_none(),
            "an absent key reads as nil in Lua; a `null` would too, but only an absent key matches every other optional field here"
        );
        assert_eq!(output["active_workspace"], "1");
    }

    // ---- derive_state: active_client (ADR-0056 decision 5) ----

    #[test]
    fn derive_state_maps_the_focused_window_onto_active_client() {
        let focused = window("src/main.rs - Neovim", "kitty", true);

        let client = derive_state(&[], Some(&focused)).active_client.expect("a focused window produces active_client");

        assert_eq!(client.title, "src/main.rs - Neovim");
        assert_eq!(client.app_id, "kitty");
        assert!(client.floating);
    }

    #[test]
    fn derive_state_has_no_active_client_when_no_window_holds_focus() {
        // Real: focusing a layer-shell surface leaves every toplevel unfocused.
        assert_eq!(derive_state(&[workspace(1, 1, "eDP-1", true, true)], None).active_client, None);
    }

    #[test]
    fn active_client_carries_fullscreen_only_when_the_compositor_said() {
        // ADR-0056 decision 5 keeps the key absent rather than fabricating `false`; ADR-0119 lets
        // a compositor that knows provide it. Absent, not `null`, otherwise.
        let json = serde_json::to_value(derive_state(&[], Some(&window("a title", "kitty", false)))).unwrap();
        let client = &json["active_client"];
        assert_eq!(client["title"], "a title");
        assert!(client.get("fullscreen").is_none());

        let mut known = window("a title", "mpv", false);
        known.fullscreen = Some(true);
        let json = serde_json::to_value(derive_state(&[], Some(&known))).unwrap();
        assert_eq!(json["active_client"]["fullscreen"], true);
    }

    #[test]
    fn an_overview_flag_is_absent_where_there_is_no_overview_and_false_where_it_is_shut() {
        let (mut publisher, _rx) = publisher();
        let workspaces = [workspace(1, 1, "eDP-1", true, true)];

        assert!(publisher.publish(&workspaces, None, None, None));
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert!(json.get("overview_open").is_none(), "no key at all, as `special` does it");

        assert!(publisher.publish(&workspaces, None, None, Some(false)), "shut is a change from unsupported");
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["overview_open"], serde_json::json!(false));
    }

    #[test]
    fn a_state_with_nothing_focused_omits_active_client_rather_than_nulling_it() {
        let json = serde_json::to_value(WorkspacesState::default()).unwrap();

        assert!(json.get("active_client").is_none());
        assert!(json.get("special").is_none());
        assert_eq!(json["outputs"], serde_json::json!([]));
    }

    // ---- StatePublisher ----

    fn publisher() -> (StatePublisher, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (StatePublisher::new(Arc::new(Mutex::new(WorkspacesState::default())), tx, CompositorKind::Niri), rx)
    }

    fn special(name: &str, shown_on: Option<&str>) -> SpecialWorkspace {
        SpecialWorkspace {
            name: name.to_string(),
            populated: true,
            app_id: None,
            window_id: None,
            shown_on: shown_on.map(str::to_string),
        }
    }

    #[test]
    fn publish_stores_the_state_and_signals_once_per_real_change() {
        let (mut publisher, mut rx) = publisher();
        let workspaces = [workspace(5, 1, "eDP-1", true, true)];

        assert!(publisher.publish(&workspaces, None, None, None));
        assert!(publisher.publish(&workspaces, None, None, None), "an event that changes nothing is not a change");
        assert!(publisher.publish(&workspaces, Some(&window("a title", "kitty", false)), None, None));

        assert_eq!(publisher.state.lock().unwrap().active_client.as_ref().unwrap().app_id, "kitty");
        let signals = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert_eq!(signals, 2, "the repeated middle publish must not wake main.rs");
    }

    #[test]
    fn publish_reports_false_once_nothing_is_listening_so_a_reader_loop_can_stop() {
        let (mut publisher, rx) = publisher();
        drop(rx);

        assert!(!publisher.publish(&[workspace(5, 1, "eDP-1", true, true)], None, None, None));
    }

    #[test]
    fn publish_stamps_the_compositor_and_sorts_specials_and_keeps_the_key_out_when_there_are_none_to_have() {
        let (mut publisher, _rx) = publisher();
        let workspaces = [workspace(5, 1, "eDP-1", true, true)];

        assert!(publisher.publish(&workspaces, None, None, None));
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["compositor"], "niri");
        assert!(json.get("special").is_none(), "no key at all: `special == nil` is the feature test");

        let mut term = special("special:term", Some("eDP-1"));
        term.window_id = Some("0xabc".to_string());
        assert!(publisher.publish(&workspaces, None, Some(&[term, special("special", None)]), None));
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["special"][0]["name"], "special");
        assert!(json["special"][0].get("window_id").is_none());
        assert_eq!(json["special"][1]["name"], "special:term");
        assert_eq!(json["special"][1]["shown_on"], "eDP-1");
        assert_eq!(json["special"][1]["window_id"], "0xabc");
        assert!(json["special"][0].get("shown_on").is_none());

        assert!(publisher.publish(&workspaces, None, Some(&[]), None));
        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["special"], serde_json::json!([]), "the compositor has specials and none exist right now");
    }

    /// A reader started by an earlier capability already wrote real state before this one
    /// attached; without a catch-up signal, `mantle.workspaces:get()` stays `nil` until the next
    /// compositor event.
    #[test]
    fn new_sends_a_catch_up_signal_when_the_shared_state_is_already_populated() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state =
            Arc::new(Mutex::new(WorkspacesState { compositor: "niri".to_string(), ..WorkspacesState::default() }));

        let _controller = WorkspacesController::new(state, Some(CompositorKind::Niri), tx);

        assert!(matches!(rx.try_recv(), Ok(())));
    }

    #[test]
    fn new_sends_nothing_when_the_shared_state_is_still_default() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let _controller =
            WorkspacesController::new(Arc::new(Mutex::new(WorkspacesState::default())), Some(CompositorKind::Niri), tx);

        assert!(rx.try_recv().is_err());
    }
}

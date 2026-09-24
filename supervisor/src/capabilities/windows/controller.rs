//! [`WindowsController`]: `mantle.windows` state owner, write actions, and the compositor-neutral
//! reduction. Backends build [`WindowEntry`] directly; [`StatePublisher::publish`] only sorts.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use shared::debug;
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::workspaces::{hyprland, niri};
use crate::compositor::{CompositorKind, unsupported_session_report};

use super::wlr;

/// `mantle.windows` payload; `nil` with no niri, Hyprland or wlr-foreign-toplevel backend.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WindowsState {
    /// `"niri"`, `"hyprland"`, or `"wlr_foreign_toplevel"`.
    pub source: String,
    /// Sorted by `workspace_id`, then backend order; windows without one last.
    pub windows: Vec<WindowEntry>,
}

/// One toplevel window. `nil` optional fields are ones the backend does not report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WindowEntry {
    /// Opaque, backend-shaped id for `:invoke`; compare it, never parse it.
    pub id: String,
    /// Window title; empty when unset.
    pub title: String,
    /// Wayland `app_id` (Hyprland's `class`); empty when unset.
    pub app_id: String,
    /// `WorkspaceEntry.id`; `nil` on wlr and on Hyprland special workspaces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<u64>,
    /// Connector name; `nil` when unknown. On wlr, the first output the window entered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Whether the window has keyboard focus.
    pub focused: bool,
    /// Whether the window floats rather than tiles; `nil` on wlr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floating: Option<bool>,
    /// Whether the window is fullscreen; `nil` on niri.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
    /// Whether the window is minimized; `nil` except on wlr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimized: Option<bool>,
    /// Whether the window is maximized; `nil` on niri.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximized: Option<bool>,
}

/// Shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSignal {
    Changed,
}

/// Reduces, drops equal updates, stores, and wakes `main.rs`.
pub struct StatePublisher {
    state: Arc<Mutex<WindowsState>>,
    events: UnboundedSender<WindowsSignal>,
    source: &'static str,
}

impl StatePublisher {
    pub fn new(state: Arc<Mutex<WindowsState>>, events: UnboundedSender<WindowsSignal>, source: &'static str) -> Self {
        Self { state, events, source }
    }

    /// `false` once no one listens, so a reader loop that owns no other publisher can stop.
    pub fn publish(&mut self, mut windows: Vec<WindowEntry>) -> bool {
        windows.sort_by_key(|window| window.workspace_id.unwrap_or(u64::MAX));
        let current = WindowsState { source: self.source.to_string(), windows };
        let mut state = self.state.lock().expect("windows state mutex poisoned");
        if *state == current {
            return true;
        }
        *state = current;
        drop(state);
        self.events.send(WindowsSignal::Changed).is_ok()
    }
}

/// `Ipc` shares its state with `workspaces`; `Wlr` owns its own dedicated Wayland connection.
enum Backend {
    Ipc(CompositorKind),
    Wlr(wlr::Handle),
    None,
}

/// No `Clone`: `Wlr` owns a dedicated Wayland connection and dispatch thread, like `idle`.
pub struct WindowsController {
    state: Arc<Mutex<WindowsState>>,
    backend: Backend,
}

impl WindowsController {
    /// `state`/`compositor` come from the shared niri/Hyprland reader; with neither, this tries
    /// `zwlr_foreign_toplevel_manager_v1` on its own connection before giving up.
    ///
    /// A reader that was already running wrote `state` and signalled before this controller
    /// existed to catch it, so a non-default `state` here needs its own signal: otherwise this
    /// generation reads `nil` until the next real window event.
    pub async fn new(
        state: Arc<Mutex<WindowsState>>,
        compositor: Option<CompositorKind>,
        events: UnboundedSender<WindowsSignal>,
    ) -> Self {
        let controller = match compositor {
            Some(kind) => Self { state, backend: Backend::Ipc(kind) },
            None => match wlr::connect(events.clone(), Arc::clone(&state)).await {
                Some(handle) => Self { state, backend: Backend::Wlr(handle) },
                None => {
                    debug!("{}; window reporting disabled for this run", unsupported_session_report());
                    Self { state, backend: Backend::None }
                }
            },
        };
        if controller.snapshot() != WindowsState::default() {
            let _ = events.send(WindowsSignal::Changed);
        }
        controller
    }

    pub fn snapshot(&self) -> WindowsState {
        self.state.lock().expect("windows state mutex poisoned").clone()
    }

    /// Whether `read`'s last known value for `id` disagrees with `desired`; niri and Hyprland only
    /// toggle, so a write is sent only on a real change.
    fn differs(&self, id: &str, read: impl Fn(&WindowEntry) -> Option<bool>, desired: bool) -> bool {
        let state = self.state.lock().expect("windows state mutex poisoned");
        state.windows.iter().find(|window| window.id == id).and_then(read).unwrap_or(false) != desired
    }

    pub fn focus(&self, id: &str) {
        match &self.backend {
            Backend::Ipc(CompositorKind::Niri) => niri::focus_window(id),
            Backend::Ipc(CompositorKind::Hyprland) => hyprland::focus_window(id),
            Backend::Wlr(handle) => wlr::activate(handle, id),
            Backend::None => debug!("focus({id:?}) called but this session has no window implementor; ignored"),
        }
    }

    pub fn close(&self, id: &str) {
        match &self.backend {
            Backend::Ipc(CompositorKind::Niri) => niri::close_window(id),
            Backend::Ipc(CompositorKind::Hyprland) => hyprland::close_window(id),
            Backend::Wlr(handle) => wlr::close(handle, id),
            Backend::None => debug!("close({id:?}) called but this session has no window implementor; ignored"),
        }
    }

    pub fn set_fullscreen(&self, id: &str, fullscreen: bool) {
        match &self.backend {
            // niri only toggles and never reports the state, so a toggle could undo the request.
            Backend::Ipc(CompositorKind::Niri) => {
                debug!("set_fullscreen({id:?}, {fullscreen}) called but niri reports no fullscreen state; ignored")
            }
            Backend::Ipc(CompositorKind::Hyprland) => {
                if self.differs(id, |w| w.fullscreen, fullscreen) {
                    hyprland::toggle_window_fullscreen(id);
                }
            }
            Backend::Wlr(handle) => wlr::set_fullscreen(handle, id, fullscreen),
            Backend::None => {
                debug!(
                    "set_fullscreen({id:?}, {fullscreen}) called but this session has no window implementor; ignored"
                )
            }
        }
    }

    pub fn set_minimized(&self, id: &str, minimized: bool) {
        match &self.backend {
            Backend::Wlr(handle) => wlr::set_minimized(handle, id, minimized),
            Backend::Ipc(_) | Backend::None => {
                debug!("set_minimized({id:?}, {minimized}) called but this backend has no minimize concept; ignored")
            }
        }
    }

    pub fn set_maximized(&self, id: &str, maximized: bool) {
        match &self.backend {
            Backend::Ipc(CompositorKind::Hyprland) => {
                if self.differs(id, |w| w.maximized, maximized) {
                    hyprland::toggle_window_maximized(id);
                }
            }
            Backend::Wlr(handle) => wlr::set_maximized(handle, id, maximized),
            Backend::Ipc(CompositorKind::Niri) | Backend::None => {
                debug!("set_maximized({id:?}, {maximized}) called but this backend has no maximize concept; ignored")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, workspace_id: Option<u64>) -> WindowEntry {
        WindowEntry {
            id: id.to_string(),
            title: String::new(),
            app_id: String::new(),
            workspace_id,
            output: None,
            focused: false,
            floating: None,
            fullscreen: None,
            minimized: None,
            maximized: None,
        }
    }

    fn publisher() -> (StatePublisher, tokio::sync::mpsc::UnboundedReceiver<WindowsSignal>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (StatePublisher::new(Arc::new(Mutex::new(WindowsState::default())), tx, "niri"), rx)
    }

    #[test]
    fn publish_orders_by_workspace_and_keeps_each_backends_own_order_within_one() {
        let (mut publisher, _rx) = publisher();
        let entries = [entry("3", Some(2)), entry("1", Some(1)), entry("2", Some(1)), entry("4", None)];

        publisher.publish(entries.to_vec());

        let state = publisher.state.lock().unwrap().clone();
        assert_eq!(
            state.windows.iter().map(|window| window.id.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3", "4"],
            "workspace 1's rows keep their input order, workspace 2 follows, no-workspace sorts last"
        );
    }

    #[test]
    fn publish_stores_state_stamps_the_source_and_signals_once_per_real_change() {
        let (mut publisher, mut rx) = publisher();
        let entries = [entry("1", Some(1))];

        assert!(publisher.publish(entries.to_vec()));
        assert!(publisher.publish(entries.to_vec()), "an event that changes nothing is not a change");
        let mut moved = entries[0].clone();
        moved.focused = true;
        assert!(publisher.publish(vec![moved]));

        let json = serde_json::to_value(publisher.state.lock().unwrap().clone()).unwrap();
        assert_eq!(json["source"], "niri");
        assert_eq!(json["windows"][0]["focused"], true);
        let signals = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert_eq!(signals, 2, "the repeated middle publish must not wake main.rs");
    }

    #[test]
    fn publish_reports_false_once_nothing_is_listening_so_a_reader_loop_can_stop() {
        let (mut publisher, rx) = publisher();
        drop(rx);

        assert!(!publisher.publish(vec![entry("1", None)]));
    }

    /// A reader started by an earlier capability already wrote real state before this one
    /// attached; without a catch-up signal, `mantle.windows:get()` stays `nil` until the next
    /// compositor event.
    #[tokio::test]
    async fn new_sends_a_catch_up_signal_when_the_shared_state_is_already_populated() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(WindowsState { source: "niri".to_string(), windows: vec![entry("1", None)] }));

        let _controller = WindowsController::new(state, Some(CompositorKind::Niri), tx).await;

        assert!(matches!(rx.try_recv(), Ok(WindowsSignal::Changed)));
    }

    #[tokio::test]
    async fn new_sends_nothing_when_the_shared_state_is_still_default() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let _controller =
            WindowsController::new(Arc::new(Mutex::new(WindowsState::default())), Some(CompositorKind::Niri), tx).await;

        assert!(rx.try_recv().is_err());
    }
}

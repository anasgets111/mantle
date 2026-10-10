//! [`WindowsController`]: `mantle.windows` state owner, write actions, and the compositor-neutral
//! reduction. Backends build [`WindowEntry`] directly; [`StatePublisher::publish`] only sorts.

pub use shared::state::windows::{WindowEntry, WindowsState};

use std::sync::{Arc, Mutex};

use shared::debug;
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::publish;

use crate::compositor::{Compositor, CompositorKind, backend_or_unsupported};

use super::wlr;

/// Reduces, drops equal updates, stores, and wakes `main.rs`.
pub struct StatePublisher {
    state: Arc<Mutex<WindowsState>>,
    events: UnboundedSender<()>,
    source: &'static str,
}

impl StatePublisher {
    pub fn new(state: Arc<Mutex<WindowsState>>, events: UnboundedSender<()>, source: &'static str) -> Self {
        Self { state, events, source }
    }

    /// `false` once no one listens, so a reader loop that owns no other publisher can stop.
    pub fn publish(&mut self, mut windows: Vec<WindowEntry>) -> bool {
        // Numeric, so "10" follows "2"; the cast puts Hyprland's negative named ids after every number.
        windows.sort_by_key(|window| {
            window.workspace_id.as_deref().and_then(|id| id.parse::<i64>().ok()).map_or(u64::MAX, |id| id as u64)
        });
        let current = WindowsState { source: self.source.to_string(), windows };
        publish(&self.state, &self.events, current)
    }
}

/// `Compositor` shares its state with `workspaces`; `Wlr` owns its own Wayland connection.
enum Backend {
    Compositor(&'static dyn Compositor),
    Wlr(wlr::Handle),
}

/// No `Clone`: `Wlr` owns a dedicated Wayland connection and dispatch thread, like `idle`.
pub struct WindowsController {
    state: Arc<Mutex<WindowsState>>,
    backend: Backend,
}

impl WindowsController {
    /// `state`/`compositor` come from the shared compositor reader; without one, this tries
    /// `zwlr_foreign_toplevel_manager_v1` on its own connection before giving up.
    ///
    /// A reader that was already running wrote `state` and signalled before this controller
    /// existed to catch it, so a non-default `state` here needs its own signal: otherwise this
    /// generation reads `nil` until the next real window event.
    pub async fn new(
        state: Arc<Mutex<WindowsState>>,
        compositor: Option<CompositorKind>,
        events: UnboundedSender<()>,
    ) -> Self {
        let controller = match compositor {
            Some(kind) => Self { state, backend: Backend::Compositor(kind.backend()) },
            None => match wlr::connect(events.clone(), Arc::clone(&state)).await {
                Some(handle) => Self { state, backend: Backend::Wlr(handle) },
                None => Self { state, backend: Backend::Compositor(backend_or_unsupported(None, "window")) },
            },
        };
        if controller.snapshot() != WindowsState::default() {
            let _ = events.send(());
        }
        controller
    }

    pub fn snapshot(&self) -> WindowsState {
        self.state.lock().expect("windows state mutex poisoned").clone()
    }

    /// `read`'s last published value for `id`; a toggle-only compositor writes only on a real change.
    fn current(&self, id: &str, read: impl Fn(&WindowEntry) -> Option<bool>) -> Option<bool> {
        let state = self.state.lock().expect("windows state mutex poisoned");
        state.windows.iter().find(|window| window.id == id).and_then(read)
    }

    pub fn focus(&self, id: &str) {
        match &self.backend {
            Backend::Compositor(compositor) => compositor.focus_window(id),
            Backend::Wlr(handle) => wlr::activate(handle, id),
        }
    }

    pub fn close(&self, id: &str) {
        match &self.backend {
            Backend::Compositor(compositor) => compositor.close_window(id),
            Backend::Wlr(handle) => wlr::close(handle, id),
        }
    }

    pub fn set_fullscreen(&self, id: &str, fullscreen: bool) {
        match &self.backend {
            Backend::Compositor(compositor) => {
                compositor.set_fullscreen(id, fullscreen, self.current(id, |w| w.fullscreen))
            }
            Backend::Wlr(handle) => wlr::set_fullscreen(handle, id, fullscreen),
        }
    }

    pub fn set_minimized(&self, id: &str, minimized: bool) {
        match &self.backend {
            Backend::Wlr(handle) => wlr::set_minimized(handle, id, minimized),
            Backend::Compositor(_) => {
                debug!("set_minimized({id:?}, {minimized}) called but this backend has no minimize concept; ignored")
            }
        }
    }

    pub fn set_maximized(&self, id: &str, maximized: bool) {
        match &self.backend {
            Backend::Compositor(compositor) => {
                compositor.set_maximized(id, maximized, self.current(id, |w| w.maximized))
            }
            Backend::Wlr(handle) => wlr::set_maximized(handle, id, maximized),
        }
    }

    pub fn move_to_workspace(&self, id: &str, workspace_id: &str) {
        match &self.backend {
            Backend::Compositor(compositor) => compositor.move_window(id, workspace_id),
            Backend::Wlr(_) => debug!("move_to_workspace({id:?}, {workspace_id}) called on wlr backend; ignored"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, workspace_id: Option<i64>) -> WindowEntry {
        WindowEntry {
            id: id.to_string(),
            title: String::new(),
            app_id: String::new(),
            workspace_id: workspace_id.map(|id| id.to_string()),
            output: None,
            focused: false,
            floating: None,
            fullscreen: None,
            minimized: None,
            maximized: None,
            urgent: false,
        }
    }

    fn publisher() -> (StatePublisher, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (StatePublisher::new(Arc::new(Mutex::new(WindowsState::default())), tx, "niri"), rx)
    }

    #[test]
    fn publish_orders_by_workspace_and_keeps_each_backends_own_order_within_one() {
        let (mut publisher, _rx) = publisher();
        let entries = [
            entry("6", Some(-1337)),
            entry("5", Some(10)),
            entry("3", Some(2)),
            entry("1", Some(1)),
            entry("2", Some(1)),
            entry("4", None),
        ];

        publisher.publish(entries.to_vec());

        let state = publisher.state.lock().unwrap().clone();
        assert_eq!(
            state.windows.iter().map(|window| window.id.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3", "5", "6", "4"],
            "input order within workspace 1, 10 after 2, a named workspace after the numbers, none last"
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

        assert!(matches!(rx.try_recv(), Ok(())));
    }

    #[tokio::test]
    async fn new_sends_nothing_when_the_shared_state_is_still_default() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let _controller =
            WindowsController::new(Arc::new(Mutex::new(WindowsState::default())), Some(CompositorKind::Niri), tx).await;

        assert!(rx.try_recv().is_err());
    }
}

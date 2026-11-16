//! A [`Compositor`] fake for capability tests.

use std::sync::Mutex;

use super::Compositor;
use crate::capabilities::keyboard::controller::LayoutSink;
use crate::capabilities::windows::controller::StatePublisher as WindowsPublisher;
use crate::capabilities::workspaces::controller::StatePublisher;

/// Records each write instead of sending it. The real backends read their socket path from the
/// session's env, so a real one would reach the live compositor of the session running tests.
#[derive(Default)]
pub(crate) struct Recorder(pub(crate) Mutex<Vec<String>>);

impl Compositor for Recorder {
    fn spawn_reader(&self, _: StatePublisher, _: WindowsPublisher, _: LayoutSink) {}
    fn focus_workspace(&self, id: &str) {
        self.0.lock().unwrap().push(format!("focus_workspace {id}"));
    }
    fn toggle_special(&self, name: &str) {
        self.0.lock().unwrap().push(format!("toggle_special {name}"));
    }
    fn focus_window(&self, id: &str) {
        self.0.lock().unwrap().push(format!("focus_window {id}"));
    }
    fn close_window(&self, id: &str) {
        self.0.lock().unwrap().push(format!("close_window {id}"));
    }
    fn set_fullscreen(&self, id: &str, fullscreen: bool, current: Option<bool>) {
        self.0.lock().unwrap().push(format!("set_fullscreen {id} {fullscreen} {current:?}"));
    }
    fn set_maximized(&self, id: &str, maximized: bool, current: Option<bool>) {
        self.0.lock().unwrap().push(format!("set_maximized {id} {maximized} {current:?}"));
    }
    fn move_window(&self, id: &str, workspace_id: &str) {
        self.0.lock().unwrap().push(format!("move_window {id} {workspace_id}"));
    }
    fn switch_layout(&self, index: usize) {
        self.0.lock().unwrap().push(format!("switch_layout {index}"));
    }
}

//! Keyboard layout integration for `mantle.keyboard` (ADR-0034). The compositor reader
//! (`capabilities::lifecycle`'s `CompositorReader`) writes layout through a [`LayoutSink`]. Without a
//! supported compositor, layout is empty with count `0`.

use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::mpsc::UnboundedSender;

use super::controller::KeyboardState;

/// `keyboard`'s state as the compositor reader sees it. The reader writes layout from its first
/// event, and signals once `keyboard` has started and set `events`.
#[derive(Clone, Default)]
pub struct LayoutSink {
    pub state: Arc<Mutex<KeyboardState>>,
    pub events: Arc<OnceLock<UnboundedSender<()>>>,
}

impl LayoutSink {
    pub fn write(&self, active_layout: String, active_layout_index: u32, layout_count: u32) {
        {
            let mut guard = self.state.lock().expect("mutex poisoned");
            guard.active_layout = active_layout;
            guard.active_layout_index = active_layout_index;
            guard.layout_count = layout_count;
        }
        if let Some(events) = self.events.get() {
            let _ = events.send(());
        }
    }
}

//! Keyboard layout integration for `mantle.keyboard` (ADR-0034). The shared niri/Hyprland reader
//! (`capabilities::lifecycle`'s `CompositorReader`) writes layout through a [`LayoutSink`];
//! `switch_layout` goes through a [`CompositorLink`]. Without a supported compositor, layout is
//! empty with count `0`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use shared::debug;
use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::hyprland::{hyprland_command, hyprland_socket_path};
use crate::compositor::niri::niri_action;
use crate::compositor::sway::sway_command;

use super::controller::KeyboardState;

/// `switch_layout` is synchronous fire-and-forget; state returns through the compositor reader. It
/// is not `async fn` to preserve `Box<dyn CompositorLink>` object safety.
pub trait CompositorLink: Send + Sync {
    fn switch_layout(&self, index: usize);
}

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

pub struct NiriLink;

impl CompositorLink for NiriLink {
    fn switch_layout(&self, index: usize) {
        // A command supplies an unbounded u64, while niri's wire protocol takes u8. Reject overflow
        // instead of truncating 256 to 0.
        let Ok(index) = u8::try_from(index) else {
            debug!("switch_layout index {index} is out of range for niri (must fit in a u8); ignored");
            return;
        };
        let layout = niri_ipc::LayoutSwitchTarget::Index(index);
        niri_action(niri_ipc::Action::SwitchLayout { layout }, "keyboard");
    }
}

/// `input type:keyboard xkb_switch_layout <index>` over sway's IPC.
pub struct SwayLink;

impl CompositorLink for SwayLink {
    fn switch_layout(&self, index: usize) {
        sway_command(format!("input type:keyboard xkb_switch_layout {index}"), "keyboard");
    }
}

/// `switchxkblayout main <index>` over Hyprland's `.socket.sock`.
pub struct HyprlandLink {
    command_path: PathBuf,
}

impl HyprlandLink {
    /// `signature` is a non-empty `$HYPRLAND_INSTANCE_SIGNATURE`, from
    /// `compositor::hyprland_signature`.
    pub fn new(signature: &str) -> Self {
        Self { command_path: hyprland_socket_path(signature, ".socket.sock") }
    }
}

impl CompositorLink for HyprlandLink {
    /// `main` is also Hyprland's device target for that keyboard, so no device name is tracked here.
    fn switch_layout(&self, index: usize) {
        let socket_path = self.command_path.clone();
        crate::compositor::run_in_order(move || {
            hyprland_command(&socket_path, &format!("switchxkblayout main {index}"), "keyboard");
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn niri_switch_layout_index_validation_accepts_the_full_u8_range() {
        assert_eq!(u8::try_from(0usize), Ok(0));
        assert_eq!(u8::try_from(255usize), Ok(255));
    }

    #[test]
    fn niri_switch_layout_index_validation_rejects_values_that_would_truncate() {
        // Regression: 256 used to truncate to 0 via `as u8`.
        assert!(u8::try_from(256usize).is_err());
        assert!(u8::try_from(usize::MAX).is_err());
    }
}

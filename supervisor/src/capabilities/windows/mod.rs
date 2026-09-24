//! `mantle.windows`: every open toplevel window, for taskbars, docks and alt-tab.
//!
//! niri and Hyprland share `workspaces`' event stream; other compositors use
//! `zwlr_foreign_toplevel_management_v1` on their own connection.

pub mod controller;
mod wlr;

pub use controller::{WindowsController, WindowsSignal};

// An action a backend does not support logs at debug and does nothing.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WindowsAction {
    /// Focuses a window.
    Focus {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        id: String,
    },
    /// Asks the compositor to close the window.
    Close {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        id: String,
    },
    /// Sets fullscreen on or off; no-op on niri.
    SetFullscreen {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        id: String,
        fullscreen: bool,
    },
    /// Sets minimized on or off; wlr only.
    SetMinimized {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        id: String,
        minimized: bool,
    },
    /// Sets maximized on or off; no-op on niri.
    SetMaximized {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        id: String,
        maximized: bool,
    },
}

/// `mantle.windows` action dispatch (ADR-0037).
pub fn dispatch(controller: &WindowsController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<WindowsAction>(&envelope.params) else { return };
    match action {
        WindowsAction::Focus { id } => controller.focus(&id),
        WindowsAction::Close { id } => controller.close(&id),
        WindowsAction::SetFullscreen { id, fullscreen } => controller.set_fullscreen(&id, fullscreen),
        WindowsAction::SetMinimized { id, minimized } => controller.set_minimized(&id, minimized),
        WindowsAction::SetMaximized { id, maximized } => controller.set_maximized(&id, maximized),
    }
}

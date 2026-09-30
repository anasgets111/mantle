//! `mantle.windows`: every open toplevel window, for taskbars, docks and alt-tab.
//!
//! niri and Hyprland share `workspaces`' event stream; other compositors use
//! `zwlr_foreign_toplevel_management_v1` on their own connection.

pub mod controller;
mod wlr;

pub use controller::WindowsController;
use shared::action::WindowsAction;

/// `mantle.windows` action dispatch (ADR-0037).
pub fn dispatch(controller: &WindowsController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<WindowsAction>(&envelope.params) else { return };
    match action {
        WindowsAction::Focus { id } => controller.focus(&id),
        WindowsAction::Close { id } => controller.close(&id),
        WindowsAction::SetFullscreen { id, fullscreen } => controller.set_fullscreen(&id, fullscreen),
        WindowsAction::SetMinimized { id, minimized } => controller.set_minimized(&id, minimized),
        WindowsAction::SetMaximized { id, maximized } => controller.set_maximized(&id, maximized),
        WindowsAction::MoveToWorkspace { id, workspace_id } => controller.move_to_workspace(&id, workspace_id),
    }
}

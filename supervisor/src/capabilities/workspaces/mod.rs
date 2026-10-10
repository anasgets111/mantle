//! `mantle.workspaces`: per-output workspace state and focused window, from niri's IPC stream
//! (ADR-0056), Hyprland's event and command sockets (ADR-0118), or sway's i3-compatible IPC.
//!
//! Top-level because this is a compositor Unix socket, not a device or D-Bus interface.
//!
//! The protocol-specific seam is `crate::compositor::Compositor`: reads go through `StatePublisher`,
//! writes through `CompositorKind::backend`. With no implementor, nothing pushes and
//! `mantle.workspaces` stays `nil`; the payload has no absence sentinel, and `outputs: []` would
//! mean no workspaces rather than no answer.
//!
//! `controller` holds the payload, reduction, and publish contract; each `crate::compositor`
//! module owns its protocol's types and also feeds `mantle.windows`' full window list from the
//! same events, through a second publisher.

pub mod controller;

pub use controller::WorkspacesController;
use shared::action::WorkspacesAction;

/// `mantle.workspaces` action dispatch (ADR-0037): each action writes over a fresh compositor
/// socket on its own thread, so arms are plain calls rather than `tokio::spawn`.
pub fn dispatch(controller: &WorkspacesController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<WorkspacesAction>(&envelope.params) else { return };
    match action {
        WorkspacesAction::Focus { id } => controller.focus(&id),
        WorkspacesAction::ToggleSpecial { name } => controller.toggle_special(&name),
    }
}

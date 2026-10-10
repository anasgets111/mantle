//! `mantle.workspaces`: per-output workspace state and focused window, from the session's
//! compositor (`crate::compositor`: niri, Hyprland, sway or mango).
//!
//! The protocol-specific seam is `crate::compositor::Compositor`: reads go through `StatePublisher`,
//! writes through `CompositorKind::backend`. With no implementor, nothing pushes and
//! `mantle.workspaces` stays `nil`; the payload has no absence sentinel, and `outputs: []` would
//! mean no workspaces rather than no answer.
//!
//! `controller` holds the payload, reduction, and publish contract; each `crate::compositor`
//! module owns its protocol's types and also feeds `mantle.windows` from the same events.

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

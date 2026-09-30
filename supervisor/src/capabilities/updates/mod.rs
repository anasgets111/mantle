//! `mantle.updates` capability: package update checking and installation (ADR-0034).
//!
//! Separates scheduling from the backend abstraction (ADR-0134): `backend.rs` defines the trait and
//! `pacman/` (Arch), `dnf/` (Fedora) and `apt/` (Debian, Ubuntu) implement it. The scheduler is
//! independent of `mantle.sysinfo` (ADR-0034).

pub mod apt;
pub mod backend;
pub mod controller;
pub mod dnf;
pub mod pacman;
pub mod reboot;

pub use controller::UpdatesController;
use shared::action::UpdatesAction;

/// `mantle.updates` dispatch (ADR-0037): `check`/`configure` send scheduler requests synchronously
/// (ADR-0034); `install` spawns the package-manager child.
pub fn dispatch(controller: &UpdatesController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<UpdatesAction>(&envelope.params) else { return };
    match action {
        UpdatesAction::Check => controller.check_now(),
        UpdatesAction::Configure { config } => controller.configure(config),
        UpdatesAction::Install => {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.install().await;
            });
        }
    }
}

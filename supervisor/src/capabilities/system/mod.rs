//! `mantle.system` provides reactive wall-clock time.
//!
//! Persisted state lives in `mantle.storage`, where config names the file (ADR-0136).

pub mod controller;

pub use controller::{SystemController, SystemSignal};
use shared::action::SystemAction;

pub fn dispatch(controller: &SystemController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<SystemAction>(&envelope.params) else { return };
    match action {
        SystemAction::Configure { settings } => controller.configure(settings),
    }
}

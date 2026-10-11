//! `mantle.clock` provides reactive wall-clock time.
//!
//! Persisted state lives in `mantle.storage`, where config names the file (ADR-0136).

pub mod controller;

pub use controller::ClockController;
use shared::action::ClockAction;

pub fn dispatch(controller: &ClockController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<ClockAction>(&envelope.params) else { return };
    match action {
        ClockAction::Configure { settings } => controller.configure(settings),
    }
}

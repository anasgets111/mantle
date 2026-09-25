//! `mantle.system` provides reactive wall-clock time.
//!
//! Persisted state lives in `mantle.storage`, where config names the file (ADR-0136).

pub mod controller;

pub use controller::{SystemController, SystemSignal};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SystemAction {
    /// Sets the push interval, `1` second until this. Each push lands on a wall-clock multiple of
    /// it, and one lands at once.
    Configure { settings: controller::SystemConfigure },
}

pub fn dispatch(controller: &SystemController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<SystemAction>(&envelope.params) else { return };
    match action {
        SystemAction::Configure { settings } => controller.configure(settings),
    }
}

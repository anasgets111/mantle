//! `mantle.keyboard` combines LED backlight, lock state, and compositor layout (ADR-0034) in
//! one `Arc<Mutex<KeyboardState>>` and signal channel.

pub mod controller;
pub mod layout;
pub mod locks;

pub use controller::{KeyboardController, KeyboardState};
use shared::action::KeyboardAction;

/// `set_backlight` is a spawned D-Bus write (ADR-0037, ADR-0029); `switch_layout` forwards
/// synchronously through the compositor link (ADR-0034).
pub fn dispatch(controller: &KeyboardController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<KeyboardAction>(&envelope.params) else { return };
    match action {
        KeyboardAction::SetBacklight { percent } => {
            controller.writes.submit(controller, percent);
        }
        KeyboardAction::SwitchLayout { index } => controller.switch_layout(index),
    }
}

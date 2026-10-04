//! `mantle.radio` reports rfkill block state per radio kind from `/dev/rfkill` alone, and writes
//! block requests to the same fd. No device node, or no radio yet, means no wake-up: Lua keeps
//! `mantle.radio` `nil`, like `brightness` without a backlight.

pub mod controller;

pub use controller::RadioController;
use shared::action::RadioAction;

/// A write is one non-blocking 8-byte `write`, so dispatch runs it inline.
pub fn dispatch(controller: &RadioController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<RadioAction>(&envelope.params) else { return };
    match action {
        RadioAction::SetBlocked { kind, blocked } => controller.set_blocked(Some(kind), blocked),
        RadioAction::SetAllBlocked { blocked } => controller.set_blocked(None, blocked),
    }
}

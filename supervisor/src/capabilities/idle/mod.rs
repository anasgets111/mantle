//! Idle capability (`mantle.idle`, ADR-0032). Notify uses the Supervisor's dedicated
//! `ext_idle_notifier_v1` Wayland connection; inhibit uses `org.freedesktop.login1.Manager.Inhibit`
//! on the existing system bus. They share one controller and generation-scoped cleanup.
//!
//! Inhibit also answers inbound: this shell owns `org.freedesktop.ScreenSaver`, where a browser's
//! video hold arrives, and takes the same logind fd for it (ADR-0231).
//!
//! Notify becomes inert (silent no-op, logged once) if the protocol is absent, its connection
//! fails, or setup exceeds `IDLE_NOTIFY_SETUP_TIMEOUT`. Background setup lets
//! [`IdleController::new`] return before a hung compositor. Inhibit rides the required system
//! bus, so only its per-request `Inhibit` call can fail (see [`IdleController::inhibit`]).
//!
//! Pure seams hold the decisions: [`notify::register_threshold_entry`] and
//! [`notify::cleanup_generation_thresholds`] for notify, [`inhibit::apply_inhibit`],
//! [`inhibit::apply_release_inhibit`] and [`inhibit::cleanup_generation_inhibit`] for refcounts.
//! [`IdleController`] wraps them with async/Wayland operations.

pub mod controller;
pub mod gate;
pub mod inhibit;
pub mod notify;
pub mod state;

pub use controller::IdleController;
pub use state::IdleState;

/// Actions accepted on an `idle` `CommandEnvelope`; exhaustive dispatch keeps variants and arms in
/// sync. `mantle.idle` has no actions; only its methods and the reload path send these.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleAction {
    /// Only `seconds` crosses the wire; the callbacks stay Renderer-side.
    Register {
        seconds: u64,
    },
    /// Sent when the Renderer's last callback at `seconds` is cancelled, never per handle
    /// (ADR-0232).
    Cancel {
        seconds: u64,
    },
    // Sent by the Renderer's `IdleRegistry::forget_thresholds` before each evaluation, not by a
    // config (ADR-0158). It rides the same ordered socket as the registrations that follow it,
    // which is the whole point: a reset the Supervisor ran on its own timing landed after them.
    ForgetThresholds,
    Inhibit {
        reason: String,
    },
    ReleaseInhibit,
}

/// `mantle.idle` action dispatch (ADR-0037): matches and spawns every `idle` `CommandEnvelope`;
/// each action carries its registering generation id (ADR-0032/ADR-0006).
pub fn dispatch(controller: &IdleController, envelope: &shared::CommandEnvelope) {
    let generation_id = envelope.params.generation_id;
    let Some(action) = crate::parse_action::<IdleAction>(&envelope.params) else { return };
    match action {
        // Both threshold arms run inline rather than in a spawned task. A forget and the
        // registrations that follow it come off one ordered socket, and two tasks would be free to
        // apply them the other way round, which is the whole failure this pair exists to stop
        // (ADR-0158).
        IdleAction::Register { seconds } => controller.register_threshold(generation_id, seconds),
        IdleAction::Cancel { seconds } => controller.cancel_threshold(generation_id, seconds),
        IdleAction::ForgetThresholds => controller.reset_thresholds(generation_id),
        IdleAction::Inhibit { reason } => {
            let controller = controller.clone();
            tokio::spawn(async move { controller.inhibit(generation_id, &reason).await });
        }
        IdleAction::ReleaseInhibit => {
            let controller = controller.clone();
            tokio::spawn(async move { controller.release_inhibit(generation_id).await });
        }
    }
}

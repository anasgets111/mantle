mod layout;
mod lua;
mod socket;
mod text;
mod wayland;

/// Bridges the socket thread (`Loader`/`Scene`/`mlua::Lua`, `!Send`, current-thread tokio
/// runtime) and the Wayland/EGL thread (`wayland::run`'s blocking dispatch loop) with three
/// plain `std::sync::mpsc` channels, matching this file's established "one dedicated OS thread
/// per blocking concern" pattern (build-steps.md Phase 14, § 15.2-15.3):
/// - `ready_tx`/`ready_rx`: the Wayland thread announces its one-time `ReadySignal` surface_id
///   list once every tracked surface has staged its null buffer.
/// - `presented_tx`/`presented_rx`: the Wayland thread announces each surface_id's
///   `wp_presentation_feedback` `presented` event as it happens, tagged with the `ActivateDraw`
///   nonce it was drawn for.
/// - `activate_tx`/`activate_rx`: the socket thread forwards a received `ActivateDraw` nonce to
///   the Wayland thread.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let (presented_tx, presented_rx) = std::sync::mpsc::channel::<shared::PresentationEvidence>();
    let (activate_tx, activate_rx) = std::sync::mpsc::channel::<u64>();

    socket::spawn_client(ready_rx, presented_rx, activate_tx);
    wayland::run(ready_tx, presented_tx, activate_rx)
}

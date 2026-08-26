mod audio;
mod dbus;
mod process;
mod reload;
mod socket;
mod watcher;

use std::error::Error;
use std::time::Duration;

use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};
use shared::{ApplyPendingReload, RendererFrame, ReevaluateReport, ReevaluateRequest, SupervisorFrame};

/// Matches `renderer/src/socket.rs`'s `OBLISK_GENERATION_ID` env-var default. Real generation-ID
/// assignment (tied to process spawning) doesn't exist yet -- same ceiling ADR-0020 already hit
/// on the transport side.
const RENDERER_GENERATION_ID: u32 = 0;

/// How long the Watcher waits after the *last* relevant `shell.lua` change before dispatching a
/// reload -- coalesces an editor's multi-event save into a single round trip. Fixed, not
/// configurable (docs/adr/0024 item 6).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// ADR-0006: an in-place reload must drop any Supervisor-held registrations tied to
/// `generation_id` before the fresh evaluation is applied, so a re-issued `idle:register_threshold`
/// (etc.) reads as a replacement, not a duplicate leak. No capability registers anything against
/// a `generation_id` yet -- the D-Bus/hardware controllers that would populate this (Phase 16)
/// don't exist -- so this is a real, called, currently-empty seam, matching this codebase's
/// established real-but-unwired precedent (`socket::GenerationRegistry` itself sat exactly like
/// this through Phase 9-10, see docs/adr/0020). See docs/adr/0024 item 2.
fn reset_registrations(_generation_id: u32) {}

/// Whether an `Unchanged` report's `sequence` still names the most recently sent `Reevaluate`
/// (`next_sequence`). A mismatch means a newer `Reevaluate` has already been sent for this
/// generation since this report's request went out (the debounced watcher fired again before
/// this round trip completed) -- the go-ahead must not be sent for a superseded evaluation
/// (Correctness review, docs/adr/0024 item 2).
fn is_current_reload(report_sequence: u64, next_sequence: u64) -> bool {
    report_sequence == next_sequence
}

/// Encodes `frame` and pushes it to `generation_id`'s connection, logging (rather than
/// propagating) either failure -- the one place every `SupervisorFrame` send goes through,
/// instead of each call site repeating its own encode-and-send-and-log-on-failure block.
fn push_frame(registry: &socket::GenerationRegistry, generation_id: u32, frame: &SupervisorFrame) {
    match serde_json::to_vec(frame) {
        Ok(payload) => {
            if !registry.send_to(generation_id, payload) {
                eprintln!("{frame:?} dropped: no Renderer connected for generation {generation_id}");
            }
        }
        Err(err) => eprintln!("failed to serialize {frame:?}: {err}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let connection = zbus::Connection::system().await?;
    let subject = current_session_subject()?;

    let (tx, mut challenges) = tokio::sync::mpsc::unbounded_channel();
    let agent = AuthenticationAgent::new(tx);
    register_agent(&connection, agent, &subject, "en_US.UTF-8", AGENT_OBJECT_PATH).await?;

    let (audio_tx, mut audio_apps) = tokio::sync::mpsc::unbounded_channel();
    // pipewire-rs's event loop is Rc-based and single-threaded (not Send) -- it needs its
    // own OS thread, not a tokio task.
    std::thread::spawn(move || audio::mixer::run(audio_tx));

    let socket_path = shared::control_socket_path()?;
    let (registry, mut inbound_frames) = socket::spawn_listener(&socket_path)?;

    let config_dir = shared::config_dir()?;
    let mut reload_events = watcher::spawn_watcher(&config_dir, RELOAD_DEBOUNCE)?;
    let mut next_sequence: u64 = 0;

    // ponytail: no dispatch table exists yet (build-steps.md Phase 9 builds transport and
    // connection identity only, not handlers for any specific capability -- see
    // docs/adr/0020-control-socket-transport-without-dispatch-or-pba-wiring.md). Draining
    // polkit challenges and inbound `Command` frames to a log line is still the ceiling until a
    // capability layer exists to push challenges to the Renderer's textfield (ADR-0005/ADR-0009,
    // see docs/adr/0015-polkit-pam-conversation-and-textfield-wiring-deferred.md) and route
    // inbound commands to a real capability handler. Audio apps got their real destination in
    // Phase 11 (docs/adr/0022): pushed as a `StateSnapshot` to the Renderer instead of logged.
    // `shell.lua` reload dispatch is real as of Phase 13 (docs/adr/0024): the Watcher fires a
    // `Reevaluate`, and the Renderer's verdict decides `reset_registrations`+`ApplyPendingReload`
    // (Unchanged), a log-only stand-in for a generation swap (TopologyChanged -- Phase 14's
    // `run_pba` is still unwired, ADR-0019), or a log-only rescue note (Failed).
    let mut audio_revision: u32 = 0;
    loop {
        tokio::select! {
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
            }
            Some(apps) = audio_apps.recv() => {
                audio_revision += 1;
                match serde_json::to_value(&apps) {
                    Ok(payload) => push_frame(&registry, RENDERER_GENERATION_ID, &SupervisorFrame::StateSnapshot(shared::StateSnapshot { revision: audio_revision, payload })),
                    Err(err) => eprintln!("failed to serialize audio StateSnapshot: {err}"),
                }
            }
            Some(()) = reload_events.recv() => {
                next_sequence += 1;
                push_frame(&registry, RENDERER_GENERATION_ID, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: next_sequence }));
            }
            Some(inbound) = inbound_frames.recv() => match inbound.frame {
                RendererFrame::Command(envelope) => {
                    eprintln!("inbound command from generation {}: {:?}", inbound.generation_id, envelope);
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence }) => {
                    if is_current_reload(sequence, next_sequence) {
                        reset_registrations(inbound.generation_id);
                        push_frame(&registry, inbound.generation_id, &SupervisorFrame::ApplyPendingReload(ApplyPendingReload { sequence }));
                    } else {
                        eprintln!(
                            "generation {}'s Unchanged report (sequence {sequence}) is stale -- a newer Reevaluate (sequence {next_sequence}) is \
                             already in flight; not applying",
                            inbound.generation_id
                        );
                    }
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) => {
                    eprintln!(
                        "generation {}'s shell.lua re-evaluation (sequence {sequence}) changed topology -- a generation swap is needed but \
                         `reload::run_pba` isn't wired to a real caller yet (docs/adr/0019, docs/adr/0024 item 1)",
                        inbound.generation_id
                    );
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence, error }) => {
                    eprintln!("generation {}'s shell.lua re-evaluation (sequence {sequence}) failed: {error}", inbound.generation_id);
                }
            },
            else => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_current_reload_matches_only_the_most_recently_sent_sequence() {
        assert!(is_current_reload(3, 3));
        assert!(!is_current_reload(3, 4), "a report for an older sequence than the last-sent one must be stale");
    }
}

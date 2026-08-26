mod audio;
mod dbus;
mod process;
mod reload;
mod reload_link;
mod socket;
mod watcher;

use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};
use reload_link::SocketCandidateLink;
use shared::{ApplyPendingReload, DeselectInput, PromoteGeneration, RendererFrame, ReevaluateReport, ReevaluateRequest, SupervisorFrame};

/// How long the Watcher waits after the *last* relevant `shell.lua` change before dispatching a
/// reload -- coalesces an editor's multi-event save into a single round trip. Fixed, not
/// configurable (docs/adr/0024 item 6).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// § 15.2/15.3's ready-signal and evidence-verification deadlines (`reload::PbaTimings`).
/// Seconds, not minutes, matching `reload.rs`'s own test constants' order of magnitude scaled up
/// for a real Candidate that has to actually bind Wayland/EGL rather than a fake resolving
/// immediately -- generous enough that a healthy Candidate never trips them, tight enough that a
/// wedged one doesn't leave a config edit hanging for a long time. `reap_grace` reuses
/// `process::DEFAULT_REAP_GRACE`, this constant's first real caller alongside `main`'s own
/// superseded-generation reap below.
const PBA_TIMINGS: reload::PbaTimings =
    reload::PbaTimings { ready_timeout: Duration::from_secs(2), evidence_timeout: Duration::from_secs(3), reap_grace: process::DEFAULT_REAP_GRACE };

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

/// Sends `frame` to `generation_id`, logging (not propagating) a failure. The one place every
/// `SupervisorFrame` send in this file's main loop goes through -- previously each call site
/// either hand-wrote its own `if let Err(err) = ... { eprintln!(...) }` (duplicated three times)
/// or, for the Swap messages, silently discarded the `Result` with `let _ =` entirely (Standards
/// + Correctness review: the only sends in this function whose failure went unlogged).
fn send_frame_logged(registry: &socket::GenerationRegistry, generation_id: u32, frame: &SupervisorFrame) {
    if let Err(err) = registry.send_frame(generation_id, frame) {
        eprintln!("failed to push {frame:?} to generation {generation_id}: {err}");
    }
}

/// Resolves the Renderer binary's path as a sibling of the currently-running Supervisor binary
/// (`Path::with_file_name` swaps the last path component, i.e. `target/{profile}/supervisor` ->
/// `target/{profile}/renderer` -- the standard same-workspace cargo layout). No packaging or
/// install-path configuration exists yet (docs/adr/0025) -- this assumption is the only one
/// available until one does.
fn renderer_binary_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name("renderer"))
}

/// One generation's identity and process handle while it's authoritative. Reassigned wholesale
/// on a successful swap -- real generation-ID assignment tied to process spawning (this phase)
/// replaces the old hardcoded `RENDERER_GENERATION_ID` constant every prior phase used.
struct Authoritative {
    generation_id: u32,
    child: tokio::process::Child,
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

    // Generation 0 is boot-spawned by the Supervisor itself, for the first time (docs/adr/0025
    // item 7) -- there is no shell without a Generation 0, so a spawn failure here is fatal to
    // `main`.
    let renderer_path = renderer_binary_path()?;
    let renderer_path_str = renderer_path.to_string_lossy().into_owned();
    let boot_child =
        process::spawn_group_leader(&renderer_path_str, &[], &[("OBLISK_GENERATION_ID".to_string(), "0".to_string())])?;
    let mut authoritative = Authoritative { generation_id: 0, child: boot_child };
    let mut next_generation_id: u32 = 1;

    // The last audio `StateSnapshot` pushed to the authoritative generation, reused to hydrate a
    // fresh Candidate's first evaluation (§ 15.2 point 1) -- or a fresh revision-0 empty one if
    // none has ever been pushed yet.
    let mut last_audio_snapshot: Option<shared::StateSnapshot> = None;
    let mut audio_revision: u32 = 0;
    loop {
        tokio::select! {
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
            }
            Some(apps) = audio_apps.recv() => {
                audio_revision += 1;
                match serde_json::to_value(&apps) {
                    Ok(payload) => {
                        let snapshot = shared::StateSnapshot { revision: audio_revision, payload };
                        send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::StateSnapshot(snapshot.clone()));
                        last_audio_snapshot = Some(snapshot);
                    }
                    Err(err) => eprintln!("failed to serialize audio StateSnapshot: {err}"),
                }
            }
            Some(()) = reload_events.recv() => {
                next_sequence += 1;
                send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: next_sequence }));
            }
            Some(inbound) = inbound_frames.recv() => match inbound.frame {
                RendererFrame::Command(envelope) => {
                    eprintln!("inbound command from generation {}: {:?}", inbound.generation_id, envelope);
                }
                RendererFrame::ReadySignal(_) | RendererFrame::PresentationEvidence(_) => {
                    // Both only matter mid-handshake, where `SocketCandidateLink` reads them
                    // directly off `inbound_frames` itself (see `TopologyChanged` below, and
                    // docs/adr/0025). One reaching this top-level match means it arrived
                    // *outside* any in-flight handshake this Supervisor is currently driving --
                    // stale, or a wire-protocol desync -- logged, not fatal.
                    eprintln!("generation {}'s handshake frame arrived outside any in-flight PBA handshake; dropping: {:?}", inbound.generation_id, inbound.frame);
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence }) => {
                    if is_current_reload(sequence, next_sequence) {
                        reset_registrations(inbound.generation_id);
                        send_frame_logged(&registry, inbound.generation_id, &SupervisorFrame::ApplyPendingReload(ApplyPendingReload { sequence }));
                    } else {
                        eprintln!(
                            "generation {}'s Unchanged report (sequence {sequence}) is stale -- a newer Reevaluate (sequence {next_sequence}) is \
                             already in flight; not applying",
                            inbound.generation_id
                        );
                    }
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) => {
                    // Phase 13's watcher becomes `run_pba`'s real caller here (build-steps.md
                    // Phase 14 item 5). Inlined synchronously inside this match arm, not
                    // `tokio::spawn`ed -- see docs/adr/0025's "why the main loop blocks" item:
                    // swaps are rare and bounded (seconds, not minutes -- PBA_TIMINGS above), and
                    // nothing else is capability-routed over this socket yet to starve.
                    let candidate_generation_id = next_generation_id;
                    next_generation_id += 1;
                    let candidate_envs = vec![
                        ("OBLISK_GENERATION_ID".to_string(), candidate_generation_id.to_string()),
                        ("OBLISK_PBA_CANDIDATE".to_string(), "1".to_string()),
                    ];
                    let snapshot = last_audio_snapshot.clone().unwrap_or(shared::StateSnapshot { revision: 0, payload: serde_json::json!({}) });
                    let mut link = SocketCandidateLink { registry: registry.clone(), candidate_generation_id, inbound: &mut inbound_frames };

                    match reload::run_pba(&renderer_path_str, &[], &candidate_envs, &mut link, &snapshot, sequence, PBA_TIMINGS).await {
                        Ok(outcome) => {
                            for surface_id in &outcome.promoted_surfaces {
                                send_frame_logged(&registry, authoritative.generation_id, &SupervisorFrame::DeselectInput(DeselectInput { surface_id: surface_id.clone() }));
                                send_frame_logged(&registry, candidate_generation_id, &SupervisorFrame::PromoteGeneration(PromoteGeneration { surface_id: surface_id.clone() }));
                            }
                            // `run_pba` never reaps `superseded` any more (docs/adr/0025 item 3)
                            // -- that's this caller's job, done only now that the Swap messages
                            // above have actually gone out.
                            match process::reap_process_group(&mut authoritative.child, process::DEFAULT_REAP_GRACE).await {
                                Ok(process::ReapOutcome::ExitedCleanly(status)) => {
                                    eprintln!("superseded generation {} exited cleanly: {status}", authoritative.generation_id);
                                }
                                Ok(process::ReapOutcome::Escalated(status)) => {
                                    eprintln!("superseded generation {} had to be escalated to SIGKILL: {status}", authoritative.generation_id);
                                }
                                Err(err) => {
                                    eprintln!("failed to reap superseded generation {}: {err}", authoritative.generation_id);
                                }
                            }
                            authoritative = Authoritative { generation_id: candidate_generation_id, child: outcome.candidate };
                        }
                        Err(failure) => {
                            match failure {
                                reload::PbaFailure::SpawnFailed(err) => {
                                    eprintln!("generation swap for sequence {sequence} failed: could not spawn the candidate: {err}");
                                }
                                reload::PbaFailure::Link { stage, source } => {
                                    eprintln!("generation swap for sequence {sequence} failed during {stage:?}: {source}");
                                }
                                reload::PbaFailure::Timeout { stage } => {
                                    eprintln!("generation swap for sequence {sequence} failed: {stage:?} timed out");
                                }
                                reload::PbaFailure::UnexpectedEvidence { stage, surface_id } => {
                                    eprintln!(
                                        "generation swap for sequence {sequence} failed during {stage:?}: unexpected evidence for surface_id {surface_id:?}"
                                    );
                                }
                                reload::PbaFailure::AbortReapFailed { original, reap_error } => {
                                    eprintln!(
                                        "generation swap for sequence {sequence} failed ({original:?}) and the candidate's abort-reap also failed: {reap_error}"
                                    );
                                }
                            }
                            eprintln!("{} stays authoritative", authoritative.generation_id);
                        }
                    }
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

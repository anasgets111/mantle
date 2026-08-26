mod audio;
mod dbus;
mod process;
mod reload;
mod socket;

use std::error::Error;

use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};

/// Matches `renderer/src/socket.rs`'s `OBLISK_GENERATION_ID` env-var default. Real generation-ID
/// assignment (tied to process spawning) doesn't exist yet -- same ceiling ADR-0020 already hit
/// on the transport side.
const RENDERER_GENERATION_ID: u32 = 0;

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
    let (registry, mut inbound_commands) = socket::spawn_listener(&socket_path)?;

    // ponytail: no dispatch table exists yet (build-steps.md Phase 9 builds transport and
    // connection identity only, not handlers for any specific capability -- see
    // docs/adr/0020-control-socket-transport-without-dispatch-or-pba-wiring.md). Draining
    // polkit challenges and inbound commands to a log line is still the ceiling until a
    // capability layer exists to push challenges to the Renderer's textfield (ADR-0005/ADR-0009,
    // see docs/adr/0015-polkit-pam-conversation-and-textfield-wiring-deferred.md) and route
    // inbound commands to a real capability handler. Audio apps got their real destination in
    // Phase 11 (docs/adr/0022): pushed as a `StateSnapshot` to the Renderer instead of logged.
    let mut audio_revision: u32 = 0;
    loop {
        tokio::select! {
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
            }
            Some(apps) = audio_apps.recv() => {
                audio_revision += 1;
                let encoded = serde_json::to_value(&apps)
                    .and_then(|payload| serde_json::to_vec(&shared::StateSnapshot { revision: audio_revision, payload }));
                match encoded {
                    Ok(payload) => {
                        if !registry.send_to(RENDERER_GENERATION_ID, payload) {
                            eprintln!("audio StateSnapshot dropped: no Renderer connected for generation {RENDERER_GENERATION_ID}");
                        }
                    }
                    Err(err) => eprintln!("failed to serialize audio StateSnapshot: {err}"),
                }
            }
            Some(command) = inbound_commands.recv() => {
                eprintln!("inbound command from generation {}: {:?}", command.generation_id, command.envelope);
            }
            else => break,
        }
    }
    Ok(())
}

mod audio;
mod dbus;
mod process;
mod reload;
mod socket;

use std::error::Error;

use dbus::polkit::{AGENT_OBJECT_PATH, AuthenticationAgent, current_session_subject, register_agent};

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
    let (_registry, mut inbound_commands) = socket::spawn_listener(&socket_path)?;

    // ponytail: no dispatch table exists yet (build-steps.md Phase 9 builds transport and
    // connection identity only, not handlers for any specific capability -- see
    // docs/adr/0020-control-socket-transport-without-dispatch-or-pba-wiring.md). Draining all
    // three channels to a log line is the ceiling until a capability layer exists to push
    // challenges to the Renderer's textfield (ADR-0005/ADR-0009, see
    // docs/adr/0015-polkit-pam-conversation-and-textfield-wiring-deferred.md), app streams to
    // Lua's audio.apps (see docs/adr/0017-audio-apps-lua-ipc-push-deferred.md), and inbound
    // commands to a real capability handler.
    loop {
        tokio::select! {
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
            }
            Some(apps) = audio_apps.recv() => {
                eprintln!("audio apps updated: {apps:?}");
            }
            Some(command) = inbound_commands.recv() => {
                eprintln!("inbound command from generation {}: {:?}", command.generation_id, command.envelope);
            }
            else => break,
        }
    }
    Ok(())
}

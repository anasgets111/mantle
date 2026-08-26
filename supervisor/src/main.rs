mod audio;
mod dbus;

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

    // ponytail: no core event loop exists yet (build-steps.md Phase 7/8 owns the Unix
    // socket server, process reaper, and reload orchestrator this process eventually runs
    // under). Draining both channels to a log line is the ceiling until that IPC layer
    // exists to push challenges to the Renderer's textfield (ADR-0005/ADR-0009, see
    // docs/adr/0015-polkit-pam-conversation-and-textfield-wiring-deferred.md) and app
    // streams to Lua's audio.apps (see docs/adr/0017-audio-apps-lua-ipc-push-deferred.md).
    loop {
        tokio::select! {
            Some(challenge) = challenges.recv() => {
                eprintln!("polkit authentication challenge received: {challenge:?}");
            }
            Some(apps) = audio_apps.recv() => {
                eprintln!("audio apps updated: {apps:?}");
            }
            else => break,
        }
    }
    Ok(())
}

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

    // ponytail: no core event loop exists yet (build-steps.md Phase 7/8 owns the Unix
    // socket server, process reaper, and reload orchestrator this process eventually runs
    // under). Draining challenges to a log line is the ceiling until that IPC layer exists
    // to push them to the Renderer's textfield instead (ADR-0005/ADR-0009); see
    // docs/adr/0015-polkit-pam-conversation-and-textfield-wiring-deferred.md.
    while let Some(challenge) = challenges.recv().await {
        eprintln!("polkit authentication challenge received: {challenge:?}");
    }
    Ok(())
}

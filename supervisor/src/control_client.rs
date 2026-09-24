//! Client half of `mantle set`, `mantle toggle` (ADR-0112) and `mantle call` (ADR-0197):
//! connect to the running Supervisor, send a handshake and one frame, and wait for its answer, so
//! a refused write or a failed call exits non-zero.
//!
//! Separate from `socket/mod.rs`, the listener: this is the only external connector, running from a
//! compositor keybind's `spawn` with no runtime, config directory, or D-Bus.

use std::error::Error;
use std::path::Path;

use std::time::Duration;

use shared::framing::{read_json_frame, write_json_frame};
use shared::{
    CONTROL_CLIENT_GENERATION, Call, CallOutcome, ConnectionHandshake, RendererFrame, SetState, SupervisorFrame,
};
use tokio::net::UnixStream;

/// How long `mantle set`/`toggle`/`call` waits for an answer.
///
/// Generous against the work a handler can actually do: config Lua runs under a 5ms CPU cap, so a
/// reply that has not arrived by now means the shell is wedged or the Renderer was replaced mid-call,
/// not that the handler is still thinking. Expiring says "outcome unknown", never "nothing
/// happened" -- the call may well have run.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Connects, handshakes as the control client, sends `frame` and returns its answer.
///
/// The `id` sent is zero and is overwritten by the Supervisor, which owns the pending table; a
/// client-chosen id would let one peer collect another's answer.
fn ask(instance_dir: &Path, frame: RendererFrame, name: &str) -> Result<CallOutcome, Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let path = shared::control_socket_path(instance_dir);
        let mut stream = UnixStream::connect(&path)
            .await
            .map_err(|err| format!("cannot reach the shell at {}: {err} (is mantle running?)", path.display()))?;
        write_json_frame(&mut stream, &ConnectionHandshake { generation_id: CONTROL_CLIENT_GENERATION }).await?;
        write_json_frame(&mut stream, &frame).await?;
        let answer = tokio::time::timeout(CALL_TIMEOUT, read_json_frame::<_, SupervisorFrame>(&mut stream))
            .await
            .map_err(|_| {
                format!("the shell did not answer `{name}` within {}s; it may still have run", CALL_TIMEOUT.as_secs())
            })??;
        match answer {
            SupervisorFrame::CallResult(result) => Ok(result.outcome),
            other => Err(format!("the shell answered `{name}` with {other:?} instead of a result").into()),
        }
    })
}

/// Writes one `state`, or fails with the onscreen generation's refusal (an undeclared name, a
/// value that does not fit).
pub fn send(set: SetState, instance_dir: &Path) -> Result<(), Box<dyn Error>> {
    let name = set.name.clone();
    match ask(instance_dir, RendererFrame::SetState { id: 0, set }, &name)? {
        CallOutcome::Failed(why) => Err(format!("state `{name}` refused: {why}").into()),
        CallOutcome::Returned(_) => Ok(()),
    }
}

/// Sends one `mantle call` and prints what the config returned.
pub fn call(name: String, arguments: Vec<serde_json::Value>, instance_dir: &Path) -> Result<(), Box<dyn Error>> {
    match ask(instance_dir, RendererFrame::Call(Call { id: 0, name: name.clone(), arguments }), &name)? {
        CallOutcome::Failed(why) => Err(format!("`{name}` failed: {why}").into()),
        CallOutcome::Returned(value) => {
            match value {
                // Nothing to say, so nothing is printed: an action run for its effect should not
                // make a keybind's shell noisy.
                serde_json::Value::Null => {}
                // A bare string prints as itself. `rec.toggle` answering `recording` is for a human
                // reading a terminal, and `"recording"` with quotes is for nobody.
                serde_json::Value::String(text) => println!("{text}"),
                other => println!("{other}"),
            }
            Ok(())
        }
    }
}

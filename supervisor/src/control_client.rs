//! Client half of `mantle set`, `mantle toggle` (ADR-0112), `mantle call` (ADR-0197), `mantle input` and their bare
//! listings: connect to the running Supervisor, send a handshake and one frame, and wait for its
//! answer, so a refused write or a failed call exits non-zero.
//!
//! Separate from `socket/mod.rs`, the listener: this is the only external connector, running from a
//! compositor keybind's `spawn` with no runtime, config directory, or D-Bus.

use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use shared::framing::{read_json_frame, write_json_frame};
use shared::{
    CONTROL_CLIENT_GENERATION, Call, CallOutcome, ConnectionHandshake, Declared, RendererFrame, SetState,
    SupervisorFrame,
};
use tokio::net::UnixStream;

/// How long `mantle set`/`toggle`/`call` waits for an answer.
///
/// Generous against the work a handler can actually do: config Lua runs under a 50ms CPU cap, so a
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

/// Sends one `mantle input` and fails with the shell's refusal (unknown or ambiguous surface, a
/// lock surface, a `secure_submit` field).
pub fn inject(inject: shared::Inject, instance_dir: &Path) -> Result<(), Box<dyn Error>> {
    let surface = inject.surface.clone();
    match ask(instance_dir, RendererFrame::Inject { id: 0, inject }, &surface)? {
        CallOutcome::Failed(why) => Err(format!("input to `{surface}` refused: {why}").into()),
        CallOutcome::Returned(_) => Ok(()),
    }
}

/// Prints the lines of a bare `call` (action names) or `set`/`toggle` (`name<TAB>value`), which the
/// generation formats.
pub fn list(declared: Declared, instance_dir: &Path) -> Result<(), Box<dyn Error>> {
    let lines = match ask(instance_dir, RendererFrame::ListDeclared { id: 0, declared }, "the listing")? {
        CallOutcome::Failed(why) => return Err(format!("the listing failed: {why}").into()),
        CallOutcome::Returned(lines) => lines,
    };
    let serde_json::Value::Array(lines) = lines else {
        return Err(format!("the shell answered the listing with {lines}").into());
    };
    // `writeln!`, not `println!`: a reader that quits early (`head`, fzf) closes the pipe, which
    // ends the listing quietly rather than panicking or printing an error.
    let mut stdout = std::io::stdout().lock();
    for line in lines.iter().filter_map(serde_json::Value::as_str) {
        match writeln!(stdout, "{line}") {
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            result => result?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use shared::framing::{read_frame_blocking, write_frame_blocking};

    use super::*;

    /// Answers one control connection in `dir` with `outcome`; joins to the frame it was sent.
    fn serve_once(dir: &Path, outcome: CallOutcome) -> std::thread::JoinHandle<RendererFrame> {
        let listener = std::os::unix::net::UnixListener::bind(shared::control_socket_path(dir)).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            // A client that never connects fails here rather than hanging `join`.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("no control client connected: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let handshake: ConnectionHandshake =
                serde_json::from_slice(&read_frame_blocking(&mut stream).unwrap()).unwrap();
            assert_eq!(handshake.generation_id, CONTROL_CLIENT_GENERATION);
            let frame = serde_json::from_slice(&read_frame_blocking(&mut stream).unwrap()).unwrap();
            let answer = SupervisorFrame::CallResult(shared::CallResult { id: 0, outcome });
            write_frame_blocking(&mut stream, &serde_json::to_vec(&answer).unwrap()).unwrap();
            frame
        })
    }

    #[test]
    fn each_command_sends_its_frame_and_a_refusal_fails_it() {
        let set = SetState { name: "open".into(), write: shared::StateWrite::Toggle };
        let inject = shared::Inject { surface: "bar".into(), steps: Vec::new() };
        type Client = Box<dyn Fn(&Path) -> Result<(), Box<dyn Error>>>;
        let cases: [(RendererFrame, Client); 4] = [
            (RendererFrame::SetState { id: 0, set: set.clone() }, Box::new(move |dir| send(set.clone(), dir))),
            (
                RendererFrame::Call(Call { id: 0, name: "go".into(), arguments: vec![serde_json::json!(1)] }),
                Box::new(|dir| call("go".into(), vec![serde_json::json!(1)], dir)),
            ),
            (
                RendererFrame::Inject { id: 0, inject: inject.clone() },
                Box::new(move |dir| super::inject(inject.clone(), dir)),
            ),
            (
                RendererFrame::ListDeclared { id: 0, declared: Declared::Actions },
                Box::new(|dir| list(Declared::Actions, dir)),
            ),
        ];
        for (expected, client) in cases {
            for outcome in [CallOutcome::Failed("no".into()), CallOutcome::Returned(serde_json::json!([]))] {
                let dir = tempfile::tempdir().unwrap();
                let refused = matches!(outcome, CallOutcome::Failed(_));
                let server = serve_once(dir.path(), outcome);
                assert_eq!(client(dir.path()).is_err(), refused, "{expected:?}");
                assert_eq!(server.join().unwrap(), expected);
            }
        }
    }
}

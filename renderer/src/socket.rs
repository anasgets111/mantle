//! Renderer-side Unix control-socket client (build-steps.md Phase 9).
//!
//! Connects to `$XDG_RUNTIME_DIR/oblisk-shell.sock` as a client; the Supervisor is the
//! listener (`supervisor/src/socket.rs`) -- Phase 11's text reuses this same direction
//! ("The Supervisor is the listener, the Renderer connects as client"), not inverted. Sends a
//! `shared::ConnectionHandshake` as the first frame, then holds the connection open.
//!
//! Runs on its own OS thread with a dedicated current-thread tokio runtime, same reasoning as
//! `text::shaping`'s dedicated worker thread (see that module's doc comment): the main thread
//! is occupied by `wayland::run()`'s blocking dispatch loop, and this is one dedicated I/O
//! task, not a general async-task need -- exactly what `renderer/Cargo.toml`'s existing
//! `rt`/`net`/`macros` tokio features (present since scaffolding, unused until now) were
//! staged for.
//!
//! Deliberately deferred: reading anything back off the connection (Phase 11, once the
//! Supervisor has a real `StateSnapshot` to push there's something to consume here); real
//! generation-ID assignment tied to process spawning (a later phase, once Phase 7/8's spawn
//! primitives are wired to this transport) -- for now the generation ID comes from the
//! `OBLISK_GENERATION_ID` env var, defaulting to `0`, matching the "no real caller yet"
//! pattern already used by `reload.rs` and `dbus/polkit.rs`.

use std::path::Path;

use shared::ConnectionHandshake;
use shared::framing::write_json_frame;
use tokio::net::UnixStream;

fn generation_id_from_env() -> u32 {
    std::env::var("OBLISK_GENERATION_ID").ok().and_then(|value| value.parse().ok()).unwrap_or(0)
}

/// Connects to `path` and sends the handshake identifying `generation_id`, returning the
/// live stream on success.
async fn connect_and_handshake(path: &Path, generation_id: u32) -> Result<UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = UnixStream::connect(path).await?;
    write_json_frame(&mut stream, &ConnectionHandshake { generation_id }).await?;
    Ok(stream)
}

/// Spawns the dedicated connect-and-hold-open thread. Connection failures (no Supervisor
/// listening yet, wrong path) are logged, not fatal -- build-steps.md doesn't yet define a
/// startup-ordering guarantee between the two processes.
pub fn spawn_client() {
    std::thread::spawn(|| {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("control-socket client: failed to start runtime: {err}");
                return;
            }
        };
        runtime.block_on(run());
    });
}

async fn run() {
    let path = match shared::control_socket_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("control-socket client: {err}");
            return;
        }
    };

    let generation_id = generation_id_from_env();
    let _stream = match connect_and_handshake(&path, generation_id).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("control-socket client: failed to connect to {}: {err}", path.display());
            return;
        }
    };

    // ponytail: nothing to read yet -- Phase 11 gives the Supervisor a real StateSnapshot to
    // push here. Holding the connection open (instead of dropping it right after the
    // handshake) is this phase's whole point: the Supervisor's listener stays able to address
    // this generation for as long as the process is alive.
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::framing::read_json_frame;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn connect_and_handshake_sends_a_handshake_the_listener_can_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk-shell.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let client = tokio::spawn({
            let path = path.clone();
            async move { connect_and_handshake(&path, 9).await }
        });

        let (mut server_side, _addr) = listener.accept().await.unwrap();
        let handshake: ConnectionHandshake = read_json_frame(&mut server_side).await.unwrap();
        assert_eq!(handshake.generation_id, 9);

        client.await.unwrap().unwrap();
    }
}

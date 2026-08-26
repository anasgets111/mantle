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
//! Reads a `shared::StateSnapshot` back off the connection on every push (Phase 11, ADR-0022):
//! decodes it, feeds its payload into a live [`crate::lua::signal::Signal`] registered on a
//! [`crate::lua::Loader`], and re-evaluates a proof-of-wiring `shell.lua` literal against it --
//! this module is the `lua` subtree's first production caller.
//!
//! Deliberately deferred: real generation-ID assignment tied to process spawning (a later phase,
//! once Phase 7/8's spawn primitives are wired to this transport) -- for now the generation ID
//! comes from the `OBLISK_GENERATION_ID` env var, defaulting to `0`; reconnection if the
//! connection drops (mirrors `supervisor/src/socket.rs`'s own "no accept-loop restart policy"
//! ceiling, same reasoning, symmetric on this side).

use std::path::Path;

use shared::framing::{self, write_json_frame};
use shared::{ConnectionHandshake, StateSnapshot};
use tokio::net::UnixStream;

use crate::layout::{self, Scene};
use crate::lua::{self, Loader};
use crate::lua::signal::LiveSignalHandle;
use crate::text::shaping::ShapingHandle;

/// No real `shell.lua` file exists yet -- Phase 13's Watcher owns that location -- so this
/// hardcoded literal proves the same `Loader::evaluate` path a real file will later go through.
/// Re-evaluated fresh on every push, matching every other `Signal`/`computed` recomputation in
/// this codebase: no caching.
const PROOF_OF_WIRING_SHELL: &str = r#"return surface { id = "bar", layer = "Top", audio_apps = audio:get() }"#;

/// Real per-output pixel dimensions aren't threaded from Wayland into this thread yet (see
/// docs/adr/0023 item 6) -- `wayland::mod`'s output/surface objects live on the main thread,
/// this thread only has the socket connection and the Lua loader. Standing in until that wiring
/// exists, matching `PROOF_OF_WIRING_SHELL`'s own hardcoded-stand-in precedent from Phase 11.
const PLACEHOLDER_OUTPUT_SIZE: layout::LogicalSize = layout::LogicalSize { width: 1920.0, height: 40.0 };

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
    let mut stream = match connect_and_handshake(&path, generation_id).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("control-socket client: failed to connect to {}: {err}", path.display());
            return;
        }
    };

    let loader = match Loader::new() {
        Ok(loader) => loader,
        Err(err) => {
            eprintln!("control-socket client: failed to start the Lua loader: {err}");
            return;
        }
    };
    let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
    if let Err(err) = loader.set_global("audio", signal) {
        eprintln!("control-socket client: failed to register the audio signal: {err}");
        return;
    }

    let mut scene = Scene::new();
    let shaping = ShapingHandle::spawn();

    receive_loop(&mut stream, &loader, &handle, |result| match result {
        Ok(output) => apply_to_scene(&mut scene, &shaping, &output),
        Err(err) => eprintln!("shell.lua evaluation failed: {err}"),
    })
    .await;
}

/// Reconciles one loader evaluation's surfaces into the retained scene and logs the outcome --
/// `layout`'s first production caller (Phase 12), matching `LoadOutput`'s downstream consumer
/// named in docs/adr/0022 item 6.
fn apply_to_scene(scene: &mut Scene, shaping: &ShapingHandle, output: &lua::LoadOutput) {
    match scene.apply(&output.surfaces, PLACEHOLDER_OUTPUT_SIZE, shaping) {
        Ok(()) => {
            for surface in &output.surfaces {
                let resolved = layout::node::parse_surface_id(&surface.properties).ok().and_then(|id| scene.surface(&id));
                match resolved {
                    Some(r) => eprintln!(
                        "layout resolved: surface {:?} kind={} rect={:?} visible={} children={} properties={}",
                        surface.kind,
                        r.kind,
                        r.rect,
                        r.visible,
                        r.children.len(),
                        r.properties.len()
                    ),
                    None => eprintln!("layout resolved but surface {:?} has no resolvable `id`", surface.kind),
                }
            }
        }
        Err(err) => eprintln!("layout resolution failed: {err}"),
    }
}

/// Feeds one received `StateSnapshot` into the live `audio` signal, then re-evaluates
/// [`PROOF_OF_WIRING_SHELL`] against it -- the seam that proves the `Loader` (Phase 10) and the
/// live `Signal` (Phase 11) compose correctly.
fn handle_snapshot(loader: &Loader, handle: &LiveSignalHandle, snapshot: StateSnapshot) -> Result<lua::LoadOutput, lua::LoaderError> {
    let value = loader.to_lua_value(&snapshot.payload)?;
    handle.set(value);
    loader.evaluate(PROOF_OF_WIRING_SHELL)
}

/// Reads `StateSnapshot` frames off `stream` until it closes or a transport error occurs,
/// running each one through [`handle_snapshot`] and reporting the result to `on_result`. A
/// frame that fails to decode as JSON is a transport-level failure here (unlike
/// `supervisor/src/socket.rs`'s inbound `CommandEnvelope` loop, which tolerates a malformed
/// frame and keeps reading) -- this connection has exactly one sender (the Supervisor) and one
/// message shape, so a bad frame means the two sides have desynced, not a stray bad actor. Logs
/// why the loop ended either way: a clean disconnect and a mid-stream decode failure both stop
/// the loop, but only one of them is expected, and telling them apart requires the log line.
async fn receive_loop<S, F>(stream: &mut S, loader: &Loader, handle: &LiveSignalHandle, mut on_result: F)
where
    S: tokio::io::AsyncRead + Unpin,
    F: FnMut(Result<lua::LoadOutput, lua::LoaderError>),
{
    loop {
        match framing::read_json_frame::<_, StateSnapshot>(stream).await {
            Ok(snapshot) => on_result(handle_snapshot(loader, handle, snapshot)),
            Err(err) => {
                eprintln!("control-socket client: connection ended: {err}");
                break;
            }
        }
    }
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

    fn sample_snapshot(app_name: &str) -> StateSnapshot {
        StateSnapshot { revision: 1, payload: serde_json::json!([{ "node_id": 1, "pid": 100, "app_name": app_name, "process_name": null }]) }
    }

    fn app_name_from_output(output: &lua::LoadOutput) -> String {
        let audio_apps = output.surfaces[0].properties.get("audio_apps").unwrap();
        let table = audio_apps.as_table().unwrap();
        let first_app: mlua::Table = table.get(1).unwrap();
        first_app.get::<String>("app_name").unwrap()
    }

    #[test]
    fn handle_snapshot_produces_a_surface_carrying_the_pushed_payload() {
        let loader = Loader::new().unwrap();
        let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("audio", signal).unwrap();

        let output = handle_snapshot(&loader, &handle, sample_snapshot("Zen")).unwrap();
        assert_eq!(app_name_from_output(&output), "Zen");
    }

    #[tokio::test]
    async fn receive_loop_updates_the_live_signal_on_every_frame_not_just_the_first() {
        let loader = Loader::new().unwrap();
        let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("audio", signal).unwrap();

        let (mut client_side, mut server_side) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            shared::framing::write_json_frame(&mut client_side, &sample_snapshot("Zen")).await.unwrap();
            shared::framing::write_json_frame(&mut client_side, &sample_snapshot("Firefox")).await.unwrap();
        });

        let mut app_names_seen = Vec::new();
        receive_loop(&mut server_side, &loader, &handle, |result| {
            app_names_seen.push(app_name_from_output(&result.unwrap()));
        })
        .await;

        writer.await.unwrap();
        assert_eq!(app_names_seen, vec!["Zen", "Firefox"], "the second frame must overwrite the first, not be ignored");
    }

    #[tokio::test]
    async fn a_received_snapshots_output_reconciles_into_a_resolvable_layout_scene() {
        let loader = Loader::new().unwrap();
        let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("audio", signal).unwrap();

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();

        let (mut client_side, mut server_side) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            shared::framing::write_json_frame(&mut client_side, &sample_snapshot("Zen")).await.unwrap();
        });

        receive_loop(&mut server_side, &loader, &handle, |result| {
            apply_to_scene(&mut scene, &shaping, &result.unwrap());
        })
        .await;

        writer.await.unwrap();
        assert!(scene.surface("bar").is_some(), "PROOF_OF_WIRING_SHELL's `bar` surface must be resolvable after apply");
    }
}

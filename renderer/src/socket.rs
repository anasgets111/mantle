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
//! Real `shell.lua` reload flow (build-steps.md Phase 13; `CONTEXT.md`, Watcher/Rollback/
//! In-place reload/Generation swap; docs/adr/0024): a `shared::StateSnapshot` push only
//! hydrates the live `audio` signal now -- it no longer triggers any Lua evaluation, unlike
//! Phase 11's proof-of-wiring hack. Evaluation is driven by the Supervisor's own
//! `shared::SupervisorFrame::Reevaluate`, sent after its `inotify` watch on
//! `~/.config/oblisk/` detects a debounced edit to `shell.lua`:
//!
//! 1. On `Reevaluate`, [`RendererClient::handle_reevaluate`] reads and evaluates the real
//!    `shell.lua` file ([`crate::lua::Loader::evaluate_file`]) *without* applying it to the
//!    retained [`Scene`] yet, diffs the evaluation's topology
//!    (`crate::layout::node::SurfaceTopology`) against whatever's currently applied, and reports
//!    back a `shared::ReevaluateReport::Unchanged`, `TopologyChanged`, or `Failed` verdict -- the
//!    Supervisor (CONTEXT.md's Watcher) owns what happens next, not this module.
//! 2. `Unchanged` evaluations are kept as `pending`, applied to the `Scene` only once the
//!    Supervisor sends back `ApplyPendingReload` for that same sequence -- never eagerly,
//!    since a `TopologyChanged` verdict must leave this generation's own scene untouched (that
//!    case is a generation swap, a different generation's job, Phase 14).
//! 3. `applied_topology` is `None` until an evaluation is actually applied (nothing yet, or the
//!    prior applied evaluation was superseded by rescue -- see below). `handle_reevaluate` treats
//!    `None` as "safe to apply", not as an empty topology to diff against: after a startup
//!    failure there's nothing to protect, so the next successful evaluation -- whether it's the
//!    file the user just fixed, or the same one retried -- must be able to recover, not be
//!    permanently misclassified as `TopologyChanged` (which nothing here ever applies).
//! 4. An evaluation failure sets the ad-hoc `rescue` global's `is_rescue`/`error_log` fields
//!    (mirrors the ad-hoc `audio` global, ADR-0022's precedent -- not the full `oblisk.*`
//!    signal tree) and leaves the prior applied scene untouched (`CONTEXT.md`'s Rollback).
//!
//! Deliberately deferred: real generation-ID assignment tied to process spawning (a later phase,
//! once Phase 7/8's spawn primitives are wired to this transport) -- for now the generation ID
//! comes from the `OBLISK_GENERATION_ID` env var, defaulting to `0`; reconnection if the
//! connection drops (mirrors `supervisor/src/socket.rs`'s own "no accept-loop restart policy"
//! ceiling, same reasoning, symmetric on this side).

use std::path::{Path, PathBuf};

use shared::framing::{self, write_json_frame};
use shared::{ApplyPendingReload, ConnectionHandshake, ReevaluateReport, ReevaluateRequest, RendererFrame, StateSnapshot, SupervisorFrame};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

use crate::layout::node::SurfaceTopology;
use crate::layout::{self, Scene};
use crate::lua::signal::LiveSignalHandle;
use crate::lua::{self, Loader};
use crate::text::shaping::ShapingHandle;

/// Real per-output pixel dimensions aren't threaded from Wayland into this thread yet (see
/// docs/adr/0023 item 6) -- `wayland::mod`'s output/surface objects live on the main thread,
/// this thread only has the socket connection and the Lua loader.
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

/// One connection's reload bookkeeping. `applied_topology` is `None` until an evaluation is
/// actually applied to the `Scene` -- see the module doc comment point 3 for why that's not the
/// same thing as an empty topology. `pending` holds the evaluated-but-not-yet-applied output
/// (and its already-computed topology, so `handle_apply_pending` doesn't need to recompute it)
/// between a `Reevaluate` that reported `Unchanged` and its matching `ApplyPendingReload`.
struct ReloadState {
    applied_topology: Option<Vec<SurfaceTopology>>,
    pending: Option<(u64, lua::LoadOutput, Vec<SurfaceTopology>)>,
}

/// Everything one connection's reload/dispatch loop needs, grouped so it travels as one
/// receiver instead of the same 6-7 pieces threaded through every function's parameter list
/// separately (Standards review, docs/adr/0024).
struct RendererClient {
    loader: Loader,
    shell_lua_path: PathBuf,
    scene: Scene,
    shaping: ShapingHandle,
    audio_handle: LiveSignalHandle,
    rescue_handle: LiveSignalHandle,
    state: ReloadState,
}

impl RendererClient {
    fn new(loader: Loader, shell_lua_path: PathBuf, shaping: ShapingHandle, audio_handle: LiveSignalHandle, rescue_handle: LiveSignalHandle) -> Self {
        Self {
            loader,
            shell_lua_path,
            scene: Scene::new(),
            shaping,
            audio_handle,
            rescue_handle,
            state: ReloadState { applied_topology: None, pending: None },
        }
    }

    fn set_rescue_state(&self, is_rescue: bool, error_log: &str) {
        match rescue_table(&self.loader, is_rescue, error_log) {
            Ok(table) => self.rescue_handle.set(mlua::Value::Table(table)),
            Err(err) => eprintln!("control-socket client: failed to build rescue state: {err}"),
        }
    }

    /// `StateSnapshot` pushes only hydrate the live `audio` signal now -- no Lua evaluation runs
    /// from this path any more (see the module doc comment).
    fn apply_state_snapshot(&self, snapshot: StateSnapshot) -> mlua::Result<()> {
        let value = self.loader.to_lua_value(&snapshot.payload)?;
        self.audio_handle.set(value);
        Ok(())
    }

    /// Evaluates `shell.lua` once at startup and applies it directly -- no round trip through the
    /// Supervisor needed, since there's no prior applied scene to protect yet (build-steps.md
    /// Phase 13). Leaves `state.applied_topology` at `None` on any failure: a startup failure
    /// leaves the shell blank (docs/adr/0024 item 4) -- `CONTEXT.md`'s Rollback guarantee is
    /// about a *re*-evaluation keeping its prior scene, and there is no prior scene on first
    /// boot. Because `None` also means "safe to apply" (not "topology []"), a later successful
    /// `Reevaluate` can still recover from this state instead of being stuck forever.
    fn run_startup_evaluation(&mut self) {
        match evaluate_and_topology(&self.loader, &self.shell_lua_path) {
            Ok((output, topology)) => match self.scene.apply(&output.surfaces, PLACEHOLDER_OUTPUT_SIZE, &self.shaping) {
                Ok(()) => {
                    log_applied_surfaces(&self.scene, &output);
                    self.set_rescue_state(false, "");
                    self.state.applied_topology = Some(topology);
                }
                Err(err) => {
                    eprintln!("control-socket client: startup shell.lua evaluated but failed to apply to the scene: {err}");
                    self.set_rescue_state(true, &err.to_string());
                }
            },
            Err(err) => {
                eprintln!("control-socket client: startup shell.lua evaluation failed: {err}");
                self.set_rescue_state(true, &err.to_string());
            }
        }
    }

    /// Runs one `Reevaluate` request: evaluates `shell.lua`, classifies the result against
    /// `state.applied_topology`, updates `state.pending` and the rescue signal, and writes the
    /// verdict back over `write_half`. `applied_topology == None` (nothing ever applied, e.g.
    /// after a startup or prior reload failure) is treated as "not changed" -- there's nothing to
    /// protect, so the fresh evaluation is safe to stage as `pending` -- see the module doc
    /// comment point 3.
    async fn handle_reevaluate<W: AsyncWrite + Unpin>(&mut self, request: ReevaluateRequest, write_half: &mut W) {
        let report = match evaluate_and_topology(&self.loader, &self.shell_lua_path) {
            Ok((output, topology)) => {
                self.set_rescue_state(false, "");
                let topology_changed = self.state.applied_topology.as_ref().is_some_and(|applied| applied != &topology);
                if topology_changed {
                    // A topology-changed generation must not have its own scene mutated -- that's
                    // the swap path, a different generation's job (CONTEXT.md, Generation swap).
                    ReevaluateReport::TopologyChanged { sequence: request.sequence }
                } else {
                    self.state.pending = Some((request.sequence, output, topology));
                    ReevaluateReport::Unchanged { sequence: request.sequence }
                }
            }
            Err(err) => {
                self.set_rescue_state(true, &err.to_string());
                ReevaluateReport::Failed { sequence: request.sequence, error: err.to_string() }
            }
        };

        if let Err(err) = write_json_frame(write_half, &RendererFrame::ReevaluateReport(report)).await {
            eprintln!("control-socket client: failed to send a ReevaluateReport: {err}");
        }
    }

    /// Applies `state.pending` to the `Scene` only if it's still the evaluation `apply.sequence`
    /// refers to -- a mismatch means a newer `Reevaluate` has already superseded it (the
    /// debounced watcher fired again before this round trip completed); logged and ignored, not
    /// fatal.
    fn handle_apply_pending(&mut self, apply: ApplyPendingReload) {
        if !matches!(&self.state.pending, Some((sequence, _, _)) if *sequence == apply.sequence) {
            eprintln!("control-socket client: ApplyPendingReload({}) doesn't match the currently pending reload; ignoring", apply.sequence);
            return;
        }
        let (_, output, topology) = self.state.pending.take().expect("just confirmed Some above");
        match self.scene.apply(&output.surfaces, PLACEHOLDER_OUTPUT_SIZE, &self.shaping) {
            Ok(()) => {
                log_applied_surfaces(&self.scene, &output);
                self.state.applied_topology = Some(topology);
            }
            Err(err) => eprintln!("control-socket client: ApplyPendingReload's stored evaluation failed to apply: {err}"),
        }
    }

    /// Reads `shared::SupervisorFrame`s off `read_half` until the connection ends, dispatching
    /// each to `apply_state_snapshot`/`handle_reevaluate`/`handle_apply_pending`. A frame that
    /// fails to decode is a transport-level failure here (this connection has exactly one
    /// sender, the Supervisor, and a fixed set of message shapes -- a bad frame means the two
    /// sides have desynced, not a stray bad actor), unlike an `ApplyPendingReload` sequence
    /// mismatch, which is an expected, recoverable race, not a decode failure.
    async fn dispatch_loop<R, W>(&mut self, read_half: &mut R, write_half: &mut W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        loop {
            match framing::read_json_frame::<_, SupervisorFrame>(read_half).await {
                Ok(SupervisorFrame::StateSnapshot(snapshot)) => {
                    if let Err(err) = self.apply_state_snapshot(snapshot) {
                        eprintln!("control-socket client: failed to convert a pushed StateSnapshot to a Lua value: {err}");
                    }
                }
                Ok(SupervisorFrame::Reevaluate(request)) => {
                    self.handle_reevaluate(request, write_half).await;
                }
                Ok(SupervisorFrame::ApplyPendingReload(apply)) => {
                    self.handle_apply_pending(apply);
                }
                Err(err) => {
                    eprintln!("control-socket client: connection ended: {err}");
                    break;
                }
            }
        }
    }
}

async fn run() {
    let path = match shared::control_socket_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("control-socket client: {err}");
            return;
        }
    };
    let shell_lua_path = match shared::shell_lua_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("control-socket client: failed to resolve shell.lua's path: {err}");
            return;
        }
    };

    let generation_id = generation_id_from_env();
    let stream = match connect_and_handshake(&path, generation_id).await {
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
    let (signal, audio_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
    if let Err(err) = loader.set_global("audio", signal) {
        eprintln!("control-socket client: failed to register the audio signal: {err}");
        return;
    }
    let rescue_handle = match register_rescue_signal(&loader) {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("control-socket client: failed to register the rescue signal: {err}");
            return;
        }
    };

    let shaping = ShapingHandle::spawn();
    let mut client = RendererClient::new(loader, shell_lua_path, shaping, audio_handle, rescue_handle);
    client.run_startup_evaluation();

    let (mut read_half, mut write_half) = stream.into_split();
    client.dispatch_loop(&mut read_half, &mut write_half).await;
}

/// Builds the `{ is_rescue, error_log }` table and registers it as the ad-hoc `rescue` global
/// (mirrors the ad-hoc `audio` global, ADR-0022's precedent -- see docs/adr/0024 item 3, not
/// the full `oblisk.*` signal tree). Returns the handle so later evaluations can update it.
fn register_rescue_signal(loader: &Loader) -> mlua::Result<LiveSignalHandle> {
    let table = rescue_table(loader, false, "")?;
    let (signal, handle) = lua::signal::Signal::new_live(mlua::Value::Table(table));
    loader.set_global("rescue", signal)?;
    Ok(handle)
}

fn rescue_table(loader: &Loader, is_rescue: bool, error_log: &str) -> mlua::Result<mlua::Table> {
    let table = loader.create_table()?;
    table.set("is_rescue", is_rescue)?;
    table.set("error_log", error_log)?;
    Ok(table)
}

/// `output.surfaces`' topology fingerprint, order-sensitive (`CONTEXT.md`, Topology change). A
/// surface whose topology fields don't type-check fails with [`lua::LoaderError::InvalidTopology`]
/// -- a distinct message from an actual top-level-return shape error, since conflating the two
/// (as an earlier version of this function did) produced a misleading `rescue.error_log`.
fn surfaces_topology(output: &lua::LoadOutput) -> Result<Vec<SurfaceTopology>, lua::LoaderError> {
    let mut topology = Vec::with_capacity(output.surfaces.len());
    for surface in &output.surfaces {
        let fingerprint = layout::node::surface_topology(&surface.properties).map_err(|err| lua::LoaderError::InvalidTopology(err.to_string()))?;
        topology.push(fingerprint);
    }
    Ok(topology)
}

fn evaluate_and_topology(loader: &Loader, shell_lua_path: &Path) -> Result<(lua::LoadOutput, Vec<SurfaceTopology>), lua::LoaderError> {
    let output = loader.evaluate_file(shell_lua_path)?;
    let topology = surfaces_topology(&output)?;
    Ok((output, topology))
}

/// Logs each surface's resolved geometry after a successful `scene.apply` -- diagnostic
/// visibility only, matching Phase 12's original `apply_to_scene` logging.
fn log_applied_surfaces(scene: &Scene, output: &lua::LoadOutput) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::framing::read_json_frame;
    use tokio::io::AsyncWriteExt;
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

    fn write_shell_lua(dir: &std::path::Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("shell.lua");
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Reads `rescue:get()`'s current `is_rescue`/`error_log` fields back out by evaluating a
    /// tiny probe script -- `LiveSignalHandle` only exposes `set`, so this is the only way to
    /// observe what a prior `set_rescue_state` call actually stored.
    fn rescue_state(loader: &Loader) -> (bool, String) {
        let output = loader
            .evaluate(r#"return surface { id = "_rescue_probe", layer = "Top", is_rescue = rescue:get().is_rescue, error_log = rescue:get().error_log }"#)
            .unwrap();
        let props = &output.surfaces[0].properties;
        let is_rescue = props.get("is_rescue").unwrap().as_boolean().unwrap();
        let error_log = props.get("error_log").unwrap().as_string().unwrap().to_string_lossy();
        (is_rescue, error_log)
    }

    fn test_client(shell_lua_path: &std::path::Path) -> RendererClient {
        let loader = Loader::new().unwrap();
        let (audio_signal, audio_handle) = lua::signal::Signal::new_live(mlua::Value::Nil);
        loader.set_global("audio", audio_signal).unwrap();
        let rescue_handle = register_rescue_signal(&loader).unwrap();
        RendererClient::new(loader, shell_lua_path.to_path_buf(), ShapingHandle::spawn(), audio_handle, rescue_handle)
    }

    #[test]
    fn apply_state_snapshot_updates_the_live_signal_without_evaluating_shell_lua() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let client = test_client(&missing);

        let snapshot = StateSnapshot { revision: 1, payload: serde_json::json!({ "app_name": "Zen" }) };
        client.apply_state_snapshot(snapshot).unwrap();

        let output = client.loader.evaluate(r#"return surface { id = "bar", layer = "Top", app_name = audio:get().app_name }"#).unwrap();
        let app_name = output.surfaces[0].properties.get("app_name").unwrap().as_string().unwrap().to_string_lossy();
        assert_eq!(app_name, "Zen");
    }

    #[test]
    fn run_startup_evaluation_applies_a_valid_file_and_clears_rescue() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);

        client.run_startup_evaluation();

        assert!(client.scene.surface("bar").is_some());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    #[test]
    fn run_startup_evaluation_on_a_missing_file_sets_rescue_and_leaves_scene_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let mut client = test_client(&missing);

        client.run_startup_evaluation();

        assert!(client.state.applied_topology.is_none());
        assert!(client.scene.surface("bar").is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(!error_log.is_empty());
    }

    #[tokio::test]
    async fn a_successful_reevaluate_after_a_startup_failure_recovers_instead_of_reporting_topology_changed_forever() {
        // Regression test for a CONFIRMED correctness finding: treating "nothing applied yet" as
        // an empty topology (rather than "no prior state to protect") made every subsequent
        // evaluation -- even a fix to a syntactically valid file -- permanently misclassify as
        // `TopologyChanged`, which nothing here ever applies, leaving the shell blank forever.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("shell.lua");
        let mut client = test_client(&missing);
        client.run_startup_evaluation();
        assert!(client.state.applied_topology.is_none(), "startup must have failed (no file yet)");

        write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 1 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(
            frame,
            RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 1 }),
            "the first successful evaluation after a startup failure must be treated as safe to apply, not a topology change"
        );
        assert!(client.state.pending.is_some());
    }

    #[tokio::test]
    async fn handle_reevaluate_reports_unchanged_and_stores_pending_when_topology_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        client.state.applied_topology = Some(surfaces_topology(&client.loader.evaluate_file(&path).unwrap()).unwrap());

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 5 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(frame, RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence: 5 }));
        assert!(matches!(&client.state.pending, Some((sequence, _, _)) if *sequence == 5));
    }

    #[tokio::test]
    async fn handle_reevaluate_reports_topology_changed_and_does_not_store_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        // Seed a *different* applied topology (a different id) so the fresh evaluation reads as changed.
        client.state.applied_topology =
            Some(vec![SurfaceTopology { id: "other".to_string(), layer: "Top".to_string(), anchor: Default::default(), monitor: "All".to_string() }]);

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 1 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(frame, RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
        assert!(client.state.pending.is_none(), "a topology-changed generation must not stage a pending apply");
    }

    #[tokio::test]
    async fn handle_reevaluate_reports_failed_and_sets_rescue_on_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), "this is not lua");
        let mut client = test_client(&path);

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 2 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        match frame {
            RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence, error }) => {
                assert_eq!(sequence, 2);
                assert!(!error.is_empty());
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(client.state.pending.is_none());
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue);
        assert!(!error_log.is_empty());
    }

    #[tokio::test]
    async fn handle_reevaluate_reports_a_topology_field_error_distinctly_from_a_top_level_return_error() {
        // Regression test for a minor correctness finding: a topology-field type error (e.g.
        // `anchor.top` not a boolean) used to be folded into `InvalidTopLevelReturn`'s fixed
        // "must be a `surface` node or an array of them" message, which is wrong for this case.
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top", anchor = { top = "yes" } }"#);
        let mut client = test_client(&path);

        let (mut wire, mut server) = tokio::io::duplex(4096);
        client.handle_reevaluate(ReevaluateRequest { sequence: 1 }, &mut server).await;
        drop(server);

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        match frame {
            RendererFrame::ReevaluateReport(ReevaluateReport::Failed { error, .. }) => {
                assert!(error.contains("topology"), "expected a topology-specific message, got: {error}");
                assert!(!error.contains("top-level return"), "must not reuse the unrelated top-level-return message, got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn handle_apply_pending_reconciles_the_pending_evaluation_into_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        let (output, topology) = evaluate_and_topology(&client.loader, &path).unwrap();
        client.state.pending = Some((3, output, topology));

        client.handle_apply_pending(ApplyPendingReload { sequence: 3 });

        assert!(client.scene.surface("bar").is_some());
        assert!(client.state.pending.is_none());
        assert_eq!(client.state.applied_topology.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn handle_apply_pending_ignores_a_mismatched_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        let (output, topology) = evaluate_and_topology(&client.loader, &path).unwrap();
        client.state.pending = Some((3, output, topology));

        client.handle_apply_pending(ApplyPendingReload { sequence: 99 });

        assert!(client.scene.surface("bar").is_none(), "a stale ApplyPendingReload must not apply");
        assert!(client.state.pending.is_some(), "the still-current pending evaluation must survive a mismatched Apply");
    }

    #[tokio::test]
    async fn dispatch_loop_answers_a_reevaluate_frame_with_a_report_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return surface { id = "bar", layer = "Top" }"#);
        let mut client = test_client(&path);
        // A different applied topology so the fresh evaluation reads as changed -- proves the
        // wire-level decode/dispatch/encode path, not `handle_reevaluate`'s own classification
        // logic (already covered by the tests above).
        client.state.applied_topology =
            Some(vec![SurfaceTopology { id: "other".to_string(), layer: "Top".to_string(), anchor: Default::default(), monitor: "All".to_string() }]);

        // Not `tokio::spawn`: `Loader`/`Scene` hold `mlua`/`Rc`-backed state that isn't `Send`
        // (the real client only ever runs on its own dedicated current-thread runtime -- see
        // the module doc comment). Everything below runs sequentially in this one task instead:
        // write the request and close the write half so `dispatch_loop`'s *second* read hits
        // EOF and returns after processing exactly one frame; its response is already sitting
        // in the duplex buffer to be read back afterward.
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        write_json_frame(&mut wire, &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 1 })).await.unwrap();
        wire.shutdown().await.unwrap();

        client.dispatch_loop(&mut server_read, &mut server_write).await;

        let frame: RendererFrame = read_json_frame(&mut wire).await.unwrap();
        assert_eq!(frame, RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence: 1 }));
    }
}

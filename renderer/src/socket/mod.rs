//! Renderer-side Unix control-socket client and `SupervisorFrame` handling. Connects to
//! `shared::control_socket_path`; the Supervisor listens (`supervisor/src/socket/mod.rs`) and
//! sends `shared::ConnectionHandshake` first. Two threads/channels (ADR-0039): [`pump`] does framed
//! I/O, while the Wayland thread owns Lua and the GL-context paint pass because `mlua::Lua` is
//! `!Send`. `StateSnapshot` hydrates a capability signal and dirties the scene (ADR-0044 decision
//! 2), then runs its `on_change` handlers (ADR-0115); only `Reevaluate` runs Lua, and the Wayland
//! loop applies it (ADR-0216). No reconnect after
//! disconnect (ADR-0059 decision 1): the Supervisor owns capabilities, `process.run` children, and
//! PAM.
//!
//! This file is the socket thread's transport; the Wayland thread's [`RendererClient`] is `client`'s.

mod client;

pub use client::{FrameOutcome, RendererClient};

use std::path::Path;

use shared::framing::{self, write_json_frame};
use shared::{ConnectionHandshake, RendererFrame, SupervisorFrame, Zeroize, error, warn};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// `SupervisorFrame`s held for the Wayland thread before the socket reader is parked.
///
/// The queue was unbounded, so Supervisor's own bounded outbound simply moved the growth here: a
/// Renderer slow to drain (a long paint, a blocking decode) accumulated frames in its own heap
/// instead. Backpressure rather than a drop policy, for the same reason it is backpressure on the
/// Supervisor side: these carry lock and reload traffic, and a dropped one is a protocol failure
/// rather than a lost log line. Sized to absorb a full snapshot replay at reconnection without
/// parking, which is the largest legitimate burst.
pub const INBOUND_CAPACITY: usize = 1024;

/// This Renderer's generation id (`OBELISK_GENERATION_ID`, default `0`), stamped into the handshake
/// and every outbound `CommandEnvelope`/`SecureSubmit`.
pub fn generation_id_from_env() -> u32 {
    std::env::var(shared::GENERATION_ID_ENV).ok().and_then(|value| value.parse().ok()).unwrap_or(0)
}

/// Connects to `path`, sends the `generation_id` handshake, and returns the live stream.
async fn connect_and_handshake(
    path: &Path,
    generation_id: u32,
) -> Result<UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = UnixStream::connect(path).await?;
    write_json_frame(&mut stream, &ConnectionHandshake { generation_id }).await?;
    Ok(stream)
}

/// Spawns the connect-and-hold-open thread. Failure logs and drops `inbound_tx`, which the Wayland
/// thread reads as `Disconnected` (ADR-0059 decision 1). `supervisor/src/main.rs` binds first, so
/// there is no startup race.
pub fn spawn_client(
    generation_id: u32,
    inbound_tx: tokio::sync::mpsc::Sender<SupervisorFrame>,
    outbound_rx: mpsc::UnboundedReceiver<RendererFrame>,
    waker: crate::wake::Waker,
) {
    std::thread::spawn(move || {
        // Hold for the thread's life: connection close, runtime failure, or panic wakes Wayland to
        // find `inbound_rx` disconnected, rather than blocking on an unsatisfiable poll (ADR-0124).
        let _wake_on_exit = crate::wake::WakeOnDrop(waker.clone());
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                error!("failed to start runtime: {err}");
                return;
            }
        };
        runtime.block_on(run(generation_id, inbound_tx, outbound_rx, waker));
    });
}

async fn run(
    generation_id: u32,
    inbound_tx: tokio::sync::mpsc::Sender<SupervisorFrame>,
    mut outbound_rx: mpsc::UnboundedReceiver<RendererFrame>,
    waker: crate::wake::Waker,
) {
    let path = match shared::instance_dir().map(|dir| shared::control_socket_path(&dir)) {
        Ok(path) => path,
        Err(err) => {
            error!("{err}");
            return;
        }
    };

    let stream = match connect_and_handshake(&path, generation_id).await {
        Ok(stream) => stream,
        Err(err) => {
            error!("failed to connect to {}: {err}", path.display());
            return;
        }
    };

    let (mut read_half, mut write_half) = stream.into_split();
    pump(&mut read_half, &mut write_half, &inbound_tx, &mut outbound_rx, Some(&waker)).await;
}

/// After handshake, forward decoded `SupervisorFrame`s to Wayland and queued `RendererFrame`s to
/// the wire (ADR-0039). Decode failure is transport failure: one sender and fixed shapes mean
/// desync. Read and write are separate
/// long-lived futures. `read_json_frame` has two sequential `read_exact`s; racing one frame read
/// against `outbound_rx.recv()` would drop partial bytes when outbound wins and desync the stream.
async fn pump<R, W>(
    read_half: &mut R,
    write_half: &mut W,
    inbound_tx: &tokio::sync::mpsc::Sender<SupervisorFrame>,
    outbound_rx: &mut mpsc::UnboundedReceiver<RendererFrame>,
    waker: Option<&crate::wake::Waker>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reader = async {
        loop {
            match framing::read_json_frame::<_, SupervisorFrame>(read_half).await {
                Ok(frame) => {
                    // Awaited, so a Supervisor pushing faster than the Wayland thread drains parks
                    // this reader rather than growing the queue. `Sender::send` is cancel-safe --
                    // if the `select!` below drops this future, the frame was not delivered and
                    // nothing half-arrives -- which is why the backpressure is safe to take here.
                    if let Err(err) = inbound_tx.send(frame).await {
                        warn!("the Wayland thread is gone; stopping the socket loop: {err}");
                        break;
                    }
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }
                Err(err) => {
                    error!("connection ended: {err}");
                    break;
                }
            }
        }
    };
    let writer = async {
        while let Some(mut frame) = outbound_rx.recv().await {
            if let Err(err) = write_json_frame(write_half, &frame).await {
                warn!("failed to send a {} frame: {err}", frame_label(&frame));
            }
            // Scrub immediately after write, not at `Drop` (ADR-0005/ADR-0027). The serialized
            // copy is `write_json_frame`'s to scrub and it does; this is the frame's own bytes.
            if let RendererFrame::SecureSubmit(inner) = &mut frame {
                inner.secret.zeroize();
            }
        }
    };
    tokio::pin!(reader, writer);
    tokio::select! {
        _ = &mut reader => {}
        _ = &mut writer => {}
    }
}

/// Names a frame for write-failure logs. Fixed labels avoid `{frame:?}`, whose derived `Debug`
/// would print `SecureSubmit.secret` (ADR-0005).
fn frame_label(frame: &RendererFrame) -> &'static str {
    match frame {
        RendererFrame::Command(_) => "Command",
        RendererFrame::SecureSubmit(_) => "SecureSubmit",
        RendererFrame::LockReport(_) => "LockReport",
        RendererFrame::StartCapability { .. } => "StartCapability",
        RendererFrame::CallResult(_) => "CallResult",
        // Never sent here (ADR-0112, ADR-0197), but a wildcard could hide a new unnamed variant.
        RendererFrame::SetState(_) => "SetState",
        RendererFrame::Call(_) => "Call",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::framing::read_json_frame;
    use shared::{CommandEnvelope, SecureSubmit};
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn connect_and_handshake_sends_a_handshake_the_listener_can_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
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

    /// Queues `frame`, returns what [`pump`] wrote. The read half produces nothing here, so timeout
    /// bounds the test.
    async fn pumped_to_the_wire(frame: RendererFrame) -> RendererFrame {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel(INBOUND_CAPACITY);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        outbound_tx.send(frame).unwrap();

        let pumping = pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx, None);
        let read_response = read_json_frame::<_, RendererFrame>(&mut wire);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                () = pumping => unreachable!("pump must not return on its own in this test"),
                frame = read_response => frame.unwrap(),
            }
        })
        .await
        .expect("the queued frame must reach the wire before the timeout")
    }

    #[tokio::test]
    async fn pump_writes_a_queued_process_command_frame_to_the_wire() {
        let envelope = CommandEnvelope {
            jsonrpc: "2.0".to_string(),
            method: "ExecuteCommand".to_string(),
            params: shared::CommandParams {
                generation_id: 0,
                capability: "process".to_string(),
                action: "run".to_string(),
                arguments: vec![serde_json::json!("echo"), serde_json::json!(["hi"])],
                expected_revision: 0,
            },
            id: 1,
        };

        match pumped_to_the_wire(RendererFrame::Command(envelope)).await {
            RendererFrame::Command(envelope) => {
                assert_eq!(envelope.params.capability, "process");
                assert_eq!(envelope.params.action, "run");
                assert_eq!(envelope.id, 1);
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pump_writes_a_secure_submit_frames_secret_to_the_wire_intact() {
        // ADR-0005/ADR-0027: wire carries the exact `SecureBuffer` secret; `pump` zeroizes its
        // plaintext frame copy immediately after write.
        let written = pumped_to_the_wire(RendererFrame::SecureSubmit(SecureSubmit {
            generation_id: 4,
            capability: shared::Capability::Polkit,
            action: "authenticate".to_string(),
            secret: b"hunter2".to_vec(),
        }))
        .await;

        assert_eq!(
            written,
            RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: shared::Capability::Polkit,
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            })
        );
    }

    #[tokio::test]
    async fn pump_forwards_a_decoded_supervisor_frame_to_the_wayland_thread() {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        // Close the write half so `pump`'s *second* read hits EOF after forwarding one frame.
        write_json_frame(&mut wire, &SupervisorFrame::Reevaluate).await.unwrap();
        wire.shutdown().await.unwrap();

        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(INBOUND_CAPACITY);
        let (_outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx, None).await;

        assert_eq!(inbound_rx.try_recv(), Ok(SupervisorFrame::Reevaluate));
    }

    /// Advances `pumping` for up to `millis`. If `pump` completes first, a loop broke, which is a
    /// test bug, not a normal yield.
    async fn let_pump_advance(mut pumping: std::pin::Pin<&mut impl std::future::Future<Output = ()>>, millis: u64) {
        tokio::select! {
            () = &mut pumping => unreachable!("pump must not return on its own in this test"),
            () = tokio::time::sleep(std::time::Duration::from_millis(millis)) => {}
        }
    }

    /// `shared::framing::read_frame` does two sequential `read_exact`s, so partial progress lives
    /// in its future. Old `pump` raced one `read_json_frame` against `outbound_rx.recv()` per
    /// `select!`; outbound could drop a stalled read, losing consumed bytes, and the next iteration
    /// read a length prefix from the middle of JSON. This reproduces the race with an inbound frame
    /// split across writes and outbound activity between them. Fixed `pump` gives each direction a
    /// long-lived loop, so the stalled read survives.
    #[tokio::test]
    async fn pump_survives_an_inbound_frame_split_around_an_outbound_frame() {
        let (mut wire, server) = tokio::io::duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(INBOUND_CAPACITY);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        let inbound_frame = SupervisorFrame::Reevaluate;
        let payload = serde_json::to_vec(&inbound_frame).unwrap();
        let mut wire_bytes = (payload.len() as u32).to_be_bytes().to_vec();
        wire_bytes.extend_from_slice(&payload);
        // Split past the length prefix, stalling the *second* `read_exact`.
        let split_at = wire_bytes.len() / 2;
        assert!(split_at > 4, "the split point must land inside the payload, not the length prefix");

        let pumping = pump(&mut server_read, &mut server_write, &inbound_tx, &mut outbound_rx, None);
        tokio::pin!(pumping);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            wire.write_all(&wire_bytes[..split_at]).await.unwrap();
            // Consume the partial payload and block mid-`read_exact`.
            let_pump_advance(pumping.as_mut(), 20).await;

            // Queue outbound while inbound is stalled mid-frame, the old `select!` race.
            outbound_tx.send(RendererFrame::StartCapability { capability: shared::Capability::Lock }).unwrap();
            let_pump_advance(pumping.as_mut(), 20).await;

            // A stuck read must not starve writes.
            let written = read_json_frame::<_, RendererFrame>(&mut wire).await.unwrap();
            assert_eq!(written, RendererFrame::StartCapability { capability: shared::Capability::Lock });

            // Complete inbound. If outbound cancelled the read, this prefix would land mid-payload.
            wire.write_all(&wire_bytes[split_at..]).await.unwrap();
            let_pump_advance(pumping.as_mut(), 20).await;
        })
        .await
        .expect("pump must keep servicing both directions within the timeout");

        assert_eq!(
            inbound_rx.try_recv(),
            Ok(SupervisorFrame::Reevaluate),
            "the inbound frame split across two writes around an outbound frame must still decode correctly"
        );
    }
}

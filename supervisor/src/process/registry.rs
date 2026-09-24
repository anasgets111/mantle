//! `process.run` bookkeeping (ADR-0026): child registration, stdout/stderr `ProcessOutput` frames,
//! kill/exit reporting, and per-generation/shutdown reap sweeps.

use std::collections::HashMap;
use std::io;

use shared::{ProcessExited, ProcessOutputLine, ProcessStream, SupervisorFrame, debug, warn};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout};

use crate::send_frame_logged;
use crate::socket;

/// Tracked `process.run` children, keyed by spawning generation and Renderer-assigned
/// `CommandEnvelope.id` (ADR-0026). Mutated only inside `main()`'s `select!`, not behind a mutex.
pub(crate) type LiveProcesses = HashMap<(u32, u64), Child>;

/// Parses `process.run` arguments as `[cmd, args]`. `None` means protocol shape mismatch, not
/// spawn failure; the caller logs it.
pub(crate) fn process_run_args(arguments: &[serde_json::Value]) -> Option<(String, Vec<String>)> {
    let cmd = arguments.first()?.as_str()?.to_string();
    let args =
        arguments.get(1)?.as_array()?.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
    Some((cmd, args))
}

/// Dispatches `process.run`/`process.kill` (ADR-0037), including parse and spawn. `async` because
/// kill must reap before sending `ProcessExited`.
pub(crate) async fn dispatch(
    processes: &mut LiveProcesses,
    registry: &socket::GenerationRegistry,
    process_done_tx: &tokio::sync::mpsc::UnboundedSender<(u32, u64)>,
    envelope: &shared::CommandEnvelope,
) {
    let generation_id = envelope.params.generation_id;
    let id = envelope.id;
    let exited =
        |code| send_frame_logged(registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code }));
    match envelope.params.action.as_str() {
        "run" => match process_run_args(&envelope.params.arguments) {
            Some((cmd, args)) => match spawn_and_register_process(processes, generation_id, id, &cmd, &args) {
                Some((stdout, stderr)) => {
                    debug!(2; "process.run: spawned generation={} id={} cmd={:?}", generation_id, id, cmd);
                    let task_registry = registry.clone();
                    let task_done_tx = process_done_tx.clone();
                    tokio::spawn(async move {
                        stream_process_output(&task_registry, generation_id, id, stdout, stderr).await;
                        let _ = task_done_tx.send((generation_id, id));
                    });
                }
                None => exited(None),
            },
            None => {
                debug!(
                    "malformed process.run command from generation {generation_id}: {:?}",
                    envelope.params.arguments
                );
                // Lua's ProcessHandle already awaits `id`'s exit_cb; this prevents a callback pair
                // leak when no process spawned.
                exited(None);
            }
        },
        // No registry entry, no handle, no callbacks: a detached program is not this shell's to
        // reap, and a Renderer replacement must leave it alone (ADR-0188). That is the whole difference
        // from `run`, and it is why this sends no `ProcessExited` -- there is no `exit_cb` waiting.
        "detach" => match process_run_args(&envelope.params.arguments) {
            Some((cmd, args)) => {
                if let Err(err) = crate::process::spawn_detached(&cmd, &args) {
                    warn!("process.detach: spawning {cmd:?} failed: {err}");
                }
            }
            None => debug!(
                "malformed process.detach command from generation {generation_id}: {:?}",
                envelope.params.arguments
            ),
        },
        "kill" => {
            if let Some(code) = kill_registered_process(processes, generation_id, id).await {
                exited(code);
            }
        }
        _ => debug!("unknown action {:?} from generation {generation_id}", envelope.params.action),
    }
}

/// Spawns `("process", "run")` with piped stdout/stderr, removes handles before registering the
/// `Child`, leaving `processes` owning it while a task reads output. Spawn failure is `None`.
pub(crate) fn spawn_and_register_process(
    processes: &mut LiveProcesses,
    generation_id: u32,
    id: u64,
    cmd: &str,
    args: &[String],
) -> Option<(ChildStdout, ChildStderr)> {
    match super::spawn_group_leader_piped(cmd, args) {
        Ok(mut child) => {
            let stdout = child.stdout.take().expect("spawn_group_leader_piped always pipes stdout");
            let stderr = child.stderr.take().expect("spawn_group_leader_piped always pipes stderr");
            processes.insert((generation_id, id), child);
            Some((stdout, stderr))
        }
        Err(err) => {
            warn!("process.run({cmd:?}, {args:?}) failed to spawn: {err}");
            None
        }
    }
}

/// The most bytes one child output line contributes to a `ProcessOutput` frame.
///
/// `tokio::io::Lines` grows one `String` until it meets a newline, and a child that never writes
/// one -- `yes | tr -d '\n'` is the whole recipe -- grows it without limit. That growth happens
/// inside Supervisor, before framing's `MAX_FRAME_LEN` ever sees a frame to reject, so the frame
/// limit does not bound it. Past this the line is delivered cut, its tail is dropped, and the next
/// line starts clean: a diagnostic stream is worth truncating, never worth an OOM.
const MAX_LINE_BYTES: u64 = 64 * 1024;

/// `tokio::io::Lines` with a ceiling.
///
/// Each read is bounded by a fresh `take`, so `read_until` stops at the newline or the remaining
/// room, whichever comes first. `next_line` keeps `Lines`'s contract: `Ok(None)` only at EOF, and a
/// final unterminated line is still delivered.
///
/// **The partial line lives on `self`, not in `next_line`'s frame, and that is load-bearing.**
/// `read_until` is not cancellation safe -- it can consume bytes from the reader and then be
/// dropped when the other arm of the `select!` below wins. Those bytes are already gone from the
/// stream, so a buffer local to the call would lose them and silently corrupt the line. Holding it
/// here means a cancelled call resumes into the same buffer on the next one, which is what makes
/// this usable in a `select!` at all.
///
/// Unlike `Lines` this decodes lossily rather than failing the stream on invalid UTF-8, so one stray
/// byte does not end a child's output reporting.
struct BoundedLines<R> {
    reader: tokio::io::BufReader<R>,
    /// The line being accumulated, never longer than [`MAX_LINE_BYTES`]. See the type docs: this
    /// is here so a cancelled `next_line` loses nothing.
    line: Vec<u8>,
    /// Names the stream in the one truncation warning it may log.
    label: String,
    /// Whether that warning has been logged, so a runaway child says it once, not once per line.
    warned: bool,
}

impl<R: tokio::io::AsyncRead + Unpin> BoundedLines<R> {
    fn new(inner: R, label: String) -> Self {
        Self { reader: tokio::io::BufReader::new(inner), line: Vec::new(), label, warned: false }
    }

    /// The next line, truncated to [`MAX_LINE_BYTES`], or `Ok(None)` at EOF.
    async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            // A full `line` is the whole discard state. `discard_to_newline` reads into its own
            // buffer, so a cancelled discard leaves `line` at the cap and the next call resumes
            // here; and once that await resolves there is no yield point before `take_line`
            // empties it. One branch, so no second discard can run after the resumed one and eat
            // the *next* line.
            if self.line.len() as u64 == MAX_LINE_BYTES {
                self.discard_to_newline().await?;
                return Ok(Some(self.take_line()));
            }
            let room = MAX_LINE_BYTES - self.line.len() as u64;
            let read = (&mut self.reader).take(room).read_until(b'\n', &mut self.line).await?;
            if read == 0 {
                // EOF. A child that exits without a trailing newline still wrote a line.
                return Ok((!self.line.is_empty()).then(|| self.take_line()));
            }
            if self.line.last() == Some(&b'\n') {
                self.line.pop();
                return Ok(Some(self.take_line()));
            }
            // Read stopped at the `take` limit rather than a newline, so go round: either there is
            // still room, or the branch above truncates.
        }
    }

    /// Takes the accumulated bytes, leaving the buffer ready for the next line.
    fn take_line(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned()
    }

    /// Reads and drops the rest of an over-long line, so the next one starts clean.
    async fn discard_to_newline(&mut self) -> io::Result<()> {
        if !self.warned {
            self.warned = true;
            warn!(
                "{}: a line exceeded {MAX_LINE_BYTES} bytes and was truncated; further truncations \
                 on this stream are not reported",
                self.label
            );
        }
        let mut dropped = Vec::new();
        loop {
            dropped.clear();
            let read = (&mut self.reader).take(MAX_LINE_BYTES).read_until(b'\n', &mut dropped).await?;
            if read == 0 || dropped.last() == Some(&b'\n') {
                return Ok(());
            }
        }
    }
}

/// Reads stdout/stderr concurrently by line, forwarding `SupervisorFrame::ProcessOutput` through
/// `registry`. After both EOFs, reports `(generation_id, id)` on `process_done_tx`; it does not own
/// the `Child`, so it cannot `wait()` for the exit code.
pub(crate) async fn stream_process_output(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    id: u64,
    stdout: ChildStdout,
    stderr: ChildStderr,
) {
    let mut stdout_lines = BoundedLines::new(stdout, format!("process {id} (generation {generation_id}) stdout"));
    let mut stderr_lines = BoundedLines::new(stderr, format!("process {id} (generation {generation_id}) stderr"));
    let mut stdout_done = false;
    let mut stderr_done = false;

    while !stdout_done || !stderr_done {
        tokio::select! {
            line = stdout_lines.next_line(), if !stdout_done => {
                stdout_done = report_process_output_line(registry, generation_id, id, ProcessStream::Stdout, line);
            }
            line = stderr_lines.next_line(), if !stderr_done => {
                stderr_done = report_process_output_line(registry, generation_id, id, ProcessStream::Stderr, line);
            }
        }
    }
}

/// Handles one stream poll: sends a frame for a line, logs read errors, and reports EOF/error done.
fn report_process_output_line(
    registry: &socket::GenerationRegistry,
    generation_id: u32,
    id: u64,
    stream: ProcessStream,
    line: io::Result<Option<String>>,
) -> bool {
    match line {
        Ok(Some(line)) => {
            send_frame_logged(
                registry,
                generation_id,
                &SupervisorFrame::ProcessOutput(ProcessOutputLine { id, stream, line }),
            );
            false
        }
        Ok(None) => true,
        Err(err) => {
            warn!("process {id} (generation {generation_id}): failed to read {stream:?}: {err}");
            true
        }
    }
}

/// Removes `(generation_id, id)` and reaps its group via `super::reap_process_group` (ADR-0018).
/// `None` means no entry: already reaped via completion, or Lua never received a handle. Otherwise
/// the code for `exit_cb`: usually `None` for a SIGTERM/SIGKILL death, and `None` when the reap
/// failed (logged here) so `id`'s `exit_cb` is still answered.
pub(crate) async fn kill_registered_process(
    processes: &mut LiveProcesses,
    generation_id: u32,
    id: u64,
) -> Option<Option<i32>> {
    let mut child = processes.remove(&(generation_id, id))?;
    match super::reap_process_group(&mut child, super::DEFAULT_REAP_GRACE).await {
        Ok(status) => Some(status.code()),
        Err(err) => {
            warn!("failed to reap process {id} (generation {generation_id}) on kill: {err}");
            Some(None)
        }
    }
}

/// Slow `process_done` half: waits for the real exit and reports it to Lua. The caller has already
/// taken the `Child` out of `LiveProcesses`; this owns it from here. Detach it, never await
/// inline in `main()`'s `select!`: streams can close while a daemonizing child keeps running after
/// redirecting them to `/dev/null`.
pub(crate) async fn wait_and_report_exit(
    registry: socket::GenerationRegistry,
    generation_id: u32,
    id: u64,
    mut child: Child,
) {
    let code = match child.wait().await {
        Ok(status) => status.code(),
        Err(err) => {
            warn!("failed to wait on exited process {id} (generation {generation_id}): {err}");
            // `None` for the same reason as a failed kill reap: `id`'s `exit_cb` is waiting
            // and no other path will answer it.
            None
        }
    };
    send_frame_logged(&registry, generation_id, &SupervisorFrame::ProcessExited(ProcessExited { id, code }));
}

/// Reaps `generation_id`'s processes, or every tracked one at shutdown when `None`, concurrently so
/// the sweep costs one grace rather than one per child. Sends no `ProcessExited`: the generation's
/// connection is already gone. Includes every Lua-spawned child, not only the Renderer.
pub(crate) async fn reap_processes(processes: &mut LiveProcesses, generation_id: Option<u32>) {
    let keys: Vec<(u32, u64)> =
        processes.keys().filter(|(entry, _)| generation_id.is_none_or(|g| g == *entry)).copied().collect();
    let children = keys.into_iter().filter_map(|key| processes.remove(&key).map(|child| (key, child)));
    futures_util::future::join_all(children.map(|(key, mut child)| async move {
        if let Err(err) = super::reap_process_group(&mut child, super::DEFAULT_REAP_GRACE).await {
            warn!("failed to reap process {key:?}: {err}");
        }
    }))
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drains a `BoundedLines` over an in-memory reader, so the cap is exercised without a child.
    async fn lines_of(input: &[u8]) -> Vec<String> {
        let mut reader = BoundedLines::new(input, "test".to_string());
        let mut out = Vec::new();
        while let Some(line) = reader.next_line().await.expect("reading a slice cannot fail") {
            out.push(line);
        }
        out
    }

    #[tokio::test]
    async fn ordinary_lines_come_back_split_and_without_their_newlines() {
        assert_eq!(lines_of(b"one\ntwo\nthree\n").await, vec!["one", "two", "three"]);
    }

    #[tokio::test]
    async fn a_final_line_without_a_newline_is_still_delivered() {
        assert_eq!(lines_of(b"first\nno trailing newline").await, vec!["first", "no trailing newline"]);
    }

    #[tokio::test]
    async fn a_child_that_never_writes_a_newline_is_cut_at_the_cap_not_buffered_forever() {
        // The failure this bounds: `Lines` would hold all of it in one `String`, inside Supervisor,
        // where framing's frame limit never sees it.
        let flood = vec![b'x'; MAX_LINE_BYTES as usize * 3];
        let lines = lines_of(&flood).await;
        assert_eq!(lines.len(), 1, "one unterminated line, however long, is one line");
        assert_eq!(lines[0].len(), MAX_LINE_BYTES as usize);
    }

    #[tokio::test]
    async fn the_line_after_an_over_long_one_starts_clean() {
        let mut input = vec![b'x'; MAX_LINE_BYTES as usize + 500];
        input.extend_from_slice(b"\nshort\n");
        let lines = lines_of(&input).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_LINE_BYTES as usize, "the head is kept and the tail dropped");
        assert_eq!(lines[1], "short", "the dropped tail must not bleed into the next line");
    }

    #[tokio::test]
    async fn invalid_utf8_is_replaced_rather_than_ending_the_stream() {
        // `tokio::io::Lines` returns `InvalidData` here, which stopped a child reporting anything
        // further over one stray byte.
        let lines = lines_of(b"ok\n\xff\nafter\n").await;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "ok");
        assert_eq!(lines[2], "after");
    }

    #[tokio::test]
    async fn a_cancelled_read_keeps_the_bytes_it_had_already_taken() {
        // `read_until` is not cancellation safe: it can consume from the reader and then be
        // dropped when the other arm of `stream_process_output`'s `select!` wins. The partial line
        // lives on `self` so the next call resumes into it; a buffer local to the call would drop
        // these bytes and splice the line back together wrong.
        let (mut client, server) = tokio::io::duplex(64);
        let mut reader = BoundedLines::new(server, "test".to_string());

        tokio::io::AsyncWriteExt::write_all(&mut client, b"partial").await.unwrap();
        // No newline yet, so this cannot complete; dropping the future is the cancellation.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), reader.next_line()).await.is_err(),
            "the line is unterminated, so the read must still be pending when it is cancelled"
        );

        tokio::io::AsyncWriteExt::write_all(&mut client, b" rest\n").await.unwrap();
        let line = reader.next_line().await.unwrap();
        assert_eq!(line, Some("partial rest".to_string()), "the cancelled read must not have eaten `partial`");
    }

    #[tokio::test]
    async fn a_cancelled_discard_resumes_and_does_not_eat_the_next_line() {
        // The bug this pins: resuming a cancelled discard and then discarding a *second* time,
        // which throws away the line after the over-long one.
        // Room for the whole line up front: a writer blocked on a cancelled reader never finishes.
        let (mut client, server) = tokio::io::duplex(2 * MAX_LINE_BYTES as usize);
        let mut reader = BoundedLines::new(server, "test".to_string());
        tokio::io::AsyncWriteExt::write_all(&mut client, &[b'x'; MAX_LINE_BYTES as usize + 64]).await.unwrap();

        // Cancelled while discarding the tail of the over-long line.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), reader.next_line()).await.is_err(),
            "the discard cannot finish yet, so this call must be cancelled mid-discard"
        );

        tokio::io::AsyncWriteExt::write_all(&mut client, b"\nkeep me\n").await.unwrap();
        assert_eq!(
            reader.next_line().await.unwrap().map(|line| line.len()),
            Some(MAX_LINE_BYTES as usize),
            "the resumed call owes the truncated head"
        );
        assert_eq!(
            reader.next_line().await.unwrap(),
            Some("keep me".to_string()),
            "the line after the over-long one must survive"
        );
    }

    #[tokio::test]
    async fn empty_output_is_no_lines_at_all() {
        assert!(lines_of(b"").await.is_empty());
    }

    #[test]
    fn process_run_args_parses_cmd_and_args_from_arguments() {
        let arguments = vec![serde_json::json!("echo"), serde_json::json!(["hello", "world"])];
        assert_eq!(
            process_run_args(&arguments),
            Some(("echo".to_string(), vec!["hello".to_string(), "world".to_string()]))
        );
    }

    #[test]
    fn process_run_args_rejects_a_malformed_shape() {
        assert_eq!(process_run_args(&[]), None, "missing both elements");
        assert_eq!(process_run_args(&[serde_json::json!(1), serde_json::json!([])]), None, "cmd is not a string");
        assert_eq!(
            process_run_args(&[serde_json::json!("echo"), serde_json::json!("not-an-array")]),
            None,
            "args is not an array"
        );
        assert_eq!(
            process_run_args(&[serde_json::json!("echo"), serde_json::json!([1, 2])]),
            None,
            "args contains non-strings"
        );
    }

    fn registry_with_connection(
        generation_id: u32,
    ) -> (socket::GenerationRegistry, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let registry = socket::GenerationRegistry::default();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        registry.register(generation_id, tx, std::sync::Arc::new(tokio::sync::Notify::new()));
        (registry, rx)
    }

    fn sh_args(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    #[tokio::test]
    async fn spawn_and_register_process_registers_a_successful_spawn_and_returns_piped_handles() {
        let mut processes: LiveProcesses = HashMap::new();
        let handles = spawn_and_register_process(&mut processes, 1, 7, "true", &[]);
        assert!(handles.is_some());
        assert!(processes.contains_key(&(1, 7)));
    }

    #[tokio::test]
    async fn spawn_and_register_process_registers_nothing_on_a_spawn_failure() {
        let mut processes: LiveProcesses = HashMap::new();
        let handles = spawn_and_register_process(&mut processes, 1, 7, "/no/such/binary", &[]);
        assert!(handles.is_none());
        assert!(!processes.contains_key(&(1, 7)));
    }

    #[tokio::test]
    async fn stream_process_output_forwards_both_streams_then_the_real_exit_code_arrives_via_wait() {
        let mut processes: LiveProcesses = HashMap::new();
        let (stdout, stderr) =
            spawn_and_register_process(&mut processes, 1, 9, "sh", &sh_args("echo out1; echo err1 >&2; exit 3"))
                .unwrap();
        let (registry, mut rx) = registry_with_connection(1);

        stream_process_output(&registry, 1, 9, stdout, stderr).await;

        let mut lines = Vec::new();
        while let Ok(payload) = rx.try_recv() {
            match serde_json::from_slice::<SupervisorFrame>(&payload).unwrap() {
                SupervisorFrame::ProcessOutput(line) => lines.push(line),
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().any(|l| l.id == 9 && l.stream == ProcessStream::Stdout && l.line == "out1"));
        assert!(lines.iter().any(|l| l.id == 9 && l.stream == ProcessStream::Stderr && l.line == "err1"));

        // stream_process_output never learns the exit code itself (it doesn't own the Child) --
        // that's the caller's `remove` + wait_and_report_exit's job, via a detached task.
        let mut child = processes.remove(&(1, 9)).expect("process must still be registered");
        assert_eq!(child.wait().await.unwrap().code(), Some(3));
    }

    #[tokio::test]
    async fn taking_an_unregistered_id_returns_none() {
        let mut processes: LiveProcesses = HashMap::new();
        assert!(processes.remove(&(1, 1)).is_none());
    }

    #[tokio::test]
    async fn wait_and_report_exit_sends_the_real_exit_code_back_to_the_generation_that_spawned_it() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 9, "sh", &sh_args("exit 7"));
        let child = processes.remove(&(1, 9)).unwrap();
        let (registry, mut rx) = registry_with_connection(1);

        wait_and_report_exit(registry, 1, 9, child).await;

        let payload = rx.try_recv().expect("a ProcessExited frame must have been sent");
        match serde_json::from_slice::<SupervisorFrame>(&payload).unwrap() {
            SupervisorFrame::ProcessExited(ProcessExited { id, code }) => {
                assert_eq!(id, 9);
                assert_eq!(code, Some(7));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_registered_process_reaps_and_reports_a_signal_death() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 3, "sh", &sh_args("sleep 5"));
        assert!(processes.contains_key(&(1, 3)));

        assert_eq!(
            kill_registered_process(&mut processes, 1, 3).await,
            Some(None),
            "a SIGTERM/SIGKILL death has no exit code"
        );
        assert!(!processes.contains_key(&(1, 3)));
    }

    #[tokio::test]
    async fn kill_registered_process_on_an_unregistered_id_is_a_silent_no_op() {
        let mut processes: LiveProcesses = HashMap::new();
        assert_eq!(kill_registered_process(&mut processes, 1, 99).await, None);
    }

    #[tokio::test]
    async fn reaping_one_generation_reaps_only_the_matching_generations_entries() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 1, "sh", &sh_args("sleep 30"));
        spawn_and_register_process(&mut processes, 2, 1, "sh", &sh_args("sleep 30"));
        let pid = processes[&(1, 1)].id().expect("freshly spawned child has a pid");

        reap_processes(&mut processes, Some(1)).await;

        assert!(!processes.contains_key(&(1, 1)), "generation 1's process must be reaped and removed");
        assert!(crate::process::exited(&[pid]).await, "process {pid} must be dead, not just removed from the registry");
        assert!(processes.contains_key(&(2, 1)), "generation 2's process must be untouched");

        kill_registered_process(&mut processes, 2, 1).await;
    }

    #[tokio::test]
    async fn reaping_without_a_generation_reaps_every_generations_entries() {
        let mut processes: LiveProcesses = HashMap::new();
        spawn_and_register_process(&mut processes, 1, 1, "sh", &sh_args("sleep 30"));
        spawn_and_register_process(&mut processes, 2, 1, "sh", &sh_args("sleep 30"));
        let pids: Vec<u32> =
            processes.values().map(|child| child.id().expect("freshly spawned child has a pid")).collect();

        reap_processes(&mut processes, None).await;

        assert!(processes.is_empty(), "shutdown must reap every tracked process, not just one generation's");
        assert!(crate::process::exited(&pids).await, "every process must be dead, not just removed from the registry");
    }
}

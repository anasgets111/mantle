//! Supervisor-side config-file watcher (build-steps.md Phase 13; `CONTEXT.md`, Watcher).
//!
//! Watches `~/.config/oblisk/` (the *directory*, not `shell.lua` itself) for changes to
//! `shell.lua` specifically, debounced: a burst of events (a typical editor save fires several
//! -- `CREATE`+`MODIFY`+`CLOSE_WRITE` in one write, or `MOVED_TO` for an atomic-save editor
//! that writes a temp file and renames it into place) coalesces into exactly one trigger, sent
//! only after the debounce window has elapsed since the *last* relevant event. Watching the
//! directory rather than the file's own inode is deliberate: an atomic-save editor unlinks and
//! recreates the file rather than writing in place, which would silently break a watch bound to
//! the old inode.
//!
//! `inotify`'s default features (including `stream`, which pulls in `futures-util`) have been
//! declared, unused, on `supervisor` since scaffolding -- this is their first real caller.

use std::io;
use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::sync::mpsc;

/// Watches `dir` for changes to `shell.lua`, debounced by `debounce`. Returns a channel that
/// receives one `()` per settled burst of relevant changes -- see the module doc comment.
pub fn spawn_watcher(dir: &Path, debounce: Duration) -> io::Result<mpsc::UnboundedReceiver<()>> {
    let inotify = Inotify::init()?;
    inotify.watches().add(dir, WatchMask::CREATE | WatchMask::MODIFY | WatchMask::MOVED_TO | WatchMask::CLOSE_WRITE)?;
    let mut stream = inotify.into_event_stream(vec![0u8; 4096])?;

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // An absolute deadline, not a relative `sleep(debounce)` re-armed fresh on every loop
        // iteration: only a *relevant* (shell.lua) event is allowed to push this forward. An
        // irrelevant event in the same directory still makes this `select!` loop back (to read
        // the next inotify event), and a relative sleep reconstructed at that point would have
        // silently restarted the debounce window from "now" -- delaying the trigger for as long
        // as unrelated directory activity kept arriving.
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(Ok(event)) if event.name.as_deref() == Some(std::ffi::OsStr::new("shell.lua")) => {
                            deadline = Some(tokio::time::Instant::now() + debounce);
                        }
                        Some(Ok(_)) => {} // some other entry in the directory -- not our concern.
                        Some(Err(err)) => {
                            eprintln!("config watcher: inotify read failed: {err}");
                        }
                        None => break, // the inotify fd closed -- nothing left to watch.
                    }
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    deadline = None;
                    if tx.send(()).is_err() {
                        break; // receiver dropped -- nobody's listening any more.
                    }
                }
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn recv_within(rx: &mut mpsc::UnboundedReceiver<()>, timeout: Duration) -> Option<()> {
        tokio::time::timeout(timeout, rx.recv()).await.unwrap_or(None)
    }

    const SHORT_DEBOUNCE: Duration = Duration::from_millis(40);
    const WAIT: Duration = Duration::from_millis(500);

    #[tokio::test]
    async fn a_single_write_to_shell_lua_fires_exactly_one_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "a write to shell.lua must fire a trigger");
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "must not fire a second trigger for the same settled write");
    }

    #[tokio::test]
    async fn a_burst_of_rapid_writes_coalesces_into_one_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();
        let path = dir.path().join("shell.lua");

        std::fs::write(&path, "return {}").unwrap();
        tokio::time::sleep(SHORT_DEBOUNCE / 4).await;
        std::fs::write(&path, "return { id = 2 }").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_some(), "the burst must fire a trigger");
        assert!(recv_within(&mut rx, SHORT_DEBOUNCE * 3).await.is_none(), "a rapid burst must coalesce into exactly one trigger, not two");
    }

    #[tokio::test]
    async fn writing_an_unrelated_file_fires_no_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), SHORT_DEBOUNCE).unwrap();

        std::fs::write(dir.path().join("notes.txt"), "not shell.lua").unwrap();

        assert!(recv_within(&mut rx, WAIT).await.is_none(), "an unrelated file must not trigger a reload");
    }

    #[tokio::test]
    async fn an_unrelated_event_during_the_debounce_window_does_not_push_back_the_deadline() {
        // Regression test for a CONFIRMED correctness finding: the original implementation
        // reconstructed `sleep(debounce)` fresh every time the loop iterated for *any* inotify
        // event, including an irrelevant one -- so an unrelated write arriving while a real
        // shell.lua debounce was still pending silently restarted the window instead of leaving
        // the original deadline alone.
        // A wider debounce than SHORT_DEBOUNCE, so the margin between "correct" and "buggy" fire
        // times below is comfortable against scheduling jitter.
        let debounce = Duration::from_millis(80);
        let dir = tempfile::tempdir().unwrap();
        let mut rx = spawn_watcher(dir.path(), debounce).unwrap();

        std::fs::write(dir.path().join("shell.lua"), "return {}").unwrap();
        tokio::time::sleep(debounce / 2).await;
        std::fs::write(dir.path().join("notes.txt"), "unrelated").unwrap();

        // The real deadline is `debounce`/2 away from here. Correct behavior fires here at
        // `debounce`/2; a buggy from-here restart would instead fire a full `debounce` later --
        // this window sits with 20ms margin inside the former and short of the latter.
        assert!(
            recv_within(&mut rx, debounce / 2 + Duration::from_millis(20)).await.is_some(),
            "an unrelated event during the debounce window must not delay the trigger"
        );
    }
}

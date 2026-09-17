//! Watches the marker file behind [`UpdatesState::reboot_required`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::sync::mpsc::UnboundedSender;

use super::controller::{UpdatesSignal, UpdatesState};

/// Marker file for [`UpdatesState::reboot_required`], written by a pacman hook.
pub(super) const REBOOT_MARKER: &str = "/run/obelisk-reboot-required";

/// Mirrors `marker`'s existence into [`UpdatesState::reboot_required`] until the controller drops.
/// `marker` is a parameter so a test can point it at a tempdir.
///
/// Watches the parent directory, because an inotify watch needs an inode and the marker usually
/// does not exist yet. Re-stats rather than reading the mask, so a create and a delete arriving in
/// one read still leave the right answer.
pub(super) async fn run_reboot_marker_task(
    marker: PathBuf,
    state: Arc<Mutex<UpdatesState>>,
    events: UnboundedSender<UpdatesSignal>,
) {
    let (Some(dir), Some(name)) = (marker.parent(), marker.file_name()) else {
        eprintln!("updates: {} is not a file path; the reboot badge stays off", marker.display());
        return;
    };
    publish_reboot_required(&marker, &state, &events);

    let mask = WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::DELETE | WatchMask::MOVED_FROM;
    let mut stream = match Inotify::init().and_then(|inotify| {
        inotify.watches().add(dir, mask)?;
        inotify.into_event_stream(vec![0u8; 4096])
    }) {
        Ok(stream) => stream,
        Err(err) => {
            // The first stat stands, so a badge raised before login survives; later changes do not.
            eprintln!("updates: cannot watch {} for the reboot marker: {err}", dir.display());
            return;
        }
    };
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) if event.name.as_deref() == Some(name) => publish_reboot_required(&marker, &state, &events),
            Ok(_) => {}
            Err(err) => eprintln!("updates: inotify read on {} failed: {err}", dir.display()),
        }
    }
}

/// Pushes only on a change: `/run` is busy and every push re-resolves every surface (ADR-0044
/// decision 2).
fn publish_reboot_required(marker: &Path, state: &Arc<Mutex<UpdatesState>>, events: &UnboundedSender<UpdatesSignal>) {
    let required = marker.exists();
    let mut guard = state.lock().unwrap();
    if guard.reboot_required == required {
        return;
    }
    guard.reboot_required = required;
    drop(guard);
    let _ = events.send(UpdatesSignal::Changed);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Polls until the watcher has published `expected`; the inotify round trip has no completion
    /// to await.
    async fn wait_for_reboot_required(state: &Arc<Mutex<UpdatesState>>, expected: bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().reboot_required != expected {
            assert!(std::time::Instant::now() < deadline, "reboot_required never became {expected}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_marker_file_written_after_startup_raises_reboot_required() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("reboot-required");
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let watcher = tokio::spawn(run_reboot_marker_task(marker.clone(), Arc::clone(&state), events_tx));

        std::fs::write(&marker, "").unwrap();
        wait_for_reboot_required(&state, true).await;
        assert_eq!(events_rx.recv().await, Some(UpdatesSignal::Changed), "the badge is Lua-visible, so it pushes");

        std::fs::remove_file(&marker).unwrap();
        wait_for_reboot_required(&state, false).await;

        watcher.abort();
    }

    #[tokio::test]
    async fn a_marker_file_that_already_exists_is_read_before_any_event() {
        // A shell restart after the hook ran: no inotify event is coming, only the first stat.
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("reboot-required");
        std::fs::write(&marker, "").unwrap();
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();

        let watcher = tokio::spawn(run_reboot_marker_task(marker, Arc::clone(&state), events_tx));
        wait_for_reboot_required(&state, true).await;

        watcher.abort();
    }

    #[tokio::test]
    async fn a_neighbouring_file_in_the_same_directory_is_ignored() {
        // `/run` is busy; only this one name may move the badge.
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(UpdatesState::default()));
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let watcher = tokio::spawn(run_reboot_marker_task(
            directory.path().join("reboot-required"),
            Arc::clone(&state),
            events_tx,
        ));

        std::fs::write(directory.path().join("something-else"), "").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(!state.lock().unwrap().reboot_required);
        assert!(events_rx.try_recv().is_err(), "an unrelated file must not push");

        watcher.abort();
    }
}

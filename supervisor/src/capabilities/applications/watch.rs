//! Rescans `mantle.applications` when an applications directory changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use shared::warn;

use super::scan::walk;

/// Rescan 250 ms after the last event: one package install writes a burst of entries.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Returns once `events` is quiet for [`DEBOUNCE`], or [`crate::debounce::MAX_WAIT`] after the first.
async fn settle<S: futures_util::Stream + Unpin>(mut events: S) {
    let mut burst = crate::debounce::Burst::default();
    burst.bump(DEBOUNCE);
    while let Some(at) = burst.at()
        && let Ok(Some(_)) = tokio::time::timeout_at(at, events.next()).await
    {
        burst.bump(DEBOUNCE);
    }
}

/// Watches `dirs`, scans, and repeats after each settled burst of changes, until aborted.
///
/// Watching comes before scanning, so a change made during the scan triggers another. A failed
/// watch still scans once and leaves `refresh` as the only rescan.
///
/// ponytail: every burst rescans every directory and rebuilds every watch, so new subdirectories
/// and directories created after startup need no bookkeeping. Ceiling: a few hundred entries per
/// burst; the upgrade is rescanning and rewatching only the changed directory.
pub(super) async fn run(dirs: Arc<Vec<PathBuf>>, rescan: Arc<dyn Fn() + Send + Sync>) {
    loop {
        let (dirs, rescan) = (Arc::clone(&dirs), Arc::clone(&rescan));
        let Ok(watched) = tokio::task::spawn_blocking(move || {
            let watched = watch(&dirs);
            rescan();
            watched
        })
        .await
        else {
            return; // the scan panicked; its mutex is poisoned and `refresh` panics the same way.
        };
        let mut stream = match watched.and_then(|inotify| inotify.into_event_stream(vec![0u8; 4096])) {
            Ok(stream) => stream,
            Err(err) => {
                warn!("cannot watch the applications directories: {err}; only `refresh` rescans them");
                return;
            }
        };
        if stream.next().await.is_none() {
            return;
        }
        settle(&mut stream).await;
    }
}

/// One inotify instance over every directory the scan walks. A missing directory is watched
/// through its nearest existing ancestor, whose `CREATE` announces it.
fn watch(dirs: &[PathBuf]) -> std::io::Result<Inotify> {
    let inotify = Inotify::init()?;
    let changes = WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF;
    for dir in dirs {
        let mut walked = Vec::new();
        walk(dir, 0, &mut walked, &mut Vec::new());
        let ancestor = walked.is_empty().then(|| dir.ancestors().skip(1).find(|path| path.is_dir())).flatten();
        let watches = walked.iter().map(|path| (path.as_path(), changes));
        for (path, mask) in watches.chain(ancestor.map(|path| (path, WatchMask::CREATE | WatchMask::MOVED_TO))) {
            // Removed since the walk: the scan that follows sees it gone.
            if let Err(err) = inotify.watches().add(path, mask)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(err);
            }
        }
    }
    Ok(inotify)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_steady_trickle_still_settles_within_the_max_wait() {
        let trickle = Box::pin(futures_util::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Some(((), ()))
        }));
        let start = tokio::time::Instant::now();
        settle(trickle).await;
        assert!(start.elapsed() <= Duration::from_secs(2));
    }
}

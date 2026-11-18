//! What identifies a file across draws: its revision, and whether its failure was already reported.

use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::inotify::WatchDescriptor;
use rustc_hash::FxHashMap as HashMap;

use super::watch::Watcher;

/// File revision at a stable path (ADR-0031 deferred item): tray updates reuse
/// `tray/{name}.png` in the instance dir, with no revision suffix. Use mtime and length, not a
/// content hash: tmpfs mtime is nanosecond-precise, length is free, and hashing reads the file to
/// decide whether to read it. Unstatable files use the default, so *missing* files retry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileVersion {
    pub(super) mtime_secs: i64,
    pub(super) mtime_nanos: i64,
    pub(super) len: u64,
}

impl FileVersion {
    /// ponytail: system assets under /usr, /nix, /var/lib/flatpak, or /opt are static.
    /// Skipping stat avoids thousands of redundant filesystem queries per second for theme icons.
    fn is_static(path: &Path) -> bool {
        path.starts_with("/usr")
            || path.starts_with("/nix")
            || path.starts_with("/var/lib/flatpak")
            || path.starts_with("/opt")
    }

    pub fn read(path: &Path) -> Self {
        if Self::is_static(path) {
            return FileVersion::default();
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            return FileVersion::default();
        };
        let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
        FileVersion {
            mtime_secs: modified.map(|d| d.as_secs() as i64).unwrap_or(-1),
            mtime_nanos: modified.map(|d| i64::from(d.subsec_nanos())).unwrap_or(-1),
            len: metadata.len(),
        }
    }
}

/// How long the stat of a path inotify cannot watch is trusted (ADR-0371).
/// ponytail: each polled path keeps two wakes a second.
const STALE_AFTER: Duration = Duration::from_millis(500);

/// A remembered stat and its directory watch, `None` while polled.
struct Stat {
    at: Instant,
    version: FileVersion,
    watch: Option<WatchDescriptor>,
}

/// Per-path [`FileVersion`] memo, so a draw does not stat its file. A watched path is trusted until
/// an event names it; a polled one until [`VersionCache::changed`] re-stats it.
pub(super) struct VersionCache {
    stats: HashMap<PathBuf, Stat>,
    watcher: Option<Watcher>,
}

impl Default for VersionCache {
    fn default() -> Self {
        Self { stats: HashMap::default(), watcher: Watcher::new() }
    }
}

impl VersionCache {
    pub(super) fn read(&mut self, path: &Path, now: Instant) -> FileVersion {
        if FileVersion::is_static(path) {
            return FileVersion::default();
        }
        if let Some(stat) = self.stats.get(path) {
            return stat.version;
        }
        // Watched before the stat, so a write between the two is an event rather than lost.
        let watch = self.watcher.as_mut().and_then(|watcher| watcher.watch(path));
        let version = FileVersion::read(path);
        self.stats.insert(path.to_path_buf(), Stat { at: now, version, watch });
        version
    }

    /// The remembered version of `path`, without a stat.
    pub(super) fn get(&self, path: &Path) -> Option<FileVersion> {
        self.stats.get(path).map(|stat| stat.version)
    }

    pub(super) fn len(&self) -> usize {
        self.stats.len()
    }

    pub(super) fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.stats.keys()
    }

    /// Drops `path`, releasing its share of each directory watch.
    pub(super) fn forget(&mut self, path: &Path) {
        if let Some(Stat { watch: Some(_), .. }) = self.stats.remove(path)
            && let Some(watcher) = self.watcher.as_mut()
        {
            watcher.unwatch(path);
        }
    }

    /// The inotify descriptor for the poll loop, which has nothing to wait for without one.
    pub(super) fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.watcher.as_ref().map(Watcher::fd)
    }

    /// When [`VersionCache::changed`] next has a polled stat to renew, for the poll loop's timeout.
    pub(super) fn due(&self) -> Option<Instant> {
        self.stats.values().filter(|stat| stat.watch.is_none()).map(|stat| stat.at + STALE_AFTER).min()
    }

    /// The remembered paths whose file changed since their last stat: those an event named or whose
    /// watch ended, and polled ones past their window. That is the repaint cue: an unchanged file
    /// costs a stat, never a paint.
    pub(super) fn changed(&mut self, now: Instant) -> Vec<PathBuf> {
        let mut suspects = Vec::new();
        // ponytail: drained every turn, one `EAGAIN` read when nothing happened, instead of
        // threading the fd's readiness from the poll down here.
        if let Some(watcher) = self.watcher.as_mut() {
            let drained = watcher.drain();
            for path in &drained.lost {
                if let Some(stat) = self.stats.get_mut(path) {
                    stat.watch = None;
                }
            }
            suspects.extend(drained.touched.into_iter().chain(drained.lost));
        }
        let stale = |stat: &Stat| stat.watch.is_none() && now.saturating_duration_since(stat.at) >= STALE_AFTER;
        suspects.extend(self.stats.iter().filter(|(_, stat)| stale(stat)).map(|(path, _)| path.clone()));
        let mut changed = Vec::new();
        for path in suspects {
            let Some(stat) = self.stats.get_mut(&path) else { continue };
            // Re-watch polled/lost paths and touched paths (a touched symlink may have re-pointed,
            // become dangling, or replaced a regular file).
            stat.watch = self.watcher.as_mut().and_then(|watcher| watcher.watch(&path));
            let fresh = FileVersion::read(&path);
            stat.at = now;
            if fresh != stat.version {
                stat.version = fresh;
                changed.push(path);
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polled() -> VersionCache {
        VersionCache { watcher: None, ..VersionCache::default() }
    }

    /// The idle repaint loop: a paint reads its file twice (`image`, then `loaded`), and each read
    /// owed a repaint at its window's end. Polled, an unchanged file costs a stat a window.
    #[test]
    fn a_polled_file_is_stat_ed_once_per_window_and_only_a_change_is_a_cue() {
        let dir = tempfile::tempdir().unwrap();
        let (path, missing) = (dir.path().join("wall.png"), dir.path().join("not-yet.png"));
        std::fs::write(&path, "one").unwrap();
        let (mut cache, t0) = (polled(), Instant::now());
        for file in [&path, &missing, &path, &missing] {
            cache.read(file, t0);
        }
        let mut repaints = 0;
        for window in 1..=20 {
            let at = t0 + STALE_AFTER * window;
            assert_eq!(cache.due(), Some(at), "the poll keeps one wake per window");
            repaints += cache.changed(at).len();
        }
        assert_eq!(repaints, 0, "20 windows of unchanged files, a missing one included");

        let at = t0 + STALE_AFTER * 21;
        std::fs::write(&path, "three").unwrap();
        std::fs::write(&missing, "now").unwrap();
        assert!(cache.changed(at - STALE_AFTER / 2).is_empty(), "inside the window nothing is rechecked");
        assert_eq!(cache.read(&path, at).len, 3, "nor stat-ed by a draw");
        let mut changed = cache.changed(at);
        changed.sort();
        assert_eq!(changed, [missing, path.clone()], "past it an edit and a file appearing are the cue");
        assert_eq!(cache.read(&path, at).len, 5, "and the next draw reads it");
    }

    #[test]
    fn a_path_whose_directory_goes_is_polled_from_then_on() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("walls");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("wall.png");
        std::fs::write(&path, "one").unwrap();
        let (mut cache, t0) = (VersionCache::default(), Instant::now());
        cache.read(&path, t0);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&path), "gone is a change");
        assert_eq!(cache.due(), Some(t0 + STALE_AFTER), "and polled, with no directory to watch");

        std::fs::create_dir(&dir).unwrap();
        std::fs::write(&path, "back").unwrap();
        assert_eq!(cache.changed(t0 + STALE_AFTER), [path], "the poll sees it return");
        assert_eq!(cache.due(), None, "and watches its directory again");
    }

    #[test]
    fn a_symlinked_file_owes_no_wake_and_its_targets_edit_is_the_cue() {
        let (link_dir, dir1, dir2) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target1 = dir1.path().join("a.png");
        let target2 = dir2.path().join("b.png");
        let link = link_dir.path().join("wall.png");
        std::fs::write(&target1, "one").unwrap();
        std::fs::write(&target2, "fourteen").unwrap();
        std::os::unix::fs::symlink(&target1, &link).unwrap();

        let (mut cache, t0) = (VersionCache::default(), Instant::now());
        cache.read(&link, t0);
        assert_eq!(cache.due(), None, "a watched symlink owes no poll wake");

        std::fs::write(&target1, "three").unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&link), "editing the target cues the link");
        assert_eq!(cache.due(), None);

        // Re-pointing to another directory updates the target watch and releases the old one.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target2, &link).unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&link), "re-pointing cues the link");
        assert_eq!(cache.due(), None);

        std::fs::write(&target1, "ignored-old").unwrap();
        assert!(cache.changed(t0).is_empty(), "the old target is no longer watched");

        std::fs::write(&target2, "new-target-edit").unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&link), "editing the new target cues the link");
        assert_eq!(cache.due(), None);

        // Re-pointing to a dangling target falls back to polling, then re-watches when it appears.
        let missing = dir2.path().join("missing.png");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&link));
        assert_eq!(cache.due(), Some(t0 + STALE_AFTER), "dangling symlink is polled");

        std::fs::write(&missing, "appeared").unwrap();
        assert_eq!(cache.changed(t0 + STALE_AFTER), std::slice::from_ref(&link));
        assert_eq!(cache.due(), None, "re-watched once the target exists");
    }

    #[test]
    fn an_intermediate_symlink_is_polled_and_repointing_it_cues_a_repaint() {
        let root = tempfile::tempdir().unwrap();
        for dir in ["links", "middle", "targets"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        let link = root.path().join("links/wall.png");
        let middle = root.path().join("middle/current.png");
        std::fs::write(root.path().join("targets/one.png"), "one").unwrap();
        std::fs::write(root.path().join("targets/two.png"), "second").unwrap();
        std::os::unix::fs::symlink("../middle/current.png", &link).unwrap();
        std::os::unix::fs::symlink("../targets/one.png", &middle).unwrap();

        let (mut cache, now) = (VersionCache::default(), Instant::now());
        assert_eq!(cache.read(&link, now).len, 3);
        assert_eq!(cache.due(), Some(now + STALE_AFTER), "a chain retains polling");
        assert!(cache.watcher.as_mut().unwrap().drain().touched.is_empty());
        std::fs::remove_file(&middle).unwrap();
        std::os::unix::fs::symlink("../targets/two.png", &middle).unwrap();
        assert_eq!(cache.changed(now + STALE_AFTER), std::slice::from_ref(&link));
        assert_eq!(cache.read(&link, now + STALE_AFTER).len, 6);
        assert_eq!(cache.due(), Some(now + STALE_AFTER * 2));
    }

    #[test]
    fn a_symlink_whose_target_directory_goes_is_polled_and_rewatched_on_return() {
        let root = tempfile::tempdir().unwrap();
        let link_dir = root.path().join("links");
        let target_dir = root.path().join("targets");
        std::fs::create_dir(&link_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("wall.png");
        let link = link_dir.join("wall.png");
        std::fs::write(&target, "one").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (mut cache, t0) = (VersionCache::default(), Instant::now());
        cache.read(&link, t0);
        assert_eq!(cache.due(), None);

        std::fs::remove_file(&target).unwrap();
        std::fs::remove_dir(&target_dir).unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&link));
        assert_eq!(cache.due(), Some(t0 + STALE_AFTER), "losing target dir falls back to polling");

        std::fs::create_dir(&target_dir).unwrap();
        std::fs::write(&target, "back").unwrap();
        assert_eq!(cache.changed(t0 + STALE_AFTER), std::slice::from_ref(&link));
        assert_eq!(cache.due(), None, "and re-watches both directories cleanly");

        std::fs::write(&target, "edited-again").unwrap();
        assert_eq!(cache.changed(t0 + STALE_AFTER), [link]);
    }

    #[test]
    fn system_paths_are_never_stat_ed_or_remembered() {
        let mut cache = VersionCache::default();
        assert_eq!(cache.read(Path::new("/usr/share/icons/x.png"), Instant::now()), FileVersion::default());
        assert!(cache.stats.is_empty() && cache.due().is_none(), "and owe no recheck");
    }
}

//! What identifies a file across draws: its revision, and whether its failure was already reported.

use std::collections::HashSet;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

/// How long a polled path's stat is trusted. Only paths inotify cannot watch are polled: a
/// symlink, a directory that does not exist yet, a refused watch (ADR-0371).
/// ponytail: each polled path keeps two wakes a second; watching a symlink's target directory too
/// is the upgrade for the common case.
const STALE_AFTER: Duration = Duration::from_millis(500);

/// A remembered stat, and the directory watch that keeps it current, or `None` while it is polled.
struct Stat {
    at: Instant,
    version: FileVersion,
    watch: Option<WatchDescriptor>,
}

/// Per-path [`FileVersion`] memo, so a draw does not stat its file. A watched path is trusted until
/// an event names it; a polled one for [`STALE_AFTER`].
pub(super) struct VersionCache {
    stats: HashMap<PathBuf, Stat>,
    watcher: Option<Watcher>,
    /// When the oldest polled stat stops being trusted.
    due: Option<Instant>,
}

impl Default for VersionCache {
    fn default() -> Self {
        Self { stats: HashMap::default(), watcher: Watcher::new(), due: None }
    }
}

impl VersionCache {
    pub(super) fn read(&mut self, path: &Path, now: Instant) -> FileVersion {
        if FileVersion::is_static(path) {
            return FileVersion::default();
        }
        if let Some(stat) = self.stats.get(path)
            && (stat.watch.is_some() || now.saturating_duration_since(stat.at) < STALE_AFTER)
        {
            return stat.version;
        }
        // Watched before the stat, so a write between the two is an event rather than lost.
        let watch = match self.stats.get(path) {
            Some(stat) => stat.watch,
            None => self.watcher.as_mut().and_then(|watcher| watcher.watch(path)),
        };
        let version = FileVersion::read(path);
        self.stats.insert(path.to_path_buf(), Stat { at: now, version, watch });
        if watch.is_none() {
            self.due = Some(self.due.map_or(now + STALE_AFTER, |due| due.min(now + STALE_AFTER)));
        }
        version
    }

    pub(super) fn len(&self) -> usize {
        self.stats.len()
    }

    pub(super) fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.stats.keys()
    }

    /// Drops `path`, releasing its share of its directory's watch.
    pub(super) fn forget(&mut self, path: &Path) {
        if let Some(Stat { watch: Some(wd), .. }) = self.stats.remove(path)
            && let Some(watcher) = self.watcher.as_mut()
        {
            watcher.unwatch(wd, path);
        }
    }

    /// The inotify descriptor for the poll loop, which has nothing to wait for without one.
    pub(super) fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.watcher.as_ref().map(Watcher::fd)
    }

    /// When [`VersionCache::changed`] next has a polled stat to renew, for the poll loop's timeout.
    pub(super) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// The remembered paths whose file changed since their last stat: those an event named, every
    /// one after a queue overflow, and polled ones past their window. That is the repaint cue: an
    /// unchanged file costs a stat, never a paint.
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
            if drained.overflow {
                suspects.extend(self.stats.keys().cloned());
            } else {
                suspects.extend(drained.touched.into_iter().chain(drained.lost));
            }
        }
        let polling = self.due.is_some_and(|due| due <= now);
        if polling {
            let stale = |stat: &Stat| stat.watch.is_none() && now.saturating_duration_since(stat.at) >= STALE_AFTER;
            suspects.extend(self.stats.iter().filter(|(_, stat)| stale(stat)).map(|(path, _)| path.clone()));
        }
        if suspects.is_empty() && !polling {
            return Vec::new();
        }
        let mut changed = Vec::new();
        for path in suspects {
            let Some(stat) = self.stats.get_mut(&path) else { continue };
            if stat.watch.is_none() {
                // A directory that has appeared since, or a watch slot freed since.
                stat.watch = self.watcher.as_mut().and_then(|watcher| watcher.watch(&path));
            }
            let fresh = FileVersion::read(&path);
            stat.at = now;
            if fresh != stat.version {
                stat.version = fresh;
                changed.push(path);
            }
        }
        self.due = self.stats.values().filter(|stat| stat.watch.is_none()).map(|stat| stat.at + STALE_AFTER).min();
        changed
    }
}

/// Whether this is the first failure seen for `key`, an icon name or image path, so one drawn every
/// frame warns once. ponytail: cleared wholesale at 1024 keys, like the icon memo, so a stream of
/// novel failing names can repeat a warning; an LRU is the upgrade if that ever floods the log.
pub(crate) fn first_failure(key: &str) -> bool {
    static WARNED: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    let mut warned = WARNED.get_or_init(Default::default).lock().expect("warned set poisoned");
    if warned.len() >= 1024 {
        warned.clear();
    }
    warned.insert(key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polled() -> VersionCache {
        VersionCache { watcher: None, ..VersionCache::default() }
    }

    #[test]
    fn a_polled_path_is_stat_ed_once_per_window_and_an_edit_shows_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("avatar.png");
        std::fs::write(&path, "one").unwrap();
        let (mut cache, t0) = (polled(), Instant::now());
        let first = cache.read(&path, t0);
        assert_eq!(first.len, 3);
        assert_eq!(cache.due(), Some(t0 + STALE_AFTER), "a recheck is owed when the window ends");

        std::fs::write(&path, "three").unwrap();
        assert_eq!(cache.read(&path, t0 + STALE_AFTER / 2), first, "inside the window the file is not stat-ed again");
        assert!(cache.changed(t0 + STALE_AFTER / 2).is_empty(), "nor rechecked");
        assert_eq!(cache.changed(t0 + STALE_AFTER), std::slice::from_ref(&path), "past the window the edit is the cue");
        assert_eq!(cache.read(&path, t0 + STALE_AFTER).len, 5, "and the next draw reads it");
    }

    /// The idle repaint loop: a paint reads its file twice (`image`, then `loaded`), and each read
    /// inside the window owed a repaint at its end, whose paint read twice again: two full-surface
    /// draws and swaps a second for a wallpaper nobody touched. Polled, an unchanged file costs a
    /// stat a window.
    #[test]
    fn an_unchanged_polled_file_costs_a_stat_per_window_and_never_a_repaint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wall.png");
        let missing = dir.path().join("not-yet.png");
        std::fs::write(&path, "one").unwrap();
        let (mut cache, t0) = (polled(), Instant::now());
        for file in [&path, &missing] {
            cache.read(file, t0);
            cache.read(file, t0);
        }
        let mut repaints = 0;
        for window in 1..=20 {
            let at = t0 + STALE_AFTER * window;
            assert_eq!(cache.due(), Some(at), "the poll keeps one wake per window");
            repaints += cache.changed(at).len();
        }
        assert_eq!(repaints, 0, "20 windows of unchanged files, a missing one included");

        std::fs::write(&missing, "now").unwrap();
        assert_eq!(cache.changed(t0 + STALE_AFTER * 21), [missing], "a file that appears is a change");
    }

    /// The same idle shell, watched: no wake at all, and a cue only for a real change.
    #[test]
    fn a_watched_file_owes_no_wake_and_each_change_is_one_cue() {
        let dir = tempfile::tempdir().unwrap();
        let (path, missing, temp) =
            (dir.path().join("wall.png"), dir.path().join("not-yet.png"), dir.path().join(".wall.png.tmp"));
        std::fs::write(&path, "one").unwrap();
        let (mut cache, t0) = (VersionCache::default(), Instant::now());
        for file in [&path, &missing] {
            cache.read(file, t0);
            cache.read(file, t0);
        }
        assert_eq!(cache.due(), None, "nothing to poll, so nothing to wake for");
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        std::fs::set_permissions(&path, permissions).unwrap();
        let repaints: usize = (1..=20).map(|window| cache.changed(t0 + STALE_AFTER * window).len()).sum();
        assert_eq!(repaints, 0, "an event that changes no version (chmod to the same mode) is a stat");

        std::fs::write(&temp, "two!").unwrap();
        std::fs::rename(&temp, &path).unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&path), "an atomic save is one cue");
        assert!(cache.changed(t0).is_empty());
        assert_eq!(cache.read(&path, t0).len, 4, "and the next draw reads it, though no window passed");
        std::fs::write(&missing, "now").unwrap();
        assert_eq!(cache.changed(t0), std::slice::from_ref(&missing), "so is a file appearing");

        cache.forget(&path);
        cache.forget(&missing);
        assert_eq!(cache.watcher.as_ref().unwrap().watches(), 0, "the last path releases the directory");
        std::fs::write(&path, "three").unwrap();
        assert!(cache.changed(t0).is_empty() && cache.stats.is_empty());
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
    fn system_paths_are_never_stat_ed_or_remembered() {
        let mut cache = VersionCache::default();
        assert_eq!(cache.read(Path::new("/usr/share/icons/x.png"), Instant::now()), FileVersion::default());
        assert!(cache.stats.is_empty() && cache.due().is_none(), "and owe no recheck");
    }
}

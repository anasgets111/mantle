//! What identifies a file across draws: its revision, and whether its failure was already reported.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap as HashMap;

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

// ponytail: a stat is trusted 500 ms, and a remembered path is re-stat-ed that often while its
// slot lives, drawn or not: up to `CACHE_CAPACITY` stats and two wakes a second. A file watcher
// (inotify on the parent directories, which see a rename over the file) is the upgrade to instant
// and wake-free.
const STALE_AFTER: Duration = Duration::from_millis(500);

/// Per-path [`FileVersion`] memo, so a draw of a non-system image stats at most twice a second.
/// ponytail: cleared wholesale past `CACHE_CAPACITY` paths, which only costs a re-stat each.
#[derive(Default)]
pub(super) struct VersionCache {
    stats: HashMap<PathBuf, (Instant, FileVersion)>,
    /// When the oldest remembered stat stops being trusted.
    due: Option<Instant>,
}

impl VersionCache {
    pub(super) fn read(&mut self, path: &Path, now: Instant) -> FileVersion {
        if FileVersion::is_static(path) {
            return FileVersion::default();
        }
        if let Some(&(at, version)) = self.stats.get(path)
            && now.saturating_duration_since(at) < STALE_AFTER
        {
            return version;
        }
        if self.stats.len() >= super::CACHE_CAPACITY {
            self.stats.clear();
        }
        let version = FileVersion::read(path);
        self.stats.insert(path.to_path_buf(), (now, version));
        self.due = Some(self.due.map_or(now + STALE_AFTER, |due| due.min(now + STALE_AFTER)));
        version
    }

    /// When [`VersionCache::recheck`] next has a stat to renew, for the poll loop's timeout.
    pub(super) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Re-stats the paths `live` keeps whose stat went stale, forgets the rest, and returns those
    /// whose file changed. That is the repaint cue: an unchanged file costs a stat, not a paint.
    pub(super) fn recheck(&mut self, now: Instant, live: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
        if self.due.is_none_or(|due| due > now) {
            return Vec::new();
        }
        let mut changed = Vec::new();
        self.stats.retain(|path, (at, version)| {
            if !live(path) {
                return false;
            }
            if now.saturating_duration_since(*at) >= STALE_AFTER {
                let fresh = FileVersion::read(path);
                if fresh != *version {
                    changed.push(path.clone());
                }
                (*at, *version) = (now, fresh);
            }
            true
        });
        self.due = self.stats.values().map(|(at, _)| *at + STALE_AFTER).min();
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

    #[test]
    fn a_path_is_stat_ed_once_per_window_and_an_edit_shows_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("avatar.png");
        std::fs::write(&path, "one").unwrap();
        let (mut cache, t0) = (VersionCache::default(), Instant::now());
        let first = cache.read(&path, t0);
        assert_eq!(first.len, 3);
        assert_eq!(cache.due(), Some(t0 + STALE_AFTER), "a recheck is owed when the window ends");

        std::fs::write(&path, "three").unwrap();
        assert_eq!(cache.read(&path, t0 + STALE_AFTER / 2), first, "inside the window the file is not stat-ed again");
        assert!(cache.recheck(t0 + STALE_AFTER / 2, |_| true).is_empty(), "nor rechecked");
        assert_eq!(
            cache.recheck(t0 + STALE_AFTER, |_| true),
            std::slice::from_ref(&path),
            "past the window the edit is the cue"
        );
        assert_eq!(cache.read(&path, t0 + STALE_AFTER).len, 5, "and the next draw reads it");
    }

    /// The idle repaint loop: a paint reads its file twice (`image`, then `loaded`), and each read
    /// inside the window owed a repaint at its end, whose paint read twice again: two full-surface
    /// draws and swaps a second for a wallpaper nobody touched. An unchanged file costs a stat.
    #[test]
    fn an_unchanged_file_costs_a_stat_per_window_and_never_a_repaint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wall.png");
        let missing = dir.path().join("not-yet.png");
        std::fs::write(&path, "one").unwrap();
        let (mut cache, t0) = (VersionCache::default(), Instant::now());
        for file in [&path, &missing] {
            cache.read(file, t0);
            cache.read(file, t0);
        }
        let mut repaints = 0;
        for window in 1..=20 {
            let at = t0 + STALE_AFTER * window;
            assert_eq!(cache.due(), Some(at), "the poll keeps one wake per window");
            repaints += cache.recheck(at, |_| true).len();
        }
        assert_eq!(repaints, 0, "20 windows of unchanged files, a missing one included");

        std::fs::write(&missing, "now").unwrap();
        let at = t0 + STALE_AFTER * 21;
        assert_eq!(cache.recheck(at, |_| true), [missing], "a file that appears is a change");
        assert!(cache.recheck(at + STALE_AFTER, |file| file != path).is_empty());
        assert!(!cache.stats.contains_key(&path), "a path nothing keeps is forgotten");
    }

    #[test]
    fn system_paths_are_never_stat_ed_or_remembered() {
        let mut cache = VersionCache::default();
        assert_eq!(cache.read(Path::new("/usr/share/icons/x.png"), Instant::now()), FileVersion::default());
        assert!(cache.stats.is_empty() && cache.due().is_none(), "and owe no recheck");
    }
}

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

// ponytail: a stat is trusted 500 ms; a file watcher is the upgrade for instant.
const STALE_AFTER: Duration = Duration::from_millis(500);

/// Per-path [`FileVersion`] memo, so a draw of a non-system image stats at most twice a second.
/// ponytail: cleared wholesale past `CACHE_CAPACITY` paths, which only costs a re-stat each.
#[derive(Default)]
pub(super) struct VersionCache(HashMap<PathBuf, (Instant, FileVersion)>);

impl VersionCache {
    /// The version and, for a remembered one, when it stops being trusted, so the caller can owe a repaint then.
    pub(super) fn read(&mut self, path: &Path, now: Instant) -> (FileVersion, Option<Instant>) {
        if FileVersion::is_static(path) {
            return (FileVersion::default(), None);
        }
        if let Some(&(at, version)) = self.0.get(path)
            && now.saturating_duration_since(at) < STALE_AFTER
        {
            return (version, Some(at + STALE_AFTER));
        }
        if self.0.len() >= super::CACHE_CAPACITY {
            self.0.clear();
        }
        let version = FileVersion::read(path);
        self.0.insert(path.to_path_buf(), (now, version));
        (version, None)
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
        let (first, due) = cache.read(&path, t0);
        assert_eq!(due, None, "a fresh stat owes nothing");
        assert_eq!(first.len, 3);

        std::fs::write(&path, "three").unwrap();
        let (inside, due) = cache.read(&path, t0 + STALE_AFTER / 2);
        assert_eq!(inside, first, "inside the window the file is not stat-ed again");
        assert_eq!(due, Some(t0 + STALE_AFTER), "and a repaint is owed when the window ends");
        let (after, _) = cache.read(&path, t0 + STALE_AFTER);
        assert_eq!(after.len, 5, "past the window the edit is seen");
    }

    #[test]
    fn system_paths_are_never_stat_ed_or_remembered() {
        let mut cache = VersionCache::default();
        assert_eq!(cache.read(Path::new("/usr/share/icons/x.png"), Instant::now()), (FileVersion::default(), None));
        assert!(cache.0.is_empty());
    }
}

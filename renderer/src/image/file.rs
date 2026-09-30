//! What identifies a file across draws: its revision, and whether its failure was already reported.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

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
    pub fn read(path: &Path) -> Self {
        // ponytail: system assets under /usr, /nix, /var/lib/flatpak, or /opt are static.
        // Skipping stat avoids thousands of redundant filesystem queries per second for theme icons.
        if path.starts_with("/usr")
            || path.starts_with("/nix")
            || path.starts_with("/var/lib/flatpak")
            || path.starts_with("/opt")
        {
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

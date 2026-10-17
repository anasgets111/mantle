//! Which remembered image files changed on disk, from inotify on their parent directories
//! (ADR-0371). A directory watch sees an in-place edit, an atomic rename over the file, a delete,
//! and a file that did not exist yet; a watch on the file itself would follow the replaced inode.

use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use rustc_hash::FxHashMap as HashMap;
use shared::warn;

/// Writes finished, renames in and out, creates, deletes, `touch`, and the directory itself moving
/// away (its paths then name nothing the watch sees). `IN_IGNORED` and `IN_Q_OVERFLOW` always come.
const MASK: AddWatchFlags = AddWatchFlags::IN_CLOSE_WRITE
    .union(AddWatchFlags::IN_MOVED_TO)
    .union(AddWatchFlags::IN_MOVED_FROM)
    .union(AddWatchFlags::IN_CREATE)
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_ATTRIB)
    .union(AddWatchFlags::IN_MOVE_SELF)
    .union(AddWatchFlags::IN_ONLYDIR);

/// What one drain found.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Drained {
    /// Watched paths an event named, to re-stat. A no-op event (`touch -c` of nothing) restats to
    /// the same version and repaints nothing.
    pub touched: Vec<PathBuf>,
    /// Paths whose directory watch ended (deleted, unmounted, moved): no longer watched, so the
    /// caller polls them.
    pub lost: Vec<PathBuf>,
    /// The kernel queue overflowed and dropped events: every path is suspect.
    pub overflow: bool,
}

pub(super) struct Watcher {
    inotify: Inotify,
    /// The paths each directory watch serves; its length is the watch's reference count. Keyed by
    /// descriptor, not directory path, because two spellings of one directory (a symlink) get
    /// the same descriptor from the kernel.
    dirs: HashMap<WatchDescriptor, Vec<PathBuf>>,
    warned: bool,
}

impl Watcher {
    /// `None`, with one warning, where inotify is unavailable; every path is then polled.
    pub(super) fn new() -> Option<Self> {
        match Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC) {
            Ok(inotify) => Some(Self { inotify, dirs: HashMap::default(), warned: false }),
            Err(err) => {
                warn!("inotify unavailable ({err}); image files are re-checked by stat twice a second");
                None
            }
        }
    }

    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
    }

    /// Watches `path`'s directory for it, or `None` to poll it: a symlink (an edit to its target
    /// happens elsewhere), a directory that does not exist yet, or a refused watch.
    pub(super) fn watch(&mut self, path: &Path) -> Option<WatchDescriptor> {
        if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()) {
            return None;
        }
        let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let wd = match self.inotify.add_watch(dir, MASK) {
            Ok(wd) => wd,
            // Missing or unreadable: so is the file, and polling sees it appear.
            Err(Errno::ENOENT | Errno::ENOTDIR | Errno::EACCES) => return None,
            Err(err) => {
                if !std::mem::replace(&mut self.warned, true) {
                    warn!(
                        "cannot watch {} ({err}; ENOSPC is fs.inotify.max_user_watches); its image files and any others refused are re-checked by stat twice a second",
                        dir.display()
                    );
                }
                return None;
            }
        };
        self.dirs.entry(wd).or_default().push(path.to_path_buf());
        Some(wd)
    }

    /// Releases `path`'s share of `wd`, removing the watch with its last path.
    pub(super) fn unwatch(&mut self, wd: WatchDescriptor, path: &Path) {
        let Some(paths) = self.dirs.get_mut(&wd) else { return };
        paths.retain(|watched| watched != path);
        if paths.is_empty() {
            self.dirs.remove(&wd);
            // EINVAL: the kernel already ended it, and its `IN_IGNORED` is still queued.
            let _ = self.inotify.rm_watch(wd);
        }
    }

    /// Reads every queued event without blocking.
    pub(super) fn drain(&mut self) -> Drained {
        let mut drained = Drained::default();
        loop {
            match self.inotify.read_events() {
                Ok(events) => {
                    for event in events {
                        self.map(event.wd, event.mask, event.name.as_deref(), &mut drained);
                    }
                }
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => break,
                Err(err) => {
                    warn!("reading inotify failed ({err}); treating every watched image file as changed");
                    drained.overflow = true;
                    break;
                }
            }
        }
        drained
    }

    /// One event's effect. An unknown descriptor is a watch already released, whose queued
    /// events and `IN_IGNORED` still arrive.
    fn map(&mut self, wd: WatchDescriptor, mask: AddWatchFlags, name: Option<&std::ffi::OsStr>, out: &mut Drained) {
        if mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
            out.overflow = true;
            return;
        }
        if mask.intersects(AddWatchFlags::IN_IGNORED | AddWatchFlags::IN_MOVE_SELF) {
            if let Some(paths) = self.dirs.remove(&wd) {
                let _ = self.inotify.rm_watch(wd);
                out.lost.extend(paths);
            }
            return;
        }
        let (Some(paths), Some(name)) = (self.dirs.get(&wd), name) else { return };
        for path in paths.iter().filter(|path| path.file_name() == Some(name)) {
            if !out.touched.contains(path) {
                out.touched.push(path.clone());
            }
        }
    }

    #[cfg(test)]
    pub(super) fn watches(&self) -> usize {
        self.dirs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_watched_once_for_all_its_paths_and_released_with_the_last() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut watcher = Watcher::new().unwrap();
        let [one, two, other] = [a.path().join("1.png"), a.path().join("2.png"), b.path().join("x.png")];
        let first = watcher.watch(&one).unwrap();
        assert_eq!(watcher.watch(&two), Some(first), "one watch per directory");
        assert_ne!(watcher.watch(&other), Some(first));
        assert_eq!(watcher.watches(), 2);

        watcher.unwatch(first, &one);
        assert_eq!(watcher.watches(), 2, "2.png still holds its directory");
        watcher.unwatch(first, &two);
        assert_eq!(watcher.watches(), 1, "the last path releases it");
        std::fs::write(&two, "x").unwrap();
        assert_eq!(watcher.drain(), Drained::default(), "a released watch reports nothing");
    }

    #[test]
    fn edits_renames_and_deletes_name_the_watched_path_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (wall, neighbour) = (dir.path().join("wall.png"), dir.path().join("other.png"));
        let mut watcher = Watcher::new().unwrap();
        watcher.watch(&wall).unwrap();

        std::fs::write(&wall, "appears").unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&wall), "a file that did not exist yet");
        std::fs::write(&neighbour, "x").unwrap();
        assert_eq!(watcher.drain(), Drained::default(), "a sibling is not this path");
        std::fs::rename(&neighbour, &wall).unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&wall), "an atomic save, once");
        std::fs::remove_file(&wall).unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&wall));
    }

    #[test]
    fn a_removed_directory_hands_its_paths_back_to_polling() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("walls");
        std::fs::create_dir(&dir).unwrap();
        let mut watcher = Watcher::new().unwrap();
        assert_eq!(watcher.watch(&root.path().join("missing/x.png")), None, "no directory yet: polled");
        let wall = dir.join("wall.png");
        watcher.watch(&wall).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(watcher.drain().lost, [wall]);
        assert_eq!(watcher.watches(), 0);
    }

    #[test]
    fn a_symlinked_file_is_polled() {
        let dir = tempfile::tempdir().unwrap();
        let (target, link) = (dir.path().join("target.png"), dir.path().join("link.png"));
        std::fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut watcher = Watcher::new().unwrap();
        assert_eq!(watcher.watch(&link), None, "its target's edits happen in another directory");
        assert!(watcher.watch(&target).is_some());
    }
}

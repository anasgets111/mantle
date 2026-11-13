//! Which remembered image files changed on disk, from inotify on their parent directories
//! (ADR-0371). A directory watch sees an in-place edit, an atomic rename over the file, a delete,
//! and a file that did not exist yet; a watch on the file itself would follow the replaced inode.
//! ponytail: a busy parent ($HOME, ~/Downloads) wakes the loop on unrelated events, each a read
//! that names no path; upgrade by polling a directory's paths once its events flood.

use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use rustc_hash::FxHashMap as HashMap;
use shared::warn;

/// Writes, renames, creates, deletes, `touch`, and the directory moving away.
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
    /// Watched paths an event named, to re-stat.
    pub touched: Vec<PathBuf>,
    /// Paths whose watch ended, now unwatched: the caller re-stats and re-watches them.
    pub lost: Vec<PathBuf>,
}

pub(super) struct Watcher {
    inotify: Inotify,
    /// Each watch's paths, its refcount, each with the name its events carry there; keyed by
    /// descriptor, which two spellings of a directory share.
    dirs: HashMap<WatchDescriptor, Vec<(PathBuf, std::ffi::OsString)>>,
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

    /// Watches `path`'s directory for it, or `None` to poll it: a dangling symlink, a directory
    /// that does not exist yet, or a refused watch. A symlink's own directory hears it re-pointed
    /// and its target's hears the edits. Chained symlinks are polled instead.
    /// ponytail: chains keep 500 ms polling; watch every hop to avoid it.
    pub(super) fn watch(&mut self, path: &Path) -> Option<WatchDescriptor> {
        // Watch the link's own parent first so a re-point before canonicalize still queues an event.
        let Some(first) = self.watch_at(path, path) else {
            self.unwatch(path);
            return None;
        };
        let linked = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink());
        let second = if linked {
            let entry = std::fs::read_link(path).ok().and_then(|target| {
                let target = path.parent()?.join(target);
                if std::fs::symlink_metadata(&target).ok()?.is_symlink() {
                    return None;
                }
                self.watch_at(path, &std::fs::canonicalize(target).ok()?)
            });
            let Some(entry) = entry else {
                self.unwatch(path);
                return None;
            };
            Some(entry)
        } else {
            None
        };
        // Drop any stale watch entries for `path` (e.g., an old symlink target) without touching
        // the newly kept ones, so active directory watches are never briefly removed.
        self.dirs.retain(|wd, paths| {
            paths.retain(|(watched, here)| {
                watched != path
                    || (*wd == first.0 && *here == first.1)
                    || second.as_ref().is_some_and(|(wd2, name2)| *wd == *wd2 && *here == *name2)
            });
            if paths.is_empty() {
                let _ = self.inotify.rm_watch(*wd);
            }
            !paths.is_empty()
        });
        Some(first.0)
    }

    /// Watches `at`'s directory for `path`, whose events there carry `at`'s name.
    fn watch_at(&mut self, path: &Path, at: &Path) -> Option<(WatchDescriptor, std::ffi::OsString)> {
        let name = at.file_name()?.to_owned();
        let dir = at.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
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
        let paths = self.dirs.entry(wd).or_default();
        if !paths.iter().any(|(watched, here)| watched == path && *here == name) {
            paths.push((path.to_path_buf(), name.clone()));
        }
        Some((wd, name))
    }

    /// Releases `path`'s share of each watch, removing a watch with its last path.
    pub(super) fn unwatch(&mut self, path: &Path) {
        self.dirs.retain(|wd, paths| {
            paths.retain(|(watched, _)| watched != path);
            if paths.is_empty() {
                // EINVAL: the kernel already ended it, and its `IN_IGNORED` is still queued.
                let _ = self.inotify.rm_watch(*wd);
            }
            !paths.is_empty()
        });
    }

    /// Reads every queued event without blocking.
    pub(super) fn drain(&mut self) -> Drained {
        let (mut drained, mut overflow) = (Drained::default(), false);
        loop {
            match self.inotify.read_events() {
                Ok(events) => {
                    for event in events {
                        overflow |= event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW);
                        self.map(event.wd, event.mask, event.name.as_deref(), &mut drained);
                    }
                }
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => break,
                Err(err) => {
                    warn!("reading inotify failed ({err}); treating every watched image file as changed");
                    overflow = true;
                    break;
                }
            }
        }
        if overflow {
            self.lose_all(&mut drained);
        }
        drained
    }

    /// Drops every watch (e.g. after `IN_Q_OVERFLOW`) and records each path once in `out.lost`.
    fn lose_all(&mut self, out: &mut Drained) {
        for (wd, paths) in self.dirs.drain() {
            let _ = self.inotify.rm_watch(wd);
            for (path, _) in paths {
                if !out.lost.contains(&path) {
                    out.lost.push(path);
                }
            }
        }
    }

    /// One event's effect. An unknown descriptor (an overflow's too) is a released watch, whose
    /// queued events and `IN_IGNORED` still arrive.
    fn map(&mut self, wd: WatchDescriptor, mask: AddWatchFlags, name: Option<&std::ffi::OsStr>, out: &mut Drained) {
        if mask.intersects(AddWatchFlags::IN_IGNORED | AddWatchFlags::IN_MOVE_SELF) {
            if let Some(paths) = self.dirs.remove(&wd) {
                let _ = self.inotify.rm_watch(wd);
                for (path, _) in paths {
                    // A symlink's other watch goes with this one; the caller watches both again.
                    self.unwatch(&path);
                    if !out.lost.contains(&path) {
                        out.lost.push(path);
                    }
                }
            }
            return;
        }
        let (Some(paths), Some(name)) = (self.dirs.get(&wd), name) else { return };
        for (path, _) in paths.iter().filter(|(_, here)| here == name) {
            if !out.touched.contains(path) {
                out.touched.push(path.clone());
            }
        }
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
        assert_eq!(watcher.dirs.len(), 2);

        watcher.unwatch(&one);
        assert_eq!(watcher.dirs.len(), 2, "2.png still holds its directory");
        watcher.unwatch(&two);
        assert_eq!(watcher.dirs.len(), 1, "the last path releases it");
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
    fn a_symlinked_file_is_watched_where_it_is_and_where_it_points() {
        let (link_dir, target_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target = target_dir.path().join("wall.png");
        let second = target_dir.path().join("second.png");
        let link = link_dir.path().join("wall.png");
        let dangling = link_dir.path().join("dangling.png");
        std::fs::write(&target, "x").unwrap();
        std::fs::write(&second, "y").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::os::unix::fs::symlink(target_dir.path().join("missing.png"), &dangling).unwrap();

        let mut watcher = Watcher::new().unwrap();
        assert_eq!(watcher.watch(&dangling), None, "a dangling symlink is polled");
        assert!(watcher.dirs.is_empty());

        // Watching a target and a same-named symlink in another dir shares the directory watch
        // without dropping either path.
        watcher.watch(&target).unwrap();
        assert!(watcher.watch(&link).is_some());
        assert_eq!(watcher.dirs.len(), 2, "one watch for the link, one for its target");
        let _ = watcher.drain();

        std::fs::write(&target, "edited").unwrap();
        let touched = watcher.drain().touched;
        assert_eq!(touched.len(), 2);
        assert!(touched.contains(&target) && touched.contains(&link), "editing the target names both");
        watcher.unwatch(&target);

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&link), "re-pointing the link names the link");

        // Re-watching after a re-point drops the old target (even if its basename matched the link)
        // and tracks the new one without duplicates.
        let third_dir = tempfile::tempdir().unwrap();
        let third = third_dir.path().join("third.png");
        let third_alt = third_dir.path().join("shared.png");
        let fourth_dir = tempfile::tempdir().unwrap();
        let fourth = fourth_dir.path().join("shared.png");
        std::fs::write(&third, "z").unwrap();
        std::fs::write(&third_alt, "a").unwrap();
        std::fs::write(&fourth, "b").unwrap();

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&third, &link).unwrap();
        assert!(watcher.watch(&link).is_some());
        assert_eq!(watcher.dirs.len(), 2, "stale target dir is released");
        let _ = watcher.drain();
        std::fs::write(&target, "old-ignored").unwrap();
        assert_eq!(watcher.drain(), Drained::default(), "old same-name target no longer triggers");
        std::fs::write(&third, "new-edit").unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&link), "new target triggers");

        // Re-pointing within the same target dir drops the old target filename.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&third_alt, &link).unwrap();
        assert!(watcher.watch(&link).is_some());
        let _ = watcher.drain();
        std::fs::write(&third, "same-dir-old-ignored").unwrap();
        assert_eq!(watcher.drain(), Drained::default(), "old filename in same target dir is pruned");

        // Re-pointing across dirs to a target with the same filename drops the previous dir watch.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&fourth, &link).unwrap();
        assert!(watcher.watch(&link).is_some());
        assert_eq!(watcher.dirs.len(), 2, "previous target dir released despite matching basename");
        let _ = watcher.drain();
        std::fs::write(&third_alt, "diff-dir-same-name-ignored").unwrap();
        assert_eq!(watcher.drain(), Drained::default());
        std::fs::write(&fourth, "fourth-edit").unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&link));

        watcher.unwatch(&link);
        assert!(watcher.dirs.is_empty(), "unwatch releases both directories");
        std::fs::write(&fourth, "after-unwatch").unwrap();
        assert_eq!(watcher.drain(), Drained::default());
    }

    #[test]
    fn a_same_directory_symlink_watches_both_names_and_cleans_up_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.png");
        let link = dir.path().join("link.png");
        std::fs::write(&target, "one").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut watcher = Watcher::new().unwrap();
        let wd = watcher.watch(&link).unwrap();
        assert_eq!(watcher.dirs.len(), 1);
        assert_eq!(watcher.dirs[&wd].len(), 2, "same directory records both link and target names");
        let _ = watcher.drain();

        std::fs::write(&target, "two").unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&link));

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(watcher.drain().touched, std::slice::from_ref(&link));

        // IN_IGNORED on the shared descriptor produces one lost entry, not two.
        let mut out = Drained::default();
        watcher.map(wd, AddWatchFlags::IN_IGNORED, None, &mut out);
        assert_eq!(out.lost, std::slice::from_ref(&link));
        assert!(watcher.dirs.is_empty());

        // Re-watch, verify re-pointing out of link_dir prunes the old same-dir target entry,
        // and verify unwatch drops all entries.
        let wd = watcher.watch(&link).unwrap();
        assert_eq!(watcher.dirs[&wd].len(), 2);
        let other_dir = tempfile::tempdir().unwrap();
        let other = other_dir.path().join("other.png");
        std::fs::write(&other, "other").unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(watcher.watch(&link), Some(wd));
        assert_eq!(watcher.dirs[&wd].len(), 1, "old same-dir target entry was pruned");
        let _ = watcher.drain();
        std::fs::write(&target, "same-dir-old-ignored").unwrap();
        assert_eq!(watcher.drain(), Drained::default());

        watcher.unwatch(&link);
        assert!(watcher.dirs.is_empty());
    }

    #[test]
    fn losing_one_watch_of_a_symlink_releases_its_pair_and_avoids_duplicate_lost_entries() {
        let (link_dir, target_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target = target_dir.path().join("target.png");
        let link = link_dir.path().join("link.png");
        std::fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut watcher = Watcher::new().unwrap();
        let link_wd = watcher.watch(&link).unwrap();
        assert_eq!(watcher.dirs.len(), 2);
        let target_wd = *watcher.dirs.keys().find(|wd| **wd != link_wd).unwrap();

        // Removing the target directory drops target_wd via IN_IGNORED and releases link_wd too.
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_dir(target_dir.path()).unwrap();
        let drained = watcher.drain();
        assert_eq!(drained.lost, std::slice::from_ref(&link));
        assert!(watcher.dirs.is_empty(), "paired link watch was also released");

        // Re-create target and re-watch cleanly; simulate both descriptors receiving IN_IGNORED in one batch.
        std::fs::create_dir(target_dir.path()).unwrap();
        std::fs::write(&target, "y").unwrap();
        let link_wd2 = watcher.watch(&link).unwrap();
        let target_wd2 = *watcher.dirs.keys().find(|wd| **wd != link_wd2).unwrap();
        assert_ne!(target_wd2, target_wd);

        let mut out = Drained::default();
        watcher.map(link_wd2, AddWatchFlags::IN_IGNORED, None, &mut out);
        watcher.map(target_wd2, AddWatchFlags::IN_IGNORED, None, &mut out);
        assert_eq!(out.lost, std::slice::from_ref(&link));
        assert!(watcher.dirs.is_empty());

        // Overflow (lose_all) also drops both watches and deduplicates `link` in `lost`.
        watcher.watch(&link).unwrap();
        let mut overflow_out = Drained::default();
        watcher.lose_all(&mut overflow_out);
        assert_eq!(overflow_out.lost, std::slice::from_ref(&link));
        assert!(watcher.dirs.is_empty());
    }

    #[test]
    fn partial_symlink_watch_failure_rolls_back_the_link_directory_watch() {
        let dir = tempfile::tempdir().unwrap();
        let root_link = dir.path().join("to-root");
        std::os::unix::fs::symlink("/", &root_link).unwrap();

        let mut watcher = Watcher::new().unwrap();
        assert_eq!(watcher.watch(&root_link), None, "canonical target `/` has no file_name");
        assert!(watcher.dirs.is_empty(), "link directory watch must be rolled back on target watch failure");

        // Rolling back a dangling symlink removes only its temporary link-dir entry, keeping siblings.
        let sibling = dir.path().join("sibling.png");
        let dangling = dir.path().join("dangling.png");
        std::fs::write(&sibling, "ok").unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing.png"), &dangling).unwrap();
        let wd = watcher.watch(&sibling).unwrap();
        assert_eq!(watcher.watch(&dangling), None);
        assert_eq!(watcher.dirs.get(&wd).map(Vec::len), Some(1));
        assert_eq!(watcher.dirs[&wd][0].0, sibling);
        watcher.unwatch(&sibling);
        assert!(watcher.dirs.is_empty());
    }
}

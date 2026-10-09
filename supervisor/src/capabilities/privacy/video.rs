//! Kernel half of `mantle.privacy` (ADR-0034): scan `/proc/*/fd/*` symlinks for processes holding
//! `/dev/videoN`, then rescan on device-node inotify `OPEN`/`CLOSE`. Live testing confirmed
//! `/dev/video0` emits `IN_OPEN`/`IN_CLOSE_NOWRITE`, a VFS mechanism unlike the unreliable sysfs
//! attribute notifications used by keyboard lock LEDs (see `keyboard::locks`).
//!
//! Do not use ADR-0034's `/sys/class/video4linux/video<n>/streaming` fast path: this real UVC
//! webcam lacks it despite running past the "6.3+" threshold, so it cannot be verified live. The
//! fd scan is sufficient and independently required. It remains the ADR upgrade path.

use std::path::{Path, PathBuf};

/// Whether `name` is a capture node name, `video` and digits.
pub fn is_video_name(name: &str) -> bool {
    name.strip_prefix("video").is_some_and(|index| !index.is_empty() && index.chars().all(|c| c.is_ascii_digit()))
}

/// Every `videoN` under `dev_root` at start; the controller follows later plug and unplug.
pub fn enumerate_video_devices(dev_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dev_root) else { return Vec::new() };
    let mut devices: Vec<_> = entries
        .flatten()
        .filter(|entry| is_video_name(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    devices.sort();
    devices
}

/// Every pid whose `<proc_root>/*/fd/*` symlink resolves exactly to one of `devices`, like `fuser`;
/// sorted, one entry per pid. One pass on this machine is ~370 `opendir`s, ~11,000 `readlink`s, and
/// ~6 MiB allocation, 58% of boot allocations under DHAT, so it walks `/proc` once for all devices
/// and only inotify pays for it.
pub fn find_device_openers(proc_root: &Path, devices: &[PathBuf]) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(proc_entries) = std::fs::read_dir(proc_root) else { return pids };
    for proc_entry in proc_entries.flatten() {
        let Ok(pid) = proc_entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        if holds_device(proc_root, pid, devices) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

/// Whether `pid` has one of `devices` open: one pid's fds, not a `/proc` walk.
pub fn holds_device(proc_root: &Path, pid: u32, devices: &[PathBuf]) -> bool {
    let Ok(fd_entries) = std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")) else { return false };
    fd_entries
        .flatten()
        .any(|fd_entry| std::fs::read_link(fd_entry.path()).is_ok_and(|target| devices.contains(&target)))
}

/// Reads `<proc_root>/<pid>/comm`, the fallback for raw V4L2 users without a matching PipeWire
/// `Video/Source` node (ADR-0034).
pub fn read_comm(proc_root: &Path, pid: u32) -> Option<String> {
    std::fs::read_to_string(proc_root.join(pid.to_string()).join("comm")).ok().map(|text| text.trim_end().to_string())
}

/// Test fixture: `<proc_root>/<pid>/fd/<fd>` pointing at `target`.
#[cfg(test)]
pub(super) fn write_fd_symlink(proc_root: &Path, pid: u32, fd: u32, target: impl AsRef<Path>) {
    let fd_dir = proc_root.join(pid.to_string()).join("fd");
    std::fs::create_dir_all(&fd_dir).unwrap();
    std::os::unix::fs::symlink(target, fd_dir.join(fd.to_string())).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_video_device_dir(video4linux_root: &Path, name: &str) {
        std::fs::create_dir(video4linux_root.join(name)).unwrap();
    }

    // ---- enumerate_video_devices ----

    #[test]
    fn enumerate_video_devices_finds_every_videon_directory_as_a_dev_path() {
        let root = tempfile::tempdir().unwrap();
        write_video_device_dir(root.path(), "video0");
        write_video_device_dir(root.path(), "video1");

        assert_eq!(enumerate_video_devices(root.path()), vec![root.path().join("video0"), root.path().join("video1")]);
    }

    #[test]
    fn enumerate_video_devices_ignores_non_video_entries() {
        let root = tempfile::tempdir().unwrap();
        write_video_device_dir(root.path(), "video0");
        write_video_device_dir(root.path(), "vbi0"); // a real video4linux sibling class, not a capture device.

        assert_eq!(enumerate_video_devices(root.path()), vec![root.path().join("video0")]);
    }

    #[test]
    fn enumerate_video_devices_is_empty_against_a_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert!(enumerate_video_devices(&root.path().join("does-not-exist")).is_empty());
    }

    // ---- find_device_openers ----

    #[test]
    fn find_device_openers_finds_a_pid_with_the_device_open() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 1234, 5, "/dev/video0");
        write_fd_symlink(root.path(), 1234, 6, "/dev/null");

        assert_eq!(find_device_openers(root.path(), &[PathBuf::from("/dev/video0")]), vec![1234]);
    }

    #[test]
    fn find_device_openers_lists_a_pid_once_even_with_multiple_fds_on_the_device() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 1234, 5, "/dev/video0");
        write_fd_symlink(root.path(), 1234, 6, "/dev/video0");

        assert_eq!(find_device_openers(root.path(), &[PathBuf::from("/dev/video0")]), vec![1234]);
    }

    #[test]
    fn find_device_openers_ignores_a_pid_with_no_matching_fd() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 1234, 5, "/dev/null");

        assert!(find_device_openers(root.path(), &[PathBuf::from("/dev/video0")]).is_empty());
    }

    #[test]
    fn holds_device_checks_one_pid_only() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 1, 3, "/dev/video0");
        write_fd_symlink(root.path(), 2, 3, "/dev/null");
        let devices = [PathBuf::from("/dev/video0")];
        assert!(holds_device(root.path(), 1, &devices));
        assert!(!holds_device(root.path(), 2, &devices));
        assert!(!holds_device(root.path(), 3, &devices));
    }

    #[test]
    fn find_device_openers_is_empty_against_an_empty_proc_root() {
        let root = tempfile::tempdir().unwrap();
        assert!(find_device_openers(root.path(), &[PathBuf::from("/dev/video0")]).is_empty());
    }

    #[test]
    fn find_device_openers_finds_multiple_distinct_pids_sorted() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 999, 3, "/dev/video0");
        write_fd_symlink(root.path(), 42, 3, "/dev/video0");

        assert_eq!(find_device_openers(root.path(), &[PathBuf::from("/dev/video0")]), vec![42, 999]);
    }

    #[test]
    fn find_device_openers_finds_the_openers_of_every_device_in_one_pass() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 7, 3, "/dev/video1");
        write_fd_symlink(root.path(), 42, 3, "/dev/video0");
        write_fd_symlink(root.path(), 99, 3, "/dev/video2");

        let devices = [PathBuf::from("/dev/video0"), PathBuf::from("/dev/video1")];
        assert_eq!(find_device_openers(root.path(), &devices), vec![7, 42]);
    }

    // ---- read_comm ----

    #[test]
    fn read_comm_reads_and_trims_a_real_comm_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("1234")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "mpv\n").unwrap();

        assert_eq!(read_comm(root.path(), 1234), Some("mpv".to_string()));
    }

    #[test]
    fn read_comm_is_none_for_a_pid_with_no_comm_file() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_comm(root.path(), 9999), None);
    }
}

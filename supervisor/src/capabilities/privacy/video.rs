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

/// Reads `<proc_root>/<pid>/comm`, the name of a raw V4L2 user (ADR-0034).
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

    #[test]
    fn enumerate_video_devices_lists_videon_directories_and_ignores_the_rest() {
        // (label, entries, expected)
        let cases: [(&str, &[&str], &[&str]); 2] = [
            ("every videoN directory as a dev path", &["video0", "video1"], &["video0", "video1"]),
            // vbi0 is a real video4linux sibling class, not a capture device.
            ("non-video entries", &["video0", "vbi0"], &["video0"]),
        ];
        for (label, entries, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            for entry in entries {
                std::fs::create_dir(root.path().join(entry)).unwrap();
            }
            let expected: Vec<PathBuf> = expected.iter().map(|name| root.path().join(name)).collect();
            assert_eq!(enumerate_video_devices(root.path()), expected, "{label}");
        }
        let root = tempfile::tempdir().unwrap();
        assert!(enumerate_video_devices(&root.path().join("does-not-exist")).is_empty(), "nonexistent root");
    }

    #[test]
    fn find_device_openers_lists_each_opening_pid_once_sorted() {
        // (label, (pid, fd, target) symlinks, devices asked about, expected pids)
        type Fds<'a> = &'a [(u32, u32, &'a str)];
        let cases: [(&str, Fds, &[&str], Vec<u32>); 5] = [
            (
                "one pid among other fds",
                &[(1234, 5, "/dev/video0"), (1234, 6, "/dev/null")],
                &["/dev/video0"],
                vec![1234],
            ),
            (
                "two fds on the device",
                &[(1234, 5, "/dev/video0"), (1234, 6, "/dev/video0")],
                &["/dev/video0"],
                vec![1234],
            ),
            ("no matching fd", &[(1234, 5, "/dev/null")], &["/dev/video0"], vec![]),
            (
                "distinct pids sorted",
                &[(999, 3, "/dev/video0"), (42, 3, "/dev/video0")],
                &["/dev/video0"],
                vec![42, 999],
            ),
            (
                "every device in one pass",
                &[(7, 3, "/dev/video1"), (42, 3, "/dev/video0"), (99, 3, "/dev/video2")],
                &["/dev/video0", "/dev/video1"],
                vec![7, 42],
            ),
        ];
        for (label, fds, devices, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            for (pid, fd, target) in fds {
                write_fd_symlink(root.path(), *pid, *fd, target);
            }
            let devices: Vec<PathBuf> = devices.iter().map(PathBuf::from).collect();
            assert_eq!(find_device_openers(root.path(), &devices), expected, "{label}");
        }
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
    fn read_comm_is_none_for_a_pid_with_no_comm_file() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_comm(root.path(), 9999), None);
    }
}

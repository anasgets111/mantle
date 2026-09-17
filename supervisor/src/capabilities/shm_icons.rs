//! Shared PNG spooling for `dbus::tray` and `dbus::notifications`: both spool bounds-checked PNG
//! bytes to `$XDG_RUNTIME_DIR/obelisk/{subdir}/...`; the error type and directory/write mechanics
//! are identical, while subdirectory and pixel encoding stay local.

use std::path::{Path, PathBuf};

/// `$XDG_RUNTIME_DIR/obelisk/{subdir}` (ADR-0142; previously `/dev/shm/obelisk-$UID` under ADR-0031).
///
/// Not `/dev/shm`: mode 1777 lets another user create `obelisk-$UID` first, redirecting [`sweep`]'s
/// delete and [`write_png`] through a symlink. The owned 0700 runtime directory provides the
/// intended isolation on the same tmpfs.
///
/// Fallback to systemd's path, not `/dev/shm`; silently reverting to a world-writable directory is
/// the hole. If the runtime directory does not exist, [`write_png`] fails and the item has no icon.
pub fn icon_dir(subdir: &str) -> PathBuf {
    spool_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), subdir)
}

/// [`icon_dir`] with an explicit environment, so fallback tests need not mutate process-wide state.
fn spool_dir(runtime: Option<&std::ffi::OsStr>, subdir: &str) -> PathBuf {
    let runtime =
        runtime.map_or_else(|| PathBuf::from(format!("/run/user/{}", nix::unistd::Uid::current())), PathBuf::from);
    runtime.join("obelisk").join(subdir)
}

/// Best-effort deletion of one spooled PNG.
///
/// Checks that `path` is one this module wrote before deleting it. The path traveled through a
/// snapshot and back, so the check is a trust boundary.
pub fn remove_png(subdir: &str, path: &str) {
    if !is_within(path, &icon_dir(subdir)) {
        return;
    }
    if let Err(err) = std::fs::remove_file(path) {
        eprintln!("{subdir}: failed to delete spooled icon {path:?}: {err}");
    }
}

/// Canonicalizes both sides, so neither `..` nor a symlink escapes `root`.
fn is_within(path: &str, root: &Path) -> bool {
    match (Path::new(path).canonicalize(), root.canonicalize()) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

/// Removes files a previous run left in `subdir`.
///
/// Safe only at startup: the fresh Supervisor registry is empty and every item re-registers and
/// respools. The runtime directory outlives the process, so killed runs otherwise leave icons until
/// session end; files from the previous day were still present when this was written.
///
/// ponytail: a second Supervisor sweeps the first's live files, showing blank icons until respool.
/// Upgrade with a lockfile or mtime cutoff if two shells become a mode, avoiding a kilobyte leak.
pub fn sweep(subdir: &str) {
    let dir = icon_dir(subdir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Writes encoded `png_bytes` to `$XDG_RUNTIME_DIR/obelisk/{subdir}/{filename}`, creating missing
/// directories. Overwrites the same path without cache-busting (ADR-0031, ADR-0033).
pub fn write_png(subdir: &str, filename: &str, png_bytes: &[u8]) -> std::io::Result<String> {
    let dir = icon_dir(subdir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    std::fs::write(&path, png_bytes)?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn the_spool_directory_is_the_private_runtime_one_and_never_a_shared_one() {
        assert_eq!(spool_dir(Some(OsStr::new("/run/user/4242")), "tray"), PathBuf::from("/run/user/4242/obelisk/tray"));

        let fallback = spool_dir(None, "notifications");
        assert!(
            fallback.starts_with("/run/user/"),
            "an unset variable must not drop the spool into a world-writable directory: {}",
            fallback.display()
        );
    }

    #[test]
    fn a_path_outside_the_spool_is_not_deleted() {
        remove_png("tray", "/etc/passwd");
        assert!(std::path::Path::new("/etc/passwd").exists(), "remove_png must refuse a path it did not write");
    }

    #[test]
    fn a_real_file_under_the_spool_is_within_it() {
        let spool_root = tempfile::tempdir().unwrap();
        let file = spool_root.path().join("notif-1.png");
        std::fs::write(&file, b"fake png bytes").unwrap();

        assert!(is_within(file.to_str().unwrap(), spool_root.path()));
    }

    #[test]
    fn a_nonexistent_or_dot_dot_path_is_not_within_the_spool() {
        let spool_root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let external_file = elsewhere.path().join("theme-icon.png");
        std::fs::write(&external_file, b"a real, externally-owned icon").unwrap();
        let dot_dot = spool_root.path().join("..").join(elsewhere.path().file_name().unwrap()).join("theme-icon.png");

        assert!(!is_within(spool_root.path().join("never-written.png").to_str().unwrap(), spool_root.path()));
        assert!(!is_within(dot_dot.to_str().unwrap(), spool_root.path()));
    }

    #[test]
    fn a_symlink_escaping_the_spool_is_not_within_it() {
        let spool_root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let external_file = elsewhere.path().join("real.png");
        std::fs::write(&external_file, b"x").unwrap();

        let symlink_path = spool_root.path().join("escape.png");
        std::os::unix::fs::symlink(&external_file, &symlink_path).unwrap();

        assert!(!is_within(symlink_path.to_str().unwrap(), spool_root.path()));
    }
}

//! PNG spooling shared by tray and notifications.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use shared::debug;

/// This Supervisor's instance directory, set once before any capability starts.
///
/// ponytail: a process global rather than a parameter through notifications and tray; pass it down
/// if one process ever hosts two Supervisors.
pub(crate) static INSTANCE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// This shell's `{subdir}` under [`INSTANCE_DIR`] (ADR-0142, ADR-0222).
fn icon_dir(subdir: &str) -> std::io::Result<PathBuf> {
    INSTANCE_DIR.get().map(|dir| dir.join(subdir)).ok_or_else(|| std::io::Error::other("no instance directory"))
}

/// Best-effort deletion of one spooled PNG.
///
/// Checks that `path` is one this module wrote before deleting it. The path traveled through a
/// snapshot and back, so the check is a trust boundary.
pub fn remove_png(subdir: &str, path: &str) {
    if !icon_dir(subdir).is_ok_and(|dir| is_within(path, &dir)) {
        return;
    }
    if let Err(err) = std::fs::remove_file(path) {
        debug!("{subdir}: failed to delete spooled icon {path:?}: {err}");
    }
}

/// Canonicalizes both sides, so neither `..` nor a symlink escapes `root`.
pub(crate) fn is_within(path: impl AsRef<Path>, root: &Path) -> bool {
    match (path.as_ref().canonicalize(), root.canonicalize()) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

/// Encodes 8-bit `data` of `color` as PNG via `png` (ADR-0031: pure Rust, encode-only, minimal
/// dependency tree).
pub(crate) fn encode_png(
    width: u32,
    height: u32,
    color: png::ColorType,
    data: &[u8],
) -> Result<Vec<u8>, png::EncodingError> {
    let mut buffer = Vec::new();
    let mut encoder = png::Encoder::new(&mut buffer, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(data)?;
    Ok(buffer)
}

/// Writes encoded `png_bytes` to [`icon_dir`]`/{filename}`, creating missing directories. Overwrites
/// the same path without cache-busting (ADR-0031, ADR-0033).
pub fn write_png(subdir: &str, filename: &str, png_bytes: &[u8]) -> std::io::Result<String> {
    let dir = icon_dir(subdir)?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    std::fs::write(&path, png_bytes)?;
    debug!(2; "{subdir}: wrote {filename}");
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_file_under_the_spool_is_within_it() {
        let spool_root = tempfile::tempdir().unwrap();
        let file = spool_root.path().join("notif-1.png");
        std::fs::write(&file, b"fake png bytes").unwrap();
        assert!(is_within(file.to_str().unwrap(), spool_root.path()), "a real file under the spool");

        // A nonexistent path and a `..` path to a real external file are both outside.
        let elsewhere = tempfile::tempdir().unwrap();
        let external_file = elsewhere.path().join("theme-icon.png");
        std::fs::write(&external_file, b"a real, externally-owned icon").unwrap();
        let dot_dot = spool_root.path().join("..").join(elsewhere.path().file_name().unwrap()).join("theme-icon.png");
        assert!(
            !is_within(spool_root.path().join("never-written.png").to_str().unwrap(), spool_root.path()),
            "nonexistent"
        );
        assert!(!is_within(dot_dot.to_str().unwrap(), spool_root.path()), "dot-dot");

        // A symlink escaping the spool.
        let symlink_path = spool_root.path().join("escape.png");
        std::os::unix::fs::symlink(&external_file, &symlink_path).unwrap();
        assert!(!is_within(symlink_path.to_str().unwrap(), spool_root.path()), "escaping symlink");
    }
}

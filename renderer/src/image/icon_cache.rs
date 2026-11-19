//! GTK's `icon-theme.cache`: the subdirectories of one theme copy that hold a name, read in a few
//! positional reads instead of a `stat` per listed directory.
//!
//! Big-endian, as `gtk-update-icon-cache` writes it: a header of `u16 major, u16 minor, u32 hash,
//! u32 directories`; the hash is `u32 buckets` and a chain head per bucket; a chain entry is
//! `u32 next, u32 name, u32 images`; an image list is `u32 count` then `u16 directory, u16 flags,
//! u32 data` each; the directory list is `u32 count` then a string offset each.
//!
//! Any process of this user can write `~/.icons`, so each offset is checked by the read that uses
//! it, nothing is mapped (a truncated mapping is `SIGBUS`), and every loop has a constant bound.

use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

/// Chain entries per bucket and images per name: the largest installed caches have 11 and 22.
// ponytail: past 256 the cache reads as malformed and the theme is walked; raise it if a real cache does.
const MAX_STEPS: u32 = 256;
/// The longest directory name read; a longer one makes the cache unusable.
// ponytail: 1024 bytes, then the walk, as above.
const MAX_DIR_NAME: usize = 1024;

/// The subdirectories `theme_dir`'s cache lists for `name`, empty if none; `None` when the cache is
/// absent, not version 1.0, malformed, or older than the directory in whole seconds, as GTK compares.
/// Every short read or out-of-range index is `None`, so the caller walks instead of hiding an icon.
pub(super) fn dirs_with(theme_dir: &Path, name: &str) -> Option<Vec<String>> {
    let path = theme_dir.join("icon-theme.cache");
    super::decode::refuse_irregular(&path).ok()?;
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.mtime() < std::fs::metadata(theme_dir).ok()?.mtime() {
        return None;
    }
    let u32_at = |at: u64| {
        let mut bytes = [0; 4];
        file.read_exact_at(&mut bytes, at).ok().map(|()| u32::from_be_bytes(bytes))
    };
    if u32_at(0)? != 0x0001_0000 {
        return None;
    }
    let (hash, dir_list) = (u64::from(u32_at(4)?), u64::from(u32_at(8)?));
    let bucket = u64::from(name_hash(name) % u32_at(hash).filter(|&buckets| buckets > 0)?);
    let mut entry = u32_at(hash + 4 + 4 * bucket)?;
    let mut stored = vec![0; name.len() + 1];
    for _ in 0..MAX_STEPS {
        if entry == u32::MAX {
            return Some(Vec::new());
        }
        let entry_at = u64::from(entry);
        let read = file.read_at(&mut stored, u64::from(u32_at(entry_at + 4)?)).ok()?;
        // Cut off by the end of the file before its NUL: malformed, not another name.
        if read < stored.len() && !stored[..read].contains(&0) {
            return None;
        }
        if stored[..read].strip_suffix(&[0]) != Some(name.as_bytes()) {
            entry = u32_at(entry_at)?;
            continue;
        }
        let (images, dirs) = (u64::from(u32_at(entry_at + 8)?), u32_at(dir_list)?);
        return (0..u64::from(u32_at(images).filter(|&count| count <= MAX_STEPS)?))
            .map(|image| {
                let index = u32_at(images + 4 + 8 * image)? >> 16;
                let name_at = u32_at(dir_list + 4 + 4 * u64::from(index)).filter(|_| index < dirs)?;
                let mut name = [0; MAX_DIR_NAME];
                let read = file.read_at(&mut name, u64::from(name_at)).ok()?;
                let end = name[..read].iter().position(|&byte| byte == 0)?;
                String::from_utf8(name[..end].to_vec()).ok()
            })
            .collect();
    }
    None
}

/// GTK's `icon_name_hash`: `h * 31 + c` over the name's bytes as signed chars.
fn name_hash(name: &str) -> u32 {
    let mut bytes = name.bytes().map(|byte| byte as i8 as u32);
    let first = bytes.next().unwrap_or(0);
    bytes.fold(first, |hash, byte| (hash << 5).wrapping_sub(hash).wrapping_add(byte))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// `gtk-update-icon-cache` over `app-icon.png` in `16x16/apps` and `32x32/apps`, and `b.svg` in `32x32/apps`.
    pub(in crate::image) const CACHE: &[u8] = include_bytes!("../../fixtures/icons/icon-theme.cache");

    #[test]
    fn a_truncated_corrupt_or_cyclic_cache_falls_back_to_the_walk_without_a_panic_or_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let lookup = |bytes: &[u8], name: &str| {
            std::fs::write(dir.path().join("icon-theme.cache"), bytes).unwrap();
            dirs_with(dir.path(), name)
        };
        let full = Some(vec!["32x32/apps".to_string(), "16x16/apps".to_string()]);
        assert_eq!(lookup(CACHE, "app-icon"), full);
        // "h" shares "app-icon"'s bucket, so it walks the chain from that entry, at 0x3c, to its end or round a cycle.
        assert_eq!(lookup(CACHE, "h"), Some(Vec::new()));
        // A malformed cache answers `None`, so the caller walks; fewer directories would hide an icon.
        for len in 0..CACHE.len() {
            let found = lookup(&CACHE[..len], "app-icon");
            assert!(found.is_none() || found == full, "cut at {len}: {found:?}");
        }
        let corrupt = |words: &[(usize, u32)]| {
            let mut bytes = CACHE.to_vec();
            words.iter().for_each(|&(at, word)| bytes[at..at + 4].copy_from_slice(&word.to_be_bytes()));
            bytes
        };
        assert_eq!(lookup(&corrupt(&[(0x0c, 0)]), "app-icon"), None, "no buckets");
        assert_eq!(lookup(&corrupt(&[(0x40, CACHE.len() as u32)]), "app-icon"), None, "a name past the end");
        assert_eq!(lookup(&corrupt(&[(0x84, 1)]), "app-icon"), None, "a directory past the table");
        // The second image, at 0x60, names directory 2 of 2: past the table, read from the right place.
        assert_eq!(lookup(&corrupt(&[(0x60, 2 << 16)]), "app-icon"), None, "the second image's directory");
        assert_eq!(lookup(&corrupt(&[(0x3c, 0x68), (0x68, 0x3c)]), "h"), None, "two entries that point at each other");
        // An image list past the cap, at the end of the file.
        let mut long = corrupt(&[(0x44, CACHE.len() as u32)]);
        long.extend(257_u32.to_be_bytes().into_iter().chain([0; 257 * 8]));
        assert_eq!(lookup(&long, "app-icon"), None);
    }
}

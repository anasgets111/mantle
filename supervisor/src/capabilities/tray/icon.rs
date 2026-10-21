//! Icon pipeline: `IconName`-vs-`IconPixmap` resolution, bounds-checking, and ARGB32-to-PNG
//! encoding/spooling (ADR-0031: prefer IconName, decode IconPixmap only as fallback).

use crate::capabilities::shm_icons;

use super::MAX_PIXMAP_DIMENSION;

/// Instance-dir subdirectory for tray pixmaps, shared by writes and per-item deletion.
pub(super) const SPOOL_SUBDIR: &str = "tray";

#[derive(Debug, Clone, PartialEq, Hash)]
pub(super) struct IconPixmap {
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) bytes: Vec<u8>,
}

/// Validates one raw `IconPixmap`: non-empty square, at most [`MAX_PIXMAP_DIMENSION`], with exactly
/// `width * height * 4` ARGB32 bytes.
fn pixmap_is_valid(width: i32, height: i32, byte_len: usize) -> bool {
    width > 0
        && width == height
        && width <= MAX_PIXMAP_DIMENSION
        && (width as usize) * (height as usize) * 4 == byte_len
}

/// Largest valid pixmap (ADR-0031: no target-size guess, cap at 128; downscaling beats upscaling).
pub(super) fn largest_valid_pixmap(pixmaps: &[IconPixmap]) -> Option<&IconPixmap> {
    pixmaps
        .iter()
        .filter(|pixmap| pixmap_is_valid(pixmap.width, pixmap.height, pixmap.bytes.len()))
        .max_by_key(|pixmap| pixmap.width)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IconSource {
    /// `IconName` resolved inside the item's `IconThemePath` (ADR-0074), ahead of [`Self::Name`].
    ThemePathFile(String),
    /// Non-empty `IconName`, preferred over pixmaps (ADR-0031), so no decode or PNG spool occurs.
    Name(String),
    /// Empty `IconName` with at least one valid pixmap.
    Pixmap,
    /// Neither source is usable.
    None,
}

/// The longest path this can usefully build. `theme_path` is a raw string from the application and
/// nothing in SNI bounds it; past `PATH_MAX` no `join` can name a file, so building and stat-ing
/// the candidates only allocates the oversized string three more times to be told `ENAMETOOLONG`.
const MAX_THEME_PATH_BYTES: usize = 4096;

/// Finds `icon_name` in the item's `IconThemePath`, trying the bare name, `.png`, then `.svg`.
/// The spec says only "a directory of icons" and leaves layout to the application. Rejects `/`
/// and `\`; a name containing `/` or `..` could reach outside it.
pub(super) fn theme_path_file(theme_path: &str, icon_name: &str) -> Option<String> {
    if theme_path.is_empty()
        || theme_path.len() > MAX_THEME_PATH_BYTES
        || icon_name.is_empty()
        || icon_name.contains('/')
        || icon_name.contains('\\')
    {
        return None;
    }
    let dir = std::path::Path::new(theme_path);
    ["", ".png", ".svg"]
        .iter()
        .map(|ext| dir.join(format!("{icon_name}{ext}")))
        .find(|candidate| candidate.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Chooses the source and whether decoding is needed (ADR-0031). This stays a cheap, pure decision;
/// the caller re-derives the largest pixmap only for [`IconSource::Pixmap`].
pub(super) fn resolve_icon_source(icon_name: &str, pixmaps: &[IconPixmap], theme_path: &str) -> IconSource {
    if let Some(path) = theme_path_file(theme_path, icon_name) {
        IconSource::ThemePathFile(path)
    } else if !icon_name.is_empty() {
        IconSource::Name(icon_name.to_string())
    } else if largest_valid_pixmap(pixmaps).is_some() {
        IconSource::Pixmap
    } else {
        IconSource::None
    }
}

/// Encodes bounds-checked ARGB32 bytes (network order A, R, G, B) to PNG.
fn encode_argb32_to_png(width: u32, height: u32, argb: &[u8]) -> Result<Vec<u8>, png::EncodingError> {
    let rgba: Vec<u8> = argb.as_chunks::<4>().0.iter().flat_map(|&[a, r, g, b]| [r, g, b, a]).collect();
    shm_icons::encode_png(width, height, png::ColorType::Rgba, &rgba)
}

/// An item id as a flat filename stem: [`super::registration::item_id`] ends in an object path and
/// the spool is one directory. `_` doubles first, so no two ids fold onto one file.
pub(super) fn icon_filename_stem(id: &str) -> String {
    id.replace('_', "__").replace('/', "_")
}

/// Writes a validated pixmap to `tray/{filename_stem}.png` in the instance dir
/// ([`shm_icons::write_png`]), creating the tree and overwriting the same path (no cache-busting,
/// ADR-0031). Base, attention, and overlay stems differ so their files do not collide (ADR-0074).
pub(super) fn write_icon_png(filename_stem: &str, pixmap: &IconPixmap) -> std::io::Result<String> {
    let png_bytes = encode_argb32_to_png(pixmap.width as u32, pixmap.height as u32, &pixmap.bytes)
        .map_err(std::io::Error::other)?;
    shm_icons::write_png(SPOOL_SUBDIR, &format!("{filename_stem}.png"), &png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spool is flat, so an id's path separators fold into the stem; two ids must not fold
    /// onto one file and serve each other's artwork.
    #[test]
    fn no_two_item_ids_share_an_icon_filename() {
        assert_eq!(icon_filename_stem("1.42/StatusNotifierItem"), "1.42_StatusNotifierItem");
        assert_ne!(icon_filename_stem("1.42/a/b"), icon_filename_stem("1.42/a_b"));
    }

    #[test]
    fn theme_path_file_resolves_an_items_own_artwork_by_name_and_extension_and_never_leaves_the_directory() {
        // Prefer artwork the session theme does not know.
        let dir = tempfile::tempdir().unwrap();
        let theme_path = dir.path().to_str().unwrap();
        assert_eq!(
            resolve_icon_source("my-app", &[], theme_path),
            IconSource::Name("my-app".to_string()),
            "a theme path that does not hold the icon falls back to the name"
        );
        assert_eq!(theme_path_file(theme_path, "my-app"), None);

        std::fs::write(dir.path().join("my-app.png"), b"not really a png").unwrap();
        let found = theme_path_file(theme_path, "my-app").expect("the shipped file");
        assert!(found.ends_with("my-app.png"));
        assert_eq!(
            resolve_icon_source("my-app", &[], theme_path),
            IconSource::ThemePathFile(found),
            "an item's own theme path outranks the name"
        );

        std::fs::write(dir.path().join("vector.svg"), b"<svg/>").unwrap();
        std::fs::write(dir.path().join("literal.ico"), b"x").unwrap();
        assert!(theme_path_file(theme_path, "vector").unwrap().ends_with("vector.svg"), "an svg is found");
        assert!(
            theme_path_file(theme_path, "literal.ico").unwrap().ends_with("literal.ico"),
            "so is a name that carries its own extension"
        );

        // A themed name has no separator; refuse one rather than clean it up.
        std::fs::write(dir.path().join("real.png"), b"x").unwrap();
        assert_eq!(theme_path_file(theme_path, "../../etc/passwd"), None);
        assert_eq!(theme_path_file(theme_path, "sub/real"), None);
    }

    #[test]
    fn pixmap_is_valid_wants_a_positive_square_within_the_cap_and_a_matching_byte_length() {
        // (width, height, bytes, valid)
        for (width, height, len, valid) in [
            (2, 2, 2 * 2 * 4, true),
            (128, 128, 128 * 128 * 4, true),
            (4, 2, 4 * 2 * 4, false),
            (129, 129, 129 * 129 * 4, false),
            (2, 2, 10, false),
            (0, 0, 0, false),
            (-1, -1, 4, false),
        ] {
            assert_eq!(pixmap_is_valid(width, height, len), valid, "{width}x{height} with {len} bytes");
        }
    }

    fn pixmap(size: i32) -> IconPixmap {
        IconPixmap { width: size, height: size, bytes: vec![0u8; (size * size * 4) as usize] }
    }

    #[test]
    fn largest_valid_pixmap_picks_the_biggest_valid_entry() {
        let pixmaps = vec![pixmap(16), pixmap(64), pixmap(32)];
        assert_eq!(largest_valid_pixmap(&pixmaps), Some(&pixmaps[1]));

        let oversized = IconPixmap { width: 200, height: 200, bytes: vec![0u8; 200 * 200 * 4] };
        let pixmaps = vec![oversized, pixmap(16)];
        assert_eq!(largest_valid_pixmap(&pixmaps), Some(&pixmaps[1]));

        let non_square = vec![IconPixmap { width: 4, height: 2, bytes: vec![0u8; 32] }];
        assert_eq!(largest_valid_pixmap(&non_square), None);
        assert_eq!(largest_valid_pixmap(&[]), None);
    }

    #[test]
    fn resolve_icon_source_prefers_the_name_then_the_pixmap_and_else_none() {
        let pixmaps = vec![pixmap(32)];
        assert_eq!(resolve_icon_source("battery-full", &pixmaps, ""), IconSource::Name("battery-full".to_string()));
        // Empty theme paths must not shadow Chromium's pixmap branch.
        assert_eq!(resolve_icon_source("", &pixmaps, ""), IconSource::Pixmap);
        assert_eq!(resolve_icon_source("", &[], ""), IconSource::None);
        let invalid = vec![IconPixmap { width: 4, height: 2, bytes: vec![0u8; 32] }];
        assert_eq!(resolve_icon_source("", &invalid, ""), IconSource::None);
    }

    #[test]
    fn encode_argb32_to_png_round_trips_a_known_pixel() {
        // One 1x1 SNI pixel: A=0x11, R=0x22, G=0x33, B=0x44.
        let argb = vec![0x11, 0x22, 0x33, 0x44];
        let png_bytes = encode_argb32_to_png(1, 1, &argb).expect("encoding must succeed");

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("valid PNG frame");
        let rgba = &buf[..info.buffer_size()];

        // PNG stores R, G, B, A, so the encoder must reorder ARGB.
        assert_eq!(rgba, &[0x22, 0x33, 0x44, 0x11]);
    }

    /// `IconThemePath` is a raw application string with no bound in SNI. Past `PATH_MAX` it cannot
    /// name a file, so it is refused before three candidate paths are built from it.
    #[test]
    fn a_theme_path_longer_than_path_max_is_refused_without_touching_the_filesystem() {
        assert_eq!(theme_path_file(&"x".repeat(MAX_THEME_PATH_BYTES + 1), "my-app"), None);
    }
}

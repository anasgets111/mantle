//! Path-trust validator for `<img src>`, `image-path`, action-icon paths, and registered sound
//! files; validates, encodes and spools the decoded image-data hints, and safely deletes spooled
//! icons. `Hints` in the parent module is what decodes them off the wire.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use zbus::zvariant::Type;

use crate::capabilities::shm_icons;

use super::markup::parse_markup;
use super::{MAX_APP_ICON_NAME_BYTES, MAX_BODY_BYTES, MAX_IMAGE_DIMENSION, NotificationSpan};
use crate::capabilities::file_url::file_url_to_path;
use crate::capabilities::truncate_utf8_bytes;

/// ADR-0033's trusted icon roots, with `$XDG_DATA_HOME` and `$HOME` resolved at runtime. Tests
/// inject fixture roots.
pub(super) fn default_trusted_icon_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/usr/share/icons"), PathBuf::from("/usr/share/pixmaps")];
    roots.extend(shared::xdg_dir("XDG_DATA_HOME", ".local/share").map(|dir| dir.join("icons")));
    roots.extend(std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".icons")));
    roots
}

/// Accepts only an existing regular file under a trusted absolute root (ADR-0033). Canonicalizing
/// both sides rejects `..` traversal and symlinks escaping the root. Relative paths and bare theme
/// names degrade to no icon.
pub(super) fn validate_trusted_path(path: impl AsRef<Path>, trusted_roots: &[PathBuf]) -> Option<PathBuf> {
    let candidate = path.as_ref();
    if !candidate.is_absolute() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    // Every caller renders the path as a string; refuse one that would be lossy-converted.
    canonical.to_str()?;
    if !canonical.is_file() {
        return None;
    }
    trusted_roots.iter().any(|root| shm_icons::is_within(&canonical, root)).then_some(canonical)
}

/// `image-path`/`app_icon` hint as a path: a `file://` URI is decoded, anything else is kept as is.
pub(super) fn hint_path(hint: &str) -> PathBuf {
    file_url_to_path(hint).unwrap_or_else(|| PathBuf::from(hint))
}

/// Truncates and parses `Notify` body markup, then validates every `<img src>` path. Untrusted
/// images are dropped (ADR-0033 closes arbitrary local-file disclosure through body markup).
pub(super) fn sanitize_body(raw_body: &str, trusted_roots: &[PathBuf]) -> Vec<NotificationSpan> {
    let truncated = truncate_utf8_bytes(raw_body, MAX_BODY_BYTES);
    parse_markup(&truncated)
        .into_iter()
        .filter_map(|span| match span {
            NotificationSpan::Image { image_path } => validate_trusted_path(&image_path, trusted_roots)
                .map(|validated| NotificationSpan::Image { image_path: validated.to_string_lossy().into_owned() }),
            text_span => Some(text_span),
        })
        .collect()
}

// Freedesktop `image-data`/`icon_data` is `(iiibiiay)`, unlike tray's square-only ARGB32
// `IconPixmap`. Deriving the decode keeps `data` one byte per byte; see `Hints`.

#[derive(Debug, Clone, PartialEq, Deserialize, Type)]
pub(super) struct RawImageData {
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
    bits_per_sample: i32,
    channels: i32,
    data: Vec<u8>,
}

/// Validates image-data bounds and the "ARGB icon rejection": positive dimensions up to
/// [`MAX_IMAGE_DIMENSION`], 8-bit samples only, channels matching alpha (3=RGB, 4=RGBA), no row
/// padding, and exact data length. ponytail: no 16-bit/float support until a real sender needs it.
pub(super) fn image_data_is_valid(image: &RawImageData) -> bool {
    image.width > 0
        && image.height > 0
        && image.width <= MAX_IMAGE_DIMENSION
        && image.height <= MAX_IMAGE_DIMENSION
        && image.bits_per_sample == 8
        && image.channels == if image.has_alpha { 4 } else { 3 }
        && image.rowstride == image.width * image.channels
        && image.data.len() == (image.rowstride as usize) * (image.height as usize)
}

/// Encodes checked [`RawImageData`] to PNG. Unlike `tray`'s `encode_argb32_to_png`, no
/// channel reorder is needed: freedesktop data is RGB(A) row-major, not ARGB network-byte-order
/// pixmaps.
pub(super) fn encode_image_data_to_png(image: &RawImageData) -> Result<Vec<u8>, png::EncodingError> {
    let color = if image.has_alpha { png::ColorType::Rgba } else { png::ColorType::Rgb };
    shm_icons::encode_png(image.width as u32, image.height as u32, color, &image.data)
}

pub(super) fn write_icon_png(id: u32, png_bytes: &[u8]) -> std::io::Result<String> {
    shm_icons::write_png("notifications", &format!("notif-{id}.png"), png_bytes)
}

/// `Notification.image_path` may be our spooled copy or a client's own `image-path`/`app_icon`
/// (for example under `/usr/share/icons`); only the first is ours to delete.
pub(super) fn delete_icon_file(path: &str) {
    shm_icons::remove_png("notifications", path);
}

/// Attached-picture precedence: `image-data`/`image_data` > `image-path`/`image_path` >
/// `icon_data`.
///
/// The three spellings accumulated across spec versions. [`resolve_app_icon`] owns the
/// application's icon, outside this picture chain (ADR-0091).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ImageInput {
    ImageData(RawImageData),
    ImagePath(String),
    IconData(RawImageData),
    None,
}

pub(super) fn resolve_image_input(
    image_data: Option<RawImageData>,
    image_path: Option<String>,
    icon_data: Option<RawImageData>,
) -> ImageInput {
    if let Some(data) = image_data {
        return ImageInput::ImageData(data);
    }
    if let Some(path) = image_path.filter(|p| !p.is_empty()) {
        return ImageInput::ImagePath(path);
    }
    if let Some(data) = icon_data {
        return ImageInput::IconData(data);
    }
    ImageInput::None
}

/// Splits `image-path`/`image_path` into `(picture, theme name)` (ADR-0096).
///
/// A `file://` URI or a freedesktop theme name is allowed; `file://` is the only URI schema
/// supported right now. Paths use [`validate_trusted_path`]; names have no path to validate.
/// Leaving names in the picture chain caused ADR-0091's `app_icon` bug, so they move to the
/// application-icon path.
///
/// Split on `/`: using `is_absolute` would misclassify `"../../etc/passwd"` as a theme name and
/// pass it to renderer lookup.
pub(super) fn split_image_path_hint(hint: Option<String>) -> (Option<String>, Option<String>) {
    match hint.filter(|hint| !hint.is_empty()) {
        Some(hint) if !hint_path(&hint).to_string_lossy().contains('/') => (None, Some(hint)),
        hint => (hint, None),
    }
}

/// Resolves `Notify`'s positional `app_icon` into something a config can draw (ADR-0091).
///
/// Theme names (`"firefox"`, `"org.telegram.desktop"`) pass through for renderer resolution
/// (`icon { name = ... }`, ADR-0054 decision 2); absolute paths and `file://` URIs use the trusted
/// root check.
///
/// A separator check, not `is_absolute`, keeps `../../etc/passwd` from becoming a theme name.
pub(super) fn resolve_app_icon(app_icon: Option<String>, trusted_roots: &[PathBuf]) -> Option<String> {
    let app_icon = app_icon.filter(|icon| !icon.is_empty())?;
    let path = hint_path(&app_icon);
    let stripped = path.to_string_lossy();
    if !stripped.contains('/') {
        return Some(truncate_utf8_bytes(&stripped, MAX_APP_ICON_NAME_BYTES));
    }
    validate_trusted_path(path, trusted_roots).map(|path| path.to_string_lossy().into_owned())
}

#[cfg(test)]
pub(super) fn valid_rgba_image(width: i32, height: i32) -> RawImageData {
    let channels = 4;
    let rowstride = width * channels;
    RawImageData {
        width,
        height,
        rowstride,
        has_alpha: true,
        bits_per_sample: 8,
        channels,
        data: vec![0u8; (rowstride * height) as usize],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_trusted_path_accepts_a_real_file_under_a_trusted_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("icon.png");
        std::fs::write(&file, b"fake png bytes").unwrap();

        let result = validate_trusted_path(file.to_str().unwrap(), &[dir.path().to_path_buf()]);
        assert_eq!(result, Some(file.canonicalize().unwrap()));
    }

    #[test]
    fn validate_trusted_path_rejects_a_relative_path_a_bare_name_a_missing_file_and_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let roots = [dir.path().to_path_buf()];
        let missing = dir.path().join("missing.png");
        for path in ["relative/icon.png", "battery-full", missing.to_str().unwrap(), dir.path().to_str().unwrap()] {
            assert_eq!(validate_trusted_path(path, &roots), None, "{path}");
        }
    }

    #[test]
    fn validate_trusted_path_rejects_an_outside_path_a_traversal_and_a_symlink_escape() {
        let trusted_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let roots = [trusted_dir.path().to_path_buf()];
        let outside_file = outside_dir.path().join("icon.png");
        std::fs::write(&outside_file, b"x").unwrap();
        assert_eq!(validate_trusted_path(outside_file.to_str().unwrap(), &roots), None, "outside every root");

        // Build traversal through the trusted dir's parent instead of assuming temp-dir layout.
        let escape_path = trusted_dir.path().join("..").join(outside_dir.path().file_name().unwrap()).join("icon.png");
        assert_eq!(validate_trusted_path(escape_path.to_str().unwrap(), &roots), None, "directory traversal");

        let symlink_path = trusted_dir.path().join("escape.png");
        std::os::unix::fs::symlink(&outside_file, &symlink_path).unwrap();
        assert_eq!(validate_trusted_path(symlink_path.to_str().unwrap(), &roots), None, "symlink escape");
    }

    #[test]
    fn hint_path_decodes_a_file_uri_and_keeps_anything_else() {
        assert_eq!(hint_path("file:///usr/share/icons/a%20b.png"), PathBuf::from("/usr/share/icons/a b.png"));
        assert_eq!(hint_path("/usr/share/icons/x.png"), PathBuf::from("/usr/share/icons/x.png"));
        assert_eq!(hint_path("battery-full"), PathBuf::from("battery-full"));
    }

    #[test]
    fn a_percent_encoded_file_uri_resolves_under_a_trusted_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a b.png");
        std::fs::write(&file, b"x").unwrap();
        let uri = format!("file://{}/a%20b.png", dir.path().display());
        let roots = [dir.path().to_path_buf()];

        assert_eq!(
            resolve_app_icon(Some(uri.clone()), &roots),
            Some(file.canonicalize().unwrap().to_string_lossy().into_owned())
        );
        assert_eq!(validate_trusted_path(hint_path(&uri), &roots), Some(file.canonicalize().unwrap()));
    }

    #[test]
    fn a_file_uri_that_escapes_or_decodes_badly_gets_no_icon() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.png"), b"x").unwrap();
        std::fs::write(dir.path().join("ok.png"), b"x").unwrap();
        let roots = [dir.path().to_path_buf()];
        let root = dir.path().display();
        let traversal =
            format!("file://{root}/%2e%2e/{}/outside.png", outside.path().file_name().unwrap().to_str().unwrap());
        let bad_name = <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b"\xff.png".to_vec());
        std::fs::write(dir.path().join(bad_name), b"x").unwrap();
        let non_utf8 = format!("file://{root}/%ff.png");
        std::os::unix::fs::symlink(outside.path().join("outside.png"), dir.path().join("link.png")).unwrap();
        for uri in [traversal, format!("file://{root}/ok.png%00"), non_utf8, format!("file://{root}/link.png")] {
            assert_eq!(validate_trusted_path(hint_path(&uri), &roots), None, "{uri}");
        }
    }

    #[test]
    fn image_data_is_valid_rejects_malformed_images() {
        let mut no_alpha = valid_rgba_image(4, 4);
        no_alpha.has_alpha = false; // channels=4 mismatches
        let mut bad_rowstride = valid_rgba_image(4, 4);
        bad_rowstride.rowstride += 4;
        let mut short_data = valid_rgba_image(4, 4);
        short_data.data.pop();
        let mut sixteen_bit = valid_rgba_image(4, 4);
        sixteen_bit.bits_per_sample = 16;
        for (case, image) in [
            ("oversized", valid_rgba_image(MAX_IMAGE_DIMENSION + 1, 4)),
            ("zero width", valid_rgba_image(0, 4)),
            ("zero height", valid_rgba_image(4, 0)),
            ("channels vs has_alpha", no_alpha),
            ("rowstride", bad_rowstride),
            ("data length", short_data),
            ("non-8-bit sample", sixteen_bit),
        ] {
            assert!(!image_data_is_valid(&image), "{case}");
        }
        assert!(image_data_is_valid(&valid_rgba_image(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION)));
    }

    #[test]
    fn encode_image_data_to_png_round_trips_a_known_pixel() {
        // One RGBA row-major pixel, unlike tray's ARGB network-byte-order pixmaps.
        let image = RawImageData {
            width: 1,
            height: 1,
            rowstride: 4,
            has_alpha: true,
            bits_per_sample: 8,
            channels: 4,
            data: vec![0x11, 0x22, 0x33, 0x44],
        };
        let png_bytes = encode_image_data_to_png(&image).expect("encoding must succeed");

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("valid PNG frame");
        assert_eq!(&buf[..info.buffer_size()], &[0x11, 0x22, 0x33, 0x44]);
    }

    fn tiny_image() -> RawImageData {
        valid_rgba_image(1, 1)
    }

    #[test]
    fn split_image_path_hint_sends_names_to_the_icon_and_paths_to_the_picture() {
        let hint = |text: &str| split_image_path_hint(Some(text.to_string()));
        // The `notify-send -i firefox` case, which is how the overwhelming majority of notifications
        // on a desktop name their icon: the positional argument is empty and the theme name rides in
        // the hint that otherwise means a picture.
        assert_eq!(
            hint("firefox"),
            (None, Some("firefox".to_string())),
            "a bare name goes to `app_icon`, not into the picture chain that would validate it away"
        );
        assert_eq!(hint("/tmp/art.png"), (Some("/tmp/art.png".to_string()), None), "a path is still a picture");
        assert_eq!(
            hint("file:///tmp/art.png"),
            (Some("file:///tmp/art.png".to_string()), None),
            "the URI is stripped when the picture is resolved, not here"
        );
        // A relative path is neither, and must not be passed off as a theme name for the renderer's
        // icon lookup to open -- `resolve_app_icon` refuses it on the same grounds.
        assert_eq!(
            hint("../../etc/passwd"),
            (Some("../../etc/passwd".to_string()), None),
            "a relative path stays in the picture chain to be refused"
        );
        assert_eq!(split_image_path_hint(None), (None, None), "absent");
        assert_eq!(hint(""), (None, None), "empty");
    }

    #[test]
    fn resolve_image_input_prefers_data_then_path_then_icon_data_and_treats_an_empty_path_as_absent() {
        let path = || Some("/path".to_string());
        assert_eq!(
            resolve_image_input(Some(tiny_image()), path(), Some(tiny_image())),
            ImageInput::ImageData(tiny_image())
        );
        assert_eq!(resolve_image_input(None, path(), Some(tiny_image())), ImageInput::ImagePath("/path".to_string()));
        assert_eq!(resolve_image_input(None, None, Some(tiny_image())), ImageInput::IconData(tiny_image()));
        assert_eq!(resolve_image_input(None, None, None), ImageInput::None);
        assert_eq!(resolve_image_input(None, Some(String::new()), None), ImageInput::None);
    }

    /// The case that was silently broken: a bare theme name is what the base spec asks senders for
    /// and what nearly all of them send, and it used to resolve to nothing.
    #[test]
    fn resolve_app_icon_carries_a_theme_name_through_untouched() {
        assert_eq!(resolve_app_icon(Some("firefox".to_string()), &[]), Some("firefox".to_string()));
        assert_eq!(
            resolve_app_icon(Some("org.telegram.desktop".to_string()), &[]),
            Some("org.telegram.desktop".to_string())
        );
    }

    #[test]
    fn resolve_app_icon_refuses_absent_empty_and_relative_icons() {
        // A relative path is neither a theme name nor a validatable path.
        for icon in [None, Some(String::new()), Some("../../etc/passwd".to_string()), Some("icons/app.png".to_string())]
        {
            assert_eq!(resolve_app_icon(icon.clone(), &[]), None, "{icon:?}");
        }
    }

    /// A path still has to earn it, by the same rule every other client-supplied path follows.
    #[test]
    fn resolve_app_icon_validates_a_path_and_accepts_a_file_uri() {
        let root = tempfile::tempdir().expect("tempdir");
        let icon = root.path().join("app.png");
        std::fs::write(&icon, b"not really a png").expect("write");
        let roots = vec![root.path().to_path_buf()];

        let canonical = icon.canonicalize().expect("canonicalize").to_string_lossy().into_owned();
        assert_eq!(resolve_app_icon(Some(icon.to_string_lossy().into_owned()), &roots), Some(canonical.clone()));
        assert_eq!(resolve_app_icon(Some(format!("file://{}", icon.display())), &roots), Some(canonical));

        assert_eq!(resolve_app_icon(Some("/etc/passwd".to_string()), &roots), None, "outside every trusted root");
    }
}

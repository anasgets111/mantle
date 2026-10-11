//! `file://` URL decoding shared by the capabilities that receive icon and art paths.

use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// A `file://` URL as a filesystem path: percent-decoded, with RFC 8089's optional `localhost`
/// authority dropped.
///
/// Decoding is byte-wise because a path is bytes on Unix, not UTF-8: a filename the shell can
/// open is not necessarily one `String` accepts.
pub(crate) fn file_url_to_path(art_url: &str) -> Option<PathBuf> {
    let rest = art_url.strip_prefix("file://")?;
    let encoded = rest.strip_prefix("localhost").filter(|tail| tail.starts_with('/')).unwrap_or(rest);
    let raw = encoded.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let escape = (raw[index] == b'%' && index + 2 < raw.len())
            .then(|| Some(hex_digit(raw[index + 1])? * 16 + hex_digit(raw[index + 2])?))
            .flatten();
        match escape {
            Some(byte) => {
                bytes.push(byte);
                index += 3;
            }
            None => {
                bytes.push(raw[index]);
                index += 1;
            }
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_to_path_decodes_escapes_and_drops_a_localhost_authority() {
        // (url, expected path): `100%` in a filename is not an escape, and dropping it would rename the file.
        for (url, expected) in [
            ("file:///tmp/cover%20art.png", Some("/tmp/cover art.png")),
            ("file://localhost/tmp/a.png", Some("/tmp/a.png")),
            ("file:///home/someone/.cache/player/art_4.png", Some("/home/someone/.cache/player/art_4.png")),
            ("file:///tmp/100%.png", Some("/tmp/100%.png")),
            ("https://example.com/a.png", None),
        ] {
            assert_eq!(file_url_to_path(url), expected.map(PathBuf::from), "{url}");
        }
    }
}

//! Pure `Metadata` (`a{sv}`) parsing, album-art trust checks, and track identity comparison
//! (ADR-0036).

use zbus::zvariant::{OwnedValue, Value};

use super::{MAX_METADATA_TEXT_BYTES, MAX_METADATA_URL_BYTES};
use crate::capabilities::file_url::file_url_to_path;
use crate::capabilities::truncate_utf8_bytes;

fn value_as_str<'a>(value: &'a Value<'_>) -> Option<&'a str> {
    match value {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// `xesam:artist` is `as` on every checked player; accept a bare string too for malformed metadata.
fn value_as_str_or_joined_array(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.as_str().to_string()),
        Value::Array(array) => {
            let parts: Vec<&str> = array.iter().filter_map(value_as_str).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

fn value_as_i64(value: &Value<'_>) -> Option<i64> {
    match value {
        Value::I64(i) => Some(*i),
        Value::U32(u) => Some(i64::from(*u)),
        Value::I32(i) => Some(i64::from(*i)),
        _ => None,
    }
}

/// `mpris:trackid` is an object path (`o`) on checked players (Zen, mpv-mpris). A bare string
/// from a type-wrong player is accepted too.
fn value_as_trackid(value: &Value<'_>) -> Option<String> {
    match value {
        Value::ObjectPath(path) => Some(path.as_str().to_string()),
        _ => value_as_str(value).map(str::to_string),
    }
}

/// One `Metadata` dict reduced to the fields exposed by `mantle.mpris`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ParsedMetadata {
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) album: String,
    pub(super) album_artist: String,
    pub(super) genre: String,
    pub(super) art_url: Option<String>,
    pub(super) length_us: Option<i64>,
    pub(super) trackid: Option<String>,
    pub(super) url: Option<String>,
}

pub(super) fn parse_metadata(metadata: &std::collections::HashMap<String, OwnedValue>) -> ParsedMetadata {
    let get = |key: &str| metadata.get(key).map(|value| &**value);
    let text = |text: &str| truncate_utf8_bytes(text, MAX_METADATA_TEXT_BYTES);
    let joined = |key: &str| text(&get(key).and_then(value_as_str_or_joined_array).unwrap_or_default());
    let url = |key: &str| {
        get(key).and_then(value_as_str).filter(|url| url.len() <= MAX_METADATA_URL_BYTES).map(str::to_string)
    };
    ParsedMetadata {
        title: text(get("xesam:title").and_then(value_as_str).unwrap_or_default()),
        artist: joined("xesam:artist"),
        album: text(get("xesam:album").and_then(value_as_str).unwrap_or_default()),
        album_artist: joined("xesam:albumArtist"),
        genre: joined("xesam:genre"),
        art_url: url("mpris:artUrl"),
        length_us: get("mpris:length").and_then(value_as_i64),
        trackid: get("mpris:trackid").and_then(value_as_trackid),
        url: url("xesam:url"),
    }
}

/// [`ParsedMetadata::track_identity`]'s composite key (ADR-0036, CONTEXT.md "Track identity"):
/// require `trackid`/`url`/`title` all to match. Real players leave any one unchanged across a
/// genuine track change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TrackIdentity {
    trackid: Option<String>,
    url: Option<String>,
    title: String,
}

impl ParsedMetadata {
    pub(super) fn track_identity(&self) -> TrackIdentity {
        TrackIdentity { trackid: self.trackid.clone(), url: self.url.clone(), title: self.title.clone() }
    }
}

/// Resolves local `file://` artwork to an existing regular file (ADR-0036). No directory allowlist:
/// players use varied locations, confirmed by Zen's `~/.config/zen/...` cache; the player already
/// runs with the user's privileges. Remote URLs produce no path.
pub(super) fn resolve_album_art_path(art_url: Option<&str>) -> String {
    let Some(path) = art_url.and_then(file_url_to_path) else { return String::new() };
    let Ok(canonical) = path.canonicalize() else { return String::new() };
    if canonical.is_file() { canonical.to_string_lossy().into_owned() } else { String::new() }
}

/// Absolute target for `mpris:seek`/`seek_relative`, clamped to `[0, length]` before
/// `SetPosition`/`Seek` (ADR-0036). Without a cached length, only the lower bound applies.
pub(super) fn clamp_seek_target(target_us: i64, length_us: Option<i64>) -> i64 {
    let lower = target_us.max(0);
    length_us.map_or(lower, |length| lower.min(length))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{Array, ObjectPath, Signature, Str};

    use super::*;

    fn owned(value: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    fn real_metadata() -> HashMap<String, OwnedValue> {
        let mut artists = Array::new(&Signature::Str);
        artists.append(Value::Str(Str::from("Ahmed Ebrahim"))).unwrap();
        let mut map = HashMap::new();
        map.insert("xesam:title".to_string(), owned(Value::Str(Str::from("Track Title"))));
        map.insert("xesam:artist".to_string(), owned(Value::Array(artists)));
        map.insert("xesam:album".to_string(), owned(Value::Str(Str::from("Record"))));
        let mut album_artists = Array::new(&Signature::Str);
        album_artists.append(Value::Str(Str::from("Producer A"))).unwrap();
        album_artists.append(Value::Str(Str::from("Producer B"))).unwrap();
        map.insert("xesam:albumArtist".to_string(), owned(Value::Array(album_artists)));
        let mut genres = Array::new(&Signature::Str);
        genres.append(Value::Str(Str::from("Rock"))).unwrap();
        genres.append(Value::Str(Str::from("Folk"))).unwrap();
        map.insert("xesam:genre".to_string(), owned(Value::Array(genres)));
        map.insert("mpris:artUrl".to_string(), owned(Value::Str(Str::from("file:///home/someone/art.png"))));
        map.insert("mpris:length".to_string(), owned(Value::I64(1_302_000_000)));
        map.insert(
            "mpris:trackid".to_string(),
            owned(Value::ObjectPath(ObjectPath::try_from("/org/mpris/MediaPlayer2/firefox").unwrap())),
        );
        map.insert("xesam:url".to_string(), owned(Value::Str(Str::from("https://example.com/watch"))));
        map
    }

    #[test]
    fn parse_metadata_reads_every_real_field_from_a_full_metadata_dict() {
        let parsed = parse_metadata(&real_metadata());
        assert_eq!(parsed.title, "Track Title");
        assert_eq!(parsed.artist, "Ahmed Ebrahim");
        assert_eq!(parsed.album, "Record");
        assert_eq!(parsed.album_artist, "Producer A, Producer B");
        assert_eq!(parsed.genre, "Rock, Folk");
        assert_eq!(parsed.art_url.as_deref(), Some("file:///home/someone/art.png"));
        assert_eq!(parsed.length_us, Some(1_302_000_000));
        assert_eq!(parsed.trackid.as_deref(), Some("/org/mpris/MediaPlayer2/firefox"));
        assert_eq!(parsed.url.as_deref(), Some("https://example.com/watch"));
    }

    /// Any session-bus application can be a player, so its text is capped on a UTF-8 boundary and
    /// a URL past its cap is dropped: cut short, it would name a different file or page.
    #[test]
    fn parse_metadata_caps_text_and_drops_an_overlong_url() {
        let long = format!("{}é{}", "a".repeat(1023), "b".repeat(4096));
        let mut artists = Array::new(&Signature::Str);
        artists.append(Value::Str(Str::from(long.as_str()))).unwrap();
        let mut map = HashMap::new();
        for key in ["xesam:title", "xesam:album"] {
            map.insert(key.to_string(), owned(Value::Str(Str::from(long.as_str()))));
        }
        for key in ["xesam:artist", "xesam:albumArtist", "xesam:genre"] {
            map.insert(key.to_string(), owned(Value::Array(artists.try_clone().unwrap())));
        }
        let data_url = format!("data:image/png;base64,{}", "A".repeat(4096));
        map.insert("mpris:artUrl".to_string(), owned(Value::Str(Str::from(data_url.as_str()))));
        map.insert("xesam:url".to_string(), owned(Value::Str(Str::from(format!("file:///{}", "d".repeat(4088))))));

        let parsed = parse_metadata(&map);

        let capped = "a".repeat(1023);
        for (field, value) in [
            ("title", &parsed.title),
            ("artist", &parsed.artist),
            ("album", &parsed.album),
            ("album_artist", &parsed.album_artist),
            ("genre", &parsed.genre),
        ] {
            assert_eq!(value, &capped, "{field}: the two-byte é straddles byte 1024, so it goes whole");
        }
        assert_eq!(parsed.art_url, None);
        assert_eq!(parsed.url.as_deref().map(str::len), Some(4096), "a URL exactly at the cap is kept");
    }

    #[test]
    fn parse_metadata_degrades_gracefully_on_an_empty_dict() {
        let parsed = parse_metadata(&HashMap::new());
        assert_eq!(parsed, ParsedMetadata::default());
    }

    #[test]
    fn parse_metadata_accepts_a_bare_string_trackid_defensively() {
        // Real players sometimes publish the wrong D-Bus type.
        let mut map = HashMap::new();
        map.insert("mpris:trackid".to_string(), owned(Value::Str(Str::from("/0"))));
        assert_eq!(parse_metadata(&map).trackid.as_deref(), Some("/0"));
    }

    #[test]
    fn track_identity_follows_trackid_url_and_title_alone() {
        let a = ParsedMetadata {
            trackid: Some("/1".into()),
            url: Some("u".into()),
            title: "t".into(),
            ..Default::default()
        };
        assert_eq!(a.track_identity(), a.clone().track_identity());
        let changed = [
            ParsedMetadata { trackid: Some("/2".into()), ..a.clone() },
            ParsedMetadata { url: Some("u2".into()), ..a.clone() },
            ParsedMetadata { title: "t2".into(), ..a.clone() },
        ];
        for b in changed {
            assert_ne!(a.track_identity(), b.track_identity(), "{b:?}");
        }
    }

    #[test]
    fn resolve_album_art_path_accepts_a_real_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cover.png");
        std::fs::write(&file, b"fake png").unwrap();
        let url = format!("file://{}", file.display());
        assert_eq!(resolve_album_art_path(Some(&url)), file.canonicalize().unwrap().to_string_lossy());
    }

    #[test]
    fn resolve_album_art_path_is_empty_for_a_dangling_file_uri_a_non_file_scheme_or_none() {
        assert_eq!(resolve_album_art_path(Some("file:///nonexistent/path/art.png")), "");
        assert_eq!(resolve_album_art_path(Some("https://example.com/art.png")), "");
        assert_eq!(resolve_album_art_path(None), "");
    }

    #[test]
    fn clamp_seek_target_clamps_into_zero_through_length() {
        // (target, length, expected): the lower bound applies without a length, the upper only with one.
        for (target, length, expected) in [
            (-500, Some(10_000), 0),
            (50_000, Some(10_000), 10_000),
            (5_000, Some(10_000), 5_000),
            (-1, None, 0),
            (999_999_999, None, 999_999_999),
        ] {
            assert_eq!(clamp_seek_target(target, length), expected, "{target} in {length:?}");
        }
    }
}

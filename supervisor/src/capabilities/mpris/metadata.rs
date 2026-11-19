//! Pure `Metadata` (`a{sv}`) parsing, album-art trust checks, and track identity comparison
//! (ADR-0036).

use std::collections::HashMap;
use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use zbus::zvariant::Signature;

use super::{MAX_METADATA_URL_BYTES, MAX_TRACK_TEXT_BYTES, fits_text_cap};
use crate::capabilities::file_url::file_url_to_path;
use crate::capabilities::{DictFields, truncate_utf8_bytes};

/// The keys [`parse_metadata`] reads.
const KEYS: [&str; 9] = [
    "xesam:title",
    "xesam:artist",
    "xesam:album",
    "xesam:albumArtist",
    "xesam:genre",
    "mpris:artUrl",
    "mpris:length",
    "mpris:trackid",
    "xesam:url",
];

/// One [`KEYS`] value in whichever of the types players send for it.
#[derive(Clone)]
enum Field {
    Str(String),
    Path(String),
    /// `as`: `xesam:artist` on every checked player.
    Strs(Vec<String>),
    /// Any integer type: the spec says `x` for `mpris:length`, some players send `t`.
    Int(i64),
}

/// The [`KEYS`] of one `Metadata` `a{sv}`, read through [`Dict`](crate::capabilities::Dict): a
/// player's other keys, an `ay` among them, are skipped rather than built as one `Value` per byte.
#[derive(Clone, Default)]
pub(super) struct Metadata(HashMap<&'static str, Field>);

impl DictFields for Metadata {
    fn read<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error> {
        let Some(&key) = KEYS.iter().find(|&&known| known == key) else { return Ok(false) };
        match map.next_value_seed(FieldSeed)? {
            Some(field) => self.0.insert(key, field),
            None => self.0.remove(key),
        };
        Ok(true)
    }
}

/// Reads one variant to a [`Field`], `None` for a type none of them is.
struct FieldSeed;

impl<'de> DeserializeSeed<'de> for FieldSeed {
    type Value = Option<Field>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("Variant", &["signature", "value"], self)
    }
}

impl<'de> Visitor<'de> for FieldSeed {
    type Value = Option<Field>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a variant")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        const STRS: &Signature = &Signature::static_array(&Signature::Str);
        let signature: Signature = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        Ok(match signature {
            Signature::Str => seq.next_element()?.map(Field::Str),
            Signature::ObjectPath => seq.next_element()?.map(Field::Path),
            Signature::I64 => seq.next_element()?.map(Field::Int),
            Signature::U64 => seq.next_element::<u64>()?.and_then(|n| i64::try_from(n).ok()).map(Field::Int),
            Signature::I32 => seq.next_element::<i32>()?.map(|n| Field::Int(n.into())),
            Signature::U32 => seq.next_element::<u32>()?.map(|n| Field::Int(n.into())),
            signature if signature == *STRS => seq.next_element()?.map(Field::Strs),
            _ => {
                seq.next_element::<IgnoredAny>()?;
                None
            }
        })
    }
}

/// Longer than any real media, so a length past it is a player's "infinite" marker: Firefox sends
/// `i64::MAX` rounded to milliseconds for a live stream, which reads as unknown, like no length.
const MAX_LENGTH_US: i64 = 100 * 365 * 24 * 3600 * 1_000_000;

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

pub(super) fn parse_metadata(Metadata(fields): &Metadata) -> ParsedMetadata {
    let str = |key: &str| match fields.get(key) {
        Some(Field::Str(text)) => Some(text.as_str()),
        _ => None,
    };
    let text = |text: &str| truncate_utf8_bytes(text, MAX_TRACK_TEXT_BYTES);
    // A bare string is accepted too, for malformed metadata.
    let joined = |key: &str| match fields.get(key) {
        Some(Field::Strs(parts)) => text(&parts.join(", ")),
        _ => text(str(key).unwrap_or_default()),
    };
    let url = |key: &str| str(key).filter(|url| url.len() <= MAX_METADATA_URL_BYTES).map(str::to_string);
    ParsedMetadata {
        title: text(str("xesam:title").unwrap_or_default()),
        artist: joined("xesam:artist"),
        album: text(str("xesam:album").unwrap_or_default()),
        album_artist: joined("xesam:albumArtist"),
        genre: joined("xesam:genre"),
        art_url: url("mpris:artUrl"),
        length_us: match fields.get("mpris:length") {
            Some(Field::Int(length)) if *length <= MAX_LENGTH_US => Some(*length),
            _ => None,
        },
        // An object path (`o`) on checked players (Zen, mpv-mpris); a bare string from a
        // type-wrong player is accepted too.
        trackid: match fields.get("mpris:trackid") {
            Some(Field::Path(id) | Field::Str(id)) if fits_text_cap(id) => Some(id.clone()),
            _ => None,
        },
        url: url("xesam:url"),
    }
}

/// `metadata` decoded as a player's `a{sv}` arrives.
#[cfg(test)]
pub(super) fn decoded(metadata: &HashMap<String, zbus::zvariant::OwnedValue>) -> Metadata {
    use zbus::zvariant::{LE, serialized::Context, to_bytes};
    let encoded = to_bytes(Context::new_dbus(LE, 0), metadata).unwrap();
    encoded.deserialize::<crate::capabilities::Dict<Metadata>>().unwrap().0.0
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
    use zbus::zvariant::{Array, ObjectPath, OwnedValue, Signature, Str, Value};

    use super::*;

    fn parse(metadata: &HashMap<String, OwnedValue>) -> ParsedMetadata {
        parse_metadata(&decoded(metadata))
    }

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
        let parsed = parse(&real_metadata());
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
    /// a URL or track id past its cap is dropped: cut short, it would name a different thing.
    #[test]
    fn parse_metadata_caps_text_and_drops_an_overlong_url_or_trackid() {
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
        let trackid = format!("/{}", "t".repeat(super::super::MAX_METADATA_TEXT_BYTES));
        map.insert("mpris:trackid".to_string(), owned(Value::ObjectPath(ObjectPath::try_from(trackid).unwrap())));

        let parsed = parse(&map);

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
        assert_eq!(parsed.trackid, None);
        assert_eq!(parsed.url.as_deref().map(str::len), Some(4096), "a URL exactly at the cap is kept");
    }

    /// The spec says `x`, but some players send `t`; one past `i64::MAX`, or Firefox's live-stream
    /// marker, names no length.
    #[test]
    fn parse_metadata_reads_a_length_of_any_integer_type() {
        for (value, expected) in [
            (Value::U64(1_302_000_000), Some(1_302_000_000)),
            (Value::U64(u64::MAX), None),
            (Value::I64(9_223_372_036_854_000_000), None),
            (Value::I64(MAX_LENGTH_US), Some(MAX_LENGTH_US)),
            (Value::I64(99 * 365 * 24 * 3600 * 1_000_000), Some(99 * 365 * 24 * 3600 * 1_000_000)),
            (Value::U32(7), Some(7)),
            (Value::I32(-1), Some(-1)),
            (Value::F64(7.0), None),
        ] {
            let map = HashMap::from([("mpris:length".to_string(), owned(value.try_clone().unwrap()))]);
            assert_eq!(parse(&map).length_us, expected, "{value:?}");
        }
    }

    /// An `ay` under a key nobody reads, or under one read as text, is skipped unbuilt.
    #[test]
    fn a_byte_array_is_skipped_under_any_key() {
        let bytes = || owned(Value::from(vec![7u8; 256 * 1024]));
        let mut map = real_metadata();
        map.insert("x-cover-bytes".to_string(), bytes());
        map.insert("xesam:album".to_string(), bytes());
        let parsed = parse(&map);
        assert_eq!((parsed.title.as_str(), parsed.album.as_str()), ("Track Title", ""));
    }

    #[test]
    fn parse_metadata_degrades_gracefully_on_an_empty_dict() {
        let parsed = parse(&HashMap::new());
        assert_eq!(parsed, ParsedMetadata::default());
    }

    #[test]
    fn parse_metadata_accepts_a_bare_string_trackid_defensively() {
        // Real players sometimes publish the wrong D-Bus type.
        let mut map = HashMap::new();
        map.insert("mpris:trackid".to_string(), owned(Value::Str(Str::from("/0"))));
        assert_eq!(parse(&map).trackid.as_deref(), Some("/0"));
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

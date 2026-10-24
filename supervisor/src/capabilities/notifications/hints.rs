//! Custom serde deserialization and hint extraction helpers for `a{sv}` `Notify` hints (ADR-0033).

use std::marker::PhantomData;

use serde::Deserialize;
use zbus::zvariant::{Signature, Type};

use shared::action::Urgency;

use crate::capabilities::truncate_utf8_bytes;

use super::icon::RawImageData;
use super::{MAX_DESKTOP_ENTRY_BYTES, MAX_REPLY_PLACEHOLDER_BYTES};

/// Maps raw urgency byte 0/1/2; absent or malformed hints default to `Normal` (ADR-0033).
pub(super) fn urgency_from_hint_byte(byte: Option<u8>) -> Urgency {
    match byte {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// Carries `desktop-entry` (ADR-0101), or `None` when absent, empty, or containing `/`. Desktop
/// ids use dashes for subdirectories, so a slash is not an id and cannot reach `by_app_id`.
pub(super) fn desktop_entry_from_hint(value: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty() && !value.contains('/'))?;
    Some(truncate_utf8_bytes(value, MAX_DESKTOP_ENTRY_BYTES))
}

/// Carries the capped reply placeholder (ADR-0101), or `None` when absent/empty.
pub(super) fn reply_placeholder_from_hint(value: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty())?;
    Some(truncate_utf8_bytes(value, MAX_REPLY_PLACEHOLDER_BYTES))
}

/// `Notify`'s `a{sv}`, decoded to the hints this server reads.
///
/// A `zvariant::Value` holds an array as one `Value` per element, so an `ay` that reaches one
/// costs 72 bytes for every pixel byte at a length the sender picks. Keys named here decode
/// straight to their Rust type instead; keys not named here are skipped without allocating.
#[derive(Default, Type)]
#[zvariant(signature = "a{sv}")]
pub(super) struct Hints {
    pub(super) urgency: Option<u8>,
    pub(super) action_icons: Option<bool>,
    pub(super) resident: Option<bool>,
    pub(super) transient: Option<bool>,
    pub(super) desktop_entry: Option<String>,
    pub(super) reply_placeholder: Option<String>,
    pub(super) suppress_sound: Option<bool>,
    pub(super) sound_file: Option<String>,
    pub(super) sound_name: Option<String>,
    // The picture hints carry all three spellings the spec accumulated; `resolve_image_input`
    // ranks them.
    pub(super) image_data: Option<RawImageData>,
    pub(super) image_data_deprecated: Option<RawImageData>,
    pub(super) icon_data: Option<RawImageData>,
    pub(super) image_path: Option<String>,
    pub(super) image_path_deprecated: Option<String>,
}

/// Walks the dict by hand rather than deriving it: serde's derive refuses a repeated key outright,
/// and refusing is the one outcome a notification server cannot afford, since the sender sees a
/// D-Bus error it ignores and the user simply never gets the notification.
impl<'de> Deserialize<'de> for Hints {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(HintsVisitor)
    }
}

struct HintsVisitor;

impl<'de> serde::de::Visitor<'de> for HintsVisitor {
    type Value = Hints;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the Notify hints dict")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut hints = Hints::default();
        while let Some(key) = map.next_key::<&str>()? {
            // A repeated key keeps the last, as a `HashMap` would.
            match key {
                "urgency" => hints.urgency = map.next_value_seed(Hint::new())?,
                "action-icons" => hints.action_icons = map.next_value_seed(Hint::new())?,
                "resident" => hints.resident = map.next_value_seed(Hint::new())?,
                "transient" => hints.transient = map.next_value_seed(Hint::new())?,
                "desktop-entry" => hints.desktop_entry = map.next_value_seed(Hint::new())?,
                "x-kde-reply-placeholder-text" => hints.reply_placeholder = map.next_value_seed(Hint::new())?,
                "suppress-sound" => hints.suppress_sound = map.next_value_seed(Hint::new())?,
                "sound-file" => hints.sound_file = map.next_value_seed(Hint::new())?,
                "sound-name" => hints.sound_name = map.next_value_seed(Hint::new())?,
                "image-data" => hints.image_data = map.next_value_seed(Hint::new())?,
                "image_data" => hints.image_data_deprecated = map.next_value_seed(Hint::new())?,
                "icon_data" => hints.icon_data = map.next_value_seed(Hint::new())?,
                "image-path" => hints.image_path = map.next_value_seed(Hint::new())?,
                "image_path" => hints.image_path_deprecated = map.next_value_seed(Hint::new())?,
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        Ok(hints)
    }
}

/// Seeds one hint value, yielding `None` where the sender's signature is not `T`'s.
///
/// `zvariant::as_value::optional` fails the whole call on a mismatch, which would drop a
/// notification over a single malformed hint; every other server on the bus ignores the hint and
/// shows the notification.
struct Hint<T>(PhantomData<T>);

impl<T> Hint<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'de, T: Deserialize<'de> + Type> serde::de::DeserializeSeed<'de> for Hint<T> {
    type Value = Option<T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("Variant", &["signature", "value"], HintVisitor(PhantomData))
    }
}

struct HintVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de> + Type> serde::de::Visitor<'de> for HintVisitor<T> {
    type Value = Option<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a notification hint")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let signature: Signature = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        if T::SIGNATURE != &signature {
            // Draining keeps the dict in step. `IgnoredAny` walks the value without allocating, so
            // an oversized array costs time and no memory.
            seq.next_element::<serde::de::IgnoredAny>()?;
            return Ok(None);
        }
        seq.next_element()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::Serialize;
    use zbus::zvariant::serialized::Context;
    use zbus::zvariant::{Array, LE, Signature, StructureBuilder, Type, Value, to_bytes};

    use super::super::icon;
    use super::*;

    /// A valid 1x1 RGBA `(iiibiiay)` image-data hint.
    fn one_pixel_image_data() -> zbus::zvariant::Value<'static> {
        let mut pixels = Array::new(&Signature::U8);
        for byte in [0x11u8, 0x22, 0x33, 0x44] {
            pixels.append(Value::U8(byte)).expect("a u8 matches this array's declared signature");
        }
        Value::Structure(
            StructureBuilder::new()
                .add_field(1i32)
                .add_field(1i32)
                .add_field(4i32)
                .add_field(true)
                .add_field(8i32)
                .add_field(4i32)
                .append_field(Value::Array(pixels))
                .build()
                .expect("a 7-field (iiibiiay) structure is well-formed"),
        )
    }

    /// An `a{sv}` holding a repeated key, which no `HashMap` can express but any peer can send.
    struct RepeatedKeys<'a>(&'a [(&'a str, zbus::zvariant::Value<'a>)]);

    impl Serialize for RepeatedKeys<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;

            let mut map = serializer.serialize_map(Some(self.0.len()))?;
            for (key, value) in self.0 {
                map.serialize_entry(key, value)?;
            }
            map.end()
        }
    }

    impl Type for RepeatedKeys<'_> {
        const SIGNATURE: &'static Signature =
            <std::collections::HashMap<String, zbus::zvariant::Value<'static>> as Type>::SIGNATURE;
    }

    #[test]
    fn urgency_from_hint_byte_maps_the_three_defined_values_and_defaults_to_normal() {
        assert_eq!(urgency_from_hint_byte(Some(0)), Urgency::Low);
        assert_eq!(urgency_from_hint_byte(Some(1)), Urgency::Normal);
        assert_eq!(urgency_from_hint_byte(Some(2)), Urgency::Critical);
        assert_eq!(urgency_from_hint_byte(None), Urgency::Normal);
        assert_eq!(urgency_from_hint_byte(Some(99)), Urgency::Normal);
    }

    #[test]
    fn a_desktop_entry_hint_is_carried_unless_it_is_empty_or_looks_like_a_path() {
        assert_eq!(desktop_entry_from_hint(Some("org.telegram.desktop")), Some("org.telegram.desktop".to_string()));
        assert_eq!(desktop_entry_from_hint(Some("")), None);
        assert_eq!(desktop_entry_from_hint(Some("../../etc/passwd")), None);
        assert_eq!(desktop_entry_from_hint(Some("/usr/share/applications/x.desktop")), None);
        assert_eq!(desktop_entry_from_hint(None), None);
        assert_eq!(desktop_entry_from_hint(Some(&"a".repeat(500))).unwrap().len(), MAX_DESKTOP_ENTRY_BYTES);
    }

    #[test]
    fn a_reply_placeholder_hint_is_carried_capped_and_never_empty() {
        assert_eq!(reply_placeholder_from_hint(Some("Reply to Alice")), Some("Reply to Alice".to_string()));
        assert_eq!(reply_placeholder_from_hint(Some("")), None);
        assert_eq!(reply_placeholder_from_hint(None), None);
        assert_eq!(reply_placeholder_from_hint(Some(&"é".repeat(100))).unwrap().len(), MAX_REPLY_PLACEHOLDER_BYTES);
    }

    /// One hint with an off-spec signature must cost that hint and nothing else: a sender who
    /// types `transient` wrong still gets its notification shown, as it did when every value was
    /// a `Value` and a bad match simply returned `None`.
    #[test]
    fn a_hint_typed_against_the_spec_drops_alone_and_the_rest_of_the_dict_still_decodes() {
        let mut oversized = Array::new(&Signature::U8);
        for byte in 0..64u8 {
            oversized.append(Value::U8(byte)).expect("a u8 matches this array's declared signature");
        }

        let hints: HashMap<String, Value<'_>> = HashMap::from([
            ("urgency".to_string(), Value::U8(2)),
            // `transient` is `b`; this sender says `u`.
            ("transient".to_string(), Value::U32(1)),
            ("desktop-entry".to_string(), Value::from("org.telegram.desktop")),
            ("image-data".to_string(), one_pixel_image_data()),
            // Never read, and never decoded: the bulk shape under an unnamed key.
            ("x-vendor-thumbnail".to_string(), Value::Array(oversized)),
        ]);

        let encoded = to_bytes(Context::new_dbus(LE, 0), &hints).expect("a{sv} encodes");
        let (decoded, _): (Hints, _) = encoded.deserialize().expect("a malformed hint must not fail the whole dict");

        assert_eq!(decoded.urgency, Some(2), "a well-typed hint beside a malformed one still decodes");
        assert_eq!(decoded.transient, None, "`u` where the spec says `b` drops that hint, not the notification");
        assert_eq!(decoded.desktop_entry.as_deref(), Some("org.telegram.desktop"));
        assert!(decoded.image_data.is_some_and(|image| icon::image_data_is_valid(&image)), "(iiibiiay) decodes");
    }

    /// serde's derive refuses a repeated key outright, which would cost the whole notification.
    #[test]
    fn a_repeated_hint_key_keeps_the_last_instead_of_refusing_the_dict() {
        let hints = RepeatedKeys(&[
            ("urgency", Value::U8(0)),
            ("urgency", Value::U8(2)),
            // A malformed first occurrence must not poison a well-typed second one.
            ("transient", Value::U32(1)),
            ("transient", Value::Bool(true)),
        ]);

        let encoded = to_bytes(Context::new_dbus(LE, 0), &hints).expect("a{sv} encodes");
        let (decoded, _): (Hints, _) = encoded.deserialize().expect("a repeated key must not fail the dict");

        assert_eq!(decoded.urgency, Some(2), "the last value of a repeated key wins, as a map insert did");
        assert_eq!(decoded.transient, Some(true));
    }

    /// `image-data` outranks `image_data`, but only when it is readable; a malformed one is not a
    /// reason to ignore the spelling the sender also supplied.
    #[test]
    fn a_malformed_picture_hint_falls_through_to_the_older_spelling() {
        let hints: HashMap<String, Value<'_>> = HashMap::from([
            ("image-data".to_string(), Value::from("not a picture")),
            ("image_data".to_string(), one_pixel_image_data()),
        ]);

        let encoded = to_bytes(Context::new_dbus(LE, 0), &hints).expect("a{sv} encodes");
        let (decoded, _): (Hints, _) = encoded.deserialize().expect("a malformed picture hint must not fail the dict");

        assert!(decoded.image_data.is_none());
        assert!(decoded.image_data_deprecated.is_some(), "the readable spelling is still used");
    }
}

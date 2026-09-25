//! Notifications capability (`mantle.notifications`, ADR-0033). Hosts
//! `org.freedesktop.Notifications` with a 100-item FIFO, a 20-item newest-first feed view, global
//! DND, and a Lua-configured per-urgency PipeWire sound registry. `sound-file` overrides a tier
//! default for one notification; `suppress-sound` or a `set_app_muted` app wins; `sound-name`
//! picks a freedesktop theme sound in place of a registered tier default.
//!
//! Like `tray`, a controller owns writes, degrades to inert without the session bus, and
//! delegates decisions to pure helpers. Queue/DND are global Supervisor state (ADR-0033), not
//! per-generation; there is no `reset_registrations`.
//!
//! The parser replaces the base spec's blanket strip-to-plain-text sanitizer with a wider allowlist
//! grammar: five constructs are allowlisted (`<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`);
//! everything else is rejected. Images, `image-path`, and action icons share
//! [`icon::validate_trusted_path`]: an existing regular file under a canonicalized trusted root,
//! otherwise no icon and no error.

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
use zbus::zvariant::{Signature, Type};

use crate::capabilities::truncate_utf8_bytes;

pub mod actions;
pub mod controller;
pub mod icon;
pub mod markup;
pub mod queue;
pub mod sound;

pub use controller::NotificationsController;
use icon::RawImageData;
pub use sound::run_sound_player;

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotificationsAction {
    /// Removes a queued notification.
    Dismiss { id: u32 },
    /// Invokes an `actions[].key`, or `"default"`; removes the notification unless it is resident.
    InvokeAction {
        id: u32,
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        key: String,
    },
    /// Sends reply text to a notification with `has_reply`; removes it unless it is resident.
    Reply { id: u32, text: String },
    /// Sets an urgency tier's sound: an existing file under `/usr/share`, `/usr/local/share`, `/opt`
    /// or `$XDG_DATA_HOME`, else ignored. Only Ogg Vorbis and 16-bit PCM WAV play.
    SetSound { urgency: Urgency, path: String },
    /// Gates non-critical notification sounds.
    SetDnd { enabled: bool },
    /// Mutes non-critical sounds like `set_dnd`, without changing `dnd`.
    SetQuiet { enabled: bool },
    /// Silences every sound from an app, critical included, matched exactly on `app_name` or
    /// `desktop_entry`.
    SetAppMuted { app: String, muted: bool },
    /// Pauses every expiry countdown for `seconds`, capped at 300; `0` releases the hold.
    HoldExpiry { seconds: u64 },
}

/// Dispatch (ADR-0037): signal-emitting `dismiss`/`invoke_action`/`reply` use `tokio::spawn`
/// (ADR-0029); locked state writes run inline (ADR-0033).
pub fn dispatch(controller: &NotificationsController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<NotificationsAction>(&envelope.params) else { return };
    let spawned = controller.clone();
    match action {
        NotificationsAction::Dismiss { id } => {
            tokio::spawn(async move { spawned.dismiss(id).await });
        }
        NotificationsAction::InvokeAction { id, key } => {
            tokio::spawn(async move { spawned.invoke_action(id, key).await });
        }
        NotificationsAction::Reply { id, text } => {
            tokio::spawn(async move { spawned.reply(id, text).await });
        }
        NotificationsAction::SetSound { urgency, path } => controller.set_sound(urgency, &path),
        NotificationsAction::SetDnd { enabled } => controller.set_dnd(enabled),
        NotificationsAction::SetQuiet { enabled } => controller.set_quiet(enabled),
        NotificationsAction::SetAppMuted { app, muted } => controller.set_app_muted(app, muted),
        NotificationsAction::HoldExpiry { seconds } => controller.hold_expiry(seconds),
    }
}

/// Well-known bus name and object path for `org.freedesktop.Notifications`.
pub const NOTIFICATIONS_BUS_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_OBJECT_PATH: &str = "/org/freedesktop/Notifications";

/// Property caps (ADR-0033), measured in bytes and truncated at UTF-8 boundaries.
const MAX_APP_NAME_BYTES: usize = 64;
const MAX_SUMMARY_BYTES: usize = 128;
const MAX_BODY_BYTES: usize = 512;

/// Action caps (ADR-0090): unprivileged session-bus input is drawn by config. Eight is well past a
/// typical card's three; labels are capped tighter than summaries.
const MAX_ACTIONS: usize = 8;
const MAX_ACTION_LABEL_BYTES: usize = 64;

/// Theme name from `app_icon` (ADR-0091), capped like the short identifier `app_name`.
const MAX_APP_ICON_NAME_BYTES: usize = MAX_APP_NAME_BYTES;

/// `desktop-entry` cap (ADR-0101). Reverse-DNS ids top out around 40 bytes; use the summary cap.
const MAX_DESKTOP_ENTRY_BYTES: usize = MAX_SUMMARY_BYTES;
/// Reply placeholder cap (ADR-0101), matching a drawn action label.
const MAX_REPLY_PLACEHOLDER_BYTES: usize = MAX_ACTION_LABEL_BYTES;

/// Backing FIFO cap and `notifications.feed` view size (ADR-0033).
const NOTIFICATION_QUEUE_CAP: usize = 100;
const NOTIFICATION_FEED_VIEW: usize = 20;

/// Raw image-data dimension cap, shared with `tray`.
const MAX_IMAGE_DIMENSION: i32 = 128;

/// Server default for `expire_timeout == -1`, matching mako/dunst (ADR-0033).
const DEFAULT_EXPIRE_MS: u64 = 5000;

/// `GetCapabilities`'s exact 10 strings (ADR-0033). Only `icon-multi` is absent because `Notify`
/// has no multi-size wire field. `sound` honors `sound-file`, `sound-name` and the tier default via
/// [`queue::should_play_sound`].
const NOTIFICATIONS_CAPABILITIES: [&str; 10] = [
    "action-icons",
    "actions",
    "body",
    "body-hyperlinks",
    "body-images",
    "body-markup",
    "icon-static",
    "persistence",
    "sound",
    "inline-reply",
];

// Wire-facing types (ADR-0033).

/// One body-markup run (ADR-0033): styled text or an image.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "kind")]
pub enum NotificationSpan {
    #[serde(rename = "text")]
    Text {
        /// Unescaped text; empty runs are omitted.
        text: String,
        /// Whether the run was inside `<b>`.
        bold: bool,
        /// Whether the run was inside `<i>`.
        italic: bool,
        /// Whether the run was inside `<u>`.
        underline: bool,
        /// `<a href>` target, or `nil` when not a link.
        href: Option<String>,
    },
    #[serde(rename = "image")]
    Image {
        /// Existing absolute path under an icon root; images elsewhere are dropped.
        image_path: String,
    },
}

/// One action button (ADR-0090).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NotificationAction {
    /// Opaque key for `:invoke_action(id, key)`.
    pub key: String,
    /// Button label, capped at 64 bytes. An empty label falls back to the key unless `icon_name` is set.
    pub label: String,
    /// Theme icon name (the key) when the sender set `action-icons`, else `nil`. Never a path.
    pub icon_name: Option<String>,
}

/// Notification urgency, also the `set_sound` tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub enum Urgency {
    #[serde(rename = "low")]
    Low,
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "critical")]
    Critical,
}

/// Maps raw urgency byte 0/1/2; absent or malformed hints default to `Normal` (ADR-0033).
fn urgency_from_hint_byte(byte: Option<u8>) -> Urgency {
    match byte {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// Carries `desktop-entry` (ADR-0101), or `None` when absent, empty, or containing `/`. Desktop
/// ids use dashes for subdirectories, so a slash is not an id and cannot reach `by_app_id`.
fn desktop_entry_from_hint(value: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty() && !value.contains('/'))?;
    Some(truncate_utf8_bytes(value, MAX_DESKTOP_ENTRY_BYTES))
}

/// Carries the capped reply placeholder (ADR-0101), or `None` when absent/empty.
fn reply_placeholder_from_hint(value: Option<&str>) -> Option<String> {
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
    urgency: Option<u8>,
    action_icons: Option<bool>,
    resident: Option<bool>,
    transient: Option<bool>,
    desktop_entry: Option<String>,
    reply_placeholder: Option<String>,
    suppress_sound: Option<bool>,
    sound_file: Option<String>,
    sound_name: Option<String>,
    // The picture hints carry all three spellings the spec accumulated; `resolve_image_input`
    // ranks them.
    image_data: Option<RawImageData>,
    image_data_deprecated: Option<RawImageData>,
    icon_data: Option<RawImageData>,
    image_path: Option<String>,
    image_path_deprecated: Option<String>,
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

/// One `notifications.feed` entry (ADR-0033).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Notification {
    /// Server id, from `1`; a replacement keeps the id it replaces.
    pub id: u32,
    /// Arrival time, Unix seconds; age is `mantle.system.time - timestamp`. A replacement restamps it.
    pub timestamp: i64,
    /// Sending application, truncated to 64 bytes.
    pub app_name: String,
    /// Title as sent, truncated to 128 bytes. Not markup-parsed: the spec makes it plain text.
    pub summary: String,
    /// Parsed body markup; the raw body is truncated to 512 bytes first.
    pub body: Vec<NotificationSpan>,
    /// Attached picture (album art, avatar) as an existing absolute path, or `nil`. Never a theme
    /// name (ADR-0091).
    pub image_path: Option<String>,
    /// Application icon for `icon { name = ... }`: a theme name such as `"firefox"` or an
    /// absolute path, or `nil` (ADR-0091).
    pub app_icon: Option<String>,
    /// `"normal"` when the sender set none. `"critical"` never expires and plays sound through DND.
    pub urgency: Urgency,
    /// The timeout ran out: drop it from popups, keep it in history until dismissed (ADR-0100).
    /// Never true for critical or `expire_timeout = 0`; a replacement resets it.
    pub expired: bool,
    /// Popup-only: removed on expiry instead of retired to history (ADR-0100).
    pub transient: bool,
    /// Sender's desktop id, e.g. `"org.telegram.desktop"`, for `mantle.applications.by_app_id`;
    /// `nil` when absent or containing `/` (ADR-0101).
    pub desktop_entry: Option<String>,
    /// The sender accepts `mantle.notifications:reply(id, text)`.
    pub has_reply: bool,
    /// Placeholder for an empty reply field, e.g. `"Reply to Alice"`, capped at 64 bytes; `nil`
    /// when unset (ADR-0101).
    pub reply_placeholder: Option<String>,
    /// Buttons in sender order, at most 8, excluding `default` and `inline-reply`.
    pub actions: Vec<NotificationAction>,
    /// Clicking the card may `:invoke_action(id, "default")`.
    pub has_default_action: bool,
    /// `hints["resident"]`: keep the notification after an action, as media prev/next needs;
    /// bookkeeping only and omitted from payload (`#[serde(skip)]`).
    #[serde(skip)]
    pub resident: bool,
    /// Bookkeeping only (`#[serde(skip)]`): incremented per `Notify` placement so stale expiry
    /// timers can detect replacement (see [`queue::find_expiring_entry`]).
    #[serde(skip)]
    pub incarnation: u64,
}

/// `mantle.notifications`'s payload (ADR-0033).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NotificationsState {
    /// The newest 20 of up to 100 queued notifications, newest first, expired ones included
    /// (ADR-0100); a replacement keeps its place. An entry past 20 stays dismissable by id.
    pub feed: Vec<Notification>,
    /// Do-not-disturb: mutes non-critical sounds only. Hiding popups is the config's call.
    pub dnd: bool,
}

/// Channel carrying queue/DND changes, matching `tray::TraySignal`'s single variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationsSignal {
    Changed,
}

/// Shared by submodule tests.
#[cfg(test)]
mod test_support {
    use super::NotificationSpan;

    /// Builds a text span from plain arguments.
    pub(super) fn text(text: &str, bold: bool, italic: bool, underline: bool, href: Option<&str>) -> NotificationSpan {
        NotificationSpan::Text { text: text.to_string(), bold, italic, underline, href: href.map(str::to_string) }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::text;
    use super::*;

    /// A valid 1x1 RGBA `(iiibiiay)` image-data hint.
    fn one_pixel_image_data() -> zbus::zvariant::Value<'static> {
        use zbus::zvariant::{Array, Signature, StructureBuilder, Value};

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
    fn app_name_summary_body_caps_match_the_spec() {
        let long = "x".repeat(1000);
        assert_eq!(truncate_utf8_bytes(&long, MAX_APP_NAME_BYTES).len(), MAX_APP_NAME_BYTES);
        assert_eq!(truncate_utf8_bytes(&long, MAX_SUMMARY_BYTES).len(), MAX_SUMMARY_BYTES);
        assert_eq!(truncate_utf8_bytes(&long, MAX_BODY_BYTES).len(), MAX_BODY_BYTES);
    }

    #[test]
    fn urgency_from_hint_byte_maps_the_three_defined_values() {
        assert_eq!(urgency_from_hint_byte(Some(0)), Urgency::Low);
        assert_eq!(urgency_from_hint_byte(Some(1)), Urgency::Normal);
        assert_eq!(urgency_from_hint_byte(Some(2)), Urgency::Critical);
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

    #[test]
    fn urgency_from_hint_byte_defaults_to_normal_when_absent_or_malformed() {
        assert_eq!(urgency_from_hint_byte(None), Urgency::Normal);
        assert_eq!(urgency_from_hint_byte(Some(99)), Urgency::Normal);
    }

    /// One hint with an off-spec signature must cost that hint and nothing else: a sender who
    /// types `transient` wrong still gets its notification shown, as it did when every value was
    /// a `Value` and a bad match simply returned `None`.
    #[test]
    fn a_hint_typed_against_the_spec_drops_alone_and_the_rest_of_the_dict_still_decodes() {
        use std::collections::HashMap;

        use zbus::zvariant::serialized::Context;
        use zbus::zvariant::{Array, LE, Signature, Value, to_bytes};

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
        use zbus::zvariant::serialized::Context;
        use zbus::zvariant::{LE, Value, to_bytes};

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
        use zbus::zvariant::serialized::Context;
        use zbus::zvariant::{LE, Value, to_bytes};

        let hints: std::collections::HashMap<String, Value<'_>> = std::collections::HashMap::from([
            ("image-data".to_string(), Value::from("not a picture")),
            ("image_data".to_string(), one_pixel_image_data()),
        ]);

        let encoded = to_bytes(Context::new_dbus(LE, 0), &hints).expect("a{sv} encodes");
        let (decoded, _): (Hints, _) = encoded.deserialize().expect("a malformed picture hint must not fail the dict");

        assert!(decoded.image_data.is_none());
        assert!(decoded.image_data_deprecated.is_some(), "the readable spelling is still used");
    }

    #[test]
    fn notification_span_text_serializes_with_a_kind_tag() {
        let span = text("hi", true, false, false, Some("url"));
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "kind": "text", "text": "hi", "bold": true, "italic": false, "underline": false, "href": "url" })
        );
    }

    #[test]
    fn notification_span_image_serializes_with_a_kind_tag() {
        let span = NotificationSpan::Image { image_path: "/tmp/x.png".to_string() };
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json, serde_json::json!({ "kind": "image", "image_path": "/tmp/x.png" }));
    }

    #[test]
    fn urgency_serializes_as_lowercase_strings() {
        assert_eq!(serde_json::to_value(Urgency::Low).unwrap(), serde_json::json!("low"));
        assert_eq!(serde_json::to_value(Urgency::Normal).unwrap(), serde_json::json!("normal"));
        assert_eq!(serde_json::to_value(Urgency::Critical).unwrap(), serde_json::json!("critical"));
    }
}

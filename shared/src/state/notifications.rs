//! `mantle.notifications` snapshot payload.

use crate::action::Urgency;
use serde::Serialize;

/// One body-markup run (ADR-0033): styled text or an image.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationSpan {
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
    Image {
        /// Existing absolute path under an icon root; images elsewhere are dropped.
        image_path: String,
    },
}

/// One action button (ADR-0090).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotificationAction {
    /// Opaque key for `:invoke_action(id, key)`.
    pub key: String,
    /// Button label, capped at 64 bytes. An empty label falls back to the key unless `icon_name` is set.
    pub label: String,
    /// Theme icon name (the key) when the sender set `action-icons`, else `nil`. Never a path.
    pub icon_name: Option<String>,
}

/// One `notifications.feed` entry (ADR-0033).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// timers can detect replacement.
    #[serde(skip)]
    pub incarnation: u64,
}

/// `mantle.notifications`'s payload (ADR-0033).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotificationsState {
    /// The newest 20 of up to 100 queued notifications, newest first, expired ones included
    /// (ADR-0100); a replacement keeps its place. An entry past 20 stays dismissable by id.
    pub feed: Vec<Notification>,
    /// Do-not-disturb: mutes non-critical sounds only. Hiding popups is the config's call.
    pub dnd: bool,
}

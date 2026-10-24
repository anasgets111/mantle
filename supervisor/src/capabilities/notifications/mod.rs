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

pub use shared::state::notifications::{Notification, NotificationAction, NotificationSpan, NotificationsState};

pub mod actions;
pub mod controller;
mod hints;
pub mod icon;
pub mod markup;
pub mod queue;
pub mod sound;

pub use controller::NotificationsController;
use shared::action::{NotificationsAction, Urgency};
pub use sound::run_sound_player;

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

/// Shared by submodule tests.
#[cfg(test)]
mod test_support {
    use super::NotificationSpan;

    /// Builds a text span from plain arguments.
    pub(super) fn text(text: &str, bold: bool, italic: bool, underline: bool, href: Option<&str>) -> NotificationSpan {
        NotificationSpan::Text { text: text.to_string(), bold, italic, underline, href: href.map(str::to_string) }
    }
}

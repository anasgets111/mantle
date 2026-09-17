//! `Notify`'s flat action array: drawable buttons plus the `default` and `inline-reply` flags
//! (ADR-0090).

use super::{MAX_ACTION_LABEL_BYTES, MAX_ACTIONS, Notification, NotificationAction};
use crate::capabilities::truncate_utf8_bytes;

/// Results of one pass over `Notify`'s flat action array, returned together rather than as three
/// passes that each re-decide what a key means (ADR-0090).
#[derive(Debug, Default, PartialEq)]
pub(super) struct ParsedActions {
    pub actions: Vec<NotificationAction>,
    /// A `"default"` key was present; the notification is activatable.
    pub has_default: bool,
    /// An `"inline-reply"` key was present, per the `x-kde-reply` convention.
    pub has_reply: bool,
}

/// Splits `Notify` actions into drawable buttons and the two non-button keys (ADR-0090).
///
/// Odd length means a key has no label. The base spec disallows it, but the notification survives:
/// the key becomes its label.
///
/// With `action_icons`, keys may also be theme icon names. Keep an action only if it has a label or
/// drawable icon; `["", ""]` produces no unlabelled, unpredictable button.
pub(super) fn parse_actions(actions: &[String], action_icons: bool) -> ParsedActions {
    let mut parsed = ParsedActions::default();
    for pair in actions.chunks(2) {
        let Some(key) = pair.first().filter(|key| !key.is_empty()) else { continue };
        match key.as_str() {
            "default" => {
                parsed.has_default = true;
                continue;
            }
            "inline-reply" => {
                parsed.has_reply = true;
                continue;
            }
            _ => {}
        }
        if parsed.actions.len() >= MAX_ACTIONS {
            continue;
        }
        // Theme name, never path: `icon` also accepts absolute paths, so this blocks file access.
        let icon_name = (action_icons && !key.contains('/')).then(|| key.clone());
        let label = pair.get(1).map(String::as_str).unwrap_or_default().trim();
        let label = if label.is_empty() && icon_name.is_none() { key.as_str() } else { label };
        if label.is_empty() && icon_name.is_none() {
            continue;
        }
        parsed.actions.push(NotificationAction {
            key: key.clone(),
            label: truncate_utf8_bytes(label, MAX_ACTION_LABEL_BYTES),
            icon_name,
        });
    }
    parsed
}

/// Whether `notification` offered `key`, including the non-button `"default"` activation.
pub(super) fn declares_action(notification: &Notification, key: &str) -> bool {
    (key == "default" && notification.has_default_action) || notification.actions.iter().any(|a| a.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::notifications::Urgency;

    fn keys(actions: &[String], action_icons: bool) -> Vec<String> {
        parse_actions(actions, action_icons).actions.into_iter().map(|a| a.key).collect()
    }

    fn flat(pairs: &[(&str, &str)]) -> Vec<String> {
        pairs.iter().flat_map(|(key, label)| [key.to_string(), label.to_string()]).collect()
    }

    /// `default` activates the card and `inline-reply` is a text field, so neither is a button.
    #[test]
    fn default_and_inline_reply_become_flags_rather_than_buttons() {
        let parsed =
            parse_actions(&flat(&[("default", "Open"), ("inline-reply", "Reply"), ("archive", "Archive")]), false);
        assert!(parsed.has_default);
        assert!(parsed.has_reply);
        assert_eq!(parsed.actions.len(), 1);
        assert_eq!(parsed.actions[0].key, "archive");
        assert_eq!(parsed.actions[0].label, "Archive");
    }

    #[test]
    fn an_array_with_neither_key_sets_neither_flag() {
        let parsed = parse_actions(&flat(&[("archive", "Archive")]), false);
        assert!(!parsed.has_default);
        assert!(!parsed.has_reply);
        assert_eq!(parse_actions(&[], false), ParsedActions::default());
    }

    /// Senders omit labels; the usually-readable key becomes the button label.
    #[test]
    fn an_empty_label_falls_back_to_the_key() {
        let parsed = parse_actions(&flat(&[("archive", "")]), false);
        assert_eq!(parsed.actions[0].label, "archive");
    }

    /// Preserve the base spec's odd key-without-label case rather than rejecting the notification.
    #[test]
    fn an_odd_length_array_still_yields_its_last_action() {
        assert_eq!(keys(&["archive".to_string()], false), ["archive"]);
    }

    /// Under `action-icons`, carry a key as a theme name unless it contains `/`; `icon` also takes
    /// paths, so the separator check blocks arbitrary file access.
    #[test]
    fn action_icons_carries_the_key_as_an_icon_name_but_never_as_a_path() {
        let parsed = parse_actions(&flat(&[("mail-archive", "")]), true);
        assert_eq!(parsed.actions[0].icon_name.as_deref(), Some("mail-archive"));

        let parsed = parse_actions(&flat(&[("/home/anas/.ssh/id_ed25519", "Archive")]), true);
        assert_eq!(parsed.actions[0].icon_name, None, "a path must not be carried as an icon name");

        let parsed = parse_actions(&flat(&[("mail-archive", "")]), false);
        assert_eq!(parsed.actions[0].icon_name, None, "without the hint the key is not an icon");
    }

    /// Drop actions with neither a label nor an icon instead of making blank buttons.
    #[test]
    fn an_action_with_nothing_to_draw_is_dropped() {
        let parsed = parse_actions(&flat(&[("", ""), ("", "Orphan label")]), true);
        assert!(parsed.actions.is_empty(), "got {:?}", parsed.actions);
    }

    #[test]
    fn the_action_count_and_label_length_are_both_capped() {
        let many: Vec<(String, String)> = (0..20).map(|i| (format!("k{i}"), format!("l{i}"))).collect();
        let flattened: Vec<String> = many.iter().flat_map(|(k, l)| [k.clone(), l.clone()]).collect();
        assert_eq!(parse_actions(&flattened, false).actions.len(), MAX_ACTIONS);

        let long = "x".repeat(1000);
        let parsed = parse_actions(&["k".to_string(), long], false);
        assert_eq!(parsed.actions[0].label.len(), MAX_ACTION_LABEL_BYTES);
    }

    /// Guards `ActionInvoked` to sender-declared keys; `"default"` counts outside the button list.
    #[test]
    fn declares_action_covers_the_buttons_and_the_default_activation() {
        let mut notification = Notification {
            id: 1,
            timestamp: 0,
            app_name: "app".to_string(),
            summary: "s".to_string(),
            body: Vec::new(),
            image_path: None,
            app_icon: None,
            urgency: Urgency::Normal,
            expired: false,
            transient: false,
            desktop_entry: None,
            has_reply: false,
            reply_placeholder: None,
            actions: vec![NotificationAction {
                key: "archive".to_string(),
                label: "Archive".to_string(),
                icon_name: None,
            }],
            has_default_action: false,
            resident: false,
            incarnation: 0,
        };
        assert!(declares_action(&notification, "archive"));
        assert!(!declares_action(&notification, "delete"));
        assert!(!declares_action(&notification, "default"));

        notification.has_default_action = true;
        assert!(declares_action(&notification, "default"));
    }
}

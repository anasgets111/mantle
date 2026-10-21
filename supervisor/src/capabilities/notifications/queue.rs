//! Urgency expiry, DND sound gating, and FIFO queue/icon lifecycle.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use super::{DEFAULT_EXPIRE_MS, NOTIFICATION_FEED_VIEW, NOTIFICATION_QUEUE_CAP, Notification, Urgency};

/// Whether a notification auto-expires, and when.
/// Resolves `expire_timeout` (ADR-0033): critical never expires; otherwise `0` means never,
/// negative means [`DEFAULT_EXPIRE_MS`] (mako/dunst convention), and positive means milliseconds.
pub(super) fn resolve_expiry(urgency: Urgency, expire_timeout: i32) -> Option<Duration> {
    if urgency == Urgency::Critical {
        return None;
    }
    match expire_timeout {
        0 => None,
        t if t < 0 => Some(Duration::from_millis(DEFAULT_EXPIRE_MS)),
        t => Some(Duration::from_millis(t as u64)),
    }
}

/// Whether a selected sound plays (ADR-0033): DND gates it except for critical urgency.
pub(super) fn should_play_sound(dnd: bool, urgency: Urgency) -> bool {
    !dnd || urgency == Urgency::Critical
}

/// Selects one `Notify` sound (ADR-0033): `suppress` wins; otherwise `client_sound_file`, already
/// validated against the sound roots, beats the tier default. `sound_name` only replaces a tier
/// default the config registered. DND/urgency is separate.
pub(super) fn resolve_sound_path(
    suppress: bool,
    client_sound_file: Option<PathBuf>,
    sound_name: Option<PathBuf>,
    tier_default: Option<PathBuf>,
) -> Option<PathBuf> {
    if suppress {
        return None;
    }
    client_sound_file.or_else(|| tier_default.map(|tier| sound_name.unwrap_or(tier)))
}

/// Resolves ids: a `replaces_id` naming a queued notification passes through without consuming an
/// id (base spec/ADR-0033); anything else allocates monotonically and wraps to `1` (wire `0` means
/// "new"). An unknown id is not reused, or a later allocation would hand it out a second time.
pub(super) fn resolve_notification_id(replaces_id: u32, queue: &VecDeque<Notification>, next_id: &mut u32) -> u32 {
    if replaces_id != 0 && queue.iter().any(|entry| entry.id == replaces_id) {
        return replaces_id;
    }
    let id = *next_id;
    *next_id = if *next_id == u32::MAX { 1 } else { *next_id + 1 };
    id
}

/// Cleanup after queue mutation. FIFO eviction needs icon deletion and
/// `NotificationClosed(evicted_id, reason=Evicted)`; same-id replacement needs only
/// deletion because the id remains queued.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum QueueCleanup {
    /// Same id; a changed/cleared icon is orphaned by `replaces_id`.
    ReplacedImage(String),
    /// A different notification fell off the queue.
    Evicted { id: u32, image_path: Option<String> },
}

/// Appends a new arrival, evicting past [`NOTIFICATION_QUEUE_CAP`]. FIFO eviction returns the
/// evicted id/icon so the caller deletes its spooled file in the same step and emits
/// `NotificationClosed(..., reason=Evicted)`.
fn push_new(queue: &mut VecDeque<Notification>, notification: Notification) -> Option<QueueCleanup> {
    queue.push_back(notification);
    if queue.len() > NOTIFICATION_QUEUE_CAP {
        queue.pop_front().map(|evicted| QueueCleanup::Evicted { id: evicted.id, image_path: evicted.image_path })
    } else {
        None
    }
}

/// Resolves replacement images: no fresh image clears and deletes the old path rather than leaving
/// a stale image attached to new text (ADR-0033); a different path also deletes old; reusing the
/// same path deletes nothing.
fn resolve_replacement_image(
    previous_image_path: Option<String>,
    fresh_image_path: Option<String>,
) -> (Option<String>, Option<String>) {
    match (&previous_image_path, &fresh_image_path) {
        (Some(old), Some(new)) if old != new => (fresh_image_path, previous_image_path),
        (Some(_), Some(_)) => (fresh_image_path, None),
        (Some(_), None) => (None, previous_image_path),
        (None, _) => (fresh_image_path, None),
    }
}

/// Replaces a present id in place, preserving order; otherwise falls back to [`push_new`]. Returns
/// the old icon cleanup or the fallback eviction (including its close signal).
pub(super) fn replace_or_push(
    queue: &mut VecDeque<Notification>,
    mut notification: Notification,
) -> Option<QueueCleanup> {
    let target_id = notification.id;
    if let Some(existing) = queue.iter_mut().find(|entry| entry.id == target_id) {
        let previous_icon = existing.image_path.clone();
        let (resolved_icon, to_delete) = resolve_replacement_image(previous_icon, notification.image_path.take());
        notification.image_path = resolved_icon;
        *existing = notification;
        to_delete.map(QueueCleanup::ReplacedImage)
    } else {
        push_new(queue, notification)
    }
}

/// Finds the entry a timer captured as `(id, incarnation)` for, or `None` when it was removed or a
/// newer `replaces_id` superseded it. The newer timer handles expiry on its own schedule.
pub(super) fn find_expiring_entry(queue: &VecDeque<Notification>, id: u32, incarnation: u64) -> Option<usize> {
    queue.iter().position(|entry| entry.id == id && entry.incarnation == incarnation)
}

/// What an expiry did to the queue (ADR-0100).
#[derive(Debug, PartialEq)]
pub(super) enum Expiry {
    /// The entry stays, now marked `expired`: out of the popup, still in the history.
    Retired,
    /// The entry is gone, because the sender marked it `transient` -- a popup and nothing more.
    /// Carries the spooled image path, if any, so the caller can delete the file.
    Removed { image_path: Option<String> },
}

/// Expiry decision for [`super::controller::NotificationsController::expire`] (ADR-0100): an ordinary entry retires
/// into history and only `dismiss` removes it, so history shows what happened; a `transient` entry
/// is removed.
/// `None` covers stale/missing or already-expired entries, preventing duplicate close signals.
pub(super) fn expire_entry(queue: &mut VecDeque<Notification>, id: u32, incarnation: u64) -> Option<Expiry> {
    let index = find_expiring_entry(queue, id, incarnation)?;
    if queue[index].expired {
        return None;
    }
    if queue[index].transient {
        return queue.remove(index).map(|removed| Expiry::Removed { image_path: removed.image_path });
    }
    queue[index].expired = true;
    Some(Expiry::Retired)
}

/// Shared removal for `dismiss`, `reply`, and `CloseNotification`; callers delete any icon and
/// emit the appropriate signal.
pub(super) fn remove_by_id(queue: &mut VecDeque<Notification>, id: u32) -> Option<Notification> {
    let index = queue.iter().position(|entry| entry.id == id)?;
    queue.remove(index)
}

/// Newest [`NOTIFICATION_FEED_VIEW`] entries for `notifications.feed` (ADR-0033), newest first.
pub(super) fn feed_view(queue: &VecDeque<Notification>) -> Vec<Notification> {
    queue.iter().rev().take(NOTIFICATION_FEED_VIEW).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::text;
    use super::*;

    #[test]
    fn resolve_expiry_follows_urgency_and_timeout() {
        let default = Some(Duration::from_millis(DEFAULT_EXPIRE_MS));
        // (urgency, timeout ms, expected): critical never expires, 0 means never, -1 the server default.
        for (urgency, timeout, expected) in [
            (Urgency::Critical, 1000, None),
            (Urgency::Critical, 0, None),
            (Urgency::Critical, -1, None),
            (Urgency::Normal, 0, None),
            (Urgency::Low, 0, None),
            (Urgency::Normal, -1, default),
            (Urgency::Normal, 2500, Some(Duration::from_millis(2500))),
            (Urgency::Low, 100, Some(Duration::from_millis(100))),
        ] {
            assert_eq!(resolve_expiry(urgency, timeout), expected, "{urgency:?} {timeout}");
        }
    }

    #[test]
    fn should_play_sound_only_under_dnd_when_critical() {
        for (dnd, urgency, expected) in [
            (true, Urgency::Low, false),
            (true, Urgency::Normal, false),
            (true, Urgency::Critical, true),
            (false, Urgency::Normal, true),
            (false, Urgency::Low, true),
        ] {
            assert_eq!(should_play_sound(dnd, urgency), expected, "dnd {dnd} {urgency:?}");
        }
    }

    #[test]
    fn resolve_sound_path_orders_suppress_client_file_tier_and_sound_name() {
        let path = |p: &str| Some(PathBuf::from(p));
        let (client, tier, named) = (path("/s/client.wav"), path("/s/tier.wav"), path("/s/bell.oga"));
        // (suppress, client file, sound name, tier default, expected)
        for (suppress, file, name, default, expected) in [
            (true, client.clone(), None, tier.clone(), None), // suppress-sound forces silence over both
            (true, None, None, None, None),
            (false, client.clone(), None, tier.clone(), client.clone()),
            (false, None, None, tier.clone(), tier.clone()),
            (false, None, named.clone(), tier.clone(), named.clone()), // a sound name replaces a registered tier
            (false, None, named, None, None),                          // no registered tier stays silent
            (false, None, None, None, None),
        ] {
            assert_eq!(resolve_sound_path(suppress, file, name, default), expected, "suppress {suppress}");
        }
    }

    #[test]
    fn resolve_notification_id_allocates_reuses_and_wraps() {
        let empty = VecDeque::new();

        // replaces_id 0 allocates monotonically
        let mut next_id = 1u32;
        assert_eq!(resolve_notification_id(0, &empty, &mut next_id), 1);
        assert_eq!(resolve_notification_id(0, &empty, &mut next_id), 2);
        assert_eq!(next_id, 3);

        // a queued replaces_id passes through without bumping the allocator
        let mut next_id = 5u32;
        let queue = VecDeque::from([sample_notification(42, None)]);
        assert_eq!(resolve_notification_id(42, &queue, &mut next_id), 42);
        assert_eq!(next_id, 5, "the allocator must not move for a replaces_id reuse");

        // an unknown replaces_id allocates rather than claiming an id the allocator will reach
        let mut next_id = 5u32;
        assert_eq!(resolve_notification_id(6, &empty, &mut next_id), 5);
        assert_eq!(resolve_notification_id(0, &empty, &mut next_id), 6);

        // wraps past u32::MAX skipping zero
        let mut next_id = u32::MAX;
        assert_eq!(resolve_notification_id(0, &empty, &mut next_id), u32::MAX);
        assert_eq!(next_id, 1, "must wrap to 1, never 0 -- 0 is reserved to mean \"new\" on the wire");
    }

    fn sample_notification(id: u32, image_path: Option<&str>) -> Notification {
        Notification {
            id,
            timestamp: 0,
            app_name: "app".to_string(),
            summary: "summary".to_string(),
            body: vec![text("body", false, false, false, None)],
            image_path: image_path.map(str::to_string),
            app_icon: None,
            urgency: Urgency::Normal,
            expired: false,
            transient: false,
            desktop_entry: None,
            has_reply: false,
            reply_placeholder: None,
            actions: Vec::new(),
            has_default_action: false,
            resident: false,
            incarnation: 0,
        }
    }

    #[test]
    fn expiring_an_ordinary_notification_retires_it_and_keeps_it_in_the_queue() {
        let mut queue = VecDeque::from([sample_notification(1, Some("/dev/shm/x/notif-1.png"))]);
        assert_eq!(expire_entry(&mut queue, 1, 0), Some(Expiry::Retired));
        assert_eq!(queue.len(), 1, "expiry is not removal any more");
        assert!(queue[0].expired);
        assert_eq!(queue[0].image_path.as_deref(), Some("/dev/shm/x/notif-1.png"), "the history still draws it");
    }

    #[test]
    fn expiring_a_transient_notification_removes_it_outright() {
        let mut transient = sample_notification(1, Some("/dev/shm/x/notif-1.png"));
        transient.transient = true;
        let mut queue = VecDeque::from([transient]);
        assert_eq!(
            expire_entry(&mut queue, 1, 0),
            Some(Expiry::Removed { image_path: Some("/dev/shm/x/notif-1.png".to_string()) })
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn a_stale_or_repeated_expiry_does_nothing() {
        let mut queue = VecDeque::from([sample_notification(1, None)]);
        queue[0].incarnation = 5;
        assert_eq!(expire_entry(&mut queue, 1, 4), None, "an older incarnation's timer");
        assert_eq!(expire_entry(&mut queue, 1, 5), Some(Expiry::Retired));
        assert_eq!(expire_entry(&mut queue, 1, 5), None, "already expired: nothing to announce twice");
        assert_eq!(expire_entry(&mut queue, 2, 0), None, "no such id");
    }

    #[test]
    fn push_new_evicts_the_oldest_entry_past_the_cap_and_reports_its_id_and_icon() {
        let mut queue = VecDeque::new();
        assert_eq!(
            push_new(&mut queue, sample_notification(0, Some("/tmp/oldest.png"))),
            None,
            "no eviction under the cap"
        );
        for i in 1..NOTIFICATION_QUEUE_CAP as u32 {
            push_new(&mut queue, sample_notification(i, None));
        }
        let evicted = push_new(&mut queue, sample_notification(9999, None));
        // The 101st insert evicts id 0 and returns its id and icon, so the caller can delete the file and
        // emit NotificationClosed(0, reason=Evicted).
        assert_eq!(evicted, Some(QueueCleanup::Evicted { id: 0, image_path: Some("/tmp/oldest.png".to_string()) }));
        assert_eq!(queue.len(), NOTIFICATION_QUEUE_CAP);
        assert_eq!(queue.front().unwrap().id, 1, "the oldest entry (id 0) must have been evicted");
    }

    #[test]
    fn resolve_replacement_icon_deletes_the_old_icon_unless_the_same_path_is_reused() {
        let path = |p: &str| Some(p.to_string());
        // (old, fresh, resolved, to delete)
        for (old, fresh, resolved, to_delete) in [
            (path("/tmp/old.png"), None, None, path("/tmp/old.png")),
            (path("/tmp/old.png"), path("/tmp/new.png"), path("/tmp/new.png"), path("/tmp/old.png")),
            (path("/tmp/same.png"), path("/tmp/same.png"), path("/tmp/same.png"), None),
            (None, path("/tmp/new.png"), path("/tmp/new.png"), None),
        ] {
            assert_eq!(
                resolve_replacement_image(old.clone(), fresh.clone()),
                (resolved, to_delete),
                "{old:?} {fresh:?}"
            );
        }
    }

    #[test]
    fn replace_or_push_replaces_in_place_and_clears_the_old_icon() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, Some("/tmp/old.png")));
        push_new(&mut queue, sample_notification(2, None));

        let mut replacement = sample_notification(1, None);
        replacement.summary = "updated".to_string();
        let cleanup = replace_or_push(&mut queue, replacement);

        assert_eq!(
            cleanup,
            Some(QueueCleanup::ReplacedImage("/tmp/old.png".to_string())),
            "a same-id replace must never report an Evicted cleanup"
        );
        assert_eq!(queue.len(), 2, "a replace must not grow the queue");
        assert_eq!(queue[0].summary, "updated");
        assert_eq!(queue[0].image_path, None);
        assert_eq!(queue[1].id, 2, "the replace must not reorder other entries");
    }

    #[test]
    fn replace_or_push_falls_back_to_a_fresh_push_and_reports_a_real_eviction_when_the_queue_is_full() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, None));

        let cleanup = replace_or_push(&mut queue, sample_notification(999, None));

        assert_eq!(cleanup, None, "the id is not present");
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.back().unwrap().id, 999);

        let mut queue = VecDeque::new();
        for i in 0..NOTIFICATION_QUEUE_CAP as u32 {
            push_new(&mut queue, sample_notification(i, None));
        }
        // Missing id falls back to push_new, which evicts id 0 at the cap.
        let cleanup = replace_or_push(&mut queue, sample_notification(9999, None));
        assert_eq!(cleanup, Some(QueueCleanup::Evicted { id: 0, image_path: None }));
    }

    #[test]
    fn remove_by_id_returns_the_matching_entry_and_none_for_an_unknown_id() {
        let mut queue = VecDeque::new();
        push_new(&mut queue, sample_notification(1, None));
        push_new(&mut queue, sample_notification(2, Some("/tmp/two.png")));

        assert_eq!(remove_by_id(&mut queue, 999), None);
        assert_eq!(remove_by_id(&mut queue, 2).map(|n| n.id), Some(2));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().id, 1);
    }

    #[test]
    fn feed_view_returns_the_newest_first_capped_at_the_view_size_even_with_a_larger_backing_queue() {
        let mut queue = VecDeque::new();
        for i in 0..NOTIFICATION_QUEUE_CAP as u32 {
            push_new(&mut queue, sample_notification(i, None));
        }
        let view = feed_view(&queue);
        let newest_first: Vec<u32> = (0..NOTIFICATION_QUEUE_CAP as u32).rev().take(NOTIFICATION_FEED_VIEW).collect();
        assert_eq!(
            view.iter().map(|n| n.id).collect::<Vec<_>>(),
            newest_first,
            "the view returns the newest entries first"
        );
    }

    /// Finding 2's race: after `replaces_id=7` moves incarnation 1 to 2, timer A must not remove
    /// the replacement 29 seconds early; timer B must still expire incarnation 2 normally.
    #[test]
    fn find_expiring_entry_ignores_a_stale_timer_superseded_by_a_replace() {
        let mut queue = VecDeque::new();
        let mut notification = sample_notification(7, None);
        notification.incarnation = 1;
        queue.push_back(notification);

        // Replace in place: id stays queued while its incarnation advances.
        let mut replacement = sample_notification(7, None);
        replacement.incarnation = 2;
        queue[0] = replacement;

        // Timer A's incarnation is superseded.
        assert_eq!(
            find_expiring_entry(&queue, 7, 1),
            None,
            "a stale timer for the pre-replace incarnation must be a no-op"
        );
        // Timer B carries the replacement's incarnation.
        assert_eq!(
            find_expiring_entry(&queue, 7, 2),
            Some(0),
            "the current timer for the post-replace incarnation must still fire normally"
        );
    }
}

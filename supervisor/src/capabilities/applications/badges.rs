//! Unity `com.canonical.Unity.LauncherEntry` badge, progress, and urgency listener.
//!
//! Apps (Telegram Desktop, Chromium, Firefox, libunity clients, etc.) broadcast
//! `LauncherEntry.Update(app_uri, a{sv})` signals from arbitrary object paths on the session bus.
//! libunity only emits while `com.canonical.Unity` has an owner and replays current state when the
//! name appears, so this listener claims it without replacing an existing owner after arming its
//! match rules. Both `LauncherEntry.Update` and `NameOwnerChanged` flow through one ordered
//! [`zbus::MessageStream`], so a final update followed by sender disconnect can never reorder.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use enumflags2::BitFlags;
use futures_util::StreamExt;
use serde::de::{IgnoredAny, MapAccess};
use shared::state::applications::{AppBadge, ApplicationsState};
use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::Signature;

use crate::capabilities::{Dict, DictFields, Hint, session_bus};

const BUS_NAME: &str = "com.canonical.Unity";
const DBUS_NAME: &str = "org.freedesktop.DBus";
const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
const MEMBER: &str = "Update";
const MAX_DESKTOP_ID_BYTES: usize = 256;
// ponytail: a real LauncherEntry.Update holds one desktop URI and a handful of scalar properties;
// 4 KiB rejects oversized peer-sent dicts before decoding. Upgrade if a legitimate sender carries
// a larger payload.
const MAX_SIGNAL_BODY_BYTES: usize = 4096;
// ponytail: at most 64 concurrent desktop ids hold badge state; upgrade: per-sender cap or LRU eviction.
const MAX_BADGES: usize = 64;

/// Normalizes a Unity `app_uri` (e.g. `"application://org.telegram.desktop.desktop"`) to a Mantle
/// desktop file id (`"org.telegram.desktop"`), matching `AppSummary.id`.
fn desktop_id_from_uri(app_uri: &str) -> Option<&str> {
    let stripped = match app_uri.strip_prefix("application://") {
        Some(rest) => rest,
        None if app_uri.contains("://") => return None,
        None => app_uri,
    };
    let id = stripped.strip_suffix(".desktop").unwrap_or(stripped);
    if id.is_empty() || id.len() > MAX_DESKTOP_ID_BYTES || id.contains('/') || id.chars().any(char::is_control) {
        return None;
    }
    Some(id)
}

/// Decodes a `count` variant from the spec's `i64` (`x`) or common off-spec integer types (`i`,
/// `u`, `t`), clamping to `0..=i64::MAX` (exact Lua 5.4 integer range) and draining anything else
/// without allocating.
struct CountHint;

impl<'de> serde::de::DeserializeSeed<'de> for CountHint {
    type Value = Option<i64>;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("Variant", &["signature", "value"], CountVisitor)
    }
}

struct CountVisitor;

impl<'de> serde::de::Visitor<'de> for CountVisitor {
    type Value = Option<i64>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an integer variant")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let signature: Signature = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        match signature {
            Signature::I64 => Ok(seq.next_element::<i64>()?.map(|v| v.max(0))),
            Signature::I32 => Ok(seq.next_element::<i32>()?.map(|v| i64::from(v).max(0))),
            Signature::U32 => Ok(seq.next_element::<u32>()?.map(i64::from)),
            Signature::U64 => Ok(seq.next_element::<u64>()?.map(|v| v.min(i64::MAX as u64) as i64)),
            _ => {
                seq.next_element::<IgnoredAny>()?;
                Ok(None)
            }
        }
    }
}

/// Partial property delta from one `LauncherEntry.Update` `a{sv}` dict.
#[derive(Default)]
struct BadgeUpdate {
    count: Option<i64>,
    count_visible: Option<bool>,
    progress: Option<f64>,
    progress_visible: Option<bool>,
    urgent: Option<bool>,
}

impl BadgeUpdate {
    fn is_empty(&self) -> bool {
        self.count.is_none()
            && self.count_visible.is_none()
            && self.progress.is_none()
            && self.progress_visible.is_none()
            && self.urgent.is_none()
    }

    fn apply_to(self, badge: &mut AppBadge) {
        if let Some(count) = self.count {
            badge.count = count;
        }
        if let Some(count_visible) = self.count_visible {
            badge.count_visible = count_visible;
        }
        if let Some(progress) = self.progress {
            badge.progress = progress;
        }
        if let Some(progress_visible) = self.progress_visible {
            badge.progress_visible = progress_visible;
        }
        if let Some(urgent) = self.urgent {
            badge.urgent = urgent;
        }
    }
}

impl DictFields for BadgeUpdate {
    fn read<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error> {
        match key {
            "count" => {
                if let Some(value) = map.next_value_seed(CountHint)? {
                    self.count = Some(value);
                }
            }
            "count-visible" => {
                if let Some(value) = map.next_value_seed(Hint::new())? {
                    self.count_visible = Some(value);
                }
            }
            "progress" => {
                if let Some(value) = map.next_value_seed(Hint::<f64>::new())?
                    && value.is_finite()
                {
                    let clamped = value.clamp(0.0, 1.0);
                    self.progress = Some(if clamped == 0.0 { 0.0 } else { clamped });
                }
            }
            "progress-visible" => {
                if let Some(value) = map.next_value_seed(Hint::new())? {
                    self.progress_visible = Some(value);
                }
            }
            "urgent" => {
                if let Some(value) = map.next_value_seed(Hint::new())? {
                    self.urgent = Some(value);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

struct OwnedBadge {
    sender: String,
    badge: AppBadge,
}

#[derive(Default)]
struct BadgeRegistry {
    entries: BTreeMap<String, OwnedBadge>,
}

impl BadgeRegistry {
    fn apply(&mut self, sender: &str, app_uri: &str, update: BadgeUpdate) -> bool {
        let Some(desktop_id) = desktop_id_from_uri(app_uri) else {
            return false;
        };
        if update.is_empty() {
            return false;
        }
        if let Some(existing) = self.entries.get_mut(desktop_id) {
            let previous = existing.badge.clone();
            if existing.sender != sender {
                // A new bus sender took over this desktop id (e.g. an app restart): reset sticky
                // state so fields from the old process cannot bleed into the new one.
                existing.sender.clear();
                existing.sender.push_str(sender);
                existing.badge = AppBadge::default();
            }
            update.apply_to(&mut existing.badge);
            return existing.badge != previous;
        }
        if self.entries.len() >= MAX_BADGES {
            debug!("dropping LauncherEntry badge for {desktop_id:?}: at cap ({MAX_BADGES})");
            return false;
        }
        let mut badge = AppBadge::default();
        update.apply_to(&mut badge);
        self.entries.insert(desktop_id.to_string(), OwnedBadge { sender: sender.to_string(), badge });
        true
    }

    fn drop_sender(&mut self, sender: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|_, owned| owned.sender != sender);
        self.entries.len() != before
    }

    fn clear(&mut self) -> bool {
        let had_entries = !self.entries.is_empty();
        self.entries.clear();
        had_entries
    }

    fn snapshot(&self) -> BTreeMap<String, AppBadge> {
        self.entries.iter().map(|(id, owned)| (id.clone(), owned.badge.clone())).collect()
    }
}

fn departure_rule() -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(DBUS_NAME)?
        .interface(DBUS_NAME)?
        .member("NameOwnerChanged")?
        .arg(2, "")?
        .build())
}

fn update_rule() -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(INTERFACE)?.member(MEMBER)?.build())
}

pub(super) struct Subscription {
    /// Kept alive for the subscription's lifetime so the bus holds our match rules and name.
    _bus: zbus::Connection,
    stream: zbus::MessageStream,
    registry: BadgeRegistry,
}

/// Drives `future` while draining `stream` in wire order so a startup burst cannot fill zbus's
/// 64-message broadcast queue and stall `AddMatch` or `RequestName`.
async fn drain_until<T>(
    stream: &mut zbus::MessageStream,
    registry: &mut BadgeRegistry,
    state: &Mutex<ApplicationsState>,
    events: &UnboundedSender<()>,
    future: impl std::future::Future<Output = T>,
) -> T {
    let mut future = std::pin::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            message = stream.next() => {
                let Some(message) = message else {
                    if registry.clear() {
                        let _ = publish_badges(state, events, registry.snapshot());
                    }
                    return future.await;
                };
                if let Ok(message) = message
                    && handle_signal(registry, &message)
                {
                    let _ = publish_badges(state, events, registry.snapshot());
                }
            }
        }
    }
}

/// Arms one ordered [`zbus::MessageStream`] for both `NameOwnerChanged` departures and
/// `LauncherEntry.Update`, then requests `com.canonical.Unity` (queued, non-replacing) so libunity
/// clients start emitting and replay any active badges into the armed stream.
pub(super) async fn subscribe(
    bus: zbus::Connection,
    state: &Mutex<ApplicationsState>,
    events: &UnboundedSender<()>,
) -> Option<Subscription> {
    let mut stream = zbus::MessageStream::from(&bus);
    let mut registry = BadgeRegistry::default();
    let dbus = match drain_until(&mut stream, &mut registry, state, events, zbus::fdo::DBusProxy::new(&bus)).await {
        Ok(proxy) => proxy,
        Err(err) => {
            error!("failed to bind {DBUS_NAME}; launcher badges disabled for this run: {err}");
            return None;
        }
    };
    let dep_rule = match departure_rule() {
        Ok(rule) => rule,
        Err(err) => {
            error!("failed to build NameOwnerChanged match rule; badges disabled for this run: {err}");
            return None;
        }
    };
    if let Err(err) = drain_until(&mut stream, &mut registry, state, events, dbus.add_match_rule(dep_rule)).await {
        error!("failed to subscribe to NameOwnerChanged; launcher badges disabled for this run: {err}");
        return None;
    }
    let upd_rule = match update_rule() {
        Ok(rule) => rule,
        Err(err) => {
            error!("failed to build LauncherEntry match rule; badges disabled for this run: {err}");
            return None;
        }
    };
    if let Err(err) = drain_until(&mut stream, &mut registry, state, events, dbus.add_match_rule(upd_rule)).await {
        error!("failed to subscribe to {INTERFACE}.{MEMBER}; badges disabled for this run: {err}");
        return None;
    }
    let request = bus.request_name_with_flags(BUS_NAME, BitFlags::<RequestNameFlags>::empty());
    match drain_until(&mut stream, &mut registry, state, events, request).await {
        Ok(reply) => debug!("RequestName({BUS_NAME}) -> {reply}"),
        Err(err) => warn!("RequestName({BUS_NAME}) failed; libunity senders may stay quiet: {err}"),
    }
    Some(Subscription { _bus: bus, stream, registry })
}

fn publish_badges(
    state: &Mutex<ApplicationsState>,
    events: &UnboundedSender<()>,
    next: BTreeMap<String, AppBadge>,
) -> bool {
    let mut current = state.lock().expect("applications state mutex poisoned");
    if current.badges == next {
        return !events.is_closed();
    }
    current.badges = next;
    drop(current);
    events.send(()).is_ok()
}

fn handle_signal(registry: &mut BadgeRegistry, message: &zbus::Message) -> bool {
    let header = message.header();
    if header.message_type() != zbus::message::Type::Signal {
        return false;
    }
    let (interface, member) = (header.interface().map(|i| i.as_str()), header.member().map(|m| m.as_str()));
    match (interface, member) {
        (Some(DBUS_NAME), Some("NameOwnerChanged")) => {
            if header.sender().map(|s| s.as_str()) != Some(DBUS_NAME) {
                return false;
            }
            let body = message.body();
            let Ok((name, _old_owner, new_owner)) = body.deserialize::<(&str, &str, &str)>() else {
                return false;
            };
            new_owner.is_empty() && name.starts_with(':') && registry.drop_sender(name)
        }
        (Some(INTERFACE), Some(MEMBER)) => {
            let Some(sender) = header.sender().map(|name| name.as_str()) else {
                return false;
            };
            let body = message.body();
            if body.data().len() > MAX_SIGNAL_BODY_BYTES {
                debug!("dropping oversized {INTERFACE}.{MEMBER} signal ({} bytes)", body.data().len());
                return false;
            }
            let Ok((app_uri, Dict(update))) = body.deserialize::<(&str, Dict<BadgeUpdate>)>() else {
                return false;
            };
            registry.apply(sender, app_uri, update)
        }
        _ => false,
    }
}

/// Runs the listener loop on an already-armed [`Subscription`].
pub(super) async fn run_subscription(
    subscription: Subscription,
    state: Arc<Mutex<ApplicationsState>>,
    events: UnboundedSender<()>,
) {
    let Subscription { _bus, mut stream, mut registry } = subscription;
    while let Some(message) = stream.next().await {
        let Ok(message) = message else { continue };
        if handle_signal(&mut registry, &message) && !publish_badges(&state, &events, registry.snapshot()) {
            return;
        }
    }
    if registry.clear() {
        let _ = publish_badges(&state, &events, registry.snapshot());
    }
}

/// Opens a dedicated session bus connection and listens for `LauncherEntry` signals without
/// blocking controller construction or the filesystem scan.
pub(super) fn spawn_session(state: Arc<Mutex<ApplicationsState>>, events: UnboundedSender<()>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(bus) = session_bus("application badges stay unavailable").await else { return };
        let Some(subscription) = subscribe(bus, &state, &events).await else { return };
        run_subscription(subscription, state, events).await;
    })
}

/// Spawns the `LauncherEntry` subscriber on a caller-supplied bus (used by tests on `private_bus`).
#[cfg(test)]
pub(super) fn spawn_on(
    bus: zbus::Connection,
    state: Arc<Mutex<ApplicationsState>>,
    events: UnboundedSender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(subscription) = subscribe(bus, &state, &events).await else { return };
        run_subscription(subscription, state, events).await;
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::serialized::Context;
    use zbus::zvariant::{LE, Value, to_bytes};

    use super::*;
    use crate::capabilities::test_support::{poll_until, private_bus, within};

    fn decode(pairs: &[(&str, Value<'_>)]) -> BadgeUpdate {
        let map: HashMap<&str, Value<'_>> = pairs.iter().map(|(k, v)| (*k, v.clone())).collect();
        let encoded = to_bytes(Context::new_dbus(LE, 0), &map).expect("a{sv} encodes");
        let (Dict(update), _): (Dict<BadgeUpdate>, _) = encoded.deserialize().expect("a{sv} decodes");
        update
    }

    async fn emit_update(sender: &zbus::Connection, path: &str, app_uri: &str, props: &[(&str, Value<'_>)]) {
        let map: HashMap<&str, Value<'_>> = props.iter().map(|(k, v)| (*k, v.clone())).collect();
        let message = zbus::Message::signal(path, INTERFACE, MEMBER)
            .expect("valid signal header")
            .build(&(app_uri, map))
            .expect("valid signal body");
        sender.send(&message).await.expect("emit LauncherEntry.Update");
    }

    #[test]
    fn desktop_id_from_uri_normalizes_application_uris_and_rejects_paths() {
        for (uri, expected) in [
            ("application://org.telegram.desktop.desktop", Some("org.telegram.desktop")),
            ("application://firefox.desktop", Some("firefox")),
            ("application://org.example.app", Some("org.example.app")),
            ("firefox.desktop", Some("firefox")),
            ("firefox", Some("firefox")),
            ("", None),
            ("application://", None),
            ("application://.desktop", None),
            ("file:///usr/share/applications/firefox.desktop", None),
            ("http://example.org/app.desktop", None),
            ("application://../etc/passwd", None),
            ("application://sub/app.desktop", None),
            ("application://bad\nid.desktop", None),
        ] {
            assert_eq!(desktop_id_from_uri(uri), expected, "{uri:?}");
        }
        let at_cap = "a".repeat(MAX_DESKTOP_ID_BYTES);
        assert_eq!(desktop_id_from_uri(&format!("application://{at_cap}.desktop")), Some(at_cap.as_str()));
        let too_long = "a".repeat(MAX_DESKTOP_ID_BYTES + 1);
        assert_eq!(desktop_id_from_uri(&format!("application://{too_long}.desktop")), None);
    }

    #[test]
    fn badge_update_decodes_integer_widths_clamps_bounds_and_ignores_malformed_fields() {
        let full = decode(&[
            ("count", Value::I64(42)),
            ("count-visible", Value::Bool(true)),
            ("progress", Value::F64(0.65)),
            ("progress-visible", Value::Bool(true)),
            ("urgent", Value::Bool(true)),
            ("quicklist", Value::from("/ignored/path")),
        ]);
        assert_eq!(full.count, Some(42));
        assert_eq!(full.count_visible, Some(true));
        assert_eq!(full.progress, Some(0.65));
        assert_eq!(full.progress_visible, Some(true));
        assert_eq!(full.urgent, Some(true));

        // Common off-spec integer signatures and clamping into exact Lua integer range `0..=i64::MAX`.
        assert_eq!(decode(&[("count", Value::I32(7))]).count, Some(7));
        assert_eq!(decode(&[("count", Value::U32(9))]).count, Some(9));
        assert_eq!(decode(&[("count", Value::U64(11))]).count, Some(11));
        assert_eq!(decode(&[("count", Value::U64(u64::MAX))]).count, Some(i64::MAX));
        assert_eq!(decode(&[("count", Value::I64(-5))]).count, Some(0));
        assert_eq!(decode(&[("count", Value::I32(-1))]).count, Some(0));

        // Progress bounds and non-finite rejection.
        assert_eq!(decode(&[("progress", Value::F64(1.8))]).progress, Some(1.0));
        assert_eq!(decode(&[("progress", Value::F64(-0.4))]).progress, Some(0.0));
        assert_eq!(decode(&[("progress", Value::F64(-0.0))]).progress.map(f64::to_bits), Some(0.0f64.to_bits()));
        assert_eq!(decode(&[("progress", Value::F64(f64::NAN))]).progress, None);
        assert_eq!(decode(&[("progress", Value::F64(f64::INFINITY))]).progress, None);

        // Wrongly typed keys drop alone without failing sibling keys.
        let mixed =
            decode(&[("count", Value::from("three")), ("urgent", Value::U32(1)), ("count-visible", Value::Bool(true))]);
        assert_eq!(mixed.count, None);
        assert_eq!(mixed.urgent, None);
        assert_eq!(mixed.count_visible, Some(true));
    }

    #[test]
    fn registry_caps_distinct_entries_while_still_updating_existing_ones() {
        let mut registry = BadgeRegistry::default();
        for i in 0..MAX_BADGES {
            assert!(registry.apply(
                ":1.1",
                &format!("application://app-{i}.desktop"),
                BadgeUpdate { count: Some(1), ..Default::default() }
            ));
        }
        assert!(!registry.apply(
            ":1.1",
            "application://overflow.desktop",
            BadgeUpdate { count: Some(1), ..Default::default() }
        ));
        assert!(registry.apply(
            ":1.1",
            "application://app-0.desktop",
            BadgeUpdate { count: Some(2), ..Default::default() }
        ));
        assert_eq!(registry.snapshot().len(), MAX_BADGES);
        assert_eq!(registry.snapshot()["app-0"].count, 2);
    }

    /// Partial updates merge per sender, oversized bodies are rejected before decoding, a new
    /// sender resets sticky fields and owns the entry, and a sender leaving the bus drops only the
    /// entries it still owns.
    #[tokio::test]
    async fn launcher_entry_updates_merge_and_drop_when_their_sender_leaves_the_bus() {
        let bus = private_bus().await;
        let state = Arc::new(Mutex::new(ApplicationsState::default()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = bus.connection().await;
        let subscription = subscribe(listener, &state, &tx).await.expect("subscribe on private bus");
        let task = tokio::spawn(run_subscription(subscription, Arc::clone(&state), tx));

        let sender_a = bus.connection().await;
        // Oversized signal body is dropped before decoding.
        let huge = "x".repeat(MAX_SIGNAL_BODY_BYTES + 128);
        emit_update(
            &sender_a,
            "/com/canonical/unity/launcherentry/huge",
            "application://oversized.desktop",
            &[("count", Value::I64(99)), ("count-visible", Value::Bool(true)), ("padding", Value::from(huge))],
        )
        .await;

        emit_update(
            &sender_a,
            "/com/canonical/unity/launcherentry/1",
            "application://org.telegram.desktop.desktop",
            &[("count", Value::I64(5)), ("count-visible", Value::Bool(true))],
        )
        .await;
        within(rx.recv()).await.unwrap();
        assert!(!state.lock().unwrap().badges.contains_key("oversized"));
        assert_eq!(
            state.lock().unwrap().badges.get("org.telegram.desktop"),
            Some(&AppBadge { count: 5, count_visible: true, progress: 0.0, progress_visible: false, urgent: false })
        );

        // Partial update keeps `count` and `count_visible` sticky while adding progress and urgency.
        emit_update(
            &sender_a,
            "/com/canonical/unity/launcherentry/1",
            "application://org.telegram.desktop.desktop",
            &[("progress", Value::F64(0.5)), ("progress-visible", Value::Bool(true)), ("urgent", Value::Bool(true))],
        )
        .await;
        within(rx.recv()).await.unwrap();
        assert_eq!(
            state.lock().unwrap().badges.get("org.telegram.desktop"),
            Some(&AppBadge { count: 5, count_visible: true, progress: 0.5, progress_visible: true, urgent: true })
        );

        // Same sender also publishes a second app.
        emit_update(
            &sender_a,
            "/com/canonical/unity/launcherentry/2",
            "application://firefox.desktop",
            &[("progress", Value::F64(0.25)), ("progress-visible", Value::Bool(true))],
        )
        .await;
        within(rx.recv()).await.unwrap();
        assert_eq!(state.lock().unwrap().badges.len(), 2);

        // A second sender takes over `org.telegram.desktop` (e.g. app restart): old sticky fields reset.
        let sender_b = bus.connection().await;
        emit_update(
            &sender_b,
            "/org/telegram/desktop/entry",
            "application://org.telegram.desktop.desktop",
            &[("count", Value::I64(1)), ("count-visible", Value::Bool(true))],
        )
        .await;
        within(rx.recv()).await.unwrap();
        assert_eq!(
            state.lock().unwrap().badges.get("org.telegram.desktop"),
            Some(&AppBadge { count: 1, count_visible: true, progress: 0.0, progress_visible: false, urgent: false })
        );

        // `sender_a` leaves: `firefox` drops, `org.telegram.desktop` (now owned by `sender_b`) stays.
        drop(sender_a);
        poll_until(|| (!state.lock().unwrap().badges.contains_key("firefox")).then_some(())).await;
        assert_eq!(
            state.lock().unwrap().badges.get("org.telegram.desktop"),
            Some(&AppBadge { count: 1, count_visible: true, progress: 0.0, progress_visible: false, urgent: false })
        );

        // `sender_b` leaves: its badge drops too.
        drop(sender_b);
        poll_until(|| state.lock().unwrap().badges.is_empty().then_some(())).await;

        task.abort();
    }

    /// libunity clients stay quiet until `com.canonical.Unity` has an owner and replay their state
    /// as soon as that name appears; the listener claims it after arming its match rules.
    #[tokio::test]
    async fn claiming_unity_bus_name_wakes_libunity_senders_without_stealing_from_an_existing_owner() {
        let bus = private_bus().await;
        let libunity = bus.connection().await;
        let dbus = zbus::fdo::DBusProxy::new(&libunity).await.unwrap();
        let mut unity_appeared = dbus.receive_name_owner_changed_with_args(&[(0, BUS_NAME)]).await.unwrap();
        let emitter = tokio::spawn(async move {
            while let Some(signal) = unity_appeared.next().await {
                if signal.args().is_ok_and(|args| args.new_owner.is_some()) {
                    emit_update(
                        &libunity,
                        "/com/canonical/unity/launcherentry/libunity",
                        "application://org.gnome.Nautilus.desktop",
                        &[("progress", Value::F64(0.4)), ("progress-visible", Value::Bool(true))],
                    )
                    .await;
                    // Keep connection alive until aborted so the badge stays published.
                    std::future::pending::<()>().await;
                }
            }
        });

        let state = Arc::new(Mutex::new(ApplicationsState::default()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = bus.connection().await;
        let subscription = subscribe(listener, &state, &tx).await.expect("subscribe on private bus");
        let task = tokio::spawn(run_subscription(subscription, Arc::clone(&state), tx.clone()));

        within(async {
            while !state.lock().unwrap().badges.contains_key("org.gnome.Nautilus") {
                rx.recv().await.unwrap();
            }
        })
        .await;
        assert_eq!(
            state.lock().unwrap().badges.get("org.gnome.Nautilus"),
            Some(&AppBadge { count: 0, count_visible: false, progress: 0.4, progress_visible: true, urgent: false })
        );

        // A second listener queues without replacing the first owner.
        let other = bus.connection().await;
        let other_state = Mutex::new(ApplicationsState::default());
        let (other_tx, _other_rx) = tokio::sync::mpsc::unbounded_channel();
        let _second = subscribe(other.clone(), &other_state, &other_tx).await.expect("second subscribe succeeds");
        let owner = dbus.get_name_owner(BUS_NAME.try_into().unwrap()).await.unwrap();
        assert_ne!(owner.as_str(), other.unique_name().unwrap().as_str());

        task.abort();
        emitter.abort();
    }

    /// A final `Update` and the sender's disconnect queued before the loop drains must stay in wire
    /// order so the update cannot recreate the badge after `NameOwnerChanged` clears it.
    #[tokio::test]
    async fn a_final_update_queued_beside_sender_departure_does_not_outlive_its_sender() {
        let bus = private_bus().await;
        let state = Arc::new(Mutex::new(ApplicationsState::default()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = bus.connection().await;
        let subscription = subscribe(listener, &state, &tx).await.expect("subscribe on private bus");

        let ephemeral = bus.connection().await;
        let ephemeral_name = ephemeral.unique_name().unwrap().to_owned();
        emit_update(
            &ephemeral,
            "/com/canonical/unity/launcherentry/ephemeral",
            "application://ephemeral.desktop",
            &[("count", Value::I64(9)), ("count-visible", Value::Bool(true))],
        )
        .await;
        drop(ephemeral);

        // Confirm the bus has already broadcast `ephemeral`'s departure before starting the loop,
        // then queue a live sentinel update behind it.
        let alive = bus.connection().await;
        let dbus = zbus::fdo::DBusProxy::new(&alive).await.unwrap();
        within(async {
            while dbus.name_has_owner(ephemeral_name.as_ref().into()).await.unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        emit_update(
            &alive,
            "/com/canonical/unity/launcherentry/alive",
            "application://alive.desktop",
            &[("urgent", Value::Bool(true))],
        )
        .await;

        let task = tokio::spawn(run_subscription(subscription, Arc::clone(&state), tx));

        poll_until(|| state.lock().unwrap().badges.contains_key("alive").then_some(())).await;
        assert!(
            !state.lock().unwrap().badges.contains_key("ephemeral"),
            "final Update must not recreate a badge after its sender's NameOwnerChanged"
        );

        task.abort();
    }
}

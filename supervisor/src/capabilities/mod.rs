//! Lazy capability startup, signal routing, and command dispatch (ADR-0037, ADR-0070, ADR-0076).
//!
//! [`Capabilities`] owns every controller and channel, so `main.rs` does not grow per capability;
//! exhaustive start, push, and dispatch matches fail to compile at a missing arm.
//!
//! No trait or boxed registry: controllers differ (`snapshot`, async
//! `handle_signal`, or a channel), and `main.rs` needs concrete types. ADR-0037 decision 3 chose
//! static calls.
//!
//! **One flat child module per roster entry.** Shared names cover `shared::Capability`,
//! `mantle.<name>`, and command `capability`. [`read_attr`] lives here, `polkit` in
//! `crate::polkit`, and `shm_icons` beside its two consumers (ADR-0076).

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

pub use latest_writes::{LatestWrites, Writer};
pub use lifecycle::Capabilities;
pub use signals::Signal;

// Reconnect backoff shared by the PipeWire mixer and the niri reader.
// ponytail: polls 1 s doubling to 30 s; a watch on the socket would reconnect at once.
pub const RETRY_FIRST: Duration = Duration::from_secs(1);
pub const RETRY_MAX: Duration = Duration::from_secs(30);
/// A connection that lasted this long resets the backoff; a daemon crashing sooner keeps backing off.
pub const STABLE: Duration = Duration::from_secs(10);

/// The reconnect delay after waiting `delay`: doubled, capped at [`RETRY_MAX`].
pub fn next_delay(delay: Duration) -> Duration {
    (delay * 2).min(RETRY_MAX)
}

/// Every Supervisor bus gets the 25s call timeout Qt, GDBus and libdbus default to; zbus has none
/// (ADR-0070 amendment).
pub async fn with_call_timeout(builder: zbus::Result<zbus::connection::Builder<'_>>) -> zbus::Result<zbus::Connection> {
    builder?.method_timeout(std::time::Duration::from_secs(25)).build().await
}

/// The session bus, or `None` after one error line saying what `what` stays off this run.
async fn session_bus(what: &str) -> Option<zbus::Connection> {
    with_call_timeout(zbus::connection::Builder::session())
        .await
        .inspect_err(|err| shared::error!("failed to connect to the session bus; {what} for this run: {err}"))
        .ok()
}

pub mod appearance;
pub mod applications;
pub mod audio;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod clock;
pub(crate) mod file_url;
pub mod folders;
pub mod idle;
pub mod keyboard;
mod latest_writes;
mod lifecycle;
pub mod lock;
pub mod mpris;
pub mod network;
pub mod notifications;
pub mod polkit;
pub mod power;
pub mod privacy;
pub mod processes;
pub mod radio;
pub mod scale;
pub mod secrets;
pub(crate) mod shm_icons;
mod signals;
pub mod storage;
pub mod sysinfo;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tray;
pub mod updates;
pub mod windows;
mod worker;
pub mod workspaces;

/// Reads and trims a sysfs attribute under `entry_dir`; missing or unreadable means absent.
pub fn read_attr(entry_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(entry_dir.join(name)).ok().map(|text| text.trim().to_string())
}

/// [`read_attr`] parsed as `T`; absent and unparsable both mean `None`.
pub fn read_parsed<T: std::str::FromStr>(entry_dir: &Path, name: &str) -> Option<T> {
    read_attr(entry_dir, name)?.parse().ok()
}

/// A udev watch on `subsystem`, readable on its uevents.
pub(crate) fn udev_watch(subsystem: &str) -> std::io::Result<tokio::io::unix::AsyncFd<udev::MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem(subsystem)?.listen()?;
    // SAFETY: the AsyncFd owns the socket, so its fd stays open and unchanged until the AsyncFd drops.
    Ok(unsafe { tokio::io::unix::AsyncFd::register(socket) }?)
}

/// Stores `next` and wakes `main.rs` when it differs from `state`; `false` once the receiver is
/// gone, so a reader loop can stop. The lock drops before the send: the receiver hydrates a
/// snapshot and must never wait on a task's mutex to do it.
pub(super) fn publish<S: PartialEq>(state: &Mutex<S>, events: &UnboundedSender<()>, next: S) -> bool {
    let mut current = state.lock().expect("capability state mutex poisoned");
    if *current == next {
        return true;
    }
    *current = next;
    drop(current);
    events.send(()).is_ok()
}

/// Sends `signal` once per item of `changes` until the stream ends or the receiver is gone.
pub(crate) async fn forward_until_closed<T: Clone>(
    mut changes: impl futures_util::Stream + Unpin,
    events: UnboundedSender<T>,
    signal: T,
) {
    while futures_util::StreamExt::next(&mut changes).await.is_some() && events.send(signal.clone()).is_ok() {}
}

/// Truncates to `max_bytes`, backing off to a UTF-8 boundary (bytes, not chars).
///
/// Every capability that copies a string out of a third party's D-Bus reply caps it here, so the
/// rule lives once: notifications for `Notify`'s properties and tray for the `StatusNotifierItem`
/// and DBusMenu text an arbitrary application supplies.
pub fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    input[..input.floor_char_boundary(max_bytes)].to_string()
}

/// The next agent prompt's request ID, unique across agents.
/// ponytail: after 2^64-2 requests, refuse more until Supervisor restarts; never reuse an id.
pub(crate) fn next_request_id() -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1)).ok().map(|id| id.to_string())
}

/// Whether `header`'s sender owns the well-known `name`. A bus lets any local process call an
/// exported agent; only the daemon it registered with is answered.
pub(crate) async fn sent_by_owner(
    bus: &zbus::Connection,
    header: &zbus::message::Header<'_>,
    name: &'static str,
) -> bool {
    let name = zbus::names::WellKnownName::from_static_str_unchecked(name).into();
    let owner = async { zbus::fdo::DBusProxy::new(bus).await?.get_name_owner(name).await };
    matches!((owner.await, header.sender()), (Ok(owner), Some(sender)) if owner.as_str() == sender.as_str())
}

/// Names leaving the bus (`new_owner == ""`), filtered by the bus so unrelated name changes never wake
/// the caller. Subscribe before scanning existing names to miss no departure; failure logs once and
/// says what `what` stays off.
pub(crate) async fn name_departures(
    dbus: &zbus::fdo::DBusProxy<'_>,
    what: &str,
) -> Option<zbus::fdo::NameOwnerChangedStream> {
    dbus.receive_name_owner_changed_with_args(&[(2, "")])
        .await
        .inspect_err(|err| shared::error!("failed to subscribe to NameOwnerChanged; {what}: {err}"))
        .ok()
}

/// Binds a macro-generated zbus proxy at `path`. A generated `<Proxy>::new` ties the proxy to
/// `&Connection` even though its builder clones the connection, so stored proxies go through the
/// builder to stay `'static`.
pub(crate) async fn bind<T>(connection: &zbus::Connection, path: zbus::zvariant::OwnedObjectPath) -> zbus::Result<T>
where
    T: zbus::proxy::Defaults + From<zbus::Proxy<'static>>,
{
    zbus::proxy::Builder::new(connection).path(path)?.build().await
}

/// An uncached `org.freedesktop.DBus.Properties` proxy for `destination`'s object at `path`; it has no
/// properties of its own, so the cache only matters for the `GetAll`s and signals it serves.
pub(crate) async fn properties_proxy<D, P>(
    connection: &zbus::Connection,
    destination: D,
    path: P,
) -> zbus::Result<zbus::fdo::PropertiesProxy<'static>>
where
    D: TryInto<zbus::names::BusName<'static>>,
    D::Error: Into<zbus::Error>,
    P: TryInto<zbus::zvariant::ObjectPath<'static>>,
    P::Error: Into<zbus::Error>,
{
    zbus::fdo::PropertiesProxy::builder(connection)
        .destination(destination)?
        .path(path)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::Capability;

    #[test]
    fn from_name_resolves_every_roster_entry_and_nothing_else() {
        assert_eq!(Capability::from_name("audio"), Some(Capability::Audio));
        assert_eq!(Capability::from_name("polkit"), Some(Capability::Polkit));
        assert_eq!(Capability::from_name("idle"), Some(Capability::Idle), "on the roster since ADR-0141");
        // `process` is command-addressable but never started; it must not resolve to a silent start.
        assert_eq!(Capability::from_name("process"), None);
        assert_eq!(Capability::from_name("screens"), None);
        assert_eq!(Capability::from_name(""), None);
    }

    #[test]
    fn truncate_utf8_bytes_caps_on_a_char_boundary() {
        // 'é' is 2 bytes, so a cap of 2 in "héllo" lands mid-character and must back off.
        for (input, cap, expected) in
            [("hello", 64, "hello"), ("hello world", 5, "hello"), ("héllo", 2, "h"), ("hello", 0, "")]
        {
            assert_eq!(truncate_utf8_bytes(input, cap), expected, "{input:?} cap {cap}");
        }
    }

    #[test]
    fn the_reconnect_delay_doubles_up_to_its_cap() {
        assert_eq!(next_delay(RETRY_FIRST), Duration::from_secs(2));
        assert_eq!(next_delay(Duration::from_secs(16)), RETRY_MAX);
        assert_eq!(next_delay(RETRY_MAX), RETRY_MAX);
    }

    /// Yields `items` units, then ends; polling past the end panics, so a forwarder that spins on
    /// an ended stream fails instead of hanging the test.
    fn ending(items: usize) -> impl futures_util::Stream<Item = ()> + Unpin {
        let mut polls = 0;
        futures_util::stream::poll_fn(move |_| {
            polls += 1;
            assert!(polls <= items + 1, "polled past the end");
            std::task::Poll::Ready((polls <= items).then_some(()))
        })
    }

    #[tokio::test]
    async fn a_forwarder_sends_once_per_item_and_stops_when_its_stream_ends() {
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        test_support::within(forward_until_closed(ending(2), events, 'x')).await;
        assert_eq!(received.recv().await, Some('x'));
        assert_eq!(received.recv().await, Some('x'));
        assert_eq!(received.recv().await, None, "exactly one send per item");
    }

    #[tokio::test]
    async fn a_forwarder_stops_once_nobody_listens() {
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        drop(received);
        let changes = futures_util::StreamExt::chain(futures_util::stream::iter([()]), futures_util::stream::pending());
        test_support::within(forward_until_closed(changes, events, ())).await;
    }
}

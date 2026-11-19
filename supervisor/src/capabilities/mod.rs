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

use std::marker::PhantomData;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::{IgnoredAny, MapAccess};
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::{Signature, Type};

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

pub mod android_connect;
pub mod appearance;
pub mod applications;
pub mod audio;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod clock;
pub mod conversation;
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
        return !events.is_closed();
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
/// rule lives once.
pub fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    input[..input.floor_char_boundary(max_bytes)].to_string()
}

/// Seconds since the Unix epoch at `now`, or 0 before it.
pub fn epoch_seconds(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0)
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

/// Seeds one peer-sent variant, yielding `None` where the sender's signature is not `T`'s.
///
/// A `zvariant::Value` holds an array as one `Value` per element, so an `ay` that reaches one
/// costs 72 bytes for every byte at a length the peer picks; this decodes straight to `T`.
/// `zvariant::as_value::optional` fails the whole call on a mismatch, which would drop a
/// notification or a tray item over a single malformed property; a mismatch is skipped instead.
pub(crate) struct Hint<T>(PhantomData<T>);

/// A variant on its own, such as a `Properties.Get` reply.
impl<T> Type for Hint<T> {
    const SIGNATURE: &'static Signature = &Signature::Variant;
}

impl<T> Hint<T> {
    pub(crate) fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'de, T: serde::Deserialize<'de> + Type> serde::de::DeserializeSeed<'de> for Hint<T> {
    type Value = Option<T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("Variant", &["signature", "value"], HintVisitor(PhantomData))
    }
}

struct HintVisitor<T>(PhantomData<T>);

impl<'de, T: serde::Deserialize<'de> + Type> serde::de::Visitor<'de> for HintVisitor<T> {
    type Value = Option<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a variant")
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

/// The keys of a peer-sent `a{sv}` that a type reads, read through [`Dict`].
pub(crate) trait DictFields: Default {
    /// Decodes `key`'s value into `self` (through [`Hint`]), or `false` to have [`Dict`] skip it.
    fn read<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error>;
}

/// An `a{sv}` decoded straight into `T`, as [`Hint`] decodes one variant: keys `T` does not read
/// are skipped with `IgnoredAny`, so they cost no memory. A repeated key keeps the last, as a
/// `HashMap` would, where serde's derive would refuse the whole dict.
pub(crate) struct Dict<T>(pub(crate) T);

impl<T> Type for Dict<T> {
    const SIGNATURE: &'static Signature = &Signature::static_dict(&Signature::Str, &Signature::Variant);
}

impl<'de, T: DictFields> serde::Deserialize<'de> for Dict<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(DictVisitor(PhantomData))
    }
}

struct DictVisitor<T>(PhantomData<T>);

impl<'de, T: DictFields> serde::de::Visitor<'de> for DictVisitor<T> {
    type Value = Dict<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an a{sv} dict")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Dict<T>, A::Error> {
        let mut fields = T::default();
        while let Some(key) = map.next_key::<&str>()? {
            if !fields.read(key, &mut map)? {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Dict(fields))
    }
}

const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Every property of `proxy`'s interface by one `GetAll`, decoded as [`Dict`]. zbus's property
/// cache and generated getters decode an `OwnedValue` first, one `Value` per byte of an array.
pub(crate) async fn get_all<T: DictFields>(proxy: &zbus::Proxy<'_>) -> zbus::Result<T> {
    let (destination, interface) = (Some(proxy.destination()), proxy.interface().as_str());
    let reply = proxy.connection().call_method(destination, proxy.path(), Some(PROPERTIES), "GetAll", &interface);
    Ok(reply.await?.body().deserialize::<Dict<T>>()?.0)
}

/// One property by `Properties.Get`, decoded straight to `T` as [`get_all`] does: `Ok(None)` when
/// the peer sent another type. A reply that is not exactly one variant is an error, as zbus's
/// generated getters make it.
pub(crate) async fn get_property<T: serde::de::DeserializeOwned + Type>(
    proxy: &zbus::Proxy<'_>,
    name: &str,
) -> zbus::Result<Option<T>> {
    let (destination, interface) = (Some(proxy.destination()), proxy.interface().as_str());
    let reply =
        proxy.connection().call_method(destination, proxy.path(), Some(PROPERTIES), "Get", &(interface, name)).await?;
    let body = reply.body();
    if *body.signature() != Signature::Variant {
        return Err(zbus::Error::Failure(format!("Get {name} replied ({}), not one variant", body.signature())));
    }
    Ok(body.data().deserialize_with_seed(Hint::new())?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::Capability;

    #[test]
    fn epoch_seconds_is_exact() {
        assert_eq!(epoch_seconds(UNIX_EPOCH + Duration::from_secs(1_700_000_000)), 1_700_000_000);
    }

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
}

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

use tokio::sync::mpsc::UnboundedSender;

pub use latest_writes::{LatestWrites, Writer};
pub use lifecycle::Capabilities;
pub use signals::Signal;

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
pub mod files;
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
pub mod system;
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

/// Truncates to `max_bytes`, backing off to a UTF-8 boundary (bytes, not chars).
///
/// Every capability that copies a string out of a third party's D-Bus reply caps it here, so the
/// rule lives once: notifications for `Notify`'s properties and tray for the `StatusNotifierItem`
/// and DBusMenu text an arbitrary application supplies.
pub fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
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

/// Binds a macro-generated zbus proxy at `path`. A generated `<Proxy>::new` ties the proxy to
/// `&Connection` even though its builder clones the connection, so stored proxies go through the
/// builder to stay `'static`.
pub async fn bind<T>(connection: &zbus::Connection, path: zbus::zvariant::OwnedObjectPath) -> zbus::Result<T>
where
    T: zbus::proxy::Defaults + From<zbus::Proxy<'static>>,
{
    zbus::proxy::Builder::new(connection).path(path)?.build().await
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
    }

    #[test]
    fn startable_rejects_a_name_this_supervisor_builds_nothing_for() {
        // `process` is command-addressable but never started; it must not resolve to a silent
        // start.
        assert_eq!(Capability::from_name("process"), None);
        assert_eq!(Capability::from_name("screens"), None);
        assert_eq!(Capability::from_name(""), None);
    }

    #[test]
    fn truncate_utf8_bytes_is_a_no_op_under_the_cap() {
        assert_eq!(truncate_utf8_bytes("hello", 64), "hello");
    }

    #[test]
    fn truncate_utf8_bytes_truncates_ascii_at_the_exact_cap() {
        assert_eq!(truncate_utf8_bytes("hello world", 5), "hello");
    }

    #[test]
    fn truncate_utf8_bytes_never_splits_a_multibyte_char() {
        // "héllo" -- 'é' is 2 bytes (0xc3 0xa9); a byte cap landing mid-character must back off.
        let input = "héllo";
        assert_eq!(input.len(), 6);
        // Cap of 2 bytes lands right in the middle of 'é' (byte 1 is not a char boundary).
        let truncated = truncate_utf8_bytes(input, 2);
        assert_eq!(truncated, "h");
        assert!(truncated.len() <= 2);
    }

    #[test]
    fn truncate_utf8_bytes_handles_a_cap_of_zero() {
        assert_eq!(truncate_utf8_bytes("hello", 0), "");
    }
}

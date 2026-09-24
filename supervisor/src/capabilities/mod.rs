//! Lazy capability startup, signal routing, and command dispatch (ADR-0037, ADR-0070, ADR-0076).
//!
//! [`Capabilities`] owns every controller and channel, so `main.rs` does not grow per capability;
//! exhaustive start, push, and dispatch matches fail to compile at a missing arm.
//!
//! No trait or boxed registry: controllers differ (`build_state`, `snapshot`, async
//! `handle_signal`, or a channel), and `main.rs` needs concrete types. ADR-0037 decision 3 chose
//! static calls.
//!
//! **One flat child module per roster entry.** Shared names cover `shared::Capability`,
//! `mantle.<name>`, and command `capability`. [`read_attr`] lives here, `polkit` in
//! `crate::polkit`, and `shm_icons` beside its two consumers (ADR-0076).

use std::path::Path;

pub use lifecycle::Capabilities;
pub use signals::Signal;

/// Every Supervisor bus gets the 25s call timeout Qt, GDBus and libdbus default to; zbus has none
/// (ADR-0070 amendment).
pub async fn with_call_timeout(builder: zbus::Result<zbus::connection::Builder<'_>>) -> zbus::Result<zbus::Connection> {
    builder?.method_timeout(std::time::Duration::from_secs(25)).build().await
}

pub mod applications;
pub mod audio;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod files;
pub mod idle;
pub mod keyboard;
mod lifecycle;
pub mod lock;
pub mod mpris;
pub mod network;
pub mod notifications;
pub mod polkit;
pub mod power;
pub mod privacy;
pub mod processes;
pub mod scale;
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

/// A list that may be omitted or `nil`; mlua sends an empty Lua table as `{}`.
pub fn lua_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    match <Option<serde_json::Value> as serde::Deserialize>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Object(map)) if map.is_empty() => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value).map_err(serde::de::Error::custom),
    }
}

/// A string argument used as a key, where empty would name nothing.
pub fn non_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_empty() { Err(serde::de::Error::custom("expected a non-empty string")) } else { Ok(value) }
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

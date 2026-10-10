//! The pure half of `network:connect`: an intent and its secret shaped into NetworkManager's dict,
//! and a failed join's reason into display text. No D-Bus, so all of it tests without a peer.

use std::collections::HashMap;

use shared::Zeroize;
use zbus::zvariant::Value;

use super::proxies::{REASON_NO_SECRETS, REASON_SSID_NOT_FOUND, REASON_SUPPLICANT_TIMEOUT, REASON_USER_REQUESTED};

/// Failures before `NetworkController::connect` reaches NetworkManager. `Display` supplies both
/// the log line and `NetworkState::connect_error`, so each says the attempt never started.
#[derive(Debug)]
pub(super) enum ConnectError {
    UnknownWifiDevice(String),
    InvalidSecret(String),
    Dbus(zbus::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownWifiDevice(id) if id.is_empty() => write!(f, "no Wi-Fi device is present"),
            Self::UnknownWifiDevice(id) => write!(f, "Wi-Fi device {id:?} is unavailable"),
            Self::InvalidSecret(message) => write!(f, "secret is not valid UTF-8: {message}"),
            Self::Dbus(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<zbus::Error> for ConnectError {
    fn from(err: zbus::Error) -> Self {
        Self::Dbus(err)
    }
}

/// Maps the Wi-Fi device's `StateChanged` reason for a failed join to display text. The active
/// connection's reason is no use here: it is `DEVICE_DISCONNECTED` for every device failure.
///
/// Other reasons are wired, modem, or dependency failures. A Wi-Fi row cannot act on them, so they
/// collapse to "connection failed".
fn connect_error_text(reason: u32) -> &'static str {
    match reason {
        REASON_NO_SECRETS => "wrong password",
        REASON_SUPPLICANT_TIMEOUT => "connection timed out",
        REASON_SSID_NOT_FOUND => "network not found",
        REASON_USER_REQUESTED => "disconnected",
        _ => "connection failed",
    }
}

/// An activation's verdict: the error text, and whether NM rejected the key. `outcome` is `None`
/// when the Supervisor's own ceiling ran out, else `activation_outcome`'s result.
pub(super) fn activation_verdict(outcome: Option<Result<(), Option<u32>>>) -> (Option<String>, bool) {
    match outcome {
        None => (Some("connection timed out".to_string()), false),
        Some(Ok(())) => (None, false),
        Some(Err(reason)) => (
            Some(reason.map_or("connection failed", connect_error_text).to_string()),
            reason == Some(REASON_NO_SECRETS),
        ),
    }
}

/// The `network:connect(ssid, hidden)` intent plus `secure_submit` secret before zbus `Value`
/// wrapping, keeping this unit-testable without D-Bus. Empty means open (ADR-0029); non-empty
/// bytes must be UTF-8 for NM's string-valued `802-11-wireless-security.psk`, or fail as
/// [`ConnectError::InvalidSecret`] instead of being mangled.
#[derive(Clone, PartialEq)]
pub(super) struct ConnectionIntent {
    pub(super) ssid: String,
    pub(super) hidden: bool,
    /// Scrubbed when the intent drops, on every path including an error return (ADR-0005), matching
    /// what [`connection_intent`]'s invalid-UTF-8 arm does for the bytes it rejects. This is the
    /// Supervisor, which never restarts, on a heap measured never to return its pages.
    ///
    /// It does not reach the copy `zbus` makes inside the `Value` [`build_connection_dict`] builds;
    /// that allocation is the D-Bus layer's and not ours to scrub, and is shorter-lived than this.
    pub(super) psk: Option<shared::Zeroizing<String>>,
}

/// Hand-written so a password cannot reach a log line through `{:?}`, following
/// `shared::SecureSubmit`'s `Debug`. The derive would have printed it in full.
impl std::fmt::Debug for ConnectionIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionIntent")
            .field("ssid", &self.ssid)
            .field("hidden", &self.hidden)
            .field("psk", &format_args!("{}", if self.psk.is_some() { "<redacted>" } else { "none" }))
            .finish()
    }
}

pub(super) fn connection_intent(ssid: &str, hidden: bool, secret: &[u8]) -> Result<ConnectionIntent, ConnectError> {
    let psk = if secret.is_empty() {
        None
    } else {
        match String::from_utf8(secret.to_vec()) {
            Ok(psk) => Some(shared::Zeroizing::new(psk)),
            Err(err) => {
                // Zeroize the invalid UTF-8 password copy before propagating (ADR-0005/ADR-0014).
                // Capture the message first because `into_bytes` consumes the error.
                let message = err.to_string();
                let mut bytes = err.into_bytes();
                bytes.zeroize();
                return Err(ConnectError::InvalidSecret(message));
            }
        }
    };
    Ok(ConnectionIntent { ssid: ssid.to_string(), hidden, psk })
}

/// Builds the minimal `AddAndActivateConnection2` dict: security only for a secured intent,
/// and `hidden` only when `intent.hidden` is set.
pub(super) fn build_connection_dict(intent: &ConnectionIntent) -> HashMap<&str, HashMap<&str, Value<'_>>> {
    let mut dict: HashMap<&str, HashMap<&str, Value>> = HashMap::new();

    let mut connection: HashMap<&str, Value> = HashMap::new();
    connection.insert("id", Value::new(intent.ssid.as_str()));
    connection.insert("type", Value::new("802-11-wireless"));
    dict.insert("connection", connection);

    let mut wireless: HashMap<&str, Value> = HashMap::new();
    wireless.insert("ssid", Value::new(intent.ssid.as_bytes()));
    wireless.insert("mode", Value::new("infrastructure"));
    if intent.hidden {
        wireless.insert("hidden", Value::new(true));
    }
    dict.insert("802-11-wireless", wireless);

    if let Some(psk) = &intent.psk {
        let mut security: HashMap<&str, Value> = HashMap::new();
        security.insert("key-mgmt", Value::new("wpa-psk"));
        security.insert("psk", Value::new(psk.as_str()));
        dict.insert("802-11-wireless-security", security);
    }

    dict
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_intent_treats_an_empty_secret_as_open_and_rejects_non_utf8() {
        assert_eq!(connection_intent("HomeWifi", false, &[]).unwrap().psk, None);
        let secured = connection_intent("HomeWifi", false, b"hunter2").unwrap();
        assert_eq!(secured.psk.as_ref().map(|psk| psk.as_str()), Some("hunter2"));
        assert!(connection_intent("HomeWifi", false, &[0xFF, 0xFE]).is_err());
    }

    #[test]
    fn build_connection_dict_carries_the_ssid_and_only_the_security_and_hidden_flags_asked_for() {
        let intent = |ssid, hidden, secret: &[u8]| connection_intent(ssid, hidden, secret).unwrap();
        let (open, secured, hidden) =
            (intent("HomeWifi", false, &[]), intent("HomeWifi", false, b"hunter2"), intent("HiddenNet", true, &[]));
        let (open, secured, hidden) =
            (build_connection_dict(&open), build_connection_dict(&secured), build_connection_dict(&hidden));
        assert!(!open.contains_key("802-11-wireless-security"));
        assert!(!open["802-11-wireless"].contains_key("hidden"));
        assert_eq!(Vec::<u8>::try_from(open["802-11-wireless"]["ssid"].clone()).unwrap(), b"HomeWifi".to_vec());

        let security = &secured["802-11-wireless-security"];
        assert_eq!(String::try_from(security["key-mgmt"].clone()).unwrap(), "wpa-psk");
        assert_eq!(String::try_from(security["psk"].clone()).unwrap(), "hunter2");

        assert!(bool::try_from(hidden["802-11-wireless"]["hidden"].clone()).unwrap());
    }

    #[test]
    fn only_the_device_reason_for_a_bad_key_reopens_the_prompt() {
        // NM fails the device with `NO_SECRETS` for a bad PSK; the active connection only says
        // `DEVICE_DISCONNECTED`.
        let failed = |reason| activation_verdict(Some(Err(Some(reason))));
        let verdict = |error: &str, rejected_key| (Some(error.to_string()), rejected_key);
        assert_eq!(failed(REASON_NO_SECRETS), verdict("wrong password", true));
        assert_eq!(failed(REASON_SUPPLICANT_TIMEOUT), verdict("connection timed out", false));
        assert_eq!(failed(REASON_SSID_NOT_FOUND), verdict("network not found", false));
        assert_eq!(activation_verdict(Some(Err(None))), verdict("connection failed", false));
        assert_eq!(activation_verdict(None), verdict("connection timed out", false));
        assert_eq!(activation_verdict(Some(Ok(()))), (None, false));
        // 57 is `MODEM_FAILED`, a modem reason a wifi row cannot act on; 255 is not a reason at all.
        assert_eq!(connect_error_text(57), "connection failed");
        assert_eq!(connect_error_text(255), "connection failed");
    }

    /// The derive would have printed the password in full the first time an intent reached a log
    /// line or an `unwrap` panic message.
    #[test]
    fn debug_redacts_the_password_but_still_says_whether_there_is_one() {
        let secured = connection_intent("net", false, b"hunter2").unwrap();
        let rendered = format!("{secured:?}");
        assert!(!rendered.contains("hunter2"), "the password must not survive formatting: {rendered}");
        assert!(rendered.contains("<redacted>"), "but the field is still reported: {rendered}");
        assert!(rendered.contains("net"), "and everything that is not the password still prints");

        let open = connection_intent("net", false, b"").unwrap();
        assert!(format!("{open:?}").contains("none"), "an open network is distinguishable from a secured one");
    }
}

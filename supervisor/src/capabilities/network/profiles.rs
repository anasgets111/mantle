//! Saved NetworkManager profiles for `mantle.network`: reading them, the saved-SSID cache,
//! `network_profiles`, `forget`, wired autoconnect, and shaping a typed key into a saved profile.

use std::collections::HashMap;

use shared::state::network_profiles::{NetworkProfilesState, ProfileError, ProfileInfo};
use zbus::zvariant::{OwnedObjectPath, Value};

use super::devices::WifiDevice;
use super::edit::Settings;
use super::proxies::{ACTIVE_STATE_ACTIVATED, ACTIVE_STATE_ACTIVATING, DeviceProxy, SettingsConnectionProxy};
use super::{NetworkController, edit, root_object_path};
use crate::capabilities::bind;
use shared::{debug, warn};

/// `network_profiles`' sources: the walk's profiles by path, `(uuid, state)` per active connection,
/// and the last failed `_profile` action.
#[derive(Default)]
pub(super) struct Profiles {
    pub(super) list: Vec<(OwnedObjectPath, ProfileInfo)>,
    pub(super) active: Vec<(String, u32)>,
    pub(super) error: Option<ProfileError>,
}

impl Profiles {
    pub(super) fn snapshot(&self) -> NetworkProfilesState {
        let profiles = self
            .list
            .iter()
            .map(|(_, profile)| {
                let state = |wanted| self.active.iter().any(|(uuid, state)| *uuid == profile.uuid && *state == wanted);
                ProfileInfo {
                    active: state(ACTIVE_STATE_ACTIVATED),
                    activating: state(ACTIVE_STATE_ACTIVATING),
                    ..profile.clone()
                }
            })
            .collect();
        NetworkProfilesState { profiles, error: self.error.clone() }
    }
}

/// Whether `get_settings()` permits autoconnect. Missing means NetworkManager's default `true`
/// (ADR-0029: only explicit `false` disqualifies a profile).
pub(super) fn connection_wants_autoconnect(settings: &Settings) -> bool {
    edit::read(settings.get("connection"), "autoconnect").unwrap_or(true)
}

/// The SSID bytes of a Wi-Fi profile's `get_settings()`, or `None` for any other connection type.
pub(super) fn profile_ssid(settings: &Settings) -> Option<Vec<u8>> {
    edit::read(settings.get("802-11-wireless"), "ssid")
}

/// ponytail: the fallback misses NM policy restrictions; upgrade path: query NM compatibility
/// for hidden or out-of-range SSIDs.
fn profile_matches_device(settings: &Settings, id: &str, mac: Option<&str>) -> bool {
    let field = |key: &str| edit::read::<String>(settings.get("connection"), key);
    if field("type").is_some_and(|kind| kind != "802-11-wireless")
        || field("interface-name").is_some_and(|name| name != id)
    {
        return false;
    }
    let binding = settings.get("802-11-wireless").and_then(|section| section.get("mac-address"));
    binding.is_none_or(|binding| {
        mac.and_then(|mac| mac.split(':').map(|part| u8::from_str_radix(part, 16)).collect::<Result<Vec<_>, _>>().ok())
            .is_some_and(|bytes| Vec::<u8>::try_from(binding.clone()).ok().as_ref() == Some(&bytes))
    })
}

/// Changes only the PSK in a full `UpdateUnsaved` dict. Rejects key management modes that take
/// no PSK; forcing WPA-PSK would downgrade SAE or break 802.1X.
pub(super) fn merge_psk<'a>(settings: &'a Settings, psk: &'a str) -> Option<edit::Dict<'a>> {
    let key_mgmt = edit::read::<String>(settings.get("802-11-wireless-security"), "key-mgmt");
    if !matches!(key_mgmt.as_deref(), None | Some("wpa-psk" | "sae")) {
        return None;
    }
    let mut merged = edit::to_dict(settings);
    let security = merged.entry("802-11-wireless-security").or_default();
    security.entry("key-mgmt").or_insert_with(|| Value::new("wpa-psk"));
    security.insert("psk", Value::new(psk));
    Some(merged)
}

/// One saved profile matched by SSID: the path for `ActivateConnection`, its proxy, and the
/// settings already read by both callers.
pub(super) struct SavedProfile {
    pub(super) path: OwnedObjectPath,
    pub(super) connection: SettingsConnectionProxy<'static>,
    pub(super) settings: Settings,
}

impl NetworkController {
    /// Saved Wi-Fi profiles for `ssid`, with settings and the caller's log context.
    pub(super) async fn saved_profiles_for_ssid(&self, ssid: &[u8], context: &str) -> Vec<SavedProfile> {
        let mut profiles = self.wifi_profiles(&format!("{context}({ssid:?})")).await;
        profiles.retain(|profile| profile_ssid(&profile.settings).is_some_and(|bytes| bytes == ssid));
        profiles
    }

    pub(super) async fn saved_profiles_for_device(
        &self,
        ssid: &[u8],
        wifi: &WifiDevice,
        hidden: bool,
    ) -> Vec<SavedProfile> {
        let mut profiles =
            self.read_profiles(wifi.device.available_connections().await.unwrap_or_default(), "connect").await;
        profiles.retain(|profile| profile_ssid(&profile.settings).is_some_and(|bytes| bytes == ssid));
        if !profiles.is_empty() {
            return profiles;
        }
        let in_range = self.access_point(&wifi.id, &String::from_utf8_lossy(ssid)).is_some();
        if !hidden && in_range {
            return profiles;
        }
        let mac = wifi.wireless.perm_hw_address().await.ok();
        profiles = self.saved_profiles_for_ssid(ssid, "connect").await;
        profiles.retain(|profile| profile_matches_device(&profile.settings, &wifi.id, mac.as_deref()));
        profiles
    }

    /// Every saved profile. Unreadable profiles are logged under `context` and skipped.
    async fn list_all_profiles(&self, context: &str) -> Vec<SavedProfile> {
        let paths = match self.settings.list_connections().await {
            Ok(paths) => paths,
            Err(err) => {
                debug!("{context} failed to list connections: {err}");
                return Vec::new();
            }
        };
        self.read_profiles(paths, context).await
    }

    /// Every saved Wi-Fi profile. Unreadable profiles are logged under `context` and skipped.
    async fn wifi_profiles(&self, context: &str) -> Vec<SavedProfile> {
        let mut profiles = self.list_all_profiles(context).await;
        profiles.retain(|profile| profile_ssid(&profile.settings).is_some());
        profiles
    }

    /// Binds and reads each profile at `paths`. Unreadable profiles are logged under `context` and
    /// skipped.
    pub(super) async fn read_profiles(&self, paths: Vec<OwnedObjectPath>, context: &str) -> Vec<SavedProfile> {
        let reads = paths.into_iter().map(|path| async move {
            let connection = match bind::<SettingsConnectionProxy>(&self.connection, path.clone()).await {
                Ok(connection) => connection,
                Err(err) => {
                    debug!("{context} failed to bind connection {path}: {err}");
                    return None;
                }
            };
            match connection.get_settings().await {
                Ok(settings) => Some(SavedProfile { path, connection, settings }),
                Err(err) => {
                    debug!("{context} failed to read settings for {path}: {err}");
                    None
                }
            }
        });
        futures_util::future::join_all(reads).await.into_iter().flatten().collect()
    }

    /// Replaces the saved-SSID and `network_profiles` caches with one `ListConnections` walk.
    pub(super) async fn refresh_profiles(&self) {
        let (mut ssids, mut list) = (HashMap::new(), Vec::new());
        for profile in self.list_all_profiles("saved profiles").await {
            if let Some(ssid) = profile_ssid(&profile.settings) {
                ssids.insert(profile.path.clone(), ssid);
            }
            if let Some(info) = edit::project(&profile.settings) {
                list.push((profile.path, info));
            }
        }
        list.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        *self.saved_ssids.lock().expect("mutex poisoned") = ssids;
        self.profiles.lock().expect("mutex poisoned").list = list;
    }

    /// Re-reads the one profile NetworkManager reported `Updated` into the same caches.
    pub(super) async fn refresh_profile(&self, path: OwnedObjectPath) {
        let Some(profile) = self.read_profiles(vec![path.clone()], "updated profile").await.pop() else { return };
        let settings = &profile.settings;
        let mut ssids = self.saved_ssids.lock().expect("mutex poisoned");
        match profile_ssid(settings) {
            Some(ssid) => ssids.insert(path.clone(), ssid),
            None => ssids.remove(&path),
        };
        drop(ssids);
        let mut profiles = self.profiles.lock().expect("mutex poisoned");
        profiles.list.retain(|(listed, _)| *listed != path);
        profiles.list.extend(edit::project(settings).map(|info| (path, info)));
        profiles.list.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    }

    /// Re-reads which profiles are active, after an activation changed.
    pub(super) async fn read_profile_activity(&self) {
        match self.active_connections().await {
            Ok(active) => {
                let active = active.into_iter().map(|(_, uuid, state)| (uuid, state)).collect();
                self.profiles.lock().expect("mutex poisoned").active = active;
            }
            // Kept, and read again on the next rebuild, rather than marking every profile inactive.
            Err(_) => self.activity.store(true, std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Sends `network_profiles` under its lock, so two pushes cannot land out of order.
    pub(super) fn push_profiles(&self) {
        let profiles = self.profiles.lock().expect("mutex poisoned");
        let _ = self.profile_states.send(profiles.snapshot());
    }

    /// Deletes every connection profile matching `ssid`.
    pub async fn forget(&self, ssid: &str) {
        let Ok(raw) = self.resolve_ssid(ssid).inspect_err(|refusal| warn!("forget({ssid:?}): {refusal}")) else {
            return;
        };
        for profile in self.saved_profiles_for_ssid(&raw, "forget").await {
            if let Err(err) = profile.connection.delete().await {
                warn!("forget({ssid:?}) failed to delete profile {}: {err}", profile.path);
            }
        }
    }

    pub(super) async fn activate_autoconnect_profile(&self, device: &DeviceProxy<'_>, device_path: &OwnedObjectPath) {
        let profile = match self.find_autoconnect_profile(device).await {
            Ok(profile) => profile,
            Err(err) => {
                debug!("failed to inspect connections for ethernet device {device_path}: {err}");
                return;
            }
        };
        let Some(conn_path) = profile else {
            // No profile exists; D-Bus cannot create one here (ADR-0029).
            return;
        };
        if let Err(err) = self.nm.activate_connection(&conn_path, device_path, &root_object_path()).await {
            warn!("failed to activate ethernet profile {conn_path} on {device_path}: {err}");
        }
    }

    /// ponytail: reads every profile the device lists before picking the first autoconnect one; a
    /// wired port lists one or two.
    async fn find_autoconnect_profile(&self, device: &DeviceProxy<'_>) -> zbus::Result<Option<OwnedObjectPath>> {
        let profiles = self.read_profiles(device.available_connections().await?, "autoconnect").await;
        Ok(profiles
            .into_iter()
            .find(|profile| connection_wants_autoconnect(&profile.settings))
            .map(|profile| profile.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::network::tests::owned;

    fn settings_with(section: &str, key: &str, value: zbus::zvariant::OwnedValue) -> Settings {
        HashMap::from([(section.to_string(), HashMap::from([(key.to_string(), value)]))])
    }

    #[test]
    fn only_an_explicit_false_declines_autoconnect() {
        let setting = |value: bool| settings_with("connection", "autoconnect", owned(value));
        assert!(connection_wants_autoconnect(&HashMap::new()), "an absent key defaults to true");
        assert!(!connection_wants_autoconnect(&setting(false)));
        assert!(connection_wants_autoconnect(&setting(true)));
    }

    #[test]
    fn profile_ssid_and_device_bindings_filter_saved_connections() {
        let settings = settings_with("802-11-wireless", "ssid", owned(b"HomeWifi".to_vec()));
        assert_eq!(profile_ssid(&settings).as_deref(), Some(&b"HomeWifi"[..]));
        assert_eq!(profile_ssid(&HashMap::new()), None, "a profile without a wireless section has no SSID");
        let named = settings_with("connection", "interface-name", owned("wlan1"));
        assert!(!profile_matches_device(&named, "wlan0", None));
        assert!(profile_matches_device(&named, "wlan1", None));
        let bound = settings_with("802-11-wireless", "mac-address", owned(vec![0x12u8, 0x34]));
        assert!(!profile_matches_device(&bound, "wlan1", None));
        assert!(profile_matches_device(&bound, "wlan1", Some("12:34")));
    }

    #[test]
    fn merge_psk_passes_every_other_section_through_untouched() {
        // `Update` replaces the whole profile; dropping any section loses static addresses,
        // metrics, or autoconnect priority.
        let settings: Settings = HashMap::from([
            ("connection".to_string(), HashMap::from([("id".to_string(), owned("HomeWifi"))])),
            (
                "ipv4".to_string(),
                HashMap::from([
                    ("method".to_string(), owned("manual")),
                    ("dns".to_string(), owned(vec![0x0101_0101_u32])),
                    ("dns-data".to_string(), owned(vec!["dns+tls://1.1.1.1"])),
                ]),
            ),
        ]);

        let merged = merge_psk(&settings, "hunter2").unwrap();
        assert_eq!(String::try_from(merged["connection"]["id"].clone()).unwrap(), "HomeWifi");
        assert_eq!(String::try_from(merged["ipv4"]["method"].clone()).unwrap(), "manual");
        // The daemon would prefer legacy `dns`, which cannot carry the DNS-over-TLS server.
        assert!(merged["ipv4"].contains_key("dns-data") && !merged["ipv4"].contains_key("dns"));
    }

    #[test]
    fn merge_psk_adds_a_security_section_to_an_open_profile_and_replaces_an_existing_key() {
        // An open profile has no security section, so add `key-mgmt` beside `psk`.
        let settings = settings_with("802-11-wireless", "ssid", owned(b"HomeWifi".to_vec()));

        let merged = merge_psk(&settings, "hunter2").unwrap();
        let security = &merged["802-11-wireless-security"];
        assert_eq!(String::try_from(security["key-mgmt"].clone()).unwrap(), "wpa-psk");
        assert_eq!(String::try_from(security["psk"].clone()).unwrap(), "hunter2");

        let settings = settings_with("802-11-wireless-security", "psk", owned("stale"));

        let merged = merge_psk(&settings, "hunter2").unwrap();
        assert_eq!(String::try_from(merged["802-11-wireless-security"]["psk"].clone()).unwrap(), "hunter2");
    }

    #[test]
    fn merge_psk_keeps_sae_and_refuses_key_types_that_take_no_psk() {
        let with = |key_mgmt: &str| settings_with("802-11-wireless-security", "key-mgmt", owned(key_mgmt));
        let sae = with("sae");
        let merged = merge_psk(&sae, "hunter2").unwrap();
        assert_eq!(String::try_from(merged["802-11-wireless-security"]["key-mgmt"].clone()).unwrap(), "sae");
        for key_mgmt in ["wpa-eap", "owe", "none"] {
            assert!(merge_psk(&with(key_mgmt), "hunter2").is_none(), "{key_mgmt} takes no PSK");
        }
    }
}

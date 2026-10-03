//! Scanning and the deduplicated `available_networks` list.

use std::collections::{HashMap, HashSet};

use shared::{debug, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::proxies::AP_FLAGS_PRIVACY;
use super::{AccessPointInfo, NetworkController, NetworkSignal};

/// How many deduplicated APs [`dedup_and_top20`] keeps.
const MAX_AVAILABLE_NETWORKS: usize = 20;

/// Frequency band, or `None` outside the three Wi-Fi ranges.
pub(super) fn resolve_band(freq_mhz: u32) -> Option<&'static str> {
    match freq_mhz {
        2400..=2500 => Some("2.4 GHz"),
        4900..=5900 => Some("5 GHz"),
        5925..=7125 => Some("6 GHz"),
        _ => None,
    }
}

/// Privacy, WPA or RSN flags mark an AP as secured.
pub(super) fn access_point_is_secure(flags: u32, wpa_flags: u32, rsn_flags: u32) -> bool {
    flags & AP_FLAGS_PRIVACY != 0 || wpa_flags != 0 || rsn_flags != 0
}

/// Keeps the strongest reading per SSID and any reading's `active` flag. Active and saved SSIDs
/// outrank signal strength; SSID breaks ties across unordered map values.
/// ponytail: more than 20 saved SSIDs still truncate; exempt saved rows to lift this ceiling.
pub(super) fn dedup_and_top20(aps: Vec<AccessPointInfo>) -> Vec<AccessPointInfo> {
    let mut best: HashMap<String, AccessPointInfo> = HashMap::new();
    for ap in aps {
        best.entry(ap.ssid.clone())
            .and_modify(|existing| {
                let active = existing.active || ap.active;
                if ap.strength > existing.strength {
                    *existing = ap.clone();
                }
                existing.active = active;
            })
            .or_insert(ap);
    }
    let mut deduped: Vec<AccessPointInfo> = best.into_values().collect();
    deduped.sort_by(|left, right| {
        right
            .active
            .cmp(&left.active)
            .then(right.saved.cmp(&left.saved))
            .then(right.strength.cmp(&left.strength))
            .then_with(|| left.ssid.cmp(&right.ssid))
    });
    deduped.truncate(MAX_AVAILABLE_NETWORKS);
    deduped
}

/// One access point's last `GetAll`, held between rebuilds.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ApReading {
    ssid: Vec<u8>,
    strength: u8,
    secure: bool,
    band: &'static str,
}

impl ApReading {
    /// `None` without the SSID, strength or frequency; absent flags read as open.
    fn from_properties(mut all: HashMap<String, OwnedValue>) -> Option<Self> {
        fn take<T: TryFrom<OwnedValue>>(all: &mut HashMap<String, OwnedValue>, name: &str) -> Option<T> {
            all.remove(name).and_then(|value| T::try_from(value).ok())
        }
        let flag = |all: &mut HashMap<String, OwnedValue>, name| take::<u32>(all, name).unwrap_or(0);
        Some(Self {
            ssid: take(&mut all, "Ssid")?,
            strength: take(&mut all, "Strength")?,
            band: resolve_band(take(&mut all, "Frequency")?).unwrap_or_default(),
            secure: access_point_is_secure(
                flag(&mut all, "Flags"),
                flag(&mut all, "WpaFlags"),
                flag(&mut all, "RsnFlags"),
            ),
        })
    }

    /// An AP row; the caller supplies the device's association state.
    fn info(&self, active: bool, saved_ssids: &HashSet<Vec<u8>>) -> AccessPointInfo {
        AccessPointInfo {
            saved: saved_ssids.contains(&self.ssid),
            ssid: String::from_utf8_lossy(&self.ssid).into_owned(),
            strength: self.strength,
            secure: self.secure,
            band: self.band.to_string(),
            active,
        }
    }
}

/// One uncached `GetAll`: a cached proxy would subscribe to every in-range AP's `Strength`.
async fn read_access_point(connection: &zbus::Connection, path: &OwnedObjectPath) -> Option<ApReading> {
    let reply = async {
        zbus::fdo::PropertiesProxy::builder(connection)
            .destination("org.freedesktop.NetworkManager")?
            .path(path.clone())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?
            .get_all(zbus::names::InterfaceName::from_static_str_unchecked(
                "org.freedesktop.NetworkManager.AccessPoint",
            ))
            .await
            .map_err(zbus::Error::from)
    };
    match reply.await {
        Ok(all) => ApReading::from_properties(all),
        Err(err) => {
            debug!("failed to read access point {path}: {err}");
            None
        }
    }
}

impl NetworkController {
    pub async fn scan_device(&self, id: Option<&str>) {
        let Some(wifi) = self.wifi(id) else {
            match id {
                Some(id) => warn!("scan_device({id:?}): Wi-Fi device is unavailable"),
                None => debug!("scan: no Wi-Fi device is available"),
            }
            return;
        };
        let _ = self.events.send(NetworkSignal::ScanStarted(wifi.id.clone()));
        if let Err(err) = wifi.wireless.request_scan(HashMap::new()).await {
            debug!("RequestScan failed on {}: {err}", wifi.id);
            // A refused scan never moves `LastScan`.
            let _ = self.events.send(NetworkSignal::ScanCompleted(wifi.id));
        }
    }

    /// Reads new and associated APs, or all APs after a scan, then deduplicates and caps at 20.
    pub(super) async fn build_available_networks(
        &self,
        wifi: &super::devices::WifiDevice,
        scanned: bool,
    ) -> Vec<AccessPointInfo> {
        let active_path = wifi.wireless.active_access_point().await.ok();
        let ap_paths = match wifi.wireless.get_access_points().await {
            Ok(paths) => paths,
            Err(err) => {
                warn!("failed to list access points: {err}");
                return Vec::new();
            }
        };

        // The lock goes around, not across, the reads: it is a plain mutex and reading awaits.
        let stale: Vec<&OwnedObjectPath> = {
            let cache = self.access_points.lock().expect("mutex poisoned");
            let held = cache.get(&wifi.id);
            ap_paths
                .iter()
                .filter(|path| {
                    scanned || active_path.as_ref() == Some(*path) || !held.is_some_and(|held| held.contains_key(*path))
                })
                .collect()
        };
        let readings =
            futures_util::future::join_all(stale.iter().map(|path| read_access_point(&self.connection, path))).await;

        let available = wifi.device.available_connections().await.unwrap_or_default();
        let saved_ssids: HashSet<Vec<u8>> = {
            let profiles = self.saved_ssids.lock().expect("mutex poisoned");
            available.iter().filter_map(|path| profiles.get(path).cloned()).collect()
        };
        let in_range: HashSet<&OwnedObjectPath> = ap_paths.iter().collect();
        let mut cache = self.access_points.lock().expect("mutex poisoned");
        let held = cache.entry(wifi.id.clone()).or_default();
        for (path, reading) in stale.into_iter().zip(readings) {
            match reading {
                Some(reading) => held.insert(path.clone(), reading),
                None => held.remove(path), // unreadable: skipped, and read again next time
            };
        }
        held.retain(|path, _| in_range.contains(path));
        let aps = ap_paths
            .iter()
            .filter_map(|path| Some(held.get(path)?.info(active_path.as_ref() == Some(path), &saved_ssids)));
        dedup_and_top20(aps.collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ap(ssid: &str, strength: u8) -> AccessPointInfo {
        AccessPointInfo {
            ssid: ssid.to_string(),
            strength,
            secure: false,
            band: "2.4 GHz".to_string(),
            active: false,
            saved: false,
        }
    }

    #[test]
    fn an_access_point_reads_from_one_get_all_and_needs_its_ssid_strength_and_frequency() {
        let all = |pairs: Vec<(&str, OwnedValue)>| pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let reading = ApReading::from_properties(all(vec![
            ("Ssid", OwnedValue::try_from(zbus::zvariant::Value::from(b"home".to_vec())).unwrap()),
            ("Strength", OwnedValue::from(62u8)),
            ("Frequency", OwnedValue::from(5180u32)),
            ("RsnFlags", OwnedValue::from(0x100u32)),
        ]))
        .unwrap();
        let mut saved = HashSet::new();
        saved.insert(b"home".to_vec());
        let info = reading.info(true, &saved);
        assert_eq!((info.ssid.as_str(), info.strength, info.band.as_str()), ("home", 62, "5 GHz"));
        assert!(info.secure && info.saved && info.active);

        let no_ssid = all(vec![("Strength", OwnedValue::from(62u8)), ("Frequency", OwnedValue::from(5180u32))]);
        assert_eq!(ApReading::from_properties(no_ssid), None, "skipped rather than listed nameless");
    }

    #[test]
    fn resolve_band_maps_each_spec_range() {
        assert_eq!(resolve_band(2400), Some("2.4 GHz"));
        assert_eq!(resolve_band(2450), Some("2.4 GHz"));
        assert_eq!(resolve_band(2500), Some("2.4 GHz"));
        assert_eq!(resolve_band(4900), Some("5 GHz"));
        assert_eq!(resolve_band(5180), Some("5 GHz"));
        assert_eq!(resolve_band(5900), Some("5 GHz"));
        assert_eq!(resolve_band(5925), Some("6 GHz"));
        assert_eq!(resolve_band(6200), Some("6 GHz"));
        assert_eq!(resolve_band(7125), Some("6 GHz"));
    }

    #[test]
    fn resolve_band_is_none_outside_every_range() {
        assert_eq!(resolve_band(0), None);
        assert_eq!(resolve_band(2399), None);
        assert_eq!(resolve_band(2501), None, "the gap between 2.4 GHz and 5 GHz");
        assert_eq!(resolve_band(5901), None, "the gap between 5 GHz and 6 GHz");
        assert_eq!(resolve_band(7126), None);
    }

    #[test]
    fn access_point_is_secure_needs_privacy_or_a_wpa_or_rsn_flag() {
        assert!(!access_point_is_secure(0, 0, 0), "a fully open network");
        assert!(access_point_is_secure(AP_FLAGS_PRIVACY, 0, 0), "WEP privacy alone");
        assert!(access_point_is_secure(0, 0b0000_0100, 0), "only WPA flags");
        assert!(access_point_is_secure(0, 0, 0b0000_0100), "only RSN flags");
    }

    #[test]
    fn dedup_and_top20_keeps_strength_and_any_active_flag() {
        let result = dedup_and_top20(vec![ap("home", 40), ap("home", 90), ap("home", 60)]);
        assert_eq!(result, vec![ap("home", 90)]);
        let mut connected = ap("home", 58);
        connected.active = true;
        for pair in [[ap("home", 62), connected.clone()], [connected, ap("home", 62)]] {
            let merged = dedup_and_top20(pair.to_vec());
            assert_eq!((merged[0].strength, merged[0].active), (62, true));
        }
    }

    #[test]
    fn dedup_and_top20_keeps_connected_and_saved_networks_above_stronger_neighbours() {
        let mut aps: Vec<AccessPointInfo> = (0..25).map(|i| ap(&format!("neighbour{i}"), 50 + i as u8)).collect();
        let mut connected = ap("home", 20);
        connected.active = true;
        aps.push(connected);
        let mut office = ap("office", 20);
        office.saved = true;
        aps.push(office);
        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert_eq!(merged[0].ssid, "home");
        assert!(merged[0].active);
        assert_eq!(merged[1].ssid, "office");
    }

    #[test]
    fn dedup_and_top20_sorts_by_strength_then_ssid() {
        let result = dedup_and_top20(vec![ap("weak", 10), ap("strong", 90), ap("mid", 50)]);
        assert_eq!(result.iter().map(|a| a.ssid.as_str()).collect::<Vec<_>>(), vec!["strong", "mid", "weak"]);
        let aps: Vec<AccessPointInfo> = ["delta", "alpha", "hotel", "charlie", "golf", "bravo", "foxtrot", "echo"]
            .iter()
            .map(|ssid| ap(ssid, 55))
            .collect();

        let merged = dedup_and_top20(aps);
        let names: Vec<&str> = merged.iter().map(|a| a.ssid.as_str()).collect();
        assert_eq!(names, ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]);
    }

    #[test]
    fn dedup_and_top20_caps_at_the_strongest_20_with_deterministic_ties() {
        let mut aps: Vec<AccessPointInfo> = (0..19).map(|i| ap(&format!("strong{i}"), 90)).collect();
        aps.insert(0, ap("weak", 1));
        aps.push(ap("zulu", 40));
        aps.push(ap("kilo", 40));
        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert_eq!(merged[19].ssid, "kilo");
        assert!(!merged.iter().any(|ap| ap.ssid == "weak" || ap.ssid == "zulu"));
    }
}

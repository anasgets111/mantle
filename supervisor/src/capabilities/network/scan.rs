//! Access-point scanning for `mantle.network`: `RequestScan`, the held access-point readings, and
//! the deduplicated, capped `available_networks` list `build_state` reads.

use std::collections::{HashMap, HashSet};

use shared::{debug, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::proxies::AP_FLAGS_PRIVACY;
use super::{AccessPointInfo, NetworkController, NetworkSignal};

/// How many deduplicated APs [`dedup_and_top20`] keeps.
const MAX_AVAILABLE_NETWORKS: usize = 20;

/// `[2400, 2500]` -> `"2.4 GHz"`, `[4900, 5900]` -> `"5 GHz"`, `[5925, 7125]` -> `"6 GHz"`.
/// Real Wi-Fi hardware falls inside one range, so `None` is honest "no band", not a
/// guessed default.
pub(super) fn resolve_band(freq_mhz: u32) -> Option<&'static str> {
    match freq_mhz {
        2400..=2500 => Some("2.4 GHz"),
        4900..=5900 => Some("5 GHz"),
        5925..=7125 => Some("6 GHz"),
        _ => None,
    }
}

/// Whether an AP requires a key: `PRIVACY` alone signals WEP; non-empty RSN (WPA2/3) or WPA1
/// key-management flags signal the other secured cases.
pub(super) fn access_point_is_secure(flags: u32, wpa_flags: u32, rsn_flags: u32) -> bool {
    flags & AP_FLAGS_PRIVACY != 0 || wpa_flags != 0 || rsn_flags != 0
}

/// Merges duplicate SSIDs by highest strength, then serializes the connected one plus the strongest
/// 19. Equal-strength duplicates keep the first sighting; a tie is only between distinct
/// BSSIDs broadcasting the same SSID, so either choice is equally correct.
///
/// `active` sorts ahead of strength because `build_state` reads `ssid` and `strength` here. Without
/// it, an association weaker than 20 neighbours is truncated and an online machine reports no
/// association. Dense apartment RF reaches 20 SSIDs easily.
///
/// `saved` sorts next, so a saved network weaker than 20 neighbours still makes the
/// list. ponytail: more than 20 saved networks in range still
/// truncate by strength. Upgrade path: exempt saved rows from the cap.
///
/// Merge `active` rather than carrying the winner's flag. NetworkManager once exposed two AP
/// objects for one SSID at the same BSSID, strengths 62 and 58, with `ActiveAccessPoint` naming the
/// 58. Keeping the stronger object dropped the flag and showed a connected machine as "offline".
///
/// Strength belongs to an AP object; `active` belongs to the associated SSID. The strongest
/// sighting supplies the numbers, and any sighting supplies the flag.
///
/// SSID is the last tiebreak because `HashMap::into_values` reshuffles as APs come and go. Stable
/// sorting then prevents equal-strength rows from swapping, including at the 20th-place cutoff.
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

/// `ssid`: `"Ethernet"` for a wired default route, the associated AP's name otherwise,
/// and `None` when neither holds, which Lua reads as `nil` for offline.
///
/// Wired wins because `ssid` names what `NetworkState::connected` describes. A docked laptop may
/// stay joined to Wi-Fi, but the association is not carrying the default route.
pub(super) fn resolve_ssid(wired: bool, associated: Option<&AccessPointInfo>) -> Option<String> {
    match (wired, associated) {
        (true, _) => Some("Ethernet".to_string()),
        (false, Some(ap)) => Some(ap.ssid.clone()),
        (false, None) => None,
    }
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

    /// The row `network.available_networks` wants. `active` is a fact about the device's
    /// association, not the access point, so the caller passes it.
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
    /// Queues [`NetworkSignal::ScanStarted`] so `scanning` flips on initiation, before
    /// `RequestScan`. Only does so with Wi-Fi hardware; otherwise [`scan`](Self::scan) no-ops and
    /// `scanning` would stick at `true`.
    pub fn mark_scanning(&self) {
        if self.devices.lock().expect("mutex poisoned").wifi.is_some() {
            let _ = self.events.send(NetworkSignal::ScanStarted);
        }
    }

    /// Dispatches `RequestScan({})`. Missing Wi-Fi hardware is logged, not fatal.
    pub async fn scan(&self) {
        let Some(wifi) = self.wifi() else {
            debug!("scan() requested but no Wi-Fi device is present");
            return;
        };
        if let Err(err) = wifi.wireless.request_scan(HashMap::new()).await {
            debug!("RequestScan failed: {err}");
            // A refused scan never moves `LastScan`, so `mark_scanning`'s flag would hold until NM
            // scans on its own, minutes later on a joined radio and never on a powered-down one.
            let _ = self.events.send(NetworkSignal::ScanCompleted);
        }
    }

    /// Re-queries, deduplicates, and caps the current AP list at 20 by strength (ADR-0029:
    /// no debounce). Returns empty, not an error, without Wi-Fi hardware.
    ///
    /// Reads the associated AP and new ones each time, and every AP after a `scan`, which is when
    /// NetworkManager updates a neighbour's strength; the rest come from the last reading.
    pub async fn build_available_networks(&self, scanned: bool) -> Vec<AccessPointInfo> {
        let Some(wifi) = self.wifi() else {
            return Vec::new();
        };
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
            let held = self.access_points.lock().expect("mutex poisoned");
            ap_paths
                .iter()
                .filter(|path| scanned || active_path.as_ref() == Some(*path) || !held.contains_key(*path))
                .collect()
        };
        let readings =
            futures_util::future::join_all(stale.iter().map(|path| read_access_point(&self.connection, path))).await;

        let saved_ssids = self.saved_ssids.lock().expect("mutex poisoned").clone();
        let in_range: HashSet<&OwnedObjectPath> = ap_paths.iter().collect();
        let mut held = self.access_points.lock().expect("mutex poisoned");
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
    fn dedup_and_top20_keeps_the_highest_strength_entry_per_ssid() {
        let result = dedup_and_top20(vec![ap("home", 40), ap("home", 90), ap("home", 60)]);
        assert_eq!(result, vec![ap("home", 90)]);
    }

    #[test]
    fn dedup_and_top20_keeps_active_even_when_a_stronger_duplicate_is_not_the_connected_one() {
        // Real session: the same BSSID appeared as strengths 62 and 58, with `ActiveAccessPoint`
        // naming 58. Keeping 62 alone dropped `active` and showed "offline".
        let mut connected = ap("home", 58);
        connected.active = true;
        let merged = dedup_and_top20(vec![ap("home", 62), connected]);

        assert_eq!(merged.len(), 1);
        assert!(merged[0].active, "the connected SSID must stay marked connected");
        assert_eq!(merged[0].strength, 62, "and still report the strongest signal seen for it");
    }

    #[test]
    fn dedup_and_top20_keeps_active_regardless_of_which_duplicate_arrives_first() {
        let mut connected = ap("home", 58);
        connected.active = true;
        let merged = dedup_and_top20(vec![connected, ap("home", 62)]);

        assert_eq!(merged.len(), 1);
        assert!(merged[0].active);
        assert_eq!(merged[0].strength, 62);
    }

    #[test]
    fn dedup_and_top20_leaves_an_unconnected_ssid_unconnected() {
        // Merge the flag, but do not invent it for an unassociated SSID.
        let merged = dedup_and_top20(vec![ap("home", 62), ap("home", 58)]);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].active);
    }

    #[test]
    fn dedup_and_top20_keeps_the_connected_network_even_when_20_neighbours_are_stronger() {
        // `build_state` reads `ssid` and `strength` here; truncating the association reports an
        // online machine as joined to nothing.
        let mut aps: Vec<AccessPointInfo> = (0..25).map(|i| ap(&format!("neighbour{i}"), 50 + i as u8)).collect();
        let mut connected = ap("home", 20);
        connected.active = true;
        aps.push(connected);

        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert!(merged[0].active, "the connected network leads the list");
        assert_eq!(merged[0].ssid, "home");
    }

    #[test]
    fn dedup_and_top20_keeps_a_saved_network_even_when_20_neighbours_are_stronger() {
        let mut aps: Vec<AccessPointInfo> = (0..25).map(|i| ap(&format!("neighbour{i}"), 50 + i as u8)).collect();
        let mut office = ap("office", 20);
        office.saved = true;
        aps.push(office);

        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert_eq!(merged[0].ssid, "office", "a saved network outranks every unsaved one");
    }

    #[test]
    fn dedup_and_top20_sorts_by_strength_descending() {
        let result = dedup_and_top20(vec![ap("weak", 10), ap("strong", 90), ap("mid", 50)]);
        assert_eq!(result.iter().map(|a| a.ssid.as_str()).collect::<Vec<_>>(), vec!["strong", "mid", "weak"]);
    }

    #[test]
    fn dedup_and_top20_breaks_strength_ties_by_ssid_so_the_order_is_deterministic() {
        // `HashMap::into_values` makes the input order nondeterministic as APs change. Eight equal
        // strengths make an accidental pass a one-in-40320 shot.
        let aps: Vec<AccessPointInfo> = ["delta", "alpha", "hotel", "charlie", "golf", "bravo", "foxtrot", "echo"]
            .iter()
            .map(|ssid| ap(ssid, 55))
            .collect();

        let merged = dedup_and_top20(aps);
        let names: Vec<&str> = merged.iter().map(|a| a.ssid.as_str()).collect();
        assert_eq!(names, ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]);
    }

    #[test]
    fn dedup_and_top20_cuts_a_boundary_tie_by_ssid_rather_than_by_luck() {
        // Nineteen strong entries and two tied for slot 20 must yield the same row on every
        // rebuild; otherwise a drawn list flickers without a radio change.
        let mut aps: Vec<AccessPointInfo> = (0..19).map(|i| ap(&format!("strong{i}"), 90)).collect();
        aps.push(ap("zulu", 40));
        aps.push(ap("kilo", 40));

        let merged = dedup_and_top20(aps);
        assert_eq!(merged.len(), 20);
        assert_eq!(merged[19].ssid, "kilo", "the alphabetically-first of the tied pair keeps the last slot");
    }

    #[test]
    fn dedup_and_top20_keeps_the_20_strongest_not_just_the_first_20() {
        let mut aps: Vec<AccessPointInfo> = (0..30).map(|i| ap(&format!("ap{i}"), i as u8)).collect();
        // Reverse the input so a naive "take the first 20" implementation fails.
        aps.reverse();
        let result = dedup_and_top20(aps);
        assert_eq!(result.len(), 20);
        assert!(result.iter().all(|a| a.strength >= 10), "must keep the strongest 20, not the first 20 seen");
    }

    #[test]
    fn resolve_ssid_names_the_associated_network_over_wi_fi() {
        assert_eq!(resolve_ssid(false, Some(&ap("home", 70))), Some("home".to_string()));
    }

    #[test]
    fn resolve_ssid_says_ethernet_even_while_wi_fi_stays_associated() {
        // Both links stay joined, but `connected` describes the cable's default route.
        assert_eq!(resolve_ssid(true, Some(&ap("home", 70))), Some("Ethernet".to_string()));
    }

    #[test]
    fn resolve_ssid_is_none_when_nothing_is_joined() {
        assert_eq!(resolve_ssid(false, None), None);
    }
}

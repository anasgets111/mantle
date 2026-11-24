//! Saved-profile settings as `network_profiles` reads them, and `update_profile`'s merge of an edit
//! into NetworkManager's settings dict.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use shared::action::{IpEdit, ProfileEdit};
use shared::state::network_profiles::{IpSettings, Metered, ProfileInfo, ProfileKind};
use zbus::zvariant::{OwnedValue, Value};

use super::profiles::{connection_wants_autoconnect, profile_ssid};

/// ponytail: `OwnedValue` holds an `ay` as one `Value` per byte; NetworkManager is the only sender; upgrade: typed fields.
pub(super) type Settings = HashMap<String, HashMap<String, OwnedValue>>;
/// A settings dict for `Update2`, `UpdateUnsaved` or `Reapply`.
pub(super) type Dict<'a> = HashMap<&'a str, HashMap<&'a str, Value<'a>>>;

/// Deprecated keys the daemon prefers over their replacement whenever both arrive
/// (`_nm_setting_use_legacy_property`), while `GetSettings` returns both. Legacy `dns` also drops
/// DNS-over-TLS servers, so a dict sent back must leave these out.
const LEGACY_TWINS: [(&str, &str); 3] = [("dns", "dns-data"), ("addresses", "address-data"), ("routes", "route-data")];

pub(super) fn read<T: TryFrom<OwnedValue>>(section: Option<&HashMap<String, OwnedValue>>, key: &str) -> Option<T> {
    T::try_from(section?.get(key)?.clone()).ok()
}

/// A profile as `network_profiles` lists it, `active` and `activating` unset; `None` for loopback or a
/// profile without a UUID or name.
pub(super) fn project(settings: &Settings) -> Option<ProfileInfo> {
    let connection = settings.get("connection");
    let kind = match read::<String>(connection, "type")?.as_str() {
        "802-3-ethernet" => ProfileKind::Ethernet,
        "802-11-wireless" => ProfileKind::Wifi,
        "vpn" => ProfileKind::Vpn,
        "wireguard" => ProfileKind::Wireguard,
        "loopback" => return None,
        _ => ProfileKind::Other,
    };
    Some(ProfileInfo {
        uuid: read(connection, "uuid")?,
        id: read(connection, "id")?,
        kind,
        ssid: profile_ssid(settings).map(|ssid| String::from_utf8_lossy(&ssid).into_owned()),
        interface: read(connection, "interface-name"),
        autoconnect: connection_wants_autoconnect(settings),
        autoconnect_priority: read(connection, "autoconnect-priority").unwrap_or(0),
        metered: match read::<i32>(connection, "metered") {
            Some(1) => Metered::Yes,
            Some(2) => Metered::No,
            _ => Metered::Unknown,
        },
        active: false,
        activating: false,
        ipv4: settings.get("ipv4").map(|section| ip_settings(section, false)),
        ipv6: settings.get("ipv6").map(|section| ip_settings(section, true)),
    })
}

fn ip_settings(section: &HashMap<String, OwnedValue>, v6: bool) -> IpSettings {
    let section = Some(section);
    let addresses = read::<Vec<HashMap<String, OwnedValue>>>(section, "address-data").unwrap_or_default();
    let addresses = addresses
        .iter()
        .filter_map(|entry| {
            Some(format!("{}/{}", read::<String>(Some(entry), "address")?, read::<u32>(Some(entry), "prefix")?))
        })
        .collect();
    // `dns-data` arrived in NetworkManager 1.42; older daemons send only the binary `dns`.
    let dns = read::<Vec<String>>(section, "dns-data").unwrap_or_else(|| match v6 {
        false => read::<Vec<u32>>(section, "dns")
            .unwrap_or_default()
            .into_iter()
            // Network byte order, from a daemon on this host.
            .map(|address| Ipv4Addr::from(address.to_ne_bytes()).to_string())
            .collect(),
        true => read::<Vec<Vec<u8>>>(section, "dns")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|bytes| Some(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?).to_string()))
            .collect(),
    });
    IpSettings {
        method: read(section, "method").unwrap_or_default(),
        addresses,
        gateway: read(section, "gateway"),
        dns,
        dns_search: read(section, "dns-search").unwrap_or_default(),
        ignore_auto_dns: read(section, "ignore-auto-dns").unwrap_or(false),
    }
}

/// `settings` as a dict to send back, every key kept but the legacy twins.
pub(super) fn to_dict(settings: &Settings) -> Dict<'_> {
    let mut dict: Dict = settings
        .iter()
        .map(|(name, keys)| {
            (name.as_str(), keys.iter().map(|(key, value)| (key.as_str(), Value::from(value.clone()))).collect())
        })
        .collect();
    drop_legacy_twins(&mut dict);
    dict
}

fn drop_legacy_twins(dict: &mut Dict) {
    for section in dict.values_mut() {
        for (legacy, modern) in LEGACY_TWINS {
            if section.contains_key(modern) {
                section.remove(legacy);
            }
        }
    }
}

/// `settings` with `edit` applied; every key `edit` leaves out survives. Refuses a result
/// NetworkManager would reject for a reason the user can fix.
pub(super) fn merge<'a>(settings: &'a Settings, edit: &'a ProfileEdit) -> Result<Dict<'a>, &'static str> {
    let mut merged = to_dict(settings);
    let connection = merged.entry("connection").or_default();
    if let Some(autoconnect) = edit.autoconnect {
        connection.insert("autoconnect", Value::from(autoconnect));
    }
    if let Some(priority) = edit.autoconnect_priority {
        connection.insert("autoconnect-priority", Value::from(priority));
    }
    if let Some(metered) = edit.metered {
        connection.insert("metered", Value::from(metered as i32));
    }
    for (name, ip) in [("ipv4", &edit.ipv4), ("ipv6", &edit.ipv6)] {
        if let Some(ip) = ip {
            merge_ip(merged.entry(name).or_default(), ip)?;
        }
    }
    // Again: an edit may add the replacement of a key the profile held only in its legacy form.
    drop_legacy_twins(&mut merged);
    Ok(merged)
}

fn merge_ip<'a>(section: &mut HashMap<&'a str, Value<'a>>, edit: &'a IpEdit) -> Result<(), &'static str> {
    // serde spells the method as `IpSettings.method` reads it.
    if let Some(serde_json::Value::String(method)) = edit.method.and_then(|method| serde_json::to_value(method).ok()) {
        section.insert("method", Value::from(method));
    }
    if let Some(addresses) = &edit.addresses {
        // The action decoder checked each entry's shape.
        let data: Vec<HashMap<&str, Value>> = addresses
            .iter()
            .filter_map(|text| {
                let (address, prefix) = text.split_once('/')?;
                Some(HashMap::from([
                    ("address", Value::from(address)),
                    ("prefix", Value::from(prefix.parse::<u32>().ok()?)),
                ]))
            })
            .collect();
        section.insert("address-data", Value::from(data));
    }
    match edit.gateway.as_deref() {
        Some("") => drop(section.remove("gateway")),
        Some(gateway) => drop(section.insert("gateway", Value::from(gateway))),
        None => {}
    }
    if let Some(dns) = &edit.dns {
        section.insert("dns-data", Value::from(dns.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    if let Some(search) = &edit.dns_search {
        section.insert("dns-search", Value::from(search.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    if let Some(ignore) = edit.ignore_auto_dns {
        section.insert("ignore-auto-dns", Value::from(ignore));
    }
    let addressed = matches!(section.get("address-data"), Some(Value::Array(data)) if !data.is_empty());
    if matches!(section.get("method"), Some(Value::Str(method)) if method.as_str() == "manual") && !addressed {
        return Err("the manual method needs an address");
    }
    if section.contains_key("gateway") && !addressed {
        return Err("a gateway needs an address");
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use shared::action::IpMethod;

    use super::*;
    use crate::capabilities::network::tests::owned;

    fn section<const N: usize>(keys: [(&str, OwnedValue); N]) -> HashMap<String, OwnedValue> {
        keys.into_iter().map(|(key, value)| (key.to_string(), value)).collect()
    }

    /// `GetSettings` for a profile made with `nmcli con add type ethernet ... ipv4.dns
    /// "dns+tls://1.1.1.1#one.one.one.one 9.9.9.9" ipv4.routes "198.51.100.0/24 192.0.2.1 table=200"`
    /// on NetworkManager 1.58.1: each legacy key drops what it cannot carry.
    fn recorded() -> Settings {
        let address = HashMap::from([("address", Value::from("192.0.2.5")), ("prefix", Value::from(24u32))]);
        let route = HashMap::from([
            ("dest", Value::from("198.51.100.0")),
            ("prefix", Value::from(24u32)),
            ("next-hop", Value::from("192.0.2.1")),
            ("table", Value::from(200u32)),
        ]);
        HashMap::from([
            (
                "connection".into(),
                section([
                    ("id", owned("lab")),
                    ("uuid", owned("u-9")),
                    ("type", owned("802-3-ethernet")),
                    ("interface-name", owned("eth1")),
                    ("autoconnect", owned(false)),
                    ("permissions", owned(vec!["user:anas:"])),
                ]),
            ),
            (
                "ipv4".into(),
                section([
                    ("method", owned("manual")),
                    ("address-data", owned(vec![address])),
                    ("addresses", owned(vec![vec![84017344u32, 24, 16908480]])),
                    ("gateway", owned("192.0.2.1")),
                    ("dns", owned(vec![151587081u32])),
                    ("dns-data", owned(vec!["dns+tls://1.1.1.1#one.one.one.one", "9.9.9.9"])),
                    ("route-data", owned(vec![route])),
                    ("routes", owned(vec![vec![6566854u32, 24, 16908480, 0]])),
                ]),
            ),
            ("ipv6".into(), section([("method", owned("auto")), ("dns-search", owned(vec!["lan"]))])),
        ])
    }

    fn strings(value: &Value<'_>) -> Vec<String> {
        Vec::<String>::try_from(value.try_clone().unwrap()).unwrap()
    }

    #[test]
    fn an_unrelated_edit_drops_only_the_legacy_twins() {
        let settings = recorded();
        let edit = ProfileEdit { autoconnect: Some(true), ..ProfileEdit::default() };
        let merged = merge(&settings, &edit).unwrap();
        let ipv4 = &merged["ipv4"];
        for legacy in ["dns", "addresses", "routes"] {
            assert!(!ipv4.contains_key(legacy), "{legacy} would win over its replacement");
        }
        assert_eq!(strings(&ipv4["dns-data"]), ["dns+tls://1.1.1.1#one.one.one.one", "9.9.9.9"]);
        let kept = |section: &str, key: &str| Value::from(settings[section][key].clone()) == merged[section][key];
        for (section, key) in
            [("ipv4", "route-data"), ("ipv4", "address-data"), ("ipv4", "gateway"), ("ipv6", "dns-search")]
        {
            assert!(kept(section, key), "{section}.{key}");
        }
        assert!(kept("connection", "permissions") && kept("connection", "interface-name"));
        assert_eq!(merged["connection"]["autoconnect"], Value::from(true));
        assert_eq!(merged["connection"].len(), settings["connection"].len());
    }

    #[test]
    fn a_list_replaces_an_empty_one_clears_and_an_absent_one_keeps() {
        let settings = recorded();
        let dns =
            |dns: Vec<&str>| IpEdit { dns: Some(dns.into_iter().map(String::from).collect()), ..IpEdit::default() };
        let edit = ProfileEdit {
            autoconnect_priority: Some(-5),
            metered: Some(Metered::No),
            ipv4: Some(dns(vec!["1.1.1.1", "8.8.8.8"])),
            ipv6: Some(IpEdit { ignore_auto_dns: Some(true), ..dns(vec![]) }),
            ..ProfileEdit::default()
        };
        let merged = merge(&settings, &edit).unwrap();
        assert_eq!(strings(&merged["ipv4"]["dns-data"]), ["1.1.1.1", "8.8.8.8"]);
        assert!(strings(&merged["ipv6"]["dns-data"]).is_empty(), "`{{}}` clears");
        assert_eq!(strings(&merged["ipv6"]["dns-search"]), ["lan"], "an absent list keeps its value");
        assert_eq!(merged["ipv6"]["ignore-auto-dns"], Value::from(true));
        assert_eq!(merged["connection"]["autoconnect-priority"], Value::from(-5));
        assert_eq!(merged["connection"]["metered"], Value::from(2));

        let mut legacy_only = recorded();
        legacy_only.get_mut("ipv4").unwrap().remove("dns-data");
        let merged = merge(&legacy_only, &edit).unwrap();
        assert!(!merged["ipv4"].contains_key("dns"), "the daemon would keep the legacy servers");
    }

    #[test]
    fn addresses_and_gateway_are_checked_against_the_merged_profile() {
        let settings = recorded();
        let ipv4 = |edit: IpEdit| ProfileEdit { ipv4: Some(edit), ..ProfileEdit::default() };
        let edit =
            ipv4(IpEdit { addresses: Some(vec!["10.0.0.2/8".into()]), gateway: Some("".into()), ..IpEdit::default() });
        let merged = merge(&settings, &edit).unwrap();
        let data = Vec::<HashMap<String, OwnedValue>>::try_from(merged["ipv4"]["address-data"].try_clone().unwrap());
        let address = &data.unwrap()[0];
        assert_eq!(String::try_from(address["address"].clone()).unwrap(), "10.0.0.2");
        assert_eq!(u32::try_from(address["prefix"].clone()).unwrap(), 8);
        assert!(!merged["ipv4"].contains_key("gateway"), "`\"\"` removes the gateway");

        let cleared = ipv4(IpEdit { addresses: Some(vec![]), ..IpEdit::default() });
        assert_eq!(merge(&settings, &cleared).unwrap_err(), "the manual method needs an address");
        let auto = || IpEdit { method: Some(IpMethod::Auto), addresses: Some(vec![]), ..IpEdit::default() };
        assert_eq!(merge(&settings, &ipv4(auto())).unwrap_err(), "a gateway needs an address");
        let dhcp = ipv4(IpEdit { gateway: Some("".into()), ..auto() });
        assert_eq!(merge(&settings, &dhcp).unwrap()["ipv4"]["method"], Value::from("auto"));
    }

    #[test]
    fn a_profile_reads_its_typed_fields_and_old_daemons_binary_dns() {
        let mut settings = recorded();
        let lab = project(&settings).unwrap();
        assert_eq!((lab.uuid.as_str(), lab.id.as_str(), lab.kind), ("u-9", "lab", ProfileKind::Ethernet));
        assert_eq!((lab.interface.as_deref(), lab.autoconnect, lab.metered), (Some("eth1"), false, Metered::Unknown));
        let ipv4 = lab.ipv4.unwrap();
        assert_eq!((ipv4.method.as_str(), ipv4.gateway.as_deref()), ("manual", Some("192.0.2.1")));
        assert_eq!(
            (ipv4.addresses, ipv4.dns),
            (vec!["192.0.2.5/24".to_string()], vec!["dns+tls://1.1.1.1#one.one.one.one".to_string(), "9.9.9.9".into()])
        );

        // NetworkManager before 1.42 sends no `dns-data`.
        let ipv4 = settings.get_mut("ipv4").unwrap();
        ipv4.remove("dns-data");
        ipv4.insert("dns".into(), owned(vec![84017344u32]));
        let mut v6 = [0u8; 16];
        v6[..2].copy_from_slice(&[0x20, 0x01]);
        v6[15] = 1;
        settings.get_mut("ipv6").unwrap().insert("dns".into(), owned(vec![v6.to_vec()]));
        let connection = settings.get_mut("connection").unwrap();
        connection.insert("metered".into(), owned(1));
        let old = project(&settings).unwrap();
        assert_eq!(old.ipv4.unwrap().dns, ["192.0.2.5"]);
        assert_eq!(old.ipv6.unwrap().dns, ["2001::1"]);
        assert_eq!(old.metered, Metered::Yes);

        let connection = settings.get_mut("connection").unwrap();
        connection.extend([("type".into(), owned("wireguard")), ("metered".into(), owned(2))]);
        let tunnel = project(&settings).unwrap();
        assert_eq!((tunnel.kind, tunnel.metered), (ProfileKind::Wireguard, Metered::No));

        settings.get_mut("connection").unwrap().insert("type".into(), owned("loopback"));
        assert_eq!(project(&settings), None, "loopback is not listed");
    }
}

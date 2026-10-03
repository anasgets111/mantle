//! [`PowerController`] owns `mantle.power` and its write action.
//! See `power/mod.rs` for why the payload has four optional fields.

pub use shared::state::power::PowerState;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{Stream, StreamExt, stream, stream_select};
use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::publish;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;

/// `OnBattery` is a manager-wide answer across all UPower supplies, not a device property. This
/// matters for a docked laptop with two mains adapters; UPower aggregates it.
#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

/// The composite `DisplayDevice`, not `battery_BAT0`: UPower sums every battery there. `EnergyRate`
/// is a positive watt magnitude while charging or discharging; `mantle.power` wants no direction,
/// so configs needing it read `mantle.battery.state`.
#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait DisplayDevice {
    #[zbus(property)]
    fn energy_rate(&self) -> zbus::Result<f64>;
}

/// power-profiles-daemon. `ActiveProfile` is a writable property, so zbus generates its setter
/// from the getter. `Profiles` contains dictionaries with a `Profile` name plus unused driver
/// details; [`profile_names`] extracts the names.
#[zbus::proxy(interface = "org.freedesktop.UPower.PowerProfiles", assume_defaults = false)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn profiles(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

/// The daemon renamed `net.hadess.PowerProfiles` to `org.freedesktop.UPower.PowerProfiles` in
/// 0.20 while keeping the old name; try both, newest first.
const POWER_PROFILES_ENDPOINTS: [(&str, &str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
        "org.freedesktop.UPower.PowerProfiles",
    ),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles", "net.hadess.PowerProfiles"),
];

/// Extracts profile names from daemon descriptions. Missing or non-string `Profile`
/// entries are skipped; `power:set_profile(p)` validates against this list.
fn profile_names(profiles: &[HashMap<String, OwnedValue>]) -> Vec<String> {
    profiles
        .iter()
        .filter_map(|entry| entry.get("Profile"))
        .filter_map(|value| <&str>::try_from(value).ok())
        .map(str::to_string)
        .collect()
}

#[derive(Clone)]
pub struct PowerController {
    state: Arc<Mutex<PowerState>>,
    system_bus: zbus::Connection,
}

impl PowerController {
    /// Returns immediately; proxies build inside the spawned task because construction is an async
    /// round trip that would serialize `main.rs` startup.
    pub fn new(system_bus: zbus::Connection, events: UnboundedSender<()>) -> Self {
        let state = Arc::new(Mutex::new(PowerState::default()));
        tokio::spawn(run_power_task(system_bus.clone(), Arc::clone(&state), events));
        Self { state, system_bus }
    }

    pub fn snapshot(&self) -> PowerState {
        self.state.lock().expect("power state mutex poisoned").clone()
    }

    /// `power:set_profile(p)`. Build a fresh proxy per click, not a cached one that must survive a
    /// daemon restart. Do not update locally; the `PropertiesChanged` that follows reports both this
    /// write and external switches.
    pub async fn set_profile(&self, profile: &str) {
        match self.snapshot().profiles {
            None => {
                debug!("set_profile({profile}) before a profile list was read; ignored");
                return;
            }
            Some(profiles) if !profiles.iter().any(|name| name == profile) => {
                warn!("set_profile({profile}) names none of {profiles:?}; ignored");
                return;
            }
            Some(_) => {}
        }
        let Some(proxy) = connect_power_profiles(&self.system_bus).await else {
            debug!("set_profile({profile}) called but no power-profiles-daemon is reachable; ignored");
            return;
        };
        if let Err(err) = proxy.set_active_profile(profile).await {
            warn!("setting ActiveProfile to {profile} failed: {err}");
        }
    }
}

/// Tries [`POWER_PROFILES_ENDPOINTS`] in order; a proxy counts only after `active_profile()` reads
/// successfully because zbus contacts no service while building it.
async fn connect_power_profiles(system_bus: &zbus::Connection) -> Option<PowerProfilesProxy<'static>> {
    for (service, path, interface) in POWER_PROFILES_ENDPOINTS {
        let built = PowerProfilesProxy::builder(system_bus)
            .destination(service)
            .ok()?
            .path(path)
            .ok()?
            .interface(interface)
            .ok()?
            .cache_properties(CacheProperties::No)
            .build()
            .await;
        match built {
            Ok(proxy) => {
                if proxy.active_profile().await.is_ok() {
                    return Some(proxy);
                }
            }
            Err(err) => debug!("failed to build a proxy for {service}: {err}"),
        }
    }
    None
}

/// Reads every available field. Failed reads become `None`, not stale values: a stopped daemon is
/// worth showing, and this codebase has hit the live-looking stale-number failure three times.
async fn read_state(
    upower: &UPowerProxy<'static>,
    device: &DisplayDeviceProxy<'static>,
    profiles: Option<&PowerProfilesProxy<'static>>,
) -> PowerState {
    let mut state = PowerState {
        on_battery: upower.on_battery().await.ok(),
        energy_rate: device.energy_rate().await.ok(),
        ..PowerState::default()
    };
    if let Some(profiles) = profiles {
        state.active_profile = profiles.active_profile().await.ok();
        state.profiles = profiles.profiles().await.ok().map(|raw| profile_names(&raw));
    }
    state
}

/// Every `PropertiesChanged` for `proxy`'s object, from whichever process owns its name.
async fn properties_changed(proxy: &zbus::Proxy<'static>) -> zbus::Result<impl Stream<Item = ()> + use<>> {
    let properties = zbus::fdo::PropertiesProxy::builder(proxy.connection())
        .destination(proxy.destination().to_owned())?
        .path(proxy.path().to_owned())?
        .build()
        .await?;
    Ok(properties.receive_properties_changed().await?.map(drop))
}

/// Every change worth a re-read: a property of any of the three objects, or UPower or
/// power-profiles-daemon changing owner. A missing daemon merges an empty stream.
async fn changes(
    upower: &UPowerProxy<'static>,
    device: &DisplayDeviceProxy<'static>,
    profiles: Option<&PowerProfilesProxy<'static>>,
) -> zbus::Result<impl Stream<Item = ()> + use<>> {
    let (profile_properties, profile_owner) = match profiles {
        Some(proxy) => (
            Some(properties_changed(proxy.inner()).await?),
            Some(proxy.inner().receive_owner_changed().await?.map(drop)),
        ),
        None => (None, None),
    };
    // `stream_select!` can re-poll a stream that already ended, so each input is fused.
    Ok(stream_select!(
        properties_changed(upower.inner()).await?.fuse(),
        properties_changed(device.inner()).await?.fuse(),
        upower.inner().receive_owner_changed().await?.map(drop).fuse(),
        stream::iter(profile_properties).flatten().fuse(),
        stream::iter(profile_owner).flatten().fuse()
    ))
}

/// Reads once, pushes, then re-reads the whole payload on every [`changes`] item.
///
/// The proxies cache nothing: zbus's cache keeps the last owner's values after a restart, and one
/// built while its service was absent never answers. A property stream on an uncached proxy ends
/// at once, hence `PropertiesChanged` itself. UPower is followed even while absent, since a later
/// start brings it; a power-profiles-daemon absent at startup is not installed, as it is
/// activatable, so it is not probed again.
async fn run_power_task(system_bus: zbus::Connection, state: Arc<Mutex<PowerState>>, events: UnboundedSender<()>) {
    let profiles = connect_power_profiles(&system_bus).await;
    if profiles.is_none() {
        debug!("no power-profiles-daemon reachable; active_profile and profiles will not be reported this run");
    }
    // Subscribe before the first read. A charger unplugged during the round trip must not land
    // between a read and a subscription that does not exist yet.
    let subscribed = async {
        let upower = UPowerProxy::builder(&system_bus).cache_properties(CacheProperties::No).build().await?;
        let device = DisplayDeviceProxy::builder(&system_bus).cache_properties(CacheProperties::No).build().await?;
        let changes = changes(&upower, &device, profiles.as_ref()).await?;
        zbus::Result::Ok((upower, device, changes))
    };
    let (upower, device, changes) = match subscribed.await {
        Ok(subscribed) => subscribed,
        Err(err) => {
            error!("cannot follow UPower on the system bus, so mantle.power stays unset this run: {err}");
            return;
        }
    };
    let mut changes = std::pin::pin!(changes);

    *state.lock().expect("power state mutex poisoned") = read_state(&upower, &device, profiles.as_ref()).await;
    if events.send(()).is_err() {
        return;
    }
    while changes.next().await.is_some() {
        if !publish(&state, &events, read_state(&upower, &device, profiles.as_ref()).await) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;
    use zbus::zvariant::Value;

    use crate::capabilities::test_support::{PrivateBus, private_bus, properties as entry, within};

    struct FakeUPower {
        on_battery: bool,
    }

    #[zbus::interface(name = "org.freedesktop.UPower")]
    impl FakeUPower {
        #[zbus(property)]
        fn on_battery(&self) -> bool {
            self.on_battery
        }
    }

    struct FakeDisplayDevice {
        energy_rate: f64,
    }

    #[zbus::interface(name = "org.freedesktop.UPower.Device")]
    impl FakeDisplayDevice {
        #[zbus(property)]
        fn energy_rate(&self) -> f64 {
            self.energy_rate
        }
    }

    async fn serve_upower(bus: &PrivateBus, on_battery: bool, energy_rate: f64) -> zbus::Connection {
        bus.builder()
            .serve_at("/org/freedesktop/UPower", FakeUPower { on_battery })
            .unwrap()
            .serve_at("/org/freedesktop/UPower/devices/DisplayDevice", FakeDisplayDevice { energy_rate })
            .unwrap()
            .name("org.freedesktop.UPower")
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    /// upowerd restarts on upgrade, and one started late is the same case: each owner is read fresh.
    #[tokio::test]
    async fn every_upower_owner_is_read_fresh_and_its_absence_clears_the_fields() {
        let bus = private_bus().await;
        let (events, mut changed) = mpsc::unbounded_channel();
        let power = PowerController::new(bus.connection().await, events);
        within(changed.recv()).await;
        assert_eq!(power.snapshot(), PowerState::default(), "no UPower yet");

        let first = serve_upower(&bus, false, 5.0).await;
        within(changed.recv()).await;
        assert_eq!((power.snapshot().on_battery, power.snapshot().energy_rate), (Some(false), Some(5.0)));

        drop(first);
        within(changed.recv()).await;
        assert_eq!(power.snapshot(), PowerState::default(), "a vanished UPower leaves no stale reading");

        let second = serve_upower(&bus, true, 7.5).await;
        within(changed.recv()).await;
        assert_eq!((power.snapshot().on_battery, power.snapshot().energy_rate), (Some(true), Some(7.5)));

        let upower = second.object_server().interface::<_, FakeUPower>("/org/freedesktop/UPower").await.unwrap();
        upower.get_mut().await.on_battery = false;
        upower.get().await.on_battery_changed(upower.signal_emitter()).await.unwrap();
        within(changed.recv()).await;
        assert_eq!(power.snapshot().on_battery, Some(false), "the new owner's PropertiesChanged is followed");
    }

    struct FakeProfiles {
        active: String,
    }

    #[zbus::interface(name = "org.freedesktop.UPower.PowerProfiles")]
    impl FakeProfiles {
        #[zbus(property)]
        fn active_profile(&self) -> String {
            self.active.clone()
        }
        #[zbus(property)]
        fn set_active_profile(&mut self, value: String) {
            self.active = value;
        }
        #[zbus(property)]
        fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
            vec![entry(&[("Profile", Value::from("power-saver"))]), entry(&[("Profile", Value::from("balanced"))])]
        }
    }

    #[tokio::test]
    async fn set_profile_writes_only_a_name_the_daemon_lists() {
        let bus = private_bus().await;
        let daemon = bus
            .builder()
            .serve_at("/org/freedesktop/UPower/PowerProfiles", FakeProfiles { active: "power-saver".into() })
            .unwrap()
            .name("org.freedesktop.UPower.PowerProfiles")
            .unwrap()
            .build()
            .await
            .unwrap();
        let (events, mut changed) = mpsc::unbounded_channel();
        let power = PowerController::new(bus.connection().await, events);
        within(changed.recv()).await;
        let fake =
            daemon.object_server().interface::<_, FakeProfiles>("/org/freedesktop/UPower/PowerProfiles").await.unwrap();

        power.set_profile("turbo").await;
        assert_eq!(fake.get().await.active, "power-saver", "an unlisted name never reaches the daemon");
        power.set_profile("balanced").await;
        assert_eq!(fake.get().await.active, "balanced");
    }

    #[test]
    fn profile_names_pulls_the_profile_key_out_of_each_description_and_keeps_the_order() {
        let raw = vec![
            entry(&[("Profile", Value::from("power-saver")), ("Driver", Value::from("intel_pstate"))]),
            entry(&[("Profile", Value::from("balanced")), ("Driver", Value::from("intel_pstate"))]),
            entry(&[("Profile", Value::from("performance")), ("Driver", Value::from("intel_pstate"))]),
        ];

        assert_eq!(profile_names(&raw), ["power-saver", "balanced", "performance"]);
    }

    #[test]
    fn profile_names_skips_an_entry_with_no_profile_key_or_a_non_string_one() {
        let raw = vec![
            entry(&[("Driver", Value::from("placeholder"))]),
            entry(&[("Profile", Value::from(3i32))]),
            entry(&[("Profile", Value::from("balanced"))]),
        ];

        assert_eq!(profile_names(&raw), ["balanced"]);
    }

    #[test]
    fn profile_names_is_empty_for_an_empty_list() {
        assert_eq!(profile_names(&[]), Vec::<String>::new());
    }

    #[test]
    fn a_field_this_host_cannot_answer_is_absent_from_the_json_rather_than_null() {
        let state = PowerState { on_battery: Some(false), energy_rate: Some(0.0), ..PowerState::default() };

        let json = serde_json::to_value(&state).unwrap();

        assert_eq!(json["on_battery"], serde_json::json!(false));
        assert_eq!(json["energy_rate"], serde_json::json!(0.0));
        assert!(json.get("active_profile").is_none());
        assert!(json.get("profiles").is_none());
    }

    #[test]
    fn a_host_that_answers_nothing_serializes_to_an_empty_object() {
        assert_eq!(serde_json::to_value(PowerState::default()).unwrap(), serde_json::json!({}));
    }
}

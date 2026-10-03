//! Individual UPower batteries outside the system supply.

use std::collections::HashMap;

use futures_util::StreamExt;
use shared::state::battery::{BatteryStatus, PeripheralBattery};
use shared::{debug, error};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::controller::{DISPLAY_DEVICE, from_upower, get};

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    fn enumerate_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(signal)]
    fn device_added(&self, device: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    fn device_removed(&self, device: OwnedObjectPath) -> zbus::Result<()>;
}

struct Watch {
    properties: zbus::fdo::PropertiesProxy<'static>,
    forwarder: JoinHandle<()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

fn kind(number: u32) -> &'static str {
    #[rustfmt::skip]
    const KINDS: [&str; 29] = [
        "unknown", "line_power", "battery", "ups", "monitor", "mouse", "keyboard", "pda", "phone", "media_player",
        "tablet", "computer", "gaming_input", "pen", "touchpad", "modem", "network", "headset", "speakers",
        "headphones", "video", "other_audio", "remote_control", "printer", "scanner", "camera", "wearable", "toy",
        "bluetooth-generic",
    ];
    KINDS.get(number as usize).copied().unwrap_or("other")
}

fn level(number: u32) -> Option<&'static str> {
    const LEVELS: [Option<&str>; 9] =
        [Some("unknown"), None, None, Some("low"), Some("critical"), None, Some("normal"), Some("high"), Some("full")];
    LEVELS.get(number as usize).copied().flatten()
}

fn from_properties(id: &str, all: &HashMap<String, OwnedValue>) -> Option<PeripheralBattery> {
    if id == DISPLAY_DEVICE {
        return None;
    }
    let reported_type = get::<u32>(all, "Type");
    let device_type = reported_type.unwrap_or(0);
    let power_supply = get::<bool>(all, "PowerSupply");
    if device_type == 1
        || (reported_type.is_some() && power_supply == Some(true))
        || (device_type == 2 && power_supply != Some(false))
    {
        return None;
    }
    if device_type == 2 && get::<bool>(all, "IsPresent") == Some(false) {
        return None;
    }
    let battery_level = reported_type.and_then(|_| get::<u32>(all, "BatteryLevel"));
    let level = battery_level.and_then(level).map(str::to_string);
    let percent = reported_type
        .and_then(|_| get::<f64>(all, "Percentage"))
        .filter(|p| battery_level.is_none_or(|level| level == 1) && (0.0..=100.0).contains(p));
    let name = ["Model", "Vendor"]
        .into_iter()
        .find_map(|key| get::<&str>(all, key).filter(|name| !name.is_empty()))
        .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id))
        .to_string();
    Some(PeripheralBattery {
        id: id.to_string(),
        name,
        kind: kind(device_type).to_string(),
        percent: percent.map(|p| p.round() as u8),
        level,
        state: reported_type.and_then(|_| get::<u32>(all, "State")).map(from_upower).unwrap_or(BatteryStatus::Unknown),
        icon: get::<&str>(all, "IconName").filter(|icon| !icon.is_empty()).map(str::to_string),
    })
}

async fn watch(
    connection: &zbus::Connection,
    path: &OwnedObjectPath,
    changed: UnboundedSender<()>,
) -> zbus::Result<Watch> {
    let properties = zbus::fdo::PropertiesProxy::builder(connection)
        .destination("org.freedesktop.UPower")?
        .path(path.clone())?
        .build()
        .await?;
    let mut stream = properties.receive_properties_changed().await?;
    let forwarder = tokio::spawn(async move { while stream.next().await.is_some() && changed.send(()).is_ok() {} });
    Ok(Watch { properties, forwarder })
}

/// ponytail: O(n) GetAll per change; above 32 devices, cache reads by path.
async fn refresh(
    upower: &UPowerProxy<'static>,
    connection: &zbus::Connection,
    watches: &mut HashMap<OwnedObjectPath, Watch>,
    changed: &UnboundedSender<()>,
    updates: &UnboundedSender<Vec<PeripheralBattery>>,
) -> bool {
    let paths = match upower.enumerate_devices().await {
        Ok(paths) => paths,
        Err(err) => {
            debug!("UPower device enumeration failed: {err}");
            return false;
        }
    };
    watches.retain(|path, _| paths.contains(path));
    let mut peripherals = Vec::new();
    for path in paths {
        if !watches.contains_key(&path) {
            match watch(connection, &path, changed.clone()).await {
                Ok(watch) => {
                    watches.insert(path.clone(), watch);
                }
                Err(err) => {
                    debug!("cannot watch UPower device {path}: {err}");
                    if let Some(device) = from_properties(path.as_str(), &HashMap::new()) {
                        peripherals.push(device);
                    }
                    continue;
                }
            }
        }
        let watch = &watches[&path];
        let interface = zbus::names::InterfaceName::from_static_str_unchecked("org.freedesktop.UPower.Device");
        let all = watch.properties.get_all(interface).await.unwrap_or_default();
        if let Some(device) = from_properties(path.as_str(), &all) {
            peripherals.push(device);
        }
    }
    peripherals.sort_by(|a, b| a.id.cmp(&b.id));
    let _ = updates.send(peripherals);
    true
}

pub(super) async fn run(connection: zbus::Connection, updates: UnboundedSender<Vec<PeripheralBattery>>) {
    let upower = match UPowerProxy::builder(&connection).build().await {
        Ok(proxy) => proxy,
        Err(err) => {
            error!("cannot watch UPower devices: {err}");
            return;
        }
    };
    let Ok(mut added) = upower.receive_device_added().await else {
        error!("cannot subscribe to UPower DeviceAdded; giving up on peripherals");
        return;
    };
    let Ok(mut removed) = upower.receive_device_removed().await else {
        error!("cannot subscribe to UPower DeviceRemoved; giving up on peripherals");
        return;
    };
    let Ok(mut owners) = upower.inner().receive_owner_changed().await else {
        error!("cannot watch who owns org.freedesktop.UPower; giving up on peripherals");
        return;
    };
    let (changed, mut device_changes) = tokio::sync::mpsc::unbounded_channel();
    let mut watches = HashMap::new();
    loop {
        let enumerated = refresh(&upower, &connection, &mut watches, &changed, &updates).await;
        if updates.is_closed() {
            return;
        }
        tokio::select! {
            _ = updates.closed() => return,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)), if !enumerated => {},
            Some(_) = added.next() => {},
            Some(_) = removed.next() => {},
            Some(_) = device_changes.recv() => {},
            Some(_) = owners.next() => {
                watches.clear();
                if updates.send(Vec::new()).is_err() { return; }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use super::super::fixtures::{FakeDevice, FakeUPower, MOUSE, serve};
    use crate::capabilities::test_support::{private_bus, properties, within};

    #[test]
    fn skips_system_supplies_and_keeps_unknown_percentage_absent() {
        let supply = properties(&[("Type", 2u32.into()), ("PowerSupply", true.into()), ("Percentage", 70.0f64.into())]);
        assert_eq!(from_properties("/battery_BAT0", &supply), None);
        let mouse = properties(&[
            ("Type", 5u32.into()),
            ("Model", "Mouse".into()),
            ("BatteryLevel", 3u32.into()),
            ("Percentage", 80.0f64.into()),
        ]);
        assert_eq!(from_properties(DISPLAY_DEVICE, &mouse), None);
        let battery = from_properties("/mouse_1", &mouse).unwrap();
        assert_eq!(
            (battery.name.as_str(), battery.kind.as_str(), battery.percent, battery.level.as_deref()),
            ("Mouse", "mouse", None, Some("low"))
        );
        let unreadable = properties(&[("Type", 5u32.into()), ("Model", "Mouse".into())]);
        let battery = from_properties("/mouse_2", &unreadable).unwrap();
        assert_eq!((battery.percent, battery.level), (None, None));
        let unknown =
            properties(&[("Type", 5u32.into()), ("BatteryLevel", 0u32.into()), ("Percentage", 0.0f64.into())]);
        let battery = from_properties("/mouse_3", &unknown).unwrap();
        assert_eq!((battery.percent, battery.level.as_deref()), (None, Some("unknown")));
        let use_percent =
            properties(&[("Type", 5u32.into()), ("BatteryLevel", 1u32.into()), ("Percentage", 0.0f64.into())]);
        assert_eq!(from_properties("/mouse_4", &use_percent).unwrap().percent, Some(0));
        assert_eq!(kind(28), "bluetooth-generic");
        let missing_type = properties(&[("Percentage", 44.0f64.into()), ("BatteryLevel", 3u32.into())]);
        let missing_type = from_properties("/mouse_5", &missing_type).unwrap();
        assert_eq!((missing_type.kind.as_str(), missing_type.percent, missing_type.level), ("unknown", None, None));
        let line = properties(&[("Type", 1u32.into()), ("Percentage", 50.0f64.into())]);
        assert_eq!(from_properties("/line_AC", &line), None);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 100.1] {
            let properties = properties(&[("Type", 5u32.into()), ("Percentage", value.into())]);
            assert_eq!(from_properties("/mouse", &properties).unwrap().percent, None, "Percentage = {value}");
        }
    }

    async fn next_matching(
        updates: &mut mpsc::UnboundedReceiver<Vec<PeripheralBattery>>,
        matches: impl Fn(&[PeripheralBattery]) -> bool,
    ) -> Vec<PeripheralBattery> {
        loop {
            let next = within(updates.recv()).await.unwrap();
            if matches(&next) {
                return next;
            }
        }
    }

    #[tokio::test]
    async fn device_add_change_remove_and_owner_restart_replace_the_list() {
        let bus = private_bus().await;
        let first = serve(&bus, Vec::new(), FakeDevice::default(), 80.0).await;
        let (tx, mut updates) = mpsc::unbounded_channel();
        let follower = tokio::spawn(run(bus.connection().await, tx));
        within(updates.recv()).await.unwrap();

        {
            let service = first.object_server().interface::<_, FakeUPower>("/org/freedesktop/UPower").await.unwrap();
            let path = OwnedObjectPath::try_from(MOUSE).unwrap();
            service.get_mut().await.paths.push(path.clone());
            FakeUPower::device_added(service.signal_emitter(), path.clone()).await.unwrap();
            next_matching(&mut updates, |list| list.first().is_some_and(|d| d.percent == Some(80))).await;

            service.get_mut().await.paths.clear();
            FakeUPower::device_removed(service.signal_emitter(), path.clone()).await.unwrap();
            next_matching(&mut updates, |list| list.is_empty()).await;

            service.get_mut().await.paths.push(path.clone());
            FakeUPower::device_added(service.signal_emitter(), path).await.unwrap();
            next_matching(&mut updates, |list| list.len() == 1).await;
        }
        drop(first);
        next_matching(&mut updates, |list| list.is_empty()).await;

        let second = serve(&bus, vec![OwnedObjectPath::try_from(MOUSE).unwrap()], FakeDevice::default(), 25.0).await;
        next_matching(&mut updates, |list| list.first().is_some_and(|d| d.percent == Some(25))).await;
        let mouse = second.object_server().interface::<_, FakeDevice>(MOUSE).await.unwrap();
        mouse.get_mut().await.percentage = 35.0;
        mouse.get().await.percentage_changed(mouse.signal_emitter()).await.unwrap();
        next_matching(&mut updates, |list| list.first().is_some_and(|d| d.percent == Some(35))).await;

        second.object_server().remove::<FakeDevice, _>(MOUSE).await.unwrap();
        let upower = second.object_server().interface::<_, FakeUPower>("/org/freedesktop/UPower").await.unwrap();
        FakeUPower::device_added(upower.signal_emitter(), OwnedObjectPath::try_from(MOUSE).unwrap()).await.unwrap();
        let unreadable = next_matching(&mut updates, |list| list.first().is_some_and(|d| d.kind == "unknown")).await;
        assert_eq!((unreadable[0].percent, unreadable[0].level.as_deref()), (None, None));

        follower.abort();
    }

    #[tokio::test]
    async fn failed_initial_enumeration_retries_without_a_signal() {
        let bus = private_bus().await;
        let service = serve(&bus, vec![OwnedObjectPath::try_from(MOUSE).unwrap()], FakeDevice::default(), 55.0).await;
        service
            .object_server()
            .interface::<_, FakeUPower>("/org/freedesktop/UPower")
            .await
            .unwrap()
            .get_mut()
            .await
            .fail_enumeration = true;
        let (tx, mut updates) = mpsc::unbounded_channel();
        let follower = tokio::spawn(run(bus.connection().await, tx));
        let list = within(updates.recv()).await.unwrap();
        assert_eq!(list[0].percent, Some(55));
        follower.abort();
    }
}

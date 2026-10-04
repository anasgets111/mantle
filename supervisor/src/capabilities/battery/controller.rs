//! [`BatteryController`] owns read-only `mantle.battery` telemetry. Module-level behavior is
//! documented in `battery/mod.rs`.

pub use shared::state::battery::{BatteryState, BatteryStatus};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use shared::error;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::capabilities::publish;
use zbus::zvariant::OwnedValue;

/// UPower's own numbering (`org.freedesktop.UPower.Device.State`). An unknown number is
/// [`BatteryStatus::Unknown`] rather than an error: a future UPower adding an eighth state
/// must not fail this capability.
pub(super) fn from_upower(state: u32) -> BatteryStatus {
    match state {
        1 => BatteryStatus::Charging,
        2 => BatteryStatus::Discharging,
        3 => BatteryStatus::Empty,
        4 => BatteryStatus::FullyCharged,
        5 => BatteryStatus::PendingCharge,
        6 => BatteryStatus::PendingDischarge,
        _ => BatteryStatus::Unknown,
    }
}

/// UPower's `DisplayDevice`, the composite of every battery. Its documented path is fixed, so
/// this reads it directly instead of calling `GetDisplayDevice()`.
pub(super) const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

/// UPower's `Type` value for a battery.
const UPOWER_TYPE_BATTERY: u32 = 2;

pub(super) fn get<'a, T: TryFrom<&'a OwnedValue>>(all: &'a HashMap<String, OwnedValue>, name: &str) -> Option<T> {
    all.get(name).and_then(|value| T::try_from(value).ok())
}

pub struct BatteryController {
    state: Arc<Mutex<BatteryState>>,
}

impl BatteryController {
    pub fn new(system_bus: zbus::Connection, events: UnboundedSender<()>) -> Self {
        let state = Arc::new(Mutex::new(BatteryState::default()));
        let (peripherals, updates) = mpsc::unbounded_channel();
        tokio::spawn(super::peripherals::run(system_bus.clone(), peripherals));
        tokio::spawn(run_battery_task(system_bus, Arc::clone(&state), events, updates));
        Self { state }
    }

    pub fn snapshot(&self) -> BatteryState {
        self.state.lock().expect("battery state mutex poisoned").clone()
    }
}

/// A positive duration UPower estimated. `0` means "no answer" on both properties; negatives are
/// not durations.
fn seconds(reported: i64) -> Option<u32> {
    u32::try_from(reported).ok().filter(|seconds| *seconds > 0)
}

/// Reads the whole payload in one `GetAll`. Failed properties keep their defaults rather than stale
/// values, the same rule as `power::controller::read_state`: a live-looking stale number is worse
/// than zero.
async fn read_state(properties: &zbus::fdo::PropertiesProxy<'static>) -> BatteryState {
    let interface = zbus::names::InterfaceName::from_static_str_unchecked("org.freedesktop.UPower.Device");
    properties.get_all(interface).await.map(|all| from_properties(&all)).unwrap_or_default()
}

/// `IsPresent` alone is true for non-battery display devices, so `Type` and `IsPresent` are checked
/// together.
fn from_properties(all: &HashMap<String, OwnedValue>) -> BatteryState {
    let is_battery = get::<u32>(all, "Type") == Some(UPOWER_TYPE_BATTERY);
    if !is_battery || get::<bool>(all, "IsPresent") != Some(true) {
        return BatteryState::default();
    }

    BatteryState {
        present: true,
        // Round rather than cast: 69.8% would otherwise show 69 for the whole minute before 70.
        percent: get::<f64>(all, "Percentage").unwrap_or(0.0).clamp(0.0, 100.0).round() as u8,
        state: get::<u32>(all, "State").map(from_upower).unwrap_or_default(),
        time_to_empty: get::<i64>(all, "TimeToEmpty").and_then(seconds),
        time_to_full: get::<i64>(all, "TimeToFull").and_then(seconds),
        ..BatteryState::default()
    }
}

/// UPower once emitted a spurious mains `Percentage` of 0 for one push, emptying the pill. After a
/// nonzero reading, retain a mains zero while not draining. A real on-battery zero is
/// indistinguishable from the glitch and passes through.
fn hold_through_glitch(previous: &BatteryState, current: BatteryState) -> BatteryState {
    let draining = matches!(current.state, BatteryStatus::Discharging | BatteryStatus::Empty);
    if current.present && !draining && current.percent == 0 && previous.percent > 0 {
        BatteryState { percent: previous.percent, ..current }
    } else {
        current
    }
}

/// Reads once, pushes, then follows `PropertiesChanged` and owner changes. Every wake re-reads all
/// five fields, as `power::controller` does, keeping them consistent instead of patching one
/// property.
///
/// One `org.freedesktop.DBus.Properties` subscription covers the object. It batches a percentage
/// move and state flip into one message. A UPower that exits or restarts ends no stream, so an owner
/// change re-reads too: no owner fails `GetAll` (not present), a new one answers fresh.
///
/// **No timer, per ADR-0080.** sysfs misses capacity changes the kernel does not announce: a plug
/// event can arrive, then `capacity` fall 69 to 65 with zero `power_supply` uevents. UPower already
/// polls and emits refreshes for other clients.
async fn run_battery_task(
    system_bus: zbus::Connection,
    state: Arc<Mutex<BatteryState>>,
    events: UnboundedSender<()>,
    mut peripheral_updates: UnboundedReceiver<super::peripherals::Readings>,
) {
    // A live `GetAll`, never zbus's property cache: its refresh task listens to the same signal, so
    // our stream can win the race, read the pre-change cache, and leave a newly plugged charger
    // showing `Discharging` until a later property moves.
    //
    // Subscribe before the first read. A cable change during the subscription round trip must not
    // land between a read and a subscription that does not exist yet.
    let properties = match zbus::fdo::PropertiesProxy::builder(&system_bus)
        .destination("org.freedesktop.UPower")
        .and_then(|builder| builder.path(DISPLAY_DEVICE))
    {
        Ok(builder) => match builder.build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                error!("cannot watch the UPower DisplayDevice for changes ({err}); giving up on it");
                return;
            }
        },
        Err(err) => {
            error!("cannot address the UPower DisplayDevice ({err}); giving up on it");
            return;
        }
    };
    let Ok(mut changed) = properties.receive_properties_changed().await else {
        error!("cannot subscribe to the UPower DisplayDevice's properties; giving up on it");
        return;
    };

    let Ok(mut owner) = properties.inner().receive_owner_changed().await else {
        error!("cannot watch who owns org.freedesktop.UPower; giving up on the DisplayDevice");
        return;
    };

    let mut previous = read_state(&properties).await;
    *state.lock().expect("battery state mutex poisoned") = previous.clone();
    if events.send(()).is_err() {
        return;
    }

    loop {
        tokio::select! {
            _ = events.closed() => return,
            Some((peripherals, capacity)) = peripheral_updates.recv() => {
                (previous.peripherals, previous.capacity) = (peripherals, capacity);
                if !publish(&state, &events, previous.clone()) { return; }
                continue;
            }
            Some(_) = changed.next() => {},
            Some(_) = owner.next() => {},
            else => return,
        }
        let current = BatteryState {
            peripherals: previous.peripherals.clone(),
            capacity: previous.capacity,
            ..read_state(&properties).await
        };
        let current = hold_through_glitch(&previous, current);
        previous = current.clone();
        if !publish(&state, &events, current) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;
    use zbus::zvariant::OwnedObjectPath;

    use super::super::fixtures::{FakeDevice, MOUSE, SUPPLY, serve};
    use crate::capabilities::test_support::{private_bus, properties, within};

    #[tokio::test]
    async fn display_and_peripherals_merge_restart_and_stop() {
        let bus = private_bus().await;
        let first = serve(
            &bus,
            [MOUSE, SUPPLY].map(|path| OwnedObjectPath::try_from(path).unwrap()).to_vec(),
            FakeDevice { kind: UPOWER_TYPE_BATTERY, percentage: 70.0, time_to_empty: 3600, time_to_full: 1200 },
            25.0,
        )
        .await;
        let (events, mut changed) = mpsc::unbounded_channel();
        let battery = BatteryController::new(bus.connection().await, events);
        while battery.snapshot().peripherals.is_empty() {
            within(changed.recv()).await.unwrap();
        }
        let snapshot = battery.snapshot();
        assert_eq!(
            (snapshot.percent, snapshot.time_to_empty, snapshot.time_to_full, snapshot.peripherals[0].percent),
            (70, Some(3600), Some(1200), Some(25))
        );
        assert_eq!(
            (snapshot.peripherals.len(), snapshot.capacity),
            (1, Some(91)),
            "the supply is health, not a peripheral"
        );
        {
            let display = first.object_server().interface::<_, FakeDevice>(DISPLAY_DEVICE).await.unwrap();
            display.get_mut().await.percentage = 60.0;
            display.get().await.percentage_changed(display.signal_emitter()).await.unwrap();
        }
        while battery.snapshot().percent != 60 {
            within(changed.recv()).await.unwrap();
        }
        let snapshot = battery.snapshot();
        assert_eq!((snapshot.percent, snapshot.peripherals[0].percent), (60, Some(25)));

        drop(first);
        while battery.snapshot().present {
            within(changed.recv()).await.unwrap();
        }
        let display = FakeDevice { kind: UPOWER_TYPE_BATTERY, percentage: 40.0, ..Default::default() };
        let _second = serve(&bus, Vec::new(), display, 50.0).await;
        while battery.snapshot().percent != 40 {
            within(changed.recv()).await.unwrap();
        }
        assert_eq!(
            (battery.snapshot().present, battery.snapshot().percent, battery.snapshot().capacity),
            (true, 40, None)
        );

        let (events, mut changed) = mpsc::unbounded_channel();
        let (peripherals, updates) = mpsc::unbounded_channel();
        let connection = bus.connection().await;
        let state = Arc::new(Mutex::new(BatteryState::default()));
        let follower = tokio::spawn(super::super::peripherals::run(connection.clone(), peripherals));
        let controller = tokio::spawn(run_battery_task(connection, state, events, updates));
        within(changed.recv()).await.unwrap();
        drop(changed);
        within(controller).await.unwrap();
        within(follower).await.unwrap();
    }

    /// Each state is distinct to a user; a `charging` boolean would collapse the middle rows.
    #[test]
    fn every_upower_state_maps_to_its_own_name() {
        for (reported, expected) in [
            (0, BatteryStatus::Unknown),
            (1, BatteryStatus::Charging),
            (2, BatteryStatus::Discharging),
            (3, BatteryStatus::Empty),
            (4, BatteryStatus::FullyCharged),
            (5, BatteryStatus::PendingCharge),
            (6, BatteryStatus::PendingDischarge),
        ] {
            assert_eq!(from_upower(reported), expected, "State = {reported}");
        }
    }

    /// An unknown future state degrades to `"unknown"` instead of dropping the payload.
    #[test]
    fn a_state_number_this_build_does_not_know_reads_as_unknown() {
        assert_eq!(from_upower(7), BatteryStatus::Unknown);
        assert_eq!(from_upower(u32::MAX), BatteryStatus::Unknown);
    }

    /// These names are the wire format and config comparisons; renaming one is breaking.
    #[test]
    fn the_state_serializes_under_the_name_a_config_compares_against() {
        let json = serde_json::to_string(&BatteryState {
            present: true,
            percent: 70,
            state: BatteryStatus::PendingCharge,
            time_to_empty: None,
            time_to_full: None,
            ..BatteryState::default()
        })
        .unwrap();
        assert_eq!(json, r#"{"present":true,"percent":70,"state":"pending_charge","peripherals":[]}"#);
    }

    #[test]
    fn a_zero_on_mains_right_after_a_reading_keeps_the_reading() {
        let previous =
            BatteryState { present: true, percent: 70, state: BatteryStatus::PendingCharge, ..Default::default() };
        let glitch = BatteryState { percent: 0, ..previous.clone() };
        assert_eq!(hold_through_glitch(&previous, glitch).percent, 70);
        let drained = BatteryState { percent: 0, state: BatteryStatus::Discharging, ..previous.clone() };
        assert_eq!(hold_through_glitch(&previous, drained).percent, 0, "on battery a zero is a zero");
        assert!(
            !hold_through_glitch(&previous, BatteryState::default()).present,
            "a battery going away is not a glitch"
        );
    }

    /// `0` means charging or not enough history to estimate on these properties, not a duration.
    #[test]
    fn an_unestimated_or_negative_duration_is_absent_rather_than_zero() {
        assert_eq!(seconds(0), None);
        assert_eq!(seconds(-1), None);
        assert_eq!(seconds(8040), Some(8040));
    }

    #[test]
    fn get_all_reads_a_present_battery_and_ignores_a_non_battery_display_device() {
        let battery = properties(&[
            ("Type", 2u32.into()),
            ("IsPresent", true.into()),
            ("Percentage", 69.8f64.into()),
            ("State", 5u32.into()),
            ("TimeToEmpty", 0i64.into()),
            ("TimeToFull", 8040i64.into()),
        ]);
        assert_eq!(
            from_properties(&battery),
            BatteryState {
                present: true,
                percent: 70,
                state: BatteryStatus::PendingCharge,
                time_to_empty: None,
                time_to_full: Some(8040),
                ..BatteryState::default()
            }
        );
        let mains = properties(&[("Type", 1u32.into()), ("IsPresent", true.into()), ("Percentage", 50f64.into())]);
        assert_eq!(from_properties(&mains), BatteryState::default());
    }
}

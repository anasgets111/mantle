//! `mantle.battery` reports the system battery through UPower's `DisplayDevice` and individual
//! peripheral batteries through device enumeration. Read-only, with no `dispatch`.
//!
//! Not a `/sys/class/power_supply` udev watch: it misses capacity changes the kernel does not
//! announce and cannot tell a reached charge limit from running on battery (ADR-0080).

pub mod controller;
mod peripherals;

pub use controller::BatteryController;

#[cfg(test)]
pub(super) mod fixtures {
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::OwnedObjectPath;

    use crate::capabilities::test_support::PrivateBus;

    use super::controller::DISPLAY_DEVICE;

    pub(super) const MOUSE: &str = "/org/freedesktop/UPower/devices/mouse_test";
    pub(super) const SUPPLY: &str = "/org/freedesktop/UPower/devices/battery_BAT0";

    /// A system battery at 38.4 of its 42 Wh design: 91% health.
    struct FakeSupply;

    #[rustfmt::skip]
    #[zbus::interface(name = "org.freedesktop.UPower.Device")]
    impl FakeSupply {
        #[zbus(property, name = "Type")]
        fn kind(&self) -> u32 { 2 }
        #[zbus(property)]
        fn power_supply(&self) -> bool { true }
        #[zbus(property)]
        fn is_present(&self) -> bool { true }
        #[zbus(property)]
        fn energy_full(&self) -> f64 { 38.4 }
        #[zbus(property)]
        fn energy_full_design(&self) -> f64 { 42.0 }
    }

    #[derive(Default)]
    pub(super) struct FakeDevice {
        pub(super) kind: u32,
        pub(super) percentage: f64,
        pub(super) time_to_empty: i64,
        pub(super) time_to_full: i64,
    }

    #[rustfmt::skip]
    #[zbus::interface(name = "org.freedesktop.UPower.Device")]
    impl FakeDevice {
        #[zbus(property, name = "Type")]
        fn kind(&self) -> u32 { self.kind }
        #[zbus(property)]
        fn is_present(&self) -> bool { true }
        #[zbus(property)]
        pub(super) fn percentage(&self) -> f64 { self.percentage }
        #[zbus(property)]
        pub(super) fn time_to_empty(&self) -> i64 { self.time_to_empty }
        #[zbus(property)]
        pub(super) fn time_to_full(&self) -> i64 { self.time_to_full }
    }

    pub(super) struct FakeUPower {
        pub(super) paths: Vec<OwnedObjectPath>,
        pub(super) fail_enumeration: bool,
    }

    #[zbus::interface(name = "org.freedesktop.UPower")]
    impl FakeUPower {
        fn enumerate_devices(&mut self) -> zbus::fdo::Result<Vec<OwnedObjectPath>> {
            if self.fail_enumeration {
                self.fail_enumeration = false;
                return Err(zbus::fdo::Error::Failed("test enumeration failure".into()));
            }
            Ok(self.paths.clone())
        }
        #[zbus(signal)]
        pub(super) async fn device_added(emitter: &SignalEmitter<'_>, device: OwnedObjectPath) -> zbus::Result<()>;
        #[zbus(signal)]
        pub(super) async fn device_removed(emitter: &SignalEmitter<'_>, device: OwnedObjectPath) -> zbus::Result<()>;
    }

    pub(super) async fn serve(
        bus: &PrivateBus,
        paths: Vec<OwnedObjectPath>,
        display: FakeDevice,
        mouse_percent: f64,
    ) -> zbus::Connection {
        bus.builder()
            .serve_at("/org/freedesktop/UPower", FakeUPower { paths, fail_enumeration: false })
            .unwrap()
            .serve_at(DISPLAY_DEVICE, display)
            .unwrap()
            .serve_at(MOUSE, FakeDevice { kind: 5, percentage: mouse_percent, ..Default::default() })
            .unwrap()
            .serve_at(SUPPLY, FakeSupply)
            .unwrap()
            .name("org.freedesktop.UPower")
            .unwrap()
            .build()
            .await
            .unwrap()
    }
}

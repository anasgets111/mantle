//! [`BrightnessController`] owns `mantle.brightness` and its write action.

pub use shared::state::brightness::BrightnessState;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared::{debug, warn};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use udev::MonitorSocket;

use super::super::scale::{percent_from_raw, raw_from_percent};
use super::super::{publish, read_attr};

/// Device preference from `Documentation/ABI/stable/sysfs-class-backlight`: firmware (0) <
/// platform (1) < raw (2), with unknown/missing last (3), not excluded.
fn device_type_rank(entry_dir: &Path) -> u8 {
    match read_attr(entry_dir, "type").as_deref() {
        Some("firmware") => 0,
        Some("platform") => 1,
        Some("raw") => 2,
        _ => 3,
    }
}

/// Parsed `max_brightness`; missing or malformed values become `0`, and the `> 0` filter in
/// [`select_backlight_device`] rejects every non-positive value.
fn read_max_brightness(entry_dir: &Path) -> i32 {
    read_attr(entry_dir, "max_brightness").and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// Picks one `max_brightness > 0` device (ADR-0053), ranked by [`device_type_rank`] and then
/// sorted directory name for deterministic boot-to-boot selection. `None` if none qualifies.
fn select_backlight_device(backlight_root: &Path) -> Option<(PathBuf, i32)> {
    std::fs::read_dir(backlight_root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|dir| {
            let max = read_max_brightness(&dir);
            (max > 0).then_some((dir, max))
        })
        .min_by(|(dir_a, _), (dir_b, _)| {
            device_type_rank(dir_a).cmp(&device_type_rank(dir_b)).then_with(|| dir_a.cmp(dir_b))
        })
}

/// Reads `brightness` (the last requested value), not `actual_brightness`: a driver fade or
/// rounded request can differ, and `set(50)` must read back `50`. Missing/malformed reads are `None`.
/// [`select_backlight_device`] guarantees positive `max`, so the `u8` result is `[0, 100]`.
fn read_percent(device_dir: &Path, max: i32) -> Option<u8> {
    percent_from_raw(read_attr(device_dir, "brightness")?.parse().ok()?, max)
}

/// `org.freedesktop.login1.Session.SetBrightness` on fixed `session/auto`, which logind resolves
/// to the caller's session. Built per write because writes are rare; `keyboard` reuses it for LEDs.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
pub(crate) trait Login1Session {
    #[zbus(name = "SetBrightness")]
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}

/// The device [`select_backlight_device`] chose. Its `max_brightness` and sysfs directory do not
/// change while it exists; `name` is cached for each `SetBrightness` call.
struct BacklightDevice {
    dir: PathBuf,
    name: String,
    max: i32,
}

/// Cadence when the udev watch is unavailable.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct BrightnessController {
    state: Arc<Mutex<BrightnessState>>,
    device: Arc<Mutex<Option<BacklightDevice>>>,
    system_bus: zbus::Connection,
    pub(super) writes: super::super::LatestWrites,
}

impl super::super::Writer for BrightnessController {
    async fn write(&self, value: f64) {
        self.set(value).await
    }
}

impl BrightnessController {
    /// `backlight_root` (default `/sys/class/backlight`) is injected for tests. `system_bus` is
    /// the Supervisor's existing connection used by [`Login1SessionProxy`]. Until a usable device
    /// exists nothing is published (see `brightness/mod.rs`); the watch picks up one added later.
    pub fn new(backlight_root: PathBuf, system_bus: zbus::Connection, events: UnboundedSender<()>) -> Self {
        let controller = Self { state: Arc::default(), device: Arc::default(), system_bus, writes: Default::default() };
        tokio::spawn(run_brightness_task(backlight_root, controller.clone(), events));
        controller
    }

    pub fn snapshot(&self) -> BrightnessState {
        *self.state.lock().expect("brightness state mutex poisoned")
    }

    /// `brightness:set(pct)`. Logs and returns when this machine has no backlight device.
    pub async fn set(&self, pct: f64) {
        let Some((name, max)) =
            self.device.lock().expect("mutex poisoned").as_ref().map(|device| (device.name.clone(), device.max))
        else {
            debug!("set called but no backlight device was found; ignored");
            return;
        };
        let proxy = match Login1SessionProxy::new(&self.system_bus).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("failed to build the login1 Session proxy: {err}");
                return;
            }
        };
        let raw = raw_from_percent(pct, max) as u32;
        // logind refuses SetBrightness from a non-active session (for example a background VT).
        // Log that error; do not retry it.
        if let Err(err) = proxy.set_brightness("backlight", &name, raw).await {
            warn!("SetBrightness(backlight, {name}, {raw}) failed: {err}");
        }
        // State changes arrive through the udev watch/poll loop, not an optimistic local update.
    }

    /// Selects the device again, for one added or removed since, and publishes its reading on
    /// change. No device, or one failing its read mid-removal, publishes nothing: `brightness` has
    /// no value meaning "gone", so the last reading stays. Returns false once the receiver is gone.
    fn refresh(&self, backlight_root: &Path, events: &UnboundedSender<()>) -> bool {
        let mut device = self.device.lock().expect("mutex poisoned");
        let Some((dir, max)) = select_backlight_device(backlight_root) else {
            *device = None;
            return !events.is_closed();
        };
        let Some(percent) = read_percent(&dir, max) else { return !events.is_closed() };
        let appeared = device.as_ref().is_none_or(|old| old.dir != dir);
        if appeared {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            *device = Some(BacklightDevice { dir, name, max });
        }
        drop(device);
        let next = BrightnessState { percent };
        if appeared {
            // A device's first reading is news even when it equals the default `0`.
            *self.state.lock().expect("brightness state mutex poisoned") = next;
            return events.send(()).is_ok();
        }
        publish(&self.state, events, next)
    }
}

/// Builds the udev watch first, so a device added during the first read is not missed. Then
/// publishes, and uses [`run_brightness_watch_loop`], falling back to [`run_brightness_poll_loop`]
/// only when the watch cannot start.
async fn run_brightness_task(backlight_root: PathBuf, controller: BrightnessController, events: UnboundedSender<()>) {
    let watch = build_backlight_watch();
    if !controller.refresh(&backlight_root, &events) {
        return;
    }
    match watch {
        Ok(watch) => run_brightness_watch_loop(watch, backlight_root, controller, events).await,
        Err(err) => {
            warn!("failed to set up the udev backlight watch ({err}); falling back to a {POLL_INTERVAL:?} poll");
            run_brightness_poll_loop(backlight_root, controller, events).await;
        }
    }
}

/// Builds the `backlight` udev watch: brightness changes and devices added or removed.
///
/// inotify misses sysfs attribute writes. `udevadm monitor --udev --subsystem-match=backlight`
/// confirmed that brightness changes emit a `change` uevent on the `backlight` subsystem instead.
fn build_backlight_watch() -> std::io::Result<AsyncFd<MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem("backlight")?.listen()?;
    AsyncFd::new(socket)
}

/// Awaits a readable udev fd, drains netlink messages, then pushes only a changed reading. Uses
/// `readable_mut`, not `readable`, because only udev's `send` feature is enabled, not `sync`.
async fn run_brightness_watch_loop(
    mut watch: AsyncFd<MonitorSocket>,
    backlight_root: PathBuf,
    controller: BrightnessController,
    events: UnboundedSender<()>,
) {
    loop {
        let mut guard = match watch.readable_mut().await {
            Ok(guard) => guard,
            Err(err) => {
                warn!(
                    "the udev backlight watch's fd errored ({err}); falling back to a {POLL_INTERVAL:?} poll for the rest of this run"
                );
                return run_brightness_poll_loop(backlight_root, controller, events).await;
            }
        };
        // Cleared before the drain, so an event landing during it re-arms the fd.
        guard.clear_ready();
        for _event in guard.get_inner().iter() {}

        if !controller.refresh(&backlight_root, &events) {
            return;
        }
    }
}

/// Fallback: fixed-timer reads with the primary path's push-on-change filter.
async fn run_brightness_poll_loop(
    backlight_root: PathBuf,
    controller: BrightnessController,
    events: UnboundedSender<()>,
) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.tick().await; // tokio::time::interval's first tick fires immediately; the caller's initial (or pre-fallback) read already covers it

    loop {
        ticker.tick().await;
        if !controller.refresh(&backlight_root, &events) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::{p2p_pair, within};

    fn write_entry(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (attr, value) in attrs {
            std::fs::write(dir.join(attr), value).unwrap();
        }
    }

    #[test]
    fn device_type_rank_orders_firmware_before_platform_before_raw_before_unknown() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "fw", &[("type", "firmware")]);
        write_entry(root.path(), "pf", &[("type", "platform")]);
        write_entry(root.path(), "rw", &[("type", "raw")]);
        write_entry(root.path(), "other", &[("type", "something-else")]);

        assert!(device_type_rank(&root.path().join("fw")) < device_type_rank(&root.path().join("pf")));
        assert!(device_type_rank(&root.path().join("pf")) < device_type_rank(&root.path().join("rw")));
        assert!(device_type_rank(&root.path().join("rw")) < device_type_rank(&root.path().join("other")));
    }

    #[test]
    fn select_backlight_device_prefers_firmware_over_platform_over_raw() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "acpi_video0", &[("type", "platform"), ("max_brightness", "100")]);
        write_entry(root.path(), "intel_backlight", &[("type", "raw"), ("max_brightness", "19200")]);
        write_entry(root.path(), "some_fw_backlight", &[("type", "firmware"), ("max_brightness", "255")]);

        let (dir, max) = select_backlight_device(root.path()).expect("expected a device to be selected");
        assert_eq!(dir, root.path().join("some_fw_backlight"));
        assert_eq!(max, 255);
    }

    #[test]
    fn select_backlight_device_ties_broken_by_sorted_directory_name() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "raw_b", &[("type", "raw"), ("max_brightness", "10")]);
        write_entry(root.path(), "raw_a", &[("type", "raw"), ("max_brightness", "20")]);

        let (dir, _) = select_backlight_device(root.path()).expect("expected a device to be selected");
        assert_eq!(dir, root.path().join("raw_a"));
    }

    #[test]
    fn select_backlight_device_skips_a_device_with_non_positive_max_brightness() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "broken", &[("type", "raw"), ("max_brightness", "0")]);
        write_entry(root.path(), "usable", &[("type", "raw"), ("max_brightness", "100")]);

        let (dir, max) = select_backlight_device(root.path()).expect("expected the usable device to be selected");
        assert_eq!(dir, root.path().join("usable"));
        assert_eq!(max, 100);
    }

    #[test]
    fn select_backlight_device_is_none_against_an_empty_or_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(select_backlight_device(root.path()), None);
        assert_eq!(select_backlight_device(&root.path().join("does-not-exist")), None);
    }

    #[test]
    fn read_percent_reads_the_requested_brightness_not_the_actual_one() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "intel_backlight", &[("brightness", "9600"), ("actual_brightness", "9601")]);

        assert_eq!(read_percent(&root.path().join("intel_backlight"), 19200), Some(50));
    }

    #[test]
    fn read_percent_is_none_for_a_missing_or_unparseable_reading() {
        let root = tempfile::tempdir().unwrap();
        write_entry(root.path(), "no_attr", &[]);
        assert_eq!(read_percent(&root.path().join("no_attr"), 100), None);

        write_entry(root.path(), "bad_attr", &[("brightness", "not-a-number")]);
        assert_eq!(read_percent(&root.path().join("bad_attr"), 100), None);
    }

    #[tokio::test]
    async fn brightness_controller_pushes_the_initial_state_before_the_first_poll_tick() {
        let root = tempfile::tempdir().unwrap();
        write_entry(
            root.path(),
            "intel_backlight",
            &[("type", "raw"), ("max_brightness", "19200"), ("brightness", "9600")],
        );
        let (_service_side, caller_side) = p2p_pair().await;
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BrightnessController::new(root.path().to_path_buf(), caller_side, events_tx);

        assert_eq!(
            within(events_rx.recv()).await,
            Some(()),
            "must announce the initial state without waiting for the first poll tick"
        );
        assert_eq!(controller.snapshot(), BrightnessState { percent: 50 });
    }

    /// A dock or a hybrid GPU can add the backlight after startup; udev's `add` is what reselects.
    #[tokio::test]
    async fn a_backlight_added_later_is_picked_up_and_a_removed_one_keeps_its_reading() {
        let root = tempfile::tempdir().unwrap(); // empty -- the desktop case
        let (_service_side, caller_side) = p2p_pair().await;
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = BrightnessController::new(root.path().to_path_buf(), caller_side, events_tx.clone());
        let quiet = tokio::time::timeout(Duration::from_millis(200), events_rx.recv()).await;
        assert!(quiet.is_err(), "no device means no signal, not even the default state");

        write_entry(root.path(), "intel_backlight", &[("max_brightness", "19200"), ("brightness", "0")]);
        assert!(controller.refresh(root.path(), &events_tx));
        assert_eq!(events_rx.try_recv(), Ok(()), "a device's first reading pushes even at 0");

        std::fs::remove_dir_all(root.path().join("intel_backlight")).unwrap();
        write_entry(root.path(), "acpi_video0", &[("max_brightness", "100"), ("brightness", "40")]);
        assert!(controller.refresh(root.path(), &events_tx));
        assert_eq!((events_rx.try_recv(), controller.snapshot().percent), (Ok(()), 40));

        std::fs::remove_file(root.path().join("acpi_video0/brightness")).unwrap();
        assert!(controller.refresh(root.path(), &events_tx));
        assert!(events_rx.try_recv().is_err(), "a device failing its read mid-removal is not off");
        std::fs::remove_dir_all(root.path().join("acpi_video0")).unwrap();
        assert!(controller.refresh(root.path(), &events_tx));
        assert!(events_rx.try_recv().is_err(), "a removed backlight publishes nothing");
        assert_eq!(controller.snapshot().percent, 40);
        assert!(controller.device.lock().unwrap().is_none());
    }
}

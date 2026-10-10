//! [`KeyboardController`] owns `mantle.keyboard` state and write actions. Backlight, lock state,
//! and layout share one `Arc<Mutex<KeyboardState>>` and signal channel (ADR-0034).

pub use shared::state::keyboard::KeyboardState;

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use shared::{debug, error};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use udev::MonitorSocket;

use crate::compositor::{CompositorKind, unsupported_session_report};

use super::super::brightness::controller::Login1SessionProxy;
use super::super::read_attr;
use super::super::scale::{percent_from_raw, raw_from_percent};
use super::locks::{find_leds, read_led_on, resolve_lock_leds};

/// A `*::kbd_backlight` LED and its `max_brightness`, which does not change at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LedBacklight {
    dir: PathBuf,
    max: i32,
}

#[derive(Clone)]
pub struct KeyboardController {
    state: Arc<Mutex<KeyboardState>>,
    backlight: Arc<Option<LedBacklight>>,
    compositor: Option<CompositorKind>,
    system_bus: zbus::Connection,
    events: UnboundedSender<()>,
    pub(super) writes: super::super::LatestWrites,
}

impl super::super::Writer for KeyboardController {
    async fn write(&self, value: f64) {
        self.set_backlight(value).await
    }
}

impl KeyboardController {
    /// `system_bus` carries logind backlight writes. `leds_root` (default `/sys/class/leds`) is
    /// test-injected and holds the backlight and the sysfs lock fallback. `state` is shared with the
    /// compositor reader, which writes layout; `compositor` picks the backend for
    /// `switch_layout`.
    pub fn new(
        system_bus: zbus::Connection,
        leds_root: &Path,
        state: Arc<Mutex<KeyboardState>>,
        compositor: Option<CompositorKind>,
        events_tx: UnboundedSender<()>,
    ) -> Self {
        let backlight = find_backlight(leds_root);
        match &backlight {
            Some(led) => watch_backlight(led.clone(), Arc::clone(&state), events_tx.clone()),
            None => {
                debug!("no usable *::kbd_backlight LED under {leds_root:?}; backlight reporting disabled for this run")
            }
        }
        tokio::spawn(watch_locks(resolve_locks(leds_root, &state), Arc::clone(&state), events_tx.clone()));
        let compositor = match compositor {
            None => {
                debug!("{}; layout reporting disabled for this run", unsupported_session_report());
                None
            }
            some => some,
        };
        Self {
            state,
            backlight: Arc::new(backlight),
            compositor,
            system_bus,
            events: events_tx,
            writes: Default::default(),
        }
    }

    /// `keyboard:set_backlight(pct)`. Logs and returns without keyboard-backlight hardware.
    pub async fn set_backlight(&self, pct: f64) {
        let Some(led) = self.backlight.as_ref() else {
            debug!("set_backlight called but this machine has no keyboard backlight; ignored");
            return;
        };
        let name = led.dir.file_name().unwrap_or_default().to_string_lossy();
        let raw = raw_from_percent(pct, led.max) as u32;
        let result =
            async { Login1SessionProxy::new(&self.system_bus).await?.set_brightness("leds", &name, raw).await }.await;
        if let Err(err) = result {
            debug!("SetBrightness(leds, {name}, {raw}) failed: {err}");
        }
        // `brightness_hw_changed` reports only hardware changes, so read this write back.
        self.state.lock().expect("mutex poisoned").backlight_percent = read_backlight_percent(led);
        let _ = self.events.send(());
    }

    /// `keyboard:switch_layout(index)`. Logs and returns without a supported compositor.
    /// Synchronous because `Compositor::switch_layout` is synchronous fire-and-forget.
    pub fn switch_layout(&self, index: usize) {
        match self.compositor {
            Some(kind) => kind.backend().switch_layout(index),
            None => debug!("switch_layout called but no supported compositor was detected; ignored"),
        }
    }

    pub fn snapshot(&self) -> KeyboardState {
        self.state.lock().expect("mutex poisoned").clone()
    }
}

/// First `*::kbd_backlight` by name with `max_brightness > 0`; a dead sibling must not hide a usable LED.
fn find_backlight(leds_root: &Path) -> Option<LedBacklight> {
    find_leds(leds_root, "::kbd_backlight").into_iter().find_map(|dir| {
        let max = read_attr(&dir, "max_brightness")?.parse().ok().filter(|max| *max > 0)?;
        Some(LedBacklight { dir, max })
    })
}

/// `None` when `brightness` cannot be read.
fn read_backlight_percent(led: &LedBacklight) -> Option<u8> {
    let raw = read_attr(&led.dir, "brightness")?.parse().ok()?;
    percent_from_raw(raw, led.max)
}

/// Opens `brightness_hw_changed` before the initial read so a hotkey in between is not lost, then
/// re-reads on each `POLLPRI`, which the kernel raises only for hardware changes (ADR-0034.1).
fn watch_backlight(led: LedBacklight, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<()>) {
    let watch = std::fs::File::open(led.dir.join("brightness_hw_changed"))
        .and_then(|file| AsyncFd::with_interest(file, Interest::PRIORITY));
    state.lock().expect("mutex poisoned").backlight_percent = read_backlight_percent(&led);
    let watch = match watch {
        Ok(watch) => watch,
        Err(err) => {
            error!("cannot watch {:?}/brightness_hw_changed; hotkey changes will not show: {err}", led.dir);
            return;
        }
    };
    tokio::spawn(async move {
        loop {
            match watch.ready(Interest::PRIORITY).await {
                Ok(mut guard) => guard.clear_ready(),
                Err(err) => {
                    error!("backlight watch failed; hotkey changes will no longer show: {err}");
                    break;
                }
            }
            // Reading from offset 0 re-arms kernfs's `POLLPRI`.
            let _ = watch.get_ref().read_at(&mut [0; 8], 0);
            state.lock().expect("mutex poisoned").backlight_percent = read_backlight_percent(&led);
            if events.send(()).is_err() {
                break;
            }
        }
    });
}

/// Publishes the LED snapshot of the first `evdev::enumerate()` device exposing `LED_CAPSL` and
/// returns the stream carrying its live changes. `EV_LED` is queued per open fd, so there is no
/// subscribe-before-read race. Silent when nothing is readable: that is the ordinary state between
/// a keyboard being unplugged and plugged back in. Verified live rather than unit-tested, because
/// enumeration scans real `/dev/input` nodes (ADR-0034).
fn open_led_stream(state: &Mutex<KeyboardState>) -> Option<evdev::EventStream> {
    let (_, device) = evdev::enumerate()
        .find(|(_, device)| device.supported_leds().is_some_and(|leds| leds.contains(evdev::LedCode::LED_CAPSL)))?;
    match device.get_led_state() {
        Ok(led_state) => {
            let mut guard = state.lock().expect("mutex poisoned");
            guard.caps_lock = led_state.contains(evdev::LedCode::LED_CAPSL);
            guard.num_lock = led_state.contains(evdev::LedCode::LED_NUML);
            guard.scroll_lock = led_state.contains(evdev::LedCode::LED_SCROLLL);
        }
        Err(err) => {
            debug!("failed to read initial evdev LED state; will pick up from the first EV_LED event: {err}")
        }
    }
    device.into_event_stream().inspect_err(|err| debug!("failed to open an EV_LED event stream: {err}")).ok()
}

/// Uses evdev first (ADR-0034); sysfs (`locks::resolve_lock_leds`) is a read-once fallback, so
/// neither source leaves all locks at their logged `false` defaults.
fn resolve_locks(leds_root: &Path, state: &Mutex<KeyboardState>) -> Option<evdev::EventStream> {
    if let Some(stream) = open_led_stream(state) {
        return Some(stream);
    }
    debug!(
        "no readable evdev device with LED_CAPSL capability; falling back to a one-time sysfs LED read for lock state"
    );
    let Some(leds) = resolve_lock_leds(leds_root) else {
        debug!(
            "no lock-state source available (neither evdev nor sysfs LED nodes); caps/num/scroll_lock will stay false"
        );
        return None;
    };
    let mut guard = state.lock().expect("mutex poisoned");
    match read_led_on(&leds.caps) {
        Ok(on) => guard.caps_lock = on,
        Err(err) => debug!("failed to read the sysfs capslock LED; caps_lock will stay false: {err}"),
    }
    match read_led_on(&leds.num) {
        Ok(on) => guard.num_lock = on,
        Err(err) => debug!("failed to read the sysfs numlock LED; num_lock will stay false: {err}"),
    }
    match read_led_on(&leds.scroll) {
        Ok(on) => guard.scroll_lock = on,
        Err(err) => debug!("failed to read the sysfs scrolllock LED; scroll_lock will stay false: {err}"),
    }
    None
}

/// Builds the `input` udev watch, like `brightness::controller::build_backlight_watch`.
fn build_input_watch() -> std::io::Result<AsyncFd<MonitorSocket>> {
    let socket = udev::MonitorBuilder::new()?.match_subsystem("input")?.listen()?;
    AsyncFd::new(socket)
}

/// Forwards `EV_LED` changes until the stream ends, which unplugging the keyboard does with
/// `ENODEV`. `false` means the signal receiver is gone and the watch should stop for good.
async fn pump_leds(
    stream: &mut evdev::EventStream,
    state: &Mutex<KeyboardState>,
    events: &UnboundedSender<()>,
) -> bool {
    loop {
        let event = match stream.next_event().await {
            Ok(event) => event,
            Err(err) => {
                debug!("evdev LED stream ended ({err}); waiting for a keyboard to appear");
                return true;
            }
        };
        let evdev::EventSummary::Led(_, code, value) = event.destructure() else { continue };
        let on = value != 0;
        {
            let mut guard = state.lock().expect("mutex poisoned");
            match code {
                evdev::LedCode::LED_CAPSL => guard.caps_lock = on,
                evdev::LedCode::LED_NUML => guard.num_lock = on,
                evdev::LedCode::LED_SCROLLL => guard.scroll_lock = on,
                _ => continue,
            }
        }
        if events.send(()).is_err() {
            return false;
        }
    }
}

/// Re-opens the stream on every `input` uevent: a replugged keyboard is a new `/dev/input/event*`
/// node, which the fd that died with the old one never sees. Without this the locks stay frozen
/// where the unplug left them until the shell restarts.
async fn watch_locks(first: Option<evdev::EventStream>, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<()>) {
    let mut stream = first;
    let mut watch = build_input_watch()
        .inspect_err(|err| {
            error!("failed to set up the udev input watch ({err}); lock state will freeze if the keyboard is replugged")
        })
        .ok();
    loop {
        if let Some(mut live) = stream.take() {
            if !pump_leds(&mut live, &state, &events).await {
                return;
            }
            continue;
        }
        let Some(watch) = watch.as_mut() else { return };
        let mut guard = match watch.readable_mut().await {
            Ok(guard) => guard,
            Err(err) => {
                error!(
                    "the udev input watch's fd errored ({err}); lock state will freeze if the keyboard is replugged"
                );
                return;
            }
        };
        for _event in guard.get_inner().iter() {}
        guard.clear_ready();
        stream = open_led_stream(&state);
        if stream.is_some() && events.send(()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_state_default_reports_nothing_available() {
        assert_eq!(
            KeyboardState::default(),
            KeyboardState {
                backlight_percent: None,
                caps_lock: false,
                num_lock: false,
                scroll_lock: false,
                active_layout: String::new(),
                active_layout_index: 0,
                layout_count: 0
            }
        );
    }

    #[test]
    fn find_backlight_skips_lock_leds_and_scales_brightness() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("input3::capslock")).unwrap();
        let dir = root.path().join("asus::kbd_backlight");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("max_brightness"), "0\n").unwrap();
        assert_eq!(find_backlight(root.path()), None);

        // Sorts after the dead one, which must not hide it.
        let dir = root.path().join("tpacpi::kbd_backlight");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("max_brightness"), "3\n").unwrap();
        std::fs::write(dir.join("brightness"), "2\n").unwrap();
        let led = find_backlight(root.path()).expect("kbd_backlight with max > 0");
        assert_eq!(read_backlight_percent(&led), Some(67));
    }
}

//! [`KeyboardController`] owns `mantle.keyboard` state and write actions. Backlight, lock state,
//! and layout share one `Arc<Mutex<KeyboardState>>` and signal channel (ADR-0034).

pub use shared::state::keyboard::KeyboardState;

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use shared::{debug, error};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{Compositor, CompositorKind, backend_or_unsupported};

use super::super::brightness::controller::set_brightness;
use super::super::scale::{raw_from_percent, read_percent};
use super::super::{read_parsed, udev_watch};
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
    compositor: &'static dyn Compositor,
    system_bus: zbus::Connection,
    events: UnboundedSender<()>,
    pub(super) writes: super::super::LatestWrites,
}

impl super::super::Writer for KeyboardController {
    /// `keyboard:set_backlight(pct)`. Logs and returns without keyboard-backlight hardware.
    async fn write(&self, pct: f64) {
        let Some(led) = self.backlight.as_ref() else {
            debug!("set_backlight called but this machine has no keyboard backlight; ignored");
            return;
        };
        let name = led.dir.file_name().unwrap_or_default().to_string_lossy();
        let raw = raw_from_percent(pct, led.max) as u32;
        if let Err(err) = set_brightness(&self.system_bus, "leds", &name, raw).await {
            debug!("SetBrightness(leds, {name}, {raw}) failed: {err}");
        }
        // `brightness_hw_changed` reports only hardware changes, so read this write back.
        self.state.lock().expect("mutex poisoned").backlight_percent = read_percent(&led.dir, led.max);
        let _ = self.events.send(());
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
        Self {
            state,
            backlight: Arc::new(backlight),
            compositor: backend_or_unsupported(compositor, "layout"),
            system_bus,
            events: events_tx,
            writes: Default::default(),
        }
    }

    /// `keyboard:switch_layout(index)`; synchronous because `Compositor::switch_layout` is
    /// fire-and-forget.
    pub fn switch_layout(&self, index: usize) {
        self.compositor.switch_layout(index);
    }

    pub fn snapshot(&self) -> KeyboardState {
        self.state.lock().expect("mutex poisoned").clone()
    }
}

/// First `*::kbd_backlight` by name with `max_brightness > 0`; a dead sibling must not hide a usable LED.
fn find_backlight(leds_root: &Path) -> Option<LedBacklight> {
    find_leds(leds_root, "::kbd_backlight").into_iter().find_map(|dir| {
        let max = read_parsed(&dir, "max_brightness").filter(|max| *max > 0)?;
        Some(LedBacklight { dir, max })
    })
}

/// Opens `brightness_hw_changed` before the initial read so a hotkey in between is not lost, then
/// re-reads on each `POLLPRI`, which the kernel raises only for hardware changes (ADR-0034.1).
fn watch_backlight(led: LedBacklight, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<()>) {
    let watch = std::fs::File::open(led.dir.join("brightness_hw_changed"))
        // SAFETY: the AsyncFd owns the file, so its fd stays open and unchanged until the AsyncFd drops.
        .and_then(|file| Ok(unsafe { AsyncFd::register_with_interest(file, Interest::PRIORITY) }?));
    state.lock().expect("mutex poisoned").backlight_percent = read_percent(&led.dir, led.max);
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
            state.lock().expect("mutex poisoned").backlight_percent = read_percent(&led.dir, led.max);
            if events.send(()).is_err() {
                break;
            }
        }
    });
}

/// `linux/input-event-codes.h`: `EV_SYN` 0, `SYN_REPORT` 0, `SYN_DROPPED` 3, `EV_LED` 0x11,
/// `LED_NUML` 0, `LED_CAPSL` 1, `LED_SCROLLL` 2.
const EV_SYN: u16 = 0x00;
const SYN_REPORT: u16 = 0x00;
const SYN_DROPPED: u16 = 0x03;
const EV_LED: u16 = 0x11;
const LED_NUML: u16 = 0x00;
const LED_CAPSL: u16 = 0x01;
const LED_SCROLLL: u16 = 0x02;

/// `struct input_event` on this ABI: `timeval` (16) + type + code + value.
const INPUT_EVENT_SIZE: usize = 24;
const _: () = assert!(std::mem::size_of::<libc::input_event>() == INPUT_EVENT_SIZE);
const _: () = assert!(std::mem::offset_of!(libc::input_event, type_) == 16);
const _: () = assert!(std::mem::offset_of!(libc::input_event, code) == 18);
const _: () = assert!(std::mem::offset_of!(libc::input_event, value) == 20);

nix::ioctl_read_buf!(eviocgbit_types, b'E', 0x20, u8);
nix::ioctl_read_buf!(eviocgbit_leds, b'E', 0x20 + 0x11, u8);
nix::ioctl_read_buf!(eviocgled, b'E', 0x19, u8);

fn bit_set(bits: &[u8], index: u16) -> bool {
    let index = usize::from(index);
    bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

fn event_fields(packet: &[u8; INPUT_EVENT_SIZE]) -> (u16, u16, i32) {
    const TYPE: usize = std::mem::offset_of!(libc::input_event, type_);
    const CODE: usize = std::mem::offset_of!(libc::input_event, code);
    const VALUE: usize = std::mem::offset_of!(libc::input_event, value);
    (
        u16::from_ne_bytes([packet[TYPE], packet[TYPE + 1]]),
        u16::from_ne_bytes([packet[CODE], packet[CODE + 1]]),
        i32::from_ne_bytes([packet[VALUE], packet[VALUE + 1], packet[VALUE + 2], packet[VALUE + 3]]),
    )
}

/// Applies one `input_event`. `true` only for `EV_LED` of caps, num, or scroll. `value != 0` is on.
fn apply_lock_event(state: &mut KeyboardState, packet: &[u8; INPUT_EVENT_SIZE]) -> bool {
    let (event_type, code, value) = event_fields(packet);
    if event_type != EV_LED {
        return false;
    }
    let on = value != 0;
    match code {
        LED_CAPSL => state.caps_lock = on,
        LED_NUML => state.num_lock = on,
        LED_SCROLLL => state.scroll_lock = on,
        _ => return false,
    }
    true
}

#[derive(Debug, PartialEq, Eq)]
enum Stream {
    Ignore,
    Apply,
    Resync,
}

/// Kernel evdev protocol: after `SYN_DROPPED` the queue is unreliable until the next `SYN_REPORT`,
/// where the state must be re-read. `dropped` carries that window across packets.
fn classify(dropped: &mut bool, packet: &[u8; INPUT_EVENT_SIZE]) -> Stream {
    match event_fields(packet) {
        (EV_SYN, SYN_DROPPED, _) => {
            *dropped = true;
            Stream::Ignore
        }
        (EV_SYN, SYN_REPORT, _) if std::mem::take(dropped) => Stream::Resync,
        _ if *dropped => Stream::Ignore,
        _ => Stream::Apply,
    }
}

/// `EV_LED` in the event bitmap, then `LED_CAPSL` in the LED bitmap. `EV_MAX` is 0x1f (4 bytes)
/// and `LED_MAX` is 0x0f (2 bytes).
fn supports_caps_led(file: &File) -> bool {
    let mut events = [0u8; 4];
    let mut leds = [0u8; 2];
    // SAFETY: `file` is an open evdev fd and each buffer is the size its ioctl is encoded with.
    unsafe {
        eviocgbit_types(file.as_raw_fd(), &mut events).is_ok()
            && bit_set(&events, EV_LED)
            && eviocgbit_leds(file.as_raw_fd(), &mut leds).is_ok()
            && bit_set(&leds, LED_CAPSL)
    }
}

/// Publishes the device's current LED bitmap.
fn sync_locks(file: &File, state: &Mutex<KeyboardState>) -> nix::Result<()> {
    let mut bits = [0u8; 2];
    // SAFETY: `file` is an open evdev fd and `bits` is the size the ioctl is encoded with.
    unsafe { eviocgled(file.as_raw_fd(), &mut bits)? };
    let mut guard = state.lock().expect("mutex poisoned");
    guard.caps_lock = bit_set(&bits, LED_CAPSL);
    guard.num_lock = bit_set(&bits, LED_NUML);
    guard.scroll_lock = bit_set(&bits, LED_SCROLLL);
    Ok(())
}

/// First readable `/dev/input/event*` that exposes `LED_CAPSL`. Publishes its LED bitmap and
/// returns the fd for live `EV_LED` events. The kernel queues those per open fd, so there is no
/// subscribe-before-read race. Silent when nothing is readable: that is the ordinary gap between
/// unplug and replug.
///
/// ponytail: the directory is `/dev/input` and the first `event*` with `LED_CAPSL` wins. A test
/// cannot mount device nodes the way `leds_root` stands in for sysfs. Upgrade: take the directory
/// the same way, once a fixture can expose event nodes.
fn open_led_stream(state: &Mutex<KeyboardState>) -> Option<AsyncFd<File>> {
    let entries = std::fs::read_dir("/dev/input").ok()?;
    let file = entries.flatten().find_map(|entry| {
        let path = entry.path();
        if !path.file_name()?.to_str()?.starts_with("event") {
            return None;
        }
        let file = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(&path).ok()?;
        supports_caps_led(&file).then_some(file)
    })?;
    if let Err(err) = sync_locks(&file, state) {
        debug!("failed to read initial evdev LED state; will pick up from the first EV_LED event: {err}");
    }
    // SAFETY: the AsyncFd owns the file, so its fd stays open and unchanged until the AsyncFd drops.
    unsafe { AsyncFd::register(file) }.inspect_err(|err| debug!("failed to open an EV_LED event stream: {err}")).ok()
}

/// Uses the input device first (ADR-0034); sysfs (`locks::resolve_lock_leds`) is a read-once
/// fallback, so neither source leaves all locks at their logged `false` defaults.
fn resolve_locks(leds_root: &Path, state: &Mutex<KeyboardState>) -> Option<AsyncFd<File>> {
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

/// Forwards `EV_LED` changes until the fd ends, which unplugging the keyboard does with `ENODEV`.
/// `false` means the signal receiver is gone and the watch should stop for good.
async fn pump_leds(fd: &AsyncFd<File>, state: &Mutex<KeyboardState>, events: &UnboundedSender<()>) -> bool {
    let mut dropped = false;
    loop {
        let mut ready = match fd.readable().await {
            Ok(ready) => ready,
            Err(err) => {
                debug!("evdev LED stream ended ({err}); waiting for a keyboard to appear");
                return true;
            }
        };
        loop {
            let mut buf = [0u8; INPUT_EVENT_SIZE * 32];
            match ready.try_io(|fd| fd.get_ref().read(&mut buf)) {
                Ok(Ok(0)) => {
                    debug!("evdev LED stream ended (eof); waiting for a keyboard to appear");
                    return true;
                }
                // The kernel returns whole events only, so `as_chunks` leaves no remainder.
                Ok(Ok(n)) => {
                    let mut changed = false;
                    for packet in buf[..n].as_chunks::<INPUT_EVENT_SIZE>().0 {
                        changed |= match classify(&mut dropped, packet) {
                            Stream::Ignore => false,
                            Stream::Apply => apply_lock_event(&mut state.lock().expect("mutex poisoned"), packet),
                            Stream::Resync => sync_locks(fd.get_ref(), state).is_ok(),
                        };
                    }
                    if changed && events.send(()).is_err() {
                        return false;
                    }
                }
                Ok(Err(err)) => {
                    debug!("evdev LED stream ended ({err}); waiting for a keyboard to appear");
                    return true;
                }
                Err(_would_block) => break,
            }
        }
    }
}

/// Re-opens the stream on every `input` uevent: a replugged keyboard is a new `/dev/input/event*`
/// node, which the fd that died with the old one never sees. Without this the locks stay frozen
/// where the unplug left them until the shell restarts.
async fn watch_locks(mut stream: Option<AsyncFd<File>>, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<()>) {
    let mut watch = udev_watch("input")
        .inspect_err(|err| {
            error!("failed to set up the udev input watch ({err}); lock state will freeze if the keyboard is replugged")
        })
        .ok();
    loop {
        if let Some(live) = stream.take() {
            if !pump_leds(&live, &state, &events).await {
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

/// `keyboard`'s state as the compositor reader sees it. The reader writes layout from its first
/// event, and signals once `keyboard` has started and set `events`.
#[derive(Clone, Default)]
pub struct LayoutSink {
    pub state: Arc<Mutex<KeyboardState>>,
    pub events: Arc<OnceLock<UnboundedSender<()>>>,
}

impl LayoutSink {
    pub fn write(&self, active_layout: String, active_layout_index: u32, layout_count: u32) {
        {
            let mut guard = self.state.lock().expect("mutex poisoned");
            guard.active_layout = active_layout;
            guard.active_layout_index = active_layout_index;
            guard.layout_count = layout_count;
        }
        if let Some(events) = self.events.get() {
            let _ = events.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(read_percent(&led.dir, led.max), Some(67));
    }

    fn input_event(type_: u16, code: u16, value: i32) -> [u8; 24] {
        let mut packet = [0u8; 24];
        packet[16..18].copy_from_slice(&type_.to_ne_bytes());
        packet[18..20].copy_from_slice(&code.to_ne_bytes());
        packet[20..24].copy_from_slice(&value.to_ne_bytes());
        packet
    }

    #[test]
    fn ev_led_packet_updates_only_that_lock() {
        let mut state = KeyboardState::default();
        assert!(apply_lock_event(&mut state, &input_event(0x11, 0x01, 1)));
        assert!(state.caps_lock && !state.num_lock && !state.scroll_lock);
        assert!(apply_lock_event(&mut state, &input_event(0x11, 0x00, 5)));
        assert!(state.caps_lock && state.num_lock && !state.scroll_lock);
        assert!(apply_lock_event(&mut state, &input_event(0x11, 0x02, -1)));
        assert!(state.caps_lock && state.num_lock && state.scroll_lock);
        assert!(apply_lock_event(&mut state, &input_event(0x11, 0x01, 0)));
        assert!(!state.caps_lock && state.num_lock && state.scroll_lock);

        assert!(!apply_lock_event(&mut state, &input_event(0x01, 0x01, 1)), "EV_KEY is ignored");
        assert!(!state.caps_lock, "a non-LED event must not turn caps back on");
        assert!(!apply_lock_event(&mut state, &input_event(0x11, 0x07, 1)), "LED_MUTE is ignored");
        assert!(state.num_lock && state.scroll_lock);
    }

    #[test]
    fn syn_dropped_ignores_events_until_syn_report_then_resyncs() {
        let mut dropped = false;
        let led = input_event(0x11, 0x01, 1);
        assert_eq!(classify(&mut dropped, &led), Stream::Apply);
        assert_eq!(classify(&mut dropped, &input_event(0, 3, 0)), Stream::Ignore);
        assert_eq!(classify(&mut dropped, &led), Stream::Ignore);
        assert_eq!(classify(&mut dropped, &input_event(0, 0, 0)), Stream::Resync);
        assert_eq!(classify(&mut dropped, &led), Stream::Apply);
        assert_eq!(classify(&mut dropped, &input_event(0, 0, 0)), Stream::Apply, "plain SYN_REPORT needs no resync");
    }
}

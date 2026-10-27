//! Lock-state monitoring for `mantle.keyboard` (ADR-0034). Sysfs LED `brightness` files do not
//! emit inotify `MODIFY` when `input_leds` changes them. `EV_LED` on the input device is primary
//! and sysfs is a read-once fallback.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use shared::state::keyboard::KeyboardState;
use shared::{debug, error, warn};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;

use super::super::udev_watch;

/// Resolved sysfs nodes for all three lock indicators. All-or-nothing: a missing sibling makes
/// [`resolve_lock_leds`] return `None` because hardware exposes all three or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockLeds {
    pub caps: PathBuf,
    pub num: PathBuf,
    pub scroll: PathBuf,
}

/// `leds_root` directories ending in `::<suffix>`, sorted by name so the pick does not follow
/// `read_dir` order; LED-class names are `<device>::<function>`.
pub(super) fn find_leds(leds_root: &Path, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(leds_root) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(suffix))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// The first device, by name, with all three lock LEDs, so they never mix across keyboards.
pub fn resolve_lock_leds(leds_root: &Path) -> Option<LockLeds> {
    find_leds(leds_root, "::capslock").into_iter().find_map(|caps| {
        let device = caps.file_name()?.to_str()?.strip_suffix("::capslock")?;
        let sibling = |function| {
            let dir = caps.with_file_name(format!("{device}{function}"));
            dir.is_dir().then_some(dir)
        };
        Some(LockLeds { num: sibling("::numlock")?, scroll: sibling("::scrolllock")?, caps })
    })
}

/// Reads one LED's `brightness`: `0` is off, any nonzero value on. Use `!= 0`, since
/// `max_brightness` need not be `1`.
pub fn read_led_on(led_dir: &Path) -> io::Result<bool> {
    let text = std::fs::read_to_string(led_dir.join("brightness"))?;
    Ok(text.trim().parse::<i64>().unwrap_or(0) != 0)
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

/// Why [`find_led_device`] found nothing.
#[derive(Debug, PartialEq, Eq)]
enum NoLedDevice {
    /// No `event*` node with `LED_CAPSL`: the ordinary gap between unplug and replug.
    Absent,
    /// A node or `dir` exists but this user cannot open it.
    Denied,
}

/// First readable `event*` in `dir` that exposes `LED_CAPSL`. One unreadable node does not stop the
/// search, but it decides the failure when no other node qualifies.
fn find_led_device(dir: &Path) -> Result<File, NoLedDevice> {
    let denied = |err: &io::Error| err.kind() == io::ErrorKind::PermissionDenied;
    let entries =
        std::fs::read_dir(dir).map_err(|err| if denied(&err) { NoLedDevice::Denied } else { NoLedDevice::Absent })?;
    let mut any_denied = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("event")) {
            continue;
        }
        match File::options().read(true).custom_flags(libc::O_NONBLOCK).open(&path) {
            Ok(file) if supports_caps_led(&file) => return Ok(file),
            Ok(_) => {}
            Err(err) => any_denied |= denied(&err),
        }
    }
    Err(if any_denied { NoLedDevice::Denied } else { NoLedDevice::Absent })
}

/// First readable `/dev/input/event*` that exposes `LED_CAPSL`. Publishes its LED bitmap and
/// returns the fd for live `EV_LED` events. The kernel queues those per open fd, so there is no
/// subscribe-before-read race. Silent when nothing is there; warns once when the nodes exist but
/// cannot be opened.
fn open_led_stream(state: &Mutex<KeyboardState>) -> Option<AsyncFd<File>> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let file = match find_led_device(Path::new("/dev/input")) {
        Ok(file) => file,
        Err(NoLedDevice::Denied) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                warn!("cannot open /dev/input/event*; lock state will freeze. Add the user to the `input` group");
            }
            return None;
        }
        Err(NoLedDevice::Absent) => return None,
    };
    if let Err(err) = sync_locks(&file, state) {
        debug!("failed to read initial evdev LED state; will pick up from the first EV_LED event: {err}");
    }
    // SAFETY: the AsyncFd owns the file, so its fd stays open and unchanged until the AsyncFd drops.
    unsafe { AsyncFd::register(file) }.inspect_err(|err| debug!("failed to open an EV_LED event stream: {err}")).ok()
}

/// Uses the input device first (ADR-0034); sysfs (`resolve_lock_leds`) is a read-once
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
pub(super) fn watch_locks(leds_root: &Path, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<()>) {
    let mut stream = resolve_locks(leds_root, &state);
    tokio::spawn(async move {
        let mut watch = udev_watch("input")
            .inspect_err(|err| {
                error!(
                    "failed to set up the udev input watch ({err}); lock state will freeze if the keyboard is replugged"
                )
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
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_led(dir: &Path, name: &str, brightness: &str) -> PathBuf {
        let led_dir = dir.join(name);
        std::fs::create_dir(&led_dir).unwrap();
        std::fs::write(led_dir.join("brightness"), brightness).unwrap();
        led_dir
    }

    #[test]
    fn find_led_device_tells_an_unreadable_node_from_an_empty_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_led_device(dir.path()).err(), Some(NoLedDevice::Absent), "empty");
        assert_eq!(find_led_device(&dir.path().join("missing")).err(), Some(NoLedDevice::Absent));
        std::fs::write(dir.path().join("event0"), b"").unwrap();
        assert_eq!(find_led_device(dir.path()).err(), Some(NoLedDevice::Absent), "readable, no LED_CAPSL");
        let locked = dir.path().join("event1");
        std::fs::write(&locked, b"").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(&locked).is_ok() {
            return; // root ignores the mode bits
        }
        assert_eq!(find_led_device(dir.path()).err(), Some(NoLedDevice::Denied));
    }

    #[test]
    fn resolve_lock_leds_is_none_when_any_led_is_missing_or_the_root_is() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(resolve_lock_leds(root.path()), None, "empty root");
        assert_eq!(resolve_lock_leds(&root.path().join("does-not-exist")), None);
        write_led(root.path(), "input3::capslock", "0");
        write_led(root.path(), "input3::numlock", "1");
        assert_eq!(resolve_lock_leds(root.path()), None, "scrolllock absent");
    }

    #[test]
    fn resolve_lock_leds_takes_all_three_from_one_device() {
        let root = tempfile::tempdir().unwrap();
        write_led(root.path(), "input3::capslock", "0");
        write_led(root.path(), "input3::numlock", "0");
        let caps = write_led(root.path(), "input5::capslock", "0");
        let num = write_led(root.path(), "input5::numlock", "0");
        let scroll = write_led(root.path(), "input5::scrolllock", "0");

        assert_eq!(resolve_lock_leds(root.path()), Some(LockLeds { caps, num, scroll }));
    }

    #[test]
    fn read_led_on_reads_nonzero_as_on_and_malformed_as_off() {
        let root = tempfile::tempdir().unwrap();
        for (brightness, on) in [("1", true), ("0", false), ("not-a-number\n", false)] {
            let led = write_led(root.path(), &format!("input3::led{brightness}"), brightness);
            assert_eq!(read_led_on(&led).unwrap(), on, "{brightness:?}");
        }
        assert!(read_led_on(&root.path().join("missing")).is_err());
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

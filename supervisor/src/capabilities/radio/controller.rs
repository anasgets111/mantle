//! [`RadioController`] owns `mantle.radio` and its block actions.

pub use shared::state::radio::{Radio, RadioKind, RadioState};

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use shared::{debug, warn};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;

use super::super::publish;

const OP_ADD: u8 = 0;
const OP_DEL: u8 = 1;
const OP_CHANGE: u8 = 2;
const OP_CHANGE_ALL: u8 = 3;

/// `struct rfkill_event` (`idx: u32, type, op, soft, hard`), native endian. Reads ask for exactly
/// this size: the kernel returns one event per read, and `rfkill_event_ext` is opt-in by ioctl.
const EVENT_SIZE: usize = 8;

/// idx -> (kind, soft, hard)
type Devices = HashMap<u32, (RadioKind, bool, bool)>;

/// Folds one raw event into `devices`. A short read is dropped; unknown types are not tracked.
fn apply(devices: &mut Devices, raw: &[u8]) {
    let Some(&[a, b, c, d, ty, op, soft, hard]) = raw.get(..EVENT_SIZE) else {
        debug!("short rfkill read of {} bytes; ignored", raw.len());
        return;
    };
    let idx = u32::from_ne_bytes([a, b, c, d]);
    match op {
        OP_ADD | OP_CHANGE => match RadioKind::from_type(ty) {
            Some(kind) => {
                devices.insert(idx, (kind, soft != 0, hard != 0));
            }
            None => debug!("rfkill device {idx} has unknown type {ty}; skipped"),
        },
        OP_DEL => {
            devices.remove(&idx);
        }
        _ => {} // CHANGE_ALL is a request, never reported
    }
}

/// One entry per kind with a device, ordered by type id; blocked when any device of the kind is.
fn aggregate(devices: &Devices) -> RadioState {
    let mut kinds = BTreeMap::<RadioKind, (bool, bool)>::new();
    for &(kind, soft, hard) in devices.values() {
        let blocked = kinds.entry(kind).or_default();
        blocked.0 |= soft;
        blocked.1 |= hard;
    }
    let radios =
        kinds.into_iter().map(|(kind, (soft_blocked, hard_blocked))| Radio { kind, soft_blocked, hard_blocked });
    RadioState { radios: radios.collect() }
}

/// One `CHANGE_ALL` request; type `0` covers every kind.
fn send(mut device: &File, ty: u8, blocked: bool) -> std::io::Result<()> {
    let mut event = [0u8; EVENT_SIZE];
    event[4] = ty;
    event[5] = OP_CHANGE_ALL;
    event[6] = blocked.into();
    device.write_all(&event)
}

/// Read-write, or read-only when permissions deny writing (a non-seat session). `false` marks the latter.
fn open(path: &Path) -> std::io::Result<(File, bool)> {
    let open = |write| File::options().read(true).write(write).custom_flags(libc::O_NONBLOCK).open(path);
    match open(true) {
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Ok((open(false)?, false)),
        opened => opened.map(|file| (file, true)),
    }
}

pub struct RadioController {
    state: Arc<Mutex<RadioState>>,
    device: Option<(Arc<AsyncFd<File>>, bool)>,
}

impl RadioController {
    /// `path` is `/dev/rfkill` in production. An unopenable device skips the read task and drops
    /// `events`, so no signal is ever sent (see `radio/mod.rs`).
    pub fn new(path: &Path, events: UnboundedSender<()>) -> Self {
        let state = Arc::new(Mutex::new(RadioState::default()));
        let device = match open(path).and_then(|(file, writable)| Ok((AsyncFd::new(file)?, writable))) {
            Ok((fd, writable)) => {
                let fd = Arc::new(fd);
                if !writable {
                    debug!("{path:?} is read-only for this session; radio actions are ignored");
                }
                tokio::spawn(run(Arc::clone(&fd), Arc::clone(&state), events));
                Some((fd, writable))
            }
            Err(err) => {
                debug!("cannot open {path:?} ({err}); radio reporting disabled for this run");
                None
            }
        };
        Self { state, device }
    }

    pub fn snapshot(&self) -> RadioState {
        self.state.lock().expect("radio state mutex poisoned").clone()
    }

    /// `radio:set_blocked(kind, blocked)`, or every kind when `kind` is `None`. State changes
    /// arrive back through the event stream, not an optimistic update.
    pub fn set_blocked(&self, kind: Option<RadioKind>, blocked: bool) {
        let Some((fd, true)) = &self.device else {
            debug!("action ignored: the rfkill device is missing or read-only for this session");
            return;
        };
        let ty = kind.map_or(0, |kind| kind as u8);
        if let Err(err) = send(fd.get_ref(), ty, blocked) {
            warn!("rfkill write failed: {err}");
        }
    }
}

/// Folds the event stream, publishing once per drain to `EAGAIN` and only on change: the initial
/// ADDs land as one push, and while no known radio exists the empty list matches the default state,
/// so Lua keeps `nil` until one appears.
async fn run(fd: Arc<AsyncFd<File>>, state: Arc<Mutex<RadioState>>, events: UnboundedSender<()>) {
    let mut devices = Devices::new();
    let mut buf = [0u8; EVENT_SIZE];
    loop {
        let mut guard = match fd.readable().await {
            Ok(guard) => guard,
            Err(err) => return warn!("the rfkill fd errored ({err}); radio updates stop"),
        };
        loop {
            match guard.try_io(|fd| fd.get_ref().read(&mut buf)) {
                Ok(Ok(0)) => return warn!("the rfkill fd closed; radio updates stop"),
                Ok(Ok(n)) => apply(&mut devices, &buf[..n]),
                Ok(Err(err)) => return warn!("reading the rfkill fd failed ({err}); radio updates stop"),
                Err(_would_block) => break,
            }
        }
        if !publish(&state, &events, aggregate(&devices)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::within;

    fn event(idx: u32, ty: u8, op: u8, soft: bool, hard: bool) -> Vec<u8> {
        let mut raw = idx.to_ne_bytes().to_vec();
        raw.extend([ty, op, soft.into(), hard.into()]);
        raw
    }

    fn fold(events: &[Vec<u8>]) -> RadioState {
        let mut devices = Devices::new();
        events.iter().for_each(|raw| apply(&mut devices, raw));
        aggregate(&devices)
    }

    fn radio(kind: RadioKind, soft_blocked: bool, hard_blocked: bool) -> Radio {
        Radio { kind, soft_blocked, hard_blocked }
    }

    #[test]
    fn events_fold_into_one_entry_per_kind_ordered_by_type_id() {
        let state = fold(&[
            event(0, 2, OP_ADD, false, false),
            event(1, 1, OP_ADD, false, false),
            event(2, 1, OP_ADD, true, false),
            event(3, 99, OP_ADD, true, true), // unknown type: skipped
        ]);
        assert_eq!(state.radios, [radio(RadioKind::Wlan, true, false), radio(RadioKind::Bluetooth, false, false)]);

        let state = fold(&[
            event(0, 1, OP_ADD, true, false),
            event(0, 1, OP_CHANGE, false, true),
            event(1, 5, OP_ADD, false, false),
            event(1, 5, OP_DEL, false, false),
            event(2, 1, OP_CHANGE_ALL, true, false),
        ]);
        assert_eq!(
            state.radios,
            [radio(RadioKind::Wlan, false, true)],
            "CHANGE replaces, DEL removes, CHANGE_ALL is ignored"
        );
    }

    #[test]
    fn a_short_read_is_dropped_and_trailing_bytes_are_ignored() {
        let mut extended = event(0, 1, OP_ADD, true, false);
        extended.push(7);
        assert_eq!(fold(&[extended[..5].to_vec()]).radios, []);
        assert_eq!(fold(&[extended]).radios, [radio(RadioKind::Wlan, true, false)]);
    }

    #[test]
    fn send_writes_one_change_all_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rfkill");
        let file = File::create(&path).unwrap();
        send(&file, 2, true).unwrap();
        send(&file, 0, false).unwrap();
        let mut expected = event(0, 2, OP_CHANGE_ALL, true, false);
        expected.extend(event(0, 0, OP_CHANGE_ALL, false, false));
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }

    #[tokio::test]
    async fn the_first_push_follows_the_initial_events_and_later_ones_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rfkill");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let mut feed = File::options().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&path).unwrap();
        feed.write_all(&event(0, 1, OP_ADD, false, false)).unwrap();
        feed.write_all(&event(1, 2, OP_ADD, true, false)).unwrap();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = RadioController::new(&path, events_tx);

        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(
            controller.snapshot().radios,
            [radio(RadioKind::Wlan, false, false), radio(RadioKind::Bluetooth, true, false)]
        );
        assert!(events_rx.try_recv().is_err(), "both ADDs are one push");

        feed.write_all(&event(1, 2, OP_CHANGE, true, false)).unwrap(); // no change
        feed.write_all(&event(0, 1, OP_CHANGE, true, false)).unwrap();
        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(controller.snapshot().radios[0], radio(RadioKind::Wlan, true, false));
        assert!(events_rx.try_recv().is_err(), "an unchanged aggregate pushes nothing");
    }

    #[tokio::test]
    async fn radio_controller_stays_silent_until_a_radio_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rfkill");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let mut feed = File::options().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&path).unwrap();
        feed.write_all(&event(0, 99, OP_ADD, true, false)).unwrap(); // unknown type
        feed.write_all(&event(1, 1, OP_ADD, false, false)).unwrap();
        feed.write_all(&event(1, 1, OP_DEL, false, false)).unwrap();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = RadioController::new(&path, events_tx);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), events_rx.recv()).await.is_err());

        feed.write_all(&event(0, 1, OP_ADD, false, false)).unwrap();
        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(controller.snapshot().radios, [radio(RadioKind::Wlan, false, false)]);
    }

    #[tokio::test]
    async fn radio_controller_never_signals_without_a_device_node() {
        let dir = tempfile::tempdir().unwrap();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = RadioController::new(&dir.path().join("missing"), events_tx);

        assert_eq!(within(events_rx.recv()).await, None, "no device node means no signal");
        assert_eq!(controller.snapshot(), RadioState::default());
    }
}

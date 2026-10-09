//! [`PrivacyController`] owns read-only `mantle.privacy` telemetry (ADR-0034).

pub use shared::state::privacy::{PrivacyState, PrivacyUser};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use shared::warn;
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::publish;
use tokio::sync::watch;

use crate::capabilities::audio::mixer::{CaptureApp, PrivacySources, VideoSourceApp};

use super::video::{enumerate_video_devices, find_device_openers, holds_device, is_video_name, read_comm};

/// Names scanned opener pids against the latest PipeWire `Video/Source` snapshot. A matching
/// PipeWire `app_name` wins, then `/proc/{pid}/comm`, then `pid {n}`; no opener is dropped. Pure
/// and unit-testable.
///
/// Takes `pids` rather than finding them: a new PipeWire `Video/Source` renames an existing opener,
/// not the `/dev/videoN` set. A fresh scan would spend the expensive scan for a string.
fn name_camera_users(proc_root: &Path, pids: &[u32], pipewire: &[VideoSourceApp]) -> Vec<PrivacyUser> {
    pids.iter()
        .copied()
        .map(|pid| {
            let app_name = pipewire
                .iter()
                .find(|source| source.pid == pid as i32)
                .and_then(|source| source.app_name.clone())
                .or_else(|| read_comm(proc_root, pid))
                .unwrap_or_else(|| format!("pid {pid}"));
            PrivacyUser { app_name }
        })
        .collect()
}

/// Names PipeWire captures by client `application.name`, then `/proc/{pid}/comm`, then `pid {n}`,
/// finally node id. Portal-created streams may have no pid, but remain captures.
///
/// Deduplicated by name, not node: browsers open one node per tab, but users need one Firefox row.
fn name_capture_users(proc_root: &Path, apps: &[CaptureApp]) -> Vec<PrivacyUser> {
    let mut users: Vec<PrivacyUser> = Vec::new();
    for app in apps {
        let app_name = app
            .app_name
            .clone()
            .or_else(|| app.pid.and_then(|pid| read_comm(proc_root, pid as u32)))
            .or_else(|| app.pid.map(|pid| format!("pid {pid}")))
            .unwrap_or_else(|| format!("node {}", app.node_id));
        if !users.iter().any(|user| user.app_name == app_name) {
            users.push(PrivacyUser { app_name });
        }
    }
    users
}

/// Test-only scan-plus-name convenience. Production calls the halves in sequence and retains pids.
#[cfg(test)]
fn resolve_camera_users(proc_root: &Path, devices: &[PathBuf], pipewire: &[VideoSourceApp]) -> Vec<PrivacyUser> {
    name_camera_users(proc_root, &find_device_openers(proc_root, devices), pipewire)
}

pub struct PrivacyController {
    state: Arc<Mutex<PrivacyState>>,
}

impl PrivacyController {
    /// `proc_root`/`dev_root` (defaults `/proc`/`/dev`) are injected per the sysfs/procfs test
    /// convention. `sources` is the PipeWire connection shared with
    /// `mantle.audio` (ADR-0034). Returns immediately.
    pub fn new(
        proc_root: PathBuf,
        dev_root: &Path,
        sources: watch::Receiver<PrivacySources>,
        events: UnboundedSender<()>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        tokio::spawn(run_privacy_task(proc_root, dev_root.to_path_buf(), Arc::clone(&state), sources, events));
        Self { state }
    }

    pub fn snapshot(&self) -> PrivacyState {
        self.state.lock().expect("mutex poisoned").clone()
    }
}

/// Watches `dev_root` and its video devices for live-reliable `OPEN`/`CLOSE` (see `privacy::video`) and
/// drains `sources`. Either triggers a full rebuild of all three lists; only a device event pays for
/// the `/proc` scan, once per burst of ready events.
///
/// The camera watch is optional, not the loop. Without a webcam, or after `Inotify`/stream failure,
/// `camera_users` stays empty while microphone and screencast continue. Before ADR-0137 those
/// failures ended the task, which was correct when camera was the only answer.
async fn run_privacy_task(
    proc_root: PathBuf,
    dev_root: PathBuf,
    state: Arc<Mutex<PrivacyState>>,
    mut sources: watch::Receiver<PrivacySources>,
    events: UnboundedSender<()>,
) {
    let mut devices = enumerate_video_devices(&dev_root);
    let mut inotify_stream = watch_video_devices(&dev_root, &devices);
    // Whatever the mixer last published, not an empty seed: this capability starts on first config
    // read, which can be long after the mixer hydrated.
    let mut pipewire = sources.borrow_and_update().clone();

    // Scan once: a camera may already be open at startup. The other lists await PipeWire.
    let mut opener_pids = find_device_openers(&proc_root, &devices);
    *state.lock().expect("mutex poisoned") = rebuild(&proc_root, &opener_pids, &pipewire);
    if events.send(()).is_err() {
        return;
    }

    let mut mixer_alive = true;
    loop {
        tokio::select! {
            event = next_device_event(&mut inotify_stream, &mut devices) => {
                // An open or an emptied set pays for the scan, a readlink of every fd in `/proc`, so
                // it runs off the two async workers.
                let rescan = match event {
                    DeviceEvent::Opened => true,
                    // A close only removes pids: recheck the known openers instead of walking `/proc`.
                    // A pid that dropped out may have passed the fd to a child no open announced: walk.
                    DeviceEvent::Closed => {
                        let known = opener_pids.len();
                        opener_pids.retain(|&pid| holds_device(&proc_root, pid, &devices));
                        opener_pids.len() < known
                    }
                    DeviceEvent::Failed(err) => {
                        warn!("inotify read failed: {err}");
                        continue;
                    }
                    // All watched device fds closed; drop the watch but keep the task.
                    DeviceEvent::Ended => {
                        inotify_stream = None;
                        continue;
                    }
                };
                if rescan {
                    let (root, watched) = (proc_root.clone(), devices.clone());
                    if let Ok(pids) = tokio::task::spawn_blocking(move || find_device_openers(&root, &watched)).await {
                        opener_pids = pids;
                    }
                }
            }
            changed = sources.changed(), if mixer_alive => {
                // Name camera users and rebuild the other lists from the same pid set.
                if changed.is_err() {
                    // The mixer thread is gone (it logs why); the PipeWire lists freeze, the camera
                    // watch stays.
                    mixer_alive = false;
                    continue;
                }
                pipewire = sources.borrow_and_update().clone();
            }
        }
        if !publish(&state, &events, rebuild(&proc_root, &opener_pids, &pipewire)) {
            break;
        }
    }
}

/// Rebuilds all three lists every time. The PipeWire lists are cheap walks, and one write prevents
/// a config observing one list a push behind.
fn rebuild(proc_root: &Path, opener_pids: &[u32], pipewire: &PrivacySources) -> PrivacyState {
    PrivacyState {
        camera_users: name_camera_users(proc_root, opener_pids, &pipewire.cameras),
        microphone_users: name_capture_users(proc_root, &pipewire.microphones),
        screencast_users: name_capture_users(proc_root, &pipewire.screencasts),
    }
}

/// Inotify on `dev_root`, so a hot-plugged node is seen, and on each current `videoN`. Failure
/// costs only `camera_users` and is logged.
fn watch_video_devices(dev_root: &Path, devices: &[PathBuf]) -> Option<VideoWatch> {
    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(err) => {
            warn!("failed to initialize inotify; camera detection disabled for this run: {err}");
            return None;
        }
    };
    let mut watches = inotify.watches();
    let dir = match watches.add(dev_root, WatchMask::CREATE | WatchMask::DELETE) {
        Ok(dir) => dir,
        Err(err) => {
            warn!("failed to watch {}; cameras plugged in later won't be detected: {err}", dev_root.display());
            return None;
        }
    };
    for device in devices {
        watch_device(&mut watches, device);
    }
    match inotify.into_event_stream(vec![0u8; 4096]) {
        // An app probing every node opens and closes each; one scan answers the whole burst.
        Ok(stream) => Some(VideoWatch { events: stream.ready_chunks(64), watches, root: dev_root.to_path_buf(), dir }),
        Err(err) => {
            warn!("failed to start the inotify event stream; camera detection disabled for this run: {err}");
            None
        }
    }
}

fn watch_device(watches: &mut Watches, device: &Path) {
    if let Err(err) = watches.add(device, WatchMask::OPEN | WatchMask::CLOSE) {
        warn!("failed to watch {}; camera opens on it won't be detected: {err}", device.display());
    }
}

type DeviceEvents = futures_util::stream::ReadyChunks<inotify::EventStream<Vec<u8>>>;

struct VideoWatch {
    events: DeviceEvents,
    watches: Watches,
    root: PathBuf,
    /// The `dev_root` watch, whose create/delete events add and drop device watches.
    dir: WatchDescriptor,
}

/// Camera watch event. The enum names cases and lets [`next_device_event`] flatten a missing watch.
enum DeviceEvent {
    Opened,
    Closed,
    Failed(std::io::Error),
    Ended,
}

/// Next device open/close or node create/delete, or a never-completing future without a watch.
/// `select!` still needs an arm future in that case. A node that appears or goes keeps `devices`
/// and its watch current; a replugged node is a new inode, so the old watch is gone with it.
async fn next_device_event(watch: &mut Option<VideoWatch>, devices: &mut Vec<PathBuf>) -> DeviceEvent {
    let Some(watch) = watch.as_mut() else { return std::future::pending().await };
    loop {
        match watch.events.next().await {
            Some(events) if events.iter().any(Result::is_ok) => {
                let (mut relevant, mut opened) = (false, false);
                for event in events.iter().flatten() {
                    if event.wd != watch.dir {
                        opened |= event.mask.contains(EventMask::OPEN);
                        relevant |=
                            event.mask.intersects(EventMask::OPEN | EventMask::CLOSE_WRITE | EventMask::CLOSE_NOWRITE);
                        continue;
                    }
                    let name = event.name.as_deref().and_then(|name| name.to_str()).filter(|n| is_video_name(n));
                    let Some(name) = name else { continue };
                    let path = watch.root.join(name);
                    devices.retain(|device| *device != path);
                    if event.mask.contains(EventMask::CREATE) {
                        watch_device(&mut watch.watches, &path);
                        devices.push(path);
                    }
                    relevant = true;
                    opened = true;
                }
                if opened {
                    return DeviceEvent::Opened;
                }
                if relevant {
                    return DeviceEvent::Closed;
                }
            }
            Some(mut events) => return DeviceEvent::Failed(events.swap_remove(0).unwrap_err()),
            None => return DeviceEvent::Ended,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::privacy::video::write_fd_symlink;
    use crate::capabilities::test_support::within;

    fn video_source(pid: i32, app_name: &str) -> VideoSourceApp {
        VideoSourceApp { node_id: 1, pid, app_name: Some(app_name.to_string()) }
    }

    #[test]
    fn resolve_camera_users_is_empty_when_nobody_has_a_device_open() {
        let root = tempfile::tempdir().unwrap();
        assert!(resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]).is_empty());
    }

    /// Keeps scan and naming separate: a PipeWire snapshot can be answered without scanning. This
    /// root has no `/dev/video0` symlink, so an empty scan can still name the previous pid set.
    #[test]
    fn a_pipewire_snapshot_names_the_openers_already_found_rather_than_scanning_again() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("1234")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "raw-binary-name\n").unwrap();

        assert_eq!(
            name_camera_users(root.path(), &[1234], &[video_source(1234, "Cheese")]),
            vec![PrivacyUser { app_name: "Cheese".to_string() }]
        );
        assert!(
            resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]).is_empty(),
            "a scan of the same root finds nobody, so the answer above came from the pid set"
        );
    }

    #[test]
    fn resolve_camera_users_prefers_the_pipewire_app_name_over_proc_comm() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "raw-binary-name\n").unwrap();

        let users =
            resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[video_source(1234, "Firefox")]);

        assert_eq!(users, vec![PrivacyUser { app_name: "Firefox".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_falls_back_to_proc_comm_when_pipewire_has_no_match() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "mpv\n").unwrap();

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]);

        assert_eq!(users, vec![PrivacyUser { app_name: "mpv".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_falls_back_to_a_pid_placeholder_when_neither_source_resolves() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        // No comm file written, no matching pipewire source.

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]);

        assert_eq!(users, vec![PrivacyUser { app_name: "pid 1234".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_dedupes_a_pid_that_has_two_devices_open() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::os::unix::fs::symlink("/dev/video1", fd_dir.join("6")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "mpv\n").unwrap();

        let users =
            resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0"), PathBuf::from("/dev/video1")], &[]);

        assert_eq!(users, vec![PrivacyUser { app_name: "mpv".to_string() }]);
    }

    fn capture(node_id: u32, pid: Option<i32>, app_name: Option<&str>) -> CaptureApp {
        CaptureApp { node_id, pid, app_name: app_name.map(str::to_string), running: true }
    }

    #[test]
    fn a_capture_stream_is_named_by_the_name_its_client_published() {
        let root = tempfile::tempdir().unwrap();
        let users = name_capture_users(root.path(), &[capture(7, Some(1234), Some("Firefox"))]);

        assert_eq!(users, vec![PrivacyUser { app_name: "Firefox".to_string() }]);
    }

    #[test]
    fn a_capture_stream_with_no_published_name_falls_back_to_the_processs_own() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("1234")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "pw-cat\n").unwrap();

        let users = name_capture_users(root.path(), &[capture(7, Some(1234), None)]);

        assert_eq!(users, vec![PrivacyUser { app_name: "pw-cat".to_string() }]);
    }

    /// Portal-created streams may lack `application.process.id`; node id is the last resort, not a
    /// dropped capture.
    #[test]
    fn a_capture_stream_with_neither_a_name_nor_a_pid_is_named_by_its_node() {
        let root = tempfile::tempdir().unwrap();
        let users = name_capture_users(root.path(), &[capture(7, None, None)]);

        assert_eq!(users, vec![PrivacyUser { app_name: "node 7".to_string() }]);
    }

    /// One row per app, not per stream; a browser opens one node per tab.
    #[test]
    fn one_app_holding_several_streams_is_listed_once() {
        let root = tempfile::tempdir().unwrap();
        let users = name_capture_users(
            root.path(),
            &[capture(7, Some(1234), Some("Firefox")), capture(8, Some(1234), Some("Firefox"))],
        );

        assert_eq!(users, vec![PrivacyUser { app_name: "Firefox".to_string() }]);
    }

    #[tokio::test]
    async fn a_burst_of_device_opens_is_one_event() {
        // Five devices, since inotify merges identical queued events on one watch. Held open: closes
        // of dropped files landed after the read in loaded full-suite runs and split the burst.
        let dev = tempfile::tempdir().unwrap();
        let mut devices: Vec<_> = (0..5).map(|n| dev.path().join(format!("video{n}"))).collect();
        devices.iter().for_each(|path| drop(std::fs::File::create(path).unwrap()));
        let mut stream = watch_video_devices(dev.path(), &devices);
        let _opened: Vec<_> = devices.iter().map(|path| std::fs::File::open(path).unwrap()).collect();

        assert!(matches!(next_device_event(&mut stream, &mut devices).await, DeviceEvent::Opened));
        let second =
            tokio::time::timeout(std::time::Duration::from_millis(100), next_device_event(&mut stream, &mut devices))
                .await;
        assert!(second.is_err(), "five queued events must cost one scan, not five");
    }

    #[tokio::test]
    async fn a_close_alone_is_not_an_open_so_it_skips_the_proc_walk() {
        let dev = tempfile::tempdir().unwrap();
        let node = dev.path().join("video0");
        drop(std::fs::File::create(&node).unwrap());
        let mut devices = vec![node.clone()];
        let mut stream = watch_video_devices(dev.path(), &devices);
        let file = std::fs::File::open(&node).unwrap();
        assert!(matches!(next_device_event(&mut stream, &mut devices).await, DeviceEvent::Opened));
        drop(file);
        assert!(matches!(next_device_event(&mut stream, &mut devices).await, DeviceEvent::Closed));
    }

    /// Pids 1111 and 3333 opened the camera; pid 2222 got 1111's fd by fork, so no open event named it.
    #[tokio::test]
    async fn a_close_by_the_opener_keeps_a_holder_that_inherited_the_fd() {
        let (dev, proc_root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let node = dev.path().join("video0");
        std::fs::write(&node, "").unwrap();
        write_fd_symlink(proc_root.path(), 1111, 5, &node);
        write_fd_symlink(proc_root.path(), 3333, 5, &node);
        let held = std::fs::File::open(&node).unwrap();
        let (_privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = PrivacyController::new(proc_root.path().to_path_buf(), dev.path(), sources, events_tx);
        assert_eq!(within(events_rx.recv()).await, Some(()), "the seed lists both openers");
        let user = |pid: u32| PrivacyUser { app_name: format!("pid {pid}") };
        assert_eq!(controller.snapshot().camera_users, vec![user(1111), user(3333)]);

        write_fd_symlink(proc_root.path(), 2222, 5, &node);
        std::fs::remove_dir_all(proc_root.path().join("1111")).unwrap();
        drop(held);
        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(controller.snapshot().camera_users, vec![user(2222), user(3333)]);
    }

    /// A webcam plugged in after start, or an unplugged and replugged node (a new inode), must
    /// still be watched, or `camera_users` stays empty while it is live.
    #[tokio::test]
    async fn a_camera_plugged_in_after_start_is_watched() {
        let dev = tempfile::tempdir().unwrap();
        let proc_root = tempfile::tempdir().unwrap();
        let (_privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        tokio::spawn(run_privacy_task(
            proc_root.path().to_path_buf(),
            dev.path().to_path_buf(),
            Arc::clone(&state),
            sources,
            events_tx,
        ));
        assert_eq!(within(events_rx.recv()).await, Some(()), "the empty seed");

        let node = dev.path().join("video0");
        // Opens the node until `camera_users.is_empty()` equals `want_empty`; a node is unwatched
        // until its create event is handled, so one open may be missed.
        let settle = |want_empty: bool| {
            let (state, node) = (Arc::clone(&state), node.clone());
            within(async move {
                while state.lock().unwrap().camera_users.is_empty() != want_empty {
                    drop(std::fs::File::open(&node).unwrap());
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
        };
        for _ in 0..2 {
            std::fs::write(&node, "").unwrap();
            let fd_dir = proc_root.path().join("1234/fd");
            std::fs::create_dir_all(&fd_dir).unwrap();
            let _ = std::fs::remove_file(fd_dir.join("5"));
            std::os::unix::fs::symlink(&node, fd_dir.join("5")).unwrap();
            settle(false).await;
            std::fs::remove_dir_all(proc_root.path().join("1234")).unwrap();
            settle(true).await;
            std::fs::remove_file(&node).unwrap();
        }
    }

    /// Nodes that are not cameras churn in `/dev`; none may pay for a `/proc` scan.
    #[tokio::test]
    async fn a_non_camera_node_in_dev_is_not_an_event() {
        let dev = tempfile::tempdir().unwrap();
        let mut devices = Vec::new();
        let mut watch = watch_video_devices(dev.path(), &devices);
        std::fs::write(dev.path().join("loop0"), "").unwrap();
        std::fs::remove_file(dev.path().join("loop0")).unwrap();
        let quiet =
            tokio::time::timeout(std::time::Duration::from_millis(200), next_device_event(&mut watch, &mut devices))
                .await;
        assert!(quiet.is_err());
        std::fs::write(dev.path().join("video0"), "").unwrap();
        assert!(matches!(within(next_device_event(&mut watch, &mut devices)).await, DeviceEvent::Opened));
    }

    #[tokio::test]
    async fn no_video_devices_still_sends_one_signal_so_the_empty_state_gets_announced() {
        let dev_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let proc_root = tempfile::tempdir().unwrap();
        let (_privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = PrivacyController::new(proc_root.path().to_path_buf(), dev_root.path(), sources, events_tx);

        assert_eq!(
            within(events_rx.recv()).await,
            Some(()),
            "must still announce an empty state when no camera hardware exists"
        );
        assert!(controller.snapshot().camera_users.is_empty());
    }

    #[tokio::test]
    async fn the_mixer_exiting_leaves_the_camera_watched() {
        let dev = tempfile::tempdir().unwrap();
        let device = dev.path().join("video0");
        std::fs::write(&device, "").unwrap();
        let proc_root = tempfile::tempdir().unwrap();
        let (privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        let dev_root = dev.path().to_path_buf();
        tokio::spawn(run_privacy_task(
            proc_root.path().to_path_buf(),
            dev_root,
            Arc::clone(&state),
            sources,
            events_tx,
        ));
        assert_eq!(events_rx.recv().await, Some(()), "the empty seed");

        drop(privacy_tx);
        tokio::task::yield_now().await;
        let fd_dir = proc_root.path().join("1234/fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink(&device, fd_dir.join("5")).unwrap();
        std::fs::File::open(&device).unwrap();

        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(state.lock().unwrap().camera_users, vec![PrivacyUser { app_name: "pid 1234".to_string() }]);
    }

    /// ADR-0137 regression: no webcam used to end the task, taking microphone and screencast down.
    #[tokio::test]
    async fn a_machine_with_no_camera_still_reports_a_microphone() {
        let dev_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let proc_root = tempfile::tempdir().unwrap();
        let (privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = PrivacyController::new(proc_root.path().to_path_buf(), dev_root.path(), sources, events_tx);
        assert_eq!(events_rx.recv().await, Some(()), "the empty seed");

        privacy_tx
            .send(PrivacySources {
                microphones: vec![capture(7, Some(1234), Some("Firefox"))],
                ..PrivacySources::default()
            })
            .unwrap();

        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(
            controller.snapshot().microphone_users,
            vec![PrivacyUser { app_name: "Firefox".to_string() }],
            "a PipeWire capture must be reported on a machine that has no camera to watch"
        );
    }
}

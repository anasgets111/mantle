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

use crate::capabilities::audio::mixer::{CaptureApp, PrivacySources};

use super::video::{enumerate_video_devices, find_device_openers, holds_device, is_video_name, read_comm};

/// `/proc/{pid}/comm`, else `pid {n}`.
fn process_name(proc_root: &Path, pid: u32) -> String {
    read_comm(proc_root, pid).unwrap_or_else(|| format!("pid {pid}"))
}

/// Adds `app_name` unless a row already has it: browsers open one node per tab, but users need one
/// Firefox row.
fn push_unique(users: &mut Vec<PrivacyUser>, app_name: String) {
    if !users.iter().any(|user| user.app_name == app_name) {
        users.push(PrivacyUser { app_name });
    }
}

/// Names PipeWire captures by client `application.name`, then `/proc/{pid}/comm`, then `pid {n}`,
/// finally node id. Portal-created streams may have no pid, but remain captures. Deduplicated by
/// name, not node.
fn name_capture_users(proc_root: &Path, apps: &[CaptureApp]) -> Vec<PrivacyUser> {
    let mut users = Vec::new();
    for app in apps {
        let app_name = app
            .app_name
            .clone()
            .or_else(|| app.pid.map(|pid| process_name(proc_root, pid)))
            .unwrap_or_else(|| format!("node {}", app.node_id));
        push_unique(&mut users, app_name);
    }
    users
}

/// PipeWire camera consumers by stream, then raw V4L2 openers by `/proc` name. The daemon holds the
/// camera for a consumer already listed (or none running), so it is dropped (ADR-0034).
fn name_camera_users(proc_root: &Path, opener_pids: &[u32], pipewire: &PrivacySources) -> Vec<PrivacyUser> {
    let mut users = name_capture_users(proc_root, &pipewire.cameras);
    for &pid in opener_pids.iter().filter(|&&pid| Some(pid) != pipewire.pipewire_pid) {
        push_unique(&mut users, process_name(proc_root, pid));
    }
    users
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
    let devices = enumerate_video_devices(&dev_root);
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
            event = next_device_event(&mut inotify_stream) => {
                // An open or an emptied set pays for the scan, a readlink of every fd in `/proc`, so
                // it runs off the two async workers.
                let rescan = match event {
                    DeviceEvent::Opened => true,
                    // A close only removes pids: recheck the known openers instead of walking `/proc`.
                    // A pid that dropped out may have passed the fd to a child no open announced: walk.
                    DeviceEvent::Closed if opener_pids.is_empty() => false,
                    DeviceEvent::Closed => recheck_openers(&proc_root, watched_devices(&inotify_stream), &mut opener_pids).await,
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
                    let (root, watched) = (proc_root.clone(), watched_devices(&inotify_stream));
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

/// Drops openers that no longer hold a device; whether any left. A panicked task keeps the known
/// set and reports true, so the caller walks `/proc`.
async fn recheck_openers(proc_root: &Path, watched: Vec<PathBuf>, opener_pids: &mut Vec<u32>) -> bool {
    // A copy goes in so a panicked task leaves the known set, not an empty one.
    let (root, mut pids) = (proc_root.to_path_buf(), opener_pids.clone());
    let recheck = move || {
        pids.retain(|&pid| holds_device(&root, pid, &watched));
        pids
    };
    match tokio::task::spawn_blocking(recheck).await {
        Ok(kept) => {
            let dropped = kept.len() < opener_pids.len();
            *opener_pids = kept;
            dropped
        }
        Err(_) => true,
    }
}

/// Rebuilds all three lists every time. The PipeWire lists are cheap walks, and one write prevents
/// a config observing one list a push behind.
fn rebuild(proc_root: &Path, opener_pids: &[u32], pipewire: &PrivacySources) -> PrivacyState {
    PrivacyState {
        camera_users: name_camera_users(proc_root, opener_pids, pipewire),
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
        Ok(stream) => Some(VideoWatch {
            events: stream.ready_chunks(64),
            watches,
            root: dev_root.to_path_buf(),
            dir,
            devices: devices.to_vec(),
        }),
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
    /// The `videoN` nodes now present, kept current by create/delete events.
    devices: Vec<PathBuf>,
}

fn watched_devices(watch: &Option<VideoWatch>) -> Vec<PathBuf> {
    watch.as_ref().map(|watch| watch.devices.clone()).unwrap_or_default()
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
async fn next_device_event(watch: &mut Option<VideoWatch>) -> DeviceEvent {
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
                    watch.devices.retain(|device| *device != path);
                    if event.mask.contains(EventMask::CREATE) {
                        watch_device(&mut watch.watches, &path);
                        watch.devices.push(path);
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
            Some(events) => {
                // An empty batch has no error to report; keep waiting.
                if let Some(err) = events.into_iter().find_map(Result::err) {
                    return DeviceEvent::Failed(err);
                }
            }
            None => return DeviceEvent::Ended,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::privacy::video::write_fd_symlink;
    use crate::capabilities::test_support::within;

    #[test]
    fn raw_openers_are_named_by_comm_then_pid() {
        // (comm file, devices the pid has open, expected name)
        let both = [PathBuf::from("/dev/video0"), PathBuf::from("/dev/video1")];
        for (comm, opened, expected) in [
            (Some("mpv\n"), &["/dev/video0"][..], "mpv"),
            (None, &["/dev/video0"][..], "pid 1234"),
            (Some("mpv\n"), &["/dev/video0", "/dev/video1"][..], "mpv"),
        ] {
            let root = tempfile::tempdir().unwrap();
            for (fd, device) in opened.iter().enumerate() {
                write_fd_symlink(root.path(), 1234, fd as u32 + 5, device);
            }
            if let Some(comm) = comm {
                std::fs::write(root.path().join("1234").join("comm"), comm).unwrap();
            }
            let users = name_camera_users(root.path(), &find_device_openers(root.path(), &both), &Default::default());
            assert_eq!(users, vec![PrivacyUser { app_name: expected.to_string() }], "{expected}");
        }
    }

    /// PipeWire's daemon holds the camera for a consumer (pid 7842); the consumer is the answer.
    #[test]
    fn pipewire_openers_give_way_to_the_streams_that_use_them() {
        let root = tempfile::tempdir().unwrap();
        for (pid, comm) in [(7842, "pipewire"), (1234, "ffmpeg"), (55, "Firefox")] {
            std::fs::create_dir_all(root.path().join(pid.to_string())).unwrap();
            std::fs::write(root.path().join(pid.to_string()).join("comm"), comm).unwrap();
        }
        let sources = PrivacySources {
            cameras: vec![capture(94, None, Some("Firefox")), capture(95, Some(77), None)],
            pipewire_pid: Some(7842),
            ..PrivacySources::default()
        };
        let names = |users: Vec<PrivacyUser>| users.into_iter().map(|user| user.app_name).collect::<Vec<_>>();
        assert_eq!(
            names(name_camera_users(root.path(), &[55, 1234, 7842], &sources)),
            ["Firefox", "pid 77", "ffmpeg"],
            "the daemon is dropped, a raw opener kept, and a name seen twice listed once"
        );
    }

    fn capture(node_id: u32, pid: Option<u32>, app_name: Option<&str>) -> CaptureApp {
        CaptureApp { node_id, pid, app_name: app_name.map(str::to_string), running: true }
    }

    #[test]
    fn a_capture_stream_is_named_by_its_published_name_then_its_process_then_its_node() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("1234")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "pw-cat\n").unwrap();
        // Portal-created streams may lack `application.process.id`; node id is the last resort, not a
        // dropped capture. A browser opens one node per tab, so one app is one row.
        for (case, streams, expected) in [
            ("published name", vec![capture(7, Some(1234), Some("Firefox"))], "Firefox"),
            ("process comm", vec![capture(7, Some(1234), None)], "pw-cat"),
            ("node", vec![capture(7, None, None)], "node 7"),
            (
                "several streams of one app",
                vec![capture(7, Some(1234), Some("Firefox")), capture(8, Some(1234), Some("Firefox"))],
                "Firefox",
            ),
        ] {
            let users = name_capture_users(root.path(), &streams);
            assert_eq!(users, vec![PrivacyUser { app_name: expected.to_string() }], "{case}");
        }
    }

    #[tokio::test]
    async fn a_burst_of_device_opens_is_one_event() {
        // Five devices, since inotify merges identical queued events on one watch. Held open: closes
        // of dropped files landed after the read in loaded full-suite runs and split the burst.
        let dev = tempfile::tempdir().unwrap();
        let devices: Vec<_> = (0..5).map(|n| dev.path().join(format!("video{n}"))).collect();
        devices.iter().for_each(|path| drop(std::fs::File::create(path).unwrap()));
        let mut stream = watch_video_devices(dev.path(), &devices);
        let _opened: Vec<_> = devices.iter().map(|path| std::fs::File::open(path).unwrap()).collect();

        assert!(matches!(next_device_event(&mut stream).await, DeviceEvent::Opened));
        // Only a second Opened splits the burst: a child forked by a parallel test holds copies of
        // these fds, and its exec closes them later as a stray Closed.
        let another_open = async { while !matches!(next_device_event(&mut stream).await, DeviceEvent::Opened) {} };
        let second = tokio::time::timeout(std::time::Duration::from_millis(100), another_open).await;
        assert!(second.is_err(), "five queued opens must cost one scan, not five");
    }

    #[tokio::test]
    async fn a_close_alone_is_not_an_open_so_it_skips_the_proc_walk() {
        let dev = tempfile::tempdir().unwrap();
        let node = dev.path().join("video0");
        drop(std::fs::File::create(&node).unwrap());
        let devices = vec![node.clone()];
        let mut stream = watch_video_devices(dev.path(), &devices);
        let file = std::fs::File::open(&node).unwrap();
        assert!(matches!(next_device_event(&mut stream).await, DeviceEvent::Opened));
        drop(file);
        assert!(matches!(next_device_event(&mut stream).await, DeviceEvent::Closed));

        let written = std::fs::OpenOptions::new().write(true).open(&node).unwrap();
        assert!(matches!(within(next_device_event(&mut stream)).await, DeviceEvent::Opened));
        drop(written);
        assert!(
            matches!(within(next_device_event(&mut stream)).await, DeviceEvent::Closed),
            "a write close counts too"
        );
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
        let controller = PrivacyController::new(proc_root.path().to_path_buf(), dev.path(), sources, events_tx);
        assert_eq!(within(events_rx.recv()).await, Some(()), "the empty seed");

        let node = dev.path().join("video0");
        // Opens the node until `camera_users.is_empty()` equals `want_empty`; a node is unwatched
        // until its create event is handled, so one open may be missed.
        let settle = |want_empty: bool| {
            let (controller, node) = (&controller, node.clone());
            within(async move {
                while controller.snapshot().camera_users.is_empty() != want_empty {
                    drop(std::fs::File::open(&node).unwrap());
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
        };
        for _ in 0..2 {
            std::fs::write(&node, "").unwrap();
            let _ = std::fs::remove_dir_all(proc_root.path().join("1234"));
            write_fd_symlink(proc_root.path(), 1234, 5, &node);
            settle(false).await;
            std::fs::remove_dir_all(proc_root.path().join("1234")).unwrap();
            settle(true).await;
            std::fs::remove_file(&node).unwrap();
        }
    }

    #[tokio::test]
    async fn a_second_camera_joins_the_watch_list_and_a_removed_one_leaves_it() {
        let dev = tempfile::tempdir().unwrap();
        let (first, second) = (dev.path().join("video0"), dev.path().join("video1"));
        std::fs::write(&first, "").unwrap();
        let mut watch = watch_video_devices(dev.path(), std::slice::from_ref(&first));

        std::fs::write(&second, "").unwrap();
        assert!(matches!(within(next_device_event(&mut watch)).await, DeviceEvent::Opened));
        assert_eq!(watched_devices(&watch), vec![first.clone(), second.clone()]);

        std::fs::remove_file(&first).unwrap();
        assert!(matches!(within(next_device_event(&mut watch)).await, DeviceEvent::Opened));
        assert_eq!(watched_devices(&watch), vec![second]);
    }

    /// Nodes that are not cameras churn in `/dev`; none may pay for a `/proc` scan.
    #[tokio::test]
    async fn a_non_camera_node_in_dev_is_not_an_event() {
        let dev = tempfile::tempdir().unwrap();
        let mut watch = watch_video_devices(dev.path(), &[]);
        std::fs::write(dev.path().join("loop0"), "").unwrap();
        std::fs::remove_file(dev.path().join("loop0")).unwrap();
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(200), next_device_event(&mut watch)).await;
        assert!(quiet.is_err());
        std::fs::write(dev.path().join("video0"), "").unwrap();
        assert!(matches!(within(next_device_event(&mut watch)).await, DeviceEvent::Opened));
    }

    #[tokio::test]
    async fn the_mixer_exiting_leaves_the_camera_watched() {
        let dev = tempfile::tempdir().unwrap();
        let device = dev.path().join("video0");
        std::fs::write(&device, "").unwrap();
        let proc_root = tempfile::tempdir().unwrap();
        let (privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = PrivacyController::new(proc_root.path().to_path_buf(), dev.path(), sources, events_tx);
        assert_eq!(within(events_rx.recv()).await, Some(()), "the empty seed");

        drop(privacy_tx);
        tokio::task::yield_now().await;
        write_fd_symlink(proc_root.path(), 1234, 5, &device);
        std::fs::File::open(&device).unwrap();

        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(controller.snapshot().camera_users, vec![PrivacyUser { app_name: "pid 1234".to_string() }]);
    }

    /// ADR-0137 regression: no webcam used to end the task, taking microphone and screencast down.
    #[tokio::test]
    async fn a_machine_with_no_camera_still_reports_a_microphone() {
        let dev_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let proc_root = tempfile::tempdir().unwrap();
        let (privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = PrivacyController::new(proc_root.path().to_path_buf(), dev_root.path(), sources, events_tx);
        assert_eq!(
            within(events_rx.recv()).await,
            Some(()),
            "the empty state is announced even with no camera hardware"
        );
        assert!(controller.snapshot().camera_users.is_empty());

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

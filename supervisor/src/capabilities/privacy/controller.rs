//! [`PrivacyController`] owns read-only `mantle.privacy` telemetry (ADR-0034).

pub use shared::state::privacy::{PrivacyState, PrivacyUser};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use shared::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::publish;
use tokio::sync::watch;

use crate::capabilities::audio::mixer::{CaptureApp, PrivacySources, VideoSourceApp};

use super::video::{find_device_openers, read_comm};

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
    /// `proc_root`/`video4linux_root` (defaults `/proc`/`/sys/class/video4linux`) are injected per
    /// the sysfs/procfs test convention. `sources` is the PipeWire connection shared with
    /// `mantle.audio` (ADR-0034). Returns immediately.
    pub fn new(
        proc_root: PathBuf,
        video4linux_root: &Path,
        sources: watch::Receiver<PrivacySources>,
        events: UnboundedSender<()>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        let devices = super::video::enumerate_video_devices(video4linux_root);
        tokio::spawn(run_privacy_task(proc_root, devices, Arc::clone(&state), sources, events));
        Self { state }
    }

    pub fn snapshot(&self) -> PrivacyState {
        self.state.lock().expect("mutex poisoned").clone()
    }
}

/// Watches resolved video devices for live-reliable `OPEN`/`CLOSE` (see `privacy::video`) and
/// drains `sources`. Either triggers a full rebuild of all three lists; only a device event pays for
/// the `/proc` scan, once per burst of ready events.
///
/// The camera watch is optional, not the loop. Without a webcam, or after `Inotify`/stream failure,
/// `camera_users` stays empty while microphone and screencast continue. Before ADR-0137 those
/// failures ended the task, which was correct when camera was the only answer.
async fn run_privacy_task(
    proc_root: PathBuf,
    devices: Vec<PathBuf>,
    state: Arc<Mutex<PrivacyState>>,
    mut sources: watch::Receiver<PrivacySources>,
    events: UnboundedSender<()>,
) {
    let mut inotify_stream = watch_video_devices(&devices);
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
                match event {
                    // Only a device open/close can change who holds it; this arm pays for the scan,
                    // a readlink of every fd in `/proc`, so it runs off the two async workers.
                    DeviceEvent::Opened => {
                        let (root, watched) = (proc_root.clone(), devices.clone());
                        if let Ok(pids) = tokio::task::spawn_blocking(move || find_device_openers(&root, &watched)).await {
                            opener_pids = pids;
                        }
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

/// Inotify stream for `/dev/videoN`, or `None` when no device exists or setup failed. Failure costs
/// only `camera_users` and is logged.
fn watch_video_devices(devices: &[PathBuf]) -> Option<DeviceEvents> {
    if devices.is_empty() {
        debug!("no /dev/videoN devices found; camera_users will stay empty");
        return None;
    }
    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(err) => {
            warn!("failed to initialize inotify; camera detection disabled for this run: {err}");
            return None;
        }
    };
    for device in devices {
        if let Err(err) = inotify.watches().add(device, WatchMask::OPEN | WatchMask::CLOSE) {
            warn!("failed to watch {}; camera opens on it won't be detected: {err}", device.display());
        }
    }
    match inotify.into_event_stream(vec![0u8; 4096]) {
        // An app probing every node opens and closes each; one scan answers the whole burst.
        Ok(stream) => Some(stream.ready_chunks(64)),
        Err(err) => {
            warn!("failed to start the inotify event stream; camera detection disabled for this run: {err}");
            None
        }
    }
}

type DeviceEvents = futures_util::stream::ReadyChunks<inotify::EventStream<Vec<u8>>>;

/// Camera watch event. The enum names cases and lets [`next_device_event`] flatten a missing watch.
enum DeviceEvent {
    Opened,
    Failed(std::io::Error),
    Ended,
}

/// Next device open/close, or a never-completing future without a camera. `select!` still needs an
/// arm future in that case.
async fn next_device_event(stream: &mut Option<DeviceEvents>) -> DeviceEvent {
    let Some(stream) = stream.as_mut() else { return std::future::pending().await };
    match stream.next().await {
        Some(events) if events.iter().any(Result::is_ok) => DeviceEvent::Opened,
        Some(mut events) => DeviceEvent::Failed(events.swap_remove(0).unwrap_err()),
        None => DeviceEvent::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let devices: Vec<_> = (0..5).map(|_| tempfile::NamedTempFile::new().unwrap()).collect();
        let paths: Vec<_> = devices.iter().map(|device| device.path().to_path_buf()).collect();
        let mut stream = watch_video_devices(&paths);
        let _opened: Vec<_> = paths.iter().map(|path| std::fs::File::open(path).unwrap()).collect();

        assert!(matches!(next_device_event(&mut stream).await, DeviceEvent::Opened));
        let second = tokio::time::timeout(std::time::Duration::from_millis(100), next_device_event(&mut stream)).await;
        assert!(second.is_err(), "five queued events must cost one scan, not five");
    }

    #[tokio::test]
    async fn no_video_devices_still_sends_one_signal_so_the_empty_state_gets_announced() {
        let video4linux_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let proc_root = tempfile::tempdir().unwrap();
        let (_privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller =
            PrivacyController::new(proc_root.path().to_path_buf(), video4linux_root.path(), sources, events_tx);

        assert_eq!(
            within(events_rx.recv()).await,
            Some(()),
            "must still announce an empty state when no camera hardware exists"
        );
        assert!(controller.snapshot().camera_users.is_empty());
    }

    #[tokio::test]
    async fn the_mixer_exiting_leaves_the_camera_watched() {
        let device = tempfile::NamedTempFile::new().unwrap();
        let proc_root = tempfile::tempdir().unwrap();
        let (privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        let devices = vec![device.path().to_path_buf()];
        tokio::spawn(run_privacy_task(proc_root.path().to_path_buf(), devices, Arc::clone(&state), sources, events_tx));
        assert_eq!(events_rx.recv().await, Some(()), "the empty seed");

        drop(privacy_tx);
        tokio::task::yield_now().await;
        let fd_dir = proc_root.path().join("1234/fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink(device.path(), fd_dir.join("5")).unwrap();
        std::fs::File::open(device.path()).unwrap();

        assert_eq!(within(events_rx.recv()).await, Some(()));
        assert_eq!(state.lock().unwrap().camera_users, vec![PrivacyUser { app_name: "pid 1234".to_string() }]);
    }

    /// ADR-0137 regression: no webcam used to end the task, taking microphone and screencast down.
    #[tokio::test]
    async fn a_machine_with_no_camera_still_reports_a_microphone() {
        let video4linux_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let proc_root = tempfile::tempdir().unwrap();
        let (privacy_tx, sources) = watch::channel(PrivacySources::default());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller =
            PrivacyController::new(proc_root.path().to_path_buf(), video4linux_root.path(), sources, events_tx);
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

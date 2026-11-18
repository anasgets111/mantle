//! A [`Compositor`] fake for capability tests.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Mutex;

use super::Compositor;
use crate::capabilities::keyboard::controller::LayoutSink;
use crate::capabilities::windows::controller::StatePublisher as WindowsPublisher;
use crate::capabilities::workspaces::controller::StatePublisher;

/// Records each write instead of sending it. The real backends read their socket path from the
/// session's env, so a real one would reach the live compositor of the session running tests.
#[derive(Default)]
pub(crate) struct Recorder(pub(crate) Mutex<Vec<String>>);

impl Compositor for Recorder {
    fn spawn_reader(&self, _: StatePublisher, _: WindowsPublisher, _: LayoutSink) {}
    fn focus_workspace(&self, id: &str) {
        self.0.lock().unwrap().push(format!("focus_workspace {id}"));
    }
    fn toggle_special(&self, name: &str) {
        self.0.lock().unwrap().push(format!("toggle_special {name}"));
    }
    fn focus_window(&self, id: &str) {
        self.0.lock().unwrap().push(format!("focus_window {id}"));
    }
    fn close_window(&self, id: &str) {
        self.0.lock().unwrap().push(format!("close_window {id}"));
    }
    fn set_fullscreen(&self, id: &str, fullscreen: bool, current: Option<bool>) {
        self.0.lock().unwrap().push(format!("set_fullscreen {id} {fullscreen} {current:?}"));
    }
    fn set_maximized(&self, id: &str, maximized: bool, current: Option<bool>) {
        self.0.lock().unwrap().push(format!("set_maximized {id} {maximized} {current:?}"));
    }
    fn move_window(&self, id: &str, workspace_id: &str) {
        self.0.lock().unwrap().push(format!("move_window {id} {workspace_id}"));
    }
    fn switch_layout(&self, index: usize) {
        self.0.lock().unwrap().push(format!("switch_layout {index}"));
    }
}

/// Set on the test process [`run_writes`] spawns. A write test's child runs only with it, so
/// `cargo test -- --ignored` alone never writes to the live compositor of the session running it.
const CHILD_ENV: &str = "MANTLE_TEST_COMPOSITOR_CHILD";

/// Runs the `#[ignore]`d test `child` in a fresh copy of this test binary with `env`, so a real
/// backend reads its socket from its own process env, as in a session, with no `set_var` here.
/// Each connection to `socket` is read by `read` and answered by `answer`. Returns the requests in
/// arrival order and the child's output, where the backend logs each reply's verdict: every line
/// of `logged` must be in it.
pub(crate) fn run_writes(
    child: &str,
    socket: &Path,
    env: &[(&str, &OsStr)],
    read: fn(&mut UnixStream) -> Vec<u8>,
    answer: impl Fn(&[u8]) -> Vec<u8> + Send + 'static,
    logged: &[&str],
) -> (Vec<String>, String) {
    let listener = UnixListener::bind(socket).unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let request = read(&mut stream);
            // The empty connection made once the child has exited: no write is empty.
            if request.is_empty() {
                return requests;
            }
            stream.write_all(&answer(&request)).unwrap();
            requests.push(String::from_utf8_lossy(&request).into_owned());
        }
        requests
    });
    // Bounded by the child: every write connects with 1 s timeouts, and `writes_child` waits 60 s.
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args([child, "--exact", "--ignored", "--nocapture", "--test-threads=1"]).env(CHILD_ENV, "1");
    // The session's own compositor vars are dropped, so a test that forgets one fails instead of
    // writing to the live compositor.
    for var in [
        "MANTLE_LOG",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "SWAYSOCK",
        "NIRI_SOCKET",
        "MANGO_INSTANCE_SIGNATURE",
        "XDG_CURRENT_DESKTOP",
    ] {
        command.env_remove(var);
    }
    let output = command.envs(env.iter().copied()).output().unwrap();
    drop(UnixStream::connect(socket).unwrap());
    let requests = server.join().unwrap();
    let log = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{child} failed: {log}");
    // A filter that matches nothing also exits zero.
    assert!(log.contains("1 passed"), "{child} did not run: {log}");
    for line in logged {
        assert!(log.contains(line), "{line:?} missing from {log}");
    }
    (requests, log)
}

/// The body of a [`run_writes`] child: logs at the loudest level, runs `writes`, then waits until
/// the shared action thread has run every write they queued.
pub(crate) fn writes_child(writes: impl FnOnce()) {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    shared::log::init("test", 3);
    writes();
    let (done, finished) = std::sync::mpsc::channel();
    super::run_in_order(move || done.send(()).unwrap());
    finished.recv_timeout(std::time::Duration::from_secs(60)).expect("every queued write ran");
}

/// One request read up to its newline (niri, mango); empty at end of stream.
pub(crate) fn read_line(stream: &mut UnixStream) -> Vec<u8> {
    let mut line = Vec::new();
    BufReader::new(stream).read_until(b'\n', &mut line).unwrap();
    line
}

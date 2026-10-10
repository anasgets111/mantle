//! Which compositor this session is running, the probe that answers it, and the one trait every
//! compositor implements.
//!
//! Compositor identity belongs to the session, not a capability. Each compositor module owns its
//! IPC, its event reader (feeding `workspaces`, `windows` and `keyboard` from one stream), its
//! layout parsing and its writes; [`CompositorKind::backend`] is the one dispatch point. The
//! trait is stateless: a write opens a fresh connection, and anything a toggle needs (a window's
//! current fullscreen state) arrives as an argument. ADR-0355 supersedes ADR-0056 decision 1.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shared::{debug, warn};

use crate::capabilities::keyboard::controller::LayoutSink;
use crate::capabilities::windows::controller::StatePublisher as WindowsPublisher;
use crate::capabilities::workspaces::controller::StatePublisher;
use crate::capabilities::{RETRY_MAX, STABLE};

mod hyprland;
mod mango;
mod niri;
mod sway;

/// A compositor implemented here, narrower than "a compositor that exists". Other sessions yield
/// [`detect_compositor`]'s `None`; dependent capabilities degrade rather than guess (ADR-0056).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorKind {
    Hyprland,
    Niri,
    Sway,
    Mango,
}

impl CompositorKind {
    /// Lowercase payload name (`mantle.workspaces.compositor`, ADR-0119), so config can choose
    /// display policy without detecting the compositor again.
    pub fn name(self) -> &'static str {
        match self {
            CompositorKind::Hyprland => "hyprland",
            CompositorKind::Niri => "niri",
            CompositorKind::Sway => "sway",
            CompositorKind::Mango => "mango",
        }
    }

    pub fn backend(self) -> &'static dyn Compositor {
        match self {
            CompositorKind::Hyprland => &hyprland::Hyprland,
            CompositorKind::Niri => &niri::Niri,
            CompositorKind::Sway => &sway::Sway,
            CompositorKind::Mango => &mango::Mango,
        }
    }
}

/// What a session's capabilities ask of its compositor. The defaults log a feature the compositor
/// lacks; a write that cannot apply is dropped, never an error.
pub trait Compositor: Sync {
    /// Starts the one reader thread that feeds `workspaces`, `windows` and `keyboard`'s layout.
    fn spawn_reader(&self, workspaces: StatePublisher, windows: WindowsPublisher, keyboard: LayoutSink);
    fn focus_workspace(&self, id: &str);
    fn toggle_special(&self, name: &str) {
        unsupported(format_args!("toggle_special({name:?})"))
    }
    fn focus_window(&self, id: &str);
    fn close_window(&self, id: &str);
    /// `current` is the last published state; a compositor that only toggles writes on a change.
    fn set_fullscreen(&self, id: &str, fullscreen: bool, _current: Option<bool>) {
        unsupported(format_args!("set_fullscreen({id:?}, {fullscreen})"))
    }
    fn set_maximized(&self, id: &str, maximized: bool, _current: Option<bool>) {
        unsupported(format_args!("set_maximized({id:?}, {maximized})"))
    }
    fn move_window(&self, id: &str, workspace_id: &str) {
        unsupported(format_args!("move_window({id:?}, {workspace_id:?})"))
    }
    fn switch_layout(&self, index: usize) {
        unsupported(format_args!("switch_layout({index})"))
    }
}

fn unsupported(call: std::fmt::Arguments) {
    debug!("{call} is not supported by this session's compositor; ignored")
}

/// Stands in when [`detect_compositor`] finds none: no reader, every write logged and dropped.
struct Unsupported;

impl Compositor for Unsupported {
    fn spawn_reader(&self, _: StatePublisher, _: WindowsPublisher, _: LayoutSink) {}
    fn focus_workspace(&self, id: &str) {
        unsupported(format_args!("focus_workspace({id:?})"))
    }
    fn focus_window(&self, id: &str) {
        unsupported(format_args!("focus_window({id:?})"))
    }
    fn close_window(&self, id: &str) {
        unsupported(format_args!("close_window({id:?})"))
    }
}

/// Whether a toggle-only compositor must act: `current` is the last published state, unknown
/// reads as off.
fn toggle_needed(current: Option<bool>, want: bool) -> bool {
    current.unwrap_or(false) != want
}

/// Env vars each compositor sets for every process in its session, in probe order.
///
/// A table, so precedence is explicit. Order breaks ties if two vars are set; that case is unlikely
/// and harmless because real sessions run one compositor.
///
/// Entries are vars set *because the compositor is running*. `$XDG_CURRENT_DESKTOP` is only a name
/// written by the launcher and remains set if the compositor never starts. It is useful to report
/// via [`backend_or_unsupported`], not to dispatch on.
const PROBES: &[(CompositorKind, &str)] = &[
    (CompositorKind::Hyprland, "HYPRLAND_INSTANCE_SIGNATURE"),
    (CompositorKind::Niri, "NIRI_SOCKET"),
    (CompositorKind::Sway, "SWAYSOCK"),
    (CompositorKind::Mango, "MANGO_INSTANCE_SIGNATURE"),
];

/// The first [`PROBES`] entry whose var this session has set, or `None` for a compositor with no
/// implementor here.
pub fn detect_compositor() -> Option<CompositorKind> {
    PROBES.iter().find(|(_, var)| std::env::var_os(var).is_some()).map(|(kind, _)| *kind)
}

/// `compositor`'s backend, or [`Unsupported`] after logging that `what` reporting is off.
///
/// The log names `$XDG_CURRENT_DESKTOP` if set, because "this session is river, which has no
/// implementor" is actionable. This is not a second detection path; an unrecognised name still
/// yields no implementor.
pub fn backend_or_unsupported(compositor: Option<CompositorKind>, what: &str) -> &'static dyn Compositor {
    if let Some(kind) = compositor {
        return kind.backend();
    }
    match session_desktop() {
        Some(desktop) => debug!("this session is {desktop}, which has no implementor; {what} reporting disabled"),
        None => debug!("no supported compositor was detected; {what} reporting disabled"),
    }
    &Unsupported
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// `var`'s socket path, or `None` when unset or empty.
fn socket_var(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|path| !path.is_empty()).map(PathBuf::from)
}

/// A connection whose reads and writes give up after [`REQUEST_TIMEOUT`], so a stalled compositor
/// cannot wedge a reader or [`run_in_order`]'s thread.
fn connect(path: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    Ok(stream)
}

/// Runs `job` after every earlier one on a single shared thread, so rapid write actions reach the
/// compositor in the order they were sent. Each job blocks for at most one IPC round trip.
fn run_in_order(job: impl FnOnce() + Send + 'static) {
    type Job = Box<dyn FnOnce() + Send>;
    static QUEUE: std::sync::OnceLock<std::sync::mpsc::Sender<Job>> = std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (queue, jobs) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || jobs.into_iter().for_each(|job| job()));
        queue
    });
    let _ = queue.send(Box::new(job));
}

/// Why a compositor reader's `follow` stopped.
#[derive(Debug, PartialEq)]
enum End {
    /// The stream ended or its state desynced; the published state is cleared and a fresh stream's
    /// replay rebuilds every part.
    Lost,
    /// Nobody listens to either publisher.
    Unwanted,
}

/// Clears both published states after a lost stream; [`End::Unwanted`] once nobody listens.
fn lost(workspaces: &mut StatePublisher, windows: &mut WindowsPublisher, overview_open: Option<bool>) -> End {
    let workspaces_alive = workspaces.publish(&[], None, None, overview_open);
    let windows_alive = windows.publish(Vec::new());
    if workspaces_alive || windows_alive { End::Lost } else { End::Unwanted }
}

/// Connects, runs `follow`, and reconnects after each loss with `RETRY_FIRST` doubling to
/// [`RETRY_MAX`] like the audio mixer, until it reports [`End::Unwanted`]. The first connect retries
/// too. `first_delay` is a parameter for tests.
/// ponytail: a compositor that never comes up keeps one thread retrying every 30 s until exit; stop once the publishers are gone.
fn keep_following<S>(
    compositor: &str,
    first_delay: Duration,
    mut connect: impl FnMut() -> std::io::Result<S>,
    mut follow: impl FnMut(S) -> End,
) {
    let mut delay = first_delay;
    loop {
        let mut failures = 0;
        let stream = loop {
            match connect() {
                Ok(stream) => break stream,
                Err(err) if failures == 0 => warn!("cannot reach {compositor} ({err}); retrying"),
                Err(err) => debug!("cannot reach {compositor} ({err}); retrying in {delay:?}"),
            }
            failures += 1;
            std::thread::sleep(delay);
            delay = (delay * 2).min(RETRY_MAX);
        };
        let started = std::time::Instant::now();
        if follow(stream) == End::Unwanted {
            return;
        }
        if started.elapsed() >= STABLE {
            delay = first_delay;
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(RETRY_MAX);
    }
}

fn session_desktop() -> Option<String> {
    let value = std::env::var("XDG_CURRENT_DESKTOP").ok()?;
    desktop_name(&value).map(str::to_string)
}

/// `$XDG_CURRENT_DESKTOP`'s first entry. The spec orders its colon-separated list most to least
/// specific, so `"niri:wlroots"` names niri. Separate from [`session_desktop`] to test parsing
/// without writing a process-global env var from a test thread.
fn desktop_name(value: &str) -> Option<&str> {
    let first = value.split(':').next()?.trim();
    (!first.is_empty()).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compositor_kind_is_detectable_by_the_var_that_compositor_sets() {
        // The exhaustive `match` forces each new kind to name its env var. `find` and the length
        // then force that pair into `PROBES`; a missing entry is undetectable.
        for kind in [CompositorKind::Hyprland, CompositorKind::Niri, CompositorKind::Sway, CompositorKind::Mango] {
            let var = match kind {
                CompositorKind::Hyprland => "HYPRLAND_INSTANCE_SIGNATURE",
                CompositorKind::Niri => "NIRI_SOCKET",
                CompositorKind::Sway => "SWAYSOCK",
                CompositorKind::Mango => "MANGO_INSTANCE_SIGNATURE",
            };
            assert_eq!(PROBES.iter().find(|(probe, _)| *probe == kind).map(|(_, v)| *v), Some(var), "{kind:?}");
        }
        assert_eq!(PROBES.len(), 4, "a PROBES entry for a kind the loop above does not list");
    }

    #[test]
    fn run_in_order_keeps_send_order_when_the_first_job_is_slow() {
        let (seen, arrived) = std::sync::mpsc::channel();
        let first = seen.clone();
        run_in_order(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            first.send(1).unwrap();
        });
        run_in_order(move || seen.send(2).unwrap());

        let wait = std::time::Duration::from_secs(5);
        let order = [arrived.recv_timeout(wait), arrived.recv_timeout(wait)];
        assert_eq!(order, [Ok(1), Ok(2)]);
    }

    #[test]
    fn a_failed_first_connect_and_a_lost_stream_both_retry_until_nobody_listens() {
        let (mut connects, mut follows) = (0, 0);
        keep_following(
            "niri",
            Duration::ZERO,
            || {
                connects += 1;
                if connects == 1 { Err(std::io::ErrorKind::NotFound.into()) } else { Ok(connects) }
            },
            |_| {
                follows += 1;
                if follows < 3 { End::Lost } else { End::Unwanted }
            },
        );

        assert_eq!((follows, connects), (3, 4), "the first connect failed once and was retried");
    }

    #[test]
    fn a_request_gives_up_on_a_socket_that_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent.sock");
        let _silent = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let started = std::time::Instant::now();
        let hypr = hyprland::hyprland_request(&path, "j/clients").unwrap_err();
        let niri = niri::niri_request(&path, &niri_ipc::Request::Version).unwrap_err();

        assert_eq!([hypr.kind(), niri.kind()], [std::io::ErrorKind::WouldBlock; 2]);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn desktop_name_takes_the_most_specific_entry_of_a_colon_separated_list() {
        assert_eq!(desktop_name("niri:wlroots"), Some("niri"));
        assert_eq!(desktop_name("Hyprland"), Some("Hyprland"));
        assert_eq!(desktop_name(" river : wlroots "), Some("river"));
    }

    #[test]
    fn desktop_name_is_none_when_the_variable_is_set_but_empty() {
        // Bare `weston`/`cage` can leave `XDG_CURRENT_DESKTOP` set but empty; reporting
        // "this session is , which has no implementor" is worse than the generic line.
        assert_eq!(desktop_name(""), None);
        assert_eq!(desktop_name("   "), None);
        assert_eq!(desktop_name(":wlroots"), None);
    }
}

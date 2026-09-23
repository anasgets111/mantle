//! A generation's authoritative identity and process handle, sibling Renderer resolution, exit
//! classification/reporting, and `RestartBrake`. `Supervisor::respawn_renderer` consults the brake.
//! Respawn stays in its `select!` loop (ADR-0037), where the arm shares loop state.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// ADR-0058 decision 3: three deaths in a minute respawn at once; a fourth waits the cooldown, so a
/// config that kills every Renderer cannot strobe the lock screen and a GPU out of memory gets time.
pub(super) const RESTART_LIMIT: usize = 3;
pub(super) const RESTART_WINDOW: Duration = Duration::from_secs(60);
pub(super) const RESTART_COOLDOWN: Duration = Duration::from_secs(30);

/// Installed Renderer filename. `renderer` is too generic for a user's `$PATH`; `cargo install`
/// puts every binary in one directory.
pub(crate) const RENDERER_BINARY: &str = "mantle-renderer";

/// Resolves the Renderer as a sibling of the running Supervisor.
///
/// Keeps the Renderer off `$PATH` in an install with no code behind it. On Linux `current_exe`
/// reads symlink-resolved `/proc/self/exe`, so `$PREFIX/bin/mantle -> ../lib/mantle/mantle` finds
/// `$PREFIX/lib/mantle/mantle-renderer`; users get one command on `$PATH` and the pair stays
/// together.
///
/// The sibling rule makes `cargo run` a trap: it rebuilds one half and launches the other's stale
/// binary. `just run` builds both.
pub(crate) fn renderer_binary_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name(RENDERER_BINARY))
}

/// glvnd loads every EGL vendor it finds, so without this a machine that renders only on NVIDIA
/// also maps Mesa's libgallium and libLLVM into the Renderer (7.5 MB PSS measured).
pub(crate) const EGL_VENDOR_ENV: &str = "__EGL_VENDOR_LIBRARY_FILENAMES";

/// NVIDIA's EGL vendor file in `vendor_dir` when every render node under `sys_root` is bound to the
/// `nvidia` driver. `None` on a hybrid or non-NVIDIA machine, which needs Mesa.
///
/// ponytail: reads one vendor directory, not `/etc/glvnd/egl_vendor.d` too; a vendor file only
/// there leaves glvnd loading every vendor, as it does without this.
pub(crate) fn nvidia_egl_vendor(sys_root: &Path, vendor_dir: &Path) -> Option<PathBuf> {
    let drivers = std::fs::read_dir(sys_root.join("class/drm"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|node| node.file_name().to_string_lossy().starts_with("renderD"))
        .map(|node| std::fs::read_link(node.path().join("device/driver")))
        .collect::<io::Result<Vec<_>>>()
        .ok()?;
    if drivers.is_empty() || !drivers.iter().all(|driver| driver.file_name().is_some_and(|name| name == "nvidia")) {
        return None;
    }
    std::fs::read_dir(vendor_dir).ok()?.filter_map(Result::ok).map(|entry| entry.path()).find(|path| {
        path.extension().is_some_and(|ext| ext == "json")
            && path.file_name().is_some_and(|name| name.to_string_lossy().contains("nvidia"))
    })
}

/// The Renderer binary and what every generation is told. Passed on each spawn, never set on the
/// Supervisor, so what the shell launches inherits only the Supervisor's own environment.
pub(crate) struct Renderer {
    path: PathBuf,
    env: Vec<(&'static str, OsString)>,
}

impl Renderer {
    pub(crate) fn new(
        path: PathBuf,
        instance_dir: &Path,
        config_dir: &Path,
        profile: Option<u64>,
        verbose: u8,
        egl_vendor: Option<PathBuf>,
    ) -> Self {
        let mut env =
            vec![(shared::INSTANCE_DIR_ENV, instance_dir.into()), (shared::CONFIG_DIR_ENV, config_dir.into())];
        env.extend(profile.map(|secs| (shared::PROFILE_ENV, secs.to_string().into())));
        env.extend(egl_vendor.map(|path| (EGL_VENDOR_ENV, path.into())));
        // Sent only when it says something: an absent `MANTLE_VERBOSE` and a `0` both read back as
        // no `-v` at all.
        if verbose > 0 {
            env.push((shared::VERBOSE_ENV, verbose.to_string().into()));
        }
        Self { path, env }
    }

    pub(crate) fn spawn(&self, generation_id: u32) -> io::Result<tokio::process::Child> {
        let generation = [(shared::GENERATION_ID_ENV, generation_id.to_string().into())];
        crate::process::spawn_group_leader(&self.path, &[], &[&self.env[..], &generation].concat())
    }
}

/// One generation's identity and process handle while authoritative; replaced wholesale on respawn.
pub(super) struct Authoritative {
    pub(super) generation_id: u32,
    pub(super) child: tokio::process::Child,
}

/// How the authoritative Renderer ended (ADR-0058 decision 2). `Clean` is not a crash, so
/// `main`'s shutdown reap is never reported as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RendererDeparture {
    Clean,
    Failed { code: i32 },
    Signalled { signal: i32 },
}

/// Check the signal first: a signalled child has no exit code, so `code()` first returns `None` and
/// loses what happened.
pub(super) fn classify_departure(status: std::process::ExitStatus) -> RendererDeparture {
    use std::os::unix::process::ExitStatusExt;

    if let Some(signal) = status.signal() {
        return RendererDeparture::Signalled { signal };
    }
    match status.code() {
        Some(0) => RendererDeparture::Clean,
        Some(code) => RendererDeparture::Failed { code },
        // Linux `wait(2)` produces neither a code nor a signal only outside its normal shapes. A
        // panic here would kill the process that can still recover it, so return `-1` instead.
        None => RendererDeparture::Failed { code: -1 },
    }
}

/// ADR-0058 decision 3: at most `RESTART_LIMIT` restarts inside `RESTART_WINDOW`, then a
/// `RESTART_COOLDOWN` wait. Without the brake, one dead bar turns into a lock screen flickering
/// every few hundred milliseconds.
///
/// Sliding window, not total count: a Renderer dying once a day for a month is a logged bug, not a
/// restart loop; a total would eventually refuse a shell healthy since the last reboot.
#[derive(Default)]
pub(super) struct RestartBrake {
    /// Restart instants in the current window, oldest first; bounded by `RESTART_LIMIT`.
    recent: std::collections::VecDeque<std::time::Instant>,
}

impl RestartBrake {
    /// Records an attempt at `now` and says whether it may proceed; a refusal clears the window for
    /// the cooldown. Injecting `now` makes the window testable without a test that takes an hour.
    pub(super) fn allow(&mut self, now: std::time::Instant) -> bool {
        while self.recent.front().is_some_and(|at| now.duration_since(*at) >= RESTART_WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= RESTART_LIMIT {
            self.recent.clear();
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// Human-readable Renderer departure with lock state (ADR-0058 decision 2). The compositor does
/// not unlock when a lock client dies, so losing a Renderer holding `ext_session_lock_v1` costs the
/// session, not just the bar.
pub(super) fn departure_report(departure: RendererDeparture, generation_id: u32, lock_active: bool) -> String {
    let what = match departure {
        RendererDeparture::Clean => "exited cleanly".to_string(),
        RendererDeparture::Failed { code } => format!("exited with code {code}"),
        RendererDeparture::Signalled { signal } => format!("was killed by signal {signal}"),
    };
    let lock = if lock_active {
        ", and it held the session lock: the compositor does not unlock when a lock client dies, so the session stays \
         locked until a replacement takes the lock over (ADR-0058)"
    } else {
        ""
    };
    format!("generation {generation_id}'s renderer {what}{lock}")
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    #[test]
    fn only_a_machine_whose_every_render_node_is_nvidia_gets_the_nvidia_egl_vendor() {
        let root = tempfile::tempdir().unwrap();
        let (sys, vendors) = (root.path().join("sys"), root.path().join("egl_vendor.d"));
        std::fs::create_dir_all(&vendors).unwrap();
        std::fs::write(vendors.join("10_nvidia.json"), "{}").unwrap();
        std::fs::write(vendors.join("50_mesa.json"), "{}").unwrap();
        let node = |name: &str, driver: &str| {
            let device = sys.join("class/drm").join(name).join("device");
            std::fs::create_dir_all(&device).unwrap();
            std::os::unix::fs::symlink(format!("../../../bus/pci/drivers/{driver}"), device.join("driver")).unwrap();
        };

        assert_eq!(nvidia_egl_vendor(&sys, &vendors), None, "no render node");
        node("renderD128", "nvidia");
        assert_eq!(nvidia_egl_vendor(&sys, &vendors), Some(vendors.join("10_nvidia.json")));
        node("renderD129", "i915");
        assert_eq!(nvidia_egl_vendor(&sys, &vendors), None, "a hybrid machine needs Mesa");
    }

    #[test]
    fn a_renderer_is_told_its_instance_config_profile_and_verbosity_explicitly() {
        let env = |profile, verbose| {
            Renderer::new(PathBuf::new(), Path::new("/instance"), Path::new("/cfg"), profile, verbose, None).env
        };
        let told = [(shared::INSTANCE_DIR_ENV, "/instance".into()), (shared::CONFIG_DIR_ENV, "/cfg".into())];
        assert_eq!(env(None, 0), told, "no profile, no -v: neither optional entry is sent");
        assert_eq!(env(Some(60), 0), [&told[..], &[(shared::PROFILE_ENV, "60".into())]].concat());
        assert_eq!(env(None, 2), [&told[..], &[(shared::VERBOSE_ENV, "2".into())]].concat());
    }

    /// Raw `wait(2)` status for normal exit `code`; keeps `<< 8` in one place.
    fn exited(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code << 8)
    }

    fn killed_by(signal: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(signal)
    }

    #[test]
    fn a_renderer_that_exits_zero_is_not_a_crash() {
        assert_eq!(classify_departure(exited(0)), RendererDeparture::Clean);
    }

    #[test]
    fn a_renderer_that_exits_nonzero_carries_its_code() {
        assert_eq!(classify_departure(exited(101)), RendererDeparture::Failed { code: 101 });
    }

    #[test]
    fn a_renderer_killed_by_a_signal_reports_the_signal_not_an_exit_code() {
        // A signalled child has no exit code; calling `code()` first returns `None` and loses this.
        assert_eq!(classify_departure(killed_by(9)), RendererDeparture::Signalled { signal: 9 });
    }

    #[test]
    fn a_departure_while_locked_says_the_session_stays_locked() {
        let report = departure_report(RendererDeparture::Signalled { signal: 9 }, 3, true);

        assert!(report.contains("signal 9"), "the signal has to survive into the message: {report}");
        assert!(
            report.contains("session stays locked"),
            "a Renderer that died holding the lock is a different emergency from one that died without it, and the \
             message is the only place that distinction reaches a human: {report}"
        );
    }

    #[test]
    fn a_departure_while_unlocked_does_not_mention_the_lock() {
        let report = departure_report(RendererDeparture::Failed { code: 101 }, 3, false);

        assert!(report.contains("code 101"), "{report}");
        assert!(!report.contains("locked"), "an unlocked crash must not cry lock: {report}");
    }

    #[test]
    fn the_brake_allows_restarts_up_to_its_limit() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::default();

        for attempt in 0..3 {
            assert!(brake.allow(start + Duration::from_secs(attempt)), "restart {attempt} is within the limit");
        }
    }

    #[test]
    fn the_brake_stops_a_crash_loop_once_the_limit_is_reached_inside_the_window() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::default();
        for attempt in 0..3 {
            brake.allow(start + Duration::from_secs(attempt));
        }

        assert!(
            !brake.allow(start + Duration::from_secs(4)),
            "a config that kills every Renderer it is handed must wait out the cooldown"
        );
        assert!(brake.allow(start + Duration::from_secs(5)), "a refusal clears the window for the cooldown");
    }

    #[test]
    fn the_brake_forgets_restarts_older_than_its_window() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::default();
        for attempt in 0..3 {
            brake.allow(start + Duration::from_secs(attempt));
        }

        // An hour later is a new incident; counting it would refuse a shell healthy all day.
        assert!(brake.allow(start + Duration::from_secs(3600)), "the window has long passed");
    }

    #[test]
    fn a_slow_crash_loop_never_trips_the_brake() {
        let start = std::time::Instant::now();
        let mut brake = RestartBrake::default();

        // One crash per window forever is a logged bug, not a restart loop.
        for attempt in 0..10 {
            assert!(
                brake.allow(start + Duration::from_secs(attempt * 61)),
                "crash {attempt} stands alone in its window"
            );
        }
    }
}

//! `mantle.sysinfo` provides CPU/RAM/swap/temperature telemetry with three independently
//! Lua-configurable poll intervals (ADR-0035).

use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub mod controller;
pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod net;
pub mod ram;
pub mod temp;

pub use controller::SysinfoController;
use shared::action::SysinfoAction;
use shared::{debug, warn};

/// `configure` is synchronous: it rewrites shared config under its lock and nudges watch channels
/// (ADR-0037, ADR-0035).
pub fn dispatch(controller: &SysinfoController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<SysinfoAction>(&envelope.params) else { return };
    match action {
        SysinfoAction::Configure { intervals } => controller.configure(intervals),
    }
}

/// Why [`run_limited`] produced no output. `Busy` is routine, the rest are worth a log line.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RunError {
    Busy,
    Missing,
    TimedOut,
    Failed(&'static str),
}

impl RunError {
    fn reason(&self) -> Option<&'static str> {
        match self {
            RunError::Busy => None,
            RunError::Missing => Some("is not installed"),
            RunError::TimedOut => Some("timed out"),
            RunError::Failed(reason) => Some(reason),
        }
    }
}

/// Stdout of a successful `program`, or why there is none: a previous run of `busy` still alive,
/// spawn failure, non-zero exit, read error or `limit` expiry. On expiry the child is killed and
/// `busy` clears only once it has actually been reaped.
pub(super) async fn run_limited(
    program: &str,
    args: &[&str],
    limit: Duration,
    busy: &'static AtomicBool,
) -> Result<Vec<u8>, RunError> {
    if busy.swap(true, Ordering::AcqRel) {
        return Err(RunError::Busy);
    }
    let spawned =
        Command::new(program).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            busy.store(false, Ordering::Release);
            return Err(if e.kind() == io::ErrorKind::NotFound {
                RunError::Missing
            } else {
                RunError::Failed("could not be started")
            });
        }
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let read = async {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.map_err(|_| RunError::Failed("output could not be read"))?;
        let status = child.wait().await.map_err(|_| RunError::Failed("could not be waited on"))?;
        if status.success() { Ok(out) } else { Err(RunError::Failed("exited with an error")) }
    };
    match tokio::time::timeout(limit, read).await {
        Ok(out) => {
            busy.store(false, Ordering::Release);
            out
        }
        Err(_) => {
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
                busy.store(false, Ordering::Release);
            });
            Err(RunError::TimedOut)
        }
    }
}

/// [`run_limited`] with its failure logged once per tool and cause, since the tick repeats forever.
/// `optional` tools sit in a fallback chain, so any failure of theirs is only a debug line: a
/// leftover `nvidia-smi` exits non-zero on a healthy AMD box.
pub(super) async fn run_logged(
    program: &str,
    args: &[&str],
    limit: Duration,
    busy: &'static AtomicBool,
    optional: bool,
) -> Option<Vec<u8>> {
    let err = match run_limited(program, args, limit, busy).await {
        Ok(out) => return Some(out),
        Err(err) => err,
    };
    if let Some(reason) = err.reason()
        && shared::log::first_failure(&format!("{program} {reason}"))
    {
        if optional {
            debug!("`{program}` {reason}");
        } else {
            warn!("`{program}` {reason}; its sysinfo readings are unavailable");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_run_still_alive_skips_the_tick_and_a_timed_out_child_is_reaped_before_the_next() {
        static BUSY: AtomicBool = AtomicBool::new(true);
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        assert_eq!(run_limited("sh", &["-c", &touch], Duration::from_secs(5), &BUSY).await, Err(RunError::Busy));
        assert!(!marker.exists(), "no second child may start while one is outstanding");

        BUSY.store(false, Ordering::Release);
        assert_eq!(run_limited("sh", &["-c", "echo hi"], Duration::from_secs(5), &BUSY).await, Ok(b"hi\n".to_vec()));
        let slow = run_limited("sh", &["-c", "sleep 30"], Duration::from_millis(100), &BUSY).await;
        assert_eq!(slow, Err(RunError::TimedOut));
        crate::capabilities::test_support::poll_until(|| (!BUSY.load(Ordering::Acquire)).then_some(())).await;
    }

    #[tokio::test]
    async fn a_missing_tool_and_a_failing_one_are_told_apart_and_released() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        let limit = Duration::from_secs(5);
        assert_eq!(run_limited("mantle-no-such-tool", &[], limit, &BUSY).await, Err(RunError::Missing));
        assert!(!BUSY.load(Ordering::Acquire), "a spawn failure must not leave the tool marked busy");
        assert_eq!(
            run_limited("sh", &["-c", "exit 3"], limit, &BUSY).await,
            Err(RunError::Failed("exited with an error"))
        );
    }

    #[tokio::test]
    async fn run_logged_hands_back_output_only_on_success() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        let limit = Duration::from_secs(5);
        assert_eq!(run_logged("sh", &["-c", "echo hi"], limit, &BUSY, false).await, Some(b"hi\n".to_vec()));
        assert_eq!(run_logged("sh", &["-c", "exit 1"], limit, &BUSY, false).await, None);
        assert_eq!(run_logged("mantle-no-such-tool", &[], limit, &BUSY, true).await, None);
        assert_eq!(RunError::Busy.reason(), None, "a busy tick is never logged");
        assert_eq!(RunError::Missing.reason(), Some("is not installed"));
    }
}

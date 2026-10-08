//! `mantle.sysinfo` provides CPU/RAM/swap/temperature telemetry with three independently
//! Lua-configurable poll intervals (ADR-0035).

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

/// `configure` is synchronous: it rewrites shared config under its lock and nudges watch channels
/// (ADR-0037, ADR-0035).
pub fn dispatch(controller: &SysinfoController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<SysinfoAction>(&envelope.params) else { return };
    match action {
        SysinfoAction::Configure { intervals } => controller.configure(intervals),
    }
}

/// Stdout of a successful `program`, or `None` on spawn failure, non-zero exit, `limit` expiry or
/// a previous run of `busy` still alive. On expiry the child is killed and `busy` clears only once
/// it has actually been reaped.
pub(super) async fn run_limited(
    program: &str,
    args: &[&str],
    limit: Duration,
    busy: &'static AtomicBool,
) -> Option<Vec<u8>> {
    if busy.swap(true, Ordering::AcqRel) {
        return None;
    }
    let spawned =
        Command::new(program).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn();
    let Ok(mut child) = spawned else {
        busy.store(false, Ordering::Release);
        return None;
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let read = async {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.ok()?;
        child.wait().await.ok()?.success().then_some(out)
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
            None
        }
    }
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
        assert_eq!(run_limited("sh", &["-c", &touch], Duration::from_secs(5), &BUSY).await, None);
        assert!(!marker.exists(), "no second child may start while one is outstanding");

        BUSY.store(false, Ordering::Release);
        assert_eq!(run_limited("sh", &["-c", "echo hi"], Duration::from_secs(5), &BUSY).await, Some(b"hi\n".to_vec()));
        assert_eq!(run_limited("sh", &["-c", "sleep 30"], Duration::from_millis(100), &BUSY).await, None);
        for _ in 0..100 {
            if !BUSY.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the killed child was never reaped");
    }
}

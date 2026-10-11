//! The signals that would end the Supervisor, caught with their sender. tokio's handlers name the
//! signal but not who sent it, and the ones it was not asked for (HUP, USR1) killed the shell
//! without a line in its log.
//!
//! A handler rather than a `signalfd`: that needs the signals blocked in every thread, and std
//! hands a blocked mask on to every child it spawns. A caught signal resets to the default at
//! `exec`, so children start as if nothing was installed.

use std::io::Read;
use std::os::fd::IntoRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Every default-terminate signal but the real-time ones. QUIT, ABRT and the other core-dumping
/// signals keep their default, so a dump asked for still lands.
const TERMINATING: [Signal; 10] = [
    Signal::SIGHUP,
    Signal::SIGINT,
    Signal::SIGTERM,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGALRM,
    Signal::SIGVTALRM,
    Signal::SIGPROF,
    Signal::SIGIO,
    Signal::SIGPWR,
];

/// The pipe's write end, for the handler; set once before any handler is installed.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// A terminating signal and who sent it, described while the sender still exists.
pub struct Received {
    pub signal: Signal,
    pub sender: String,
}

/// Catches [`TERMINATING`], the way TERM and INT were caught before. Overrides an inherited
/// ignore too, so a shell started as a background job still stops on Ctrl-C's signal.
pub fn watch() -> std::io::Result<UnboundedReceiver<Received>> {
    watch_signals(&TERMINATING)
}

fn watch_signals(signals: &[Signal]) -> std::io::Result<UnboundedReceiver<Received>> {
    let (mut reader, writer) = std::io::pipe()?;
    // Never block in a handler: a full pipe drops the record, not the process.
    fcntl(&writer, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    WRITE_FD.store(writer.into_raw_fd(), Ordering::Relaxed);
    let action =
        SigAction::new(SigHandler::SigAction(record), SaFlags::SA_SIGINFO | SaFlags::SA_RESTART, SigSet::empty());
    for &signal in signals {
        // SAFETY: `record` only calls `write(2)`, which is async-signal-safe.
        unsafe { sigaction(signal, &action) }?;
    }
    let (tx, rx) = unbounded_channel();
    std::thread::Builder::new().name("mantle-signals".into()).spawn(move || {
        let mut bytes = [0u8; 12];
        while reader.read_exact(&mut bytes).is_ok() {
            let field = |i: usize| i32::from_ne_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
            let Ok(signal) = Signal::try_from(field(0)) else { continue };
            // Here, not in the main loop: `kill` exits as soon as it has sent.
            let sender = sender(Path::new("/proc"), field(4) as u32, field(8) as u32);
            if tx.send(Received { signal, sender }).is_err() {
                return;
            }
        }
    })?;
    Ok(rx)
}

extern "C" fn record(signo: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid `siginfo_t` to an `SA_SIGINFO` handler.
    let (pid, uid) = unsafe { ((*info).si_pid(), (*info).si_uid()) };
    let mut bytes = [0u8; 12];
    bytes[..4].copy_from_slice(&signo.to_ne_bytes());
    bytes[4..8].copy_from_slice(&pid.to_ne_bytes());
    bytes[8..].copy_from_slice(&uid.to_ne_bytes());
    // The interrupted code may be about to read `errno`.
    let errno = nix::errno::Errno::last_raw();
    // SAFETY: a plain `write(2)` of a local buffer; 12 bytes is under `PIPE_BUF`, so it lands whole.
    unsafe { libc::write(WRITE_FD.load(Ordering::Relaxed), bytes.as_ptr().cast(), bytes.len()) };
    nix::errno::Errno::set_raw(errno);
}

/// `pid 812 uid 1000 (kill -HUP 4410)`; `gone` when it exited before this read.
fn sender(proc_root: &Path, pid: u32, uid: u32) -> String {
    if pid == 0 {
        return "the kernel".to_string();
    }
    let cmdline = std::fs::read(proc_root.join(pid.to_string()).join("cmdline")).map_or_else(
        |_| "gone".to_string(),
        |bytes| String::from_utf8_lossy(&bytes).replace('\0', " ").trim_end().to_string(),
    );
    format!("pid {pid} uid {uid} ({cmdline})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sender_is_named_by_its_command_line() {
        let proc_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(proc_root.path().join("812")).unwrap();
        std::fs::write(proc_root.path().join("812/cmdline"), b"kill\0-HUP\x004410\0").unwrap();
        assert_eq!(sender(proc_root.path(), 812, 1000), "pid 812 uid 1000 (kill -HUP 4410)");
        assert_eq!(sender(proc_root.path(), 813, 1000), "pid 813 uid 1000 (gone)");
        assert_eq!(sender(proc_root.path(), 0, 0), "the kernel");
    }

    /// USR2 alone: TERM caught for good would outlive a `timeout` around the test binary. Ignored
    /// first, as INT is for a background job: the catch must override that, and the child must not
    /// inherit it.
    #[tokio::test]
    async fn a_signal_arrives_with_the_process_that_sent_it() {
        // SAFETY: sets a disposition; no handler code runs.
        unsafe { nix::sys::signal::signal(Signal::SIGUSR2, SigHandler::SigIgn) }.unwrap();
        let mut received = watch_signals(&[Signal::SIGUSR2]).unwrap();
        // `read`, a builtin, holds the shell as itself; a trailing `sleep` could be exec'd in its place.
        let script = "kill -USR2 $PPID; read _";
        let child = tokio::process::Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let Received { signal, sender } =
            crate::capabilities::test_support::within(received.recv()).await.expect("the reader is running");
        assert_eq!(signal, Signal::SIGUSR2);
        let (pid, uid) = (child.id().unwrap(), nix::unistd::getuid());
        assert_eq!(sender, format!("pid {pid} uid {uid} (sh -c {script})"));
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let ignored = status.lines().find_map(|line| line.strip_prefix("SigIgn:")).unwrap().trim();
        let ignored = u64::from_str_radix(ignored, 16).unwrap();
        assert_eq!(ignored & 1 << (Signal::SIGUSR2 as u64 - 1), 0, "the child inherited USR2 ignored");
    }
}

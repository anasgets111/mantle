//! Lock before sleep (ADR-0385): a logind `sleep` delay inhibitor, so a suspend waits until the
//! lock screen is on the glass. `main.rs` starts it on the first `mantle.lock` start, which a lock
//! surface's password field sends and so does any read of `mantle.lock`; it then runs for the
//! Supervisor's lifetime. A config that never starts `mantle.lock` never delays a suspend.

use std::time::Duration;

use futures_util::StreamExt;
use shared::warn;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::capabilities::idle::inhibit::{INHIBIT_WHO, Login1ManagerProxy};

/// Longest a suspend waits for the lock, under logind's default `InhibitDelayMaxSec` of 5 s.
pub const LOCK_BEFORE_SLEEP: Duration = Duration::from_secs(3);

/// Takes a delay, then answers each `PrepareForSleep`. Sleep sends `main.rs` a lock request and
/// closes the delay when it answers or `bound` passes; resume takes a fresh delay. A refused first
/// delay is logged once and never retried: sleep still asks for the lock, which may then land only
/// on resume.
pub async fn delay_sleep_until_locked(
    system_bus: zbus::Connection,
    lock_requests: UnboundedSender<oneshot::Sender<()>>,
    bound: Duration,
) {
    let manager = match Login1ManagerProxy::new(&system_bus).await {
        Ok(manager) => manager,
        Err(err) => return warn!("logind is unreachable, so a suspend will not wait for the lock: {err}"),
    };
    let mut sleeps = match manager.receive_prepare_for_sleep().await {
        Ok(sleeps) => sleeps,
        Err(err) => return warn!("failed to subscribe to logind's PrepareForSleep, so a suspend will not lock: {err}"),
    };
    let mut delay = take_delay(&manager).await;
    let retake = delay.is_some();
    while let Some(signal) = sleeps.next().await {
        let Ok(args) = signal.args() else { continue };
        if !args.start {
            if retake {
                delay = take_delay(&manager).await;
            }
            continue;
        }
        let (locked_tx, locked) = oneshot::channel();
        if lock_requests.send(locked_tx).is_err() {
            return; // main is gone.
        }
        if tokio::time::timeout(bound, locked).await.is_err() {
            warn!("the lock was not up {bound:?} after logind announced sleep; letting the suspend go ahead");
        }
        drop(delay.take());
    }
}

async fn take_delay(manager: &Login1ManagerProxy<'_>) -> Option<zbus::zvariant::OwnedFd> {
    let delay = manager.inhibit("sleep", INHIBIT_WHO, "Lock the screen before sleep", "delay").await;
    delay.inspect_err(|err| warn!("logind refused a sleep delay, so a suspend may not wait for the lock: {err}")).ok()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::capabilities::idle::inhibit::tests::StubLogin1Manager;
    use crate::capabilities::test_support::{p2p_pair_serving, poll_until, within};

    type Inhibited = ((String, String, String, String), std::io::PipeReader);
    type Requests = UnboundedReceiver<oneshot::Sender<()>>;

    /// A stub logind and the guard running against it with `bound`.
    async fn guard(bound: Duration) -> (zbus::Connection, UnboundedReceiver<Inhibited>, Requests) {
        let (calls_tx, calls) = unbounded_channel();
        let (system_bus, logind) =
            p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/login1", StubLogin1Manager { calls: calls_tx }))
                .await;
        let (requests_tx, requests) = unbounded_channel();
        tokio::spawn(delay_sleep_until_locked(system_bus, requests_tx, bound));
        (logind, calls, requests)
    }

    async fn sleep(logind: &zbus::Connection, start: bool) {
        logind
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "PrepareForSleep",
                &start,
            )
            .await
            .expect("emit PrepareForSleep");
    }

    /// A delay taken with the expected arguments; its read end, non-blocking for [`held`].
    async fn delay(calls: &mut UnboundedReceiver<Inhibited>) -> std::io::PipeReader {
        let ((what, who, _, mode), lock) = within(calls.recv()).await.expect("logind was asked for a delay");
        assert_eq!((what.as_str(), who.as_str(), mode.as_str()), ("sleep", "mantle", "delay"));
        nix::fcntl::fcntl(&lock, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK)).unwrap();
        lock
    }

    /// The read end blocks while any write end is open and reads EOF once logind would see it closed.
    fn held(lock: &std::io::PipeReader) -> bool {
        match (&*lock).read(&mut [0]) {
            Ok(0) => false,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
            other => panic!("nothing writes to the delay: {other:?}"),
        }
    }

    async fn released(lock: &std::io::PipeReader) {
        poll_until(|| (!held(lock)).then_some(())).await;
    }

    #[tokio::test]
    async fn sleep_holds_the_delay_until_the_lock_is_up_and_resume_takes_a_fresh_one() {
        let (logind, mut calls, mut requests) = guard(Duration::from_secs(60)).await;
        let lock = delay(&mut calls).await;

        sleep(&logind, true).await;
        let locked = within(requests.recv()).await.expect("sleep asks for a lock");
        assert!(held(&lock), "suspend must wait for the lock");
        locked.send(()).unwrap();
        released(&lock).await;

        sleep(&logind, false).await;
        let lock = delay(&mut calls).await;
        assert!(held(&lock), "the next sleep must wait too");
    }

    #[tokio::test]
    async fn a_lock_that_never_comes_up_releases_the_delay_at_the_bound() {
        let (logind, mut calls, mut requests) = guard(Duration::from_millis(50)).await;
        let lock = delay(&mut calls).await;

        sleep(&logind, true).await;
        let _unanswered = within(requests.recv()).await.expect("sleep asks for a lock");
        released(&lock).await;
    }

    /// logind refusing every `Inhibit`, as a denied polkit check does.
    struct Refusing(UnboundedSender<()>);

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl Refusing {
        fn inhibit(
            &self,
            _what: &str,
            _who: &str,
            _why: &str,
            _mode: &str,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
            let _ = self.0.send(());
            Err(zbus::fdo::Error::AccessDenied("refused".into()))
        }
    }

    #[tokio::test]
    async fn a_refused_delay_still_locks_on_sleep_and_is_not_asked_for_again() {
        let (calls_tx, mut calls) = unbounded_channel();
        let (system_bus, logind) =
            p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/login1", Refusing(calls_tx))).await;
        let (requests_tx, mut requests) = unbounded_channel();
        tokio::spawn(delay_sleep_until_locked(system_bus, requests_tx, Duration::from_secs(60)));
        within(calls.recv()).await.expect("logind was asked for a delay");

        for _ in 0..2 {
            sleep(&logind, true).await;
            within(requests.recv()).await.expect("sleep still asks for a lock").send(()).unwrap();
            sleep(&logind, false).await;
        }
        // Signals are handled in order, so a retry on the first resume would have arrived by now.
        assert!(calls.try_recv().is_err(), "a refusal must be logged once, not retried on every resume");
    }
}

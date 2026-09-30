//! One eventfd wakes the Wayland thread's poll (ADR-0124). Poll blocks on the connection and this
//! fd with no timeout; the socket thread writes after each handed-over frame and decode workers
//! after each result. The loop runs when work exists, not 66 times per second on a 15ms timer.

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::time::Duration;

use nix::poll::PollTimeout;
use nix::sys::eventfd::{EfdFlags, EventFd};

/// Cloneable handle on the shared fd.
#[derive(Clone)]
pub struct Waker(Arc<EventFd>);

impl Waker {
    pub fn new() -> io::Result<Self> {
        Ok(Waker(Arc::new(EventFd::from_flags(EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)?)))
    }

    /// Makes the fd readable until [`Waker::drain`]. Eventfd counts accumulate, so a burst is one
    /// pending wakeup. `EAGAIN` means poll is already due.
    pub fn wake(&self) {
        let _ = self.0.write(1);
    }

    /// Clears the count so the next `poll` blocks. Called after wakeup, before servicing the turn,
    /// so a wake during that turn is not lost. Empty is `EAGAIN`.
    pub fn drain(&self) {
        let _ = self.0.read();
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Poll timeout for `remaining`, rounded up: truncating the last fraction of a wait to 0 ms makes
/// poll return at once, and the caller spins until the deadline. Overflow waits without a timeout.
pub fn poll_timeout(remaining: Duration) -> PollTimeout {
    PollTimeout::try_from(remaining.as_micros().div_ceil(1000)).unwrap_or(PollTimeout::NONE)
}

/// Wakes poll when the socket thread drops its `Sender`, which the loop reads as Supervisor exit.
pub struct WakeOnDrop(pub Waker);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        self.0.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(waker: &Waker) -> bool {
        let mut fds = [nix::poll::PollFd::new(waker.fd(), nix::poll::PollFlags::POLLIN)];
        matches!(nix::poll::poll(&mut fds, nix::poll::PollTimeout::ZERO), Ok(1))
    }

    #[test]
    fn a_sub_millisecond_remainder_polls_for_one_millisecond_not_zero() {
        assert_eq!(poll_timeout(Duration::from_micros(300)), PollTimeout::from(1u8));
        assert_eq!(poll_timeout(Duration::from_millis(5)), PollTimeout::from(5u8));
        assert_eq!(poll_timeout(Duration::ZERO), PollTimeout::ZERO);
    }

    #[test]
    fn a_wake_makes_the_fd_readable_once_and_a_drain_clears_it() {
        let waker = Waker::new().unwrap();
        assert!(!readable(&waker));
        waker.wake();
        waker.wake();
        assert!(readable(&waker), "two wakes are one pending wakeup");
        waker.drain();
        assert!(!readable(&waker));
    }

    #[test]
    fn dropping_the_guard_wakes() {
        let waker = Waker::new().unwrap();
        drop(WakeOnDrop(waker.clone()));
        assert!(readable(&waker));
    }
}

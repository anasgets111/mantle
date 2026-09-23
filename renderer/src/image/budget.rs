//! Decoded-byte accounting for the decode pool: who waits for room and who does not (ADR-0187).

use std::sync::{Condvar, Mutex};

use super::DECODE_POOL_BYTES;

/// The pool's shared ceiling on decoded pixels in flight, in bytes (ADR-0187).
///
/// A worker waits here until its decode fits, so four wallpapers arriving together decode in turn
/// rather than all at once, and one large file decodes alone rather than not at all.
///
/// ponytail: a decode is charged twice its output plus an RGBA8 copy, which covers a progressive
/// JPEG's coefficient planes and the conversion, and `decode_raster` holds the charge until it
/// returns. A source at the ceiling is charged past the pool and runs alone. Still uncharged: pixels
/// waiting in the result channel and `landed` for upload, and inline decodes, which never wait.
/// Upgrade path: hold the permit until `upload_landed` consumes the pixels, so it travels with them.
#[derive(Default)]
pub(crate) struct Budget {
    pub(super) in_flight: Mutex<u64>,
    room: Condvar,
}

impl Budget {
    /// Waits until `bytes` fit, then charges them.
    ///
    /// A decode is admitted when nothing else is in flight, whatever its size, so nothing is ever
    /// too big to run and no set of waiters can deadlock each other. `decode_within_limits` has
    /// already refused a source whose output alone is past the whole budget, so a larger charge is
    /// one decode's working copies, run alone.
    ///
    /// A poisoned lock hands back an uncharged permit and lets the decode through: a decode pool
    /// that has stopped accounting is worth less than a shell that has stopped drawing.
    fn acquire(&self, bytes: u64) -> Permit<'_> {
        let Ok(mut in_flight) = self.in_flight.lock() else { return Permit { budget: self, bytes: 0 } };
        loop {
            if admits(*in_flight, bytes) {
                *in_flight += bytes;
                return Permit { budget: self, bytes };
            }
            let Ok(waited) = self.room.wait(in_flight) else { return Permit { budget: self, bytes: 0 } };
            in_flight = waited;
        }
    }

    /// Charges `bytes` without waiting for room, for a decode that cannot afford to block: an
    /// inline load runs on the Wayland dispatch thread, and stalling that to wait on a background
    /// worker is a frozen shell. It still counts, so the workers see it.
    fn charge(&self, bytes: u64) -> Permit<'_> {
        if let Ok(mut in_flight) = self.in_flight.lock() {
            *in_flight += bytes;
            return Permit { budget: self, bytes };
        }
        Permit { budget: self, bytes: 0 }
    }
}

/// Where a decode's pixels are charged, and whether it may wait for room (ADR-0187).
#[derive(Clone, Copy)]
pub(crate) enum Charge<'a> {
    /// Not counted. A cached thumbnail is bounded by the slot size the caller asked for rather than
    /// by the source, so it never approaches the budget and waiting for one would be nothing but
    /// latency.
    Free,
    /// A background worker: waits until its pixels fit, which is what serializes four wallpapers
    /// arriving together.
    Waiting(&'a Budget),
    /// The Wayland dispatch thread: counted, so the workers see it, but never waiting. Blocking
    /// here stalls Wayland dispatch, Supervisor reads and input, trading a frozen shell for an
    /// accounting nicety.
    Immediate(&'a Budget),
}

impl<'a> Charge<'a> {
    /// Takes the charge, blocking only where that is safe.
    pub(super) fn take(self, bytes: u64) -> Option<Permit<'a>> {
        match self {
            Charge::Free => None,
            Charge::Waiting(budget) => Some(budget.acquire(bytes)),
            Charge::Immediate(budget) => Some(budget.charge(bytes)),
        }
    }
}

/// Whether `bytes` may start decoding with `in_flight` already charged.
///
/// Pure so the rule is testable without threads, which is where the deadlock would be. An empty
/// budget admits any size. `decode_within_limits` has already refused any output past
/// [`DECODE_POOL_BYTES`], so a larger charge runs only alone, and no decode waits on waiters that
/// are all waiting on it.
fn admits(in_flight: u64, bytes: u64) -> bool {
    in_flight == 0 || in_flight + bytes <= DECODE_POOL_BYTES
}

/// Holds a [`Budget`] charge for as long as the pixels it paid for are being produced. RAII because
/// `decode_raster` has a dozen `?` exits and every one of them has to give the bytes back.
pub(crate) struct Permit<'a> {
    budget: &'a Budget,
    bytes: u64,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        if let Ok(mut in_flight) = self.budget.in_flight.lock() {
            *in_flight = in_flight.saturating_sub(self.bytes);
        }
        // Outside the lock's scope above only by `notify_all`'s own rules; waking every waiter
        // rather than one because they want different amounts, and the one this would wake might
        // be the one that still does not fit.
        self.budget.room.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0187. The rule the old per-decode cap got wrong: what fits is a property of the pool,
    /// not of one worker's quarter of it.
    #[test]
    fn the_decode_budget_admits_by_what_is_in_flight_and_never_by_a_fixed_share() {
        // A 6024x3401 wallpaper decodes to 78 MiB. Alone it fits and always did; under the old
        // 64 MiB per-decode cap it was refused unread while three quarters of the pool sat idle.
        let wallpaper = 6024 * 3401 * 4;
        assert!(admits(0, wallpaper), "a lone wallpaper decode fits the pool it is charged against");
        assert!(admits(wallpaper, wallpaper), "and so does a second, at 156 MiB of 256");
        assert!(!admits(wallpaper * 3, wallpaper), "a fourth does not, and waits for one to finish");

        // Nothing in flight admits anything, so no decode is too big to ever run and no set of
        // waiters can be waiting only on each other.
        assert!(admits(0, DECODE_POOL_BYTES), "an empty budget admits a decode at the ceiling");
        assert!(!admits(1, DECODE_POOL_BYTES), "and one byte of company is enough to make it wait");
    }

    /// ADR-0187. The permit is RAII because `decode_raster` has a dozen `?` exits; a decode that
    /// fails after charging must still give the bytes back, or the pool shrinks by that much for
    /// the life of the process.
    #[test]
    fn a_permit_returns_its_bytes_and_wakes_a_waiter_however_the_decode_ends() {
        let budget = std::sync::Arc::new(Budget::default());
        {
            let _whole = budget.acquire(DECODE_POOL_BYTES);
            assert_eq!(*budget.in_flight.lock().unwrap(), DECODE_POOL_BYTES);
        }
        assert_eq!(*budget.in_flight.lock().unwrap(), 0, "a dropped permit gives its bytes back");

        // A waiter blocked behind a full budget is released when the permit drops, rather than
        // waiting for a timeout it does not have.
        let held = budget.acquire(DECODE_POOL_BYTES);
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = std::sync::Arc::clone(&budget);
        let joined = std::thread::spawn(move || {
            let _permit = waiting.acquire(DECODE_POOL_BYTES);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(),
            "a decode that does not fit must wait rather than run"
        );
        drop(held);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "and must be woken by the permit that made room, not by a poll"
        );
        joined.join().unwrap();
        assert_eq!(*budget.in_flight.lock().unwrap(), 0);
    }
}

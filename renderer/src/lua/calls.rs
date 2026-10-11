//! The id, pending-callback and result-channel core behind `palette.quantize` and `file.*`: work runs
//! off the Lua thread, its result returns by channel, and the loop's `poll` runs each callback once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use mlua::Function;

/// A unit of work already bound to its result channel and waker.
pub type Job = Box<dyn FnOnce() + Send>;

pub struct Calls<T>(Rc<RefCell<Inner<T>>>);

// Not derived: a derive would demand `T: Clone`.
impl<T> Clone for Calls<T> {
    fn clone(&self) -> Self {
        Calls(self.0.clone())
    }
}

struct Inner<T> {
    next_id: u64,
    pending: HashMap<u64, Function>,
    result_tx: Sender<(u64, T)>,
    results: Receiver<(u64, T)>,
    /// `None` under `mantle check`, which runs no loop to wake.
    waker: Option<crate::wake::Waker>,
    /// Under `mantle check` and the docs tests, which run no loop: a call returns its id and does no work.
    dry: bool,
}

impl<T: Send + 'static> Calls<T> {
    pub fn new(waker: Option<crate::wake::Waker>, dry: bool) -> Self {
        let (result_tx, results) = std::sync::mpsc::channel();
        Calls(Rc::new(RefCell::new(Inner { next_id: 0, pending: HashMap::new(), result_tx, results, waker, dry })))
    }

    /// Takes an id and, unless dry, returns the job for the caller to run on a thread of its choosing.
    pub fn submit(&self, on_done: Option<Function>, work: impl FnOnce() -> T + Send + 'static) -> (u64, Option<Job>) {
        let mut inner = self.0.borrow_mut();
        let id = inner.next_id;
        inner.next_id += 1;
        if inner.dry {
            return (id, None);
        }
        if let Some(on_done) = on_done {
            inner.pending.insert(id, on_done);
        }
        let (tx, waker) = (inner.result_tx.clone(), inner.waker.clone());
        let job = Box::new(move || {
            let _ = tx.send((id, work()));
            if let Some(waker) = waker {
                waker.wake();
            }
        });
        (id, Some(job))
    }

    /// Drops the callback; the work itself still finishes.
    pub fn cancel(&self, id: u64) {
        self.0.borrow_mut().pending.remove(&id);
    }

    /// Drops every pending callback at an evaluation boundary: each closes over the locals of an
    /// evaluation being replaced or rejected.
    pub fn clear(&self) {
        self.0.borrow_mut().pending.clear();
    }

    /// Hands each finished call's callback and result to `run` once; a cancelled or cleared id is dropped.
    pub fn poll(&self, mut run: impl FnMut(u64, &Function, T)) {
        let results: Vec<(u64, T)> = std::iter::from_fn(|| self.0.borrow().results.try_recv().ok()).collect();
        for (id, result) in results {
            let Some(on_done) = self.0.borrow_mut().pending.remove(&id) else { continue };
            run(id, &on_done, result);
        }
    }

    /// Takes the next result off the channel, runs `between`, then puts it back, so a test can cancel
    /// or clear after the work finished and before `poll` sees it.
    #[cfg(test)]
    pub fn replay(&self, between: impl FnOnce()) {
        let result = self.0.borrow().results.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        between();
        self.0.borrow().result_tx.send(result).unwrap();
    }
}

/// Calls `poll` every 5 ms until `done`, failing after 5 s.
#[cfg(test)]
pub fn poll_until(poll: impl Fn(), done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "callback never fired");
        poll();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

//! Latest-wins queue for a write action dispatched once per command.

use std::sync::{Arc, Mutex};

/// A value written by [`LatestWrites`], one at a time.
pub trait Writer: Clone + Send + 'static {
    fn write(&self, value: f64) -> impl Future<Output = ()> + Send;
}

#[derive(Default)]
struct Slot {
    pending: Option<f64>,
    draining: bool,
}

/// Values reach the writer in submit order, at most one write is in flight, and a burst collapses
/// to its last value: a slider drag cannot land `SetBrightness` calls out of order.
#[derive(Clone, Default)]
pub struct LatestWrites(Arc<Mutex<Slot>>);

impl LatestWrites {
    pub fn submit<W: Writer>(&self, writer: &W, value: f64) {
        let start = {
            let mut slot = self.0.lock().expect("mutex poisoned");
            slot.pending = Some(value);
            !std::mem::replace(&mut slot.draining, true)
        };
        if start {
            let (slot, writer) = (Arc::clone(&self.0), writer.clone());
            tokio::spawn(async move {
                loop {
                    let next = {
                        let mut slot = slot.lock().expect("mutex poisoned");
                        let next = slot.pending.take();
                        slot.draining = next.is_some();
                        next
                    };
                    let Some(value) = next else { return };
                    writer.write(value).await;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct Recorder {
        seen: Arc<Mutex<Vec<f64>>>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    impl Writer for Recorder {
        async fn write(&self, value: f64) {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            self.seen.lock().unwrap().push(value);
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_burst_is_written_in_order_one_at_a_time_and_ends_on_the_last_value() {
        let (queue, recorder) = (LatestWrites::default(), Recorder::default());
        for value in 1..=100 {
            queue.submit(&recorder, f64::from(value));
            tokio::task::yield_now().await;
        }
        while queue.0.lock().unwrap().draining {
            tokio::task::yield_now().await;
        }
        let seen = recorder.seen.lock().unwrap();
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "writes out of order: {seen:?}");
        assert_eq!(seen.last(), Some(&100.0));
        assert!(seen.len() < 100, "a burst must collapse");
        assert_eq!(recorder.max_in_flight.load(Ordering::SeqCst), 1);
    }
}

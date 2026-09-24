use shared::{CommandEnvelope, debug};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::log_unstarted;

/// A controller whose construction and signal handling wait on another service, so both run in its
/// own task: a NetworkManager that owned its name but stopped answering held the loop, and every lock
/// frame behind it, with no timeout. Requests queue in the channel until the build lands, in order.
pub(super) type Worker<C> = UnboundedSender<Box<dyn FnOnce(&C) + Send>>;

/// Builds a [`Worker`]; `handle` turns each run of equal queued signals into the state sent to the loop. A failed build
/// closes the worker, so the next start retries it.
pub(super) fn spawn_worker<C, S, T, Fut>(
    build: impl Future<Output = Option<C>> + Send + 'static,
    mut signals: UnboundedReceiver<S>,
    handle: impl Fn(C, S) -> Fut + Send + 'static,
    states: UnboundedSender<T>,
) -> Worker<C>
where
    C: Clone + Send + Sync + 'static,
    S: PartialEq + Send + 'static,
    T: Send + 'static,
    Fut: Future<Output = T> + Send,
{
    let (worker, mut requests) = unbounded_channel::<Box<dyn FnOnce(&C) + Send>>();
    tokio::spawn(async move {
        let Some(controller) = build.await else { return };
        loop {
            tokio::select! {
                Some(request) = requests.recv() => request(&controller),
                Some(signal) = signals.recv() => {
                    // Every handler re-reads the live service, so one pass covers a run of the same
                    // signal queued before it; a Wi-Fi scan's burst is one rebuild, not one per AP.
                    let mut burst = vec![signal];
                    while let Ok(signal) = signals.try_recv() {
                        burst.push(signal);
                    }
                    burst.dedup();
                    for signal in burst {
                        if states.send(handle(controller.clone(), signal).await).is_err() {
                            return;
                        }
                    }
                }
                else => break,
            }
        }
    });
    worker
}

/// Queues a command for a worker capability, or logs it like any unstarted one.
pub(super) fn queue<C: 'static>(
    worker: &Option<Worker<C>>,
    envelope: &CommandEnvelope,
    dispatch: fn(&C, &CommandEnvelope),
) {
    let Some(worker) = worker else { return log_unstarted(envelope) };
    let command = envelope.clone();
    if worker.send(Box::new(move |controller| dispatch(controller, &command))).is_err() {
        debug!(
            "{} for {} was dropped: its backend failed to start",
            envelope.params.action, envelope.params.capability
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_worker_runs_requests_sent_before_its_build_landed_in_order_and_a_failed_build_closes_it() {
        let (release, built) = tokio::sync::oneshot::channel::<()>();
        let (signal_tx, signals) = unbounded_channel();
        let (states_tx, mut states) = unbounded_channel();
        let worker = spawn_worker(
            async move { built.await.ok().map(|()| 7u32) },
            signals,
            |c, s: u32| async move { c + s },
            states_tx,
        );
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        for n in [1, 2] {
            let seen = seen.clone();
            worker.send(Box::new(move |c: &u32| seen.lock().unwrap().push(c * 10 + n))).unwrap();
        }
        signal_tx.send(1).unwrap();
        release.send(()).unwrap();

        assert_eq!(states.recv().await, Some(8), "a signal becomes state from the built controller");
        let (done, finished) = tokio::sync::oneshot::channel();
        worker.send(Box::new(move |_| done.send(()).unwrap())).unwrap();
        finished.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), [71, 72]);

        let failed = spawn_worker(
            async { None::<u32> },
            unbounded_channel::<u32>().1,
            |c, s| async move { c + s },
            unbounded_channel().0,
        );
        failed.closed().await;
    }

    /// A queued run of one signal is one rebuild; distinct signals keep their order.
    #[tokio::test]
    async fn a_worker_handles_a_run_of_equal_queued_signals_once() {
        let (release, built) = tokio::sync::oneshot::channel::<()>();
        let (signal_tx, signals) = unbounded_channel();
        let (states_tx, mut states) = unbounded_channel();
        // No request sender, so the task ends once the signals close.
        let _ = spawn_worker(async move { built.await.ok() }, signals, |(), s: u32| async move { s }, states_tx);
        for signal in [1, 1, 1, 2, 2, 1] {
            signal_tx.send(signal).unwrap();
        }
        release.send(()).unwrap();

        drop(signal_tx);
        let mut seen = Vec::new();
        while let Some(state) = states.recv().await {
            seen.push(state);
        }
        assert_eq!(seen, [1, 2, 1]);
    }
}

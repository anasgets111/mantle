//! [`IdleController`]: `mantle.idle`'s write dispatcher and state owner for notify and inhibit.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use shared::{debug, error, info, warn};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::Proxy;

use super::bus::{BusInhibit, BusLease};
use super::gate::{IdleGate, blocks};
use super::inhibit::{
    INHIBIT_MODE, INHIBIT_WHAT, INHIBIT_WHO, InhibitState, LiveInhibit, Login1ManagerProxy, SLEEP_INHIBIT_WHAT,
    apply_inhibit, apply_release_inhibit, cleanup_generation_inhibit,
};
use super::notify::{
    ListenerId, NotifyState, cancel_threshold_entry, cleanup_generation_thresholds, connect_wayland_idle,
    register_threshold_entry, spawn_idle_event_forwarder, take_unused_listeners,
};
use super::state::{IdleState, foreign_blockers};

/// Bound for [`connect_wayland_idle`]'s `spawn_blocking` task (see [`IdleController::new`]). A
/// local niri roundtrip is well under one second; 5s leaves headroom while bounding the genuine
/// compositor deadlock observed once at 60+ seconds without a timeout.
const IDLE_NOTIFY_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The two answers to "is anything holding the session awake", and the last payload built from
/// them (ADR-0160).
///
/// Different tasks watch logind's `BlockInhibited` and the compositor's silence. Neither may
/// publish a whole `IdleState`, which would erase what the other knows. Each sets its own field
/// here and takes back a payload to send, or `None` when nothing moved.
#[derive(Default)]
pub struct PublishedIdle {
    logind_blocked: bool,
    logind_inhibitors: Vec<super::state::IdleInhibitor>,
    sleep_blocked: bool,
    sleep_inhibitors: Vec<super::state::IdleInhibitor>,
    wayland_inhibited: bool,
    wayland_observed: bool,
    screensaver: Vec<super::state::IdleInhibitor>,
    power_management: Vec<super::state::IdleInhibitor>,
    last_sent: IdleState,
}

impl PublishedIdle {
    /// The merged payload. Nothing can name the compositor's half. No protocol lists
    /// idle-inhibitor holders, and the withholding has causes besides a surface inhibitor (see
    /// `notify::wayland_inhibited`). So it is an inhibitor with an empty `who`, the shape a config
    /// already draws for a logind holder that gave none, and a `why` stating the observation. An
    /// observation made before input resumed is kept in `inhibited`, but omitted from the roster.
    fn merged(&self) -> IdleState {
        let mut inhibitors = self.logind_inhibitors.clone();
        // Ours by the time logind hears of them, and `foreign_blockers` drops this shell's own
        // row, so the names can only come from here (ADR-0231).
        inhibitors.extend(self.screensaver.iter().cloned());
        if self.wayland_inhibited && self.wayland_observed {
            inhibitors.push(super::state::IdleInhibitor {
                who: String::new(),
                why: "the compositor is holding off idle notifications".to_string(),
            });
        }
        let mut sleep_inhibitors = self.sleep_inhibitors.clone();
        sleep_inhibitors.extend(self.power_management.iter().cloned());
        IdleState {
            inhibited: self.logind_blocked || self.wayland_inhibited || !self.screensaver.is_empty(),
            compositor_hold_stale: self.wayland_inhibited && !self.wayland_observed,
            inhibitors,
            sleep_inhibited: self.sleep_blocked || !self.power_management.is_empty(),
            sleep_inhibitors,
        }
    }

    /// Records a change and returns the payload to send, or `None` when the answer is unchanged.
    fn settle(&mut self) -> Option<IdleState> {
        let next = self.merged();
        if next == self.last_sent {
            return None;
        }
        self.last_sent = next.clone();
        Some(next)
    }

    /// `None` means input resumed and the compositor gave no current inhibitor answer. Keep the
    /// last gate value, but publish that its hold is unconfirmed; see `notify::wayland_inhibited`.
    pub(crate) fn set_wayland_inhibited(&mut self, held: Option<bool>) -> Option<IdleState> {
        self.wayland_observed = held.is_some();
        self.wayland_inhibited = held.unwrap_or(self.wayland_inhibited);
        self.settle()
    }

    /// Session-bus roster (`ScreenSaver` into `inhibitors`, `PowerManagement` into `sleep_inhibitors`).
    pub(super) fn set_bus_holds(
        &mut self,
        kind: BusInhibit,
        holds: Vec<super::state::IdleInhibitor>,
    ) -> Option<IdleState> {
        match kind {
            BusInhibit::ScreenSaver => self.screensaver = holds,
            BusInhibit::PowerManagement => self.power_management = holds,
        }
        self.settle()
    }

    pub(super) fn set_logind(
        &mut self,
        blocked: bool,
        inhibitors: Vec<super::state::IdleInhibitor>,
        sleep_blocked: bool,
        sleep_inhibitors: Vec<super::state::IdleInhibitor>,
    ) -> Option<IdleState> {
        self.logind_blocked = blocked;
        self.logind_inhibitors = inhibitors;
        self.sleep_blocked = sleep_blocked;
        self.sleep_inhibitors = sleep_inhibitors;
        self.settle()
    }
}

#[derive(Clone)]
pub struct IdleController {
    /// Registrations received while notify was [`NotifyState::Inert`], replayed when it becomes
    /// `Live` (ADR-0139). Config evaluation reliably beats setup, so without this every boot
    /// threshold was dropped, observed as `register_threshold(generation 0, 20s) ignored`.
    pending: Arc<std::sync::Mutex<Vec<(u32, u64)>>>,
    /// `std::sync::RwLock` around `Inert`/`Live`, swapped by the background setup task so
    /// constructing an `IdleController` never blocks on Wayland. Not the tokio one. Nothing holds
    /// it across an await, and a sync threshold half is what lets `dispatch` apply a forget and the
    /// registrations behind it in the order the socket delivered them (ADR-0158).
    notify: Arc<RwLock<NotifyState>>,
    pub(super) inhibit: Arc<LiveInhibit>,
    pub(crate) sleep_inhibit: Arc<LiveInhibit>,
    /// The last state [`watch_idle_inhibitors`] published, so `Capabilities::start` returns the
    /// current answer instead of `nil` until the next inhibitor (ADR-0141).
    pub(super) published: Arc<std::sync::Mutex<PublishedIdle>>,
    gate: Arc<std::sync::Mutex<IdleGate>>,
    /// Also held here, not just handed to the forwarder: reaping the last listener has to answer
    /// for the compositor itself, and no raw event will arrive to let the forwarder do it.
    pub(super) state_tx: UnboundedSender<IdleState>,
    /// Live session-bus lease while this process owns `ScreenSaver` or `PowerManagement`. Kept
    /// here so a failed or ended departure watch cannot drop the bus names, and dropping the
    /// controller aborts the watcher. Left `None` in clones handed to the object server and peer
    /// watcher to avoid an `Arc` cycle.
    pub(super) bus_lease: Option<Arc<BusLease>>,
}

impl IdleController {
    /// Constructs both halves and returns immediately. `system_bus` is the Supervisor's system bus;
    /// inhibit uses it directly (ADR-0032). `session_bus` carries the inbound halves,
    /// `org.freedesktop.ScreenSaver` (ADR-0231) and `org.freedesktop.PowerManagement`; `None`
    /// leaves both unserved.
    ///
    /// Notify starts [`NotifyState::Inert`] and upgrades to `Live` in a bounded `spawn_blocking`
    /// task running [`connect_wayland_idle`]. Its `roundtrip()` hung once against niri; awaiting
    /// it here would wedge the Supervisor.
    pub async fn new(
        system_bus: zbus::Connection,
        session_bus: Option<zbus::Connection>,
        events_tx: UnboundedSender<shared::IdleEvent>,
        state_tx: UnboundedSender<IdleState>,
    ) -> Self {
        let notify = Arc::new(RwLock::new(NotifyState::Inert));
        // Watch the always-present system bus separately: a held inhibitor matters even when
        // Wayland notify degraded to inert (ADR-0139).
        let gate = Arc::new(std::sync::Mutex::new(IdleGate::default()));
        let published = Arc::new(std::sync::Mutex::new(PublishedIdle::default()));
        tokio::spawn(watch_idle_inhibitors(
            system_bus.clone(),
            gate.clone(),
            events_tx.clone(),
            state_tx.clone(),
            published.clone(),
        ));

        let mut controller = Self {
            notify: notify.clone(),
            published: published.clone(),
            gate: gate.clone(),
            state_tx: state_tx.clone(),
            pending: Arc::new(std::sync::Mutex::new(Vec::new())),
            inhibit: Arc::new(LiveInhibit::new(system_bus.clone(), INHIBIT_WHAT)),
            sleep_inhibit: Arc::new(LiveInhibit::new(system_bus, SLEEP_INHIBIT_WHAT)),
            bus_lease: None,
        };

        let controller_for_task = controller.clone();
        if let Some(session_bus) = session_bus {
            controller.export_session_services(session_bus).await;
        }

        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(IDLE_NOTIFY_SETUP_TIMEOUT, tokio::task::spawn_blocking(connect_wayland_idle))
                    .await;
            match outcome {
                Ok(Ok(Ok((live, raw_events_rx)))) => {
                    spawn_idle_event_forwarder(
                        live.registry.clone(),
                        gate,
                        published,
                        raw_events_rx,
                        events_tx,
                        state_tx,
                    );
                    *notify.write().expect("rwlock poisoned") = NotifyState::Live(live);
                    debug!(
                        "dedicated Wayland connection for ext_idle_notifier_v1 established; notify live for this run"
                    );
                    // Swap first: otherwise `register_threshold` sees inert and requeues the
                    // replay.
                    controller_for_task.replay_pending_registrations();
                }
                Ok(Ok(Err(err))) => {
                    error!(
                        "dedicated Wayland connection for ext_idle_notifier_v1 unavailable; notify disabled for this run: {err}"
                    );
                }
                Ok(Err(join_err)) => {
                    error!(
                        "the dedicated Wayland connection setup task panicked; notify disabled for this run: {join_err}"
                    );
                }
                Err(_) => {
                    error!(
                        "dedicated Wayland connection setup for ext_idle_notifier_v1 did not complete within {IDLE_NOTIFY_SETUP_TIMEOUT:?} (possible compositor stall); notify disabled for this run"
                    );
                }
            }
        });

        controller
    }

    /// Inhibitor state for the initial `Capabilities::start` push; without it quiet machines read
    /// `nil` forever (ADR-0076).
    pub fn snapshot(&self) -> IdleState {
        self.published.lock().expect("mutex poisoned").merged()
    }

    /// Registers queued thresholds oldest first. Drain under the queue lock, then register outside
    /// it: `register_threshold` takes the same lock to requeue when notify is still inert.
    fn replay_pending_registrations(&self) {
        let queued: Vec<(u32, u64)> = std::mem::take(&mut *self.pending.lock().expect("mutex poisoned"));
        if queued.is_empty() {
            return;
        }
        debug!("notify is live; registering {} threshold(s) that arrived before it was", queued.len());
        for (generation_id, sec) in queued {
            self.register_threshold(generation_id, sec);
        }
    }

    /// Supervisor half of `idle:register_threshold(sec, on_idle, on_resume)` (ADR-0032). Inert
    /// notify queues nothing here; live notify applies [`register_threshold_entry`] and creates a
    /// new listener if needed.
    pub fn register_threshold(&self, generation_id: u32, sec: u64) {
        let notify = self.notify.read().expect("rwlock poisoned");
        let NotifyState::Live(live) = &*notify else {
            // Inert means degraded or still setting up. Queue entries in either case; a failed
            // setup leaves a small `(u32, u64)` queue rather than dropping every boot registration.
            self.pending.lock().expect("mutex poisoned").push((generation_id, sec));
            debug!("register_threshold(generation {generation_id}, {sec}s) queued: notify is not live yet");
            return;
        };

        let created_new_listener = {
            let mut registry = live.registry.lock().expect("mutex poisoned");
            register_threshold_entry(&mut registry.fanout, generation_id, sec)
        };

        if created_new_listener {
            let duration = Duration::from_secs(sec);
            let timeout_ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
            let gated = ListenerId { duration, respects_inhibitors: true };
            let notification = live.notifier.get_idle_notification(timeout_ms, &live.seat, &live.queue_handle, gated);
            live.registry.lock().expect("mutex poisoned").listeners.insert(gated, notification);

            // The twin the compositor may not withhold. It detects withheld idle notifications
            // and distinguishes input from other regular-listener activity (ADR-0160, ADR-0299).
            // Version 1 has no such request and reports logind inhibition only.
            if live.notifier.version() >= 2 {
                let input = ListenerId { duration, respects_inhibitors: false };
                let notification =
                    live.notifier.get_input_idle_notification(timeout_ms, &live.seat, &live.queue_handle, input);
                live.registry.lock().expect("mutex poisoned").listeners.insert(input, notification);
            }

            if let Err(err) = live.connection.flush() {
                error!("failed to flush the get_idle_notification request for {sec}s: {err}");
            }
        }
    }

    /// Supervisor half of `idle:cancel_threshold(handle)` (ADR-0232), sent only for the last
    /// callback at `sec`. Destroys the listener nothing feeds any more, unlike a reload, which
    /// keeps it for the registration landing behind it (ADR-0159).
    pub fn cancel_threshold(&self, generation_id: u32, sec: u64) {
        self.pending.lock().expect("mutex poisoned").retain(|&queued| queued != (generation_id, sec));
        let notify = self.notify.read().expect("rwlock poisoned");
        let NotifyState::Live(live) = &*notify else { return };
        {
            let mut registry = live.registry.lock().expect("mutex poisoned");
            let registry = &mut *registry;
            cancel_threshold_entry(&mut registry.fanout, generation_id, sec);
            for listener in take_unused_listeners(&mut registry.fanout, &mut registry.listeners) {
                listener.destroy();
            }
        }
        self.gate.lock().expect("mutex poisoned").forget_threshold(generation_id, sec);
    }

    /// `idle:inhibit(reason)` (ADR-0032): refcount decision, `Inhibit` call, and
    /// `fd`/count write stay under one `state` lock. Releasing it allows a stale zero during the
    /// call (leak) or lets an inhibit/release pair write `fd` out of order (clobber).
    pub async fn inhibit(&self, generation_id: u32, reason: &str) {
        let mut state = self.inhibit.state.lock().await;
        apply_inhibit(&mut state.counts, generation_id);
        open_shared_fd(&self.inhibit, &mut state, generation_id, reason).await;
    }

    /// `idle:release_inhibit()` (ADR-0032): refcount decision and `fd` clear use the same held
    /// `state` lock as [`IdleController::inhibit`]. Dropping `OwnedFd` closes the logind lock.
    pub async fn release_inhibit(&self, generation_id: u32) {
        let mut state = self.inhibit.state.lock().await;
        if apply_release_inhibit(&mut state.counts, generation_id) {
            state.fd = None;
        }
    }

    /// Notify half alone (ADR-0158). Drops the generation's threshold entries because its Renderer
    /// just dropped the callbacks they feed, and is about to register what the new tree asks for.
    /// The Renderer's own socket orders that, so the fresh registrations land behind this one.
    ///
    /// Leaves inhibit counts alone. The VM lives through an in-place reload, so a config's
    /// `state(...)` record of its own hold survives with it. Zeroing the count here would drop the
    /// logind fd while the config still believed it held one, and nothing would retake it.
    pub fn reset_thresholds(&self, generation_id: u32) {
        // Remove queued registrations first: a reload replaced the tree owning their callbacks
        // and must not replay them later (ADR-0139).
        self.pending
            .lock()
            .expect("mutex poisoned")
            .retain(|&(queued_generation, _)| queued_generation != generation_id);
        let notify = self.notify.read().expect("rwlock poisoned");
        if let NotifyState::Live(live) = &*notify {
            cleanup_generation_thresholds(&mut live.registry.lock().expect("mutex poisoned").fanout, generation_id);
        }
    }

    /// Notify and inhibit halves of `reset_registrations` (ADR-0006/ADR-0032): everything
    /// `generation_id` owned, for a generation that is gone. Closes the shared fd if it was the
    /// last holder. Uses the same `state` lock as inhibit/release, serializing reload races.
    pub async fn reset_registrations(&self, generation_id: u32) {
        self.reset_thresholds(generation_id);
        // Destroying listeners is reap-only: see `take_unused_listeners`.
        let mut listening = true;
        if let NotifyState::Live(live) = &*self.notify.read().expect("rwlock poisoned") {
            let mut registry = live.registry.lock().expect("mutex poisoned");
            let registry = &mut *registry;
            for listener in take_unused_listeners(&mut registry.fanout, &mut registry.listeners) {
                listener.destroy();
            }
            listening = !registry.listeners.is_empty();
        }
        // With none left, no raw event can arrive and the forwarder's sweep never runs, so a
        // `true` published before the reap would stand for the session. Nothing is watching, which
        // is not the same evidence as an inhibitor, but it is the answer that does not strand a
        // config reporting the compositor as holding the session awake.
        if !listening
            && let Some(next) = self.published.lock().expect("mutex poisoned").set_wayland_inhibited(Some(false))
        {
            let _ = self.state_tx.send(next);
        }
        // Also not in `reset_thresholds`: an in-place reload keeps the generation id and its
        // listeners, and the compositor never resends `idled`, so the gate's entries still belong
        // to it.
        self.gate.lock().expect("mutex poisoned").forget(generation_id);

        let mut state = self.inhibit.state.lock().await;
        if cleanup_generation_inhibit(&mut state.counts, generation_id) {
            state.fd = None;
        }
    }
}

/// The one logind fd every holder for `target.what` shares, taken while none is held. The caller
/// holds `state` for the reasons [`IdleController::inhibit`] gives. A login1 proxy-build failure is
/// a silent no-op (built fresh, not cached; see [`LiveInhibit::system_bus`]). For `idle`, a failed
/// `Inhibit` call keeps its count so a later release still matches and the next hold retries;
/// `PowerManagement` rolls back on failure.
pub(super) async fn open_shared_fd(target: &LiveInhibit, state: &mut InhibitState, holder: u32, reason: &str) {
    if state.fd.is_some() {
        return;
    }
    let proxy = match Login1ManagerProxy::new(&target.system_bus).await {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!("inhibit(holder {holder}, {reason:?}) failed to build the login1 Manager proxy: {err}");
            return;
        }
    };
    match proxy.inhibit(target.what, INHIBIT_WHO, reason, INHIBIT_MODE).await {
        Ok(fd) => state.fd = Some(fd),
        Err(err) => warn!("Inhibit({:?}, {INHIBIT_WHO:?}, {reason:?}, {INHIBIT_MODE:?}) failed: {err}", target.what),
    }
}

/// Follows `Manager.BlockInhibited` and keeps [`IdleGate`] in step (ADR-0139).
///
/// Watches the property rather than polling `ListInhibitors`: logind emits `PropertiesChanged`,
/// so any `systemd-inhibit --what=idle` reaches the gate in one round trip and costs nothing idle.
/// `sleep` blocks ride the same watch and publish as `sleep_inhibited`/`sleep_inhibitors` without
/// closing the idle gate.
///
/// Subscribing first avoids missing changes; the first loop iteration reads the initial state so
/// startup inhibitors are not missed. Failure degrades to a permanently open gate, logged once.
async fn watch_idle_inhibitors(
    system_bus: zbus::Connection,
    gate: Arc<std::sync::Mutex<IdleGate>>,
    events_tx: UnboundedSender<shared::IdleEvent>,
    state_tx: UnboundedSender<IdleState>,
    published: Arc<std::sync::Mutex<PublishedIdle>>,
) {
    let proxy = match Login1ManagerProxy::new(&system_bus).await {
        Ok(proxy) => proxy,
        Err(err) => {
            error!("cannot reach logind to watch idle inhibitors; nothing will hold off idle actions: {err}");
            return;
        }
    };
    let mut changes = proxy.receive_block_inhibited_changed().await;
    let mut run_now = true;
    while run_now || futures_util::StreamExt::next(&mut changes).await.is_some() {
        run_now = false;
        // Read through the proxy: zbus caches the property and avoids naming the stream item's
        // borrowed type.
        let Ok(what) = proxy.block_inhibited().await else { continue };
        let blocked = blocks(&what, "idle");
        let sleep_blocked = blocks(&what, "sleep");
        // `None` means the same idle answer as before. `BlockInhibited` changes for every kind of
        // inhibitor, while the list can change without the answer moving (mpv releases while
        // Firefox still holds one), so publish state on every change but gate on transitions.
        {
            // Match the forwarder's lock-through-send ordering across the two producer tasks.
            let mut gate = gate.lock().expect("mutex poisoned");
            if let Some(owed) = gate.set_blocked(blocked) {
                if blocked {
                    info!("logind reports an idle inhibitor ({what}); threshold events are held until it is released");
                } else {
                    info!("no idle inhibitor is held any more; threshold events resume");
                }
                for event in owed {
                    if events_tx.send(event).is_err() {
                        return;
                    }
                }
            }
        }

        // One `ListInhibitors` per change, never a timer: ADR-0139 rejected polling, and the
        // property edge provides one call to hang it off. Skip it when neither idle nor sleep is
        // blocked. A newer property edge cancels an in-flight list call so a slow reply (such as
        // on a sleep-only change) never stalls an idle gate transition behind it.
        let (inhibitors, sleep_inhibitors) = if blocked || sleep_blocked {
            let fetch = proxy.list_inhibitors();
            tokio::pin!(fetch);
            let rows = tokio::select! {
                biased;
                change = futures_util::StreamExt::next(&mut changes) => {
                    if change.is_none() {
                        return;
                    }
                    run_now = true;
                    continue;
                }
                res = &mut fetch => res.unwrap_or_default(),
            };
            let idle = if blocked { foreign_blockers(&rows, "idle") } else { Vec::new() };
            let sleep = if sleep_blocked { foreign_blockers(&rows, "sleep") } else { Vec::new() };
            (idle, sleep)
        } else {
            (Vec::new(), Vec::new())
        };
        // Held across the send; see the matching comment in `spawn_idle_event_forwarder`.
        let mut published = published.lock().expect("mutex poisoned");
        let Some(next_state) = published.set_logind(blocked, inhibitors, sleep_blocked, sleep_inhibitors) else {
            continue;
        };
        if state_tx.send(next_state).is_err() {
            return;
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::super::inhibit::tests::{HeldLogind, StubLogin1Manager};
    use super::*;
    use crate::capabilities::test_support::{envelope, p2p_pair_serving, poll_until, within};
    use serde_json::json;

    pub(in super::super) type Inhibited = ((String, String, String, String), std::io::PipeReader);

    /// An active seat gives no compositor answer. Keep the last gate value but stop presenting its
    /// unnamed holder as current; the next idle period can confirm or clear it.
    #[test]
    fn no_evidence_keeps_the_last_compositor_answer_and_only_an_explicit_one_replaces_it() {
        let mut published = PublishedIdle::default();
        published.set_wayland_inhibited(Some(true));
        assert!(!published.merged().compositor_hold_stale);
        assert_eq!(published.merged().inhibitors.len(), 1);

        let stale = published.set_wayland_inhibited(None).expect("the Renderer must hear that the hold is unconfirmed");
        assert!(stale.inhibited, "no evidence must leave the held answer standing");
        assert!(stale.compositor_hold_stale);
        assert!(stale.inhibitors.is_empty(), "an unconfirmed holder must not be named as current");

        published.set_wayland_inhibited(Some(false));
        assert!(!published.merged().inhibited, "a reap with no listeners left must be able to clear it");
        assert!(!published.merged().compositor_hold_stale);
    }

    /// The inhibit half on a stub logind, with notify left inert: `new` would dial the session's
    /// compositor. The connection keeps logind answering.
    pub(in super::super) async fn controller() -> (IdleController, UnboundedReceiver<Inhibited>, zbus::Connection) {
        let (locks, taken) = unbounded_channel();
        let (system_bus, logind) =
            p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/login1", StubLogin1Manager { calls: locks })).await;
        let controller = IdleController {
            pending: Arc::default(),
            notify: Arc::new(RwLock::new(NotifyState::Inert)),
            inhibit: Arc::new(LiveInhibit::new(system_bus.clone(), INHIBIT_WHAT)),
            sleep_inhibit: Arc::new(LiveInhibit::new(system_bus, SLEEP_INHIBIT_WHAT)),
            published: Arc::default(),
            gate: Arc::default(),
            state_tx: unbounded_channel().0,
            bus_lease: None,
        };
        (controller, taken, logind)
    }

    pub(in super::super) async fn taken(locks: &mut UnboundedReceiver<Inhibited>) -> std::io::PipeReader {
        within(locks.recv()).await.expect("logind was asked for a lock").1
    }

    /// The lock's read end reads EOF once logind sees the lock released.
    pub(in super::super) async fn released(lock: std::io::PipeReader) {
        let mut lock = tokio::net::unix::pipe::Receiver::from_owned_fd(lock.into()).expect("a pipe's read end");
        assert_eq!(within(lock.read(&mut [0])).await.expect("read the lock"), 0, "the lock is still held");
    }

    #[tokio::test]
    async fn a_config_releasing_or_leaving_with_the_last_hold_closes_the_logind_lock() {
        let (controller, mut locks, _logind) = controller().await;

        controller.inhibit(1, "presenting").await;
        let lock = taken(&mut locks).await;
        assert!(controller.inhibit.state.lock().await.fd.is_some());
        controller.release_inhibit(1).await;
        released(lock).await;

        controller.inhibit(2, "presenting").await;
        let lock = taken(&mut locks).await;
        controller.reset_registrations(2).await;
        released(lock).await;
    }

    /// Each arm acts for the envelope's generation. Notify is inert, so the threshold arms show in
    /// the queue a live notifier replays.
    #[tokio::test]
    async fn dispatch_acts_for_the_envelopes_generation() {
        let (controller, mut locks, _logind) = controller().await;
        let send = |generation_id, action: &str, arguments: serde_json::Value| {
            let mut envelope =
                envelope(shared::Capability::Idle, action, arguments.as_array().expect("a list").clone());
            envelope.params.generation_id = generation_id;
            super::super::dispatch(&controller, &envelope);
        };
        let queued = || controller.pending.lock().expect("mutex poisoned").clone();

        send(7, "register", json!([20]));
        send(7, "register", json!([60]));
        send(8, "register", json!([20]));
        assert_eq!(queued(), [(7, 20), (7, 60), (8, 20)]);
        send(7, "cancel", json!([20]));
        assert_eq!(queued(), [(7, 60), (8, 20)], "only generation 7's 20s goes");
        send(8, "forget_thresholds", json!([]));
        assert_eq!(queued(), [(7, 60)], "only generation 8's go");

        send(7, "inhibit", json!(["presenting"]));
        let ((_, _, why, _), lock) = within(locks.recv()).await.expect("logind was asked for a lock");
        assert_eq!(why, "presenting");
        // The spawned arms report nothing back; each is done once its generation's count moves.
        let state = &controller.inhibit.state;
        let counted = |generation, count| {
            poll_until(move || (state.try_lock().ok()?.counts.get(&generation) == Some(&count)).then_some(()))
        };
        counted(7, 1).await;
        send(8, "inhibit", json!(["presenting too"]));
        counted(8, 1).await;
        send(8, "release_inhibit", json!([]));
        counted(8, 0).await;
        assert_eq!(state.lock().await.counts.get(&7), Some(&1), "generation 8 released 7's hold");
        assert!(state.lock().await.fd.is_some(), "generation 7 still holds the lock");
        send(7, "release_inhibit", json!([]));
        released(lock).await;
    }

    /// Idle and sleep inhibitors held at startup and released in turn reach the gate and published
    /// state: releasing idle re-announces the threshold while keeping sleep published, an idle-only
    /// state clears sleep even if the list still has a sleep row, a `ListInhibitors` error still
    /// publishes `sleep_inhibited`, and clearing both returns default without touching the gate.
    #[tokio::test]
    async fn logind_inhibitors_held_at_startup_and_released_later_reach_the_gate_and_the_published_state() {
        let (system_bus, logind) =
            p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/login1", HeldLogind::new("sleep:idle"))).await;
        let event = |state| shared::IdleEvent { generation_id: 1, threshold_sec: 30, state };
        let gate = Arc::new(std::sync::Mutex::new(IdleGate::default()));
        gate.lock().expect("mutex poisoned").observe(event(shared::IdleState::Idled));
        let (events_tx, mut events) = unbounded_channel();
        let (state_tx, mut pushed) = unbounded_channel();
        tokio::spawn(watch_idle_inhibitors(system_bus, gate, events_tx, state_tx, Arc::default()));

        let inhibitor = shared::IdleState::Resumed { cause: shared::ResumeCause::Inhibitor };
        assert_eq!(within(events.recv()).await, Some(event(inhibitor)));
        let mpv = super::super::state::IdleInhibitor { who: "mpv".into(), why: "Playing video".into() };
        let backup = super::super::state::IdleInhibitor { who: "backup".into(), why: "Backing up".into() };
        let both = IdleState {
            inhibited: true,
            compositor_hold_stale: false,
            inhibitors: vec![mpv.clone()],
            sleep_inhibited: true,
            sleep_inhibitors: vec![backup.clone()],
        };
        assert_eq!(within(pushed.recv()).await, Some(both));

        let manager =
            logind.object_server().interface::<_, HeldLogind>("/org/freedesktop/login1").await.expect("served");
        let emit = |next: &'static str, fail_list: bool| {
            let manager = manager.clone();
            async move {
                let guard = manager.get().await;
                *guard.blocked.lock().expect("mutex poisoned") = next;
                guard.fail_list.store(fail_list, std::sync::atomic::Ordering::Relaxed);
                guard.block_inhibited_changed(manager.signal_emitter()).await.expect("emit the change");
            }
        };

        emit("sleep", false).await;
        assert_eq!(within(events.recv()).await, Some(event(shared::IdleState::Idled)));
        let sleep_only = IdleState { sleep_inhibited: true, sleep_inhibitors: vec![backup], ..IdleState::default() };
        assert_eq!(within(pushed.recv()).await, Some(sleep_only));

        emit("idle", false).await;
        assert_eq!(within(events.recv()).await, Some(event(inhibitor)));
        let idle_only = IdleState { inhibited: true, inhibitors: vec![mpv], ..IdleState::default() };
        assert_eq!(within(pushed.recv()).await, Some(idle_only));

        emit("sleep", true).await;
        assert_eq!(within(events.recv()).await, Some(event(shared::IdleState::Idled)));
        assert_eq!(within(pushed.recv()).await, Some(IdleState { sleep_inhibited: true, ..IdleState::default() }));

        emit("", false).await;
        assert_eq!(within(pushed.recv()).await, Some(IdleState::default()));
        assert!(events.try_recv().is_err(), "releasing a sleep inhibitor must not emit threshold events");
    }

    /// A newer `BlockInhibited` change cancels a slow `ListInhibitors` call so an idle inhibitor
    /// arriving while a sleep-only roster fetch is in flight still closes the gate at once.
    #[tokio::test]
    async fn an_idle_inhibitor_closes_the_gate_while_a_sleep_list_call_is_in_flight() {
        let (list_called_tx, mut list_called) = unbounded_channel();
        let (open_list, list_gate) = tokio::sync::watch::channel(false);
        let mut logind_stub = HeldLogind::new("sleep");
        logind_stub.list_called = Some(list_called_tx);
        logind_stub.list_gate = Some(list_gate);
        let (system_bus, logind) = p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/login1", logind_stub)).await;

        let event = |state| shared::IdleEvent { generation_id: 1, threshold_sec: 30, state };
        let gate = Arc::new(std::sync::Mutex::new(IdleGate::default()));
        gate.lock().expect("mutex poisoned").observe(event(shared::IdleState::Idled));
        let (events_tx, mut events) = unbounded_channel();
        let (state_tx, mut pushed) = unbounded_channel();
        tokio::spawn(watch_idle_inhibitors(system_bus, gate, events_tx, state_tx, Arc::default()));

        within(list_called.recv()).await.expect("sleep edge started ListInhibitors");

        let manager =
            logind.object_server().interface::<_, HeldLogind>("/org/freedesktop/login1").await.expect("served");
        {
            let guard = manager.get().await;
            *guard.blocked.lock().expect("mutex poisoned") = "sleep:idle";
            guard.block_inhibited_changed(manager.signal_emitter()).await.expect("emit sleep:idle");
        }

        // The idle gate closes before `open_list` releases the stalled `ListInhibitors` call.
        let inhibitor = shared::IdleState::Resumed { cause: shared::ResumeCause::Inhibitor };
        assert_eq!(within(events.recv()).await, Some(event(inhibitor)));

        open_list.send_replace(true);
        let state = within(pushed.recv()).await.expect("published once the superseded list finishes");
        assert!(state.inhibited && state.sleep_inhibited);
    }
}

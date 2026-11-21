//! [`IdleController`]: `mantle.idle`'s write dispatcher and state owner for notify and inhibit.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use shared::{debug, error, info, warn};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::Proxy;
use zbus::object_server::SignalEmitter;

use super::gate::{IdleGate, blocks};
use super::inhibit::{
    BUS_HOLDER, INHIBIT_MODE, INHIBIT_WHAT, INHIBIT_WHO, InhibitState, LiveInhibit, Login1ManagerProxy,
    POWER_MANAGEMENT_BUS_NAME, POWER_MANAGEMENT_OBJECT_PATH, PowerManagementInhibit, SCREENSAVER_BUS_NAME,
    SCREENSAVER_OBJECT_PATHS, SLEEP_INHIBIT_WHAT, ScreenSaver, apply_bus_inhibit, apply_bus_release, apply_inhibit,
    apply_release_inhibit, cleanup_generation_inhibit, drop_bus_peer,
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
    /// Reported before logind answers, so a config sees the holder in the same push that stops its countdown.
    fn set_bus_holds(&mut self, kind: BusInhibit, holds: Vec<super::state::IdleInhibitor>) -> Option<IdleState> {
        match kind {
            BusInhibit::ScreenSaver => self.screensaver = holds,
            BusInhibit::PowerManagement => self.power_management = holds,
        }
        self.settle()
    }

    fn set_logind(
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

/// Which inbound session-bus inhibit service is acting.
#[derive(Clone, Copy)]
pub(crate) enum BusInhibit {
    ScreenSaver,
    PowerManagement,
}

/// Owns the session-bus connection and departure-watcher task while `org.freedesktop.ScreenSaver`
/// or `org.freedesktop.PowerManagement` is claimed. Dropping the last owner aborts the watcher
/// (whose `NameOwnerChangedStream` holds a strong connection handle) and drops the connection,
/// releasing the bus name(s) cleanly.
struct BusLease {
    _connection: zbus::Connection,
    watcher: Option<tokio::task::AbortHandle>,
}

impl Drop for BusLease {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
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
    inhibit: Arc<LiveInhibit>,
    pub(crate) sleep_inhibit: Arc<LiveInhibit>,
    /// The last state [`watch_idle_inhibitors`] published, so `Capabilities::start` returns the
    /// current answer instead of `nil` until the next inhibitor (ADR-0141).
    published: Arc<std::sync::Mutex<PublishedIdle>>,
    gate: Arc<std::sync::Mutex<IdleGate>>,
    /// Also held here, not just handed to the forwarder: reaping the last listener has to answer
    /// for the compositor itself, and no raw event will arrive to let the forwarder do it.
    state_tx: UnboundedSender<IdleState>,
    /// Live session-bus lease while this process owns `ScreenSaver` or `PowerManagement`. Kept
    /// here so a failed or ended departure watch cannot drop the bus names, and dropping the
    /// controller aborts the watcher. Left `None` in clones handed to the object server and peer
    /// watcher to avoid an `Arc` cycle.
    bus_lease: Option<Arc<BusLease>>,
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

    fn bus_target(&self, kind: BusInhibit) -> (&LiveInhibit, &'static str) {
        match kind {
            BusInhibit::ScreenSaver => (&self.inhibit, "ScreenSaver"),
            BusInhibit::PowerManagement => (&self.sleep_inhibit, "PowerManagement"),
        }
    }

    /// Session-bus `Inhibit`: `ScreenSaver` takes the shared `idle` fd (ADR-0231);
    /// `PowerManagement` takes a shared `sleep` fd, which never closes the idle gate (ADR-0412).
    pub(crate) async fn bus_inhibit(
        &self,
        kind: BusInhibit,
        peer: String,
        who: String,
        why: String,
        connection: Option<&zbus::Connection>,
        emitter: Option<&SignalEmitter<'_>>,
    ) -> zbus::fdo::Result<u32> {
        let (target, name) = self.bus_target(kind);
        let (cookie, live) = {
            let mut state = target.state.lock().await;
            // Verify the sender still owns its unique name under the lock shared with
            // `watch_bus_peers`, so a client that exits before this handler runs cannot leak an fd.
            if let Some(connection) = connection
                && !peer_alive(connection, &peer).await
            {
                return Err(zbus::fdo::Error::Failed("sender already left the bus".into()));
            }
            let was_empty = state.holds.is_empty();
            let cookie = apply_bus_inhibit(&mut state, name, peer.clone(), super::state::IdleInhibitor { who, why })
                .ok_or_else(|| zbus::fdo::Error::LimitsExceeded(format!("too many {name} holds")))?;
            // As recorded, so logind gets the capped reason too.
            let reason = state.holds[&cookie].1.why.clone();
            open_shared_fd(target, &mut state, BUS_HOLDER, &reason).await;
            if matches!(kind, BusInhibit::PowerManagement) && state.fd.is_none() {
                apply_bus_release(&mut state, cookie, &peer);
                return Err(zbus::fdo::Error::Failed("logind refused sleep inhibit".into()));
            }
            if was_empty && let Some(emitter) = emitter {
                let _ = PowerManagementInhibit::has_inhibit_changed(emitter, true).await;
            }
            // Publish under `state` so concurrent inhibit/release/departure pushes stay ordered.
            let holds = bus_holds(&state);
            let live = holds.len();
            self.publish_bus_holds(kind, holds);
            (cookie, live)
        };
        // Debug, not info: a browser retakes its hold on every play, and the gate already says
        // once, at info, that something holds the session awake.
        debug!("{name}.Inhibit -> cookie {cookie}; {live} hold(s) now live");
        Ok(cookie)
    }

    /// Session-bus `UnInhibit`; false for a cookie nothing holds or held by another `peer`.
    pub(crate) async fn bus_release(
        &self,
        kind: BusInhibit,
        cookie: u32,
        peer: &str,
        emitter: Option<&SignalEmitter<'_>>,
    ) -> bool {
        let live = self.drop_bus_holds(kind, emitter, |s| apply_bus_release(s, cookie, peer)).await;
        if let Some(live) = live {
            debug!("{}.UnInhibit(cookie {cookie}); {live} hold(s) still live", self.bus_target(kind).1);
        }
        live.is_some()
    }

    async fn drop_bus_holds(
        &self,
        kind: BusInhibit,
        emitter: Option<&SignalEmitter<'_>>,
        drop_fn: impl FnOnce(&mut InhibitState) -> Option<bool>,
    ) -> Option<usize> {
        let (target, _) = self.bus_target(kind);
        let mut state = target.state.lock().await;
        if drop_fn(&mut state)? {
            state.fd = None;
        }
        if state.holds.is_empty()
            && let Some(emitter) = emitter
        {
            let _ = PowerManagementInhibit::has_inhibit_changed(emitter, false).await;
        }
        let holds = bus_holds(&state);
        let live = holds.len();
        self.publish_bus_holds(kind, holds);
        Some(live)
    }

    /// Held across the send; see the matching comment in `spawn_idle_event_forwarder`.
    fn publish_bus_holds(&self, kind: BusInhibit, holds: Vec<super::state::IdleInhibitor>) {
        let mut published = self.published.lock().expect("mutex poisoned");
        if let Some(next) = published.set_bus_holds(kind, holds) {
            let _ = self.state_tx.send(next);
        }
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
    /// Clone without the session-bus lease, for objects and tasks owned by the lease itself so
    /// neither can form an `Arc` cycle back to the bus connection.
    fn without_lease(&self) -> Self {
        Self { bus_lease: None, ..self.clone() }
    }

    /// Claims `org.freedesktop.ScreenSaver` (ADR-0231) and `org.freedesktop.PowerManagement` on
    /// `session_bus`, retaining the connection and one departure-watcher handle while either name
    /// is owned. Subscribes to departures first so no client can take a hold and leave unseen
    /// while `add_match` is still in flight.
    async fn export_session_services(&mut self, session_bus: zbus::Connection) {
        // Filtered by the bus on `new_owner == ""`, not here: every application start and exit on
        // the session bus changes some name, and none of those need to wake this shell.
        let lost = "a crashed inhibit client's hold would outlive it";
        let departures = match zbus::fdo::DBusProxy::new(&session_bus).await {
            Ok(proxy) => crate::capabilities::name_departures(&proxy, lost).await,
            Err(err) => {
                warn!("cannot watch for departing inhibit clients: {err}; {lost}");
                None
            }
        };
        self.export_session_services_with(session_bus, departures).await;
    }

    async fn export_session_services_with(
        &mut self,
        session_bus: zbus::Connection,
        departures: Option<zbus::fdo::NameOwnerChangedStream>,
    ) {
        let controller = self.without_lease();
        // Start draining `departures` before claiming names so a burst of bus exits cannot fill
        // zbus's 64-message socket-reader queue while `claim_service` awaits `RequestName`.
        let watcher = departures
            .map(|names| tokio::spawn(watch_bus_peers(names, controller.clone(), session_bus.clone())).abort_handle());
        let lease = BusLease { _connection: session_bus.clone(), watcher };
        let has_screensaver = claim_service(&session_bus, SCREENSAVER_BUS_NAME, &SCREENSAVER_OBJECT_PATHS, || {
            ScreenSaver { controller: controller.clone() }
        })
        .await;
        let has_power_management =
            claim_service(&session_bus, POWER_MANAGEMENT_BUS_NAME, &[POWER_MANAGEMENT_OBJECT_PATH], || {
                PowerManagementInhibit { controller: controller.clone() }
            })
            .await;
        if has_screensaver || has_power_management {
            self.bus_lease = Some(Arc::new(lease));
        }
    }
}

/// Checks whether `peer` still owns its unique name on `connection`.
async fn peer_alive(connection: &zbus::Connection, peer: &str) -> bool {
    let Ok(name) = zbus::names::BusName::try_from(peer) else { return false };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(connection).await else { return false };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// The one logind fd every holder for `target.what` shares, taken while none is held. The caller
/// holds `state` for the reasons [`IdleController::inhibit`] gives. A login1 proxy-build failure is
/// a silent no-op (built fresh, not cached; see [`LiveInhibit::system_bus`]). For `idle`, a failed
/// `Inhibit` call keeps its count so a later release still matches and the next hold retries;
/// `PowerManagement` rolls back on failure.
async fn open_shared_fd(target: &LiveInhibit, state: &mut InhibitState, holder: u32, reason: &str) {
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

/// Exports `paths` before claiming `name` so any call routed to the new owner finds the object.
/// `DoNotQueue` lets an existing daemon keep the name rather than taking it mid-session for holds
/// this shell never saw; removes exports on failure so a shared connection serves no unowned object.
async fn claim_service<I: zbus::object_server::Interface>(
    session_bus: &zbus::Connection,
    name: &str,
    paths: &[&str],
    make: impl Fn() -> I,
) -> bool {
    for &path in paths {
        if let Err(err) = session_bus.object_server().at(path, make()).await {
            warn!("failed to export {name} at {path}: {err}");
        }
    }
    match session_bus.request_name_with_flags(name, zbus::fdo::RequestNameFlags::DoNotQueue.into()).await {
        // A fresh connection with `DoNotQueue` gets only `PrimaryOwner`; zbus turns `Exists` into `NameTaken`.
        Ok(_) => {
            debug!("holding {name}");
            true
        }
        Err(err) => {
            for &path in paths {
                let _ = session_bus.object_server().remove::<I, _>(path).await;
            }
            match err {
                zbus::Error::NameTaken => warn!("{name} is taken; another daemon answers its clients"),
                err => warn!("RequestName({name}) failed: {err}; no client's inhibit reaches this shell"),
            }
            false
        }
    }
}

fn bus_holds(state: &InhibitState) -> Vec<super::state::IdleInhibitor> {
    state.holds.values().map(|(_, hold)| hold.clone()).collect()
}

/// Releases a departed client's holds across both services (ADR-0231). Drains the D-Bus stream
/// continuously into an unbounded queue so waiting on a hold mutex never backpressures zbus's
/// 64-message socket reader and stalls `NameHasOwner` replies on the same connection.
///
/// ponytail: the queue is unbounded; it only grows by the departures that arrive while one hold
/// call holds the mutex, at most zbus's 25 s call timeout. Upgrade: bound it and, on overflow,
/// reconcile every holder with `NameHasOwner`.
async fn watch_bus_peers(
    mut names: zbus::fdo::NameOwnerChangedStream,
    controller: IdleController,
    connection: zbus::Connection,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let drain = async {
        while let Some(signal) = futures_util::StreamExt::next(&mut names).await {
            if let Ok(args) = signal.args()
                && tx.send(args.name.to_string()).is_err()
            {
                break;
            }
        }
        drop(tx);
    };
    let apply = async {
        let pm_emitter = SignalEmitter::new(&connection, POWER_MANAGEMENT_OBJECT_PATH).ok();
        while let Some(departed) = rx.recv().await {
            for (kind, emitter) in [(BusInhibit::ScreenSaver, None), (BusInhibit::PowerManagement, pm_emitter.as_ref())]
            {
                if controller.drop_bus_holds(kind, emitter, |s| drop_bus_peer(s, &departed)).await.is_some() {
                    debug!(
                        "{departed} left the bus holding a {} inhibitor; released it",
                        controller.bus_target(kind).1
                    );
                }
            }
        }
    };
    tokio::join!(drain, apply);
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
mod tests {
    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::super::inhibit::tests::{HeldLogind, REFUSED, StubLogin1Manager};
    use super::BusInhibit::*;
    use super::*;
    use crate::capabilities::test_support::{envelope, p2p_pair_serving, poll_until, private_bus, within};
    use serde_json::json;

    type Inhibited = ((String, String, String, String), std::io::PipeReader);

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
    async fn controller() -> (IdleController, UnboundedReceiver<Inhibited>, zbus::Connection) {
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

    async fn taken(locks: &mut UnboundedReceiver<Inhibited>) -> std::io::PipeReader {
        within(locks.recv()).await.expect("logind was asked for a lock").1
    }

    /// The lock's read end reads EOF once logind sees the lock released.
    async fn released(lock: std::io::PipeReader) {
        let mut lock = tokio::net::unix::pipe::Receiver::from_owned_fd(lock.into()).expect("a pipe's read end");
        assert_eq!(within(lock.read(&mut [0])).await.expect("read the lock"), 0, "the lock is still held");
    }

    async fn screensaver(client: &zbus::Connection) -> zbus::Proxy<'static> {
        zbus::Proxy::new(client, SCREENSAVER_BUS_NAME, SCREENSAVER_OBJECT_PATHS[0], SCREENSAVER_BUS_NAME)
            .await
            .expect("bind the exported ScreenSaver")
    }

    async fn pm_proxy(client: &zbus::Connection, dest: &str) -> zbus::Proxy<'static> {
        let dest = zbus::names::OwnedBusName::try_from(dest).unwrap();
        zbus::Proxy::new(client, dest, POWER_MANAGEMENT_OBJECT_PATH, "org.freedesktop.PowerManagement.Inhibit")
            .await
            .unwrap()
    }

    async fn next_bool(sig: &mut zbus::proxy::SignalStream<'_>) -> bool {
        within(futures_util::StreamExt::next(sig)).await.unwrap().body().deserialize().unwrap()
    }

    async fn wait_name_released(observer: &zbus::Connection, bus_name: &'static str) {
        let dbus = zbus::fdo::DBusProxy::new(observer).await.expect("bind org.freedesktop.DBus");
        let name = zbus::names::BusName::from_static_str(bus_name).expect("valid bus name");
        within(async {
            while dbus.name_has_owner(name.clone()).await.expect("NameHasOwner") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
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

    /// The whole inbound path on a bus: a client's `UnInhibit`, a client that leaves holding one,
    /// and dropping the controller releases the bus name by aborting the live departure watcher.
    #[tokio::test]
    async fn a_screensaver_client_releasing_or_leaving_closes_the_logind_lock() {
        let (mut controller, mut locks, _logind) = controller().await;
        let bus = private_bus().await;
        controller.export_session_services(bus.connection().await).await;
        assert!(controller.bus_lease.is_some(), "the bus name was free");
        let observer = bus.connection().await;
        let client = screensaver(&observer).await;

        let cookie: u32 = client.call("Inhibit", &("", "Playing video")).await.expect("Inhibit");
        let lock = taken(&mut locks).await;
        client.call::<_, _, ()>("UnInhibit", &cookie).await.expect("UnInhibit of a live cookie");
        released(lock).await;
        client.call::<_, _, ()>("UnInhibit", &cookie).await.expect_err("nothing holds that cookie any more");

        let leaver = bus.connection().await;
        let _: u32 = screensaver(&leaver).await.call("Inhibit", &("", "Playing video")).await.expect("Inhibit");
        let lock = taken(&mut locks).await;
        leaver.close().await.expect("leave the bus");
        released(lock).await;

        drop(controller);
        wait_name_released(&observer, SCREENSAVER_BUS_NAME).await;
    }

    /// Regression for #166: `org.freedesktop.ScreenSaver` must stay owned on the controller even
    /// when `name_departures` failed (`None`), and no clone handed to the object server keeps it
    /// past the controller (no `Arc` cycle).
    #[tokio::test]
    async fn screensaver_stays_owned_without_a_departure_watch() {
        let (mut controller, mut locks, _logind) = controller().await;
        let bus = private_bus().await;
        controller.export_session_services_with(bus.connection().await, None).await;
        assert!(controller.bus_lease.is_some(), "must keep the owned connection");
        assert!(controller.without_lease().bus_lease.is_none(), "without_lease must drop the lease");

        let observer = bus.connection().await;
        let client = screensaver(&observer).await;
        let cookie: u32 = client.call("Inhibit", &("", "Playing video")).await.expect("ScreenSaver still owned");
        let lock = taken(&mut locks).await;
        client.call::<_, _, ()>("UnInhibit", &cookie).await.expect("UnInhibit");
        released(lock).await;

        drop(controller);
        wait_name_released(&observer, SCREENSAVER_BUS_NAME).await;
    }

    /// A hold logind refused must neither leave the next holder without a lock nor, released
    /// later, close the lock that holder took.
    #[tokio::test]
    async fn a_hold_logind_refused_never_costs_the_next_holder_its_lock() {
        let (controller, mut locks, _logind) = controller().await;
        let refused =
            controller.bus_inhibit(ScreenSaver, ":1.1".into(), "".into(), REFUSED.into(), None, None).await.unwrap();
        controller
            .bus_inhibit(ScreenSaver, ":1.2".into(), "".into(), "Playing video".into(), None, None)
            .await
            .unwrap();
        let _lock = taken(&mut locks).await;
        assert!(controller.bus_release(ScreenSaver, refused, ":1.1", None).await);
        assert!(controller.inhibit.state.lock().await.fd.is_some(), "the refused hold closed the video's lock");
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

    /// A ScreenSaver hold reaches the pushed state and the start snapshot, and leaves both on release.
    #[tokio::test]
    async fn a_screensaver_hold_is_published_and_snapshotted_until_released() {
        let (controller, mut locks, _logind) = controller().await;
        let (state_tx, mut pushed) = unbounded_channel();
        let controller = IdleController { state_tx, ..controller };
        let video = super::super::state::IdleInhibitor { who: "firefox".into(), why: "Playing video".into() };

        let cookie =
            controller.bus_inhibit(ScreenSaver, ":1.1".into(), video.who.clone(), video.why.clone(), None, None).await;
        let _lock = taken(&mut locks).await;
        let held = IdleState { inhibited: true, inhibitors: vec![video], ..IdleState::default() };
        assert_eq!(within(pushed.recv()).await, Some(held.clone()));
        assert_eq!(controller.snapshot(), held);

        assert!(controller.bus_release(ScreenSaver, cookie.expect("a cookie"), ":1.1", None).await);
        assert_eq!(within(pushed.recv()).await, Some(IdleState::default()));
        assert_eq!(controller.snapshot(), IdleState::default());
    }

    /// PowerManagement.Inhibit: logind denial rolls back without signalling, sleep fd open/close,
    /// `HasInhibit`/`HasInhibitChanged` edges, peer departure (and late handler after exit),
    /// per-peer `LimitsExceeded`, and bus-name release on drop.
    #[tokio::test]
    async fn power_management_inhibit_holds_sleep_signals_changes_and_enforces_limits() {
        let (mut controller, mut locks, _logind) = controller().await;
        let bus = private_bus().await;
        controller.export_session_services(bus.connection().await).await;
        let observer = bus.connection().await;
        let client = pm_proxy(&observer, POWER_MANAGEMENT_BUS_NAME).await;
        let mut sig = client.receive_signal("HasInhibitChanged").await.unwrap();
        assert!(!client.call::<_, _, bool>("HasInhibit", &()).await.unwrap());
        client.call::<_, _, u32>("Inhibit", &("zen", REFUSED)).await.expect_err("logind denied");
        assert!(
            !client.call::<_, _, bool>("HasInhibit", &()).await.unwrap()
                && controller.snapshot() == IdleState::default()
        );

        let c1: u32 = client.call("Inhibit", &("zen", "Playing audio")).await.unwrap();
        let ((what, who, why, mode), lock) = within(locks.recv()).await.unwrap();
        assert_eq!((&*what, &*who, &*why, &*mode), ("sleep", "mantle", "Playing audio", "block"));
        assert!(next_bool(&mut sig).await && client.call::<_, _, bool>("HasInhibit", &()).await.unwrap());

        let c2: u32 = client.call("Inhibit", &("zen", "Video")).await.unwrap();
        client.call::<_, _, ()>("UnInhibit", &c1).await.unwrap();
        assert!(client.call::<_, _, bool>("HasInhibit", &()).await.unwrap());
        client.call::<_, _, ()>("UnInhibit", &c2).await.unwrap();
        released(lock).await;
        assert!(!next_bool(&mut sig).await && !client.call::<_, _, bool>("HasInhibit", &()).await.unwrap());
        client.call::<_, _, ()>("UnInhibit", &c2).await.expect_err("already released");

        let leaver = bus.connection().await;
        let departed = leaver.unique_name().unwrap().to_string();
        let _: u32 =
            pm_proxy(&leaver, POWER_MANAGEMENT_BUS_NAME).await.call("Inhibit", &("zen", "Audio")).await.unwrap();
        let lock = taken(&mut locks).await;
        assert!(next_bool(&mut sig).await);
        leaver.close().await.unwrap();
        released(lock).await;
        assert!(!next_bool(&mut sig).await);
        // A handler running after its sender already left must not leak an fd or hold.
        controller
            .bus_inhibit(PowerManagement, departed, "zen".into(), "Late".into(), Some(&observer), None)
            .await
            .expect_err("departed sender refused");
        assert!(locks.try_recv().is_err() && controller.snapshot() == IdleState::default());

        for _ in 0..super::super::inhibit::MAX_BUS_HOLDS_PER_PEER {
            let _: u32 = client.call("Inhibit", &("zen", "hold")).await.unwrap();
        }
        let err = client.call::<_, _, u32>("Inhibit", &("zen", "over")).await.unwrap_err();
        assert!(matches!(err, zbus::Error::MethodError(n, _, _) if n == "org.freedesktop.DBus.Error.LimitsExceeded"));

        // Over 64 departures while `sleep_inhibit` is locked must not stall `ScreenSaver.Inhibit`
        // (and its `NameHasOwner` call) on the shared connection.
        let hold_lock = controller.sleep_inhibit.state.lock().await;
        for _ in 0..70 {
            bus.connection().await.close().await.unwrap();
        }
        let ss_cookie: u32 = within(screensaver(&observer).await.call("Inhibit", &("", "Burst"))).await.unwrap();
        drop(hold_lock);
        screensaver(&observer).await.call::<_, _, ()>("UnInhibit", &ss_cookie).await.unwrap();
        drop(controller);
        wait_name_released(&observer, POWER_MANAGEMENT_BUS_NAME).await;
    }

    /// PowerManagement publishes into `sleep_inhibitors`/`sleep_inhibited`, merges cleanly with
    /// logind's `sleep` edge, and leaves `inhibited` untouched.
    #[tokio::test]
    async fn power_management_hold_publishes_sleep_without_closing_the_idle_gate() {
        let (controller, mut locks, _logind) = controller().await;
        let (state_tx, mut pushed) = unbounded_channel();
        let controller = IdleController { state_tx, ..controller };
        let audio = super::super::state::IdleInhibitor { who: "zen".into(), why: "Playing audio".into() };
        let cookie = controller
            .bus_inhibit(PowerManagement, ":1.1".into(), "zen".into(), "Playing audio".into(), None, None)
            .await
            .unwrap();
        let _lock = taken(&mut locks).await;
        let held = IdleState { sleep_inhibited: true, sleep_inhibitors: vec![audio], ..IdleState::default() };
        assert_eq!((within(pushed.recv()).await, controller.snapshot()), (Some(held.clone()), held));
        assert_eq!(controller.published.lock().unwrap().set_logind(false, vec![], true, vec![]), None);
        assert!(controller.bus_release(PowerManagement, cookie, ":1.1", None).await);
        assert_eq!(within(pushed.recv()).await, Some(IdleState { sleep_inhibited: true, ..IdleState::default() }));
        assert_eq!(
            controller.published.lock().unwrap().set_logind(false, vec![], false, vec![]),
            Some(IdleState::default())
        );
    }

    /// Taken `org.freedesktop.PowerManagement` stays with its owner and removes its export even
    /// while `ScreenSaver` succeeds; `PowerManagement` alone keeps a lease when `ScreenSaver` is
    /// taken; when both are taken, no lease is kept.
    #[tokio::test]
    async fn power_management_name_taken_does_not_export() {
        let (mut first, _locks, _logind) = controller().await;
        let bus = private_bus().await;
        let rival = bus.builder().name(POWER_MANAGEMENT_BUS_NAME).unwrap().build().await.unwrap();
        let conn = bus.connection().await;
        let unique = conn.unique_name().unwrap().to_string();
        first.export_session_services(conn).await;
        assert!(first.bus_lease.is_some());
        let observer = bus.connection().await;
        let dbus = zbus::fdo::DBusProxy::new(&observer).await.unwrap();
        let owner = dbus.get_name_owner(POWER_MANAGEMENT_BUS_NAME.try_into().unwrap()).await.unwrap();
        assert_eq!(owner.as_str(), rival.unique_name().unwrap().as_str());
        let err = pm_proxy(&observer, &unique).await.call::<_, _, bool>("HasInhibit", &()).await.unwrap_err();
        assert!(matches!(err, zbus::Error::MethodError(n, _, _) if n == "org.freedesktop.DBus.Error.UnknownObject"));
        let (mut second, _, _logind2) = controller().await;
        second.export_session_services(bus.connection().await).await;
        assert!(second.bus_lease.is_none());
        drop(rival);
        wait_name_released(&observer, POWER_MANAGEMENT_BUS_NAME).await;
        second.export_session_services(bus.connection().await).await;
        assert!(second.bus_lease.is_some());
        assert!(
            !pm_proxy(&observer, POWER_MANAGEMENT_BUS_NAME).await.call::<_, _, bool>("HasInhibit", &()).await.unwrap()
        );
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

//! [`IdleController`]: `mantle.idle`'s write dispatcher and state owner for notify and inhibit.
//! Split from `dbus::idle` -- see `hardware/idle/mod.rs` for the module-level doc.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use shared::{debug, error, info, warn};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::Proxy;

use super::gate::{IdleGate, blocks_idle};
use super::inhibit::{
    INHIBIT_MODE, INHIBIT_WHAT, INHIBIT_WHO, InhibitState, LiveInhibit, Login1ManagerProxy, SCREENSAVER_BUS_NAME,
    SCREENSAVER_HOLDER, SCREENSAVER_OBJECT_PATHS, ScreenSaver, apply_inhibit, apply_release_inhibit,
    apply_screensaver_inhibit, apply_screensaver_release, cleanup_generation_inhibit, drop_screensaver_peer,
};
use super::notify::{
    ListenerId, NotifyState, cleanup_generation_thresholds, connect_wayland_idle, register_threshold_entry,
    spawn_idle_event_forwarder, take_unused_listeners,
};
use super::state::{IdleState, foreign_idle_inhibitors};

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
    wayland_inhibited: bool,
    screensaver: Vec<super::state::IdleInhibitor>,
    last_sent: IdleState,
}

impl PublishedIdle {
    /// The merged payload. Nothing can name the compositor's half. No protocol lists
    /// idle-inhibitor holders, and the withholding has causes besides a surface inhibitor (see
    /// `notify::wayland_inhibited`). So it is an inhibitor with an empty `who`, the shape a config
    /// already draws for a logind holder that gave none, and a `why` stating the observation.
    fn merged(&self) -> IdleState {
        let mut inhibitors = self.logind_inhibitors.clone();
        // Ours by the time logind hears of them, and `foreign_idle_inhibitors` drops this shell's
        // own row, so the names can only come from here (ADR-0231).
        inhibitors.extend(self.screensaver.iter().cloned());
        if self.wayland_inhibited {
            inhibitors.push(super::state::IdleInhibitor {
                who: String::new(),
                why: "the compositor is holding off idle notifications".to_string(),
            });
        }
        IdleState {
            inhibited: self.logind_blocked || self.wayland_inhibited || !self.screensaver.is_empty(),
            inhibitors,
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

    /// `None` means the compositor gave no evidence this round, so the last answer stands; see
    /// `notify::wayland_inhibited`.
    pub(crate) fn set_wayland_inhibited(&mut self, held: Option<bool>) -> Option<IdleState> {
        self.wayland_inhibited = held.unwrap_or(self.wayland_inhibited);
        self.settle()
    }

    /// The `org.freedesktop.ScreenSaver` roster (ADR-0231). Reported before logind answers, so a
    /// config sees the holder in the same push that stops its countdown.
    fn set_screensaver(&mut self, holds: Vec<super::state::IdleInhibitor>) -> Option<IdleState> {
        self.screensaver = holds;
        self.settle()
    }

    fn set_logind(&mut self, blocked: bool, inhibitors: Vec<super::state::IdleInhibitor>) -> Option<IdleState> {
        self.logind_blocked = blocked;
        self.logind_inhibitors = inhibitors;
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
    inhibit: Arc<LiveInhibit>,
    /// The last state [`watch_idle_inhibitors`] published, so `Capabilities::start` returns the
    /// current answer instead of `nil` until the next inhibitor (ADR-0141).
    published: Arc<std::sync::Mutex<PublishedIdle>>,
    gate: Arc<std::sync::Mutex<IdleGate>>,
    /// Also held here, not just handed to the forwarder: reaping the last listener has to answer
    /// for the compositor itself, and no raw event will arrive to let the forwarder do it.
    state_tx: UnboundedSender<IdleState>,
}

impl IdleController {
    /// Constructs both halves and returns immediately. `system_bus` is the Supervisor's system bus;
    /// inhibit uses it directly (ADR-0032). `session_bus` carries the inbound half,
    /// `org.freedesktop.ScreenSaver` (ADR-0231); `None` leaves it unserved.
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

        let controller = Self {
            notify: notify.clone(),
            published: published.clone(),
            gate: gate.clone(),
            state_tx: state_tx.clone(),
            pending: Arc::new(std::sync::Mutex::new(Vec::new())),
            inhibit: Arc::new(LiveInhibit {
                system_bus,
                state: tokio::sync::Mutex::new(InhibitState {
                    counts: HashMap::new(),
                    screensaver: std::collections::BTreeMap::new(),
                    next_cookie: 1,
                    fd: None,
                }),
            }),
        };

        if let Some(session_bus) = session_bus {
            export_screensaver(&session_bus, controller.clone()).await;
        }

        let notify_for_task = notify.clone();
        let published_for_task = published.clone();
        let controller_for_task = controller.clone();
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(IDLE_NOTIFY_SETUP_TIMEOUT, tokio::task::spawn_blocking(connect_wayland_idle))
                    .await;
            match outcome {
                Ok(Ok(Ok((live, raw_events_rx)))) => {
                    spawn_idle_event_forwarder(
                        live.registry.clone(),
                        gate,
                        published_for_task,
                        raw_events_rx,
                        events_tx,
                        state_tx,
                    );
                    *notify_for_task.write().unwrap() = NotifyState::Live(live);
                    info!(
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
        self.published.lock().unwrap().merged()
    }

    /// Registers queued thresholds oldest first. Drain under the queue lock, then register outside
    /// it: `register_threshold` takes the same lock to requeue when notify is still inert.
    fn replay_pending_registrations(&self) {
        let queued: Vec<(u32, u64)> = std::mem::take(&mut *self.pending.lock().unwrap());
        if queued.is_empty() {
            return;
        }
        info!("notify is live; registering {} threshold(s) that arrived before it was", queued.len());
        for (generation_id, sec) in queued {
            self.register_threshold(generation_id, sec);
        }
    }

    /// Supervisor half of `idle:register_threshold(sec, on_idle, on_resume)` (ADR-0032). Inert
    /// notify queues nothing here; live notify applies [`register_threshold_entry`] and creates a
    /// new listener if needed.
    pub fn register_threshold(&self, generation_id: u32, sec: u64) {
        let notify = self.notify.read().unwrap();
        let NotifyState::Live(live) = &*notify else {
            // Inert means degraded or still setting up. Queue entries in either case; a failed
            // setup leaves a small `(u32, u64)` queue rather than dropping every boot registration.
            self.pending.lock().unwrap().push((generation_id, sec));
            debug!("register_threshold(generation {generation_id}, {sec}s) queued: notify is not live yet");
            return;
        };

        let created_new_listener = {
            let mut registry = live.registry.lock().unwrap();
            register_threshold_entry(&mut registry.fanout, generation_id, sec)
        };

        if created_new_listener {
            let duration = Duration::from_secs(sec);
            let timeout_ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
            let gated = ListenerId { duration, respects_inhibitors: true };
            let notification = live.notifier.get_idle_notification(timeout_ms, &live.seat, &live.queue_handle, gated);
            live.registry.lock().unwrap().listeners.insert(gated, notification);

            // The twin the compositor may not withhold. Its only job is to prove that silence on
            // the gated listener means an application is holding the session awake, rather than a
            // seat that is simply in use (ADR-0160). Version 1 compositors have no such request,
            // and degrade to the pre-ADR-0160 answer: `inhibited` reports logind only.
            if live.notifier.version() >= 2 {
                let input = ListenerId { duration, respects_inhibitors: false };
                let notification =
                    live.notifier.get_input_idle_notification(timeout_ms, &live.seat, &live.queue_handle, input);
                live.registry.lock().unwrap().listeners.insert(input, notification);
            }

            if let Err(err) = live.connection.flush() {
                warn!("failed to flush the get_idle_notification request for {sec}s: {err}");
            }
        }
    }

    /// `idle:inhibit(reason)` (ADR-0032): refcount decision, global 0->1 `Inhibit` call, and
    /// `fd`/count write stay under one `state` lock. Releasing it allows a stale zero during the
    /// call (leak) or lets an inhibit/release pair write `fd` out of order (clobber).
    pub async fn inhibit(&self, generation_id: u32, reason: &str) {
        let mut state = self.inhibit.state.lock().await;
        if apply_inhibit(&mut state.counts, generation_id).should_open_fd {
            self.open_shared_fd(&mut state, generation_id, reason).await;
        }
    }

    /// The one logind fd every holder shares, taken on the global 0->1. The caller holds `state`
    /// for the reasons [`IdleController::inhibit`] gives.
    ///
    /// A login1 proxy-build failure is a silent no-op (built fresh, not cached; see
    /// [`LiveInhibit::system_bus`]). A failed `Inhibit` call rolls back its count bump; a
    /// `ScreenSaver` cookie recorded against it stays, and releasing it then finds a zero count
    /// and does nothing.
    async fn open_shared_fd(&self, state: &mut InhibitState, holder: u32, reason: &str) {
        let proxy = match Login1ManagerProxy::new(&self.inhibit.system_bus).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("inhibit(holder {holder}, {reason:?}) failed to build the login1 Manager proxy: {err}");
                apply_release_inhibit(&mut state.counts, holder);
                return;
            }
        };

        match proxy.inhibit(INHIBIT_WHAT, INHIBIT_WHO, reason, INHIBIT_MODE).await {
            Ok(fd) => {
                state.fd = Some(fd);
            }
            Err(err) => {
                warn!("Inhibit({INHIBIT_WHAT:?}, {INHIBIT_WHO:?}, {reason:?}, {INHIBIT_MODE:?}) failed: {err}");
                apply_release_inhibit(&mut state.counts, holder);
            }
        }
    }

    /// `org.freedesktop.ScreenSaver.Inhibit` (ADR-0231): takes the same logind fd a config's
    /// `idle:inhibit` takes, so one refcount answers for both and the gate keeps one source.
    pub async fn screensaver_inhibit(&self, peer: String, who: String, why: String) -> u32 {
        let (cookie, holds) = {
            let mut state = self.inhibit.state.lock().await;
            let hold = super::state::IdleInhibitor { who, why };
            let reason = hold.why.clone();
            let (cookie, transition) = apply_screensaver_inhibit(&mut state, peer, hold);
            if transition.should_open_fd {
                self.open_shared_fd(&mut state, SCREENSAVER_HOLDER, &reason).await;
            }
            (cookie, screensaver_holds(&state))
        };
        // Debug, not info: a browser retakes its hold on every play, and the gate already says
        // once, at info, that something holds the session awake.
        debug!("ScreenSaver.Inhibit -> cookie {cookie}; {} hold(s) now live", holds.len());
        self.publish_screensaver(holds);
        cookie
    }

    /// `org.freedesktop.ScreenSaver.UnInhibit`; false for a cookie nothing holds, which the
    /// interface answers as an error.
    pub async fn screensaver_release(&self, cookie: u32) -> bool {
        let holds = {
            let mut state = self.inhibit.state.lock().await;
            let Some(transition) = apply_screensaver_release(&mut state, cookie) else { return false };
            if transition.should_close_fd {
                state.fd = None;
            }
            screensaver_holds(&state)
        };
        debug!("ScreenSaver.UnInhibit(cookie {cookie}); {} hold(s) still live", holds.len());
        self.publish_screensaver(holds);
        true
    }

    async fn screensaver_peer_left(&self, departed: &str) {
        let holds = {
            let mut state = self.inhibit.state.lock().await;
            let Some(transition) = drop_screensaver_peer(&mut state, departed) else { return };
            if transition.should_close_fd {
                state.fd = None;
            }
            screensaver_holds(&state)
        };
        info!("{departed} left the bus still holding an idle inhibitor; released it");
        self.publish_screensaver(holds);
    }

    /// Held across the send; see the matching comment in `spawn_idle_event_forwarder`.
    fn publish_screensaver(&self, holds: Vec<super::state::IdleInhibitor>) {
        let mut published = self.published.lock().unwrap();
        if let Some(next) = published.set_screensaver(holds) {
            let _ = self.state_tx.send(next);
        }
    }

    /// `idle:release_inhibit()` (ADR-0032): refcount decision and `fd` clear use the same held
    /// `state` lock as [`IdleController::inhibit`]. Dropping `OwnedFd` closes the logind lock.
    pub async fn release_inhibit(&self, generation_id: u32) {
        let mut state = self.inhibit.state.lock().await;
        if apply_release_inhibit(&mut state.counts, generation_id).should_close_fd {
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
        self.pending.lock().unwrap().retain(|&(queued_generation, _)| queued_generation != generation_id);
        let notify = self.notify.read().unwrap();
        if let NotifyState::Live(live) = &*notify {
            cleanup_generation_thresholds(&mut live.registry.lock().unwrap().fanout, generation_id);
        }
    }

    /// Notify and inhibit halves of `reset_registrations` (ADR-0006/ADR-0032): everything
    /// `generation_id` owned, for a generation that is gone. Closes the shared fd if it was the
    /// last holder. Uses the same `state` lock as inhibit/release, serializing reload races.
    pub async fn reset_registrations(&self, generation_id: u32) {
        self.reset_thresholds(generation_id);
        // Destroying listeners is reap-only: see `take_unused_listeners`.
        let mut listening = true;
        if let NotifyState::Live(live) = &*self.notify.read().unwrap() {
            let mut registry = live.registry.lock().unwrap();
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
        if !listening && let Some(next) = self.published.lock().unwrap().set_wayland_inhibited(Some(false)) {
            let _ = self.state_tx.send(next);
        }
        // Also not in `reset_thresholds`: an in-place reload keeps the generation id and its
        // listeners, and the compositor never resends `idled`, so the gate's entries still belong
        // to it.
        self.gate.lock().unwrap().forget(generation_id);

        let mut state = self.inhibit.state.lock().await;
        if cleanup_generation_inhibit(&mut state.counts, generation_id).should_close_fd {
            state.fd = None;
        }
    }
}

/// Every live `ScreenSaver` hold, oldest cookie first.
fn screensaver_holds(state: &InhibitState) -> Vec<super::state::IdleInhibitor> {
    state.screensaver.values().map(|(_, hold)| hold.clone()).collect()
}

/// Claims `org.freedesktop.ScreenSaver` and answers on both object paths (ADR-0231).
///
/// Exports before claiming the name, like the tray watcher: a call routed to the new owner has to
/// find the object. `DoNotQueue` because a session already running a screensaver daemon keeps it;
/// queueing would take the name later, mid-session, and answer for holds this shell never saw.
async fn export_screensaver(session_bus: &zbus::Connection, controller: IdleController) {
    for path in SCREENSAVER_OBJECT_PATHS {
        let export = session_bus.object_server().at(path, ScreenSaver { controller: controller.clone() });
        if let Err(err) = export.await {
            warn!("failed to export {SCREENSAVER_BUS_NAME} at {path}: {err}");
        }
    }
    match session_bus
        .request_name_with_flags(SCREENSAVER_BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => {
            info!("holding {SCREENSAVER_BUS_NAME}; idle inhibits from browsers and players reach the gate");
            tokio::spawn(watch_screensaver_peers(session_bus.clone(), controller));
        }
        Ok(other) => warn!("RequestName({SCREENSAVER_BUS_NAME}) -> {other:?}; another daemon answers its clients"),
        Err(err) => warn!("RequestName({SCREENSAVER_BUS_NAME}) failed: {err}; no client's inhibit reaches this shell"),
    }
}

/// Releases a departed client's holds (ADR-0231). A player that crashes mid-video sends no
/// `UnInhibit`, and nothing else would ever drop what it held.
async fn watch_screensaver_peers(session_bus: zbus::Connection, controller: IdleController) {
    let Ok(proxy) = zbus::fdo::DBusProxy::new(&session_bus).await else {
        warn!("cannot watch for departing screensaver clients; a crashed client's hold would outlive it");
        return;
    };
    // Filtered by the bus on `new_owner == ""`, not here: every application start and exit on the
    // session bus changes some name, and none of those need to wake this shell.
    let Ok(mut names) = proxy.receive_name_owner_changed_with_args(&[(2, "")]).await else { return };
    while let Some(signal) = futures_util::StreamExt::next(&mut names).await {
        let Ok(args) = signal.args() else { continue };
        controller.screensaver_peer_left(&args.name.to_string()).await;
    }
}

/// Follows `Manager.BlockInhibited` and keeps [`IdleGate`] in step (ADR-0139).
///
/// Watches the property rather than polling `ListInhibitors`: logind emits `PropertiesChanged`,
/// so any `systemd-inhibit --what=idle` reaches the gate in one round trip and costs nothing idle.
///
/// Reading before subscribing races; zbus replays the cached property on subscribe, making the
/// first item the startup state. Failure degrades to a permanently open gate, logged once.
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
    while futures_util::StreamExt::next(&mut changes).await.is_some() {
        // Read through the proxy: zbus caches the property and avoids naming the stream item's
        // borrowed type.
        let Ok(what) = proxy.block_inhibited().await else { continue };
        let blocked = blocks_idle(&what);
        // `None` means the same idle answer as before. `BlockInhibited` changes for every kind of
        // inhibitor, while the list can change without the answer moving (mpv releases while
        // Firefox still holds one), so publish state on every change but gate on transitions.
        if let Some(owed) = gate.lock().unwrap().set_blocked(blocked) {
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

        // One `ListInhibitors` per change, never a timer: ADR-0139 rejected polling, and the
        // property edge provides one call to hang it off. Skip it when nothing blocks idle.
        let inhibitors = if blocked {
            proxy.list_inhibitors().await.map(foreign_idle_inhibitors).unwrap_or_default()
        } else {
            Vec::new()
        };
        // Held across the send; see the matching comment in `spawn_idle_event_forwarder`.
        let mut published = published.lock().unwrap();
        let Some(next_state) = published.set_logind(blocked, inhibitors) else { continue };
        if state_tx.send(next_state).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract `reset_registrations` leans on when it reaps the last listener: `None` latches
    /// the previous answer, so only an explicit one clears a `true` nothing will contradict.
    #[test]
    fn no_evidence_keeps_the_last_compositor_answer_and_only_an_explicit_one_replaces_it() {
        let mut published = PublishedIdle::default();
        published.set_wayland_inhibited(Some(true));

        published.set_wayland_inhibited(None);
        assert!(published.merged().inhibited, "no evidence must leave the held answer standing");

        published.set_wayland_inhibited(Some(false));
        assert!(!published.merged().inhibited, "a reap with no listeners left must be able to clear it");
    }
}

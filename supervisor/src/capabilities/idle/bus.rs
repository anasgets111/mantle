//! Session-bus inhibit hosting for `mantle.idle`: `org.freedesktop.ScreenSaver` (ADR-0231) and
//! `org.freedesktop.PowerManagement` (ADR-0414).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use shared::{debug, warn};
use zbus::object_server::SignalEmitter;

use super::controller::{IdleController, open_shared_fd};
use super::inhibit::{
    BUS_HOLDER, InhibitState, LiveInhibit, POWER_MANAGEMENT_BUS_NAME, POWER_MANAGEMENT_OBJECT_PATH,
    PowerManagementInhibit, SCREENSAVER_BUS_NAME, SCREENSAVER_OBJECT_PATHS, ScreenSaver, apply_bus_inhibit,
    apply_bus_release, drop_bus_peer,
};

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
pub(super) struct BusLease {
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

impl IdleController {
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
                && !peer_alive(connection, &peer).await.unwrap_or(true)
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

    async fn release_departed_peer(&self, kind: BusInhibit, emitter: Option<&SignalEmitter<'_>>, departed: &str) {
        if self.drop_bus_holds(kind, emitter, |s| drop_bus_peer(s, departed)).await.is_some() {
            debug!("{departed} left the bus holding a {} inhibitor; released it", self.bus_target(kind).1);
        }
    }

    /// Held across the send; see the matching comment in `spawn_idle_event_forwarder`.
    fn publish_bus_holds(&self, kind: BusInhibit, holds: Vec<super::state::IdleInhibitor>) {
        let mut published = self.published.lock().expect("mutex poisoned");
        if let Some(next) = published.set_bus_holds(kind, holds) {
            let _ = self.state_tx.send(next);
        }
    }

    /// Clone without the session-bus lease, for objects and tasks owned by the lease itself so
    /// neither can form an `Arc` cycle back to the bus connection.
    fn without_lease(&self) -> Self {
        let mut clone = self.clone();
        clone.bus_lease = None;
        clone
    }

    /// Claims `org.freedesktop.ScreenSaver` (ADR-0231) and `org.freedesktop.PowerManagement` on
    /// `session_bus`, retaining the connection and one departure-watcher handle while either name
    /// is owned. Subscribes to departures first so no client can take a hold and leave unseen
    /// while `add_match` is still in flight.
    pub(super) async fn export_session_services(&mut self, session_bus: zbus::Connection) {
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
        let watcher = departures.map(|names| {
            tokio::spawn(watch_bus_peers(names, controller.clone(), session_bus.clone(), DEPARTURE_QUEUE_CAP))
                .abort_handle()
        });
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

/// Checks whether `peer` still owns its unique name on `connection`. `Err` means the lookup
/// itself failed: `bus_inhibit` counts that as alive (refusing would push Gecko off `ScreenSaver`
/// to a sleep-only hold for the rest of its process, ADR-0414), while overflow reconciliation
/// keeps the hold and re-arms reconciliation so the next departure retries it.
async fn peer_alive(connection: &zbus::Connection, peer: &str) -> zbus::fdo::Result<bool> {
    let Ok(name) = zbus::names::BusName::try_from(peer) else { return Ok(false) };
    zbus::fdo::DBusProxy::new(connection).await?.name_has_owner(name).await
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

/// Bound on queued peer departures while a hold mutex is busy; past it, live holders are
/// reconciled with `NameHasOwner` instead of growing the queue.
const DEPARTURE_QUEUE_CAP: usize = 256;

/// Releases departed clients' holds across both services (ADR-0231). Drains the D-Bus stream
/// continuously into a bounded queue so waiting on a hold mutex never backpressures zbus's
/// 64-message socket reader and stalls `NameHasOwner` replies on the same connection; on
/// overflow, reconciles every live holder with `NameHasOwner`.
async fn watch_bus_peers(
    mut names: zbus::fdo::NameOwnerChangedStream,
    controller: IdleController,
    connection: zbus::Connection,
    cap: usize,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(cap);
    let overflowed = AtomicBool::new(false);
    let drain = async {
        while let Some(signal) = futures_util::StreamExt::next(&mut names).await {
            if let Ok(args) = signal.args() {
                match tx.try_send(args.name.to_string()) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => overflowed.store(true, Ordering::Release),
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        }
        drop(tx);
    };
    let apply = async {
        let pm_emitter = SignalEmitter::new(&connection, POWER_MANAGEMENT_OBJECT_PATH).ok();
        let services = [(BusInhibit::ScreenSaver, None), (BusInhibit::PowerManagement, pm_emitter.as_ref())];
        while let Some(departed) = rx.recv().await {
            if overflowed.swap(false, Ordering::AcqRel) {
                while rx.try_recv().is_ok() {}
                warn!("departure queue overflowed ({cap}); reconciling live inhibit holders");
                for (kind, emitter) in services {
                    let peers: BTreeSet<String> = controller
                        .bus_target(kind)
                        .0
                        .state
                        .lock()
                        .await
                        .holds
                        .values()
                        .map(|(peer, _)| peer.clone())
                        .collect();
                    for peer in peers {
                        match peer_alive(&connection, &peer).await {
                            Ok(false) => controller.release_departed_peer(kind, emitter, &peer).await,
                            Ok(true) => {}
                            Err(err) => {
                                warn!("NameHasOwner({peer}) failed during reconcile: {err}; will retry");
                                overflowed.store(true, Ordering::Release);
                            }
                        }
                    }
                }
            } else {
                for (kind, emitter) in services {
                    controller.release_departed_peer(kind, emitter, &departed).await;
                }
            }
        }
    };
    tokio::join!(drain, apply);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::unbounded_channel;

    use super::super::controller::tests::{controller, released, taken};
    use super::super::inhibit::tests::REFUSED;
    use super::super::state::IdleState;
    use super::BusInhibit::*;
    use super::*;
    use crate::capabilities::test_support::{p2p_pair_serving, poll_until, private_bus, within};

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

    /// A ScreenSaver hold reaches the pushed state and the start snapshot, and leaves both on release.
    #[tokio::test]
    async fn a_screensaver_hold_is_published_and_snapshotted_until_released() {
        let (mut controller, mut locks, _logind) = controller().await;
        let (state_tx, mut pushed) = unbounded_channel();
        controller.state_tx = state_tx;
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
        let dbus = zbus::fdo::DBusProxy::new(&observer).await.unwrap();
        let mut observed = crate::capabilities::name_departures(&dbus, "burst").await.unwrap();
        let mut peers = Vec::with_capacity(70);
        for _ in 0..70 {
            peers.push(bus.connection().await);
        }
        let mut pending: BTreeSet<String> = peers.iter().map(|p| p.unique_name().unwrap().to_string()).collect();
        let hold_lock = controller.sleep_inhibit.state.lock().await;
        futures_util::future::join_all(peers.into_iter().map(|p| async { p.close().await.unwrap() })).await;
        within(async {
            while !pending.is_empty() {
                let sig = futures_util::StreamExt::next(&mut observed).await.unwrap();
                pending.remove(sig.args().unwrap().name.as_str());
            }
        })
        .await;
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
        let (mut controller, mut locks, _logind) = controller().await;
        let (state_tx, mut pushed) = unbounded_channel();
        controller.state_tx = state_tx;
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

    async fn wait_peer_departed(connection: &zbus::Connection, peer: &str) {
        within(async {
            while peer_alive(connection, peer).await != Ok(false) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
    }

    /// Overflowing the bounded departure queue reconciles every live holder with `NameHasOwner`,
    /// releasing departed peers' holds across both services while keeping live peers' holds intact.
    #[tokio::test]
    async fn departure_queue_overflow_reconciles_dead_holders_and_keeps_live_ones() {
        let (controller, mut locks, _logind) = controller().await;
        let bus = private_bus().await;
        let server_conn = bus.connection().await;
        let dbus = zbus::fdo::DBusProxy::new(&server_conn).await.unwrap();
        let departures = crate::capabilities::name_departures(&dbus, "test").await.unwrap();
        let watcher = tokio::spawn(watch_bus_peers(departures, controller.without_lease(), server_conn.clone(), 1));

        let live = bus.connection().await;
        let live_name = live.unique_name().unwrap().to_string();
        let leaver = bus.connection().await;
        let leaver_name = leaver.unique_name().unwrap().to_string();

        for (peer, who) in [(&live_name, "live"), (&leaver_name, "dead")] {
            for kind in [ScreenSaver, PowerManagement] {
                controller
                    .bus_inhibit(kind, peer.clone(), who.into(), "Playing".into(), Some(&server_conn), None)
                    .await
                    .unwrap();
            }
        }
        let idle_lock = taken(&mut locks).await;
        let sleep_lock = taken(&mut locks).await;

        // Park `apply` on `ScreenSaver`'s hold mutex, wait for `blocker` and `filler1` in order so
        // `leaver`'s departure overflows the 1-slot queue and must be cleaned up by reconciliation.
        let hold_lock = controller.inhibit.state.lock().await;
        let blocker = bus.connection().await;
        let blocker_name = blocker.unique_name().unwrap().to_string();
        let filler1 = bus.connection().await;
        let filler1_name = filler1.unique_name().unwrap().to_string();
        let filler2 = bus.connection().await;
        let filler2_name = filler2.unique_name().unwrap().to_string();
        blocker.close().await.unwrap();
        wait_peer_departed(&server_conn, &blocker_name).await;
        filler1.close().await.unwrap();
        wait_peer_departed(&server_conn, &filler1_name).await;
        leaver.close().await.unwrap();
        filler2.close().await.unwrap();
        wait_peer_departed(&server_conn, &filler2_name).await;
        drop(hold_lock);

        let expected = super::super::state::IdleInhibitor { who: "live".into(), why: "Playing".into() };
        poll_until(|| {
            let snap = controller.snapshot();
            (snap.inhibitors == [expected.clone()] && snap.sleep_inhibitors == [expected.clone()]).then_some(())
        })
        .await;
        assert!(controller.inhibit.state.lock().await.fd.is_some());
        assert!(controller.sleep_inhibit.state.lock().await.fd.is_some());

        // Normal departure handling still works after overflow, and closing the bus ends the task.
        live.close().await.unwrap();
        released(idle_lock).await;
        released(sleep_lock).await;
        poll_until(|| (controller.snapshot() == IdleState::default()).then_some(())).await;

        drop(bus);
        within(watcher).await.unwrap();
    }

    /// Stand-in `org.freedesktop.DBus` for `NameHasOwner` fault injection on a p2p connection.
    struct StubDBus {
        dead: String,
        fail: Arc<AtomicBool>,
        errored: tokio::sync::mpsc::UnboundedSender<()>,
    }

    #[zbus::interface(name = "org.freedesktop.DBus")]
    impl StubDBus {
        #[zbus(name = "NameHasOwner")]
        fn name_has_owner(&self, name: String) -> zbus::fdo::Result<bool> {
            if name == self.dead {
                if self.fail.load(Ordering::Acquire) {
                    let _ = self.errored.send(());
                    return Err(zbus::fdo::Error::Failed("transient lookup failure".into()));
                }
                return Ok(false);
            }
            Ok(true)
        }
    }

    /// A failed `NameHasOwner` during overflow reconciliation keeps the hold and re-arms
    /// reconciliation so the next departure retries and frees the dead peer's holds.
    #[tokio::test]
    async fn overflow_reconcile_retries_after_name_has_owner_error() {
        let (controller, mut locks, _logind) = controller().await;
        let bus = private_bus().await;
        let server_conn = bus.connection().await;
        let dbus = zbus::fdo::DBusProxy::new(&server_conn).await.unwrap();
        let departures = crate::capabilities::name_departures(&dbus, "test").await.unwrap();

        let live = bus.connection().await;
        let live_name = live.unique_name().unwrap().to_string();
        let leaver = bus.connection().await;
        let leaver_name = leaver.unique_name().unwrap().to_string();

        let fail = Arc::new(AtomicBool::new(true));
        let (errored_tx, mut errored_rx) = unbounded_channel();
        let (fake_bus, _dbus_peer) = p2p_pair_serving(|peer| {
            peer.serve_at(
                "/org/freedesktop/DBus",
                StubDBus { dead: leaver_name.clone(), fail: fail.clone(), errored: errored_tx },
            )
        })
        .await;
        let watcher = tokio::spawn(watch_bus_peers(departures, controller.without_lease(), fake_bus, 1));

        for (peer, who) in [(&live_name, "live"), (&leaver_name, "dead")] {
            for kind in [ScreenSaver, PowerManagement] {
                controller
                    .bus_inhibit(kind, peer.clone(), who.into(), "Playing".into(), Some(&server_conn), None)
                    .await
                    .unwrap();
            }
        }
        let _idle_lock = taken(&mut locks).await;
        let _sleep_lock = taken(&mut locks).await;

        let hold_lock = controller.inhibit.state.lock().await;
        let blocker = bus.connection().await;
        let blocker_name = blocker.unique_name().unwrap().to_string();
        let filler1 = bus.connection().await;
        let filler1_name = filler1.unique_name().unwrap().to_string();
        let filler2 = bus.connection().await;
        let filler2_name = filler2.unique_name().unwrap().to_string();
        blocker.close().await.unwrap();
        wait_peer_departed(&server_conn, &blocker_name).await;
        filler1.close().await.unwrap();
        wait_peer_departed(&server_conn, &filler1_name).await;
        leaver.close().await.unwrap();
        filler2.close().await.unwrap();
        wait_peer_departed(&server_conn, &filler2_name).await;
        drop(hold_lock);

        // Both services hit the injected lookup failure and must keep the holds until retry.
        within(errored_rx.recv()).await.unwrap();
        within(errored_rx.recv()).await.unwrap();
        let snap = controller.snapshot();
        assert_eq!((snap.inhibitors.len(), snap.sleep_inhibitors.len()), (2, 2));

        // Clearing the fault and sending one unrelated departure retries reconciliation and frees
        // `leaver` without needing another overflow, while keeping `live`.
        fail.store(false, Ordering::Release);
        bus.connection().await.close().await.unwrap();

        let expected = super::super::state::IdleInhibitor { who: "live".into(), why: "Playing".into() };
        poll_until(|| {
            let snap = controller.snapshot();
            (snap.inhibitors == [expected.clone()] && snap.sleep_inhibitors == [expected.clone()]).then_some(())
        })
        .await;

        drop(bus);
        within(watcher).await.unwrap();
    }
}

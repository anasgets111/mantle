//! Saved VPN and WireGuard profiles: listing, `connect_vpn`/`disconnect_vpn` and the verdict.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use shared::state::network::{VpnError, VpnInfo, VpnKind};
use shared::state::network_profiles::{ProfileInfo, ProfileKind};
use tokio::sync::mpsc::UnboundedSender;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::OwnedObjectPath;

use super::connect::{ACTIVATION_CEILING, ActiveConnectionProxy};
use super::profile_actions::refusal;
use super::proxies::{ACTIVE_STATE_ACTIVATED, ACTIVE_STATE_DEACTIVATED};
use super::secret_agent::PendingSecret;
use super::{NetworkController, NetworkSignal, NetworkState, root_object_path};
use crate::capabilities::{bind_uncached, get_property};
use shared::{error, warn};

/// Last failure, secret request and `connect_vpn` waits behind one lock; the agent shares it.
#[derive(Default)]
pub(super) struct VpnState {
    pub(super) error: Option<VpnError>,
    pub(super) secret: Option<PendingSecret>,
    /// The `connect_vpn` attempt waiting on each profile. A user stop removes it, so its verdict sets no error.
    pub(super) waiting: HashMap<String, String>,
}

impl VpnState {
    pub(super) fn overlay(&self, state: &mut NetworkState) {
        state.vpn_error.clone_from(&self.error);
        state.vpn_secret = self.secret.as_ref().map(|pending| Box::new(pending.request.clone()));
    }
}

/// Whether NetworkManager, as `owner` last named it, sent `message`. The bus applies a `sender`
/// rule to broadcasts only, and zbus cannot match a well-known name, so a signal sent to the
/// Supervisor directly lands on a match-rule stream too.
fn sent_by_nm(owner: &Mutex<Option<OwnedUniqueName>>, message: &zbus::Message) -> bool {
    let owner = owner.lock().expect("mutex poisoned");
    matches!((owner.as_deref(), message.header().sender()), (Some(owner), Some(sender)) if owner == sender)
}

/// Display text for an `ActiveConnection.StateChanged` reason.
fn failure_text(reason: u32) -> &'static str {
    match reason {
        2 => "disconnected",
        6 | 7 => "connection timed out",
        9 => "secrets not provided",
        10 => "login failed",
        _ => "connection failed",
    }
}

/// Every active connection's StateChanged, which `ActiveConnections` does not report. Each also
/// sets `activity`, so the next rebuild re-reads which profiles are active.
pub(super) async fn forward_active_changes(
    connection: &zbus::Connection,
    nm_owner: Arc<Mutex<Option<OwnedUniqueName>>>,
    events: UnboundedSender<NetworkSignal>,
    activity: Arc<AtomicBool>,
) {
    let rule = "type='signal',sender='org.freedesktop.NetworkManager',interface='org.freedesktop.NetworkManager.Connection.Active',member='StateChanged'";
    forward_nm_signal(connection, nm_owner, rule, events, move |_| {
        activity.store(true, Ordering::Relaxed);
        Some(NetworkSignal::Changed)
    })
    .await;
}

/// Forwards each broadcast `rule` matches as `signal(message)`, when NetworkManager sent it.
pub(super) async fn forward_nm_signal(
    connection: &zbus::Connection,
    nm_owner: Arc<Mutex<Option<OwnedUniqueName>>>,
    rule: &'static str,
    events: UnboundedSender<NetworkSignal>,
    signal: impl Fn(&zbus::Message) -> Option<NetworkSignal> + Send + 'static,
) {
    let match_rule = zbus::MatchRule::try_from(rule).expect("a valid match rule");
    // A bus call here would stall the connection's reader once a flood fills the stream's queue.
    let stream = match zbus::MessageStream::for_match_rule(match_rule, connection, None).await {
        Ok(stream) => stream,
        Err(err) => return error!("failed to subscribe to {rule}; those changes need a restart: {err}"),
    };
    let mut signals = Box::pin(stream.filter_map(move |message| {
        let message = message.ok().filter(|message| sent_by_nm(&nm_owner, message));
        std::future::ready(message.and_then(|message| signal(&message)))
    }));
    tokio::spawn(async move {
        while let Some(signal) = signals.next().await
            && events.send(signal).is_ok()
        {}
    });
}

impl NetworkController {
    /// Active connections as `(path, uuid, state)`, read uncached: `nm`'s cache outlives a restarted
    /// NetworkManager, and a cached proxy per rebuild costs a `GetAll` and a match rule.
    pub(super) async fn active_connections(&self) -> zbus::Result<Vec<(OwnedObjectPath, String, u32)>> {
        let mut active = Vec::new();
        let paths = get_property::<Vec<OwnedObjectPath>>(self.nm.inner(), "ActiveConnections").await?;
        for path in paths.unwrap_or_default() {
            let Ok(proxy) = bind_uncached::<ActiveConnectionProxy>(&self.connection, path.clone()).await else {
                continue;
            };
            if let (Ok(uuid), Ok(state)) = tokio::join!(proxy.uuid(), proxy.state()) {
                active.push((path, uuid, state));
            }
        }
        Ok(active)
    }

    /// The VPN and WireGuard profiles `network_profiles` lists, with the activations it last read.
    pub(super) fn build_vpns(&self) -> Vec<VpnInfo> {
        let vpn = |profile: ProfileInfo| {
            let kind = match profile.kind {
                ProfileKind::Vpn => VpnKind::Vpn,
                ProfileKind::Wireguard => VpnKind::Wireguard,
                _ => return None,
            };
            let (id, uuid, active, activating) = (profile.id, profile.uuid, profile.active, profile.activating);
            Some(VpnInfo { id, kind, uuid, active, activating })
        };
        self.profiles.lock().expect("mutex poisoned").snapshot().profiles.into_iter().filter_map(vpn).collect()
    }

    /// `ActivateConnection`, dropping `accepted` once NetworkManager replies, then the verdict into
    /// `vpn_error`. A second call while one waits is a no-op.
    pub async fn connect_vpn(&self, uuid: &str, accepted: tokio::sync::oneshot::Sender<()>) {
        let Some(id) = self.build_vpns().into_iter().find(|vpn| vpn.uuid == uuid).map(|vpn| vpn.id) else {
            return warn!("connect_vpn({uuid:?}): no such VPN profile");
        };
        let attempt = {
            let mut vpn = self.vpn.lock().expect("mutex poisoned");
            if vpn.waiting.contains_key(uuid) {
                return;
            }
            let Some(attempt) = crate::capabilities::next_request_id() else { return };
            vpn.waiting.insert(uuid.into(), attempt.clone());
            vpn.error = None;
            attempt
        };
        let root = root_object_path();
        let activate = async {
            self.nm.activate_connection(&self.path_of(uuid).await?.as_ref(), &root, &root).await.map_err(refusal)
        };
        let activated = activate.await;
        drop(accepted);
        let outcome = match activated {
            Ok(active) => self.vpn_outcome(&active, uuid).await.map_err(String::from),
            Err(err) => Err(err),
        };
        {
            let mut vpn = self.vpn.lock().expect("mutex poisoned");
            if vpn.waiting.get(uuid) == Some(&attempt) {
                vpn.waiting.remove(uuid);
                if let Err(message) = outcome {
                    warn!("connect_vpn({id:?}) failed: {message}");
                    vpn.error = Some(VpnError { uuid: uuid.into(), message });
                }
            }
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    /// Deactivates every activation of `uuid`, reporting the first refusal; a waiting `connect_vpn`
    /// then reports no error.
    pub async fn disconnect_vpn(&self, uuid: &str) -> zbus::Result<()> {
        self.vpn.lock().expect("mutex poisoned").waiting.remove(uuid);
        let mut stopped = Ok(());
        for (path, _, _) in self.active_connections().await?.into_iter().filter(|(_, active, _)| active == uuid) {
            stopped = stopped.and(self.nm.deactivate_connection(&path).await);
        }
        stopped
    }

    /// Waits for `active` to settle. The ceiling pauses while this profile's secret prompt is up,
    /// and deactivates the attempt when it runs out.
    async fn vpn_outcome(&self, active: &OwnedObjectPath, uuid: &str) -> Result<(), &'static str> {
        let (mut changes, settled) = self.watch_active(active).await.map_err(|err| {
            warn!("failed to watch the VPN activation {active}: {err}");
            "connection failed"
        })?;
        if settled == Some(true) {
            return Ok(());
        }
        // After an early DEACTIVATED read, only a failure NM queued ahead of the reply is left to read.
        let ceiling = if settled.is_some() { std::time::Duration::ZERO } else { ACTIVATION_CEILING };
        let prompting =
            || self.vpn.lock().expect("mutex poisoned").secret.as_ref().is_some_and(|p| p.request.uuid == uuid);
        loop {
            match tokio::time::timeout(ceiling, changes.next()).await {
                Ok(Some(change)) => {
                    let Ok(args) = change.args() else { continue };
                    match args.state {
                        ACTIVE_STATE_ACTIVATED => return Ok(()),
                        ACTIVE_STATE_DEACTIVATED => return Err(failure_text(args.reason)),
                        _ => {}
                    }
                }
                Ok(None) => return Err(failure_text(0)),
                Err(_) if settled.is_some() => return Err(failure_text(0)),
                Err(_) if prompting() => {}
                Err(_) => {
                    if let Err(err) = self.nm.deactivate_connection(active).await {
                        warn!("failed to stop the timed-out VPN activation {active}: {err}");
                    }
                    return Err("connection timed out");
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedReceiver;
    use zbus::zvariant::OwnedValue;

    use super::*;
    use crate::capabilities::network::connect::tests::{ListedProfiles, attempting};
    use crate::capabilities::network::tests::{Nm, SavedProfile, owned};
    use crate::capabilities::test_support::within;

    pub(crate) const ACTIVE: &str = "/org/freedesktop/NetworkManager/ActiveConnection/3";
    const PROFILE: &str = "/org/freedesktop/NetworkManager/Settings/5";

    pub(crate) fn settings(kind: &str) -> HashMap<String, HashMap<String, OwnedValue>> {
        HashMap::from([(
            "connection".to_string(),
            HashMap::from([("type".into(), owned(kind)), ("id".into(), owned("work")), ("uuid".into(), owned("u-1"))]),
        )])
    }

    #[tokio::test]
    async fn only_vpn_and_wireguard_profiles_are_listed() {
        let (controller, _events, _peer) = attempting(Ok).await;
        for kind in ["vpn", "wireguard", "802-11-wireless"] {
            let listed =
                (OwnedObjectPath::try_from(PROFILE).unwrap(), super::super::edit::project(&settings(kind)).unwrap());
            controller.profiles.lock().unwrap().list.push(listed);
        }
        let kinds: Vec<_> = controller.build_vpns().into_iter().map(|vpn| vpn.kind).collect();
        assert_eq!(kinds, [VpnKind::Vpn, VpnKind::Wireguard]);
    }

    /// An activation on wlan0 reading `state`, whose `State` read first queues `then`, as NM does.
    pub(crate) struct FakeActive {
        pub(crate) state: u32,
        pub(crate) then: Option<(u32, u32)>,
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
    impl FakeActive {
        #[zbus(property)]
        async fn state(&self, #[zbus(connection)] connection: &zbus::Connection) -> u32 {
            if let Some(change) = self.then {
                let active = "org.freedesktop.NetworkManager.Connection.Active";
                connection.emit_signal(None::<&str>, ACTIVE, active, "StateChanged", &change).await.unwrap();
            }
            self.state
        }

        #[zbus(property)]
        fn uuid(&self) -> String {
            "u-1".into()
        }

        /// wlan0 in `network::tests`.
        #[zbus(property)]
        fn devices(&self) -> Vec<OwnedObjectPath> {
            vec![OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Devices/1").unwrap()]
        }
    }

    /// The controller, and every call its NetworkManager answers.
    async fn controller(active: FakeActive) -> (NetworkController, UnboundedReceiver<String>, zbus::Connection) {
        let (calls, asked) = tokio::sync::mpsc::unbounded_channel();
        let (controller, _events, peer) = attempting(|peer| {
            peer.serve_at("/org/freedesktop/NetworkManager", Nm(calls.clone(), None))?
                .serve_at(
                    "/org/freedesktop/NetworkManager/Settings",
                    ListedProfiles(vec![PROFILE.try_into().unwrap()]),
                )?
                .serve_at(PROFILE, SavedProfile(calls, settings("vpn"), 0, Vec::new()))?
                .serve_at(ACTIVE, active)
        })
        .await;
        controller.refresh_profiles().await;
        (controller, asked, peer)
    }

    fn vpn_error(controller: &NetworkController) -> Option<String> {
        controller.vpn.lock().unwrap().error.as_ref().map(|error| error.message.clone())
    }

    #[tokio::test]
    async fn a_listed_profile_reads_activating_and_its_activation_succeeds() {
        let (controller, _, _peer) = controller(FakeActive { state: 1, then: Some((2, 0)) }).await;
        assert_eq!(
            {
                controller.read_profile_activity().await;
                controller.build_vpns()
            },
            [VpnInfo { id: "work".into(), kind: VpnKind::Vpn, uuid: "u-1".into(), active: false, activating: true }]
        );
        tokio::time::timeout(Duration::from_secs(1), controller.connect_vpn("u-1", tokio::sync::oneshot::channel().0))
            .await
            .unwrap();
        assert_eq!(vpn_error(&controller), None);
        assert!(controller.vpn.lock().unwrap().waiting.is_empty());
    }

    #[tokio::test]
    async fn a_failure_queued_ahead_of_the_state_read_keeps_its_reason() {
        let (controller, _, _peer) = controller(FakeActive { state: 4, then: Some((4, 10)) }).await;
        tokio::time::timeout(Duration::from_secs(1), controller.connect_vpn("u-1", tokio::sync::oneshot::channel().0))
            .await
            .unwrap();
        assert_eq!(vpn_error(&controller).as_deref(), Some("login failed"));
    }

    #[tokio::test]
    async fn only_networkmanager_can_report_an_activation_change() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let nm = bus.builder().name("org.freedesktop.NetworkManager").unwrap().build().await.unwrap();
        let (ours, impostor) = (bus.connection().await, bus.connection().await);
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let owner = Some(nm.unique_name().unwrap().to_owned());
        forward_active_changes(&ours, Arc::new(Mutex::new(owner)), events, Arc::default()).await;
        let interface = "org.freedesktop.NetworkManager.Connection.Active";

        let direct = ours.unique_name().unwrap().to_string();
        for destination in [None, Some(direct.as_str())] {
            impostor.emit_signal(destination, ACTIVE, interface, "StateChanged", &(2u32, 0u32)).await.unwrap();
        }
        // Answered only once the bus has routed both signals, so they reach `ours` ahead of NM's.
        zbus::fdo::DBusProxy::new(&impostor).await.unwrap().get_id().await.unwrap();
        nm.emit_signal(None::<&str>, ACTIVE, interface, "StateChanged", &(2u32, 0u32)).await.unwrap();

        assert_eq!(within(received.recv()).await, Some(NetworkSignal::Changed));
        let more = tokio::time::timeout(Duration::from_millis(50), received.recv()).await;
        assert!(more.is_err(), "another sender's StateChanged must not rebuild the state");
    }

    #[tokio::test]
    async fn an_impostor_flood_delays_neither_a_genuine_signal_nor_a_method_call() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let nm = bus.builder().name("org.freedesktop.NetworkManager").unwrap().build().await.unwrap();
        let (ours, impostor) = (bus.connection().await, bus.connection().await);
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let owner = Some(nm.unique_name().unwrap().to_owned());
        forward_active_changes(&ours, Arc::new(Mutex::new(owner)), events, Arc::default()).await;
        let interface = "org.freedesktop.NetworkManager.Connection.Active";

        let direct = ours.unique_name().unwrap().to_string();
        for _ in 0..200 {
            impostor
                .emit_signal(Some(direct.as_str()), ACTIVE, interface, "StateChanged", &(2u32, 0u32))
                .await
                .unwrap();
        }
        zbus::fdo::DBusProxy::new(&impostor).await.unwrap().get_id().await.unwrap();
        let bound = Duration::from_secs(2);
        let call = async { zbus::fdo::DBusProxy::new(&ours).await.unwrap().get_id().await.unwrap() };
        tokio::time::timeout(bound, call).await.expect("an unrelated call on the flooded connection must answer");
        nm.emit_signal(None::<&str>, ACTIVE, interface, "StateChanged", &(2u32, 0u32)).await.unwrap();

        let genuine = tokio::time::timeout(bound, received.recv()).await;
        assert_eq!(
            genuine.expect("NetworkManager's signal must not queue behind the flood"),
            Some(NetworkSignal::Changed)
        );
        let more = tokio::time::timeout(Duration::from_millis(50), received.recv()).await;
        assert!(more.is_err(), "the impostors' StateChanged must not rebuild the state");
    }

    #[tokio::test]
    async fn the_ceiling_stops_a_stuck_activation_and_a_user_disconnect_is_no_error() {
        let (controller, mut asked, _peer) = controller(FakeActive { state: 1, then: None }).await;
        tokio::time::pause();
        let (_, stopped) = tokio::join!(
            controller.connect_vpn("u-1", tokio::sync::oneshot::channel().0),
            controller.disconnect_vpn("u-1")
        );
        stopped.unwrap();
        assert_eq!(vpn_error(&controller), None, "the user stopped it");

        controller.connect_vpn("u-1", tokio::sync::oneshot::channel().0).await;
        assert_eq!(vpn_error(&controller).as_deref(), Some("connection timed out"));
        let deactivated =
            std::iter::from_fn(|| asked.try_recv().ok()).filter(|call| call.contains("DeactivateConnection"));
        assert_eq!(deactivated.count(), 3, "the disconnect, then each ceiling");
    }
}

//! Saved VPN and WireGuard profiles: listing, `connect_vpn`/`disconnect_vpn` and the verdict.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use shared::state::network::{VpnError, VpnInfo, VpnKind};
use tokio::sync::mpsc::UnboundedSender;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::connect::{ACTIVATION_CEILING, ActiveConnectionProxy};
use super::devices::forward;
use super::proxies::{ACTIVE_STATE_ACTIVATED, ACTIVE_STATE_ACTIVATING, ACTIVE_STATE_DEACTIVATED};
use super::secret_agent::PendingSecret;
use super::{NetworkController, NetworkSignal, NetworkState, root_object_path};
use shared::warn;

/// Profile cache, last failure, secret request and `connect_vpn` waits behind one lock; the agent shares it.
#[derive(Default)]
pub(super) struct VpnState {
    /// Saved profiles by path, `active` and `activating` unset.
    pub(super) profiles: Vec<(OwnedObjectPath, VpnInfo)>,
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

/// A saved profile with `connection.type` `vpn` or `wireguard`.
pub(super) fn profile_of(settings: &HashMap<String, HashMap<String, OwnedValue>>) -> Option<VpnInfo> {
    let text = |key: &str| String::try_from(settings.get("connection")?.get(key)?.clone()).ok();
    let kind = match text("type")?.as_str() {
        "vpn" => VpnKind::Vpn,
        "wireguard" => VpnKind::Wireguard,
        _ => return None,
    };
    Some(VpnInfo { id: text("id")?, kind, uuid: text("uuid")?, active: false, activating: false })
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

/// Every active connection's StateChanged, which `ActiveConnections` does not report.
pub(super) async fn forward_active_changes(
    connection: &zbus::Connection,
    nm_owner: Arc<Mutex<Option<OwnedUniqueName>>>,
    events: UnboundedSender<NetworkSignal>,
) {
    let rule = "type='signal',sender='org.freedesktop.NetworkManager',interface='org.freedesktop.NetworkManager.Connection.Active',member='StateChanged'";
    let rule = zbus::MatchRule::try_from(rule).expect("a valid match rule");
    // The bus applies `sender` to broadcasts only, and zbus cannot match a well-known name: a
    // signal sent to the Supervisor directly still lands here, so check it against NM's owner. A bus
    // call here would stall the connection's reader once a flood fills the stream's queue.
    let subscribed = zbus::MessageStream::for_match_rule(rule, connection, None).await.map(|stream| {
        Box::pin(stream.filter(move |message| {
            let header = message.as_ref().ok().map(zbus::Message::header);
            let owner = nm_owner.lock().expect("mutex poisoned");
            let sender = header.as_ref().and_then(zbus::message::Header::sender);
            std::future::ready(matches!((owner.as_deref(), sender), (Some(owner), Some(sender)) if owner == sender))
        }))
    });
    let subscribed = subscribed.map(|stream| (stream, futures_util::stream::empty::<()>()));
    forward(subscribed, NetworkSignal::Changed, events);
}

impl NetworkController {
    /// Active connections as `(path, uuid, state)`, read uncached: a cached proxy per rebuild costs a `GetAll` and a match rule.
    async fn active_connections(&self) -> Vec<(OwnedObjectPath, String, u32)> {
        let mut active = Vec::new();
        for path in self.nm.active_connections().await.unwrap_or_default() {
            let proxy = match ActiveConnectionProxy::builder(&self.connection).path(path.clone()) {
                Ok(builder) => builder.cache_properties(zbus::proxy::CacheProperties::No).build().await,
                Err(err) => Err(err),
            };
            let Ok(proxy) = proxy else { continue };
            if let (Ok(uuid), Ok(state)) = tokio::join!(proxy.uuid(), proxy.state()) {
                active.push((path, uuid, state));
            }
        }
        active
    }

    pub(super) async fn build_vpns(&self) -> Vec<VpnInfo> {
        let mut vpns: Vec<_> =
            self.vpn.lock().expect("mutex poisoned").profiles.iter().map(|(_, vpn)| vpn.clone()).collect();
        if vpns.is_empty() {
            return vpns;
        }
        let active = self.active_connections().await;
        for vpn in &mut vpns {
            let state = |wanted| active.iter().any(|(_, uuid, state)| *uuid == vpn.uuid && *state == wanted);
            (vpn.active, vpn.activating) = (state(ACTIVE_STATE_ACTIVATED), state(ACTIVE_STATE_ACTIVATING));
        }
        vpns
    }

    /// `ActivateConnection`, then the verdict into `vpn_error`. A second call while one waits is a no-op.
    pub async fn connect_vpn(&self, uuid: &str) {
        let ((path, profile), attempt) = {
            let mut vpn = self.vpn.lock().expect("mutex poisoned");
            let Some(profile) = vpn.profiles.iter().find(|(_, p)| p.uuid == uuid).cloned() else {
                return warn!("connect_vpn({uuid:?}): no such VPN profile");
            };
            if vpn.waiting.contains_key(uuid) {
                return;
            }
            let Some(attempt) = crate::capabilities::next_request_id() else { return };
            vpn.waiting.insert(uuid.into(), attempt.clone());
            vpn.error = None;
            (profile, attempt)
        };
        let root = root_object_path();
        let outcome = match self.nm.activate_connection(&path, &root, &root).await {
            Ok(active) => self.vpn_outcome(&active, uuid).await,
            Err(err) => {
                warn!("connect_vpn({:?}): ActivateConnection failed: {err}", profile.id);
                Err(match err {
                    zbus::Error::MethodError(name, ..) if name.ends_with(".PermissionDenied") => "not authorized",
                    _ => "connection failed",
                })
            }
        };
        {
            let mut vpn = self.vpn.lock().expect("mutex poisoned");
            if vpn.waiting.get(uuid) == Some(&attempt) {
                vpn.waiting.remove(uuid);
                if let Err(message) = outcome {
                    warn!("connect_vpn({:?}) failed: {message}", profile.id);
                    vpn.error = Some(VpnError { uuid: uuid.into(), message: message.into() });
                }
            }
        }
        let _ = self.events.send(NetworkSignal::Changed);
    }

    pub async fn disconnect_vpn(&self, uuid: &str) {
        self.vpn.lock().expect("mutex poisoned").waiting.remove(uuid);
        for (path, _, _) in self.active_connections().await.into_iter().filter(|(_, active, _)| active == uuid) {
            if let Err(err) = self.nm.deactivate_connection(&path).await {
                warn!("failed to deactivate VPN {uuid}: {err}");
            }
        }
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
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use zbus::zvariant::{ObjectPath, Value};

    use super::*;
    use crate::capabilities::network::connect::tests::{ListedProfiles, attempting};
    use crate::capabilities::test_support::within;

    const ACTIVE: &str = "/org/freedesktop/NetworkManager/ActiveConnection/3";
    const PROFILE: &str = "/org/freedesktop/NetworkManager/Settings/5";

    fn settings(kind: &str) -> HashMap<String, HashMap<String, OwnedValue>> {
        let field = |value: &str| OwnedValue::try_from(Value::from(value)).unwrap();
        HashMap::from([(
            "connection".to_string(),
            HashMap::from([("type".into(), field(kind)), ("id".into(), field("work")), ("uuid".into(), field("u-1"))]),
        )])
    }

    #[test]
    fn only_vpn_and_wireguard_profiles_are_listed() {
        assert_eq!(profile_of(&settings("vpn")).unwrap().kind, VpnKind::Vpn);
        assert_eq!(profile_of(&settings("wireguard")).unwrap().kind, VpnKind::Wireguard);
        assert_eq!(profile_of(&settings("802-11-wireless")), None);
    }

    /// Counts `DeactivateConnection` calls.
    struct FakeNm(Arc<AtomicUsize>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl FakeNm {
        #[zbus(property)]
        fn active_connections(&self) -> Vec<OwnedObjectPath> {
            vec![OwnedObjectPath::try_from(ACTIVE).unwrap()]
        }

        fn activate_connection(
            &self,
            _profile: ObjectPath<'_>,
            _device: ObjectPath<'_>,
            _ap: ObjectPath<'_>,
        ) -> OwnedObjectPath {
            OwnedObjectPath::try_from(ACTIVE).unwrap()
        }

        fn deactivate_connection(&self, _active: ObjectPath<'_>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeProfile;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl FakeProfile {
        fn get_settings(&self) -> HashMap<String, HashMap<String, OwnedValue>> {
            settings("vpn")
        }
    }

    /// An activation reading `state`, whose `State` read first queues `then`, as NM does.
    struct FakeActive {
        state: u32,
        then: Option<(u32, u32)>,
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
    }

    async fn controller(active: FakeActive) -> (NetworkController, Arc<AtomicUsize>, zbus::Connection) {
        let deactivated = Arc::<AtomicUsize>::default();
        let nm = FakeNm(Arc::clone(&deactivated));
        let (controller, _events, peer) = attempting(|peer| {
            peer.serve_at("/org/freedesktop/NetworkManager", nm)?
                .serve_at(
                    "/org/freedesktop/NetworkManager/Settings",
                    ListedProfiles(vec![PROFILE.try_into().unwrap()]),
                )?
                .serve_at(PROFILE, FakeProfile)?
                .serve_at(ACTIVE, active)
        })
        .await;
        controller.refresh_profiles().await;
        (controller, deactivated, peer)
    }

    fn vpn_error(controller: &NetworkController) -> Option<String> {
        controller.vpn.lock().unwrap().error.as_ref().map(|error| error.message.clone())
    }

    #[tokio::test]
    async fn a_listed_profile_reads_activating_and_its_activation_succeeds() {
        let (controller, _, _peer) = controller(FakeActive { state: 1, then: Some((2, 0)) }).await;
        assert_eq!(
            controller.build_vpns().await,
            [VpnInfo { id: "work".into(), kind: VpnKind::Vpn, uuid: "u-1".into(), active: false, activating: true }]
        );
        tokio::time::timeout(Duration::from_secs(1), controller.connect_vpn("u-1")).await.unwrap();
        assert_eq!(vpn_error(&controller), None);
        assert!(controller.vpn.lock().unwrap().waiting.is_empty());
    }

    #[tokio::test]
    async fn a_failure_queued_ahead_of_the_state_read_keeps_its_reason() {
        let (controller, _, _peer) = controller(FakeActive { state: 4, then: Some((4, 10)) }).await;
        tokio::time::timeout(Duration::from_secs(1), controller.connect_vpn("u-1")).await.unwrap();
        assert_eq!(vpn_error(&controller).as_deref(), Some("login failed"));
    }

    #[tokio::test]
    async fn only_networkmanager_can_report_an_activation_change() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let nm = bus.builder().name("org.freedesktop.NetworkManager").unwrap().build().await.unwrap();
        let (ours, impostor) = (bus.connection().await, bus.connection().await);
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let owner = Some(nm.unique_name().unwrap().to_owned());
        forward_active_changes(&ours, Arc::new(Mutex::new(owner)), events).await;
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
        forward_active_changes(&ours, Arc::new(Mutex::new(owner)), events).await;
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
        let (controller, deactivated, _peer) = controller(FakeActive { state: 1, then: None }).await;
        tokio::time::pause();
        tokio::join!(controller.connect_vpn("u-1"), controller.disconnect_vpn("u-1"));
        assert_eq!(vpn_error(&controller), None, "the user stopped it");

        controller.connect_vpn("u-1").await;
        assert_eq!(vpn_error(&controller).as_deref(), Some("connection timed out"));
        assert_eq!(deactivated.load(Ordering::SeqCst), 3, "the disconnect, then each ceiling");
    }
}

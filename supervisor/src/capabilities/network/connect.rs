//! NetworkManager activation, profile updates and activation verdicts for `network:connect`.
//! `intent.rs` shapes a join intent into NetworkManager's dict.
use std::collections::HashMap;

use futures_util::StreamExt;
use shared::{Zeroize, debug, warn};
use zbus::zvariant::OwnedObjectPath;

use super::devices::WifiDevice;
use super::intent::{ConnectError, ConnectionIntent, activation_verdict, build_connection_dict, connection_intent};
use super::join::InFlight;
use super::profiles::merge_psk;
use super::proxies::{ACTIVE_STATE_ACTIVATED, ACTIVE_STATE_DEACTIVATED, SettingsConnectionProxy};
use super::{NetworkController, NetworkSignal, PendingNetworkConnect, root_object_path};
use crate::capabilities::bind;

/// Backstop for an activation that stops answering without a verdict.
const ACTIVATION_CEILING: std::time::Duration = std::time::Duration::from_secs(45);

/// Kept separate from `Device` because zbus names both signal types `StateChanged`.
#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait ActiveConnection {
    #[zbus(signal, name = "StateChanged")]
    fn active_state_changed(&self, state: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

impl NetworkController {
    pub(super) fn pending_wifi(&self, pending: &PendingNetworkConnect) -> Option<WifiDevice> {
        self.wifi(Some(&pending.device_id)).filter(|wifi| Some(&wifi.device_path) == pending.device_path.as_ref())
    }

    /// Decides whether a stashed `network:connect` can complete or needs a password.
    ///
    /// Saved profiles and open APs connect without a key. Unknown security prompts for one.
    /// Hidden SSIDs have unknown security. Activation checks profiles again after the prompt.
    pub async fn resolve_connect_intent(&self) {
        let Some(pending) = self.pending_intent() else {
            return;
        };
        let Some(wifi) = self.pending_wifi(&pending) else {
            if let Some(pending) = self.take_current_intent(&pending) {
                self.connect(pending, shared::Zeroizing::new(Vec::new())).await;
            }
            return;
        };
        let saved = !self.saved_profiles_for_device(&pending.ssid, &wifi, pending.hidden).await.is_empty();
        // Unknown AP security takes the password branch.
        let secure = pending.hidden || self.access_point(&pending.device_id, &pending.ssid).is_none_or(|ap| ap.secure);
        if !saved && secure {
            debug!("connect {:?}: saved={saved} secure={secure}, asking for a password", pending.ssid);
            self.request_password(&pending);
            return;
        }
        debug!("connect {:?}: saved={saved} secure={secure}, connecting directly", pending.ssid);
        // A later click may have replaced the intent during the profile read.
        let taken = self.take_current_intent(&pending);
        if let Some(pending) = taken {
            self.connect(pending, shared::Zeroizing::new(Vec::new())).await;
        }
    }

    /// Clears the current join before stopping its NM activation. A late acceptance sees the
    /// advanced attempt ID and stops itself instead.
    pub fn abort_connect(&self) {
        let aborted = self.join.lock().expect("mutex poisoned").abort();
        let Some(in_flight) = aborted else { return };
        debug!("the join in flight was aborted");
        let _ = self.events.send(NetworkSignal::Changed);
        if let Some(in_flight) = in_flight {
            let controller = self.clone();
            tokio::spawn(async move { controller.stop(&in_flight).await });
        }
    }

    /// Deletes a profile this join created; otherwise deactivates its connection.
    pub(super) async fn stop(&self, in_flight: &InFlight) {
        let result = match &in_flight.created {
            Some(created) => match bind::<SettingsConnectionProxy>(&self.connection, created.clone()).await {
                Ok(connection) => connection.delete().await,
                Err(err) => Err(err),
            },
            None => self.nm.deactivate_connection(&in_flight.active).await,
        };
        if let Err(err) = result {
            warn!("failed to stop the aborted join {}: {err}", in_flight.active);
        }
    }

    /// Saves a typed key only after NM accepts the join.
    ///
    /// ponytail: a rejected or aborted key stays in memory, shadowing the good one, until NM
    /// restarts or a later key is accepted. `ReloadConnections` would drop it, but polkit asks
    /// `auth_admin_keep` for it. Upgrade path: restore the old key from `GetSecrets`.
    async fn save_typed_key(&self, in_flight: &InFlight) {
        let Some(profile) = &in_flight.unsaved else { return };
        let result = match bind::<SettingsConnectionProxy>(&self.connection, profile.clone()).await {
            Ok(connection) => connection.save().await,
            Err(err) => Err(err),
        };
        if let Err(err) = result {
            warn!("failed to save the accepted key for {profile}: {err}");
        }
    }

    /// Owns the secret in `Zeroizing` so cancellation before activation drops it safely.
    pub async fn connect(&self, pending: PendingNetworkConnect, secret: shared::Zeroizing<Vec<u8>>) {
        let Some(attempt) = self.begin_connect(&pending) else { return };
        let result = self.connect_inner(&pending, &secret).await;
        // Drop plaintext before reporting the result.
        drop(secret);
        match result {
            // NM accepted the request, not completed it; the activation reports the verdict.
            Ok(in_flight) if self.accept(attempt, &in_flight) => self.watch_activation(attempt, in_flight, pending),
            Ok(in_flight) => self.stop(&in_flight).await,
            Err(err) => {
                warn!("connect(ssid={:?}) failed: {err}", pending.ssid);
                self.finish_connect(attempt, &pending, Some(err.to_string()), false);
            }
        }
    }

    /// Reopens the prompt on a rejected key, except for 802.1X, which takes no typed PSK.
    fn watch_activation(&self, attempt: u64, in_flight: InFlight, pending: PendingNetworkConnect) {
        let controller = self.clone();
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(ACTIVATION_CEILING, controller.activation_outcome(&in_flight.active, &pending))
                    .await
                    .ok();
            let activated = matches!(outcome, Some(Ok(())));
            let (error, rejected_key) = activation_verdict(outcome);
            let ask_password = rejected_key
                && match controller.pending_wifi(&pending) {
                    Some(wifi) => !controller
                        .saved_profiles_for_device(&pending.ssid, &wifi, pending.hidden)
                        .await
                        .iter()
                        .any(|profile| profile.settings.contains_key("802-1x")),
                    None => false,
                };
            // A superseded attempt must not persist its key.
            if controller.finish_connect(attempt, &pending, error, ask_password) && activated {
                controller.save_typed_key(&in_flight).await;
            }
        });
    }

    /// Returns the device's failure reason; the active connection reports only disconnection.
    /// NM emits the device signal first, so `biased` reads it before the verdict.
    async fn activation_outcome(
        &self,
        active: &OwnedObjectPath,
        pending: &PendingNetworkConnect,
    ) -> Result<(), Option<u32>> {
        let proxy = match bind::<ActiveConnectionProxy>(&self.connection, active.clone()).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("failed to bind the active connection {active}: {err}");
                return Err(None);
            }
        };
        let Some(wifi) = self.pending_wifi(pending) else {
            return Err(None);
        };
        // Use the signals, not `receive_state_changed()`: the property stream gives no reason.
        let (mut changes, mut device_changes) =
            match tokio::try_join!(proxy.receive_active_state_changed(), wifi.device.receive_device_state_changed()) {
                Ok(streams) => streams,
                Err(err) => {
                    warn!("failed to subscribe to StateChanged for {active}: {err}");
                    return Err(None);
                }
            };

        // Subscription follows activation; read the property to catch an early verdict.
        // ponytail: an early failure loses its reason. Upgrade path: subscribe before activation.
        match proxy.state().await {
            Ok(ACTIVE_STATE_ACTIVATED) => return Ok(()),
            Ok(ACTIVE_STATE_DEACTIVATED) => return Err(None),
            _ => {}
        }

        let mut reason = None;
        loop {
            tokio::select! {
                biased;
                Some(change) = device_changes.next() => {
                    // NM follows FAILED with DISCONNECTED(NONE); keep the failure reason.
                    if let Ok(args) = change.args()
                        && args.reason != 0
                    {
                        reason = Some(args.reason);
                    }
                }
                change = changes.next() => {
                    // `None`: the object disappeared without a terminal state.
                    let Some(change) = change else { return Err(reason) };
                    let Ok(args) = change.args() else { continue };
                    match args.state {
                        ACTIVE_STATE_ACTIVATED => return Ok(()),
                        ACTIVE_STATE_DEACTIVATED => return Err(reason),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn connect_inner(&self, pending: &PendingNetworkConnect, secret: &[u8]) -> Result<InFlight, ConnectError> {
        let wifi =
            self.pending_wifi(pending).ok_or_else(|| ConnectError::UnknownWifiDevice(pending.device_id.clone()))?;
        let mut intent = connection_intent(&pending.ssid, pending.hidden, secret)?;
        let result = self.activate_intent(&intent, &wifi).await;
        // Dicts borrow this plaintext PSK and are consumed now, so zeroize it explicitly
        // (ADR-0005/ADR-0014) rather than relying on Drop.
        if let Some(psk) = intent.psk.as_mut() {
            psk.zeroize();
        }
        result
    }

    /// Reuses a saved profile; NM would otherwise create duplicates for the same SSID.
    async fn activate_intent(&self, intent: &ConnectionIntent, wifi: &WifiDevice) -> Result<InFlight, ConnectError> {
        let Some(saved) = self.saved_profiles_for_device(&intent.ssid, wifi, intent.hidden).await.into_iter().next()
        else {
            let dict = build_connection_dict(intent);
            let (created, active, _) = self
                .nm
                .add_and_activate_connection2(dict, &wifi.device_path, &root_object_path(), HashMap::new())
                .await?;
            return Ok(InFlight { active, created: Some(created), unsaved: None });
        };

        // Keep a corrected key in memory until activation succeeds.
        let unsaved = match intent.psk.as_ref().and_then(|psk| merge_psk(&saved.settings, psk)) {
            Some(merged) => {
                saved.connection.update_unsaved(merged).await?;
                Some(saved.path.clone())
            }
            None => None,
        };
        let active = self.nm.activate_connection(&saved.path, &wifi.device_path, &root_object_path()).await?;
        Ok(InFlight { active, created: None, unsaved })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::capabilities::network::NetworkState;
    use crate::capabilities::network::proxies::{DeviceProxy, NetworkManagerProxy, SettingsProxy, WirelessProxy};
    use crate::capabilities::test_support::p2p_pair_serving;

    /// A controller bound to a peer serving what `serve` installs.
    async fn attempting<F>(serve: F) -> (NetworkController, UnboundedReceiver<NetworkSignal>, zbus::Connection)
    where
        F: FnOnce(zbus::connection::Builder<'static>) -> zbus::Result<zbus::connection::Builder<'static>>,
    {
        let (connection, peer) = p2p_pair_serving(serve).await;
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let controller = NetworkController {
            nm: NetworkManagerProxy::new(&connection).await.expect("binding makes no call"),
            settings: SettingsProxy::new(&connection).await.expect("binding makes no call"),
            connection,
            devices: Arc::default(),
            access_points: Arc::default(),
            saved_ssids: Arc::default(),
            state: Arc::new(Mutex::new(NetworkState::default())),
            join: Arc::default(),
            events,
        };
        (controller, receiver, peer)
    }

    fn joined(n: u32) -> InFlight {
        let active = OwnedObjectPath::try_from(format!("/org/freedesktop/NetworkManager/ActiveConnection/{n}"))
            .expect("valid object path");
        InFlight { active, created: None, unsaved: None }
    }

    /// One saved profile, counting each `Save`.
    struct FakeProfile(Arc<std::sync::atomic::AtomicUsize>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl FakeProfile {
        async fn save(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn get_settings(&self) -> HashMap<String, HashMap<String, zbus::zvariant::OwnedValue>> {
            HashMap::from([(
                "802-11-wireless".into(),
                HashMap::from([(
                    "ssid".into(),
                    zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from(b"home".to_vec())).unwrap(),
                )]),
            )])
        }
    }

    struct AvailableProfile(OwnedObjectPath);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl AvailableProfile {
        #[zbus(property)]
        fn available_connections(&self) -> Vec<OwnedObjectPath> {
            vec![self.0.clone()]
        }
    }

    struct ListedProfiles(Vec<OwnedObjectPath>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
    impl ListedProfiles {
        fn list_connections(&self) -> Vec<OwnedObjectPath> {
            self.0.clone()
        }
    }

    struct SlowDelete {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: tokio::sync::Mutex<tokio::sync::oneshot::Receiver<()>>,
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl SlowDelete {
        async fn delete(&self) {
            self.entered.lock().unwrap().take().unwrap().send(()).unwrap();
            let mut release = self.release.lock().await;
            (&mut *release).await.unwrap();
        }
    }

    async fn wifi(controller: &NetworkController, id: &str, path: OwnedObjectPath) -> WifiDevice {
        WifiDevice {
            id: id.into(),
            device: bind::<DeviceProxy>(&controller.connection, path.clone()).await.unwrap(),
            wireless: bind::<WirelessProxy>(&controller.connection, path.clone()).await.unwrap(),
            device_path: path,
        }
    }

    #[tokio::test]
    async fn an_accepted_join_saves_only_the_key_it_typed() {
        const PROFILE: &str = "/org/freedesktop/NetworkManager/Settings/9";
        let saves = Arc::default();
        let profile = FakeProfile(Arc::clone(&saves));
        let (controller, _receiver, _peer) = attempting(|peer| peer.serve_at(PROFILE, profile)).await;
        let typed = InFlight { unsaved: Some(OwnedObjectPath::try_from(PROFILE).unwrap()), ..joined(1) };

        controller.save_typed_key(&joined(1)).await;
        controller.save_typed_key(&typed).await;

        assert_eq!(saves.load(std::sync::atomic::Ordering::SeqCst), 1, "a join that typed no key saves nothing");
    }

    #[tokio::test]
    async fn nm_available_profile_wins_over_another_saved_profile() {
        const FIRST: &str = "/org/freedesktop/NetworkManager/Settings/1";
        const CHOSEN: &str = "/org/freedesktop/NetworkManager/Settings/2";
        let (controller, _receiver, _peer) = attempting(|peer| {
            peer.serve_at(FIRST, FakeProfile(Arc::default()))?
                .serve_at(CHOSEN, FakeProfile(Arc::default()))?
                .serve_at(
                    "/org/freedesktop/NetworkManager/Settings",
                    ListedProfiles(vec![
                        OwnedObjectPath::try_from(FIRST).unwrap(),
                        OwnedObjectPath::try_from(CHOSEN).unwrap(),
                    ]),
                )?
                .serve_at(DEVICE, AvailableProfile(OwnedObjectPath::try_from(CHOSEN).unwrap()))
        })
        .await;
        let path = OwnedObjectPath::try_from(DEVICE).unwrap();
        let wifi = wifi(&controller, "wlan0", path).await;
        let profiles = controller.saved_profiles_for_device("home", &wifi, false).await;
        assert_eq!(profiles.iter().map(|profile| profile.path.as_str()).collect::<Vec<_>>(), [CHOSEN]);
    }

    #[tokio::test]
    async fn a_new_join_waits_for_the_old_profiles_deletion() {
        const OLD: &str = "/org/freedesktop/NetworkManager/Settings/7";
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let old = SlowDelete {
            entered: std::sync::Mutex::new(Some(entered_tx)),
            release: tokio::sync::Mutex::new(release_rx),
        };
        let (controller, mut events, _peer) = attempting(|peer| peer.serve_at(OLD, old)).await;
        let first = PendingNetworkConnect {
            attempt: 0,
            ssid: "home".into(),
            hidden: false,
            device_id: String::new(),
            device_path: None,
        };
        {
            let mut join = controller.join.lock().unwrap();
            join.stash(first);
            let first = join.pending().unwrap();
            join.take_current(&first);
            let attempt = join.begin(&first).unwrap();
            let accepted = InFlight { created: Some(OwnedObjectPath::try_from(OLD).unwrap()), ..joined(1) };
            assert!(join.accept(attempt, &accepted));
        }

        controller.stash_connect_intent("bad".into(), false, Some("wlan9"));
        let mut state = NetworkState::default();
        controller.join.lock().unwrap().overlay(&mut state);
        assert_eq!(state.connecting_ssid.as_deref(), Some("home"));

        controller.stash_connect_intent("next".into(), false, None);
        entered_rx.await.unwrap();
        assert_eq!(events.recv().await, Some(NetworkSignal::Changed));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(30), events.recv()).await.is_err());
        release_tx.send(()).unwrap();
        assert_eq!(events.recv().await, Some(NetworkSignal::Changed));
        assert_eq!(events.recv().await, Some(NetworkSignal::Changed));
        controller.join.lock().unwrap().overlay(&mut state);
        assert!(state.connect_error.is_some());
    }

    const ACTIVE: &str = "/org/freedesktop/NetworkManager/ActiveConnection/1";

    const DEVICE: &str = "/org/freedesktop/NetworkManager/Devices/4";

    /// An activation whose `State` read queues, ahead of its reply, what NM emits for a rejected key:
    /// device FAILED(NO_SECRETS), device DISCONNECTED(NONE), then the activation's DEACTIVATED.
    struct RejectedKey;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
    impl RejectedKey {
        #[zbus(property)]
        async fn state(&self, #[zbus(connection)] connection: &zbus::Connection) -> u32 {
            let device = "org.freedesktop.NetworkManager.Device";
            for body in [(120u32, 50u32, 7u32), (30, 120, 0)] {
                connection.emit_signal(None::<&str>, DEVICE, device, "StateChanged", &body).await.unwrap();
            }
            let active = "org.freedesktop.NetworkManager.Connection.Active";
            connection.emit_signal(None::<&str>, ACTIVE, active, "StateChanged", &(4u32, 3u32)).await.unwrap();
            1
        }
    }

    #[tokio::test]
    async fn a_rejected_key_keeps_its_reason_past_the_disconnect_nm_queues_after_it() {
        let (controller, _receiver, _peer) = attempting(|peer| peer.serve_at(ACTIVE, RejectedKey)).await;
        let device_path = OwnedObjectPath::try_from(DEVICE).unwrap();
        let other_path = OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Devices/5").unwrap();
        for (id, path) in [("wlan1", other_path.clone()), ("wlan0", device_path)] {
            let device = wifi(&controller, id, path).await;
            controller.devices.lock().unwrap().wifi.push(device);
        }

        let mut pending = PendingNetworkConnect {
            attempt: 0,
            ssid: "home".into(),
            hidden: false,
            device_id: "wlan0".into(),
            device_path: Some(OwnedObjectPath::try_from(DEVICE).unwrap()),
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            controller.activation_outcome(&OwnedObjectPath::try_from(ACTIVE).unwrap(), &pending),
        )
        .await
        .unwrap();

        assert_eq!(outcome, Err(Some(7)), "NO_SECRETS, so the password prompt comes back");
        pending.device_path = Some(other_path);
        assert!(matches!(controller.connect_inner(&pending, &[]).await, Err(ConnectError::UnknownWifiDevice(_))));
    }
}

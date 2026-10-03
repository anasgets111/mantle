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

/// [`NetworkController::watch_activation`]'s backstop timeout. NetworkManager normally gives up
/// well inside 45s and reports `StateChanged`; this covers an activation object that stops
/// answering without pre-empting NM or leaving a spinner stuck.
const ACTIVATION_CEILING: std::time::Duration = std::time::Duration::from_secs(45);

/// `Connection.Active`, kept out of `proxies.rs`: zbus names signal types after the D-Bus member,
/// so this `StateChanged` would redefine `Device`'s there. The signal is renamed off the `state`
/// property's `receive_state_changed`.
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
    /// Decides whether a stashed `network:connect` can complete or needs a password.
    ///
    /// A saved profile or open AP connects on click with no typed secret. Only a secured
    /// network without a profile sets [`NetworkState::password_ssid`](super::NetworkState::password_ssid) and waits for
    /// `secure_submit(network, connect)`.
    ///
    /// Without those branches, `network:connect` only stashes and every click leaves an intent
    /// nothing consumes.
    ///
    /// Hidden networks take the password branch because no in-range AP reports their security.
    /// Guessing wrong costs one
    /// keystroke on an open network, versus an unjoinable secured one.
    ///
    /// The SSID is looked up again in `activate_intent`, an extra `ListConnections` walk of a few
    /// profiles. Passing the match through `connect` saves about a millisecond at the cost of three
    /// signatures.
    pub async fn resolve_connect_intent(&self) {
        let Some(pending) = self.pending_intent() else {
            return;
        };
        let saved = !self.saved_profiles_for_ssid(&pending.ssid, "connect").await.is_empty();
        // No in-range AP means secured, like a hidden SSID: nothing can say otherwise.
        let secure = pending.hidden
            || self
                .state
                .lock()
                .unwrap()
                .available_networks
                .iter()
                .find(|ap| ap.ssid == pending.ssid)
                .is_none_or(|ap| ap.secure);
        if !saved && secure {
            // Log the fork: saved-profile and security facts come from different sources, so a
            // missing prompt otherwise leaves three plausible causes.
            debug!("connect {:?}: saved={saved} secure={secure}, asking for a password", pending.ssid);
            self.request_password(&pending);
            return;
        }
        debug!("connect {:?}: saved={saved} secure={secure}, connecting directly", pending.ssid);
        // Only this click's intent: another connect may have replaced it while the lookup was on the
        // wire, and that one resolves itself.
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

    /// Stops an aborted join. It deletes a profile the join created, which also ends the activation
    /// and leaves no half-typed key saved; a join through an existing profile is only deactivated.
    async fn stop(&self, in_flight: &InFlight) {
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

    /// Writes a typed key to disk once NM accepted the join. `UpdateUnsaved` held it in memory until
    /// then, so a rejected key never replaces a good one on disk, where `GetSettings` could not read
    /// it back. No-op for a join that typed no key.
    ///
    /// ponytail: a rejected or aborted key stays in memory, shadowing the good one, until NM
    /// restarts or a later key is accepted. `ReloadConnections` would drop it, but polkit asks
    /// `auth_admin_keep` for it, an admin password per typo. Upgrade path: `GetSecrets` before the
    /// update, restored on failure.
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

    /// Turns `pending` and `secret` (empty open, non-empty WPA-PSK) into
    /// `AddAndActivateConnection2`'s dict. The caller `mem::take`s `secret` from the wire frame,
    /// making this function its owner (ADR-0005/ADR-0014).
    ///
    /// `Zeroizing`, not a bare `Vec`, for the reason `pam_worker`'s two entry points take one: this
    /// runs in a spawned task, and cancelling it mid-activation drops the future without running
    /// anything written after the `await`.
    pub async fn connect(&self, pending: PendingNetworkConnect, secret: shared::Zeroizing<Vec<u8>>) {
        let attempt = self.begin_connect(&pending.ssid);
        let result = self.connect_inner(&pending, &secret).await;
        // Straight after the read, not at end of scope: the reporting below logs and takes a lock,
        // and none of it needs the plaintext alive.
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

    /// Watches one activation in the background. A rejected key reopens the password prompt, or every
    /// later click would reuse the saved bad key. Not for 802.1X, whose profile never takes a typed
    /// PSK, so the prompt would loop.
    fn watch_activation(&self, attempt: u64, in_flight: InFlight, pending: PendingNetworkConnect) {
        let controller = self.clone();
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(ACTIVATION_CEILING, controller.activation_outcome(&in_flight.active)).await.ok();
            let activated = matches!(outcome, Some(Ok(())));
            let (error, rejected_key) = activation_verdict(outcome);
            let ask_password = rejected_key
                && !controller
                    .saved_profiles_for_ssid(&pending.ssid, "connect")
                    .await
                    .iter()
                    .any(|profile| profile.settings.contains_key("802-1x"));
            // Saved only once this attempt is confirmed current: an abort or a newer join between
            // the verdict and here must not persist the key it typed.
            if controller.finish_connect(attempt, &pending, error, ask_password) && activated {
                controller.save_typed_key(&in_flight).await;
            }
        });
    }

    /// `Ok` on `ACTIVATED`. `Err` on deactivation, carrying the Wi-Fi device's last `StateChanged`
    /// reason. The active connection's own reason cannot say a key was rejected: NM reports every
    /// device failure to it as `DEVICE_DISCONNECTED` (`nm-act-request.c`). NM emits the device
    /// signal first (`_set_state_full`), and `biased` reads it first.
    async fn activation_outcome(&self, active: &OwnedObjectPath) -> Result<(), Option<u32>> {
        let proxy = match bind::<ActiveConnectionProxy>(&self.connection, active.clone()).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("failed to bind the active connection {active}: {err}");
                return Err(None);
            }
        };
        let Some(wifi) = self.wifi() else { return Err(None) };
        // Use the signals, not `receive_state_changed()`: the property stream gives no reason.
        let (mut changes, mut device_changes) =
            match tokio::try_join!(proxy.receive_active_state_changed(), wifi.device.receive_device_state_changed()) {
                Ok(streams) => streams,
                Err(err) => {
                    warn!("failed to subscribe to StateChanged for {active}: {err}");
                    return Err(None);
                }
            };

        // Subscription follows activation, so a verdict can land in the gap. Read the property
        // once.
        //
        // ponytail: a failure in that gap loses its reason and reports the generic line; only the
        // signal carries it. Success does not, and is the likelier race.
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
                    // Leaving FAILED, NM queues DISCONNECTED with reason NONE (`nm-device.c`), and
                    // `biased` can drain both before the verdict; NONE must not erase the reason.
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
        let wifi = self.wifi().ok_or(ConnectError::NoWifiDevice)?;
        let mut intent = connection_intent(&pending.ssid, pending.hidden, secret)?;
        let result = self.activate_intent(&intent, &wifi).await;
        // Dicts borrow this plaintext PSK and are consumed now, so zeroize it explicitly
        // (ADR-0005/ADR-0014) rather than relying on Drop.
        if let Some(psk) = intent.psk.as_mut() {
            psk.zeroize();
        }
        result
    }

    /// Joins `intent`'s network, reusing a saved profile when present. NM does not deduplicate:
    /// `AddAndActivateConnection2` accepts another profile with the same id and SSID, so creating
    /// unconditionally left stale duplicates that autoconnect could choose. Returns the activation
    /// where its outcome is reported, and the profile it created, if it created one.
    async fn activate_intent(&self, intent: &ConnectionIntent, wifi: &WifiDevice) -> Result<InFlight, ConnectError> {
        let Some(saved) = self.saved_profiles_for_ssid(&intent.ssid, "connect").await.into_iter().next() else {
            let dict = build_connection_dict(intent);
            let (created, active, _) = self
                .nm
                .add_and_activate_connection2(dict, &wifi.device_path, &root_object_path(), HashMap::new())
                .await?;
            return Ok(InFlight { active, created: Some(created), unsaved: None });
        };

        // A typed password corrects the saved key; otherwise a bad profile could only be forgotten
        // and re-added. In memory only until NM accepts it (`save_typed_key`), so a typo never
        // replaces a good key on disk.
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
        let wifi = WifiDevice {
            device: bind::<DeviceProxy>(&controller.connection, device_path.clone()).await.unwrap(),
            wireless: bind::<WirelessProxy>(&controller.connection, device_path.clone()).await.unwrap(),
            device_path,
        };
        controller.devices.lock().unwrap().wifi = Some(wifi);

        let outcome = controller.activation_outcome(&OwnedObjectPath::try_from(ACTIVE).unwrap()).await;

        assert_eq!(outcome, Err(Some(7)), "NO_SECRETS, so the password prompt comes back");
    }
}

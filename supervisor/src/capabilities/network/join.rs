//! The pending network intent, password prompt and latest join attempt. NetworkManager owns the
//! activation and profile operations in `connect.rs`; this state owns their ordering.

use zbus::zvariant::OwnedObjectPath;

use super::{JoinError, NetworkController, NetworkSignal, NetworkState, PendingNetworkConnect};
use shared::debug;

/// A join NetworkManager has accepted. Only the join that created a profile may delete it on abort.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct InFlight {
    pub(super) active: OwnedObjectPath,
    pub(super) created: Option<OwnedObjectPath>,
    pub(super) unsaved: Option<OwnedObjectPath>,
}

/// Where a join stands; `Connecting` holds the accepted join so abort can stop it.
#[derive(Debug, Default)]
enum Phase {
    #[default]
    Idle,
    Connecting {
        ssid: String,
        joined: Option<InFlight>,
    },
    AwaitingPassword {
        ssid: String,
        error: Option<JoinError>,
    },
    Failed(JoinError),
}

/// One lock owns the pending intent, phase, attempt ID and accepted join. An old verdict cannot
/// consume a newer intent, including an abort followed by another click on the same SSID.
#[derive(Debug, Default)]
pub(super) struct JoinState {
    pending: Option<PendingNetworkConnect>,
    target_id: Option<String>,
    id: u64,
    phase: Phase,
}

impl JoinState {
    pub(super) fn stash(&mut self, mut pending: PendingNetworkConnect) -> Option<InFlight> {
        let old = self.abort().flatten();
        self.id += 1;
        pending.attempt = self.id;
        self.target_id = Some(pending.device_id.clone());
        self.pending = Some(pending);
        old
    }

    pub(super) fn pending(&self) -> Option<PendingNetworkConnect> {
        self.pending.clone()
    }

    pub(super) fn take_current(&mut self, pending: &PendingNetworkConnect) -> Option<PendingNetworkConnect> {
        self.pending.take_if(|current| current == pending)
    }

    pub(super) fn take_prompted(&mut self) -> Option<PendingNetworkConnect> {
        self.pending.take_if(|_| matches!(self.phase, Phase::AwaitingPassword { .. }))
    }

    pub(super) fn request_password(&mut self, pending: &PendingNetworkConnect) -> bool {
        if self.pending.as_ref() != Some(pending) {
            return false;
        }
        self.phase = Phase::AwaitingPassword { ssid: pending.ssid.clone(), error: None };
        true
    }

    pub(super) fn cancel(&mut self) -> bool {
        let pending = self.pending.take();
        let settled = matches!(self.phase, Phase::AwaitingPassword { .. } | Phase::Failed(_));
        if pending.is_none() && !settled {
            return false;
        }
        // A running join stays in `Connecting` so abort can still stop its activation.
        if settled {
            self.phase = Phase::Idle;
        }
        self.target_id = None;
        self.id += 1;
        true
    }

    /// `None` means no attempt was active; `Some(None)` means one was active but NM had not yet
    /// accepted it. Both cases differ from an accepted join that the caller must stop.
    pub(super) fn abort(&mut self) -> Option<Option<InFlight>> {
        self.target_id.as_ref()?;
        let old = std::mem::take(self);
        self.id = old.id + 1;
        Some(if let Phase::Connecting { joined, .. } = old.phase { joined } else { None })
    }

    pub(super) fn begin(&mut self, pending: &PendingNetworkConnect) -> Option<u64> {
        if self.id != pending.attempt {
            return None;
        }
        self.phase = Phase::Connecting { ssid: pending.ssid.clone(), joined: None };
        Some(self.id)
    }

    pub(super) fn accept(&mut self, attempt: u64, in_flight: &InFlight) -> bool {
        let Phase::Connecting { joined, .. } = &mut self.phase else { return false };
        if self.id != attempt {
            return false;
        }
        *joined = Some(in_flight.clone());
        true
    }

    pub(super) fn finish(
        &mut self,
        attempt: u64,
        pending: &PendingNetworkConnect,
        error: Option<String>,
        ask_password: bool,
    ) -> bool {
        if self.id != attempt {
            return false;
        }
        let error = error.map(|message| JoinError { ssid: pending.ssid.clone(), message });
        self.phase = if ask_password && self.pending.is_none() {
            self.pending = Some(pending.clone());
            Phase::AwaitingPassword { ssid: pending.ssid.clone(), error }
        } else {
            error.map_or(Phase::Idle, Phase::Failed)
        };
        true
    }

    pub(super) fn overlay(&self, state: &mut NetworkState) {
        let (connecting_ssid, connect_error, password_ssid) = match &self.phase {
            Phase::Idle => (None, None, None),
            Phase::Connecting { ssid, .. } => (Some(ssid.clone()), None, None),
            Phase::AwaitingPassword { ssid, error } => (None, error.clone(), Some(ssid.clone())),
            Phase::Failed(error) => (None, Some(error.clone()), None),
        };
        for device in &mut state.wifi_devices {
            let current = self.target_id.as_ref() == Some(&device.id);
            device.connecting_ssid = current.then(|| connecting_ssid.clone()).flatten();
            device.connect_error = current.then(|| connect_error.clone()).flatten();
            device.password_ssid = current.then(|| password_ssid.clone()).flatten();
        }
        state.connecting_ssid = connecting_ssid;
        state.connect_error = connect_error;
        state.password_ssid = password_ssid;
    }
}

impl NetworkController {
    /// The latest intent wins; the secret frame contains no SSID.
    pub fn stash_connect_intent(&self, ssid: String, hidden: bool, id: Option<&str>) {
        let wifi = self.wifi(id);
        if id.is_some() && wifi.is_none() {
            shared::warn!("connect_device({id:?}): Wi-Fi device is unavailable");
            return;
        }
        let pending = PendingNetworkConnect {
            attempt: 0,
            ssid,
            hidden,
            device_id: id.map(str::to_owned).or_else(|| wifi.as_ref().map(|wifi| wifi.id.clone())).unwrap_or_default(),
            device_path: wifi.map(|wifi| wifi.device_path),
        };
        let (old, attempt) = {
            let mut join = self.join.lock().expect("mutex poisoned");
            let old = join.stash(pending);
            (old, join.id)
        };
        let _ = self.events.send(NetworkSignal::Changed);
        let controller = self.clone();
        tokio::spawn(async move {
            if let Some(old) = old {
                controller.stop(&old).await;
            }
            if controller.pending_intent().is_some_and(|pending| pending.attempt == attempt) {
                controller.resolve_connect_intent().await;
            }
        });
    }

    pub fn take_prompted_intent(&self) -> Option<PendingNetworkConnect> {
        self.join.lock().expect("mutex poisoned").take_prompted()
    }

    pub(super) fn pending_intent(&self) -> Option<PendingNetworkConnect> {
        self.join.lock().expect("mutex poisoned").pending()
    }

    pub(super) fn take_current_intent(&self, pending: &PendingNetworkConnect) -> Option<PendingNetworkConnect> {
        self.join.lock().expect("mutex poisoned").take_current(pending)
    }

    pub(super) fn request_password(&self, pending: &PendingNetworkConnect) {
        if self.join.lock().expect("mutex poisoned").request_password(pending) {
            debug!("requesting password for ssid: {}", pending.ssid);
            let _ = self.events.send(NetworkSignal::Changed);
        }
    }

    pub fn cancel_connect(&self) {
        if self.join.lock().expect("mutex poisoned").cancel() {
            debug!("the pending connect was cancelled; the password prompt is down");
            let _ = self.events.send(NetworkSignal::Changed);
        }
    }

    pub(super) fn begin_connect(&self, pending: &PendingNetworkConnect) -> Option<u64> {
        let id = self.join.lock().expect("mutex poisoned").begin(pending);
        if id.is_some() {
            let _ = self.events.send(NetworkSignal::Changed);
        }
        id
    }

    pub(super) fn accept(&self, attempt: u64, in_flight: &InFlight) -> bool {
        self.join.lock().expect("mutex poisoned").accept(attempt, in_flight)
    }

    pub(super) fn finish_connect(
        &self,
        attempt: u64,
        pending: &PendingNetworkConnect,
        error: Option<String>,
        ask_password: bool,
    ) -> bool {
        let current = self.join.lock().expect("mutex poisoned").finish(attempt, pending, error, ask_password);
        if current {
            let _ = self.events.send(NetworkSignal::Changed);
        }
        current
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn home() -> PendingNetworkConnect {
        PendingNetworkConnect {
            attempt: 0,
            ssid: "home".to_string(),
            hidden: true,
            device_id: "wlan0".to_string(),
            device_path: None,
        }
    }

    pub(crate) fn joined(n: u32) -> InFlight {
        let active = OwnedObjectPath::try_from(format!("/org/freedesktop/NetworkManager/ActiveConnection/{n}"))
            .expect("valid object path");
        InFlight { active, created: None, unsaved: None }
    }

    fn wifi_state() -> NetworkState {
        NetworkState {
            wifi_devices: ["wlan0", "wlan1"]
                .map(|id| shared::state::network::WifiDeviceInfo { id: id.into(), ..Default::default() })
                .to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn a_rejected_key_reopens_the_prompt_and_clears_stale_projection() {
        let mut join = JoinState::default();
        join.stash(home());
        let pending = join.pending().unwrap();
        join.take_current(&pending);
        let attempt = join.begin(&pending).unwrap();
        assert!(join.finish(attempt, &pending, Some("wrong password".to_string()), true));

        let mut state = NetworkState {
            connecting_ssid: Some("stale".to_string()),
            password_ssid: Some("stale".to_string()),
            connect_error: Some(JoinError { ssid: "stale".into(), message: "stale".into() }),
            ..wifi_state()
        };
        state.wifi_devices[1].password_ssid = Some("stale".into());
        join.overlay(&mut state);
        assert_eq!(state.wifi_devices[0].password_ssid.as_deref(), Some("home"));
        assert_eq!(state.wifi_devices[1].password_ssid, None);
        assert_eq!(state.connecting_ssid, None);
        assert_eq!(state.password_ssid.as_deref(), Some("home"));
        assert_eq!(
            state.connect_error,
            Some(JoinError { ssid: "home".to_string(), message: "wrong password".to_string() }),
        );
        assert_eq!(join.take_prompted(), Some(pending));
        assert!(join.cancel());
        join.overlay(&mut state);
        assert_eq!((&state.connecting_ssid, &state.password_ssid, &state.connect_error), (&None, &None, &None));
        assert_eq!(join.take_prompted(), None);
        join.stash(PendingNetworkConnect { device_id: String::new(), ..home() });
        assert_eq!(join.take_prompted(), None, "a stashed intent is not a prompt");
        let missing = join.pending().unwrap();
        join.take_current(&missing);
        let attempt = join.begin(&missing).unwrap();
        assert!(join.finish(attempt, &missing, Some("no Wi-Fi device is present".into()), false));
        state.wifi_devices.clear();
        join.overlay(&mut state);
        assert_eq!(state.connect_error.as_ref().unwrap().message, "no Wi-Fi device is present");
        assert_eq!(state.password_ssid, None);
    }

    #[test]
    fn a_superseded_verdict_cannot_take_another_devices_prompt() {
        let mut join = JoinState::default();
        let office = PendingNetworkConnect {
            ssid: "office".to_string(),
            hidden: false,
            device_id: "wlan1".to_string(),
            ..home()
        };
        join.stash(home());
        let first = join.pending().unwrap();
        join.take_current(&first);
        let attempt = join.begin(&first).unwrap();
        assert!(join.accept(attempt, &joined(1)));
        assert_eq!(join.stash(office), Some(joined(1)));
        let second = join.pending().unwrap();
        assert!(join.request_password(&second));
        assert!(!join.request_password(&first));
        assert!(!join.finish(attempt, &first, Some("wrong password".into()), true));
        let mut state = wifi_state();
        join.overlay(&mut state);
        assert_eq!(state.password_ssid.as_deref(), Some("office"));
        assert_eq!((&state.connecting_ssid, &state.connect_error), (&None, &None));
        assert_eq!(state.wifi_devices[0].password_ssid, None);
        assert_eq!(state.wifi_devices[1].password_ssid.as_deref(), Some("office"));
        assert_eq!(join.take_prompted(), Some(second));
    }

    #[test]
    fn a_join_aborted_and_clicked_again_never_stands_in_for_the_new_one() {
        let mut join = JoinState::default();
        join.stash(home());
        let first = join.pending().unwrap();
        join.take_current(&first);
        let first_attempt = join.begin(&first).unwrap();
        assert_eq!(join.abort(), Some(None));
        assert!(!join.finish(first_attempt, &first, None, false), "an aborted attempt's verdict is stale");
        join.stash(home());
        let second = join.pending().unwrap();
        join.take_current(&second);
        let second_attempt = join.begin(&second).unwrap();

        assert!(!join.accept(first_attempt, &joined(1)));
        assert!(join.accept(second_attempt, &joined(2)));
        assert!(!join.finish(first_attempt, &first, Some("disconnected".into()), false));
        let mut state = wifi_state();
        join.overlay(&mut state);
        assert_eq!((state.connecting_ssid.as_deref(), state.connect_error.as_ref()), (Some("home"), None));
        assert_eq!(state.wifi_devices[0].connecting_ssid.as_deref(), Some("home"));
        assert_eq!(state.wifi_devices[1].connecting_ssid, None);
        assert!(!join.cancel(), "cancel never drops a running join");
        join.overlay(&mut state);
        assert_eq!(state.connecting_ssid.as_deref(), Some("home"));
        assert!(join.finish(second_attempt, &second, None, false));
        join.overlay(&mut state);
        assert_eq!((&state.connecting_ssid, &state.connect_error, &state.password_ssid), (&None, &None, &None));
        assert_eq!(join.abort(), Some(None), "finish dropped the accepted join");
    }
}

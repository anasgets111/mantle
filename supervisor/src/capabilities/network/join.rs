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

/// One lock owns the pending intent, prompt, attempt ID and accepted join. An old verdict cannot
/// consume a newer intent, including an abort followed by another click on the same SSID.
#[derive(Debug, Default)]
pub(super) struct JoinState {
    pending: Option<PendingNetworkConnect>,
    id: u64,
    joined: Option<InFlight>,
    connecting_ssid: Option<String>,
    connect_error: Option<JoinError>,
    password_ssid: Option<String>,
}

impl JoinState {
    pub(super) fn stash(&mut self, pending: PendingNetworkConnect) {
        self.pending = Some(pending);
    }

    pub(super) fn pending(&self) -> Option<PendingNetworkConnect> {
        self.pending.clone()
    }

    pub(super) fn take_current(&mut self, pending: &PendingNetworkConnect) -> Option<PendingNetworkConnect> {
        self.pending.take_if(|current| current == pending)
    }

    pub(super) fn take_prompted(&mut self) -> Option<PendingNetworkConnect> {
        self.pending.take_if(|pending| self.password_ssid.as_deref() == Some(pending.ssid.as_str()))
    }

    pub(super) fn request_password(&mut self, pending: &PendingNetworkConnect) -> bool {
        if self.pending.as_ref() != Some(pending) {
            return false;
        }
        self.password_ssid = Some(pending.ssid.clone());
        self.connect_error = None;
        true
    }

    pub(super) fn cancel(&mut self) -> bool {
        let pending = self.pending.take();
        if pending.is_none() && self.password_ssid.is_none() {
            return false;
        }
        self.password_ssid = None;
        self.connect_error = None;
        true
    }

    /// `None` means no attempt was active; `Some(None)` means one was active but NM had not yet
    /// accepted it. Both cases differ from an accepted join that the caller must stop.
    pub(super) fn abort(&mut self) -> Option<Option<InFlight>> {
        self.connecting_ssid.take()?;
        self.connect_error = None;
        self.id += 1;
        Some(self.joined.take())
    }

    pub(super) fn begin(&mut self, ssid: &str) -> u64 {
        self.connecting_ssid = Some(ssid.to_string());
        self.connect_error = None;
        self.password_ssid = None;
        self.id += 1;
        self.joined = None;
        self.id
    }

    pub(super) fn accept(&mut self, attempt: u64, in_flight: &InFlight) -> bool {
        if self.id != attempt {
            return false;
        }
        self.joined = Some(in_flight.clone());
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
        self.connecting_ssid = None;
        self.connect_error = error.map(|message| JoinError { ssid: pending.ssid.clone(), message });
        self.joined = None;
        if ask_password && self.pending.is_none() {
            self.password_ssid = Some(pending.ssid.clone());
            self.pending = Some(pending.clone());
        }
        true
    }

    pub(super) fn overlay(&self, state: &mut NetworkState) {
        state.connecting_ssid.clone_from(&self.connecting_ssid);
        state.connect_error.clone_from(&self.connect_error);
        state.password_ssid.clone_from(&self.password_ssid);
    }
}

impl NetworkController {
    /// The latest intent wins; the secret frame contains no SSID.
    pub fn stash_connect_intent(&self, pending: PendingNetworkConnect) {
        self.join.lock().expect("mutex poisoned").stash(pending);
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

    pub(super) fn begin_connect(&self, ssid: &str) -> u64 {
        let id = self.join.lock().expect("mutex poisoned").begin(ssid);
        let _ = self.events.send(NetworkSignal::Changed);
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
    ) {
        if self.join.lock().expect("mutex poisoned").finish(attempt, pending, error, ask_password) {
            let _ = self.events.send(NetworkSignal::Changed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PendingNetworkConnect {
        PendingNetworkConnect { ssid: "home".to_string(), hidden: true }
    }

    fn joined(n: u32) -> InFlight {
        let active = OwnedObjectPath::try_from(format!("/org/freedesktop/NetworkManager/ActiveConnection/{n}"))
            .expect("valid object path");
        InFlight { active, created: None, unsaved: None }
    }

    #[test]
    fn a_rejected_key_reopens_the_prompt_for_the_same_network() {
        let mut join = JoinState::default();
        assert!(join.finish(0, &home(), Some("wrong password".to_string()), true));

        let mut state = NetworkState {
            connecting_ssid: Some("stale".to_string()),
            password_ssid: Some("stale".to_string()),
            ..NetworkState::default()
        };
        join.overlay(&mut state);
        assert_eq!(state.connecting_ssid, None);
        assert_eq!(state.password_ssid.as_deref(), Some("home"));
        assert_eq!(
            state.connect_error,
            Some(JoinError { ssid: "home".to_string(), message: "wrong password".to_string() }),
        );
        assert_eq!(join.take_prompted(), Some(home()));
    }

    #[test]
    fn a_key_typed_for_one_network_never_joins_another() {
        let mut join = JoinState::default();
        let office = PendingNetworkConnect { ssid: "office".to_string(), hidden: false };

        join.stash(home());
        assert!(join.request_password(&home()));
        join.stash(office.clone());
        assert_eq!(join.take_prompted(), None);

        join.password_ssid = None;
        assert!(!join.request_password(&home()));
        assert_eq!(join.password_ssid, None);

        assert!(join.finish(0, &home(), Some("wrong password".to_string()), true));
        assert_eq!(join.take_prompted(), None);
        assert_eq!(join.pending, Some(office));
    }

    #[test]
    fn a_join_aborted_and_clicked_again_never_stands_in_for_the_new_one() {
        let mut join = JoinState::default();
        let first = join.begin("home");
        assert_eq!(join.abort(), Some(None));
        assert_eq!(join.connecting_ssid, None);
        let second = join.begin("home");

        assert!(!join.accept(first, &joined(1)));
        assert!(join.accept(second, &joined(2)));
        assert!(!join.finish(first, &home(), Some("disconnected".to_string()), false));
        assert_eq!((join.connecting_ssid.as_deref(), join.connect_error.as_ref()), (Some("home"), None));

        assert!(join.finish(second, &home(), None, false));
        assert_eq!((join.connecting_ssid, join.connect_error, join.joined), (None, None, None));
    }

    #[test]
    fn a_new_attempt_owns_no_join_until_nm_accepts_it() {
        let mut join = JoinState::default();
        assert!(join.accept(0, &joined(1)));
        join.begin("office");
        assert_eq!(join.joined, None);
    }
}

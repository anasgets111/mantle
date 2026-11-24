//! `mantle.network`'s `_profile` actions on saved NetworkManager profiles, run one at a time in call
//! order, with their failures in `network_profiles.error`.

use std::collections::HashMap;

use shared::action::{NetworkAction, ProfileEdit};
use shared::state::network_profiles::{ProfileAction, ProfileError, ProfileKind};
use tokio::sync::oneshot;
use zbus::zvariant::{OwnedObjectPath, Value};

use super::connect::{ACTIVATION_CEILING, ActiveConnectionProxy, reasons_on};
use super::proxies::{
    REASON_USER_REQUESTED, SETTINGS_FLAG_NM_GENERATED, SETTINGS_FLAG_UNSAVED, SettingsConnectionProxy,
    UPDATE2_FLAG_NO_REAPPLY, UPDATE2_FLAG_TO_DISK,
};
use super::{NetworkController, edit, root_object_path};
use crate::capabilities::{bind, bind_uncached};
use shared::warn;

/// `update_profile` writes `dns-data` (1.42) and sends `version-id` (1.44).
fn edits_supported(version: &str) -> bool {
    let mut parts = version.split('.').map(|part| part.parse::<u32>().ok());
    matches!((parts.next().flatten(), parts.next().flatten()), (Some(major), Some(minor)) if (major, minor) >= (1, 44))
}

/// Display text for a refused call: NetworkManager's own message, which names the offending
/// setting, except for a polkit refusal.
pub(super) fn refusal(err: zbus::Error) -> String {
    match err {
        zbus::Error::MethodError(name, ..) if name.ends_with(".PermissionDenied") => "not authorized".into(),
        zbus::Error::MethodError(name, ..) if name.ends_with(".VersionIdMismatch") => {
            "the profile changed meanwhile; try again".into()
        }
        zbus::Error::MethodError(_, Some(message), _) => message,
        err => err.to_string(),
    }
}

/// [`refusal`], except that NetworkManager's error ending in `suffix` reads as `text`.
fn refusal_or(suffix: &'static str, text: impl FnOnce() -> String) -> impl FnOnce(zbus::Error) -> String {
    move |err| match &err {
        zbus::Error::MethodError(name, ..) if name.ends_with(suffix) => text(),
        _ => refusal(err),
    }
}

impl NetworkController {
    /// The next turn among `_profile` actions, taken now and ready once every earlier turn drops
    /// its sender.
    pub(super) fn profile_turn(&self) -> impl Future<Output = oneshot::Sender<()>> + Send + 'static {
        let (written, landed) = oneshot::channel();
        let previous = self.profile_writes.lock().expect("mutex poisoned").replace(landed);
        async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            written
        }
    }

    /// Runs a `_profile` action in its turn, so an update lands before the `reapply_profile` or
    /// `activate_profile` called after it, then records its result. An activation ends its turn
    /// once NetworkManager takes the request, not at the verdict.
    pub(super) fn queue_profile_action(&self, command: NetworkAction) {
        let (turn, controller) = (self.profile_turn(), self.clone());
        tokio::spawn(async move {
            let written = turn.await;
            let (action, result, uuid) = match command {
                NetworkAction::UpdateProfile { uuid, edit } => {
                    (ProfileAction::Update, controller.update_profile(&uuid, &edit).await, uuid)
                }
                NetworkAction::DeleteProfile { uuid } => {
                    (ProfileAction::Delete, controller.delete_profile(&uuid).await, uuid)
                }
                NetworkAction::ReapplyProfile { uuid, interface } => {
                    (ProfileAction::Reapply, controller.reapply_profile(&uuid, interface.as_deref()).await, uuid)
                }
                NetworkAction::ActivateProfile { uuid, interface } => {
                    let result = controller.activate_profile(&uuid, interface.as_deref(), written).await;
                    (ProfileAction::Activate, result, uuid)
                }
                NetworkAction::DeactivateProfile { uuid } => {
                    (ProfileAction::Deactivate, controller.deactivate_profile(&uuid).await, uuid)
                }
                _ => return,
            };
            controller.settle(&uuid, action, result);
        });
    }

    /// Records a `_profile` action's result in `error`, then pushes.
    pub(super) fn settle(&self, uuid: &str, action: ProfileAction, result: Result<(), String>) {
        let mut profiles = self.profiles.lock().expect("mutex poisoned");
        match result {
            Err(message) => {
                warn!("{action:?} profile {uuid}: {message}");
                profiles.error = Some(ProfileError { uuid: uuid.into(), action, message });
            }
            Ok(()) if profiles.error.as_ref().is_some_and(|error| error.uuid == uuid && error.action == action) => {
                profiles.error = None;
            }
            Ok(()) => return,
        }
        let _ = self.profile_states.send(profiles.snapshot());
    }

    fn listed(&self, uuid: &str) -> Result<ProfileKind, String> {
        let profiles = self.profiles.lock().expect("mutex poisoned");
        let found = profiles.list.iter().find(|(_, profile)| profile.uuid == uuid);
        found.map(|(_, profile)| profile.kind).ok_or_else(|| "no such profile".into())
    }

    /// The profile's path now: a restarted NetworkManager may give a listed path to another profile.
    /// ponytail: a restart between this lookup and the call on the path can still reach another
    /// profile. Upgrade path: send both to NetworkManager's unique name, so a new daemon refuses.
    pub(super) async fn path_of(&self, uuid: &str) -> Result<OwnedObjectPath, String> {
        let lookup = self.settings.get_connection_by_uuid(uuid).await;
        lookup.map_err(refusal_or(".InvalidConnection", || "no such profile".into()))
    }

    /// Merges `edit` into the saved profile and writes it to disk with `Update2`, which keeps the
    /// stored secrets `GetSettings` leaves out. A concurrent edit is retried once.
    pub(super) async fn update_profile(&self, uuid: &str, edit: &ProfileEdit) -> Result<(), String> {
        if *edit == ProfileEdit::default() {
            return Ok(());
        }
        let version = self.nm.version().await.unwrap_or_default();
        if !edits_supported(&version) {
            return Err(format!("needs NetworkManager 1.44+, found {version:?}"));
        }
        let path = self.path_of(uuid).await?;
        let connection = bind_uncached::<SettingsConnectionProxy>(&self.connection, path).await.map_err(refusal)?;
        let mut retried = false;
        loop {
            // Read before `Flags`: a key staged after it changes the version, so `Update2` refuses.
            let version_id = connection.version_id().await.map_err(refusal)?;
            // A join stages a typed key in memory until NetworkManager accepts it; a write to disk now
            // would save a key that may be wrong. A generated profile, such as `Wired connection 1`,
            // is unsaved by nature and holds no staged key.
            // ponytail: a rejected key stays staged until a join with the right key or a
            // NetworkManager restart; see `save_typed_key` for the upgrade path.
            let flags = connection.flags().await.map_err(refusal)?;
            if flags & SETTINGS_FLAG_UNSAVED != 0 && flags & SETTINGS_FLAG_NM_GENERATED == 0 {
                let held = match self.listed(uuid) {
                    Ok(ProfileKind::Wifi) => "a key typed in connect is not saved yet; join with the right key",
                    _ => "the profile has unsaved changes; save them",
                };
                return Err(format!("{held}, or restart NetworkManager"));
            }
            let settings = connection.get_settings().await.map_err(refusal)?;
            let merged = edit::merge(&settings, edit)?;
            let args = HashMap::from([("version-id", Value::from(version_id))]);
            match connection.update2(merged, UPDATE2_FLAG_TO_DISK | UPDATE2_FLAG_NO_REAPPLY, args).await {
                Ok(_) => return Ok(()),
                Err(zbus::Error::MethodError(name, ..)) if !retried && name.ends_with(".VersionIdMismatch") => {
                    retried = true;
                }
                Err(err) => return Err(refusal(err)),
            }
        }
    }

    pub(super) async fn delete_profile(&self, uuid: &str) -> Result<(), String> {
        let path = self.path_of(uuid).await?;
        let connection = bind::<SettingsConnectionProxy>(&self.connection, path).await.map_err(refusal)?;
        connection.delete().await.map_err(refusal)
    }

    /// `Reapply` with the saved settings on `interface`, else on every device the profile's
    /// activations name, each checked against the device's applied version so a concurrent
    /// activation is not overwritten. Never reconnects on its own.
    pub(super) async fn reapply_profile(&self, uuid: &str, interface: Option<&str>) -> Result<(), String> {
        let mut devices = Vec::new();
        match interface {
            Some(name) => devices.push(self.device_named(name).await?),
            None => {
                for (path, active, _) in self.active_connections().await.map_err(refusal)? {
                    if active == uuid {
                        let active = bind_uncached::<ActiveConnectionProxy>(&self.connection, path).await;
                        devices.extend(active.map_err(refusal)?.devices().await.map_err(refusal)?);
                    }
                }
            }
        }
        let not_active =
            || format!("the profile is not active{}", interface.map_or(String::new(), |name| format!(" on {name}")));
        if devices.is_empty() {
            return Err(not_active());
        }
        for path in devices {
            let device = bind::<super::proxies::DeviceProxy>(&self.connection, path).await.map_err(refusal)?;
            let (applied, version_id) =
                device.get_applied_connection(0).await.map_err(refusal_or(".NotActive", not_active))?;
            // A VPN's activation names the device under it, where another profile is applied.
            if edit::read::<String>(applied.get("connection"), "uuid").as_deref() != Some(uuid) {
                return Err(not_active());
            }
            device.reapply(HashMap::new(), version_id, 0).await.map_err(refusal)?;
        }
        Ok(())
    }

    async fn device_named(&self, interface: &str) -> Result<OwnedObjectPath, String> {
        let found = self.nm.get_device_by_ip_iface(interface).await;
        found.map_err(refusal_or(".UnknownDevice", || format!("no interface {interface}")))
    }

    /// Activates on `interface`, else where NetworkManager picks, which honours the profile's own
    /// `interface-name` and creates a bridge or VLAN that does not exist yet. VPNs take
    /// `connect_vpn`'s path, with its secret prompt and `vpn_error`.
    pub(super) async fn activate_profile(
        &self,
        uuid: &str,
        interface: Option<&str>,
        written: oneshot::Sender<()>,
    ) -> Result<(), String> {
        let kind = self.listed(uuid)?;
        if matches!(kind, ProfileKind::Vpn | ProfileKind::Wireguard) {
            self.connect_vpn(uuid, written).await;
            return Ok(());
        }
        let device = match interface {
            Some(name) => Some(self.device_named(name).await?),
            None => None,
        };
        let path = self.path_of(uuid).await?;
        // Before the activation, so a failure ahead of its watch keeps its reason.
        let changes = self.device_changes().await.map_err(refusal)?;
        let root = root_object_path();
        let target = device.as_ref().map_or(root.clone(), |device| device.as_ref());
        let active = self.nm.activate_connection(&path, &target, &root).await.map_err(refusal)?;
        drop(written);
        let named =
            async { bind_uncached::<ActiveConnectionProxy>(&self.connection, active.clone()).await?.devices().await };
        let mut devices = match device {
            Some(device) => vec![device],
            None => named.await.unwrap_or_default(),
        };
        // A fast failure may leave no activation to name its device; a Wi-Fi device's reason is then
        // this Wi-Fi profile's.
        if devices.is_empty() && kind == ProfileKind::Wifi {
            devices =
                self.devices.lock().expect("mutex poisoned").wifi.iter().map(|wifi| wifi.device_path.clone()).collect();
        }
        let outcome = self.activation_outcome(&active, reasons_on(changes, devices));
        let outcome = tokio::time::timeout(ACTIVATION_CEILING, outcome).await;
        if outcome.is_err()
            && let Err(err) = self.nm.deactivate_connection(&active).await
        {
            warn!("failed to stop the timed-out activation {active}: {err}");
        }
        // A `deactivate_profile` meanwhile is no failure.
        if let Ok(Err(Some(REASON_USER_REQUESTED))) = outcome {
            return Ok(());
        }
        match super::intent::activation_verdict(outcome.ok()) {
            (_, true) if kind == ProfileKind::Wifi => {
                Err("the saved password is missing or wrong; join with connect".into())
            }
            (Some(error), _) => Err(error),
            (None, _) => Ok(()),
        }
    }

    /// Deactivates every activation of the profile; a VPN's waiting `connect_vpn` reports no
    /// `vpn_error`.
    pub(super) async fn deactivate_profile(&self, uuid: &str) -> Result<(), String> {
        self.listed(uuid)?;
        self.disconnect_vpn(uuid).await.map_err(refusal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::network::connect::tests::attempting;
    use crate::capabilities::test_support::within;

    #[test]
    fn editing_needs_networkmanager_1_44() {
        for (version, supported) in [("1.44.0", true), ("1.58.1", true), ("2.0", true), ("1.43.90", false), ("", false)]
        {
            assert_eq!(edits_supported(version), supported, "{version:?}");
        }
    }

    #[tokio::test]
    async fn an_error_stays_until_the_same_action_on_the_same_profile_succeeds() {
        let (mut controller, _events, _peer) = attempting(Ok).await;
        let (states, mut pushed) = tokio::sync::mpsc::unbounded_channel();
        controller.profile_states = states;
        let error = |controller: &NetworkController| {
            controller.profiles.lock().unwrap().error.as_ref().map(|error| (error.uuid.clone(), error.action))
        };
        controller.settle("a", ProfileAction::Update, Err("refused".into()));
        assert_eq!(pushed.try_recv().unwrap().error.unwrap().message, "refused");
        controller.settle("b", ProfileAction::Update, Ok(()));
        controller.settle("a", ProfileAction::Reapply, Ok(()));
        assert_eq!(error(&controller), Some(("a".into(), ProfileAction::Update)));
        assert!(pushed.try_recv().is_err(), "an unrelated success pushes nothing");
        controller.settle("a", ProfileAction::Update, Ok(()));
        assert_eq!(error(&controller), None);
        assert_eq!(pushed.try_recv().unwrap().error, None);
    }

    #[tokio::test]
    async fn profile_turns_come_in_call_order() {
        let (controller, _events, _peer) = attempting(Ok).await;
        let (first, second, third) = (controller.profile_turn(), controller.profile_turn(), controller.profile_turn());
        let (log, mut ran) = tokio::sync::mpsc::unbounded_channel();
        for (name, turn) in [("third", Box::pin(third)), ("second", Box::pin(second))] {
            let log = log.clone();
            tokio::spawn(async move {
                let _written = turn.await;
                log.send(name).unwrap();
            });
        }
        tokio::task::yield_now().await;
        assert!(ran.try_recv().is_err(), "nothing runs before the first turn ends");
        drop(first.await);
        for expected in ["second", "third"] {
            assert_eq!(within(ran.recv()).await, Some(expected));
        }
    }
}

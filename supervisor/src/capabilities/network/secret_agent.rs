//! `org.freedesktop.NetworkManager.SecretAgent`: VPN and WireGuard secrets wait in `vpn_secret` for
//! `network`/`vpn_secret` secure fields, so they never reach Lua. Other types get `NoSecrets`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared::state::network::VpnSecretRequest;
use shared::{Zeroizing, debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::proxies::AgentManagerProxy;
use super::vpn::VpnState;
use super::{NetworkController, NetworkSignal};

/// Fixed by NetworkManager: it calls the agent here.
const AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
const AGENT_ID: &str = "org.mantle.secret-agent";
const NM_NAME: &str = "org.freedesktop.NetworkManager";
/// `NM_SECRET_AGENT_CAPABILITY_VPN_HINTS`: NM then names the secrets a VPN plugin wants.
const CAPABILITY_VPN_HINTS: u32 = 1;
const FLAG_ALLOW_INTERACTION: u32 = 1;
const FLAG_REQUEST_NEW: u32 = 2;
/// Secret flags NM sets for "agent-owned" (1) and "not saved" (2): secrets the agent must supply.
const SECRET_FLAGS_ASKED: u32 = 3;
const ASK_TIMEOUT: Duration = Duration::from_secs(120);

type Settings = HashMap<String, HashMap<String, OwnedValue>>;

/// `org.freedesktop.NetworkManager.SecretAgent.*`, the names libnm agents reply with.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent")]
enum AgentError {
    PermissionDenied(String),
    NoSecrets(String),
    UserCanceled(String),
}

/// The request on screen and the reply NetworkManager waits on.
pub(super) struct PendingSecret {
    pub(super) request: VpnSecretRequest,
    path: OwnedObjectPath,
    setting: String,
    collected: HashMap<String, Zeroizing<String>>,
    reply: oneshot::Sender<Option<HashMap<String, Zeroizing<String>>>>,
}

/// Keys from NM's hints, else the profile's agent-owned or unsaved `<key>-flags`. WireGuard: `private-key` only.
fn wanted_keys(connection: &Settings, setting: &str, hints: &[String]) -> Vec<String> {
    let field = |section: &str, key: &str| connection.get(section)?.get(key).cloned();
    if field("connection", "type").and_then(|kind| String::try_from(kind).ok()).as_deref() != Some(setting) {
        return Vec::new();
    }
    let asked = |flags: u32| flags & SECRET_FLAGS_ASKED != 0;
    let mut keys: Vec<String> = match setting {
        "vpn" => hints.iter().filter(|hint| !hint.starts_with("x-vpn-message:")).cloned().collect(),
        "wireguard" => {
            let flagged = field(setting, "private-key-flags").and_then(|f| u32::try_from(f).ok()).is_some_and(asked);
            let hinted = hints.iter().any(|hint| hint == "private-key");
            return if flagged || hinted { vec!["private-key".into()] } else { Vec::new() };
        }
        _ => return Vec::new(),
    };
    if keys.is_empty() {
        let data = field(setting, "data").and_then(|data| HashMap::<String, String>::try_from(data).ok());
        let flagged = data.into_iter().flatten().filter(|(_, flags)| flags.parse().is_ok_and(asked));
        keys = flagged.filter_map(|(key, _)| key.strip_suffix("-flags").map(str::to_owned)).collect();
    }
    keys.retain(|key| key.len() <= 64 && shared::valid_secret_name(key));
    keys.sort();
    keys.dedup();
    keys
}

/// The `GetSecrets` reply: plugin VPNs take an `a{ss}` `secrets` entry, WireGuard its own keys.
fn reply_dict(setting: &str, secrets: &HashMap<String, Zeroizing<String>>) -> Settings {
    // ponytail: zbus needs owned Strings, so the reply holds unscrubbed copies; use a secret-aware serializer.
    let plain: HashMap<String, String> = secrets.iter().map(|(key, value)| (key.clone(), value.to_string())).collect();
    let owned = |value: Value| OwnedValue::try_from(value).expect("strings and dicts hold no fds");
    let section = if setting == "vpn" {
        HashMap::from([("secrets".to_string(), owned(Value::from(zbus::zvariant::Dict::from(plain))))])
    } else {
        plain.into_iter().map(|(key, value)| (key, owned(Value::from(value)))).collect()
    };
    HashMap::from([(setting.to_string(), section)])
}

struct SecretAgent {
    vpn: Arc<Mutex<VpnState>>,
    events: UnboundedSender<NetworkSignal>,
}

/// Takes the pending request for which `matches` holds and declines it. `by_user` drops its
/// `connect_vpn` wait, so the failed activation sets no `vpn_error`.
fn drop_request(
    vpn: &Mutex<VpnState>,
    events: &UnboundedSender<NetworkSignal>,
    by_user: bool,
    matches: impl FnOnce(&PendingSecret) -> bool,
) -> bool {
    let taken = {
        let mut vpn = vpn.lock().expect("mutex poisoned");
        let taken = vpn.secret.take_if(|pending| matches(pending));
        if by_user && let Some(pending) = &taken {
            vpn.waiting.remove(&pending.request.uuid);
        }
        taken
    };
    let Some(pending) = taken else { return false };
    let _ = pending.reply.send(None);
    let _ = events.send(NetworkSignal::Changed);
    true
}

/// The system bus lets any local process call an agent; only NetworkManager's name owner may.
async fn from_nm(bus: &zbus::Connection, header: &Header<'_>) -> Result<(), AgentError> {
    let nm = zbus::names::WellKnownName::from_static_str_unchecked(NM_NAME).into();
    let owner = async { zbus::fdo::DBusProxy::new(bus).await?.get_name_owner(nm).await };
    match (owner.await, header.sender()) {
        (Ok(owner), Some(sender)) if owner.as_str() == sender.as_str() => Ok(()),
        _ => Err(AgentError::PermissionDenied("only NetworkManager may call the secret agent".into())),
    }
}

impl SecretAgent {
    async fn ask(
        &self,
        connection: Settings,
        connection_path: OwnedObjectPath,
        setting_name: String,
        hints: Vec<String>,
        flags: u32,
    ) -> Result<Settings, AgentError> {
        let keys = wanted_keys(&connection, &setting_name, &hints);
        // A closed channel means the network worker is gone and no UI would see the prompt.
        if keys.is_empty() || flags & FLAG_ALLOW_INTERACTION == 0 || self.events.is_closed() {
            return Err(AgentError::NoSecrets("nothing to ask, or nobody to ask".into()));
        }
        let text = |key: &str| {
            connection.get("connection").and_then(|c| String::try_from(c.get(key)?.clone()).ok()).unwrap_or_default()
        };
        let id = crate::capabilities::next_request_id().ok_or(AgentError::NoSecrets("request ids ran out".into()))?;
        let (reply, answered) = oneshot::channel();
        {
            let mut vpn = self.vpn.lock().expect("mutex poisoned");
            if vpn.secret.is_some() {
                debug!("refused a secret request for {connection_path}: another is on screen");
                return Err(AgentError::NoSecrets("another secret request is on screen".into()));
            }
            vpn.secret = Some(PendingSecret {
                request: VpnSecretRequest {
                    id: id.clone(),
                    uuid: text("uuid"),
                    vpn_id: text("id"),
                    fields: keys,
                    retry: flags & FLAG_REQUEST_NEW != 0,
                },
                path: connection_path,
                setting: setting_name.clone(),
                collected: HashMap::new(),
                reply,
            });
        }
        let _ = self.events.send(NetworkSignal::Changed);
        let answer = tokio::time::timeout(ASK_TIMEOUT, answered).await;
        drop_request(&self.vpn, &self.events, false, |pending| pending.request.id == id);
        match answer {
            Ok(Ok(Some(secrets))) => Ok(reply_dict(&setting_name, &secrets)),
            _ => Err(AgentError::UserCanceled("the user declined, or NetworkManager cancelled the request".into())),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl SecretAgent {
    #[allow(clippy::too_many_arguments)]
    async fn get_secrets(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] bus: &zbus::Connection,
        connection: Settings,
        connection_path: OwnedObjectPath,
        setting_name: String,
        hints: Vec<String>,
        flags: u32,
    ) -> Result<Settings, AgentError> {
        from_nm(bus, &header).await?;
        self.ask(connection, connection_path, setting_name, hints, flags).await
    }

    async fn cancel_get_secrets(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] bus: &zbus::Connection,
        connection_path: OwnedObjectPath,
        setting_name: String,
    ) -> Result<(), AgentError> {
        from_nm(bus, &header).await?;
        drop_request(&self.vpn, &self.events, false, |p| p.path == connection_path && p.setting == setting_name);
        Ok(())
    }

    /// No-ops, so they skip the caller check: NM calls them only for agent-owned secrets, asked for each time.
    async fn save_secrets(&self, _connection: Settings, _connection_path: OwnedObjectPath) {}

    async fn delete_secrets(&self, _connection: Settings, _connection_path: OwnedObjectPath) {}
}

/// One field of the pending request, from a `network`/`vpn_secret` field named
/// `<request id>/<key>`. The last field answers NetworkManager. A stale, unknown or non-UTF-8
/// field is dropped, which zeroizes it. Returns whether the request changed.
fn submit(vpn: &Mutex<VpnState>, target: &str, secret: &Zeroizing<Vec<u8>>) -> bool {
    let answered = {
        let mut vpn = vpn.lock().expect("mutex poisoned");
        let Some((id, key)) = target.split_once('/') else { return false };
        let Some(pending) = vpn.secret.as_mut().filter(|p| p.request.id == id) else { return false };
        let Some(index) = pending.request.fields.iter().position(|field| field == key) else { return false };
        let Ok(value) = std::str::from_utf8(secret) else { return false };
        pending.request.fields.remove(index);
        pending.collected.insert(key.to_owned(), Zeroizing::new(value.to_owned()));
        if pending.request.fields.is_empty() { vpn.secret.take() } else { None }
    };
    if let Some(pending) = answered {
        let _ = pending.reply.send(Some(pending.collected));
    }
    true
}

impl NetworkController {
    pub fn submit_vpn_secret(&self, target: &str, secret: Zeroizing<Vec<u8>>) {
        if submit(&self.vpn, target, &secret) {
            let _ = self.events.send(NetworkSignal::Changed);
        }
    }

    /// Declines the pending request; NetworkManager fails the activation.
    pub fn cancel_vpn_secret(&self) {
        drop_request(&self.vpn, &self.events, true, |_| true);
    }
}

/// Exports the agent, replacing one a previous controller left behind.
pub(super) async fn export(
    connection: &zbus::Connection,
    vpn: Arc<Mutex<VpnState>>,
    events: UnboundedSender<NetworkSignal>,
) {
    let server = connection.object_server();
    let _ = server.remove::<SecretAgent, _>(AGENT_PATH).await;
    if let Err(err) = server.at(AGENT_PATH, SecretAgent { vpn, events }).await {
        error!("failed to export the SecretAgent object at {AGENT_PATH}: {err}");
    }
}

/// Offers the exported agent to NetworkManager, which forgets its agents when it exits. Needs
/// polkit's `network-control`.
pub(super) async fn register(connection: &zbus::Connection) {
    let result = async {
        let manager = AgentManagerProxy::new(connection).await?;
        // A registration left by an earlier controller on this connection would refuse the next.
        let _ = manager.unregister().await;
        manager.register_with_capabilities(AGENT_ID, CAPABILITY_VPN_HINTS).await
    };
    match result.await {
        Err(zbus::Error::MethodError(name, ..)) if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" => {
            debug!("NetworkManager left before the secret agent registered");
        }
        Err(err) => warn!("failed to register the NetworkManager secret agent; VPN secret prompts are off: {err}"),
        Ok(()) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::private_bus;

    fn value(value: impl Into<Value<'static>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).unwrap()
    }

    fn profile(kind: &str, section: Option<(&str, OwnedValue)>) -> Settings {
        let mut settings =
            HashMap::from([("connection".to_string(), HashMap::from([("type".to_string(), value(kind.to_string()))]))]);
        if let Some((key, flags)) = section {
            settings.insert(kind.to_string(), HashMap::from([(key.to_string(), flags)]));
        }
        settings
    }

    fn strings(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|key| (*key).to_string()).collect()
    }

    #[test]
    fn keys_come_from_hints_then_from_agent_owned_flags_and_only_for_vpns() {
        let hints = strings(&["x-vpn-message:Sign in", "password", "otp"]);
        assert_eq!(wanted_keys(&profile("vpn", None), "vpn", &hints), ["otp", "password"]);

        let data = HashMap::from([("password-flags", "1"), ("cert-flags", "0"), ("otp-flags", "2"), ("remote", "x")]);
        let data = data.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        let flagged = profile("vpn", Some(("data", value(zbus::zvariant::Dict::from(data)))));
        assert_eq!(wanted_keys(&flagged, "vpn", &[]), ["otp", "password"]);

        let wireguard = profile("wireguard", Some(("private-key-flags", value(1u32))));
        assert_eq!(wanted_keys(&wireguard, "wireguard", &[]), ["private-key"]);
        assert_eq!(wanted_keys(&wireguard, "wireguard", &strings(&["private-key", "peers"])), ["private-key"]);
        assert!(
            wanted_keys(&profile("wireguard", Some(("private-key-flags", value(0u32)))), "wireguard", &[]).is_empty()
        );
        assert!(wanted_keys(&profile("802-11-wireless", None), "802-11-wireless-security", &hints).is_empty());
        assert!(wanted_keys(&profile("vpn", None), "wireguard", &hints).is_empty(), "the setting must match the type");
    }

    const PATH: &str = "/org/freedesktop/NetworkManager/Settings/5";

    fn agent() -> (SecretAgent, tokio::sync::mpsc::UnboundedReceiver<NetworkSignal>) {
        let (events, changes) = tokio::sync::mpsc::unbounded_channel();
        (SecretAgent { vpn: Arc::default(), events }, changes)
    }

    async fn ask(agent: &SecretAgent, fields: &[&str], flags: u32) -> Result<Settings, AgentError> {
        let path = OwnedObjectPath::try_from(PATH).unwrap();
        agent.ask(profile("vpn", None), path, "vpn".into(), strings(fields), flags).await
    }

    #[tokio::test]
    async fn a_request_waits_for_every_field_and_is_answered_once() {
        let (agent, mut changes) = agent();
        let interactive = FLAG_ALLOW_INTERACTION | FLAG_REQUEST_NEW;
        let user = async {
            changes.recv().await.unwrap();
            let request = agent.vpn.lock().unwrap().secret.as_ref().unwrap().request.clone();
            assert_eq!((request.fields.as_slice(), request.retry), (&strings(&["otp", "password"])[..], true));
            assert!(matches!(ask(&agent, &["password"], interactive).await, Err(AgentError::NoSecrets(_))));

            let secret = Zeroizing::new(b"hunter2".to_vec());
            for stale in ["0/password", &format!("{}/other", request.id), "password"] {
                assert!(!submit(&agent.vpn, stale, &secret), "{stale}");
            }
            assert!(!submit(&agent.vpn, &format!("{}/password", request.id), &Zeroizing::new(vec![0xff])));
            assert!(submit(&agent.vpn, &format!("{}/otp", request.id), &Zeroizing::new(b"123456".to_vec())));
            let left = agent.vpn.lock().unwrap().secret.as_ref().unwrap().request.fields.clone();
            assert_eq!(left, ["password"], "the first field only stores");
            assert!(submit(&agent.vpn, &format!("{}/password", request.id), &secret));
        };
        let (reply, ()) = tokio::join!(ask(&agent, &["password", "otp"], interactive), user);
        let secrets = HashMap::<String, String>::try_from(reply.unwrap()["vpn"]["secrets"].clone()).unwrap();
        assert_eq!(secrets, HashMap::from([("otp".into(), "123456".into()), ("password".into(), "hunter2".into())]));
        assert!(agent.vpn.lock().unwrap().secret.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_cancel_or_the_timeout_declines_and_a_gone_worker_is_never_asked() {
        let (agent, mut changes) = agent();
        assert!(matches!(ask(&agent, &["password"], 0).await, Err(AgentError::NoSecrets(_))), "no interaction");
        agent.vpn.lock().unwrap().waiting.insert(String::new(), "1".into());
        let cancel = async {
            changes.recv().await.unwrap();
            assert!(drop_request(&agent.vpn, &agent.events, true, |_| true));
        };
        let (declined, ()) = tokio::join!(ask(&agent, &["password"], FLAG_ALLOW_INTERACTION), cancel);
        assert!(matches!(declined, Err(AgentError::UserCanceled(_))));
        assert!(agent.vpn.lock().unwrap().waiting.is_empty(), "a user cancel is no activation failure");

        let timed_out = ask(&agent, &["password"], FLAG_ALLOW_INTERACTION).await;
        assert!(matches!(timed_out, Err(AgentError::UserCanceled(_))));
        assert!(agent.vpn.lock().unwrap().secret.is_none());
        drop(changes);
        assert!(matches!(ask(&agent, &["password"], FLAG_ALLOW_INTERACTION).await, Err(AgentError::NoSecrets(_))));
    }

    /// The reply's D-Bus error name, or `None` on success.
    async fn call<B>(from: &zbus::Connection, agent: &zbus::Connection, method: &str, body: &B) -> Option<String>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let to = agent.unique_name().unwrap().as_str();
        let interface = Some("org.freedesktop.NetworkManager.SecretAgent");
        match from.call_method(Some(to), AGENT_PATH, interface, method, body).await {
            Ok(_) => None,
            Err(zbus::Error::MethodError(name, ..)) => Some(name.to_string()),
            Err(err) => panic!("{method}: {err}"),
        }
    }

    #[tokio::test]
    async fn only_networkmanager_reaches_the_agent_and_errors_carry_libnm_names() {
        let bus = private_bus().await;
        let (served, mut changes) = agent();
        let vpn = Arc::clone(&served.vpn);
        let agent = bus.builder().serve_at(AGENT_PATH, served).unwrap().build().await.unwrap();
        let nm = bus.builder().name(NM_NAME).unwrap().build().await.unwrap();
        let rogue = bus.connection().await;
        let path = OwnedObjectPath::try_from(PATH).unwrap();
        let get = |flags: u32| (profile("vpn", None), path.clone(), "vpn", strings(&["password"]), flags);
        let cancel = |path: &str| (OwnedObjectPath::try_from(path).unwrap(), "vpn");
        let denied = Some("org.freedesktop.NetworkManager.SecretAgent.PermissionDenied".to_string());

        let interactive = get(FLAG_ALLOW_INTERACTION);
        let waiting = call(&nm, &agent, "GetSecrets", &interactive);
        let checks = async {
            changes.recv().await.unwrap();
            assert_eq!(call(&rogue, &agent, "CancelGetSecrets", &cancel(PATH)).await, denied);
            assert_eq!(call(&rogue, &agent, "GetSecrets", &get(0)).await, denied);
            assert_eq!(call(&nm, &agent, "CancelGetSecrets", &cancel("/other")).await, None);
            assert!(vpn.lock().unwrap().secret.is_some(), "neither cancel reached the request");
            assert_eq!(call(&nm, &agent, "CancelGetSecrets", &cancel(PATH)).await, None);
        };
        let (declined, ()) = tokio::join!(waiting, checks);
        assert_eq!(declined.as_deref(), Some("org.freedesktop.NetworkManager.SecretAgent.UserCanceled"));
        let refused = call(&nm, &agent, "GetSecrets", &get(0)).await;
        assert_eq!(refused.as_deref(), Some("org.freedesktop.NetworkManager.SecretAgent.NoSecrets"));
    }
}

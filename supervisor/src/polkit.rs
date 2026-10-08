//! Polkit authentication agent registration handshake.
//!
//! `RegisterAuthenticationAgent(subject: (sa{sv}), locale: s, object_path: s) -> ()` is the
//! verified D-Bus method, checked against polkit's source and introspection XML, not
//! `RegisterAgent`. Use `zbus_polkit`'s `Authority` proxy and `Subject` directly, avoiding
//! hand-derived zvariant types (ADR-0013).
//!
//! No maintained crate covers `org.freedesktop.PolicyKit1.AuthenticationAgent`, so the hand-written
//! interface below uses the verified signature:
//! `BeginAuthentication(action_id: s, message: s, icon_name: s, details: a{ss}, cookie: s,
//! identities: a(sa{sv})) -> ()` and `CancelAuthentication(cookie: s) -> ()`.

use std::collections::HashMap;

use futures_util::StreamExt;

use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use zbus::interface;
use zbus::message::Header;
use zbus::zvariant::{OwnedValue, Value};
pub use zbus_polkit::policykit1::{AuthorityProxy, Subject};

/// Agent-side protocol error. Dismissing the dialog returns
/// `org.freedesktop.PolicyKit1.Error.Cancelled`.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum AgentError {
    Cancelled,
    PermissionDenied(String),
}

/// Any local process can call an exported agent; only polkitd may.
async fn from_polkitd(bus: &zbus::Connection, header: &Header<'_>) -> Result<(), AgentError> {
    if crate::capabilities::sent_by_owner(bus, header, "org.freedesktop.PolicyKit1").await {
        return Ok(());
    }
    Err(AgentError::PermissionDenied("only polkitd may call the authentication agent".into()))
}

/// polkitd request forwarded to `main.rs`, which owns the answer.
///
/// `Cancel` without a cookie is polkitd leaving the bus. `Begin` carries the reply. polkitd treats an early return as failed authentication, so
/// `begin_authentication` awaits the answer before returning to the bus. Dropping the sender
/// cancels.
pub enum AgentRequest {
    Begin { call: BeginAuthenticationCall, reply: oneshot::Sender<Result<(), AgentError>> },
    Cancel { cookie: Option<String> },
}

/// Export path on our unique connection. Any path we control is valid; the spec leaves it to the
/// caller.
pub const AGENT_OBJECT_PATH: &str = "/org/mantle/PolicyKit1/AuthenticationAgent";

/// Builds the `unix-session` `Subject` for this process's session.
///
/// ponytail: reads `$XDG_SESSION_ID`, not logind's `Manager.GetSessionByPID`. Upgrade to that
/// round trip if pam_systemd stops setting it or a logind client arrives here (ADR-0010 covers
/// Wayland idle/lock, not logind sessions).
pub fn current_session_subject() -> Result<Subject, std::env::VarError> {
    Ok(session_subject(std::env::var("XDG_SESSION_ID")?))
}

/// `Subject` construction split from `$XDG_SESSION_ID` lookup so tests avoid `set_var`, which
/// races every environment reader in the test binary and is `unsafe` in Rust 2024.
fn session_subject(session_id: String) -> Subject {
    let mut subject_details = HashMap::new();
    subject_details.insert(
        "session-id".to_string(),
        OwnedValue::try_from(Value::from(session_id)).expect("String -> OwnedValue conversion is infallible"),
    );
    Subject { subject_kind: "unix-session".to_string(), subject_details }
}

/// One wire-parsed `BeginAuthentication` call from polkitd.
///
/// No `Eq`: `identities` can contain `zvariant::Value::F64`, and `f64` is only `PartialEq`.
#[derive(Debug, Clone, PartialEq)]
pub struct BeginAuthenticationCall {
    pub action_id: String,
    pub message: String,
    pub icon_name: String,
    pub details: HashMap<String, String>,
    pub cookie: String,
    pub identities: Vec<(String, HashMap<String, OwnedValue>)>,
}

/// uid for `AuthenticationAgentResponse2`, parsed from `BeginAuthentication`'s `identities`: a
/// `unix-user` identity stores `uint32` under `"uid"`.
///
/// ponytail: takes the *first* `unix-user`, not all. polkitd may list several (for example every
/// `wheel` member), but this codebase has no picker. First-match stands until one exists; upgrade
/// path: ADR-0028.
pub fn first_unix_user_uid(identities: &[(String, HashMap<String, OwnedValue>)]) -> Option<u32> {
    identities
        .iter()
        .find(|(kind, _)| kind == "unix-user")
        .and_then(|(_, details)| details.get("uid"))
        .and_then(|v| u32::try_from(v.clone()).ok())
}

/// `org.freedesktop.PolicyKit1.AuthenticationAgent`, called by polkitd after [`PolkitAgent::register`].
///
/// Forwards only. The challenge becomes `mantle.polkit` state; the root helper runs PAM after
/// `secure_submit("polkit", "authenticate")`, and `main.rs` releases the held reply when it answers
/// (ADR-0114). zbus uses one task per call, so cancel can arrive while begin waits.
pub struct AuthenticationAgent {
    requests: UnboundedSender<AgentRequest>,
}

impl AuthenticationAgent {
    pub fn new(requests: UnboundedSender<AgentRequest>) -> Self {
        Self { requests }
    }
}

#[interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl AuthenticationAgent {
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] bus: &zbus::Connection,
        action_id: String,
        message: String,
        icon_name: String,
        details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> Result<(), AgentError> {
        from_polkitd(bus, &header).await?;
        let (reply, answer) = oneshot::channel();
        let call = BeginAuthenticationCall { action_id, message, icon_name, details, cookie, identities };
        // A dropped receiver means nobody is listening (mid-shutdown); the sender below is gone
        // with it, and `answer` reads that as a cancel.
        let _ = self.requests.send(AgentRequest::Begin { call, reply });
        answer.await.unwrap_or(Err(AgentError::Cancelled))
    }

    async fn cancel_authentication(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] bus: &zbus::Connection,
        cookie: String,
    ) -> Result<(), AgentError> {
        from_polkitd(bus, &header).await?;
        let _ = self.requests.send(AgentRequest::Cancel { cookie: Some(cookie) });
        Ok(())
    }
}

/// Authentication agent, unregistered until config reads `mantle.polkit` or names it in
/// `secure_submit` (ADR-0070 decisions 5 and 6, ADR-0114).
pub struct PolkitAgent {
    /// Taken by the first [`Self::register`], making later calls no-ops instead of duplicate wire
    /// registrations.
    agent: Option<AuthenticationAgent>,
}

impl PolkitAgent {
    pub fn new(requests: UnboundedSender<AgentRequest>) -> Self {
        PolkitAgent { agent: Some(AuthenticationAgent::new(requests)) }
    }

    /// Registers once, in a spawned task so a slow polkitd cannot hold the Supervisor loop, and
    /// again whenever polkitd restarts, since it forgets its agents.
    pub fn register(&mut self, connection: &zbus::Connection) {
        match current_session_subject() {
            Ok(subject) => self.register_for(connection, subject),
            Err(err) => {
                error!("$XDG_SESSION_ID names no session to register an agent for; agent disabled for this run: {err}");
                // Do not retry: `$XDG_SESSION_ID` will not appear mid-run.
                self.agent = None;
            }
        }
    }

    /// [`Self::register`] once the subject is known, preserving the take-once rule. Separate so
    /// tests can call it twice without `set_var`, whose process-wide `environ` rewrite races any
    /// concurrent `getenv`.
    fn register_for(&mut self, connection: &zbus::Connection, subject: Subject) {
        let Some(agent) = self.agent.take() else {
            return;
        };
        let connection = connection.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_agent(&connection, agent, &subject).await {
                warn!("cannot export the authentication agent, so another agent answers this session: {err}");
            }
        });
    }
}

/// Exports `agent` before the first `RegisterAuthenticationAgent`, so an immediate callback finds a
/// live object, then registers it with every polkitd that takes the name.
///
/// ponytail: a caller reaching a restarted polkitd before the re-registration finds no agent, the
/// same window libpolkitagent leaves. Closing it would need polkitd to announce its agents, which it
/// does not.
async fn serve_agent(connection: &zbus::Connection, agent: AuthenticationAgent, subject: &Subject) -> zbus::Result<()> {
    let requests = agent.requests.clone();
    connection.object_server().at(AGENT_OBJECT_PATH, agent).await?;
    let authority = AuthorityProxy::new(connection).await?;
    // Subscribed before the first registration, so a restart in between is not missed.
    let mut owners = authority.inner().receive_owner_changed().await?;
    register_with(&authority, subject).await;
    while let Some(owner) = owners.next().await {
        // A challenge the old polkitd held died with it, and a held one refuses every new one.
        let _ = requests.send(AgentRequest::Cancel { cookie: None });
        if owner.is_some() {
            register_with(&authority, subject).await;
        }
    }
    Ok(())
}

/// Logs a failure rather than propagating it: another agent for the subject is normal elsewhere
/// and must not stop the shell.
async fn register_with(authority: &AuthorityProxy<'_>, subject: &Subject) {
    match authority.register_authentication_agent(subject, "en_US.UTF-8", AGENT_OBJECT_PATH).await {
        Ok(()) => debug!("registered as this session's authentication agent"),
        Err(err) => warn!("RegisterAuthenticationAgent failed, so another agent answers this session: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::{p2p_pair_serving, private_bus, within};
    use tokio::sync::mpsc;

    /// Stand-in Authority on the p2p peer, exercising the real wire call without a system bus.
    struct MockAuthority {
        calls: mpsc::UnboundedSender<(Subject, String, String)>,
    }

    #[interface(name = "org.freedesktop.PolicyKit1.Authority")]
    impl MockAuthority {
        async fn register_authentication_agent(&self, subject: Subject, locale: String, object_path: String) {
            let _ = self.calls.send((subject, locale, object_path));
        }
    }

    fn test_subject() -> Subject {
        let mut subject_details = HashMap::new();
        subject_details.insert("session-id".to_string(), OwnedValue::try_from(Value::from("c1")).unwrap());
        Subject { subject_kind: "unix-session".to_string(), subject_details }
    }

    /// A second registration would ask polkitd for another agent on one subject. Every generation
    /// sends its own starts, so it is reachable (ADR-0070 decision 3).
    #[tokio::test]
    async fn registering_twice_makes_only_one_wire_call() {
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        let (agent_side, _authority_side) = p2p_pair_serving(|peer| {
            peer.serve_at("/org/freedesktop/PolicyKit1/Authority", MockAuthority { calls: calls_tx })
        })
        .await;
        let (challenges_tx, _challenges_rx) = mpsc::unbounded_channel();
        let mut agent = PolkitAgent::new(challenges_tx);

        agent.register_for(&agent_side, test_subject());
        agent.register_for(&agent_side, test_subject());

        calls_rx.recv().await.expect("the first register must reach the Authority");
        assert!(calls_rx.try_recv().is_err(), "the second register must be a no-op");
    }

    /// polkitd forgets its agents when it restarts, and a challenge it held died with it.
    #[tokio::test]
    async fn a_restarted_polkitd_gets_the_agent_again_and_the_held_challenge_is_cancelled() {
        let bus = private_bus().await;
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        let authority = |calls: mpsc::UnboundedSender<(Subject, String, String)>| {
            bus.builder()
                .serve_at("/org/freedesktop/PolicyKit1/Authority", MockAuthority { calls })
                .unwrap()
                .name("org.freedesktop.PolicyKit1")
                .unwrap()
                .build()
        };
        let first = authority(calls_tx.clone()).await.unwrap();
        let agent_side = bus.connection().await;
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        PolkitAgent::new(requests_tx).register_for(&agent_side, test_subject());

        let (subject, locale, object_path) = within(calls_rx.recv()).await.unwrap();
        assert_eq!(subject.subject_kind, "unix-session");
        assert_eq!(
            subject.subject_details.get("session-id").cloned().and_then(|v| String::try_from(v).ok()),
            Some("c1".to_string())
        );
        assert_eq!((locale.as_str(), object_path.as_str()), ("en_US.UTF-8", AGENT_OBJECT_PATH));
        let identities: Vec<(String, HashMap<String, OwnedValue>)> = vec![];
        let caller = first.clone();
        let agent_name = agent_side.unique_name().unwrap().to_owned();
        let begin = tokio::spawn(async move {
            caller
                .call_method(
                    Some(agent_name),
                    AGENT_OBJECT_PATH,
                    Some("org.freedesktop.PolicyKit1.AuthenticationAgent"),
                    "BeginAuthentication",
                    &("a", "m", "i", HashMap::<String, String>::new(), "cookie-1", identities),
                )
                .await
        });
        let Some(AgentRequest::Begin { reply: _held, .. }) = within(requests.recv()).await else {
            panic!("BeginAuthentication was not forwarded");
        };

        // The caller is polkitd's own connection; release the name it holds.
        begin.abort();
        let _ = begin.await;
        drop(first);
        assert!(
            matches!(within(requests.recv()).await, Some(AgentRequest::Cancel { cookie: None })),
            "polkitd leaving must cancel the challenge it held"
        );
        let _second = authority(calls_tx).await.unwrap();
        within(calls_rx.recv()).await.expect("the new polkitd must get RegisterAuthenticationAgent");
    }

    /// Tests [`session_subject`] rather than `$XDG_SESSION_ID`, which parallel tests would race on.
    #[test]
    fn a_session_id_becomes_a_unix_session_subject() {
        let subject = session_subject("test-session-42".to_string());
        assert_eq!(subject.subject_kind, "unix-session");
        assert_eq!(
            subject.subject_details.get("session-id").cloned().and_then(|v| String::try_from(v).ok()),
            Some("test-session-42".to_string())
        );
    }

    /// Agent served on a private bus; the returned connection owns polkitd's name.
    async fn agent_on_bus(
        bus: &crate::capabilities::test_support::PrivateBus,
        tx: mpsc::UnboundedSender<AgentRequest>,
    ) -> (zbus::Connection, zbus::Connection) {
        let agent =
            bus.builder().serve_at(AGENT_OBJECT_PATH, AuthenticationAgent::new(tx)).unwrap().build().await.unwrap();
        let polkitd = bus.builder().name("org.freedesktop.PolicyKit1").unwrap().build().await.unwrap();
        (polkitd, agent)
    }

    async fn call(
        from: &zbus::Connection,
        to: &zbus::Connection,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> zbus::Result<zbus::Message> {
        let to = to.unique_name().unwrap().to_owned();
        from.call_method(
            Some(to),
            AGENT_OBJECT_PATH,
            Some("org.freedesktop.PolicyKit1.AuthenticationAgent"),
            method,
            body,
        )
        .await
    }

    #[tokio::test]
    async fn begin_authentication_forwards_the_parsed_challenge_and_returns_only_once_answered() {
        let bus = private_bus().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (polkitd, agent) = agent_on_bus(&bus, tx).await;

        let details: HashMap<String, String> =
            HashMap::from([("polkit.gettext_domain".to_string(), "polkit".to_string())]);
        let identity_details: HashMap<String, OwnedValue> =
            HashMap::from([("uid".to_string(), OwnedValue::try_from(Value::from(1000u32)).unwrap())]);
        let identities: Vec<(String, HashMap<String, OwnedValue>)> = vec![("unix-user".to_string(), identity_details)];
        let call = tokio::spawn(async move {
            let body = (
                "org.mantle.test.action",
                "Authenticate to do the thing",
                "dialog-password",
                details,
                "cookie-123",
                identities,
            );
            self::call(&polkitd, &agent, "BeginAuthentication", &body).await
        });

        let Some(AgentRequest::Begin { call: received, reply }) = within(rx.recv()).await else {
            panic!("BeginAuthentication was never forwarded over the channel");
        };
        assert!(!call.is_finished(), "the D-Bus call must stay open until the flow answers it");
        reply.send(Err(AgentError::Cancelled)).expect("the agent is waiting on this reply");
        let err = call.await.unwrap().expect_err("a Cancelled reply must reach the caller as a D-Bus error");
        let zbus::Error::MethodError(name, _, _) = err else { panic!("expected a method error, got {err:?}") };
        assert_eq!(name.as_str(), "org.freedesktop.PolicyKit1.Error.Cancelled");
        assert_eq!(received.action_id, "org.mantle.test.action");
        assert_eq!(received.message, "Authenticate to do the thing");
        assert_eq!(received.icon_name, "dialog-password");
        assert_eq!(received.cookie, "cookie-123");
        assert_eq!(received.details.get("polkit.gettext_domain").map(String::as_str), Some("polkit"));
        assert_eq!(
            first_unix_user_uid(&received.identities),
            Some(1000),
            "identities must be forwarded, not discarded"
        );
    }

    #[tokio::test]
    async fn cancel_authentication_forwards_the_cookie() {
        let bus = private_bus().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (polkitd, agent) = agent_on_bus(&bus, tx).await;

        call(&polkitd, &agent, "CancelAuthentication", &("cookie-123",))
            .await
            .expect("CancelAuthentication call should succeed");
        assert!(
            matches!(within(rx.recv()).await, Some(AgentRequest::Cancel { cookie: Some(cookie) }) if cookie == "cookie-123")
        );
    }

    #[tokio::test]
    async fn only_polkitd_reaches_the_agent() {
        let bus = private_bus().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_polkitd, agent) = agent_on_bus(&bus, tx).await;
        let rogue = bus.connection().await;
        let identities: Vec<(String, HashMap<String, OwnedValue>)> = vec![];

        let begin = ("a", "m", "i", HashMap::<String, String>::new(), "cookie", identities);
        for result in [
            call(&rogue, &agent, "BeginAuthentication", &begin).await,
            call(&rogue, &agent, "CancelAuthentication", &("cookie",)).await,
        ] {
            let Err(zbus::Error::MethodError(name, ..)) = result else { panic!("a non-polkitd caller was answered") };
            assert_eq!(name.as_str(), "org.freedesktop.PolicyKit1.Error.PermissionDenied");
        }
        assert!(rx.try_recv().is_err(), "a refused call must not reach the flow");
    }

    fn unix_user_identity(uid: u32) -> (String, HashMap<String, OwnedValue>) {
        ("unix-user".to_string(), HashMap::from([("uid".to_string(), OwnedValue::try_from(Value::from(uid)).unwrap())]))
    }

    #[test]
    fn first_unix_user_uid_is_none_for_an_empty_list() {
        assert_eq!(first_unix_user_uid(&[]), None);
    }

    #[test]
    fn first_unix_user_uid_is_none_when_only_a_unix_group_is_present() {
        let group = (
            "unix-group".to_string(),
            HashMap::from([("gid".to_string(), OwnedValue::try_from(Value::from(100u32)).unwrap())]),
        );
        assert_eq!(first_unix_user_uid(&[group]), None);
    }

    #[test]
    fn first_unix_user_uid_returns_the_uid_of_a_unix_user_identity() {
        assert_eq!(first_unix_user_uid(&[unix_user_identity(1000)]), Some(1000));
    }

    #[test]
    fn first_unix_user_uid_is_none_when_the_unix_user_entry_is_missing_the_uid_key() {
        let malformed = ("unix-user".to_string(), HashMap::new());
        assert_eq!(first_unix_user_uid(&[malformed]), None);
    }

    #[test]
    fn first_unix_user_uid_returns_the_first_unix_user_entrys_uid_when_multiple_are_present() {
        let identities = [unix_user_identity(1000), unix_user_identity(2000)];
        assert_eq!(first_unix_user_uid(&identities), Some(1000), "must take the first identity, not just any of them");
    }
}

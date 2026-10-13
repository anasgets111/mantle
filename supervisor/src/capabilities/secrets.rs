//! Named writes to the session Secret Service. Secret bytes never enter a state snapshot.

pub use shared::state::secrets::{SecretStatus, SecretsState};

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use shared::{Zeroizing, error, valid_secret_name};
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::{Dict, ObjectPath, OwnedObjectPath, OwnedValue, Type, Value};

pub struct SecretsController {
    inner: Arc<Mutex<SecretsInner>>,
    events: UnboundedSender<SecretsState>,
}

#[derive(Default)]
struct SecretsInner {
    state: SecretsState,
    in_flight: HashSet<String>,
}

const SCHEMA: &str = "io.github.anasgets111.mantle.Secret";
const CONTENT_TYPE: &str = "text/plain;charset=utf-8";
const NONE: &str = "/";

fn ensure_utf8(secret: &[u8]) -> zbus::Result<()> {
    if std::str::from_utf8(secret).is_ok() { Ok(()) } else { Err(zbus::Error::Failure("secret is not utf-8".into())) }
}

/// `(oayays)`: session path, empty parameters, secret bytes, content type.
#[derive(serde::Serialize, Type)]
#[zvariant(signature = "(oayays)")]
struct SecretBytes<'a> {
    session: OwnedObjectPath,
    parameters: &'a [u8],
    bytes: &'a [u8],
    content_type: &'a str,
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Service",
    default_service = "org.freedesktop.secrets",
    default_path = "/org/freedesktop/secrets"
)]
trait Service {
    #[zbus(name = "OpenSession")]
    fn open_session(&self, algorithm: &str, input: &Value<'_>) -> zbus::Result<(OwnedValue, OwnedObjectPath)>;

    #[zbus(name = "ReadAlias")]
    fn read_alias(&self, name: &str) -> zbus::Result<OwnedObjectPath>;

    #[zbus(name = "CreateCollection")]
    fn create_collection(
        &self,
        properties: HashMap<&str, Value<'_>>,
        alias: &str,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;

    #[zbus(name = "Unlock")]
    fn unlock(&self, objects: &[&ObjectPath<'_>]) -> zbus::Result<(Vec<OwnedObjectPath>, OwnedObjectPath)>;
}

#[zbus::proxy(interface = "org.freedesktop.Secret.Collection", default_service = "org.freedesktop.secrets")]
trait Collection {
    #[zbus(name = "CreateItem")]
    fn create_item(
        &self,
        properties: HashMap<&str, Value<'_>>,
        secret: &SecretBytes<'_>,
        replace: bool,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
}

#[zbus::proxy(interface = "org.freedesktop.Secret.Prompt", default_service = "org.freedesktop.secrets")]
trait Prompt {
    #[zbus(name = "Prompt")]
    fn prompt(&self, window_id: &str) -> zbus::Result<()>;

    #[zbus(signal, name = "Completed")]
    fn completed(&self, dismissed: bool, result: Value<'_>) -> zbus::Result<()>;
}

/// Runs the service's own prompt and returns its result, or `None` when `path` is no prompt.
/// It waits as long as the user takes: gnome-keyring keeps a dismissed dialog open and still
/// unlocks from it. A service that leaves the bus meanwhile fails the call.
async fn complete(connection: &zbus::Connection, path: &OwnedObjectPath) -> zbus::Result<Option<OwnedValue>> {
    if path.as_str() == NONE {
        return Ok(None);
    }
    let prompt =
        PromptProxy::builder(connection).path(path)?.cache_properties(zbus::proxy::CacheProperties::No).build().await?;
    // Subscribe first, or a fast service's signal is lost.
    let mut completed = prompt.receive_completed().await?;
    let mut owner = prompt.inner().receive_owner_changed().await?;
    prompt.prompt("").await?;
    let signal = tokio::select! {
        signal = completed.next() => signal,
        // A restarted service never answers the old prompt, so any owner change ends the wait.
        _ = owner.next() => return Err(zbus::Error::Failure("secret service exited".into())),
    };
    let signal = signal.ok_or_else(|| zbus::Error::Failure("prompt closed".into()))?;
    let args = signal.args()?;
    if args.dismissed {
        return Err(zbus::Error::Failure("prompt dismissed".into()));
    }
    Ok(Some(args.result.try_to_owned()?))
}

/// What zbus's own call timeout returns.
fn timed_out(err: &zbus::Error) -> bool {
    matches!(err, zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut)
}

async fn store_on(connection: &zbus::Connection, name: &str, secret: &[u8]) -> zbus::Result<()> {
    ensure_utf8(secret)?;
    let service = ServiceProxy::builder(connection).cache_properties(zbus::proxy::CacheProperties::No).build().await?;
    let (_output, session) = service.open_session("plain", &Value::from("")).await?;
    let mut collection = service.read_alias("default").await?;
    if collection.as_str() == NONE {
        let label = HashMap::from([("org.freedesktop.Secret.Collection.Label", Value::from("Default keyring"))]);
        let (created, prompt) = service.create_collection(label, "default").await?;
        collection = match complete(connection, &prompt).await? {
            Some(result) => OwnedObjectPath::try_from(result)?,
            None => created,
        };
    }
    // A collection that stays locked makes CreateItem fail, which reports unavailable.
    let (_unlocked, prompt) = service.unlock(&[&collection]).await?;
    complete(connection, &prompt).await?;
    let attributes =
        HashMap::from([("xdg:schema".to_string(), SCHEMA.to_string()), ("name".to_string(), name.to_string())]);
    let properties = HashMap::from([
        ("org.freedesktop.Secret.Item.Label", Value::from(format!("Mantle: {name}"))),
        ("org.freedesktop.Secret.Item.Attributes", Value::from(Dict::from(attributes))),
    ]);
    let body = SecretBytes { session, parameters: &[], bytes: secret, content_type: CONTENT_TYPE };
    let collection = CollectionProxy::builder(connection)
        .path(&collection)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let (_item, prompt) = collection.create_item(properties, &body, true).await?;
    complete(connection, &prompt).await?;
    Ok(())
}

impl SecretsController {
    pub fn new(events: UnboundedSender<SecretsState>) -> Self {
        let controller = Self { inner: Arc::new(Mutex::new(SecretsInner::default())), events };
        controller.publish();
        controller
    }

    fn publish(&self) {
        let _ = self.events.send(self.inner.lock().expect("mutex poisoned").state.clone());
    }

    /// The only entry from `SecureSubmit`. A second write to a pending name is refused.
    pub fn store(&self, name: String, secret: Zeroizing<Vec<u8>>) {
        self.store_with(name, secret, |name, secret| async move {
            let connection = super::with_call_timeout(zbus::connection::Builder::session()).await?;
            store_on(&connection, &name, &secret).await
        });
    }

    fn store_with<F, Fut>(&self, name: String, secret: Zeroizing<Vec<u8>>, backend: F)
    where
        F: FnOnce(String, Zeroizing<Vec<u8>>) -> Fut + Send + 'static,
        Fut: Future<Output = zbus::Result<()>> + Send + 'static,
    {
        if !valid_secret_name(&name) {
            return;
        }
        {
            let mut inner = self.inner.lock().expect("mutex poisoned");
            if !inner.in_flight.insert(name.clone()) {
                return;
            }
            inner.state.entries.insert(name.clone(), SecretStatus::Pending);
        }
        self.publish();
        let inner = Arc::clone(&self.inner);
        let events = self.events.clone();
        tokio::spawn(async move {
            let status = match backend(name.clone(), secret).await {
                Ok(()) => SecretStatus::Stored,
                Err(err) => {
                    error!("failed to store a secret named {name}: {err}");
                    if timed_out(&err) { SecretStatus::TimedOut } else { SecretStatus::Unavailable }
                }
            };
            let mut inner = inner.lock().expect("mutex poisoned");
            inner.in_flight.remove(&name);
            inner.state.entries.insert(name, status);
            let _ = events.send(inner.state.clone());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::capabilities::test_support::{PrivateBus, poll_until, private_bus, within};
    use serde::Deserialize;

    #[tokio::test]
    async fn a_mock_store_reports_pending_then_stored_without_the_secret_in_state() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        assert!(rx.recv().await.unwrap().entries.is_empty());
        controller.store_with("mail".into(), Zeroizing::new(b"marker-secret".to_vec()), |name, secret| {
            assert_eq!(name, "mail");
            assert_eq!(secret.as_slice(), b"marker-secret");
            async { Ok(()) }
        });
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Pending));
        let done = rx.recv().await.unwrap();
        assert!(matches!(done.entries["mail"], SecretStatus::Stored));
        assert!(!serde_json::to_string(&done).unwrap().contains("marker-secret"));
    }

    #[tokio::test]
    async fn failure_has_a_fixed_public_code() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        let _ = rx.recv().await;
        controller.store_with("mail".into(), Zeroizing::new(b"private".to_vec()), |_, _| async {
            Err(zbus::Error::Failure("private".into()))
        });
        let _ = rx.recv().await;
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Unavailable));
    }

    #[tokio::test]
    async fn a_pending_name_cannot_start_a_second_write() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        let _ = rx.recv().await;
        let (release, blocked) = tokio::sync::oneshot::channel::<()>();
        controller.store_with("mail".into(), Zeroizing::new(b"first".to_vec()), move |_, _| async move {
            let _ = blocked.await;
            Ok(())
        });
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Pending));
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        controller.store_with("mail".into(), Zeroizing::new(b"second".to_vec()), move |_, _| async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });
        release.send(()).unwrap();
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Stored));
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_timeout_error_reports_timed_out_and_frees_the_name() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        let _ = rx.recv().await;
        controller.store_with("mail".into(), Zeroizing::new(b"first".to_vec()), |_, _| async {
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into())
        });
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Pending));
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::TimedOut));
        controller.store_with("mail".into(), Zeroizing::new(b"second".to_vec()), |_, _| async { Ok(()) });
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Pending));
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Stored));
    }

    const ALIAS: &str = "/org/freedesktop/secrets/aliases/default";
    const CREATED: &str = "/org/freedesktop/secrets/collection/new";

    type Log = Arc<Mutex<Vec<String>>>;

    struct Opened {
        algorithm: String,
        input: String,
        session: String,
    }

    struct Recorded {
        opened: Opened,
        label: String,
        schema: String,
        name: String,
        replace: bool,
        parameters: Vec<u8>,
        secret: Vec<u8>,
        content_type: String,
        secret_session: String,
    }

    fn path(value: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(value).expect("static path")
    }

    struct StubService {
        opened: Arc<Mutex<Option<Opened>>>,
        log: Log,
        has_default: bool,
        locked: bool,
    }

    #[derive(Deserialize, Type)]
    #[zvariant(signature = "(oayays)")]
    struct StoredSecret {
        session: OwnedObjectPath,
        parameters: Vec<u8>,
        bytes: Vec<u8>,
        content_type: String,
    }

    #[zbus::interface(name = "org.freedesktop.Secret.Service")]
    impl StubService {
        #[zbus(name = "OpenSession")]
        fn open_session(
            &mut self,
            algorithm: &str,
            input: Value<'_>,
        ) -> zbus::fdo::Result<(OwnedValue, OwnedObjectPath)> {
            let input = <&str>::try_from(&input).unwrap_or("not-a-string").to_string();
            let session = path("/org/freedesktop/secrets/session/1");
            *self.opened.lock().expect("mutex poisoned") =
                Some(Opened { algorithm: algorithm.to_string(), input, session: session.to_string() });
            Ok((OwnedValue::try_from(Value::from("")).expect("empty string"), session))
        }

        #[zbus(name = "ReadAlias")]
        fn read_alias(&self, name: &str) -> OwnedObjectPath {
            assert_eq!(name, "default");
            path(if self.has_default { ALIAS } else { "/" })
        }

        #[zbus(name = "CreateCollection")]
        fn create_collection(
            &mut self,
            properties: HashMap<String, OwnedValue>,
            alias: &str,
        ) -> (OwnedObjectPath, OwnedObjectPath) {
            let label = properties.get("org.freedesktop.Secret.Collection.Label").map(string_value);
            self.log.lock().expect("mutex poisoned").push(format!("CreateCollection {alias} {label:?}"));
            (path("/"), path("/prompt/create"))
        }

        #[zbus(name = "Unlock")]
        fn unlock(&self, objects: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
            self.log.lock().expect("mutex poisoned").push(format!("Unlock {}", objects[0]));
            if self.locked { (vec![], path("/prompt/unlock")) } else { (objects, path("/")) }
        }
    }

    struct StubPrompt {
        log: Log,
        dismissed: bool,
        silent: bool,
        result: Value<'static>,
    }

    #[zbus::interface(name = "org.freedesktop.Secret.Prompt")]
    impl StubPrompt {
        #[zbus(name = "Prompt")]
        async fn prompt(
            &self,
            window_id: &str,
            #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        ) -> zbus::fdo::Result<()> {
            self.log.lock().expect("mutex poisoned").push(format!("Prompt {window_id:?}"));
            if !self.silent {
                Self::completed(&emitter, self.dismissed, self.result.clone()).await?;
            }
            Ok(())
        }

        #[zbus(signal)]
        async fn completed(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            dismissed: bool,
            result: Value<'_>,
        ) -> zbus::Result<()>;
    }

    struct StubCollection {
        opened: Arc<Mutex<Option<Opened>>>,
        log: Log,
        at: &'static str,
        done: UnboundedSender<Recorded>,
    }

    fn string_value(value: &OwnedValue) -> String {
        String::try_from(value.clone()).unwrap_or_default()
    }

    fn attributes(value: &OwnedValue) -> HashMap<String, String> {
        HashMap::try_from(value.clone()).unwrap_or_default()
    }

    #[zbus::interface(name = "org.freedesktop.Secret.Collection")]
    impl StubCollection {
        #[zbus(name = "CreateItem")]
        fn create_item(
            &mut self,
            properties: HashMap<String, OwnedValue>,
            secret: StoredSecret,
            replace: bool,
        ) -> zbus::fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
            let opened = self.opened.lock().expect("mutex poisoned").take().expect("OpenSession ran first");
            self.log.lock().expect("mutex poisoned").push(format!("CreateItem {}", self.at));
            let attrs = properties.get("org.freedesktop.Secret.Item.Attributes").map(attributes).unwrap_or_default();
            let _ = self.done.send(Recorded {
                label: properties.get("org.freedesktop.Secret.Item.Label").map(string_value).unwrap_or_default(),
                schema: attrs.get("xdg:schema").cloned().unwrap_or_default(),
                name: attrs.get("name").cloned().unwrap_or_default(),
                replace,
                parameters: secret.parameters,
                secret: secret.bytes,
                content_type: secret.content_type,
                secret_session: secret.session.to_string(),
                opened,
            });
            Ok((path("/org/freedesktop/secrets/collection/default/1"), path("/")))
        }
    }

    struct Scenario {
        has_default: bool,
        locked: bool,
        dismissed: bool,
        silent: bool,
    }

    const HAPPY: Scenario = Scenario { has_default: true, locked: false, dismissed: false, silent: false };

    /// Serves the stubs for `scenario` on a private bus, since `complete` watches the service's bus name;
    /// returns the client, the service, what the service saw and the bus.
    async fn serve(
        scenario: Scenario,
    ) -> (zbus::Connection, zbus::Connection, Log, tokio::sync::mpsc::UnboundedReceiver<Recorded>, PrivateBus) {
        let opened = Arc::new(Mutex::new(None));
        let log = Log::default();
        let (done, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (service_log, prompt_log) = (Arc::clone(&log), Arc::clone(&log));
        let collection =
            |at| StubCollection { opened: Arc::clone(&opened), log: Arc::clone(&log), at, done: done.clone() };
        let bus = private_bus().await;
        let peer = bus.builder().name("org.freedesktop.secrets").expect("well-known name");
        let peer = {
            let service = StubService {
                opened: Arc::clone(&opened),
                log: service_log,
                has_default: scenario.has_default,
                locked: scenario.locked,
            };
            let unlocked = Value::new(vec![ObjectPath::from_static_str_unchecked(ALIAS)]);
            let prompt = |result| StubPrompt {
                log: Arc::clone(&prompt_log),
                dismissed: scenario.dismissed,
                silent: scenario.silent,
                result,
            };
            peer.serve_at("/org/freedesktop/secrets", service)
                .and_then(|p| p.serve_at(ALIAS, collection(ALIAS)))
                .and_then(|p| p.serve_at(CREATED, collection(CREATED)))
                .and_then(|p| p.serve_at("/prompt/unlock", prompt(unlocked)))
                .and_then(|p| {
                    p.serve_at("/prompt/create", prompt(Value::from(ObjectPath::from_static_str_unchecked(CREATED))))
                })
                .expect("stub interfaces")
        };
        let server = peer.build().await.expect("serve the stubs");
        (bus.connection().await, server, log, done_rx, bus)
    }

    fn logged(log: &Log) -> Vec<String> {
        log.lock().expect("mutex poisoned").clone()
    }

    #[tokio::test]
    async fn a_plain_session_stores_the_named_secret_on_the_default_collection() {
        let (client, _server, log, mut done_rx, _bus) = serve(HAPPY).await;

        assert!(store_on(&client, "mail", &[0xff]).await.is_err());
        assert!(done_rx.try_recv().is_err(), "a non-utf-8 secret never reaches the service");

        store_on(&client, "mail", b"a\0b").await.expect("CreateItem");
        let recorded = done_rx.recv().await.expect("CreateItem was recorded");
        assert_eq!(recorded.opened.algorithm, "plain");
        assert_eq!(recorded.opened.input, "");
        assert_eq!(recorded.opened.session, recorded.secret_session);
        assert_eq!(recorded.label, "Mantle: mail");
        assert_eq!(recorded.schema, "io.github.anasgets111.mantle.Secret");
        assert_eq!(recorded.name, "mail");
        assert!(recorded.replace);
        assert!(recorded.parameters.is_empty());
        assert_eq!(recorded.secret, b"a\0b");
        assert_eq!(recorded.content_type, "text/plain;charset=utf-8");
        assert_eq!(logged(&log), [format!("Unlock {ALIAS}"), format!("CreateItem {ALIAS}")]);
    }

    #[tokio::test]
    async fn a_locked_collection_is_unlocked_with_the_services_prompt_then_stored() {
        let (client, _server, log, mut done_rx, _bus) = serve(Scenario { locked: true, ..HAPPY }).await;
        within(store_on(&client, "mail", b"s")).await.expect("store");
        assert!(done_rx.try_recv().is_ok());
        assert_eq!(logged(&log), [format!("Unlock {ALIAS}"), "Prompt \"\"".into(), format!("CreateItem {ALIAS}")]);
    }

    #[tokio::test]
    async fn a_dismissed_unlock_prompt_fails_without_creating_the_item() {
        let (client, _server, log, mut done_rx, _bus) =
            serve(Scenario { locked: true, dismissed: true, ..HAPPY }).await;
        assert!(within(store_on(&client, "mail", b"s")).await.is_err());
        assert!(done_rx.try_recv().is_err());
        assert!(!logged(&log).iter().any(|line| line.starts_with("CreateItem")));
    }

    #[tokio::test]
    async fn a_service_that_exits_mid_prompt_fails_the_store() {
        let (client, server, log, _done_rx, _bus) = serve(Scenario { locked: true, silent: true, ..HAPPY }).await;
        let store = tokio::spawn(async move { store_on(&client, "mail", b"s").await });
        poll_until(|| logged(&log).contains(&"Prompt \"\"".to_string()).then_some(())).await;
        drop(server);
        assert!(within(store).await.expect("store task").is_err());
    }

    #[tokio::test]
    async fn a_missing_default_collection_is_created_with_the_services_prompt() {
        let (client, _server, log, mut done_rx, _bus) = serve(Scenario { has_default: false, ..HAPPY }).await;
        within(store_on(&client, "mail", b"s")).await.expect("store");
        assert!(done_rx.try_recv().is_ok());
        assert_eq!(
            logged(&log),
            [
                "CreateCollection default Some(\"Default keyring\")".to_string(),
                "Prompt \"\"".into(),
                format!("Unlock {CREATED}"),
                format!("CreateItem {CREATED}"),
            ]
        );
    }
}

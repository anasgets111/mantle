//! Hand-written `org.bluez.Agent1`; pairing waits for the user's answer in `pairing_request`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shared::{Zeroizing, debug, error};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::proxies::bind_agent_manager;
use super::registry::DeviceRegistry;
use super::{AGENT_OBJECT_PATH, BluetoothSignal, PairingKind, PairingRequest};

/// How long a newly shown request ignores a yes, so a click meant for a request that was just
/// replaced cannot accept the one that replaced it.
pub(super) const ACCEPT_GRACE: Duration = Duration::from_millis(750);

/// `org.bluez.Error.Rejected` as the D-Bus error reply; `zbus::fdo::Error::Failed` has the wrong
/// name (`org.freedesktop.DBus.Error.Failed`).
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// The request on screen, and the reply BlueZ waits on unless the request only displays a code.
pub(in crate::capabilities) struct PendingPrompt {
    pub(super) request: PairingRequest,
    pub(super) reply: Option<oneshot::Sender<Option<Zeroizing<Vec<u8>>>>>,
    shown_at: Instant,
    ready: bool,
    previous: Option<Box<PendingPrompt>>,
}

impl PendingPrompt {
    pub(super) fn visible_request(&self) -> Option<&PairingRequest> {
        if self.ready { Some(&self.request) } else { self.previous.as_ref().map(|prompt| &prompt.request) }
    }
}

#[cfg(test)]
impl PendingPrompt {
    pub(super) fn display(mac: &str) -> Self {
        let request = PairingRequest {
            kind: PairingKind::Display,
            id: String::new(),
            mac: mac.to_string(),
            name: String::new(),
            code: None,
        };
        Self { request, reply: None, shown_at: Instant::now(), ready: true, previous: None }
    }

    pub(super) fn reserved(mac: &str, previous: PendingPrompt) -> Self {
        let (reply, _answer) = oneshot::channel();
        Self { reply: Some(reply), ready: false, previous: Some(Box::new(previous)), ..Self::display(mac) }
    }
}

/// One prompt at a time. The agent fills it; [`answer`], [`submit`] and [`clear_display`] remove prompts.
pub(super) type PromptSlot = Arc<Mutex<Option<PendingPrompt>>>;

/// Whether the device with this MAC may raise a prompt now; `BluetoothController::invited` answers.
pub(super) type Invited = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Answers the current prompt. `Some(mac)` must match a ready prompt; accepts for entry kinds are
/// refused. `None`, from BlueZ's `Cancel`, cancels the slot's request. A yes before [`ACCEPT_GRACE`]
/// has passed is ignored. Returns whether the visible request changed.
pub(super) fn answer(prompts: &PromptSlot, mac: Option<&str>, accept: bool, now: Instant) -> bool {
    let (prompt, changed) = {
        let mut slot = prompts.lock().expect("mutex poisoned");
        let Some(current) = slot.as_ref() else {
            return false;
        };
        if let Some(mac) = mac {
            if !current.ready || mac != current.request.mac {
                return false;
            }
            if accept && matches!(current.request.kind, PairingKind::PinEntry | PairingKind::PasskeyEntry) {
                return false;
            }
        }
        if accept && now.duration_since(current.shown_at) < ACCEPT_GRACE {
            return false;
        }
        let mut prompt = slot.take().expect("prompt exists");
        let changed = prompt.ready;
        if !changed {
            *slot = prompt.previous.take().map(|previous| *previous);
        }
        (prompt, changed)
    };
    if let Some(reply) = prompt.reply {
        let _ = reply.send(accept.then(|| Zeroizing::new(Vec::new())));
    }
    changed
}

/// The target name carries both values; a stale field cannot answer a later request for the same
/// device. Invalid entry rejects the BlueZ call after its bytes leave this function and zeroize.
pub(in crate::capabilities) fn submit(prompts: &PromptSlot, target: &str, secret: Zeroizing<Vec<u8>>) -> bool {
    let prompt = {
        let mut slot = prompts.lock().expect("mutex poisoned");
        let Some(current) = slot.as_ref() else { return false };
        if !current.ready
            || !matches!(current.request.kind, PairingKind::PinEntry | PairingKind::PasskeyEntry)
            || target != format!("{}/{}", current.request.id, current.request.mac)
        {
            return false;
        }
        slot.take().expect("entry was checked under the slot lock")
    };
    let valid = match prompt.request.kind {
        PairingKind::PinEntry => (1..=16).contains(&secret.len()) && secret.iter().all(u8::is_ascii_alphanumeric),
        PairingKind::PasskeyEntry => (1..=6).contains(&secret.len()) && secret.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if let Some(reply) = prompt.reply {
        let _ = reply.send(valid.then_some(secret));
    }
    true
}

/// Takes down a code display for `mac`, if that is what is showing. Returns whether it did.
pub(super) fn clear_display(prompts: &PromptSlot, mac: &str) -> bool {
    let mut slot = prompts.lock().expect("mutex poisoned");
    match slot.as_mut() {
        Some(prompt) if prompt.ready && prompt.reply.is_none() && prompt.request.mac == mac => {
            slot.take();
            true
        }
        Some(prompt)
            if !prompt.ready && prompt.previous.as_ref().is_some_and(|previous| previous.request.mac == mac) =>
        {
            prompt.previous.take();
            true
        }
        _ => false,
    }
}

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` to `AA:BB:CC:DD:EE:FF`. BlueZ names every device object
/// this way, so the prompt is labelled without a call.
fn mac_from_path(path: &str) -> String {
    path.rsplit('/').next().and_then(|leaf| leaf.strip_prefix("dev_")).unwrap_or(path).replace('_', ":")
}

/// `org.bluez.Agent1`, this session's only pairing agent. Only an invited device (the adapter is
/// visible, or this Supervisor is pairing it) raises a prompt; a `"service"` request asks instead
/// when a tracked device reads `Paired`.
///
/// ponytail: hidden adapters refuse other clients' pairing; upgrade by checking `Device1.Pairing`.
struct BluetoothAgent {
    prompts: PromptSlot,
    devices: DeviceRegistry,
    invited: Invited,
    events: UnboundedSender<BluetoothSignal>,
}

impl BluetoothAgent {
    /// Shows `kind` for an invited `device` and waits for the user.
    async fn ask(
        &self,
        kind: PairingKind,
        device: &OwnedObjectPath,
        code: Option<String>,
    ) -> Result<Zeroizing<Vec<u8>>, AgentError> {
        let (reply, answered) = oneshot::channel();
        if !self.show(kind, device, code, Some(reply)).await {
            return Err(AgentError::Rejected("the device is not invited, or another request is on screen".to_string()));
        }
        match answered.await {
            Ok(Some(secret)) => Ok(secret),
            _ => Err(AgentError::Rejected("the user declined, or BlueZ cancelled the request".to_string())),
        }
    }

    /// Puts the request on screen and returns whether it went up. An uninvited device's request
    /// stays down, and so does a `"service"` request from a device that is not tracked and paired:
    /// BlueZ asks for any untrusted device, and the card says it is paired. Nothing replaces a
    /// prompt already showing, except that a request BlueZ waits on replaces a code display, which
    /// has no answer to lose.
    async fn show(
        &self,
        kind: PairingKind,
        device: &OwnedObjectPath,
        code: Option<String>,
        reply: Option<oneshot::Sender<Option<Zeroizing<Vec<u8>>>>>,
    ) -> bool {
        let mac = mac_from_path(device.as_str());
        let proxy = self.devices.lock().expect("mutex poisoned").get(device).map(|entry| entry.device.clone());
        if (kind == PairingKind::Service && proxy.is_none()) || (kind != PairingKind::Service && !(self.invited)(&mac))
        {
            debug!("refused a {kind:?} request from {mac}: not invited, or an unknown service device");
            return false;
        }
        let Some(id) = crate::capabilities::next_request_id() else { return false };
        {
            let mut slot = self.prompts.lock().expect("mutex poisoned");
            let free = slot.as_ref().is_none_or(|current| current.ready && current.reply.is_none() && reply.is_some());
            if !free {
                return false;
            }
            let previous = slot.take().map(Box::new);
            *slot = Some(PendingPrompt {
                request: PairingRequest { kind, id: id.clone(), mac: mac.clone(), name: String::new(), code },
                reply,
                shown_at: Instant::now(),
                ready: false,
                previous,
            });
        }
        let allowed = match (kind, &proxy) {
            (PairingKind::Service, Some(proxy)) => proxy.paired().await.unwrap_or(false),
            _ => true,
        };
        let name = match (allowed, proxy) {
            (true, Some(proxy)) => proxy.name().await.unwrap_or_default(),
            _ => String::new(),
        };
        let mut slot = self.prompts.lock().expect("mutex poisoned");
        if !slot.as_ref().is_some_and(|current| current.request.id == id) {
            return false;
        }
        if !allowed {
            *slot = slot.take().and_then(|prompt| prompt.previous.map(|previous| *previous));
            debug!("refused a {kind:?} request from {mac}: not invited, or a service request from an unpaired device");
            return false;
        }
        let current = slot.as_mut().expect("reservation exists");
        current.request.name = name;
        current.shown_at = Instant::now();
        current.ready = true;
        current.previous.take();
        drop(slot);
        let _ = self.events.send(BluetoothSignal::PairingChanged);
        true
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl BluetoothAgent {
    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        let mut secret = self.ask(PairingKind::PinEntry, &device, None).await?;
        // ponytail: zbus requires an owned String; use a secret-aware serializer to remove that copy.
        String::from_utf8(std::mem::take(&mut *secret)).map_err(|_| AgentError::Rejected("invalid PIN".to_string()))
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        let secret = self.ask(PairingKind::PasskeyEntry, &device, None).await?;
        Ok(secret.iter().fold(0, |value, digit| value * 10 + u32::from(digit - b'0')))
    }

    /// An error here cancels the pairing, which is right when nobody was shown the PIN.
    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) -> Result<(), AgentError> {
        if self.show(PairingKind::Display, &device, Some(pincode), None).await {
            Ok(())
        } else {
            Err(AgentError::Rejected("the PIN could not be shown".to_string()))
        }
    }

    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> Result<(), AgentError> {
        self.ask(PairingKind::Confirm, &device, Some(format!("{passkey:06}"))).await.map(|_| ())
    }

    /// BlueZ repeats this for every key typed on the device. The first call shows the code, and the
    /// rest find the slot taken by that same code.
    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        self.show(PairingKind::Display, &device, Some(format!("{passkey:06}")), None).await;
    }

    async fn authorize_service(&self, device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        self.ask(PairingKind::Service, &device, None).await.map(|_| ())
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        self.ask(PairingKind::Authorize, &device, None).await.map(|_| ())
    }

    async fn cancel(&self) {
        debug!(2; "pairing cancelled");
        if answer(&self.prompts, None, false, Instant::now()) {
            let _ = self.events.send(BluetoothSignal::PairingChanged);
        }
    }

    async fn release(&self) {
        self.cancel().await;
    }
}

/// Exports [`BluetoothAgent`] once; [`register_with_bluez`] offers it to each `bluetoothd`.
pub(super) async fn export_agent(
    connection: &zbus::Connection,
    prompts: PromptSlot,
    devices: DeviceRegistry,
    invited: Invited,
    events: UnboundedSender<BluetoothSignal>,
) {
    let agent = BluetoothAgent { prompts, devices, invited, events };
    if let Err(err) = connection.object_server().at(AGENT_OBJECT_PATH, agent).await {
        error!("failed to export the Agent1 object at {AGENT_OBJECT_PATH}: {err}");
    }
}

/// Registers the exported agent as the system default `"KeyboardDisplay"` agent, so pairing started
/// elsewhere asks here too and BlueZ runs numeric comparison, not Just Works. Runs for every
/// `bluetoothd`, which forgets its agents when it exits. Each step logs and continues, since a
/// missing `bluetoothd` must not take down the Supervisor.
pub(super) async fn register_with_bluez(connection: &zbus::Connection) {
    let agent_manager = match bind_agent_manager(connection).await {
        Ok(proxy) => proxy,
        Err(err) => {
            debug!("failed to bind org.bluez.AgentManager1 (bluetoothd not running?): {err}");
            return;
        }
    };
    let path = ObjectPath::from_static_str_unchecked(AGENT_OBJECT_PATH);
    if let Err(err) = agent_manager.register_agent(&path, "KeyboardDisplay").await {
        error!("RegisterAgent failed: {err}");
        return;
    }
    if let Err(err) = agent_manager.request_default_agent(&path).await {
        debug!("RequestDefaultAgent failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::super::proxies::Device1Proxy;
    use super::super::registry::DeviceEntry;
    use super::*;
    use crate::capabilities::test_support::p2p_pair_serving;

    const MAC: &str = "00:11:22:33:44:55";

    /// A p2p pair with the agent already exported, returned as `(caller, agent, prompts, signals)`.
    /// `invited` answers every device the same way. Every test here really calls the agent, so it
    /// goes in through the builder rather than `object_server().at(..)` afterwards; see
    /// `test_support::p2p_pair` for why that ordering is the difference between a reply and a
    /// dropped call.
    async fn agent_pair(
        invited: bool,
        devices: DeviceRegistry,
    ) -> (zbus::Connection, zbus::Connection, PromptSlot, UnboundedReceiver<BluetoothSignal>) {
        let prompts = PromptSlot::default();
        let (events, signals) = unbounded_channel();
        let agent = BluetoothAgent { prompts: prompts.clone(), devices, invited: Arc::new(move |_| invited), events };
        let (caller, agent_side) = p2p_pair_serving(|peer| peer.serve_at(AGENT_OBJECT_PATH, agent)).await;
        (caller, agent_side, prompts, signals)
    }

    async fn agent1_proxy(caller_side: &zbus::Connection) -> zbus::Proxy<'_> {
        zbus::proxy::Builder::new(caller_side)
            .destination("org.mantle.Supervisor")
            .expect("valid destination bus name")
            .path(AGENT_OBJECT_PATH)
            .expect("valid object path")
            .interface("org.bluez.Agent1")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p proxy to the agent")
    }

    fn dummy_device_path() -> zbus::zvariant::ObjectPath<'static> {
        zbus::zvariant::ObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55").expect("valid object path")
    }

    fn rejected<T: std::fmt::Debug>(result: zbus::Result<T>) -> bool {
        matches!(result, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.bluez.Error.Rejected")
    }

    /// A yes as the user can first give it, once the grace has passed.
    fn after_grace() -> Instant {
        Instant::now() + ACCEPT_GRACE
    }

    #[tokio::test]
    async fn pin_and_passkey_entry_wait_for_the_matching_secure_target() {
        let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(),);
        let pin = proxy.call::<_, _, String>("RequestPinCode", &args);
        tokio::pin!(pin);
        tokio::select! {
            _ = &mut pin => panic!("PIN returned without a secure answer"),
            _ = signals.recv() => {}
        }
        let request = prompts.lock().unwrap().as_ref().unwrap().request.clone();
        assert_eq!(request.kind, PairingKind::PinEntry);
        let target = format!("{}/{}", request.id, request.mac);
        assert!(!submit(
            &prompts,
            &format!("{}/{}", request.id, "AA:AA:AA:AA:AA:AA"),
            Zeroizing::new(b"Ab12".to_vec())
        ));
        assert!(!submit(&prompts, &format!("0/{}", request.mac), Zeroizing::new(b"Ab12".to_vec())));
        assert!(!answer(&prompts, Some(MAC), true, after_grace()), "Lua cannot accept an entry prompt");
        assert!(submit(&prompts, &target, Zeroizing::new(b"A123456789012345".to_vec())));
        assert_eq!(pin.await.unwrap(), "A123456789012345");

        let passkey = proxy.call::<_, _, u32>("RequestPasskey", &args);
        tokio::pin!(passkey);
        tokio::select! {
            _ = &mut passkey => panic!("passkey returned without a secure answer"),
            _ = signals.recv() => {}
        }
        let next = prompts.lock().unwrap().as_ref().unwrap().request.clone();
        assert_eq!(next.kind, PairingKind::PasskeyEntry);
        assert_ne!(request.id, next.id);
        assert!(!submit(&prompts, &target, Zeroizing::new(b"123456".to_vec())));
        assert!(submit(&prompts, &format!("{}/{}", next.id, next.mac), Zeroizing::new(b"999999".to_vec())));
        assert_eq!(passkey.await.unwrap(), 999999);

        let declined = proxy.call::<_, _, String>("RequestPinCode", &args);
        tokio::pin!(declined);
        tokio::select! {
            _ = &mut declined => panic!("PIN returned before a refusal"),
            _ = signals.recv() => {}
        }
        assert!(!answer(&prompts, Some(MAC), true, after_grace()), "Lua cannot accept an entry prompt");
        assert!(answer(&prompts, Some(MAC), false, Instant::now()));
        assert!(rejected(declined.await));
    }

    #[tokio::test]
    async fn invalid_entry_rejects_bluez_and_clears_the_slot() {
        for (method, bytes) in [
            ("RequestPinCode", b"".as_slice()),
            ("RequestPinCode", b"a-".as_slice()),
            ("RequestPinCode", b"12345678901234567".as_slice()),
            ("RequestPinCode", b"\xc3\xa9".as_slice()),
            ("RequestPasskey", b"".as_slice()),
            ("RequestPasskey", b"1000000".as_slice()),
            ("RequestPasskey", b"12a4".as_slice()),
        ] {
            let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
            let proxy = agent1_proxy(&caller).await;
            let args = (dummy_device_path(),);
            let call = proxy.call::<_, _, zbus::zvariant::OwnedValue>(method, &args);
            tokio::pin!(call);
            tokio::select! {
                _ = &mut call => panic!("entry returned without an answer"),
                _ = signals.recv() => {}
            }
            let request = prompts.lock().unwrap().as_ref().unwrap().request.clone();
            assert!(submit(&prompts, &format!("{}/{}", request.id, request.mac), Zeroizing::new(bytes.to_vec())));
            assert!(rejected(call.await));
            assert!(prompts.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn a_confirmation_waits_for_the_user_and_returns_their_yes() {
        let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), 1234u32);
        let call = proxy.call::<_, _, ()>("RequestConfirmation", &args);
        tokio::pin!(call);

        tokio::select! {
            _ = &mut call => panic!("the call returned before the user answered"),
            signal = signals.recv() => assert_eq!(signal, Some(BluetoothSignal::PairingChanged)),
        }
        let request = prompts.lock().unwrap().as_ref().expect("the request is on screen").request.clone();
        assert_eq!(request.kind, PairingKind::Confirm);
        assert_eq!(request.code.as_deref(), Some("001234"), "a passkey keeps its leading zeros");
        assert_eq!(request.mac, MAC);

        assert!(answer(&prompts, Some(MAC), true, after_grace()));
        call.await.expect("an accepted confirmation returns Ok");
    }

    #[tokio::test]
    async fn an_uninvited_device_raises_nothing() {
        let (caller, _agent, prompts, _signals) = agent_pair(false, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;

        assert!(rejected(proxy.call::<_, _, ()>("RequestAuthorization", &(dummy_device_path(),)).await));
        assert!(rejected(proxy.call::<_, _, ()>("RequestConfirmation", &(dummy_device_path(), 1u32)).await));
        assert!(
            rejected(proxy.call::<_, _, ()>("DisplayPinCode", &(dummy_device_path(), "0000")).await),
            "a PIN nobody was shown cancels the pairing"
        );
        assert!(prompts.lock().unwrap().is_none());

        *prompts.lock().unwrap() = Some(PendingPrompt::display("AA:AA:AA:AA:AA:AA"));
        assert!(rejected(proxy.call::<_, _, ()>("RequestAuthorization", &(dummy_device_path(),)).await));
        assert_eq!(prompts.lock().unwrap().as_ref().unwrap().request.mac, "AA:AA:AA:AA:AA:AA");
    }

    /// A `Device1` that answers only `Paired`.
    struct Bonded(bool);

    #[zbus::interface(name = "org.bluez.Device1")]
    impl Bonded {
        #[zbus(property)]
        async fn paired(&self) -> bool {
            self.0
        }
    }

    async fn tracked_with(
        served_device: impl zbus::object_server::Interface,
        cache: zbus::proxy::CacheProperties,
    ) -> (DeviceRegistry, zbus::Connection, zbus::Connection) {
        let (caller, served) = p2p_pair_serving(|peer| peer.serve_at(dummy_device_path(), served_device)).await;
        let device = Device1Proxy::builder(&caller)
            .path(dummy_device_path())
            .unwrap()
            .cache_properties(cache)
            .build()
            .await
            .unwrap();
        let entry = DeviceEntry { mac: MAC.to_string(), device, battery: None, forwarder: tokio::spawn(async {}) };
        let devices = DeviceRegistry::default();
        devices.lock().unwrap().insert(dummy_device_path().into(), entry);
        (devices, caller, served)
    }

    /// A registry tracking one device at `dummy_device_path()` whose `Paired` reads `paired`.
    async fn tracked(paired: bool) -> (DeviceRegistry, zbus::Connection, zbus::Connection) {
        tracked_with(Bonded(paired), zbus::proxy::CacheProperties::Lazily).await
    }

    struct SlowName {
        started: Arc<tokio::sync::Notify>,
        resume: Arc<tokio::sync::Notify>,
    }

    #[zbus::interface(name = "org.bluez.Device1")]
    impl SlowName {
        #[zbus(property)]
        async fn name(&self) -> String {
            self.started.notify_one();
            self.resume.notified().await;
            "Late name".to_string()
        }
    }

    #[tokio::test]
    async fn cancel_during_name_read_cannot_publish_the_reserved_request() {
        let started = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let device = SlowName { started: started.clone(), resume: resume.clone() };
        let (devices, _caller, _served) = tracked_with(device, zbus::proxy::CacheProperties::No).await;
        let prompts = PromptSlot::default();
        *prompts.lock().unwrap() = Some(PendingPrompt::display("AA:AA:AA:AA:AA:AA"));
        let (events, mut signals) = unbounded_channel();
        let agent = BluetoothAgent { prompts: prompts.clone(), devices, invited: Arc::new(|_| true), events };
        let path = dummy_device_path().into();
        let show = agent.show(PairingKind::PinEntry, &path, None, Some(oneshot::channel().0));
        tokio::pin!(show);
        tokio::select! {
            _ = &mut show => panic!("name read returned before the test released it"),
            _ = started.notified() => {}
        }
        let request = prompts.lock().unwrap().as_ref().unwrap().request.clone();
        assert_eq!(prompts.lock().unwrap().as_ref().unwrap().visible_request().unwrap().mac, "AA:AA:AA:AA:AA:AA");
        assert!(!submit(&prompts, &format!("{}/{}", request.id, request.mac), Zeroizing::new(b"1234".to_vec())));
        assert!(!answer(&prompts, Some(MAC), false, Instant::now()));
        assert!(!answer(&prompts, None, false, Instant::now()));
        assert_eq!(prompts.lock().unwrap().as_ref().unwrap().request.mac, "AA:AA:AA:AA:AA:AA");
        resume.notify_one();
        assert!(!show.await);
        assert_eq!(prompts.lock().unwrap().as_ref().unwrap().request.mac, "AA:AA:AA:AA:AA:AA");
        assert!(signals.try_recv().is_err(), "the cancelled request was never published");
    }

    #[tokio::test]
    async fn a_service_request_while_hidden_needs_a_tracked_paired_device() {
        let hid = "00001124-0000-1000-8000-00805f9b34fb";
        let (unpaired, _unpaired_caller, _unpaired_device) = tracked(false).await;
        for devices in [DeviceRegistry::default(), unpaired] {
            let (caller, _agent, prompts, _signals) = agent_pair(false, devices).await;
            let proxy = agent1_proxy(&caller).await;
            assert!(rejected(proxy.call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), hid)).await));
            assert!(prompts.lock().unwrap().is_none(), "an unknown or unpaired device raises nothing");
            *prompts.lock().unwrap() = Some(PendingPrompt::display("AA:AA:AA:AA:AA:AA"));
            assert!(rejected(proxy.call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), hid)).await));
            assert_eq!(prompts.lock().unwrap().as_ref().unwrap().request.mac, "AA:AA:AA:AA:AA:AA");
        }

        let (paired, _paired_caller, _paired_device) = tracked(true).await;
        let (caller, _agent, prompts, mut signals) = agent_pair(false, paired).await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), hid);
        let call = proxy.call::<_, _, ()>("AuthorizeService", &args);
        tokio::pin!(call);
        tokio::select! {
            _ = &mut call => panic!("a paired device's request waits for the user"),
            _ = signals.recv() => {}
        }
        assert_eq!(prompts.lock().unwrap().as_ref().map(|prompt| prompt.request.kind), Some(PairingKind::Service));
        assert!(answer(&prompts, Some(MAC), false, Instant::now()));
        assert!(rejected(call.await));
    }

    #[tokio::test]
    async fn cancel_and_release_reject_waiting_requests() {
        // zbus serves `Cancel` while `RequestConfirmation` is parked on the user; this pins that.
        let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), 1234u32);
        let call = proxy.call::<_, _, ()>("RequestConfirmation", &args);
        tokio::pin!(call);
        tokio::select! {
            _ = &mut call => panic!("the call returned before anyone answered"),
            _ = signals.recv() => {}
        }

        let (waiting, cancelled) = tokio::join!(call, proxy.call::<_, _, ()>("Cancel", &()));

        cancelled.expect("Cancel must succeed");
        assert!(rejected(waiting));
        assert!(prompts.lock().unwrap().is_none());
        assert_eq!(signals.try_recv(), Ok(BluetoothSignal::PairingChanged));

        let entry_args = (dummy_device_path(),);
        let entry = proxy.call::<_, _, String>("RequestPinCode", &entry_args);
        tokio::pin!(entry);
        tokio::select! {
            _ = &mut entry => panic!("the PIN returned before anyone answered"),
            _ = signals.recv() => {}
        }
        let (waiting, released) = tokio::join!(entry, proxy.call::<_, _, ()>("Release", &()));
        released.expect("Release must succeed");
        assert!(rejected(waiting));
        assert!(prompts.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_second_request_while_one_waits_is_refused_at_once() {
        let (caller, _agent, prompts, _signals) = agent_pair(true, DeviceRegistry::default()).await;
        let (reply, _answered) = oneshot::channel();
        *prompts.lock().unwrap() = Some(PendingPrompt { reply: Some(reply), ..PendingPrompt::display(MAC) });

        let proxy = agent1_proxy(&caller).await;
        let result = proxy
            .call::<_, _, ()>("AuthorizeService", &(dummy_device_path(), "0000110b-0000-1000-8000-00805f9b34fb"))
            .await;

        assert!(rejected(result));
        assert!(prompts.lock().unwrap().is_some(), "the prompt already waiting stays");
    }

    #[tokio::test]
    async fn a_confirmation_replaces_a_code_display() {
        let (caller, _agent, prompts, mut signals) = agent_pair(true, DeviceRegistry::default()).await;
        *prompts.lock().unwrap() = Some(PendingPrompt::display("AA:AA:AA:AA:AA:AA"));
        let proxy = agent1_proxy(&caller).await;
        let args = (dummy_device_path(), 7u32);
        let call = proxy.call::<_, _, ()>("RequestConfirmation", &args);
        tokio::pin!(call);
        tokio::select! {
            _ = &mut call => panic!("the call returned before anyone answered"),
            _ = signals.recv() => {}
        }

        assert_eq!(prompts.lock().unwrap().as_ref().map(|prompt| prompt.request.kind), Some(PairingKind::Confirm));
        assert!(answer(&prompts, Some(MAC), false, Instant::now()));
        assert!(rejected(call.await));
    }

    #[test]
    fn a_yes_is_ignored_too_soon_or_for_another_device() {
        let prompts = PromptSlot::default();
        let (reply, mut answered) = oneshot::channel();
        *prompts.lock().unwrap() = Some(PendingPrompt { reply: Some(reply), ..PendingPrompt::display(MAC) });

        assert!(!answer(&prompts, Some(MAC), true, Instant::now()), "a yes inside the grace is ignored");
        assert!(
            !answer(&prompts, Some("AA:AA:AA:AA:AA:AA"), true, after_grace()),
            "an answer for another device is ignored"
        );
        assert!(prompts.lock().unwrap().is_some());

        assert!(answer(&prompts, Some(MAC), true, after_grace()));
        assert!(answered.try_recv().unwrap().is_some());
    }
}

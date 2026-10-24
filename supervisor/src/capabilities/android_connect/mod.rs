//! `mantle.android_connect` tracks connected Android / KDE Connect devices and actions.

pub(super) mod config;
pub mod controller;
pub mod proxies;

pub use controller::AndroidConnectController;
use shared::action::AndroidConnectAction;

/// `mantle.android_connect` action dispatch (ADR-0037): one spawned task per action, a no-op while
/// the daemon is absent.
pub fn dispatch(controller: &AndroidConnectController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<AndroidConnectAction>(&envelope.params) else { return };
    let controller = controller.clone();
    let name = envelope.params.action.clone();
    tokio::spawn(async move {
        let Some(proxy) = controller.proxy().await else { return };
        let result = match &action {
            AndroidConnectAction::Broadcast => proxy.broadcast_identity().await,
            AndroidConnectAction::Ring { id } => proxy.ring_device(id).await,
            AndroidConnectAction::Ping { id, message } => proxy.send_ping(id, message.as_deref().unwrap_or("")).await,
            AndroidConnectAction::SendClipboard { id, text } => proxy.send_clipboard(id, text).await,
            AndroidConnectAction::ShareClipboard { id } => proxy.share_clipboard(id).await,
            AndroidConnectAction::SendFiles { id, files } => {
                if !controller::sendable(files) {
                    return;
                }
                proxy.send_files(id, files).await
            }
            AndroidConnectAction::Browse { id } => {
                controller.edit(id, |d| d.browse_error = None);
                proxy.browse_device(id).await
            }
            AndroidConnectAction::Unmount { id } => proxy.unmount_device(id).await,
            AndroidConnectAction::Pair { id } => {
                let result = proxy.pair_device(id).await;
                if result.is_ok() {
                    controller.set_pairing_sent(id, true);
                }
                result
            }
            AndroidConnectAction::Unpair { id } => {
                controller.set_pairing_sent(id, false);
                proxy.unpair_device(id).await
            }
            AndroidConnectAction::AcceptPairing { id } => proxy.accept_pairing(id).await,
            AndroidConnectAction::RejectPairing { id } => {
                // The daemon sends nothing on a reject that would clear the flag.
                controller.edit(id, |d| d.pairing_requested = false);
                proxy.reject_pairing(id).await
            }
            AndroidConnectAction::SetPluginEnabled { id, plugin, enabled } => {
                let result = proxy.set_plugin_enabled(id, plugin, *enabled).await;
                // The daemon saves after it replies, so asking for the list now can return the old one.
                if result.is_ok() {
                    controller.set_plugin_disabled(id, plugin, !enabled);
                }
                result
            }
            AndroidConnectAction::AddCommand { name: label, command } => {
                resend(&controller, &proxy, &name, controller.add_command(label, command)).await;
                return;
            }
            AndroidConnectAction::RemoveCommand { id } => {
                resend(&controller, &proxy, &name, controller.remove_command(id)).await;
                return;
            }
            AndroidConnectAction::SetClipboardAutoShare { id, enabled } => {
                if let Err(err) = controller.set_clipboard_auto_share(id, *enabled) {
                    shared::warn!("set_clipboard_auto_share: {err}");
                }
                return;
            }
        };
        if let Err(err) = result {
            shared::debug!("{name} failed: {err}");
        }
    });
}

/// After a command edit that changed the file, resends the list to every connected paired device.
async fn resend(
    controller: &AndroidConnectController,
    proxy: &proxies::DaemonProxy<'_>,
    action: &str,
    changed: std::io::Result<bool>,
) {
    match changed {
        Ok(true) => {
            for d in controller.snapshot().devices.iter().filter(|d| d.paired && d.reachable) {
                let _ = proxy.push_local_commands(&d.id).await;
            }
        }
        Ok(false) => {}
        Err(err) => shared::warn!("{action}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    use zbus::object_server::SignalEmitter;

    use super::controller::*;
    use super::proxies::{DbusDevice, SERVICE_NAME};
    use crate::capabilities::test_support::{PrivateBus, p2p_pair_serving, poll_until, private_bus};

    const DAEMON_PATH: &str = "/io/github/hepp3n/kdeconnect/Daemon";

    /// What the fake daemon saw, shared with the test.
    #[derive(Default, Clone)]
    struct Calls {
        files: Arc<Mutex<Vec<Vec<String>>>>,
        /// `method:args` of the pass-through calls.
        log: Arc<Mutex<Vec<String>>>,
        list_attempts: Arc<AtomicUsize>,
        /// `list_devices` fails until set: the daemon holds its name while it still initialises.
        ready: Arc<AtomicBool>,
    }

    struct FakeDaemon(Calls);

    fn phone(paired: bool) -> DbusDevice {
        DbusDevice {
            id: "phone-123".into(),
            name: "Test Phone".into(),
            device_type: "phone".into(),
            is_paired: paired,
            is_reachable: true,
        }
    }

    #[zbus::interface(name = "io.github.hepp3n.kdeconnect.Daemon")]
    impl FakeDaemon {
        async fn list_devices(&self) -> zbus::fdo::Result<Vec<DbusDevice>> {
            self.0.list_attempts.fetch_add(1, Ordering::SeqCst);
            if !self.0.ready.load(Ordering::SeqCst) {
                return Err(zbus::fdo::Error::Failed("still starting".into()));
            }
            Ok(vec![phone(true)])
        }

        async fn mounted_devices(&self) -> Vec<String> {
            vec!["phone-123".into()]
        }

        async fn broadcast_identity(&self) {
            self.0.log.lock().unwrap().push("broadcast".into());
        }

        async fn ring_device(&self, device_id: &str) {
            self.0.log.lock().unwrap().push(format!("ring:{device_id}"));
        }

        async fn send_ping(&self, device_id: &str, message: &str) {
            self.0.log.lock().unwrap().push(format!("ping:{device_id}:{message}"));
        }

        async fn send_clipboard(&self, device_id: &str, content: &str) {
            self.0.log.lock().unwrap().push(format!("send_clipboard:{device_id}:{content}"));
        }

        async fn share_clipboard(&self, device_id: &str) {
            self.0.log.lock().unwrap().push(format!("share_clipboard:{device_id}"));
        }

        async fn unmount_device(&self, device_id: &str) {
            self.0.log.lock().unwrap().push(format!("unmount:{device_id}"));
        }

        async fn accept_pairing(&self, device_id: &str) {
            self.0.log.lock().unwrap().push(format!("accept_pairing:{device_id}"));
        }

        async fn reject_pairing(&self, device_id: &str) {
            self.0.log.lock().unwrap().push(format!("reject_pairing:{device_id}"));
        }

        async fn push_local_commands(&self, device_id: &str) {
            self.0.log.lock().unwrap().push(format!("push_commands:{device_id}"));
        }

        async fn pair_device(&self, _device_id: &str) {}

        async fn unpair_device(&self, _device_id: &str) {}

        async fn browse_device(&self, _device_id: &str) {}

        async fn get_disabled_plugins(&self, _device_id: &str) -> Vec<String> {
            vec!["sms".into()]
        }

        async fn set_plugin_enabled(&self, _device_id: &str, _plugin_id: &str, _enabled: bool) {}

        async fn send_files(&self, _device_id: &str, files: Vec<String>) {
            self.0.files.lock().unwrap().push(files);
        }

        #[zbus(signal)]
        async fn device_connected(e: &SignalEmitter<'_>, device_id: String, device: DbusDevice) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn device_paired(e: &SignalEmitter<'_>, device_id: String, device: DbusDevice) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn device_disconnected(e: &SignalEmitter<'_>, device_id: String) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn browse_failed(e: &SignalEmitter<'_>, device_id: String, message: String) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn mount_state_changed(e: &SignalEmitter<'_>, device_id: String, mounted: bool) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn update_transfer_progress(e: &SignalEmitter<'_>, progress: u8) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn pairing_requested(e: &SignalEmitter<'_>, device_id: String, device_name: String) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn battery_received(
            e: &SignalEmitter<'_>,
            device_id: String,
            level: i32,
            is_charging: bool,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn connectivity_received(
            e: &SignalEmitter<'_>,
            device_id: String,
            signal_strength: i32,
        ) -> zbus::Result<()>;
    }

    fn ready_calls() -> Calls {
        let calls = Calls::default();
        calls.ready.store(true, Ordering::SeqCst);
        calls
    }

    /// The fake daemon on a private bus, holding the daemon's well-known name.
    async fn daemon_on(bus: &PrivateBus, calls: Calls) -> zbus::Connection {
        let builder = bus.builder().name(SERVICE_NAME).and_then(|b| b.serve_at(DAEMON_PATH, FakeDaemon(calls)));
        builder.expect("serve the fake daemon").build().await.expect("fake daemon connects")
    }

    fn controller_on(connection: zbus::Connection) -> AndroidConnectController {
        AndroidConnectController::new(Some(connection), None, mpsc::unbounded_channel().0)
    }

    /// A running session against the fake over p2p, once it reports the fake's devices.
    async fn session() -> (Arc<Mutex<AndroidConnectState>>, zbus::object_server::InterfaceRef<FakeDaemon>) {
        let (connection, server) = p2p_pair_serving(|b| b.serve_at(DAEMON_PATH, FakeDaemon(ready_calls()))).await;
        let state = Arc::new(Mutex::new(AndroidConnectState::default()));
        tokio::spawn(run_session(connection, Arc::clone(&state), mpsc::unbounded_channel().0, None));
        poll_until(|| {
            let guard = state.lock().unwrap();
            (guard.available && guard.devices.len() == 1 && guard.devices[0].mounted).then_some(())
        })
        .await;
        (state, server.object_server().interface::<_, FakeDaemon>(DAEMON_PATH).await.unwrap())
    }

    #[tokio::test]
    async fn battery_and_signal_follow_events_within_their_ranges() {
        let (state, iface) = session().await;
        FakeDaemon::battery_received(iface.signal_emitter(), "phone-123".into(), 150, true).await.unwrap();
        FakeDaemon::connectivity_received(iface.signal_emitter(), "phone-123".into(), 9).await.unwrap();
        poll_until(|| {
            let device = state.lock().unwrap().devices[0].clone();
            (device.battery == Some(100) && device.charging == Some(true) && device.signal_strength == Some(4))
                .then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn browse_errors_and_transfer_progress_follow_their_signals() {
        let (state, iface) = session().await;
        let emitter = iface.signal_emitter();
        let device = || state.lock().unwrap().devices[0].clone();
        let progress = || state.lock().unwrap().transfer_progress;
        assert_eq!(device().disabled_plugins, ["sms"]);

        FakeDaemon::browse_failed(emitter, "phone-123".into(), "sshfs missing".into()).await.unwrap();
        poll_until(|| (device().browse_error.as_deref() == Some("sshfs missing")).then_some(())).await;
        // The battery marks the reconnect handled.
        FakeDaemon::device_connected(emitter, "phone-123".into(), phone(true)).await.unwrap();
        FakeDaemon::battery_received(emitter, "phone-123".into(), 50, false).await.unwrap();
        poll_until(|| (device().battery == Some(50)).then_some(())).await;
        assert!(device().browse_error.is_some(), "a reconnect keeps the error");
        FakeDaemon::mount_state_changed(emitter, "phone-123".into(), true).await.unwrap();
        poll_until(|| device().browse_error.is_none().then_some(())).await;

        FakeDaemon::update_transfer_progress(emitter, 40).await.unwrap();
        poll_until(|| (progress() == Some(40)).then_some(())).await;
        FakeDaemon::update_transfer_progress(emitter, 100).await.unwrap();
        poll_until(|| progress().is_none().then_some(())).await;
        // A failed transfer never reports 100.
        FakeDaemon::update_transfer_progress(emitter, 40).await.unwrap();
        poll_until(|| (progress() == Some(40)).then_some(())).await;
        FakeDaemon::device_disconnected(emitter, "phone-123".into()).await.unwrap();
        poll_until(|| (progress().is_none() && !device().reachable).then_some(())).await;
        assert_eq!(device().battery, None, "a disconnect forgets the battery");
    }

    #[tokio::test]
    async fn plugin_switches_and_browse_update_the_device_before_the_daemon_reports() {
        let bus = private_bus().await;
        let server = daemon_on(&bus, ready_calls()).await;
        let controller = controller_on(bus.connection().await);
        poll_until(|| controller.snapshot().available.then_some(())).await;
        let iface = server.object_server().interface::<_, FakeDaemon>(DAEMON_PATH).await.unwrap();
        let emitter = iface.signal_emitter();
        let device = || controller.snapshot().devices[0].clone();
        let set = |plugin: &str, enabled: bool| {
            let arguments = vec!["phone-123".into(), plugin.into(), enabled.into()];
            super::dispatch(&controller, &envelope("set_plugin_enabled", arguments));
        };

        set("sms", true);
        poll_until(|| device().disabled_plugins.is_empty().then_some(())).await;
        set("ping", false);
        poll_until(|| (device().disabled_plugins == ["ping"]).then_some(())).await;
        assert!(device().reconnecting);
        // The daemon drops the connection to apply a switch; its list wins on the reconnect.
        FakeDaemon::device_disconnected(emitter, "phone-123".into()).await.unwrap();
        poll_until(|| (!device().reachable).then_some(())).await;
        assert!(device().reconnecting, "the disconnect is part of the switch");
        FakeDaemon::device_connected(emitter, "phone-123".into(), phone(true)).await.unwrap();
        poll_until(|| (device().disabled_plugins == ["sms"] && device().reachable).then_some(())).await;
        assert!(!device().reconnecting);

        // A phone that never comes back stops showing as reconnecting.
        set("ping", true);
        poll_until(|| device().reconnecting.then_some(())).await;
        tokio::time::pause();
        tokio::time::advance(PAIR_TIMEOUT).await;
        tokio::time::resume();
        poll_until(|| (!device().reconnecting).then_some(())).await;

        FakeDaemon::browse_failed(emitter, "phone-123".into(), "sshfs missing".into()).await.unwrap();
        poll_until(|| device().browse_error.is_some().then_some(())).await;
        super::dispatch(&controller, &envelope("browse", vec!["phone-123".into()]));
        poll_until(|| device().browse_error.is_none().then_some(())).await;
    }

    #[tokio::test]
    async fn commands_are_saved_listed_and_pushed_to_paired_devices() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("runcommand.json");
        let bus = private_bus().await;
        let calls = ready_calls();
        let _server = daemon_on(&bus, calls.clone()).await;
        let controller = AndroidConnectController::new(
            Some(bus.connection().await),
            Some(dir.path().into()),
            mpsc::unbounded_channel().0,
        );
        poll_until(|| controller.snapshot().available.then_some(())).await;
        let on_disk = || serde_json::from_slice::<Vec<AndroidCommand>>(&std::fs::read(&file).unwrap()).unwrap();
        let add = |name: &str| super::dispatch(&controller, &envelope("add_command", vec![name.into(), "true".into()]));

        add("one");
        poll_until(|| (calls.log.lock().unwrap().iter().any(|c| c == "push_commands:phone-123")).then_some(())).await;
        add("two");
        poll_until(|| (controller.snapshot().commands.len() == 2).then_some(())).await;
        let [one, two] = controller.snapshot().commands.try_into().unwrap();
        assert_ne!(one.id, two.id);
        assert_eq!(on_disk(), [one.clone(), two.clone()]);

        assert!(!controller.remove_command("nope").unwrap());
        super::dispatch(&controller, &envelope("remove_command", vec![one.id.clone().into()]));
        poll_until(|| (controller.snapshot().commands == [two.clone()]).then_some(())).await;
        assert_eq!(on_disk(), [two]);

        // A file that does not parse is the user's: it is neither listed nor overwritten.
        std::fs::write(&file, "{ not json").unwrap();
        assert!(controller.add_command("three", "true").is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"{ not json");
    }

    #[tokio::test]
    async fn clipboard_auto_share_is_read_on_connect_and_written_by_its_action() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("phone-123/kdeconnect_clipboard/config");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[General]\nsendPassword=true\nautoShare=false\n").unwrap();
        let bus = private_bus().await;
        let server = daemon_on(&bus, ready_calls()).await;
        let controller = AndroidConnectController::new(
            Some(bus.connection().await),
            Some(dir.path().into()),
            mpsc::unbounded_channel().0,
        );
        poll_until(|| controller.snapshot().available.then_some(())).await;
        let auto_share = || controller.snapshot().devices[0].clipboard_auto_share;
        assert!(!auto_share());

        let arguments = vec!["phone-123".into(), true.into()];
        super::dispatch(&controller, &envelope("set_clipboard_auto_share", arguments));
        poll_until(|| auto_share().then_some(())).await;
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "[General]\nautoShare=true\nsendPassword=true\n");

        // Another program switches it off; the next connect picks that up.
        std::fs::write(&config, "autoShare=false\n").unwrap();
        let iface = server.object_server().interface::<_, FakeDaemon>(DAEMON_PATH).await.unwrap();
        FakeDaemon::device_connected(iface.signal_emitter(), "phone-123".into(), phone(true)).await.unwrap();
        poll_until(|| (!auto_share()).then_some(())).await;

        assert!(controller.set_clipboard_auto_share("stranger", true).is_err());
        assert!(!dir.path().join("stranger").exists(), "an unknown id writes nothing");
    }

    #[tokio::test]
    async fn the_state_follows_the_daemon_name() {
        let bus = private_bus().await;
        let controller = controller_on(bus.connection().await);
        let server = daemon_on(&bus, ready_calls()).await;

        poll_until(|| {
            let state = controller.snapshot();
            (state.available && state.devices.len() == 1 && state.devices[0].mounted).then_some(())
        })
        .await;

        server.release_name(SERVICE_NAME).await.unwrap();
        poll_until(|| {
            let state = controller.snapshot();
            (!state.available && state.devices.is_empty()).then_some(())
        })
        .await;

        let _second = daemon_on(&bus, ready_calls()).await;
        poll_until(|| {
            let state = controller.snapshot();
            (state.available && state.devices.len() == 1).then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn devices_appear_when_the_daemon_serves_after_taking_its_name() {
        let bus = private_bus().await;
        let calls = Calls::default();
        let _server = daemon_on(&bus, calls.clone()).await;
        let controller = controller_on(bus.connection().await);

        poll_until(|| (calls.list_attempts.load(Ordering::SeqCst) > 0).then_some(())).await;
        assert!(!controller.snapshot().available, "no list_devices answer yet");
        calls.ready.store(true, Ordering::SeqCst);
        poll_until(|| {
            let state = controller.snapshot();
            (state.available && state.devices.len() == 1).then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn pairing_requested_survives_the_connect_that_follows_it_and_clears_on_reject() {
        let bus = private_bus().await;
        let server = daemon_on(&bus, ready_calls()).await;
        let controller = controller_on(bus.connection().await);
        poll_until(|| controller.snapshot().available.then_some(())).await;
        let iface = server.object_server().interface::<_, FakeDaemon>(DAEMON_PATH).await.unwrap();
        let emitter = iface.signal_emitter();
        let device = || controller.snapshot().devices[0].clone();

        // The daemon sends pairing_requested, then device_connected, for one request.
        FakeDaemon::pairing_requested(emitter, "phone-123".into(), "Test Phone".into()).await.unwrap();
        FakeDaemon::device_connected(emitter, "phone-123".into(), phone(false)).await.unwrap();
        FakeDaemon::battery_received(emitter, "phone-123".into(), 50, false).await.unwrap();
        poll_until(|| (device().battery == Some(50)).then_some(())).await;
        assert!(device().pairing_requested, "the connect keeps the flag");

        super::dispatch(&controller, &envelope("reject_pairing", vec!["phone-123".into()]));
        poll_until(|| (!device().pairing_requested).then_some(())).await;

        // An unpaired device is forgotten on disconnect, as after a daemon restart.
        FakeDaemon::device_disconnected(emitter, "phone-123".into()).await.unwrap();
        poll_until(|| controller.snapshot().devices.is_empty().then_some(())).await;
    }

    #[tokio::test]
    async fn pairing_flags_survive_the_echo_and_lapse_without_an_answer() {
        let bus = private_bus().await;
        let server = daemon_on(&bus, ready_calls()).await;
        let controller = controller_on(bus.connection().await);
        poll_until(|| controller.snapshot().available.then_some(())).await;
        let iface = server.object_server().interface::<_, FakeDaemon>(DAEMON_PATH).await.unwrap();
        let emitter = iface.signal_emitter();
        let device = || controller.snapshot().devices[0].clone();
        let pair = || super::dispatch(&controller, &envelope("pair", vec!["phone-123".into()]));

        pair();
        poll_until(|| device().pairing_sent.then_some(())).await;
        // The daemon echoes the request as an unpaired device_connected; the battery marks it handled.
        FakeDaemon::device_connected(emitter, "phone-123".into(), phone(false)).await.unwrap();
        FakeDaemon::battery_received(emitter, "phone-123".into(), 50, false).await.unwrap();
        poll_until(|| (device().battery == Some(50)).then_some(())).await;
        assert!(device().pairing_sent, "the echo keeps the flag");
        FakeDaemon::pairing_requested(emitter, "phone-123".into(), "Test Phone".into()).await.unwrap();
        poll_until(|| device().pairing_requested.then_some(())).await;

        // No answer arrives in either direction: both flags lapse with the peer's timeout.
        tokio::time::pause();
        tokio::time::advance(PAIR_TIMEOUT).await;
        tokio::time::resume();
        poll_until(|| (!device().pairing_sent && !device().pairing_requested).then_some(())).await;

        pair();
        poll_until(|| device().pairing_sent.then_some(())).await;
        FakeDaemon::device_paired(emitter, "phone-123".into(), phone(true)).await.unwrap();
        poll_until(|| (!device().pairing_sent && device().paired).then_some(())).await;

        pair();
        poll_until(|| device().pairing_sent.then_some(())).await;
        super::dispatch(&controller, &envelope("unpair", vec!["phone-123".into()]));
        poll_until(|| (!device().pairing_sent).then_some(())).await;
    }

    fn envelope(action: &str, arguments: Vec<serde_json::Value>) -> shared::CommandEnvelope {
        shared::CommandEnvelope {
            params: shared::CommandParams {
                generation_id: 0,
                capability: "android_connect".into(),
                action: action.into(),
                arguments,
                legacy_expected_revision: 0,
            },
            id: 1,
        }
    }

    #[test]
    fn sendable_takes_only_absolute_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let file = file.to_str().unwrap();
        assert!(sendable(&[file.into()]));
        for bad in [dir.path().to_str().unwrap(), "a.txt", "/no/such/file"] {
            assert!(!sendable(&[file.into(), bad.into()]), "{bad} rejects the batch");
        }
    }

    #[tokio::test]
    async fn pass_through_actions_reach_the_daemon() {
        let bus = private_bus().await;
        let calls = ready_calls();
        let _server = daemon_on(&bus, calls.clone()).await;
        let controller = controller_on(bus.connection().await);
        poll_until(|| controller.snapshot().available.then_some(())).await;
        // The session's own broadcast may still be in flight.
        poll_until(|| (!calls.log.lock().unwrap().is_empty()).then_some(())).await;
        calls.log.lock().unwrap().clear();

        let table: [(&str, Vec<serde_json::Value>, &str); 8] = [
            ("broadcast", vec![], "broadcast"),
            ("ring", vec!["phone-123".into()], "ring:phone-123"),
            ("ping", vec!["phone-123".into(), "hi".into()], "ping:phone-123:hi"),
            ("send_clipboard", vec!["phone-123".into(), "text".into()], "send_clipboard:phone-123:text"),
            ("share_clipboard", vec!["phone-123".into()], "share_clipboard:phone-123"),
            ("unmount", vec!["phone-123".into()], "unmount:phone-123"),
            ("accept_pairing", vec!["phone-123".into()], "accept_pairing:phone-123"),
            ("reject_pairing", vec!["phone-123".into()], "reject_pairing:phone-123"),
        ];
        for (action, arguments, _) in &table {
            super::dispatch(&controller, &envelope(action, arguments.clone()));
        }
        let mut expected: Vec<String> = table.iter().map(|(.., record)| (*record).to_owned()).collect();
        expected.sort();
        poll_until(|| {
            let mut seen = calls.log.lock().unwrap().clone();
            seen.sort();
            (seen == expected).then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn send_files_reaches_the_daemon() {
        let bus = private_bus().await;
        let calls = ready_calls();
        let _server = daemon_on(&bus, calls.clone()).await;
        let controller = controller_on(bus.connection().await);
        poll_until(|| controller.snapshot().available.then_some(())).await;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let file = file.to_str().unwrap();
        super::dispatch(&controller, &envelope("send_files", vec!["phone-123".into(), vec![file].into()]));
        poll_until(|| (!calls.files.lock().unwrap().is_empty()).then_some(())).await;
        assert_eq!(*calls.files.lock().unwrap(), [vec![file.to_owned()]]);
    }
}

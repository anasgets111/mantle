//! Virtual devices a config declares (ADR-0383). Null sinks are daemon objects the mixer creates on
//! its own connection; every other kind is a module, and a module runs in the process that loads
//! it, so they share one `pipewire -c` child this task writes the conf for and keeps running.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use shared::action::{VirtualDevice, VirtualKind};
use shared::{debug, notice, warn};
use tokio::process::Child;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::mixer::{AudioCommand, AudioCommandSender};
use crate::capabilities::{RETRY_FIRST, STABLE, next_delay};
use crate::process::{DEFAULT_REAP_GRACE, reap_process_group, spawn_group_leader_stdio_piped};

pub enum Request {
    /// `generation`'s `set_virtual_devices`, replacing what any generation declared.
    Declare { generation: u32, devices: Vec<VirtualDevice> },
    /// A departed generation; its declaration goes unless a newer one replaced it.
    Forget(u32),
    /// The mixer has a snapshot from PipeWire. The child waits for the first, as one started with
    /// no daemon runs with none of its modules; a later one restarts it, since a lost daemon took
    /// the modules and left the child running.
    Connected,
}

/// The conf file the child runs, one per Supervisor; its owner removes it on exit.
pub fn conf_path() -> Option<PathBuf> {
    shared::runtime_root().ok().map(|root| root.join(format!("pipewire-{}.conf", std::process::id())))
}

/// Starts the task; it ends when the returned sender drops.
pub fn spawn(commands: AudioCommandSender) -> UnboundedSender<Request> {
    let (requests, receiver) = unbounded_channel();
    let conf = conf_path();
    tokio::spawn(run(receiver, commands, move |modules| start(conf.as_deref(), modules, &[])));
    requests
}

/// The devices `request` leaves declared, or `None` when it changes nothing.
fn declared(owner: &mut Option<u32>, request: Request) -> Option<Vec<VirtualDevice>> {
    match request {
        Request::Declare { generation, devices } => {
            *owner = Some(generation);
            Some(devices)
        }
        Request::Forget(departed) if *owner == Some(departed) => {
            *owner = None;
            Some(Vec::new())
        }
        Request::Forget(_) | Request::Connected => None,
    }
}

/// `start` runs the child for the declared modules; a test counts its calls.
pub(super) async fn run(
    mut requests: UnboundedReceiver<Request>,
    commands: AudioCommandSender,
    mut start: impl FnMut(&[VirtualDevice]) -> Option<Child>,
) {
    let (mut owner, mut modules, mut child, mut connected) = (None, Vec::new(), None::<Child>, false);
    let (mut retry, mut delay, mut started) = (None, RETRY_FIRST, Instant::now());
    loop {
        // ponytail: devices wait on the mixer's first snapshot, so a mixer thread that dies before
        // it leaves them unstarted, silently; upgrade: log a declare held past a deadline.
        if connected && child.is_none() && retry.is_none() && !modules.is_empty() {
            child = start(&modules);
            started = Instant::now();
            if child.is_none() {
                retry = Some(tokio::time::Instant::now() + delay);
                delay = next_delay(delay);
            }
        }
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { break };
                if matches!(request, Request::Connected) {
                    if let Some(mut old) = child.take() {
                        let _ = reap_process_group(&mut old, DEFAULT_REAP_GRACE).await;
                    }
                    (connected, retry, delay) = (true, None, RETRY_FIRST);
                    continue;
                }
                let Some(devices) = declared(&mut owner, request) else { continue };
                let (null_sinks, next): (Vec<_>, Vec<_>) =
                    devices.into_iter().partition(|device| device.kind == VirtualKind::NullSink);
                if commands.send(AudioCommand::SetNullSinks(null_sinks)).is_err() {
                    warn!("the PipeWire command channel is closed; null sinks were not changed");
                }
                if next != modules {
                    if let Some(mut old) = child.take() {
                        let _ = reap_process_group(&mut old, DEFAULT_REAP_GRACE).await;
                    }
                    (modules, retry, delay) = (next, None, RETRY_FIRST);
                }
            }
            status = async { child.as_mut()?.wait().await.ok() }, if child.is_some() => {
                child = None;
                warn!("the virtual devices' pipewire exited ({status:?}); restarting it");
                if started.elapsed() >= STABLE {
                    delay = RETRY_FIRST;
                }
                retry = Some(tokio::time::Instant::now() + delay);
                delay = next_delay(delay);
            }
            () = tokio::time::sleep_until(retry.unwrap_or_else(tokio::time::Instant::now)), if retry.is_some() => {
                retry = None;
            }
        }
    }
    if let Some(mut child) = child {
        let _ = reap_process_group(&mut child, DEFAULT_REAP_GRACE).await;
    }
}

/// Writes the conf and runs `pipewire -c` on it, killed with the Supervisor (`PR_SET_PDEATHSIG`).
pub(super) fn start(conf: Option<&Path>, modules: &[VirtualDevice], envs: &[(String, String)]) -> Option<Child> {
    let Some(conf) = conf else {
        warn!("XDG_RUNTIME_DIR is not set; the virtual devices have nowhere for their conf");
        return None;
    };
    if let Err(err) = shared::write_atomic(conf, conf_text(modules).as_bytes()) {
        warn!("cannot write {}: {err}", conf.display());
        return None;
    }
    match spawn_group_leader_stdio_piped("pipewire", &["-c".to_string(), conf.display().to_string()], envs) {
        Ok(mut child) => {
            // Nothing is read from or written to it; its log goes to the inherited stderr.
            drop((child.stdin.take(), child.stdout.take()));
            notice!("started pipewire for {} virtual devices", modules.len());
            Some(child)
        }
        Err(err) => {
            warn!("cannot start pipewire for the virtual devices: {err}");
            None
        }
    }
}

/// A client conf like PipeWire's own `filter-chain.conf`, one module per device. SPA-JSON reads JSON.
pub(super) fn conf_text(modules: &[VirtualDevice]) -> String {
    let base = [
        json!({ "name": "libpipewire-module-rt", "args": { "nice.level": -11 }, "flags": ["ifexists", "nofail"] }),
        json!({ "name": "libpipewire-module-protocol-native" }),
        json!({ "name": "libpipewire-module-client-node" }),
        json!({ "name": "libpipewire-module-adapter" }),
    ];
    let modules: Vec<Value> = base.into_iter().chain(modules.iter().filter_map(module)).collect();
    let libs = json!({ "audio.convert.*": "audioconvert/libspa-audioconvert", "support.*": "support/libspa-support" });
    format!("context.spa-libs = {libs}\ncontext.modules = {}\n", Value::Array(modules))
}

/// One `context.modules` entry; `None` for a null sink, which is no module. `nofail`, so one bad
/// device leaves the others running.
fn module(device: &VirtualDevice) -> Option<Value> {
    let (name, description) = (&device.name, device.description.as_deref().unwrap_or(&device.name));
    let node = json!({ "node.name": name, "node.description": description });
    // A loopback or filter chain is a sink by default, PipeWire's virtual-sink shape: its capture
    // stream is the node apps play into. Echo-cancel names its source and sink in their own props.
    let sink = json!({ "media.class": "Audio/Sink", "node.name": name, "node.description": description });
    let (module, defaults) = match device.kind {
        VirtualKind::NullSink => return None,
        VirtualKind::Combine => ("libpipewire-module-combine-stream", node),
        VirtualKind::Loopback => ("libpipewire-module-loopback", json!({ "capture.props": sink })),
        VirtualKind::FilterChain => ("libpipewire-module-filter-chain", json!({ "capture.props": sink })),
        VirtualKind::EchoCancel => (
            "libpipewire-module-echo-cancel",
            json!({
                "source.props": node,
                "sink.props": { "node.name": format!("{name}.sink"), "node.description": format!("{description} (playback)") },
            }),
        ),
    };
    let Value::Object(mut args) = defaults else { return None };
    args.extend(device.args.clone());
    debug!("virtual device {name} loads {module}");
    Some(json!({ "name": module, "args": args, "flags": ["nofail"] }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::within;
    use crate::process::sh_args;

    /// The module lists `run` starts a child for, given `requests` in order.
    async fn starts(requests: Vec<Request>) -> Vec<Vec<VirtualDevice>> {
        let (sender, receiver) = unbounded_channel();
        requests.into_iter().for_each(|request| sender.send(request).unwrap());
        drop(sender);
        let (commands, _receiver) = super::super::mixer::command_channel();
        let mut started = Vec::new();
        within(run(receiver, commands, |modules| {
            started.push(modules.to_vec());
            spawn_group_leader_stdio_piped("sh", &sh_args("sleep 30"), &[]).ok()
        }))
        .await;
        started
    }

    /// The child waits for the mixer's first connection, whichever arrives first, and a later
    /// connection, as after a lost daemon, restarts it once.
    #[tokio::test]
    async fn the_child_starts_once_the_mixer_connects_and_restarts_on_a_later_connection() {
        let loopback = vec![device(VirtualKind::Loopback, "loop", json!({}))];
        let declare = || Request::Declare { generation: 1, devices: loopback.clone() };
        assert_eq!(starts(vec![declare(), Request::Connected]).await, [&loopback[..]], "declared first");
        assert_eq!(starts(vec![Request::Connected, declare()]).await, [&loopback[..]], "connected first");
        let reconnected = starts(vec![Request::Connected, declare(), Request::Connected]).await;
        assert_eq!(reconnected, [loopback.clone(), loopback.clone()]);
    }

    fn device(kind: VirtualKind, name: &str, args: Value) -> VirtualDevice {
        let Value::Object(args) = args else { panic!("args must be an object") };
        VirtualDevice { kind, name: name.to_string(), description: Some("Mic".to_string()), args }
    }

    #[test]
    fn the_conf_loads_one_module_per_device_named_where_the_module_reads_it_and_the_configs_args_last() {
        let text = conf_text(&[
            device(VirtualKind::EchoCancel, "mic_aec", json!({ "aec.args": { "webrtc.gain_control": true } })),
            device(VirtualKind::Loopback, "loop", json!({})),
            device(VirtualKind::FilterChain, "eq", json!({ "capture.props": { "node.name": "eq_override" } })),
            device(VirtualKind::NullSink, "silent", json!({})),
        ]);
        let modules = text.lines().find_map(|line| line.strip_prefix("context.modules = ")).unwrap();
        let modules: Vec<Value> = serde_json::from_str(modules).unwrap();
        let names: Vec<&str> = modules.iter().map(|module| module["name"].as_str().unwrap()).collect();
        assert_eq!(
            names[4..],
            ["libpipewire-module-echo-cancel", "libpipewire-module-loopback", "libpipewire-module-filter-chain"],
            "after the client modules, and no module for a null sink"
        );
        assert_eq!(
            modules[4]["args"],
            json!({
                "source.props": { "node.name": "mic_aec", "node.description": "Mic" },
                "sink.props": { "node.name": "mic_aec.sink", "node.description": "Mic (playback)" },
                "aec.args": { "webrtc.gain_control": true },
            })
        );
        assert_eq!(
            modules[5]["args"]["capture.props"],
            json!({ "media.class": "Audio/Sink", "node.name": "loop", "node.description": "Mic" }),
            "a loopback is a sink"
        );
        assert_eq!(modules[6]["args"]["capture.props"]["node.name"], "eq_override", "the config's args win");
        assert!(modules[4..].iter().all(|module| module["flags"] == json!(["nofail"])), "one bad device fails alone");
        assert!(!text.contains("log.level"), "PipeWire's own warnings reach stderr");
    }

    #[test]
    fn a_departed_generation_clears_only_a_declaration_it_still_owns() {
        let mut owner = None;
        let devices = vec![device(VirtualKind::Loopback, "loop", json!({}))];
        let declare = |generation| Request::Declare { generation, devices: devices.clone() };

        assert_eq!(declared(&mut owner, declare(1)), Some(devices.clone()));
        assert_eq!(declared(&mut owner, declare(2)), Some(devices.clone()), "a reload re-declares");
        assert_eq!(declared(&mut owner, Request::Forget(1)), None, "generation 2 owns it now");
        assert_eq!(declared(&mut owner, Request::Forget(2)), Some(Vec::new()), "a config declaring none removes them");
        assert_eq!(declared(&mut owner, Request::Forget(2)), None);
    }
}

//! [`AndroidConnectController`] manages `mantle.android_connect` state and actions.

pub use shared::state::android_connect::{AndroidCommand, AndroidConnectState, AndroidDevice, DeviceType};

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;

use super::proxies::{DaemonProxy, DbusDevice, SERVICE_NAME};
use crate::capabilities::{RETRY_FIRST, next_delay};

impl From<DbusDevice> for AndroidDevice {
    fn from(d: DbusDevice) -> Self {
        Self {
            id: d.id,
            name: d.name,
            device_type: DeviceType::parse(&d.device_type),
            paired: d.is_paired,
            reachable: d.is_reachable,
            ..Default::default()
        }
    }
}

/// KDE Connect's own pairing timeout: the peer drops an unanswered request after it.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AndroidConnectController {
    state: Arc<Mutex<AndroidConnectState>>,
    events: UnboundedSender<()>,
    bus: Option<zbus::Connection>,
    config_dir: Option<PathBuf>,
}

impl AndroidConnectController {
    /// Without a session bus the state stays at its unavailable default. `config_dir` is the
    /// daemon's, from [`config_dir`].
    pub fn new(
        session_bus: Option<zbus::Connection>,
        config_dir: Option<PathBuf>,
        events: UnboundedSender<()>,
    ) -> Self {
        let state = Arc::new(Mutex::new(AndroidConnectState::default()));
        if let Some(bus) = &session_bus {
            tokio::spawn(run_watcher(bus.clone(), Arc::clone(&state), events.clone(), config_dir.clone()));
        }
        Self { state, events, bus: session_bus, config_dir }
    }

    pub fn snapshot(&self) -> AndroidConnectState {
        self.state.lock().expect("mutex poisoned").clone()
    }

    /// The daemon's D-Bus surface reports neither an outgoing request nor its rejection, so the
    /// controller tracks it.
    pub fn set_pairing_sent(&self, id: &str, sent: bool) {
        self.edit(id, |d| d.pairing_sent = sent);
        if sent {
            expire(Arc::clone(&self.state), self.events.clone(), id.to_owned(), |d| d.pairing_sent = false);
        }
    }

    /// Records a plugin switch before the daemon reports it. The daemon then drops the connection
    /// without a signal, so a reachable device is marked `reconnecting` until it connects again.
    pub fn set_plugin_disabled(&self, id: &str, plugin: &str, disabled: bool) {
        self.edit(id, |d| {
            d.disabled_plugins.retain(|p| p != plugin);
            if disabled {
                d.disabled_plugins.push(plugin.to_owned());
            }
            d.reconnecting = d.reachable;
        });
        expire(Arc::clone(&self.state), self.events.clone(), id.to_owned(), |d| d.reconnecting = false);
    }

    /// Applies `f` to the device, if it is known, and tells the Renderer.
    pub fn edit(&self, id: &str, f: impl FnOnce(&mut AndroidDevice)) {
        edit(&self.state, &self.events, id, f);
    }

    /// Rewrites the `autoShare` line of the device's clipboard config, keeping the rest.
    pub fn set_clipboard_auto_share(&self, id: &str, enabled: bool) -> io::Result<()> {
        if !self.snapshot().devices.iter().any(|d| d.id == id) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "unknown device"));
        }
        let path = self.config_dir.as_deref().and_then(|dir| clipboard_config(dir, id));
        let path =
            path.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no config dir or a bad device id"))?;
        let text = match std::fs::read_to_string(&path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            read => read?,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        shared::write_atomic(&path, with_auto_share(&text, enabled).as_bytes())?;
        self.edit(id, |d| d.clipboard_auto_share = enabled);
        Ok(())
    }

    /// Offers a command to paired devices. `Ok(true)`: the file changed.
    pub fn add_command(&self, name: &str, command: &str) -> io::Result<bool> {
        self.edit_commands(|list| {
            let mut n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            while list.iter().any(|c| c.id == format!("{n:x}")) {
                n += 1;
            }
            list.push(AndroidCommand { id: format!("{n:x}"), name: name.into(), command: command.into() });
            true
        })
    }

    /// Stops offering a command. `Ok(false)`: the id is unknown and nothing was written.
    pub fn remove_command(&self, id: &str) -> io::Result<bool> {
        self.edit_commands(|list| {
            let before = list.len();
            list.retain(|c| c.id != id);
            list.len() != before
        })
    }

    /// Rereads `runcommand.json` under the state lock, so concurrent edits cannot lose one, applies
    /// `f` and, when it returns true, saves the file. A file that does not parse is never overwritten.
    fn edit_commands(&self, f: impl FnOnce(&mut Vec<AndroidCommand>) -> bool) -> io::Result<bool> {
        let path = self
            .config_dir
            .as_deref()
            .map(|dir| dir.join(COMMANDS_FILE))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no config dir"))?;
        let mut state = self.state.lock().expect("mutex poisoned");
        let mut list = read_commands(&path)?;
        if !f(&mut list) {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        shared::write_atomic(&path, &serde_json::to_vec_pretty(&list).map_err(io::Error::other)?)?;
        state.commands = list;
        let _ = self.events.send(());
        Ok(true)
    }

    /// The daemon proxy, or `None` while the daemon is absent.
    pub async fn proxy(&self) -> Option<DaemonProxy<'static>> {
        if !self.snapshot().available {
            return None;
        }
        daemon_proxy(self.bus.as_ref()?).await.ok()
    }
}

async fn daemon_proxy(bus: &zbus::Connection) -> zbus::Result<DaemonProxy<'static>> {
    DaemonProxy::builder(bus).cache_properties(zbus::proxy::CacheProperties::No).build().await
}

/// Whether every path is an absolute regular file: the daemon `expect`s to open each one, so a
/// directory would crash its event task. One bad path rejects the batch.
pub fn sendable(files: &[String]) -> bool {
    let ok = files.iter().all(|f| {
        let p = Path::new(f);
        p.is_absolute() && std::fs::metadata(p).is_ok_and(|m| m.is_file())
    });
    if !ok {
        warn!("send_files: every path must be an absolute path to an existing regular file");
    }
    ok
}

/// The daemon's config directory, `$XDG_CONFIG_HOME/kdeconnect`.
pub fn config_dir() -> Option<PathBuf> {
    shared::xdg_dir("XDG_CONFIG_HOME", ".config").map(|dir| dir.join("kdeconnect"))
}

const COMMANDS_FILE: &str = "runcommand.json";

/// The offered commands: none when the file is missing, an error when it does not parse.
// ponytail: the list is reread when a session starts and after our own writes, so a hand edit or
// the daemon merging commands from another desktop shows after a daemon restart; a file watch
// would fix it.
fn read_commands(path: &Path) -> io::Result<Vec<AndroidCommand>> {
    match std::fs::read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        read => serde_json::from_slice(&read?).map_err(io::Error::other),
    }
}

/// The device's clipboard config, a KDE INI file the daemon rereads on every clipboard change.
fn clipboard_config(dir: &Path, id: &str) -> Option<PathBuf> {
    // Ids come from the network: anything but a plain name could leave `dir`.
    let plain = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    plain.then(|| dir.join(id).join("kdeconnect_clipboard").join("config"))
}

fn auto_share_key(line: &str) -> bool {
    line.split_once('=').is_some_and(|(key, _)| key.trim() == "autoShare")
}

/// `autoShare` as the daemon reads it: the last one wins, and anything but `false` is on.
fn auto_share(text: &str) -> bool {
    let value = text.lines().rfind(|l| auto_share_key(l)).and_then(|l| l.split_once('='));
    value.is_none_or(|(_, v)| v.trim() != "false")
}

fn read_auto_share(dir: Option<&Path>, id: &str) -> bool {
    let path = dir.and_then(|dir| clipboard_config(dir, id));
    path.and_then(|p| std::fs::read_to_string(p).ok()).is_none_or(|text| auto_share(&text))
}

/// `text` with one `autoShare` line set to `enabled`, under `[General]`, every other line kept.
fn with_auto_share(text: &str, enabled: bool) -> String {
    let setting = format!("autoShare={enabled}");
    let mut lines: Vec<&str> = text.lines().filter(|l| !auto_share_key(l)).collect();
    match lines.iter().position(|l| l.trim() == "[General]") {
        Some(i) => lines.insert(i + 1, &setting),
        None => {
            lines.insert(0, &setting);
            lines.insert(0, "[General]");
        }
    }
    lines.join("\n") + "\n"
}

/// Replaces the stored copy of a device with `new`, keeping what only live signals report.
/// `plugins_known` is false when the daemon failed to list the plugins: the old list stays.
fn upsert(devices: &mut Vec<AndroidDevice>, new: AndroidDevice, plugins_known: bool) {
    match devices.iter_mut().find(|d| d.id == new.id) {
        Some(old) => {
            *old = AndroidDevice {
                mounted: old.mounted,
                browse_error: old.browse_error.take(),
                battery: old.battery,
                charging: old.charging,
                signal_strength: old.signal_strength,
                // The daemon echoes our own request as an unpaired device_connected.
                pairing_sent: old.pairing_sent && !new.paired,
                // pairing_requested precedes the device_connected the daemon sends for the same request.
                pairing_requested: old.pairing_requested && !new.paired,
                disabled_plugins: if plugins_known {
                    new.disabled_plugins
                } else {
                    std::mem::take(&mut old.disabled_plugins)
                },
                ..new
            }
        }
        None => devices.push(new),
    }
}

/// Locks the state, applies `f` to its devices, then tells the Renderer.
fn change(state: &Mutex<AndroidConnectState>, events: &UnboundedSender<()>, f: impl FnOnce(&mut Vec<AndroidDevice>)) {
    f(&mut state.lock().expect("mutex poisoned").devices);
    let _ = events.send(());
}

/// [`change`] for one device, if it is known.
fn edit(
    state: &Mutex<AndroidConnectState>,
    events: &UnboundedSender<()>,
    id: &str,
    f: impl FnOnce(&mut AndroidDevice),
) {
    change(state, events, |v| {
        if let Some(d) = v.iter_mut().find(|d| d.id == id) {
            f(d);
        }
    });
}

/// Runs `f` on the device after [`PAIR_TIMEOUT`]: the daemon emits nothing when the peer rejects,
/// cancels or lets a request lapse.
// ponytail: a repeated request inside the timeout clears with the first one's timer, and a timer
// from before a daemon restart can clear a fresh flag for the same id; a per-request token would
// fix both.
fn expire(state: Arc<Mutex<AndroidConnectState>>, events: UnboundedSender<()>, id: String, f: fn(&mut AndroidDevice)) {
    tokio::spawn(async move {
        tokio::time::sleep(PAIR_TIMEOUT).await;
        edit(&state, &events, &id, f);
    });
}

/// `device` with the settings the daemon keeps apart from its signals, and whether the plugin list
/// was read.
async fn with_settings(proxy: &DaemonProxy<'_>, dir: Option<&Path>, device: AndroidDevice) -> (AndroidDevice, bool) {
    let plugins = proxy.get_disabled_plugins(&device.id).await.ok();
    let known = plugins.is_some();
    let device = AndroidDevice {
        disabled_plugins: plugins.unwrap_or_default(),
        clipboard_auto_share: read_auto_share(dir, &device.id),
        ..device
    };
    (device, known)
}

/// Follows the daemon's name: every owner change drops the running session and, if the name has an
/// owner, starts a fresh one, so an A to B handover restarts it too.
async fn run_watcher(
    bus: zbus::Connection,
    state: Arc<Mutex<AndroidConnectState>>,
    events: UnboundedSender<()>,
    config_dir: Option<PathBuf>,
) {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&bus).await else {
        error!("failed to bind org.freedesktop.DBus; disabled for this run");
        return;
    };
    let mut owners = match dbus.receive_name_owner_changed_with_args(&[(0, SERVICE_NAME)]).await {
        Ok(stream) => stream,
        Err(err) => {
            error!("failed to subscribe to NameOwnerChanged; disabled for this run: {err}");
            return;
        }
    };
    let name = zbus::names::WellKnownName::from_static_str_unchecked(SERVICE_NAME);
    // `None` once the bus connection is gone.
    let mut owned = Some(dbus.name_has_owner(name.into()).await.unwrap_or(false));
    let mut session: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        // The await matters: on a multi-thread runtime an aborted task can be mid-poll, and it must
        // not write state after this point.
        if let Some(old) = session.take() {
            old.abort();
            let _ = old.await;
        }
        // Reset on every change: a new owner serves nothing until its session answers.
        *state.lock().expect("mutex poisoned") = AndroidConnectState::default();
        let _ = events.send(());
        match owned {
            None => return,
            Some(true) => {
                session =
                    Some(tokio::spawn(run_session(bus.clone(), Arc::clone(&state), events.clone(), config_dir.clone())))
            }
            Some(false) => {}
        }
        owned = loop {
            let Some(signal) = owners.next().await else { break None };
            if let Ok(args) = signal.args() {
                break Some(args.new_owner.is_some());
            }
        };
    }
}

/// Retries [`session`] with backoff until it ends cleanly; the daemon claims its name well before
/// it serves its object, so the first attempts can fail.
pub(super) async fn run_session(
    bus: zbus::Connection,
    state: Arc<Mutex<AndroidConnectState>>,
    events: UnboundedSender<()>,
    config_dir: Option<PathBuf>,
) {
    let mut delay = RETRY_FIRST;
    while let Err(err) = session(&bus, &state, &events, config_dir.as_deref()).await {
        debug!("daemon setup failed, retrying in {delay:?}: {err}");
        tokio::time::sleep(delay).await;
        delay = next_delay(delay);
    }
}

async fn session(
    bus: &zbus::Connection,
    state: &Arc<Mutex<AndroidConnectState>>,
    events: &UnboundedSender<()>,
    dir: Option<&Path>,
) -> zbus::Result<()> {
    let proxy = daemon_proxy(bus).await?;
    let mut connected = proxy.receive_device_connected().await?;
    let mut paired = proxy.receive_device_paired().await?;
    let mut disconnected = proxy.receive_device_disconnected().await?;
    let mut pair_requests = proxy.receive_pairing_requested().await?;
    let mut battery = proxy.receive_battery_received().await?;
    let mut connectivity = proxy.receive_connectivity_received().await?;
    let mut mounts = proxy.receive_mount_state_changed().await?;
    let mut browse_failures = proxy.receive_browse_failed().await?;
    let mut progress = proxy.receive_update_transfer_progress().await?;

    let mounted = proxy.mounted_devices().await?;
    let mut devices = Vec::new();
    for d in proxy.list_devices().await? {
        let device = AndroidDevice { mounted: mounted.contains(&d.id), ..d.into() };
        devices.push(with_settings(&proxy, dir, device).await.0);
    }
    let commands = dir.map(|dir| dir.join(COMMANDS_FILE)).map_or(Ok(Vec::new()), |path| read_commands(&path));
    {
        let mut guard = state.lock().expect("mutex poisoned");
        guard.available = true;
        guard.devices = devices;
        guard.commands = commands.unwrap_or_else(|err| {
            warn!("{COMMANDS_FILE} is unreadable, offering none: {err}");
            Vec::new()
        });
    }
    let _ = events.send(());

    let _ = proxy.broadcast_identity().await;

    loop {
        tokio::select! {
            Some(sig) = connected.next() => {
                if let Ok(args) = sig.args() {
                    let (device, known) = with_settings(&proxy, dir, args.device.into()).await;
                    change(state, events, |v| upsert(v, device, known));
                }
            }
            Some(sig) = paired.next() => {
                if let Ok(args) = sig.args() {
                    let device = AndroidDevice { paired: true, ..args.device.into() };
                    let (device, known) = with_settings(&proxy, dir, device).await;
                    change(state, events, |v| upsert(v, device, known));
                }
            }
            Some(sig) = disconnected.next() => {
                if let Ok(args) = sig.args() {
                    // A failed transfer never reports 100; a running one sets it again within 100 ms.
                    state.lock().expect("mutex poisoned").transfer_progress = None;
                    // The daemon lists only paired devices after a restart, so an unpaired one goes.
                    change(state, events, |v| {
                        let Some(i) = v.iter().position(|d| d.id == args.device_id) else { return };
                        if !v[i].paired {
                            v.remove(i);
                            return;
                        }
                        let d = &mut v[i];
                        d.reachable = false;
                        d.mounted = false;
                        d.pairing_requested = false;
                        d.pairing_sent = false;
                        d.battery = None;
                        d.charging = None;
                        d.signal_strength = None;
                    });
                }
            }
            Some(sig) = pair_requests.next() => {
                if let Ok(args) = sig.args() {
                    expire(Arc::clone(state), events.clone(), args.device_id.clone(), |d| d.pairing_requested = false);
                    let clipboard_auto_share = read_auto_share(dir, &args.device_id);
                    change(state, events, |v| match v.iter_mut().find(|d| d.id == args.device_id) {
                        Some(d) => {
                            d.name = args.device_name;
                            d.pairing_requested = true;
                        }
                        None => v.push(AndroidDevice {
                            id: args.device_id,
                            name: args.device_name,
                            pairing_requested: true,
                            reachable: true,
                            clipboard_auto_share,
                            ..Default::default()
                        }),
                    });
                }
            }
            Some(sig) = battery.next() => {
                if let Ok(args) = sig.args() {
                    edit(state, events, &args.device_id, |d| {
                        d.battery = Some(args.level.clamp(0, 100) as u8);
                        d.charging = Some(args.is_charging);
                    });
                }
            }
            Some(sig) = connectivity.next() => {
                if let Ok(args) = sig.args() {
                    let bars = args.signal_strength.clamp(0, 4) as u8;
                    edit(state, events, &args.device_id, |d| d.signal_strength = Some(bars));
                }
            }
            Some(sig) = mounts.next() => {
                if let Ok(args) = sig.args() {
                    edit(state, events, &args.device_id, |d| {
                        d.mounted = args.mounted;
                        if args.mounted {
                            d.browse_error = None;
                        }
                    });
                }
            }
            Some(sig) = browse_failures.next() => {
                if let Ok(args) = sig.args() {
                    edit(state, events, &args.device_id, |d| d.browse_error = Some(args.message));
                }
            }
            Some(sig) = progress.next() => {
                if let Ok(args) = sig.args() {
                    let percent = (args.progress < 100).then_some(args.progress);
                    // The daemon ticks every 100 ms; only a new percent reaches the Renderer.
                    let changed = std::mem::replace(&mut state.lock().expect("mutex poisoned").transfer_progress, percent) != percent;
                    if changed {
                        let _ = events.send(());
                    }
                }
            }
            else => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_share_reads_like_the_daemon_and_rewrites_only_its_line() {
        assert!(auto_share(""), "missing means on");
        assert!(!auto_share("[General]\nautoShare = false\n"));
        assert!(auto_share("autoShare=false\nautoShare=true\n"), "the last one wins");
        assert!(auto_share("#autoShare=false\n"), "a comment is not the key");

        let kept = "[General]\nsendPassword=true\nautoShare=true\n";
        assert_eq!(with_auto_share(kept, false), "[General]\nautoShare=false\nsendPassword=true\n");
        assert_eq!(with_auto_share("", false), "[General]\nautoShare=false\n");
    }

    #[test]
    fn clipboard_config_refuses_ids_that_leave_the_dir() {
        let dir = Path::new("/c");
        assert_eq!(clipboard_config(dir, "a1_b-2"), Some(PathBuf::from("/c/a1_b-2/kdeconnect_clipboard/config")));
        for bad in ["", "..", "../x", "a/b"] {
            assert_eq!(clipboard_config(dir, bad), None, "{bad:?}");
        }
    }
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use shared::{Capability, CommandEnvelope, debug, error};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::watch;

use super::applications::{self, ApplicationsController};
use super::audio::{self, mixer::PrivacySources};
use super::battery::BatteryController;
use super::bluetooth::{self, BluetoothController};
use super::brightness::{self, BrightnessController};
use super::files::{self, FilesController};
use super::idle::{self, IdleController};
use super::keyboard::{self, KeyboardController};
use super::lock::{self, LockController};
use super::mpris::{self, MprisController, MprisSignal};
use super::network::{self, NetworkController};
use super::notifications::{self, NotificationsController, NotificationsSignal};
use super::power::{self, PowerController};
use super::privacy::PrivacyController;
use super::processes::{self, ProcessesController};
use super::signals::{Senders, Signal, Signals};
use super::storage::{self, StorageController};
use super::sysinfo::{self, SysinfoController};
use super::system::SystemController;
use super::tray::{self, TrayController, TraySignal};
use super::updates::{self, UpdatesController};
use super::windows::{self, WindowsController};
use super::with_call_timeout;
use super::worker::{Worker, queue, spawn_worker};
use super::workspaces::{self, WorkspacesController};
use crate::compositor::CompositorKind;
use crate::snapshot::push_snapshot;
use crate::{log_unstarted, socket};

/// On-demand controllers and their inputs. `audio` starts the PipeWire mixer and stores its command
/// channel; `lock` is boot-built in `main.rs` for relock (ADR-0060) and passed to dispatch.
pub struct Capabilities {
    network: Option<Worker<NetworkController>>,
    bluetooth: Option<Worker<BluetoothController>>,
    tray: Option<TrayController>,
    notifications: Option<NotificationsController>,
    mpris: Option<MprisController>,
    sysinfo: Option<SysinfoController>,
    keyboard: Option<Worker<KeyboardController>>,
    privacy: Option<PrivacyController>,
    updates: Option<UpdatesController>,
    battery: Option<BatteryController>,
    brightness: Option<BrightnessController>,
    workspaces: Option<WorkspacesController>,
    windows: Option<WindowsController>,
    power: Option<PowerController>,
    system: Option<SystemController>,
    applications: Option<ApplicationsController>,
    files: Option<FilesController>,
    storage: Option<StorageController>,
    processes: Option<ProcessesController>,
    audio: Option<audio::mixer::AudioCommandSender>,
    idle: Option<IdleController>,

    senders: Senders,
    /// Event-shaped, not a roster entry: never pushes a `StateSnapshot`; events go to `main.rs`
    /// (ADR-0032).
    idle_tx: UnboundedSender<shared::IdleEvent>,
    /// Shared Supervisor system bus (ADR-0034); session-bus capabilities open their own.
    connection: zbus::Connection,
    sound_tx: std::sync::mpsc::SyncSender<PathBuf>,
    /// Mixer privacy channel (ADR-0034, ADR-0137). `audio` or `privacy` may start the mixer first
    /// and it publishes either way, so a queue here would retain every snapshot for a config that
    /// draws volume and no privacy indicator. A watch keeps only the latest, which is all
    /// `run_privacy_task` ever reads.
    privacy_tx: Option<watch::Sender<PrivacySources>>,
    privacy_sources: watch::Receiver<PrivacySources>,
    /// `workspaces` and `windows`' shared niri/Hyprland reader: whichever starts first spawns it,
    /// the other attaches instead of opening a second connection.
    compositor_reader: CompositorReader,
}

/// The state handles always exist; only the reader thread is deferred to first use.
#[derive(Default)]
struct CompositorReader {
    workspaces: Arc<Mutex<workspaces::controller::WorkspacesState>>,
    windows: Arc<Mutex<windows::controller::WindowsState>>,
    compositor: Option<CompositorKind>,
    started: bool,
}

impl Capabilities {
    /// Builds channels and returns both halves. Controllers start only when config reads their
    /// `mantle` member (ADR-0070 decision 1).
    pub fn new(
        connection: zbus::Connection,
        sound_tx: std::sync::mpsc::SyncSender<PathBuf>,
        idle_tx: UnboundedSender<shared::IdleEvent>,
    ) -> (Self, Signals) {
        let (senders, signals) = Senders::channels();
        let (privacy_tx, privacy_sources) = watch::channel(PrivacySources::default());

        let capabilities = Self {
            network: None,
            bluetooth: None,
            tray: None,
            notifications: None,
            mpris: None,
            sysinfo: None,
            keyboard: None,
            privacy: None,
            updates: None,
            battery: None,
            brightness: None,
            workspaces: None,
            windows: None,
            power: None,
            system: None,
            applications: None,
            files: None,
            storage: None,
            processes: None,
            audio: None,
            idle: None,
            senders,
            idle_tx,
            connection,
            sound_tx,
            privacy_tx: Some(privacy_tx),
            privacy_sources,
            compositor_reader: CompositorReader::default(),
        };
        (capabilities, signals)
    }

    /// Spawns the niri/Hyprland reader on the first call; later calls reuse it. Returns the
    /// detected compositor, if any.
    fn ensure_compositor_reader(&mut self) -> Option<CompositorKind> {
        if !self.compositor_reader.started {
            self.compositor_reader.started = true;
            self.compositor_reader.compositor = crate::compositor::detect_compositor();
            if let Some(kind) = self.compositor_reader.compositor {
                let workspaces_publisher = workspaces::controller::StatePublisher::new(
                    Arc::clone(&self.compositor_reader.workspaces),
                    self.senders.workspaces.clone(),
                    kind,
                );
                let windows_publisher = windows::controller::StatePublisher::new(
                    Arc::clone(&self.compositor_reader.windows),
                    self.senders.windows.clone(),
                    kind.name(),
                );
                match kind {
                    CompositorKind::Niri => workspaces::niri::spawn_reader(workspaces_publisher, windows_publisher),
                    CompositorKind::Hyprland => {
                        workspaces::hyprland::spawn_reader(workspaces_publisher, windows_publisher)
                    }
                }
            }
        }
        self.compositor_reader.compositor
    }

    /// Stops every program declared with `session_process` and waits for it, the session-lifetime
    /// counterpart to `reap_processes`. A no-op on most shutdowns: the controller exists only
    /// once a config has read `mantle.processes`.
    ///
    /// Awaited rather than dropped because these are the processes whose exit path was worth
    /// declaring a signal for; a shell that exits without giving them theirs is the reason the
    /// signal is configurable.
    pub async fn reap_sessions(&self) {
        if let Some(processes) = &self.processes {
            processes.reap_all().await;
        }
    }

    /// Live `idle` controller for resetting generation threshold registrations on reload
    /// (ADR-0006); `None` when no threshold was configured.
    pub fn idle(&self) -> Option<&IdleController> {
        self.idle.as_ref()
    }

    /// `secure_submit(network, connect)`: the worker pairs the secret with the intent it holds
    /// (ADR-0029). A request that is never run drops the secret, which zeroizes it.
    pub fn connect_prompted(&self, generation_id: u32, secret: shared::Zeroizing<Vec<u8>>) {
        let connect = move |network: &NetworkController| match network.take_prompted_intent() {
            Some(pending) => {
                let network = network.clone();
                tokio::spawn(async move { network.connect(pending, secret).await });
            }
            None => debug!(
                "generation {generation_id}'s secure_submit(network, connect) arrived with no intent for the network the prompt names; dropping"
            ),
        };
        let sent = match &self.network {
            Some(network) => network.send(Box::new(connect)).is_ok(),
            None => false,
        };
        if !sent {
            debug!(
                "generation {generation_id}'s secure_submit(network, connect) arrived with no network backend; dropping"
            );
        }
    }

    /// Drops what a departed generation asked for. Its replacement starts with fresh state (named
    /// state survives only an in-place reload), so nothing would send the Bluetooth discovery stop
    /// or the Wi-Fi prompt cancel the old one owed, and discovery ran for the rest of the session.
    /// Its folder watches likewise kept a task and a kernel watch. The worker sends are queued
    /// behind that generation's own requests, so none runs after it.
    pub fn forget_departed_requests(&self) {
        if let Some(bluetooth) = &self.bluetooth {
            let _ = bluetooth.send(Box::new(|bluetooth| bluetooth.set_discovery(false)));
        }
        if let Some(network) = &self.network {
            let _ = network.send(Box::new(|network| network.cancel_connect()));
        }
        if let Some(files) = &self.files {
            files.forget_watches();
        }
    }

    /// ADR-0070 lazy start, re-entrant across generation swaps (decision 3), with each arm a no-op
    /// after construction. Backends that wait on another service build in a [`Worker`].
    pub async fn start(&mut self, capability: Capability) {
        match capability {
            Capability::Network => {
                if self.network.as_ref().is_none_or(UnboundedSender::is_closed) {
                    let (events, signals) = unbounded_channel();
                    let connection = self.connection.clone();
                    let build = async move {
                        NetworkController::new(connection, events)
                            .await
                            .map_err(|err| {
                                debug!("NetworkManager is unreachable; disabled until the next start: {err}")
                            })
                            .ok()
                    };
                    let handle =
                        |network: NetworkController, signal| async move { network.handle_signal(signal).await };
                    self.network = Some(spawn_worker(build, signals, handle, self.senders.network.clone()));
                }
            }
            Capability::Bluetooth => {
                if self.bluetooth.is_none() {
                    let (events, signals) = unbounded_channel();
                    let build = BluetoothController::new(self.connection.clone(), events);
                    let handle =
                        |bluetooth: BluetoothController, signal| async move { bluetooth.handle_signal(signal).await };
                    self.bluetooth = Some(spawn_worker(
                        async move { Some(build.await) },
                        signals,
                        handle,
                        self.senders.bluetooth.clone(),
                    ));
                }
            }
            // Own session bus; missing it yields `inert`. Tray, Notifications and Mpris push once when
            // built: with no item, notification or player they never speak.
            Capability::Tray => {
                if self.tray.is_none() {
                    self.tray = Some(match with_call_timeout(zbus::connection::Builder::session()).await {
                        Ok(bus) => TrayController::new(bus, self.senders.tray.clone()).await,
                        Err(err) => {
                            error!("failed to connect to the session bus; tray host disabled for this run: {err}");
                            TrayController::inert(self.senders.tray.clone())
                        }
                    });
                    let _ = self.senders.tray.send(TraySignal::RegistryChanged);
                }
            }
            // Own session bus (ADR-0033); an existing notification owner makes this inert via
            // RequestName's DoNotQueue.
            Capability::Notifications => {
                if self.notifications.is_none() {
                    self.notifications = Some(match with_call_timeout(zbus::connection::Builder::session()).await {
                        Ok(bus) => {
                            NotificationsController::new(bus, self.senders.notifications.clone(), self.sound_tx.clone())
                                .await
                        }
                        Err(err) => {
                            error!(
                                "failed to connect to the session bus; notifications server disabled for this run: {err}"
                            );
                            NotificationsController::inert(self.senders.notifications.clone(), self.sound_tx.clone())
                        }
                    });
                    let _ = self.senders.notifications.send(NotificationsSignal::Changed);
                }
            }
            // Own session bus (ADR-0036); `new` spawns discovery and returns.
            Capability::Mpris => {
                if self.mpris.is_none() {
                    self.mpris = Some(match with_call_timeout(zbus::connection::Builder::session()).await {
                        Ok(bus) => MprisController::new(bus, self.senders.mpris.clone()),
                        Err(err) => {
                            error!(
                                "failed to connect to the session bus; player discovery disabled for this run: {err}"
                            );
                            MprisController::inert()
                        }
                    });
                    let _ = self.senders.mpris.send(MprisSignal::Changed);
                }
            }
            // Three dormant poll tasks until `sysinfo:configure` (ADR-0035).
            Capability::Sysinfo => {
                if self.sysinfo.is_none() {
                    self.sysinfo = Some(SysinfoController::new(
                        PathBuf::from("/proc"),
                        PathBuf::from("/sys/class/hwmon"),
                        self.senders.sysinfo.clone(),
                    ));
                }
            }
            // No `*::kbd_backlight` LED -> -1; missing lock source -> `false` (ADR-0034).
            Capability::Keyboard => {
                if self.keyboard.is_none() {
                    let (events, signals) = unbounded_channel();
                    let connection = self.connection.clone();
                    let build =
                        async move { Some(KeyboardController::new(connection, Path::new("/sys/class/leds"), events)) };
                    let handle = |keyboard: KeyboardController, _| async move { keyboard.snapshot() };
                    self.keyboard = Some(spawn_worker(build, signals, handle, self.senders.keyboard.clone()));
                }
            }
            // Camera `/dev/videoN` inotify plus `/proc` scan, enriched by `privacy_sources`
            // (ADR-0034); microphone/screencast share that channel (ADR-0137).
            Capability::Privacy => {
                if self.privacy.is_none() {
                    self.ensure_mixer_thread();
                    self.privacy = Some(PrivacyController::new(
                        PathBuf::from("/proc"),
                        &PathBuf::from("/sys/class/video4linux"),
                        self.privacy_sources.clone(),
                        self.senders.privacy.clone(),
                    ));
                }
            }
            // Separate from sysinfo's scheduler, dormant until Lua sets an interval; construction
            // detects the package manager and pushes its name immediately (ADR-0034, ADR-0134).
            Capability::Updates => {
                if self.updates.is_none() {
                    self.updates = Some(UpdatesController::new(self.senders.updates.clone()));
                }
            }
            // UPower DisplayDevice, composite across batteries; no UPower means no push
            // (ADR-0080).
            Capability::Battery => {
                if self.battery.is_none() {
                    self.battery = Some(BatteryController::new(self.connection.clone(), self.senders.battery.clone()));
                }
            }
            // Firmware > platform > raw; no device means no push (see `brightness`).
            Capability::Brightness => {
                if self.brightness.is_none() {
                    self.brightness = Some(BrightnessController::new(
                        PathBuf::from("/sys/class/backlight"),
                        self.connection.clone(),
                        self.senders.brightness.clone(),
                    ));
                }
            }
            // niri/Hyprland IPC, shared with `windows` (ADR-0247); no implementor means no push.
            Capability::Workspaces => {
                if self.workspaces.is_none() {
                    let compositor = self.ensure_compositor_reader();
                    self.workspaces = Some(WorkspacesController::new(
                        Arc::clone(&self.compositor_reader.workspaces),
                        compositor,
                        self.senders.workspaces.clone(),
                    ));
                }
            }
            // Shares the same reader with `workspaces`, or falls back to
            // `zwlr_foreign_toplevel_manager_v1` on its own connection (ADR-0247).
            Capability::Windows => {
                if self.windows.is_none() {
                    let compositor = self.ensure_compositor_reader();
                    self.windows = Some(
                        WindowsController::new(
                            Arc::clone(&self.compositor_reader.windows),
                            compositor,
                            self.senders.windows.clone(),
                        )
                        .await,
                    );
                }
            }
            // UPower supplies on_battery/energy_rate; power-profiles-daemon supplies profiles;
            // either may be missing (ADR-0053).
            Capability::Power => {
                if self.power.is_none() {
                    self.power = Some(PowerController::new(self.connection.clone(), self.senders.power.clone()));
                }
            }
            // 1Hz clock, and nothing else since ADR-0136 (ADR-0053).
            Capability::System => {
                if self.system.is_none() {
                    self.system = Some(SystemController::new(self.senders.system.clone()));
                }
            }
            // Installed `.desktop` entries; scans in background and returns before parsing starts.
            Capability::Applications => {
                if self.applications.is_none() {
                    self.applications = Some(ApplicationsController::new(
                        applications::application_dirs(
                            shared::xdg_dir("XDG_DATA_HOME", ".local/share"),
                            std::env::var("XDG_DATA_DIRS").ok(),
                        ),
                        self.senders.applications.clone(),
                    ));
                }
            }
            // `files:watch` starts work; nothing is listed before it.
            Capability::Files => {
                if self.files.is_none() {
                    self.files = Some(FilesController::new(self.senders.files.clone()));
                }
            }
            // `persistent_table` opens storage on demand.
            Capability::Storage => {
                if self.storage.is_none() {
                    self.storage = Some(StorageController::new(self.senders.storage.clone()));
                }
            }
            // `session_process` declares on demand, the way `persistent_table` opens storage.
            Capability::Processes => {
                if self.processes.is_none() {
                    self.processes = Some(ProcessesController::new(self.senders.processes.clone()));
                }
            }
            Capability::Audio => self.ensure_mixer_thread(),
            // On the roster since ADR-0141. Push immediately after lazy start because a quiet
            // inhibitor watch may never speak. Takes a session bus as well as the system one, to
            // serve `org.freedesktop.ScreenSaver` (ADR-0231); without it that half stays unserved.
            Capability::Idle => {
                if self.idle.is_none() {
                    let session_bus = with_call_timeout(zbus::connection::Builder::session())
                        .await
                        .inspect_err(|err| {
                            error!(
                                "failed to connect to the session bus; org.freedesktop.ScreenSaver inhibits are unavailable for this run: {err}"
                            )
                        })
                        .ok();
                    self.idle = Some(
                        IdleController::new(
                            self.connection.clone(),
                            session_bus,
                            self.idle_tx.clone(),
                            self.senders.idle.clone(),
                        )
                        .await,
                    );
                }
                if let Some(idle) = &self.idle {
                    let _ = self.senders.idle.send(idle.snapshot());
                }
            }
            // `LockController` is boot-built in `main.rs` (ADR-0060); polkit starts its agent
            // there.
            Capability::Lock | Capability::Polkit => {}
        }
    }

    /// Starts the PipeWire mixer once for whichever of `audio`/`privacy` asks first.
    fn ensure_mixer_thread(&mut self) {
        if self.audio.is_some() {
            return;
        }
        let Some(privacy_tx) = self.privacy_tx.take() else { return };
        let (command_tx, command_rx) = audio::mixer::command_channel();
        let audio_tx = self.senders.audio.clone();
        std::thread::spawn(move || audio::mixer::run(audio_tx, privacy_tx, command_rx));
        self.audio = Some(command_tx);
    }

    /// Pushes one received [`Signal`] from the winning `select!` arm.
    pub fn push(
        &self,
        signal: Signal,
        registry: &socket::GenerationRegistry,
        generation_id: u32,
        last_snapshots: &mut HashMap<Capability, crate::snapshot::Published>,
    ) {
        macro_rules! push {
            ($capability:expr, $state:expr) => {
                push_snapshot(registry, generation_id, last_snapshots, $capability, $state)
            };
        }
        match signal {
            Signal::Audio(state) => push!(Capability::Audio, &state),
            Signal::Network(state) => push!(Capability::Network, &state),
            Signal::Bluetooth(state) => push!(Capability::Bluetooth, &state),
            // No debounce: `build_state` already snapshots recomputed data (ADR-0031).
            Signal::Tray => {
                if let Some(tray) = &self.tray {
                    push!(Capability::Tray, &tray.build_state());
                }
            }
            // No debounce (ADR-0036).
            Signal::Mpris => {
                if let Some(mpris) = &self.mpris {
                    push!(Capability::Mpris, &mpris.build_state());
                }
            }
            // No debounce; each mutation re-derives notification state (ADR-0033).
            Signal::Notifications => {
                if let Some(notifications) = &self.notifications {
                    push!(Capability::Notifications, &notifications.build_state());
                }
            }
            // Poll task already wrote fields under its lock; clone and push (ADR-0035).
            Signal::Sysinfo => {
                if let Some(sysinfo) = &self.sysinfo {
                    push!(Capability::Sysinfo, &sysinfo.snapshot());
                }
            }
            Signal::Keyboard(state) => push!(Capability::Keyboard, &state),
            Signal::Battery => {
                if let Some(battery) = &self.battery {
                    push!(Capability::Battery, &battery.snapshot());
                }
            }
            // Only emitted when a backlight device exists (ADR-0053).
            Signal::Brightness => {
                if let Some(brightness) = &self.brightness {
                    push!(Capability::Brightness, &brightness.snapshot());
                }
            }
            // Controller already filters compositor events to real changes.
            Signal::Workspaces => {
                if let Some(workspaces) = &self.workspaces {
                    push!(Capability::Workspaces, &workspaces.snapshot());
                }
            }
            Signal::Windows => {
                if let Some(windows) = &self.windows {
                    push!(Capability::Windows, &windows.snapshot());
                }
            }
            // Controller filters UPower's roughly once-per-minute EnergyRate repeats.
            Signal::Power => {
                if let Some(power) = &self.power {
                    push!(Capability::Power, &power.snapshot());
                }
            }
            Signal::Applications => {
                if let Some(applications) = &self.applications {
                    push!(Capability::Applications, &applications.snapshot());
                }
            }
            // `watch`/`unwatch` and each settled folder-change burst.
            Signal::Files => {
                if let Some(files) = &self.files {
                    push!(Capability::Files, &files.snapshot());
                }
            }
            // Every `open`/`set`, before debounced save (ADR-0136).
            Signal::Storage => {
                if let Some(storage) = &self.storage {
                    push!(Capability::Storage, &storage.snapshot());
                }
            }
            // Every declare, start, signal answered, and exit noticed.
            Signal::Processes => {
                if let Some(processes) = &self.processes {
                    push!(Capability::Processes, &processes.snapshot());
                }
            }
            // Once per wall-clock second, when the epoch changes (ADR-0053 decision 2).
            Signal::System => {
                if let Some(system) = &self.system {
                    push!(Capability::System, &system.snapshot());
                }
            }
            Signal::Privacy => {
                if let Some(privacy) = &self.privacy {
                    push!(Capability::Privacy, &privacy.snapshot());
                }
            }
            // Periodic checks and install progress updates.
            Signal::Updates => {
                if let Some(updates) = &self.updates {
                    push!(Capability::Updates, &updates.snapshot());
                }
            }
            // Inhibitor watch sends state directly, like `Audio`.
            Signal::Idle(state) => push!(Capability::Idle, &state),
        }
    }

    /// Routes a command to its module (ADR-0037). Optional controllers exist only after the config
    /// reads their member (ADR-0070), so missing ones call `log_unstarted`; boot-built `lock` is
    /// passed in, and read-only `battery`/`privacy`/`system` have no dispatch.
    pub fn dispatch(&mut self, capability: Capability, envelope: &CommandEnvelope, lock: &LockController) {
        debug!(2; "dispatching command: {capability} {}", envelope.params.action);
        macro_rules! to {
            ($held:expr, $dispatch:path) => {
                match &$held {
                    Some(controller) => $dispatch(controller, envelope),
                    None => log_unstarted(envelope),
                }
            };
        }
        match capability {
            Capability::Network => queue(&self.network, envelope, network::dispatch),
            Capability::Bluetooth => queue(&self.bluetooth, envelope, bluetooth::dispatch),
            Capability::Tray => to!(self.tray, tray::dispatch),
            Capability::Notifications => to!(self.notifications, notifications::dispatch),
            Capability::Mpris => to!(self.mpris, mpris::dispatch),
            Capability::Sysinfo => to!(self.sysinfo, sysinfo::dispatch),
            Capability::Keyboard => queue(&self.keyboard, envelope, keyboard::dispatch),
            Capability::Brightness => to!(self.brightness, brightness::dispatch),
            Capability::Workspaces => to!(self.workspaces, workspaces::dispatch),
            Capability::Windows => to!(self.windows, windows::dispatch),
            Capability::Power => to!(self.power, power::dispatch),
            Capability::Updates => to!(self.updates, updates::dispatch),
            Capability::Applications => to!(self.applications, applications::dispatch),
            Capability::Files => to!(self.files, files::dispatch),
            Capability::Storage => to!(self.storage, storage::dispatch),
            Capability::Processes => to!(self.processes, processes::dispatch),
            Capability::Audio => to!(self.audio, audio::dispatch),
            Capability::Idle => to!(self.idle, idle::dispatch),
            Capability::Lock => lock::dispatch(lock, envelope),
            // Answered in `Supervisor::dispatch_capability_command`, where its controller lives
            // beside the state push that cancel handling needs.
            Capability::Polkit => {}
            // Read-only: no action enum; a named command is malformed Renderer input.
            Capability::Battery | Capability::Privacy | Capability::System => {
                debug!(
                    "{capability}: read-only capability received a command from generation {}; dropping",
                    envelope.params.generation_id
                )
            }
        }
    }
}

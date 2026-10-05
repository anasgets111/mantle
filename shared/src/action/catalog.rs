//! Every capability's action enum and the types its fields decode into. `supervisor/src/stubs.rs`
//! turns each variant into a typed method, its `///` block the description.

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApplicationsAction {
    /// Rescans installed desktop entries. The directories are watched, so only a failed watch
    /// (logged) needs this.
    Refresh,
    /// Launches `entries[].id`, detached; `Terminal=true` entries run in `$TERMINAL`.
    Launch { id: String },
    /// Opens an `http`, `https` or `mailto` URL with `xdg-open` (ADR-0103). One over 2048 bytes or
    /// holding whitespace or a control character is refused.
    OpenUrl { url: String },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AudioAction {
    /// Sets master output volume in percent, clamped to `[0, 150]`.
    SetVolume { volume: f32 },
    /// Sets master output mute.
    SetMuted { muted: bool },
    /// Toggles master output mute.
    ToggleMute,
    /// Sets default output balance, `-1.0` (left) to `1.0` (right), clamped; the louder side keeps its level.
    SetBalance { balance: f32 },
    /// Makes this `sinks[].id` the default output.
    SetDefaultSink { id: u32 },
    /// Makes this `sources[].id` the default input.
    SetDefaultSource { id: u32 },
    /// Sets one `sinks[].channels[]` level in percent, clamped to `[0, 150]`. Other channels stay unchanged.
    SetSinkChannelVolume { id: u32, index: u32, volume: f32 },
    /// Sets one `sources[].channels[]` level in percent, clamped to `[0, 100]`. Other channels stay unchanged.
    SetSourceChannelVolume { id: u32, index: u32, volume: f32 },
    /// Sets default input volume in percent, clamped to `[0, 100]`.
    SetSourceVolume { volume: f32 },
    /// Sets default input mute.
    SetSourceMuted { muted: bool },
    /// Toggles default input mute.
    ToggleSourceMute,
    /// Sets an `apps[].id` stream's volume in percent, clamped to `[0, 100]`.
    SetAppVolume { id: u32, volume: f32 },
    /// Sets an `apps[].id` stream's mute.
    SetAppMuted { id: u32, muted: bool },
    /// Switches a `bluetooth[].device` to one of its `codecs[].index`.
    SetBluetoothProfile { device: u32, index: i32 },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BluetoothAction {
    /// Powers the adapter on or off.
    SetEnabled { enabled: bool },
    /// Makes the adapter findable by other devices, or not.
    SetDiscoverable { discoverable: bool },
    /// Clears `discovered_devices` and scans. The request holds, so a scan starts once the adapter
    /// powers on and pauses while a `pair` runs.
    StartDiscovery,
    /// Stops discovery; `discovered_devices` stays.
    StopDiscovery,
    /// Pairs a discovered device, then trusts and connects it.
    Pair { mac: String },
    /// Trusts and connects a paired device.
    Connect { mac: String },
    /// Disconnects a connected device.
    Disconnect { mac: String },
    /// Removes a device from BlueZ, unpairing it.
    Forget { mac: String },
    /// Accepts or rejects a confirmation, authorization or service `pairing_request` for `mac`;
    /// a yes within 750 ms of it appearing is ignored. Entry requests use `secure_submit`.
    AnswerPairing { mac: String, accept: bool },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BrightnessAction {
    /// Sets the screen backlight, `0` to `100`, fractions allowed; values outside clamp to it.
    Set { percent: f64 },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RadioAction {
    /// Soft-blocks or unblocks every device of `kind`. A hardware block stays.
    SetBlocked { kind: crate::state::radio::RadioKind, blocked: bool },
    /// Soft-blocks or unblocks every radio (airplane mode).
    SetAllBlocked { blocked: bool },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FilesAction {
    /// Keeps `folders[path]` listing an absolute folder. `extensions` match case-insensitively,
    /// dot optional; omitted means every file.
    Watch {
        #[serde(deserialize_with = "super::absolute")]
        path: String,
        #[serde(default, deserialize_with = "super::lua_list")]
        extensions: Vec<String>,
    },
    /// Stops watching `path` and removes it from `folders`.
    Unwatch {
        #[serde(deserialize_with = "super::absolute")]
        path: String,
    },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProcessesAction {
    /// Registers `name` (required before `start`) and sets its stop signal, default `TERM`.
    /// Redeclaring updates the signal without touching a running program.
    Declare {
        #[serde(deserialize_with = "super::non_empty")]
        name: String,
        #[serde(default)]
        stop_signal: Option<SignalName>,
    },
    /// Runs `cmd` with `args` (no shell) as its own process group. No-op while `running` or when
    /// `name` is undeclared.
    Start {
        #[serde(deserialize_with = "super::non_empty")]
        name: String,
        #[serde(deserialize_with = "super::non_empty")]
        cmd: String,
        #[serde(default, deserialize_with = "super::lua_list")]
        args: Vec<String>,
    },
    /// Sends `signal` to the program's process (not its group); no-op when not running.
    Signal {
        #[serde(deserialize_with = "super::non_empty")]
        name: String,
        signal: SignalName,
    },
    /// Sends the declared stop signal to the process group, then `KILL` if it is still up 5 s
    /// later; no-op when not running.
    Stop {
        #[serde(deserialize_with = "super::non_empty")]
        name: String,
    },
}

/// A signal name without the `SIG` prefix.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "UPPERCASE")]
pub enum SignalName {
    Term,
    Int,
    Hup,
    Quit,
    Usr1,
    Usr2,
    Kill,
    Stop,
    Cont,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum KeyboardAction {
    /// Sets the keyboard backlight, `0` to `100`, fractions allowed; values outside clamp to it.
    SetBacklight { percent: f64 },
    /// Switches to the 0-based configured layout `index`.
    SwitchLayout { index: usize },
}

/// `mantle.lock` actions. There is no `unlock`; only a correct password unlocks (ADR-0042).
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LockAction {
    /// Locks the session; a no-op while `active`.
    Lock,
    /// Keeps the lock up `ms` after a correct password for an out-animation (ADR-0190). Clamped to
    /// 600; omitted is `0`.
    SetUnlockAnimation {
        #[serde(default)]
        ms: Option<u64>,
    },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MprisAction {
    /// Sends a playback command to `players[].id`.
    Control { id: String, cmd: PlayerCommand },
    /// Seeks to an absolute position in microseconds, clamped to `[0, length]` (only `>= 0` when `length` is `nil`).
    Seek { id: String, position_us: i64 },
    /// Seeks by a signed offset in microseconds, unclamped; past the end may skip to the next track.
    SeekRelative { id: String, offset_us: i64 },
    /// Calls Raise; check `players[].can_raise` before calling.
    Raise { id: String },
    /// Calls Quit; check `players[].can_quit` before calling.
    Quit { id: String },
    /// Opens an absolute URI in the player.
    OpenUri { id: String, uri: String },
    /// Sets MPRIS `Volume` in percent; finite values at or above zero are accepted.
    SetVolume { id: String, value: f64 },
    /// Sets MPRIS `LoopStatus`.
    SetLoopStatus { id: String, value: LoopStatus },
    /// Sets MPRIS `Shuffle`.
    SetShuffle { id: String, value: bool },
    /// Sets a positive finite MPRIS playback rate.
    SetRate { id: String, value: f64 },
    /// Inserts a URI after a track id, or after `/org/mpris/MediaPlayer2/TrackList/NoTrack` to prepend.
    TrackListAddTrack { id: String, uri: String, after_track: String, set_as_current: bool },
    /// Removes a track by its TrackList object path.
    TrackListRemoveTrack { id: String, track_id: String },
    /// Starts the track identified by its TrackList object path.
    TrackListGoTo { id: String, track_id: String },
    /// Reads a bounded playlist page into `players[].playlists`.
    PlaylistsGet { id: String, index: u32, count: u32, order: String, reverse: bool },
    /// Activates a playlist by its object path.
    PlaylistsActivate { id: String, playlist_id: String },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PlayerCommand {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    Stop,
}

/// MPRIS `LoopStatus`, also the `set_loop_status` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LoopStatus {
    /// Plays through once.
    None,
    /// Repeats the current track.
    Track,
    /// Repeats the playlist.
    Playlist,
}

impl LoopStatus {
    /// The MPRIS wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Track => "Track",
            Self::Playlist => "Playlist",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NetworkAction {
    /// Turns NetworkManager networking on or off.
    SetNetworkingEnabled { enabled: bool },
    /// Powers the Wi-Fi radio.
    SetWifiEnabled { enabled: bool },
    /// `false` disconnects every wired device; `true` activates each one's autoconnect profile, and a
    /// device without one stays down.
    SetEthernetEnabled { enabled: bool },
    /// Requests a Wi-Fi scan; a no-op without Wi-Fi hardware.
    Scan,
    /// Requests a scan on the named Wi-Fi interface.
    ScanDevice { id: String },
    /// Joins a network. Without a saved profile, a secured, `hidden` or out-of-range one sets
    /// `password_ssid` and waits for a key.
    Connect { ssid: String, hidden: bool },
    /// Joins through the named Wi-Fi interface; a removed ID is never retargeted.
    ConnectDevice { ssid: String, hidden: bool, id: String },
    /// Drops the one current password request, on any Wi-Fi device; a join already running continues.
    CancelConnect,
    /// Stops the one current join on any Wi-Fi device, deleting a profile the join created.
    AbortConnect,
    /// Deletes every saved profile for this SSID.
    Forget { ssid: String },
    /// Disconnects Wi-Fi; NetworkManager does not autoconnect it again until the next join.
    DisconnectWifi,
    /// Disconnects the named Wi-Fi interface.
    DisconnectWifiDevice { id: String },
    /// Activates the saved VPN or WireGuard profile with this UUID. A missing secret raises `vpn_secret`.
    ConnectVpn { uuid: String },
    /// Deactivates the VPN or WireGuard profile with this UUID.
    DisconnectVpn { uuid: String },
    /// Declines the pending `vpn_secret` request, failing that activation.
    CancelVpnSecret,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotificationsAction {
    /// Removes a queued notification.
    Dismiss { id: u32 },
    /// Invokes an `actions[].key`, or `"default"`; removes the notification unless it is resident.
    InvokeAction {
        id: u32,
        #[serde(deserialize_with = "super::non_empty")]
        key: String,
    },
    /// Sends reply text to a notification with `has_reply`; removes it unless it is resident.
    Reply { id: u32, text: String },
    /// Sets an urgency tier's sound: an existing file under `/usr/share`, `/usr/local/share`, `/opt`
    /// or `$XDG_DATA_HOME`, else ignored. Only Ogg Vorbis and 16-bit PCM WAV play.
    SetSound { urgency: Urgency, path: String },
    /// Gates non-critical notification sounds.
    SetDnd { enabled: bool },
    /// Mutes non-critical sounds like `set_dnd`, without changing `dnd`.
    SetQuiet { enabled: bool },
    /// Silences every sound from an app, critical included, matched exactly on `app_name` or
    /// `desktop_entry`.
    SetAppMuted { app: String, muted: bool },
    /// Pauses every expiry countdown for `seconds`, capped at 300; `0` releases the hold.
    HoldExpiry { seconds: u64 },
}

/// Notification urgency, also the `set_sound` tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    /// Switches to one of `profiles`; a name not in it, or one the daemon rejects, is logged and
    /// `active_profile` stays.
    SetProfile { name: String },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SysinfoAction {
    /// Sets poll intervals; every one starts at `0`, so nothing is read until this. The first
    /// reading lands on the next wall-clock second (CPU: one interval after it).
    Configure { intervals: SysinfoConfigure },
}

/// `sysinfo:configure`'s table. Absent keys keep their interval; one wrong-typed key raises at the call.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SysinfoConfigure {
    /// Seconds between CPU reads; `0` (the default) stops them.
    pub cpu_interval: Option<u64>,
    /// Seconds between memory and swap reads; `0` (the default) stops them.
    pub ram_interval: Option<u64>,
    /// Seconds between temperature reads; `0` (the default) stops them.
    pub temp_interval: Option<u64>,
    /// Seconds between disk space reads; `0` (the default) stops them.
    pub disk_interval: Option<u64>,
    /// Seconds between GPU telemetry reads; `0` (the default) stops them.
    pub gpu_interval: Option<u64>,
    /// Seconds between network throughput reads; `0` (the default) stops them.
    pub net_interval: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StorageAction {
    /// Loads an absolute JSON file into `files[path]`, filling missing top-level keys from
    /// `defaults`. `persistent_table` sends this; stored values win over defaults.
    Open {
        path: String,
        #[serde(default)]
        defaults: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// Sets `key` in a declared file, `nil` deleting it; saved 1 s after the last write.
    Set {
        path: String,
        key: String,
        #[serde(default)]
        value: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PolkitAction {
    /// Dismisses the prompt; the requesting program sees the request cancelled.
    Cancel,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TrayAction {
    /// Left-click activation at screen coordinates `x`, `y`; a no-op when `item_is_menu`.
    Activate { id: String, x: i32, y: i32 },
    /// Context-menu activation at screen coordinates `x`, `y`.
    ContextMenu { id: String, x: i32, y: i32 },
    /// Middle-click activation at screen coordinates `x`, `y` (ADR-0074).
    SecondaryActivate { id: String, x: i32, y: i32 },
    /// Scrolls the icon by `delta` along `orientation` (ADR-0074).
    Scroll { id: String, delta: i32, orientation: ScrollOrientation },
    /// Clicks the item's `MenuItem.id`.
    ActivateMenuItem { id: String, menu_item_id: i32 },
    /// Tells the application submenu `submenu_id` is opening, then refetches the menu unless it
    /// answers that nothing changed.
    MenuWillShow { id: String, submenu_id: i32 },
}

/// The `tray:scroll` axis, sent to the item as spelled.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ScrollOrientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UpdatesAction {
    /// Checks for upgrades now, even when dormant; ignored while `checking`.
    Check,
    /// Sets the check schedule and AUR use, and seeds a remembered check.
    Configure { config: UpdatesConfigure },
    /// Runs a full upgrade through `pkexec`: `pacman -Syu --noconfirm`, or `aur_helper` when `aur`
    /// is on; `dnf upgrade -y --refresh`; `apt-get update` then `apt-get upgrade --with-new-pkgs`.
    /// Ignored while `installing`. Does not recheck afterwards.
    Install,
}

/// `configure`'s table. One wrong-typed key raises at the call.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdatesConfigure {
    /// Seconds between scheduled checks, the first at once unless `last_successful_check` is
    /// younger; `0` checks only on `check`.
    #[serde(rename = "interval")]
    pub interval_secs: u64,
    /// Persisted Unix seconds of the last successful check. Seeds `last_successful_check` only
    /// while that is `nil`, so a restart need not recheck at once.
    pub checked_at: Option<i64>,
    /// Persisted `packages` from that check, seeded on the same terms; ignored without `checked_at`.
    #[serde(default, deserialize_with = "super::lua_list")]
    pub packages: Vec<UpdateCandidate>,
    /// Also check the AUR and install through `aur_helper` (ADR-0250). Sends every foreign package
    /// name to aur.archlinux.org and builds without PKGBUILD review.
    #[serde(default)]
    pub aur: bool,
}

/// One installed package with a newer version.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdateCandidate {
    /// Package name.
    pub name: String,
    /// Installed version.
    pub old_version: String,
    /// Version on offer.
    pub new_version: String,
    /// Bytes to fetch; `0` when already cached.
    pub download_size: i64,
    /// Bytes the new version occupies installed; not a delta.
    pub installed_size: i64,
    /// Source repository, e.g. `"extra"` or `"aur"`; empty in a seeded list that lacks it.
    #[serde(default)]
    pub repository: String,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkspacesAction {
    /// Focuses a `WorkspaceEntry.id`. Hyprland creates a missing number; niri ignores it.
    Focus { id: String },
    /// Shows or hides a `special[].name` on Hyprland, creating an unknown one; no-op on niri.
    ToggleSpecial {
        #[serde(deserialize_with = "super::non_empty")]
        name: String,
    },
}

// An action a backend does not support logs at debug and does nothing.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WindowsAction {
    /// Focuses a window.
    Focus {
        #[serde(deserialize_with = "super::non_empty")]
        id: String,
    },
    /// Asks the compositor to close the window.
    Close {
        #[serde(deserialize_with = "super::non_empty")]
        id: String,
    },
    /// Sets fullscreen on or off; no-op on niri.
    SetFullscreen {
        #[serde(deserialize_with = "super::non_empty")]
        id: String,
        fullscreen: bool,
    },
    /// Sets minimized on or off; wlr only.
    SetMinimized {
        #[serde(deserialize_with = "super::non_empty")]
        id: String,
        minimized: bool,
    },
    /// Sets maximized on or off; no-op on niri.
    SetMaximized {
        #[serde(deserialize_with = "super::non_empty")]
        id: String,
        maximized: bool,
    },
    /// Moves a window to a workspace.
    MoveToWorkspace {
        #[serde(deserialize_with = "super::non_empty")]
        id: String,
        workspace_id: String,
    },
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SystemAction {
    /// Sets the push interval, `1` second until this. Each push lands on a wall-clock multiple of
    /// it, and one lands at once.
    Configure { settings: SystemConfigure },
}

/// `system:configure`'s table. An absent `interval` keeps the current one.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SystemConfigure {
    /// Seconds between pushes, each on a multiple of it since the epoch, so `60` lands on every
    /// minute; `1` is the default and `0` stops them.
    pub interval: Option<u64>,
}

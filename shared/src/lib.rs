use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

pub mod framing;
pub mod log;
mod paths;
mod secure_buffer;
pub use paths::{
    CHECK_ENV, CONFIG_DIR_ENV, EXIT_COMPOSITOR_GONE, GENERATION_ID_ENV, INSTANCE_DIR_ENV, PROFILE_ENV, VERBOSE_ENV,
    config_dir, control_socket_path, instance_dir, profile_interval, runtime_root, session_locked_flag_path,
    shell_lua_path, xdg_dir,
};
pub use secure_buffer::SecureBuffer;
pub use zeroize::{Zeroize, Zeroizing};

/// std's `eprintln!`/`eprint!` panic on a failed write, and under `panic = "abort"` a full log tmpfs
/// (ADR-0199) would kill lock authority. Both binaries import these over std's with `#[macro_use]`.
///
/// ponytail: a dependency's own write or panic still aborts; containing that needs a separate
/// lock/PAM process.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("{}\n", format_args!($($arg)*)));
    }};
}

/// See [`eprintln!`].
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {{
        let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!($($arg)*));
    }};
}

/// One diagnostic at that level, named by the module it was written in (ADR-0199, amended).
///
/// Imported by path (`use shared::warn;`) rather than through `#[macro_use]`, which both crate roots
/// apply only `cfg(not(test))`, leaving library code under test unable to name these.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Error, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`].
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Warn, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`]. The shell's lifecycle: start, reload, respawn, stop (ADR-0251).
#[macro_export]
macro_rules! notice {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Notice, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`].
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Info, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`]. `debug!(2; "...")` writes at verbosity 2 (`-vvv`); bare
/// `debug!(...)` is verbosity 1, and any other verbosity fails to compile. The `;` (not `,`) keeps a plain format string from ever parsing as
/// a verbosity: `debug!("a, b")`'s first token is the whole string literal, not a bare integer.
#[macro_export]
macro_rules! debug {
    (2; $($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Debug(2), module_path!(), format_args!($($arg)*))
    };
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Debug(1), module_path!(), format_args!($($arg)*))
    };
}

/// The snapshot-hydrated capability roster (ADR-0037; CONTEXT.md). Each [`Capability::as_str`]
/// name is both the Lua `mantle.<name>` member and command `capability` field, so
/// one spelling reaches one capability. Reading a name starts its Supervisor controller
/// (ADR-0070); it remains `nil` until the first `StateSnapshot`, so an unread name costs nothing.
/// A `secure_submit` naming `polkit` starts it too (ADR-0114).
///
/// An enum, not strings (ADR-0076): exhaustive matches make starting a controller and dispatching
/// its commands fail to compile for an unimplemented name. The `roster!` list generates
/// [`Capability::ALL`] and [`Capability::as_str`], so a new variant missing from the Lua
/// namespace, stubs or schema check fails to compile instead of staying silently `nil`.
macro_rules! roster {
    ($($variant:ident => $name:literal, $blurb:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Capability {
            $($variant),+
        }

        impl Capability {
            /// Every variant, in the order the roster has always listed them.
            pub const ALL: &'static [Capability] = &[$(Capability::$variant),+];

            /// The shared wire/Lua spelling. It matches serde's `snake_case` rename.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Capability::$variant => $name),+
                }
            }

            /// The `mantle.<name>` line for generated stubs (`supervisor/src/stubs.rs`). Kept here,
            /// not in the Renderer, so a new variant must provide one.
            pub const fn blurb(self) -> &'static str {
                match self {
                    $(Capability::$variant => $blurb),+
                }
            }
        }
    };
}

roster! {
    Audio => "audio", "PipeWire: output and input volume and mute, device lists, per-app streams and Bluetooth codecs.",
    Network => "network", "NetworkManager: connectivity, Wi-Fi and wired state, scanned access points and join progress.",
    Bluetooth => "bluetooth", "BlueZ: adapter power, discovery, connected, paired and discovered devices, and pairing prompts.",
    Tray => "tray", "StatusNotifierItem: registered tray items with artwork, status and menus.",
    Notifications => "notifications", "The notification server: the newest 20 notifications and do-not-disturb.",
    Mpris => "mpris", "MPRIS: media players with track metadata, playback state and position.",
    Sysinfo => "sysinfo", "CPU, memory and swap use, CPU and GPU temperatures. `nil` until `configure` sets intervals.",
    Keyboard => "keyboard", "Lock keys, the active layout and the keyboard backlight.",
    Privacy => "privacy", "Apps using the camera, microphone or screen capture right now.",
    Updates => "updates", "Pending package upgrades (pacman, optionally AUR), install progress and whether a reboot is due.",
    Lock => "lock", "The session lock: whether it is held, authentication progress and the last failure.",
    Polkit => "polkit", "The pending polkit authentication request, its progress and the last failure.",
    Battery => "battery", "UPower's display device: charge, state and time estimates.",
    System => "system", "Wall and monotonic clocks, pushed once a second.",
    Brightness => "brightness", "The screen backlight percentage; `nil` without a backlight.",
    Workspaces => "workspaces", "Workspaces per output, special workspaces and the focused window.",
    Power => "power", "Power profiles, mains or battery, and battery power draw.",
    Applications => "applications", "Installed desktop entries, indexed by window `app_id`.",
    Files => "files", "Live file listings of watched folders.",
    Storage => "storage", "Each `persistent_table` JSON file, keyed by absolute path.",
    Idle => "idle", "Idle inhibitors, plus threshold and inhibit methods.",
    Processes => "processes", "Programs declared with `session_process`: running state, start time and last exit.",
    Windows => "windows", "Open toplevel windows with title, app ID, workspace, output and state flags.",
}

impl Capability {
    /// Resolves a Renderer-supplied wire string at the trust boundary, or returns `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|capability| capability.as_str() == name)
    }
}

impl Capability {
    /// The `invoke` action names, empty for a read-only capability. The Renderer refuses any other
    /// name at call time; the Supervisor still validates arguments. `supervisor/src/stubs.rs` pins
    /// each list to its serde action enum.
    pub const fn actions(self) -> &'static [&'static str] {
        match self {
            Capability::Applications => &["refresh", "launch", "open_url"],
            Capability::Audio => &[
                "set_volume",
                "set_muted",
                "toggle_mute",
                "set_balance",
                "set_default_sink",
                "set_default_source",
                "set_source_volume",
                "set_source_muted",
                "toggle_source_mute",
                "set_app_volume",
                "set_app_muted",
                "set_bluetooth_profile",
            ],
            Capability::Bluetooth => &[
                "set_enabled",
                "set_discoverable",
                "start_discovery",
                "stop_discovery",
                "pair",
                "connect",
                "disconnect",
                "forget",
                "answer_pairing",
            ],
            Capability::Brightness => &["set"],
            Capability::Files => &["watch", "unwatch"],
            Capability::Processes => &["declare", "start", "signal", "stop"],
            Capability::Keyboard => &["set_backlight", "switch_layout"],
            Capability::Lock => &["lock", "set_unlock_animation"],
            Capability::Mpris => &["control", "seek", "seek_relative"],
            Capability::Network => &[
                "set_networking_enabled",
                "set_wifi_enabled",
                "set_ethernet_enabled",
                "scan",
                "connect",
                "cancel_connect",
                "abort_connect",
                "forget",
                "disconnect_wifi",
            ],
            Capability::Notifications => &[
                "dismiss",
                "invoke_action",
                "reply",
                "set_sound",
                "set_dnd",
                "set_quiet",
                "set_app_muted",
                "hold_expiry",
            ],
            Capability::Power => &["set_profile"],
            Capability::Sysinfo => &["configure"],
            Capability::Storage => &["open", "set"],
            Capability::Polkit => &["cancel"],
            Capability::Tray => &["activate", "secondary_activate", "scroll", "activate_menu_item", "menu_will_show"],
            Capability::Updates => &["check", "configure", "install"],
            Capability::Workspaces => &["focus", "toggle_special"],
            Capability::Windows => &["focus", "close", "set_fullscreen", "set_minimized", "set_maximized"],
            Capability::Battery | Capability::Idle | Capability::Privacy | Capability::System => &[],
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
/// Guarded envelope for a Lua write action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandEnvelope {
    pub params: CommandParams,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandParams {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub arguments: Vec<serde_json::Value>,
    pub expected_revision: u32,
}

/// Supervisor update on system changes that hydrates active Lua signals. `apply_state_snapshot`
/// (`renderer/src/socket/client/mod.rs`) routes by `capability` (ADR-0029); `revision` is that capability's
/// state-version counter (ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    pub capability: String,
    pub revision: u32,
    pub payload: serde_json::Value,
}

/// First frame on every control-socket connection, identifying the generation for commands and
/// pushes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionHandshake {
    pub generation_id: u32,
}

/// Control clients (`mantle set`, `mantle toggle`) use this `generation_id` in
/// [`ConnectionHandshake`] (ADR-0112). It is not a generation: the Supervisor registers no
/// outbound channel or snapshot replay for a one-frame peer that hangs up. `u32::MAX` because
/// generations count up from zero and a real one will never reach it.
pub const CONTROL_CLIENT_GENERATION: u32 = u32::MAX;

/// External write to a config `state(name, initial)` signal (ADR-0112), such as
/// `mantle set launcher_open true`. A control client sends it as [`RendererFrame`], the
/// Supervisor forwards it as [`SupervisorFrame`] to the authoritative generation, and that
/// generation applies the same marshal checks as `signal:set()`, refusing undeclared names. This
/// is the compositor keybind's only write path into a running config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetState {
    /// Name passed to `state(name, initial)`.
    pub name: String,
    pub write: StateWrite,
}

/// Operation applied by [`SetState`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StateWrite {
    /// Store this value, converted to Lua like a capability payload.
    Set(serde_json::Value),
    /// Flip a boolean. Refused on any other value, since a keybind cannot know the current one
    /// and "toggle" means nothing else.
    Toggle,
    /// `mantle toggle <name> <value>`: store this value, unless the state already holds it, in
    /// which case restore the initial the config declared. One keybind opens and closes a modal
    /// whose state is the name of the one showing (`state("modal", "")`).
    ToggleTo(serde_json::Value),
}

/// `mantle call <name> [json...]`: one call into a config-exported `action(name, fn)` (ADR-0197).
///
/// `name` is opaque and never split. `"rec.toggle"` is one key; the dot groups for a reader the way
/// a Lua module path does, and nothing here parses it, so an action may contain any character its
/// config wrote.
///
/// Distinct from [`CommandParams`], which is the config calling *out* to a capability and carries a
/// `generation_id` and `expected_revision` describing the Renderer's view of that capability. An
/// external caller has neither and needs neither.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Call {
    /// Assigned by the Supervisor, not the client: it owns the pending table and the reply route,
    /// and a client id would let one peer answer another's call.
    pub id: u64,
    pub name: String,
    pub arguments: Vec<serde_json::Value>,
}

/// The answer to one [`Call`], carrying `id` back so the Supervisor can find the peer that waits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallResult {
    pub id: u64,
    pub outcome: CallOutcome,
}

/// What a [`Call`] produced. A handler returning nothing and one returning `nil` are both
/// `Returned(null)`: Lua cannot tell them apart, and inventing a difference here would invent one
/// in every config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CallOutcome {
    /// The handler ran and returned this. `null` is a value, not an absence.
    Returned(serde_json::Value),
    /// No such action, a handler that raised, or a returned value that would not marshal. A
    /// returned `{ error = ... }` table is **not** this: it is a config returning a table.
    Failed(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

/// Supervisor -> Renderer: one `ext_idle_notification_v1` event, with wire values `"idled"` and
/// `"resumed"` (ADR-0032). `#[serde(rename)]` pins the protocol's lowercase names instead of
/// Rust's derived PascalCase.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdleState {
    #[serde(rename = "idled")]
    Idled,
    #[serde(rename = "resumed")]
    Resumed,
}

/// Supervisor -> Renderer: an `ext_idle_notification_v1` event fanned out to `generation_id`
/// (ADR-0032). The Renderer finds the callback by `threshold_sec`, the listener's registration
/// duration, not through `StateSnapshot`/`revision`; idle is event-shaped, not pollable state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdleEvent {
    pub generation_id: u32,
    pub threshold_sec: u64,
    pub state: IdleState,
}

/// Supervisor -> Renderer: one stdout/stderr line from a `process.run` child. `id` is the
/// Renderer-assigned spawning `"process"`/`"run"` `CommandEnvelope.id`, not a Supervisor result
/// (ADR-0026).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessOutputLine {
    pub id: u64,
    pub stream: ProcessStream,
    pub line: String,
}

/// Supervisor -> Renderer: the `process.run` child for `id` exited. `code` is absent when
/// [`std::process::ExitStatus::code()`] returns `None`, including signal kills and no spawn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessExited {
    pub id: u64,
    pub code: Option<i32>,
}

/// Renderer -> Supervisor: completed `textfield` `secure_submit` (ADR-0005/ADR-0009/ADR-0027).
/// `secret` is read once through `SecureBuffer::expose_secret`, never through
/// [`CommandParams::arguments`], whose `Vec<serde_json::Value>` would leave a plaintext copy
/// `.zeroize()` cannot reach. The Renderer zeroizes the source in `secure_submit_frame`
/// (`renderer/src/wayland/input/keyboard/secure.rs`) and this copy after `pump` writes it
/// (`renderer/src/socket/mod.rs`). Because the frame crosses an unbounded, unwrapped channel,
/// `Zeroize`/`ZeroizeOnDrop` also scrub failed sends and buffered frames when `outbound_rx` drops.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SecureSubmit {
    pub generation_id: u32,
    #[zeroize(skip)]
    pub capability: Capability,
    pub action: String,
    pub secret: Vec<u8>,
}

/// Hand-written `Debug` prints only `secret`'s length. `RendererFrame` derives `Debug`, so deriving
/// here would put a password in the log from any `{:?}` of a frame. `ZeroizeOnDrop` keeps plaintext
/// from outliving its read, but a formatter can defeat it.
impl std::fmt::Debug for SecureSubmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureSubmit")
            .field("generation_id", &self.generation_id)
            .field("capability", &self.capability)
            .field("action", &self.action)
            .field("secret", &format_args!("<{} bytes redacted>", self.secret.len()))
            .finish()
    }
}

/// Supervisor -> Renderer: take or release `ext_session_lock_v1` (ADR-0042, ADR-0052 decision 1).
/// One flag covers both directions; the Renderer matches its lock state to the flag and reports
/// the outcome. Only `locked = false` may call `unlock_and_destroy`, and only after the
/// Supervisor's PAM worker returns [`PamOutcome::Success`]; the Renderer does not enforce it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetSessionLock {
    pub locked: bool,
}

/// Renderer report of the lock state (ADR-0052 decision 4): "never acquired" differs from "acquired
/// then torn down" even though both end unlocked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LockOutcome {
    /// `ext_session_lock_v1::locked` arrived; lock surfaces are up.
    Locked,
    /// Never acquired: no `lock` node (ADR-0052 decision 3), immediate compositor `finished`, or
    /// no advertised `ext_session_lock_manager_v1`.
    Refused(String),
    /// `finished` after `Locked`: the compositor tore it down through its secure mechanism, not a
    /// denial or a Supervisor request.
    Finished,
    /// `unlock_and_destroy` ran for `SetSessionLock { locked: false }`.
    Unlocked,
}

/// Renderer -> Supervisor: one [`LockOutcome`] per lock state change. The connection already
/// carries the generation id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockReport {
    pub outcome: LockOutcome,
}

/// Supervisor -> Renderer frames, adjacently tagged so one read loop dispatches on `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data")]
pub enum SupervisorFrame {
    StateSnapshot(StateSnapshot),
    /// A config file changed: evaluate `shell.lua` and apply it in place (ADR-0216).
    Reevaluate,
    ProcessOutput(ProcessOutputLine),
    ProcessExited(ProcessExited),
    IdleEvent(IdleEvent),
    SetSessionLock(SetSessionLock),
    /// A control client's `state` write, forwarded to the authoritative generation (ADR-0112).
    /// Answered by a [`CallResult`] with this `id`: `Returned(null)` or the refusal.
    SetState {
        id: u64,
        set: SetState,
    },
    /// A control client's `mantle call`, forwarded to the authoritative generation (ADR-0197).
    Call(Call),
    /// That call's answer, routed back to the waiting control client (ADR-0197).
    CallResult(CallResult),
}

/// Renderer -> Supervisor frames, tagged like [`SupervisorFrame`]. `Command` is the Lua-write
/// envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data")]
pub enum RendererFrame {
    Command(CommandEnvelope),
    SecureSubmit(SecureSubmit),
    LockReport(LockReport),
    /// Control-client frame, not a Renderer frame: `mantle set`/`mantle toggle` uses
    /// [`CONTROL_CLIENT_GENERATION`] (ADR-0112). It stays in this enum because the listener has one
    /// decoder for every peer; a separate peer type would duplicate it.
    SetState {
        id: u64,
        set: SetState,
    },
    /// Control-client frame like [`Self::SetState`]: `mantle call` (ADR-0197). Its `id`, like
    /// `SetState`'s, is zero on the way in; the Supervisor assigns the real one when it forwards.
    Call(Call),
    /// A generation answering a forwarded [`Call`] (ADR-0197).
    CallResult(CallResult),
    /// Idempotently starts `capability`'s controller when this generation first reads
    /// `mantle.<capability>` (ADR-0070 decision 1) or a scene's `secure_submit` names it (decision
    /// 5). No generation ID is needed because the socket identifies the sender. A repeat is a no-op
    /// (decision 3); an unknown name fails decode.
    StartCapability {
        capability: Capability,
    },
}

/// Final result of the Supervisor's re-exec'd PAM worker's conversation, carried as the last
/// [`PamMessage::Outcome`] on the worker's stdout (ADR-0028, ADR-0241). It crosses a different
/// process boundary from `RendererFrame`/`SupervisorFrame`, so is not reused as one. There is no
/// `OtherError`: spawn failure, pipe I/O, a wedged worker or an undecodable frame returns
/// `io::Result::Err` from `supervisor::pam_worker::exchange_over`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PamOutcome {
    Success,
    StartFailed(String),
    AuthFailed,
    MaxTries,
    PamError(String),
}

/// One PAM worker <-> Supervisor conversation frame, carried on the worker's piped stdio in
/// `shared::framing`'s length-prefixed JSON, alongside [`PamOutcome`] (ADR-0241). `Prompt` and
/// `Outcome` are the worker's to send; `Response` answers the `Prompt` just received and only the
/// Supervisor sends it.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PamMessage {
    /// A PAM module asked for input. `echo` tells `pam_prompt`'s echo-on kind from the masked one;
    /// neither is refused.
    Prompt { text: String, echo: bool },
    /// The answer to the most recent `Prompt`.
    Response { secret: Vec<u8> },
    /// The conversation ended.
    Outcome(PamOutcome),
}

/// Hand-written like [`SecureSubmit`]'s: a derived `Debug` would print `Response`'s plaintext.
impl std::fmt::Debug for PamMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt { text, echo } => f.debug_struct("Prompt").field("text", text).field("echo", echo).finish(),
            Self::Response { secret } => {
                f.debug_struct("Response").field("secret", &format_args!("<{} bytes redacted>", secret.len())).finish()
            }
            Self::Outcome(outcome) => f.debug_tuple("Outcome").field(outcome).finish(),
        }
    }
}

impl Zeroize for PamMessage {
    fn zeroize(&mut self) {
        if let Self::Response { secret } = self {
            secret.zeroize();
        }
    }
}

/// glibc's arena totals from `mallinfo2`, in bytes. Both processes report it under `--profile`:
/// `smaps` says how much a process holds, only the in-use/free split says whether it is live.
/// A growing `in_use` is a leak; a growing `free` under a flat `in_use` is glibc holding freed
/// chunks a `malloc_trim` could return.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Malloc {
    /// `arena`: bytes taken from the kernel via `brk`, across every per-thread arena.
    pub arena: u64,
    /// `hblkhd`: bytes in `mmap`ed blocks, which allocations past `M_MMAP_THRESHOLD` take instead.
    pub mmapped: u64,
    /// `uordblks`: bytes handed out and not yet freed.
    pub in_use: u64,
    /// `fordblks`: bytes on glibc's free lists, still charged to the process until a trim.
    pub free: u64,
}

impl Malloc {
    /// Reads every arena's totals, or zeroes on a platform without `mallinfo2`.
    #[cfg(target_env = "gnu")]
    pub fn now() -> Self {
        // SAFETY: plain FFI returning a POD struct by value. `mallinfo2` takes no arguments, locks
        // the arenas itself, and only reads counters.
        let info = unsafe { libc::mallinfo2() };
        Self {
            arena: info.arena as u64,
            mmapped: info.hblkhd as u64,
            in_use: info.uordblks as u64,
            free: info.fordblks as u64,
        }
    }

    #[cfg(not(target_env = "gnu"))]
    pub fn now() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secure_submit_never_formats_its_secret() {
        let submit = SecureSubmit {
            generation_id: 3,
            capability: Capability::Polkit,
            action: "authenticate".into(),
            secret: b"hunter2".to_vec(),
        };

        // Whole rejected frames go through this `Debug` via the enum too.
        let rendered = format!("{:?}", RendererFrame::SecureSubmit(submit));

        assert!(rendered.contains("<7 bytes redacted>"), "the length is the only thing worth logging: {rendered}");
        assert!(!rendered.contains("104"), "a byte of the plaintext reached the formatter: {rendered}");
        assert!(rendered.contains("Polkit"), "everything that is not the secret still prints: {rendered}");
    }

    #[test]
    fn command_envelope_matches_idl_wire_format() {
        let wire = serde_json::json!({
            "params": {
                "generation_id": 4,
                "capability": "audio",
                "action": "set_volume",
                "arguments": [0.75],
                "expected_revision": 42
            },
            "id": 105
        });

        let envelope: CommandEnvelope = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(envelope.id, 105);
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "audio");
        assert_eq!(envelope.params.action, "set_volume");
        assert_eq!(envelope.params.expected_revision, 42);

        assert_eq!(serde_json::to_value(&envelope).unwrap(), wire);
    }

    #[test]
    fn state_snapshot_round_trips() {
        let snapshot = StateSnapshot {
            capability: "audio".to_string(),
            revision: 42,
            payload: serde_json::json!({"volume": 0.75}),
        };

        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(wire, serde_json::json!({ "capability": "audio", "revision": 42, "payload": {"volume": 0.75} }));
        let parsed: StateSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn state_snapshot_routes_by_capability_not_just_payload_shape() {
        // ADR-0029: `capability` tells two structurally identical payloads apart.
        let audio = StateSnapshot { capability: "audio".to_string(), revision: 1, payload: serde_json::json!({}) };
        let network = StateSnapshot { capability: "network".to_string(), revision: 1, payload: serde_json::json!({}) };
        assert_ne!(audio, network);
    }

    #[test]
    fn connection_handshake_round_trips() {
        let handshake = ConnectionHandshake { generation_id: 3 };
        let wire = serde_json::to_value(handshake).unwrap();
        assert_eq!(wire, serde_json::json!({ "generation_id": 3 }));

        let parsed: ConnectionHandshake = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, handshake);
    }

    #[test]
    fn supervisor_frame_state_snapshot_is_adjacently_tagged() {
        let frame = SupervisorFrame::StateSnapshot(StateSnapshot {
            capability: "audio".to_string(),
            revision: 1,
            payload: serde_json::json!({"volume": 0.5}),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "kind": "StateSnapshot", "data": { "capability": "audio", "revision": 1, "payload": {"volume": 0.5} } })
        );

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn supervisor_frame_reevaluate_is_a_bare_kind_with_no_data() {
        // The one payload-free frame: serde omits `data` for a unit variant, so the decoder must
        // accept this shape.
        let wire = serde_json::to_value(SupervisorFrame::Reevaluate).unwrap();
        assert_eq!(wire, serde_json::json!({ "kind": "Reevaluate" }));

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, SupervisorFrame::Reevaluate);
    }

    #[test]
    fn renderer_frame_command_is_adjacently_tagged() {
        let envelope = CommandEnvelope {
            params: CommandParams {
                generation_id: 4,
                capability: "audio".to_string(),
                action: "set_volume".to_string(),
                arguments: vec![serde_json::json!(0.75)],
                expected_revision: 42,
            },
            id: 105,
        };
        let frame = RendererFrame::Command(envelope.clone());
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire["kind"], "Command");
        assert_eq!(wire["data"]["id"], 105);

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        match parsed {
            RendererFrame::Command(parsed_envelope) => assert_eq!(parsed_envelope.id, envelope.id),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn renderer_frame_secure_submit_is_adjacently_tagged() {
        let frame = RendererFrame::SecureSubmit(SecureSubmit {
            generation_id: 4,
            capability: Capability::Polkit,
            action: "authenticate".to_string(),
            secret: b"hunter2".to_vec(),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "kind": "SecureSubmit",
                "data": { "generation_id": 4, "capability": "polkit", "action": "authenticate", "secret": [104, 117, 110, 116, 101, 114, 50] }
            })
        );

        let parsed: RendererFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    /// An unbounded channel carries bare `SecureSubmit` values, so the type must scrub `secret`
    /// on zeroize/drop.
    #[test]
    fn zeroizing_a_secure_submit_clears_its_secret() {
        let mut submit = SecureSubmit {
            generation_id: 4,
            capability: Capability::Polkit,
            action: "authenticate".to_string(),
            secret: b"hunter2".to_vec(),
        };

        submit.zeroize();

        assert_eq!(submit.secret, Vec::<u8>::new());
    }

    #[test]
    fn supervisor_frame_process_output_is_adjacently_tagged() {
        let frame = SupervisorFrame::ProcessOutput(ProcessOutputLine {
            id: 3,
            stream: ProcessStream::Stdout,
            line: "hello".to_string(),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "kind": "ProcessOutput", "data": { "id": 3, "stream": "Stdout", "line": "hello" } })
        );

        let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn supervisor_frame_process_exited_is_adjacently_tagged() {
        for code in [Some(3), None] {
            let frame = SupervisorFrame::ProcessExited(ProcessExited { id: 3, code });
            let wire = serde_json::to_value(&frame).unwrap();
            assert_eq!(wire, serde_json::json!({ "kind": "ProcessExited", "data": { "id": 3, "code": code } }));

            let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, frame);
        }
    }

    #[test]
    fn supervisor_frame_idle_event_is_adjacently_tagged() {
        for (state, wire_state) in [(IdleState::Idled, "idled"), (IdleState::Resumed, "resumed")] {
            let frame = SupervisorFrame::IdleEvent(IdleEvent { generation_id: 4, threshold_sec: 30, state });
            let wire = serde_json::to_value(&frame).unwrap();
            assert_eq!(
                wire,
                serde_json::json!({ "kind": "IdleEvent", "data": { "generation_id": 4, "threshold_sec": 30, "state": wire_state } })
            );

            let parsed: SupervisorFrame = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, frame);
        }
    }

    #[test]
    fn pam_outcome_round_trips_a_unit_and_a_data_carrying_variant() {
        for outcome in [PamOutcome::AuthFailed, PamOutcome::StartFailed("pam_start failed".to_string())] {
            let wire = serde_json::to_value(&outcome).unwrap();
            let parsed: PamOutcome = serde_json::from_value(wire).unwrap();
            assert_eq!(parsed, outcome);
        }
    }

    #[test]
    fn a_pam_message_response_never_formats_its_secret() {
        let rendered = format!("{:?}", PamMessage::Response { secret: b"hunter2".to_vec() });
        assert!(rendered.contains("<7 bytes redacted>"), "the length is the only thing worth logging: {rendered}");
        assert!(!rendered.contains("104"), "a byte of the plaintext reached the formatter: {rendered}");
    }

    #[test]
    fn zeroizing_a_pam_message_clears_only_a_response_s_secret() {
        let mut prompt = PamMessage::Prompt { text: "Password:".to_string(), echo: false };
        prompt.zeroize();
        assert_eq!(prompt, PamMessage::Prompt { text: "Password:".to_string(), echo: false }, "nothing to scrub here");

        let mut response = PamMessage::Response { secret: b"hunter2".to_vec() };
        response.zeroize();
        assert_eq!(response, PamMessage::Response { secret: Vec::new() });
    }
}

#[cfg(test)]
mod capability_tests {
    use super::Capability;

    #[test]
    fn every_entry_round_trips_through_its_name() {
        // One `roster!` list makes omission from `ALL` or `as_str` unrepresentable; this pins
        // `from_name` agreeing with the two wire-facing matches.
        assert_eq!(Capability::ALL.len(), 23, "a variant was added or removed; check every iterator over ALL");
        for capability in Capability::ALL {
            assert_eq!(Capability::from_name(capability.as_str()), Some(*capability));
        }
    }

    #[test]
    fn every_name_is_unique_so_two_variants_cannot_claim_one_lua_member() {
        let mut names: Vec<&str> = Capability::ALL.iter().map(|capability| capability.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two capabilities share a name; `from_name` would resolve only the first");
    }

    #[test]
    fn the_serde_spelling_is_the_same_string_as_as_str() {
        // `StateSnapshot::capability` is written from `as_str` and read by configs; if serde
        // ever disagreed, a payload would arrive under a name nothing is listening on.
        for capability in Capability::ALL {
            let json = serde_json::to_string(capability).unwrap();
            assert_eq!(json, format!("\"{}\"", capability.as_str()));
        }
    }

    #[test]
    fn a_name_that_is_not_on_the_roster_resolves_to_nothing() {
        // `process` is command-addressable, not a capability, and never starts. It sits one
        // letter from `processes`, which is a capability, and the two route through different
        // arms of `main.rs`; a config's `process.run` reaching the session-process controller
        // would spawn something nothing reaps per generation.
        assert_eq!(Capability::from_name("process"), None);
        assert_eq!(Capability::from_name("processes"), Some(Capability::Processes));
        assert_eq!(Capability::from_name("screens"), None);
        assert_eq!(Capability::from_name(""), None);
        assert_eq!(Capability::from_name("Audio"), None);
    }
}

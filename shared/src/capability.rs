use serde::{Deserialize, Serialize};

/// The snapshot-hydrated capability roster (ADR-0037; CONTEXT.md). Each [`Capability::as_str`]
/// name is both the Lua `mantle.<name>` member and command `capability` field, so
/// one spelling reaches one capability. Reading a name starts its Supervisor controller
/// (ADR-0070); it remains `nil` until the first `StateSnapshot`, so an unread name costs nothing.
/// A `secure_submit` naming `polkit` starts it too (ADR-0114).
///
/// An enum, not strings (ADR-0076): exhaustive matches make starting a controller and dispatching
/// its commands fail to compile for an unimplemented name. The `roster!` list generates
/// [`Capability::ALL`], [`Capability::as_str`], [`crate::action::check`] and the stub schemas from
/// each entry's state type and optional action enum, so a new variant missing from the Lua
/// namespace, stubs or schema check fails to compile instead of staying silently `nil`.
/// `renderer/build.rs` derives the Renderer's action names from the same schemas.
macro_rules! roster {
    (@schema) => { None };
    (@schema $action:ident) => { Some(schemars::schema_for!(crate::action::$action)) };
    (@check $arguments:ident $action:ident) => {
        Err(serde::de::Error::custom("it has no actions"))
    };
    (@check $arguments:ident $action:ident $enum:ident) => {
        crate::action::decode::<crate::action::$enum>($action, $arguments).map(drop)
    };
    ($($variant:ident => $name:literal, $($state:ident)::+, $($enum:ident,)? $blurb:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

        /// Whether `arguments` fit `capability`'s `action`. Only the shape: a check that needs live
        /// state, such as whether a profile or MAC exists, stays with the Supervisor.
        pub fn check(
            capability: Capability,
            action: &str,
            arguments: &[serde_json::Value],
        ) -> Result<(), serde_json::Error> {
            match capability {
                $(Capability::$variant => roster!(@check arguments action $($enum)?)),+
            }
        }

        /// Capability payload and action schemas used by stubs and build-time check samples.
        /// `None` means no actions.
        #[cfg(feature = "schema")]
        pub fn capability_schemas() -> Vec<(&'static str, schemars::Schema, Option<schemars::Schema>)> {
            vec![$(($name, schemars::schema_for!(crate::state::$($state)::+), roster!(@schema $($enum)?))),+]
        }
    };
}

roster! {
    AndroidConnect => "android_connect", android_connect::AndroidConnectState, AndroidConnectAction, "Android device synchronization through KDE Connect: battery, cellular signal, find my phone, clipboard sync, file sharing, and SFTP mount.",
    Audio => "audio", audio::AudioState, AudioAction, "PipeWire: output and input volume and mute, device lists, per-app streams and Bluetooth codecs.",
    Network => "network", network::NetworkState, NetworkAction, "NetworkManager: connectivity, Wi-Fi and wired state, scanned access points and join progress.",
    Secrets => "secrets", secrets::SecretsState, "Named Secret Service writes and their pending, stored or error status. Names are public metadata.",
    Bluetooth => "bluetooth", bluetooth::BluetoothState, BluetoothAction, "BlueZ: adapter power, discovery, connected, paired and discovered devices, and pairing prompts.",
    Tray => "tray", tray::TrayState, TrayAction, "StatusNotifierItem: registered tray items with artwork, status and menus.",
    Notifications => "notifications", notifications::NotificationsState, NotificationsAction, "The notification server: the newest 20 notifications and do-not-disturb.",
    Mpris => "mpris", mpris::MprisState, MprisAction, "MPRIS: media players with metadata, controls, TrackList and Playlists.",
    Sysinfo => "sysinfo", sysinfo::SysinfoState, SysinfoAction, "CPU, memory, swap, disks, GPU, network and temperatures. `nil` until `configure` sets intervals.",
    Keyboard => "keyboard", keyboard::KeyboardState, KeyboardAction, "Lock keys, the active layout and the keyboard backlight.",
    Privacy => "privacy", privacy::PrivacyState, "Apps using the camera, microphone or screen capture right now.",
    Updates => "updates", updates::UpdatesState, UpdatesAction, "Pending package upgrades (pacman, optionally AUR), install progress and whether a reboot is due.",
    Lock => "lock", lock::LockState, LockAction, "The session lock: whether it is held, authentication progress and the last failure.",
    Polkit => "polkit", polkit::PolkitState, PolkitAction, "The pending polkit authentication request, its progress and the last failure.",
    Battery => "battery", battery::BatteryState, "UPower: system battery charge, state and time estimates, plus peripheral batteries.",
    Clock => "clock", clock::ClockState, ClockAction, "Wall and monotonic clocks, pushed once a second until `configure` sets the interval.",
    Brightness => "brightness", brightness::BrightnessState, BrightnessAction, "The screen backlight percentage; `nil` without a backlight.",
    Workspaces => "workspaces", workspaces::WorkspacesState, WorkspacesAction, "Workspaces per output, special workspaces and the focused window.",
    Radio => "radio", radio::RadioState, RadioAction, "rfkill: soft and hard block state per radio kind; `nil` without `/dev/rfkill`.",
    Power => "power", power::PowerState, PowerAction, "Power profiles, mains or battery, and battery power draw.",
    Applications => "applications", applications::ApplicationsState, ApplicationsAction, "Installed desktop entries, indexed by window `app_id`.",
    Folders => "folders", folders::FoldersState, FoldersAction, "Live file listings of watched folders.",
    Storage => "storage", storage::StorageState, StorageAction, "Each `persistent_table` JSON file, keyed by absolute path.",
    Idle => "idle", idle::IdleState, "Idle inhibitors, plus threshold and inhibit methods.",
    Processes => "processes", processes::ProcessesState, ProcessesAction, "Programs declared with `session_process`: running state, start time and last exit.",
    Appearance => "appearance", appearance::AppearanceState, "Portal appearance settings: colour scheme, accent colour, contrast and reduced motion.",
    Windows => "windows", windows::WindowsState, WindowsAction, "Open toplevel windows with title, app ID, workspace, output and state flags.",
    AudioLevels => "audio_levels", audio_levels::AudioLevelsState, "Peak levels of the sinks and sources `mantle.audio:set_level_meters` names; `nil` before the first call this session.",
}

impl Capability {
    /// Resolves a Renderer-supplied wire string at the trust boundary, or returns `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|capability| capability.as_str() == name)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn every_entry_round_trips_through_its_name() {
        // One `roster!` list makes omission from `ALL` or `as_str` unrepresentable; this pins
        // `from_name` agreeing with the two wire-facing matches.
        for capability in Capability::ALL {
            assert_eq!(Capability::from_name(capability.as_str()), Some(*capability));
        }
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

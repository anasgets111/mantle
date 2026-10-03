use serde::{Deserialize, Serialize};

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
    Secrets => "secrets", "Named Secret Service writes and their pending, stored or error status. Names are public metadata.",
    Bluetooth => "bluetooth", "BlueZ: adapter power, discovery, connected, paired and discovered devices, and pairing prompts.",
    Tray => "tray", "StatusNotifierItem: registered tray items with artwork, status and menus.",
    Notifications => "notifications", "The notification server: the newest 20 notifications and do-not-disturb.",
    Mpris => "mpris", "MPRIS: media players with metadata, controls, TrackList and Playlists.",
    Sysinfo => "sysinfo", "CPU, memory, swap, disks, GPU, network and temperatures. `nil` until `configure` sets intervals.",
    Keyboard => "keyboard", "Lock keys, the active layout and the keyboard backlight.",
    Privacy => "privacy", "Apps using the camera, microphone or screen capture right now.",
    Updates => "updates", "Pending package upgrades (pacman, optionally AUR), install progress and whether a reboot is due.",
    Lock => "lock", "The session lock: whether it is held, authentication progress and the last failure.",
    Polkit => "polkit", "The pending polkit authentication request, its progress and the last failure.",
    Battery => "battery", "UPower: system battery charge, state and time estimates, plus peripheral batteries.",
    System => "system", "Wall and monotonic clocks, pushed once a second until `configure` sets the interval.",
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
    /// The action names, each a method on `mantle.<name>`; empty for a read-only capability. The
    /// Renderer refuses any other name at call time; the Supervisor still validates arguments.
    /// `supervisor/src/stubs.rs` pins each list to its serde action enum.
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
                "set_sink_channel_volume",
                "set_source_channel_volume",
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
            Capability::Mpris => &[
                "control",
                "seek",
                "seek_relative",
                "raise",
                "quit",
                "open_uri",
                "set_volume",
                "set_loop_status",
                "set_shuffle",
                "set_rate",
                "track_list_add_track",
                "track_list_remove_track",
                "track_list_go_to",
                "playlists_get",
                "playlists_activate",
            ],
            Capability::Network => &[
                "set_networking_enabled",
                "set_wifi_enabled",
                "set_ethernet_enabled",
                "scan",
                "scan_device",
                "connect",
                "connect_device",
                "cancel_connect",
                "abort_connect",
                "forget",
                "disconnect_wifi",
                "disconnect_wifi_device",
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
            Capability::Tray => {
                &["activate", "context_menu", "secondary_activate", "scroll", "activate_menu_item", "menu_will_show"]
            }
            Capability::Updates => &["check", "configure", "install"],
            Capability::Workspaces => &["focus", "toggle_special"],
            Capability::Windows => {
                &["focus", "close", "set_fullscreen", "set_minimized", "set_maximized", "move_to_workspace"]
            }
            Capability::System => &["configure"],
            Capability::Battery | Capability::Idle | Capability::Privacy | Capability::Secrets => &[],
        }
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
        assert_eq!(Capability::ALL.len(), 24, "a variant was added or removed; check every iterator over ALL");
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

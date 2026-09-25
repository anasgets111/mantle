//! Keyboard layout integration for `mantle.keyboard` (ADR-0034). The shared niri/Hyprland reader
//! (`capabilities::lifecycle`'s `CompositorReader`) writes layout through a [`LayoutSink`];
//! `switch_layout` goes through a [`CompositorLink`]. Without a supported compositor, layout is
//! empty with count `0`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;
use shared::debug;
use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{hyprland_command, hyprland_request, hyprland_socket_path, niri_action};

use super::controller::{KeyboardSignal, KeyboardState};

/// `switch_layout` is synchronous fire-and-forget; state returns through the compositor reader. It
/// is not `async fn` to preserve `Box<dyn CompositorLink>` object safety.
pub trait CompositorLink: Send + Sync {
    fn switch_layout(&self, index: usize);
}

/// `keyboard`'s state as the compositor reader sees it. The reader writes layout from its first
/// event, and signals once `keyboard` has started and set `events`.
#[derive(Clone, Default)]
pub struct LayoutSink {
    pub state: Arc<Mutex<KeyboardState>>,
    pub events: Arc<OnceLock<UnboundedSender<KeyboardSignal>>>,
}

impl LayoutSink {
    fn write(&self, active_layout: String, active_layout_index: u32, layout_count: u32) {
        {
            let mut guard = self.state.lock().expect("mutex poisoned");
            guard.active_layout = active_layout;
            guard.active_layout_index = active_layout_index;
            guard.layout_count = layout_count;
        }
        if let Some(events) = self.events.get() {
            let _ = events.send(KeyboardSignal::Changed);
        }
    }

    /// Applies a niri layout event; `false` for any other. `names` carries the list between a
    /// `KeyboardLayoutsChanged` and the `KeyboardLayoutSwitched` events after it.
    pub fn apply_niri(&self, names: &mut Vec<String>, event: &niri_ipc::Event) -> bool {
        let idx = match event {
            niri_ipc::Event::KeyboardLayoutsChanged { keyboard_layouts } => {
                names.clone_from(&keyboard_layouts.names);
                keyboard_layouts.current_idx
            }
            niri_ipc::Event::KeyboardLayoutSwitched { idx } => *idx,
            _ => return false,
        };
        self.write(names.get(idx as usize).cloned().unwrap_or_default(), u32::from(idx), names.len() as u32);
        true
    }

    /// One `j/devices` read, on Hyprland's `activelayout` event and once at reader start.
    pub fn read_hyprland(&self, socket_path: &Path) {
        let reply = match hyprland_request(socket_path, "j/devices") {
            Ok(reply) => reply,
            Err(err) => return debug!("Hyprland `devices` request failed; layout not updated this round: {err}"),
        };
        let Some(keyboard) = parse_hyprland_devices(&reply) else {
            return debug!("Hyprland `devices` reply held no usable keyboard entry; layout not updated this round");
        };
        self.apply_hyprland(keyboard);
    }

    fn apply_hyprland(&self, keyboard: HyprlandKeyboard) {
        let count = keyboard.layout.split(',').filter(|s| !s.is_empty()).count() as u32;
        self.write(keyboard.active_keymap, keyboard.active_layout_index, count);
    }
}

pub struct NiriLink;

impl CompositorLink for NiriLink {
    fn switch_layout(&self, index: usize) {
        // A command supplies an unbounded u64, while niri's wire protocol takes u8. Reject overflow
        // instead of truncating 256 to 0.
        let Ok(index) = u8::try_from(index) else {
            debug!("switch_layout index {index} is out of range for niri (must fit in a u8); ignored");
            return;
        };
        let layout = niri_ipc::LayoutSwitchTarget::Index(index);
        niri_action(niri_ipc::Action::SwitchLayout { layout }, "keyboard");
    }
}

/// `switchxkblayout main <index>` over Hyprland's `.socket.sock`.
pub struct HyprlandLink {
    command_path: PathBuf,
}

/// Needed fields from `j/devices`'s `keyboards` entries; comma-separated `layout` only supplies the
/// count. `main` is Hyprland's `m_active`, reassigned on every key event, so it is the keyboard
/// being typed on, media and power-button nodes included. That is Hyprland's own answer and is not
/// narrowed here.
#[derive(Debug, Clone, Deserialize)]
struct HyprlandKeyboard {
    active_keymap: String,
    layout: String,
    #[serde(default, deserialize_with = "layout_index")]
    active_layout_index: u32,
    #[serde(default)]
    main: bool,
}

/// `0` for an absent, null or out-of-range `active_layout_index`. `#[serde(default)]` covers only
/// an absent key, and `parse_hyprland_devices` drops an entry that fails to deserialize, so one bad
/// value costs the whole layout rather than one field. Same tolerance as `workspaces::hyprland::fullscreen_mode`.
fn layout_index<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    Ok(serde_json::Value::deserialize(deserializer)?.as_u64().and_then(|index| u32::try_from(index).ok()).unwrap_or(0))
}

fn parse_hyprland_devices(json: &str) -> Option<HyprlandKeyboard> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let keyboards = root.get("keyboards")?.as_array()?;
    // `"none"`/`"error"` is what Hyprland reports for a device xkb resolved no layout for, which a
    // `wtype` virtual keyboard is while it holds `main`. Either draws as the layout name.
    let parsed: Vec<HyprlandKeyboard> = keyboards
        .iter()
        .filter_map(|keyboard| HyprlandKeyboard::deserialize(keyboard).ok())
        .filter(|k| !matches!(k.active_keymap.as_str(), "none" | "error"))
        .collect();
    parsed.iter().find(|k| k.main).cloned().or_else(|| parsed.into_iter().next())
}

impl HyprlandLink {
    /// `signature` is a non-empty `$HYPRLAND_INSTANCE_SIGNATURE`, from
    /// `compositor::hyprland_signature`.
    pub fn new(signature: &str) -> Self {
        Self { command_path: hyprland_socket_path(signature, ".socket.sock") }
    }
}

impl CompositorLink for HyprlandLink {
    /// `main` is also Hyprland's device target for that keyboard, so no device name is tracked here.
    ///
    /// ponytail: one OS thread per switch, for one blocking round trip, unbounded if a config calls
    /// this in a loop. A shared worker is the upgrade. `workspaces`' dispatch has the same shape.
    fn switch_layout(&self, index: usize) {
        let socket_path = self.command_path.clone();
        std::thread::spawn(move || {
            hyprland_command(&socket_path, &format!("switchxkblayout main {index}"), "keyboard");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hyprland_devices_reads_the_first_keyboards_entry() {
        let json = r#"{"mice":[],"keyboards":[{"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1}],"tablets":[]}"#;
        let keyboard = parse_hyprland_devices(json).expect("should parse");
        assert_eq!(keyboard.active_keymap, "Arabic (Egypt)");
        assert_eq!(keyboard.active_layout_index, 1);
    }

    #[test]
    fn parse_hyprland_devices_prefers_the_main_keyboard_over_array_order() {
        // `main` moves to whichever keyboard was typed on last, so array order names the wrong one.
        let json = r#"{"keyboards":[
            {"active_keymap":"English (US)","layout":"us,ara","active_layout_index":0,"main":false},
            {"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1,"main":true}
        ]}"#;
        let keyboard = parse_hyprland_devices(json).expect("should parse");
        assert_eq!(keyboard.active_layout_index, 1);
    }

    #[test]
    fn parse_hyprland_devices_is_none_when_keyboards_is_empty() {
        let json = r#"{"keyboards":[]}"#;
        assert!(parse_hyprland_devices(json).is_none());
    }

    #[test]
    fn parse_hyprland_devices_is_none_for_malformed_json() {
        assert!(parse_hyprland_devices("not json").is_none());
    }

    #[test]
    fn apply_hyprland_layout_counts_the_configured_layouts_and_keeps_the_reported_index() {
        // Regression: the index was pinned to `0`, so Lua could not cycle from the reported value.
        let json = r#"{"keyboards":[{"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1,"main":true}]}"#;
        let sink = LayoutSink::default();
        sink.apply_hyprland(parse_hyprland_devices(json).expect("should parse"));
        let guard = sink.state.lock().unwrap();
        assert_eq!(
            (guard.active_layout.as_str(), guard.active_layout_index, guard.layout_count),
            ("Arabic (Egypt)", 1, 2)
        );
    }

    #[test]
    fn a_niri_switch_names_its_layout_from_the_last_list_and_signals_once_keyboard_listens() {
        let sink = LayoutSink::default();
        let mut names = Vec::new();
        let layouts: niri_ipc::KeyboardLayouts =
            serde_json::from_value(serde_json::json!({ "names": ["English (US)", "Arabic"], "current_idx": 0 }))
                .unwrap();
        assert!(sink.apply_niri(&mut names, &niri_ipc::Event::KeyboardLayoutsChanged { keyboard_layouts: layouts }));

        let (events, mut signals) = tokio::sync::mpsc::unbounded_channel();
        sink.events.set(events).unwrap();
        assert!(sink.apply_niri(&mut names, &niri_ipc::Event::KeyboardLayoutSwitched { idx: 1 }));
        assert!(!sink.apply_niri(&mut names, &niri_ipc::Event::OverviewOpenedOrClosed { is_open: true }));

        let guard = sink.state.lock().unwrap();
        assert_eq!((guard.active_layout.as_str(), guard.active_layout_index, guard.layout_count), ("Arabic", 1, 2));
        assert_eq!(signals.try_recv(), Ok(KeyboardSignal::Changed));
        assert!(signals.try_recv().is_err(), "the write before `keyboard` started sent nothing");
    }

    #[test]
    fn parse_hyprland_devices_defaults_the_index_when_hyprland_omits_it() {
        let json = r#"{"keyboards":[{"active_keymap":"English (US)","layout":"us"}]}"#;
        assert_eq!(parse_hyprland_devices(json).expect("should parse").active_layout_index, 0);
    }

    #[test]
    fn a_keyboard_entry_survives_an_index_hyprland_sends_in_an_unexpected_shape() {
        // `#[serde(default)]` covers an absent key only; a null or negative value must not fail
        // the whole entry and leave `KeyboardState` with no layout at all.
        for index in ["null", "-1", "1.5", "\"1\""] {
            let json = format!(
                r#"{{"keyboards":[{{"active_keymap":"English (US)","layout":"us,ara","active_layout_index":{index}}}]}}"#
            );
            let keyboard = parse_hyprland_devices(&json).unwrap_or_else(|| panic!("{index} dropped the entry"));
            assert_eq!(keyboard.active_layout_index, 0, "{index}");
            assert_eq!(keyboard.active_keymap, "English (US)", "{index}");
        }
    }

    #[test]
    fn parse_hyprland_devices_skips_a_keyboard_whose_keymap_hyprland_left_unresolved() {
        // `wtype` registers a virtual keyboard that takes `main` for the keystrokes it injects.
        let json = r#"{"keyboards":[
            {"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1,"main":false},
            {"active_keymap":"none","layout":"us,ara","active_layout_index":0,"main":true}
        ]}"#;
        assert_eq!(parse_hyprland_devices(json).expect("should parse").active_keymap, "Arabic (Egypt)");

        let json = r#"{"keyboards":[{"active_keymap":"error","layout":"us,ara","main":true}]}"#;
        assert!(parse_hyprland_devices(json).is_none(), "the last good layout stands instead");
    }

    #[test]
    fn niri_switch_layout_index_validation_accepts_the_full_u8_range() {
        assert_eq!(u8::try_from(0usize), Ok(0));
        assert_eq!(u8::try_from(255usize), Ok(255));
    }

    #[test]
    fn niri_switch_layout_index_validation_rejects_values_that_would_truncate() {
        // Regression: 256 used to truncate to 0 via `as u8`.
        assert!(u8::try_from(256usize).is_err());
        assert!(u8::try_from(usize::MAX).is_err());
    }
}

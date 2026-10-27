//! Hyprland over its two instance-directory sockets (ADR-0118).
//! Live against 0.56.2; the fixtures below still carry the shapes it was written to.
//!
//! 0.56 replaced the command socket's plain-text language with Lua: `dispatch <what>` is now
//! sugar for `return hl.dispatch(<what>)`, so the pre-0.56 `dispatch workspace 2` reaches the
//! parser as `hl.dispatch(workspace 2)` and dies on the space. Reads are untouched -- `j/` is
//! still the legacy path -- so a stale write syntax leaves a strip that tracks the compositor
//! perfectly and cannot drive it. Writes here target 0.56 and no earlier.
//!
//! Hyprland has no state event stream. `.socket2.sock` sends `event>>payload` lines; `.socket.sock`
//! returns the `hyprctl -j` JSON. On a workspace/monitor/window event, re-read
//! `workspaces`/`monitors`/`clients`/`activewindow`, reduce, and publish, once per burst: every
//! line already buffered joins the one that woke the reader (a window opening sends four or
//! five). A title change alone patches the last read from `windowtitlev2` without a request.
//!
//! How Hyprland's model lands on `WorkspaceRow`:
//!
//! - `id` is Hyprland's workspace id in decimal, opaque to configs. A positive id is the workspace's
//!   number, also `number`, and goes into [`focus_command`] as is, so focusing a new number creates
//!   it. Named workspaces have negative ids and no `number`; Hyprland reads a negative number as a
//!   relative move, so [`named_selector`] swaps in `"name:<name>"`. `name` is set only when it
//!   differs from the number.
//! - Per-output active is `activeWorkspace`; focused is the active workspace of the monitor with
//!   `focused: true`, matching niri's `is_active`/`is_focused` split.
//! - `populated` is `windows`; `app_id` is the `class` of the lowest `focusHistoryID` (`0` focused,
//!   higher older), giving ADR-0117's "focused, else first" a real order.
//! - Focused window is `activewindow`, `{}` while a layer surface has focus. Using
//!   `clients[].focusHistoryID == 0` would incorrectly name the last toplevel.
//! - Specials ([`is_special`]) leave the rows and become `special` (ADR-0119),
//!   shown on the monitor naming them in `specialWorkspace`; toggle with
//!   [`toggle_special_command`]. A shown special does not replace
//!   the focused monitor's regular workspace.
//! - `active_client.fullscreen` is active-window `fullscreen`: int since Hyprland 0.42 (`0` none, `1`
//!   maximized, `2` fullscreen), bool before; only the real value counts.
//!
//! `spawn_reader` also folds every `clients` entry into a `windows` row, from the same reads.

use std::collections::HashSet;
use std::io::BufReader;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Compositor, connect, toggle_needed};
use crate::capabilities::keyboard::controller::LayoutSink;
use crate::capabilities::windows::controller::StatePublisher as WindowsPublisher;
use crate::capabilities::workspaces::controller::StatePublisher;
use shared::{Capability, debug, error};

mod commands;
mod wire;

use commands::*;
use wire::*;

pub(super) fn read<T: for<'de> Deserialize<'de>>(socket_path: &Path, name: &str) -> Option<T> {
    let reply = match hyprland_request(socket_path, &format!("j/{name}")) {
        Ok(reply) => reply,
        Err(err) => {
            debug!("Hyprland `{name}` request failed: {err}");
            return None;
        }
    };
    match serde_json::from_str(&reply) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            debug!("Hyprland `{name}` reply did not parse: {err}");
            None
        }
    }
}

fn read_state(socket_path: &Path) -> Option<State> {
    let workspaces: Vec<HyprlandWorkspace> = read(socket_path, "workspaces")?;
    let monitors: Vec<HyprlandMonitor> = read(socket_path, "monitors")?;
    let clients: Vec<HyprlandClient> = read(socket_path, "clients")?;
    let active = match hyprland_request(socket_path, "j/activewindow") {
        Ok(reply) => reply,
        Err(err) => {
            debug!("Hyprland `activewindow` request failed; skipping this update: {err}");
            return None;
        }
    };
    let active_client = parse_active_client(&active);
    Some((
        workspace_rows(&workspaces, &monitors, &clients),
        active_client.as_ref().map(to_focused_window),
        special_list(&workspaces, &monitors, &clients),
        window_rows(&clients, &monitors, active_client.as_ref().map(|client| client.address.as_str())),
    ))
}

/// `$HYPRLAND_INSTANCE_SIGNATURE`, or `None` when it is unset or empty.
///
/// [`super::detect_compositor`] probes with `var_os`, which accepts bytes `var` rejects, so a session
/// detected as Hyprland can still have no usable signature. An empty one builds
/// `$XDG_RUNTIME_DIR/hypr//.socket.sock`, which resolves and never connects.
fn hyprland_signature() -> Option<String> {
    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|signature| !signature.is_empty())
}

/// A socket in Hyprland's per-instance directory,
/// `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`. `"socket2.sock"` pushes
/// newline-terminated `event>>payload` lines; `"socket.sock"` answers one plain-text command per
/// connection (`j/workspaces` for JSON, `dispatch ...` for a write).
fn hyprland_socket_path(signature: &str, name: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    hyprland_socket_path_in(&runtime_dir, signature, name)
}

/// The `join` half of [`hyprland_socket_path`], split from `$XDG_RUNTIME_DIR` lookup so tests avoid
/// `set_var`. `setenv` rewrites process-wide `environ` and races every concurrent `getenv` in the
/// test binary, even for unrelated variables.
fn hyprland_socket_path_in(runtime_dir: &str, signature: &str, name: &str) -> PathBuf {
    PathBuf::from(runtime_dir).join("hypr").join(signature).join(name)
}

/// One `.socket.sock` command. Hyprland answers once per connection and closes it: `j/<what>`
/// returns the `hyprctl -j` JSON, a write returns `ok` or the reason it refused (ADR-0118).
pub(super) fn hyprland_request(socket_path: &Path, command: &str) -> std::io::Result<String> {
    let mut stream = connect(socket_path)?;
    stream.write_all(command.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

/// A write and its reply check, blocking; `capability` prefixes the log line. An unread reply
/// makes a refusal silent: a bad device, an index out of range.
fn hyprland_command(socket_path: &Path, command: &str, capability: &str) {
    match hyprland_request(socket_path, command) {
        Ok(reply) if reply.trim() == "ok" => {
            debug!(2; "{capability}: Hyprland command `{command}` succeeded");
        }
        Ok(reply) => debug!("{capability}: Hyprland refused `{command}`: {}", reply.trim()),
        Err(err) => debug!("{capability}: Hyprland `{command}` request failed: {err}"),
    }
}

impl LayoutSink {
    /// One `j/devices` read, on Hyprland's `activelayout` event and once at reader start.
    fn read_hyprland(&self, socket_path: &Path) {
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
/// value costs the whole layout rather than one field. Same tolerance as `fullscreen_mode`.
fn layout_index<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    Ok(serde_json::Value::deserialize(deserializer)?.as_u64().and_then(|index| u32::try_from(index).ok()).unwrap_or(0))
}

fn parse_hyprland_devices(json: &str) -> Option<HyprlandKeyboard> {
    let root: serde_json::Value =
        serde_json::from_str(json).inspect_err(|err| debug!("Hyprland `devices` reply did not parse: {err}")).ok()?;
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

/// Hyprland over its two instance-directory sockets.
pub struct Hyprland;

impl Compositor for Hyprland {
    /// Connects to the event socket before the first state read, so an intervening change remains a
    /// line to process. One OS thread then re-reads once per burst with a trigger until socket end or
    /// no listener. Also drives `mantle.windows` from the same reads, and `keyboard`'s layout.
    fn spawn_reader(
        &self,
        mut publisher: StatePublisher,
        mut windows_publisher: WindowsPublisher,
        keyboard: LayoutSink,
    ) {
        let Some(signature) = hyprland_signature() else {
            return debug!(
                "HYPRLAND_INSTANCE_SIGNATURE is unset or empty; workspace and window reporting disabled for this run"
            );
        };
        let events_path = hyprland_socket_path(&signature, ".socket2.sock");
        let command_path = hyprland_socket_path(&signature, ".socket.sock");
        let stream = match UnixStream::connect(&events_path) {
            Ok(stream) => stream,
            Err(err) => {
                error!(
                    "failed to connect to Hyprland's event socket at {}; workspace and window reporting disabled for this run: {err}",
                    events_path.display()
                );
                return;
            }
        };

        macro_rules! publish {
            ($rows:expr, $focused:expr, $special:expr, $windows:expr) => {
                let workspaces_alive = publisher.publish($rows, $focused, $special, None);
                let windows_alive = windows_publisher.publish($windows);
                if !workspaces_alive && !windows_alive {
                    return;
                }
            };
        }

        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            keyboard.read_hyprland(&command_path);
            let mut last = read_state(&command_path);
            let mut urgent = HashSet::new();
            loop {
                if let Some((rows, focused, special, windows)) = &last {
                    publish!(rows, focused.as_ref(), Some(special.as_slice()), windows.clone());
                }
                let burst = match read_burst(&mut reader) {
                    Ok(Some(burst)) => burst,
                    Ok(None) => break,
                    Err(_) => {
                        error!("Hyprland event socket read failed; workspaces and windows will no longer update");
                        return;
                    }
                };
                if burst.layout {
                    keyboard.read_hyprland(&command_path);
                }
                urgent.extend(burst.urgent);
                // Urgency only changes on a read: an `urgent` event, focus and close all trigger one.
                if burst.reread {
                    let Some(mut state) = read_state(&command_path) else { continue };
                    mark_urgent(&mut state, &mut urgent);
                    last = Some(state);
                } else if let Some(state) = &mut last {
                    burst.titles.iter().for_each(|(address, title)| patch_title(state, address, title));
                }
            }
            error!("Hyprland event socket closed; workspaces and windows will no longer update");
        });
    }

    /// `workspaces:focus(id)`; a new number creates the empty slot a strip can pad into.
    fn focus_workspace(&self, id: &str) {
        dispatch_to_workspace(id, Capability::Workspaces.as_str(), focus_command);
    }

    fn focus_window(&self, id: &str) {
        window_dispatch("hl.dsp.focus", id, None);
    }

    fn close_window(&self, id: &str) {
        window_dispatch("hl.dsp.window.close", id, None);
    }

    /// Hyprland only toggles, so a write is sent only on a real change.
    fn set_fullscreen(&self, id: &str, fullscreen: bool, current: Option<bool>) {
        if toggle_needed(current, fullscreen) {
            window_dispatch("hl.dsp.window.fullscreen", id, None);
        }
    }

    /// Same dispatcher as fullscreen, with `mode = "maximized"`.
    fn set_maximized(&self, id: &str, maximized: bool, current: Option<bool>) {
        if toggle_needed(current, maximized) {
            window_dispatch("hl.dsp.window.fullscreen", id, Some("maximized"));
        }
    }

    fn move_window(&self, id: &str, workspace_id: &str) {
        let id = id.to_string();
        dispatch_to_workspace(workspace_id, Capability::Windows.as_str(), move |selector| {
            move_window_to_workspace_command(&id, selector)
        });
    }

    /// `workspaces:toggle_special(name)`.
    fn toggle_special(&self, name: &str) {
        dispatch(toggle_special_command(name), Capability::Workspaces.as_str());
    }

    /// `switchxkblayout main <index>` over `.socket.sock`. `main` is also Hyprland's device target
    /// for that keyboard, so no device name is tracked here.
    fn switch_layout(&self, index: usize) {
        on_command_socket(move |path| hyprland_command(path, &format!("switchxkblayout main {index}"), "keyboard"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyprland_socket_path_joins_runtime_dir_hypr_signature_and_name() {
        assert_eq!(
            hyprland_socket_path_in("/run/user/1000", "abc123", "socket2.sock"),
            PathBuf::from("/run/user/1000/hypr/abc123/socket2.sock")
        );
    }

    #[test]
    fn parse_hyprland_devices_reads_the_main_keyboard_and_gives_up_on_nothing_usable() {
        // `main` moves to whichever keyboard was typed on last, so array order names the wrong one.
        let two = r#"{"keyboards":[
            {"active_keymap":"English (US)","layout":"us,ara","active_layout_index":0,"main":false},
            {"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1,"main":true}
        ]}"#;
        let one = r#"{"mice":[],"keyboards":[{"active_keymap":"Arabic (Egypt)","layout":"us,ara","active_layout_index":1}],"tablets":[]}"#;
        let omitted = r#"{"keyboards":[{"active_keymap":"English (US)","layout":"us"}]}"#;
        for (case, json, want) in [
            ("first entry", one, Some(("Arabic (Egypt)", 1))),
            ("main over array order", two, Some(("Arabic (Egypt)", 1))),
            ("index omitted defaults to 0", omitted, Some(("English (US)", 0))),
            ("empty keyboards", r#"{"keyboards":[]}"#, None),
            ("malformed json", "not json", None),
        ] {
            let got = parse_hyprland_devices(json);
            let got = got.as_ref().map(|k| (k.active_keymap.as_str(), k.active_layout_index));
            assert_eq!(got, want, "{case}");
        }
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
}

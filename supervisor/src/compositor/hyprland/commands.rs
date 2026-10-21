//! Lua dispatcher strings for `.socket.sock` writes (0.56+) and the dispatch helpers.

use std::path::Path;

use super::{HyprlandWorkspace, hyprland_command, hyprland_signature, hyprland_socket_path, read};
use crate::compositor::run_in_order;
use shared::{Capability, debug, warn};

/// Runs `job` on the shared action thread with `.socket.sock`'s path.
pub(super) fn on_command_socket(job: impl FnOnce(&Path) + Send + 'static) {
    let Some(signature) = hyprland_signature() else {
        return debug!("HYPRLAND_INSTANCE_SIGNATURE is unset or empty; Hyprland write ignored");
    };
    run_in_order(move || job(&hyprland_socket_path(&signature, ".socket.sock")));
}

pub(super) fn dispatch(what: String, capability: &'static str) {
    on_command_socket(move |socket_path| hyprland_command(socket_path, &format!("dispatch {what}"), capability));
}

/// Hyprland reads a negative number as a relative move, so a named workspace goes by `name:`.
pub(super) fn named_selector(name: &str) -> String {
    format!("\"name:{}\"", lua_escape(name))
}

/// Sends `build(selector)`: a number as is, a named workspace's name from a fresh `j/workspaces`.
pub(super) fn dispatch_to_workspace(
    id: &str,
    capability: &'static str,
    build: impl FnOnce(&str) -> String + Send + 'static,
) {
    let Ok(parsed) = id.parse::<i64>() else {
        warn!("{id:?} is not a Hyprland workspace id; ignored");
        return;
    };
    if parsed > 0 {
        return dispatch(build(id), capability);
    }
    on_command_socket(move |socket_path| {
        let Some(workspaces) = read::<Vec<HyprlandWorkspace>>(socket_path, "workspaces") else { return };
        let Some(workspace) = workspaces.iter().find(|workspace| workspace.id == parsed) else {
            return warn!("no Hyprland workspace has id {parsed}; ignored");
        };
        hyprland_command(socket_path, &format!("dispatch {}", build(&named_selector(&workspace.name))), capability);
    });
}

/// The table form, not `hl.dsp.focus(N)`: `focus` takes one table and reads the field, the same
/// call that moves focus by `direction`. Parenthesised because Hyprland appends a "syntax might need
/// to be updated" note to errors from a command with no `(` in it.
pub(super) fn focus_command(selector: &str) -> String {
    format!("hl.dsp.focus({{ workspace = {selector} }})")
}

/// `dispatcher` is `hl.dsp.focus` or an `hl.dsp.window.*` name; every window write shares this
/// table shape.
pub(super) fn window_dispatch_command(dispatcher: &str, id: &str, mode: Option<&str>) -> String {
    let id = lua_escape(id);
    let mode = mode.map(|value| format!(r#", mode = "{value}""#)).unwrap_or_default();
    format!(r#"{dispatcher}({{ window = "address:{id}"{mode} }})"#)
}

pub(super) fn window_dispatch(dispatcher: &str, id: &str, mode: Option<&str>) {
    dispatch(window_dispatch_command(dispatcher, id, mode), Capability::Windows.as_str());
}

pub(super) fn move_window_to_workspace_command(id: &str, selector: &str) -> String {
    let id = lua_escape(id);
    format!(r#"hl.dsp.window.move({{ window = "address:{id}", workspace = {selector}, follow = false }})"#)
}

/// Strip `special:`; the unnamed `special` passes the empty string, which the dispatcher reads as
/// the unnamed one. The dispatcher adds the prefix, so a full name would become
/// `special:special:term`.
pub(super) fn toggle_special_command(name: &str) -> String {
    let argument = name.strip_prefix("special:").unwrap_or(if name == "special" { "" } else { name });
    format!("hl.dsp.workspace.toggle_special(\"{}\")", lua_escape(argument))
}

/// The command is Lua source now, and a quote in a name -- from a config or the compositor's own
/// list -- would close the string and run the rest. `\\ddd` is padded to three digits: Lua reads up
/// to three, so an unpadded escape swallows a following digit.
pub(super) fn lua_escape(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            c if c.is_ascii_control() => format!("\\{:03}", c as u32),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.56 reads the command as Lua, so both writes are checked as source. Captured from a live
    /// 0.56.2 socket, which answers `ok` to each of these and a parse error to what they replaced.
    #[test]
    fn both_writes_are_the_lua_dispatchers_0_56_accepts() {
        assert_eq!(focus_command("3"), "hl.dsp.focus({ workspace = 3 })");
        assert_eq!(focus_command(r#""name:chat""#), r#"hl.dsp.focus({ workspace = "name:chat" })"#);
        assert_eq!(toggle_special_command("special:term"), r#"hl.dsp.workspace.toggle_special("term")"#);
        assert_eq!(
            toggle_special_command("special"),
            r#"hl.dsp.workspace.toggle_special("")"#,
            "the unnamed special is the empty argument, not a missing one"
        );
        assert_eq!(
            toggle_special_command("term"),
            r#"hl.dsp.workspace.toggle_special("term")"#,
            "a config passing the short name is not punished"
        );
        assert_eq!(
            toggle_special_command(r#"special:a") hl.dsp.exit(--"#),
            r#"hl.dsp.workspace.toggle_special("a\") hl.dsp.exit(--")"#,
            "a quote in a name stays inside the string"
        );
        assert_eq!(
            toggle_special_command("special:a\n5"),
            r#"hl.dsp.workspace.toggle_special("a\0105")"#,
            "a control byte is escaped, padded so the digit after it stays a digit"
        );
        assert_eq!(
            toggle_special_command("special:é"),
            r#"hl.dsp.workspace.toggle_special("é")"#,
            "a non-ASCII name reaches the socket as itself, not as its bytes read as codepoints"
        );
    }

    #[test]
    fn a_named_workspace_is_addressed_by_its_escaped_name() {
        assert_eq!(named_selector("chat"), r#""name:chat""#);
        assert_eq!(named_selector(r#"a"b"#), r#""name:a\"b""#);
    }

    /// `focuswindow`/`closewindow` as top-level dispatchers do not exist ("attempt to call a nil
    /// value"); windows go through `hl.dsp.focus`/`hl.dsp.window.*` like every other 0.56 write.
    #[test]
    fn window_writes_are_the_lua_dispatchers_0_56_actually_has() {
        assert_eq!(
            window_dispatch_command("hl.dsp.focus", "0xa11ce", None),
            r#"hl.dsp.focus({ window = "address:0xa11ce" })"#
        );
        assert_eq!(
            window_dispatch_command("hl.dsp.window.close", "0xa11ce", None),
            r#"hl.dsp.window.close({ window = "address:0xa11ce" })"#
        );
        assert_eq!(
            window_dispatch_command("hl.dsp.window.fullscreen", "0xa11ce", None),
            r#"hl.dsp.window.fullscreen({ window = "address:0xa11ce" })"#
        );
        assert_eq!(
            window_dispatch_command("hl.dsp.window.fullscreen", "0xa11ce", Some("maximized")),
            r#"hl.dsp.window.fullscreen({ window = "address:0xa11ce", mode = "maximized" })"#
        );
        assert_eq!(
            move_window_to_workspace_command("0x55d1c0a3b2c0", "3"),
            r#"hl.dsp.window.move({ window = "address:0x55d1c0a3b2c0", workspace = 3, follow = false })"#
        );
        assert_eq!(
            move_window_to_workspace_command(r#"0x"bad"#, "1"),
            r#"hl.dsp.window.move({ window = "address:0x\"bad", workspace = 1, follow = false })"#
        );
    }
}

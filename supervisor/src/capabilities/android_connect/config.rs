//! Disk persistence for KDE Connect's INI clipboard configuration and `runcommand.json`.

use std::io;
use std::path::{Path, PathBuf};

use shared::state::android_connect::AndroidCommand;

/// The daemon's config directory, `$XDG_CONFIG_HOME/kdeconnect`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    shared::xdg_dir("XDG_CONFIG_HOME", ".config").map(|dir| dir.join("kdeconnect"))
}

pub(super) const COMMANDS_FILE: &str = "runcommand.json";

/// The offered commands: none when the file is missing, an error when it does not parse.
// ponytail: the list is reread when a session starts and after our own writes, so a hand edit or
// the daemon merging commands from another desktop shows after a daemon restart; a file watch
// would fix it.
pub(super) fn read_commands(path: &Path) -> io::Result<Vec<AndroidCommand>> {
    match std::fs::read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        read => serde_json::from_slice(&read?).map_err(io::Error::other),
    }
}

/// The device's clipboard config, a KDE INI file the daemon rereads on every clipboard change.
pub(super) fn clipboard_config(dir: &Path, id: &str) -> Option<PathBuf> {
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

pub(super) fn read_auto_share(dir: Option<&Path>, id: &str) -> bool {
    let path = dir.and_then(|dir| clipboard_config(dir, id));
    path.and_then(|p| std::fs::read_to_string(p).ok()).is_none_or(|text| auto_share(&text))
}

/// `text` with one `autoShare` line set to `enabled`, under `[General]`, every other line kept.
pub(super) fn with_auto_share(text: &str, enabled: bool) -> String {
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

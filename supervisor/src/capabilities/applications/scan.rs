//! The applications-directory scan (ADR-0061): which directories, in what precedence, and
//! how an `app_id` finds its entry.

pub use shared::state::applications::AppSummary;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use shared::debug;

use super::entry::{desktop_file_id, flag, parse_group, tokenize_exec};

/// Walk depth under an applications directory. Subdirectories are allowed and real trees use one
/// level (`kde4/`); the cap bounds symlink loops without canonicalizing every directory as
/// `watcher.rs` does.
const MAX_DEPTH: usize = 4;

/// What `launch` needs but Lua never sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTarget {
    pub command: String,
    pub args: Vec<String>,
    /// `Terminal=true`: wrap this console program in an emulator.
    pub terminal: bool,
}

/// One scan's result.
pub struct ScanResult {
    /// Entries sorted by display name. Sorting in Lua would repeat work on every reload.
    pub entries: Vec<AppSummary>,
    /// `app_id` to the entry's 1-based position in `entries`, for callers holding
    /// `workspaces.active_client.app_id` or a tray item with no icon of its own rather than a desktop id.
    pub by_app_id: BTreeMap<String, usize>,
    pub launch: HashMap<String, LaunchTarget>,
}

/// Applications directories in XDG precedence order.
///
/// `data_home` is [`shared::xdg_dir`]'s. Applies the `XDG_DATA_DIRS` default: plain login shells
/// often unset it, and skipping `/usr/share` in that case finds nothing on a normal system.
///
/// Injected, like `ClockController::new`, so precedence is testable without changing the process
/// environment.
pub fn application_dirs(data_home: Option<PathBuf>, data_dirs: Option<String>) -> Vec<PathBuf> {
    shared::xdg_data_dirs(data_home, data_dirs.as_deref()).into_iter().map(|dir| dir.join("applications")).collect()
}

/// Scans `dirs` in order; the first occurrence of each desktop id wins.
///
/// First-wins is the specification's rule, so `~/.local/share/applications` comes first: a same-
/// named home file overrides a system name, icon, or command. Last-wins would ignore the override.
pub fn scan(dirs: &[PathBuf]) -> ScanResult {
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<AppSummary> = Vec::new();
    let mut launch: HashMap<String, LaunchTarget> = HashMap::new();
    // Keep beside the summary until the map is built; `StartupWMClass` is only a matching key.
    let mut wm_classes: Vec<(String, Option<String>)> = Vec::new();

    for dir in dirs {
        let mut files = Vec::new();
        walk(dir, 0, &mut Vec::new(), &mut files);
        // Filesystem directory order is unspecified; sort so duplicate ids have a stable winner.
        files.sort();
        for path in files {
            let Some(id) = desktop_file_id(&path, dir) else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue; // a higher-precedence directory already provided this id.
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                debug!(2; "{}: unreadable, skipping this entry", path.display());
                continue; // unreadable entry: one missing app, never a failed scan.
            };
            let Some(group) = parse_group(&contents) else {
                debug!(2; "{}: no [Desktop Entry] group, skipping this entry", path.display());
                continue; // no `[Desktop Entry]` group at all.
            };
            // `Type` has no `Application` default; a `Link` or `Directory` is not launchable.
            if group.get("Type").map(String::as_str) != Some("Application") {
                continue;
            }
            if flag(&group, "Hidden") {
                continue;
            }
            let (Some(name), Some(exec)) = (group.get("Name"), group.get("Exec")) else {
                debug!(2; "{id}: missing Name or Exec, skipping this entry");
                continue;
            };
            let Some((command, args)) = tokenize_exec(exec) else {
                debug!(2; "{id}: Exec={exec:?} has no command left after field-code removal, skipping this entry");
                continue;
            };
            launch.insert(id.clone(), LaunchTarget { command, args, terminal: flag(&group, "Terminal") });
            wm_classes.push((id.clone(), group.get("StartupWMClass").cloned()));
            entries.push(AppSummary {
                id,
                name: name.clone(),
                icon: group.get("Icon").cloned(),
                no_display: flag(&group, "NoDisplay"),
                comment: group.get("Comment").cloned(),
                generic_name: group.get("GenericName").cloned(),
                // The specification's trailing `;` leaves an empty last field. `\;` is not
                // unescaped: no shipped entry uses it, and a stray backslash costs one keyword.
                keywords: group
                    .get("Keywords")
                    .map(|value| {
                        value.split(';').map(str::trim).filter(|word| !word.is_empty()).map(String::from).collect()
                    })
                    .unwrap_or_default(),
            });
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let by_app_id = build_app_id_map(&entries, &wm_classes);
    ScanResult { entries, by_app_id, launch }
}

/// Ways an `app_id` finds an entry, strongest first.
///
/// Two passes keep a weak key from one entry from beating a strong key from another. Entry-by-entry
/// insertion would make `org.gnome.Nautilus` depend on scan order instead of its filename.
///
/// The last-segment rule covers bare ids: niri reports Nautilus as `org.gnome.Nautilus`, and
/// Files ships that exact filename, but many apps report `nautilus` against a reverse-DNS name.
fn build_app_id_map(entries: &[AppSummary], wm_classes: &[(String, Option<String>)]) -> BTreeMap<String, usize> {
    // 1-based: the value indexes `entries` from Lua, where the JSON array lands as a sequence.
    let by_id: HashMap<&str, usize> =
        entries.iter().zip(1..).map(|(entry, index)| (entry.id.as_str(), index)).collect();
    let mut map: BTreeMap<String, usize> = BTreeMap::new();

    // Pass 1, exact: `StartupWMClass` is the specification's key for this question, so it outranks
    // the filename even when they usually agree.
    for (id, wm_class) in wm_classes {
        let Some(&index) = by_id.get(id.as_str()) else { continue };
        if let Some(class) = wm_class.as_deref().filter(|class| !class.is_empty()) {
            map.entry(class.to_string()).or_insert(index);
        }
    }
    for (entry, index) in entries.iter().zip(1..) {
        map.entry(entry.id.clone()).or_insert(index);
    }

    // Pass 2, case-folded and shortened. These are guesses and cannot displace exact keys.
    for (id, wm_class) in wm_classes {
        let Some(&index) = by_id.get(id.as_str()) else { continue };
        if let Some(class) = wm_class.as_deref().filter(|class| !class.is_empty()) {
            map.entry(class.to_lowercase()).or_insert(index);
        }
    }
    for (entry, index) in entries.iter().zip(1..) {
        map.entry(entry.id.to_lowercase()).or_insert(index);
        if let Some(last) = entry.id.rsplit('.').next().filter(|last| *last != entry.id) {
            map.entry(last.to_lowercase()).or_insert(index);
        }
    }
    map
}

/// Every depth-capped readable directory and `.desktop` file under `dir`, `dir` included; the scan
/// reads the files, the watch the directories. A missing directory contributes nothing; most
/// systems lack `/usr/local/share/applications`.
pub(super) fn walk(dir: &Path, depth: usize, dirs: &mut Vec<PathBuf>, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    dirs.push(dir.to_path_buf());
    for entry in read.flatten() {
        let path = entry.path();
        // Use `metadata` so symlinked entries are followed; packaged application trees such as
        // Flatpak exports commonly link entries from elsewhere.
        let Ok(meta) = path.metadata() else { continue };
        if meta.is_dir() {
            walk(&path, depth + 1, dirs, out);
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_entry(dir: &Path, file: &str, body: &str) {
        if let Some(parent) = dir.join(file).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn application(name: &str, extra: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec=/usr/bin/{name}\n{extra}")
    }

    #[test]
    fn application_dirs_default_when_xdg_data_dirs_is_unset_or_empty_and_the_users_directory_comes_first() {
        // Skipping `/usr/share` when `XDG_DATA_DIRS` is unset finds nothing on a normal system.
        let default = vec![PathBuf::from("/usr/local/share/applications"), PathBuf::from("/usr/share/applications")];
        assert_eq!(application_dirs(None, None), default);
        assert_eq!(application_dirs(None, Some(String::new())), default, "an empty value is unset, not no directories");
        // The user's own directory leads so an override wins; a relative entry is dropped.
        assert_eq!(
            application_dirs(Some(PathBuf::from("/custom/data")), Some("/a:relative:/b".to_string())),
            vec![
                PathBuf::from("/custom/data/applications"),
                PathBuf::from("/a/applications"),
                PathBuf::from("/b/applications"),
            ]
        );
    }

    /// A user's copy replaces the system entry, allowing a renamed app or changed command;
    /// last-wins would ignore it.
    #[test]
    fn scan_lets_an_earlier_directory_win_the_same_desktop_file_id() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        write_entry(home.path(), "editor.desktop", &application("Mine", ""));
        write_entry(system.path(), "editor.desktop", &application("Theirs", ""));

        let result = scan(&[home.path().to_path_buf(), system.path().to_path_buf()]);

        assert_eq!(result.entries.len(), 1, "one id is one entry however many directories carry it");
        assert_eq!(result.entries[0].name, "Mine");
    }

    #[test]
    fn scan_keeps_nodisplay_icons_but_excludes_hidden_and_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "shown.desktop", &application("Shown", ""));
        write_entry(dir.path(), "hidden.desktop", &application("Hidden", "Hidden=true\n"));
        write_entry(
            dir.path(),
            "org.example.Helper.desktop",
            &application("Helper", "NoDisplay=true\nIcon=helper\nStartupWMClass=HelperWindow\n"),
        );
        write_entry(dir.path(), "link.desktop", "[Desktop Entry]\nType=Link\nName=Link\nURL=http://x\n");
        write_entry(dir.path(), "noexec.desktop", "[Desktop Entry]\nType=Application\nName=NoExec\n");

        let result = scan(&[dir.path().to_path_buf()]);

        for app_id in ["org.example.Helper", "org.example.helper", "helper", "HelperWindow", "helperwindow"] {
            let index = result.by_app_id[app_id];
            assert_eq!(result.entries[index - 1].icon.as_deref(), Some("helper"), "{app_id}");
            assert!(result.entries[index - 1].no_display, "{app_id}");
        }
        assert!(!result.by_app_id.contains_key("hidden"));
        assert_eq!(result.entries.len(), 2);
        let shown: Vec<&str> =
            result.entries.iter().filter(|entry| !entry.no_display).map(|entry| entry.name.as_str()).collect();
        assert_eq!(shown, vec!["Shown"]);
    }

    #[test]
    fn a_hidden_override_removes_the_system_entry_from_launcher_and_window_lookup() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        write_entry(home.path(), "org.example.Helper.desktop", &application("Helper", "Hidden=true\n"));
        write_entry(system.path(), "org.example.Helper.desktop", &application("Helper", "Icon=helper\n"));

        let result = scan(&[home.path().to_path_buf(), system.path().to_path_buf()]);

        assert!(result.entries.is_empty());
        assert!(result.by_app_id.is_empty());
        assert!(result.launch.is_empty());
    }

    #[test]
    fn scan_sorts_entries_by_display_name_so_a_launcher_needs_no_sort_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "c.desktop", &application("Cherry", ""));
        write_entry(dir.path(), "a.desktop", &application("Apple", ""));
        write_entry(dir.path(), "b.desktop", &application("Banana", ""));

        let names: Vec<String> = scan(&[dir.path().to_path_buf()]).entries.into_iter().map(|e| e.name).collect();

        assert_eq!(names, vec!["Apple", "Banana", "Cherry"]);
    }

    /// "text editor" finds Zed through `GenericName` and "image" finds GIMP through `Keywords`;
    /// neither word is in those entries' `Name` or `Comment`.
    #[test]
    fn scan_carries_the_two_search_fields_an_entry_holds_beyond_its_name_and_comment() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "zed.desktop", &application("Zed", "GenericName=Text Editor\n"));
        write_entry(dir.path(), "gimp.desktop", &application("GIMP", "Keywords=GNU;Image Manipulation Program;\n"));

        let entries = scan(&[dir.path().to_path_buf()]).entries;

        assert_eq!(entries[0].name, "GIMP");
        assert_eq!(entries[0].keywords, vec!["GNU".to_string(), "Image Manipulation Program".to_string()]);
        assert_eq!(entries[1].generic_name.as_deref(), Some("Text Editor"));
        assert!(entries[1].keywords.is_empty(), "a missing Keywords is no keywords, not one empty one");
    }

    #[test]
    fn scan_keeps_the_parsed_argv_out_of_the_entry_a_config_can_read() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(
            dir.path(),
            "browser.desktop",
            "[Desktop Entry]\nType=Application\nName=Browser\nExec=firefox --new-tab %u\n",
        );

        let result = scan(&[dir.path().to_path_buf()]);

        let target = result.launch.get("browser").expect("the launch map carries the argv");
        assert_eq!(target.command, "firefox");
        assert_eq!(target.args, vec!["--new-tab".to_string()], "the field code must not reach argv");
        let serialized = serde_json::to_value(&result.entries[0]).unwrap();
        assert!(
            serialized.get("exec").is_none() && serialized.get("command").is_none(),
            "no argv may appear in the Lua-visible payload"
        );
    }

    /// A toplevel's app_id reaches its entry by exact desktop file id, by the last segment of a
    /// reverse-DNS id (`nautilus` for `org.gnome.Nautilus.desktop`), or by `StartupWMClass`.
    #[test]
    fn an_app_id_finds_its_entry_by_file_id_last_segment_or_startup_wm_class() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "org.gnome.Nautilus.desktop", &application("Files", ""));
        write_entry(dir.path(), "tg.desktop", &application("Telegram", "StartupWMClass=TelegramDesktop\n"));

        let result = scan(&[dir.path().to_path_buf()]);
        let name = |key: &str| result.by_app_id.get(key).map(|&index| result.entries[index - 1].name.as_str());

        assert_eq!(name("org.gnome.Nautilus"), Some("Files"));
        assert_eq!(name("nautilus"), Some("Files"));
        assert_eq!(name("TelegramDesktop"), Some("Telegram"));
    }

    /// Two passes prevent one entry's case-folded guess from displacing another's exact filename;
    /// one pass would make the winner depend on read order.
    #[test]
    fn an_exact_id_beats_another_entrys_case_folded_wm_class() {
        let dir = tempfile::tempdir().unwrap();
        // Sort first; a single pass could let a guess claim "zed" before the real entry.
        write_entry(dir.path(), "aaa.desktop", &application("Impostor", "StartupWMClass=Zed\n"));
        write_entry(dir.path(), "zed.desktop", &application("Zed Editor", ""));

        let result = scan(&[dir.path().to_path_buf()]);
        let name = |key: &str| result.by_app_id.get(key).map(|&index| result.entries[index - 1].name.as_str());

        assert_eq!(name("zed"), Some("Zed Editor"), "an exact desktop file id outranks a lowercased WM class");
        assert_eq!(name("Zed"), Some("Impostor"), "the exact WM class still resolves to the entry declaring it");
    }

    #[test]
    fn a_missing_applications_directory_contributes_nothing_rather_than_failing_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "real.desktop", &application("Real", ""));

        let result = scan(&[PathBuf::from("/nonexistent/applications"), dir.path().to_path_buf()]);

        assert_eq!(
            result.entries.len(),
            1,
            "most systems have no /usr/local/share/applications and that is not an error"
        );
    }

    #[test]
    fn scan_descends_into_a_subdirectory_and_keys_it_by_the_specifications_dashed_id() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "kde4/konsole.desktop", &application("Konsole", ""));

        let result = scan(&[dir.path().to_path_buf()]);

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].id, "kde4-konsole");
    }

    #[test]
    fn walk_stops_at_the_depth_cap_and_survives_a_symlink_fan_out() {
        let root = tempfile::tempdir().unwrap();
        // The root is depth 0, so a chain of MAX_DEPTH directories ends at the cap and one more passes it.
        let deep = (0..MAX_DEPTH).fold(root.path().to_path_buf(), |path, level| path.join(format!("d{level}")));
        std::fs::create_dir_all(deep.join("past")).unwrap();
        std::fs::write(deep.join("at-cap.desktop"), "").unwrap();
        std::fs::write(deep.join("past/past-cap.desktop"), "").unwrap();
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        walk(root.path(), 0, &mut dirs, &mut files);
        assert_eq!(files, vec![deep.join("at-cap.desktop")]);
        assert_eq!(dirs.len(), MAX_DEPTH + 1);

        // Two links back to the root: a cycle that fans out, so depth must bound the visits.
        // A detached thread, so a lost bound fails the timeout instead of hanging the suite.
        let cyclic = tempfile::tempdir().unwrap();
        for name in ["x", "y"] {
            std::os::unix::fs::symlink(cyclic.path(), cyclic.path().join(name)).unwrap();
        }
        let (start, (sender, receiver)) = (cyclic.path().to_path_buf(), std::sync::mpsc::channel());
        std::thread::spawn(move || {
            let (mut dirs, mut files) = (Vec::new(), Vec::new());
            walk(&start, 0, &mut dirs, &mut files);
            let _ = sender.send(dirs.len());
        });
        let visited = receiver.recv_timeout(std::time::Duration::from_secs(5)).expect("the walk must end");
        assert_eq!(visited, (1 << (MAX_DEPTH + 1)) - 1, "2^0 + ... + 2^MAX_DEPTH directories");
    }
}

//! Theme name to file path, in the Renderer (ADR-0054 decision 1).
//!
//! The Supervisor plan failed because synchronous lookup would block its dispatch thread on a
//! control-socket round trip. That socket carries one-way commands and `StateSnapshot`s, not
//! request/response pairs, so the answer is resolved locally here.
//!
//! `app_id` -> `.desktop` -> `Icon=` remains deferred with no caller, and no Lua `find_icon`
//! exists (ADR-0054 decision 5): `name` resolution leaves it nothing to do.

use rustc_hash::FxHashMap as HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(test)]
thread_local! {
    /// A flat directory of `<name>.svg` or `.png` that replaces the icon theme on this thread, so
    /// the docs screenshots draw the same icons on every machine. A name it lacks panics rather
    /// than drawing nothing.
    pub(crate) static FIXTURE_ICONS: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The file for `name` at `px` pixels, or `None` if the active theme chain has no match.
///
/// An absolute `name` is returned directly. `.desktop` `Icon=` accepts either spelling, letting the
/// tray use `icon { name = item.icon_name or item.icon_path }` without two node kinds.
///
/// Absolute paths are not checked: `ImageCache::image` opens them, logs once, and caches failure.
///
/// Memoized because `ImageCache` keys on the resolved *path*: the texture was cached while this
/// ran every frame for every icon. Idle bar, release build, 30 seconds: 62 calls, 101.6ms, mean
/// 1.6ms, worst 5.5ms, or 62% of `layout::paint::execute` draw-command time, more than text,
/// boxes, and images combined. Misses walk the inheritance chain and `stat` every candidate, so
/// `None` is memoized too.
///
/// Each theme's `index.theme` is parsed once per process. This map removes the per-name walk,
/// which is what the 1.6ms measurement was.
pub fn resolve(name: &str, px: u32) -> Option<PathBuf> {
    // Themes have no directory above 512.
    let size = px.min(512);
    if name.is_empty() {
        return None;
    }
    if Path::new(name).is_absolute() {
        return Some(PathBuf::from(name));
    }
    // One path component. A slash would join outside the theme directory; `./x.png` is not a name.
    if Path::new(name).file_name() != Some(OsStr::new(name)) {
        return None;
    }
    #[cfg(test)]
    if let Some(dir) = FIXTURE_ICONS.with_borrow(Clone::clone) {
        let found = ["svg", "png"].iter().map(|ext| dir.join(format!("{name}.{ext}"))).find(|path| path.is_file());
        return Some(found.unwrap_or_else(|| panic!("no fixture icon {name:?}: add {name}.svg to {}", dir.display())));
    }
    memoized(resolved(), name, size, |theme| lookup(name, size, theme))
}

/// Switches to the settings portal's theme, or back to [`gtk_theme`] for `None`, and forgets every
/// resolved path, misses included. Returns whether the theme changed.
pub fn follow_portal_theme(portal: Option<String>) -> bool {
    switch_theme(resolved(), portal.unwrap_or_else(gtk_theme))
}

fn switch_theme(resolved: &Mutex<Resolved>, theme: String) -> bool {
    let mut resolved = resolved.lock().expect("icon memo poisoned");
    if resolved.theme.as_deref() == Some(theme.as_str()) {
        return false;
    }
    *resolved = Resolved { theme: Some(theme), memo: Memo::default() };
    true
}

fn lookup(name: &str, size: u32, theme: &str) -> Option<PathBuf> {
    let roots = data_dirs();
    let bases: Vec<PathBuf> = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".icons"))
        .into_iter()
        .chain(roots.iter().map(|root| root.join("icons")))
        .collect();
    let pixmaps: Vec<PathBuf> =
        roots.iter().map(|root| root.join("pixmaps")).chain([PathBuf::from("/usr/share/pixmaps")]).collect();
    let found = find_icon(name, size, theme, &bases, &pixmaps);
    if found.is_none() && super::first_failure(&format!("{theme}/{name}")) {
        shared::warn!("icon {name:?} is in neither the {theme:?} theme nor its fallbacks");
    }
    found
}

/// [`resolve`]'s memo, with the filesystem walk injected so tests can count it. A correct answer
/// re-derived every frame is the defect.
fn memoized(
    resolved: &Mutex<Resolved>,
    name: &str,
    size: u32,
    lookup: impl FnOnce(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let theme = {
        let mut resolved = resolved.lock().expect("icon memo poisoned");
        if let Some(hit) = resolved.memo.get(&size).and_then(|by_name| by_name.get(name)) {
            return hit.clone();
        }
        resolved.theme.get_or_insert_with(gtk_theme).clone()
    };
    let found = lookup(&theme);
    // Do not hold the map across `lookup`: its filesystem walk would serialize callers.
    let mut resolved = resolved.lock().expect("icon memo poisoned");
    // A switch during `lookup` made this answer the old theme's.
    if resolved.theme.as_deref() == Some(theme.as_str()) {
        let by_name = resolved.memo.entry(size).or_default();
        if by_name.len() >= MEMO_CAPACITY {
            by_name.clear();
        }
        by_name.insert(name.to_string(), found.clone());
    }
    found
}

/// The active theme and its resolved lookups, keyed by size then name so hits borrow a `&str`.
///
/// One lock, so a theme switch and the memo it empties never disagree. Use `Mutex`, not
/// `thread_local!`, because callers are not limited to the render thread; an uncontended lock is
/// nanoseconds against the measured 1.6ms lookup.
///
/// Capped at [`MEMO_CAPACITY`] names per size and cleared wholesale there, because the set of names
/// is not bounded by what is installed: a notification's `app_icon` is a bare theme name chosen by
/// whichever application sent it, and a config drawing it (`icon { name = group.app_icon }`) hands
/// it straight to [`resolve`]. Every novel name also pays the ~1.6ms walk this memo exists to
/// avoid, on the render thread.
///
/// Cleared rather than evicted, following `text::shaping`'s cache: an LRU maintains a recency order
/// on every hit, which is work on the path the memo exists to make cheap.
fn resolved() -> &'static Mutex<Resolved> {
    static RESOLVED: OnceLock<Mutex<Resolved>> = OnceLock::new();
    RESOLVED.get_or_init(Mutex::default)
}

/// `theme` is the directory name, `None` until the first lookup or portal frame sets it.
#[derive(Default)]
struct Resolved {
    theme: Option<String>,
    memo: Memo,
}

/// Names remembered per size. Sized like `text::shaping`'s cache: far past any real working set (a
/// bar and its panels resolve tens of icons), while still bounding a stream of novel names.
const MEMO_CAPACITY: usize = 4096;

/// Size -> name -> path, nested so hits borrow the inner `&str` without allocating.
type Memo = HashMap<u32, HashMap<String, Option<PathBuf>>>;

/// The theme until the settings portal names one, which the Supervisor reads (`appearance.rs`): GTK's
/// setting, else `hicolor`, the spec's implicit fallback.
fn gtk_theme() -> String {
    gtk_setting("gtk-icon-theme-name").unwrap_or_else(|| "hicolor".to_string())
}

/// Do not spawn `gsettings get org.gnome.desktop.interface icon-theme`: that is one process per
/// render-thread call (the Wayland dispatch thread since ADR-0039).
///
/// Read `key` directly from settings, GTK 4 before GTK 3.
///
/// ponytail: GTK settings only. KDE uses `Icons/Theme` in `kdeglobals`, so Plasma falls to
/// `hicolor`, drawing app icons but almost no status icons. Upgrade: another `find_map` arm; not
/// built because Plasma is untested and a second untested parser is worse than this gap.
pub(crate) fn gtk_setting(key: &str) -> Option<String> {
    gtk_setting_in(&shared::xdg_dir("XDG_CONFIG_HOME", ".config")?, key)
}

/// [`gtk_setting`] under the config directory `config`, so a test needs no environment.
fn gtk_setting_in(config: &Path, key: &str) -> Option<String> {
    ["gtk-4.0", "gtk-3.0"].into_iter().find_map(|version| {
        let text = read_text(&config.join(version).join("settings.ini"))?;
        setting_from_ini(&text, key)
    })
}

/// A config or theme file's text: a regular file of at most 1 MiB, which these never come near.
// ponytail: past 1 MiB a file reads as absent; raise the cap if a real index.theme ever outgrows it.
fn read_text(path: &Path) -> Option<String> {
    String::from_utf8(super::decode::read_capped(path, 1 << 20).ok()??).ok()
}

/// `key`'s value in `settings.ini`, split out so parsing needs no home dir.
fn setting_from_ini(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix(key))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
        .map(|value| value.trim().trim_matches('"').to_string())
        .find(|value| !value.is_empty())
}

/// `$XDG_DATA_HOME` (default `~/.local/share`), then each absolute `$XDG_DATA_DIRS` entry.
fn data_dirs() -> Vec<PathBuf> {
    shared::xdg_data_dirs(
        shared::xdg_dir("XDG_DATA_HOME", ".local/share"),
        std::env::var("XDG_DATA_DIRS").ok().as_deref(),
    )
}

/// Named theme, then its `Inherits` chain, then `hicolor`, then `<base>/<name>.<ext>` directly,
/// then `pixmaps`. `bases` are the `icons` directories in spec order.
fn find_icon(name: &str, px: u32, theme: &str, bases: &[PathBuf], pixmaps: &[PathBuf]) -> Option<PathBuf> {
    let mut seen = HashSet::new();
    walk_theme(name, px, theme, bases, &mut seen)
        .or_else(|| if seen.contains("hicolor") { None } else { search_theme(name, px, "hicolor", bases) })
        .or_else(|| bases.iter().chain(pixmaps).find_map(|dir| existing_icon(dir, name)))
}

fn walk_theme(name: &str, px: u32, theme: &str, bases: &[PathBuf], seen: &mut HashSet<String>) -> Option<PathBuf> {
    if !seen.insert(theme.to_string()) {
        return None;
    }
    search_theme(name, px, theme, bases)
        .or_else(|| theme_inherits(theme, bases).iter().find_map(|parent| walk_theme(name, px, parent, bases, seen)))
}

/// A copy of `theme` under `base` with the theme's index: its own, else the first base's. Packages
/// may add icons to a theme in one base while another holds its `index.theme`.
fn theme_index(base: &Path, theme: &str, bases: &[PathBuf]) -> Option<(PathBuf, Arc<super::icon_index::ThemeIndex>)> {
    let theme_dir = base.join(theme);
    if !theme_dir.is_dir() {
        return None;
    }
    let index = load_index(&theme_dir.join("index.theme"))
        .or_else(|| bases.iter().find_map(|other| load_index(&other.join(theme).join("index.theme"))))?;
    Some((theme_dir, index))
}

/// Parents from every copy of `theme`, in base order. The first copy's empty `Inherits` must
/// not hide a later copy that names a parent.
fn theme_inherits(theme: &str, bases: &[PathBuf]) -> Vec<String> {
    let mut parents = Vec::new();
    for base in bases {
        let Some((_, index)) = theme_index(base, theme, bases) else { continue };
        for parent in &index.inherits {
            if !parents.contains(parent) {
                parents.push(parent.clone());
            }
        }
    }
    parents
}

/// Exact directory match in an earlier base beats a later one. A later base's exact match
/// beats an earlier base that only has a farther file.
fn search_theme(name: &str, px: u32, theme: &str, bases: &[PathBuf]) -> Option<PathBuf> {
    let mut closest: Option<(u32, PathBuf)> = None;
    for base in bases {
        let Some((theme_dir, index)) = theme_index(base, theme, bases) else { continue };
        if let Some((distance, path)) = scan_theme_dir(&theme_dir, &index, name, px) {
            if distance == 0 {
                return Some(path);
            }
            if closest.as_ref().is_none_or(|(best, _)| distance < *best) {
                closest = Some((distance, path));
            }
        }
    }
    closest.map(|(_, path)| path)
}

fn scan_theme_dir(
    theme_dir: &Path,
    index: &super::icon_index::ThemeIndex,
    name: &str,
    px: u32,
) -> Option<(u32, PathBuf)> {
    let mut closest: Option<(u32, PathBuf)> = None;
    for dir in &index.dirs {
        let Some(path) = existing_icon(&theme_dir.join(&dir.subdir), name) else { continue };
        let distance = dir.distance(px);
        if distance == 0 {
            return Some((0, path));
        }
        if closest.as_ref().is_none_or(|(best, _)| distance < *best) {
            closest = Some((distance, path));
        }
    }
    closest
}

/// png, svg, svgz, then ico and bmp (off-spec, but some apps ship only `pixmaps/<name>.ico`): the
/// extensions this renderer decodes. `.xpm` is not among them, so a pixmap that exists only in that
/// format stays unresolved rather than returning a path nothing can draw.
fn existing_icon(dir: &Path, name: &str) -> Option<PathBuf> {
    ["png", "svg", "svgz", "ico", "bmp"]
        .into_iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .find(|path| path.is_file())
}

fn load_index(path: &Path) -> Option<Arc<super::icon_index::ThemeIndex>> {
    let cached = indexes().lock().expect("icon index cache poisoned").get(path).cloned();
    if let Some(cached) = cached {
        return Some(cached);
    }
    let text = read_text(path)?;
    let parsed = Arc::new(super::icon_index::parse(&text));
    indexes().lock().expect("icon index cache poisoned").insert(path.to_path_buf(), Arc::clone(&parsed));
    Some(parsed)
}

fn indexes() -> &'static Mutex<HashMap<PathBuf, Arc<super::icon_index::ThemeIndex>>> {
    static INDEXES: OnceLock<Mutex<HashMap<PathBuf, Arc<super::icon_index::ThemeIndex>>>> = OnceLock::new();
    INDEXES.get_or_init(|| Mutex::new(HashMap::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_icon_warns_once_in_each_theme() {
        let name = "mantle-test-missing-icon-theme-warning";
        for theme in ["mantle-test-old-theme", "mantle-test-new-theme"] {
            assert_eq!(lookup(name, 16, theme), None);
            assert!(!super::super::first_failure(&format!("{theme}/{name}")));
        }
    }

    #[test]
    fn only_an_absolute_name_is_its_own_path() {
        // Tray icons arrive as absolute `icon_path`s with no stat. `./x.png` is not a theme name
        // and is not resolved against the working directory.
        for (name, expected) in [
            ("/run/user/1000/mantle/tray/telegram.png", Some(PathBuf::from("/run/user/1000/mantle/tray/telegram.png"))),
            ("/nonexistent/x.png", Some(PathBuf::from("/nonexistent/x.png"))),
            ("", None),
            ("./mantle-does-not-exist.png", None),
            ("foo/", None),
            ("a/b", None),
            (".", None),
            ("..", None),
        ] {
            assert_eq!(resolve(name, 16), expected, "{name:?}");
        }
    }

    #[test]
    fn the_settings_parse_returns_the_directory_name_gtk_wrote() {
        let ini = "[Settings]\ngtk-theme-name=Adwaita-dark\ngtk-icon-theme-name=Tela-circle-dracula\ngtk-font-name=Cantarell 11\n";
        assert_eq!(setting_from_ini(ini, "gtk-icon-theme-name").as_deref(), Some("Tela-circle-dracula"));
        // GLib permits spaces around `=`; some tools quote the value.
        assert_eq!(
            setting_from_ini("gtk-icon-theme-name = Papirus", "gtk-icon-theme-name").as_deref(),
            Some("Papirus")
        );
        assert_eq!(
            setting_from_ini("gtk-icon-theme-name=\"Papirus\"", "gtk-icon-theme-name").as_deref(),
            Some("Papirus")
        );
    }

    #[test]
    fn a_settings_file_without_the_key_falls_through_rather_than_matching_a_prefix() {
        assert_eq!(setting_from_ini("[Settings]\ngtk-theme-name=Adwaita\n", "gtk-icon-theme-name"), None);
        assert_eq!(setting_from_ini("", "gtk-icon-theme-name"), None);
        // Empty is not a theme: `with_theme("")` silently loses every icon.
        assert_eq!(setting_from_ini("gtk-icon-theme-name=", "gtk-icon-theme-name"), None);
    }

    #[test]
    fn the_memo_keys_on_both_name_and_size() {
        let state = in_theme("hicolor");
        // A collision draws the *wrong* icon, harder to notice than a missing one: a plausible bar
        // full of icons other than the requested ones.
        assert_eq!(
            memoized(&state, "mantle-test-beta", 16, |_| Some(PathBuf::from("/memo/beta-16"))),
            Some(PathBuf::from("/memo/beta-16"))
        );
        assert_eq!(
            memoized(&state, "mantle-test-beta", 32, |_| Some(PathBuf::from("/memo/beta-32"))),
            Some(PathBuf::from("/memo/beta-32"))
        );
        assert_eq!(
            memoized(&state, "mantle-test-gamma", 16, |_| Some(PathBuf::from("/memo/gamma-16"))),
            Some(PathBuf::from("/memo/gamma-16"))
        );
        assert_eq!(
            memoized(&state, "mantle-test-beta", 16, |_| panic!("a remembered name must not be looked up again")),
            Some(PathBuf::from("/memo/beta-16"))
        );
    }

    #[test]
    fn a_name_the_theme_does_not_have_is_remembered_as_absent() {
        let state = in_theme("hicolor");
        // Misses walk the whole inheritance chain and stat every candidate; worst call was 5.5ms.
        // Memoize `None` exactly like a hit.
        assert_eq!(memoized(&state, "mantle-test-missing", 16, |_| None), None);
        assert_eq!(
            memoized(&state, "mantle-test-missing", 16, |_| panic!("an absent name must not be looked up again")),
            None
        );
    }

    /// A notification's `app_icon` is an arbitrary name from whichever application sent it, so the
    /// set of names is not bounded by what is installed. Without the cap this map grew for the life
    /// of the generation.
    #[test]
    fn the_memo_drops_its_names_rather_than_growing_without_bound() {
        let state = in_theme("hicolor");
        let mut walks = 0;
        for i in 0..=MEMO_CAPACITY {
            memoized(&state, &format!("name-that-no-theme-has-{i}"), 999, |_| {
                walks += 1;
                None
            });
        }
        let held = state.lock().unwrap().memo.get(&999).map(HashMap::len).unwrap_or_default();
        assert!(held <= MEMO_CAPACITY, "the map must not hold more than the cap, held {held}");
        assert_eq!(walks, MEMO_CAPACITY + 1, "every novel name still resolves; the cap bounds memory, not correctness");
    }

    /// The process-wide state: a repeated frame must not repaint every icon surface again. Only
    /// this test sets it, and every other test resolves through fixtures or a local state.
    #[test]
    fn the_same_portal_theme_twice_is_one_switch() {
        assert!(follow_portal_theme(Some("mantle-test-portal".into())));
        assert!(!follow_portal_theme(Some("mantle-test-portal".into())));
    }

    fn in_theme(theme: &str) -> Mutex<Resolved> {
        let state = Mutex::default();
        switch_theme(&state, theme.to_string());
        state
    }

    #[test]
    fn a_theme_switch_resolves_a_remembered_name_again_including_a_miss() {
        let root = tempfile::tempdir().unwrap();
        for theme in ["Old", "New"] {
            write_index(root.path(), theme, &fixed_index(None));
            write_icon(root.path(), theme, "16x16/apps", "both");
        }
        write_icon(root.path(), "New", "16x16/apps", "only-new");
        let state = in_theme("Old");
        let resolve = |name: &str| memoized(&state, name, 16, |theme| find(name, 16, theme, &[root.path()]));
        assert_eq!(resolve("both"), Some(root.path().join("icons/Old/16x16/apps/both.png")));
        assert_eq!(resolve("only-new"), None);

        assert!(!switch_theme(&state, "Old".into()), "an unchanged theme is no switch");
        assert!(memoized(&state, "both", 16, |_| panic!("an unchanged theme must keep the memo")).is_some());

        assert!(switch_theme(&state, "New".into()));
        assert_eq!(resolve("both"), Some(root.path().join("icons/New/16x16/apps/both.png")));
        assert_eq!(resolve("only-new"), Some(root.path().join("icons/New/16x16/apps/only-new.png")));
    }

    fn find(name: &str, px: u32, theme: &str, roots: &[&Path]) -> Option<PathBuf> {
        let bases: Vec<_> = roots.iter().map(|root| root.join("icons")).collect();
        let pixmaps: Vec<_> = roots.iter().map(|root| root.join("pixmaps")).collect();
        find_icon(name, px, theme, &bases, &pixmaps)
    }

    fn fixed_index(inherits: Option<&str>) -> String {
        let inherits = inherits.map(|parents| format!("Inherits={parents}\n")).unwrap_or_default();
        format!("[Icon Theme]\n{inherits}Directories=16x16/apps\n\n[16x16/apps]\nSize=16\nType=Fixed\n")
    }

    fn write_index(root: &Path, theme: &str, index: &str) {
        let dir = root.join("icons").join(theme);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.theme"), index).unwrap();
    }

    fn write_icon(root: &Path, theme: &str, subdir: &str, name: &str) {
        let dir = root.join("icons").join(theme).join(subdir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.png")), b"x").unwrap();
    }

    #[test]
    fn directory_type_matches_only_its_own_size_rule() {
        let root = tempfile::tempdir().unwrap();
        write_index(
            root.path(),
            "Sized",
            "[Icon Theme]\nDirectories=16x16/apps,scalable/apps,threshold/apps\n\
             \n[16x16/apps]\nSize=16\nType=Fixed\n\
             \n[scalable/apps]\nSize=128\nType=Scalable\nMinSize=48\nMaxSize=256\n\
             \n[threshold/apps]\nSize=32\nType=Threshold\nThreshold=2\n",
        );
        for subdir in ["16x16/apps", "scalable/apps", "threshold/apps"] {
            write_icon(root.path(), "Sized", subdir, "kind");
        }
        let roots = [root.path()];
        assert_eq!(find("kind", 16, "Sized", &roots), Some(root.path().join("icons/Sized/16x16/apps/kind.png")));
        assert_eq!(find("kind", 64, "Sized", &roots), Some(root.path().join("icons/Sized/scalable/apps/kind.png")));
        assert_eq!(find("kind", 34, "Sized", &roots), Some(root.path().join("icons/Sized/threshold/apps/kind.png")));
    }

    #[test]
    fn a_scaled_directory_matches_size_times_scale() {
        let root = tempfile::tempdir().unwrap();
        write_index(
            root.path(),
            "Dpi",
            "[Icon Theme]\nDirectories=16/apps,16@2/apps\n\
             \n[16/apps]\nSize=16\nType=Fixed\n\
             \n[16@2/apps]\nSize=16\nType=Fixed\nScale=2\n",
        );
        write_icon(root.path(), "Dpi", "16/apps", "dpi");
        write_icon(root.path(), "Dpi", "16@2/apps", "dpi");
        assert_eq!(find("dpi", 16, "Dpi", &[root.path()]), Some(root.path().join("icons/Dpi/16/apps/dpi.png")));
        assert_eq!(find("dpi", 32, "Dpi", &[root.path()]), Some(root.path().join("icons/Dpi/16@2/apps/dpi.png")));
    }

    #[test]
    fn a_pixel_off_the_scaled_size_does_not_count_as_that_directory() {
        let root = tempfile::tempdir().unwrap();
        write_index(
            root.path(),
            "Dpi",
            "[Icon Theme]\nDirectories=16@2/apps,33/apps\n\
             \n[16@2/apps]\nSize=16\nType=Fixed\nScale=2\n\
             \n[33/apps]\nSize=33\nType=Fixed\n",
        );
        write_icon(root.path(), "Dpi", "16@2/apps", "off");
        write_icon(root.path(), "Dpi", "33/apps", "off");
        assert_eq!(find("off", 33, "Dpi", &[root.path()]), Some(root.path().join("icons/Dpi/33/apps/off.png")));
    }

    #[test]
    fn a_threshold_miss_is_closer_to_its_window_than_to_a_fixed_directory() {
        let root = tempfile::tempdir().unwrap();
        write_index(
            root.path(),
            "Near",
            "[Icon Theme]\nDirectories=22/apps,32/apps\n\
             \n[22/apps]\nSize=22\nType=Fixed\n\
             \n[32/apps]\nSize=32\nType=Threshold\nThreshold=2\n",
        );
        write_icon(root.path(), "Near", "22/apps", "edge");
        write_icon(root.path(), "Near", "32/apps", "edge");
        // 27 is 3 px outside 30..34 and 5 px from 22. The window edge wins.
        assert_eq!(find("edge", 27, "Near", &[root.path()]), Some(root.path().join("icons/Near/32/apps/edge.png")));
    }

    #[test]
    fn an_explicit_hicolor_parent_is_searched_where_it_was_listed() {
        let root = tempfile::tempdir().unwrap();
        write_index(root.path(), "Child", &fixed_index(Some("hicolor,Parent")));
        write_index(root.path(), "hicolor", &fixed_index(None));
        write_index(root.path(), "Parent", &fixed_index(None));
        write_icon(root.path(), "hicolor", "16x16/apps", "both");
        write_icon(root.path(), "Parent", "16x16/apps", "both");
        assert_eq!(
            find("both", 16, "Child", &[root.path()]),
            Some(root.path().join("icons/hicolor/16x16/apps/both.png"))
        );
    }

    #[test]
    fn a_later_copy_of_the_theme_still_contributes_its_parents() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        write_index(home.path(), "Foo", &fixed_index(None));
        write_index(system.path(), "Foo", &fixed_index(Some("Parent")));
        write_index(system.path(), "Parent", &fixed_index(None));
        write_icon(system.path(), "Parent", "16x16/apps", "only-parent");
        assert_eq!(
            find("only-parent", 16, "Foo", &[home.path(), system.path()]),
            Some(system.path().join("icons/Parent/16x16/apps/only-parent.png"))
        );
    }

    #[test]
    fn a_copy_without_an_index_uses_the_index_of_the_same_theme_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        write_index(system.path(), "hicolor", &fixed_index(None));
        write_icon(home.path(), "hicolor", "16x16/apps", "steam_icon");
        assert_eq!(
            find("steam_icon", 16, "hicolor", &[home.path(), system.path()]),
            Some(home.path().join("icons/hicolor/16x16/apps/steam_icon.png"))
        );
    }

    #[test]
    fn inherits_is_walked_before_hicolor() {
        let root = tempfile::tempdir().unwrap();
        write_index(root.path(), "Child", &fixed_index(Some("Parent")));
        write_index(root.path(), "Parent", &fixed_index(None));
        write_index(root.path(), "hicolor", &fixed_index(None));
        write_icon(root.path(), "Parent", "16x16/apps", "from-parent");
        write_icon(root.path(), "hicolor", "16x16/apps", "from-parent");
        assert_eq!(
            find("from-parent", 16, "Child", &[root.path()]),
            Some(root.path().join("icons/Parent/16x16/apps/from-parent.png"))
        );
    }

    #[test]
    fn hicolor_is_used_when_the_named_theme_misses() {
        let root = tempfile::tempdir().unwrap();
        write_index(root.path(), "Child", &fixed_index(None));
        write_index(root.path(), "hicolor", &fixed_index(None));
        write_icon(root.path(), "hicolor", "16x16/apps", "only-hicolor");
        assert_eq!(
            find("only-hicolor", 16, "Child", &[root.path()]),
            Some(root.path().join("icons/hicolor/16x16/apps/only-hicolor.png"))
        );
    }

    #[test]
    fn an_unthemed_file_in_a_base_beats_pixmaps_and_pixmaps_is_the_last_resort() {
        let root = tempfile::tempdir().unwrap();
        write_index(root.path(), "Child", &fixed_index(None));
        let icons = root.path().join("icons");
        let pixmaps = root.path().join("pixmaps");
        std::fs::create_dir_all(&pixmaps).unwrap();
        std::fs::write(pixmaps.join("mark.png"), b"x").unwrap();
        std::fs::write(pixmaps.join("mark.ico"), b"x").unwrap();
        std::fs::write(pixmaps.join("app.ico"), b"x").unwrap();
        std::fs::write(pixmaps.join("both.png"), b"x").unwrap();
        std::fs::write(icons.join("both.svg"), b"x").unwrap();
        let roots = [root.path()];
        assert_eq!(find("mark", 16, "Child", &roots), Some(pixmaps.join("mark.png")));
        assert_eq!(find("app", 16, "Child", &roots), Some(pixmaps.join("app.ico")), "an ico-only app still resolves");
        assert_eq!(find("both", 16, "Child", &roots), Some(icons.join("both.svg")));
    }

    #[test]
    fn a_config_file_that_is_a_fifo_or_huge_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("settings.ini");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("the test needs a real FIFO");
        assert_eq!(read_text(&fifo), None, "a FIFO is refused, not opened");
        assert!(load_index(&fifo).is_none());
        let huge = dir.path().join("index.theme");
        std::fs::write(&huge, vec![b'#'; (1 << 20) + 1]).unwrap();
        assert_eq!(read_text(&huge), None);
        std::fs::write(&huge, "[Icon Theme]\n").unwrap();
        assert_eq!(read_text(&huge).as_deref(), Some("[Icon Theme]\n"));
        // GTK 4 wins when both name the key; one that is a FIFO falls through to GTK 3.
        std::fs::create_dir_all(dir.path().join("gtk-4.0")).unwrap();
        std::fs::create_dir_all(dir.path().join("gtk-3.0")).unwrap();
        std::fs::write(dir.path().join("gtk-3.0/settings.ini"), "gtk-icon-theme-name=Three\n").unwrap();
        std::fs::write(dir.path().join("gtk-4.0/settings.ini"), "gtk-icon-theme-name=\"Four\"\n").unwrap();
        assert_eq!(gtk_setting_in(dir.path(), "gtk-icon-theme-name").as_deref(), Some("Four"));
        std::fs::remove_file(dir.path().join("gtk-4.0/settings.ini")).unwrap();
        nix::unistd::mkfifo(&dir.path().join("gtk-4.0/settings.ini"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert_eq!(gtk_setting_in(dir.path(), "gtk-icon-theme-name").as_deref(), Some("Three"));
        assert_eq!(gtk_setting_in(dir.path(), "gtk-other"), None);
    }

    #[test]
    fn an_index_theme_is_parsed_once_for_two_names() {
        let root = tempfile::tempdir().unwrap();
        write_index(root.path(), "Once", &fixed_index(None));
        let path = root.path().join("icons/Once/index.theme");
        let first = load_index(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &load_index(&path).unwrap()));
    }
}

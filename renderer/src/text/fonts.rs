//! Resolves the shell's declared font chain to concrete font files and loads only those
//! into a `fontdb::Database` (ADR-0043 decision 2).
//!
//! Ask fontconfig for each requested family and load only those files. A full system scan took
//! roughly one second on the machine measured in ADR-0043. Parley shapes against the loaded
//! faces; FemtoVG draws the same faces (`text::atlas`, ADR-0211).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use fontdb::{Database, FaceInfo, Family, Query, Source};
use shared::{debug, error, warn};

/// Generic CSS family names fontconfig always resolves by substitution -- there is no literal
/// "sans-serif" family to compare a match against, so the miss check (does the resolved family
/// equal what was asked for) doesn't apply: whatever fontconfig returns for them *is* the answer.
const GENERIC_ALIASES: &[&str] = &["sans-serif", "serif", "monospace", "cursive", "fantasy"];

/// What a config that declares no `fonts { ... }` gets: a system sans serif, then CJK coverage,
/// then color emoji (ADR-0043 decision 2), covering codepoints that arrive from outside the
/// shell's own strings (MPRIS titles, notification bodies, window titles).
///
/// This is the fallback, not the chain. `crate::lua::fonts` records what a config asked for and
/// `ShapingHandle::set_chain` installs it after the startup evaluation. A shell drawing its chrome
/// with Nerd Font private-use glyphs has to declare one: none of the three families here carries
/// those codepoints, so without a declaration they render as tofu.
pub const DEFAULT_CHAIN: &[&str] = &["sans-serif", "Noto Sans CJK JP", "Noto Color Emoji"];

/// A font chain resolved to loaded faces: the database holding exactly those faces, and the
/// family name shaping puts first in Parley's ordered family list.
pub struct ResolvedFonts {
    pub db: Database,
    pub primary_family: String,
    /// Every file already in `db`, so a family loaded later by [`load_chain`] can tell a new file
    /// from one the declared chain already mapped (ADR-0144). Carried out of resolution rather
    /// than rebuilt from `db.faces()`, which would have to re-derive it on every runtime load.
    pub loaded_paths: HashSet<PathBuf>,
    /// The faces of the config's font files, whose families resolve to them before fontconfig.
    pub registered: Vec<FaceInfo>,
}

thread_local! {
    /// A `fonts.conf` this thread's `fc-match` calls read instead of the system's, so the docs
    /// screenshots render in pinned faces. Per thread, not the process environment: tests share one
    /// process. Set by `ShapingHandle::spawn_with` on its worker.
    pub(crate) static FONTCONFIG_FILE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Resolves `chain` in order and loads every entry that hits into one `Database`.
///
/// A chain entry that fontconfig can't honor (not installed, or genuinely absent) is skipped,
/// not silently substituted -- see `fc_match`'s doc comment for how a "hit" is told apart from
/// fontconfig's own substitution, which never fails on its own.
///
/// A chain entry naming a family from `files` takes those files rather than fontconfig's answer,
/// the way CSS `@font-face` shadows an installed family of the same name. Database order is
/// fallback order (`text::shaping::worker`), so each file joins at the first entry naming one of
/// its families, and a file no entry names joins after the whole chain.
///
/// Logs per chain entry, to diagnose "why is my bar drawing in the wrong font".
pub fn resolve_chain(chain: &[&str], files: &[PathBuf]) -> ResolvedFonts {
    let mut db = Database::new();
    let mut loaded_paths: HashSet<PathBuf> = HashSet::new();
    let registered = scan_files(files);
    let primary_family = load_chain(&mut db, chain, &mut loaded_paths, &registered);
    for face in &registered {
        load_file(&mut db, &registered, face_path(face), &mut loaded_paths);
    }
    match primary_family {
        Some(primary_family) => ResolvedFonts { db, primary_family, loaded_paths, registered },
        None => system_fallback(chain, ResolvedFonts { db, primary_family: String::new(), loaded_paths, registered }),
    }
}

/// Reads the faces `files` hold, each file once, without adding them to any chain yet. A file that
/// yields no face is the config naming the wrong path, so it warns rather than logging at `-vvv`.
fn scan_files(files: &[PathBuf]) -> Vec<FaceInfo> {
    let mut scratch = Database::new();
    for (index, path) in files.iter().enumerate() {
        if files[..index].contains(path) {
            continue;
        }
        // A FIFO would block startup in the open below.
        if let Err(e) = crate::image::refuse_irregular(path) {
            warn!("font file {path:?} skipped: {e}");
            continue;
        }
        let before = scratch.len();
        match scratch.load_font_file(path) {
            Err(e) => warn!("font file {path:?} failed to load: {e}"),
            Ok(()) if scratch.len() == before => warn!("font file {path:?} holds no font face"),
            Ok(()) => {}
        }
    }
    scratch.faces().cloned().collect()
}

/// The file a face came from. Every face here was loaded from a path.
fn face_path(face: &FaceInfo) -> &Path {
    match &face.source {
        Source::File(path) | Source::SharedFile(path, _) => path,
        Source::Binary(_) => Path::new(""),
    }
}

/// The spelling the font files give `name`, when one of them holds that family.
fn registered_family<'a>(registered: &'a [FaceInfo], name: &str) -> Option<&'a str> {
    registered
        .iter()
        .flat_map(|face| &face.families)
        .map(|(family, _)| family.as_str())
        .find(|family| family.eq_ignore_ascii_case(name))
}

/// Adds every face of the registered file at `path` to `db`, unless it is already there.
fn load_file(db: &mut Database, registered: &[FaceInfo], path: &Path, loaded_paths: &mut HashSet<PathBuf>) {
    if loaded_paths.insert(path.to_path_buf()) {
        for face in registered.iter().filter(|face| face_path(face) == path) {
            db.push_face_info(face.clone());
        }
    }
}

/// The file fontconfig ranks first for `ch`, a codepoint every face the chain loaded drew as a
/// box (ADR-0239). The shaping worker runs it on its probe thread, or inline without one (#55).
///
/// ponytail: the face is fontconfig's own first-ranked answer -- the system's declared font
/// policy, not a designer's pick. With Font Awesome installed, `:charset=1f600` resolves to it
/// rather than to an emoji face. A shell that cares names the family in its chain.
pub fn covering_file(ch: char) -> Option<PathBuf> {
    fc_match_raw(&format!(":charset={:x}", ch as u32)).map(|(path, _)| path)
}

/// Loads `path`, the file [`covering_file`] answered for `ch`. `true` when the database gained a
/// face and the caller must shape again.
pub fn load_cover(
    db: &mut Database,
    ch: char,
    path: PathBuf,
    loaded_paths: &mut HashSet<PathBuf>,
    registered: &[FaceInfo],
) -> bool {
    let cp = ch as u32;
    // The shaper tried every loaded face before reaching here, so fontconfig answering with one of
    // them is it declining rather than helping -- this query's substitution check (ADR-0239).
    if loaded_paths.contains(&path) {
        debug!(2; "font chain: U+{cp:04X} -> no installed face covers it");
        return false;
    }
    if let Err(reason) = load_installed(db, &path, registered) {
        debug!(2; "font chain: U+{cp:04X} resolved to {path:?}, skipped: {reason}");
        return false;
    }
    debug!(2; "font chain: U+{cp:04X} -> {path:?}");
    loaded_paths.insert(path);
    true
}

/// Loads the installed file at `path`, or none of it when it holds a family the config's files
/// register. Not only the shadowed faces: Fontique registers every face of a file
/// (`Collection::register_fonts`) and unregisters by width, style and weight alone, which would
/// take the registered face too, so a collection's other faces would bring the shadowed one along.
fn load_installed(db: &mut Database, path: &Path, registered: &[FaceInfo]) -> Result<(), String> {
    let ids = db.load_font_source(Source::File(path.to_path_buf()));
    if ids.is_empty() {
        return Err("it holds no loadable face".into());
    }
    let shadowed = ids
        .iter()
        .filter_map(|id| db.face(*id))
        .flat_map(|face| &face.families)
        .find_map(|(family, _)| registered_family(registered, family))
        .map(str::to_string);
    prune_shadowed(db, registered);
    shadowed.map_or(Ok(()), |family| Err(format!("it also holds {family:?}, which a font file registers")))
}

/// Removes every face of each file, other than a registered one, that holds a registered family.
fn prune_shadowed(db: &mut Database, registered: &[FaceInfo]) {
    let own: HashSet<&Path> = registered.iter().map(face_path).collect();
    let shadows =
        |face: &FaceInfo| face.families.iter().any(|(family, _)| registered_family(registered, family).is_some());
    let files: HashSet<PathBuf> = db
        .faces()
        .filter(|face| !own.contains(face_path(face)) && shadows(face))
        .map(|face| face_path(face).to_path_buf())
        .collect();
    let ids: Vec<fontdb::ID> = db.faces().filter(|face| files.contains(face_path(face))).map(|face| face.id).collect();
    ids.into_iter().for_each(|id| db.remove_face(id));
}

/// Loads every entry of one chain into `db` and returns the first that hit, which is that chain's
/// primary family, along with that primary's bold, italic and bold-italic files.
///
/// A one-name chain is `text { font = "..." }` loaded by hand into the database the declared chain
/// already filled (ADR-0144): same `fc_match`, same hit-versus-substitution check. Reusing that
/// database keeps the declared chain's CJK and emoji faces working as fallback behind the named
/// family, and keeps this from being a second font discovery (ADR-0043 decision 2). A family the
/// config's font files hold, named or answered for a generic alias, loads those files and no
/// installed copy or variant. `None` for a
/// family nothing answers: the caller draws in the declared chain and logs it once at `-vvv`.
pub(crate) fn load_chain(
    db: &mut Database,
    chain: &[&str],
    loaded_paths: &mut HashSet<PathBuf>,
    registered: &[FaceInfo],
) -> Option<String> {
    let mut primary_family: Option<String> = None;

    for &name in chain {
        let (resolved_family, path) = match registered_family(registered, name) {
            Some(family) => (family.to_string(), None),
            None => {
                let Some((path, resolved)) = fc_match(name) else {
                    debug!(2; "font chain: {name:?} -> no installed match, skipped");
                    continue;
                };
                match registered_family(registered, &resolved) {
                    Some(family) => (family.to_string(), None),
                    None => (resolved, Some(path)),
                }
            }
        };
        let Some(path) = path else {
            // Every file of the family, variants included: the config listed the ones it wants.
            debug!(2; "font chain: {name:?} -> registered files ({resolved_family:?})");
            let paths: Vec<PathBuf> = registered
                .iter()
                .filter(|face| face.families.iter().any(|(family, _)| family.eq_ignore_ascii_case(&resolved_family)))
                .map(|face| face_path(face).to_path_buf())
                .collect();
            paths.iter().for_each(|path| load_file(db, registered, path, loaded_paths));
            set_generic(db, name, &resolved_family);
            primary_family.get_or_insert(resolved_family);
            continue;
        };
        debug!(2; "font chain: {name:?} -> {path:?} ({resolved_family:?})");

        // Two chain entries can resolve to the same file (e.g. a generic alias and the literal
        // family name it happens to expand to) -- load it once.
        if !loaded_paths.contains(&path) {
            if let Err(reason) = load_installed(db, &path, registered) {
                warn!("font chain: {name:?} resolved to {path:?}, skipped: {reason}");
                continue;
            }
            loaded_paths.insert(path);
        }

        set_generic(db, name, &resolved_family);
        if primary_family.is_none() {
            load_variants(db, name, &resolved_family, loaded_paths, registered);
            primary_family = Some(resolved_family);
        }
    }

    primary_family
}

/// Points `name`, when it is a generic alias, at the family it resolved to.
fn set_generic(db: &mut Database, name: &str, family: &str) {
    match name.to_ascii_lowercase().as_str() {
        "serif" => db.set_serif_family(family),
        "sans-serif" => db.set_sans_serif_family(family),
        "monospace" => db.set_monospace_family(family),
        "cursive" => db.set_cursive_family(family),
        "fantasy" => db.set_fantasy_family(family),
        _ => {}
    }
}

/// Loads the primary family's bold, italic and bold-italic files, when fontconfig has them, so a
/// styled run (ADR-0104) is shaped and painted in a real face rather than the regular one. Only
/// the primary gets variants: a fallback entry is there for codepoint coverage and nothing asks it
/// for a weight.
///
/// Three more `fc-match` calls at startup, each asking for `family:weight=bold` and the like. A
/// family shipped as one `.ttc` resolves every variant to the file already loaded, and
/// `loaded_paths` makes that free; one shipped as separate files loads each. fontconfig answers a
/// variant the family does not have with the nearest face it does, which is the regular file again
/// or a different family entirely -- the same family check `fc_match` applies everywhere rejects
/// the latter, and the former is a duplicate path and skipped.
fn load_variants(
    db: &mut Database,
    name: &str,
    resolved_family: &str,
    loaded_paths: &mut HashSet<PathBuf>,
    registered: &[FaceInfo],
) {
    for (variant, pattern) in
        [("bold", "weight=bold"), ("italic", "slant=italic"), ("bold italic", "weight=bold:slant=italic")]
    {
        let Some((path, family)) = fc_match(&format!("{name}:{pattern}")) else {
            continue;
        };
        if !family.eq_ignore_ascii_case(resolved_family) || loaded_paths.contains(&path) {
            continue;
        }
        match load_installed(db, &path, registered) {
            Ok(()) => {
                debug!(2; "font chain: {name:?} {variant} -> {path:?}");
                loaded_paths.insert(path);
            }
            Err(reason) => debug!(2; "font chain: {name:?} {variant} resolved to {path:?}, skipped: {reason}"),
        }
    }
}

/// The whole chain missed -- no fontconfig installed, `fc-match` missing from `$PATH`, or every
/// entry genuinely absent. A real degradation path, not a panic.
///
/// ponytail: this is `load_system_fonts()` plus a `SansSerif` query, exactly the ~1s-startup,
/// whole-system-scan path ADR-0043 decision 2 spends this module avoiding. Correct only
/// because it's the last resort, not the common case -- if this line shows up in a real user's
/// logs, the fix is diagnosing why `fc-match` isn't reachable, not tuning this fallback.
///
/// `fonts` holds the registered files, which stay: the scan adds to them.
fn system_fallback(chain: &[&str], mut fonts: ResolvedFonts) -> ResolvedFonts {
    warn!(
        "font chain: none of {chain:?} resolved via fc-match (fontconfig not installed, or \
         fc-match missing from $PATH) -- falling back to a full system font scan, which is \
         the slow, memory-heavy path ADR-0043 decision 2 exists to avoid"
    );
    fonts.db.load_system_fonts();
    prune_shadowed(&mut fonts.db, &fonts.registered);
    fonts.primary_family = system_primary(&fonts.db).unwrap_or_else(|| {
        error!("font chain: no usable face in the system font database -- no text can be drawn");
        String::new()
    });
    // The scan's faces came from no named file, so only a registered file's path is one a later
    // `load_chain` could collide with.
    fonts
}

/// The family to shape the fallback database against: its sans serif, else any face it holds.
///
/// `None` is a machine with no font files, which `text::shaping::worker::shape` answers by measuring every
/// string as empty -- the shell draws its chrome without text rather than losing the render thread.
fn system_primary(db: &Database) -> Option<String> {
    let query = Query { families: &[Family::SansSerif], ..Default::default() };
    let id = db.query(&query).or_else(|| db.faces().next().map(|face| face.id))?;
    Some(db.face(id)?.families.first()?.0.clone())
}

/// Runs `fc-match` for `name` and returns the file it resolved to plus the family fontconfig
/// actually gave back, or `None` if that doesn't count as a hit.
///
/// fontconfig substitutes rather than failing: `fc-match "ZZ No Such Family"` still exits 0 and
/// returns a real font, so a nonempty result alone can't tell a hit from a miss. The check is
/// whether one of the returned families (comma-separated for a multi-name face) matches what was
/// asked for, case-insensitively -- except `GENERIC_ALIASES`, where substitution is intended and
/// any result counts.
///
/// ponytail: one `fc-match` subprocess per chain entry, roughly three at startup, a few
/// milliseconds total. Linking libfontconfig directly (the `fontconfig-parser` crate fontdb's
/// own `fontconfig` feature already pulls in) is the fix if that ever shows up in a profile.
/// A subprocess is preferred today for two reasons: it adds no new crate to this workspace, and
/// fontconfig's own configuration file is the system's declared font policy -- a better default
/// than "first face in scan order", which is what this whole module replaces.
fn fc_match(name: &str) -> Option<(PathBuf, String)> {
    let (path, families) = fc_match_raw(name)?;

    // `Family:weight=bold` is fontconfig's own pattern syntax and the family is the part before
    // the colon; a bare family name has no colon and is itself.
    let family_asked = name.split(':').next().unwrap_or(name);
    let is_generic = GENERIC_ALIASES.iter().any(|generic| generic.eq_ignore_ascii_case(family_asked));
    let resolved = if is_generic {
        families.split(',').next().unwrap_or(&families)
    } else {
        families.split(',').find(|family| family.trim().eq_ignore_ascii_case(family_asked))?
    };
    Some((path, resolved.trim().to_string()))
}

/// The file `fc-match` answers `pattern` with, and the families that file holds, before any check
/// that the answer is a hit rather than fontconfig's substitution -- which [`covering_file`],
/// asking by codepoint, has no family name to make.
fn fc_match_raw(pattern: &str) -> Option<(PathBuf, String)> {
    let mut command = Command::new("fc-match");
    if let Some(file) = FONTCONFIG_FILE.with_borrow(Clone::clone) {
        command.env("FONTCONFIG_FILE", file);
    }
    let output = command.args(["-f", "%{file}\t%{family}\n", pattern]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (file, families) = stdout.lines().next()?.split_once('\t')?;
    (!file.is_empty()).then(|| (PathBuf::from(file), families.to_string()))
}

/// True if `fc-match` is reachable at all -- gates this module's tests and `text::shaping`'s:
/// print a skip line and return, don't fail a machine or CI image without fontconfig installed.
/// `pub(crate)` since `text::shaping`'s tests need the same gate.
#[cfg(test)]
pub(crate) fn fc_match_available() -> bool {
    Command::new("fc-match").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// True if `fc-list` finds at least one face matching `pattern`: a family name, a
/// `:charset=<hex>` codepoint query, or a family and a codepoint together. Unlike `fc-match`,
/// `fc-list` never substitutes -- an absent family or an uncovered codepoint prints nothing rather
/// than someone else's font -- so this gates a test on what the machine actually has, which varies
/// (no bundled dev font set in this workspace). `pub(crate)` for `fc_match_available`'s reason.
#[cfg(test)]
pub(crate) fn fc_lists(pattern: &str) -> bool {
    Command::new("fc-list").args([pattern, "file"]).output().is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picked because it happens to be installed on machines this runs on, and resolves to
    /// itself rather than a substitute -- both checked via `fc_lists`/`resolve_chain`
    /// rather than assumed. There is no bundled dev font set in this workspace, so a hardcoded
    /// family must be verified, not assumed present.
    const TEST_FAMILY: &str = "Noto Sans Mono";

    #[test]
    fn an_uninstalled_family_is_skipped_rather_than_silently_substituted() {
        if !fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        if !fc_lists(TEST_FAMILY) {
            eprintln!("{TEST_FAMILY:?} not installed on this machine, skip");
            return;
        }
        // If the miss were silently accepted, `primary_family` would be whatever fontconfig
        // picked for the absent name instead of the real hit that follows it.
        let resolved = resolve_chain(&["ZZ No Such Family 9184", TEST_FAMILY], &[]);
        assert_eq!(resolved.primary_family, TEST_FAMILY);
        assert!(!resolved.db.is_empty(), "the real hit's own face should have loaded");
    }

    /// The point of naming a family on a node: it lands in the database the declared chain already
    /// filled, so the chain's own faces stay available as fallback behind it.
    #[test]
    fn a_named_family_loads_into_the_chains_own_database() {
        if !fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        let named = ["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono"]
            .into_iter()
            .find(|family| fc_lists(family) && *family != TEST_FAMILY);
        let (Some(named), true) = (named, fc_lists(TEST_FAMILY)) else {
            eprintln!("no two distinct families installed to tell apart, skip");
            return;
        };
        let mut resolved = resolve_chain(&[TEST_FAMILY], &[]);
        let before = resolved.db.faces().count();

        let hit = load_chain(&mut resolved.db, &[named], &mut resolved.loaded_paths, &[]);
        assert_eq!(hit.as_deref(), Some(named), "the named family resolves to itself, not a substitute");
        assert!(resolved.db.faces().count() > before, "its faces joined the same database");
        for family in [TEST_FAMILY, named] {
            assert!(
                resolved.db.faces().any(|face| face.families.iter().any(|(name, _)| name == family)),
                "{family:?} should be loaded alongside the other"
            );
        }
    }

    /// A family nothing answers is a diagnostic, not a failure: the node draws in the declared
    /// chain, so the database comes back untouched. A family the chain already loaded must not
    /// map its file a second time.
    #[test]
    fn load_chain_leaves_the_database_unchanged_for_known_and_unknown_families() {
        if !fc_match_available() || !fc_lists(TEST_FAMILY) {
            eprintln!("fc-match or {TEST_FAMILY:?} not available, skip");
            return;
        }
        for (family, expected) in [("ZZ No Such Family 9184", None), (TEST_FAMILY, Some(TEST_FAMILY))] {
            let mut resolved = resolve_chain(&[TEST_FAMILY], &[]);
            let before = resolved.db.faces().count();
            assert_eq!(load_chain(&mut resolved.db, &[family], &mut resolved.loaded_paths, &[]).as_deref(), expected);
            assert_eq!(resolved.db.faces().count(), before, "{family:?} must not change the database");
        }
    }

    fn fixture(path: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(path)
    }

    /// Each family's first face, in database order: the order shaping falls back in.
    fn family_order(db: &Database) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for (family, _) in db.faces().filter_map(|face| face.families.first()) {
            if !order.contains(family) {
                order.push(family.clone());
            }
        }
        order
    }

    /// A config's file answers its family, by any case, ahead of an installed copy (Inter Variable
    /// is one on some machines), and no `fc-match` answer joins it. A missing path, a file that is
    /// no font, a FIFO (which would block the open) and a repeated path are skipped without costing
    /// the rest.
    #[test]
    fn a_registered_file_answers_its_family_before_fontconfig() {
        let file = fixture("variable/InterVariable-subset.ttf");
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo.ttf");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("the test needs a real FIFO");
        let once = resolve_chain(&["Inter Variable"], std::slice::from_ref(&file)).db.len();
        let files = [fixture("no-such-font.ttf"), fixture("../Cargo.toml"), fifo, file.clone(), file.clone()];
        let mut resolved = resolve_chain(&["Inter Variable"], &files);
        assert_eq!(resolved.primary_family, "Inter Variable");
        assert_eq!(resolved.db.len(), once, "a repeated path loads once");
        let hit = load_chain(&mut resolved.db, &["inter variable"], &mut resolved.loaded_paths, &resolved.registered);
        assert_eq!(hit.as_deref(), Some("Inter Variable"), "a node's `font` resolves to the registered family");
        assert_eq!(resolved.db.len(), once, "and loads nothing more");
        assert!(once > 0);
        for face in resolved.db.faces() {
            assert_eq!(face_path(face), file, "every face is the fixture's, none fontconfig's");
        }
    }

    /// Database order is fallback order, so a registered family takes its place in the chain, not
    /// the front, and one the chain never names (registered for a node's `font`) goes last.
    #[test]
    fn registered_files_join_at_their_chain_position_and_unnamed_ones_after_it() {
        FONTCONFIG_FILE.set(Some(fixture("shots/fonts/fonts.conf")));
        // The split subset is one fontconfig is told to reject, so only the registration finds it.
        let files = [fixture("variable/InterVariable-subset.ttf"), fixture("shots/fonts/split/Mono-Regular-2500.ttf")];
        let resolved = resolve_chain(&["Noto Sans Symbols 2", "Noto Sans", "Noto Sans Mono"], &files);
        assert_eq!(
            family_order(&resolved.db),
            ["Noto Sans Symbols 2", "Noto Sans", "Noto Sans Mono", "Inter Variable"],
            "Latin text falls back to Noto Sans before a font registered for one node"
        );
    }

    /// `sans-serif` answered with the family a config ships takes the shipped file: the installed
    /// copy's bold must not join, by the chain's variants or by a codepoint rescue.
    #[test]
    fn a_generic_alias_resolving_to_a_registered_family_keeps_out_the_installed_copy() {
        FONTCONFIG_FILE.set(Some(fixture("shots/fonts/fonts.conf")));
        let regular = fixture("shots/fonts/NotoSans-Regular.ttf");
        let mut resolved = resolve_chain(&["sans-serif"], std::slice::from_ref(&regular));
        assert_eq!(resolved.primary_family, "Noto Sans");
        assert_eq!(resolved.db.family_name(&Family::SansSerif), "Noto Sans", "SVG `sans-serif` text finds it too");
        let bold = fixture("shots/fonts/NotoSans-Bold.ttf");
        let landed = load_cover(&mut resolved.db, 'A', bold.clone(), &mut resolved.loaded_paths, &resolved.registered);
        assert!(!landed, "a rescue answered with the installed copy is declined");
        for face in resolved.db.faces() {
            assert_eq!(face_path(face), regular, "the installed bold never loads");
        }
    }

    /// An installed collection holding a registered family brings none of its faces, not even its
    /// other family: the shaper would register the shadowed face with them. A rescue declining it
    /// leaves the file free to decline a later named lookup too, rather than report a hit.
    #[test]
    fn an_installed_collection_holding_a_registered_family_loads_no_face() {
        let dir = tempfile::tempdir().unwrap();
        let collection = dir.path().join("Mono-Symbols2-25CF.ttc");
        std::fs::copy(fixture("shots/fonts/split/Mono-Symbols2-25CF.ttc"), &collection).unwrap();
        let config = dir.path().join("fonts.conf");
        let d = dir.path().display();
        std::fs::write(&config, format!("<fontconfig><dir>{d}</dir><cachedir>{d}/cache</cachedir></fontconfig>"))
            .unwrap();
        FONTCONFIG_FILE.set(Some(config));
        let mono = fixture("shots/fonts/split/Mono-Regular-2500.ttf");
        let mut resolved = resolve_chain(&["Noto Sans Mono", "Noto Sans Symbols 2"], std::slice::from_ref(&mono));
        assert_eq!(family_order(&resolved.db), ["Noto Sans Mono"], "the collection's Symbols 2 is skipped with it");
        let cover = load_cover(&mut resolved.db, '●', collection, &mut resolved.loaded_paths, &resolved.registered);
        assert!(!cover, "a rescue answered with the collection is declined");
        let named =
            load_chain(&mut resolved.db, &["Noto Sans Symbols 2"], &mut resolved.loaded_paths, &resolved.registered);
        assert_eq!(named, None, "a family that never loaded is a miss, not a hit");
        for face in resolved.db.faces() {
            assert_eq!(face_path(face), mono, "only the registered file's face is loaded");
        }
    }

    /// The system scan, the last resort, drops a shadowing file the same way.
    #[test]
    fn pruning_keeps_registered_files_and_drops_every_face_of_a_shadowing_one() {
        let mono = fixture("shots/fonts/split/Mono-Regular-2500.ttf");
        let registered = scan_files(std::slice::from_ref(&mono));
        let mut db = Database::new();
        db.load_font_file(fixture("shots/fonts/split/Mono-Symbols2-25CF.ttc")).unwrap();
        db.load_font_file(fixture("shots/fonts/split/Symbols2-2713.ttf")).unwrap();
        load_file(&mut db, &registered, &mono, &mut HashSet::new());
        prune_shadowed(&mut db, &registered);
        let mut paths: Vec<PathBuf> = db.faces().map(|face| face_path(face).to_path_buf()).collect();
        paths.sort();
        assert_eq!(paths, [mono, fixture("shots/fonts/split/Symbols2-2713.ttf")], "an unrelated file stays");
    }

    /// A machine with no font files at all -- the case `system_fallback` reaches when even the
    /// system scan comes up empty.
    #[test]
    fn a_database_with_no_faces_has_no_primary_family() {
        assert_eq!(system_primary(&Database::new()), None);
    }

    #[test]
    fn the_default_chain_loads_only_its_own_faces_not_the_system_set() {
        if !fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        let resolved = resolve_chain(DEFAULT_CHAIN, &[]);
        // The system has 2648 faces on the machine ADR-0043 measured against; three requests
        // stay well under 100 -- not a fragile exact count.
        assert!(
            resolved.db.faces().count() < 100,
            "expected a small chain-sized database, got {} faces",
            resolved.db.faces().count()
        );
        assert!(!resolved.primary_family.is_empty());
    }
}

//! One `.desktop` file, parsed (ADR-0061). The format needs one group header, seven keys and one
//! tokenizer, so its `Exec` semantics stay here rather than in a dependency.

use std::collections::HashMap;
use std::path::Path;

/// Every `Exec` field code. A launcher has no file, URL or invoking menu item, so all are dropped.
///
/// `%i` expands to `--icon <Icon>` but is dropped too. The app's `Icon=` is already read for
/// config, and passing a second value could make argv and displayed metadata disagree.
const FIELD_CODES: [char; 13] = ['f', 'F', 'u', 'U', 'd', 'D', 'n', 'N', 'i', 'c', 'k', 'v', 'm'];

/// One `[Desktop Entry]` or `[Desktop Action ...]` group with the file format's `\s \n \t \r \\`
/// escapes decoded. `Exec`'s own quoting is a second layer, applied by [`tokenize_exec`].
pub type Group = HashMap<String, String>;

/// A parsed `.desktop` file: its `[Desktop Entry]` group plus any `[Desktop Action <id>]` groups,
/// keeping bare keys and localized keys that match the active locale chain.
#[derive(Debug, Default)]
pub struct DesktopEntry {
    pub main: Group,
    pub actions: HashMap<String, Group>,
}

/// Active message locale from the environment, in POSIX precedence (`LC_ALL`, `LC_MESSAGES`, `LANG`).
pub fn current_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"].into_iter().find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
}

/// Locale fallback tags (`ll_CC@mod`, `ll_CC`, `ll@mod`, `ll`) for a POSIX locale string such as
/// `ar_EG.UTF-8` or `sr_RS.UTF-8@latin`. Returns an empty list for unset, empty, `C`, or `POSIX`,
/// so only bare unlocalized keys match.
pub fn locale_chain(locale: Option<&str>) -> Vec<String> {
    let Some(raw) = locale.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Vec::new();
    };
    let (base, modifier) = match raw.split_once('@') {
        Some((base, modifier)) => (base, Some(modifier).filter(|m| !m.is_empty())),
        None => (raw, None),
    };
    let base = base.split_once('.').map_or(base, |(before, _)| before);
    let (lang, country) = match base.split_once('_') {
        Some((lang, country)) => (lang, Some(country).filter(|c| !c.is_empty())),
        None => (base, None),
    };
    if lang.is_empty() || lang == "C" || lang == "POSIX" {
        return Vec::new();
    }
    let mut tags = Vec::with_capacity(4);
    if let (Some(country), Some(modifier)) = (country, modifier) {
        tags.push(format!("{lang}_{country}@{modifier}"));
    }
    if let Some(country) = country {
        tags.push(format!("{lang}_{country}"));
    }
    if let Some(modifier) = modifier {
        tags.push(format!("{lang}@{modifier}"));
    }
    tags.push(lang.to_string());
    tags
}

/// Resolves a localizable key (`Name`, `GenericName`, `Comment`, `Keywords`, `Icon`) through
/// `locales` in specificity order, falling back to the bare key. Empty values are ignored so a
/// blank translation never hides a fallback.
pub fn localized<'a>(group: &'a Group, key: &str, locales: &[String]) -> Option<&'a str> {
    for tag in locales {
        if let Some(value) = group.get(&format!("{key}[{tag}]")).filter(|value| !value.is_empty()) {
            return Some(value.as_str());
        }
    }
    group.get(key).filter(|value| !value.is_empty()).map(String::as_str)
}

/// Reads `[Desktop Entry]` and every `[Desktop Action <id>]` group out of a `.desktop` file.
///
/// Unrecognized groups, duplicate group headers, and localized keys outside `locales` are skipped
/// so irrelevant translations allocate nothing and later groups never overwrite `[Desktop Entry]`.
pub fn parse_entry(contents: &str, locales: &[String]) -> Option<DesktopEntry> {
    enum Section {
        Ignore,
        Main,
        Action(String),
    }

    let mut main: Option<Group> = None;
    let mut actions: HashMap<String, Group> = HashMap::new();
    let mut section = Section::Ignore;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let header = line.strip_prefix('[').and_then(|s| s.strip_suffix(']'));
            section = match header {
                Some(h) if h.eq_ignore_ascii_case("Desktop Entry") && main.is_none() => {
                    main = Some(Group::new());
                    Section::Main
                }
                Some(h) => match h.strip_prefix("Desktop Action ") {
                    Some(id) if !id.is_empty() && !actions.contains_key(id) => {
                        actions.insert(id.to_string(), Group::new());
                        Section::Action(id.to_string())
                    }
                    _ => Section::Ignore,
                },
                None => Section::Ignore,
            };
            continue;
        }
        let target = match &section {
            Section::Ignore => continue,
            Section::Main => main.as_mut()?,
            Section::Action(id) => actions.get_mut(id)?,
        };
        let Some((key, value)) = line.split_once('=') else {
            continue; // not a key/value line; the specification has no other line shape.
        };
        let key = key.trim();
        if let Some(rest) = key.strip_suffix(']')
            && let Some((_, tag)) = rest.split_once('[')
        {
            if !locales.iter().any(|wanted| wanted == tag) {
                continue;
            }
        } else if key.contains('[') {
            continue;
        }
        target.insert(key.to_string(), unescape(value.trim()));
    }
    main.map(|main| DesktopEntry { main, actions })
}

/// Decodes the value escapes; an unknown `\x` stays as written. Splitting on `\\` first keeps an
/// escaped backslash from joining the letter after it.
fn unescape(value: &str) -> String {
    let decode = |part: &str| part.replace("\\s", " ").replace("\\n", "\n").replace("\\t", "\t").replace("\\r", "\r");
    value.split("\\\\").map(decode).collect::<Vec<_>>().join("\\")
}

/// Whether a key is the specification's `true`; absent is false for `NoDisplay`, `Hidden`, and
/// `Terminal`.
pub fn flag(group: &Group, key: &str) -> bool {
    group.get(key).is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// Splits `Exec` into a command and arguments, applying its quoting rules and dropping field codes.
///
/// Quoting is double-quote only, with backslash escapes inside. Single quotes are literal here,
/// unlike in shells; treating them as quoting would split `Exec=foo 'a b'` into three arguments.
///
/// Returns `None` when field-code removal leaves no command, which is malformed rather than empty.
pub fn tokenize_exec(exec: &str) -> Option<(String, Vec<String>)> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    // Tracks an existing empty token, so `Exec=foo ""` keeps its empty argument; bare spaces do
    // not create a token to flush.
    let mut started = false;
    let mut chars = exec.chars();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if quoted => {
                // Inside quotes, backslash escapes the next character. Push it verbatim: escaping
                // a non-listed character means the literal character either way.
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            ch => {
                current.push(ch);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        tokens.push(current);
    }

    let mut expanded = tokens.into_iter().filter_map(|token| expand_field_codes(&token));
    let command = expanded.next().filter(|command| !command.is_empty())?;
    Some((command, expanded.collect()))
}

/// Removes field codes and unescapes `%%`; drops the token if nothing remains.
///
/// Keeps unknown `%x` as written. The specification lists codes exhaustively, so it is malformed;
/// keeping it lets the app complain instead of receiving a silently changed command line.
///
/// ponytail: mixed tokens such as `--file=%f` keep the other text, yielding `--file=`. Dropping
/// the token would fit that shape but not `%c` embedded in longer text. Neither case is common,
/// and this launcher passes no files, so both spellings are currently unreachable.
fn expand_field_codes(token: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = token.chars();
    let mut had_code = false;
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(code) if FIELD_CODES.contains(&code) => had_code = true,
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    // `Exec=foo ""` is a real empty argument and stays; a token emptied by a field code drops.
    if out.is_empty() && had_code { None } else { Some(out) }
}

/// The path relative to its applications directory, with `/` replaced by `-` and `.desktop`
/// removed.
///
/// Subdirectories are part of the specification and preserve identity: `kde4/konsole.desktop`
/// becomes `kde4-konsole`, distinct from top-level `konsole.desktop`.
pub fn desktop_file_id(path: &Path, base: &Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    let text = relative.to_str()?.strip_suffix(".desktop")?;
    Some(text.replace('/', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[Desktop Action ...]` has its own `Name`/`Exec`; keeping groups separate lets actions be
    /// exposed without overwriting the application's own `Name`/`Exec`.
    #[test]
    fn parse_entry_keeps_desktop_actions_separate_from_the_main_group() {
        let contents = "\
[Desktop Entry]
Name=Firefox
Exec=firefox %u

[X-Extra]
Exec=should-be-ignored

[desktop action new-window]
Name=Wrong Case
Exec=wrong-cmd

[Desktop Action new-window]
Name=New Window
Name[ar]=نافذة جديدة
Exec=firefox --new-window
Icon=window-new

[Desktop Entry]
Name=Duplicate

[Desktop Action new-window]
Name=Duplicate Action
";
        let locales = locale_chain(Some("ar_EG.UTF-8"));
        let entry = parse_entry(contents, &locales).expect("the file has a [Desktop Entry] group");
        assert_eq!(localized(&entry.main, "Name", &locales), Some("Firefox"));
        assert_eq!(
            entry.main.get("Exec").map(String::as_str),
            Some("firefox %u"),
            "the action's Exec must not overwrite the application's"
        );
        let action = entry.actions.get("new-window").expect("action group parsed");
        assert_eq!(localized(action, "Name", &locales), Some("نافذة جديدة"));
        assert_eq!(action.get("Exec").map(String::as_str), Some("firefox --new-window"));
        assert_eq!(action.get("Icon").map(String::as_str), Some("window-new"));
        assert_eq!(entry.actions.len(), 1);
    }

    #[test]
    fn locale_chain_follows_the_desktop_entry_fallback_order() {
        assert_eq!(locale_chain(Some("sr_RS.UTF-8@latin")), ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]);
        assert_eq!(locale_chain(Some("ar_EG.UTF-8")), ["ar_EG", "ar"]);
        assert_eq!(locale_chain(Some("ca@valencia")), ["ca@valencia", "ca"]);
        assert_eq!(locale_chain(Some("de")), ["de"]);
        for neutral in [None, Some(""), Some("C"), Some("C.UTF-8"), Some("POSIX"), Some(".UTF-8")] {
            assert!(locale_chain(neutral).is_empty(), "{neutral:?}");
        }
    }

    #[test]
    fn parse_entry_resolves_localized_keys_skips_other_locales_and_needs_a_desktop_entry_group() {
        let contents = "\
[Desktop Entry]
# a comment

Name=Thing
Name[sr]=Sr
Name[sr@latin]=SrLatin
Name[sr_RS]=SrRS
Name[sr_RS@latin]=SrRSLatin
Name[de]=Ding
Comment=Fallback
Comment[sr_RS@latin]=
";
        let full = locale_chain(Some("sr_RS.UTF-8@latin"));
        let entry = parse_entry(contents, &full).unwrap();
        assert_eq!(localized(&entry.main, "Name", &full), Some("SrRSLatin"));
        assert_eq!(localized(&entry.main, "Comment", &full), Some("Fallback"), "empty translation falls back");
        assert!(!entry.main.contains_key("Name[de]"), "unrelated locales are skipped");

        let country = locale_chain(Some("sr_RS.UTF-8"));
        let entry = parse_entry(contents, &country).unwrap();
        assert_eq!(localized(&entry.main, "Name", &country), Some("SrRS"));

        let modifier = locale_chain(Some("sr@latin"));
        let entry = parse_entry(contents, &modifier).unwrap();
        assert_eq!(localized(&entry.main, "Name", &modifier), Some("SrLatin"));

        let lang = locale_chain(Some("sr_BA.UTF-8"));
        let entry = parse_entry(contents, &lang).unwrap();
        assert_eq!(localized(&entry.main, "Name", &lang), Some("Sr"));

        let neutral = locale_chain(Some("C.UTF-8"));
        let entry = parse_entry(contents, &neutral).unwrap();
        assert_eq!(localized(&entry.main, "Name", &neutral), Some("Thing"));
        assert_eq!(entry.main.len(), 2);

        assert!(parse_entry("[Desktop Action foo]\nName=x\n", &[]).is_none());
    }

    /// A Wine launcher writes `\\\\` for each backslash of its Windows path: the value layer
    /// halves it, then `Exec`'s quoting halves it again.
    #[test]
    fn parse_entry_decodes_value_escapes_before_exec_quoting() {
        let entry = parse_entry("[Desktop Entry]\nName=a\\sb\\\\n\nExec=wine \"C:\\\\\\\\x\" \\q\n", &[]).unwrap();
        assert_eq!(entry.main.get("Name").map(String::as_str), Some("a b\\n"));
        assert_eq!(
            tokenize_exec(&entry.main["Exec"]),
            Some(("wine".to_string(), vec![r"C:\x".to_string(), r"\q".to_string()]))
        );
    }

    #[test]
    fn flag_reads_the_specifications_true_and_treats_an_absent_key_as_false() {
        let entry = parse_entry("[Desktop Entry]\nNoDisplay=true\nHidden=false\n", &[]).unwrap();
        assert!(flag(&entry.main, "NoDisplay"));
        assert!(!flag(&entry.main, "Hidden"));
        assert!(!flag(&entry.main, "Terminal"), "an absent boolean key is false, not an error");
    }

    #[test]
    fn tokenize_exec_follows_desktop_entry_quoting_and_drops_field_codes() {
        let argv =
            |command: &str, args: &[&str]| Some((command.to_string(), args.iter().map(|a| a.to_string()).collect()));
        // Only double quotes quote; field codes vanish (no file/URL to give); an unknown `%` escape stays visible.
        for (exec, expected) in [
            ("kitty --single-instance", argv("kitty", &["--single-instance"])),
            (r#"prog "one two" 'three"#, argv("prog", &["one two", "'three"])),
            (r#"prog "a\"b""#, argv("prog", &[r#"a"b"#])),
            ("firefox %u", argv("firefox", &[])),
            ("gimp %U %f %F", argv("gimp", &[])),
            ("prog 100%%", argv("prog", &["100%"])),
            ("prog %z", argv("prog", &["%z"])),
            (r#"prog "" %f"#, argv("prog", &[""])),
            ("prog --file=%f", argv("prog", &["--file="])),
            ("%f", None),
            ("   ", None),
            (r#""""#, None),
            (r#"prog "unterminated"#, None),
        ] {
            assert_eq!(tokenize_exec(exec), expected, "{exec:?}");
        }
    }

    #[test]
    fn desktop_file_id_turns_a_subdirectory_into_a_dash_and_rejects_other_files() {
        let base = Path::new("/usr/share/applications");
        for (path, id) in [
            ("/usr/share/applications/kde4/konsole.desktop", Some("kde4-konsole")),
            ("/usr/share/applications/firefox.desktop", Some("firefox")),
            ("/usr/share/applications/mimeinfo.cache", None),
        ] {
            assert_eq!(desktop_file_id(Path::new(path), base), id.map(str::to_string), "{path}");
        }
    }
}

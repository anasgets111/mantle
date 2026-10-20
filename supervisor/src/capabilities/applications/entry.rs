//! One `.desktop` file, parsed (ADR-0061). The format needs one group header, seven keys and one
//! tokenizer, so its `Exec` semantics stay here rather than in a dependency.

use std::collections::HashMap;
use std::path::Path;

/// Every `Exec` field code. A launcher has no file, URL or invoking menu item, so all are dropped.
///
/// `%i` expands to `--icon <Icon>` but is dropped too. The app's `Icon=` is already read for
/// config, and passing a second value could make argv and displayed metadata disagree.
const FIELD_CODES: [char; 13] = ['f', 'F', 'u', 'U', 'd', 'D', 'n', 'N', 'i', 'c', 'k', 'v', 'm'];

/// `[Desktop Entry]` keys as written. Only `Exec` is unescaped by [`tokenize_exec`]; other keys
/// are display strings passed through verbatim.
pub type Group = HashMap<String, String>;

/// Reads the `[Desktop Entry]` group out of a `.desktop` file's contents.
///
/// Stops at the next group header. `[Desktop Action new-window]` groups commonly follow the main
/// group; flattening them could let an action's `Exec` replace the application's and launch a new
/// window instead of the app.
///
/// Skips localized keys (`Name[de]`), so `Name` stays the unlocalized value, English on a
/// localized system. ponytail: locale lookup is the upgrade, using `$LC_MESSAGES`/`$LANG` with
/// `ll_CC`, then `ll`, then bare. A partial lookup could silently choose the wrong region.
pub fn parse_group(contents: &str) -> Option<Group> {
    let mut group = Group::new();
    let mut inside = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if inside {
                break; // the next group starts here -- see this function's doc comment.
            }
            inside = line.eq_ignore_ascii_case("[Desktop Entry]");
            continue;
        }
        if !inside {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue; // not a key/value line; the specification has no other line shape.
        };
        let key = key.trim();
        if key.contains('[') {
            continue; // a localized key, skipped deliberately.
        }
        group.insert(key.to_string(), value.trim().to_string());
    }
    inside.then_some(group)
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
    let mut chars = exec.chars().peekable();

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
    if started {
        tokens.push(current);
    }

    let mut expanded = tokens.into_iter().filter_map(|token| expand_field_codes(&token));
    let command = expanded.next()?;
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
    let mut chars = token.chars().peekable();
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

    /// `[Desktop Action ...]` has its own `Name`/`Exec`; flattening the file lets the last group
    /// win and opens an app window instead of starting the app.
    #[test]
    fn parse_group_stops_at_the_next_group_header() {
        let contents = "\
[Desktop Entry]
Name=Firefox
Exec=firefox %u

[Desktop Action new-window]
Name=New Window
Exec=firefox --new-window
";
        let group = parse_group(contents).expect("the file has a [Desktop Entry] group");
        assert_eq!(group.get("Name").map(String::as_str), Some("Firefox"));
        assert_eq!(
            group.get("Exec").map(String::as_str),
            Some("firefox %u"),
            "the action's Exec must not overwrite the application's"
        );
    }

    #[test]
    fn parse_group_skips_localized_keys_comments_and_blank_lines_and_needs_a_desktop_entry_group() {
        let group = parse_group("[Desktop Entry]\n# a comment\n\nName=Thing\nName[de]=Ding\n").unwrap();
        assert_eq!(group.get("Name").map(String::as_str), Some("Thing"), "always the unlocalized value");
        assert_eq!(group.len(), 1);
        assert!(parse_group("[Desktop Action foo]\nName=x\n").is_none());
    }

    #[test]
    fn flag_reads_the_specifications_true_and_treats_an_absent_key_as_false() {
        let group = parse_group("[Desktop Entry]\nNoDisplay=true\nHidden=false\n").unwrap();
        assert!(flag(&group, "NoDisplay"));
        assert!(!flag(&group, "Hidden"));
        assert!(!flag(&group, "Terminal"), "an absent boolean key is false, not an error");
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

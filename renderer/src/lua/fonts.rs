//! `fonts { ... }` declares the font families used for measurement and paint (ADR-0043 decision 2).
//!
//! It records in `Lua::app_data` (`shell.lua` returns surfaces, so there is no slot for a chain) and is
//! read after config evaluation. Not a `lua::signal`: this is a plain, non-reactive list read once.
//!
//! The chain is the fallback every node uses unless it names a family itself: Parley picks each
//! glyph's face across every loaded family (ADR-0211), so CJK and emoji sit behind a sans body face.
//! A `text` node's `font` leads with its family and keeps this chain behind it (ADR-0144).
//!
//! `files` registers font files a config ships, so a self-contained config needs no install.

use std::path::{Path, PathBuf};

use mlua::Lua;

use super::luacats::{As, spelled};

/// The declared chain in config order and the font files to register, absent until `fonts` is called.
#[derive(Default)]
struct FontRegistry(Vec<String>, Vec<PathBuf>);

/// The config directory a relative `files` path resolves against.
struct FilesRoot(PathBuf);

/// `fonts`' argument: family names, plus an optional `files` list.
struct Chain;

spelled!(Chain => r#"{ [integer]: string, files?: string[], [string]: "no such property" }"#);

/// Registers `fonts(chain)`, resolving relative `files` against `config_dir`. Last call wins:
/// appending would build a chain neither call asked for.
pub fn register(lua: &Lua, config_dir: &Path) -> mlua::Result<()> {
    lua.set_app_data(FilesRoot(config_dir.to_path_buf()));
    super::luacats::lua_fn!(
        lua,
        /// Declares the font fallback chain. Each glyph falls back across it, so one chain covers body text,
        /// CJK and emoji; a `text` node's `font` goes in front of it (ADR-0144). Read once at startup: a
        /// change needs a shell restart (ADR-0043). The last call wins. `files` registers font files, so
        /// `fonts { files = { "fonts/Caveat.ttf" }, "Caveat" }` needs no install.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#fonts)
        fn fonts(
            lua,
            /// Family names. The first is the body face and the only one whose bold and italic load; the rest
            /// cover glyphs it lacks. A hole, a named key other than `files` or a non-string raises; an uninstalled family is
            /// skipped and logged at `-vvv`. Never calling it keeps `sans-serif`, Noto Sans CJK JP, Noto Color
            /// Emoji. `files`: paths relative to the config directory, absolute or `~/`, whose families
            /// resolve to these files before fontconfig; a file that loads no face is skipped with a warning.
            chain: As<mlua::Table, Chain>,
        ) {
            // A copy without `files`, so the list check still refuses every other named key.
            let list = lua.create_table()?;
            for pair in chain.0.pairs::<mlua::Value, mlua::Value>() {
                let (key, value) = pair?;
                if key.as_string().is_none_or(|key| key != "files") {
                    list.raw_set(key, value)?;
                }
            }
            let mut files = Vec::new();
            match chain.0.raw_get::<mlua::Value>("files")? {
                mlua::Value::Nil => {}
                mlua::Value::Table(paths) => {
                    let paths = super::marshal::list_entries(&paths)
                        .map_err(|detail| mlua::Error::runtime(format!("fonts: files must be a list of paths: {detail}")))?;
                    for (index, path) in (1..).zip(paths) {
                        let mlua::Value::String(path) = path else {
                            let got = crate::layout::node::preview_for_error(&path);
                            return Err(mlua::Error::runtime(format!("fonts: files entry {index} must be a path string, got {got}")));
                        };
                        let path = super::marshal::expand_path(&format!("fonts: files entry {index}"), &path.to_str()?)?;
                        let root = lua.app_data_ref::<FilesRoot>().map(|root| root.0.clone()).unwrap_or_default();
                        files.push(root.join(path));
                    }
                }
                other => {
                    let got = crate::layout::node::preview_for_error(&other);
                    return Err(mlua::Error::runtime(format!("fonts: files must be a list of paths, got {got}")));
                }
            }
            let entries = super::marshal::list_entries(&list)
                .map_err(|detail| mlua::Error::runtime(format!("fonts: chain must be a list of names: {detail}")))?;
            let mut families = Vec::with_capacity(entries.len());
            for (index, entry) in (1..).zip(entries) {
                // Check before mlua's `FromLua`, which coerces numbers like Lua. Without this,
                // `fonts { 12 }` records family `"12"`; only `resolve_chain` then reports a
                // missing font, hiding the config error.
                let mlua::Value::String(family) = entry else {
                    let got = crate::layout::node::preview_for_error(&entry);
                    return Err(mlua::Error::runtime(format!("fonts: entry {index} must be a family string, got {got}")));
                };
                families.push(family.to_string_lossy());
            }
            lua.set_app_data(FontRegistry(families, files));
            Ok(())
        }
    )
}

/// What the config declared: the chain, empty to keep `text::fonts::DEFAULT_CHAIN`, and the files.
// A chain change invalidates every measurement, unlike reload's in-place restyle.
// ponytail: read once at startup, a live edit is ignored until restart; upgrade: rebuild the `TextPainter` via a new `FrameOutcome` variant.
pub fn declared_chain(lua: &Lua) -> (Vec<String>, Vec<PathBuf>) {
    lua.app_data_ref::<FontRegistry>().map(|registry| (registry.0.clone(), registry.1.clone())).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::tests::lua_registered;

    fn registered(lua: &Lua) -> mlua::Result<()> {
        register(lua, Path::new("/cfg"))
    }

    #[test]
    fn a_config_that_never_mentions_fonts_declares_no_chain() {
        assert_eq!(declared_chain(&lua_registered(registered)), (vec![], vec![]));
    }

    #[test]
    fn a_declared_chain_is_read_back_in_the_order_it_was_written() {
        let lua = lua_registered(registered);
        lua.load(r#"fonts { "CaskaydiaCove Nerd Font Propo", "Noto Sans", "Noto Color Emoji" }"#).exec().unwrap();
        assert_eq!(
            declared_chain(&lua).0,
            vec!["CaskaydiaCove Nerd Font Propo", "Noto Sans", "Noto Color Emoji"],
            "order is the fallback order, so it has to survive the round trip"
        );
    }

    #[test]
    fn files_resolve_against_the_config_directory_unless_absolute_or_under_home() {
        let lua = lua_registered(registered);
        lua.load(r#"fonts { files = { "fonts/Caveat.ttf", "/abs/A.ttf", "~/fonts/B.otf" }, "Caveat", "Noto Sans" }"#)
            .exec()
            .unwrap();
        let (chain, files) = declared_chain(&lua);
        assert_eq!(chain, vec!["Caveat", "Noto Sans"], "`files` is not a chain entry");
        assert_eq!(
            files,
            vec![
                PathBuf::from("/cfg/fonts/Caveat.ttf"),
                PathBuf::from("/abs/A.ttf"),
                shared::expand_home("~/fonts/B.otf").unwrap()
            ]
        );
    }

    /// `sequence_values` stops at the first `nil`, so `{ [1] = "A", [3] = "C" }` once recorded
    /// `["A"]` and shortened the fallback chain. The key walk catches named keys, which are not
    /// array entries. mlua's `FromLua<String>` coerces numbers, so `12` once became family `"12"`.
    #[test]
    fn a_malformed_chain_is_refused_and_records_nothing() {
        for (call, needle) in [
            (r#"local t = {} t[1] = "A" t[3] = "C" fonts(t)"#, "entry 2"),
            (r#"fonts { family = "Noto Sans" }"#, "key `family` is not a list index"),
            (r#"fonts { "Noto Sans", 12 }"#, "entry 2"),
            (r#"fonts("Noto Sans")"#, "expected a table, got string"),
            (r#"fonts { files = "a.ttf" }"#, "files must be a list of paths, got string"),
            (r#"fonts { files = { 1 } }"#, "files entry 1 must be a path string"),
        ] {
            let lua = lua_registered(registered);
            let err = lua.load(call).exec().unwrap_err().to_string();
            assert!(err.contains(needle), "{call}: the message must name the problem: {err}");
            assert_eq!(declared_chain(&lua), (vec![], vec![]), "{call}: a refused call records nothing");
        }
    }

    #[test]
    fn the_last_declaration_wins_rather_than_the_two_being_appended() {
        let lua = lua_registered(registered);
        lua.load(r#"fonts { "A" } fonts { "B" }"#).exec().unwrap();
        assert_eq!(declared_chain(&lua).0, vec!["B"]);
    }
}

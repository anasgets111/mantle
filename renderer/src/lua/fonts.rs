//! `fonts { ... }` declares the font families used for measurement and paint (ADR-0043 decision 2).
//!
//! It records in `Lua::app_data` (`shell.lua` returns surfaces, so there is no slot for a chain) and is
//! read after config evaluation. Not a `lua::signal`: this is a plain, non-reactive list read once.
//!
//! The chain is the fallback every node uses unless it names a family itself: Parley picks each
//! glyph's face across every loaded family (ADR-0211), so CJK and emoji sit behind a sans body face.
//! A `text` node's `font` leads with its family and keeps this chain behind it (ADR-0144).

use mlua::Lua;

/// The declared chain in config order, absent until `fonts` is called.
#[derive(Default)]
struct FontRegistry(Vec<String>);

/// Registers `fonts(chain)`. Last call wins: appending would build a chain neither call asked for.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    super::luacats::lua_fn!(
        lua,
        /// Declares the font fallback chain. Each glyph falls back across it, so one chain covers body text,
        /// CJK and emoji; a `text` node's `font` goes in front of it (ADR-0144). Read once at startup: a
        /// change needs a shell restart (ADR-0043). The last call wins.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#fonts)
        fn fonts(
            lua,
            /// Family names. The first is the body face and the only one whose bold and italic load; the rest
            /// cover glyphs it lacks. A hole, a named key or a non-string raises; an uninstalled family is
            /// skipped and logged at `-vvv`. Never calling it keeps `sans-serif`, Noto Sans CJK JP, Noto Color
            /// Emoji.
            chain: super::luacats::As<mlua::Table, Vec<String>>,
        ) {
            let entries = super::marshal::list_entries(&chain.0)
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
            lua.set_app_data(FontRegistry(families));
            Ok(())
        }
    )
}

/// What the config declared; empty keeps `text::fonts::DEFAULT_CHAIN`.
// A chain change invalidates every measurement, unlike reload's in-place restyle.
// ponytail: read once at startup, a live edit is ignored until restart; upgrade: rebuild the `TextPainter` via a new `FrameOutcome` variant.
pub fn declared_chain(lua: &Lua) -> Vec<String> {
    lua.app_data_ref::<FontRegistry>().map(|registry| registry.0.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::tests::lua_registered;

    #[test]
    fn a_config_that_never_mentions_fonts_declares_no_chain() {
        assert!(declared_chain(&lua_registered(register)).is_empty());
    }

    #[test]
    fn a_declared_chain_is_read_back_in_the_order_it_was_written() {
        let lua = lua_registered(register);
        lua.load(r#"fonts { "CaskaydiaCove Nerd Font Propo", "Noto Sans", "Noto Color Emoji" }"#).exec().unwrap();
        assert_eq!(
            declared_chain(&lua),
            vec!["CaskaydiaCove Nerd Font Propo", "Noto Sans", "Noto Color Emoji"],
            "order is the fallback order, so it has to survive the round trip"
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
        ] {
            let lua = lua_registered(register);
            let err = lua.load(call).exec().unwrap_err().to_string();
            assert!(err.contains(needle), "{call}: the message must name the problem: {err}");
            assert!(declared_chain(&lua).is_empty(), "{call}: a refused call records nothing");
        }
    }

    #[test]
    fn the_last_declaration_wins_rather_than_the_two_being_appended() {
        let lua = lua_registered(register);
        lua.load(r#"fonts { "A" } fonts { "B" }"#).exec().unwrap();
        assert_eq!(declared_chain(&lua), vec!["B"]);
    }
}

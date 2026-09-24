//! Leaf-node content value types: text, runs, fonts and captures. None affect the box model;
//! geometry types live beside them rather than here. [`paint_style`](super::paint_style()) reads
//! them once per node per pass.

use std::ops::Range;
use std::sync::Arc;

use mlua::Value;

use crate::text::shaping::FontRun;
use crate::text::snap::LogicalRect;

use super::prop::keywords;
use super::*;
use crate::lua::luacats::lua_shape;

/// A stretch of a `text`'s content drawn differently from the rest (ADR-0104): in the chain's
/// bold and/or italic face, underlined, or in its own colour. Ranges are bytes into the node's
/// `content` string, in order and non-overlapping, and `layout::scene` remaps them when a wrap or
/// an elide rewrites that string. A `text` whose content is one plain string has none.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleRun {
    pub range: Range<usize>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub color: Option<Rgba>,
    /// What a press on this run hands the node's `on_link` (ADR-0106). Carried, never opened: the
    /// engine knows which run was pressed and nothing about URLs.
    pub href: Option<String>,
}

/// The bold/italic subset of `runs` in the form the shaper needs. Underline and colour do not
/// change shaping, so contents differing only by underline or colour share a memo entry.
pub fn font_runs(runs: &[StyleRun]) -> Vec<FontRun> {
    runs.iter()
        .filter(|run| run.bold || run.italic)
        .map(|run| FontRun { range: run.range.clone(), bold: run.bold, italic: run.italic })
        .collect()
}

/// `text.content`: one string or notification-body-style runs (ADR-0104), joining run text and
/// preserving each run's style. The run shape is a body span minus `kind`, so text spans can stream
/// through; the caller filters image spans, which have no `text`.
///
/// Absent `content` is empty (ADR-0044 decision 1): before the first `StateSnapshot`, a capability
/// signal reads `nil`, and `run_startup_evaluation` runs before the poll loop drains one. A typo in
/// `content` therefore renders an empty node; `mantle.rescue` covers the important failures.
pub(crate) struct Content;

spelled!(Content => format!("{}|{}", String::lua(), Vec::<TextRun>::lua()));

lua_shape! {
    /// One styled stretch of `text.content` (ADR-0104). A notification body's text spans fit as-is;
    /// drop image spans, which have no `text` and are refused.
    #[class = "TextRun"]
    pub(crate) struct TextRun {
        /// Empty runs are skipped.
        text: String,
        /// Uses the family's bold face when fontconfig has one.
        bold?: bool,
        /// Uses the family's italic face when fontconfig has one.
        italic?: bool,
        /// Underline in the run's colour.
        underline?: bool,
        /// Overrides the node's `foreground`.
        color: Option<Rgba>,
        /// Passed to the node's `on_link` when clicked; never opened by the engine (ADR-0106).
        href: Option<String>,
        /// A notification text span's, so one passes through; not read.
        kind?: () as SpanKind,
    }
}

keywords! {
    /// A [`TextRun`]'s `kind`: the notification span it may be.
    #[derive(Clone, Copy, PartialEq)]
    #[cfg_attr(not(test), expect(dead_code, reason = "spelled by the stubs, a test"))]
    pub(crate) enum SpanKind {
        Text = "text",
    }
}

impl Prop for Content {
    type Out = (String, Vec<StyleRun>);
    fn read(_: &Property, value: Option<&Value>) -> Result<(String, Vec<StyleRun>), LayoutError> {
        match value {
            None => Ok((String::new(), Vec::new())),
            Some(Value::String(s)) => Ok((checked_string("content", s)?, Vec::new())),
            Some(Value::Table(runs)) => parse_runs(runs),
            Some(other) => Err(invalid(
                "content",
                format!("expected a string or an array of runs, got {}", preview_for_error(other)),
            )),
        }
    }
}

fn parse_runs(runs: &mlua::Table) -> Result<(String, Vec<StyleRun>), LayoutError> {
    let mut content = String::new();
    let mut styles = Vec::new();
    for (position, run) in runs.sequence_values::<Value>().enumerate() {
        if styles.len() == MAX_ARRAY_ELEMENTS {
            return Err(invalid("content", format!("more than {MAX_ARRAY_ELEMENTS} runs in one text node")));
        }
        let index = position + 1;
        let run = run.map_err(|e| invalid("content", format!("run {index}: {e}")))?;
        let Value::Table(run) = run else {
            return Err(invalid("content", format!("run {index}: expected a table, got {}", preview_for_error(&run))));
        };
        let text = match run.get::<Value>("text") {
            Ok(Value::String(s)) => checked_string("content", &s)?,
            Ok(Value::Nil) => {
                return Err(invalid(
                    "content",
                    format!("run {index} has no `text` -- an image span has no place in a line of text, leave it out"),
                ));
            }
            Ok(other) => {
                return Err(invalid(
                    "content",
                    format!("run {index}: expected `text` to be a string, got {}", preview_for_error(&other)),
                ));
            }
            Err(e) => return Err(invalid("content", format!("run {index}: {e}"))),
        };
        // After `text`, so an image span gets the message above.
        crate::lua::marshal::only_keys(&run, TextRun::KEYS)
            .map_err(|detail| invalid("content", format!("run {index}: {detail}")))?;
        let flag = |key: &str| -> Result<bool, LayoutError> {
            match run.get::<Value>(key) {
                Ok(Value::Nil) => Ok(false),
                Ok(Value::Boolean(b)) => Ok(b),
                Ok(other) => Err(invalid(
                    "content",
                    format!("run {index}: expected `{key}` to be a boolean, got {}", preview_for_error(&other)),
                )),
                Err(e) => Err(invalid("content", format!("run {index}: {e}"))),
            }
        };
        let (bold, italic, underline) = (flag("bold")?, flag("italic")?, flag("underline")?);
        let color = match run.get::<Value>("color") {
            Ok(Value::Nil) => None,
            Ok(Value::String(s)) => Some(parse_hex_color("content", &checked_string("content", &s)?)?),
            Ok(other) => {
                return Err(invalid(
                    "content",
                    format!("run {index}: expected `color` to be a hex string, got {}", preview_for_error(&other)),
                ));
            }
            Err(e) => return Err(invalid("content", format!("run {index}: {e}"))),
        };
        let href = match run.get::<Value>("href") {
            Ok(Value::Nil) => None,
            Ok(Value::String(s)) => Some(checked_string("content", &s)?).filter(|href| !href.is_empty()),
            Ok(other) => {
                return Err(invalid(
                    "content",
                    format!("run {index}: expected `href` to be a string, got {}", preview_for_error(&other)),
                ));
            }
            Err(e) => return Err(invalid("content", format!("run {index}: {e}"))),
        };
        let run = TextRun { text, bold, italic, underline, color, href, kind: () };
        if run.text.is_empty() {
            continue;
        }
        let start = content.len();
        content.push_str(&run.text);
        let TextRun { bold, italic, underline, color, href, kind: (), .. } = run;
        if bold || italic || underline || color.is_some() || href.is_some() {
            styles.push(StyleRun { range: start..content.len(), bold, italic, underline, color, href });
        }
    }
    Ok((content, styles))
}

/// `capture.live` (ADR-0248, ADR-0263): frames per second in (0, 1000], `true` uncapped
/// (infinite), `false` or absent one-shot (`None`).
pub(crate) struct Live;

spelled!(Live => format!("{}|{}", bool::lua(), f32::lua()));

impl Prop for Live {
    type Out = Option<f32>;
    fn read(_: &Property, value: Option<&Value>) -> Result<Option<f32>, LayoutError> {
        match value {
            None | Some(Value::Boolean(false)) => Ok(None),
            Some(Value::Boolean(true)) => Ok(Some(f32::INFINITY)),
            Some(value) => match value_as_f32("live", value)? {
                Some(fps) if fps > 0.0 && fps <= 1000.0 => Ok(Some(fps)),
                _ => {
                    let got = preview_for_error(value);
                    Err(invalid("live", format!("expected a boolean or frames per second in (0, 1000], got {got}")))
                }
            },
        }
    }
}

/// `capture.region` (ADR-0263): `{ x, y, width, height }` in the output's logical pixels, every
/// key required and within the row's range, the size positive.
pub(crate) struct Region;

spelled!(Region => LogicalRect::lua());

impl Prop for Region {
    type Out = Option<LogicalRect>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<LogicalRect>, LayoutError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let Value::Table(table) = value else {
            let got = preview_for_error(value);
            return Err(invalid("region", format!("expected an {{ x, y, width, height }} table, got {got}")));
        };
        only_keys("region", table, LogicalRect::KEYS)?;
        let field = |key| {
            style::table_number("region", table, key)?
                .ok_or_else(|| invalid("region", format!("`{key}` is required")))
                .and_then(|n| prop::within(row, n))
        };
        let region = LogicalRect { x: field("x")?, y: field("y")?, width: field("width")?, height: field("height")? };
        if region.width == 0.0 || region.height == 0.0 {
            return Err(invalid("region", "`width` and `height` must be positive"));
        }
        Ok(Some(region))
    }
}

keywords! {
    /// `TextAlign` places glyphs inside the node's box, unlike `align_h`, which places the node in its
    /// parent; it matters only when the box is wider than the measured text.
    ///
    /// Its own type rather than reusing [`super::Align`]: that carries `Stretch`, which would
    /// be meaningless here since a run of glyphs has no size to force.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum TextAlign {
        #[default]
        Start,
        Center,
        End,
    }
}

impl TextAlign {
    /// The x a line `width` wide starts at in `x0..x1`; `Start` and `End` follow `rtl` (ADR-0211).
    pub fn line_left(self, rtl: bool, x0: f32, x1: f32, width: f32) -> f32 {
        match (self, rtl) {
            (TextAlign::Center, _) => (x0 + x1 - width) / 2.0,
            (TextAlign::Start, false) | (TextAlign::End, true) => x0,
            (TextAlign::Start, true) | (TextAlign::End, false) => x1 - width,
        }
    }
}

/// `font`: the font family this node measures and paints in, as the config wrote it
/// (ADR-0144). Absent -- which is most nodes -- means the chain `fonts { ... }` declared.
///
/// A family name rather than a fixed set of roles, because a Nerd-Font-patched body family carries
/// the private-use icon block itself and always wins per-glyph fallback: no chain ordering reaches
/// a second family that also has those codepoints, so the node has to name one. The same mechanism
/// then covers a heading face or a monospaced readout without new IDL.
///
/// `Arc<str>` rather than `String`: this is cloned into a measurement cache key, a display-list
/// command and a paint call for every text node every pass, and the string is a theme constant
/// repeated across dozens of nodes.
///
/// The name is not validated here. Parsing sees the property, not the loaded font set -- and the
/// set is not fixed at parse time, since a family is resolved on first sight. An unresolvable name
/// draws in the declared chain and logs it once at `-vvv`, the same bargain `fonts { ... }` already
/// makes for a chain entry nothing on the system answers.
pub(crate) struct Font;

spelled!(Font => String::lua());

impl Prop for Font {
    type Out = Option<Arc<str>>;
    fn read(_: &Property, value: Option<&Value>) -> Result<Option<Arc<str>>, LayoutError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let Value::String(s) = value else {
            return Err(invalid("font", format!("must be a family name string, got {}", preview_for_error(value))));
        };
        let family = checked_string("font", s)?;
        // An empty string is a config bug that would otherwise look like "no family named", and the
        // node would silently draw in the declared chain with nothing to point at.
        if family.is_empty() {
            return Err(invalid("font", "must be a family name, got an empty string".to_string()));
        }
        Ok(Some(Arc::from(family)))
    }
}

keywords! {
    /// What to do with text too wide for its box. Only `"End"` is offered: middle elision needs a
    /// grapheme budget across runs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Elide {
        /// Let the clip cut it off.
        #[default]
        None,
        /// Drop trailing characters and finish with a single-character ellipsis.
        End,
    }
}

keywords! {
    /// Whether oversized text breaks onto another line. It composes with `elide`: `wrap = "Word"`
    /// and `elide = "End"` fills the allowed lines, then ellipsizes the last one. The default,
    /// `None`, measures one line, so a fixed-width `text` reserves the height it paints.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Wrap {
        /// One line, however long.
        #[default]
        None,
        /// Break at word boundaries, falling back to a glyph boundary for a word wider than the box,
        /// using cosmic-text's `Wrap::WordOrGlyph`.
        Word,
    }
}

/// `max_lines` is uncapped when absent or `0`; zero lets signal-driven values spell "absent"
/// because `Bound` cannot. Negatives error rather than being clamped, which would hide a sign
/// mistake in config arithmetic. It is consulted only under `wrap = "Word"`, so setting both
/// unconditionally is safe.
pub(crate) struct MaxLines;

spelled!(MaxLines => f32::lua());

impl Prop for MaxLines {
    type Out = Option<usize>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<usize>, LayoutError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let n = value_as_f32(row.name, value)?
            .ok_or_else(|| invalid(row.name, format!("expected a number, got {}", preview_for_error(value))))?;
        if n < 0.0 {
            return Err(invalid(row.name, format!("must not be negative, got {n}")));
        }
        Ok((n >= 1.0).then_some(n as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Fit;
    use crate::lua::nodes::deserialize_lua_table;

    /// `Start` and `End` are the line's own reading direction (ADR-0211); `Center` is either way, and
    /// an odd-width box centres on its true half pixel.
    #[test]
    fn start_and_end_follow_a_lines_reading_direction() {
        assert_eq!(TextAlign::Start.line_left(false, 10.0, 90.0, 20.0), 10.0);
        assert_eq!(TextAlign::Start.line_left(true, 10.0, 90.0, 20.0), 70.0);
        assert_eq!(TextAlign::End.line_left(false, 10.0, 90.0, 20.0), 70.0);
        assert_eq!(TextAlign::End.line_left(true, 10.0, 90.0, 20.0), 10.0);
        assert_eq!(TextAlign::Center.line_left(true, 0.0, 15.0, 6.0), 4.5);
    }

    #[test]
    fn text_content_absent_defaults_to_the_empty_string() {
        let props = PropMap::default();
        assert_eq!(fields::text::content.read(&props).unwrap().0, "");
    }

    #[test]
    fn a_signal_resolving_to_a_string_satisfies_content() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let hello = lua.create_string("hello").unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(hello), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("content", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(
            fields::text::content.read(&resolve_properties(node.properties, "text", &lua).unwrap()).unwrap().0,
            "hello"
        );
    }

    #[test]
    fn a_signal_resolving_to_a_number_reports_the_same_error_a_literal_number_would() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();

        let literal_table: mlua::Table = lua.load(r#"return { kind = "text", content = 5 }"#).eval().unwrap();
        let literal_props = props_from_table(&literal_table);
        let literal_err =
            fields::text::content.read(&resolve_properties(literal_props, "text", &lua).unwrap()).unwrap_err();

        let signal = crate::lua::signal::Signal::new_live(Value::Integer(5), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("content", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        let signal_err =
            fields::text::content.read(&resolve_properties(node.properties, "text", &lua).unwrap()).unwrap_err();

        for err in [&literal_err, &signal_err] {
            assert!(matches!(
                err,
                LayoutError::InvalidProperty { property, detail }
                    if property == "content" && detail.starts_with("expected a string or an array of runs")
            ));
        }
    }

    // ---- styled runs (ADR-0104) ----

    fn runs_content(lua: &mlua::Lua, src: &str) -> Result<(String, Vec<StyleRun>), LayoutError> {
        let table: mlua::Table = lua.load(format!(r#"return {{ kind = "text", content = {src} }}"#)).eval().unwrap();
        fields::text::content.read(&props_from_table(&table))
    }

    #[test]
    fn an_array_of_runs_joins_their_text_and_keeps_where_each_styled_one_lies() {
        let lua = mlua::Lua::new();
        let (content, runs) = runs_content(
            &lua,
            r##"{ { text = "Alice" , bold = true }, { text = ": see " }, { text = "this", underline = true, color = "#ff0000" }, { text = "!" } }"##,
        )
        .unwrap();
        assert_eq!(content, "Alice: see this!");
        assert_eq!(runs.len(), 2, "plain runs are text with no style entry of their own");
        assert_eq!(runs[0].range, 0..5);
        assert!(runs[0].bold && !runs[0].italic && !runs[0].underline && runs[0].color.is_none());
        assert_eq!(runs[1].range, 11..15);
        assert!(runs[1].underline);
        assert_eq!(runs[1].color, Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }));
    }

    #[test]
    fn an_empty_run_array_is_empty_content_and_an_empty_run_is_skipped() {
        let lua = mlua::Lua::new();
        assert_eq!(runs_content(&lua, "{}").unwrap(), (String::new(), Vec::new()));
        let (content, runs) = runs_content(&lua, r#"{ { text = "", bold = true }, { text = "a" } }"#).unwrap();
        assert_eq!((content.as_str(), runs.len()), ("a", 0));
    }

    #[test]
    fn a_misspelled_run_field_is_refused_naming_the_run() {
        let lua = mlua::Lua::new();
        let err = runs_content(&lua, r##"{ { kind = "text", text = "a", colour = "#ffffff" } }"##).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail }
            if property == "content" && detail.starts_with("run 1: unknown key `colour`")),
            "{err}"
        );
    }

    /// A notification body span of `kind = "image"` has no `text`. It is refused with a message
    /// that says what to do about it, rather than drawn as nothing or as its path.
    #[test]
    fn a_run_without_text_is_refused_naming_the_run() {
        let lua = mlua::Lua::new();
        let err = runs_content(&lua, r#"{ { text = "a" }, { kind = "image", image_path = "/x.png" } }"#).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail }
            if property == "content" && detail.starts_with("run 2 has no `text`")),
            "{err}"
        );
    }

    #[test]
    fn a_run_with_a_mistyped_flag_or_colour_is_refused() {
        let lua = mlua::Lua::new();
        assert!(runs_content(&lua, r#"{ { text = "a", bold = "yes" } }"#).is_err());
        assert!(runs_content(&lua, r#"{ { text = "a", color = "red" } }"#).is_err());
        assert!(runs_content(&lua, r#"{ "just a string" }"#).is_err());
    }

    #[test]
    fn a_run_with_an_href_is_a_styled_run_even_with_no_other_style() {
        let lua = mlua::Lua::new();
        let (content, runs) =
            runs_content(&lua, r#"{ { text = "see " }, { text = "this", href = "https://x.example/" } }"#).unwrap();
        assert_eq!(content, "see this");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].href.as_deref(), Some("https://x.example/"));
        assert_eq!(runs[0].range, 4..8);
        let (_, none) = runs_content(&lua, r#"{ { text = "a", href = "" } }"#).unwrap();
        assert!(none.is_empty(), "an empty href is no href");
    }

    #[test]
    fn font_runs_keep_only_the_runs_the_shaper_can_see() {
        let runs = vec![
            StyleRun { range: 0..2, bold: false, italic: false, underline: true, color: None, href: None },
            StyleRun { range: 2..4, bold: true, italic: false, underline: false, color: None, href: None },
            StyleRun { range: 4..6, bold: false, italic: true, underline: true, color: None, href: None },
        ];
        let fonts = font_runs(&runs);
        assert_eq!(fonts.len(), 2);
        assert_eq!((fonts[0].range.clone(), fonts[0].bold, fonts[0].italic), (2..4, true, false));
        assert_eq!((fonts[1].range.clone(), fonts[1].bold, fonts[1].italic), (4..6, false, true));
    }

    #[test]
    fn fit_parses_the_three_spelled_modes_and_nothing_else() {
        let lua = mlua::Lua::new();
        for (spelling, expected) in [("cover", Fit::Cover), ("contain", Fit::Contain), ("stretch", Fit::Stretch)] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "image", fit = "{spelling}" }}"#)).eval().unwrap();
            assert_eq!(fields::image::fit.read(&props_from_table(&table)).unwrap(), expected);
        }
        let table: mlua::Table = lua.load(r#"return { kind = "image" }"#).eval().unwrap();
        assert_eq!(fields::image::fit.read(&props_from_table(&table)).unwrap(), Fit::Cover, "an absent `fit` covers");
        // Case matters: `Cover` is not a spelling.
        let table: mlua::Table = lua.load(r#"return { kind = "image", fit = "Cover" }"#).eval().unwrap();
        assert!(fields::image::fit.read(&props_from_table(&table)).is_err());
    }

    #[test]
    fn fit_rejects_a_mode_that_does_not_exist_rather_than_covering_silently() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "image", fit = "fill" }"#).eval().unwrap();
        let err = fields::image::fit.read(&props_from_table(&table)).unwrap_err();
        assert!(format!("{err}").contains("cover"), "the error should name the modes that do exist, got {err}");

        let table: mlua::Table = lua.load(r#"return { kind = "image", fit = 3 }"#).eval().unwrap();
        assert!(fields::image::fit.read(&props_from_table(&table)).is_err());
    }

    #[test]
    fn an_image_source_that_is_not_a_string_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "image", source = 5 }"#).eval().unwrap();
        assert!(fields::image::source.read(&props_from_table(&table)).is_err());
        let table: mlua::Table = lua.load(r#"return { kind = "image", source = "/tmp/w.png" }"#).eval().unwrap();
        assert_eq!(fields::image::source.read(&props_from_table(&table)).unwrap(), "/tmp/w.png");
    }

    #[test]
    fn capture_output_defaults_to_empty_and_reads_a_connector_name() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "capture" }"#).eval().unwrap();
        assert_eq!(fields::capture::output.read(&props_from_table(&table)).unwrap(), "");
        let table: mlua::Table = lua.load(r#"return { kind = "capture", output = "DP-1" }"#).eval().unwrap();
        assert_eq!(fields::capture::output.read(&props_from_table(&table)).unwrap(), "DP-1");
    }

    #[test]
    fn live_and_paint_cursor_default_false_and_reject_non_booleans() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "capture" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(fields::capture::live.read(&props).unwrap(), None);
        assert!(!fields::capture::paint_cursor.read(&props).unwrap());

        let table: mlua::Table =
            lua.load(r#"return { kind = "capture", live = true, paint_cursor = true }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(fields::capture::live.read(&props).unwrap(), Some(f32::INFINITY));
        assert!(fields::capture::paint_cursor.read(&props).unwrap());

        let table: mlua::Table = lua.load(r#"return { kind = "capture", live = "yes" }"#).eval().unwrap();
        assert!(fields::capture::live.read(&props_from_table(&table)).is_err());
    }

    /// ADR-0263: frames per second in (0, 1000].
    #[test]
    fn live_takes_frames_per_second() {
        let lua = mlua::Lua::new();
        let live = |src: &str| {
            let table: mlua::Table = lua.load(format!("return {{ kind = 'capture', live = {src} }}")).eval().unwrap();
            fields::capture::live.read(&props_from_table(&table))
        };
        assert_eq!(live("false").unwrap(), None);
        assert_eq!(live("60").unwrap(), Some(60.0));
        assert_eq!(live("1000").unwrap(), Some(1000.0));
        for refused in ["0", "-5", "1001"] {
            assert!(live(refused).is_err(), "{refused}");
        }
    }

    /// ADR-0263: every key required, `x`/`y` at least 0, the size positive.
    #[test]
    fn region_is_a_positive_rect() {
        let lua = mlua::Lua::new();
        let region = |src: &str| {
            let table: mlua::Table = lua.load(format!("return {{ kind = 'capture', region = {src} }}")).eval().unwrap();
            fields::capture::region.read(&props_from_table(&table))
        };
        assert_eq!(
            region("{ x = 10, y = 20.5, width = 300, height = 200 }").unwrap(),
            Some(LogicalRect { x: 10.0, y: 20.5, width: 300.0, height: 200.0 })
        );
        for refused in [
            "{ x = 0, y = 0, width = 0, height = 10 }",
            "{ x = 0, y = 0, width = 10, height = -1 }",
            "{ x = 0, y = 0, width = 10 }",
            "{ x = -1, y = 0, width = 10, height = 10 }",
            "{ x = 0, y = 0, width = 1/0, height = 10 }",
            "5",
        ] {
            assert!(region(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn source_blur_defaults_to_zero_and_rejects_negative() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "image", source = "/tmp/w.png" }"#).eval().unwrap();
        assert_eq!(fields::image::source_blur.read(&props_from_table(&table)).unwrap(), 0.0);

        let table: mlua::Table = lua.load(r#"return { kind = "image", source_blur = 12 }"#).eval().unwrap();
        assert_eq!(fields::image::source_blur.read(&props_from_table(&table)).unwrap(), 12.0);

        let table: mlua::Table = lua.load(r#"return { kind = "image", source_blur = -1 }"#).eval().unwrap();
        assert!(matches!(
            fields::image::source_blur.read(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "source_blur"
        ));
    }

    #[test]
    fn font_size_of_1e300_is_rejected_instead_of_overflowing_to_inf() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "text", font_size = 1e300 }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            fields::text::font_size.read(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "font_size"
        ));
    }

    /// `line_height` is `font_size * 1.2`, and cosmic-text's `Buffer::new` asserts a non-zero line
    /// height, so a zero here aborted the Renderer. `-0.0` counts: IEEE 754 says it equals `0.0`.
    #[test]
    fn font_size_of_zero_is_rejected_rather_than_reaching_the_shaper() {
        let lua = mlua::Lua::new();
        for source in [r#"return { kind = "text", font_size = 0 }"#, r#"return { kind = "text", font_size = -0.0 }"#] {
            let table: mlua::Table = lua.load(source).eval().unwrap();
            assert!(matches!(
                fields::text::font_size.read(&props_from_table(&table)).unwrap_err(),
                LayoutError::InvalidProperty { property, .. } if property == "font_size"
            ));
        }
    }

    #[test]
    fn wrap_defaults_to_one_line_and_rejects_a_mode_that_does_not_exist() {
        let lua = mlua::Lua::new();
        assert_eq!(fields::text::wrap.read(&PropMap::default()).unwrap(), Wrap::None);

        let table: mlua::Table = lua.load(r#"return { kind = "text", wrap = "Word" }"#).eval().unwrap();
        assert_eq!(fields::text::wrap.read(&props_from_table(&table)).unwrap(), Wrap::Word);

        // "WordWrap" is the plausible typo.
        let table: mlua::Table = lua.load(r#"return { kind = "text", wrap = "WordWrap" }"#).eval().unwrap();
        let err = fields::text::wrap.read(&props_from_table(&table)).unwrap_err();
        assert!(format!("{err}").contains("Word"), "the error should name the modes that do exist, got {err}");
    }

    /// Zero is the uncapped spelling a `Bound` needs, since a signal has no way to be absent. A
    /// negative has no reading at all, and clamping one would swallow a sign slip in a config's
    /// own arithmetic.
    #[test]
    fn max_lines_treats_absent_and_zero_alike_and_refuses_a_negative() {
        let lua = mlua::Lua::new();
        assert_eq!(fields::text::max_lines.read(&PropMap::default()).unwrap(), None);

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = 0 }"#).eval().unwrap();
        assert_eq!(fields::text::max_lines.read(&props_from_table(&table)).unwrap(), None);

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = 2 }"#).eval().unwrap();
        assert_eq!(fields::text::max_lines.read(&props_from_table(&table)).unwrap(), Some(2));

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = -1 }"#).eval().unwrap();
        assert!(matches!(
            fields::text::max_lines.read(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "max_lines"
        ));

        let table: mlua::Table = lua.load(r#"return { kind = "text", max_lines = "two" }"#).eval().unwrap();
        assert!(fields::text::max_lines.read(&props_from_table(&table)).is_err());
    }

    #[test]
    fn font_size_absent_defaults_to_twelve() {
        let props = PropMap::default();
        assert_eq!(fields::text::font_size.read(&props).unwrap(), 12.0);
    }

    #[test]
    fn a_signal_resolving_to_a_number_satisfies_font_size_through_marshals_check_number() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Number(18.0), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "text").unwrap();
        table.set("font_size", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(
            fields::text::font_size.read(&resolve_properties(node.properties, "text", &lua).unwrap()).unwrap(),
            18.0
        );
    }

    #[test]
    fn icon_size_absent_defaults_to_twelve() {
        let props = PropMap::default();
        assert_eq!(fields::icon::size.read(&props).unwrap(), 12.0);
    }

    #[test]
    fn node_id_absent_is_none() {
        let props = PropMap::default();
        assert_eq!(fields::common::id.read(&props).unwrap(), None);
    }

    #[test]
    fn node_id_reads_the_string() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "rect", id = "handle" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(fields::common::id.read(&props).unwrap(), Some("handle".to_string()));
    }

    #[test]
    fn a_signal_userdata_in_node_id_is_rejected() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true), crate::lua::signal::DirtyFlag::new()).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(
            matches!(fields::common::id.read(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "id")
        );
    }

    #[test]
    fn a_non_utf8_node_id_is_rejected_rather_than_lossily_converted() {
        let lua = mlua::Lua::new();
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("id", lua.create_string(b"\xff").unwrap()).unwrap();
        let props = props_from_table(&table);
        let err = fields::common::id.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "id"),
            "a non-UTF-8 id must be a LayoutError naming the property: {err:?}"
        );
    }

    #[test]
    fn two_distinct_non_utf8_ids_do_not_collapse_onto_one_replacement_character() {
        let lua = mlua::Lua::new();
        for byte in [b"\xff".as_slice(), b"\xfe".as_slice()] {
            let table = lua.create_table().unwrap();
            table.set("kind", "rect").unwrap();
            table.set("id", lua.create_string(byte).unwrap()).unwrap();
            let props = props_from_table(&table);
            assert!(
                matches!(fields::common::id.read(&props), Err(LayoutError::InvalidProperty { ref property, .. }) if property == "id")
            );
        }
    }

    #[test]
    fn foreground_absent_defaults_to_white() {
        let props = PropMap::default();
        assert_eq!(fields::text::foreground.read(&props).unwrap(), Some(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }));
    }

    #[test]
    fn foreground_reads_a_hex_colour() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r##"return { kind = "text", foreground = "#00ff0080" }"##).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::text::foreground.read(&props).unwrap(),
            Some(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 0x80 as f32 / 255.0 })
        );
    }

    #[test]
    fn foreground_wrong_type_is_rejected() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "text", foreground = 5 }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::text::foreground.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "foreground" && detail.contains("expected a string")),
            "{err}"
        );
    }
}

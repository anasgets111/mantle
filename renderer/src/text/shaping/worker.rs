use std::collections::{HashMap, HashSet};
use std::env;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, Weight};
use shared::debug;

use super::{FontData, FontFace, FontRun, Glyph, SHAPE_CACHE_CAPACITY, ShapeRequest, ShapeResult, ShapedLine};
use crate::text::fonts::{self, ResolvedFonts};

/// Everything the worker thread knows about fonts: the declared chain's database and primary, the
/// families nodes have named so far, and the face list femtovg is handed.
///
/// One value rather than five locals in the loop, because replacing the chain has to replace all of
/// them together: a `families` map kept across a `set_chain` would point at faces that went down
/// with the old database.
pub(super) struct WorkerFonts {
    pub(super) font_system: FontSystem,
    pub(super) primary_family: String,
    /// What each family a node named resolved to, or `None` for one nothing answered. Both
    /// outcomes are remembered, so a font that is simply not installed costs one `fc-match` rather
    /// than one per measurement.
    families: HashMap<Arc<str>, Option<String>>,
    /// Every file already loaded, so `load_family` can tell a new one from a repeat.
    loaded_paths: HashSet<PathBuf>,
    /// Codepoints already looked up by [`WorkerFonts::cover`], found or not, so one nothing on the
    /// system covers costs a single `fc-match` for the life of the process rather than one per
    /// measurement -- the bargain `families` strikes for a family that is not installed.
    probed: HashSet<char>,
    /// What `font_chain_data` last produced: the faces `TextPainter` registers with femtovg.
    pub(super) chain_data: Vec<FontFace>,
}

impl WorkerFonts {
    pub(super) fn new(chain: &[&str]) -> Self {
        let ResolvedFonts { mut db, primary_family, loaded_paths } = fonts::resolve_chain(chain);
        // Mapped once, before the `Database` reaches cosmic-text, so the mappings stay *in* the
        // database instead of being mapped twice.
        let families = HashMap::new();
        let chain_data = font_chain_data(&mut db);
        let font_system = FontSystem::new_with_locale_and_db(detect_locale(), db);
        Self { font_system, primary_family, families, loaded_paths, probed: HashSet::new(), chain_data }
    }

    /// The family name to shape `asked` under, resolving it on first sight (ADR-0144).
    ///
    /// A new family is loaded into the database the declared chain already filled, so that chain's
    /// CJK and emoji faces stay available as fallback behind it, and `generation` is bumped so the
    /// painter knows to register the new faces before drawing with them.
    ///
    /// A family nothing on the system answers resolves to the declared chain's own primary. That is
    /// what makes a typo draw the text in the wrong face rather than not at all.
    pub(super) fn family_for(
        &mut self,
        asked: Option<&Arc<str>>,
        generation: &AtomicU64,
        ensured: &Mutex<HashSet<Arc<str>>>,
    ) -> String {
        let Some(asked) = asked else {
            return self.primary_family.clone();
        };
        if !self.families.contains_key(asked) {
            // Config-supplied names, resolved and mistyped alike, fill this; `set_chain` is its
            // only other reset and production calls that once. Cleared with `ensure_family`'s
            // half, which would otherwise skip a family the worker has forgotten.
            if self.families.len() >= SHAPE_CACHE_CAPACITY {
                self.families.clear();
                ensured.lock().unwrap_or_else(PoisonError::into_inner).clear();
            }
            let hit = fonts::load_family(self.font_system.db_mut(), asked, &mut self.loaded_paths);
            self.families.insert(Arc::clone(asked), hit);
            self.chain_data = font_chain_data(self.font_system.db_mut());
            generation.fetch_add(1, Ordering::Release);
        }
        self.families[asked].clone().unwrap_or_else(|| self.primary_family.clone())
    }

    /// Loads a face for a codepoint the chain drew as a box (ADR-0239). `true` when the database
    /// changed, which is the caller's cue to shape the same request again.
    ///
    /// A shell draws text it did not write -- notification bodies, MPRIS titles, window titles --
    /// so the codepoints it meets are not the ones its chain was chosen for.
    pub(super) fn cover(&mut self, ch: char, generation: &AtomicU64) -> bool {
        if !self.probed.insert(ch) {
            return false;
        }
        // Bounded for `families`' reason: the text arriving here is not the shell's own.
        if self.probed.len() > SHAPE_CACHE_CAPACITY {
            self.probed.clear();
        }
        if !fonts::load_covering(self.font_system.db_mut(), ch, &mut self.loaded_paths) {
            return false;
        }
        self.chain_data = font_chain_data(self.font_system.db_mut());
        generation.fetch_add(1, Ordering::Release);
        true
    }
}

/// Measures `request`, and reports every codepoint the loaded faces had no glyph for so the worker
/// can go find faces for them (ADR-0239). All of them, not the first: one codepoint nothing on the
/// system covers would otherwise hide every box after it in the same string.
pub(super) fn shape(
    font_system: &mut FontSystem,
    primary_family: &str,
    request: &ShapeRequest,
    glyphs: bool,
) -> (ShapeResult, Vec<char>) {
    // cosmic-text panics ("no default font found") the moment it shapes a run against a database
    // with no faces, which is what a machine with no fonts installed hands the worker.
    if font_system.db().is_empty() {
        let empty = ShapeResult {
            width: 0.0,
            height: 0.0,
            lines: Vec::new().into(),
            line_ranges: Vec::new().into(),
            shaped: Vec::new().into(),
        };
        return (empty, Vec::new());
    }
    let metrics = Metrics::new(request.font_size, request.line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(request.max_width, None);
    let attrs = Attrs::new().family(Family::Name(primary_family));
    if request.runs.is_empty() {
        buffer.set_text(&request.text, &attrs, Shaping::Advanced, None);
    } else {
        buffer.set_rich_text(rich_spans(&request.text, &request.runs, &attrs), &attrs, Shaping::Advanced, None);
    }
    buffer.shape_until_scroll(font_system, false);

    // Where each paragraph starts in the source. cosmic-text splits the text into one
    // `BufferLine` per paragraph and a layout run's glyph offsets count from *its* paragraph, so
    // turning them into offsets into the whole string means adding the paragraph's own start --
    // its predecessors' text plus whichever line ending each of them was split on.
    let mut paragraph_starts = Vec::with_capacity(buffer.lines.len());
    let mut cursor = 0usize;
    for line in &buffer.lines {
        paragraph_starts.push(cursor);
        cursor += line.text().len() + line.ending().as_str().len();
    }

    let mut width = 0.0f32;
    let mut missing: Vec<char> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut line_ranges: Vec<Range<usize>> = Vec::new();
    let mut shaped: Vec<ShapedLine> = Vec::new();
    for run in buffer.layout_runs() {
        width = width.max(run.line_w);
        // `run.text` is cosmic-text's "original text line" -- the whole source paragraph, handed
        // back again for every visual line the wrap broke it into. The glyphs are what say which
        // slice of it this run is. Read as min/max over the cluster indices rather than as the
        // first and last glyph's, because a bidi run's glyphs come in visual order and its byte
        // range is not theirs to be sorted by.
        let (start, slice) = match (run.glyphs.iter().map(|g| g.start).min(), run.glyphs.iter().map(|g| g.end).max()) {
            (Some(start), Some(end)) => (start, &run.text[start..end]),
            // A blank line carries no glyphs and still takes up its height.
            _ => (0, ""),
        };
        // Trailing whitespace only: a word wrap leaves the break's space on the line it broke, and
        // a trailing space shifts a centred or right-aligned line by its own advance. Leading
        // space is the author's own indentation and stays.
        let trimmed = slice.trim_end();
        let paragraph_start = paragraph_starts.get(run.line_i).copied().unwrap_or(0);
        line_ranges.push(paragraph_start + start..paragraph_start + start + trimmed.len());
        lines.push(trimmed.to_string());

        // Glyph 0 is `.notdef`, every sfnt font's box. Read whether or not this request keeps its
        // glyphs, because a string measured as a box and painted as a letter is laid out wrong.
        // Repeats are left in: `cover` dedupes them against every codepoint it has already tried.
        missing.extend(
            run.glyphs
                .iter()
                .filter(|glyph| glyph.glyph_id == 0)
                .filter_map(|glyph| run.text[glyph.start..glyph.end].chars().next()),
        );

        // Positions from the line's own left edge: paint places the line by its alignment.
        let left = run.glyphs.iter().map(|glyph| glyph.x).fold(f32::INFINITY, f32::min);
        let placed = match glyphs {
            true => run
                .glyphs
                .iter()
                .map(|glyph| Glyph {
                    face: glyph.font_id,
                    weight: glyph.font_weight.0,
                    id: glyph.glyph_id,
                    x: glyph.x + glyph.x_offset * glyph.font_size - left,
                    y: glyph.y - glyph.y_offset * glyph.font_size,
                    advance: glyph.w,
                    start: paragraph_start + glyph.start,
                    end: paragraph_start + glyph.end,
                    rtl: glyph.level.is_rtl(),
                })
                .collect(),
            false => Box::default(),
        };
        shaped.push(ShapedLine {
            rtl: run.rtl,
            width: run.line_w,
            baseline: run.line_y - run.line_top,
            glyphs: placed,
        });
    }

    let result = ShapeResult {
        width,
        height: lines.len() as f32 * metrics.line_height,
        lines: lines.into(),
        line_ranges: line_ranges.into(),
        shaped: shaped.into(),
    };
    (result, missing)
}

/// `text` as the `(slice, attrs)` spans `Buffer::set_rich_text` takes: each run in a face of its
/// own weight and style, and the text between runs in `base`. A run reaching past the end of the
/// text, or one that would start before the previous ended, is clamped rather than refused: the
/// ranges are built by `layout` from the same string, so neither happens, and a shaper that panics
/// on a range is a worse outcome than one that measures a character in the wrong weight.
fn rich_spans<'t, 'a>(text: &'t str, runs: &[FontRun], base: &Attrs<'a>) -> Vec<(&'t str, Attrs<'a>)> {
    let mut spans = Vec::with_capacity(runs.len() * 2 + 1);
    let mut cursor = 0usize;
    for run in runs {
        let start = run.range.start.clamp(cursor, text.len());
        let end = run.range.end.clamp(start, text.len());
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        if start > cursor {
            spans.push((&text[cursor..start], base.clone()));
        }
        let mut attrs = base.clone();
        attrs.weight = if run.bold { Weight::BOLD } else { Weight::NORMAL };
        attrs.style = if run.italic { Style::Italic } else { Style::Normal };
        spans.push((&text[start..end], attrs));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push((&text[cursor..], base.clone()));
    }
    spans
}

/// Every face in the database, for femtovg to load (ADR-0211). Paint draws the faces cosmic-text
/// chose, and cosmic-text may choose any face the database holds -- a collection's second face, a
/// bold, a named family's -- so the painter is handed all of them. The database holds only the
/// chain's files and the families nodes named, so this maps no file nothing asked for.
///
/// Maps rather than reads, the entire memory story here: `Noto Color Emoji` is an 11MB CBDT bitmap
/// font, and the `data.to_vec()` this replaces held it three times over (worker `Vec<Vec<u8>>`,
/// femtovg's `add_font_mem` copy, cosmic-text's own mapping); dropping it on an idle eleven-surface
/// session cut the Renderer's private-dirty memory from 49.7MB to 22.7MB. `make_shared_face_data`
/// rewrites every face sharing the path to `Source::SharedFile`, so this is also cosmic-text's map,
/// and asking it for two faces of one file maps the file once.
///
/// SAFETY: `make_shared_face_data` is `unsafe` because a font file rewritten on disk changes
/// under the mapping, which can fault or produce nonsense glyphs. That is the same bargain
/// cosmic-text already makes internally for every font it renders, and the alternative is paying
/// a private copy per font per process to defend against someone editing a system font in place.
///
/// A face whose mapping cannot be established is skipped rather than fatal, matching
/// `resolve_chain`'s own treatment of an entry it can't honor: losing the emoji font is a missing
/// glyph, not a dead shell.
fn font_chain_data(db: &mut fontdb::Database) -> Vec<FontFace> {
    // Collected first: `make_shared_face_data` needs `&mut db`, so nothing may be borrowing it.
    let ids: Vec<fontdb::ID> = db.faces().map(|face| face.id).collect();
    let mut data = Vec::with_capacity(ids.len());
    for id in ids {
        // SAFETY: mapping a font file the process does not own, as the doc comment above spells
        // out. A rewrite in place changes the bytes under the mapping. Same bargain cosmic-text
        // already makes for every font it renders.
        match unsafe { db.make_shared_face_data(id) } {
            Some((bytes, index)) => data.push(FontFace { data: FontData(bytes), index, id }),
            None => debug!(2; "font chain: face {id:?} could not be mapped, skipped"),
        }
    }
    data
}

/// The process locale, read the way glibc's own env-var chain does: `LC_ALL`, then `LC_CTYPE`,
/// then `LANG`, defaulting to `"en-US"` when none are set (or all name `"C"`/`"POSIX"`). Replaces
/// the `sys_locale` lookup `FontSystem::new()` does internally, now that construction bypasses it.
fn detect_locale() -> String {
    let raw = env::var("LC_ALL").or_else(|_| env::var("LC_CTYPE")).or_else(|_| env::var("LANG")).unwrap_or_default();

    // `en_US.UTF-8@euro` -> `en-US`: cosmic-text's fallback tables key off the language and
    // region subtags, not the encoding or modifier, so both are dropped rather than parsed.
    let without_modifier = raw.split('@').next().unwrap_or("");
    let without_encoding = without_modifier.split('.').next().unwrap_or("");
    let normalized = without_encoding.replace('_', "-");

    if normalized.is_empty() || normalized.eq_ignore_ascii_case("C") || normalized.eq_ignore_ascii_case("POSIX") {
        "en-US".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::req;
    use super::super::*;
    use super::*;

    #[test]
    fn shape_measures_under_the_family_it_is_given() {
        // `shape()` is called twice against the *same* `FontSystem` -- built once, over a
        // database holding two Latin faces with very different metrics -- varying only the
        // `primary_family` argument. Holding the database fixed isolates that argument: varying
        // the chain instead (two separate `resolve_chain` calls) would also vary the database's
        // load order, so a width difference couldn't be pinned on the argument specifically.
        if !fonts::fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        if !fonts::fc_lists("Noto Sans") || !fonts::fc_lists("Noto Sans Mono") {
            eprintln!("\"Noto Sans\" and/or \"Noto Sans Mono\" not installed on this machine, skip");
            return;
        }

        let ResolvedFonts { db, .. } = fonts::resolve_chain(&["Noto Sans", "Noto Sans Mono"]);
        let mut font_system = FontSystem::new_with_locale_and_db(detect_locale(), db);

        let request = ShapeRequest {
            text: "Mantle Engine Renderer".into(),
            font_size: 24.0,
            line_height: 28.8,
            max_width: None,
            runs: Vec::new(),
            font: None,
        };
        let (proportional, _) = shape(&mut font_system, "Noto Sans", &request, false);
        let (monospace, _) = shape(&mut font_system, "Noto Sans Mono", &request, false);

        // Near-equal widths would mean the family argument did nothing; 10% clears rounding noise.
        let diff = (proportional.width - monospace.width).abs();
        let tolerance = proportional.width.max(monospace.width) * 0.10;
        assert!(
            diff > tolerance,
            "\"Noto Sans\" measured {} and \"Noto Sans Mono\" measured {} for the same string at \
             the same size against the same database -- too close to prove `shape()` is actually \
             keying off the family argument rather than ignoring it",
            proportional.width,
            monospace.width
        );
    }

    /// A machine with no font files installed: `fonts::resolve_chain` hands back an empty
    /// database, and cosmic-text's shaper panics outright on one ("no default font found"),
    /// taking the worker thread with it.
    #[test]
    fn shaping_against_an_empty_database_measures_nothing_rather_than_panicking() {
        let mut font_system = FontSystem::new_with_locale_and_db(detect_locale(), fontdb::Database::new());
        let (measured, missing) = shape(&mut font_system, "", &req("Mantle", 14.0), true);
        assert_eq!((measured.width, measured.height), (0.0, 0.0));
        assert!(measured.lines.is_empty() && measured.shaped.is_empty());
        assert!(missing.is_empty(), "a database with nothing in it has no codepoint to go looking for");
    }

    #[test]
    fn the_family_memo_clears_at_its_cap_and_takes_the_handle_side_with_it() {
        let mut fonts = WorkerFonts::new(fonts::DEFAULT_CHAIN);
        let generation = AtomicU64::new(0);
        let ensured: Mutex<HashSet<Arc<str>>> = Mutex::new(HashSet::new());
        for i in 0..SHAPE_CACHE_CAPACITY {
            let filler: Arc<str> = Arc::from(format!("never-installed-{i}").as_str());
            ensured.lock().unwrap().insert(Arc::clone(&filler));
            fonts.families.insert(filler, None);
        }

        let asked: Arc<str> = Arc::from("sans-serif");
        fonts.family_for(Some(&asked), &generation, &ensured);

        assert_eq!(fonts.families.len(), 1, "the memo is dropped whole, keeping only the family that overflowed it");
        assert!(ensured.lock().unwrap().is_empty(), "`ensure_family` must not skip a family the worker forgot");
    }
}

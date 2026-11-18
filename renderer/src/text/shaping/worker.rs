use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use parley::fontique::{Blob, CharmapIndex, Collection, CollectionOptions, FamilyId};
use parley::setting::Tag;
use parley::{
    FontContext, FontFamilyName, FontStyle, FontVariation, FontWeight, Layout, LayoutContext, LineHeight, OverflowWrap,
    StyleProperty,
};
use shared::debug;

use super::{FontData, FontFace, Glyph, SHAPE_CACHE_CAPACITY, ShapeRequest, ShapeResult, ShapedLine};
use crate::text::fonts::{self, ResolvedFonts};

/// Everything the worker thread knows about fonts: the declared chain's database and primary, the
/// families nodes have named so far, and the face list femtovg is handed.
///
/// One value rather than five locals in the loop, because replacing the chain has to replace all of
/// them together: a `families` map kept across a `set_chain` would point at faces that went down
/// with the old database.
pub(super) struct WorkerFonts {
    pub(super) db: fontdb::Database,
    font_context: FontContext,
    layout_context: LayoutContext<()>,
    /// A Parley font is identified by its mapped file and collection index.
    faces: HashMap<(u64, u32), fontdb::ID>,
    charmaps: HashMap<(u64, u32), CharmapIndex>,
    /// Every Fontique family registered, so a face joining one is told from a new family.
    family_ids: HashSet<FamilyId>,
    /// Bumped when a face joins a registered family: every unboxed result may pick it now (#133).
    restyles: Arc<AtomicU64>,
    blobs: HashMap<usize, Blob<u8>>,
    family_names: Vec<String>,
    pub(super) primary_family: String,
    /// What each family a node named resolved to, or `None` for one nothing answered. Both
    /// outcomes are remembered, so a font that is simply not installed costs one `fc-match` rather
    /// than one per measurement.
    families: HashMap<Arc<str>, Option<String>>,
    /// Every file already loaded, so `load_chain` can tell a new one from a repeat.
    loaded_paths: HashSet<PathBuf>,
    /// Codepoints already looked up by [`WorkerFonts::cover`], found or not, so one nothing on the
    /// system covers costs a single `fc-match` for the life of the process rather than one per
    /// measurement -- the bargain `families` strikes for a family that is not installed.
    probed: HashSet<char>,
    /// What `font_chain_data` last produced: the faces `TextPainter` registers with femtovg.
    pub(super) chain_data: Vec<FontFace>,
}

impl WorkerFonts {
    pub(super) fn new(chain: &[&str], restyles: Arc<AtomicU64>) -> Self {
        let ResolvedFonts { mut db, primary_family, loaded_paths } = fonts::resolve_chain(chain);
        // Map once in fontdb, then register the same bytes with Parley.
        let chain_data = font_chain_data(&mut db);
        let mut font_context = FontContext::new();
        font_context.collection = Collection::new(CollectionOptions { shared: false, system_fonts: false });
        let mut worker = Self {
            db,
            font_context,
            layout_context: LayoutContext::new(),
            faces: HashMap::new(),
            charmaps: HashMap::new(),
            family_ids: HashSet::new(),
            restyles,
            blobs: HashMap::new(),
            family_names: Vec::new(),
            primary_family,
            families: HashMap::new(),
            loaded_paths,
            probed: HashSet::new(),
            chain_data,
        };
        worker.register_faces();
        worker
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
            // Files and generic mappings: what shaping and SVG text read. Compared by value, so
            // a name mapped where it already was, or to nothing, changes nothing and owes no pass.
            let generics = |db: &fontdb::Database| {
                use fontdb::Family::{Cursive, Fantasy, Monospace, SansSerif, Serif};
                [Serif, SansSerif, Cursive, Fantasy, Monospace].map(|generic| db.family_name(&generic).to_owned())
            };
            let before = (self.loaded_paths.len(), generics(&self.db));
            let hit = fonts::load_chain(&mut self.db, &[&**asked], &mut self.loaded_paths);
            self.families.insert(Arc::clone(asked), hit);
            if (self.loaded_paths.len(), generics(&self.db)) != before {
                self.chain_data = font_chain_data(&mut self.db);
                self.register_faces();
                generation.fetch_add(1, Ordering::Release);
            }
        }
        self.families[asked].clone().unwrap_or_else(|| self.primary_family.clone())
    }

    /// Loads a face for a codepoint the chain drew as a box (ADR-0239). `true` when the database
    /// changed, which is the caller's cue to shape the same request again.
    ///
    /// A shell draws text it did not write -- notification bodies, MPRIS titles, window titles --
    /// so the codepoints it meets are not the ones its chain was chosen for.
    pub(super) fn cover(&mut self, ch: char, generation: &AtomicU64) -> bool {
        self.first_probe(ch) && fonts::covering_file(ch).is_some_and(|path| self.land(ch, path, generation))
    }

    /// Whether `ch` has not been looked up yet, marking it looked up.
    pub(super) fn first_probe(&mut self, ch: char) -> bool {
        if !self.probed.insert(ch) {
            return false;
        }
        // Bounded for `families`' reason: the text arriving here is not the shell's own.
        if self.probed.len() > SHAPE_CACHE_CAPACITY {
            self.probed.clear();
        }
        true
    }

    /// Forgets that `ch` was looked up, for a probe the queue had no room for.
    pub(super) fn unprobe(&mut self, ch: char) {
        self.probed.remove(&ch);
    }

    /// Loads `path`, the face fontconfig answered `ch` with. `true` when the database changed.
    pub(super) fn land(&mut self, ch: char, path: PathBuf, generation: &AtomicU64) -> bool {
        if !fonts::load_cover(&mut self.db, ch, path, &mut self.loaded_paths) {
            return false;
        }
        self.chain_data = font_chain_data(&mut self.db);
        self.register_faces();
        generation.fetch_add(1, Ordering::Release);
        true
    }

    fn register_faces(&mut self) {
        let mut joined = Vec::new();
        for face in &self.chain_data {
            let blob = self.blobs.entry(face.data.addr()).or_insert_with(|| {
                let blob = Blob::new(face.data.0.clone());
                for (family, fonts) in self.font_context.collection.register_fonts(blob.clone(), None) {
                    joined.push(family);
                    self.charmaps.extend(fonts.iter().map(|font| ((blob.id(), font.index()), font.charmap_index())));
                }
                blob
            });
            self.faces.insert((blob.id(), face.index), face.id);
        }
        // Families from before this call: a new family's regular and bold arrive together.
        if joined.iter().any(|family| self.family_ids.contains(family)) {
            self.restyles.fetch_add(1, Ordering::Release);
        }
        self.family_ids.extend(joined);
        self.family_names.clear();
        for face in self.db.faces() {
            for (name, _) in &face.families {
                if !self.family_names.contains(name) {
                    self.family_names.push(name.clone());
                }
            }
        }
    }
}

/// Measures `request`, and reports every codepoint the loaded faces had no glyph for so the worker
/// can go find faces for them (ADR-0239). All of them, not the first: one codepoint nothing on the
/// system covers would otherwise hide every box after it in the same string. The `bool` is a
/// cluster drawn without glyph 0 by a face lacking one of its characters (#133).
pub(super) fn shape(
    fonts: &mut WorkerFonts,
    primary_family: &str,
    request: &ShapeRequest,
    glyphs: bool,
) -> (ShapeResult, Vec<char>, bool) {
    if fonts.chain_data.is_empty() {
        return (
            ShapeResult {
                width: 0.0,
                height: 0.0,
                lines: Vec::new().into(),
                line_ranges: Vec::new().into(),
                shaped: Vec::new().into(),
                coords: Vec::new().into(),
            },
            Vec::new(),
            false,
        );
    }

    // Fontique has no system fallback when discovery is disabled. Give Parley the same loaded
    // chain, with the requested family first; a newly rescued face joins on the next shape.
    let family_list: Vec<_> = std::iter::once(primary_family)
        .chain(fonts.family_names.iter().map(String::as_str))
        .map(FontFamilyName::named)
        .collect();
    let mut builder = fonts.layout_context.ranged_builder(&mut fonts.font_context, &request.text, 1.0, false);
    builder.push_default(StyleProperty::FontFamily(family_list.as_slice().into()));
    builder.push_default(StyleProperty::FontSize(request.font_size));
    builder.push_default(StyleProperty::LineHeight(LineHeight::Absolute(request.line_height)));
    builder.push_default(StyleProperty::LetterSpacing(request.letter_spacing));
    builder.push_default(StyleProperty::FontWeight(FontWeight::new(request.font_weight)));
    builder.push_default(StyleProperty::FontStyle(if request.italic { FontStyle::Italic } else { FontStyle::Normal }));
    let variations: Vec<_> = request
        .variations
        .iter()
        .map(|(tag, value)| FontVariation::new(Tag::from_bytes(*tag), f32::from_bits(*value)))
        .collect();
    builder.push_default(StyleProperty::FontVariations(variations.as_slice().into()));
    // A word wider than the line breaks inside it. Not at width 0, the solver's min-content probe,
    // which CSS `break-word` keeps at the longest word rather than one character.
    if request.max_width.is_some_and(|width| width > 0.0) {
        builder.push_default(StyleProperty::OverflowWrap(OverflowWrap::BreakWord));
    }
    for run in &request.runs {
        let start = run.range.start.min(request.text.len());
        let end = run.range.end.min(request.text.len());
        if start >= end || !request.text.is_char_boundary(start) || !request.text.is_char_boundary(end) {
            continue;
        }
        if run.bold {
            builder.push(StyleProperty::FontWeight(FontWeight::BOLD), start..end);
        }
        if run.italic {
            builder.push(StyleProperty::FontStyle(FontStyle::Italic), start..end);
        }
    }
    let mut layout: Layout<()> = builder.build(&request.text);
    layout.break_all_lines(request.max_width.map(|width| width + super::FIT_SLACK));

    let mut width = 0.0_f32;
    let mut lines = Vec::new();
    let mut line_ranges = Vec::new();
    let mut shaped = Vec::new();
    let mut coords: Vec<Box<[i16]>> = Vec::new();
    let mut missing = Vec::new();
    let mut partial = false;
    // ASCII holds no right-to-left character, so every paragraph of it is LTR.
    let bidi = (!request.text.is_ascii()).then(|| unicode_bidi::BidiInfo::new(&request.text, None));
    for line in layout.lines() {
        let mut source_range = line.text_range();
        source_range.end = source_range.end.min(request.text.len());
        source_range.start = source_range.start.min(source_range.end);
        let source = &request.text[source_range.clone()];
        let trimmed = source.trim_end_matches(char::is_whitespace);
        let end = source_range.start + trimmed.len();
        lines.push(trimmed.to_string());
        line_ranges.push(source_range.start..end);
        let metrics = line.metrics();
        // An empty line still reports the advance of a placeholder space that does not hang.
        let line_width = if trimmed.is_empty() { 0.0 } else { (metrics.advance - metrics.hanging_advance).max(0.0) };
        width = width.max(line_width);
        let mut placed = Vec::new();
        let mut pen = metrics.inline_min_coord + metrics.offset;
        for run in line.runs() {
            let key = (run.font().data.id(), run.font().index);
            let face = fonts.faces.get(&key).copied();
            let charmap = fonts.charmaps.get(&key).and_then(|index| index.charmap(run.font().data.data()));
            let bits = || run.normalized_coords().iter().map(|c| c.to_bits());
            let instance = coords.iter().position(|at| at.iter().copied().eq(bits())).unwrap_or_else(|| {
                coords.push(bits().collect());
                coords.len() - 1
            }) as u32;
            for cluster in run.visual_clusters() {
                let mut range = cluster.text_range();
                range.end = range.end.min(request.text.len());
                range.start = range.start.min(range.end);
                // The shaper fakes a thin space or hides a ZWJ the face lacks, so no glyph 0 shows
                // that a face landing later takes the cluster.
                let text = request.text.get(range.clone()).unwrap_or_default();
                partial |= !text.is_ascii()
                    && charmap.as_ref().is_some_and(|map| {
                        let lacks = |ch: char| map.map(ch).is_none_or(|glyph| glyph == 0);
                        text.chars().any(|ch| counts_toward_coverage(ch) && lacks(ch))
                    });
                for glyph in cluster.glyphs() {
                    if glyph.id == 0 {
                        missing.extend(text.chars().next());
                    }
                    if glyphs && let Some(face) = face {
                        placed.push(Glyph {
                            face,
                            coords: instance,
                            id: glyph.id as u16,
                            x: pen + glyph.x,
                            y: glyph.y,
                            advance: glyph.advance,
                            start: range.start,
                            end: range.end,
                            rtl: run.is_rtl(),
                        });
                    }
                    pen += glyph.advance;
                }
            }
        }
        let rtl = match &bidi {
            Some(bidi) => bidi
                .paragraphs
                .iter()
                .find(|paragraph| paragraph.range.contains(&source_range.start))
                .map_or_else(|| layout.is_rtl(), |paragraph| paragraph.level.is_rtl()),
            // ASCII: only the empty line after a trailing newline lies outside a paragraph, and it
            // takes the layout's direction as the bidi arm's fallback does.
            None => source_range.start >= request.text.len() && layout.is_rtl(),
        };
        shaped.push(ShapedLine {
            rtl,
            width: line_width,
            baseline: metrics.baseline - metrics.block_min_coord,
            glyphs: placed.into_boxed_slice(),
        });
    }
    let result = ShapeResult {
        width,
        height: lines.len() as f32 * request.line_height,
        lines: lines.into(),
        line_ranges: line_ranges.into(),
        shaped: shaped.into(),
        coords: coords.into(),
    };
    (result, missing, partial)
}

/// Whether Parley counts `ch` toward a face's coverage: not a control, line or paragraph separator,
/// or format character outside the Inherited script (`parley_engine` `contributes_to_shaping`), nor
/// VS15/16, which `CharCluster::fill` drops in emoji clusters. VS1-14 and ZWJ/ZWNJ count.
/// ponytail: Unicode 16's format characters, by hand; a newer one stays boxed. Upgrade: Parley's
/// own flag, once it is public.
fn counts_toward_coverage(ch: char) -> bool {
    !ch.is_control()
        && !matches!(ch, '\u{AD}' | '\u{600}'..='\u{605}' | '\u{61C}' | '\u{6DD}' | '\u{70F}' | '\u{890}'..='\u{891}'
            | '\u{8E2}' | '\u{180E}' | '\u{200B}' | '\u{200E}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}' | '\u{FE0E}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}'
            | '\u{110CD}' | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// Every loaded face for FemtoVG (ADR-0211). Parley may choose a collection's second face,
/// a bold face, or a named family's face, so the painter receives all of them.
///
/// Maps rather than reads, the entire memory story here: `Noto Color Emoji` is an 11MB CBDT bitmap
/// font, and the `data.to_vec()` this replaces held it three times over (worker `Vec<Vec<u8>>`,
/// FemtoVG's `add_font_mem` copy, and the shaper's mapping); dropping it on an idle eleven-surface
/// session cut the Renderer's private-dirty memory from 49.7MB to 22.7MB. `make_shared_face_data`
/// rewrites every face sharing the path to `Source::SharedFile`. Parley receives that same mapping.
///
/// SAFETY: `make_shared_face_data` is `unsafe` because the font file is mmapped, and a file
/// rewritten or truncated in place changes under the mapping: that can fault (SIGBUS) or yield bad
/// glyphs. A private copy per font would defend against that, at the memory cost measured above.
/// Accepted.
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
        // out. A rewrite in place changes the bytes under the mapping.
        match unsafe { db.make_shared_face_data(id) } {
            Some((bytes, index)) => data.push(FontFace { data: FontData(bytes), index, id }),
            None => debug!(2; "font chain: face {id:?} could not be mapped, skipped"),
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::super::tests::req;
    use super::super::*;
    use super::*;

    #[test]
    fn a_ligature_continuation_does_not_add_advance_twice() {
        if !fonts::fc_lists("Noto Sans") {
            return;
        }
        let mut fonts = WorkerFonts::new(&["Noto Sans"], Arc::default());
        let (result, ..) = shape(&mut fonts, "Noto Sans", &req("finished", 20.0), true);
        let glyphs = &result.shaped[0].glyphs;
        let fi = glyphs.iter().find(|glyph| glyph.start == 0).unwrap();
        let n = glyphs.iter().find(|glyph| glyph.start == 2).unwrap();
        assert!((n.x - (fi.x + fi.advance)).abs() < 1.0, "a ligature's zero-glyph continuation adds no pen advance");
    }

    #[test]
    fn shape_measures_under_the_family_it_is_given() {
        // `shape()` is called twice against the same font set, built once over a
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

        let mut fonts = WorkerFonts::new(&["Noto Sans", "Noto Sans Mono"], Arc::default());

        let request = ShapeRequest { line_height: 28.8, ..req("Mantle Engine Renderer", 24.0) };
        let (proportional, ..) = shape(&mut fonts, "Noto Sans", &request, false);
        let (monospace, ..) = shape(&mut fonts, "Noto Sans Mono", &request, false);

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
    /// database. It must measure nothing without asking Parley to select a face.
    #[test]
    fn shaping_against_an_empty_database_measures_nothing_rather_than_panicking() {
        let mut fonts = WorkerFonts::new(fonts::DEFAULT_CHAIN, Arc::default());
        fonts.chain_data.clear();
        let (measured, missing, _) = shape(&mut fonts, "", &req("Mantle", 14.0), true);
        assert_eq!((measured.width, measured.height), (0.0, 0.0));
        assert!(measured.lines.is_empty() && measured.shaped.is_empty());
        assert!(missing.is_empty(), "a database with nothing in it has no codepoint to go looking for");
    }

    /// Only a changed database bumps the generation: a new file, or a generic mapped anew.
    #[test]
    fn only_a_changed_database_bumps_the_generation() {
        if !fonts::fc_match_available() {
            return;
        }
        let shots = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts/fonts.conf");
        fonts::FONTCONFIG_FILE.set(Some(shots));
        let mut fonts = WorkerFonts::new(&["Noto Sans"], Arc::default());
        let (generation, ensured) = (AtomicU64::new(0), Mutex::new(HashSet::new()));
        fonts.family_for(Some(&Arc::from("noto sans")), &generation, &ensured);
        assert_eq!(generation.load(Ordering::Acquire), 0, "another name for the loaded Noto Sans");
        assert!(fonts.cover('●', &generation) && generation.load(Ordering::Acquire) == 1, "Symbols 2 loads");
        assert!(!fonts.cover('●', &generation), "and only once");
        fonts.family_for(Some(&Arc::from("Noto Sans Symbols 2")), &generation, &ensured);
        assert_eq!(generation.load(Ordering::Acquire), 1, "a family whose file is loaded");
        for (bumps, why) in [(2, "every file loaded, monospace maps to Noto Sans"), (2, "and again, forgotten")] {
            fonts.families.clear();
            fonts.family_for(Some(&Arc::from("monospace")), &generation, &ensured);
            assert_eq!(generation.load(Ordering::Acquire), bumps, "{why}");
        }
    }

    /// #133: only a face joining a family registered before restyles, not a new family's files.
    #[test]
    fn only_a_face_joining_a_registered_family_restyles() {
        if !fonts::fc_match_available() {
            return;
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts");
        fonts::FONTCONFIG_FILE.set(Some(dir.join("fonts.conf")));
        let counter = Arc::new(AtomicU64::new(0));
        let mut fonts = WorkerFonts::new(&["Noto Sans Symbols 2"], Arc::clone(&counter));
        let (generation, ensured) = (AtomicU64::new(0), Mutex::new(HashSet::new()));
        let restyles = || counter.load(Ordering::Acquire);
        fonts.family_for(Some(&Arc::from("Noto Sans")), &generation, &ensured);
        assert_eq!((generation.load(Ordering::Acquire), restyles()), (1, 0), "four files of a new family");
        assert!(fonts.land('─', dir.join("split/Mono-Regular-2500.ttf"), &generation));
        assert_eq!(restyles(), 0, "a new family");
        assert!(fonts.land('│', dir.join("split/Mono-Bold-2500-2502.ttf"), &generation));
        assert_eq!(restyles(), 1, "its bold joins it");
    }

    #[test]
    fn the_family_memo_clears_at_its_cap_and_takes_the_handle_side_with_it() {
        let mut fonts = WorkerFonts::new(fonts::DEFAULT_CHAIN, Arc::default());
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

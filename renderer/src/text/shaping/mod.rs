//! Off-thread Parley shaping. Fontconfig selects the declared chain, and the worker registers
//! only those mapped files with Fontique. FemtoVG draws the faces Parley chose (ADR-0211).

use rustc_hash::FxHashMap as HashMap;
use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak, mpsc};
use std::thread;
use std::time::Instant;

use shared::debug;

use super::fonts;
use crate::wake::Waker;

mod caret;
mod worker;

pub use caret::{caret_at, caret_thickness, caret_visible_left, caret_x, cluster_span};
use worker::{WorkerFonts, shape};

/// A byte range of a request's text shaped in a bold and/or italic face rather than the chain's
/// regular one (ADR-0104). Only what changes a *measurement* is here: an underline and a colour
/// are paint's business and never reach the worker, which is why this is not `layout`'s
/// `StyleRun` but the half of it the shaper reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FontRun {
    pub range: Range<usize>,
    pub bold: bool,
    pub italic: bool,
}

/// `font_variations`: OpenType axis tags and values as `f32` bits, sorted by tag so equal tables key
/// equal. Parley applies them after the `wght` it derives from the weight, so an explicit axis wins.
pub type Variations = Arc<[([u8; 4], u32)]>;

/// A shaping request: the text to measure and the metrics to shape it at.
pub struct ShapeRequest {
    pub text: String,
    pub font_size: f32,
    pub line_height: f32,
    pub letter_spacing: f32,
    pub font_weight: f32,
    pub italic: bool,
    pub variations: Variations,
    /// Logical-pixel width to wrap at. `None` measures the text unconstrained, on one line.
    pub max_width: Option<f32>,
    /// The parts of `text` in another face than the regular one, in order, non-overlapping, on
    /// character boundaries -- `layout` builds them that way. Empty for plain text, which is every
    /// request but a notification body's.
    pub runs: Vec<FontRun>,
    /// The family this node named, exactly as the config wrote it (ADR-0144). `None` is the
    /// declared chain, which is every node that says nothing. A measurement is only usable by the
    /// paint that draws the same family, which is why this is part of the cache key too.
    pub font: Option<Arc<str>>,
}

/// Styles shared by each separately painted paragraph of one text node.
#[derive(Clone, Copy)]
pub struct ShapingStyle<'a> {
    pub font_size: f32,
    pub line_height: f32,
    pub letter_spacing: f32,
    pub font_weight: f32,
    pub italic: bool,
    pub variations: &'a Variations,
}

/// The measured result of shaping a request: its tight bounding box in logical pixels, plus the
/// lines the text was broken onto getting there.
///
/// Behind an `Arc` because [`ShapingHandle::shape`] hands a clone back on every memo hit, and a
/// hit is the common case: `Scene::apply` re-measures every text node it resolves.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeResult {
    pub width: f32,
    pub height: f32,
    /// One entry per laid-out line, in order, each trimmed of the whitespace a word wrap leaves
    /// behind at the break. Never empty for non-empty text, except on a machine with no fonts: a
    /// string that needs no break is one entry holding the whole string.
    pub lines: Arc<[String]>,
    /// Where each of `lines` came from: the byte range of the request's text it is a slice of,
    /// parallel to `lines` (`text[line_ranges[i]] == lines[i]`). What lets `layout` carry a
    /// styled run through a wrap: the runs are ranges over the source, and a line that knows its
    /// own range can say which runs it holds (ADR-0104).
    pub line_ranges: Arc<[Range<usize>]>,
    /// How each of `lines` is laid out, parallel to it (ADR-0211). Paint and link hit-testing
    /// read these glyphs rather than shaping the line a second time.
    pub shaped: Arc<[ShapedLine]>,
    /// The normalized axis coordinates Parley shaped each run at, in `fvar` order, indexed by
    /// [`Glyph::coords`]: paint rasterizes at exactly these, so outlines match the advances.
    pub coords: Arc<[Box<[i16]>]>,
}

/// One laid-out line (ADR-0211): which way it reads, how far its baseline sits below the line's
/// top, and its glyphs.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedLine {
    pub rtl: bool,
    pub width: f32,
    pub baseline: f32,
    pub glyphs: Box<[Glyph]>,
}

/// One placed glyph: the face, axis coordinates and glyph Parley chose, its pen position from the
/// line's left edge and baseline, its advance, and the byte of the request's text it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub face: fontdb::ID,
    /// Index into [`ShapeResult::coords`].
    pub coords: u32,
    pub id: u16,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
    // ponytail: u32 keeps a glyph at 36 bytes, not 48; offsets past 4 GiB of text would wrap.
    // Upgrade: usize.
    pub start: u32,
    pub end: u32,
    /// This glyph's own direction, which an embedded run does not share with its line.
    pub rtl: bool,
}

/// One font file's bytes, held once and shared by every reader. The inner `Arc` is `fontdb`'s:
/// `Database::make_shared_face_data` maps the file and rewrites every face from it to point at
/// the mapping, so Parley and FemtoVG read the same `Shared_Clean` pages. The newtype exists
/// because femtovg's `add_shared_font_with_index` takes `T: AsRef<[u8]> + 'static` by value, and
/// `Arc<dyn AsRef<[u8]>>` doesn't itself implement `AsRef<[u8]>`.
#[derive(Clone)]
pub struct FontData(std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>);

/// One face FemtoVG should load: its file's shared bytes, collection index, and mapped fontdb id.
#[derive(Clone)]
pub struct FontFace {
    pub data: FontData,
    pub index: u32,
    pub id: fontdb::ID,
}

impl FontData {
    /// The address of the shared mapping behind this face, which identifies it across rebuilds of
    /// the chain list. `fontdb::Database::make_shared_face_data` hands back the *same* `Arc` for a
    /// face it has already shared (its `Source::SharedFile` arm is a `data.clone()`), so two
    /// `FontFace`s built from one file in two `font_chain_data` calls compare equal here.
    ///
    /// What `text::atlas` keys its femtovg registrations on: femtovg's `add_shared_font_with_index`
    /// mints a fresh `FontId` on every call and never dedups by bytes, so re-registering a face it
    /// already holds parses the file again and leaks the old entry (ADR-0144).
    pub fn addr(&self) -> usize {
        std::sync::Arc::as_ptr(&self.0) as *const () as usize
    }
}

impl AsRef<[u8]> for FontData {
    fn as_ref(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

/// Immutable view of the shaping worker's loaded fonts. Shared mappings survive chain replacement.
#[derive(Clone, Default)]
pub(crate) struct FontDatabase {
    pub generation: u64,
    pub primary_family: String,
    pub db: Arc<fontdb::Database>,
}

enum Request {
    /// The `bool` keeps each line's glyphs ([`ShapingHandle::shape_glyphs`]).
    /// Replies with the result's [`Stamped`] stamp, so the handle reshapes it once a face changes it.
    Shape(ShapeRequest, bool, mpsc::Sender<Stamped>),
    /// Look uncovered codepoints up on a probe thread with this lookup rather than before replying,
    /// and wake this waker when one loads a face (#55). Weak, so the probe thread never keeps the
    /// worker alive after the last handle drops.
    Background(Waker, Box<dyn Fn(char) -> Option<PathBuf> + Send>, Weak<mpsc::Sender<Request>>),
    /// See [`ShapingHandle::look_up_inline_until`].
    Inline(Option<Instant>),
    /// A probe's answer: the file fontconfig ranks first for the codepoint.
    Covered(char, PathBuf),
    FontChainData(mpsc::Sender<Vec<FontFace>>),
    FontDatabase(mpsc::Sender<FontDatabase>),
    /// Replace the chain this worker measures against, once the config has said what it wants
    /// (ADR-0043 decision 2). Replies once the new font set is live, so the next
    /// `font_chain_data` call answers with the new faces.
    SetChain(Vec<String>, mpsc::Sender<()>),
    /// Resolve a family without measuring anything (ADR-0144). A `text` node with an explicit
    /// width *and* height is never handed to taffy's measure callback (`compute_leaf_layout`
    /// returns early when both are known) and, without `elide` or `wrap`, never reaches
    /// `fit_text_to_box`'s shaping either -- so nothing would ever resolve its family and it would
    /// paint in the declared chain.
    EnsureFamily(Arc<str>, mpsc::Sender<()>),
    // Test-only (`#[cfg(test)]`, not `#[allow(dead_code)]`): nothing outside a test binary sends
    // this. `fonts::resolve_chain`'s own logging is the production diagnostic.
    #[cfg(test)]
    ResolvedPrimaryFamily(mpsc::Sender<String>),
}

/// Slack on every width a text is fitted to: a box's content width is its border box minus its
/// padding, which float rounding can leave a hair under the width it was measured at.
pub const FIT_SLACK: f32 = 0.01;

/// How many measured strings [`ShapingHandle`] remembers before it drops the lot. A bar resolves
/// about twenty text nodes and a long list a few hundred; a clock shapes strings nobody asks for
/// again, so an unbounded map would grow for the session. Idle churn measured 2026-09-06: 3-4 new
/// entries a minute, so about 16 h to reach the cap. `HashMap::clear` keeps its table, so the
/// buckets are a floor after the first fill.
const SHAPE_CACHE_CAPACITY: usize = 4096;

/// And the bytes they may hold, as [`held`] counts them, since another app can choose the text
/// (#70). A backstop: 500 rows of 2,000 characters peak at 21 MiB.
const SHAPE_CACHE_BYTES: usize = 32 << 20;

/// And the share holding [`ShapingHandle::shape_glyphs`]'s glyphs, which paint asks for on-screen
/// text alone, so they go first. That list's 13 MiB of measurements alone would pass it.
const SHAPE_GLYPH_BYTES: usize = 8 << 20;

/// How many codepoints may wait for a background probe. At about 10 ms an `fc-match`, a full
/// queue is 10 s of probing. A codepoint past it is forgotten and queued again when its text is
/// next shaped.
///
/// ponytail: if everything queued is a codepoint nothing covers, no face lands, nothing reshapes,
/// and a dropped coverable one stays a box until its text changes. Upgrade: requeue on drain.
const PROBE_QUEUE: usize = 1024;

/// What a measurement is keyed by: every field of a [`ShapeRequest`]. Keying on a subset would be
/// a wrong-answer bug, not a slow one, which is why `line_height` is here even though every caller
/// today derives it as `font_size * 1.2`. Floats are held as bit patterns because `f32` is not
/// `Hash` or `Eq`, making equality here stricter than `==` in both harmless directions: `NaN`s
/// sharing a bit pattern share an entry `==` would call different, and `0.0`/`-0.0` get separate
/// entries `==` would call equal. Neither direction can return another request's answer.
#[derive(PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    font_size: u32,
    line_height: u32,
    letter_spacing: u32,
    font_weight: u32,
    italic: bool,
    variations: Variations,
    max_width: Option<u32>,
    runs: Vec<FontRun>,
    font: Option<Arc<str>>,
    glyphs: bool,
}

/// The memo and the [`held`] bytes of its entries, all and those keeping glyphs.
#[derive(Default)]
struct Memo {
    entries: HashMap<ShapeKey, Stamped>,
    bytes: usize,
    glyph_bytes: usize,
}

/// A result, whether a face landing can change it (a glyph 0, or a cluster its face lacks a
/// character of), and the generation it was shaped at if so, else the restyle count (#133).
type Stamped = (ShapeResult, bool, u64);

/// A handle to a dedicated shaping worker thread and its warm font cache. `Clone` clones only
/// `Arc`s, the request `Sender` among them, so every clone addresses the one worker and font set, letting
/// `wayland::App` and the `RendererClient` it owns share a warm cache. The measurement
/// cache is `Arc`-shared for the same reason, and lives on this side of the channel: a worker-side
/// cache would still pay an `mpsc` round trip and thread wake per node, a real fraction of the
/// 17us each costs on a 500-row list.
#[derive(Clone)]
pub struct ShapingHandle {
    requests: Arc<mpsc::Sender<Request>>,
    // ponytail: Fx hashing, and keys carry text a window title can choose, so collisions can be
    // forced; the SHAPE_CACHE_CAPACITY cap bounds the damage. Upgrade: a keyed hasher.
    /// Each result with its stamp: stale once a face loads if boxed, else once one joins a
    /// loaded family.
    cache: Arc<Mutex<Memo>>,
    /// See [`ShapingHandle::font_generation`].
    generation: Arc<AtomicU64>,
    restyles: Arc<AtomicU64>,
    /// Families [`ShapingHandle::ensure_family`] has already asked the worker about, so the common
    /// case costs a hash lookup instead of a channel round trip. Mirrors the worker's own memo
    /// rather than replacing it: the worker still has to answer a family this side never saw.
    ensured: Arc<Mutex<HashSet<Arc<str>>>>,
}

impl ShapingHandle {
    /// Spawns the worker thread and creates its font contexts there.
    pub fn spawn() -> Self {
        Self::spawn_with(None)
    }

    /// A worker that sees only `fixtures/variable`: Inter Variable subset to "Mantle", with `wght` and
    /// `opsz`, so axis tests run on any machine.
    #[cfg(test)]
    pub(crate) fn spawn_variable_fixture() -> Self {
        let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/variable/fonts.conf");
        let handle = Self::spawn_with(Some(config));
        handle.set_chain(&["Inter Variable".into()]);
        handle
    }

    /// [`spawn`](Self::spawn), resolving fonts through `fontconfig` (a `fonts.conf`) when given.
    pub(crate) fn spawn_with(fontconfig: Option<std::path::PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        // Bumped by the worker whenever its font database changes, read by the painter to know
        // its femtovg font registry is stale (ADR-0144). An atomic rather than another request:
        // paint asks once a frame and must not pay a channel round trip to hear "nothing changed".
        let generation = Arc::new(AtomicU64::new(0));
        let handle_generation = Arc::clone(&generation);
        let restyles = Arc::new(AtomicU64::new(0));
        let handle_restyles = Arc::clone(&restyles);
        // Shared with the worker so it can drop this side's half of the family memo with its own.
        let ensured: Arc<Mutex<HashSet<Arc<str>>>> = Arc::new(Mutex::new(HashSet::new()));
        let worker_ensured = Arc::clone(&ensured);
        thread::Builder::new()
            .name("mantle-text-shaping".into())
            .spawn(move || {
                fonts::FONTCONFIG_FILE.set(fontconfig);
                // A handle dropped before its first request needs no font discovery.
                let Ok(first) = rx.recv() else { return };
                let mut fonts = WorkerFonts::new(fonts::DEFAULT_CHAIN, Arc::clone(&restyles));
                let mut probes: Option<(mpsc::SyncSender<char>, Waker)> = None;
                // Lookups before this still run before the reply with a probe thread (#110).
                let mut inline_until: Option<Instant> = None;
                for request in std::iter::once(first).chain(rx) {
                    match request {
                        Request::Shape(req, glyphs, reply) => {
                            let family = fonts.family_for(req.font.as_ref(), &generation, &worker_ensured);
                            let (mut result, mut missing, mut partial) = shape(&mut fonts, &family, &req, glyphs);
                            // Asked before each lookup, an `fc-match` of about 15 ms, so a spent
                            // deadline overruns by one at most.
                            let inline = || probes.is_none() || inline_until.is_some_and(|at| Instant::now() < at);
                            // `any` is the whole loop control: it runs out of codepoints when
                            // none can be covered, and short-circuits on the first that can,
                            // which is exactly when shaping again is worth it. A codepoint
                            // nothing covers is remembered by `cover`, so neither can spin.
                            while missing.iter().take_while(|_| inline()).any(|&ch| fonts.cover(ch, &generation)) {
                                (result, missing, partial) = shape(&mut fonts, &family, &req, glyphs);
                            }
                            if let Some((queue, _)) = &probes {
                                // Answer with the faces loaded now; an `fc-match` per codepoint
                                // here stalled the Wayland thread a second for 100 of them (#55).
                                // A codepoint looked up above is skipped by `first_probe`.
                                for &ch in &missing {
                                    if fonts.first_probe(ch) && queue.try_send(ch).is_err() {
                                        fonts.unprobe(ch);
                                    }
                                }
                            }
                            let boxed = !missing.is_empty() || partial;
                            let at = if boxed { &generation } else { &restyles }.load(Ordering::Acquire);
                            // A dropped receiver just means the result is discarded.
                            let _ = reply.send((result, boxed, at));
                        }
                        Request::Background(waker, lookup, requests) => {
                            let (queue, queued) = mpsc::sync_channel::<char>(PROBE_QUEUE);
                            let fontconfig = fonts::FONTCONFIG_FILE.with_borrow(Clone::clone);
                            thread::Builder::new()
                                .name("mantle-font-probe".into())
                                .spawn(move || {
                                    fonts::FONTCONFIG_FILE.set(fontconfig);
                                    for ch in queued {
                                        let Some(path) = lookup(ch) else { continue };
                                        let Some(requests) = requests.upgrade() else { return };
                                        let _ = requests.send(Request::Covered(ch, path));
                                    }
                                })
                                .expect("failed to spawn mantle-font-probe thread");
                            probes = Some((queue, waker));
                        }
                        Request::Covered(ch, path) => {
                            // An already loaded file changes nothing, so a face covering many
                            // queued codepoints reshapes the text once.
                            if fonts.land(ch, path, &generation)
                                && let Some((_, waker)) = &probes
                            {
                                waker.wake();
                            }
                        }
                        Request::Inline(until) => inline_until = until,
                        Request::EnsureFamily(asked, reply) => {
                            fonts.family_for(Some(&asked), &generation, &worker_ensured);
                            let _ = reply.send(());
                        }
                        Request::FontDatabase(reply) => {
                            let mut db = fonts.db.clone();
                            // usvg falls back through serif. Preserve a loaded generic declaration;
                            // otherwise use the primary without loading another family.
                            if db
                                .query(&fontdb::Query { families: &[fontdb::Family::Serif], ..Default::default() })
                                .is_none()
                            {
                                db.set_serif_family(fonts.primary_family.clone());
                            }
                            let _ = reply.send(FontDatabase {
                                generation: generation.load(Ordering::Acquire),
                                primary_family: fonts.primary_family.clone(),
                                db: Arc::new(db),
                            });
                        }
                        Request::FontChainData(reply) => {
                            let _ = reply.send(fonts.chain_data.clone());
                        }
                        Request::SetChain(chain, reply) => {
                            let borrowed: Vec<&str> = chain.iter().map(String::as_str).collect();
                            fonts = WorkerFonts::new(&borrowed, Arc::clone(&restyles));
                            generation.fetch_add(1, Ordering::Release);
                            let _ = reply.send(());
                        }
                        #[cfg(test)]
                        Request::ResolvedPrimaryFamily(reply) => {
                            let _ = reply.send(fonts.primary_family.clone());
                        }
                    }
                }
            })
            .expect("failed to spawn mantle-text-shaping thread");
        Self {
            requests: Arc::new(tx),
            cache: Arc::default(),
            generation: handle_generation,
            restyles: handle_restyles,
            ensured,
        }
    }

    /// Answers each shape at once and looks codepoints no loaded face covers up in the background,
    /// waking `waker` when a face lands (#55). Without it a shape waits for every lookup, which
    /// is what a single pass with no loop to reshape (`mantle check`, screenshots) wants.
    pub fn probe_in_background(&self, waker: Waker) {
        self.probe_with(waker, fonts::covering_file);
    }

    /// Even with a probe thread, looks codepoints up before replying until `deadline`, or until
    /// called with `None`: the Renderer's first frame then draws the faces, not boxes (#110).
    pub fn look_up_inline_until(&self, deadline: Option<Instant>) {
        self.requests.send(Request::Inline(deadline)).expect("mantle-text-shaping worker thread died");
    }

    fn probe_with(&self, waker: Waker, lookup: impl Fn(char) -> Option<PathBuf> + Send + 'static) {
        let weak = Arc::downgrade(&self.requests);
        self.requests
            .send(Request::Background(waker, Box::new(lookup), weak))
            .expect("mantle-text-shaping worker thread died");
    }

    /// Measures `request`, from [`SHAPE_CACHE_CAPACITY`]'s memo when asked before and from the
    /// worker thread otherwise, blocking only on a miss. An entry holding a missing glyph is
    /// reshaped once the font generation moves, and any entry once a face joins a loaded family
    /// (#133); [`ShapingHandle::set_chain`] clears the whole map
    /// rather than deciding which measurements the new faces would have changed.
    ///
    /// `Scene::apply` re-measures every text node it resolves, and ADR-0044 decision 2's dirty
    /// flag turned that from once per config edit into once per push. On a 500-row list, this memo
    /// takes 6.64ms off a 10.61ms pass, and the same 6.64ms off the 12.78ms variant that also
    /// draws an icon: the saving is measurement, not something else moving. Panics on a miss if
    /// the worker thread has died: a bug, not a recoverable state. A hit never reaches the worker,
    /// so it keeps answering after death, correct rather than lucky, but the next new string, not
    /// the next call, is the first symptom.
    pub fn shape(&self, request: ShapeRequest) -> ShapeResult {
        self.shape_keyed(request, false).0
    }

    /// [`shape`](Self::shape), and whether it holds a glyph 0 or a cluster its face lacks a character
    /// of, all a new family's face can change (#125, #133).
    pub fn shape_boxed(&self, request: ShapeRequest) -> (ShapeResult, bool) {
        self.shape_keyed(request, false)
    }

    /// [`shape`](Self::shape), keeping each line's glyphs for paint and link hit-testing (ADR-0211).
    pub fn shape_glyphs(&self, request: ShapeRequest) -> ShapeResult {
        self.shape_keyed(request, true).0
    }

    /// Each line of `text` shaped alone with its glyphs and `runs` re-based onto it, paired with the
    /// byte it starts at (ADR-0211), and wrapped at `max_width` when given. Lines end where Parley
    /// ends them, so paint draws the rows measurement counted.
    ///
    /// ponytail: hit-testing asks per pointer event -- a memo hit that still allocates each line's
    /// key. Upgrade path: carry the glyphs in the display list.
    pub fn shape_lines(
        &self,
        text: &str,
        runs: &[FontRun],
        style: ShapingStyle<'_>,
        font: Option<&Arc<str>>,
        max_width: Option<f32>,
    ) -> Vec<(usize, ShapeResult)> {
        paragraph_ranges(text)
            .into_iter()
            .map(|range| {
                let runs = runs
                    .iter()
                    .filter_map(|run| {
                        let (from, to) = (run.range.start.max(range.start), run.range.end.min(range.end));
                        (from < to).then(|| FontRun {
                            range: from - range.start..to - range.start,
                            bold: run.bold,
                            italic: run.italic,
                        })
                    })
                    .collect();
                let shaped = self.shape_glyphs(ShapeRequest {
                    text: text[range.clone()].to_string(),
                    font_size: style.font_size,
                    line_height: style.line_height,
                    letter_spacing: style.letter_spacing,
                    font_weight: style.font_weight,
                    italic: style.italic,
                    variations: Arc::clone(style.variations),
                    max_width,
                    runs,
                    font: font.cloned(),
                });
                (range.start, shaped)
            })
            .collect()
    }

    fn shape_keyed(&self, request: ShapeRequest, glyphs: bool) -> (ShapeResult, bool) {
        // The text moves into the key rather than being cloned into it, so a hit allocates
        // nothing and only a miss pays for the copy the worker needs.
        let key = ShapeKey {
            text: request.text,
            font_size: request.font_size.to_bits(),
            line_height: request.line_height.to_bits(),
            letter_spacing: request.letter_spacing.to_bits(),
            font_weight: request.font_weight.to_bits(),
            italic: request.italic,
            variations: request.variations,
            max_width: request.max_width.map(f32::to_bits),
            runs: request.runs,
            font: request.font,
            glyphs,
        };
        // A poisoned lock is recovered rather than propagated: this map is a pure memo, so no
        // invariant can break, and an unrelated thread's death shouldn't kill text measurement.
        if let Some((hit, boxed, at)) = self.cache.lock().unwrap_or_else(PoisonError::into_inner).entries.get(&key)
            && *at == if *boxed { &self.generation } else { &self.restyles }.load(Ordering::Acquire)
        {
            return (hit.clone(), *boxed);
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::Shape(
                ShapeRequest {
                    text: key.text.clone(),
                    font_size: f32::from_bits(key.font_size),
                    line_height: f32::from_bits(key.line_height),
                    letter_spacing: f32::from_bits(key.letter_spacing),
                    font_weight: f32::from_bits(key.font_weight),
                    italic: key.italic,
                    variations: Arc::clone(&key.variations),
                    max_width: key.max_width.map(f32::from_bits),
                    runs: key.runs.clone(),
                    font: key.font.clone(),
                },
                key.glyphs,
                reply_tx,
            ))
            .expect("mantle-text-shaping worker thread died");
        let (result, boxed, at) = reply_rx.recv().expect("mantle-text-shaping worker thread died before replying");

        let mut memo = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        // A stale entry being replaced, whose bytes leave with it.
        if let Some((stale, ..)) = memo.entries.remove(&key) {
            let stale = held(&key, &stale);
            memo.bytes -= stale;
            memo.glyph_bytes -= if key.glyphs { stale } else { 0 };
        }
        let size = held(&key, &result);
        let glyphs = if key.glyphs { size } else { 0 };
        // Answered but never kept: kept, it would hold the memo past its bound on every hit.
        if size > if key.glyphs { SHAPE_GLYPH_BYTES } else { SHAPE_CACHE_BYTES } {
            return (result, boxed);
        }
        // Cleared wholesale rather than evicted one at a time: an LRU needs a recency order
        // maintained on every hit, work on the path this exists to make cheap. Clearing every
        // measurement where only glyphs go stalled a 500-row list's pass 210 ms, against 9 ms.
        //
        // ponytail: a working set larger than a bound is shaped again every pass. The largest
        // list measured is 500 rows; the fix if that changes is an LRU.
        if memo.entries.len() >= SHAPE_CACHE_CAPACITY || memo.bytes + size > SHAPE_CACHE_BYTES {
            memo.entries.clear();
            (memo.bytes, memo.glyph_bytes) = (0, 0);
        } else if memo.glyph_bytes + glyphs > SHAPE_GLYPH_BYTES {
            memo.entries.retain(|key, _| !key.glyphs);
            memo.bytes -= memo.glyph_bytes;
            memo.glyph_bytes = 0;
        }
        (memo.bytes, memo.glyph_bytes) = (memo.bytes + size, memo.glyph_bytes + glyphs);
        memo.entries.insert(key, (result.clone(), boxed, at));
        (result, boxed)
    }

    /// Replaces the font chain every reader measures and paints against, and clears the
    /// measurement cache, since every entry was measured against the faces this call replaces.
    /// Called once, after startup evaluation, with whatever chain the config declared. Works by
    /// ordering, not respawn: `ShapingHandle::spawn` runs before any Lua has been read
    /// (`renderer/src/wayland/main_loop.rs`), and `TextPainter` is built lazily on a surface's first paint, after, so
    /// femtovg picks up the new faces once the caller drops any painter it already built.
    ///
    /// A no-op for an empty chain: a config declaring no fonts keeps [`fonts::DEFAULT_CHAIN`],
    /// since a chain no font on the system can honour would otherwise leave `TextPainter::new`
    /// with nothing to load and the shell with no text.
    pub fn set_chain(&self, chain: &[String]) {
        if chain.is_empty() {
            return;
        }
        debug!("setting font chain to {} faces", chain.len());
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = Memo::default();
        // The worker drops its own family memo with the database those families resolved against,
        // so this side must forget them too or `ensure_family` would skip re-resolving one.
        self.ensured.lock().unwrap_or_else(PoisonError::into_inner).clear();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::SetChain(chain.to_vec(), reply_tx))
            .expect("mantle-text-shaping worker thread died");
        let _ = reply_rx.recv();
    }

    /// How many measurements are memoized. Test-only: exposing this in production would invite a
    /// caller to reason about cache state instead of treating [`shape`](Self::shape) as pure.
    #[cfg(test)]
    fn cached_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).entries.len()
    }

    /// Entry count and a byte floor for `wayland::memory_profile`, which reports rather than
    /// decides: no branch may read this, for `cached_len`'s reason. Walks the map rather than
    /// reading the memo's running total, so a drifting total shows as a difference.
    pub fn census(&self) -> (usize, usize) {
        let cache = &self.cache.lock().unwrap_or_else(PoisonError::into_inner).entries;
        (cache.len(), cache.iter().map(|(key, (result, ..))| held(key, result)).sum())
    }

    /// Every loaded face's shared bytes, for `text::atlas::TextPainter` to register (ADR-0211).
    /// Cloning `FontData` clones an `Arc`, so repeated calls are cheap and copy no font file.
    pub fn font_chain_data(&self) -> Vec<FontFace> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests.send(Request::FontChainData(reply_tx)).expect("mantle-text-shaping worker thread died");
        reply_rx.recv().expect("mantle-text-shaping worker thread died before replying")
    }

    /// Snapshot for SVG decoding, taken on the same worker that changes the font generation.
    pub(crate) fn font_database(&self) -> FontDatabase {
        let (tx, rx) = mpsc::channel();
        self.requests.send(Request::FontDatabase(tx)).expect("mantle-text-shaping worker thread died");
        rx.recv().expect("mantle-text-shaping worker thread died before replying")
    }

    /// Resolves `family` without measuring anything, so a node that names one is drawn in it even
    /// when nothing ever measures that node (ADR-0144).
    ///
    /// Needed because measurement is not guaranteed. A `text` with an explicit width *and* height
    /// never reaches taffy's measure callback, and without `elide` or `wrap` never reaches
    /// `layout::scene::fit::fit_text_to_box`'s shaping either -- so its family would never be loaded and
    /// paint would fall back to the declared chain. `layout::scene` calls this for every named
    /// family it prepares.
    ///
    /// One channel round trip per distinct family for the life of a chain, and a hash lookup after
    /// that; `set_chain` clears the memo along with the worker's.
    pub fn ensure_family(&self, family: &Arc<str>) {
        if self.ensured.lock().unwrap_or_else(PoisonError::into_inner).contains(family) {
            return;
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::EnsureFamily(Arc::clone(family), reply_tx))
            .expect("mantle-text-shaping worker thread died");
        let _ = reply_rx.recv();
        self.ensured.lock().unwrap_or_else(PoisonError::into_inner).insert(Arc::clone(family));
    }

    /// How many times the font database has changed (ADR-0144). The painter records this
    /// alongside the faces it registered with femtovg and re-syncs when the two differ, which is
    /// what makes a family first named at runtime reach paint as well as measurement.
    ///
    /// Read once a frame, so it is an atomic rather than a request: hearing "nothing changed"
    /// must not cost a channel round trip and a worker wake-up.
    pub fn font_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// How many faces joined a family registered before them (#133). Bumped before the generation.
    pub fn restyles(&self) -> u64 {
        self.restyles.load(Ordering::Acquire)
    }

    /// Returns the font chain's resolved primary family: the same name `shape()` asks
    /// Parley with highest priority. Test-only. See `Request::ResolvedPrimaryFamily`.
    #[cfg(test)]
    pub fn resolved_primary_family(&self) -> String {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests.send(Request::ResolvedPrimaryFamily(reply_tx)).expect("mantle-text-shaping worker thread died");
        reply_rx.recv().expect("mantle-text-shaping worker thread died before replying")
    }
}

/// Paragraph boundaries for painting and hit testing. A CRLF pair is one break, an LFCR two, as
/// Parley breaks them for measurement (UAX #14).
fn paragraph_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' || bytes[i] == b'\n' {
            ranges.push(start..i);
            i += if bytes[i..].starts_with(b"\r\n") { 2 } else { 1 };
            start = i;
        } else {
            i += 1;
        }
    }
    ranges.push(start..text.len());
    ranges
}

/// The heap one memo entry owns: the key's text and font runs, the result's lines, ranges and
/// glyphs. A floor, labelled `approx` in the memory report: it leaves out the `HashMap`'s table,
/// each line's `String` header and the coordinates. `Arc` contents count once per entry even when
/// two entries share one, which cannot happen here: `shape` builds a fresh `ShapeResult` per miss.
fn held(key: &ShapeKey, result: &ShapeResult) -> usize {
    let key_bytes =
        key.text.len() + key.runs.len() * std::mem::size_of::<FontRun>() + key.font.as_deref().map_or(0, str::len);
    let line_bytes: usize = result.lines.iter().map(String::len).sum();
    let range_bytes = result.line_ranges.len() * std::mem::size_of::<Range<usize>>();
    let glyph_bytes: usize =
        result.shaped.iter().map(|line| std::mem::size_of::<ShapedLine>() + std::mem::size_of_val(&*line.glyphs)).sum();
    std::mem::size_of::<ShapeKey>()
        + std::mem::size_of::<ShapeResult>()
        + key_bytes
        + line_bytes
        + range_bytes
        + glyph_bytes
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(super) fn req(text: &str, font_size: f32) -> ShapeRequest {
        ShapeRequest {
            text: text.into(),
            font_size,
            line_height: font_size * 1.2,
            letter_spacing: 0.0,
            font_weight: 400.0,
            italic: false,
            variations: Variations::default(),
            max_width: None,
            runs: Vec::new(),
            font: None,
        }
    }

    fn req_in(text: &str, font_size: f32, family: Option<&str>) -> ShapeRequest {
        ShapeRequest { font: family.map(Arc::from), ..req(text, font_size) }
    }

    /// Two families both installed on this machine, so a test can tell one from the other. `None`
    /// when the machine has only one, which is a skip rather than a failure.
    fn two_families() -> Option<(&'static str, &'static str)> {
        if !fonts::fc_match_available() {
            return None;
        }
        let (declared, named) = ("Noto Sans", "Noto Sans Mono");
        (fonts::fc_lists(declared) && fonts::fc_lists(named)).then_some((declared, named))
    }

    /// A family installed here, and a codepoint it cannot draw that something else here can.
    /// `None` when the machine offers no such pair, which is a skip rather than a failure: a font
    /// set is a property of the machine, not of this workspace.
    fn a_family_and_a_codepoint_it_lacks() -> Option<(&'static str, char)> {
        let (declared, _) = two_families()?;
        let covered = |ch: char| fonts::fc_lists(&format!(":charset={:x}", ch as u32));
        let declared_draws = |ch: char| fonts::fc_lists(&format!("{declared}:charset={:x}", ch as u32));
        // Symbols a text face is unlikely to carry; which one this machine can draw is asked.
        let ch = "\u{2661}\u{2687}\u{2B50}\u{1F0A1}".chars().find(|&ch| covered(ch) && !declared_draws(ch))?;
        Some((declared, ch))
    }

    /// A codepoint no face in the chain can draw is given one, rather than left as the box every
    /// sfnt font keeps at glyph 0 (ADR-0239).
    #[test]
    fn a_codepoint_the_chain_cannot_draw_fetches_a_face_that_can() {
        let Some((declared, rescued)) = a_family_and_a_codepoint_it_lacks() else {
            eprintln!("skip: no codepoint this machine covers and the declared family does not");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        let before = handle.font_generation();

        let shaped = handle.shape_glyphs(req(&rescued.to_string(), 20.0));

        let glyphs = &shaped.shaped[0].glyphs;
        assert!(!glyphs.is_empty(), "a codepoint has to shape to something");
        assert!(glyphs.iter().all(|glyph| glyph.id != 0), "a codepoint the machine can draw must not stay a box");
        assert!(handle.font_generation() > before, "the painter has to hear about the face that drew it");
    }

    /// A codepoint nothing on the system covers must not hide the boxes behind it. Caught live:
    /// `"한국어 ภาษาไทย"` drew both as boxes because no face on that machine had Korean, and
    /// reporting only the first box meant the Thai after it was never looked up at all -- though
    /// 112 installed faces could draw it.
    #[test]
    fn a_codepoint_nothing_covers_does_not_hide_the_ones_that_can_be() {
        let Some((declared, rescued)) = a_family_and_a_codepoint_it_lacks() else {
            eprintln!("skip: no codepoint this machine covers and the declared family does not");
            return;
        };
        // Plane 15 is private use: assigned to nobody, so nothing is expected to draw it.
        let hopeless = '\u{F0000}';
        if fonts::fc_lists(&format!(":charset={:x}", hopeless as u32)) {
            eprintln!("skip: this machine has a face for {hopeless:?}, so it cannot stand for a hopeless one");
            return;
        }
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);

        let shaped = handle.shape_glyphs(req(&format!("{hopeless}{rescued}"), 20.0));

        let unreachable = hopeless.len_utf8();
        let boxes: Vec<&Glyph> = shaped.shaped[0].glyphs.iter().filter(|glyph| glyph.id == 0).collect();
        assert!(
            boxes.iter().all(|glyph| (glyph.start as usize) < unreachable),
            "only the codepoint nothing covers may stay a box, but {boxes:?} did"
        );
    }

    /// The hole `ensure_family` exists for: a node whose box is fully specified is never measured,
    /// so nothing would resolve its family and paint would fall back to the declared chain.
    #[test]
    fn a_family_can_be_resolved_without_measuring_anything_in_it() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        let before = handle.font_generation();

        handle.ensure_family(&Arc::from(named));

        assert!(handle.font_generation() > before, "the painter has to hear about the face");
        assert_eq!(handle.cached_len(), 0, "resolving a family measures nothing");
        assert!(named_face_is_handed(&handle, named), "and the face has to be in the list femtovg is handed");
    }

    /// Whether `named` shapes in a face of its own, and that face is one femtovg is handed.
    fn named_face_is_handed(handle: &ShapingHandle, named: &str) -> bool {
        let face = |family| handle.shape_glyphs(req_in("hello", 20.0, family)).shaped[0].glyphs[0].face;
        let named_face = face(Some(named));
        named_face != face(None) && handle.font_chain_data().iter().any(|candidate| candidate.id == named_face)
    }

    /// The client-side memo must not outlive the database its families resolved against, or a
    /// `set_chain` would leave `ensure_family` skipping a family the worker has forgotten.
    #[test]
    fn replacing_the_chain_makes_ensure_family_resolve_again() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        handle.ensure_family(&Arc::from(named));

        handle.set_chain(&[declared.to_string()]);
        let after_reset = handle.font_generation();
        handle.ensure_family(&Arc::from(named));

        assert!(handle.font_generation() > after_reset, "the family must be loaded into the new database");
        assert!(
            named_face_is_handed(&handle, named),
            "or a node naming it would paint in the declared chain after a chain change"
        );
    }

    /// Loading a family the painter has not registered yet is only safe because the painter is
    /// told to catch up; the generation is how it finds out.
    #[test]
    fn resolving_a_new_family_bumps_the_generation_once() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        let before = handle.font_generation();

        handle.shape(req_in("hello", 20.0, Some(named)));
        let after_first = handle.font_generation();
        assert!(after_first > before, "the painter has to hear about a face it does not hold");

        handle.shape(req_in("different text entirely", 20.0, Some(named)));
        assert_eq!(handle.font_generation(), after_first, "a family already resolved changes nothing");
    }

    /// Measuring the same string in two families has to reach different faces, or the box a node
    /// reserves is the declared family's width and the glyphs it draws will not fit it.
    #[test]
    fn a_named_family_measures_differently_and_is_cached_apart_from_the_declared_one() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        // Proportional against monospaced: "iiii" is much narrower in a sans than in a mono, which
        // no amount of cache-key confusion could produce on its own.
        let plain = handle.shape(req_in("iiii", 32.0, None)).width;
        let mono = handle.shape(req_in("iiii", 32.0, Some(named))).width;
        assert!(mono > plain, "monospaced {named:?} at {mono} should be wider than {declared:?} at {plain}");
        // The family is part of the cache key, so the first measurement cannot answer for another
        // family. Without it whichever node measured first would poison every other family's box.
        assert_eq!(handle.cached_len(), 2, "the two families are separate entries, not one");
    }

    /// A family nothing on the system answers draws in the declared chain rather than as nothing,
    /// and changes no face.
    #[test]
    fn a_family_nothing_answers_measures_in_the_declared_chain_and_bumps_nothing() {
        let handle = ShapingHandle::spawn();
        let missing = Some("ZZ No Such Family 9184");
        assert_eq!(handle.shape(req_in("Mantle", 20.0, missing)).width, handle.shape(req("Mantle", 20.0)).width);
        let generation = handle.font_generation();
        handle.shape(req_in("Mantle Engine", 20.0, missing));
        assert_eq!(handle.font_generation(), generation, "a miss loads nothing");
    }

    /// Replacing the chain invalidates the families resolved against the old database, or a node
    /// keeps pointing at faces that went down with it.
    #[test]
    fn replacing_the_chain_forgets_the_families_resolved_against_the_old_one() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        handle.shape(req_in("hello", 20.0, Some(named)));
        assert!(named_face_is_handed(&handle, named));

        handle.set_chain(&[named.to_string()]);
        // Face ids restart with a new database, so compare against a chain that never knew the old
        // family rather than against the old ids.
        let fresh = ShapingHandle::spawn();
        fresh.set_chain(&[named.to_string()]);
        let sizes = |handle: &ShapingHandle| {
            let mut sizes: Vec<usize> = handle.font_chain_data().iter().map(|face| face.data.as_ref().len()).collect();
            sizes.sort_unstable();
            sizes
        };
        assert_eq!(sizes(&handle), sizes(&fresh), "the new database holds no family the old one had resolved");
    }

    /// An installed family other than `primary`, so a declared chain has something to move to.
    fn another_family(primary: &str) -> Option<String> {
        ["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono"]
            .into_iter()
            .find(|family| *family != primary && fonts::fc_lists(family))
            .map(str::to_string)
    }

    /// The chain a config declares has to reach the worker, or `fonts { ... }` is a no-op that
    /// looks like it worked. Skips rather than fails on a machine with no second family installed.
    #[test]
    fn a_declared_chain_replaces_the_one_the_worker_started_with() {
        let handle = ShapingHandle::spawn();
        let Some(wanted) = another_family(&handle.resolved_primary_family()) else {
            eprintln!("skip: no second family installed, so the chain could not change");
            return;
        };
        handle.set_chain(std::slice::from_ref(&wanted));
        assert_eq!(handle.resolved_primary_family(), wanted, "the worker measures against what the config asked for");
    }

    /// `font_chain_data` is what `TextPainter::new` loads into femtovg, so a `set_chain` that moved
    /// the measuring side and not this one would measure text against one font and paint it with
    /// another (ADR-0043 decision 2).
    #[test]
    fn a_declared_chain_reaches_the_faces_femtovg_paints_with_too() {
        let handle = ShapingHandle::spawn();
        let Some(wanted) = another_family(&handle.resolved_primary_family()) else {
            eprintln!("skip: no second family installed, so nothing could change");
            return;
        };
        let before: Vec<usize> = handle.font_chain_data().iter().map(|data| data.data.as_ref().len()).collect();
        handle.set_chain(&[wanted]);
        let after: Vec<usize> = handle.font_chain_data().iter().map(|data| data.data.as_ref().len()).collect();
        assert!(!after.is_empty(), "a one-family chain loads that family's faces");
        assert_ne!(after, before, "the bytes femtovg would load must be the new font's, not the old chain's");
        assert!(after[0] > 0, "and they are real bytes, not an empty mapping");
    }

    // ---- styled runs (ADR-0104) ----

    /// Paint draws the faces Parley chose, so every face a shape names has to be one the
    /// painter is handed: the regular, a bold run's, and coverage like emoji and Arabic (ADR-0211).
    #[test]
    fn every_face_a_shape_names_is_one_the_painter_is_handed() {
        let handle = ShapingHandle::spawn();
        let text = "Mantle 🙏 شكرا";
        let bold = ShapeRequest { runs: vec![FontRun { range: 0..7, bold: true, italic: false }], ..req(text, 20.0) };
        let shapes = [handle.shape_glyphs(req(text, 20.0)), handle.shape_glyphs(bold)];
        // Read after shaping, as paint reads it: a codepoint the chain cannot draw loads a face
        // mid-shape (ADR-0239), and `TextPainter` re-syncs on the generation that bumps.
        let faces: Vec<fontdb::ID> = handle.font_chain_data().iter().map(|face| face.id).collect();
        for shaped in shapes {
            for glyph in shaped.shaped.iter().flat_map(|line| line.glyphs.iter()) {
                assert!(faces.contains(&glyph.face), "{glyph:?} names a face femtovg is never given");
            }
        }
    }

    #[test]
    fn letter_spacing_changes_glyph_positions_and_wrap_width() {
        let handle = ShapingHandle::spawn();
        let plain = handle.shape_glyphs(req("AB CD", 20.0));
        let spaced_request = ShapeRequest { letter_spacing: 5.0, ..req("AB CD", 20.0) };
        let spaced = handle.shape_glyphs(ShapeRequest { letter_spacing: 5.0, ..req("AB CD", 20.0) });
        assert!(spaced.width > plain.width + 5.0);
        assert!(spaced.shaped[0].glyphs[1].x > plain.shaped[0].glyphs[1].x + 3.0);
        let boundary = (plain.width + spaced.width) / 2.0;
        assert_eq!(handle.shape(ShapeRequest { max_width: Some(boundary), ..req("AB CD", 20.0) }).lines.len(), 1);
        assert!(handle.shape(ShapeRequest { max_width: Some(boundary), ..spaced_request }).lines.len() > 1);
    }

    fn coords_at(result: &ShapeResult, start: usize) -> Vec<i16> {
        let glyph = result.shaped[0].glyphs.iter().find(|glyph| glyph.start as usize == start).unwrap();
        result.coords[glyph.coords as usize].to_vec()
    }

    fn axes(pairs: &[(&[u8; 4], f32)]) -> Variations {
        pairs.iter().map(|(tag, value)| (**tag, value.to_bits())).collect()
    }

    /// Paint rasterizes at the coords shaping used, so outlines match advances (ADR-0211): a
    /// variable family's bold is one face at another `wght`, and Inter's `opsz` moves advances.
    #[test]
    fn weights_runs_and_font_variations_reach_the_coords_paint_draws_at() {
        let handle = ShapingHandle::spawn_variable_fixture();
        let inter = |request| handle.shape_glyphs(ShapeRequest { font: Some("Inter Variable".into()), ..request });
        let weight = |weight| coords_at(&inter(ShapeRequest { font_weight: weight, ..req("Mantle", 20.0) }), 0);
        let run = |run| inter(ShapeRequest { font_weight: 600.0, runs: vec![run], ..req("Mantle", 20.0) });
        let italic = run(FontRun { range: 0..1, bold: false, italic: true });
        assert_eq!(coords_at(&italic, 1), weight(600.0), "the node weight reaches the coords");
        let bold = run(FontRun { range: 0..1, bold: true, italic: false });
        assert_eq!(coords_at(&bold, 0), weight(700.0), "a bold run overrides the node weight");
        assert_ne!(weight(625.5), weight(600.0), "a fractional weight is its own instance");

        let shape = |pairs| inter(ShapeRequest { variations: axes(pairs), ..req("Mantle", 20.0) });
        let (text, display) = (shape(&[(b"opsz", 14.0)]), shape(&[(b"opsz", 32.0)]));
        assert_ne!(text.width, display.width, "opsz must reach shaping");
        assert_ne!(coords_at(&text, 0), coords_at(&display, 0), "and the coords paint draws at");
        assert_eq!(coords_at(&shape(&[(b"opsz", 99.0)]), 0), coords_at(&display, 0), "clamped to the face's range");
        assert_eq!(coords_at(&shape(&[(b"ABCD", 3.0)]), 0), weight(400.0), "an axis the face lacks is ignored");
        let wght =
            inter(ShapeRequest { font_weight: 700.0, variations: axes(&[(b"wght", 400.0)]), ..req("Mantle", 20.0) });
        assert_eq!(coords_at(&wght, 0), weight(400.0), "an explicit `wght` beats `font_weight`");
    }

    #[test]
    fn node_italic_uses_the_installed_italic_face() {
        if !fonts::fc_lists("Noto Sans:style=Italic") || !fonts::fc_lists("Noto Sans:style=Regular") {
            return;
        }
        let handle = ShapingHandle::spawn();
        handle.set_chain(&["Noto Sans".to_string()]);
        let plain = handle.shape_glyphs(req("Mantle", 20.0));
        let italic = handle.shape_glyphs(ShapeRequest { italic: true, ..req("Mantle", 20.0) });
        assert_ne!(plain.shaped[0].glyphs[0].face, italic.shaped[0].glyphs[0].face);
    }

    /// A right-to-left paragraph says so, and lays the letter it starts with out rightmost.
    #[test]
    fn a_right_to_left_line_reports_its_direction_and_starts_at_the_right() {
        let handle = ShapingHandle::spawn();
        let shaped = handle.shape_glyphs(req("اول one", 20.0));
        let line = &shaped.shaped[0];
        assert!(line.rtl, "an Arabic first letter makes the paragraph right to left");
        let first = line.glyphs.iter().find(|glyph| glyph.start == 0).expect("a glyph for the first letter");
        assert!(line.glyphs.iter().all(|glyph| glyph.x <= first.x), "and that letter is drawn rightmost");
        assert!(!handle.shape(req("one اول", 20.0)).shaped[0].rtl);
        let ascii = handle.shape(req("one\n", 20.0)).shaped;
        assert!(
            ascii.len() == 2 && ascii.iter().all(|line| !line.rtl),
            "ASCII with a trailing newline is LTR, empty line too"
        );
    }

    #[test]
    fn a_line_break_at_the_end_of_the_text_opens_an_empty_last_paragraph() {
        for (text, want) in [
            ("", vec![(0, 0)]),
            ("a\n", vec![(0, 1), (2, 2)]),
            ("a\r\n", vec![(0, 1), (3, 3)]),
            ("a\n\r", vec![(0, 1), (2, 2), (3, 3)]),
            ("a\n\nb", vec![(0, 1), (2, 2), (3, 4)]),
        ] {
            let got: Vec<_> = paragraph_ranges(text).into_iter().map(|range| (range.start, range.end)).collect();
            assert_eq!(got, want, "{text:?}");
        }
    }

    /// Paint draws the paragraphs these ranges cut, measurement the lines Parley breaks.
    #[test]
    fn paint_draws_the_lines_measurement_counts() {
        let handle = ShapingHandle::spawn();
        for text in ["a\nb", "a\r\nb", "a\n\rb", "a\r\rb", "a\n\r\nb", "a\r\n\r\nb"] {
            assert_eq!(paragraph_ranges(text).len(), handle.shape(req(text, 20.0)).lines.len(), "{text:?}");
        }
    }

    /// ADR-0211: femtovg dropped `خامس`'s last letter and overlapped glyphs at a direction change.
    #[test]
    fn a_mixed_direction_line_has_a_glyph_per_letter_and_none_overlap() {
        let handle = ShapingHandle::spawn();
        let text = "ثالث three خامس four";
        let mut glyphs = handle.shape_glyphs(req(text, 20.0)).shaped[0].glyphs.to_vec();
        for (start, _) in text.char_indices() {
            assert!(glyphs.iter().any(|glyph| glyph.start as usize == start), "no glyph for byte {start} of {text:?}");
        }
        glyphs.sort_by(|a, b| a.x.total_cmp(&b.x));
        for pair in glyphs.windows(2) {
            assert!(pair[0].x + pair[0].advance <= pair[1].x + 0.01, "{:?} overlaps {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn a_bold_run_measures_wider_than_the_same_text_regular() {
        let handle = ShapingHandle::spawn();
        let plain = handle.shape_glyphs(req("Mantle Engine Renderer", 20.0));
        let bold = handle.shape_glyphs(ShapeRequest {
            runs: vec![FontRun { range: 0..21, bold: true, italic: false }],
            ..req("Mantle Engine Renderer", 20.0)
        });
        if bold.shaped[0].glyphs[0].face == plain.shaped[0].glyphs[0].face {
            eprintln!("skip: the default chain's family has no bold face installed");
            return;
        }
        assert!(bold.width > plain.width, "bold {} should be wider than regular {}", bold.width, plain.width);
    }

    /// The ranges are the whole point: a wrapped line has to say which bytes of the source it
    /// holds so `layout` can carry the runs across the break, including past an explicit newline.
    #[test]
    fn each_line_range_slices_the_source_to_exactly_that_line() {
        let handle = ShapingHandle::spawn();
        let text = "first paragraph that wraps\nsecond";
        let result = handle.shape(ShapeRequest { max_width: Some(90.0), ..req(text, 14.0) });
        assert_eq!(result.lines.len(), result.line_ranges.len());
        assert!(result.lines.len() >= 3, "the first paragraph wraps and the second is its own line");
        for (line, range) in result.lines.iter().zip(result.line_ranges.iter()) {
            assert_eq!(&text[range.clone()], line.as_str(), "range {range:?} must slice to its line");
        }
        assert_eq!(result.lines.last().map(String::as_str), Some("second"));
        assert_eq!(result.line_ranges.last().cloned(), Some(27..33));
    }

    #[test]
    fn an_empty_declaration_leaves_the_default_chain_standing() {
        let handle = ShapingHandle::spawn();
        let before = handle.resolved_primary_family();
        handle.set_chain(&[]);
        assert_eq!(handle.resolved_primary_family(), before, "declaring nothing is not declaring an empty chain");
    }

    /// Every entry was measured against faces the new chain replaces, so keeping any of them would
    /// be answering with the old font's metrics under the new font's name.
    #[test]
    fn setting_a_chain_drops_every_measurement_taken_under_the_old_one() {
        let handle = ShapingHandle::spawn();
        handle.shape(req("Mantle", 14.0));
        assert_eq!(handle.cached_len(), 1);
        handle.set_chain(&["Noto Sans".to_string()]);
        assert_eq!(handle.cached_len(), 0);
    }

    #[test]
    fn the_same_request_is_measured_once_and_remembered() {
        let handle = ShapingHandle::spawn();
        let first = handle.shape(req("network access point 1", 13.0));
        assert_eq!(handle.cached_len(), 1);
        let second = handle.shape(req("network access point 1", 13.0));
        assert_eq!(handle.cached_len(), 1, "asking again must not add a second entry");
        assert_eq!(first, second);
        // A cold handle proves the cached answer is the worker's, not merely some answer.
        assert_eq!(second, ShapingHandle::spawn().shape(req("network access point 1", 13.0)));
    }

    /// The bug a subset key would cause: a hit that returns another request's box. Each of these
    /// differs from the first in exactly one field. A subset key could reuse the wrong geometry.
    #[test]
    fn every_field_of_a_request_is_part_of_its_identity() {
        let handle = ShapingHandle::spawn();
        handle.shape(req("abc", 13.0));
        for (label, request) in [
            ("text", ShapeRequest { text: "abd".into(), ..req("abc", 13.0) }),
            ("font_size", ShapeRequest { font_size: 26.0, ..req("abc", 13.0) }),
            ("line_height", ShapeRequest { line_height: 40.0, ..req("abc", 13.0) }),
            ("max_width", ShapeRequest { max_width: Some(10.0), ..req("abc", 13.0) }),
            ("letter_spacing", ShapeRequest { letter_spacing: 2.0, ..req("abc", 13.0) }),
            ("font_weight", ShapeRequest { font_weight: 700.0, ..req("abc", 13.0) }),
            ("italic", ShapeRequest { italic: true, ..req("abc", 13.0) }),
            ("variations", ShapeRequest { variations: axes(&[(b"FILL", 1.0)]), ..req("abc", 13.0) }),
        ] {
            let before = handle.cached_len();
            handle.shape(request);
            assert_eq!(handle.cached_len(), before + 1, "a request differing only in `{label}` must not hit");
        }
    }

    /// A clock is why this bound exists: it shapes a string nobody asks for twice, once a second,
    /// forever. Driven here with distinct strings rather than by waiting.
    #[test]
    fn the_cache_clears_rather_than_growing_past_its_cap() {
        let handle = ShapingHandle::spawn();
        for i in 0..SHAPE_CACHE_CAPACITY {
            handle.shape(req(&format!("{i}"), 13.0));
        }
        assert_eq!(handle.cached_len(), SHAPE_CACHE_CAPACITY, "the cap is where it clears, not before");
        handle.shape(req("one too many", 13.0));
        assert_eq!(handle.cached_len(), 1, "the map is dropped whole, keeping only the request that overflowed it");
    }

    /// The running totals against a walk of the map, all bytes and those keeping glyphs.
    fn totals_match_a_walk(handle: &ShapingHandle) {
        let census = handle.census().1;
        let memo = handle.cache.lock().unwrap();
        let glyphs = memo.entries.iter().filter(|(key, _)| key.glyphs).map(|(key, (result, ..))| held(key, result));
        assert_eq!((memo.bytes, memo.glyph_bytes), (census, glyphs.sum()), "the running totals are the walk's");
        assert!(memo.bytes <= SHAPE_CACHE_BYTES && memo.glyph_bytes <= SHAPE_GLYPH_BYTES, "within both ceilings");
    }

    /// Paint keeps a text's glyphs, many times its measurement, so under the count cap alone a
    /// memo of long lines could hold hundreds of MiB (#70).
    #[test]
    fn glyphs_past_their_ceiling_go_and_measurements_stay() {
        let handle = ShapingHandle::spawn();
        handle.shape(req("kept", 13.0));
        handle.shape_glyphs(req("painted", 13.0));
        // Stale entries replaced: the totals must drop the bytes that leave with them.
        handle.restyles.fetch_add(1, Ordering::Release);
        handle.shape(req("kept", 13.0));
        handle.shape_glyphs(req("painted", 13.0));
        totals_match_a_walk(&handle);

        let (mut glyph_entries, mut before) = (1, 0);
        while handle.cached_len() == glyph_entries + 1 {
            assert!(glyph_entries < 1000, "nothing dropped after {glyph_entries} paragraphs");
            before = handle.cache.lock().unwrap().glyph_bytes;
            handle.shape_glyphs(req(&format!("{glyph_entries} {}", "a wrapped paragraph ".repeat(100)), 13.0));
            totals_match_a_walk(&handle);
            glyph_entries += 1;
        }
        assert_eq!(handle.cached_len(), 2, "the measurement and the paragraph that tripped the ceiling");
        assert!(before > SHAPE_GLYPH_BYTES / 2, "glyphs went at {before} bytes, not near their ceiling");
        totals_match_a_walk(&handle);
        handle.shape(req("kept", 13.0));
        assert_eq!(handle.cached_len(), 2, "the measurement is still a hit");
    }

    /// A list of long rows measures each a few ways, past the glyphs' ceiling on its own (13 MiB
    /// for 500 rows of 2,000 characters), and must not take the on-screen glyphs with it. Only
    /// past the total does everything go. Words a line each, so the bytes cost little shaping.
    #[test]
    fn measurements_drop_glyphs_only_past_the_total() {
        let handle = ShapingHandle::spawn();
        let measure = |i: usize| {
            handle.shape(ShapeRequest { max_width: Some(1.0), ..req(&format!("{i} {}", "ab ".repeat(20_000)), 13.0) });
        };
        let mut measured = 0;
        while handle.census().1 <= SHAPE_GLYPH_BYTES {
            assert!(measured < 100, "the measurements never passed the glyph ceiling's size");
            measure(measured);
            measured += 1;
        }
        handle.shape_glyphs(req("painted", 13.0));
        measure(measured);
        measured += 1;
        assert_eq!(handle.cached_len(), measured + 1, "a measurement miss past the glyph ceiling kept every entry");

        while handle.cached_len() == measured + 1 {
            assert!(measured < 100, "nothing cleared after {measured} measurements");
            measure(measured);
            totals_match_a_walk(&handle);
            measured += 1;
        }
        assert_eq!(handle.cached_len(), 1, "past the total, everything went but the request that crossed it");
    }

    /// One text whose glyphs alone pass their ceiling is answered and not kept: kept, every hit
    /// would hold the memo past its bound.
    #[test]
    fn a_result_past_its_ceiling_alone_is_answered_but_not_kept() {
        let handle = ShapingHandle::spawn();
        let huge = "wide ".repeat(SHAPE_GLYPH_BYTES / std::mem::size_of::<Glyph>() / 5 + 1);
        handle.shape(req(&huge, 13.0));
        assert!(handle.census().1 > 2 * huge.len(), "its key and its line both count");
        assert!(!handle.shape_glyphs(req(&huge, 13.0)).shaped[0].glyphs.is_empty());
        assert_eq!(handle.cached_len(), 1, "only the measurement is kept");
        totals_match_a_walk(&handle);
    }

    #[test]
    fn a_clone_shares_the_cache_rather_than_starting_its_own() {
        let handle = ShapingHandle::spawn();
        let clone = handle.clone();
        handle.shape(req("shared", 13.0));
        clone.shape(req("shared", 13.0));
        assert_eq!(
            clone.cached_len(),
            1,
            "`wayland::App` and the client it owns must not measure the same string twice"
        );
    }

    #[test]
    fn measures_text_width_and_height() {
        let handle = ShapingHandle::spawn();
        let measure = |text| handle.shape(ShapeRequest { line_height: 18.0, ..req(text, 14.0) });
        let (short, long) = (measure("O"), measure("Mantle Engine"));
        assert!(short.width > 0.0, "expected nonzero width, got {}", short.width);
        assert_eq!(short.height, 18.0);
        assert!(long.width > short.width);
        assert_eq!(measure("").width, 0.0);
    }

    #[test]
    fn font_chain_data_is_all_loadable_fonts() {
        let handle = ShapingHandle::spawn();
        let chain = handle.font_chain_data();
        assert!(!chain.is_empty(), "the default chain must resolve to at least one loaded face");
        use skrifa::{FontRef, raw::TableProvider};
        for data in &chain {
            let font = FontRef::from_index(data.data.as_ref(), data.index)
                .expect("every chain entry's bytes should parse as a font face");
            assert!(
                font.head().is_ok() && font.hhea().is_ok() && font.maxp().is_ok(),
                "every chain entry should have the head, hhea and maxp tables a face needs"
            );
        }
    }

    /// The point of `FontData`: two calls hand back the same mapping rather than two copies of
    /// the file. Compares the pointers the slices start at, since that is what "same pages" means
    /// -- equal *contents* would pass just as well for the copying implementation this replaced.
    #[test]
    fn two_asks_for_the_font_chain_share_one_mapping() {
        let handle = ShapingHandle::spawn();
        let first = handle.font_chain_data();
        let second = handle.font_chain_data();
        assert!(!first.is_empty(), "the default chain must resolve to at least one loaded face");
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(
                a.data.as_ref().as_ptr(),
                b.data.as_ref().as_ptr(),
                "each ask should share one mapping, not copy the file"
            );
        }
    }

    #[test]
    fn the_resolved_primary_family_is_findable_among_the_loaded_chain_fonts() {
        // Reconstructs a database from `font_chain_data()` -- the exact bytes
        // `text::atlas::TextPainter` loads into femtovg -- and queries its face metadata.
        // Comparing name records instead would pass for the wrong reason:
        // a font's `fontdb`-visible family and its raw TTF `FAMILY` record are different sources
        // of truth and aren't required to agree.
        let handle = ShapingHandle::spawn();
        let primary_family = handle.resolved_primary_family();

        let mut db = fontdb::Database::new();
        for data in handle.font_chain_data() {
            db.load_font_data(data.data.as_ref().to_vec());
        }

        let query = fontdb::Query { families: &[fontdb::Family::Name(&primary_family)], ..Default::default() };
        assert!(
            db.query(&query).is_some(),
            "primary_family {primary_family:?} is not findable in the loaded font chain"
        );
    }

    /// A hard newline is a break the shaper already honoured for measurement and paint ignored,
    /// drawing the `\n` as a glyph. It arrives as two lines like any other break.
    #[test]
    fn an_explicit_newline_is_two_lines() {
        let handle = ShapingHandle::spawn();
        let result = handle.shape(req("first\nsecond", 14.0));
        assert_eq!(&*result.lines, ["first", "second"]);
    }

    /// A wrapped line carries only its own source slice, not the whole paragraph.
    #[test]
    fn a_max_width_narrower_than_the_unconstrained_text_wraps_to_more_lines() {
        const TEXT: &str = "Mantle Engine Renderer";
        let handle = ShapingHandle::spawn();
        let unconstrained = handle.shape(ShapeRequest { line_height: 18.0, ..req(TEXT, 14.0) });
        assert_eq!(&*unconstrained.lines, [TEXT], "an unwrapped string is one line holding all of it");
        let wrapped = handle.shape(ShapeRequest {
            line_height: 18.0,
            max_width: Some(unconstrained.width / 2.0),
            ..req(TEXT, 14.0)
        });
        let lines = &wrapped.lines;
        assert!(lines.len() > 1 && lines.join(" ") == TEXT, "rejoining to the source, not repeating it: {lines:?}");
        assert!(wrapped.height > unconstrained.height, "wrapping onto more lines must grow the measured height");
        assert!(wrapped.width <= unconstrained.width, "a wrapped line can't be wider than the unconstrained text");
        // Height is the line count times the step: one walk, not two counts.
        assert_eq!(wrapped.height, wrapped.lines.len() as f32 * 18.0);
    }

    /// The docs shots' fonts under the chain `Noto Sans`: `●` and `✓` are in their Symbols 2 face.
    pub(crate) fn shots_fixture() -> Option<ShapingHandle> {
        if !fonts::fc_match_available() {
            eprintln!("skip: fc-match not available");
            return None;
        }
        let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts/fonts.conf");
        let handle = ShapingHandle::spawn_with(Some(config));
        handle.set_chain(&["Noto Sans".into()]);
        Some(handle)
    }

    /// [`gated_shots_fixture`] recording into `looked_up` each codepoint its probe thread looks up,
    /// then answering with `answer`.
    fn recorded_gated_fixture(
        looked_up: &Arc<Mutex<Vec<char>>>,
        answer: impl Fn(char) -> Option<PathBuf> + Send + 'static,
    ) -> Option<(ShapingHandle, mpsc::Sender<()>)> {
        let seen = Arc::clone(looked_up);
        gated_shots_fixture(move |ch| {
            seen.lock().unwrap().push(ch);
            answer(ch)
        })
    }

    /// Whether the first line has glyphs in its first `prefix_len` bytes, none of them a box.
    fn drawn_before(shaped: &ShapeResult, prefix_len: usize) -> bool {
        let mut glyphs = shaped.shaped[0].glyphs.iter().filter(|glyph| (glyph.start as usize) < prefix_len).peekable();
        glyphs.peek().is_some() && glyphs.all(|glyph| glyph.id != 0)
    }

    /// [`shots_fixture`] whose lookups each wait for the returned sender to drop, then run `lookup`.
    pub(crate) fn gated_shots_fixture(
        lookup: impl Fn(char) -> Option<PathBuf> + Send + 'static,
    ) -> Option<(ShapingHandle, mpsc::Sender<()>)> {
        let handle = shots_fixture()?;
        let (open, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        handle.probe_with(Waker::new().unwrap(), move |ch| {
            let _ = gate.lock().unwrap().recv();
            lookup(ch)
        });
        Some((handle, open))
    }

    /// Drops `open`, then waits for the face it held back to land.
    pub(crate) fn land(shaping: &ShapingHandle, open: mpsc::Sender<()>) {
        let before = shaping.font_generation();
        drop(open);
        crate::lua::calls::poll_until(|| (), || shaping.font_generation() > before);
    }

    /// #55: uncovered codepoints answer before any lookup returns, and each is looked up once, even
    /// past a full queue.
    #[test]
    fn a_shape_replies_before_its_lookups_and_looks_each_codepoint_up_once() {
        let looked_up = Arc::new(Mutex::new(Vec::new()));
        let Some((handle, open)) = recorded_gated_fixture(&looked_up, |_| None) else { return };
        let before = handle.font_generation();
        let text: String = (0..PROBE_QUEUE as u32 + 100).map(|i| char::from_u32(0x10_0000 + i).unwrap()).collect();

        let (reply, replied) = mpsc::channel();
        let (shaper, asked) = (handle.clone(), text.clone());
        thread::spawn(move || reply.send(shaper.shape_glyphs(req(&asked, 20.0))));
        let shaped = replied.recv_timeout(std::time::Duration::from_secs(5)).expect("the reply waited on a lookup");

        let glyphs = &shaped.shaped[0].glyphs;
        assert!(!glyphs.is_empty() && glyphs.iter().all(|glyph| glyph.id == 0), "answered with the loaded faces");
        assert_eq!(handle.font_generation(), before);
        drop(open);
        // A new size each try, so a barrier the full queue turned away is queued again.
        let size = std::cell::Cell::new(20.0);
        let drained_to = |barrier: char| {
            let shape = || handle.shape(req(&barrier.to_string(), size.replace(size.get() + 1.0)));
            crate::lua::calls::poll_until(|| drop(shape()), || looked_up.lock().unwrap().last() == Some(&barrier));
        };
        drained_to('\u{10_FFFD}');
        // Requeues the codepoints the full queue turned away.
        handle.shape(req(&text, 99.0));
        drained_to('\u{10_FFFC}');
        let looked_up = looked_up.lock().unwrap();
        assert_eq!(looked_up.len(), PROBE_QUEUE + 102, "every codepoint and both barriers");
        assert_eq!(looked_up.iter().collect::<HashSet<_>>().len(), looked_up.len(), "none twice");
    }

    #[test]
    fn probing_in_the_background_answers_before_the_face_loads() {
        let Some(handle) = shots_fixture() else { return };
        handle.probe_in_background(Waker::new().unwrap());
        assert!(handle.shape_glyphs(req("●", 20.0)).shaped[0].glyphs.iter().all(|glyph| glyph.id == 0));
    }

    /// #55: once the lookup lands a face, the text shaped with boxes reshapes in it, and two
    /// codepoints that face covers bump the generation once.
    #[test]
    fn a_face_that_lands_reshapes_the_text_that_lacked_it() {
        let Some(handle) = shots_fixture() else { return };
        let barrier = '\u{10_0000}';
        let (done, looked_up) = mpsc::channel();
        let done = Mutex::new(done);
        let waker = Waker::new().unwrap();
        handle.probe_with(waker.clone(), move |ch| {
            if ch == barrier {
                let _ = done.lock().unwrap().send(());
                return None;
            }
            fonts::covering_file(ch)
        });
        let before = handle.font_generation();
        let request = || req(&format!("●✓{barrier}"), 20.0);
        let covered = |shaped: &ShapeResult| drawn_before(shaped, "●✓".len());

        assert!(!covered(&handle.shape_glyphs(request())), "answered before the lookup");
        looked_up.recv_timeout(std::time::Duration::from_secs(5)).expect("the barrier is looked up last");
        // Queued behind both answers, so it returns once the worker has handled them.
        let faces = handle.font_database();

        assert_eq!(handle.font_generation(), before + 1, "one face, one reshape");
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts");
        let from_fixture = |face: &fontdb::FaceInfo| match &face.source {
            fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => path.starts_with(&fixtures),
            fontdb::Source::Binary(_) => false,
        };
        let symbols = |face: &fontdb::FaceInfo| face.families.iter().any(|(name, _)| name == "Noto Sans Symbols 2");
        assert!(faces.db.faces().any(|face| symbols(face) && from_fixture(face)), "looked up under the fixture");
        let mut fds = [nix::poll::PollFd::new(waker.fd(), nix::poll::PollFlags::POLLIN)];
        assert_eq!(nix::poll::poll(&mut fds, nix::poll::PollTimeout::ZERO), Ok(1), "and is woken for it");
        assert!(covered(&handle.shape_glyphs(request())), "the cached boxes are reshaped");
    }

    /// [`gated_shots_fixture`] answering each of `files`' codepoints with its one-face subset, and
    /// a step that shapes a text to queue its lookup, then lands the face.
    fn split_fixture(files: &'static [(char, &'static str)]) -> Option<(ShapingHandle, impl Fn(&str))> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts/split");
        let (handle, open) =
            gated_shots_fixture(move |ch| files.iter().find(|(at, _)| *at == ch).map(|(_, file)| dir.join(file)))?;
        let shaping = handle.clone();
        let land = move |text: &str| {
            let before = shaping.font_generation();
            shaping.shape(req(text, 20.0));
            open.send(()).unwrap();
            crate::lua::calls::poll_until(|| (), || shaping.font_generation() > before);
        };
        Some((handle, land))
    }

    /// The PostScript name of each glyph's face in `shaped`.
    fn faces_of(handle: &ShapingHandle, shaped: &ShapeResult) -> Vec<String> {
        let db = handle.font_database().db;
        shaped.shaped[0].glyphs.iter().map(|glyph| db.face(glyph.face).unwrap().post_script_name.clone()).collect()
    }

    /// #133 (a): a bold file joining a family that drew bold text synthetically takes that text,
    /// and a face of a new family leaves text it cannot change cached.
    #[test]
    fn a_face_joining_a_loaded_family_takes_the_text_it_now_draws() {
        let files = &[('─', "Mono-Regular-2500.ttf"), ('│', "Mono-Bold-2500-2502.ttf")];
        let Some((handle, land)) = split_fixture(files) else { return };
        let bold = || ShapeRequest { font_weight: 700.0, ..req("─", 20.0) };
        let plain = handle.shape(req("abc", 20.0));
        land("─");
        assert!(
            Arc::ptr_eq(&plain.lines, &handle.shape(req("abc", 20.0)).lines),
            "a new family changes no cached text"
        );
        assert_eq!(faces_of(&handle, &handle.shape_glyphs(bold())), ["NotoSansMono-Regular"]);
        land("│");
        assert_eq!(faces_of(&handle, &handle.shape_glyphs(bold())), ["NotoSansMono-Bold"]);
    }

    /// #133 (b): a ZWJ sequence drawn without a box by a face lacking the ZWJ is taken by a face
    /// landing for other text that maps all of it. VS16 is not coverage, so `❤️` is not stale.
    #[test]
    fn a_cluster_its_face_lacks_a_character_of_is_taken_by_a_face_with_all_of_it() {
        let files = &[('❤', "Symbols2-2764.ttf"), ('😀', "ColorEmoji-2764-200D-1F600.ttf")];
        let Some((handle, land)) = split_fixture(files) else { return };
        let sequence = || req("❤\u{200D}❤", 20.0);
        land("❤");
        let text_face = handle.shape_glyphs(sequence());
        assert_eq!(faces_of(&handle, &text_face), ["NotoSansSymbols2-Regular", "NotoSansSymbols2-Regular"]);
        assert!(text_face.shaped[0].glyphs.iter().all(|glyph| glyph.id != 0), "no box");
        assert!(handle.shape_boxed(sequence()).1, "yet stale at a landing");
        assert!(!handle.shape_boxed(req("❤\u{FE0F}", 20.0)).1, "VS16 is out of coverage");
        assert!(!handle.shape_boxed(req("a\u{200B}b", 20.0)).1, "nor is a format character");
        assert!(handle.shape_boxed(req("a\u{2009}b", 20.0)).1, "a thin space faked from a space");
        land("😀");
        assert_eq!(faces_of(&handle, &handle.shape_glyphs(sequence())), ["NotoColorEmoji", "NotoColorEmoji"]);
    }

    /// #110: before the startup deadline the face loads before the reply, with the probe thread
    /// shut, so the first frame draws it and nothing lands after. A codepoint looked up then, even
    /// one nothing covers, is never queued again.
    #[test]
    fn before_the_startup_deadline_a_missing_face_loads_before_the_reply() {
        let looked_up = Arc::new(Mutex::new(Vec::new()));
        let Some((handle, open)) = recorded_gated_fixture(&looked_up, fonts::covering_file) else { return };
        handle.look_up_inline_until(Some(Instant::now() + std::time::Duration::from_secs(60)));
        let before = handle.font_generation();
        let uncovered = '\u{10_0001}';

        let shaped = handle.shape_glyphs(req(&format!("●{uncovered}"), 20.0));
        assert!(drawn_before(&shaped, "●".len()), "drawn in the face, not boxes");
        assert_eq!(handle.font_generation(), before + 1, "the face is in before the first frame");

        handle.look_up_inline_until(None);
        drop(open);
        let probed = |barrier: char| {
            let _ = handle.shape(req(&format!("●{uncovered}{barrier}"), 21.0));
            crate::lua::calls::poll_until(|| (), || looked_up.lock().unwrap().contains(&barrier));
        };
        let (first, second) = ('\u{10_0000}', '\u{10_0002}');
        probed(first);
        // The probe thread sends `first`'s answer before it looks `second` up.
        probed(second);
        let _ = handle.font_database();
        assert_eq!(*looked_up.lock().unwrap(), [first, second], "past the first frame, only new codepoints are probed");
        assert_eq!(handle.font_generation(), before + 1, "no face lands after the first frame");
    }

    /// #110: once the startup deadline is spent, lookups go to the probe thread as after it (#55).
    #[test]
    fn past_the_startup_deadline_lookups_go_to_the_probe_thread() {
        let looked_up = Arc::new(Mutex::new(Vec::new()));
        let Some((handle, open)) = recorded_gated_fixture(&looked_up, fonts::covering_file) else { return };
        handle.look_up_inline_until(Some(Instant::now()));
        let before = handle.font_generation();

        assert!(!drawn_before(&handle.shape_glyphs(req("●✓", 20.0)), "●✓".len()), "answered with boxes");
        assert_eq!(handle.font_generation(), before);

        drop(open);
        crate::lua::calls::poll_until(
            || (),
            || handle.font_generation() > before && looked_up.lock().unwrap().len() == 2,
        );
        assert_eq!(*looked_up.lock().unwrap(), ['●', '✓'], "both on the probe thread");
        assert!(drawn_before(&handle.shape_glyphs(req("●✓", 20.0)), "●✓".len()), "reshaped in the face that landed");
    }
}

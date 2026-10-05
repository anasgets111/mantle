//! Glyph rasterization and GPU texture atlas management, via FemtoVG.
//!
//! FemtoVG owns its glyph atlas entirely internally (see ADR-0012): rasterized glyphs pack
//! into private atlas pages that start at a fixed size and grow by adding further pages, not by
//! expanding one large texture. There is no public API to configure a single fixed-size page.
//! What it draws is Parley's (ADR-0211): `fill_glyph_run` takes glyphs another shaper placed,
//! so femtovg rasterizes and packs, and shapes nothing.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;
use std::sync::Arc;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, FontId, ImageId, Paint, Path, PositionedGlyph, TextContext};
use shared::debug;

use crate::layout::node::{CaretStyle, Rgba, StyleRun, TextAlign, Typeface, font_runs};
use crate::layout::paint::DrawCmd;
use crate::text::shaping::{FontFace, FontRun, Glyph, ShapeResult, ShapingHandle, caret_thickness, caret_x};

use super::snap::{LogicalRect, snap_to_physical};

/// Distinct offscreen sizes [`TextPainter`] keeps between paints (ADR-0217): a clip tweening its
/// width asks for a new one every frame and reuses none.
const SCRATCH_SIZES: usize = 16;
/// The bytes those pooled offscreens may hold between paints, as [`LAYER_BYTES`] caps layers.
const SCRATCH_BYTES: usize = 64 << 20;

/// Paints, by any surface, a finished layer outlives the last paint of its own surface's list that
/// held it, and the bytes all of them may hold, least recently held first (ADR-0258). The age
/// is a fallback for a surface whose layers [`TextPainter::release_surface`] did not free: every
/// paint of a live one sweeps its own.
pub(crate) const LAYER_PAINTS: u64 = 1000;
const LAYER_BYTES: usize = 64 << 20;

/// A layer's shadows, one image per layer it casts, and content, finished at `size` on one surface for the
/// command that drew them, with the paint that last held it.
type KeptLayer = (String, DrawCmd, (Vec<ImageId>, ImageId), (usize, usize), u64);

/// Cache key for shaped lines in [`TextPainter`].
struct TextLineKey {
    text: String,
    runs: Vec<FontRun>,
    face: Typeface,
}

impl TextLineKey {
    fn matches(&self, text: &str, runs: &[FontRun], style: &TextDraw<'_>) -> bool {
        self.face == *style.face && self.text == text && self.runs == runs
    }
}

type CachedLineEntry = (TextLineKey, Arc<Vec<(usize, ShapeResult)>>);

/// A FemtoVG canvas bound to the calling thread's current EGL/GL context, with every face the
/// shaping worker can place a glyph in registered and ready to draw with.
pub struct TextPainter {
    canvas: Canvas<OpenGl>,
    /// femtovg's id for each face, keyed by the worker's mapped fontdb id (ADR-0211).
    faces: HashMap<fontdb::ID, FontId>,
    /// The shaping worker's face-set generation this was built from, so [`TextPainter::sync`] can
    /// tell in one atomic load whether femtovg's registry is behind.
    generation: u64,
    /// femtovg's font registry, kept so a family first named at runtime can be added without
    /// rebuilding the canvas and losing its warm glyph atlas.
    text_context: TextContext,
    /// The `FontId` already registered for each `(FontData::addr, face index)`, so
    /// [`TextPainter::sync`] adds only what femtovg does not hold yet.
    ///
    /// Not an optimisation: `add_shared_font_with_index` is `self.fonts.insert(font)` into a
    /// `SlotMap`, which mints a new key every call and never dedups by bytes. Re-registering the
    /// whole list would re-parse every face, strand the previous entries in the slot map for the
    /// life of the surface, and call femtovg's `clear_caches` once per face (ADR-0144).
    registered: HashMap<(usize, u32), FontId>,
    /// The worker and memo that measured each line, asked again for its glyphs and its faces.
    shaping: ShapingHandle,
    /// `layout::paint::canvas::draw_clipped`'s offscreen targets, kept between paints and keyed by exact
    /// size, each with the paint that last asked for that size (ADR-0217). Here, not beside
    /// `ImageCache`, because the ids belong to `canvas` and have to die with it.
    scratch: HashMap<PoolKey, (u64, Vec<ImageId>)>,
    /// What [`TextPainter::recycle_scratch`] ages by. Not a timer: a size goes stale because other
    /// sizes were asked for since, not because seconds passed.
    paints: u64,
    /// `layout::paint::canvas::draw_layer`'s finished images, kept here beside `scratch` for the
    /// same reason (ADR-0258).
    layers: Vec<KeptLayer>,
    /// Warm cache of shaped lines to bypass re-shaping and glyph vector clones on static text frames.
    lines_cache: HashMap<u64, Vec<CachedLineEntry>>,
    lines_cache_len: usize,
}

/// A pooled offscreen's owner and exact size. Owned, so a released surface frees its own and no
/// other's: sizes alone cannot tell a gone full-screen lock from the live full-screen wallpaper.
type PoolKey = (String, (usize, usize));

/// The sizes to delete to bring a scratch pool back to [`SCRATCH_SIZES`] and [`SCRATCH_BYTES`]:
/// those asked for longest ago, never one the paint `now` asked for (ADR-0262). Pure so the policy
/// is testable without the GL context `delete_image` needs.
/// ponytail: one paint's own sizes stay past either ceiling, so a surface of many glasses at sigma
/// 32 or more can hold more until its next paint asks for fewer.
fn stalest<T>(scratch: &HashMap<PoolKey, (u64, Vec<T>)>, now: u64) -> Vec<PoolKey> {
    let mut by_age: Vec<_> = scratch.iter().map(|(key, (asked, _))| (*asked, key.clone())).collect();
    by_age.sort_unstable();
    let mut bytes: usize = scratch.iter().map(|((_, (w, h)), (_, free))| w * h * 4 * free.len()).sum();
    let mut sizes = scratch.len();
    let mut evicted = Vec::new();
    for (asked, key) in by_age {
        if (sizes <= SCRATCH_SIZES && bytes <= SCRATCH_BYTES) || asked == now {
            break;
        }
        sizes -= 1;
        bytes -= (key.1).0 * (key.1).1 * 4 * scratch[&key].1.len();
        evicted.push(key);
    }
    evicted
}

/// What [`TextPainter::draw_text`] draws, apart from where: one `Draw::Text` command's worth,
/// borrowed rather than cloned out of it.
pub struct TextDraw<'a> {
    pub text: &'a str,
    pub runs: &'a [StyleRun],
    pub face: &'a Typeface,
    pub color: Rgba,
    pub align: TextAlign,
    /// A focused plain `textfield`'s `(anchor, caret)` byte offsets into `text` (ADR-0236).
    pub caret: Option<(usize, usize)>,
    /// The blink's phase: off drops the bar and keeps the selection and scroll.
    pub caret_on: bool,
    pub caret_style: CaretStyle,
}

/// Registers every face of `font_chain` femtovg does not hold yet, and maps each face's shaping id
/// to its `FontId`. `registered` carries the ids femtovg minted across calls, because it mints a
/// new one every time it is asked (see [`TextPainter::registered`]).
fn register(
    text_context: &TextContext,
    registered: &mut HashMap<(usize, u32), FontId>,
    font_chain: Vec<FontFace>,
) -> HashMap<fontdb::ID, FontId> {
    let mut faces = HashMap::with_capacity(font_chain.len());
    for face in font_chain {
        let key = (face.data.addr(), face.index);
        let id = match registered.get(&key) {
            Some(id) => *id,
            None => match text_context.add_shared_font_with_index(face.data.clone(), face.index) {
                Ok(id) => {
                    registered.insert(key, id);
                    id
                }
                // fontdb accepts faces femtovg's parser refuses; that one draws nothing, the rest draw.
                Err(e) => {
                    debug!(2; "font chain: femtovg refused face {:?}, skipped: {e}", face.id);
                    continue;
                }
            },
        };
        faces.insert(face.id, id);
    }
    faces
}

/// The spans under the glyphs `in_run` picks, one per visually contiguous group of them: bidi can
/// split a run around text that is not in it (ADR-0211). `glyphs` are in visual order.
fn x_spans(glyphs: &[Glyph], in_run: impl Fn(&Glyph) -> bool) -> Vec<(f32, f32)> {
    glyphs
        .chunk_by(|a, b| in_run(a) == in_run(b))
        .filter(|group| in_run(&group[0]))
        .map(|group| {
            group.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), glyph| {
                (lo.min(glyph.x), hi.max(glyph.x + glyph.advance))
            })
        })
        .filter(|(x0, x1)| x0 < x1)
        .collect()
}

impl TextPainter {
    /// `load_fn` must resolve GL function pointers against a context that's already current on
    /// this thread -- FemtoVG doesn't make any context current itself.
    ///
    /// Registers `shaping`'s own `font_chain_data()`, so every face a glyph can name is one femtovg
    /// holds (ADR-0211). Errors if femtovg loads none of it -- there would be nothing to draw with.
    ///
    /// Registers through a `TextContext` and `add_shared_font_with_index` rather than
    /// `Canvas::add_font_mem`, because `add_font_mem` is `data.to_owned()` inside femtovg: it
    /// would give the canvas a private copy of every font file and undo the sharing `FontData`
    /// exists for. `Canvas::add_font_mem` is the only route femtovg exposes on the canvas itself,
    /// so reaching the shared API means building the context first and handing it over.
    pub fn new(
        load_fn: impl FnMut(&str) -> *const c_void,
        width: u32,
        height: u32,
        shaping: ShapingHandle,
    ) -> Result<Self, Box<dyn Error>> {
        // SAFETY: femtovg loads every GL entry point through `load_fn` and calls them on this
        // thread. The caller binds the context with `eglMakeCurrent` before constructing this
        // (`wayland::surface::ensure_bound`, or the headless EGL helper in paint's tests),
        // and the renderer is used only from that same thread.
        let renderer = unsafe { OpenGl::new_from_function(load_fn)? };
        let text_context = TextContext::default();
        let mut canvas = Canvas::new_with_text_context(renderer, text_context.clone())?;
        canvas.set_size(width, height, 1.0);
        let mut registered = HashMap::new();
        // Generation before faces: a face loaded between the two reads leaves this behind, not ahead.
        let generation = shaping.font_generation();
        let faces = register(&text_context, &mut registered, shaping.font_chain_data());
        if faces.is_empty() {
            return Err("TextPainter::new requires at least one loaded font".into());
        }
        Ok(Self {
            canvas,
            faces,
            generation,
            text_context,
            registered,
            shaping,
            scratch: HashMap::new(),
            paints: 0,
            layers: Vec::new(),
            lines_cache: HashMap::new(),
            lines_cache_len: 0,
        })
    }

    /// The shaping-worker face-set generation this painter's femtovg registry is built from.
    #[cfg(test)]
    pub fn font_generation(&self) -> u64 {
        self.generation
    }

    /// An offscreen of exactly `size`, reused from the pool when one is free. The caller clears it:
    /// a reused target still holds the last paint's pixels.
    pub fn take_scratch(&mut self, surface: &str, size: (usize, usize)) -> Option<ImageId> {
        self.scratch.get_mut(&(surface.to_owned(), size))?.1.pop()
    }

    /// What `command` last finished into on `surface`.
    pub fn layer(&self, surface: &str, command: &DrawCmd) -> Option<(Vec<ImageId>, ImageId)> {
        self.layers
            .iter()
            .find(|(on, kept, ..)| on == surface && kept == command)
            .map(|(_, _, images, ..)| images.clone())
    }

    pub fn keep_layer(
        &mut self,
        surface: &str,
        command: &DrawCmd,
        images: (Vec<ImageId>, ImageId),
        size: (usize, usize),
    ) {
        self.layers.push((surface.to_owned(), command.clone(), images, size, self.paints));
    }

    /// Frees `surface`'s layers its list no longer `holds`, then any past [`LAYER_PAINTS`] or
    /// [`LAYER_BYTES`], answering their images for the pool. A held layer lives on while
    /// the region skips it.
    pub fn sweep_layers(&mut self, surface: &str, holds: impl Fn(&DrawCmd) -> bool) -> Vec<(ImageId, (usize, usize))> {
        for (on, kept, .., held) in &mut self.layers {
            if on == surface && holds(kept) {
                *held = self.paints;
            }
        }
        self.layers.sort_unstable_by_key(|(.., held)| std::cmp::Reverse(*held));
        let (paints, mut bytes) = (self.paints, 0);
        let (kept, retired): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.layers).into_iter().partition(|(on, _, (casts, _), (width, height), held)| {
                let live = if on == surface { *held == paints } else { paints - held <= LAYER_PAINTS };
                let size = (1 + casts.len()) * width * height * 4;
                live && bytes + size <= LAYER_BYTES && {
                    bytes += size;
                    true
                }
            });
        self.layers = kept;
        retired
            .into_iter()
            .flat_map(|(.., (casts, content), size, _)| casts.into_iter().chain([content]).map(move |id| (id, size)))
            .collect()
    }

    /// Returns one paint's offscreens to the pool and deletes whatever that pushes over capacity.
    pub fn recycle_scratch(&mut self, surface: &str, used: impl IntoIterator<Item = (ImageId, (usize, usize))>) {
        self.paints += 1;
        for (id, size) in used {
            let entry = self.scratch.entry((surface.to_owned(), size)).or_insert((self.paints, Vec::new()));
            entry.0 = self.paints;
            entry.1.push(id);
        }
        for key in stalest(&self.scratch, self.paints) {
            for id in self.scratch.remove(&key).into_iter().flat_map(|(_, free)| free) {
                self.canvas.delete_image(id);
            }
        }
    }

    /// What the offscreen pool and the kept layers hold, as `(pool images, pool bytes, layers,
    /// layer bytes)`, for `wayland::memory_profile`. Both are GPU images the driver may mirror in
    /// host memory, which `malloc` counts and `image` does not.
    pub fn census(&self) -> (usize, usize, usize, usize) {
        let area = |(width, height): (usize, usize), images: usize| images * width * height * 4;
        let pooled = self.scratch.iter().map(|((_, size), (_, free))| (free.len(), area(*size, free.len())));
        let (images, pool_bytes) = pooled.fold((0, 0), |(n, bytes), (more, extra)| (n + more, bytes + extra));
        let layer_bytes = self.layers.iter().map(|(_, _, (casts, _), size, _)| area(*size, 1 + casts.len())).sum();
        (images, pool_bytes, self.layers.len(), layer_bytes)
    }

    /// Deletes `surface`'s finished layers and pooled offscreens. A gone surface never paints again
    /// to sweep its own, and an idle shell paints too little for [`LAYER_PAINTS`] to age them: a
    /// full-screen lock with a blur held 50 MiB after unlock. The GL context must be current.
    pub fn release_surface(&mut self, surface: &str) {
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.layers).into_iter().partition(|(on, ..)| on == surface);
        self.layers = kept;
        let layers = gone.into_iter().flat_map(|(.., (casts, content), _, _)| casts.into_iter().chain([content]));
        let owned: Vec<_> = self.scratch.keys().filter(|(on, _)| on == surface).cloned().collect();
        let pooled = owned.into_iter().filter_map(|key| self.scratch.remove(&key)).flat_map(|(_, free)| free);
        for id in layers.chain(pooled) {
            self.canvas.delete_image(id);
        }
    }

    /// Registers any faces the shaping worker has loaded since this painter was built (ADR-0144).
    pub fn sync(&mut self) {
        let generation = self.shaping.font_generation();
        if generation == self.generation {
            return;
        }
        let font_chain = self.shaping.font_chain_data();
        if font_chain.is_empty() {
            return;
        }
        debug!("syncing fonts to generation {generation}");
        self.faces = register(&self.text_context, &mut self.registered, font_chain);
        self.generation = generation;
        self.lines_cache.clear();
        self.lines_cache_len = 0;
    }

    /// Updates the canvas's viewport to match the surface's current size. Cheap and idempotent --
    /// callers should call this every frame rather than caching a size from construction time.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.canvas.set_size(width, height, 1.0);
    }

    /// The same canvas `draw_text` fills text onto, exposed so `layout::paint`'s tree walk can
    /// draw a node's background/border on it too (ADR-0023): one canvas per surface, shared by
    /// every paint operation, not one per property kind.
    pub fn canvas_mut(&mut self) -> &mut Canvas<OpenGl> {
        &mut self.canvas
    }

    /// femtovg's id for a face the shaping worker names -- the test seam for checking a face
    /// reached the painter, and kept the id it was first given.
    #[cfg(test)]
    pub fn font_id(&self, face: fontdb::ID) -> Option<FontId> {
        self.faces.get(&face).copied()
    }

    /// Draws `text` with its snapped top-left corner at `rect`'s origin, in `color`, row by row.
    /// `rect` is in buffer pixels; the shaped glyphs are logical, so `scale` places and rasterizes them.
    /// Does not flush or swap buffers: `layout::paint`'s tree walk draws a whole surface's worth of
    /// nodes onto this same canvas and flushes once at the end.
    ///
    /// Rows are the glyphs [`ShapingHandle::shape_lines`] laid out, so measurement and paint share
    /// one shaper (ADR-0211); `runs` (ADR-0104) colour and underline by the byte each glyph came from.
    pub fn draw_text(&mut self, line: TextDraw<'_>, rect: LogicalRect, scale: f32) {
        let TextDraw { text, runs, face, color, align, caret, caret_on, caret_style } = line;
        let Typeface { font_size, line_height, letter_spacing, font_weight, italic, ref variations, ref font } = *face;
        let physical = snap_to_physical(rect, 1.0);
        let step = line_height * scale;
        let thickness = caret_thickness(font_size) * scale;
        let bar_width = caret_style.width * scale;

        let runs_key = font_runs(runs);
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            font_size.to_bits().hash(&mut hasher);
            line_height.to_bits().hash(&mut hasher);
            letter_spacing.to_bits().hash(&mut hasher);
            font_weight.to_bits().hash(&mut hasher);
            italic.hash(&mut hasher);
            variations.hash(&mut hasher);
            font.hash(&mut hasher);
            runs_key.hash(&mut hasher);
            hasher.finish()
        };

        let shaped_lines = if let Some(bucket) = self.lines_cache.get(&hash)
            && let Some((_, lines)) = bucket.iter().find(|(k, _)| k.matches(text, &runs_key, &line))
        {
            Arc::clone(lines)
        } else {
            let lines = Arc::new(self.shaping.shape_lines(text, &runs_key, face.shaping_style(), font.as_ref()));
            // ponytail: 1024 entries bounds lines cache memory. Clears wholesale at cap like shaping cache. Upgrade path: per-frame generational epoch.
            if self.lines_cache_len >= 1024 {
                self.lines_cache.clear();
                self.lines_cache_len = 0;
            }
            self.lines_cache
                .entry(hash)
                .or_default()
                .push((TextLineKey { text: text.to_string(), runs: runs_key, face: face.clone() }, Arc::clone(&lines)));
            self.lines_cache_len += 1;
            lines
        };

        let mut row = 0;
        for (line_start, shaped) in shaped_lines.iter() {
            let line_start = *line_start;
            for laid in shaped.shaped.iter() {
                let baseline = physical.y0 as f32 + row as f32 * step + laid.baseline * scale;
                row += 1;
                // A `textfield`'s selection and caret, in the ink the field already declared for
                // its text (ADR-0236). A draft holds no newline, so only the first row has either.
                let top = baseline - laid.baseline * scale;
                let selection = caret.filter(|_| row == 1);
                // Past the width that fits, the line follows the caret rather than its alignment,
                // or the end of a long draft is drawn outside the field it belongs to (ADR-0236).
                let left = match selection {
                    Some((_, at)) => crate::layout::hit::field_line_left(
                        Some(laid),
                        align,
                        physical.x0 as f32,
                        physical.x1 as f32,
                        at,
                        caret_style.width,
                        scale,
                    ),
                    None => align.line_left(laid.rtl, physical.x0 as f32, physical.x1 as f32, laid.width * scale),
                };
                // Behind the glyphs, so the words inside it stay readable. One rect per visually
                // contiguous stretch: a selection crossing a direction change is not one box.
                if let Some((lo, hi)) = selection.map(|(anchor, at)| (anchor.min(at), anchor.max(at))) {
                    for (x0, x1) in x_spans(&laid.glyphs, |glyph| glyph.start < hi && glyph.end > lo) {
                        self.fill(
                            left + x0 * scale,
                            top,
                            (x1 - x0) * scale,
                            step,
                            0.0,
                            Rgba { a: color.a * 0.3, ..color },
                        );
                    }
                }
                let style = |start: usize| runs.iter().find(|run| run.range.contains(&(line_start + start)));
                let key = |glyph: &Glyph| {
                    (glyph.face, glyph.coords, style(glyph.start).and_then(|run| run.color).unwrap_or(color))
                };
                for group in laid.glyphs.chunk_by(|a, b| key(a) == key(b)) {
                    let glyphs = group.iter().map(|glyph| PositionedGlyph {
                        x: left + glyph.x * scale,
                        y: baseline + glyph.y * scale,
                        glyph_id: glyph.id,
                    });
                    let (face, coords, tint) = key(&group[0]);
                    self.fill_run(face, &shaped.coords[coords as usize], tint, glyphs, font_size * scale);
                }
                // Over them, so a glyph's side bearing cannot swallow it.
                if let Some((.., at)) = selection.filter(|_| caret_on) {
                    let height = caret_style.bar_height(line_height) * scale;
                    self.fill(
                        left + caret_x(laid, at) * scale,
                        top + (step - height) / 2.0,
                        bar_width,
                        height,
                        caret_style.radius * scale,
                        caret_style.color,
                    );
                }

                for run in runs.iter().filter(|run| run.underline) {
                    let tint = run.color.unwrap_or(color);
                    for (x0, x1) in x_spans(&laid.glyphs, |glyph| run.range.contains(&(line_start + glyph.start))) {
                        self.fill(
                            left + x0 * scale,
                            (baseline + thickness).round(),
                            (x1 - x0) * scale,
                            thickness,
                            0.0,
                            tint,
                        );
                    }
                }
            }
        }
    }

    /// One filled rectangle, rounded by `radius` (femtovg squares off a zero one): an underline, a caret, or the highlight behind a selection.
    fn fill(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32, color: Rgba) {
        let mut path = Path::new();
        path.rounded_rect(x, y, width, height, radius);
        self.canvas.fill_path(&path, &Paint::color(color.into()));
    }

    /// Draws `glyphs` in `face` at the axis `coords` Parley shaped them at; a face femtovg never
    /// registered draws nothing.
    fn fill_run(
        &mut self,
        face: fontdb::ID,
        coords: &[i16],
        tint: Rgba,
        glyphs: impl IntoIterator<Item = PositionedGlyph>,
        font_size: f32,
    ) {
        let Some(font) = self.faces.get(&face) else { return };
        let mut paint = Paint::color(tint.into());
        paint.set_font_size(font_size);
        let _ = self.canvas.fill_glyph_run(*font, coords, glyphs, &paint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_tweening_its_width_does_not_keep_every_size_it_passed_through() {
        // Each frame of the tween is a size nothing will ask for again. Without a cap the pool
        // holds one target per pixel of travel for the rest of the session.
        let pool = |sizes: std::ops::Range<usize>| {
            sizes.map(|n| ((String::new(), (n, 40)), (n as u64, Vec::<()>::new()))).collect::<HashMap<_, _>>()
        };
        let now = 100;
        assert!(stalest(&pool(0..SCRATCH_SIZES), now).is_empty(), "a pool at capacity deletes nothing");

        let evicted = stalest(&pool(0..SCRATCH_SIZES + 3), now);
        assert_eq!(evicted.len(), 3, "only the overflow goes");
        assert_eq!(
            evicted.iter().map(|k| k.1).collect::<Vec<_>>(),
            vec![(0, 40), (1, 40), (2, 40)],
            "asked for longest ago, not largest or newest"
        );
    }

    #[test]
    fn a_pool_over_its_bytes_deletes_the_stalest_sizes_first() {
        let free = |count: usize| vec![(); count];
        // 3440x1440 is 19.8 MB: four pooled is 79 MB, over the 64 MiB cap.
        let pool = HashMap::from([
            ((String::new(), (3440, 1440)), (1, free(2))),
            ((String::new(), (1720, 720)), (2, free(1))),
            ((String::new(), (3440, 1441)), (3, free(2))),
            ((String::new(), (8, 8)), (4, free(1))),
        ]);
        assert_eq!(stalest(&pool, 4), [(String::new(), (3440, 1440))], "the oldest alone brings it under");
        assert!(stalest(&pool, 1).is_empty(), "the current paint's size stays, over the cap or not");
    }

    /// ADR-0262. A paint asking for more sizes than the cap keeps them all, or the next frame
    /// allocates its blurs' chains again.
    #[test]
    fn a_paint_keeps_every_size_it_asked_for() {
        let pool = |asked: fn(usize) -> u64| {
            (0..SCRATCH_SIZES + 3)
                .map(|n| ((String::new(), (n, 40)), (asked(n), Vec::<()>::new())))
                .collect::<HashMap<_, _>>()
        };
        let evicted = stalest(&pool(|n| 7 + (n % 2) as u64), 8);
        assert_eq!(evicted.len(), 3);
        assert!(evicted.iter().all(|(_, (n, _))| n % 2 == 0), "only the older paint's: {evicted:?}");
        assert!(stalest(&pool(|_| 8), 8).is_empty());
    }

    /// A run bidi splits around other text is drawn piece by piece, never across the text between
    /// the pieces -- for an underline, and for the selection behind a `textfield` (ADR-0236).
    #[test]
    fn a_run_split_around_other_text_is_drawn_under_each_piece() {
        let glyph = |x: f32, start: usize| Glyph {
            face: fontdb::ID::dummy(),
            coords: 0,
            id: 0,
            x,
            y: 0.0,
            advance: 10.0,
            start,
            end: start + 1,
            rtl: false,
        };
        let glyphs = [glyph(0.0, 4), glyph(10.0, 5), glyph(20.0, 0), glyph(30.0, 6)];
        assert_eq!(x_spans(&glyphs, |glyph| glyph.start >= 4), vec![(0.0, 20.0), (30.0, 40.0)]);
        assert_eq!(x_spans(&glyphs, |glyph| glyph.start == 9), Vec::new());
        // Bytes 4..7 are contiguous in the source and two boxes on screen, which is why a
        // selection cannot be one rect.
        let (lo, hi) = (4, 7);
        assert_eq!(
            x_spans(&glyphs, |glyph| glyph.start < hi && glyph.end > lo),
            vec![(0.0, 20.0), (30.0, 40.0)],
            "one rect per visually contiguous stretch"
        );
    }
}

//! Glyph rasterization and GPU texture atlas management, via FemtoVG.
//!
//! FemtoVG owns its glyph atlas entirely internally (see ADR-0012): rasterized glyphs pack
//! into private atlas pages that start at a fixed size and grow by adding further pages, not by
//! expanding one large texture. There is no public API to configure a single fixed-size page.
//! What it draws is Parley's (ADR-0211): `fill_glyph_run` takes glyphs another shaper placed,
//! so femtovg rasterizes and packs, and shapes nothing.

use rustc_hash::FxHashMap as HashMap;
use std::error::Error;
use std::ffi::c_void;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, FontId, ImageId, Paint, Path, PositionedGlyph, TextContext};
use shared::debug;

use crate::layout::field_rows;
use crate::layout::node::{CaretStyle, Rgba, StyleRun, TextAlign, Typeface, font_runs};
use crate::layout::paint::{Draw, DrawCmd};
use crate::text::shaping::{
    FontFace, Glyph, ShapedLine, ShapingHandle, caret_thickness, caret_x, cluster_span, clusters,
};

use super::snap::{LogicalRect, PhysicalRect, snap_to_physical};

/// Distinct offscreen sizes [`TextPainter`] keeps between paints (ADR-0217).
const SCRATCH_SIZES: usize = 16;
/// The bytes those pooled offscreens may hold between paints, as [`LAYER_BYTES`] caps layers.
const SCRATCH_BYTES: usize = 64 << 20;

/// Paints, by any surface, a finished layer outlives the last paint of its own surface's list that
/// held it, and the bytes all of them may hold, least recently held first (ADR-0258). The age
/// is a fallback for a surface whose layers [`TextPainter::release_surface`] did not free: every
/// paint of a live one sweeps its own.
pub(crate) const LAYER_PAINTS: u64 = 1000;
const LAYER_BYTES: usize = 64 << 20;

/// A layer's shadows, one image per layer it casts, and content, finished on one surface for the
/// command that drew them, whether every kept image is safe to sample off texel centres (exact size
/// or edge-padded slack), and the paint that last held it.
type KeptLayer = (String, DrawCmd, (Vec<ImageId>, ImageId), bool, u64);

/// An unfiltered layer subtree kept across frames while its `effect.blur` or `effect.tone` tweens,
/// so the blur reads the cached raster instead of repainting and flushing every frame (#198).
type KeptSource = (String, PhysicalRect, Vec<DrawCmd>, ImageId, u64);

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
    /// `layout::paint::canvas::offscreen::draw_clipped`'s offscreen targets, kept between paints and keyed by
    /// allocated size, each with the paint that last asked for that size (ADR-0217, ADR-0370).
    /// Here, not beside `ImageCache`, because the ids belong to `canvas` and have to die with it.
    scratch: HashMap<PoolKey, (u64, Vec<ImageId>)>,
    /// What [`TextPainter::recycle_scratch`] ages by. Not a timer: a size goes stale because other
    /// sizes were asked for since, not because seconds passed.
    paints: u64,
    /// `layout::paint::canvas::draw_layer`'s finished images, kept here beside `scratch` for the
    /// same reason (ADR-0258).
    layers: Vec<KeptLayer>,
    /// Unfiltered layer subtrees kept only while their filter parameters tween (#198).
    sources: Vec<KeptSource>,
    /// Subtrees of layers retired on the current paint of a surface, used to detect active filter
    /// tweens without retaining unfiltered sources for static layers.
    retired_layers: Vec<Vec<DrawCmd>>,
    /// `GL_MAX_TEXTURE_SIZE`: femtovg 0.27 creates a larger texture without checking, and its
    /// framebuffer comes out incomplete, so the subtree drew into the parent with no clip.
    max_texture: usize,
}

/// A pooled offscreen's owner and allocated size. Owned, so a released surface frees its own and no
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
    /// A focused plain `textfield`'s `(anchor, caret, upstream)` (ADR-0236, [`field_rows::row_of`]).
    pub caret: Option<(usize, usize, bool)>,
    /// The blink's phase: off drops the bar and keeps the selection and scroll.
    pub caret_on: bool,
    pub caret_style: CaretStyle,
    /// A `multiline` field's wrap width and row scroll, logical px.
    pub wrap: Option<(f32, f32)>,
}

/// Registers every face of `font_chain` femtovg does not hold yet, and maps each face's shaping id
/// to its `FontId`. `registered` carries the ids femtovg minted across calls, because it mints a
/// new one every time it is asked (see [`TextPainter::registered`]).
fn register(
    text_context: &TextContext,
    registered: &mut HashMap<(usize, u32), FontId>,
    font_chain: Vec<FontFace>,
) -> HashMap<fontdb::ID, FontId> {
    let mut faces = HashMap::with_capacity_and_hasher(font_chain.len(), Default::default());
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

/// The spans under the line-relative bytes `range`, one per visually contiguous stretch: bidi can
/// split a run around text that is not in it (ADR-0211). `glyphs` are in visual order. A cluster
/// partly in `range`, as a ligature joining a struck letter to a plain one, gives the share of its
/// width those graphemes hold, as Parley splits a ligature's advance evenly over them. `text` is
/// what the glyphs' byte offsets index.
pub(crate) fn x_spans(text: &str, glyphs: &[Glyph], range: Range<usize>) -> Vec<(f32, f32)> {
    let graphemes = |from: usize, to: usize| text.get(from..to).map_or(to - from, |s| s.graphemes(true).count());
    let mut spans: Vec<(f32, f32)> = Vec::new();
    let mut open = false;
    for cluster in clusters(glyphs) {
        let (start, end) = (cluster[0].start as usize, cluster[0].end as usize);
        let (lo, hi) = (start.max(range.start), end.min(range.end));
        if lo >= hi {
            open = false;
            continue;
        }
        let (x0, x1) = cluster_span(cluster);
        let share = |at: usize| graphemes(start, at) as f32 / graphemes(start, end) as f32 * (x1 - x0);
        // A covered edge is the cluster's own, never one rebuilt in floats, so abutting clusters
        // meet exactly. Only a stretch reaching the right edge runs on into the next cluster: the
        // uncovered half of a ligature between them is not bridged.
        let (left_in, right_in) = match cluster[0].rtl {
            false => (lo == start, hi == end),
            true => (hi == end, lo == start),
        };
        let a = match (left_in, cluster[0].rtl) {
            (true, _) => x0,
            (false, false) => x0 + share(lo),
            (false, true) => x1 - share(hi),
        };
        let b = match (right_in, cluster[0].rtl) {
            (true, _) => x1,
            (false, false) => x0 + share(hi),
            (false, true) => x1 - share(lo),
        };
        match spans.last_mut().filter(|_| open && left_in) {
            Some(span) => *span = (span.0.min(a), span.1.max(b)),
            None => spans.push((a, b)),
        }
        open = right_in;
    }
    spans.retain(|(x0, x1)| x0 < x1);
    spans
}

/// The underlines and strike-throughs `runs` draw on `laid`, as `(x, y, width, tint)` rects
/// `thickness` tall, with the line at `left` and `baseline` in buffer px. `text` is the paragraph
/// starting at byte `line_start`. Both lines take the underline's weight, so a run with both draws
/// them alike.
fn decorations(
    (text, line_start, laid): (&str, usize, &ShapedLine),
    runs: &[StyleRun],
    strikes: &[(fontdb::ID, u32, f32)],
    color: Rgba,
    (left, baseline, scale): (f32, f32, f32),
    thickness: f32,
) -> Vec<(f32, f32, f32, Rgba)> {
    let mut rects = Vec::new();
    for run in runs.iter().filter(|run| run.underline || run.strikethrough) {
        let range = run.range.start.saturating_sub(line_start)..run.range.end.saturating_sub(line_start);
        // At the face instance most of the run's glyphs here are in, as Pango weighs one across
        // fonts: a fallback icon in the run does not move it, and a neighbour in another font never
        // does. Counted in one pass, and only for a run that strikes.
        let mut counts: Vec<((fontdb::ID, u32), usize)> = Vec::new();
        for glyph in laid.glyphs.iter().filter(|glyph| run.strikethrough && selected(glyph, range.start, range.end)) {
            match counts.iter_mut().find(|(key, _)| *key == (glyph.face, glyph.coords)) {
                Some((_, count)) => *count += 1,
                None => counts.push(((glyph.face, glyph.coords), 1)),
            }
        }
        let face = counts.iter().max_by_key(|(_, count)| *count).map(|(key, _)| *key);
        let centre = strikes.iter().find(|(id, coords, _)| Some((*id, *coords)) == face).map_or(0.0, |strike| strike.2);
        let lines = [
            (run.underline, (baseline + thickness).round()),
            (run.strikethrough, (baseline - centre * scale - thickness / 2.0).round()),
        ];
        for (x0, x1) in x_spans(text, &laid.glyphs, range) {
            for (_, y) in lines.iter().filter(|(on, _)| *on) {
                rects.push((left + x0 * scale, *y, (x1 - x0) * scale, run.color.unwrap_or(color)));
            }
        }
    }
    rects
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
        gl: &glow::Context,
        width: u32,
        height: u32,
        shaping: ShapingHandle,
    ) -> Result<Self, Box<dyn Error>> {
        // SAFETY: femtovg loads every GL entry point through `load_fn` and calls them on this
        // thread. The caller binds the context with `eglMakeCurrent` before constructing this
        // (`wayland::surface::ensure_bound`, or the headless EGL helper in paint's tests),
        // and the renderer is used only from that same thread. `gl` is that context.
        let (max_texture, renderer) = unsafe {
            use glow::HasContext;
            (gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE), OpenGl::new_from_function(load_fn)?)
        };
        let text_context = TextContext::default();
        let mut canvas = Canvas::new_with_text_context(renderer, text_context.clone())?;
        canvas.set_size(width, height, 1.0);
        let mut registered = HashMap::default();
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
            scratch: HashMap::default(),
            paints: 0,
            layers: Vec::new(),
            sources: Vec::new(),
            retired_layers: Vec::new(),
            max_texture: usize::try_from(max_texture).unwrap_or(0),
        })
    }

    /// The largest side, in pixels, a texture this context can hold.
    pub fn max_texture(&self) -> usize {
        self.max_texture
    }

    /// Lowers [`TextPainter::max_texture`], so a test can pass it without a gigabyte target.
    #[cfg(test)]
    pub fn cap_texture(&mut self, side: usize) {
        self.max_texture = side;
    }

    /// The shaping-worker face-set generation this painter's femtovg registry is built from.
    #[cfg(test)]
    pub fn font_generation(&self) -> u64 {
        self.generation
    }

    /// An offscreen allocated at `size`, reused from the pool when one is free. The caller clears
    /// it: a reused target still holds the last paint's pixels.
    pub fn take_scratch(&mut self, surface: &str, size: (usize, usize)) -> Option<ImageId> {
        self.scratch.get_mut(&(surface.to_owned(), size))?.1.pop()
    }

    /// Returns a dead offscreen to the pool mid-frame so later layers in the same paint reuse it.
    /// Stamped for the coming [`Self::recycle_scratch`] tick so `stalest` treats it as active on
    /// this paint.
    pub fn return_scratch(&mut self, surface: &str, id: ImageId, size: (usize, usize)) {
        let now = self.paints + 1;
        let entry = self.scratch.entry((surface.to_owned(), size)).or_insert((now, Vec::new()));
        entry.0 = now;
        entry.1.push(id);
    }

    /// What `command` last finished into on `surface`, and whether every image in it is safe to
    /// sample off texel centres.
    pub fn layer(&self, surface: &str, command: &DrawCmd) -> Option<((Vec<ImageId>, ImageId), bool)> {
        self.layers
            .iter()
            .find(|(on, kept, ..)| on == surface && kept == command)
            .map(|(_, _, images, clamped, _)| (images.clone(), *clamped))
    }

    /// Keeps `images` for `command`, answering the images of one it replaces for the pool.
    pub fn keep_layer(
        &mut self,
        surface: &str,
        command: &DrawCmd,
        images: (Vec<ImageId>, ImageId),
        clamped: bool,
    ) -> Vec<(ImageId, (usize, usize))> {
        let old = self.layers.iter().position(|(on, kept, ..)| on == surface && kept == command);
        let old = old.map(|at| self.layers.swap_remove(at));
        self.layers.push((surface.to_owned(), command.clone(), images, clamped, self.paints));
        self.pooled(old.into_iter().flat_map(|(.., (casts, content), _, _)| casts.into_iter().chain([content])))
    }

    /// A cached unfiltered raster for `commands` at `clip` on `surface`, refreshed for this paint.
    pub fn layer_source(&mut self, surface: &str, clip: PhysicalRect, commands: &[DrawCmd]) -> Option<ImageId> {
        let paints = self.paints;
        let entry = self
            .sources
            .iter_mut()
            .find(|(on, at, kept, ..)| on == surface && *at == clip && kept.as_slice() == commands)?;
        entry.4 = paints;
        Some(entry.3)
    }

    /// Whether a layer with `commands` was just retired on this paint (its filter parameters moved).
    pub fn layer_tweening(&self, commands: &[DrawCmd]) -> bool {
        self.retired_layers.iter().any(|retired| retired.as_slice() == commands)
    }

    /// Keeps `image` as the unfiltered source for `commands` at `clip`, answering any replaced
    /// image for the pool.
    pub fn keep_source(
        &mut self,
        surface: &str,
        clip: PhysicalRect,
        commands: &[DrawCmd],
        image: ImageId,
    ) -> Option<(ImageId, (usize, usize))> {
        let old = self
            .sources
            .iter()
            .position(|(on, _, kept, ..)| on == surface && kept.as_slice() == commands)
            .map(|at| self.sources.swap_remove(at).3);
        self.sources.push((surface.to_owned(), clip, commands.to_vec(), image, self.paints));
        old.map(|id| (id, self.canvas.image_size(id).unwrap_or_default()))
    }

    /// Drops `surface`'s layers its list no longer `holds`, and any cached unfiltered sources whose
    /// layer is no longer changing, answering their images so this paint can reuse them immediately.
    pub fn prune_surface_layers(
        &mut self,
        surface: &str,
        holds: impl Fn(&DrawCmd) -> bool,
    ) -> Vec<(ImageId, (usize, usize))> {
        self.retired_layers.clear();
        let mut retired_ids = Vec::new();
        let paints = self.paints;
        let mut kept_layers = Vec::with_capacity(self.layers.len());
        for (on, kept, images, clamped, mut held) in std::mem::take(&mut self.layers) {
            if on == surface {
                if holds(&kept) {
                    held = paints;
                    kept_layers.push((on, kept, images, clamped, held));
                } else {
                    retired_ids.extend(images.0.into_iter().chain([images.1]));
                    if let Draw::Layer { shader: None, silhouette: false, commands, .. } = kept.draw {
                        self.retired_layers.push(commands);
                    }
                }
            } else {
                kept_layers.push((on, kept, images, clamped, held));
            }
        }
        self.layers = kept_layers;
        let (kept_sources, retired_sources): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.sources).into_iter().partition(|(on, _, commands, ..)| {
                on != surface || self.retired_layers.iter().any(|retired| retired == commands)
            });
        self.sources = kept_sources;
        retired_ids.extend(retired_sources.into_iter().map(|(_, _, _, id, _)| id));
        self.pooled(retired_ids.into_iter())
    }

    /// Enforces [`LAYER_PAINTS`] and [`LAYER_BYTES`] across kept layers and tweening sources after
    /// a paint, answering evicted images for the pool.
    pub fn trim_layers(&mut self, surface: &str) -> Vec<(ImageId, (usize, usize))> {
        self.retired_layers.clear();
        self.layers.sort_unstable_by_key(|(.., held)| std::cmp::Reverse(*held));
        self.sources.sort_unstable_by_key(|(.., held)| std::cmp::Reverse(*held));
        let (paints, mut bytes) = (self.paints, 0);
        let (kept, retired): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.layers).into_iter().partition(|(on, _, images, _, held)| {
                let live = if on == surface { *held == paints } else { paints - held <= LAYER_PAINTS };
                let size = self.layer_bytes(images);
                live && bytes + size <= LAYER_BYTES && {
                    bytes += size;
                    true
                }
            });
        self.layers = kept;
        let (kept_sources, retired_sources): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.sources).into_iter().partition(|(on, _, _, id, held)| {
                let live = if on == surface { *held == paints } else { paints - held <= LAYER_PAINTS };
                let size = self.image_bytes(*id);
                live && bytes + size <= LAYER_BYTES && {
                    bytes += size;
                    true
                }
            });
        self.sources = kept_sources;
        self.pooled(
            retired
                .into_iter()
                .flat_map(|(.., (casts, content), _, _)| casts.into_iter().chain([content]))
                .chain(retired_sources.into_iter().map(|(_, _, _, id, _)| id)),
        )
    }

    /// Frees `surface`'s layers its list no longer `holds`, then any past [`LAYER_PAINTS`] or
    /// [`LAYER_BYTES`], answering their images for the pool. A held layer lives on while
    /// the region skips it.
    #[cfg(test)]
    pub fn sweep_layers(&mut self, surface: &str, holds: impl Fn(&DrawCmd) -> bool) -> Vec<(ImageId, (usize, usize))> {
        let mut retired = self.prune_surface_layers(surface, holds);
        retired.extend(self.trim_layers(surface));
        retired
    }

    fn image_bytes(&self, id: ImageId) -> usize {
        self.canvas.image_size(id).map_or(0, |(w, h)| w * h * 4)
    }

    /// What a layer's images hold, at the sizes they are allocated at.
    fn layer_bytes(&self, (casts, content): &(Vec<ImageId>, ImageId)) -> usize {
        casts.iter().chain([content]).map(|&id| self.image_bytes(id)).sum()
    }

    /// `images` with the sizes they are allocated at, which the pool keys by (ADR-0370).
    fn pooled(&self, images: impl Iterator<Item = ImageId>) -> Vec<(ImageId, (usize, usize))> {
        images.map(|id| (id, self.canvas.image_size(id).unwrap_or_default())).collect()
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
        let pooled = self.scratch.iter().map(|((_, (w, h)), (_, free))| (free.len(), free.len() * w * h * 4));
        let (images, pool_bytes) = pooled.fold((0, 0), |(n, bytes), (more, extra)| (n + more, bytes + extra));
        let layer_bytes = self.layers.iter().map(|(_, _, images, ..)| self.layer_bytes(images)).sum::<usize>()
            + self.sources.iter().map(|(_, _, _, id, _)| self.image_bytes(*id)).sum::<usize>();
        (images, pool_bytes, self.layers.len() + self.sources.len(), layer_bytes)
    }

    /// Deletes `surface`'s finished layers and pooled offscreens. A gone surface never paints again
    /// to sweep its own, and an idle shell paints too little for [`LAYER_PAINTS`] to age them: a
    /// full-screen lock with a blur held 50 MiB after unlock. The GL context must be current.
    pub fn release_surface(&mut self, surface: &str) {
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.layers).into_iter().partition(|(on, ..)| on == surface);
        self.layers = kept;
        let (gone_sources, kept_sources): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.sources).into_iter().partition(|(on, ..)| on == surface);
        self.sources = kept_sources;
        let layers = gone.into_iter().flat_map(|(.., (casts, content), _, _)| casts.into_iter().chain([content]));
        let sources = gone_sources.into_iter().map(|(_, _, _, id, _)| id);
        let owned: Vec<_> = self.scratch.keys().filter(|(on, _)| on == surface).cloned().collect();
        let pooled = owned.into_iter().filter_map(|key| self.scratch.remove(&key)).flat_map(|(_, free)| free);
        for id in layers.chain(sources).chain(pooled) {
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
        // A face landed in the background can fill what a kept raster drew as a box (#55).
        for (.., (casts, content), _, _) in std::mem::take(&mut self.layers) {
            for id in casts.into_iter().chain([content]) {
                self.canvas.delete_image(id);
            }
        }
        for (_, _, _, id, _) in std::mem::take(&mut self.sources) {
            self.canvas.delete_image(id);
        }
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
    /// one shaper (ADR-0211); `runs` (ADR-0104) colour and decorate by the byte each glyph came from.
    pub fn draw_text(&mut self, line: TextDraw<'_>, rect: LogicalRect, scale: f32) {
        let TextDraw { text, runs, face, color, align, caret, caret_on, caret_style, wrap } = line;
        let Typeface { font_size, line_height, ref font, .. } = *face;
        let physical = snap_to_physical(rect, 1.0);
        let step = line_height * scale;
        let thickness = caret_thickness(font_size) * scale;
        let bar_width = caret_style.width * scale;

        let width = wrap.map(|(width, _)| width);
        let shaped_lines = self.shaping.shape_lines(text, &font_runs(runs), face.shaping_style(), font.as_ref(), width);

        // A multiline field's rows: which one the caret is on, and how far they scroll up.
        let (caret_row, top) = match wrap {
            Some((_, scroll)) => {
                let rows = field_rows::rows(text, &shaped_lines);
                let scrolled =
                    field_rows::clamp_scroll(scroll * scale, rows.len(), step, (physical.y1 - physical.y0) as f32);
                (caret.map(|(_, at, upstream)| field_rows::row_of(&rows, at, upstream)), physical.y0 as f32 - scrolled)
            }
            None => (None, physical.y0 as f32),
        };
        let x1 = wrap.map_or(physical.x1 as f32, |(width, _)| physical.x0 as f32 + width * scale);
        let mut row = 0;
        for (line_start, shaped) in shaped_lines.iter() {
            let line_start = *line_start;
            let paragraph = text.get(line_start..).unwrap_or_default();
            for laid in shaped.shaped.iter() {
                let baseline = top + row as f32 * step + laid.baseline * scale;
                row += 1;
                // A `textfield`'s selection and caret, in the ink the field already declared for
                // its text (ADR-0236). A draft holds no newline, so only the first row has either.
                // A multiline selection spans rows, its offsets counted from each paragraph's start.
                let top = baseline - laid.baseline * scale;
                let selection = match wrap {
                    Some(_) => {
                        caret.map(|(anchor, at, _)| (anchor.saturating_sub(line_start), at.saturating_sub(line_start)))
                    }
                    None => caret.filter(|_| row == 1).map(|(anchor, at, _)| (anchor, at)),
                };
                let bar_here = wrap.is_none() || caret_row == Some(row - 1);
                // Past the width that fits, the line follows the caret rather than its alignment,
                // or the end of a long draft is drawn outside the field it belongs to (ADR-0236).
                let left = match selection.filter(|_| wrap.is_none()) {
                    Some((_, at)) => crate::layout::hit::field_line_left(
                        paragraph,
                        Some(laid),
                        align,
                        physical.x0 as f32,
                        physical.x1 as f32,
                        at,
                        caret_style.width,
                        scale,
                    ),
                    None => align.line_left(laid.rtl, physical.x0 as f32, x1, laid.width * scale),
                };
                // Behind the glyphs, so the words inside it stay readable. One rect per visually
                // contiguous stretch: a selection crossing a direction change is not one box.
                if let Some((lo, hi)) = selection.map(|(anchor, at)| (anchor.min(at), anchor.max(at))) {
                    for (x0, x1) in x_spans(paragraph, &laid.glyphs, lo..hi) {
                        self.fill(
                            left + x0 * scale,
                            top,
                            (x1 - x0) * scale,
                            step,
                            0.0,
                            caret_style.selection.unwrap_or(Rgba { a: color.a * 0.3, ..color }),
                        );
                    }
                }
                let style = |start: usize| runs.iter().find(|run| run.range.contains(&(line_start + start)));
                let selected_text = caret_style.selected_text.zip(selection.map(|(a, b)| (a.min(b), a.max(b))));
                let key = |glyph: &Glyph| {
                    let ink = match selected_text {
                        Some((ink, (lo, hi))) if selected(glyph, lo, hi) => ink,
                        _ => style(glyph.start as usize).and_then(|run| run.color).unwrap_or(color),
                    };
                    (glyph.face, glyph.coords, ink)
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
                if let Some((.., at)) = selection.filter(|_| caret_on && bar_here) {
                    let height = caret_style.bar_height(line_height) * scale;
                    self.fill(
                        left + caret_x(paragraph, laid, at) * scale,
                        top + (step - height) / 2.0,
                        bar_width,
                        height,
                        caret_style.radius * scale,
                        caret_style.color,
                    );
                }

                let at = (left, baseline, scale);
                let line = (paragraph, line_start, laid);
                for (x, y, width, tint) in decorations(line, runs, &shaped.strikes, color, at, thickness) {
                    self.fill(x, y, width, thickness, 0.0, tint);
                }
            }
        }
    }

    /// One filled rectangle, rounded by `radius` (femtovg squares off a zero one): an underline, a strike-through, a caret, or the highlight behind a selection.
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

/// Whether `glyph` lies in the selected bytes `lo..hi`.
fn selected(glyph: &Glyph, lo: usize, hi: usize) -> bool {
    lo < hi && (glyph.start as usize) < hi && glyph.end as usize > lo
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
        let pool = HashMap::from_iter([
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
        let glyph = |x: f32, start: u32| Glyph {
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
        assert_eq!(x_spans("", &glyphs, 4..9), vec![(0.0, 20.0), (30.0, 40.0)]);
        assert_eq!(x_spans("", &glyphs, 9..10), Vec::new());
        // Bytes 4..7 are contiguous in the source and two boxes on screen, which is why a
        // selection cannot be one rect.
        let (lo, hi) = (4, 7);
        assert_eq!(
            x_spans("", &glyphs, lo..hi),
            vec![(0.0, 20.0), (30.0, 40.0)],
            "one rect per visually contiguous stretch"
        );
        // Half-open: the glyph ending at `lo` and the one starting at `hi` are outside it.
        assert_eq!(x_spans("", &glyphs, 1..5), vec![(0.0, 10.0)]);
        // A right-to-left ligature of bytes 0..2 leads on the right, so its second byte is the left half.
        let lam_alef = Glyph { end: 2, advance: 12.0, rtl: true, ..glyph(0.0, 0) };
        assert_eq!(x_spans("", &[lam_alef], 1..2), vec![(0.0, 6.0)]);
        // A cluster covered only on one side ends the stretch there: the half outside the range is
        // not bridged to the next cluster.
        let fi = Glyph { end: 2, advance: 12.0, ..glyph(0.0, 0) };
        let space = Glyph { x: 12.0, advance: 4.0, ..glyph(0.0, 2) };
        assert_eq!(x_spans("", &[fi, space], 0..1), vec![(0.0, 6.0)], "the `f` half alone");
        // `مرحبا fi` leaving `i` plain: the space after `fi` on screen is the byte before it.
        let fi_after = Glyph { start: 1, end: 3, ..fi };
        let space_before = Glyph { x: 12.0, advance: 4.0, ..glyph(0.0, 0) };
        assert_eq!(
            x_spans("", &[fi_after, space_before], 0..2),
            vec![(0.0, 6.0), (12.0, 16.0)],
            "`f`, not `i`, then the space"
        );
        // Shares are graphemes, as Parley splits a ligature: `f̩` is three bytes and one of two.
        assert_eq!(x_spans("f\u{0329}i", &[Glyph { end: 4, ..fi }], 3..4), vec![(6.0, 12.0)]);
        let lam_alef_at = Glyph { x: 10.0, end: 3, start: 1, advance: 12.0, rtl: true, ..glyph(0.0, 0) };
        assert_eq!(
            x_spans("", &[glyph(0.0, 0), lam_alef_at], 0..2),
            vec![(0.0, 10.0), (16.0, 22.0)],
            "`ab لا` leaving `ا` plain"
        );
        assert!(!selected(&fi, 1, 1), "an empty selection inside a ligature selects nothing");
        assert!(!selected(&glyph(0.0, 0), 1, 2), "half-open: the glyph ending at `lo` is outside");
        // Fractional advances: a fully covered cluster keeps its exact edges, so the stretch stays one.
        for rtl in [false, true] {
            let left = Glyph { x: 0.73, advance: 11.41, rtl, ..glyph(0.0, 0) };
            let right = Glyph { x: left.x + left.advance, advance: 11.41, rtl, ..glyph(0.0, 1) };
            let x1 = right.x + right.advance;
            assert_eq!(x_spans("", &[left, right], 0..2), vec![(0.73, x1)], "one span, rtl {rtl}");
        }
        let zero_width = Glyph { advance: 0.0, ..glyph(0.0, 0) };
        assert_eq!(x_spans("", &[zero_width], 0..1), Vec::new(), "a mark with no advance draws no line");
    }

    /// #170: a strike or underline on the `i` of an `fi` ligature covers its half of the glyph,
    /// and each run is struck at its own face's height, at scale 2.
    #[test]
    fn decorations_cover_half_a_ligature_at_their_own_face_height() {
        let mut db = fontdb::Database::new();
        let fonts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts");
        for file in ["NotoSans-Regular.ttf", "NotoSansSymbols2-Regular.ttf"] {
            db.load_font_file(fonts.join(file)).unwrap();
        }
        let ids: Vec<_> = db.faces().map(|face| face.id).collect();
        let (latin, symbols) = (ids[0], ids[1]);
        let glyph = |face, x: f32, advance: f32, start: u32, end: u32| Glyph {
            face,
            coords: 0,
            id: 0,
            x,
            y: 0.0,
            advance,
            start,
            end,
            rtl: false,
        };
        // "afi✓b": a plain `a`, the `fi` ligature (bytes 1..3), a check mark in a fallback face, a `b`.
        let laid = ShapedLine {
            rtl: false,
            width: 42.0,
            baseline: 15.0,
            glyphs: vec![
                glyph(latin, 0.0, 10.0, 0, 1),
                glyph(latin, 10.0, 12.0, 1, 3),
                glyph(symbols, 22.0, 10.0, 3, 6),
                glyph(latin, 32.0, 10.0, 6, 7),
            ]
            .into(),
        };
        let run = |range: Range<usize>, underline: bool| StyleRun {
            range,
            bold: false,
            italic: false,
            underline,
            strikethrough: true,
            color: None,
            href: None,
        };
        let strikes = [(latin, 0, 5.0), (symbols, 0, 3.0), (latin, 1, 4.0)];
        let ink = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        // The line starts at byte 10 of the text, 100 px in, its baseline at 40.
        let rects = decorations(
            ("", 10, &laid),
            &[run(12..13, true), run(13..16, false)],
            &strikes,
            ink,
            (100.0, 40.0, 2.0),
            2.0,
        );
        assert_eq!(
            rects,
            vec![(132.0, 42.0, 12.0, ink), (132.0, 29.0, 12.0, ink), (144.0, 33.0, 20.0, ink)],
            "the `i`'s half of `fi` underlined and struck at Noto Sans's height, the check mark at its own"
        );
        let whole = decorations(("", 10, &laid), &[run(10..17, false)], &strikes, ink, (100.0, 40.0, 2.0), 2.0);
        assert_eq!(
            whole,
            vec![(100.0, 29.0, 84.0, ink)],
            "three Latin glyphs to one symbol: struck at Noto Sans's height"
        );
        // A bolder instance of the same variable face: struck at its own height.
        let bold =
            ShapedLine { glyphs: laid.glyphs.iter().map(|glyph| Glyph { coords: 1, ..*glyph }).collect(), ..laid };
        let instance = decorations(("", 10, &bold), &[run(10..11, false)], &strikes, ink, (100.0, 40.0, 2.0), 2.0);
        assert_eq!(instance, vec![(100.0, 31.0, 20.0, ink)]);
        // The symbol first, then two Latin glyphs: the majority still wins.
        let order = [glyph(symbols, 0.0, 10.0, 0, 3), glyph(latin, 10.0, 10.0, 3, 4), glyph(latin, 20.0, 10.0, 4, 5)];
        let first = ShapedLine { glyphs: order.into(), ..bold };
        let majority = decorations(("", 0, &first), &[run(0..5, false)], &strikes, ink, (100.0, 40.0, 2.0), 2.0);
        assert_eq!(majority, vec![(100.0, 29.0, 60.0, ink)]);
    }
}

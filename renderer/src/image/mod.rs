//! Decodes, caches, and fits images into GPU textures (ADR-0054). PNG/JPEG/WebP use `image`
//! (`decode::decode_raster`); SVG uses `resvg` because Adwaita ships scalable icons; a GIF is one texture
//! plus its frames' changed rects, replayed onto it as the slot's elapsed time advances
//! (`decode::decode_gif`, ADR-0233, ADR-0235).
//!
//! The key is path plus physical-pixel box: a vector made for 12px would blur at 24px, while a
//! raster is downscaled to cover its box (ADR-0122), so a 4K wallpaper in a 230px thumbnail is a
//! 230px texture, not 32MB. Mtime and length (ADR-0031) refresh tray files overwritten in place.
//! Failures are cached as `Failed`, unreadable files included; a *missing* file retries when its
//! key changes on appearance.
//!
//! Decoding is inline by default, or on a worker pool for `async = true` (ADR-0122). The slot is
//! `Pending` until [`ImageCache::poll`] finds pixels; [`ImageCache::upload_landed`] uploads them at
//! the next paint's start. Workers decode; only this canvas-current thread uploads (ADR-0039).
//! Pool decodes use [`thumbnails`]: read a current thumbnail instead of the file, and leave one
//! after a full decode for later opens and other cache users.
//!
//! Eviction has two bounds (ADR-0123): [`CACHE_CAPACITY`] entries, least recently asked for first and
//! never one asked for since the current paint began (the map may exceed it while more are visible), and
//! [`ImageCache::set_texture_budget`] bytes of textures not shown by a mapped surface, least recently asked-for
//! first. `wayland::App` triggers it after each paint using pins from surfaces' last lists.
//! [`ImageCache::release_evicted`] frees textures at the next paint's start, never mid-frame.

mod budget;
pub mod capture;
mod decode;
mod file;
mod fit;
pub mod icons;
pub mod quantize;
mod svg;
mod texture;
pub mod thumbnails;

pub use file::FileVersion;
pub(crate) use file::first_failure;
use fit::cache_box;
pub use fit::fitted_rect;

use budget::{Budget, Charge};
use decode::{decode, is_animated, is_vector};
use svg::packed_rgb;
use texture::{Animation, upload_or_log};

use crate::layout::node::Rgba;
use crate::text::shaping::{FontDatabase, ShapingHandle};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};
use shared::{debug, error};

/// Maximum map entries, including `Failed` and `Pending`. The texture budget bounds bytes; this
/// keeps a config cycling through a thousand failing paths from growing the map without bound.
const CACHE_CAPACITY: usize = 128;

/// Starting texture budget, replaced by [`ImageCache::set_texture_budget`] as soon as the outputs
/// are known (ADR-0182). Before any budget, moved-on 1920x1200 wallpapers (12 MB each) survived the
/// next 128 inserts and closed picker tiles did too.
///
/// [`ImageCache::trim`] charges only idle bytes against it, so it is an allowance on top of what
/// mapped surfaces show and not a total.
///
/// `wayland::output::texture_budget` owns the real figure, because what fits is a property of the
/// displays and not of this file.
const STARTING_TEXTURE_BUDGET: usize = 16 << 20;

/// Decode workers: machine parallelism capped so forty wallpapers arriving together do not take
/// every compositor core.
const MAX_DECODE_WORKERS: usize = 4;

/// Background decodes in flight at once, counted until their pixels are *consumed*.
///
/// Bounding the job channel alone does not bound the pipeline: a worker frees its queue slot the
/// moment it dequeues, so more jobs enqueue while finished results pile up in the result channel
/// waiting for [`ImageCache::poll`]. Queued, decoding and decoded-but-unconsumed all have to be
/// one number, which is what the pool's `wanted` set already counts -- a key joins it at enqueue
/// and leaves when `poll` takes its result or the entry is evicted.
///
/// Past this the request is refused without recording a slot, so the next paint asks again: one
/// retry per frame is its own backoff, and unlike `Slot::Failed` it does not remember a busy
/// moment as a permanently broken file.
const MAX_INFLIGHT_DECODES: usize = 64;

/// Longest edge a raster source may declare before it is refused unread.
///
/// `image::open` decodes the whole file before anything downscales it, so a thumbnail-sized
/// request still paid for the full surface: one 8000x6000 photo is roughly 192 MB of RGBA. The
/// limit is checked from the header, before the pixels are read. 8192 is twice a 4K display's
/// width, which is past anything this shell has to show.
const MAX_DECODE_EDGE: u32 = 8_192;

/// Decoded pixels the background pool holds at once, and the most one decode may produce.
///
/// This replaced a 64 MiB *per-decode* cap whose own doc said the real ceiling was itself times
/// [`MAX_DECODE_WORKERS`]. A per-decoder proxy for a pool figure gets the pool right and each
/// decode wrong: a 6024x3401 wallpaper decodes to 78 MiB, well inside a 256 MiB pool and refused
/// unread by a 64 MiB slice of it, so a legitimate file was permanently unloadable while three
/// quarters of the budget sat idle. The ceiling is unchanged; where it is enforced is not
/// (ADR-0187).
const DECODE_POOL_BYTES: u64 = 256 * 1024 * 1024;

/// Screenfuls of host bytes one animated source's kept deltas may hold (ADR-0235). Display-derived
/// for ADR-0182's reason, and larger than the GPU pool because these are host bytes: three kept 52
/// of a measured 1080p wallpaper's 86 frames.
///
/// ponytail: past this, trailing frames are dropped and the wrap shows as a jump. Upgrade: merge
/// deltas so what fits spreads over the loop instead of being cut from its end.
const ANIMATION_BUDGETS: usize = 8;

/// One cache slot. `box_px` is the physical-pixel target: SVGs use their longest edge; rasters
/// downscale to cover it (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    box_px: (u32, u32),
    version: FileVersion,
    /// Rasterization `currentColor`, packed `0x00RRGGBB` (ADR-0072). `None` for rasters and
    /// untinted SVGs. It keys bar-white and popup-dim textures separately; otherwise first tint
    /// wins for the process.
    tint: Option<u32>,
    /// Stored cropped to `box_px`, which only [`Fit::Cover`] rasters are. In the key because a
    /// `Contain` draw of the same file and box needs the uncropped pixels.
    cropped: bool,
    /// `image.source_blur` in physical pixels (ADR-0240): a sharp and a blurred draw of the same
    /// file and box are different slots. Zero for every kind but a static `image`; `decode_gif`
    /// never reads it, so an animated source is keyed as if it were always zero.
    blur_px: u32,
    font_generation: u64,
}

/// A display list's next image lookup. `trim` resolves it against current file and font versions
/// and pins either that exact texture or the temporary texture the next draw would use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ImageRequest<'a> {
    pub path: &'a Path,
    pub box_px: (u32, u32),
    pub tint: Option<Rgba>,
    pub fit: Fit,
    pub blur_px: u32,
}

crate::layout::node::prop::keywords! {
    /// How an image fills its layout box (ADR-0055 decision 3).
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub enum Fit {
        /// Covers the box and crops overflow. Default because it alone cannot leave wallpaper bars.
        #[default]
        Cover,
        /// Fits inside the box, leaving the remainder unpainted.
        Contain,
        /// Ignores aspect ratio.
        Stretch,
    }
}

/// Whether a draw waits for pixels (ADR-0122). `Inline` is the default for icons and wallpaper:
/// decode in the first frame so its presentation is complete (ADR-0003). `Background` queues the
/// decode and draws nothing until it lands, avoiding a second of frozen shell for forty tiles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Load {
    Inline,
    Background,
}

/// Decoder output awaiting texture upload: a still's one frame, or a GIF's base frame plus its
/// later frames' changed rects (ADR-0235). `deltas` and `delays` are empty and length-1
/// respectively for anything that is not an animation.
struct Decoded {
    /// Frame 0, full `width`x`height`.
    base: Vec<u8>,
    /// One delay per shown frame: `delays[0]` is the base's, `delays[i]` pairs with `deltas[i - 1]`.
    delays: Vec<Duration>,
    /// Frame `i + 1`'s rect, already composited with its disposal applied.
    deltas: Vec<GifDelta>,
    width: u32,
    height: u32,
    /// tiny-skia `Pixmap` is premultiplied RGBA8; `image` is straight. The wrong femtovg flag gives
    /// every anti-aliased icon edge a dark halo, not an outright failure.
    premultiplied: bool,
    /// An SVG that asked for a font, which a font change must redraw.
    text: bool,
}

/// One GIF frame's changed rectangle after disposal (ADR-0235): physical-pixel left/top/width/
/// height and its RGBA8 pixels, ready for `Canvas::update_image`.
struct GifDelta {
    rect: (u32, u32, u32, u32),
    pixels: Vec<u8>,
}

/// Slot state. `Pending` draws and enqueues nothing after its first job until it lands.
enum Slot {
    Pending,
    /// One texture, the instant it went up, and the bytes charged to the budget: the texture's,
    /// plus the animation's host deltas, which nothing else would reclaim. `anim` is `None` for a
    /// still: nothing to replay onto it.
    Ready {
        image: ImageId,
        bytes: usize,
        start: Instant,
        anim: Option<Animation>,
    },
    Failed,
}

/// A slot and its last [`ImageCache::tick`] hit, used by [`ImageCache::trim`].
struct Entry {
    slot: Slot,
    last_hit: u64,
    /// See [`Decoded::text`]; beside the slot so tests can set it without an `ImageId`.
    text: bool,
}

/// A queued decode's key, so a late result lands in the right slot, plus its tint and the
/// animation ceiling read when it was queued.
struct Job {
    fonts: FontDatabase,
    key: CacheKey,
    tint: Option<Rgba>,
    animation_bytes: usize,
}

/// Shared decode queue and result channel, at most `MAX_DECODE_WORKERS` threads. If resource
/// limits refuse every worker, background requests fall back to the inline path.
struct Pool {
    jobs: SyncSender<Job>,
    workers: usize,
    results: Receiver<(CacheKey, Result<Decoded, String>)>,
    /// Keys whose decode is still wanted. A worker checks this before spending anything on a job,
    /// so closing the picker stops the queued tiles rather than decoding all of them into slots
    /// that were evicted while they waited.
    wanted: Arc<Mutex<HashSet<CacheKey>>>,
    /// Decoded bytes in flight (ADR-0187). Shared with `ImageCache` so an inline decode on the
    /// dispatch thread is counted against the same ceiling the workers wait on.
    budget: Arc<Budget>,
}

/// [`ImageCache::census`]'s reading, for `wayland::memory_profile::Census`, which documents the
/// live/total split `ready`/`pending` and `failed`/`evicted`/`landed` carry over unchanged.
#[derive(Clone, Copy, Debug)]
pub struct ImageCacheCensus {
    pub resident_bytes: usize,
    pub ready: usize,
    pub pending: usize,
    pub failed: usize,
    pub evicted: usize,
    pub landed: usize,
}

/// Path/size to uploaded texture for one generation (`CONTEXT.md`, **Image cache**). Not shared or
/// persisted: a replaced Renderer starts cold (ADR-0054).
pub struct ImageCache {
    shaping: Option<ShapingHandle>,
    /// What vector keys carry. `fonts` may lag it until a vector decode needs the snapshot,
    /// whose round trip queues behind shaping work.
    font_generation: u64,
    fonts: FontDatabase,
    entries: HashMap<CacheKey, Entry>,
    /// Evicted since [`ImageCache::release_evicted`], not yet freed.
    evicted: Vec<ImageId>,
    /// Textures released, textures uploaded and slots that failed to decode, over the cache's
    /// life, for `wayland::memory_profile`. Totals rather than live reads; `Census` says why.
    evicted_total: usize,
    landed_total: usize,
    failed_total: usize,
    /// Bytes across `Ready` slots -- texture, plus an animation's host deltas (ADR-0235) --
    /// maintained by [`ImageCache::insert`] and [`ImageCache::evict`].
    resident_bytes: usize,
    /// Lamport clock incremented per [`ImageCache::image`], avoiding `Instant` and frame state.
    tick: u64,
    /// `tick` when the current paint began: entries with a later `last_hit` were asked for by it.
    paint_start: u64,
    pool: Pool,
    /// Decodes taken by [`ImageCache::poll`] but not uploaded: `poll` runs in the main-loop turn,
    /// where no canvas is current.
    landed: Vec<(CacheKey, Result<Decoded, String>)>,
    /// What [`ImageCache::trim`] holds idle textures to, set from the displays by
    /// `wayland::output::texture_budget` (ADR-0182) and [`STARTING_TEXTURE_BUDGET`] until they are
    /// known.
    texture_budget: usize,
    /// When this paint owes another: now, for a request turned away for [`MAX_INFLIGHT_DECODES`]
    /// with no slot recorded, or an animated source's next frame. Read and cleared by
    /// [`ImageCache::take_deferred`] straight after the `execute` that set it, which is what makes
    /// one field enough for every surface: paints are serialized on the dispatch thread.
    deferred: Option<Instant>,
    /// Whether the pool's closed result channel has been reported. `poll` runs every turn and the
    /// channel never reopens, so without this one dead pool writes a line per turn into a log that
    /// does not rotate (ADR-0199).
    workers_gone: bool,
    /// Paths whose queued decode was cancelled by an eviction, drained by [`ImageCache::poll`].
    ///
    /// A `Pending` entry evicted for capacity or budget takes its job out of the pool's `wanted`
    /// set, so the worker skips it and no result is ever sent. Without this the surface showing
    /// that file waits on a decode that will never land: the same stall a refused request causes,
    /// reached from the other side. `poll` already means "these files changed, invalidate the
    /// lists that draw them", which is exactly the cue a cancelled decode owes.
    cancelled: Vec<PathBuf>,
}

impl ImageCache {
    /// Test cache: its pool wakes nobody; tests poll for landings.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::build(None, MAX_DECODE_WORKERS)
    }

    /// Test cache that decodes every request inline, so one paint shows every image.
    #[cfg(test)]
    pub fn inline() -> Self {
        Self::build(None, 0)
    }

    /// What one animated source may hold, scaled by the displays (ADR-0182, ADR-0233).
    fn animation_bytes(&self) -> usize {
        self.texture_budget * ANIMATION_BUDGETS
    }

    /// Holds idle textures to `budget` bytes from here on (ADR-0182). Called whenever the outputs
    /// change, which is the only thing that changes the answer.
    pub fn set_texture_budget(&mut self, budget: usize) {
        self.texture_budget = budget;
    }

    /// Renderer cache: a landing wakes the Wayland poll (ADR-0124).
    pub fn with_waker(waker: crate::wake::Waker) -> Self {
        Self::build(Some(waker), MAX_DECODE_WORKERS)
    }

    fn build(waker: Option<crate::wake::Waker>, max_workers: usize) -> Self {
        ImageCache {
            shaping: None,
            font_generation: 0,
            fonts: FontDatabase::default(),
            entries: HashMap::new(),
            evicted: Vec::new(),
            evicted_total: 0,
            paint_start: 0,
            landed_total: 0,
            failed_total: 0,
            resident_bytes: 0,
            deferred: None,
            workers_gone: false,
            cancelled: Vec::new(),
            tick: 0,
            texture_budget: STARTING_TEXTURE_BUDGET,
            pool: Pool::spawn(waker, max_workers),
            landed: Vec::new(),
        }
    }

    /// Shares the text worker's font selection with SVG decoding.
    pub fn with_fonts(mut self, shaping: ShapingHandle) -> Self {
        self.fonts = shaping.font_database();
        self.font_generation = self.fonts.generation;
        self.shaping = Some(shaping);
        self
    }

    /// Evicts vectors drawn with text, and queued ones whose text is unknown, on any generation
    /// bump: a fallback face loaded for one glyph can fill that glyph in SVG text too. The rest
    /// are re-keyed, so an icon is not rasterized again for a font it never used.
    fn sync_fonts(&mut self) {
        let Some(shaping) = &self.shaping else { return };
        let generation = shaping.font_generation();
        if generation == self.font_generation {
            return;
        }
        self.font_generation = generation;
        let vectors: Vec<_> = self.entries.keys().filter(|key| is_vector(&key.path)).cloned().collect();
        for key in vectors {
            let entry = &self.entries[&key];
            if entry.text || matches!(entry.slot, Slot::Pending) {
                // poll's existing file invalidation also reaches retained image and mask layers.
                if !matches!(entry.slot, Slot::Pending) {
                    self.cancelled.push(key.path.clone());
                }
                self.evict(&key);
            } else if let Some(entry) = self.entries.remove(&key) {
                self.entries.insert(CacheKey { font_generation: generation, ..key }, entry);
            }
        }
    }

    /// Takes the snapshot a vector decode reads, only when the cached one is older than the keys.
    fn refresh_fonts(&mut self) {
        if let Some(shaping) = &self.shaping
            && self.fonts.generation < self.font_generation
        {
            self.fonts = shaping.font_database();
        }
    }

    /// Counts slots rather than reading `resident_bytes` alone: bytes flat against a rising
    /// `pending` is a decode queue backing up, which the bytes cannot show.
    pub fn census(&self) -> ImageCacheCensus {
        let mut ready = 0;
        let mut pending = 0;
        for entry in self.entries.values() {
            match entry.slot {
                Slot::Ready { .. } => ready += 1,
                Slot::Pending => pending += 1,
                Slot::Failed => {}
            }
        }
        ImageCacheCensus {
            resident_bytes: self.resident_bytes,
            ready,
            pending,
            failed: self.failed_total,
            evicted: self.evicted_total,
            landed: self.landed_total,
        }
    }

    /// Frees last frame's evictions. `layout::paint::canvas::paint_tree` calls this before walking because
    /// femtovg resolves `ImageId` at `flush`, not `fill_path`; mid-walk deletion unbinds a texture
    /// a recorded command still names, drawing blank.
    pub fn release_evicted(&mut self, canvas: &mut Canvas<OpenGl>) {
        self.begin_paint();
        for id in self.evicted.drain(..) {
            canvas.delete_image(id);
        }
    }

    /// Marks what is asked for from here as this paint's, which the capacity bound never evicts.
    fn begin_paint(&mut self) {
        self.paint_start = self.tick;
    }

    /// Takes finished background decodes and returns landed files as the repaint cue. No canvas is
    /// current here, so pixels wait in `landed` for the following paint.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        self.sync_fonts();
        let mut files = Vec::new();
        loop {
            match self.pool.results.try_recv() {
                Ok(result) => {
                    // A decode that finished just as its entry was evicted has nowhere to go.
                    // `upload_landed` would skip it anyway; dropping it here also keeps its path
                    // out of the repaint cue, which would otherwise redraw for nothing.
                    self.unwant(&result.0);
                    if !matches!(self.entries.get(&result.0).map(|entry| &entry.slot), Some(Slot::Pending)) {
                        continue;
                    }
                    files.push(result.0.path.clone());
                    self.landed.push(result);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !std::mem::replace(&mut self.workers_gone, true) && self.pool.workers > 0 {
                        error!("every decode worker is gone; background images will not load");
                    }
                    break;
                }
            }
        }
        // Decodes cancelled by an eviction send no result, so they reach the invalidation the same
        // way a landing does: as files whose lists are now wrong (ADR-0185).
        files.append(&mut self.cancelled);
        files
    }

    /// When this paint owes another, clearing the field. The painting surface calls it straight
    /// after its own `execute` and carries the answer into its `stale`, which is what gets that
    /// surface painted again (ADR-0185, ADR-0233).
    pub fn take_deferred(&mut self) -> Option<Instant> {
        self.deferred.take()
    }

    /// Uploads [`ImageCache::poll`] results at paint start, alongside
    /// [`ImageCache::release_evicted`]. Results for evicted slots are dropped; the next draw queues
    /// the file again.
    pub fn upload_landed(&mut self, canvas: &mut Canvas<OpenGl>) {
        for (key, result) in std::mem::take(&mut self.landed) {
            if !matches!(self.entries.get(&key).map(|entry| &entry.slot), Some(Slot::Pending)) {
                continue;
            }
            let text = result.as_ref().is_ok_and(|decoded| decoded.text);
            let slot = upload_or_log(canvas, &key.path, result);
            match slot {
                Slot::Ready { bytes, .. } => {
                    self.resident_bytes += bytes;
                    self.landed_total += 1;
                }
                Slot::Failed => self.failed_total += 1,
                Slot::Pending => {}
            }
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.slot = slot;
                entry.text = text;
            }
        }
    }

    /// Evicts idle textures to [`ImageCache::set_texture_budget`], oldest ask first (ADR-0123). The
    /// early return over-estimates on purpose -- idle bytes never exceed resident -- so an untouched
    /// cache skips the walk and [`victims`] measures the idle half. `pinned` contains the image
    /// requests from mapped surfaces' last display lists, collected by `wayland::App`
    /// after paint. Compute it lazily because list walks matter only when evicting. Icons are not
    /// pinned: lists carry theme names, not paths; an icon costs a few KB and one inline reraster.
    pub fn trim<'a>(&mut self, pinned: impl FnOnce() -> Vec<ImageRequest<'a>>) {
        if self.resident_bytes <= self.texture_budget {
            return;
        }
        let pinned = pinned()
            .into_iter()
            .filter_map(|request| {
                let wanted = self.key_for(&request);
                if self
                    .entries
                    .get(&wanted)
                    .is_some_and(|entry| matches!(entry.slot, Slot::Ready { .. } | Slot::Failed))
                {
                    Some(wanted)
                } else {
                    self.stand_in_key(&wanted).or(Some(wanted))
                }
            })
            .collect::<Vec<_>>();
        let candidates = self.entries.iter().filter_map(|(key, entry)| match entry.slot {
            Slot::Ready { bytes, .. } => Some((key.clone(), bytes, entry.last_hit)),
            Slot::Pending | Slot::Failed => None,
        });
        for key in victims(candidates, self.texture_budget, &pinned) {
            self.evict(&key);
        }
    }

    /// Uploaded texture for `path`, or `None` for a once-logged failure or pending background load.
    /// `box_px` is physical pixels: SVGs use their longest edge; rasters cover without upscaling.
    /// The canvas must be current on the sole painting thread (ADR-0039). Static system assets
    /// bypass stat; dynamic paths check [`FileVersion`].
    ///
    /// `Load::Inline` reads and rasterizes inside the frame, also the Wayland dispatch/config-VM
    /// thread (ADR-0039). That keeps a wallpaper's first frame whole; tile grids use
    /// `Load::Background` (ADR-0122).
    pub fn image(&mut self, canvas: &mut Canvas<OpenGl>, request: &ImageRequest, load: Load) -> Option<ImageId> {
        self.sync_fonts();
        let (vector, tint) = (is_vector(request.path), request.tint);
        let key = self.key_for(request);
        self.tick += 1;
        if let Some(cached) = self.entries.get_mut(&key) {
            cached.last_hit = self.tick;
            return self.showing(canvas, &key);
        }
        if vector {
            self.refresh_fonts();
        }
        let load = if self.pool.workers == 0 { Load::Inline } else { load };
        // A new pending slot can hit the map's capacity bound before the caller asks for its
        // stand-in. Keep that ready texture through the insertion which may evict cold entries.
        if load == Load::Background
            && let Some(stand_in) = self.stand_in_key(&key)
        {
            self.tick += 1;
            if let Some(entry) = self.entries.get_mut(&stand_in) {
                entry.last_hit = self.tick;
            }
        }
        match load {
            Load::Inline => {
                // Counted against the same ceiling the workers wait on, but never waiting for it:
                // this is the dispatch thread (ADR-0187). Nothing is queued, so nothing can be
                // evicted mid-decode and the request is still wanted by definition.
                let decoded = decode(
                    &key,
                    tint,
                    None,
                    Charge::Immediate(&self.pool.budget),
                    &|| true,
                    self.animation_bytes(),
                    &self.fonts,
                );
                let text = decoded.as_ref().is_ok_and(|decoded| decoded.text);
                let slot = upload_or_log(canvas, &key.path, decoded);
                self.insert(key.clone(), slot).text = text;
                self.showing(canvas, &key)
            }
            Load::Background => {
                // One gate for the whole pipeline, not just the queue: see `MAX_INFLIGHT_DECODES`.
                // Marked before the send, because a worker that takes the job immediately must
                // find it in the set.
                if !self.admit(&key) {
                    return None;
                }
                match self.pool.jobs.try_send(Job {
                    fonts: self.fonts.clone(),
                    key: key.clone(),
                    tint,
                    animation_bytes: self.animation_bytes(),
                }) {
                    Ok(()) => {
                        self.insert(key, Slot::Pending);
                    }
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        // The set has room but the channel does not, which the workers will clear.
                        // No slot again, so this owes the same repaint the ceiling above does.
                        self.unwant(&key);
                        self.deferred = Some(Instant::now());
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        debug!("{}: no decode worker left to take it", key.path.display());
                        self.unwant(&key);
                        self.insert(key, Slot::Failed);
                    }
                }
                None
            }
        }
    }

    fn key_for(&self, &ImageRequest { path, box_px, tint, fit, blur_px }: &ImageRequest) -> CacheKey {
        let vector = is_vector(path);
        CacheKey {
            font_generation: if vector { self.font_generation } else { 0 },
            path: path.to_path_buf(),
            box_px: cache_box(path, box_px),
            version: FileVersion::read(path),
            // Only vectors carry `currentColor`; drop PNG tint instead of splitting unused slots.
            tint: if vector { tint.map(packed_rgb) } else { None },
            // An SVG rasterizes straight to its box, so there is never overflow to crop.
            cropped: !vector && fit == Fit::Cover,
            // `decode` never reads `blur_px` for an animated source (ADR-0240); zeroed here too,
            // or a blurred and a sharp draw of the same GIF would be two slots each paying its
            // full animation budget for byte-identical frames.
            blur_px: if is_animated(path) { 0 } else { blur_px },
        }
    }

    fn stand_in_key(&self, wanted: &CacheKey) -> Option<CacheKey> {
        if wanted.blur_px != 0 {
            return None;
        }
        self.entries
            .iter()
            .filter_map(|(key, entry)| {
                (matches!(entry.slot, Slot::Ready { .. }) && compatible_stand_in(wanted, key)).then_some(key)
            })
            .max_by_key(|key| (u64::from(key.box_px.0) * u64::from(key.box_px.1), key.box_px))
            .cloned()
    }

    /// A ready texture at another physical box while a background decode catches up. Its fitted
    /// rect still uses the new logical box; the caller must not treat it as an exact source hit.
    pub(crate) fn stand_in(&mut self, canvas: &mut Canvas<OpenGl>, request: &ImageRequest) -> Option<ImageId> {
        let wanted = self.key_for(request);
        if self.entries.get(&wanted).is_some_and(|entry| matches!(entry.slot, Slot::Failed)) {
            return None;
        }
        let key = self.stand_in_key(&wanted)?;
        self.tick += 1;
        self.entries.get_mut(&key)?.last_hit = self.tick;
        self.showing(canvas, &key)
    }

    /// Reserves a pipeline slot for `key`, answering whether the caller may queue it.
    ///
    /// One gate for the whole pipeline, not just the queue: see [`MAX_INFLIGHT_DECODES`]. The
    /// reservation is made before the send, because a worker that takes the job immediately must
    /// find it in the set.
    ///
    /// Separate from [`ImageCache::image`] so the ceiling can be tested without a GL canvas, which
    /// no test has. The two refusals differ and must not be merged: capacity clears on its own and
    /// records that a repaint is owed, while a poisoned lock never clears and asking again every
    /// frame would spin forever (ADR-0185).
    fn admit(&mut self, key: &CacheKey) -> bool {
        match self.pool.wanted.lock() {
            Ok(mut wanted) if wanted.len() < MAX_INFLIGHT_DECODES => {
                wanted.insert(key.clone());
                true
            }
            Ok(_) => {
                self.deferred = Some(Instant::now());
                false
            }
            Err(_) => false,
        }
    }

    /// Drops `key` from the pool's wanted set, so a queued job for it is skipped rather than
    /// decoded. A poisoned lock is ignored: the worst case is one wasted decode.
    ///
    /// ponytail: eviction is the only thing that cancels today, so hiding a surface leaves its
    /// tiles decoding until the budget or the capacity evicts them. Bounded waste, not unbounded:
    /// [`MAX_INFLIGHT_DECODES`] caps how much can be in flight at all. Upgrade path: cancel
    /// against the pin set `wayland::App` already computes after paint, which names exactly the
    /// path/box pairs a mapped surface still shows.
    fn unwant(&self, key: &CacheKey) {
        if let Ok(mut wanted) = self.pool.wanted.lock() {
            wanted.remove(key);
        }
    }

    /// Evicts before inserting, keeping the map within [`CACHE_CAPACITY`]. Queue textures for
    /// [`ImageCache::release_evicted`]: femtovg frees nothing by `ImageId` until told to, so losing
    /// the id leaks the GPU allocation permanently.
    fn insert(&mut self, key: CacheKey, slot: Slot) -> &mut Entry {
        // Its own tick, so an insert that did not come through `image` still orders after every
        // earlier one and `last_hit` never ties. That makes the scan below fall back to insertion
        // order exactly where nothing has been asked for twice.
        self.tick += 1;
        while self.entries.len() >= CACHE_CAPACITY {
            // Least recently *asked for*, not oldest inserted (ADR-0183). This bound is the one
            // eviction path that never sees the pin list, and the oldest insert is typically the
            // wallpaper: put up first and asked for on every frame since, so a picker filling the
            // map evicted the one texture certain to be on screen. `image` bumps `last_hit` on
            // every hit, so whatever a mapped surface drew this frame is the newest thing here.
            //
            // ponytail: a linear scan of at most `CACHE_CAPACITY` entries, on the insert that hits
            // the bound and not on the others. A heap would order it in log time and would have to
            // be reordered on every hit, which is the common case; this is the rarer one.
            //
            // Never one this paint asked for: with more images visible than the cap, evicting them
            // re-decodes every one on each repaint. The map exceeds the cap until they go unused;
            // `trim` still bounds the bytes.
            let Some(coldest) = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.last_hit <= self.paint_start)
                .min_by_key(|(_, entry)| entry.last_hit)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.evict(&coldest);
        }
        match &slot {
            Slot::Ready { bytes, .. } => self.resident_bytes += *bytes,
            Slot::Failed => self.failed_total += 1,
            Slot::Pending => {}
        }
        self.entries.entry(key).insert_entry(Entry { slot, last_hit: self.tick, text: false }).into_mut()
    }

    /// Drops one entry, queues its texture for [`ImageCache::release_evicted`], and subtracts its
    /// bytes.
    fn evict(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            match &entry.slot {
                Slot::Ready { image, bytes, .. } => {
                    self.evicted.push(*image);
                    self.evicted_total += 1;
                    self.resident_bytes -= *bytes;
                }
                // Its decode is still queued or running; stop it being spent on a slot that has
                // just gone away. Say so: a surface may be showing a `retain` cover while it waits
                // for exactly this file, and cancelling in silence leaves it waiting for a result
                // no worker will ever send (ADR-0185).
                Slot::Pending => {
                    self.unwant(key);
                    self.cancelled.push(key.path.clone());
                }
                Slot::Failed => {}
            }
        }
    }
}

/// Evictions from `(key, bytes, last_hit)` to bring the *idle* bytes under `budget`: oldest ask
/// first, skip `pinned` path/box pairs, stop at budget or when unpinned candidates end. Pure so the
/// policy is testable without an `ImageId`, which femtovg cannot make outside a canvas.
///
/// Idle, not total: those are the only bytes eviction returns, so charging a pin against the budget
/// would evict every idle entry on the way to a figure the pin alone already exceeds.
fn victims(
    candidates: impl Iterator<Item = (CacheKey, usize, u64)>,
    budget: usize,
    pinned: &[CacheKey],
) -> Vec<CacheKey> {
    let mut idle: Vec<(CacheKey, usize, u64)> = candidates.filter(|(key, _, _)| !pinned.contains(key)).collect();
    idle.sort_by_key(|(_, _, last_hit)| *last_hit);
    let mut resident: usize = idle.iter().map(|(_, bytes, _)| *bytes).sum();
    let mut out = Vec::new();
    for (key, bytes, _) in idle {
        if resident <= budget {
            break;
        }
        resident -= bytes;
        out.push(key);
    }
    out
}

fn compatible_stand_in(wanted: &CacheKey, candidate: &CacheKey) -> bool {
    if wanted.path != candidate.path
        || wanted.version != candidate.version
        || wanted.tint != candidate.tint
        || wanted.cropped != candidate.cropped
        || wanted.blur_px != candidate.blur_px
        || wanted.font_generation != candidate.font_generation
        || wanted.box_px == candidate.box_px
    {
        return false;
    }
    if !wanted.cropped {
        return true;
    }
    let ((a, b), (c, d)) = (wanted.box_px, candidate.box_px);
    let (a, b, c, d) = (u128::from(a), u128::from(b), u128::from(c), u128::from(d));
    2 * (a * d).abs_diff(b * c) <= a + b + c + d + 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::snap::LogicalRect;

    fn box_rect() -> LogicalRect {
        LogicalRect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 }
    }

    #[test]
    fn a_failing_name_warns_once() {
        assert!(first_failure("a-failing-name-warns-once"));
        assert!(!first_failure("a-failing-name-warns-once"), "the second frame drawing it must stay quiet");
    }

    #[test]
    fn stretch_takes_the_box_exactly() {
        assert_eq!(fitted_rect(box_rect(), 640.0, 480.0, Fit::Stretch), box_rect());
    }

    #[test]
    fn contain_fits_inside_and_centres() {
        let fitted = fitted_rect(box_rect(), 64.0, 64.0, Fit::Contain);
        assert_eq!((fitted.width, fitted.height), (50.0, 50.0));
        assert_eq!((fitted.x, fitted.y), (35.0, 20.0));
    }

    #[test]
    fn cover_overflows_the_box_rather_than_leaving_a_gap() {
        // `layout::paint`'s scissor crops the overflow, so this can exceed the box.
        let fitted = fitted_rect(box_rect(), 64.0, 64.0, Fit::Cover);
        assert_eq!((fitted.width, fitted.height), (100.0, 100.0));
        assert_eq!((fitted.x, fitted.y), (10.0, -5.0));
    }

    #[test]
    fn a_zero_sized_image_falls_back_to_the_box_instead_of_dividing_by_zero() {
        // Otherwise an infinite scale and NaN rect reach femtovg.
        assert_eq!(fitted_rect(box_rect(), 0.0, 64.0, Fit::Cover), box_rect());
        assert_eq!(fitted_rect(box_rect(), 64.0, 0.0, Fit::Contain), box_rect());
    }

    #[test]
    fn one_file_tinted_two_ways_is_two_cache_slots() {
        // Without tint in the key, the first colour wins for the process: bar and popup share one
        // texture.
        let a = CacheKey { tint: Some(0xffffff), ..key("/x.svg", 18, FileVersion::default()) };
        let b = CacheKey { tint: Some(0x808080), ..a.clone() };
        assert_ne!(a, b);
    }

    #[test]
    fn a_sharp_and_a_blurred_draw_of_the_same_file_and_box_are_different_slots() {
        let sharp = key("/x.png", 18, FileVersion::default());
        let blurred = CacheKey { blur_px: 6, ..sharp.clone() };
        assert_ne!(sharp, blurred);
    }

    #[test]
    fn a_temporary_texture_keeps_source_identity_and_crop_geometry() {
        let wanted = CacheKey { box_px: (300, 150), cropped: true, ..key("/x.png", 18, FileVersion::default()) };
        let half = CacheKey { box_px: (150, 75), ..wanted.clone() };
        assert!(compatible_stand_in(&wanted, &half));
        assert!(!compatible_stand_in(&wanted, &CacheKey { box_px: (150, 150), ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { tint: Some(0xff00ff), ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { blur_px: 2, ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { cropped: false, ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { path: "/other.png".into(), ..half.clone() }));
        assert!(!compatible_stand_in(
            &wanted,
            &CacheKey { version: FileVersion { len: 1, ..FileVersion::default() }, ..half.clone() }
        ));
        let uncropped = CacheKey { cropped: false, ..wanted };
        assert!(compatible_stand_in(&uncropped, &CacheKey { cropped: false, ..half }));
    }

    #[test]
    fn a_pending_size_can_draw_a_ready_size_until_its_decode_lands() {
        use crate::layout::paint::{init_headless_egl, text_painter};
        use crate::text::shaping::ShapingHandle;

        let Some(instance) = init_headless_egl(64, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("picture.png");
        std::fs::write(&path, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        if cache.pool.workers == 0 {
            return;
        }
        let small = ImageRequest { path: &path, box_px: (16, 16), tint: None, fit: Fit::Contain, blur_px: 0 };
        let large = ImageRequest { box_px: (32, 32), ..small };
        let first = cache.image(painter.canvas_mut(), &small, Load::Inline).unwrap();
        assert!(cache.image(painter.canvas_mut(), &large, Load::Background).is_none());
        assert_eq!(cache.stand_in(painter.canvas_mut(), &large), Some(first));
        assert!(cache.stand_in(painter.canvas_mut(), &ImageRequest { fit: Fit::Cover, ..large }).is_none());
    }

    /// A wallpaper's shape without a wallpaper's art: a 16:9 viewBox filled corner to corner by one
    /// gradient. Enough to tell a rasterizer that works from one that renders an empty pixmap.
    pub(super) const GRADIENT_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1920 1080">
      <defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0" stop-color="#001020"/><stop offset="1" stop-color="#a0d0ff"/>
      </linearGradient></defs>
      <rect width="1920" height="1080" fill="url(#g)"/>
    </svg>"##;

    /// A 2x2 RGBA PNG encoded by Pillow, byte for byte. An independent encoder distinguishes a
    /// working decoder from a round trip through a broken one.
    pub(super) const PIL_2X2_RGBA_PNG: [u8; 80] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00,
        0x17, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x05, 0xc1, 0x01, 0x01, 0x00, 0x00, 0x00, 0x82, 0x20, 0xa6, 0xf7,
        0xdc, 0x40, 0x24, 0x43, 0xc1, 0x01, 0x3a, 0xdc, 0x05, 0x7c, 0xf2, 0x4a, 0x44, 0x5b, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// ADR-0183. The capacity bound never sees the pin list, so it evicts by what has been asked
    /// for least recently rather than by what was inserted first -- otherwise the entry most
    /// certain to be on screen, the wallpaper put up before everything else, is the first to go
    /// when a picker fills the map.
    #[test]
    fn the_capacity_bound_evicts_the_coldest_entry_and_not_the_oldest_one() {
        // Negative entries only: `ImageId` has no public constructor.
        let mut cache = ImageCache::new();
        let first = key("/tmp/0.png", 0, FileVersion::default());
        for n in 0..CACHE_CAPACITY {
            cache.insert(key(format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }

        // Asked for again, the way a mapped surface asks for what it draws every frame.
        cache.begin_paint();
        cache.tick += 1;
        let hit = cache.tick;
        cache.entries.get_mut(&first).unwrap().last_hit = hit;

        // Ten more at the bound, so ten entries have to go.
        for n in CACHE_CAPACITY..(CACHE_CAPACITY + 10) {
            cache.insert(key(format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        let newest = key(format!("/tmp/{}.png", CACHE_CAPACITY + 9), 0, FileVersion::default());
        assert!(cache.entries.contains_key(&newest));
        assert!(cache.entries.contains_key(&first), "the oldest insert survives, because it is still being asked for");
        for n in 1..=10 {
            assert!(
                !cache.entries.contains_key(&key(format!("/tmp/{n}.png"), 0, FileVersion::default())),
                "the ten coldest go instead, /tmp/{n}.png among them"
            );
        }
    }

    /// A repaint that draws more images than the cap must not evict what it just drew, or every
    /// repaint re-decodes the overflow. Ready slots need a canvas, so `Failed` ones stand in.
    #[test]
    fn a_paint_never_evicts_what_it_drew_and_unused_entries_shrink_back_to_the_cap() {
        let visible = CACHE_CAPACITY + 40;
        let k = |n: usize| key(format!("/tmp/{n}.png"), 0, FileVersion::default());
        let mut cache = ImageCache::new();
        for frame in 0..4 {
            cache.begin_paint();
            for n in 0..visible {
                if cache.entries.contains_key(&k(n)) {
                    cache.tick += 1;
                    let hit = cache.tick;
                    cache.entries.get_mut(&k(n)).unwrap().last_hit = hit;
                } else {
                    cache.insert(k(n), Slot::Failed);
                }
            }
            assert_eq!(cache.entries.len(), visible, "frame {frame}: the visible set stays whole");
            // Failed slots free nothing, so re-decodes show as re-inserts.
            assert_eq!(cache.census().failed, visible, "frame {frame}: nothing was evicted and re-inserted");
        }
        // A different set replaces it: the old one is unused, so the map returns to the cap.
        cache.begin_paint();
        for n in visible..visible + CACHE_CAPACITY {
            cache.insert(k(n), Slot::Failed);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        assert!((visible..visible + CACHE_CAPACITY).all(|n| cache.entries.contains_key(&k(n))));
    }

    /// Eviction frees a `Failed` slot's memory; it does not un-fail the load.
    #[test]
    fn failed_census_survives_capacity_eviction_of_old_failed_entries() {
        let mut cache = ImageCache::new();
        for n in 0..CACHE_CAPACITY {
            cache.insert(key(format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }
        cache.begin_paint();
        for n in CACHE_CAPACITY..(CACHE_CAPACITY + 10) {
            cache.insert(key(format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY, "the ten coldest were evicted to stay at the bound");
        assert_eq!(cache.census().failed, CACHE_CAPACITY + 10, "every insert failed once, evicted or not");
    }

    pub(super) fn key(path: impl Into<PathBuf>, px: u32, version: FileVersion) -> CacheKey {
        CacheKey {
            path: path.into(),
            box_px: (px, px),
            version,
            tint: None,
            cropped: false,
            blur_px: 0,
            font_generation: 0,
        }
    }

    #[test]
    fn the_budget_evicts_the_least_recently_asked_for_idle_texture_and_never_a_pinned_one() {
        // Three 12 MB wallpapers plus tiles: shown is pinned, tiles were asked after the older
        // wallpaper, so the older wallpaper goes first without touching tiles.
        let mb = 1 << 20;
        let v = FileVersion::default();
        let old = key("/w/old.jpg", 1920, v);
        let older = key("/w/older.jpg", 1920, v);
        let shown = key("/w/shown.jpg", 1920, v);
        let tiles = key("/w/tiles.png", 232, v);
        let candidates = vec![
            (older.clone(), 12 * mb, 1),
            (old.clone(), 12 * mb, 2),
            (tiles.clone(), 7 * mb, 3),
            (shown.clone(), 12 * mb, 4),
        ];
        let pinned = vec![shown.clone()];
        // 31 MB idle against a 19 MB budget: the oldest goes and the rest stay.
        let out = victims(candidates.clone().into_iter(), 19 * mb, &pinned);
        assert_eq!(out, vec![older.clone()]);
        // A tighter budget takes the next oldest, then the tiles, and stops at pinned even while
        // over budget: oversized working sets do not thrash.
        let out = victims(candidates.clone().into_iter(), 5 * mb, &pinned);
        assert_eq!(out, vec![older, old, tiles]);
        // Idle already under budget, nothing moves -- and the pinned 12 MB is not charged for.
        assert!(victims(candidates.into_iter(), 31 * mb, &pinned).is_empty());
    }

    #[test]
    fn a_pin_the_size_of_the_whole_budget_still_leaves_room_for_idle_textures() {
        // A cover wallpaper is one screenful and so is the budget on a 3440x1440 output. If the pin
        // counted, every icon would go and the cache would still be over, for nothing back.
        let mb = 1 << 20;
        let v = FileVersion::default();
        let budget = 19 * mb;
        let mut candidates = vec![(key("/w/shown.jpg", 3440, v), budget, 99)];
        candidates.extend((0..11).map(|n| (key(format!("/i/{n}.png"), 32, v), 4 << 10, n)));
        let pinned = vec![candidates[0].0.clone()];

        assert!(
            victims(candidates.into_iter(), budget, &pinned).is_empty(),
            "44 KB of idle icons beside a pin that fills the budget is not a reason to evict any of them"
        );
    }

    #[test]
    fn a_pin_is_by_path_and_box_so_a_tile_of_the_shown_file_is_still_idle() {
        let v = FileVersion::default();
        let full = key("/w/a.jpg", 1920, v);
        let tile = key("/w/a.jpg", 232, v);
        let pinned = vec![full.clone()];
        let out = victims(vec![(full.clone(), 10, 1), (tile.clone(), 10, 2)].into_iter(), 5, &pinned);
        assert_eq!(out, vec![tile]);
    }

    #[test]
    fn trim_under_budget_never_asks_for_the_pins() {
        let mut cache = ImageCache::new();
        cache.trim(|| unreachable!("nothing resident, nothing to walk"));
        assert_eq!(cache.resident_bytes, 0);
    }

    #[test]
    fn a_vector_and_a_raster_alike_take_one_slot_per_box() {
        let v = FileVersion::default();
        assert_ne!(key("/tmp/a.png", 12, v), key("/tmp/a.png", 24, v));
        assert_ne!(key("/tmp/a.svg", 12, v), key("/tmp/a.svg", 24, v));
        assert_ne!(key("/tmp/a.svg", 12, v), key("/tmp/a.png", 12, v));
    }

    #[test]
    fn the_inflight_gate_counts_decodes_until_their_results_are_consumed() {
        // The hole a queue-depth bound leaves: a worker frees its slot the moment it dequeues, so
        // more jobs enqueue while finished results wait for `poll`. The `wanted` set is the count
        // that spans queued, decoding and decoded-but-unconsumed.
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let cache = ImageCache::new();
        let version = FileVersion::read(&png);
        let key = |n: u32| key(&png, n, version);
        {
            let mut wanted = cache.pool.wanted.lock().unwrap();
            for n in 0..MAX_INFLIGHT_DECODES as u32 {
                wanted.insert(key(n + 1));
            }
            assert_eq!(wanted.len(), MAX_INFLIGHT_DECODES, "the pipeline is now full");
        }
        // At the ceiling nothing new may join, however much room the channel has.
        assert!(
            cache.pool.wanted.lock().unwrap().len() >= MAX_INFLIGHT_DECODES,
            "a request arriving here must be refused rather than queued"
        );
        // Consuming one frees exactly one.
        cache.unwant(&key(1));
        assert_eq!(cache.pool.wanted.lock().unwrap().len(), MAX_INFLIGHT_DECODES - 1);
    }

    /// ADR-0185. The refusal records no slot, so asking again is the whole retry -- and only a
    /// paint asks. A surface whose display list has not changed is never painted again, so the
    /// refusal has to say a repaint is owed or the image never loads at all.
    #[test]
    fn a_request_refused_for_pipeline_capacity_says_a_repaint_is_owed() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        let version = FileVersion::read(&png);
        let key = |n: u32| key(&png, n, version);

        // Admitted while there is room, and nothing is owed: the caller got its slot.
        assert!(cache.admit(&key(1)), "the first request has the whole pipeline to itself");
        assert!(cache.take_deferred().is_none(), "an admitted request owes no repaint");

        for n in 1..MAX_INFLIGHT_DECODES as u32 {
            assert!(cache.admit(&key(n + 1)));
        }
        assert!(cache.take_deferred().is_none(), "filling the pipeline is not a refusal");

        // At the ceiling: refused, and the refusal is visible to the paint that has to retry it.
        assert!(!cache.admit(&key(9999)), "a request past the ceiling must be refused");
        assert!(cache.take_deferred().is_some(), "a refused request must say a repaint is owed");
        assert!(cache.take_deferred().is_none(), "and the flag is taken, not left set for the next surface");

        // Room again: admitted, and it owes nothing.
        cache.unwant(&key(1));
        assert!(cache.admit(&key(9999)), "a freed slot admits the next request");
        assert!(cache.take_deferred().is_none());
    }

    /// ADR-0185, the same stall reached from the other side: nobody refused this request, an
    /// eviction cancelled it after it was queued. The worker skips a job that has left `wanted`
    /// and sends no result, so without a cue the surface waits on a decode that will never land.
    #[test]
    fn a_decode_cancelled_by_an_eviction_still_invalidates_the_lists_that_drew_it() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        let key = key(&png, 8, FileVersion::read(&png));
        cache.insert(key.clone(), Slot::Pending);
        cache.pool.wanted.lock().unwrap().insert(key.clone());

        assert!(cache.poll().is_empty(), "nothing has landed and nothing has been cancelled");
        cache.evict(&key);
        assert_eq!(cache.poll(), vec![png.clone()], "a cancelled decode owes the same invalidation a landed one does");
        assert!(cache.poll().is_empty(), "and it is reported once, not on every turn after");
    }

    #[test]
    fn evicting_a_pending_entry_stops_its_queued_decode() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        let key = key(&png, 8, FileVersion::read(&png));
        cache.insert(key.clone(), Slot::Pending);
        cache.pool.wanted.lock().unwrap().insert(key.clone());
        cache.evict(&key);
        assert!(
            !cache.pool.wanted.lock().unwrap().contains(&key),
            "a worker must be able to see that this decode is no longer wanted"
        );
    }

    #[test]
    fn changing_fonts_cancels_svg_jobs_and_text_preserves_icons_and_rasters_and_rejects_late_pixels() {
        let shaping = svg::tests::fixture_shaping();
        let mut cache = ImageCache::inline().with_fonts(shaping.clone());
        let (results_tx, results_rx) = std::sync::mpsc::channel();
        cache.pool.results = results_rx;
        let at =
            |path: &str| CacheKey { font_generation: cache.font_generation, ..key(path, 24, FileVersion::default()) };
        let (vector, icon, label) = (at("fixture.svg"), at("icon.svg"), at("label.svg"));
        let raster = key("fixture.png", 24, FileVersion::default());
        cache.admit(&vector);
        cache.insert(vector.clone(), Slot::Pending);
        cache.insert(raster.clone(), Slot::Failed);
        // `Failed` stands in for `Ready`, which needs a canvas: `text` alone decides.
        cache.insert(icon.clone(), Slot::Failed);
        cache.insert(label.clone(), Slot::Failed).text = true;
        let snapshot = cache.fonts.generation;
        shaping.set_chain(&["Noto Sans Symbols 2".into()]);
        let mut cue = cache.poll();
        cue.sort();
        assert_eq!(cue, vec![vector.path.clone(), label.path.clone()]);
        assert_eq!(cache.fonts.generation, snapshot, "no decode ran, so no snapshot was fetched");
        assert!(!cache.entries.contains_key(&vector));
        assert!(!cache.entries.contains_key(&label), "SVG text redraws with the new fonts");
        assert!(cache.entries.contains_key(&raster));
        let icon_now = CacheKey { font_generation: cache.font_generation, ..icon.clone() };
        assert!(cache.entries.contains_key(&icon_now), "a text-free SVG keeps its pixels under the new key");
        assert!(!cache.pool.wanted.lock().unwrap().contains(&vector));
        let fresh = CacheKey { font_generation: cache.font_generation, ..vector.clone() };
        assert_ne!(fresh, vector, "old results cannot match a replacement pending slot");
        cache.insert(fresh.clone(), Slot::Pending);
        results_tx.send((vector, Err("old generation".into()))).unwrap();
        assert!(cache.poll().is_empty(), "a late old result is not a current repaint cue");
        assert!(cache.landed.is_empty(), "old pixels never reach the upload queue");
        assert!(matches!(cache.entries[&fresh].slot, Slot::Pending));
        cache.refresh_fonts();
        assert_eq!(cache.fonts.generation, cache.font_generation, "the next vector decode reads current fonts");
    }

    #[test]
    fn the_pool_rasterizes_svg_text_with_its_queued_font_snapshot() {
        let shaping = svg::tests::fixture_shaping();
        let mut cache = ImageCache::new().with_fonts(shaping);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.svg");
        std::fs::write(&path, r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 120 40"><text x="2" y="30" font-size="28">Mantle</text></svg>"#).unwrap();
        let key = CacheKey { font_generation: cache.font_generation, ..key(&path, 120, FileVersion::read(&path)) };
        assert!(cache.admit(&key));
        cache.insert(key.clone(), Slot::Pending);
        cache
            .pool
            .jobs
            .send(Job { fonts: cache.fonts.clone(), key, tint: None, animation_bytes: STARTING_TEXTURE_BUDGET })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.poll().is_empty() {
            assert!(Instant::now() < deadline, "the SVG decode never landed");
            std::thread::yield_now();
        }
        let decoded = cache.landed[0].1.as_ref().unwrap();
        assert!(decoded.premultiplied);
        assert!(decoded.base.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count() > 200);
    }

    #[test]
    fn the_pool_decodes_a_job_off_thread_and_poll_reports_it_landed() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        let key = key(&png, 8, FileVersion::read(&png));
        cache.insert(key.clone(), Slot::Pending);
        // A worker skips a job nobody wants, so this stands in for what `image` records when it
        // queues one.
        cache.pool.wanted.lock().unwrap().insert(key.clone());
        cache
            .pool
            .jobs
            .send(Job {
                fonts: FontDatabase::default(),
                key: key.clone(),
                tint: None,
                animation_bytes: STARTING_TEXTURE_BUDGET,
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut files = cache.poll();
        while files.is_empty() {
            assert!(std::time::Instant::now() < deadline, "the decode never landed");
            std::thread::sleep(std::time::Duration::from_millis(5));
            files = cache.poll();
        }
        assert_eq!(files, vec![png.clone()]);
        let (landed_key, result) = &cache.landed[0];
        assert_eq!(*landed_key, key);
        let decoded = result.as_ref().expect("a 2x2 PNG decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert!(!decoded.premultiplied);
        assert!(
            matches!(cache.entries.get(&key).map(|e| &e.slot), Some(Slot::Pending)),
            "no canvas, so nothing uploaded yet"
        );
    }

    #[test]
    fn one_path_rewritten_in_place_is_a_different_slot() {
        let first = FileVersion { mtime_secs: 1_700_000_000, mtime_nanos: 0, len: 512 };
        let same_time_new_size = FileVersion { len: 640, ..first };
        let same_size_new_time = FileVersion { mtime_nanos: 1, ..first };
        assert_ne!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, same_time_new_size));
        assert_ne!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, same_size_new_time));
        assert_eq!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, first));
    }

    #[test]
    fn a_missing_file_and_a_real_one_read_different_versions() {
        assert_eq!(FileVersion::read(Path::new("/nonexistent/mantle-x.png")), FileVersion::default());
        // Any file with bytes in it; the rule is about read versus missing, not about the contents.
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.svg");
        std::fs::write(&present, GRADIENT_SVG).unwrap();
        let version = FileVersion::read(&present);
        assert_ne!(version, FileVersion::default());
        assert!(version.len > 0);
    }
}

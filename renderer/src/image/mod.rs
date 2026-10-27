//! Decodes, caches, and fits images into GPU textures (ADR-0054). PNG/JPEG/WebP use `image`
//! (`decode::decode_raster`); SVG uses `resvg` because Adwaita ships scalable icons; an animated GIF, WebP or APNG
//! is one texture plus its frames' changed rects, replayed onto it as the slot's elapsed time advances
//! (`animation::decode_gif` and `animation::decode_frames` for WebP and APNG, ADR-0233, ADR-0235).
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

mod animation;
mod budget;
pub mod capture;
mod decode;
mod evict;
mod file;
mod fit;
mod icon_index;
pub mod icons;
pub mod quantize;
mod svg;
mod texture;
pub mod thumbnails;
mod watch;

pub use file::FileVersion;
use fit::cache_box;
pub use fit::fitted_rect;
pub(crate) use shared::log::first_failure;

use budget::{Budget, Charge};
use decode::{Refused, decode};
pub(crate) use decode::{is_gif, is_vector};
use svg::packed_rgb;
#[cfg(test)]
pub(crate) use texture::tests::{evict_all_for_test, fail_for_test, play_for_test};
use texture::{Animation, Clock, NodeClock, upload_or_log};

use crate::layout::node::Rgba;
use crate::layout::scene::NodeId;
use crate::text::shaping::{FontDatabase, ShapingHandle};
use rustc_hash::FxHashMap as HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
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
/// Bounding the job queue alone does not bound the pipeline: a worker frees its queue slot the
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

/// 16 MiB a sampler image, 128 MiB for a node's eight: they are decoded inline and pinned past the texture budget.
/// A shader's `images` entry is requested as a mask over a box of this size: whole and uncropped (a
/// larger file is downscaled), since a sampler reads texels and not a fitted picture.
pub(crate) const MAX_SAMPLER_EDGE: u32 = 2_048;

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
    /// file and box are different slots. Zero for every kind but a static `image`; an animation
    /// never reads it, and a GIF is keyed as if it were always zero.
    blur_px: u32,
    /// A WebP or PNG drawn by an `image` node, the one request that plays it. A mask, a shader
    /// sampler or an icon takes the first frame, from the still path, and no animation's deltas:
    /// a different slot even for the same file and box.
    plays: bool,
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
    /// Read the source's size from its header if a thumbnail stands in for it, for an image whose
    /// `status` reports it. Not part of the key: a slot decoded without it reads the header later,
    /// when [`ImageCache::loaded`] is asked with it.
    pub wants_size: bool,
    /// The `image` node drawing it and its `playing`, which time its animation; `None` uses the
    /// slot's own clock, always playing. Not part of the slot's key.
    pub clock: Option<(NodeId, bool)>,
}

impl ImageRequest<'_> {
    /// An image `mask`'s file, stretched over a box of `box_px`.
    pub(crate) fn mask(path: &Path, box_px: (u32, u32)) -> ImageRequest<'_> {
        ImageRequest { path, box_px, tint: None, fit: Fit::Stretch, blur_px: 0, wants_size: false, clock: None }
    }
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

/// A source's own pixel size, before any downscale for its box.
pub type Natural = (u32, u32);

/// Decoder output awaiting texture upload: a still's one frame, or an animation's base frame plus its
/// later frames' changed rects (ADR-0235). `deltas` and `delays` are empty and length-1
/// respectively for anything that is not an animation.
struct Decoded {
    /// Frame 0, full `width`x`height`.
    base: Vec<u8>,
    /// One delay per shown frame: `delays[0]` is the base's, `delays[i]` pairs with `deltas[i - 1]`.
    delays: Vec<Duration>,
    /// Frame `i + 1`'s rect, already composited with its disposal applied.
    deltas: Vec<Delta>,
    width: u32,
    height: u32,
    /// The source's own size, before any downscale for the box: what `image_status` reports.
    natural: Option<Natural>,
    /// tiny-skia `Pixmap` is premultiplied RGBA8; `image` is straight. The wrong femtovg flag gives
    /// every anti-aliased icon edge a dark halo, not an outright failure.
    premultiplied: bool,
    /// An SVG that asked for a font, which a font change must redraw.
    text: bool,
}

/// One animation frame's changed rectangle after disposal (ADR-0235): physical-pixel left/top/width/
/// height and its RGBA8 pixels, ready for `Canvas::update_image`.
struct Delta {
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
        /// `None` for an animation until a draw without a node asks: nodes draw their own.
        image: Option<ImageId>,
        bytes: usize,
        clock: Clock,
        anim: Option<Animation>,
        natural: Option<Natural>,
    },
    Failed,
}

/// What a draw's exact slot holds, for `image_status` (`layout::paint::DrawnImage`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Loaded {
    /// Decoded, with the source's own size when known.
    Ready(Option<Natural>),
    Failed,
}

/// A slot and its last [`ImageCache::tick`] hit, used by [`ImageCache::trim`].
struct Entry {
    slot: Slot,
    last_hit: u64,
    /// See [`Decoded::text`]; beside the slot so tests can set it without an `ImageId`.
    text: bool,
    /// A size-only job was queued for a ready slot that lacked its source's size.
    sized: bool,
}

/// A queued decode's key, so a late result lands in the right slot, plus its tint and the
/// animation ceiling read when it was queued.
struct Job {
    fonts: FontDatabase,
    key: CacheKey,
    tint: Option<Rgba>,
    animation_bytes: usize,
    wants_size: bool,
    /// Read only the source's size, for a slot already ready without it.
    size_only: bool,
}

/// Shared decode queue and result channel, at most `MAX_DECODE_WORKERS` threads. If resource
/// limits refuse every worker, background requests fall back to the inline path.
struct Pool {
    jobs: Arc<decode::Queue>,
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
    // ponytail: Fx hashing, and keys can carry text a window title chooses (icon names, SVG text),
    // so collisions can be forced; CACHE_CAPACITY bounds the damage. Upgrade: a keyed hasher.
    entries: HashMap<CacheKey, Entry>,
    /// Recent stats of the paths in `entries`, so a draw does not stat its file.
    versions: file::VersionCache,
    /// Animated slots per path, the sizes of one file being separate slots: a WebP or APNG is told
    /// from a still only by decoding it, and the scene must not read the file to ask.
    animated: HashMap<PathBuf, usize>,
    /// Evicted since [`ImageCache::release_evicted`], not yet freed.
    evicted: Vec<ImageId>,
    /// Textures released, textures uploaded and slots that failed to decode, over the cache's
    /// life, for `wayland::memory_profile`. Totals rather than live reads; `Census` says why.
    evicted_total: usize,
    /// A texture's slot went since [`ImageCache::take_freed`] last asked: its host bytes, an
    /// animation's deltas above all, are on glibc's free lists until something trims them.
    freed: bool,
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
    /// Each `image` node's playback of each animated file it draws (`texture::showing`).
    clocks: HashMap<NodeId, Vec<NodeClock>>,
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

    /// Renderer cache: a landing wakes the Wayland poll (ADR-0124).
    pub fn with_waker(waker: crate::wake::Waker) -> Self {
        Self::build(Some(waker), MAX_DECODE_WORKERS)
    }

    fn build(waker: Option<crate::wake::Waker>, max_workers: usize) -> Self {
        ImageCache {
            shaping: None,
            font_generation: 0,
            fonts: FontDatabase::default(),
            entries: HashMap::default(),
            versions: Default::default(),
            animated: HashMap::default(),
            evicted: Vec::new(),
            evicted_total: 0,
            freed: false,
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
            clocks: HashMap::default(),
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

    /// Takes finished background decodes and returns landed files as the repaint cue. No canvas is
    /// current here, so pixels wait in `landed` for the following paint.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        self.sync_fonts();
        let mut files = Vec::new();
        loop {
            match self.pool.results.try_recv() {
                Ok(result) => {
                    // A size-only answer (no pixels) arriving at a slot that went Pending again must
                    // not land as that slot's pixels nor cancel its real job.
                    let size_only = result.1.as_ref().is_ok_and(|decoded| decoded.base.is_empty());
                    let slot = self.entries.get(&result.0).map(|entry| &entry.slot);
                    if size_only && matches!(slot, Some(Slot::Pending)) {
                        continue;
                    }
                    // A decode that finished just as its entry was evicted has nowhere to go.
                    // `upload_landed` would skip it anyway; dropping it here also keeps its path
                    // out of the repaint cue, which would otherwise redraw for nothing.
                    self.unwant(&result.0);
                    let pending = match (self.entries.get_mut(&result.0).map(|entry| &mut entry.slot), &result.1) {
                        (Some(Slot::Pending), _) => true,
                        // A size-only job's answer: no pixels, nothing to upload.
                        (Some(Slot::Ready { natural: natural @ None, .. }), Ok(sized)) if sized.natural.is_some() => {
                            *natural = sized.natural;
                            false
                        }
                        _ => continue,
                    };
                    files.push(result.0.path.clone());
                    if pending {
                        self.landed.push(result);
                    }
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
        // Rewritten in place: a list naming the file cannot show it, the path being the same.
        files.append(&mut self.versions.changed(Instant::now()));
        files
    }

    /// When [`ImageCache::poll`] next re-stats a file inotify cannot watch, for the poll loop's
    /// timeout.
    pub fn next_recheck(&self) -> Option<Instant> {
        self.versions.due()
    }

    /// Readable when a watched image file changed; [`ImageCache::poll`] drains it.
    pub fn watch_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.versions.fd()
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
            self.count_animated(&key.path, &slot, true);
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
            return self.showing(canvas, &key, request.clock);
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
                self.showing(canvas, &key, request.clock)
            }
            Load::Background => {
                // Marked wanted before the send: see `admit`.
                if !self.admit(&key) {
                    return None;
                }
                match self.pool.try_send(Job {
                    fonts: self.fonts.clone(),
                    key: key.clone(),
                    tint,
                    wants_size: request.wants_size,
                    size_only: false,
                    animation_bytes: self.animation_bytes(),
                }) {
                    Ok(()) => {
                        self.insert(key, Slot::Pending);
                    }
                    Err(Refused::Full) => {
                        // The set has room but the queue does not, which the workers will clear.
                        // No slot again, so this owes the same repaint the ceiling above does.
                        self.unwant(&key);
                        self.deferred = Some(Instant::now());
                    }
                    Err(Refused::Disconnected) => {
                        debug!("{}: no decode worker left to take it", key.path.display());
                        self.unwant(&key);
                        self.insert(key, Slot::Failed);
                    }
                }
                None
            }
        }
    }

    /// What `request`'s own slot holds after a draw asked for it; `None` while it decodes. A stand-in
    /// or a `retain` cover drawn meanwhile says nothing about this source.
    ///
    /// A ready slot without the size a `status` wants asks a worker for it, once, from the header.
    pub(crate) fn loaded(&mut self, request: &ImageRequest) -> Option<Loaded> {
        let key = self.key_for(request);
        let entry = self.entries.get_mut(&key)?;
        let loaded = match entry.slot {
            Slot::Ready { natural, .. } => Loaded::Ready(natural),
            Slot::Failed => return Some(Loaded::Failed),
            Slot::Pending => return None,
        };
        if loaded == Loaded::Ready(None) && request.wants_size && !entry.sized {
            let job = Job {
                fonts: self.fonts.clone(),
                key: key.clone(),
                tint: None,
                wants_size: true,
                size_only: true,
                animation_bytes: 0,
            };
            // `sized` only once queued, so a refused send is asked again next draw.
            if self.admit(&key) {
                match self.pool.try_send(job) {
                    Ok(()) => {}
                    Err(Refused::Full) => {
                        self.unwant(&key);
                        self.deferred = Some(Instant::now());
                        return Some(loaded);
                    }
                    // No worker will ever answer; asking again every draw would only repeat this.
                    Err(Refused::Disconnected) => self.unwant(&key),
                }
                if let Some(entry) = self.entries.get_mut(&key) {
                    entry.sized = true;
                }
            }
        }
        Some(loaded)
    }

    fn key_for(&mut self, &ImageRequest { path, box_px, tint, fit, blur_px, clock, .. }: &ImageRequest) -> CacheKey {
        let vector = is_vector(path);
        // Bounded by the slots, not wholesale: a forgotten path with a texture on screen would miss
        // its next rewrite. Paths without a slot are the transient ones, refused for capacity.
        if self.versions.len() >= 2 * CACHE_CAPACITY {
            let slotted: HashSet<&Path> = self.entries.keys().map(|key| key.path.as_path()).collect();
            let unslotted =
                self.versions.paths().filter(|path| !slotted.contains(path.as_path())).cloned().collect::<Vec<_>>();
            unslotted.iter().for_each(|path| self.versions.forget(path));
        }
        let version = self.versions.read(path, Instant::now());
        CacheKey {
            font_generation: if vector { self.font_generation } else { 0 },
            path: path.to_path_buf(),
            box_px: cache_box(path, box_px),
            version,
            // Only vectors carry `currentColor`; drop PNG tint instead of splitting unused slots.
            tint: if vector { tint.map(packed_rgb) } else { None },
            // An SVG rasterizes straight to its box, so there is never overflow to crop.
            cropped: !vector && fit == Fit::Cover,
            // `decode` never reads `blur_px` for an animated source (ADR-0240); zeroed here too
            // for a GIF, or a blurred and a sharp draw of it would be two slots each paying its
            // full animation budget for byte-identical frames.
            blur_px: if is_gif(path) { 0 } else { blur_px },
            plays: clock.is_some() && decode::may_animate(path),
        }
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

    /// Drops `key` from the pool's wanted set and its jobs from the queue, so a cancelled decode
    /// frees its slot instead of waiting behind newer ones. A poisoned lock is ignored: the worst case is one wasted decode.
    fn unwant(&self, key: &CacheKey) {
        if let Ok(mut wanted) = self.pool.wanted.lock() {
            wanted.remove(key);
        }
        self.pool.jobs.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::snap::LogicalRect;

    fn box_rect() -> LogicalRect {
        LogicalRect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 }
    }

    #[test]
    fn fitted_rect_places_the_image_per_fit() {
        // `layout::paint`'s scissor crops Cover's overflow; a zero-sized image must not divide by zero.
        let rect = |x, y, w, h| LogicalRect { x, y, width: w, height: h };
        for (fit, (iw, ih), expected, case) in [
            (Fit::Stretch, (640.0, 480.0), box_rect(), "stretch takes the box exactly"),
            (Fit::Contain, (64.0, 64.0), rect(35.0, 20.0, 50.0, 50.0), "contain fits inside and centres"),
            (Fit::Cover, (64.0, 64.0), rect(10.0, -5.0, 100.0, 100.0), "cover overflows rather than leave a gap"),
            (Fit::Cover, (0.0, 64.0), box_rect(), "a zero-width image falls back to the box"),
            (Fit::Contain, (64.0, 0.0), box_rect(), "a zero-height image falls back to the box"),
        ] {
            assert_eq!(fitted_rect(box_rect(), iw, ih, fit), expected, "{case}");
        }
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

    pub(super) fn job(key: CacheKey) -> Job {
        Job {
            fonts: FontDatabase::default(),
            key,
            tint: None,
            animation_bytes: STARTING_TEXTURE_BUDGET,
            wants_size: false,
            size_only: false,
        }
    }

    pub(super) fn request(path: &Path, edge: u32) -> ImageRequest<'_> {
        ImageRequest {
            path,
            box_px: (edge, edge),
            tint: None,
            fit: Fit::Contain,
            blur_px: 0,
            wants_size: false,
            clock: None,
        }
    }

    pub(super) fn key(path: impl Into<PathBuf>, px: u32, version: FileVersion) -> CacheKey {
        CacheKey {
            path: path.into(),
            box_px: (px, px),
            version,
            tint: None,
            cropped: false,
            blur_px: 0,
            plays: false,
            font_generation: 0,
        }
    }

    #[test]
    fn a_watched_file_wakes_no_one_and_a_rewrite_is_one_landed_file_until_its_slot_goes() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("tray.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        let request = request(&png, 8);
        // A draw's two lookups, `image` and `loaded`.
        let key = cache.key_for(&request);
        cache.key_for(&request);
        assert!(cache.take_deferred().is_none(), "a remembered version owes no repaint");
        assert_eq!(cache.next_recheck(), None, "nor a wake");
        assert!(cache.watch_fd().is_some());
        cache.insert(key.clone(), Slot::Pending);
        assert!((0..20).all(|_| cache.poll().is_empty()), "an unchanged file is never a cue");

        std::fs::write(&png, [&PIL_2X2_RGBA_PNG[..], b"!"].concat()).unwrap();
        // `IN_CLOSE_WRITE` waits for the last close, which a fork in another test thread delays to its exec.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut cue = cache.poll();
        while cue.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
            cue = cache.poll();
        }
        assert_eq!(cue, std::slice::from_ref(&png), "a rewrite is, once");
        assert!(cache.poll().is_empty());
        let rewritten = cache.key_for(&request);
        assert_ne!(rewritten, key, "and the next draw keys the new version");
        cache.insert(rewritten.clone(), Slot::Pending);
        cache.evict(&key);
        assert_eq!(cache.versions.len(), 1, "the path is kept while another slot draws it");
        cache.evict(&rewritten);
        assert_eq!(cache.versions.len(), 0, "evicting its last slot forgets the path and its watch");
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
        assert!(!cache.pool.wanted.lock().unwrap().contains(&key), "a worker must see the decode is no longer wanted");
    }

    fn size_only_answer() -> Decoded {
        Decoded {
            base: Vec::new(),
            delays: Vec::new(),
            deltas: Vec::new(),
            width: 0,
            height: 0,
            natural: Some((2, 2)),
            premultiplied: false,
            text: false,
        }
    }

    #[test]
    fn a_size_request_the_pipeline_refused_is_asked_again_and_a_stray_answer_never_lands_as_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::inline();
        // Stands in for a worker holding the queue; dropped below, the pool has none left.
        let queue = Arc::clone(&cache.pool.jobs);
        let request = ImageRequest { wants_size: true, ..request(&png, 8) };
        let key = cache.key_for(&request);
        cache.insert(
            key.clone(),
            Slot::Ready { image: None, bytes: 0, clock: Clock::default(), anim: None, natural: None },
        );
        for n in 0..MAX_INFLIGHT_DECODES as u32 {
            cache.admit(&super::tests::key(format!("/busy{n}.png"), 8, FileVersion::default()));
        }
        assert_eq!(cache.loaded(&request), Some(Loaded::Ready(None)));
        assert!(
            queue.state.lock().unwrap().0.is_empty() && !cache.entries[&key].sized,
            "refused, so not marked as asked"
        );
        cache.pool.wanted.lock().unwrap().clear();
        cache.loaded(&request);
        assert!(queue.state.lock().unwrap().0.pop_back().is_some_and(|job| job.size_only), "the next draw asks again");
        assert!(cache.entries[&key].sized);

        // No worker left: marked asked, and no immediate repaint for an answer that cannot come.
        cache.entries.get_mut(&key).unwrap().sized = false;
        drop(queue);
        cache.take_deferred();
        let before = Instant::now();
        cache.loaded(&request);
        assert!(cache.entries[&key].sized, "sized");
        // Only the file-version recheck (in the future) may be owed, not an immediate repaint.
        assert!(cache.take_deferred().is_none_or(|due| due > before), "deferred");

        // The slot went Pending again before the stale size-only answer arrived.
        let (results, inbox) = std::sync::mpsc::channel();
        cache.pool.results = inbox;
        cache.entries.get_mut(&key).unwrap().slot = Slot::Pending;
        cache.pool.wanted.lock().unwrap().insert(key.clone());
        results.send((key.clone(), Ok(size_only_answer()))).unwrap();
        assert!(cache.poll().is_empty());
        assert!(cache.landed.is_empty(), "no pixels to upload");
        assert!(cache.pool.wanted.lock().unwrap().contains(&key), "the real job is still wanted");
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
        cache.pool.try_send(Job { fonts: cache.fonts.clone(), ..job(key) }).unwrap();
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
        cache.pool.try_send(job(key.clone())).unwrap();
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

    /// Only an `image` node, which has a clock, asks for a WebP, PNG or APNG to play; a mask, sampler or
    /// icon takes a slot of its own, and no other kind of file splits on it.
    #[test]
    fn only_an_image_node_asks_for_a_webp_png_or_apng_to_play() {
        let mut cache = ImageCache::inline();
        for (file, node, plays) in [
            ("/w/a.webp", true, true),
            ("/w/a.PNG", true, true),
            ("/w/a.apng", true, true),
            ("/w/a.webp", false, false),
            ("/w/a.png", false, false),
            ("/w/a.jpg", true, false),
            ("/w/a.gif", true, false),
        ] {
            let request = ImageRequest { clock: node.then(|| (NodeId::test(1), true)), ..request(Path::new(file), 8) };
            assert_eq!(cache.key_for(&request).plays, plays, "{file}, node: {node}");
        }
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

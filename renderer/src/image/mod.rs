//! Decodes, caches, and fits images into GPU textures (ADR-0054). PNG/JPEG/WebP use `image`
//! ([`decode_raster`]); SVG uses `resvg` because Adwaita ships scalable icons; a GIF is one texture
//! plus its frames' changed rects, replayed onto it as the slot's elapsed time advances
//! ([`decode_gif`], ADR-0233, ADR-0235).
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
//! Eviction has two bounds (ADR-0123): [`CACHE_CAPACITY`] entries, oldest insert first, and
//! [`ImageCache::set_texture_budget`] bytes of textures not shown by a mapped surface, least recently asked-for
//! first. `wayland::App` triggers it after each paint using pins from surfaces' last lists.
//! [`ImageCache::release_evicted`] frees textures at the next paint's start, never mid-frame.

mod budget;
pub mod icons;
mod svg;
pub mod thumbnails;

use budget::{Budget, Charge, Permit};
use svg::{packed_rgb, rasterize_svg};

use crate::layout::node::Rgba;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ErrorKind, ImageFlags, ImageId, ImageSource};
use shared::{error, warn};

use crate::text::snap::LogicalRect;

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
}

/// File revision at a stable path (ADR-0031 deferred item): tray updates reuse
/// `tray/{name}.png` in the instance dir, with no revision suffix. Use mtime and length, not a
/// content hash: tmpfs mtime is nanosecond-precise, length is free, and hashing reads the file to
/// decide whether to read it. Unstatable files use the default, so *missing* files retry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileVersion {
    mtime_secs: i64,
    mtime_nanos: i64,
    len: u64,
}

impl FileVersion {
    pub fn read(path: &Path) -> Self {
        let Ok(metadata) = std::fs::metadata(path) else {
            return FileVersion::default();
        };
        let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
        FileVersion {
            mtime_secs: modified.map(|d| d.as_secs() as i64).unwrap_or(-1),
            mtime_nanos: modified.map(|d| i64::from(d.subsec_nanos())).unwrap_or(-1),
            len: metadata.len(),
        }
    }
}

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

impl Fit {
    /// `"cover"`, `"contain"`, `"stretch"`; anything else `None` for the caller's property-named
    /// `LayoutError`.
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "cover" => Some(Fit::Cover),
            "contain" => Some(Fit::Contain),
            "stretch" => Some(Fit::Stretch),
            _ => None,
        }
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
}

/// One GIF frame's changed rectangle after disposal (ADR-0235): physical-pixel left/top/width/
/// height and its RGBA8 pixels, ready for `Canvas::update_image`.
struct GifDelta {
    rect: (u32, u32, u32, u32),
    pixels: Vec<u8>,
}

/// A landed GIF's replay state (ADR-0235): the base frame's own pixels, kept to re-upload on loop
/// wrap, and the rects later frames change, applied to the live texture in [`ImageCache::showing`]
/// as its shown index advances.
struct Animation {
    base: Vec<u8>,
    width: u32,
    height: u32,
    delays: Vec<Duration>,
    deltas: Vec<GifDelta>,
    /// The frame index currently on the texture.
    current: usize,
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
}

/// A queued decode's key, so a late result lands in the right slot, plus its tint and the
/// animation ceiling read when it was queued.
struct Job {
    key: CacheKey,
    tint: Option<Rgba>,
    animation_bytes: usize,
}

/// Shared decode queue and result channel, at most `MAX_DECODE_WORKERS` threads. Spawned with the
/// cache before Lua reads anything, so a failed spawn is a startup failure, not a later blank tile.
struct Pool {
    jobs: SyncSender<Job>,
    results: Receiver<(CacheKey, Result<Decoded, String>)>,
    /// Keys whose decode is still wanted. A worker checks this before spending anything on a job,
    /// so closing the picker stops the queued tiles rather than decoding all of them into slots
    /// that were evicted while they waited.
    wanted: Arc<Mutex<HashSet<CacheKey>>>,
    /// Decoded bytes in flight (ADR-0187). Shared with `ImageCache` so an inline decode on the
    /// dispatch thread is counted against the same ceiling the workers wait on.
    budget: Arc<Budget>,
}

impl Pool {
    fn spawn(waker: Option<crate::wake::Waker>) -> Self {
        let (jobs, job_rx) = std::sync::mpsc::sync_channel::<Job>(MAX_INFLIGHT_DECODES);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, results) = std::sync::mpsc::channel();
        let wanted: Arc<Mutex<HashSet<CacheKey>>> = Arc::new(Mutex::new(HashSet::new()));
        let budget: Arc<Budget> = Arc::new(Budget::default());
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, MAX_DECODE_WORKERS);
        let cache_root = thumbnails::cache_dir();
        for index in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let result_tx = result_tx.clone();
            let cache_root = cache_root.clone();
            let waker = waker.clone();
            let wanted = Arc::clone(&wanted);
            let budget = Arc::clone(&budget);
            std::thread::Builder::new()
                .name(format!("mantle-image-decode-{index}"))
                .spawn(move || {
                    loop {
                        // Hold the lock only to take a job; workers drain while another decodes.
                        let job = match job_rx.lock() {
                            Ok(rx) => rx.recv(),
                            Err(_) => return,
                        };
                        let Ok(job) = job else { return };
                        // Nobody is waiting for this any more: the entry was evicted, or the
                        // surface that asked went away. Decoding it would cost a full raster and
                        // land in a slot that `upload_landed` then skips.
                        if !wanted.lock().is_ok_and(|wanted| wanted.contains(&job.key)) {
                            continue;
                        }
                        // The permit is taken inside, where the source has been chosen and its
                        // size is known; `still_wanted` is re-asked there because a worker can now
                        // wait for room, and an entry can be evicted while it does (ADR-0187).
                        let still_wanted = || wanted.lock().is_ok_and(|wanted| wanted.contains(&job.key));
                        let result = decode(
                            &job.key,
                            job.tint,
                            cache_root.as_deref(),
                            Charge::Waiting(&budget),
                            &still_wanted,
                            job.animation_bytes,
                        );
                        if result_tx.send((job.key, result)).is_err() {
                            return;
                        }
                        // After sending, so the woken loop finds it in `poll`.
                        if let Some(waker) = &waker {
                            waker.wake();
                        }
                    }
                })
                .expect("failed to spawn a mantle-image-decode thread");
        }
        Pool { jobs, results, wanted, budget }
    }
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

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageCache {
    /// Test cache: its pool wakes nobody; tests poll for landings.
    pub fn new() -> Self {
        Self::build(None)
    }

    /// Holds idle textures to `budget` bytes from here on (ADR-0182). Called whenever the outputs
    /// change, which is the only thing that changes the answer.
    /// What one animated source may hold, scaled by the displays (ADR-0182, ADR-0233).
    fn animation_bytes(&self) -> usize {
        self.texture_budget * ANIMATION_BUDGETS
    }

    pub fn set_texture_budget(&mut self, budget: usize) {
        self.texture_budget = budget;
    }

    /// Renderer cache: a landing wakes the Wayland poll (ADR-0124).
    pub fn with_waker(waker: crate::wake::Waker) -> Self {
        Self::build(Some(waker))
    }

    fn build(waker: Option<crate::wake::Waker>) -> Self {
        ImageCache {
            entries: HashMap::new(),
            evicted: Vec::new(),
            evicted_total: 0,
            landed_total: 0,
            failed_total: 0,
            resident_bytes: 0,
            deferred: None,
            workers_gone: false,
            cancelled: Vec::new(),
            tick: 0,
            texture_budget: STARTING_TEXTURE_BUDGET,
            pool: Pool::spawn(waker),
            landed: Vec::new(),
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
        for id in self.evicted.drain(..) {
            canvas.delete_image(id);
        }
    }

    /// Takes finished background decodes and returns landed files as the repaint cue. No canvas is
    /// current here, so pixels wait in `landed` for the following paint.
    pub fn poll(&mut self) -> Vec<PathBuf> {
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
                    if !std::mem::replace(&mut self.workers_gone, true) {
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
            }
        }
    }

    /// Evicts idle textures to [`ImageCache::set_texture_budget`], oldest ask first (ADR-0123). The
    /// early return over-estimates on purpose -- idle bytes never exceed resident -- so an untouched
    /// cache skips the walk and [`victims`] measures the idle half. `pinned` contains
    /// `(path, box)` pairs from mapped surfaces' last display lists, collected by `wayland::App`
    /// after paint. Compute it lazily because list walks matter only when evicting. Icons are not
    /// pinned: lists carry theme names, not paths; an icon costs a few KB and one inline reraster.
    pub fn trim(&mut self, pinned: impl FnOnce() -> Vec<(PathBuf, (u32, u32))>) {
        if self.resident_bytes <= self.texture_budget {
            return;
        }
        let pinned = pinned();
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
    /// The canvas must be current on the sole painting thread (ADR-0039). Stat every call, hits
    /// included, because the key carries [`FileVersion`]: ten icons at 60Hz means 600 stats/sec.
    ///
    /// `Load::Inline` reads and rasterizes inside the frame, also the Wayland dispatch/config-VM
    /// thread (ADR-0039). That keeps a wallpaper's first frame whole; tile grids use
    /// `Load::Background` (ADR-0122).
    pub fn image(
        &mut self,
        canvas: &mut Canvas<OpenGl>,
        path: &Path,
        box_px: (u32, u32),
        tint: Option<Rgba>,
        load: Load,
        fit: Fit,
    ) -> Option<ImageId> {
        let vector = is_vector(path);
        let key = CacheKey {
            path: path.to_path_buf(),
            box_px: cache_box(path, box_px),
            version: FileVersion::read(path),
            // Only vectors carry `currentColor`; drop PNG tint instead of splitting unused slots.
            tint: if vector { tint.map(packed_rgb) } else { None },
            // An SVG rasterizes straight to its box, so there is never overflow to crop.
            cropped: !vector && fit == Fit::Cover,
        };
        self.tick += 1;
        if let Some(cached) = self.entries.get_mut(&key) {
            cached.last_hit = self.tick;
            return self.showing(canvas, &key);
        }
        match load {
            Load::Inline => {
                // Counted against the same ceiling the workers wait on, but never waiting for it:
                // this is the dispatch thread (ADR-0187). Nothing is queued, so nothing can be
                // evicted mid-decode and the request is still wanted by definition.
                let decoded =
                    decode(&key, tint, None, Charge::Immediate(&self.pool.budget), &|| true, self.animation_bytes());
                let slot = upload_or_log(canvas, &key.path, decoded);
                self.insert(key.clone(), slot);
                self.showing(canvas, &key)
            }
            Load::Background => {
                // One gate for the whole pipeline, not just the queue: see `MAX_INFLIGHT_DECODES`.
                // Marked before the send, because a worker that takes the job immediately must
                // find it in the set.
                if !self.admit(&key) {
                    return None;
                }
                match self.pool.jobs.try_send(Job { key: key.clone(), tint, animation_bytes: self.animation_bytes() }) {
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
                        warn!("{}: no decode worker left to take it", key.path.display());
                        self.unwant(&key);
                        self.insert(key, Slot::Failed);
                    }
                }
                None
            }
        }
    }

    /// The texture `key` is showing now, replaying its animation's deltas onto it if the elapsed
    /// time has moved its frame on, and the repaint its successor owes (ADR-0235). `None` for a
    /// pending or failed slot, which draws nothing.
    fn showing(&mut self, canvas: &mut Canvas<OpenGl>, key: &CacheKey) -> Option<ImageId> {
        let Some(Slot::Ready { image, start, anim, .. }) = self.entries.get_mut(key).map(|entry| &mut entry.slot)
        else {
            return None;
        };
        let id = *image;
        // A still, or an animation with nothing to replay, never reads the clock.
        let Some(anim) = anim.as_mut().filter(|anim| !anim.deltas.is_empty()) else {
            return Some(id);
        };
        let (target, next) = frame_at(&anim.delays, start.elapsed());
        if target != anim.current {
            let (reset, catch_up) = replay_steps(anim.current, target);
            if reset {
                let source = ImageSource::from(femtovg::imgref::Img::new(
                    anim.base.as_rgba(),
                    anim.width as usize,
                    anim.height as usize,
                ));
                let _ = canvas.update_image(id, source, 0, 0);
            }
            for delta in &anim.deltas[catch_up] {
                let (left, top, width, height) = delta.rect;
                let source = ImageSource::from(femtovg::imgref::Img::new(
                    delta.pixels.as_rgba(),
                    width as usize,
                    height as usize,
                ));
                let _ = canvas.update_image(id, source, left as usize, top as usize);
            }
            anim.current = target;
        }
        if let Some(next) = next {
            // The soonest wins: one pass can draw a refused tile and a 10 fps GIF.
            let due = Instant::now() + next;
            self.deferred = Some(self.deferred.map_or(due, |owed| owed.min(due)));
        }
        Some(id)
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
    fn insert(&mut self, key: CacheKey, slot: Slot) {
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
            let Some(coldest) = self.entries.iter().min_by_key(|(_, entry)| entry.last_hit).map(|(key, _)| key.clone())
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
        self.entries.insert(key, Entry { slot, last_hit: self.tick });
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

/// The box a source is *stored* under, which is not always the box it is drawn into: a vector
/// texture uses its longest edge, so 24x30 shares the 30x30 slot (ADR-0122).
///
/// Public because a pin has to name the same thing the entry does. `DisplayList::drawn_images`
/// collects the drawn box, and before this an `image` pointing at an SVG pinned 200x40 while the
/// entry sat under 200x200, so the exact comparison in [`victims`] missed it and evicted a texture
/// a mapped surface was showing (ADR-0183).
pub fn cache_box(path: &Path, box_px: (u32, u32)) -> (u32, u32) {
    let box_px = (box_px.0.max(1), box_px.1.max(1));
    if is_vector(path) { (box_px.0.max(box_px.1), box_px.0.max(box_px.1)) } else { box_px }
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
    pinned: &[(PathBuf, (u32, u32))],
) -> Vec<CacheKey> {
    let mut idle: Vec<(CacheKey, usize, u64)> = candidates
        .filter(|(key, _, _)| !pinned.iter().any(|(path, box_px)| *path == key.path && *box_px == key.box_px))
        .collect();
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

/// Uploads a decode or logs the failure once when filling its slot; `Failed` stops the next frame
/// asking again.
fn upload_or_log(canvas: &mut Canvas<OpenGl>, path: &Path, decoded: Result<Decoded, String>) -> Slot {
    let result = decoded.and_then(|decoded| {
        // Host delta bytes counted beside the texture's, or nothing reclaims them: `trim` is the
        // only thing that drops an entry, a shown source is pinned against it either way, and an
        // animation nobody draws held its whole ceiling for the life of the process (ADR-0235).
        let bytes = decoded.base.len() + decoded.deltas.iter().map(|delta| delta.pixels.len()).sum::<usize>();
        upload(canvas, decoded).map(|(image, anim)| Slot::Ready { image, bytes, start: Instant::now(), anim })
    });
    match result {
        Ok(slot) => slot,
        Err(err) => {
            warn!("{}: {err}", path.display());
            Slot::Failed
        }
    }
}

/// The frame showing `elapsed` after the first, and how long until the next. A still is frame 0
/// with nothing owed; anything else loops. A zero-delay frame is skipped rather than held, which
/// is why [`decode_gif`] floors what the file asks for.
fn frame_at(delays: &[Duration], elapsed: Duration) -> (usize, Option<Duration>) {
    let total: Duration = delays.iter().sum();
    if total.is_zero() {
        return (0, None);
    }
    let mut at = Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64);
    for (index, delay) in delays.iter().enumerate() {
        if at < *delay {
            return (index, Some(*delay - at));
        }
        at -= *delay;
    }
    unreachable!("the remainder is under the total, so some frame holds it")
}

/// [`Animation::deltas`] to replay to move the shown frame from `current` to `target`, and whether
/// the base has to go back up first. A loop wrap (`target < current`) always does, since nothing
/// else undoes a later frame's rects; moving forward replays only what changed since `current`.
fn replay_steps(current: usize, target: usize) -> (bool, std::ops::Range<usize>) {
    if target >= current { (false, current..target) } else { (true, 0..target) }
}

/// By extension, not sniffing: `freedesktop-icons` returns `.svg`/`.png`, and `shm_icons.rs` writes
/// `.png`. `rasterize_svg` inflates a gzipped `.svgz` itself (ADR-0234).
fn is_vector(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("svg") || ext.eq_ignore_ascii_case("svgz"))
}

/// Canvas-free load half for pool threads: raster decode/downscale through `thumbnails` when
/// available, or SVG rasterization at `box_px`'s longest edge.
fn decode(
    key: &CacheKey,
    tint: Option<Rgba>,
    thumbnails: Option<&Path>,
    charge: Charge<'_>,
    still_wanted: &dyn Fn() -> bool,
    animation_bytes: usize,
) -> Result<Decoded, String> {
    let CacheKey { path, box_px, cropped, .. } = key;
    // An SVG rasterizes to `box_px`, not to whatever the file declares, so it is bounded by the
    // request and never approaches the pool budget. `MAX_SVG_BYTES` is what bounds the parse.
    if is_vector(path) {
        let (pixels, width, height) = rasterize_svg(path, box_px.0.max(box_px.1), tint)?;
        return Ok(Decoded {
            base: pixels,
            delays: vec![Duration::ZERO],
            deltas: Vec::new(),
            width,
            height,
            premultiplied: true,
        });
    }
    // ponytail: GIF only, so an animated WebP or APNG draws its first frame. Upgrade: match the
    // sniffed format; `AnimationDecoder` covers both.
    if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("gif")) {
        return decode_gif(path, *box_px, *cropped, charge, animation_bytes);
    }
    let (pixels, width, height) = decode_raster(path, *box_px, thumbnails, charge, still_wanted)?;
    // Here rather than inside `decode_raster`, which returns from three places (thumbnail hit,
    // rescaled thumbnail, full decode) and would need the crop at each.
    let (pixels, width, height) =
        if *cropped { crop_to_box(pixels, width, height, *box_px) } else { (pixels, width, height) };
    Ok(Decoded { base: pixels, delays: vec![Duration::ZERO], deltas: Vec::new(), width, height, premultiplied: false })
}

/// The base frame and each later frame's own rect, composited here rather than by `image`'s
/// `AnimationDecoder`, which discards the rects [`ImageCache::showing`] replays (ADR-0235).
///
/// Past [`thumbnails`], which holds one surface per path and would answer frame 0 forever. A box
/// that scales or crops loses the rects and keeps whole frames instead; `native` is that split.
fn decode_gif(
    path: &Path,
    box_px: (u32, u32),
    cropped: bool,
    charge: Charge<'_>,
    budget: usize,
) -> Result<Decoded, String> {
    refuse_irregular(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let file = std::io::BufReader::new(std::fs::File::open(path).map_err(|err| err.to_string())?);
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(file).map_err(|err| err.to_string())?;
    let (source_width, source_height) = (u32::from(decoder.width()), u32::from(decoder.height()));
    if source_width.max(source_height) > MAX_DECODE_EDGE {
        return Err(format!("{source_width}x{source_height} is past the {MAX_DECODE_EDGE}px limit"));
    }
    let (stored_width, stored_height) = stored_size(source_width, source_height, box_px);
    let (width, height) =
        if cropped { (stored_width.min(box_px.0), stored_height.min(box_px.1)) } else { (stored_width, stored_height) };
    // Unscaled and uncropped only; see the doc comment above for the smaller-box fallback.
    let native = !cropped && (stored_width, stored_height) == (source_width, source_height);

    // The kept deltas' byte ceiling plus the canvas compositing runs on, which stays the source
    // size whatever the box asks for -- the same charge this function always took, now for a
    // canvas it owns directly instead of one `image::AnimationDecoder` owned internally.
    let _permit = charge.take(budget as u64 + 4 * u64::from(source_width) * u64::from(source_height));

    let mut canvas = vec![0u8; source_width as usize * source_height as usize * 4];
    let extract = |canvas: &[u8], rect: (u32, u32, u32, u32)| -> Vec<u8> {
        if native {
            return read_rect(canvas, source_width, rect);
        }
        let scaled = if (stored_width, stored_height) == (source_width, source_height) {
            canvas.to_vec()
        } else {
            let image = ::image::RgbaImage::from_raw(source_width, source_height, canvas.to_vec())
                .expect("canvas is exactly source_width * source_height * 4 bytes");
            ::image::DynamicImage::ImageRgba8(image).thumbnail(stored_width, stored_height).into_rgba8().into_raw()
        };
        if cropped { crop_to_box(scaled, stored_width, stored_height, box_px).0 } else { scaled }
    };
    let whole = (0, 0, source_width, source_height);

    let mut base = None;
    let mut delays = Vec::new();
    let mut deltas: Vec<GifDelta> = Vec::new();
    let mut delta_bytes = 0usize;
    while let Some(frame) = decoder.read_next_frame().map_err(|err| err.to_string())? {
        let rect = (u32::from(frame.left), u32::from(frame.top), u32::from(frame.width), u32::from(frame.height));
        // A floor, because a 0 ms frame is one `frame_at` skips and an all-0 file is a still, which
        // much of the web's GIFs are. 20 ms, not the 100 ms browsers substitute: a frame here costs
        // a whole surface repaint, and 50 fps is already the ceiling that buys.
        let delay = Duration::from_millis(u64::from(frame.delay) * 10).max(Duration::from_millis(20));
        // `Previous` disposal undoes this frame's draw once it has been shown; the pixels it would
        // overwrite have to be saved before that draw happens.
        let restore = (frame.dispose == gif::DisposalMethod::Previous).then(|| read_rect(&canvas, source_width, rect));
        blend_rect(&mut canvas, source_width, rect, &frame.buffer);

        if base.is_none() {
            base = Some(extract(&canvas, whole));
            delays.push(delay);
        } else {
            let pixels = extract(&canvas, rect);
            if delta_bytes + pixels.len() > budget {
                break;
            }
            delta_bytes += pixels.len();
            delays.push(delay);
            deltas.push(GifDelta { rect: if native { rect } else { (0, 0, width, height) }, pixels });
        }

        match frame.dispose {
            gif::DisposalMethod::Background => clear_rect(&mut canvas, source_width, rect),
            gif::DisposalMethod::Previous => write_rect(&mut canvas, source_width, rect, &restore.unwrap()),
            gif::DisposalMethod::Keep | gif::DisposalMethod::Any => {}
        }
    }
    let Some(base) = base else { return Err("no frames".to_string()) };
    Ok(Decoded { base, delays, deltas, width, height, premultiplied: false })
}

/// Copies a `width`-wide RGBA8 buffer's rect out, row by row.
fn read_rect(pixels: &[u8], width: u32, rect: (u32, u32, u32, u32)) -> Vec<u8> {
    let (left, top, w, h) = rect;
    let row = w as usize * 4;
    let mut out = Vec::with_capacity(row * h as usize);
    for y in 0..h {
        let start = ((top + y) as usize * width as usize + left as usize) * 4;
        out.extend_from_slice(&pixels[start..start + row]);
    }
    out
}

/// The inverse of [`read_rect`]: overwrites a rect from `src`, for `DisposalMethod::Previous`.
fn write_rect(pixels: &mut [u8], width: u32, rect: (u32, u32, u32, u32), src: &[u8]) {
    let (left, top, w, h) = rect;
    let row = w as usize * 4;
    for y in 0..h {
        let start = ((top + y) as usize * width as usize + left as usize) * 4;
        pixels[start..start + row].copy_from_slice(&src[y as usize * row..(y as usize + 1) * row]);
    }
}

/// Zeroes a rect to transparent, for `DisposalMethod::Background`. The file's own background
/// colour is not it: browsers ignore it and so does `image`'s own GIF decoder, "for web
/// compatibility", which is the behaviour a delta replay has to match.
fn clear_rect(pixels: &mut [u8], width: u32, rect: (u32, u32, u32, u32)) {
    let (left, top, w, h) = rect;
    for y in 0..h {
        let start = ((top + y) as usize * width as usize + left as usize) * 4;
        pixels[start..start + w as usize * 4].fill(0);
    }
}

/// Draws `src` (a frame's own buffer, `rect`'s size) onto a rect, skipping a transparent source
/// pixel so whatever the canvas already holds shows through. GIF transparency is a 1-bit mask,
/// never partial, so "skip" is the whole rule.
fn blend_rect(pixels: &mut [u8], width: u32, rect: (u32, u32, u32, u32), src: &[u8]) {
    let (left, top, w, _) = rect;
    for (i, pixel) in src.as_chunks::<4>().0.iter().enumerate() {
        if pixel[3] == 0 {
            continue;
        }
        let (x, y) = (left as usize + i % w as usize, top as usize + i / w as usize);
        let at = (y * width as usize + x) * 4;
        pixels[at..at + 4].copy_from_slice(pixel);
    }
}

/// Centered crop of RGBA8 `pixels` to `box_px`, for the [`Fit::Cover`] rasters [`stored_size`]
/// scales to *cover* it. The overflow is texture memory `layout::paint`'s scissor discards every
/// frame and nothing samples: 3.2 MiB a piece across 56 wallpapers on a 3440x1440 output.
///
/// Never larger than what it is given, so a source smaller than its box comes back untouched and
/// [`fitted_rect`] still letterboxes it. The centering is integer where `fitted_rect`'s is float,
/// leaving an odd overflow half a physical pixel off.
fn crop_to_box(pixels: Vec<u8>, width: u32, height: u32, box_px: (u32, u32)) -> (Vec<u8>, u32, u32) {
    let (kept_width, kept_height) = (box_px.0.min(width), box_px.1.min(height));
    if (kept_width, kept_height) == (width, height) {
        return (pixels, width, height);
    }
    let (left, top) = ((width - kept_width) / 2, (height - kept_height) / 2);
    (read_rect(&pixels, width, (left, top, kept_width, kept_height)), kept_width, kept_height)
}

/// Canvas-dependent half of a load: one texture, from the base frame, plus the later frames' rects
/// kept on the CPU for [`ImageCache::showing`] to replay (ADR-0235).
///
/// Uploads premultiplied, whatever the decoder produced (ADR-0184). `image` and `gif` hand back
/// straight alpha and `resvg` hands back premultiplied, and passing that difference on as a femtovg
/// flag was enough while femtovg was the only thing sampling these textures. A config shader
/// samples them directly, and cannot be handed two conventions: it would have to know which decoder
/// produced its endpoint, which is an engine detail with no business in a config's `main()`.
///
/// Multiplying after the sample would not do instead. A texture lookup filters between texels
/// first, so a straight-alpha edge interpolates colour the alpha was meant to hide, and no later
/// multiply recovers it. Premultiplying the buffer is one pass over pixels that are about to be
/// copied to the GPU anyway.
fn upload(canvas: &mut Canvas<OpenGl>, decoded: Decoded) -> Result<(ImageId, Option<Animation>), String> {
    let Decoded { mut base, delays, mut deltas, width, height, premultiplied } = decoded;
    if !premultiplied {
        premultiply(&mut base);
        for delta in &mut deltas {
            premultiply(&mut delta.pixels);
        }
    }
    let source = ImageSource::from(femtovg::imgref::Img::new(base.as_rgba(), width as usize, height as usize));
    let image = canvas.create_image(source, ImageFlags::PREMULTIPLIED).map_err(femtovg_error)?;
    let anim = (!deltas.is_empty()).then_some(Animation { base, width, height, delays, deltas, current: 0 });
    Ok((image, anim))
}

/// Scales each RGBA8 pixel's colour by its own alpha, rounding the way a straight-to-premultiplied
/// conversion has to: `(c * a + 127) / 255`, not `c * a / 255`, or every translucent pixel darkens
/// by up to half a level and a large flat region bands visibly.
fn premultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            let scaled = u16::from(*channel) * alpha + 127;
            *channel = ((scaled + scaled / 255) / 256) as u8;
        }
    }
}

/// Every one of femtovg's sixteen `ErrorKind` variants formats as `"canvas error"`; `Debug` names
/// the actual failure.
fn femtovg_error(err: ErrorKind) -> String {
    format!("{err:?}")
}

/// Stored raster size for a `box_px` box (ADR-0122): scale by the larger ratio to cover as
/// `Fit::Cover` crops, never upscale a small file. Use the same rule for every `Fit`; `Contain`
/// could be smaller, but one rule keeps one slot per box.
fn stored_size(width: u32, height: u32, box_px: (u32, u32)) -> (u32, u32) {
    if width == 0 || height == 0 || (width <= box_px.0 && height <= box_px.1) {
        return (width, height);
    }
    let scale = (box_px.0 as f64 / width as f64).max(box_px.1 as f64 / height as f64);
    if scale >= 1.0 {
        return (width, height);
    }
    (((width as f64 * scale).ceil() as u32).max(1), ((height as f64 * scale).ceil() as u32).max(1))
}

/// Decodes PNG/JPEG/WebP to straight-alpha RGBA8 at [`stored_size`]. `image` is direct because
/// femtovg's `Canvas::load_image_file` declares it `default-features = false` with no format:
/// every PNG returned `Unsupported(Exact(Png))`, breaking tray and notification pixmaps (ADR-0031).
/// `into_rgba8` also handles grayscale+alpha and 16-bit variants femtovg refuses, while theme
/// icons already decode to RGBA8. `thumbnail`'s triangle filter avoids a second Lanczos pass when
/// shrinking a 4K file to a tile.
///
/// If `thumbnails` has a covering size, decode a current thumbnail instead of the file. A full
/// decode larger than that size leaves one behind; a 32px tray icon is never thumbnailed. Write
/// failure logs once and still produces the texture.
fn decode_raster(
    path: &Path,
    box_px: (u32, u32),
    thumbnails: Option<&Path>,
    charge: Charge<'_>,
    still_wanted: &dyn Fn() -> bool,
) -> Result<(Vec<u8>, u32, u32), String> {
    let slot = thumbnails.and_then(|root| thumbnails::Slot::for_file(root, path, box_px));
    if let Some(slot) = &slot
        && let Some((pixels, width, height)) = slot.read_valid()
    {
        let (stored_width, stored_height) = stored_size(width, height, box_px);
        if (stored_width, stored_height) == (width, height) {
            return Ok((pixels, width, height));
        }
        let image = ::image::RgbaImage::from_raw(width, height, pixels).ok_or("thumbnail pixel count is off")?;
        let scaled = ::image::DynamicImage::ImageRgba8(image).thumbnail(stored_width, stored_height).into_rgba8();
        let (width, height) = scaled.dimensions();
        return Ok((scaled.into_raw(), width, height));
    }
    // Past the thumbnail branch, so the budget is charged for the source actually decoded and a
    // covering thumbnail is never made to wait for room it does not need (ADR-0187).
    let (decoded, _permit) = decode_within_limits(path, MAX_DECODE_EDGE, charge, still_wanted)?;
    let (width, height) = (decoded.width(), decoded.height());
    let (stored_width, stored_height) = stored_size(width, height, box_px);
    // The thumbnail this pass writes is also the best source for the texture it is about to make,
    // whenever it still covers the stored size: scaling 128x128 down beats scaling 4096x4096 down
    // to the same place, and the second one measured 27 ms on a 4096 source here. It is also what
    // every *later* open of this file already does, one branch up, so a first open producing
    // pixels from the full source was the odd one out rather than the careful one.
    let mut covering_thumbnail = None;
    if let Some(slot) = &slot
        && width.max(height) > slot.px
    {
        let thumb = decoded.thumbnail(slot.px, slot.px).into_rgba8();
        let (thumb_width, thumb_height) = thumb.dimensions();
        if let Err(err) = slot.write(thumb.as_raw(), thumb_width, thumb_height) {
            warn!("{}: thumbnail not written: {err}", path.display());
        }
        // Both axes, because `stored_size` fills the box while `thumbnail` fits inside it: a wide
        // source thumbnails to 128x72 and stores at 228x128, and rescaling from that would be an
        // upscale of a thumbnail rather than a downscale of a photograph.
        if thumb_width >= stored_width && thumb_height >= stored_height {
            covering_thumbnail = Some(::image::DynamicImage::ImageRgba8(thumb));
        }
    }
    let decoded = covering_thumbnail.unwrap_or(decoded);
    let scaled = if (stored_width, stored_height) == (decoded.width(), decoded.height()) {
        decoded
    } else {
        decoded.thumbnail(stored_width, stored_height)
    };
    let rgba = scaled.into_rgba8();
    let (width, height) = rgba.dimensions();
    Ok((rgba.into_raw(), width, height))
}

/// Reads at most `cap` bytes of `path`, or `None` if the file has more than that.
fn read_capped(path: &Path, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    refuse_irregular(path)?;
    take_capped(std::fs::File::open(path)?, cap)
}

/// Reads at most `cap` bytes of `source`, or `None` past that.
///
/// Reads `cap + 1` so "exactly at the limit" and "over it" are distinguishable, and never
/// allocates more than that however much the source turns out to hold.
fn take_capped(source: impl std::io::Read, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut data = Vec::new();
    source.take(cap + 1).read_to_end(&mut data)?;
    Ok((data.len() as u64 <= cap).then_some(data))
}

/// Refuses anything that is not a regular file, before anything tries to open it.
///
/// `File::open` on a FIFO with no writer blocks until one appears, and [`Load::Inline`] -- the
/// default -- opens on the Wayland dispatch thread. One such path is therefore not a failed image
/// but a shell frozen with no way back, which is a far worse outcome than any decode error this
/// module already handles.
///
/// `icons::resolve` returns an absolute name as a path without looking at it, and a config may name
/// any path at all, so the check belongs at the open rather than at one of the callers.
///
/// Stat-then-open leaves a TOCTOU window, the same one
/// `capabilities::notifications::icon::validate_trusted_path` already accepts. It turns "hangs
/// forever" into "hangs only if something wins a race", without an `O_NONBLOCK` fd dance.
fn refuse_irregular(path: &Path) -> std::io::Result<()> {
    if std::fs::metadata(path)?.is_file() {
        return Ok(());
    }
    Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"))
}

/// Decodes one raster file under `max_edge`, charging its pixels to `budget`.
///
/// `max_edge` is [`MAX_DECODE_EDGE`] for a source file, whose size nothing here gets to choose.
/// A caller that already knows what the file is allowed to be passes that instead, and the limit
/// then binds the decoder that produces the pixels rather than a header read of an earlier open:
/// see [`thumbnails::Slot::read_valid`], which validates one open and decodes another, in a
/// directory any process of this user can write between the two.
///
/// `image::open` reads the header and then the whole surface, so the size the caller wanted never
/// entered into it: a thumbnail request for a 8000x6000 photo still allocated ~192 MB, and
/// [`MAX_DECODE_WORKERS`] of those at once is most of a gigabyte. `ImageReader` applies the edge
/// limits during decoding, so an oversized source is refused rather than allocated for.
///
/// The size comes from the decoder rather than from `w * h * 4`, and one decoder serves both the
/// question and the answer. Guessing the output from the dimensions is wrong in both directions:
/// a 16-bit source needs `w * h * 8` and would be admitted on half its true cost, while the RGB8
/// JPEG in the folder this was written for needs `w * h * 3` and would be charged a third more
/// than it takes. `total_bytes` is the number the crate's own `max_alloc` checks, exactly.
///
/// Reusing the decoder also avoids opening the file twice: `into_dimensions` is not the free header
/// read it looks like, reading the whole compressed file for a JPEG (8.7 ms against a PNG's 27 µs
/// here). Paid once by the decode that follows, it costs nothing; paid by a separate probe first,
/// it doubles.
pub(super) fn decode_within_limits<'a>(
    path: &Path,
    max_edge: u32,
    charge: Charge<'a>,
    still_wanted: &dyn Fn() -> bool,
) -> Result<(::image::DynamicImage, Option<Permit<'a>>), String> {
    refuse_irregular(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut reader = ::image::ImageReader::open(path)
        .map_err(|err| err.to_string())?
        .with_guessed_format()
        .map_err(|err| err.to_string())?;
    let mut limits = ::image::Limits::no_limits();
    limits.max_image_width = Some(max_edge);
    limits.max_image_height = Some(max_edge);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|err| err.to_string())?;
    let need = ::image::ImageDecoder::total_bytes(&decoder);
    // One decode may have the whole pool but not more than it, which is what keeps the ceiling a
    // ceiling now that it is no longer divided by [`MAX_DECODE_WORKERS`]. Without this an
    // 8192x8192 16-bit source would be admitted alone at 512 MiB, twice what the four workers
    // could reach before.
    if need > DECODE_POOL_BYTES {
        return Err(format!("decodes to {need} bytes, past the {DECODE_POOL_BYTES}-byte pool budget"));
    }
    // Refused on `need` alone, so a source at the ceiling still decodes alone; see `Budget`.
    let (width, height) = ::image::ImageDecoder::dimensions(&decoder);
    let permit = charge.take(2 * need + 4 * u64::from(width) * u64::from(height));
    // Re-asked after the wait, not only before it: waiting is what this added, and an entry can be
    // evicted while a worker sits in `acquire`. Cheap to ask, a whole decode to get wrong.
    if !still_wanted() {
        return Err("evicted while waiting for decode budget".to_string());
    }
    Ok((::image::DynamicImage::from_decoder(decoder).map_err(|err| err.to_string())?, permit))
}

/// Image rect inside `box_rect` for its dimensions and [`Fit`]. `Cover` may exceed the box because
/// `layout::paint`'s scissor crops overflow. femtovg clamps outside a paint extent unless
/// `REPEAT_X`/`REPEAT_Y` are set; a smaller rect would smear edge pixels, while `Contain` returns
/// the smaller rect.
pub fn fitted_rect(box_rect: LogicalRect, image_width: f32, image_height: f32, fit: Fit) -> LogicalRect {
    if fit == Fit::Stretch || image_width <= 0.0 || image_height <= 0.0 {
        return box_rect;
    }
    let horizontal = box_rect.width / image_width;
    let vertical = box_rect.height / image_height;
    let scale = match fit {
        Fit::Cover => horizontal.max(vertical),
        Fit::Contain => horizontal.min(vertical),
        Fit::Stretch => unreachable!("returned above"),
    };
    let width = image_width * scale;
    let height = image_height * scale;
    LogicalRect {
        x: box_rect.x + (box_rect.width - width) / 2.0,
        y: box_rect.y + (box_rect.height - height) / 2.0,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_rect() -> LogicalRect {
        LogicalRect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 }
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
    fn only_svg_is_rasterized_by_size() {
        assert!(is_vector(Path::new("/usr/share/icons/Adwaita/symbolic/x.svg")));
        assert!(is_vector(Path::new("/tmp/X.SVG")));
        assert!(is_vector(Path::new("/tmp/gzipped.svgz")));
        assert!(!is_vector(Path::new("/run/user/1000/mantle/tray/telegram.png")));
        assert!(!is_vector(Path::new("/tmp/no-extension")));
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
    const PIL_2X2_RGBA_PNG: [u8; 80] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00,
        0x17, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x05, 0xc1, 0x01, 0x01, 0x00, 0x00, 0x00, 0x82, 0x20, 0xa6, 0xf7,
        0xdc, 0x40, 0x24, 0x43, 0xc1, 0x01, 0x3a, 0xdc, 0x05, 0x7c, 0xf2, 0x4a, 0x44, 0x5b, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn a_png_decodes_to_the_pixels_it_was_written_with() {
        // Regression: no decoder, not a wrong pixel. femtovg disables every `image` format, so
        // before `decode_raster` this file, themed PNGs, and tray pixmaps (ADR-0031) failed with
        // `Unsupported(Png)`.
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();

        let (pixels, width, height) =
            decode_raster(&png, (2, 2), None, Charge::Free, &|| true).expect("a PNG decoder must be compiled in");
        assert_eq!((width, height), (2, 2));
        // Straight alpha in Pillow's order: half-transparent green stays 0x00ff00, not
        // premultiplied 0x008000.
        assert_eq!(pixels, vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn a_file_that_is_not_an_image_reports_why_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("not-really.png");
        std::fs::write(&fake, b"<svg/>").unwrap();
        assert!(decode_raster(&fake, (8, 8), None, Charge::Free, &|| true).is_err());
    }

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

    /// Eviction frees a `Failed` slot's memory; it does not un-fail the load.
    #[test]
    fn failed_census_survives_capacity_eviction_of_old_failed_entries() {
        let mut cache = ImageCache::new();
        for n in 0..(CACHE_CAPACITY + 10) {
            cache.insert(key(format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY, "the ten coldest were evicted to stay at the bound");
        assert_eq!(cache.census().failed, CACHE_CAPACITY + 10, "every insert failed once, evicted or not");
    }

    fn key(path: impl Into<PathBuf>, px: u32, version: FileVersion) -> CacheKey {
        CacheKey { path: path.into(), box_px: (px, px), version, tint: None, cropped: false }
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
        let pinned = vec![(PathBuf::from("/w/shown.jpg"), (1920, 1920))];
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
        let pinned = vec![(PathBuf::from("/w/shown.jpg"), (3440, 3440))];

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
        let pinned = vec![(PathBuf::from("/w/a.jpg"), (1920, 1920))];
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
    fn a_raster_is_stored_scaled_down_to_cover_its_box_and_never_up() {
        // 4K into a 16:9 tile: ratios agree.
        assert_eq!(stored_size(3840, 2160, (230, 130)), (232, 130));
        // Portrait into landscape: the larger width ratio covers.
        assert_eq!(stored_size(1080, 1920, (230, 130)), (230, 409));
        // Smaller on both edges: untouched.
        assert_eq!(stored_size(16, 16, (24, 24)), (16, 16));
        // Larger on one edge only: the larger ratio is still under one.
        assert_eq!(stored_size(300, 10, (100, 100)), (300, 10));
        assert_eq!(stored_size(0, 0, (100, 100)), (0, 0));
    }

    /// One byte per pixel, so a crop reads back as the pixels it kept.
    fn gray(width: u32, height: u32) -> Vec<u8> {
        (0..width * height).flat_map(|i| [i as u8, i as u8, i as u8, 255]).collect()
    }

    fn crop(width: u32, height: u32, box_px: (u32, u32)) -> ((u32, u32), Vec<u8>) {
        let (pixels, width, height) = crop_to_box(gray(width, height), width, height, box_px);
        ((width, height), pixels.iter().step_by(4).copied().collect())
    }

    #[test]
    fn a_cover_crop_keeps_the_middle_and_only_the_axis_that_overflows() {
        // 4x4 into a 2x2 box: the centred 2x2 is rows 1-2, columns 1-2.
        assert_eq!(crop(4, 4, (2, 2)), ((2, 2), vec![5, 6, 9, 10]));
        // `stored_size` covers, so one axis overflows unless the ratios agree; the other must come
        // through whole rather than be squared off to the box.
        assert_eq!(crop(2, 4, (2, 2)), ((2, 2), vec![2, 3, 4, 5]));
        // Smaller than its box: untouched, so `fitted_rect` still letterboxes a small file.
        assert_eq!(crop(2, 2, (8, 8)), ((2, 2), vec![0, 1, 2, 3]));
    }

    /// ADR-0187, the case that started it: a real wallpaper of 6024x3401 decodes to 78 MiB of RGBA
    /// from 400 KB on disk. That is comfortably inside the 256 MiB pool and past a 64 MiB quarter
    /// of it, so the per-decode cap refused it unread, permanently, while the pool sat idle.
    ///
    /// Solid colour keeps the fixture a few hundred KB and the encode near instant; the dimensions
    /// are what matter, because they are what the decoder charges for.
    #[test]
    fn a_source_past_one_workers_old_share_of_the_budget_decodes_and_gives_its_bytes_back() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("wallpaper.png");
        ::image::RgbaImage::from_pixel(6024, 3401, ::image::Rgba([7, 9, 11, 255])).save(&big).unwrap();

        let pixels = 6024_u64 * 3401 * 4;
        assert!(pixels > 64 * 1024 * 1024, "the fixture must be past the per-decode cap this replaced");
        assert!(pixels < DECODE_POOL_BYTES, "and inside the pool budget that replaced it");

        let budget = Budget::default();
        let decoded = decode_within_limits(&big, MAX_DECODE_EDGE, Charge::Waiting(&budget), &|| true);
        let (image, permit) = decoded.expect("a source inside the pool budget must decode");
        assert_eq!((image.width(), image.height()), (6024, 3401));
        assert_eq!(*budget.in_flight.lock().unwrap(), 3 * pixels, "held for the caller's scaling");
        drop(permit);
        assert_eq!(*budget.in_flight.lock().unwrap(), 0, "and given back with it");

        // And the wait's own hazard: an entry evicted while its worker sat in `acquire` must not
        // then be decoded into a slot that has gone away.
        assert!(
            decode_within_limits(&big, MAX_DECODE_EDGE, Charge::Free, &|| false).is_err(),
            "a decode nobody wants any more must be abandoned rather than paid for"
        );
    }

    #[test]
    fn a_source_wider_than_the_decode_limit_is_refused_rather_than_allocated_for() {
        // The point of the limit: a thumbnail-sized request used to pay for the whole surface
        // first, four workers at a time. One pixel tall keeps this test cheap while still being
        // genuinely over the edge limit, which is what the decoder checks.
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide.png");
        ::image::RgbaImage::from_pixel(MAX_DECODE_EDGE + 1, 1, ::image::Rgba([1, 2, 3, 255])).save(&wide).unwrap();
        assert!(
            decode_within_limits(&wide, MAX_DECODE_EDGE, Charge::Free, &|| true).is_err(),
            "a source past MAX_DECODE_EDGE must not be decoded"
        );

        // The limit a caller supplies binds the same way, and this is the one that matters:
        // `thumbnails::Slot::read_valid` validates one open of a shared cache file and decodes
        // another, so the header it checked is not evidence about the pixels it gets.
        let swapped = dir.path().join("swapped.png");
        ::image::RgbaImage::from_pixel(200, 200, ::image::Rgba([1, 2, 3, 255])).save(&swapped).unwrap();
        assert!(
            decode_within_limits(&swapped, 128, Charge::Free, &|| true).is_err(),
            "a 128px slot must not decode a 200px file, however valid it looked a moment ago"
        );
        assert!(decode_within_limits(&swapped, 256, Charge::Free, &|| true).is_ok());

        let ordinary = dir.path().join("ordinary.png");
        ::image::RgbaImage::from_pixel(4, 4, ::image::Rgba([1, 2, 3, 255])).save(&ordinary).unwrap();
        assert!(
            decode_within_limits(&ordinary, MAX_DECODE_EDGE, Charge::Free, &|| true).is_ok(),
            "an ordinary file must still decode"
        );
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
    fn a_large_png_decodes_to_its_box_and_a_small_one_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        ::image::RgbaImage::from_pixel(400, 200, ::image::Rgba([10, 20, 30, 255])).save(&big).unwrap();
        let (_, width, height) = decode_raster(&big, (100, 100), None, Charge::Free, &|| true).unwrap();
        assert_eq!((width, height), (200, 100));
        let (_, width, height) = decode_raster(&big, (1000, 1000), None, Charge::Free, &|| true).unwrap();
        assert_eq!((width, height), (400, 200));
    }

    #[test]
    fn a_pool_decode_leaves_a_thumbnail_behind_and_the_next_one_reads_it_instead_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        ::image::RgbaImage::from_pixel(400, 200, ::image::Rgba([10, 20, 30, 255])).save(&big).unwrap();
        let cache = dir.path().join("cache");

        let (_, width, height) = decode_raster(&big, (100, 100), Some(&cache), Charge::Free, &|| true).unwrap();
        assert_eq!((width, height), (200, 100), "the texture is the box's, whatever the thumbnail is");
        let slot = thumbnails::Slot::for_file(&cache, &big, (100, 100)).unwrap();
        let (_, thumb_width, thumb_height) = slot.read_valid().expect("a `normal` thumbnail was written");
        assert_eq!((thumb_width, thumb_height), (128, 64));

        // Source gone: only the thumbnail can answer, and it does.
        std::fs::remove_file(&big).unwrap();
        assert!(
            decode_raster(&big, (100, 100), Some(&cache), Charge::Free, &|| true).is_err(),
            "no source, no mtime, no slot"
        );
    }

    #[test]
    fn a_file_no_larger_than_a_thumbnail_is_not_thumbnailed() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("icon.png");
        ::image::RgbaImage::from_pixel(32, 32, ::image::Rgba([10, 20, 30, 255])).save(&small).unwrap();
        let cache = dir.path().join("cache");
        decode_raster(&small, (24, 24), Some(&cache), Charge::Free, &|| true).unwrap();
        assert!(!cache.exists(), "a 32px file has nothing to gain from a 128px thumbnail");
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
        cache.pool.jobs.send(Job { key: key.clone(), tint: None, animation_bytes: STARTING_TEXTURE_BUDGET }).unwrap();
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

    #[test]
    fn fit_parses_the_three_spelled_modes_and_nothing_else() {
        assert_eq!(Fit::from_str("cover"), Some(Fit::Cover));
        assert_eq!(Fit::from_str("contain"), Some(Fit::Contain));
        assert_eq!(Fit::from_str("stretch"), Some(Fit::Stretch));
        assert_eq!(Fit::from_str("Cover"), None);
        assert_eq!(Fit::from_str("fill"), None);
        assert_eq!(Fit::default(), Fit::Cover);
    }

    /// The failure this prevents is not a bad image but a frozen shell: `File::open` on a FIFO with
    /// no writer blocks forever, and `Load::Inline` opens on the Wayland dispatch thread.
    #[test]
    fn a_fifo_is_refused_rather_than_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("icon.png");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `mkfifo` takes a NUL-terminated path and a mode; `path` outlives the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0, "the test needs a real FIFO");

        // Both open paths must refuse it. Neither call may block, which is what this asserts by
        // returning at all.
        assert!(
            decode_within_limits(&fifo, MAX_DECODE_EDGE, Charge::Free, &|| true).is_err(),
            "a FIFO must not reach the decoder"
        );
        assert!(read_capped(&fifo, 1024).is_err(), "nor the SVG reader");

        // A regular file at the same name still works, so the guard refuses the type, not the path.
        let real = dir.path().join("real.svg");
        std::fs::write(&real, b"<svg/>").unwrap();
        assert!(read_capped(&real, 1024).unwrap().is_some());
    }

    /// ADR-0233. Each frame's own delay, the zero-delay frame skipped, and a loop rather than a
    /// stop on the last frame. A still owes nothing, which is what keeps a PNG off the poll
    /// loop's timeout.
    #[test]
    fn a_frame_index_follows_each_delay_and_loops() {
        let delays = [Duration::from_millis(100), Duration::ZERO, Duration::from_millis(50)];
        assert_eq!(frame_at(&delays, Duration::ZERO), (0, Some(Duration::from_millis(100))));
        assert_eq!(frame_at(&delays, Duration::from_millis(99)), (0, Some(Duration::from_millis(1))));
        assert_eq!(frame_at(&delays, Duration::from_millis(100)), (2, Some(Duration::from_millis(50))));
        assert_eq!(frame_at(&delays, Duration::from_millis(150)), (0, Some(Duration::from_millis(100))));
        assert_eq!(frame_at(&delays, Duration::from_millis(1_000_000)), (2, Some(Duration::from_millis(50))));
        assert_eq!(frame_at(&[Duration::ZERO], Duration::from_secs(9)), (0, None));
    }

    /// [`replay_steps`] moving forward replays only what changed; a loop wrap always re-uploads the
    /// base first, since nothing else undoes a later frame's rect.
    #[test]
    fn replay_steps_only_resets_on_a_loop_wrap() {
        assert_eq!(replay_steps(0, 3), (false, 0..3));
        assert_eq!(replay_steps(2, 3), (false, 2..3));
        assert_eq!(replay_steps(2, 2), (false, 2..2), "no movement, nothing to replay");
        assert_eq!(replay_steps(3, 0), (true, 0..0), "wrapping to the base itself replays no delta");
        assert_eq!(replay_steps(3, 1), (true, 0..1), "wrapping past the base replays up to the target");
    }

    /// Writes a `width`x`height` GIF frame of one RGBA colour at `rect`, with `dispose` and `delay`
    /// (centiseconds, the file's own unit).
    fn write_frame(
        encoder: &mut gif::Encoder<&mut std::fs::File>,
        rect: (u16, u16, u16, u16),
        rgba: [u8; 4],
        dispose: gif::DisposalMethod,
        delay: u16,
    ) {
        let (left, top, width, height) = rect;
        let mut pixels = rgba.repeat(width as usize * height as usize);
        let mut frame = gif::Frame::from_rgba(width, height, &mut pixels);
        frame.left = left;
        frame.top = top;
        frame.dispose = dispose;
        frame.delay = delay;
        encoder.write_frame(&frame).unwrap();
    }

    /// ADR-0235. Every disposal method decides what the *next* frame's transparent pixels show
    /// through to, never the current one's own draw -- `Keep` carries a frame's pixels forward,
    /// `Background` clears its rect to transparent first, `Previous` undoes the draw entirely. A
    /// delta replay has to land on the same pixels an independent full recomposite gives at every
    /// frame, which is `image`'s own `AnimationDecoder` here, not this module.
    #[test]
    fn disposal_methods_replay_to_the_same_pixels_a_full_recomposite_gives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dispose.gif");
        {
            let mut file = std::fs::File::create(&path).unwrap();
            let mut encoder = gif::Encoder::new(&mut file, 4, 4, &[]).unwrap();
            write_frame(&mut encoder, (0, 0, 4, 4), [255, 0, 0, 255], gif::DisposalMethod::Keep, 5);
            // Background: its green square must not still show once this frame is gone.
            write_frame(&mut encoder, (0, 0, 2, 2), [0, 255, 0, 255], gif::DisposalMethod::Background, 5);
            // Transparent over a `Background`-disposed rect shows through to nothing, not to the
            // red frame 0 underneath it.
            write_frame(&mut encoder, (0, 0, 2, 2), [0, 0, 0, 0], gif::DisposalMethod::Keep, 5);
            // Previous: its blue square must not still show once this frame is gone either.
            write_frame(&mut encoder, (0, 0, 2, 2), [0, 0, 255, 255], gif::DisposalMethod::Previous, 5);
            write_frame(&mut encoder, (0, 0, 2, 2), [0, 0, 0, 0], gif::DisposalMethod::Keep, 5);
        }

        let decoded = decode_gif(&path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.deltas.len(), 4, "5 frames, the first is the base");
        assert!(decoded.deltas.iter().all(|delta| delta.rect == (0, 0, 2, 2)), "the native rect, not the full canvas");

        let gif_decoder =
            ::image::codecs::gif::GifDecoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
                .unwrap();
        let ground_truth = ::image::AnimationDecoder::into_frames(gif_decoder).collect_frames().unwrap();
        assert_eq!(ground_truth.len(), 5);
        for (index, frame) in ground_truth.iter().enumerate() {
            let mut replayed = decoded.base.clone();
            for delta in &decoded.deltas[..index] {
                write_rect(&mut replayed, 4, delta.rect, &delta.pixels);
            }
            assert_eq!(
                replayed,
                frame.buffer().as_raw().as_slice(),
                "frame {index} diverged from the full recomposite"
            );
        }
    }

    /// ADR-0235. The frame that would push the kept deltas past their byte budget is dropped
    /// rather than collapsing the whole animation to a still: playback loops over what fit.
    #[test]
    fn a_delta_past_its_byte_budget_is_dropped_and_the_rest_still_play() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.gif");
        {
            let mut file = std::fs::File::create(&path).unwrap();
            let mut encoder = gif::Encoder::new(&mut file, 4, 4, &[]).unwrap();
            write_frame(&mut encoder, (0, 0, 4, 4), [255, 0, 0, 255], gif::DisposalMethod::Keep, 5);
            for n in 0..4u8 {
                write_frame(&mut encoder, (0, 0, 2, 2), [0, n * 50, 0, 255], gif::DisposalMethod::Keep, 5);
            }
        }
        // Each 2x2 delta is 16 bytes; a 32-byte budget keeps exactly two.
        let decoded = decode_gif(&path, (4, 4), false, Charge::Free, 32).unwrap();
        assert_eq!(decoded.deltas.len(), 2, "a third 16-byte delta would total 48 bytes, past the 32-byte budget");
        assert_eq!(decoded.delays.len(), 3, "the base plus the two kept deltas");
    }
}

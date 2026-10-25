//! Off-thread worker pool (`Pool`) and raster image decoding (`decode_raster`, ADR-0122, ADR-0187).

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use shared::{debug, error};

use super::budget::{Budget, Charge, Permit};
use super::svg::rasterize_svg;
use super::{
    CacheKey, DECODE_POOL_BYTES, Decoded, Job, MAX_DECODE_EDGE, MAX_DECODE_WORKERS, MAX_INFLIGHT_DECODES, Natural,
    Pool, thumbnails,
};
use crate::layout::node::Rgba;

/// Why a job was not queued.
#[derive(Debug)]
pub(super) enum Refused {
    Full,
    Disconnected,
}

/// Jobs bounded at [`MAX_INFLIGHT_DECODES`], popped newest first: after a fast scroll the tiles
/// on screen are the last asked for, and the older ones are about to be cancelled.
#[derive(Default)]
pub(super) struct Queue {
    pub(super) state: Mutex<(VecDeque<Job>, bool)>,
    ready: Condvar,
}

impl Queue {
    fn try_push(&self, job: Job) -> Result<(), Refused> {
        let Ok(mut state) = self.state.lock() else { return Err(Refused::Disconnected) };
        if state.0.len() >= MAX_INFLIGHT_DECODES {
            return Err(Refused::Full);
        }
        state.0.push_back(job);
        self.ready.notify_one();
        Ok(())
    }

    /// Blocks for the newest job; `None` once the pool has dropped and the queue is empty.
    fn pop(&self) -> Option<Job> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(job) = state.0.pop_back() {
                return Some(job);
            }
            if state.1 {
                return None;
            }
            state = self.ready.wait(state).ok()?;
        }
    }

    /// Drops `key`'s queued jobs, so a cancelled decode frees its slot and cannot run beside a re-ask.
    pub(super) fn remove(&self, key: &CacheKey) {
        if let Ok(mut state) = self.state.lock() {
            state.0.retain(|job| job.key != *key);
        }
    }

    pub(super) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.0.clear();
            state.1 = true;
        }
        self.ready.notify_all();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.jobs.close();
    }
}

impl Pool {
    pub(super) fn try_send(&self, job: Job) -> Result<(), Refused> {
        if Arc::strong_count(&self.jobs) == 1 {
            return Err(Refused::Disconnected);
        }
        self.jobs.try_push(job)
    }

    /// `max_workers` threads at most; zero decodes every request inline.
    pub(super) fn spawn(waker: Option<crate::wake::Waker>, max_workers: usize) -> Self {
        let jobs = Arc::new(Queue::default());
        let (result_tx, results) = std::sync::mpsc::channel();
        let wanted: Arc<Mutex<HashSet<CacheKey>>> = Arc::new(Mutex::new(HashSet::new()));
        let budget = Arc::new(Budget::default());
        let workers =
            std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, MAX_DECODE_WORKERS).min(max_workers);
        let cache_root = thumbnails::cache_dir();
        let mut started = 0;
        for index in 0..workers {
            let jobs = Arc::clone(&jobs);
            let result_tx = result_tx.clone();
            let cache_root = cache_root.clone();
            let waker = waker.clone();
            let wanted = Arc::clone(&wanted);
            let budget = Arc::clone(&budget);
            let spawned = std::thread::Builder::new().name(format!("mantle-image-decode-{index}")).spawn(move || {
                loop {
                    let Some(job) = jobs.pop() else { return };
                    // The permit is taken inside `decode`, where the source has been chosen and its
                    // size is known; `still_wanted` is re-asked there because a worker can now
                    // wait for room, and an entry can be evicted while it does (ADR-0187).
                    let still_wanted = || wanted.lock().is_ok_and(|wanted| wanted.contains(&job.key));
                    // Nobody is waiting for this any more: the entry was evicted, or the
                    // surface that asked went away. Decoding it would cost a full raster and
                    // land in a slot that `upload_landed` then skips.
                    if !still_wanted() {
                        continue;
                    }
                    let mut result = if job.size_only {
                        Ok(Decoded {
                            base: Vec::new(),
                            delays: Vec::new(),
                            deltas: Vec::new(),
                            width: 0,
                            height: 0,
                            natural: None,
                            premultiplied: false,
                            text: false,
                        })
                    } else {
                        decode(
                            &job.key,
                            job.tint,
                            cache_root.as_deref(),
                            Charge::Waiting(&budget),
                            &still_wanted,
                            job.animation_bytes,
                            &job.fonts,
                        )
                    };
                    // A thumbnail hit does not know the source's size; read it only if asked.
                    if let Ok(decoded) = &mut result
                        && job.wants_size
                        && decoded.natural.is_none()
                    {
                        decoded.natural = header_size(&job.key.path);
                    }
                    if result_tx.send((job.key, result)).is_err() {
                        return;
                    }
                    // After sending, so the woken loop finds it in `poll`.
                    if let Some(waker) = &waker {
                        waker.wake();
                    }
                }
            });
            match spawned {
                Ok(_) => started += 1,
                Err(err) => error!("failed to spawn mantle-image-decode thread: {err}"),
            }
        }
        Pool { jobs, workers: started, results, wanted, budget }
    }
}

/// By extension, not sniffing: icon lookup returns `.svg`, `.svgz` or `.png`, and `shm_icons.rs` writes
/// `.png`. `rasterize_svg` inflates a gzipped `.svgz` itself (ADR-0234).
pub(crate) fn is_vector(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("svg") || ext.eq_ignore_ascii_case("svgz"))
}

/// ponytail: GIF only, so an animated WebP or APNG draws its first frame like a still (and so, per
/// [`ImageCache::image`](super::ImageCache::image), can carry a `source_blur`). Upgrade: match the sniffed format;
/// `AnimationDecoder` covers both.
pub(crate) fn is_animated(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("gif"))
}

/// Canvas-free load half for pool threads: raster decode/downscale through `thumbnails` when
/// available, or SVG rasterization at `box_px`'s longest edge.
pub(super) fn decode(
    key: &CacheKey,
    tint: Option<Rgba>,
    thumbnails: Option<&Path>,
    charge: Charge<'_>,
    still_wanted: &dyn Fn() -> bool,
    animation_bytes: usize,
    fonts: &crate::text::shaping::FontDatabase,
) -> Result<Decoded, String> {
    let CacheKey { path, box_px, cropped, blur_px, .. } = key;
    // An SVG rasterizes to `box_px`, not to whatever the file declares, so it is bounded by the
    // request and never approaches the pool budget. `MAX_SVG_BYTES` is what bounds the parse.
    if is_vector(path) {
        let (pixels, width, height, text, natural) = rasterize_svg(path, box_px.0.max(box_px.1), tint, fonts)?;
        let natural = plausible(natural);
        let pixels = blur_rgba(pixels, width, height, *blur_px, true)?;
        return Ok(Decoded {
            base: pixels,
            delays: vec![Duration::ZERO],
            deltas: Vec::new(),
            width,
            height,
            natural,
            premultiplied: true,
            text,
        });
    }
    // `blur_px` is never passed on here (ADR-0240): the delta replay `decode_gif` stores keeps
    // only each frame's changed rect, and a blur samples past that rect's edge, so blurring
    // correctly means storing a full frame per delta -- the exact cost the deltas exist to avoid.
    // A `source_blur` on an animated source is silently ignored, matching ADR-0195 decision 7's
    // rule for a capability the config asked for and the pipeline does not have; `ImageCache::
    // image` also zeroes it in the key, so a blurred and a sharp draw of the same GIF share one
    // slot instead of each paying the animation budget for identical frames.
    if is_animated(path) {
        return super::gif::decode_gif(path, *box_px, *cropped, charge, animation_bytes);
    }
    let (mut pixels, mut width, mut height, natural) = decode_raster(path, *box_px, thumbnails, charge, still_wanted)?;
    // Here rather than inside `decode_raster`, which returns from three places and would need the crop at each.
    if *cropped && let Some(kept) = crop_to_box(&pixels, width, height, *box_px) {
        (pixels, width, height) = kept; // drops the uncropped source before the blur
    }
    let premultiplied = *blur_px > 0;
    let pixels = blur_rgba(pixels, width, height, *blur_px, false)?;
    Ok(Decoded {
        base: pixels,
        delays: vec![Duration::ZERO],
        deltas: Vec::new(),
        width,
        height,
        natural,
        premultiplied,
        text: false,
    })
}

/// Centered crop of RGBA8 `pixels` to `box_px`, for the [`Fit::Cover`](super::Fit::Cover) rasters [`stored_size`]
/// scales to *cover* it. The overflow is texture memory `layout::paint`'s scissor discards every
/// frame and nothing samples: 3.2 MiB a piece across 56 wallpapers on a 3440x1440 output.
///
/// Never larger than what it is given: `None` when nothing is cropped, so the caller keeps its buffer and
/// [`fitted_rect`](super::fitted_rect) still letterboxes a small source. The centering is integer where `fitted_rect`'s is float,
/// leaving an odd overflow half a physical pixel off.
pub(super) fn crop_to_box(pixels: &[u8], width: u32, height: u32, box_px: (u32, u32)) -> Option<(Vec<u8>, u32, u32)> {
    let (kept_width, kept_height) = (box_px.0.min(width), box_px.1.min(height));
    if (kept_width, kept_height) == (width, height) {
        return None;
    }
    let (left, top) = ((width - kept_width) / 2, (height - kept_height) / 2);
    Some((super::gif::read_rect(pixels, width, (left, top, kept_width, kept_height)), kept_width, kept_height))
}

/// Scales each RGBA8 pixel's colour by its own alpha, rounding the way a straight-to-premultiplied
/// conversion has to: `(c * a + 127) / 255`, not `c * a / 255`, or every translucent pixel darkens
/// by up to half a level and a large flat region bands visibly.
pub(super) fn premultiply(pixels: &mut [u8]) {
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

/// `image.source_blur` (ADR-0240): a static blur, run once here rather than every repaint because
/// the source it blurs never changes for the life of a decode. `fast_blur`, not the exact
/// `imageops::blur`: the true Gaussian is a separable FIR whose cost scales with sigma and whose
/// peak working set is several multiples of the raster (measured ~12x at 4K), which turned a
/// large `source_blur` on a wallpaper-sized image into a multi-second stall on the thread that
/// called `decode` (the dispatch/config-VM thread itself, under the default `async = false`).
/// `fast_blur`'s three box passes cost the same regardless of sigma and peak at a small multiple
/// of the raster instead. Both assume premultiplied input for a non-constant alpha; a
/// straight-alpha raster is premultiplied first, and the result is handed back already in that
/// state so `upload` does not premultiply it a second time. A no-op for `blur_px == 0`; no
/// dimension guard beside it; `box_px` is never zero (`physical_edge` floors at 1), and
/// `fast_blur` itself tolerates a zero-sized buffer.
fn blur_rgba(pixels: Vec<u8>, width: u32, height: u32, blur_px: u32, premultiplied: bool) -> Result<Vec<u8>, String> {
    if blur_px == 0 {
        return Ok(pixels);
    }
    let mut pixels = pixels;
    if !premultiplied {
        premultiply(&mut pixels);
    }
    let image = ::image::RgbaImage::from_raw(width, height, pixels).ok_or("blurred image pixel count is off")?;
    Ok(::image::imageops::fast_blur(&image, blur_px as f32).into_raw())
}

/// `image` scaled to exactly `size` as straight-alpha RGBA8 (ADR-0317), in its decoded pixel type
/// so a JPEG widens to RGBA at the target size, never the source's.
pub(super) fn downscale(image: &::image::DynamicImage, size: (u32, u32)) -> Result<::image::RgbaImage, String> {
    use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
    let mut scaled = ::image::DynamicImage::new(size.0, size.1, image.color());
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    Resizer::new().resize(image, &mut scaled, &options).map_err(|err| err.to_string())?;
    Ok(scaled.into_rgba8())
}

/// The largest size with `width`x`height`'s aspect that fits an `edge`-sided square, rounded and
/// never zero: `DynamicImage::thumbnail`'s rule, so thumbnails it cached keep their size.
pub(super) fn fit_inside(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let ratio = (f64::from(edge) / f64::from(width)).min(f64::from(edge) / f64::from(height));
    let scaled = |side: u32| ((f64::from(side) * ratio).round() as u32).max(1);
    (scaled(width), scaled(height))
}

/// Stored raster size for a `box_px` box (ADR-0122): scale by the larger ratio to cover as
/// `Fit::Cover` crops, never upscale a small file. Use the same rule for every `Fit`; `Contain`
/// could be smaller, but one rule keeps one slot per box.
pub(super) fn stored_size(width: u32, height: u32, box_px: (u32, u32)) -> (u32, u32) {
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
/// icons already decode to RGBA8. Every shrink goes through [`downscale`].
///
/// If `thumbnails` has a covering size, decode a current thumbnail instead of the file. A full
/// decode larger than that size leaves one behind; a 32px tray icon is never thumbnailed. Write
/// failure logs once and still produces the texture. The last field is the source's own size,
/// unknown on a thumbnail hit.
fn decode_raster(
    path: &Path,
    box_px: (u32, u32),
    thumbnails: Option<&Path>,
    charge: Charge<'_>,
    still_wanted: &dyn Fn() -> bool,
) -> Result<(Vec<u8>, u32, u32, Option<Natural>), String> {
    let slot = thumbnails.and_then(|root| thumbnails::Slot::for_file(root, path, box_px));
    if let Some(slot) = &slot
        && let Some((pixels, width, height)) = slot.read_valid()
    {
        let (stored_width, stored_height) = stored_size(width, height, box_px);
        if (stored_width, stored_height) == (width, height) {
            return Ok((pixels, width, height, None));
        }
        let image = ::image::RgbaImage::from_raw(width, height, pixels).ok_or("thumbnail pixel count is off")?;
        let scaled = downscale(&::image::DynamicImage::ImageRgba8(image), (stored_width, stored_height))?;
        return Ok((scaled.into_raw(), stored_width, stored_height, None));
    }
    // Past the thumbnail branch, so the budget is charged for the source actually decoded and a
    // covering thumbnail is never made to wait for room it does not need (ADR-0187).
    let (decoded, _permit) = decode_within_limits(path, MAX_DECODE_EDGE, charge, still_wanted)?;
    let (width, height) = (decoded.width(), decoded.height());
    let (stored_width, stored_height) = stored_size(width, height, box_px);
    // The thumbnail this pass writes is also the best source for the texture, whenever it still
    // covers the stored size: scaling it down beats scaling a 4096x4096 source (27 ms here), and
    // every later open already does.
    let mut covering_thumbnail = None;
    if let Some(slot) = &slot
        && width.max(height) > slot.px
    {
        let thumb = downscale(&decoded, fit_inside(width, height, slot.px))?;
        let (thumb_width, thumb_height) = thumb.dimensions();
        if let Err(err) = slot.write(thumb.as_raw(), thumb_width, thumb_height) {
            debug!("{}: thumbnail not written: {err}", path.display());
        }
        // Both axes, because `stored_size` fills the box while `fit_inside` fits inside it: a wide
        // source thumbnails to 128x72 and stores at 228x128, and rescaling from that would be an
        // upscale of a thumbnail rather than a downscale of a photograph.
        if thumb_width >= stored_width && thumb_height >= stored_height {
            covering_thumbnail = Some(::image::DynamicImage::ImageRgba8(thumb));
        }
    }
    let decoded = covering_thumbnail.unwrap_or(decoded);
    let rgba = if (stored_width, stored_height) == (decoded.width(), decoded.height()) {
        decoded.into_rgba8()
    } else {
        downscale(&decoded, (stored_width, stored_height))?
    };
    Ok((rgba.into_raw(), stored_width, stored_height, Some((width, height))))
}

/// The source's size from its header, without decoding pixels. Not free: a JPEG's is a scan of
/// the whole file for its markers, so it runs on a worker and only for a bound `status`.
fn header_size(path: &Path) -> Option<Natural> {
    refuse_irregular(path).ok()?;
    plausible(::image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_dimensions().ok()?)
}

/// A reported source size only when both edges are in `1..=MAX_DECODE_EDGE`.
pub(super) fn plausible(size: Natural) -> Option<Natural> {
    let edges = 1..=MAX_DECODE_EDGE;
    (edges.contains(&size.0) && edges.contains(&size.1)).then_some(size)
}

/// Reads at most `cap` bytes of `path`, or `None` if the file has more than that.
pub(super) fn read_capped(path: &Path, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    refuse_irregular(path)?;
    take_capped(std::fs::File::open(path)?, cap)
}

/// Reads at most `cap` bytes of `source`, or `None` past that.
///
/// Reads `cap + 1` so "exactly at the limit" and "over it" are distinguishable, and never
/// allocates more than that however much the source turns out to hold.
pub(super) fn take_capped(source: impl std::io::Read, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut data = Vec::new();
    source.take(cap + 1).read_to_end(&mut data)?;
    Ok((data.len() as u64 <= cap).then_some(data))
}

/// Refuses anything that is not a regular file, before anything tries to open it.
///
/// `File::open` on a FIFO with no writer blocks until one appears, and [`Load::Inline`](super::Load::Inline) -- the
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
pub(super) fn refuse_irregular(path: &Path) -> std::io::Result<()> {
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
    // One decode may have the whole pool but not more than it, which keeps the pool a ceiling:
    // an 8192x8192 16-bit source would otherwise be admitted alone at 512 MiB.
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

#[cfg(test)]
mod tests {
    use super::super::tests::{PIL_2X2_RGBA_PNG, job, key};
    use super::super::{FileVersion, STARTING_TEXTURE_BUDGET};
    use super::*;

    #[test]
    fn the_newest_queued_job_decodes_first_and_the_bound_holds() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::spawn(None, 1);
        let keys = ["a", "b", "c"].map(|name| {
            let path = dir.path().join(format!("{name}.png"));
            std::fs::write(&path, PIL_2X2_RGBA_PNG).unwrap();
            let key = key(&path, 8, FileVersion::read(&path));
            pool.wanted.lock().unwrap().insert(key.clone());
            key
        });
        // The worker is blocked on this lock, so all three are queued before it takes one.
        let mut held = pool.jobs.state.lock().unwrap();
        keys.iter().for_each(|key| held.0.push_back(job(key.clone())));
        drop(held);
        pool.jobs.ready.notify_one();
        let order: Vec<_> = (0..3).map(|_| pool.results.recv_timeout(Duration::from_secs(5)).unwrap().0).collect();
        assert_eq!(order, [&keys[2], &keys[1], &keys[0]].map(Clone::clone), "FIFO would decode a first");

        let full = Queue::default();
        for n in 0..MAX_INFLIGHT_DECODES as u32 {
            assert!(full.try_push(job(key("/full.png", n, FileVersion::default()))).is_ok());
        }
        assert!(matches!(full.try_push(job(key("/full.png", 99, FileVersion::default()))), Err(Refused::Full)));
    }

    #[test]
    fn workers_exit_when_the_pool_drops() {
        let pool = Pool::spawn(None, 2);
        let queue = Arc::clone(&pool.jobs);
        drop(pool);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&queue) > 1 {
            assert!(std::time::Instant::now() < deadline, "a worker is still waiting on the dropped pool's queue");
            std::thread::yield_now();
        }
    }

    #[test]
    fn only_svg_is_rasterized_by_size_and_only_gif_is_animated() {
        assert!(is_vector(Path::new("/usr/share/icons/Adwaita/symbolic/x.svg")));
        assert!(is_vector(Path::new("/tmp/X.SVG")));
        assert!(is_vector(Path::new("/tmp/gzipped.svgz")));
        assert!(!is_vector(Path::new("/run/user/1000/mantle/tray/telegram.png")));
        assert!(!is_vector(Path::new("/tmp/no-extension")));

        assert!(is_animated(Path::new("/tmp/wallpaper.gif")));
        assert!(is_animated(Path::new("/tmp/WALLPAPER.GIF")));
        assert!(!is_animated(Path::new("/tmp/still.png")));
        assert!(!is_animated(Path::new("/tmp/no-extension")));
    }

    #[test]
    fn a_png_decodes_to_the_pixels_it_was_written_with() {
        // Regression: no decoder, not a wrong pixel. femtovg disables every `image` format, so
        // before `decode_raster` this file, themed PNGs, and tray pixmaps (ADR-0031) failed with
        // `Unsupported(Png)`.
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();

        let (pixels, width, height, _) =
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
        let err = decode_raster(&fake, (8, 8), None, Charge::Free, &|| true).unwrap_err();
        assert!(err.contains("Invalid PNG signature"), "the decoder's own reason, not a bare failure: {err}");
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
        // Thumbnails fit inside their square instead, and a sliver keeps one pixel.
        assert_eq!(fit_inside(3840, 2160, 128), (128, 72));
        assert_eq!(fit_inside(10_000, 10, 128), (128, 1));
    }

    /// ADR-0317. A one-pixel checker of opaque white and transparent black averages to white at
    /// half alpha. Straight-alpha averaging would give grey: the transparent pixels' black would
    /// bleed into every edge a scaled icon has.
    #[test]
    fn a_downscale_averages_its_area_with_alpha_premultiplied() {
        let checker = ::image::RgbaImage::from_fn(16, 16, |x, y| {
            ::image::Rgba(if (x + y) % 2 == 0 { [255, 255, 255, 255] } else { [0, 0, 0, 0] })
        });
        let scaled = downscale(&::image::DynamicImage::ImageRgba8(checker), (4, 4)).unwrap();
        assert_eq!(scaled.dimensions(), (4, 4));
        for pixel in scaled.pixels() {
            assert!(pixel.0[..3].iter().all(|&c| c >= 253), "colour stays white: {pixel:?}");
            assert!(pixel.0[3].abs_diff(128) <= 2, "alpha is the area's mean: {pixel:?}");
        }
    }

    /// A JPEG decodes to RGB8 and scales in it; the RGBA it comes back as is opaque.
    #[test]
    fn an_rgb_downscale_comes_back_opaque_rgba() {
        let rgb = ::image::RgbImage::from_pixel(8, 4, ::image::Rgb([10, 20, 30]));
        let rgba = downscale(&::image::DynamicImage::ImageRgb8(rgb), (4, 2)).unwrap();
        assert_eq!(rgba.dimensions(), (4, 2));
        assert!(rgba.pixels().all(|pixel| pixel.0 == [10, 20, 30, 255]));
    }

    /// One byte per pixel, so a crop reads back as the pixels it kept.
    fn gray(width: u32, height: u32) -> Vec<u8> {
        (0..width * height).flat_map(|i| [i as u8, i as u8, i as u8, 255]).collect()
    }

    fn crop(width: u32, height: u32, box_px: (u32, u32)) -> ((u32, u32), Vec<u8>) {
        let pixels = gray(width, height);
        let (pixels, width, height) = crop_to_box(&pixels, width, height, box_px).unwrap_or((pixels, width, height));
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
    fn a_sub_pixel_svg_reports_no_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.svg");
        std::fs::write(&path, r#"<svg xmlns="http://www.w3.org/2000/svg" width="0.4" height="0.4"/>"#).unwrap();
        let key = CacheKey { box_px: (8, 8), ..super::super::tests::key(&path, 8, Default::default()) };
        let decoded = decode(&key, None, None, Charge::Free, &|| true, 0, &Default::default()).unwrap();
        assert_eq!(decoded.natural, None, "a 0px width is no size");
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
    }

    #[test]
    fn a_pool_decode_leaves_a_thumbnail_behind_and_the_next_one_reads_it_instead_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        ::image::RgbaImage::from_pixel(400, 200, ::image::Rgba([10, 20, 30, 255])).save(&big).unwrap();
        let cache = dir.path().join("cache");

        let (_, width, height, _) = decode_raster(&big, (100, 100), Some(&cache), Charge::Free, &|| true).unwrap();
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

    /// A thumbnail hit draws the thumbnail, left as written, without the source's size, which
    /// the source's header gives when `image` can read it.
    #[test]
    fn a_thumbnail_hit_draws_as_written_and_the_header_gives_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let png = dir.path().join("wall.png");
        ::image::RgbaImage::from_pixel(400, 200, ::image::Rgba([10, 20, 30, 255])).save(&png).unwrap();
        let unreadable = dir.path().join("wall.xcf");
        std::fs::write(&unreadable, b"not a format `image` knows").unwrap();
        for (source, natural) in [(&png, Some((400, 200))), (&unreadable, None)] {
            let slot = thumbnails::Slot::for_file(&cache, source, (100, 100)).unwrap();
            slot.write(&[9, 8, 7, 255].repeat(128 * 64), 128, 64).unwrap();
            let written = std::fs::read(slot.path()).unwrap();
            let (_, width, height, size) =
                decode_raster(source, (100, 100), Some(&cache), Charge::Free, &|| true).unwrap();
            assert_eq!(((width, height), size), ((128, 64), None), "the thumbnail drew: {}", source.display());
            assert_eq!(header_size(source), natural);
            assert_eq!(std::fs::read(slot.path()).unwrap(), written, "the thumbnail is never rewritten");
        }
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

    /// The failure this prevents is not a bad image but a frozen shell: `File::open` on a FIFO with
    /// no writer blocks forever, and `Load::Inline` opens on the Wayland dispatch thread.
    #[test]
    fn a_fifo_is_refused_rather_than_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("icon.png");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("the test needs a real FIFO");

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

    /// ADR-0240. `blur_px == 0` must be a true no-op: `physical_blur` in `layout::paint` floors at
    /// 0 rather than [`physical_edge`](crate::layout::paint::physical_edge)'s 1 for exactly this
    /// reason, and this is the decode-side half of that guarantee.
    #[test]
    fn source_blur_softens_a_hard_edge_and_zero_leaves_it_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.png");
        let (w, h) = (8u32, 8u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let v = if x < w / 2 { 0 } else { 255 };
                pixels[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        ::image::RgbaImage::from_raw(w, h, pixels).unwrap().save(&path).unwrap();

        let sharp = key(&path, w, FileVersion::read(&path));
        let blurred = CacheKey { blur_px: 3, ..sharp.clone() };
        let fonts = crate::text::shaping::FontDatabase::default();
        let a = decode(&sharp, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET, &fonts).unwrap();
        let b = decode(&blurred, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET, &fonts).unwrap();

        // The last black pixel before the edge, on the middle row.
        let at = ((h / 2 * w + w / 2 - 1) * 4) as usize;
        assert_eq!(a.base[at], 0, "source_blur = 0 leaves the hard edge untouched");
        assert!(b.base[at] > 0 && b.base[at] < 255, "source_blur = 3 softens across it, got {}", b.base[at]);
        assert!(!a.premultiplied, "an untouched raster stays straight alpha");
        assert!(b.premultiplied, "blurring premultiplies so upload does not do it twice");
    }
}

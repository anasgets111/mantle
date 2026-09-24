use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared::{debug, error};

use super::budget::{Budget, Charge, Permit};
use super::svg::rasterize_svg;
use super::{
    CacheKey, DECODE_POOL_BYTES, Decoded, GifDelta, Job, MAX_DECODE_EDGE, MAX_DECODE_WORKERS, MAX_INFLIGHT_DECODES,
    Pool, thumbnails,
};
use crate::layout::node::Rgba;

impl Pool {
    /// `max_workers` threads at most; zero decodes every request inline.
    pub(super) fn spawn(waker: Option<crate::wake::Waker>, max_workers: usize) -> Self {
        let (jobs, job_rx) = std::sync::mpsc::sync_channel::<Job>(MAX_INFLIGHT_DECODES);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, results) = std::sync::mpsc::channel();
        let wanted: Arc<Mutex<HashSet<CacheKey>>> = Arc::new(Mutex::new(HashSet::new()));
        let budget: Arc<Budget> = Arc::new(Budget::default());
        let workers =
            std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, MAX_DECODE_WORKERS).min(max_workers);
        let cache_root = thumbnails::cache_dir();
        let mut started = 0;
        for index in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let result_tx = result_tx.clone();
            let cache_root = cache_root.clone();
            let waker = waker.clone();
            let wanted = Arc::clone(&wanted);
            let budget = Arc::clone(&budget);
            let spawned = std::thread::Builder::new().name(format!("mantle-image-decode-{index}")).spawn(move || {
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
            });
            match spawned {
                Ok(_) => started += 1,
                Err(err) => error!("failed to spawn mantle-image-decode thread: {err}"),
            }
        }
        Pool { jobs, workers: started, results, wanted, budget }
    }
}

/// By extension, not sniffing: `freedesktop-icons` returns `.svg`/`.png`, and `shm_icons.rs` writes
/// `.png`. `rasterize_svg` inflates a gzipped `.svgz` itself (ADR-0234).
pub(super) fn is_vector(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("svg") || ext.eq_ignore_ascii_case("svgz"))
}

/// ponytail: GIF only, so an animated WebP or APNG draws its first frame like a still (and so, per
/// [`ImageCache::image`](super::ImageCache::image), can carry a `source_blur`). Upgrade: match the sniffed format;
/// `AnimationDecoder` covers both.
pub(super) fn is_animated(path: &Path) -> bool {
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
) -> Result<Decoded, String> {
    let CacheKey { path, box_px, cropped, blur_px, .. } = key;
    // An SVG rasterizes to `box_px`, not to whatever the file declares, so it is bounded by the
    // request and never approaches the pool budget. `MAX_SVG_BYTES` is what bounds the parse.
    if is_vector(path) {
        let (pixels, width, height) = rasterize_svg(path, box_px.0.max(box_px.1), tint)?;
        let pixels = blur_rgba(pixels, width, height, *blur_px, true)?;
        return Ok(Decoded {
            base: pixels,
            delays: vec![Duration::ZERO],
            deltas: Vec::new(),
            width,
            height,
            premultiplied: true,
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
        return decode_gif(path, *box_px, *cropped, charge, animation_bytes);
    }
    let (pixels, width, height) = decode_raster(path, *box_px, thumbnails, charge, still_wanted)?;
    // Here rather than inside `decode_raster`, which returns from three places (thumbnail hit,
    // rescaled thumbnail, full decode) and would need the crop at each.
    let (pixels, width, height) =
        if *cropped { crop_to_box(pixels, width, height, *box_px) } else { (pixels, width, height) };
    let premultiplied = *blur_px > 0;
    let pixels = blur_rgba(pixels, width, height, *blur_px, false)?;
    Ok(Decoded { base: pixels, delays: vec![Duration::ZERO], deltas: Vec::new(), width, height, premultiplied })
}

/// The base frame and each later frame's own rect, composited here rather than by `image`'s
/// `AnimationDecoder`, which discards the rects [`ImageCache::showing`](super::ImageCache::showing) replays (ADR-0235).
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
    // Where a source rect lands in the stored frame: floor the origin and ceil the far edge, so a
    // shrunk change keeps the edge pixels it bleeds into, then take off what the centred crop cut.
    let map_rect = |(left, top, w, h): (u32, u32, u32, u32)| {
        let start = |v: u32, from: u32, to: u32| (u64::from(v) * u64::from(to) / u64::from(from).max(1)) as u32;
        let end = |v: u32, from: u32, to: u32| (u64::from(v) * u64::from(to)).div_ceil(u64::from(from).max(1)) as u32;
        let (crop_x, crop_y) = ((stored_width - width) / 2, (stored_height - height) / 2);
        let x0 = start(left, source_width, stored_width).saturating_sub(crop_x).min(width);
        let y0 = start(top, source_height, stored_height).saturating_sub(crop_y).min(height);
        let x1 = end(left + w, source_width, stored_width).saturating_sub(crop_x).min(width);
        let y1 = end(top + h, source_height, stored_height).saturating_sub(crop_y).min(height);
        (x0, y0, x1 - x0, y1 - y0)
    };
    // The pixels for `rect` and where they land, both in the stored frame's coordinates. Scaling
    // the whole canvas and cutting the rect out of the result keeps resampling seam-free while what
    // is kept stays the size of the change; extracting from a scaled sub-rect would seam.
    let extract = |canvas: &[u8], rect: (u32, u32, u32, u32)| -> (Vec<u8>, (u32, u32, u32, u32)) {
        if native {
            return (read_rect(canvas, source_width, rect), rect);
        }
        let scaled = if (stored_width, stored_height) == (source_width, source_height) {
            canvas.to_vec()
        } else {
            let image = ::image::RgbaImage::from_raw(source_width, source_height, canvas.to_vec())
                .expect("canvas is exactly source_width * source_height * 4 bytes");
            ::image::DynamicImage::ImageRgba8(image)
                .thumbnail_exact(stored_width, stored_height)
                .into_rgba8()
                .into_raw()
        };
        let frame = if cropped { crop_to_box(scaled, stored_width, stored_height, box_px).0 } else { scaled };
        let mapped = map_rect(rect);
        if mapped == (0, 0, width, height) { (frame, mapped) } else { (read_rect(&frame, width, mapped), mapped) }
    };
    let whole = (0, 0, source_width, source_height);

    let mut base = None;
    let mut delays = Vec::new();
    let mut deltas: Vec<GifDelta> = Vec::new();
    let mut delta_bytes = 0usize;
    while let Some(frame) = decoder.read_next_frame().map_err(|err| err.to_string())? {
        let (left, top, w, h) =
            (u32::from(frame.left), u32::from(frame.top), u32::from(frame.width), u32::from(frame.height));
        if left + w > source_width || top + h > source_height {
            continue;
        }
        let rect = (left, top, w, h);
        // A floor, because a 0 ms frame is one `frame_at` skips and an all-0 file is a still, which
        // much of the web's GIFs are. 20 ms, not the 100 ms browsers substitute: a frame here costs
        // a whole surface repaint, and 50 fps is already the ceiling that buys.
        let delay = Duration::from_millis(u64::from(frame.delay) * 10).max(Duration::from_millis(20));
        // `Previous` disposal undoes this frame's draw once it has been shown; the pixels it would
        // overwrite have to be saved before that draw happens.
        let restore = (frame.dispose == gif::DisposalMethod::Previous).then(|| read_rect(&canvas, source_width, rect));
        blend_rect(&mut canvas, source_width, rect, &frame.buffer);

        if base.is_none() {
            base = Some(extract(&canvas, whole).0);
            delays.push(delay);
        } else {
            let (pixels, rect) = extract(&canvas, rect);
            if delta_bytes + pixels.len() > budget {
                break;
            }
            delta_bytes += pixels.len();
            delays.push(delay);
            deltas.push(GifDelta { rect, pixels });
        }

        match frame.dispose {
            gif::DisposalMethod::Background => clear_rect(&mut canvas, source_width, rect),
            gif::DisposalMethod::Previous => {
                if let Some(ref saved) = restore {
                    write_rect(&mut canvas, source_width, rect, saved);
                }
            }
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
    let (left, top, w, h) = rect;
    if w == 0 || h == 0 {
        return;
    }
    for (i, pixel) in src.as_chunks::<4>().0.iter().enumerate() {
        if pixel[3] == 0 {
            continue;
        }
        let (x, y) = (left as usize + i % w as usize, top as usize + i / w as usize);
        let at = (y * width as usize + x) * 4;
        pixels[at..at + 4].copy_from_slice(pixel);
    }
}

/// Centered crop of RGBA8 `pixels` to `box_px`, for the [`Fit::Cover`](super::Fit::Cover) rasters [`stored_size`]
/// scales to *cover* it. The overflow is texture memory `layout::paint`'s scissor discards every
/// frame and nothing samples: 3.2 MiB a piece across 56 wallpapers on a 3440x1440 output.
///
/// Never larger than what it is given, so a source smaller than its box comes back untouched and
/// [`fitted_rect`](super::fitted_rect) still letterboxes it. The centering is integer where `fitted_rect`'s is float,
/// leaving an odd overflow half a physical pixel off.
fn crop_to_box(pixels: Vec<u8>, width: u32, height: u32, box_px: (u32, u32)) -> (Vec<u8>, u32, u32) {
    let (kept_width, kept_height) = (box_px.0.min(width), box_px.1.min(height));
    if (kept_width, kept_height) == (width, height) {
        return (pixels, width, height);
    }
    let (left, top) = ((width - kept_width) / 2, (height - kept_height) / 2);
    (read_rect(&pixels, width, (left, top, kept_width, kept_height)), kept_width, kept_height)
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
            debug!("{}: thumbnail not written: {err}", path.display());
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
    use super::super::tests::{PIL_2X2_RGBA_PNG, key};
    use super::super::{FileVersion, STARTING_TEXTURE_BUDGET};
    use super::*;

    #[test]
    fn only_svg_is_rasterized_by_size() {
        assert!(is_vector(Path::new("/usr/share/icons/Adwaita/symbolic/x.svg")));
        assert!(is_vector(Path::new("/tmp/X.SVG")));
        assert!(is_vector(Path::new("/tmp/gzipped.svgz")));
        assert!(!is_vector(Path::new("/run/user/1000/mantle/tray/telegram.png")));
        assert!(!is_vector(Path::new("/tmp/no-extension")));
    }

    #[test]
    fn only_gif_is_animated() {
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

    /// A wallpaper is scaled, cropped, or both, so it never takes the native path; keeping a whole
    /// stored frame per delta there cost a screenful each and clipped the loop at the byte budget.
    #[test]
    fn a_scaled_gif_keeps_the_changed_rect_rather_than_a_whole_stored_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scaled.gif");
        {
            let mut file = std::fs::File::create(&path).unwrap();
            let mut encoder = gif::Encoder::new(&mut file, 8, 8, &[]).unwrap();
            write_frame(&mut encoder, (0, 0, 8, 8), [255, 0, 0, 255], gif::DisposalMethod::Keep, 5);
            write_frame(&mut encoder, (4, 4, 4, 4), [0, 255, 0, 255], gif::DisposalMethod::Keep, 5);
        }

        let decoded = decode_gif(&path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!((decoded.width, decoded.height), (4, 4), "8x8 halved into a 4x4 box");
        assert_eq!(decoded.base.len(), 4 * 4 * 4, "the base is still the whole stored frame");
        assert_eq!(decoded.deltas[0].rect, (2, 2, 2, 2), "the source rect halved with it");
        assert_eq!(decoded.deltas[0].pixels.len(), 2 * 2 * 4, "against 64 bytes for a whole stored frame");
    }

    /// A 10x7 source covering a 3x3 box stores at 5x3 (`stored_size` ceils both edges), which an
    /// aspect-keeping `thumbnail` would round down to 4x3 and the crop would then read past.
    #[test]
    fn a_gif_whose_cover_size_rounds_up_scales_to_exactly_that_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.gif");
        {
            let mut file = std::fs::File::create(&path).unwrap();
            let mut encoder = gif::Encoder::new(&mut file, 10, 7, &[]).unwrap();
            write_frame(&mut encoder, (0, 0, 10, 7), [255, 0, 0, 255], gif::DisposalMethod::Keep, 5);
            write_frame(&mut encoder, (6, 4, 4, 3), [0, 255, 0, 255], gif::DisposalMethod::Keep, 5);
        }

        let decoded = decode_gif(&path, (3, 3), true, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!((decoded.width, decoded.height), (3, 3));
        assert_eq!(decoded.base.len(), 3 * 3 * 4);
        let (_, _, w, h) = decoded.deltas[0].rect;
        assert_eq!(decoded.deltas[0].pixels.len(), (w * h * 4) as usize);
    }

    /// ADR-0240. `decode`'s call into `decode_gif` never passes `blur_px`, so this is a structural
    /// no-op rather than a checked one; this test is what would catch a future refactor wiring it
    /// through by accident.
    #[test]
    fn source_blur_is_ignored_on_an_animated_gif() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("animated.gif");
        {
            let mut file = std::fs::File::create(&path).unwrap();
            let mut encoder = gif::Encoder::new(&mut file, 4, 4, &[]).unwrap();
            write_frame(&mut encoder, (0, 0, 4, 4), [255, 0, 0, 255], gif::DisposalMethod::Keep, 5);
            write_frame(&mut encoder, (0, 0, 4, 4), [0, 255, 0, 255], gif::DisposalMethod::Keep, 5);
        }
        let sharp = key(&path, 4, FileVersion::read(&path));
        let blurred = CacheKey { blur_px: 50, ..sharp.clone() };

        let a = decode(&sharp, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET).unwrap();
        let b = decode(&blurred, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(a.base, b.base, "an animated source keeps playing sharp regardless of source_blur");
        assert_eq!(a.deltas.len(), b.deltas.len());
        for (sharp, blurred) in a.deltas.iter().zip(&b.deltas) {
            assert_eq!(sharp.pixels, blurred.pixels);
        }
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
        let a = decode(&sharp, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET).unwrap();
        let b = decode(&blurred, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET).unwrap();

        // The last black pixel before the edge, on the middle row.
        let at = ((h / 2 * w + w / 2 - 1) * 4) as usize;
        assert_eq!(a.base[at], 0, "source_blur = 0 leaves the hard edge untouched");
        assert!(b.base[at] > 0 && b.base[at] < 255, "source_blur = 3 softens across it, got {}", b.base[at]);
        assert!(!a.premultiplied, "an untouched raster stays straight alpha");
        assert!(b.premultiplied, "blurring premultiplies so upload does not do it twice");
    }
}

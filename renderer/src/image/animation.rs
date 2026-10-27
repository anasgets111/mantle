//! Animated image decoding and software frame compositing: GIF through `decode_gif` (ADR-0235), WebP
//! and APNG through `decode_frames` and `image`'s `AnimationDecoder`.

use std::path::Path;
use std::time::Duration;

use ::image::{AnimationDecoder, ImageDecoder};
use shared::debug;

use super::budget::Charge;
use super::decode::{crop_to_box, downscale, refuse_irregular, stored_size};
use super::{Decoded, Delta, MAX_DECODE_EDGE};

type Rect = (u32, u32, u32, u32);

/// The base frame and each later frame's own rect, composited here rather than by `image`'s
/// `AnimationDecoder`, which discards the rects [`ImageCache::showing`](super::ImageCache::showing) replays (ADR-0235).
pub(super) fn decode_gif(
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
    let source = (u32::from(decoder.width()), u32::from(decoder.height()));
    // The previous frame's disposal, applied before the next draws: its rect, and the pixels
    // `Previous` restores (`None` for `Background`, which clears to transparent).
    let mut pending: Option<(Rect, Option<Vec<u8>>)> = None;
    assemble(source, box_px, cropped, charge, budget, 1, |canvas| {
        // Pixels outside the next frame's rect that disposal changed; the delta has to carry them,
        // or a replay keeps showing what disposal cleared or restored.
        let disposed = pending.take().map(|(rect, saved)| {
            match saved {
                Some(saved) => write_rect(canvas, source.0, rect, &saved),
                None => clear_rect(canvas, source.0, rect),
            }
            rect
        });
        while let Some(frame) = decoder.read_next_frame().map_err(|err| err.to_string())? {
            let (left, top, w, h) =
                (u32::from(frame.left), u32::from(frame.top), u32::from(frame.width), u32::from(frame.height));
            if left + w > source.0 || top + h > source.1 {
                continue;
            }
            let rect = (left, top, w, h);
            // `Previous` disposal undoes this frame's draw once it has been shown; the pixels it
            // would overwrite have to be saved before that draw happens.
            let restore = (frame.dispose == gif::DisposalMethod::Previous).then(|| read_rect(canvas, source.0, rect));
            blend_rect(canvas, source.0, rect, &frame.buffer);
            pending = match frame.dispose {
                gif::DisposalMethod::Background => Some((rect, None)),
                gif::DisposalMethod::Previous => restore.map(|saved| (rect, Some(saved))),
                gif::DisposalMethod::Keep | gif::DisposalMethod::Any => None,
            };
            let changed = disposed.map_or(rect, |(x, y, w, h)| {
                let (x0, y0) = (left.min(x), top.min(y));
                (x0, y0, (left + rect.2).max(x + w) - x0, (top + rect.3).max(y + h) - y0)
            });
            return Ok(Some((changed, Duration::from_millis(u64::from(frame.delay) * 10))));
        }
        Ok(None)
    })
}

/// An animated WebP or APNG by `AnimationDecoder`, which hands over each frame already composited,
/// so a delta is the bounding box of what changed between two. `None` for a file that is not
/// animated, plays nothing (one frame, no frame that changes, a first delta past the budget), or
/// will not decode as one, so the caller draws it as a still, through the thumbnail, crop and blur
/// a still gets. A file that animates nothing is decoded twice; that is the price of not knowing
/// before the second frame. By content, not extension: most `.png` and many `.webp` files are
/// stills, and `Slot::Ready` is where the cache learns which paths animate ([`ImageCache::is_animated`](super::ImageCache::is_animated)).
pub(super) fn decode_frames(
    path: &Path,
    box_px: (u32, u32),
    cropped: bool,
    charge: Charge<'_>,
    budget: usize,
) -> Option<Decoded> {
    if !super::decode::may_animate(path) {
        return None;
    }
    let webp = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("webp"));
    refuse_irregular(path).ok()?;
    let file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let (source, mut frames) = if webp {
        let decoder = ::image::codecs::webp::WebPDecoder::new(file).ok()?;
        decoder.has_animation().then(|| (decoder.dimensions(), decoder.into_frames()))?
    } else {
        let decoder = ::image::codecs::png::PngDecoder::new(file).ok()?;
        if !decoder.is_apng().ok()? {
            return None;
        }
        (decoder.dimensions(), decoder.apng().ok()?.into_frames())
    };
    // Four canvases: ours, the frame in hand, and the decoder's own two.
    assemble(source, box_px, cropped, charge, budget, 4, |canvas| {
        let Some(frame) = frames.next() else { return Ok(None) };
        let frame = frame.map_err(|err| err.to_string())?;
        let delay = Duration::from(frame.delay());
        let next = frame.into_buffer().into_raw();
        let changed = changed_rect(canvas, &next, source.0);
        *canvas = next;
        Ok(Some((changed, delay)))
    })
    .inspect_err(|err| debug!("{}: drawn as a still: {err}", path.display()))
    .ok()
    .filter(|decoded| !decoded.deltas.is_empty())
}

/// Builds a [`Decoded`] from `next`, which draws the next frame onto the source-size RGBA8 canvas
/// and says which rect that changed and how long the frame shows, or `None` at the end.
///
/// Past [`super::thumbnails`], which holds one surface per path and would answer frame 0 forever. A box
/// that scales or crops loses the rects and keeps whole frames instead; `native` is that split.
/// `canvases` is how many source-size buffers `next` keeps alive, for the budget charge.
fn assemble(
    (source_width, source_height): (u32, u32),
    box_px: (u32, u32),
    cropped: bool,
    charge: Charge<'_>,
    budget: usize,
    canvases: u64,
    mut next: impl FnMut(&mut Vec<u8>) -> Result<Option<(Rect, Duration)>, String>,
) -> Result<Decoded, String> {
    if source_width.max(source_height) > MAX_DECODE_EDGE {
        return Err(format!("{source_width}x{source_height} is past the {MAX_DECODE_EDGE}px limit"));
    }
    let (stored_width, stored_height) = stored_size(source_width, source_height, box_px);
    let (width, height) =
        if cropped { (stored_width.min(box_px.0), stored_height.min(box_px.1)) } else { (stored_width, stored_height) };
    // Unscaled and uncropped only; see the doc comment above for the smaller-box fallback.
    let native = !cropped && (stored_width, stored_height) == (source_width, source_height);

    // The kept deltas' byte ceiling plus the canvases compositing runs on, which stay the source
    // size whatever the box asks for.
    let _permit = charge.take(budget as u64 + canvases * 4 * u64::from(source_width) * u64::from(source_height));

    let mut canvas = vec![0u8; source_width as usize * source_height as usize * 4];
    // Where a source rect lands in the stored frame: floor the origin and ceil the far edge, then
    // one more stored pixel each side, because `downscale`'s bilinear kernel reaches one target
    // pixel past the change (ADR-0317); then take off what the centred crop cut.
    let map_rect = |(left, top, w, h): (u32, u32, u32, u32)| {
        let start = |v: u32, from: u32, to: u32| (u64::from(v) * u64::from(to) / u64::from(from).max(1)) as u32;
        let end = |v: u32, from: u32, to: u32| (u64::from(v) * u64::from(to)).div_ceil(u64::from(from).max(1)) as u32;
        let (crop_x, crop_y) = ((stored_width - width) / 2, (stored_height - height) / 2);
        let x0 = start(left, source_width, stored_width).saturating_sub(1 + crop_x).min(width);
        let y0 = start(top, source_height, stored_height).saturating_sub(1 + crop_y).min(height);
        let x1 = (end(left + w, source_width, stored_width) + 1).saturating_sub(crop_x).min(width);
        let y1 = (end(top + h, source_height, stored_height) + 1).saturating_sub(crop_y).min(height);
        (x0, y0, x1 - x0, y1 - y0)
    };
    // The pixels for `rect` and where they land, both in the stored frame's coordinates. Scaling
    // the whole canvas and cutting the rect out of the result keeps resampling seam-free while what
    // is kept stays the size of the change; extracting from a scaled sub-rect would seam.
    let extract = |canvas: &[u8], rect: (u32, u32, u32, u32)| {
        if native {
            return Ok::<_, String>((read_rect(canvas, source_width, rect), rect));
        }
        // Crop from the borrowed canvas when the size is unchanged: a copy first would hold two.
        let frame = if (stored_width, stored_height) == (source_width, source_height) {
            let kept = if cropped { crop_to_box(canvas, stored_width, stored_height, box_px) } else { None };
            kept.map_or_else(|| canvas.to_vec(), |kept| kept.0)
        } else {
            let image = ::image::RgbaImage::from_raw(source_width, source_height, canvas.to_vec())
                .ok_or("canvas is not source_width * source_height * 4 bytes")?;
            let scaled =
                downscale(&::image::DynamicImage::ImageRgba8(image), (stored_width, stored_height))?.into_raw();
            let kept = if cropped { crop_to_box(&scaled, stored_width, stored_height, box_px) } else { None };
            kept.map_or(scaled, |kept| kept.0)
        };
        let mapped = map_rect(rect);
        Ok(if mapped == (0, 0, width, height) { (frame, mapped) } else { (read_rect(&frame, width, mapped), mapped) })
    };
    let whole = (0, 0, source_width, source_height);

    let mut base = None;
    let mut delays = Vec::new();
    let mut deltas: Vec<Delta> = Vec::new();
    let mut delta_bytes = 0usize;
    while let Some((changed, delay)) = next(&mut canvas)? {
        // A floor, because a 0 ms frame is one `frame_at` skips and an all-0 file is a still, which
        // much of the web's GIFs are. 20 ms, not the 100 ms browsers substitute: a frame here costs
        // a whole surface repaint, and 50 fps is already the ceiling that buys.
        let delay = delay.max(Duration::from_millis(20));
        if base.is_none() {
            base = Some(extract(&canvas, whole)?.0);
            delays.push(delay);
            continue;
        }
        // A frame that changes nothing keeps its delay and costs no pixels; scaling the canvas for it would be work for nothing.
        let (pixels, rect) =
            if changed.2 == 0 || changed.3 == 0 { (Vec::new(), (0, 0, 0, 0)) } else { extract(&canvas, changed)? };
        // At least a 64th of the canvas, because a frame costs a compare, and a scale off the native
        // path, over all of it however little changed; and at least what its delay and delta take to
        // hold, which is all a 1x1 canvas has to charge. Without a floor a crafted file of identical
        // frames never reaches the ceiling and holds a worker, and its memory, indefinitely.
        let cost = pixels.len().max(canvas.len() / 64).max(size_of::<Delta>() + size_of::<Duration>());
        if delta_bytes + cost > budget {
            break;
        }
        delta_bytes += cost;
        delays.push(delay);
        deltas.push(Delta { rect, pixels });
    }
    // Frames that change nothing leave a still, whatever their delays.
    if deltas.iter().all(|delta| delta.pixels.is_empty()) {
        deltas.clear();
        delays.truncate(1);
    }
    let Some(base) = base else { return Err("no frames".to_string()) };
    let natural = Some((source_width, source_height));
    Ok(Decoded { base, delays, deltas, width, height, natural, premultiplied: false, text: false })
}

/// The smallest rect holding every pixel where two same-size RGBA8 canvases differ; empty if none.
fn changed_rect(old: &[u8], new: &[u8], width: u32) -> Rect {
    let row = width as usize * 4;
    let (mut x0, mut x1, mut rows) = (u32::MAX, 0, None::<(u32, u32)>);
    for (y, (a, b)) in old.chunks_exact(row).zip(new.chunks_exact(row)).enumerate() {
        let Some(first) = a.iter().zip(b).position(|(p, q)| p != q) else { continue };
        let last = a.iter().zip(b).rposition(|(p, q)| p != q).unwrap_or(first);
        x0 = x0.min(first as u32 / 4);
        x1 = x1.max(last as u32 / 4 + 1);
        rows = Some((rows.map_or(y as u32, |(top, _)| top), y as u32 + 1));
    }
    rows.map_or((0, 0, 0, 0), |(top, bottom)| (x0, top, x1 - x0, bottom - top))
}

/// The byte index of pixel `(x, y)` in a `width`-wide RGBA8 buffer.
fn offset(width: u32, x: u32, y: u32) -> usize {
    (y as usize * width as usize + x as usize) * 4
}

/// Copies a `width`-wide RGBA8 buffer's rect out, row by row.
pub(super) fn read_rect(pixels: &[u8], width: u32, rect: (u32, u32, u32, u32)) -> Vec<u8> {
    let (left, top, w, h) = rect;
    let row = w as usize * 4;
    let mut out = Vec::with_capacity(row * h as usize);
    for y in 0..h {
        let start = offset(width, left, top + y);
        out.extend_from_slice(&pixels[start..start + row]);
    }
    out
}

/// The inverse of [`read_rect`]: overwrites a rect from `src`, for `DisposalMethod::Previous`.
fn write_rect(pixels: &mut [u8], width: u32, rect: (u32, u32, u32, u32), src: &[u8]) {
    let (left, top, w, h) = rect;
    let row = w as usize * 4;
    for y in 0..h {
        let start = offset(width, left, top + y);
        pixels[start..start + row].copy_from_slice(&src[y as usize * row..(y as usize + 1) * row]);
    }
}

/// Zeroes a rect to transparent, for `DisposalMethod::Background`. The file's own background
/// colour is not it: browsers ignore it and so does `image`'s own GIF decoder, "for web
/// compatibility", which is the behaviour a delta replay has to match.
fn clear_rect(pixels: &mut [u8], width: u32, rect: (u32, u32, u32, u32)) {
    let (left, top, w, h) = rect;
    for y in 0..h {
        let start = offset(width, left, top + y);
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
        let at = offset(width, left + i as u32 % w, top + i as u32 / w);
        pixels[at..at + 4].copy_from_slice(pixel);
    }
}

#[cfg(test)]
pub(in crate::image) mod tests {
    use std::path::Path;

    use gif::DisposalMethod::{Background, Keep, Previous};

    use super::super::decode::{crop_to_box, decode, downscale};
    use super::super::tests::key;
    use super::super::{CacheKey, Decoded, FileVersion, STARTING_TEXTURE_BUDGET};
    use super::*;

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

    pub(in crate::image) type Frame = ((u16, u16, u16, u16), [u8; 4], gif::DisposalMethod);

    /// A GIF of `size` holding `frames` (rect, colour, disposal), each shown 5 centiseconds.
    pub(in crate::image) fn gif_fixture(
        dir: &Path,
        name: &str,
        size: (u16, u16),
        frames: &[Frame],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        let mut encoder = gif::Encoder::new(&mut file, size.0, size.1, &[]).unwrap();
        for &(rect, rgba, dispose) in frames {
            write_frame(&mut encoder, rect, rgba, dispose, 5);
        }
        drop(encoder);
        path
    }

    /// ADR-0235. Every disposal method decides what the *next* frame's transparent pixels show
    /// through to, never the current one's own draw -- `Keep` carries a frame's pixels forward,
    /// `Background` clears its rect to transparent first, `Previous` undoes the draw entirely. A
    /// delta replay has to land on the same pixels an independent full recomposite gives at every
    /// frame, which is `image`'s own `AnimationDecoder` here, not this module.
    #[test]
    fn disposal_methods_replay_to_the_same_pixels_a_full_recomposite_gives() {
        let dir = tempfile::tempdir().unwrap();
        // Background's green square and Previous's blue one must not show once their frame is gone.
        let path = gif_fixture(
            dir.path(),
            "dispose.gif",
            (4, 4),
            &[
                ((0, 0, 4, 4), [255, 0, 0, 255], Keep),
                ((0, 0, 2, 2), [0, 255, 0, 255], Background),
                ((0, 0, 2, 2), [0, 0, 0, 0], Keep),
                ((0, 0, 2, 2), [0, 0, 255, 255], Previous),
                ((0, 0, 2, 2), [0, 0, 0, 0], Keep),
            ],
        );

        let decoded = decode_gif(&path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.deltas.len(), 4, "5 frames, the first is the base");
        assert!(decoded.deltas.iter().all(|delta| delta.rect == (0, 0, 2, 2)), "the native rect, not the full canvas");

        assert_replays_like_a_full_recomposite(&path, &decoded, |frame| frame.as_raw().clone());

        // A disposal outside the next frame's rect is still a change that frame's delta must carry.
        let moving = gif_fixture(
            dir.path(),
            "moving.gif",
            (4, 4),
            &[
                ((0, 0, 4, 4), [255, 0, 0, 255], Keep),
                ((0, 0, 2, 2), [0, 255, 0, 255], Background),
                ((2, 2, 2, 2), [0, 0, 255, 255], Previous),
                ((0, 2, 1, 1), [255, 255, 0, 255], Keep),
            ],
        );
        let decoded = decode_gif(&moving, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_replays_like_a_full_recomposite(&moving, &decoded, |frame| frame.as_raw().clone());
    }

    /// Replays `decoded`'s deltas over its base and compares every frame against `image`'s own
    /// `AnimationDecoder`, an independent full recomposite, put through `stored` for a scaled box.
    fn assert_replays_like_a_full_recomposite(
        path: &std::path::Path,
        decoded: &Decoded,
        stored: impl Fn(&::image::RgbaImage) -> Vec<u8>,
    ) {
        let width = decoded.width;
        let gif_decoder =
            ::image::codecs::gif::GifDecoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap())).unwrap();
        let ground_truth = ::image::AnimationDecoder::into_frames(gif_decoder).collect_frames().unwrap();
        assert_eq!(ground_truth.len(), decoded.deltas.len() + 1);
        for (index, frame) in ground_truth.iter().enumerate() {
            let mut replayed = decoded.base.clone();
            for delta in &decoded.deltas[..index] {
                write_rect(&mut replayed, width, delta.rect, &delta.pixels);
            }
            assert_eq!(replayed, stored(frame.buffer()), "frame {index} diverged from the full recomposite");
        }
    }

    /// ADR-0235. The frame that would push the kept deltas past their byte budget is dropped
    /// rather than collapsing the whole animation to a still: playback loops over what fit.
    #[test]
    fn a_delta_past_its_byte_budget_is_dropped_and_the_rest_still_play() {
        let dir = tempfile::tempdir().unwrap();
        let mut frames = vec![((0, 0, 4, 4), [255, 0, 0, 255], Keep)];
        frames.extend((0..4u8).map(|n| ((0, 0, 2, 2), [0, n * 50, 0, 255], Keep)));
        let path = gif_fixture(dir.path(), "many.gif", (4, 4), &frames);
        // Each 2x2 delta is 16 bytes, charged the 56 a frame takes to hold; 112 keeps exactly two.
        let decoded = decode_gif(&path, (4, 4), false, Charge::Free, 112).unwrap();
        assert_eq!(decoded.deltas.len(), 2, "a third would total 168 bytes, past the 112-byte budget");
        assert_eq!(decoded.delays.len(), 3, "the base plus the two kept deltas");
    }

    /// A wallpaper is scaled, cropped, or both, so it never takes the native path; keeping a whole
    /// stored frame per delta there cost a screenful each and clipped the loop at the byte budget.
    #[test]
    fn a_scaled_gif_keeps_the_changed_rect_rather_than_a_whole_stored_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = gif_fixture(
            dir.path(),
            "scaled.gif",
            (8, 8),
            &[((0, 0, 8, 8), [255, 0, 0, 255], Keep), ((4, 4, 4, 4), [0, 255, 0, 255], Keep)],
        );

        let decoded = decode_gif(&path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!((decoded.width, decoded.height), (4, 4), "8x8 halved into a 4x4 box");
        assert_eq!(decoded.base.len(), 4 * 4 * 4, "the base is still the whole stored frame");
        assert_eq!(decoded.deltas[0].rect, (1, 1, 3, 3), "the source rect halved, plus the kernel's reach");
        assert_eq!(decoded.deltas[0].pixels.len(), 3 * 3 * 4, "against 64 bytes for a whole stored frame");
    }

    /// ADR-0317. Bilinear reaches a stored pixel past a scaled change, so a delta cut to the
    /// change alone left a stale ring around it. Scaled and cropped, every replayed frame must be
    /// the full composite put through the same scale and crop.
    #[test]
    fn a_scaled_and_cropped_gif_replays_like_scaling_each_whole_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = gif_fixture(
            dir.path(),
            "moving.gif",
            (24, 16),
            &[
                ((0, 0, 24, 16), [255, 0, 0, 255], Keep),
                ((9, 5, 3, 3), [0, 255, 0, 255], Keep),
                ((0, 13, 5, 3), [0, 0, 255, 255], Background),
                ((17, 0, 1, 1), [255, 255, 0, 255], Keep),
            ],
        );
        // 24x16 covers a 10x10 box at 15x10, then crops to 10x10.
        let decoded = decode_gif(&path, (10, 10), true, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!((decoded.width, decoded.height), (10, 10));
        assert_eq!(decoded.deltas[0].rect, (2, 2, 5, 4), "the 3x3 change at (9, 5), scaled, grown and cropped");
        assert_replays_like_a_full_recomposite(&path, &decoded, |frame| {
            let scaled = downscale(&::image::DynamicImage::ImageRgba8(frame.clone()), (15, 10)).unwrap();
            crop_to_box(&scaled.into_raw(), 15, 10, (10, 10)).expect("15x10 overflows a 10x10 box").0
        });
    }

    /// A 10x7 source covering a 3x3 box stores at 5x3 (`stored_size` ceils both edges), which an
    /// aspect-keeping `fit_inside` would round down to 4x3 and the crop would then read past.
    #[test]
    fn a_gif_whose_cover_size_rounds_up_scales_to_exactly_that_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = gif_fixture(
            dir.path(),
            "wide.gif",
            (10, 7),
            &[((0, 0, 10, 7), [255, 0, 0, 255], Keep), ((6, 4, 4, 3), [0, 255, 0, 255], Keep)],
        );

        let decoded = decode_gif(&path, (3, 3), true, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!((decoded.width, decoded.height), (3, 3));
        assert_eq!(decoded.base.len(), 3 * 3 * 4);
        let (_, _, w, h) = decoded.deltas[0].rect;
        assert_eq!(decoded.deltas[0].pixels.len(), (w * h * 4) as usize);
    }

    /// Paints `rect` of a 4-wide RGBA frame one colour.
    fn paint(frame: &mut [u8], (x, y, w, h): (usize, usize, usize, usize), rgba: [u8; 4]) {
        for row in y..y + h {
            for column in x..x + w {
                frame[(row * 4 + column) * 4..][..4].copy_from_slice(&rgba);
            }
        }
    }

    /// 4x4 RGBA frames: red, then red with a green 2x2 at (1, 1), then that with a blue pixel at (3, 3).
    pub(in crate::image) fn frames() -> [Vec<u8>; 3] {
        let mut frame = [255, 0, 0, 255].repeat(16);
        let red = frame.clone();
        paint(&mut frame, (1, 1, 2, 2), [0, 255, 0, 255]);
        let green = frame.clone();
        paint(&mut frame, (3, 3, 1, 1), [0, 0, 255, 255]);
        [red, green, frame]
    }

    /// An animated WebP of 4x4 `frames`, each replacing the last, shown for `delays` ms.
    pub(in crate::image) fn webp_fixture(
        dir: &Path,
        name: &str,
        frames: &[Vec<u8>],
        delays: &[u32],
    ) -> std::path::PathBuf {
        fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let pad = data.len() % 2;
            [&tag[..], &(data.len() as u32).to_le_bytes(), data, &[0][..pad]].concat()
        }
        let mut body = b"WEBP".to_vec();
        // Animation and alpha flags, then the 4x4 canvas as `size - 1` in 24 bits.
        body.extend(chunk(b"VP8X", &[0x12, 0, 0, 0, 3, 0, 0, 3, 0, 0]));
        body.extend(chunk(b"ANIM", &[0; 6]));
        for (rgba, delay) in frames.iter().zip(delays) {
            let mut lossless = Vec::new();
            ::image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
                .encode(rgba, 4, 4, ::image::ExtendedColorType::Rgba8)
                .unwrap();
            // Position 0, 0, size 4x4, the delay, and "do not blend" so a frame replaces the last.
            let mut frame = vec![0, 0, 0, 0, 0, 0, 3, 0, 0, 3, 0, 0];
            frame.extend(&delay.to_le_bytes()[..3]);
            frame.push(0b10);
            frame.extend(&lossless[12..]);
            body.extend(chunk(b"ANMF", &frame));
        }
        let path = dir.join(name);
        std::fs::write(&path, [&b"RIFF"[..], &(body.len() as u32).to_le_bytes(), &body].concat()).unwrap();
        path
    }

    /// An APNG of 4x4 `frames`, shown for `delays` ms.
    pub(in crate::image) fn apng_fixture(
        dir: &Path,
        name: &str,
        frames: &[Vec<u8>],
        delays: &[u16],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut encoder = png::Encoder::new(std::fs::File::create(&path).unwrap(), 4, 4);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(delays.len() as u32, 0).unwrap();
        encoder.set_frame_delay(delays[0], 1000).unwrap();
        let mut writer = encoder.write_header().unwrap();
        for (rgba, delay) in frames.iter().zip(delays) {
            writer.set_frame_delay(*delay, 1000).unwrap();
            writer.write_image_data(rgba).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    /// The pixels frame `index` shows: the base with every delta up to it written over.
    fn replayed(decoded: &Decoded, index: usize) -> Vec<u8> {
        let mut pixels = decoded.base.clone();
        decoded.deltas[..index]
            .iter()
            .for_each(|delta| write_rect(&mut pixels, decoded.width, delta.rect, &delta.pixels));
        pixels
    }

    /// An animated WebP or APNG takes the GIF's shape: the base, then each later frame's changed
    /// rect only, replaying to the composited frames, and every delay at least the 20 ms floor.
    #[test]
    fn an_animated_webp_and_apng_decode_to_a_base_and_the_rects_that_changed() {
        let dir = tempfile::tempdir().unwrap();
        let webp = webp_fixture(dir.path(), "a.webp", &frames(), &[100, 0, 60]);
        let apng = apng_fixture(dir.path(), "a.png", &frames(), &[100, 0, 60]);
        for path in [webp, apng] {
            let decoded = decode_frames(&path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET)
                .unwrap_or_else(|| panic!("{} is animated", path.display()));
            let rects: Vec<_> = decoded.deltas.iter().map(|delta| delta.rect).collect();
            assert_eq!(rects, [(1, 1, 2, 2), (3, 3, 1, 1)], "{}", path.display());
            assert_eq!(decoded.delays, [100, 20, 60].map(Duration::from_millis), "{}", path.display());
            assert_eq!(decoded.natural, Some((4, 4)));
            for (index, frame) in frames().iter().enumerate() {
                assert_eq!(&replayed(&decoded, index), frame, "{} frame {index}", path.display());
            }
        }
    }

    /// An APNG's frames are sub-rects with their own disposal: what `Background` clears after a
    /// frame is a change the next frame's delta must carry, outside that frame's own rect.
    #[test]
    fn an_apng_with_sub_rect_frames_and_disposal_replays_to_each_composited_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub.png");
        let mut encoder = png::Encoder::new(std::fs::File::create(&path).unwrap(), 4, 4);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(3, 0).unwrap();
        encoder.set_frame_delay(5, 100).unwrap();
        let mut writer = encoder.write_header().unwrap();
        let red = frames()[0].clone();
        writer.write_image_data(&red).unwrap();
        // A green 2x2 at (1, 1), cleared once shown; then a blue 1x2 at (2, 0) over what it left.
        writer.set_frame_dimension(2, 2).unwrap();
        writer.set_frame_position(1, 1).unwrap();
        writer.set_dispose_op(png::DisposeOp::Background).unwrap();
        writer.write_image_data(&[0, 255, 0, 255].repeat(4)).unwrap();
        writer.set_frame_dimension(1, 2).unwrap();
        writer.set_frame_position(2, 0).unwrap();
        writer.set_dispose_op(png::DisposeOp::None).unwrap();
        writer.write_image_data(&[0, 0, 255, 255].repeat(2)).unwrap();
        writer.finish().unwrap();

        let (mut green, mut cleared) = (red.clone(), red.clone());
        paint(&mut green, (1, 1, 2, 2), [0, 255, 0, 255]);
        paint(&mut cleared, (1, 1, 2, 2), [0; 4]);
        paint(&mut cleared, (2, 0, 1, 2), [0, 0, 255, 255]);
        let decoded = decode_frames(&path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        let rects: Vec<_> = decoded.deltas.iter().map(|delta| delta.rect).collect();
        assert_eq!(rects, [(1, 1, 2, 2), (1, 0, 2, 3)], "the second covers the cleared square and its own rect");
        for (index, frame) in [red, green, cleared].iter().enumerate() {
            assert_eq!(&replayed(&decoded, index), frame, "frame {index}");
        }
    }

    /// Frames that change nothing keep their delay and cost a 64th of the canvas each, so a file
    /// of identical frames is cut off by the budget instead of being decoded to its last frame.
    #[test]
    fn identical_frames_are_cut_off_by_the_budget_rather_than_decoded_without_end() {
        let mut calls = 0;
        // A 64x64 canvas is 16 KiB, so a frame costs 256 bytes and 4 KiB admits sixteen.
        let decoded = assemble((64, 64), (64, 64), false, Charge::Free, 4096, 1, |_| {
            calls += 1;
            Ok((calls < 10_000).then_some(((0, 0, 0, 0), Duration::from_millis(50))))
        })
        .unwrap();
        assert_eq!(calls, 1 + 16 + 1, "the base, sixteen that fit, and the one that did not");
        assert!(decoded.deltas.is_empty() && decoded.delays.len() == 1, "nothing changed: a still");

        // A 1x1 canvas is 4 bytes, where a 64th is nothing: a frame still costs what its delay and
        // delta take to hold, or the lists grow with the file.
        let mut calls = 0;
        let held = size_of::<Delta>() + size_of::<Duration>();
        assemble((1, 1), (1, 1), false, Charge::Free, 10 * held, 1, |_| {
            calls += 1;
            Ok((calls < 10_000).then_some(((0, 0, 0, 0), Duration::from_millis(50))))
        })
        .unwrap();
        assert_eq!(calls, 1 + 10 + 1, "the base, ten that fit, and the one that did not");

        // With one that does change, the frames that do not still keep their delays, and an empty
        // rect on either axis is none, whatever the other says.
        let mut calls = 0;
        let decoded = assemble((64, 64), (64, 64), false, Charge::Free, 1 << 20, 1, |canvas| {
            calls += 1;
            canvas[0] = calls as u8;
            let changed = [(0, 0, 0, 0), (0, 0, 0, 5), (5, 0, 5, 0), (0, 0, 1, 1)].get(calls as usize - 1).copied();
            Ok(changed.map(|changed| (changed, Duration::from_millis(50))))
        })
        .unwrap();
        let rects: Vec<_> = decoded.deltas.iter().map(|delta| delta.rect).collect();
        assert_eq!((rects, decoded.delays.len()), (vec![(0, 0, 0, 0), (0, 0, 0, 0), (0, 0, 1, 1)], 4));
    }

    /// A scaled box replays to each whole frame put through the same scale, as a GIF does, and the
    /// byte budget drops the frames past it.
    #[test]
    fn an_animated_webp_scales_and_budgets_like_a_gif() {
        let dir = tempfile::tempdir().unwrap();
        let path = webp_fixture(dir.path(), "a.webp", &frames(), &[50, 50, 50]);
        let halved = decode_frames(&path, (2, 2), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!((halved.width, halved.height), (2, 2));
        for (index, frame) in frames().into_iter().enumerate() {
            let frame = ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_raw(4, 4, frame).unwrap());
            assert_eq!(replayed(&halved, index), downscale(&frame, (2, 2)).unwrap().into_raw(), "frame {index}");
        }
        // Each delta is charged the 56 bytes a frame takes to hold: that keeps the first and drops the second.
        let kept = decode_frames(&path, (4, 4), false, Charge::Free, 56).unwrap();
        assert_eq!((kept.deltas.len(), kept.delays.len()), (1, 2));
    }

    /// The pool is charged the delta ceiling plus every source-size canvas the source keeps alive,
    /// for as long as frames are being decoded.
    #[test]
    fn decoding_holds_the_ceiling_and_the_sources_canvases_in_the_budget() {
        let pool = super::super::budget::Budget::default();
        let seen = std::cell::Cell::new(0);
        let mut calls = 0;
        let decoded = assemble((4, 4), (4, 4), false, Charge::Immediate(&pool), 100, 3, |_| {
            seen.set(*pool.in_flight.lock().unwrap());
            calls += 1;
            Ok((calls == 1).then_some(((0, 0, 4, 4), Duration::ZERO)))
        })
        .unwrap();
        assert_eq!((seen.get(), *pool.in_flight.lock().unwrap()), (100 + 3 * 4 * 16, 0), "returned once done");
        assert_eq!(decoded.delays, [Duration::from_millis(20)], "a 0 ms frame is floored");
    }

    /// A GIF frame's delay is in centiseconds, and a frame the canvas cannot hold is skipped rather
    /// than drawn out of bounds. A later frame's delta covers the disposed rect as well as its own.
    #[test]
    fn a_gif_frame_keeps_its_delay_skips_one_past_the_canvas_and_unites_a_disposal_with_its_own_rect() {
        let dir = tempfile::tempdir().unwrap();
        let path = gif_fixture(
            dir.path(),
            "union.gif",
            (4, 4),
            &[
                ((0, 0, 4, 4), [255, 0, 0, 255], Keep),
                ((1, 0, 1, 2), [0, 255, 0, 255], Background),
                ((3, 1, 1, 2), [0, 0, 255, 255], Keep),
                ((1, 1, 2, 2), [0, 255, 0, 255], Background),
                ((0, 0, 1, 1), [255, 255, 0, 255], Keep),
                // Taller than what is left of the canvas below it, and nowhere wider than it.
                ((0, 3, 4, 2), [9, 9, 9, 255], Keep),
            ],
        );
        let decoded = decode_gif(&path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.delays, [Duration::from_millis(50); 5], "5 cs each, the out-of-canvas frame gone");
        assert_eq!(decoded.deltas[1].rect, (1, 0, 3, 3), "the cleared (1, 0, 1, 2) joined to (3, 1, 1, 2)");
        assert_eq!(decoded.deltas[3].rect, (0, 0, 3, 3), "the cleared (1, 1, 2, 2) joined to (0, 0, 1, 1)");
    }

    /// Two canvases that differ nowhere have no changed rect; otherwise it is the box around every difference.
    #[test]
    fn the_changed_rect_is_the_box_around_every_difference() {
        let [red, green, both] = frames();
        assert_eq!(changed_rect(&red, &red, 4), (0, 0, 0, 0));
        assert_eq!(changed_rect(&red, &green, 4), (1, 1, 2, 2));
        assert_eq!(changed_rect(&red, &both, 4), (1, 1, 3, 3));
        assert_eq!(changed_rect(&both, &red, 4), (1, 1, 3, 3));
    }

    /// `decode` hands a WebP or APNG to the animation reader ahead of the still path and keeps
    /// `source_blur` off what plays. A file that is not an animation is no business of
    /// `decode_frames`: it is a still, with thumbnails and `source_blur`, and so is one that
    /// animates nothing (one frame, frames that all match, a truncated file).
    #[test]
    fn decode_plays_an_animated_webp_and_apng_and_leaves_a_file_that_animates_nothing_to_the_raster_path() {
        let dir = tempfile::tempdir().unwrap();
        let [red, green, _] = frames();
        let apng = apng_fixture(dir.path(), "a.png", &frames(), &[50; 3]);
        let bytes = std::fs::read(&apng).unwrap();
        let truncated = dir.path().join("truncated.png");
        std::fs::write(&truncated, &bytes[..bytes.len() - 30]).unwrap();
        let still_png = dir.path().join("still.png");
        std::fs::write(&still_png, super::super::tests::PIL_2X2_RGBA_PNG).unwrap();
        let still_webp = dir.path().join("still.webp");
        let mut lossless = Vec::new();
        ::image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
            .encode(&red, 4, 4, ::image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&still_webp, lossless).unwrap();
        let same = [red.clone(), red.clone(), red.clone()];
        let animated = [
            webp_fixture(dir.path(), "a.webp", &frames(), &[50; 3]),
            apng_fixture(dir.path(), "a.apng", &frames(), &[50; 3]),
            apng,
        ];
        let stills = [
            still_png,
            still_webp,
            webp_fixture(dir.path(), "one.webp", std::slice::from_ref(&red), &[50]),
            apng_fixture(dir.path(), "one.png", &[green], &[50]),
            webp_fixture(dir.path(), "same.webp", &same, &[50; 3]),
            apng_fixture(dir.path(), "same.png", &same, &[50; 3]),
            truncated,
        ];
        let garbage = dir.path().join("garbage.webp");
        std::fs::write(&garbage, b"RIFF....WEBPVP8X not an animation").unwrap();
        let fonts = crate::text::shaping::FontDatabase::default();
        let run = |path: &Path, blur_px| {
            let key = CacheKey { blur_px, plays: true, ..key(path, 4, FileVersion::read(path)) };
            decode(&key, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET, &fonts).unwrap()
        };
        for path in &animated {
            let (sharp, blurred) = (run(path, 0), run(path, 50));
            assert_eq!(sharp.deltas.len(), 2, "{}", path.display());
            assert_eq!((blurred.base, blurred.premultiplied), (sharp.base, false), "{}", path.display());
        }
        for path in &stills {
            let blurred = run(path, 50);
            assert!(blurred.deltas.is_empty() && blurred.premultiplied, "{} is a still, so it blurs", path.display());
        }
        let others = [garbage, dir.path().join("missing.png"), dir.path().join("a.jpg")];
        for path in stills.iter().chain(&others) {
            assert!(
                decode_frames(path, (4, 4), false, Charge::Free, STARTING_TEXTURE_BUDGET).is_none(),
                "{}",
                path.display()
            );
        }
    }

    /// ADR-0240. `decode`'s call into `decode_gif` never passes `blur_px`, so this is a structural
    /// no-op rather than a checked one; this test is what would catch a future refactor wiring it
    /// through by accident.
    #[test]
    fn source_blur_is_ignored_on_an_animated_gif() {
        let dir = tempfile::tempdir().unwrap();
        let path = gif_fixture(
            dir.path(),
            "animated.gif",
            (4, 4),
            &[((0, 0, 4, 4), [255, 0, 0, 255], Keep), ((0, 0, 4, 4), [0, 255, 0, 255], Keep)],
        );
        let sharp = CacheKey { plays: true, ..key(&path, 4, FileVersion::read(&path)) };
        let blurred = CacheKey { blur_px: 50, ..sharp.clone() };

        let fonts = crate::text::shaping::FontDatabase::default();
        let a = decode(&sharp, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET, &fonts).unwrap();
        let b = decode(&blurred, None, None, Charge::Free, &|| true, STARTING_TEXTURE_BUDGET, &fonts).unwrap();
        assert_eq!(a.base, b.base, "an animated source keeps playing sharp regardless of source_blur");
        assert_eq!(a.deltas.len(), b.deltas.len());
        for (sharp, blurred) in a.deltas.iter().zip(&b.deltas) {
            assert_eq!(sharp.pixels, blurred.pixels);
        }
    }
}

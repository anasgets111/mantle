//! Animated image decoding and software frame compositing: GIF through `decode_gif` (ADR-0235), WebP
//! and APNG through `decode_frames` and `image`'s `AnimationDecoder`.

use std::path::Path;
use std::time::Duration;

use ::image::{AnimationDecoder, ImageDecoder};
use shared::debug;

use super::budget::Charge;
use super::decode::{crop_to_box, downscale_kept, refuse_irregular, stored_size};
use super::{Decoded, Delta, MAX_DECODE_EDGE};

type Rect = (u32, u32, u32, u32);

/// The base frame and the part of each later frame's rect it changed, composited here rather than by `image`'s
/// `AnimationDecoder`, which discards the rects [`ImageCache::showing`](super::ImageCache::showing) replays (ADR-0235).
pub(super) fn decode_gif(
    path: &Path,
    box_px: (u32, u32),
    cropped: bool,
    tiled: bool,
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
    assemble(source, box_px, cropped, tiled, charge, budget, 1, |canvas| {
        while let Some(frame) = decoder.read_next_frame().map_err(|err| err.to_string())? {
            let (left, top, w, h) =
                (u32::from(frame.left), u32::from(frame.top), u32::from(frame.width), u32::from(frame.height));
            if left + w > source.0 || top + h > source.1 {
                continue;
            }
            let rect = (left, top, w, h);
            // `Previous` restores before this frame draws and stays in the delta whole; `Background`
            // clears in `blend_rect` against the last shown frame so pixels redrawn alike drop out.
            let (disposed, cleared) = match pending.take() {
                Some((prev, Some(saved))) => {
                    write_rect(canvas, source.0, prev, &saved);
                    (Some(prev), None)
                }
                Some((bg, None)) => (None, Some(bg)),
                None => (None, None),
            };
            // `Previous` disposal undoes this frame's draw once it has been shown; the pixels it
            // would overwrite have to be saved before that draw happens, with any `Background` clear.
            let restore = (frame.dispose == gif::DisposalMethod::Previous).then(|| {
                let mut saved = read_rect(canvas, source.0, rect);
                if let Some(bg) = cleared
                    && let Some((ox, oy, ow, oh)) = intersect(rect, bg)
                {
                    clear_rect(&mut saved, rect.2, (ox - rect.0, oy - rect.1, ow, oh));
                }
                saved
            });
            let drawn = blend_rect(canvas, source.0, rect, &frame.buffer, cleared);
            pending = match frame.dispose {
                gif::DisposalMethod::Background => Some((rect, None)),
                // Restoring what the draw left equal to the post-clear canvas changes nothing.
                gif::DisposalMethod::Previous => {
                    restore.filter(|saved| rect_differs(canvas, source.0, rect, saved)).map(|saved| (rect, Some(saved)))
                }
                gif::DisposalMethod::Keep | gif::DisposalMethod::Any => None,
            };
            let changed = match disposed {
                Some((x, y, w, h)) if drawn.2 > 0 => {
                    let (x0, y0) = (drawn.0.min(x), drawn.1.min(y));
                    (x0, y0, (drawn.0 + drawn.2).max(x + w) - x0, (drawn.1 + drawn.3).max(y + h) - y0)
                }
                Some(disposed) => disposed,
                None => drawn,
            };
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
    tiled: bool,
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
    assemble(source, box_px, cropped, tiled, charge, budget, 4, |canvas| {
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
#[allow(clippy::too_many_arguments)] // `cropped` and `tiled` are the key's own fields, not a shape to invent.
fn assemble(
    (source_width, source_height): (u32, u32),
    box_px: (u32, u32),
    cropped: bool,
    tiled: bool,
    charge: Charge<'_>,
    budget: usize,
    canvases: u64,
    mut next: impl FnMut(&mut Vec<u8>) -> Result<Option<(Rect, Duration)>, String>,
) -> Result<Decoded, String> {
    if source_width.max(source_height) > MAX_DECODE_EDGE {
        return Err(format!("{source_width}x{source_height} is past the {MAX_DECODE_EDGE}px limit"));
    }
    let (stored_width, stored_height) = stored_size(source_width, source_height, box_px, tiled);
    let (width, height) =
        if cropped { (stored_width.min(box_px.0), stored_height.min(box_px.1)) } else { (stored_width, stored_height) };
    // Unscaled and uncut only, as a cover box the source fits is; see the doc comment above for the smaller-box fallback.
    let native = (width, height) == (stored_width, stored_height) && (width, height) == (source_width, source_height);

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
            let image = ::image::DynamicImage::ImageRgba8(image);
            downscale_kept(&image, (stored_width, stored_height), (width, height))?.into_raw()
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
        // Resampling leaves the palette, so only a native delta can be indexed.
        let delta = if native { Delta::indexed(rect, pixels) } else { Delta { rect, pixels, palette: Vec::new() } };
        // At least a 64th of the canvas, because a frame costs a compare, and a scale off the native
        // path, over all of it however little changed; and at least what its delay and delta take to
        // hold, which is all a 1x1 canvas has to charge. Without a floor a crafted file of identical
        // frames never reaches the ceiling and holds a worker, and its memory, indefinitely.
        let cost = delta.bytes().max(canvas.len() / 64).max(size_of::<Delta>() + size_of::<Duration>());
        if delta_bytes + cost > budget {
            break;
        }
        delta_bytes += cost;
        delays.push(delay);
        deltas.push(delta);
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

impl Delta {
    /// `pixels` as an index into their own colours, when there are at most 256 and that is smaller,
    /// as in most native GIF deltas; one cut from the canvas can hold several frames' palettes.
    fn indexed(rect: Rect, pixels: Vec<u8>) -> Self {
        let (mut palette, mut indices) = (Vec::new(), Vec::with_capacity(pixels.len() / 4));
        // Open addressing over four times the most colours kept, so a probe rarely passes one slot.
        let mut slots = [None::<(u32, u8)>; 1024];
        for pixel in pixels.as_chunks::<4>().0 {
            let colour = u32::from_ne_bytes(*pixel);
            let mut at = (colour.wrapping_mul(0x9e37_79b9) >> 22) as usize;
            while slots[at].is_some_and(|(held, _)| held != colour) {
                at = (at + 1) % slots.len();
            }
            if slots[at].is_none() {
                let Ok(index) = u8::try_from(palette.len()) else { break };
                slots[at] = Some((colour, index));
                palette.push(*pixel);
            }
            indices.extend(slots[at].map(|(_, index)| index));
        }
        if 4 * indices.len() == pixels.len() {
            palette.shrink_to_fit();
            let delta = Self { rect, pixels: indices, palette };
            if delta.bytes() < pixels.len() {
                return delta;
            }
        }
        Self { rect, pixels, palette: Vec::new() }
    }

    /// Host bytes held, as the decode and texture budgets charge them.
    pub(super) fn bytes(&self) -> usize {
        self.pixels.len() + 4 * self.palette.len()
    }

    /// Premultiplies the colours: the palette's, when there is one, expands to the premultiplied pixels.
    pub(super) fn premultiply(&mut self) {
        super::decode::premultiply(if self.palette.is_empty() {
            &mut self.pixels
        } else {
            self.palette.as_flattened_mut()
        });
    }

    /// The RGBA8 pixels, expanded from the palette if there is one.
    pub(super) fn rgba(&self) -> std::borrow::Cow<'_, [u8]> {
        if self.palette.is_empty() {
            return std::borrow::Cow::Borrowed(&self.pixels);
        }
        self.pixels.iter().flat_map(|&index| self.palette[usize::from(index)]).collect()
    }
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

/// Whether `saved` (packed `rect.2`x`rect.3`) differs from `rect` in `pixels` (stride `width`).
fn rect_differs(pixels: &[u8], width: u32, (left, top, w, h): Rect, saved: &[u8]) -> bool {
    let row = w as usize * 4;
    (0..h).any(|y| {
        let start = offset(width, left, top + y);
        pixels[start..start + row] != saved[y as usize * row..(y as usize + 1) * row]
    })
}

/// Overlap of two rects, or `None` when disjoint or empty.
fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x0, y0) = (a.0.max(b.0), a.1.max(b.1));
    let (x1, y1) = ((a.0 + a.2).min(b.0 + b.2), (a.1 + a.3).min(b.1 + b.3));
    (x0 < x1 && y0 < y1).then(|| (x0, y0, x1 - x0, y1 - y0))
}

/// Blends one row span of `src` onto `row`, updating `span` with the first and last changed `x`.
/// With `CLEARED`, a transparent source pixel zeroes the canvas pixel instead of keeping it.
fn blend_span<const CLEARED: bool>(row: &mut [[u8; 4]], src: &[[u8; 4]], start_x: u32, span: &mut (u32, u32)) {
    for (x, (pixel, drawn)) in (start_x..).zip(row.iter_mut().zip(src)) {
        let next = if drawn[3] != 0 {
            *drawn
        } else if CLEARED {
            [0; 4]
        } else {
            continue;
        };
        if *pixel != next {
            *pixel = next;
            *span = (span.0.min(x), x);
        }
    }
}

/// Draws `src` (a frame's own buffer, `rect`'s size) onto a rect, skipping a transparent source
/// pixel so whatever the canvas already holds shows through, or clearing it to transparent where
/// `cleared` (`DisposalMethod::Background` from the previous frame) covers it. GIF transparency is
/// a 1-bit mask, never partial, so "skip" (or "clear") is the whole rule.
///
/// Returns the smallest rect holding every pixel that changed from the canvas before `cleared` and
/// `src` ran, empty if none: an encoder without frame optimisation (or one that marks full-size
/// frames `Background`, like `image`'s) would otherwise hold the whole canvas for a moving corner.
fn blend_rect(pixels: &mut [u8], width: u32, rect: Rect, src: &[u8], cleared: Option<Rect>) -> Rect {
    let (left, top, w, h) = rect;
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    let mut record = |y: u32, (first, last): (u32, u32)| {
        if first != u32::MAX {
            (x0, y0, x1, y1) = (x0.min(first), y0.min(y), x1.max(last + 1), y1.max(y + 1));
        }
    };
    let mut drawn_rows = 0u32;
    if w > 0 {
        for (y, src_row) in (top..top + h).zip(src.chunks_exact(w as usize * 4)) {
            drawn_rows += 1;
            let start = offset(width, left, y);
            let row = pixels[start..start + src_row.len()].as_chunks_mut::<4>().0;
            let src_px = src_row.as_chunks::<4>().0;
            let (c0, c1) = cleared.filter(|&(_, by, _, bh)| y >= by && y < by + bh).map_or((0, 0), |(bx, _, bw, _)| {
                ((bx.clamp(left, left + w) - left) as usize, ((bx + bw).clamp(left, left + w) - left) as usize)
            });
            let mut span = (u32::MAX, 0);
            blend_span::<false>(&mut row[..c0], &src_px[..c0], left, &mut span);
            blend_span::<true>(&mut row[c0..c1], &src_px[c0..c1], left + c0 as u32, &mut span);
            blend_span::<false>(&mut row[c1..], &src_px[c1..], left + c1 as u32, &mut span);
            record(y, span);
        }
    }
    if let Some((bx, by, bw, bh)) = cleared {
        let drawn_bottom = top + drawn_rows;
        for y in by..by + bh {
            let spans = if y < top || y >= drawn_bottom {
                [(bx, bx + bw), (0, 0)]
            } else {
                [(bx, left.clamp(bx, bx + bw)), ((left + w).clamp(bx, bx + bw), bx + bw)]
            };
            let mut span = (u32::MAX, 0);
            for (sx, ex) in spans {
                let start = offset(width, sx, y);
                let row = pixels[start..start + (ex - sx) as usize * 4].as_chunks_mut::<4>().0;
                for (x, pixel) in (sx..).zip(row.iter_mut()) {
                    if *pixel != [0; 4] {
                        *pixel = [0; 4];
                        span = (span.0.min(x), x);
                    }
                }
            }
            record(y, span);
        }
    }
    if x1 == 0 { (0, 0, 0, 0) } else { (x0, y0, x1 - x0, y1 - y0) }
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

        let decoded = decode_gif(&path, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.deltas.len(), 4, "5 frames, the first is the base");
        assert!(decoded.deltas.iter().all(|delta| delta.rect == (0, 0, 2, 2)), "the native rect, not the full canvas");
        assert!(decoded.deltas.iter().all(|delta| delta.pixels.len() == 4), "a native delta holds one byte a pixel");

        assert_replays_like_a_full_recomposite(&path, &decoded, |frame| frame.as_raw().clone());
        let covered = decode_gif(&path, (4, 4), true, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert!(covered.deltas.iter().all(|delta| delta.pixels.len() == 4), "a cover box the source fits is native");
        assert_replays_like_a_full_recomposite(&path, &covered, |frame| frame.as_raw().clone());
        let cut = decode_gif(&path, (4, 2), true, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_replays_like_a_full_recomposite(&path, &cut, |frame| {
            crop_to_box(frame.as_raw(), 4, 4, (4, 2)).unwrap().0
        });

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
        let decoded = decode_gif(&moving, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
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
                write_rect(&mut replayed, width, delta.rect, &delta.rgba());
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
        // Each 2x2 delta is 16 bytes, charged the 80 a frame takes to hold; 160 keeps exactly two.
        let decoded = decode_gif(&path, (4, 4), false, false, Charge::Free, 160).unwrap();
        assert_eq!(decoded.deltas.len(), 2, "a third would total 240 bytes, past the 160-byte budget");
        assert_eq!(decoded.delays.len(), 3, "the base plus the two kept deltas");
        // A native delta is charged indexed: 256 indices and one colour for a 16x16 frame of one colour.
        let frames = [((0, 0, 16, 16), [255, 0, 0, 255], Keep), ((0, 0, 16, 16), [0, 255, 0, 255], Keep)];
        let path = gif_fixture(dir.path(), "indexed.gif", (16, 16), &frames);
        assert_eq!(decode_gif(&path, (16, 16), false, false, Charge::Free, 260).unwrap().deltas.len(), 1);
    }

    /// Indexing must not lose a colour: past 256, or where the palette costs more than it saves, a
    /// delta stays RGBA.
    #[test]
    fn a_delta_with_more_than_256_colours_stays_rgba() {
        // Scrambled, so that some colours share a slot and the probe runs.
        let colour = |n: u32| (n.wrapping_mul(0x0100_0193) ^ 0x811c_9dc5).to_le_bytes();
        let colours =
            |count: u32| (0..count).flat_map(|n| [&colour(n)[..3], &[255]].concat()).collect::<Vec<u8>>().repeat(2);
        let fits = Delta::indexed((0, 0, 512, 1), colours(256));
        assert_eq!((fits.pixels.len(), fits.palette.len()), (512, 256));
        assert_eq!(fits.rgba(), colours(256));
        let over = Delta::indexed((0, 0, 514, 1), colours(257));
        assert!(over.palette.is_empty());
        assert_eq!(over.pixels, colours(257));
        let even = [&colours(3)[..12], &colours(3)[..4]].concat();
        assert!(Delta::indexed((0, 0, 4, 1), even).palette.is_empty(), "4 indices and 3 colours save nothing");
        let one = Delta::indexed((0, 0, 2, 1), colours(1));
        assert_eq!(one.palette.capacity(), 1, "charged what it holds");
    }

    /// Premultiplying a palette has to give what premultiplying its expansion gives: alpha 0,
    /// translucent, and two colours that premultiply alike.
    #[test]
    fn a_premultiplied_delta_expands_to_the_premultiplied_pixels() {
        let pixels = [[200, 100, 50, 128], [10, 20, 30, 0], [101, 0, 0, 1], [100, 0, 0, 1], [9, 9, 9, 255]].repeat(2);
        let indexed = Delta::indexed((0, 0, 10, 1), pixels.concat());
        assert!(!indexed.palette.is_empty());
        for mut delta in [indexed, Delta { rect: (0, 0, 10, 1), pixels: pixels.concat(), palette: Vec::new() }] {
            let mut expected = delta.rgba().into_owned();
            super::super::decode::premultiply(&mut expected);
            delta.premultiply();
            assert_eq!(delta.rgba(), expected);
        }
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

        let decoded = decode_gif(&path, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
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
        let decoded = decode_gif(&path, (10, 10), true, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
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

        let decoded = decode_gif(&path, (3, 3), true, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
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
            .for_each(|delta| write_rect(&mut pixels, decoded.width, delta.rect, &delta.rgba()));
        pixels
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
        let decoded = decode_frames(&path, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
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
        let decoded = assemble((64, 64), (64, 64), false, false, Charge::Free, 4096, 1, |_| {
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
        assemble((1, 1), (1, 1), false, false, Charge::Free, 10 * held, 1, |_| {
            calls += 1;
            Ok((calls < 10_000).then_some(((0, 0, 0, 0), Duration::from_millis(50))))
        })
        .unwrap();
        assert_eq!(calls, 1 + 10 + 1, "the base, ten that fit, and the one that did not");

        // With one that does change, the frames that do not still keep their delays, and an empty
        // rect on either axis is none, whatever the other says.
        let mut calls = 0;
        let decoded = assemble((64, 64), (64, 64), false, false, Charge::Free, 1 << 20, 1, |canvas| {
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
        let halved = decode_frames(&path, (2, 2), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!((halved.width, halved.height), (2, 2));
        for (index, frame) in frames().into_iter().enumerate() {
            let frame = ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_raw(4, 4, frame).unwrap());
            assert_eq!(replayed(&halved, index), downscale(&frame, (2, 2)).unwrap().into_raw(), "frame {index}");
        }
        // Each delta is charged the 80 bytes a frame takes to hold: that keeps the first and drops the second.
        let kept = decode_frames(&path, (4, 4), false, false, Charge::Free, 80).unwrap();
        assert_eq!((kept.deltas.len(), kept.delays.len()), (1, 2));
    }

    /// The pool is charged the delta ceiling plus every source-size canvas the source keeps alive,
    /// for as long as frames are being decoded.
    #[test]
    fn decoding_holds_the_ceiling_and_the_sources_canvases_in_the_budget() {
        let pool = super::super::budget::Budget::default();
        let seen = std::cell::Cell::new(0);
        let mut calls = 0;
        let decoded = assemble((4, 4), (4, 4), false, false, Charge::Immediate(&pool), 100, 3, |_| {
            seen.set(*pool.in_flight.lock().unwrap());
            calls += 1;
            Ok((calls == 1).then_some(((0, 0, 4, 4), Duration::ZERO)))
        })
        .unwrap();
        assert_eq!((seen.get(), *pool.in_flight.lock().unwrap()), (100 + 3 * 4 * 16, 0), "returned once done");
        assert_eq!(decoded.delays, [Duration::from_millis(20)], "a 0 ms frame is floored");
    }

    /// An encoder without frame optimisation writes every frame canvas-sized; each delta keeps
    /// only the pixels its frame changed, joined to what the last frame's disposal changed.
    #[test]
    fn a_canvas_sized_gif_frame_keeps_only_the_pixels_it_changed() {
        let dir = tempfile::tempdir().unwrap();
        let red = ((0, 0, 4, 4), [255, 0, 0, 255], Keep);
        let frames = [
            red,
            ((1, 2, 1, 1), [0, 0, 255, 255], Background),
            // Draws red over red: only the disposal's cleared pixel changes.
            ((3, 3, 1, 1), [255, 0, 0, 255], Keep),
            red,
            red,
        ];
        let path = gif_fixture(dir.path(), "full.gif", (4, 4), &frames);
        let decoded = decode_gif(&path, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        let rects: Vec<_> = decoded.deltas.iter().map(|delta| delta.rect).collect();
        let (pixel, none) = ((1, 2, 1, 1), (0, 0, 0, 0));
        assert_eq!(rects, [pixel, pixel, pixel, none], "a redraw restores one pixel, then changes none");
        assert_replays_like_a_full_recomposite(&path, &decoded, |frame| frame.as_raw().clone());

        let previous = ((0, 0, 4, 4), [255, 0, 0, 255], Previous);
        let path = gif_fixture(dir.path(), "previous.gif", (4, 4), &[red, previous, red]);
        let decoded = decode_gif(&path, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert!(decoded.deltas.is_empty(), "red restored over red changes nothing: a still");
    }

    /// `Background` disposal clears its rect before the next frame draws, then diffs the result
    /// against the last shown frame: a full-size `Background` clip (such as `image`'s GIF encoder
    /// writes) keeps only what changed, and partial overlaps and `Background` -> `Previous` chains
    /// still replay identically to a full recomposite.
    #[test]
    fn background_disposal_diffs_against_the_last_shown_frame() {
        let dir = tempfile::tempdir().unwrap();

        // Full-size frames written by `image`'s GIF encoder (which marks every frame `Background`),
        // with a 2x2 square moving over a solid background: each delta keeps only the moved band.
        let encoded_path = dir.path().join("image_encoder.gif");
        {
            let mut encoder = ::image::codecs::gif::GifEncoder::new(std::fs::File::create(&encoded_path).unwrap());
            for step in 0..3u32 {
                let mut img = ::image::RgbaImage::from_pixel(8, 8, ::image::Rgba([20, 40, 60, 255]));
                for y in 2..4 {
                    for x in (1 + step)..(3 + step) {
                        img.put_pixel(x, y, ::image::Rgba([255, 200, 0, 255]));
                    }
                }
                let delay = ::image::Delay::from_numer_denom_ms(50, 1);
                encoder.encode_frame(::image::Frame::from_parts(img, 0, 0, delay)).unwrap();
            }
        }
        let decoded = decode_gif(&encoded_path, (8, 8), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        let rects: Vec<_> = decoded.deltas.iter().map(|delta| delta.rect).collect();
        assert_eq!(rects, [(1, 2, 3, 2), (2, 2, 3, 2)], "only the square's old and new positions");
        assert_replays_like_a_full_recomposite(&encoded_path, &decoded, |frame| frame.as_raw().clone());

        // Full-size `Background` frames that redraw the same pixels collapse to a still.
        let red_bg = ((0, 0, 4, 4), [255, 0, 0, 255], Background);
        let still = gif_fixture(dir.path(), "bg_still.gif", (4, 4), &[red_bg, red_bg, red_bg]);
        assert!(
            decode_gif(&still, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap().deltas.is_empty()
        );

        // Exhaustive oracle over all 3x3 (cleared, drawn) rect pairs: pixels and exact bounds
        // must match an independent clear-then-draw pass.
        let r = [255, 0, 0, 255];
        let g = [0, 255, 0, 255];
        let b = [0, 0, 255, 255];
        let t = [0, 0, 0, 0];
        let all_rects: Vec<Rect> = (0..=3)
            .flat_map(|x| {
                (0..=3).flat_map(move |y| (0..=3 - x).flat_map(move |w| (0..=3 - y).map(move |h| (x, y, w, h))))
            })
            .collect();
        let initial = [r, r, t, r, t, g, t, g, r].as_flattened().to_vec();
        for &bg in &all_rects {
            for &draw in &all_rects {
                let (x, y, w, h) = draw;
                let src: Vec<u8> = (0..w * h).flat_map(|i| [r, t, g][i as usize % 3]).collect();
                let mut expected = initial.clone();
                clear_rect(&mut expected, 3, bg);
                for dy in 0..h {
                    for dx in 0..w {
                        let px = &src[offset(w, dx, dy)..][..4];
                        if px[3] != 0 {
                            expected[offset(3, x + dx, y + dy)..][..4].copy_from_slice(px);
                        }
                    }
                }
                let mut actual = initial.clone();
                let bounds = blend_rect(&mut actual, 3, draw, &src, Some(bg));
                assert_eq!(actual, expected, "bg {bg:?} draw {draw:?}");
                assert_eq!(bounds, changed_rect(&initial, &expected, 3), "bg {bg:?} draw {draw:?}");
            }
        }

        // Tighter-than-cleared delta: clearing (0,0,2,2) where its other three pixels are already
        // zero and redrawing (0,0) blue must yield a 1x1 delta, not the full 2x2 cleared rect.
        let tighter = dir.path().join("tighter.gif");
        {
            let mut file = std::fs::File::create(&tighter).unwrap();
            let mut encoder = gif::Encoder::new(&mut file, 4, 4, &[]).unwrap();
            let mut write = |left, top, w, h, mut buf: Vec<u8>, dispose| {
                let mut frame = gif::Frame::from_rgba(w, h, &mut buf);
                (frame.left, frame.top, frame.dispose, frame.delay) = (left, top, dispose, 5);
                encoder.write_frame(&frame).unwrap();
            };
            write(0, 0, 1, 1, r.to_vec(), Keep);
            write(0, 0, 2, 2, [g, t, t, t].concat(), Background);
            write(0, 0, 1, 1, b.to_vec(), Keep);
        }
        let decoded = decode_gif(&tighter, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.deltas[1].rect, (0, 0, 1, 1), "already-zero pixels in cleared rect stay out of delta");
        assert_replays_like_a_full_recomposite(&tighter, &decoded, |frame| frame.as_raw().clone());

        // `intersect` rejects strictly disjoint, touching, and one-axis-only overlaps without overflow.
        assert_eq!(intersect((0, 0, 2, 1), (3, 0, 2, 1)), None);
        assert_eq!(intersect((0, 0, 1, 2), (0, 3, 1, 2)), None);
        assert_eq!(intersect((0, 0, 2, 2), (2, 0, 2, 2)), None);
        assert_eq!(intersect((0, 0, 2, 2), (0, 2, 2, 2)), None);
        assert_eq!(intersect((0, 0, 2, 2), (1, 2, 2, 2)), None);
        assert_eq!(intersect((0, 0, 2, 2), (2, 1, 2, 2)), None);
        assert_eq!(intersect((1, 1, 3, 3), (0, 0, 2, 2)), Some((1, 1, 1, 1)));

        // Strictly disjoint `Background` then `Previous` (columns 0..2 then 3..5) must not panic.
        let disjoint = gif_fixture(
            dir.path(),
            "disjoint_bg_prev.gif",
            (5, 1),
            &[((0, 0, 2, 1), r, Background), ((3, 0, 2, 1), b, Previous), ((0, 0, 5, 1), t, Keep)],
        );
        let decoded = decode_gif(&disjoint, (5, 1), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.deltas.len(), 2);
        assert_replays_like_a_full_recomposite(&disjoint, &decoded, |frame| frame.as_raw().clone());

        // A fully transparent full-canvas `Previous` after `Background` changes nothing over the
        // post-clear canvas, so its restore is dropped and the next small draw fits the budget.
        let noop_restore = gif_fixture(
            dir.path(),
            "noop_restore.gif",
            (16, 16),
            &[
                ((0, 0, 16, 16), r, Keep),
                ((0, 0, 2, 2), g, Background),
                ((0, 0, 16, 16), t, Previous),
                ((0, 0, 1, 1), b, Keep),
            ],
        );
        let budget = 3 * (size_of::<Delta>() + size_of::<Duration>());
        let decoded = decode_gif(&noop_restore, (16, 16), false, false, Charge::Free, budget).unwrap();
        assert_eq!(decoded.deltas.len(), 3, "the final 1x1 draw fits once the no-op restore is dropped");
        assert_eq!(decoded.deltas[2].rect, (0, 0, 1, 1));
        assert_replays_like_a_full_recomposite(&noop_restore, &decoded, |frame| frame.as_raw().clone());

        // `Background` followed by `Previous`: `Previous` must save and restore the post-clear
        // canvas even when the `Previous` frame redraws the same pixels as the `Background` frame.
        let bg_then_prev = gif_fixture(
            dir.path(),
            "bg_prev.gif",
            (4, 4),
            &[
                ((0, 0, 4, 4), r, Background),
                // Redraws the exact same red, so delta vs frame 0 is empty; `Previous` still restores clear!
                ((0, 0, 4, 4), r, Previous),
                ((0, 0, 4, 4), t, Keep),
            ],
        );
        let decoded = decode_gif(&bg_then_prev, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.deltas[0].rect, (0, 0, 0, 0), "frame 1 matches frame 0");
        assert_replays_like_a_full_recomposite(&bg_then_prev, &decoded, |frame| frame.as_raw().clone());

        // Partial overlap between `Background` and a following `Previous`: `Previous`'s restore
        // clears only the overlap with `Background` and keeps the pixels outside it; changing only
        // a lower row of `Previous` still arms the restore.
        let partial_bg_prev = gif_fixture(
            dir.path(),
            "partial_bg_prev.gif",
            (4, 4),
            &[
                ((0, 0, 4, 4), r, Keep),
                ((0, 0, 2, 2), g, Background),
                ((1, 1, 3, 3), b, Previous),
                ((0, 0, 4, 4), t, Keep),
            ],
        );
        let decoded =
            decode_gif(&partial_bg_prev, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_replays_like_a_full_recomposite(&partial_bg_prev, &decoded, |frame| frame.as_raw().clone());

        // `rect_differs` inspects non-zero rows and offsets accurately.
        let mut grid = r.repeat(16);
        let saved = read_rect(&grid, 4, (1, 1, 2, 2));
        assert!(!rect_differs(&grid, 4, (1, 1, 2, 2), &saved));
        grid[offset(4, 2, 2)..][..4].copy_from_slice(&b);
        assert!(rect_differs(&grid, 4, (1, 1, 2, 2), &saved));
    }

    /// A GIF frame's delay is in centiseconds, and a frame the canvas cannot hold is skipped rather
    /// than drawn out of bounds. A later frame's delta is the bounding box of what disposal and
    /// draw changed.
    #[test]
    fn a_gif_frame_keeps_its_delay_skips_one_past_the_canvas_and_covers_what_disposal_and_draw_changed() {
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
        let decoded = decode_gif(&path, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).unwrap();
        assert_eq!(decoded.delays, [Duration::from_millis(50); 5], "5 cs each, the out-of-canvas frame gone");
        assert_eq!(decoded.deltas[1].rect, (1, 0, 3, 3), "the cleared (1, 0, 1, 2) joined to (3, 1, 1, 2)");
        assert_eq!(decoded.deltas[3].rect, (0, 0, 3, 3), "the cleared (1, 1, 2, 2) joined to (0, 0, 1, 1)");
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
                decode_frames(path, (4, 4), false, false, Charge::Free, STARTING_TEXTURE_BUDGET).is_none(),
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

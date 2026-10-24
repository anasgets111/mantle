//! Animated GIF decoding and software frame compositing (ADR-0235).

use std::path::Path;
use std::time::Duration;

use super::budget::Charge;
use super::decode::{crop_to_box, downscale, refuse_irregular, stored_size};
use super::{Decoded, GifDelta, MAX_DECODE_EDGE};

/// The base frame and each later frame's own rect, composited here rather than by `image`'s
/// `AnimationDecoder`, which discards the rects [`ImageCache::showing`](super::ImageCache::showing) replays (ADR-0235).
///
/// Past [`super::thumbnails`], which holds one surface per path and would answer frame 0 forever. A box
/// that scales or crops loses the rects and keeps whole frames instead; `native` is that split.
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
    let mut deltas: Vec<GifDelta> = Vec::new();
    let mut delta_bytes = 0usize;
    // The previous frame's disposal changed pixels outside this frame's rect; the delta has to
    // carry them, or a replay keeps showing what disposal cleared or restored.
    let mut disposed: Option<(u32, u32, u32, u32)> = None;
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
            base = Some(extract(&canvas, whole)?.0);
            delays.push(delay);
        } else {
            let changed = disposed.map_or(rect, |(x, y, w, h)| {
                let (x0, y0) = (left.min(x), top.min(y));
                (x0, y0, (left + rect.2).max(x + w) - x0, (top + rect.3).max(y + h) - y0)
            });
            let (pixels, rect) = extract(&canvas, changed)?;
            if delta_bytes + pixels.len() > budget {
                break;
            }
            delta_bytes += pixels.len();
            delays.push(delay);
            deltas.push(GifDelta { rect, pixels });
        }

        disposed = match frame.dispose {
            gif::DisposalMethod::Background => {
                clear_rect(&mut canvas, source_width, rect);
                Some(rect)
            }
            gif::DisposalMethod::Previous => {
                if let Some(ref saved) = restore {
                    write_rect(&mut canvas, source_width, rect, saved);
                }
                Some(rect)
            }
            gif::DisposalMethod::Keep | gif::DisposalMethod::Any => None,
        };
    }
    let Some(base) = base else { return Err("no frames".to_string()) };
    let natural = Some((source_width, source_height));
    Ok(Decoded { base, delays, deltas, width, height, natural, premultiplied: false, text: false })
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
        let sharp = key(&path, 4, FileVersion::read(&path));
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

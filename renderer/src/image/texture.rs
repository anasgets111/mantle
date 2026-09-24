use std::path::Path;
use std::time::{Duration, Instant};

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ErrorKind, ImageFlags, ImageId, ImageSource};
use shared::warn;

use super::decode::premultiply;
use super::{CacheKey, Decoded, GifDelta, ImageCache, Slot};

/// A landed GIF's replay state (ADR-0235): the base frame's own pixels, kept to re-upload on loop
/// wrap, and the rects later frames change, applied to the live texture in [`ImageCache::showing`]
/// as its shown index advances.
pub(super) struct Animation {
    base: Vec<u8>,
    width: u32,
    height: u32,
    delays: Vec<Duration>,
    deltas: Vec<GifDelta>,
    /// The frame index currently on the texture.
    current: usize,
}

/// Uploads a decode or warns once per path when filling its slot; `Failed` stops the next frame
/// asking again.
pub(super) fn upload_or_log(canvas: &mut Canvas<OpenGl>, path: &Path, decoded: Result<Decoded, String>) -> Slot {
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
            if super::first_failure(&path.to_string_lossy()) {
                warn!("{}: {err}", path.display());
            }
            Slot::Failed
        }
    }
}

/// The frame showing `elapsed` after the first, and how long until the next. A still is frame 0
/// with nothing owed; anything else loops. A zero-delay frame is skipped rather than held, which
/// is why `decode_gif` floors what the file asks for.
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

/// Every one of femtovg's sixteen `ErrorKind` variants formats as `"canvas error"`; `Debug` names
/// the actual failure.
fn femtovg_error(err: ErrorKind) -> String {
    format!("{err:?}")
}

impl ImageCache {
    /// The texture `key` is showing now, replaying its animation's deltas onto it if the elapsed
    /// time has moved its frame on, and the repaint its successor owes (ADR-0235). `None` for a
    /// pending or failed slot, which draws nothing.
    pub(super) fn showing(&mut self, canvas: &mut Canvas<OpenGl>, key: &CacheKey) -> Option<ImageId> {
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
            // A change the crop cut away entirely extracts to nothing and uploads nothing.
            for delta in anim.deltas[catch_up].iter().filter(|delta| !delta.pixels.is_empty()) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

//! The shm path both protocols share: format choice, the negotiated buffer, and staging a landed
//! frame's rows for upload (ADR-0248 decision 3).

use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};

use super::*;
use crate::image::capture::PendingFrame;

/// A capture's negotiated shm buffer, reused across frames while size, stride and format match
/// (ADR-0248 decision 3).
///
/// ponytail: one buffer, not a double-buffer swapchain; correct only because pacing admits one
/// frame in flight per source. Upgrade if a later phase allows more.
pub(super) struct NegotiatedBuffer {
    pool: SlotPool,
    pub(super) buffer: Buffer,
    pub(super) width: u32,
    pub(super) height: u32,
    stride: u32,
    format: wl_shm::Format,
}

/// Preferred shm formats: both are wl_shm-mandatory.
pub(super) fn pick_format(candidates: &[wl_shm::Format]) -> Option<wl_shm::Format> {
    [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888].into_iter().find(|preferred| candidates.contains(preferred))
}

/// One `ShmFormat` event's contribution to a negotiation batch: last-event-wins would let a later
/// unsupported format overwrite an earlier supported pick with `None`, so `current` wins once it
/// is `Some`.
pub(super) fn keep_first_supported(current: Option<wl_shm::Format>, offered: wl_shm::Format) -> Option<wl_shm::Format> {
    current.or_else(|| pick_format(&[offered]))
}

pub(super) fn negotiate_buffer(
    shm: &smithay_client_toolkit::shm::Shm,
    existing: Option<NegotiatedBuffer>,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
) -> Option<NegotiatedBuffer> {
    if let Some(buf) = &existing
        && buf.width == width
        && buf.height == height
        && buf.stride == stride
        && buf.format == format
    {
        return existing;
    }
    let mut pool = SlotPool::new((stride as usize * height as usize).max(1), shm).ok()?;
    let (buffer, _canvas) = pool.create_buffer(width as i32, height as i32, stride as i32, format).ok()?;
    Some(NegotiatedBuffer { pool, buffer, width, height, stride, format })
}

/// Reads `negotiated`'s pool memory and stages it for the next canvas-current upload. Copies only
/// the damaged rows unless the size changed or nothing was reported, when the whole buffer is
/// needed anyway.
pub(super) fn stage_landed(
    cache: &mut CaptureCache,
    id: NodeId,
    negotiated: &mut NegotiatedBuffer,
    damage: Vec<DamageRect>,
) {
    let (width, height, stride) = (negotiated.width, negotiated.height, negotiated.stride as usize);
    let Some(buffer) = negotiated.pool.canvas(&negotiated.buffer) else { return };
    let resized = cache.get(id).is_none_or(|(_, w, h)| (w, h) != (width, height));
    let (y0, y1) = if resized || damage.is_empty() {
        (0, height)
    } else {
        let lo = damage.iter().map(|r| r.y).min().unwrap_or(0).min(height);
        let hi = damage.iter().map(|r| r.y.saturating_add(r.height).min(height)).max().unwrap_or(height).max(lo);
        (lo, hi)
    };
    let row_bytes = width as usize * 4;
    let mut pixels = Vec::with_capacity(row_bytes * (y1 - y0) as usize);
    for row in y0..y1 {
        let start = row as usize * stride;
        pixels.extend_from_slice(&buffer[start..start + row_bytes]);
    }
    cache.stage(id, PendingFrame { width, height, y_offset: y0, pixels, damage });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_formats_are_tried_in_order() {
        assert_eq!(pick_format(&[wl_shm::Format::Argb8888]), Some(wl_shm::Format::Argb8888));
        assert_eq!(pick_format(&[wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]), Some(wl_shm::Format::Xrgb8888));
        assert_eq!(pick_format(&[wl_shm::Format::Bgr888]), None);
    }

    #[test]
    fn a_later_unsupported_shm_format_does_not_overwrite_an_earlier_supported_one() {
        let first = keep_first_supported(None, wl_shm::Format::Xrgb8888);
        assert_eq!(first, Some(wl_shm::Format::Xrgb8888));
        assert_eq!(keep_first_supported(first, wl_shm::Format::Bgr888), Some(wl_shm::Format::Xrgb8888));
    }

    #[test]
    fn an_unsupported_first_format_still_lets_a_later_supported_one_through() {
        let first = keep_first_supported(None, wl_shm::Format::Bgr888);
        assert_eq!(first, None);
        assert_eq!(keep_first_supported(first, wl_shm::Format::Argb8888), Some(wl_shm::Format::Argb8888));
    }
}

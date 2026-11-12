//! The shm path both protocols share: format choice, the negotiated buffer, and staging a landed
//! frame's rows for upload (ADR-0248 decision 3).

use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};

use super::*;

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

impl NegotiatedBuffer {
    /// The landed pixels, their size and their row stride in bytes.
    pub(super) fn rows(&mut self) -> Option<(&[u8], (u32, u32), usize)> {
        let size = (self.width, self.height);
        Some((self.pool.canvas(&self.buffer)?, size, self.stride as usize))
    }

    /// Reverses the landed rows and their `damage` in place: a `y_invert` capture arrives
    /// bottom-up, and every consumer (staging, the desktop unmix) reads top-down.
    pub(super) fn flip_rows(&mut self, damage: &mut [DamageRect]) {
        let (height, stride) = (self.height as usize, self.stride as usize);
        if let Some(canvas) = self.pool.canvas(&self.buffer) {
            flip_rows(canvas, stride, height);
        }
        flip_damage(damage, self.height);
    }
}

fn flip_damage(damage: &mut [DamageRect], height: u32) {
    for d in damage {
        d.y = height.saturating_sub(d.y.saturating_add(d.height));
    }
}

/// `create_buffer`'s arguments for a compositor-chosen size, `None` for an empty one or past the
/// protocol's `i32`, pool size included.
fn buffer_layout(width: u32, height: u32, stride: u32) -> Option<(i32, i32, i32, usize)> {
    let len = i32::try_from(u64::from(stride) * u64::from(height)).ok().filter(|&len| len > 0 && width > 0)?;
    Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?, i32::try_from(stride).ok()?, len as usize))
}

fn flip_rows(canvas: &mut [u8], stride: usize, height: usize) {
    let rows = &mut canvas[..stride * height];
    for y in 0..height / 2 {
        let (top, bottom) = rows.split_at_mut((height - 1 - y) * stride);
        top[y * stride..][..stride].swap_with_slice(&mut bottom[..stride]);
    }
}

/// Preferred shm formats: both are wl_shm-mandatory.
pub(super) fn pick_format(candidates: &[wl_shm::Format]) -> Option<wl_shm::Format> {
    [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888].into_iter().find(|preferred| candidates.contains(preferred))
}

/// One `ShmFormat` event's contribution to a negotiation batch: last-event-wins would let a later
/// unsupported format overwrite an earlier supported pick with `None`. Windows prefer ARGB
/// regardless of event order; output capture keeps the first supported format.
pub(super) fn keep_supported(
    current: Option<wl_shm::Format>,
    offered: wl_shm::Format,
    opaque: bool,
) -> Option<wl_shm::Format> {
    if !opaque && offered == wl_shm::Format::Argb8888 {
        return Some(offered);
    }
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
    let (w, h, s, len) = buffer_layout(width, height, stride)?;
    let mut pool = SlotPool::new(len, shm).ok()?;
    let (buffer, _canvas) = pool.create_buffer(w, h, s, format).ok()?;
    Some(NegotiatedBuffer { pool, buffer, width, height, stride, format })
}

/// Hands completed pool memory to the texture cache, which retains damage until upload.
pub(super) fn stage_landed(
    cache: &mut CaptureCache,
    id: NodeId,
    negotiated: &mut NegotiatedBuffer,
    damage: Vec<DamageRect>,
    opaque: bool,
) {
    let has_alpha = !opaque && negotiated.format == wl_shm::Format::Argb8888;
    let Some((buffer, (width, height), stride)) = negotiated.rows() else { return };
    cache.stage_shm(id, buffer, (width, height), stride, has_alpha, damage);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flipping_rows_reverses_them_and_leaves_padding_with_its_row() {
        let mut odd = [1, 1, 9, 2, 2, 9, 3, 3, 9];
        flip_rows(&mut odd, 3, 3);
        assert_eq!(odd, [3, 3, 9, 2, 2, 9, 1, 1, 9]);
        let mut even = [1, 1, 2, 2];
        flip_rows(&mut even, 2, 2);
        assert_eq!(even, [2, 2, 1, 1]);
    }

    #[test]
    fn flipping_damage_mirrors_it_and_survives_rects_past_the_buffer() {
        let mut damage = [DamageRect { x: 0, y: 10, width: 5, height: 20 }, DamageRect::full(4, u32::MAX)];
        damage[1].y = 7;
        flip_damage(&mut damage, 100);
        assert_eq!(damage[0].y, 70);
        assert_eq!(damage[1].y, 0, "a compositor rect overflowing u32 clamps to the top");
    }

    #[test]
    fn buffer_layout_refuses_sizes_the_protocol_cannot_carry() {
        assert_eq!(buffer_layout(1920, 1080, 7680), Some((1920, 1080, 7680, 7680 * 1080)));
        assert_eq!(buffer_layout(1 << 30, 1, (1u32 << 30).saturating_mul(4)), None, "stride past i32");
        assert_eq!(buffer_layout(1 << 31, 1, u32::MAX), None, "width past i32");
        assert_eq!(buffer_layout(16, 1 << 31, 64), None, "height past i32");
        assert_eq!(buffer_layout(16_384, 1 << 15, 65_536), None, "a pool past i32::MAX bytes");
        // wl_shm_pool.create_buffer answers a zero dimension with a fatal `invalid_stride`.
        for (width, height, stride) in [(0, 0, 0), (0, 10, 64), (16, 0, 64)] {
            assert_eq!(buffer_layout(width, height, stride), None, "{width}x{height}");
        }
    }

    #[test]
    fn preferred_formats_are_tried_in_order() {
        assert_eq!(pick_format(&[wl_shm::Format::Argb8888]), Some(wl_shm::Format::Argb8888));
        assert_eq!(pick_format(&[wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]), Some(wl_shm::Format::Xrgb8888));
        assert_eq!(pick_format(&[wl_shm::Format::Bgr888]), None);
    }

    #[test]
    fn keep_supported_never_overwrites_a_supported_format_with_an_unsupported_one() {
        let first = keep_supported(None, wl_shm::Format::Xrgb8888, true);
        assert_eq!(first, Some(wl_shm::Format::Xrgb8888));
        assert_eq!(keep_supported(first, wl_shm::Format::Bgr888, true), Some(wl_shm::Format::Xrgb8888));
        let alpha = keep_supported(first, wl_shm::Format::Argb8888, false);
        assert_eq!(alpha, Some(wl_shm::Format::Argb8888));
        assert_eq!(keep_supported(alpha, wl_shm::Format::Xrgb8888, false), alpha);
        assert_eq!(keep_supported(alpha, wl_shm::Format::Bgr888, false), alpha);
        // An unsupported first format still lets a later supported one through.
        let none = keep_supported(None, wl_shm::Format::Bgr888, true);
        assert_eq!(none, None);
        assert_eq!(keep_supported(none, wl_shm::Format::Argb8888, true), Some(wl_shm::Format::Argb8888));
    }
}

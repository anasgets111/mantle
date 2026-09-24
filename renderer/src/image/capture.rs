//! Texture cache for `capture` nodes (ADR-0248). Uploads inline on the canvas-current thread like
//! `ImageCache`, but with no decode step and one entry per node, replaced in place.

use std::collections::HashMap;

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ImageFlags, ImageId, ImageSource};
use shared::warn;

use crate::image::{Fit, fitted_rect};
use crate::layout::scene::NodeId;
use crate::text::snap::LogicalRect;

/// One damaged rectangle in physical buffer pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    pub fn full(width: u32, height: u32) -> Self {
        DamageRect { x: 0, y: 0, width, height }
    }
}

/// Past this many rects, upload their bounding box instead of each one.
///
/// ponytail: a fixed count, not measured against femtovg's per-call cost. Upgrade: compare areas.
const MERGE_ABOVE: usize = 16;

/// Bounding box over every rect, or `None` for an empty slice.
fn union(rects: &[DamageRect]) -> Option<DamageRect> {
    rects.iter().copied().reduce(|a, b| {
        let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
        let (x1, y1) = ((a.x + a.width).max(b.x + b.width), (a.y + a.height).max(b.y + b.height));
        DamageRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
    })
}

/// Rects to upload, clipped to `full` because the compositor reports them: each one under
/// [`MERGE_ABOVE`], their union past it, or `full` when none were reported.
pub(crate) fn upload_plan(rects: &[DamageRect], full: DamageRect) -> Vec<DamageRect> {
    if rects.is_empty() {
        return vec![full];
    }
    let rects: Vec<DamageRect> = rects
        .iter()
        .filter(|r| r.x < full.width && r.y < full.height)
        .map(|r| DamageRect { width: r.width.min(full.width - r.x), height: r.height.min(full.height - r.y), ..*r })
        .collect();
    if rects.len() > MERGE_ABOVE {
        return union(&rects).into_iter().collect();
    }
    rects
}

/// Whether `bytes` fits the shared texture budget (ADR-0182, ADR-0248 decision 6). An empty pool
/// admits any size, as `image::budget::admits` does for the decode pool. Nothing here evicts
/// another capture to make room; past the ceiling a fresh source is refused and logs once.
pub(crate) fn fits_budget(resident_before: usize, bytes: usize, budget: usize) -> bool {
    resident_before == 0 || resident_before + bytes <= budget
}

/// wl_shm's `Argb8888`/`Xrgb8888` are BGRX in memory on a little-endian machine; femtovg wants
/// RGBA. Alpha is forced opaque: screen content carries none worth keeping.
///
/// ponytail: only these two formats are ever requested (`wayland::capture`'s format pick).
pub(crate) fn bgrx_to_rgba(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
        pixel[3] = 0xFF;
    }
}

/// `region`, in an output's logical pixels, as fractions of that output's frame, clipped to it
/// (ADR-0263). Fractions need no output scale: a HiDPI buffer multiplies them by its own size.
/// Empty when the region misses the output or its size is unknown.
pub(crate) fn crop_fraction(region: LogicalRect, output: (f32, f32)) -> LogicalRect {
    if output.0 <= 0.0 || output.1 <= 0.0 {
        return LogicalRect::default();
    }
    let x0 = (region.x / output.0).clamp(0.0, 1.0);
    let y0 = (region.y / output.1).clamp(0.0, 1.0);
    let x1 = ((region.x + region.width) / output.0).clamp(0.0, 1.0);
    let y1 = ((region.y + region.height) / output.1).clamp(0.0, 1.0);
    LogicalRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
}

/// Where a `width` x `height` frame draws in `box_rect`: the rect filled, and the rect the whole
/// frame is patterned over so that only `crop` (fractions, `None` for all of it) shows, placed by
/// `fit` as if it were the whole image (ADR-0263). `None` for an empty crop, which draws nothing.
pub(crate) fn placement(
    box_rect: LogicalRect,
    width: u32,
    height: u32,
    crop: Option<LogicalRect>,
    fit: Fit,
) -> Option<(LogicalRect, LogicalRect)> {
    let crop = crop.unwrap_or(LogicalRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 });
    if crop.width <= 0.0 || crop.height <= 0.0 {
        return None;
    }
    let fill = fitted_rect(box_rect, crop.width * width as f32, crop.height * height as f32, fit);
    let (frame_width, frame_height) = (fill.width / crop.width, fill.height / crop.height);
    let at = LogicalRect {
        x: fill.x - crop.x * frame_width,
        y: fill.y - crop.y * frame_height,
        width: frame_width,
        height: frame_height,
    };
    Some((fill, at))
}

/// Pixels a capture source landed, awaiting upload once a GL context is current (ADR-0039).
/// `pixels` covers rows `[y_offset, y_offset + pixels.len() / (width * 4))`; the caller sends only
/// the damaged band unless the size changed or nothing was reported, when it is the whole frame.
pub struct PendingFrame {
    pub width: u32,
    pub height: u32,
    pub y_offset: u32,
    pub pixels: Vec<u8>,
    pub damage: Vec<DamageRect>,
}

fn rgba_bytes(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

struct Entry {
    image: ImageId,
    width: u32,
    height: u32,
    bytes: usize,
    /// False for a dma-buf texture, which its swapchain slot owns and reuses.
    owned: bool,
}

/// Per-`capture`-node textures (ADR-0248 decision 6). No pending/failed slots like `ImageCache`: a
/// node with nothing landed yet has no entry and draws nothing.
#[derive(Default)]
pub struct CaptureCache {
    entries: HashMap<NodeId, Entry>,
    pending: HashMap<NodeId, PendingFrame>,
    to_free: Vec<ImageId>,
    texture_budget: usize,
    /// Nodes already warned over budget; cleared once a frame fits again.
    over_budget_warned: std::collections::HashSet<NodeId>,
    /// Nodes [`Self::stage`] was called for since the last [`Self::poll`]: the repaint cue, since a
    /// staged frame changes no `Draw::Capture` field for `DisplayList` equality to notice.
    landed: Vec<NodeId>,
    /// Draw-time crops as fractions of the frame, for a region the protocol cannot crop (ADR-0263).
    crops: HashMap<NodeId, LogicalRect>,
}

impl CaptureCache {
    /// A changed crop is a repaint cue, as a landed frame is.
    pub(crate) fn set_crop(&mut self, node: NodeId, crop: Option<LogicalRect>) {
        let old = match crop {
            Some(crop) => self.crops.insert(node, crop),
            None => self.crops.remove(&node),
        };
        if old != crop {
            self.landed.push(node);
        }
    }

    pub(crate) fn crop(&self, node: NodeId) -> Option<LogicalRect> {
        self.crops.get(&node).copied()
    }

    pub fn set_texture_budget(&mut self, budget: usize) {
        self.texture_budget = budget;
    }

    /// Replaces whichever frame is still waiting: a source landing faster than it paints only
    /// shows the latest.
    pub(crate) fn stage(&mut self, node: NodeId, frame: PendingFrame) {
        self.pending.insert(node, frame);
        self.landed.push(node);
    }

    /// Nodes with a frame waiting to be painted. Draining: called once per turn.
    pub fn poll(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.landed)
    }

    /// Whether `bytes` fits the shared budget alongside everything else charged to it, less
    /// `node`'s own current entry (about to be replaced). Warns once per node while it does not,
    /// shared by [`Self::install_texture`] and [`Self::upload_landed`].
    fn admits(&mut self, node: NodeId, bytes: usize, width: u32, height: u32) -> bool {
        let others = self.resident_bytes() - self.entries.get(&node).map_or(0, |e| e.bytes);
        if fits_budget(others, bytes, self.texture_budget) {
            self.over_budget_warned.remove(&node);
            return true;
        }
        if self.over_budget_warned.insert(node) {
            warn!(
                "a capture texture ({bytes} bytes at {width}x{height}) does not fit the {} byte texture budget; \
                 dropping this frame",
                self.texture_budget
            );
        }
        false
    }

    /// Installs a texture `wayland::dmabuf` already imported (ADR-0248 amendment): no pixels move
    /// through here, since dma-buf capture uploads nothing. `false` means it did not fit the
    /// texture budget and was not installed, same as a CPU frame dropped in [`Self::upload_landed`].
    pub fn install_texture(&mut self, node: NodeId, image: ImageId, width: u32, height: u32) -> bool {
        let bytes = rgba_bytes(width, height);
        if !self.admits(node, bytes, width, height) {
            return false;
        }
        if let Some(old) = self.entries.insert(node, Entry { image, width, height, bytes, owned: false })
            && old.owned
        {
            self.to_free.push(old.image);
        }
        self.landed.push(node);
        true
    }

    /// Drops a node's texture and any pixels still waiting for one. Freed at the next
    /// [`Self::upload_landed`], the same one-turn lag `ImageCache::release_evicted` accepts.
    pub(crate) fn forget(&mut self, node: NodeId) {
        self.pending.remove(&node);
        self.over_budget_warned.remove(&node);
        self.crops.remove(&node);
        if let Some(entry) = self.entries.remove(&node)
            && entry.owned
        {
            self.to_free.push(entry.image);
        }
    }

    pub fn get(&self, node: NodeId) -> Option<(ImageId, u32, u32)> {
        self.entries.get(&node).map(|entry| (entry.image, entry.width, entry.height))
    }

    pub fn resident_bytes(&self) -> usize {
        self.entries.values().map(|entry| entry.bytes).sum()
    }

    /// Frees textures [`Self::forget`] queued, then uploads every staged frame. The one place any
    /// of this touches `canvas` (ADR-0039).
    pub fn upload_landed(&mut self, canvas: &mut Canvas<OpenGl>) {
        for image in self.to_free.drain(..) {
            canvas.delete_image(image);
        }
        let pending: Vec<(NodeId, PendingFrame)> = self.pending.drain().collect();
        for (node, mut frame) in pending {
            bgrx_to_rgba(&mut frame.pixels);
            let bytes = rgba_bytes(frame.width, frame.height);
            if !self.admits(node, bytes, frame.width, frame.height) {
                continue;
            }
            // A dma-buf swapchain slot owns a `!owned` entry's texture, so shm never writes into it.
            let needs_new_texture =
                self.entries.get(&node).is_none_or(|e| (e.width, e.height) != (frame.width, frame.height) || !e.owned);
            if needs_new_texture {
                if frame.y_offset != 0 || frame.pixels.len() != bytes {
                    continue;
                }
                if let Some(old) = self.entries.remove(&node)
                    && old.owned
                {
                    canvas.delete_image(old.image);
                }
                let source = ImageSource::from(femtovg::imgref::Img::new(
                    frame.pixels.as_rgba(),
                    frame.width as usize,
                    frame.height as usize,
                ));
                let Ok(image) = canvas.create_image(source, ImageFlags::PREMULTIPLIED) else { continue };
                self.entries
                    .insert(node, Entry { image, width: frame.width, height: frame.height, bytes, owned: true });
                continue;
            }
            let Some(entry) = self.entries.get(&node) else { continue };
            let stride = frame.width as usize * 4;
            for rect in upload_plan(&frame.damage, DamageRect::full(frame.width, frame.height)) {
                let (x, y, w, h) = (rect.x as usize, rect.y as usize, rect.width as usize, rect.height as usize);
                let local_y = y - frame.y_offset as usize;
                let mut patch = Vec::with_capacity(w * h * 4);
                for row in local_y..local_y + h {
                    let start = row * stride + x * 4;
                    patch.extend_from_slice(&frame.pixels[start..start + w * 4]);
                }
                let source = ImageSource::from(femtovg::imgref::Img::new(patch.as_rgba(), w, h));
                let _ = canvas.update_image(entry.image, source, x, y);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_damage_reported_uploads_the_whole_buffer() {
        let full = DamageRect::full(200, 100);
        assert_eq!(upload_plan(&[], full), vec![full]);
    }

    #[test]
    fn a_few_rects_upload_individually() {
        let full = DamageRect::full(200, 100);
        let rects =
            vec![DamageRect { x: 0, y: 0, width: 10, height: 10 }, DamageRect { x: 50, y: 50, width: 5, height: 5 }];
        assert_eq!(upload_plan(&rects, full), rects);
    }

    #[test]
    fn past_the_cap_rects_merge_to_their_bounding_box() {
        let full = DamageRect::full(200, 100);
        let rects: Vec<DamageRect> =
            (0..=MERGE_ABOVE).map(|i| DamageRect { x: i as u32, y: 0, width: 1, height: 1 }).collect();
        let plan = upload_plan(&rects, full);
        assert_eq!(plan, vec![DamageRect { x: 0, y: 0, width: MERGE_ABOVE as u32 + 1, height: 1 }]);
    }

    #[test]
    fn rects_past_the_frame_are_clipped_to_it() {
        let full = DamageRect::full(200, 100);
        let rects = vec![
            DamageRect { x: 190, y: 90, width: 50, height: 50 },
            DamageRect { x: 200, y: 0, width: 1, height: 1 },
            DamageRect { x: 0, y: u32::MAX, width: u32::MAX, height: u32::MAX },
        ];
        assert_eq!(upload_plan(&rects, full), vec![DamageRect { x: 190, y: 90, width: 10, height: 10 }]);
    }

    #[test]
    fn union_of_nothing_is_none() {
        assert_eq!(union(&[]), None);
    }

    #[test]
    fn union_covers_every_rect_and_the_gap_between_them() {
        let rects =
            vec![DamageRect { x: 0, y: 0, width: 10, height: 10 }, DamageRect { x: 40, y: 5, width: 10, height: 10 }];
        assert_eq!(union(&rects), Some(DamageRect { x: 0, y: 0, width: 50, height: 15 }));
    }

    #[test]
    fn a_lone_capture_fits_even_past_the_budget() {
        assert!(fits_budget(0, 100, 10));
    }

    #[test]
    fn a_second_capture_is_refused_once_the_budget_is_full() {
        assert!(fits_budget(5, 5, 10));
        assert!(!fits_budget(6, 5, 10));
    }

    #[test]
    fn bgrx_to_rgba_swaps_the_colour_channels_and_forces_opaque() {
        let mut pixels = vec![0x10, 0x20, 0x30, 0x00, 0xAA, 0xBB, 0xCC, 0x7F];
        bgrx_to_rgba(&mut pixels);
        assert_eq!(pixels, vec![0x30, 0x20, 0x10, 0xFF, 0xCC, 0xBB, 0xAA, 0xFF]);
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> LogicalRect {
        LogicalRect { x, y, width, height }
    }

    #[test]
    fn a_region_becomes_fractions_of_the_output_clipped_to_it() {
        let output = (3440.0, 1440.0);
        assert_eq!(crop_fraction(rect(860.0, 360.0, 1720.0, 720.0), output), rect(0.25, 0.25, 0.5, 0.5));
        assert_eq!(crop_fraction(rect(1720.0, 0.0, 9999.0, 1440.0), output), rect(0.5, 0.0, 0.5, 1.0));
        assert_eq!(crop_fraction(rect(3440.0, 0.0, 10.0, 10.0), output).width, 0.0, "past the right edge");
        assert_eq!(crop_fraction(rect(0.0, 0.0, 10.0, 10.0), (0.0, 0.0)).width, 0.0, "no logical size known");
    }

    #[test]
    fn no_crop_patterns_the_frame_over_exactly_what_it_fills() {
        let box_rect = rect(10.0, 20.0, 100.0, 50.0);
        for fit in [Fit::Cover, Fit::Contain, Fit::Stretch] {
            let fitted = fitted_rect(box_rect, 640.0, 480.0, fit);
            assert_eq!(placement(box_rect, 640, 480, None, fit), Some((fitted, fitted)), "{fit:?}");
        }
    }

    /// A 400x200 frame's top-left quarter (200x100) into a 100x100 box: the crop is placed as if it
    /// were the image, and the whole frame is patterned twice its size, anchored at the fill.
    #[test]
    fn a_crop_is_placed_by_each_fit_as_if_it_were_the_whole_image() {
        let box_rect = rect(10.0, 20.0, 100.0, 100.0);
        let quarter = Some(rect(0.0, 0.0, 0.5, 0.5));
        assert_eq!(
            placement(box_rect, 400, 200, quarter, Fit::Contain),
            Some((rect(10.0, 45.0, 100.0, 50.0), rect(10.0, 45.0, 200.0, 100.0)))
        );
        assert_eq!(
            placement(box_rect, 400, 200, quarter, Fit::Cover),
            Some((rect(-40.0, 20.0, 200.0, 100.0), rect(-40.0, 20.0, 400.0, 200.0)))
        );
        let right_half = Some(rect(0.5, 0.0, 0.5, 1.0));
        assert_eq!(
            placement(box_rect, 400, 200, right_half, Fit::Stretch),
            Some((box_rect, rect(-90.0, 20.0, 200.0, 100.0)))
        );
        assert_eq!(placement(box_rect, 400, 200, Some(rect(1.0, 0.0, 0.0, 1.0)), Fit::Contain), None, "empty");
    }

    #[test]
    fn a_hidpi_frame_places_a_crop_where_a_one_x_frame_does() {
        let box_rect = rect(0.0, 0.0, 300.0, 200.0);
        let crop = Some(rect(0.25, 0.25, 0.5, 0.5));
        let corners = |(a, b): (LogicalRect, LogicalRect)| [a.x, a.y, a.width, a.height, b.x, b.y, b.width, b.height];
        for fit in [Fit::Cover, Fit::Contain, Fit::Stretch] {
            let one_x = corners(placement(box_rect, 3440, 1440, crop, fit).unwrap());
            let one_and_a_half = corners(placement(box_rect, 5160, 2160, crop, fit).unwrap());
            assert!(one_x.iter().zip(one_and_a_half).all(|(a, b)| (a - b).abs() < 1e-3), "{fit:?}");
        }
    }

    #[test]
    fn a_lone_capture_still_fits_past_the_budget() {
        let mut cache = CaptureCache::default();
        cache.set_texture_budget(10);
        assert!(cache.admits(crate::layout::scene::NodeId::test(1), 100, 5, 5));
    }
}

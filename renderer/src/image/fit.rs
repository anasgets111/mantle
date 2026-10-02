//! Where an image lands: the box its cache entry is stored under, and its rect inside a layout box.

use std::path::Path;

use super::Fit;
use super::decode::is_vector;
use crate::text::snap::LogicalRect;

/// The box a source is *stored* under, which is not always the box it is drawn into: a vector
/// texture uses its longest edge, so 24x30 shares the 30x30 slot (ADR-0122).
/// A pin resolves through the same key, or an `image` pointing at an SVG pins 200x40 while the
/// entry sits under 200x200 and `victims` evicts a texture a mapped surface shows (ADR-0183).
pub(super) fn cache_box(path: &Path, box_px: (u32, u32)) -> (u32, u32) {
    let box_px = (box_px.0.max(1), box_px.1.max(1));
    if is_vector(path) { (box_px.0.max(box_px.1), box_px.0.max(box_px.1)) } else { box_px }
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

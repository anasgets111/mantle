//! Subpixel-to-physical-pixel snapping math (build-steps.md Phase 4, point 3).
//!
//! build-steps.md cites `docs/oblisk-layout-engine-geometry.md § 5` for this, but that
//! section is actually "Overlay Input Region Bounding Box Calculations" (Phase 3's
//! click-through input region, already implemented in `wayland::mod`) -- a stale
//! cross-reference, not text/border snapping (see docs/adr/0012). What §5.1 *does*
//! give is the general technique Oblisk uses for snapping fractional layout
//! coordinates to physical pixel boundaries: floor the top-left corner, ceil the
//! bottom-right, so the physical box always fully contains the logical one. This
//! module is that same technique as a small reusable function, for text line boxes
//! and rect borders instead of input regions.

/// A rectangle in logical (fractional, DPI-independent) pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A rectangle snapped to physical (integer) pixel boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// Snaps `rect` to physical pixel boundaries at fractional output scale `scale`.
///
/// The top-left corner floors down and the bottom-right corner ceils up, so the
/// snapped rect always fully contains the logical one -- shrinking would clip a glyph
/// or a border stroke, but growing by less than one physical pixel per edge is what
/// prevents the antialiasing blur build-steps.md Phase 4 point 3 calls out.
pub fn snap_to_physical(rect: LogicalRect, scale: f32) -> PhysicalRect {
    PhysicalRect {
        x0: (rect.x * scale).floor() as i32,
        y0: (rect.y * scale).floor() as i32,
        x1: ((rect.x + rect.width) * scale).ceil() as i32,
        y1: ((rect.y + rect.height) * scale).ceil() as i32,
    }
}

/// Snaps a border/hairline's centerline coordinate so a 1-physical-pixel-wide stroke
/// centered on it lands exactly on one row/column of physical pixels, instead of
/// straddling two and blurring -- the second, distinct half of build-steps.md Phase 4
/// point 3 ("snap borders strictly to single physical pixels"), which `snap_to_physical`
/// deliberately does not cover: that function grows a *containment* box outward to the
/// nearest whole pixels (correct for damage rects, wrong for a hairline, which would
/// end up 2 physical pixels wide if you just floored one edge and ceiled the other).
///
/// Physical pixel `n` covers `[n, n+1)` and is centered at `n + 0.5`; a stroke needs
/// its centerline there; a coordinate sitting on the integer boundary itself is
/// exactly between two pixel centers and rasterizes as a blurred 2px-wide line.
/// This rounds to the nearest such center. Returns physical pixel units (unlike
/// `snap_to_physical`'s integer `PhysicalRect`, this is fractional by construction --
/// pixel centers sit at half-integers, not whole ones), so pair it with a stroke width
/// of `1.0 / scale` in logical units to draw an exactly-one-physical-pixel-wide line.
///
/// No caller yet -- nothing in this milestone draws a border/rect stroke (that's the
/// future scene-graph `Rect` node, see docs/oblisk-layout-engine-geometry.md § 1.1).
/// The function and its tests are the spec deliverable for now (build-steps.md Phase
/// 4 point 3's second half); wiring it up is that later phase's job, not this one's.
#[allow(dead_code)]
pub fn snap_border_to_physical(position: f32, scale: f32) -> f32 {
    let physical = position * scale;
    (physical - 0.5).round() + 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_scale_is_exact() {
        let r = LogicalRect { x: 10.0, y: 20.0, width: 30.0, height: 40.0 };
        assert_eq!(snap_to_physical(r, 1.0), PhysicalRect { x0: 10, y0: 20, x1: 40, y1: 60 });
    }

    #[test]
    fn already_pixel_aligned_rect_is_unchanged() {
        let r = LogicalRect { x: 4.0, y: 4.0, width: 8.0, height: 8.0 };
        assert_eq!(snap_to_physical(r, 1.0), PhysicalRect { x0: 4, y0: 4, x1: 12, y1: 12 });
    }

    #[test]
    fn fractional_scale_rounds_outward_never_inward() {
        // 1.5x: origin floors from 15.6 to 15, far edge ceils from 23.1 to 24 -- the
        // physical box (15..24) strictly contains the logical box's scaled span.
        let r = LogicalRect { x: 10.4, y: 0.0, width: 5.0, height: 0.0 };
        let p = snap_to_physical(r, 1.5);
        assert_eq!(p.x0, 15);
        assert_eq!(p.x1, 24);
    }

    #[test]
    fn zero_size_rect_snaps_to_zero_width_box() {
        let r = LogicalRect { x: 3.0, y: 3.0, width: 0.0, height: 0.0 };
        let p = snap_to_physical(r, 2.0);
        assert_eq!(p.x0, 6);
        assert_eq!(p.x1, 6);
    }

    #[test]
    fn negative_coordinates_snap_consistently() {
        // A node positioned left of/above its surface origin (e.g. mid-scroll).
        let r = LogicalRect { x: -10.6, y: 0.0, width: 5.0, height: 0.0 };
        let p = snap_to_physical(r, 1.0);
        assert_eq!(p.x0, -11); // floor(-10.6) = -11
        assert_eq!(p.x1, -5); // ceil(-5.6) = -5
    }

    #[test]
    fn border_snaps_to_nearest_pixel_center_below() {
        // 5.2 is closer to pixel 5's center (5.5) than pixel 4's (4.5).
        assert_eq!(snap_border_to_physical(5.2, 1.0), 5.5);
    }

    #[test]
    fn border_snaps_to_nearest_pixel_center_above() {
        // 6.1 is closer to pixel 6's center (6.5) than pixel 5's (5.5) -- this only
        // passes if rounding is genuinely direction-sensitive, not a fixed offset.
        assert_eq!(snap_border_to_physical(6.1, 1.0), 6.5);
    }

    #[test]
    fn border_on_exact_pixel_boundary_resolves_deterministically() {
        // Sitting exactly on the boundary between two pixel centers (4.5 and 5.5) is
        // a tie; `round`'s away-from-zero convention breaks it towards 5.5.
        assert_eq!(snap_border_to_physical(5.0, 1.0), 5.5);
    }

    #[test]
    fn border_snapping_differs_from_containment_snapping() {
        // The whole point of a separate function: for the same input, snap_to_physical
        // grows a box outward (here, to 5..6), while snap_border_to_physical picks the
        // single nearest pixel center (5.5) -- neither result is the other's edge.
        let scale = 1.0;
        let r = LogicalRect { x: 5.2, y: 0.0, width: 0.0, height: 0.0 };
        let containment = snap_to_physical(r, scale);
        let border = snap_border_to_physical(5.2, scale);
        assert_eq!(containment.x0, 5);
        assert_eq!(border, 5.5);
        assert_ne!(containment.x0 as f32, border);
    }

    #[test]
    fn border_snaps_correctly_at_fractional_scale() {
        // Logical 2.0 at 2x scale is physical 4.0, exactly on a boundary -- nearest
        // center is 4.5 (tie broken the same away-from-zero direction as the 1x case).
        assert_eq!(snap_border_to_physical(2.0, 2.0), 4.5);
    }

    #[test]
    fn border_snaps_negative_coordinates_consistently() {
        // -3.4 is closer to pixel -4's center (-3.5) than pixel -3's (-2.5).
        assert_eq!(snap_border_to_physical(-3.4, 1.0), -3.5);
    }
}

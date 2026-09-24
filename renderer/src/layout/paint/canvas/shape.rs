//! Box paths, fills, gradients and borders.

use std::f32::consts::{FRAC_PI_2, PI};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, Paint, Path, Solidity};

use crate::layout::node::{BorderColor, EdgeInsets, Fill, Gradient, GradientShape, Rgba};
use crate::text::snap::{LogicalRect, snap_border_band};

/// Below this the two sides of a box count as equal (`box_path`): the 0.05 px band the sweep in
/// its doc comment found clear of femtovg's bevel fold, and far above any layout rounding error.
const HAIR: f32 = 0.05;

/// The path a box with `radius` asks for: a rectangle, rounded rectangle, or stadium. femtovg
/// clamps radius with `rad.min(halfw)`; near that clamp, `rounded_rect` fails in two bands. At
/// exactly half, its zero-length straight segments collapse the fill to a square. Just below,
/// `path::cache`'s half-pixel `woff` bevel inset folds the fill fan back at each join: opaque fill
/// hides it, translucent fill blends folded slivers twice (a one-pixel chord at 1.6x alpha on a
/// 42%-alpha ground).
///
/// A sweep of square boxes from 24 to 43.5 logical pixels, at two sub-pixel offsets and 80
/// geometries per row, found:
///
/// | radius below half | filled square | interior seams |
/// | ----------------- | ------------- | -------------- |
/// | 0 (exactly half)  | 80            | 0              |
/// | 0.0001 to 0.01 px | 0             | 29             |
/// | 0.05 px and more  | 0             | 0              |
///
/// A shortfall clears both bands. An epsilon of 0.01 sits in the second, and so does Qt's
/// `qMin(w, h) * 0.4999f` (`qsgbasicinternalrectanglenode.cpp`) at every size. Any epsilon is a
/// constant tuned to one tessellator, so this builds the shape instead.
///
/// Half the smaller side is how config spells a pill (`radius = side / 2`), and a radius scaled
/// apart from the height can exceed half of it. Equal sides use femtovg's circle (four beziers,
/// no straight segments); unequal sides use two semicircular caps joined by `|width - height|`.
/// Both wind like `rounded_rect` (left, bottom, right, top), which controls the fill-fan inset.
///
/// A box whose sides differ by a hair is a circle. A hair-length straight run between the two
/// caps is worse than a bevel: for a box 2 µm narrower than it is tall, the vertical-cap path's
/// fill fan folds over and paints the whole bounding square (`a_box_a_hair_narrower_than_tall_is_
/// still_a_circle`). Tweens produce exactly that: a `width = "Fill"` circle inside a cell whose
/// width and padding both ease lands a rounding error either side of its height on different
/// frames, and the narrow frames flashed as squares.
pub(super) fn box_path(rect: LogicalRect, radius: f32) -> Path {
    let LogicalRect { x, y, width: w, height: h } = rect;
    let mut path = Path::new();

    if radius == 0.0 || w <= 0.0 || h <= 0.0 {
        path.rect(x, y, w, h);
    } else if radius < 0.0 {
        // Each arc is centred on a corner point and swept inward; `arc` joins them with the
        // straight edges. Below half the shorter side, so neighbouring arcs never meet and fold.
        // Wound left, bottom, right, top like the shapes below: the other way, femtovg's
        // antialiasing inset pushes the edge up to 3px into the scoop.
        let r = (-radius).min(w.min(h) / 2.0 - HAIR).max(0.0);
        path.arc(x, y + h, r, -FRAC_PI_2, 0.0, Solidity::Hole);
        path.arc(x + w, y + h, r, PI, 3.0 * FRAC_PI_2, Solidity::Hole);
        path.arc(x + w, y, r, FRAC_PI_2, PI, Solidity::Hole);
        path.arc(x, y, r, 0.0, FRAC_PI_2, Solidity::Hole);
        path.close();
    } else if radius < w.min(h) / 2.0 {
        path.rounded_rect(x, y, w, h, radius);
    } else if (w - h).abs() <= HAIR {
        path.circle(x + w / 2.0, y + h / 2.0, w.min(h) / 2.0);
    } else if w > h {
        let r = h / 2.0;
        let (cy, right) = (y + r, x + w - r);
        // Top of the left cap, round the left to its bottom; the bottom edge; the right cap, round
        // to its top; `close` walks the top edge back. `Solidity::Solid` sweeps by *decreasing*
        // angle, which with y pointing down is the left-bottom-right-top direction wanted here.
        path.arc(x + r, cy, r, 3.0 * FRAC_PI_2, FRAC_PI_2, Solidity::Solid);
        path.arc(right, cy, r, FRAC_PI_2, -FRAC_PI_2, Solidity::Solid);
        path.close();
    } else {
        let r = w / 2.0;
        let (cx, bottom) = (x + r, y + h - r);
        path.arc(cx, y + r, r, 0.0, -PI, Solidity::Solid);
        path.arc(cx, bottom, r, PI, 0.0, Solidity::Solid);
        path.close();
    }

    path
}

/// The background fill, rounded when the node asked for it. See [`box_path`] for why a radius at
/// half the box is its own shape rather than a `rounded_rect` argument.
pub(super) fn fill_rect(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, radius: f32, fill: &Fill) {
    // femtovg's antialias fringe paints an empty path as a 1px line.
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let paint = match fill {
        Fill::Color(color) => Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)),
        Fill::Gradient(gradient) => gradient_paint(gradient, rect),
    };
    canvas.fill_path(&box_path(rect, radius), &paint);
}

/// `gradient` laid over `rect` with CSS's geometry (ADR-0255).
pub(super) fn gradient_paint(gradient: &Gradient, rect: LogicalRect) -> Paint {
    let stops = gradient.stops.iter().map(|(at, c)| (*at, Color::rgbaf(c.r, c.g, c.b, c.a)));
    let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    match gradient.shape {
        GradientShape::Linear { angle } => {
            // CSS's gradient line: long enough that the corners it points between take the end stops.
            let (sin, cos) = angle.to_radians().sin_cos();
            let half = (rect.width * sin.abs() + rect.height * cos.abs()) / 2.0;
            let (dx, dy) = (sin * half, -cos * half);
            Paint::linear_gradient_stops(cx - dx, cy - dy, cx + dx, cy + dy, stops)
        }
        GradientShape::Radial => {
            Paint::elliptical_gradient_stops(cx, cy, 0.0, 0.0, rect.width / 2.0, rect.height / 2.0, stops)
        }
        // femtovg starts a turn at three o'clock, CSS at twelve.
        GradientShape::Conic { angle } => {
            Paint::conic_gradient_stops_with_angle(cx, cy, (angle - 90.0).to_radians(), stops)
        }
    }
}

/// femtovg has no per-edge border primitive, so this covers exactly two cases. Uniform borders
/// (all four edges same width and colour) with `radius` above 0 get one `stroke_path` over the
/// rounded rect, inset by half the stroke width: femtovg strokes centred on the path, so drawing on
/// `rect`'s own edge would straddle it, half inside and half outside. Everything else, any edge
/// differing or radius 0, fills each edge that declares both a non-zero width and a colour as its
/// own rectangle.
///
/// ponytail: the per-edge-rectangle fallback ignores `radius`, giving square corners where a
/// rounded background shows through. Upgrade path: four corner arcs plus four mitred edge
/// segments, once a real config needs a rounded per-edge border.
pub(super) fn paint_border(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    radius: f32,
    colors: BorderColor,
    widths: EdgeInsets,
    scale: f32,
) {
    let uniform_width = widths.top == widths.right && widths.right == widths.bottom && widths.bottom == widths.left;
    let uniform_color = matches!(
        (colors.top, colors.right, colors.bottom, colors.left),
        (Some(t), Some(r), Some(b), Some(l)) if t == r && r == b && b == l
    );

    if uniform_width && uniform_color && widths.top > 0.0 && radius > 0.0 {
        let color = colors.top.expect("uniform_color's match arm above guarantees Some on every edge");
        // `snap_border_band` rounds a box's own two edges to nearest, so it snaps the node's span
        // on each axis, not only a hairline's thickness. The stroke's thickness is snapped the same
        // way (band-of-one starting at `rect.x`, only the thickness half kept), so with integer box
        // edges and an integer thickness the centerline lands on an integer for an even width and a
        // half-integer for an odd one, the parity femtovg actually rasterizes (this module's
        // `snap_border_band` doc comment).
        let (box_x, box_width) = snap_border_band(rect.x, rect.width, scale);
        let (box_y, box_height) = snap_border_band(rect.y, rect.height, scale);
        let (_, width) = snap_border_band(rect.x, widths.top, scale);
        let inset = width / 2.0;
        let path = box_path(
            LogicalRect {
                x: box_x + inset,
                y: box_y + inset,
                width: (box_width - width).max(0.0),
                height: (box_height - width).max(0.0),
            },
            radius,
        );
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        paint.set_line_width(width);
        canvas.stroke_path(&path, &paint);
        return;
    }

    // Corners overlap here rather than mitre: each edge is its own filled rect spanning the node's
    // full width or height, so two adjacent non-zero edges both cover the corner they share.
    let LogicalRect { x, y, width: w, height: h } = rect;
    for (color, thickness, edge_rect, axis) in [
        (colors.top, widths.top, LogicalRect { x, y, width: w, height: widths.top }, EdgeAxis::Horizontal),
        (
            colors.bottom,
            widths.bottom,
            LogicalRect { x, y: y + h - widths.bottom, width: w, height: widths.bottom },
            EdgeAxis::Horizontal,
        ),
        (colors.left, widths.left, LogicalRect { x, y, width: widths.left, height: h }, EdgeAxis::Vertical),
        (
            colors.right,
            widths.right,
            LogicalRect { x: x + w - widths.right, y, width: widths.right, height: h },
            EdgeAxis::Vertical,
        ),
    ] {
        paint_border_edge(canvas, color, thickness, edge_rect, axis, scale);
    }
}

/// Which dimension of an edge rect is the thin one: top/bottom are thin in y, left/right in x.
/// `paint_border_edge` needs this to know which axis to hand `snap_border_band`; inferring it from
/// the rect's own width/height would be ambiguous whenever a node's height equals its border width.
enum EdgeAxis {
    Horizontal,
    Vertical,
}

/// One border edge: paints only where both a colour and a non-zero width say so
/// (`node::BorderColor`'s doc comment: `border_width` alone is documented behaviour,
/// not a bug). Snaps the edge's thin axis with `snap_border_band` first, the same whole-physical-
/// pixel treatment as the uniform-radius stroke above; the long axis is left alone, since only the
/// thin axis can straddle a pixel boundary and blur.
fn paint_border_edge(
    canvas: &mut Canvas<OpenGl>,
    color: Option<Rgba>,
    width: f32,
    edge_rect: LogicalRect,
    axis: EdgeAxis,
    scale: f32,
) {
    let Some(color) = color else { return };
    if width <= 0.0 {
        return;
    }
    let edge_rect = match axis {
        EdgeAxis::Horizontal => {
            let (y, height) = snap_border_band(edge_rect.y, edge_rect.height, scale);
            LogicalRect { y, height, ..edge_rect }
        }
        EdgeAxis::Vertical => {
            let (x, width) = snap_border_band(edge_rect.x, edge_rect.width, scale);
            LogicalRect { x, width, ..edge_rect }
        }
    };
    let mut path = Path::new();
    path.rect(edge_rect.x, edge_rect.y, edge_rect.width, edge_rect.height);
    canvas.fill_path(&path, &Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)));
}

#[cfg(test)]
mod tests {
    use super::super::tests::{init_headless_egl, paint_points, pixel_at, text_painter};
    use super::super::*;
    use super::*;

    use mlua::Lua;

    use crate::layout::paint::tests::resolved_surface;
    use crate::layout::scene::LogicalSize;
    use crate::text::shaping::ShapingHandle;

    /// CSS geometry: a linear gradient runs top to bottom unless turned, a radial one from the centre
    /// out to the edges.
    #[test]
    fn a_gradient_background_follows_css_geometry() {
        let child = r##"rect { width = "Fill", height = "Fill",
            background = { gradient = "Linear", stops = { { 0, "#FF0000" }, { 1, "#0000FF" } } } }"##;
        let Some(px) = paint_points(child, &[(32, 0), (32, 63), (32, 32)]) else { return };
        assert!(px[0].0 > 245 && px[0].2 < 10, "top is the first stop, got {:?}", px[0]);
        assert!(px[1].2 > 245 && px[1].0 < 10, "bottom is the last stop, got {:?}", px[1]);
        assert!((120..=136).contains(&px[2].0), "the middle is halfway, got {:?}", px[2]);

        let turned = |shape: &str| {
            format!(
                r##"rect {{ width = "Fill", height = "Fill",
                    background = {{ {shape}, stops = {{ {{ 0, "#FF0000" }}, {{ 1, "#0000FF" }} }} }} }}"##
            )
        };
        let Some(px) = paint_points(&turned(r#"gradient = "Linear", angle = 90"#), &[(0, 32), (63, 32)]) else {
            return;
        };
        assert!(px[0].0 > 245 && px[1].2 > 245, "90 degrees runs left to right, got {px:?}");
        let Some(px) = paint_points(&turned(r#"gradient = "Radial""#), &[(32, 32), (32, 0), (0, 0)]) else { return };
        assert!(px[0].0 > 245 && px[1].2 > 245 && px[2].2 > 245, "centre out to the edges, got {px:?}");
    }

    /// femtovg strokes through the stencil buffer; a context without one paints a translucent
    /// border's overlapping segments twice, which read as 75% where 50% was asked.
    #[test]
    fn a_translucent_rounded_border_paints_its_alpha_once() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, radius = 8, border_width = 4, border_color = "#FF000080",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        for (x, y) in [(20, 1), (1, 20), (38, 20), (20, 38)] {
            let alpha = pixel_at(painter.canvas_mut(), x, y).3;
            assert!((126..=130).contains(&alpha), "({x}, {y}) is painted once, got alpha {alpha}");
        }
    }

    /// A scoop cuts each corner out along a circle centred on the corner point, and a translucent
    /// fill covers everything else exactly once: no fold where an arc meets a straight edge.
    #[test]
    fn a_scooped_box_cuts_each_corner_in_and_fills_the_rest_once() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, radius = 12, corner_shape = "Scoop", background = "#FF000080",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        for (x, y) in [(1, 1), (38, 1), (1, 38), (38, 38), (5, 5)] {
            assert_eq!(pixel_at(painter.canvas_mut(), x, y).3, 0, "({x}, {y}) is inside a scoop");
        }
        for (x, y) in [(20, 1), (1, 20), (20, 20), (10, 10), (38, 20)] {
            let alpha = pixel_at(painter.canvas_mut(), x, y).3;
            assert!((126..=130).contains(&alpha), "({x}, {y}) is filled once, got alpha {alpha}");
        }
        // Row 3's centre crosses the arc at x = sqrt(12^2 - 3.5^2) = 11.5: the edge sits there,
        // not pushed into the scoop.
        assert_eq!(pixel_at(painter.canvas_mut(), 10, 3).3, 0, "the pixel before the arc is empty");
        assert!(pixel_at(painter.canvas_mut(), 12, 3).3 >= 100, "the pixel after the arc is filled");
    }

    #[test]
    fn a_per_edge_border_paints_only_the_edge_that_declared_both_a_colour_and_a_width() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, background = "#000000FF",
                border_width = { top = 4, bottom = 4 },
                border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 20, 1), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 20, 38), (0, 0, 0, 255));
    }

    /// A pill's shape, and the bug it hid. `radius = side / 2`, and a radius over half the side,
    /// both reached femtovg's half-box clamp, whose fill tessellation collapses to a rectangle:
    /// every pill and circle had a square ground under a round border.
    ///
    /// 40x40 at 20 rounded before the fix while
    /// 32x32 did not, showing size-dependent degeneracy rather than a clean threshold; keep both
    /// so one passing size cannot hide it. Both use [`box_path`]'s stadium branch: this catches an
    /// exact half radius, while the companion test catches the just-below-half fill-fold case.
    ///
    /// Both halves are asserted: the corner must show the parent through the round fill, while the
    /// mid-edge must remain border. Squaring the border to match a broken fill would satisfy only
    /// one of those checks.
    #[test]
    fn a_radius_of_half_the_box_fills_a_stadium_not_a_square() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        // side, radius. The third asks for more than half, which must clamp to the same stadium
        // rather than square off.
        for (side, radius) in [(32.0_f32, 16.0_f32), (40.0, 20.0), (32.0, 17.0)] {
            let src = format!(
                r##"return panel {{ id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = {{ top = 4, left = 4 }}, child = rect {{
                    width = {side}, height = {side}, background = "#000000FF", radius = {radius},
                    border_width = 2, border_color = "#FFFFFFFF",
                }} }}"##
            );
            let root = resolved_surface(&lua, &src, LogicalSize { width: 64.0, height: 64.0 });
            paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

            let case = format!("{side}x{side} radius {radius}");
            // Well outside the inscribed circle: `side/2 * (sqrt(2) - 1)` clear, about 6px at 32
            // and 8px at 40, so this is not an antialiasing read.
            assert_eq!(
                pixel_at(painter.canvas_mut(), 5, 5),
                (255, 0, 0, 255),
                "{case}: the corner of a stadium is outside it, so the panel behind must show through"
            );
            assert_eq!(
                pixel_at(painter.canvas_mut(), 4 + side as usize / 2, 4 + side as usize / 2),
                (0, 0, 0, 255),
                "{case}: and the middle is still filled"
            );
            assert_eq!(
                pixel_at(painter.canvas_mut(), 5, 4 + side as usize / 2),
                (255, 255, 255, 255),
                "{case}: the border was always round here and must stay so"
            );
        }
    }

    /// A translucent ground at radius half the box must blend exactly once everywhere inside it.
    ///
    /// The shape being right is not enough, and that is the point of this test sitting beside
    /// [`a_radius_of_half_the_box_fills_a_stadium_not_a_square`]: `rounded_rect` at exactly half
    /// draws a correct outline and then folds its fill fan over itself at each of the four
    /// collapsed straight segments, so a translucent ground gets a second helping of itself along
    /// a one-pixel chord out of each cap. Opaque fills hide it; a 42%-alpha ground does not.
    ///
    /// 33x33 at offset 20 rather than a round 32, because the fold is erratic in the size: a sweep
    /// of square boxes from 24 to 43.5 at radius exactly half found seams at 30 of the 80
    /// geometries tried, and 32x32 at an integer offset was one of the clean ones. This case is one
    /// of the dirty ones, so it fails against a plain `rounded_rect`.
    #[test]
    fn a_translucent_ground_at_half_radius_blends_once_not_twice() {
        let Some(instance) = init_headless_egl(96, 96) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 96) else { return };

        // Black at 42% over the panel's red: one blend is 255 * 0.58, two is 255 * 0.58^2 = 86.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF", padding = { top = 20, left = 20 }, child = rect {
                width = 33, height = 33, background = "#0000006B", radius = 16.5,
            } }"##,
            LogicalSize { width: 96.0, height: 96.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let image = painter.canvas_mut().screenshot().expect("screenshot reads back the pbuffer's own framebuffer");

        // The inscribed disc less four pixels, which clears the antialiased rim on every side.
        let centre = 20.0 + 33.0 / 2.0;
        let radius = 33.0 / 2.0 - 4.0;
        let mut doubled = Vec::new();
        for y in 0..96usize {
            for x in 0..96usize {
                let (dx, dy) = (x as f32 + 0.5 - centre, y as f32 + 0.5 - centre);
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let pixel = image[(x, y)];
                if (pixel.r, pixel.g, pixel.b) != (148, 0, 0) {
                    doubled.push((x, y, pixel.r));
                }
            }
        }
        assert!(doubled.is_empty(), "the ground blended twice at {doubled:?}");

        // And the shape is still a circle, so nothing above can be satisfied by drawing less.
        assert_eq!(
            pixel_at(painter.canvas_mut(), 21, 21),
            (255, 0, 0, 255),
            "the corner of a circle is outside it, so the panel behind must show through"
        );
    }

    /// The uniform-border-with-radius branch of [`paint_border`] had no test at all: the per-edge
    /// case above takes the fallback path, so the whole `stroke_path` arm, including the half-width
    /// inset its doc comment reasons carefully about, went unexercised. That inset is the part
    /// worth pinning: femtovg strokes centred on the path, so an uninset stroke straddles the
    /// node's own edge with half of it painted outside the box.
    #[test]
    fn a_uniform_border_with_a_radius_strokes_inside_the_nodes_own_box() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10, left = 10 }, child = rect {
                width = 40, height = 40, background = "#000000FF",
                radius = 8, border_width = 4, border_color = "#FFFFFFFF",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 12, 30), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 16, 30), (0, 0, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 9, 30), (255, 0, 0, 255));
    }

    /// Regression test: a 1px border at a fractional position must land on exactly one physical
    /// pixel row, not blur across two. `padding.top = 10.3` puts the bordered rect's absolute y at
    /// a fractional offset -- unsnapped, femtovg's own antialiasing fills part of row 10 and part
    /// of row 11 at partial coverage instead of one row at full coverage.
    ///
    /// Proved this catches the bug it exists for: with `snap_border_band` removed from
    /// `paint_border_edge`'s horizontal branch (using the raw, unsnapped `edge_rect` instead),
    /// row 10 came back `(201, 178, 178, 255)`, a red/white blend rather than white, and the
    /// row-10 assertion failed. Restored before finishing.
    #[test]
    fn a_1px_border_at_a_fractional_position_is_exactly_one_pixel_row() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10.3 }, child = rect {
                width = 30, height = 20, background = "#000000FF",
                border_width = { top = 1 }, border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 9), (255, 0, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 10), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 11), (0, 0, 0, 255));
    }

    /// The same snap as the 1px test above, at a width wide enough that "one row either way"
    /// is not the whole story: a 4px band must stay exactly four rows, neither growing to five
    /// nor losing one, which is what pins `snap_border_band` rounding both edges independently
    /// rather than rounding the near edge and adding an unrounded thickness. `padding.top =
    /// 31.3` is the fractional geometry a content-sized surface resolves to, so this is the real
    /// shape of the bug, not a contrived one.
    ///
    /// Note this is the filled-edge branch, not the stroke: `border_width = { top = 4 }` leaves
    /// the other three edges at zero, so `paint_border`'s `uniform_width` test fails and it
    /// takes the per-edge path. Stroke parity, the thing the deleted `snap_border_to_physical`
    /// actually got wrong, is covered by
    /// `a_uniform_stroked_border_at_a_fractional_position_covers_whole_pixel_columns` below.
    ///
    /// Proved this catches the bug: with the same unsnapped `edge_rect` change as the 1px test
    /// above, row 31 came back `(201, 178, 178, 255)` instead of full white -- the loop below
    /// fails on the first row it checks, so this only pins that one value, not all four rows'
    /// worth of blend. Restored before finishing.
    #[test]
    fn a_4px_border_at_a_fractional_position_stays_exactly_four_rows() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 31.3 }, child = rect {
                width = 30, height = 20, background = "#000000FF",
                border_width = { top = 4 }, border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 30), (255, 0, 0, 255));
        for y in 31..35usize {
            assert_eq!(pixel_at(painter.canvas_mut(), 15, y), (255, 255, 255, 255), "row {y} is not fully white");
        }
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 35), (0, 0, 0, 255));
    }

    /// The uniform-width-with-radius branch of `paint_border` strokes rather than fills, and a
    /// stroke is where physical-pixel parity actually bites: femtovg centres a stroke on its
    /// path, so a 4-wide stroke centred on a half-integer spreads across five rows at partial
    /// coverage while the same stroke centred on an integer covers exactly four. Snapping the
    /// box span and the thickness as bands is what puts the centreline on the right side of that
    /// split without the caller reasoning about parity at all.
    ///
    /// `padding.left = 10.3` is what makes this a real test: with an integer padding the stroke
    /// already lands on whole pixels and passes without any snapping, which is why the existing
    /// `a_uniform_border_with_a_radius_strokes_inside_the_nodes_own_box` test above cannot see
    /// this. Measured with the `snap_border_band` calls in that branch removed: column 10 came
    /// back `(201, 178, 178, 255)`, a red/white blend, instead of full white.
    #[test]
    fn a_uniform_stroked_border_at_a_fractional_position_covers_whole_pixel_columns() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10, left = 10.3 }, child = rect {
                width = 40, height = 40, background = "#000000FF",
                radius = 8, border_width = 4, border_color = "#FFFFFFFF",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(
            pixel_at(painter.canvas_mut(), 9, 30),
            (255, 0, 0, 255),
            "column 9 should be the surface's red, outside the border"
        );
        for x in 10..14usize {
            assert_eq!(pixel_at(painter.canvas_mut(), x, 30), (255, 255, 255, 255), "column {x} is not fully white");
        }
        assert_eq!(
            pixel_at(painter.canvas_mut(), 14, 30),
            (0, 0, 0, 255),
            "column 14 should be the rect's own black fill, past the border"
        );
    }

    // ---- `clip = "Rounded"` ----

    /// A `width = "Fill"` circle in a tweening cell lands a rounding error narrower than its
    /// height on some frames. That box took the vertical-cap branch, whose hair-length straight run
    /// folded the fill fan over the whole square; the wider case never did, which is why it showed
    /// on some frames and not others.
    #[test]
    fn a_box_a_hair_narrower_than_tall_is_still_a_circle() {
        let Some(instance) = init_headless_egl(64, 48) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 48) else { return };
        let white = &Fill::Color(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        for (name, w) in [("a hair narrower", 31.999_998), ("a hair wider", 32.000_004), ("square", 32.0)] {
            let canvas = painter.canvas_mut();
            canvas.clear_rect(0, 0, 64, 48, Color::rgbaf(0.0, 0.0, 0.0, 1.0));
            fill_rect(canvas, LogicalRect { x: 8.0, y: 8.0, width: w, height: 32.0 }, 17.0, white);
            canvas.flush();
            assert_eq!(pixel_at(canvas, 9, 9), (0, 0, 0, 255), "{name}: the corner outside the circle stays black");
            assert_eq!(pixel_at(canvas, 24, 24), (255, 255, 255, 255), "{name}: the centre is filled");
        }
    }

    /// A slider's fill at 0% is a zero-width box; it once drew a 1px line.
    #[test]
    fn a_zero_width_box_paints_nothing() {
        let Some(instance) = init_headless_egl(64, 48) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 48) else { return };
        let canvas = painter.canvas_mut();
        canvas.clear_rect(0, 0, 64, 48, Color::rgbaf(0.0, 0.0, 0.0, 1.0));
        let white = &Fill::Color(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        fill_rect(canvas, LogicalRect { x: 24.0, y: 8.0, width: 0.0, height: 32.0 }, 6.0, white);
        canvas.flush();
        for x in 22..27 {
            assert_eq!(pixel_at(canvas, x, 24), (0, 0, 0, 255), "column {x} stays black");
        }
    }
}

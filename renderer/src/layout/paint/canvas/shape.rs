//! Box paths, fills, gradients and borders.

use std::f32::consts::{FRAC_PI_2, PI};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, Paint, Path, Solidity};
use kurbo::Point;

use super::vector_path::femtovg_path;
use crate::layout::node::corner::Squircle;
use crate::layout::node::shape::{boolean, inset, inset_loops};
use crate::layout::node::{BorderColor, BorderPaint, EdgeInsets, Fill, Gradient, GradientShape, Radii, Rgba};
use crate::text::snap::{LogicalRect, snap_border_band};
use kurbo::{BezPath, PathEl, Shape as _};
use linesweeper::BinaryOp;

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
/// still_a_circle`). Tweens produce exactly that: a `width = "fill"` circle inside a cell whose
/// width and padding both ease lands a rounding error either side of its height on different
/// frames, and the narrow frames flashed as squares.
///
/// Unequal corners go to femtovg's `rounded_rect_varying`, which shrinks them by CSS's rule; a
/// scoop and a smoothed corner take the same rule from [`Radii::fit`].
pub(super) fn box_path(rect: LogicalRect, radii: &Radii) -> Path {
    if let Some(outline) = &radii.2 {
        return femtovg_path(&outline.bez(rect), outline.voids(rect).into_iter());
    }
    let LogicalRect { x, y, width: w, height: h } = rect;
    let [tl, tr, br, bl] = radii.0;
    let mut path = Path::new();

    if radii.is_zero() || w <= 0.0 || h <= 0.0 {
        path.rect(x, y, w, h);
    } else if radii.scoop() {
        // Each arc is centred on a corner point and swept inward; `arc` joins them with the
        // straight edges. Short of half the shorter side, so neighbouring arcs never meet and fold.
        // Wound left, bottom, right, top like the shapes below: the other way, femtovg's
        // antialiasing inset pushes the edge up to 3px into the scoop.
        let [tl, tr, br, bl] = radii.fit(w - 2.0 * HAIR, h - 2.0 * HAIR).0.map(|r| -r);
        path.arc(x, y + h, bl, -FRAC_PI_2, 0.0, Solidity::Hole);
        path.arc(x + w, y + h, br, PI, 3.0 * FRAC_PI_2, Solidity::Hole);
        path.arc(x + w, y, tr, FRAC_PI_2, PI, Solidity::Hole);
        path.arc(x, y, tl, 0.0, FRAC_PI_2, Solidity::Hole);
        path.close();
    } else if radii.1 > 0.0 {
        return smoothed_path(rect, radii);
    } else if tl != tr || tr != br || br != bl {
        path.rounded_rect_varying(x, y, w, h, tl, tr, br, bl);
    } else if tl < w.min(h) / 2.0 {
        path.rounded_rect(x, y, w, h, tl);
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

/// A box with continuous corners, wound left, bottom, right, top like `rounded_rect`. Each corner
/// is its [`Squircle`] chain, so this is the one outline the border bands, clips and masks share.
fn smoothed_path(rect: LogicalRect, radii: &Radii) -> Path {
    let LogicalRect { x, y, width: w, height: h } = rect;
    let squircles = radii.fit(w, h).squircles(w, h);
    let mut outline = Outline { path: Path::new(), pen: None };
    // Bottom left, bottom right, top right, top left: each corner's frame and the way it is walked.
    for (i, origin, toward, reversed) in [
        (3, (x, y + h), (1.0, -1.0), false),
        (2, (x + w, y + h), (-1.0, -1.0), true),
        (1, (x + w, y), (-1.0, 1.0), false),
        (0, (x, y), (1.0, 1.0), true),
    ] {
        let place = |(px, py): (f32, f32)| (origin.0 + toward.0 * px, origin.1 + toward.1 * py);
        match squircles[i] {
            Some(squircle) => outline.chain(&squircle, place, if reversed { (1.0, 0.0) } else { (0.0, 1.0) }),
            None => outline.to(origin),
        }
    }
    outline.close(Solidity::Solid);
    outline.path
}

/// The background fill, rounded when the node asked for it. See [`box_path`] for why a radius at
/// half the box is its own shape rather than a `rounded_rect` argument.
pub(super) fn fill_rect(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, radius: &Radii, fill: &Fill) {
    // femtovg's antialias fringe paints an empty path as a 1px line.
    if rect.is_empty() {
        return;
    }
    let paint = fill_paint(fill, rect);
    canvas.fill_path(&box_path(rect, radius), &paint);
}

pub(super) fn fill_paint(fill: &Fill, rect: LogicalRect) -> Paint {
    match fill {
        Fill::Color(color) => Paint::color((*color).into()),
        Fill::Gradient(gradient) => gradient_paint(gradient, rect),
    }
}

/// `gradient` laid over `rect` with CSS's geometry (ADR-0255).
pub(super) fn gradient_paint(gradient: &Gradient, rect: LogicalRect) -> Paint {
    // femtovg pads before the first stop but leaves the row past the last one transparent; CSS holds its colour.
    let tail = gradient.stops.last().filter(|(at, _)| *at < 1.0).map(|(_, c)| (1.0, *c));
    let stops = gradient.stops.iter().copied().chain(tail).map(|(at, c)| (at, Color::from(c)));
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

/// femtovg has no per-edge border primitive, so this covers three cases. Uniform borders (all four
/// edges same width and colour) with `radius` above 0 get one `stroke_path` over the rounded rect,
/// inset by half the stroke width: femtovg strokes centred on the path, so drawing on `rect`'s own
/// edge would straddle it, half inside and half outside. With radius 0, each edge that declares
/// both a non-zero width and a colour fills its own rectangle. Every other border, per-edge on a
/// rounded box or any on a scoop, fills the band between the box's outline and one inset by each
/// edge's width ([`shaped_border`]). A gradient is one paint over the node's whole box, so it
/// counts as a uniform colour and every edge takes it.
pub(super) fn paint_border(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    radius: &Radii,
    border: &BorderPaint,
    widths: EdgeInsets,
) {
    let uniform_width = widths.top == widths.right && widths.right == widths.bottom && widths.bottom == widths.left;
    let uniform_paint = match border {
        BorderPaint::Gradient(gradient) => Some(gradient_paint(gradient, rect)),
        BorderPaint::Edges(BorderColor { top: Some(t), right: Some(r), bottom: Some(b), left: Some(l) })
            if t == r && r == b && b == l =>
        {
            Some(Paint::color((*t).into()))
        }
        BorderPaint::Edges(_) => None,
    };

    // The band between the contour and the contour moved in by the width, which the parser holds
    // to one width and one paint.
    if let Some(outline) = &radius.2 {
        if let Some(paint) = uniform_paint
            && widths.top > 0.0
        {
            let (by, voids) = (f64::from(widths.top), outline.voids(rect));
            let (band, holes) = if voids.contains(&true) {
                let Some(cut) = cut_band(outline, rect, by, BinaryOp::Intersection) else { return };
                cut
            } else {
                let mut band = outline.bez(rect);
                band.extend(inset(&outline.polygon(rect, 0.1), by));
                (band, voids.iter().copied().chain(voids.iter().map(|void| !void)).collect())
            };
            canvas.fill_path(&femtovg_path(&band, holes.into_iter()), &paint);
        }
        return;
    }

    if let Some(mut paint) = uniform_paint
        && uniform_width
        && widths.top > 0.0
        && !radius.is_zero()
        && !radius.scoop()
    {
        // The span and the thickness snap as bands (thickness: a band-of-one at `rect.x`), so the
        // centreline lands on an integer for an even width and a half-integer for an odd one, the
        // parity femtovg rasterizes.
        let (box_x, box_width) = snap_border_band(rect.x, rect.width);
        let (box_y, box_height) = snap_border_band(rect.y, rect.height);
        let (_, width) = snap_border_band(rect.x, widths.top);
        let inset = width / 2.0;
        // The centre line is half a band in, so its radius is the outer one less half a band: concentric with the fill.
        let centre = Radii(radius.0.map(|r| (r - inset).max(0.0)), radius.1, None);
        let path = box_path(
            LogicalRect {
                x: box_x + inset,
                y: box_y + inset,
                width: (box_width - width).max(0.0),
                height: (box_height - width).max(0.0),
            },
            &centre,
        );
        paint.set_line_width(width);
        canvas.stroke_path(&path, &paint);
        return;
    }

    if !radius.is_zero() {
        shaped_border(canvas, rect, radius, border, widths);
        return;
    }

    // Corners overlap here rather than mitre: each edge is its own filled rect spanning the node's
    // full width or height, so two adjacent non-zero edges both cover the corner they share.
    let LogicalRect { x, y, width: w, height: h } = rect;
    let edge_paint = |color: Option<Rgba>| match border {
        BorderPaint::Gradient(gradient) => Some(gradient_paint(gradient, rect)),
        BorderPaint::Edges(_) => color.map(|color| Paint::color(color.into())),
    };
    let colors = match border {
        BorderPaint::Edges(colors) => *colors,
        BorderPaint::Gradient(_) => BorderColor::default(),
    };
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
        paint_border_edge(canvas, edge_paint(color), thickness, edge_rect, axis);
    }
}

/// A corner's arc in the corner's own frame: the corner at the origin, the box towards +x and +y,
/// swept from the vertical edge's end to the horizontal edge's. Angles are in radians, y down.
#[derive(Clone, Copy)]
struct CornerArc {
    centre: (f32, f32),
    radii: (f32, f32),
    from: f32,
    to: f32,
}

/// One corner of a [`shaped_border`]: where it is, which way the box lies from it, and its outer
/// and inner arcs. `reversed` corners are walked against their frame's sweep, since the walk goes
/// clockwise round the box.
struct Corner {
    origin: (f32, f32),
    toward: (f32, f32),
    reversed: bool,
    outer: CornerArc,
    inner: CornerArc,
    /// A smoothed corner's outer and inner outlines, which replace the arcs.
    smooth: Option<[Squircle; 2]>,
}

impl Corner {
    /// `radius` as [`Radii`] holds it, negative for a scoop; `vertical` and `horizontal` are the
    /// widths of the two edges meeting here.
    fn new(
        origin: (f32, f32),
        toward: (f32, f32),
        reversed: bool,
        radius: f32,
        squircle: Option<Squircle>,
        vertical: f32,
        horizontal: f32,
    ) -> Self {
        // The inner outline is the outer one scaled to the inner radii, as the circle's is, and
        // moved in by the two widths.
        let smooth = squircle.map(|outer| {
            let (rx, ry) = ((radius - vertical).max(0.0) / radius, (radius - horizontal).max(0.0) / radius);
            [outer, outer.map(|(x, y)| (vertical + x * rx, horizontal + y * ry))]
        });
        let (outer, inner) = if radius >= 0.0 {
            // CSS's inner corner: each radius less the width beside it, and a square once a width
            // passes the radius.
            let (rx, ry) = ((radius - vertical).max(0.0), (radius - horizontal).max(0.0));
            let sweep = |centre, radii| CornerArc { centre, radii, from: PI, to: 3.0 * FRAC_PI_2 };
            (sweep((radius, radius), (radius, radius)), sweep((vertical + rx, horizontal + ry), (rx, ry)))
        } else {
            // A scoop is centred on the corner point; the inner arc keeps that centre and reaches
            // the inset edges.
            let r = -radius;
            let (rx, ry) = (r + vertical, r + horizontal);
            let outer = CornerArc { centre: (0.0, 0.0), radii: (r, r), from: FRAC_PI_2, to: 0.0 };
            let inner = CornerArc {
                centre: (0.0, 0.0),
                radii: (rx, ry),
                from: (vertical / rx).clamp(-1.0, 1.0).acos(),
                to: (horizontal / ry).clamp(-1.0, 1.0).asin(),
            };
            (outer, inner)
        };
        Corner { origin, toward, reversed, outer, inner, smooth }
    }

    fn place(&self, (x, y): (f32, f32)) -> (f32, f32) {
        (self.origin.0 + self.toward.0 * x, self.origin.1 + self.toward.1 * y)
    }

    /// The angle `at` of the way through this corner, clockwise round the box.
    fn angle(&self, arc: CornerArc, at: f32) -> f32 {
        let at = if self.reversed { 1.0 - at } else { at };
        arc.from + (arc.to - arc.from) * at
    }
}

/// A path under construction that never repeats a point.
/// ponytail: femtovg 0.27 draws a repeated point's zero-length edge as a spike; drop once it skips them.
struct Outline {
    path: Path,
    pen: Option<(f32, f32)>,
}

impl Outline {
    fn to(&mut self, point: (f32, f32)) {
        match self.pen {
            None => self.path.move_to(point.0, point.1),
            Some(pen) if pen != point => self.path.line_to(point.0, point.1),
            Some(_) => {}
        }
        self.pen = Some(point);
    }

    /// `corner`'s outer or inner arc from `from` to `to` of the way through it, either direction,
    /// as one cubic: no corner turns more than a quarter.
    fn arc(&mut self, corner: &Corner, inner: bool, from: f32, to: f32) {
        if let Some(curves) = &corner.smooth {
            let (from, to) = if corner.reversed { (1.0 - from, 1.0 - to) } else { (from, to) };
            return self.chain(&curves[usize::from(inner)], |point| corner.place(point), (from, to));
        }
        let arc = if inner { corner.inner } else { corner.outer };
        let (a0, a1) = (corner.angle(arc, from), corner.angle(arc, to));
        let point = |a: f32| corner.place((arc.centre.0 + arc.radii.0 * a.cos(), arc.centre.1 + arc.radii.1 * a.sin()));
        let (start, end) = (point(a0), point(a1));
        self.to(start);
        if start == end {
            return;
        }
        let k = 4.0 / 3.0 * ((a1 - a0) / 4.0).tan();
        let handle = |a: f32, k: f32| {
            corner.place((
                arc.centre.0 + arc.radii.0 * (a.cos() - k * a.sin()),
                arc.centre.1 + arc.radii.1 * (a.sin() + k * a.cos()),
            ))
        };
        let (c1, c2) = (handle(a0, k), handle(a1, -k));
        self.path.bezier_to(c1.0, c1.1, c2.0, c2.1, end.0, end.1);
        self.pen = Some(end);
    }

    /// `squircle`'s outline from `from` to `to` of the way through it, in its frame placed by `place`.
    fn chain(&mut self, squircle: &Squircle, place: impl Fn((f32, f32)) -> (f32, f32), (from, to): (f32, f32)) {
        for [start, c1, c2, end] in squircle.span(from, to) {
            self.to(place(start));
            // Equal points are a zero-length piece, which `to` already dropped for a line.
            if [c1, c2, end].iter().all(|point| *point == start) {
                continue;
            }
            let (c1, c2, end) = (place(c1), place(c2), place(end));
            self.path.bezier_to(c1.0, c1.1, c2.0, c2.1, end.0, end.1);
            self.pen = Some(end);
        }
    }

    fn close(&mut self, solidity: Solidity) {
        self.path.close();
        self.path.solidity(solidity);
        self.pen = None;
    }
}

/// A border that follows a rounded or scooped outline: the band between the box's outline and one
/// inset by each edge's width, as CSS draws it. Neighbouring edges of one colour fill as one band,
/// so no seam shows between them; where two colours meet, the change sits on the corner at the
/// share of its sweep their widths give it, CSS's rule, so a lone edge's colour runs round both
/// corners and tapers to nothing.
fn shaped_border(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    radius: &Radii,
    border: &BorderPaint,
    widths: EdgeInsets,
) {
    let (x, w) = snap_border_band(rect.x, rect.width);
    let (y, h) = snap_border_band(rect.y, rect.height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let thick = |width: f32| snap_border_band(rect.x, width).1;
    // Clockwise from the top, the order the walk below takes; corner `i` opens edge `i`.
    let edges = [widths.top, widths.right, widths.bottom, widths.left].map(thick);
    // A gradient is one paint on every edge, so a placeholder colour makes them all alike.
    let paints = match border {
        BorderPaint::Edges(colors) => [colors.top, colors.right, colors.bottom, colors.left],
        BorderPaint::Gradient(_) => [Some(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }); 4],
    };
    let drawn: [Option<Rgba>; 4] = std::array::from_fn(|i| paints[i].filter(|_| edges[i] > 0.0));
    if drawn.iter().all(Option::is_none) {
        return;
    }
    let corners = corners((x, y, w, h), radius, edges);
    let [top, right, bottom, left] = edges;
    // Where corner `i`'s two colours meet: the share of its sweep the edge before it takes.
    let split = |i: usize| {
        let (before, after) = (edges[(i + 3) % 4], edges[i]);
        if before + after > 0.0 { before / (before + after) } else { 0.5 }
    };
    let fill = |canvas: &mut Canvas<OpenGl>, outline: Outline, color: Rgba| {
        let paint = match border {
            BorderPaint::Gradient(gradient) => gradient_paint(gradient, rect),
            BorderPaint::Edges(_) => Paint::color(color.into()),
        };
        canvas.fill_path(&outline.path, &paint);
    };

    if let [Some(first), ..] = drawn
        && drawn.iter().all(|edge| *edge == Some(first))
    {
        let mut outline = Outline { path: Path::new(), pen: None };
        for corner in &corners {
            outline.arc(corner, false, 0.0, 1.0);
        }
        outline.close(Solidity::Solid);
        if w > left + right && h > top + bottom {
            for corner in &corners {
                outline.arc(corner, true, 0.0, 1.0);
            }
            outline.close(Solidity::Hole);
        }
        fill(canvas, outline, first);
        return;
    }

    // Runs of neighbouring edges sharing a colour. Each opens at the one edge whose predecessor
    // differs; the all-alike case returned above.
    for start in (0..4).filter(|&edge| drawn[edge].is_some() && drawn[(edge + 3) % 4] != drawn[edge]) {
        let Some(color) = drawn[start] else { continue };
        let length = 1 + (1..4).take_while(|&step| drawn[(start + step) % 4] == Some(color)).count();
        let close = (start + length) % 4;
        let mut outline = Outline { path: Path::new(), pen: None };
        outline.arc(&corners[start], false, split(start), 1.0);
        for step in 1..length {
            outline.arc(&corners[(start + step) % 4], false, 0.0, 1.0);
        }
        outline.arc(&corners[close], false, 0.0, split(close));
        outline.arc(&corners[close], true, split(close), 0.0);
        for step in (1..length).rev() {
            outline.arc(&corners[(start + step) % 4], true, 1.0, 0.0);
        }
        outline.arc(&corners[start], true, 1.0, split(start));
        outline.close(Solidity::Solid);
        fill(canvas, outline, color);
    }
}

/// The corners of the box at `(x, y)` sized `w` by `h`, clockwise from the top left, their inner
/// arcs set in by the `[top, right, bottom, left]` widths.
fn corners((x, y, w, h): (f32, f32, f32, f32), radius: &Radii, [top, right, bottom, left]: [f32; 4]) -> [Corner; 4] {
    let radius = match radius.scoop() {
        false => radius.fit(w, h),
        true => radius.fit(w - 2.0 * HAIR, h - 2.0 * HAIR),
    };
    let [tl, tr, br, bl] = radius.0;
    let [q_tl, q_tr, q_br, q_bl] = radius.squircles(w, h);
    [
        Corner::new((x, y), (1.0, 1.0), false, tl, q_tl, left, top),
        Corner::new((x + w, y), (-1.0, 1.0), true, tr, q_tr, right, top),
        Corner::new((x + w, y + h), (-1.0, -1.0), false, br, q_br, right, bottom),
        Corner::new((x, y + h), (1.0, -1.0), true, bl, q_bl, left, bottom),
    ]
}

/// An outer point of a border's band and the inner point the painter's split lines join it to.
pub(super) type Station = ((f32, f32), (f32, f32));

/// The band [`paint_border`] paints, as stations clockwise round each contour from its start: a
/// box's from where its left edge meets the top-left corner, a `shape`'s from each loop's first
/// point. Built from the painter's own snapped box, widths and corners; an edge it leaves unpainted
/// on a square box has no depth, so its neighbour keeps the corner. Empty when nothing paints.
pub(super) fn border_stations(
    rect: LogicalRect,
    radius: &Radii,
    border: &BorderPaint,
    widths: EdgeInsets,
) -> Vec<Vec<Station>> {
    let paints = match border {
        BorderPaint::Edges(colors) => [colors.top, colors.right, colors.bottom, colors.left].map(|c| c.is_some()),
        BorderPaint::Gradient(_) => [true; 4],
    };
    let raw = [widths.top, widths.right, widths.bottom, widths.left];
    let point = |p: Point| (p.x as f32, p.y as f32);
    if let Some(outline) = &radius.2 {
        // `paint_border` fills a shape's band only in one paint, which the parser guarantees.
        if raw[0] <= 0.0 || !paints[0] {
            return Vec::new();
        }
        let outer = outline.polygon(rect, 0.1);
        let inner = inset_loops(&outer, f64::from(raw[0]));
        let pair = |(o, i): (&Vec<Point>, &Vec<Point>)| {
            let n = o.len();
            let bends = |k: usize| (o[k] - o[(k + n - 1) % n]).cross(o[(k + 1) % n] - o[k]).abs() > 1e-6;
            // A point partway along a straight run takes its inner point from the run's inner edge,
            // held between the corners' inner points, so it can't cross a corner's split line.
            let inner = |k: usize| {
                let Some((a, b)) = (1..n)
                    .map(|back| (k + n - back) % n)
                    .find(|&j| bends(j))
                    .zip((1..n).map(|ahead| (k + ahead) % n).find(|&j| bends(j)))
                else {
                    return i[k];
                };
                let (a, b) = (i[a], i[b]);
                let t = ((o[k] - a).dot(b - a) / (b - a).hypot2().max(f64::EPSILON)).clamp(0.0, 1.0);
                if bends(k) { i[k] } else { a.lerp(b, t) }
            };
            let corners: Vec<Station> = (0..n).map(|k| (point(o[k]), point(inner(k)))).collect();
            let next = |k: usize| corners[(k + 1) % corners.len()];
            corners
                .iter()
                .enumerate()
                .flat_map(|(k, &corner)| {
                    [Some(corner)].into_iter().chain(square(corner, next(k)).into_iter().flatten().map(Some)).flatten()
                })
                .collect()
        };
        return outer.iter().zip(&inner).map(pair).collect();
    }
    let (x, w) = snap_border_band(rect.x, rect.width);
    let (y, h) = snap_border_band(rect.y, rect.height);
    if w <= 0.0 || h <= 0.0 || !(0..4).any(|i| paints[i] && raw[i] > 0.0) {
        return Vec::new();
    }
    let [top, right, bottom, left] = if radius.is_zero() {
        // Each painted edge's own rect, snapped on its thin axis as `paint_border_edge` snaps it; an
        // unpainted edge has no depth.
        let depth = |i: usize, start: f32, near: bool, edge: f32| {
            if !paints[i] || raw[i] <= 0.0 {
                return 0.0;
            }
            let (start, thick) = snap_border_band(start, raw[i]);
            (if near { start + thick - edge } else { edge - start }).max(0.0)
        };
        [
            depth(0, rect.y, true, y),
            depth(1, rect.x + rect.width - raw[1], false, x + w),
            depth(2, rect.y + rect.height - raw[2], false, y + h),
            depth(3, rect.x, true, x),
        ]
    } else {
        raw.map(|width| snap_border_band(rect.x, width).1)
    };
    // Opposite bands that overlap meet in proportion to their depths, so neither's gap crosses the
    // other's dash and their inner points never cross.
    let fit = |near: f32, far: f32, span: f32| match near + far {
        sum if sum > span => (near * span / sum, far * span / sum),
        _ => (near, far),
    };
    let ((top, bottom), (left, right)) = (fit(top, bottom, h), fit(left, right, w));
    let corners = corners((x, y, w, h), radius, [top, right, bottom, left]);
    // A corner's outer and inner points `t` of the way round it, as the painter's arcs place them.
    let at = |corner: &Corner, t: f32| {
        let end = |inner: bool| {
            let mut arc = Outline { path: Path::new(), pen: None };
            if t > 0.5 {
                arc.arc(corner, inner, 0.0, t)
            } else {
                arc.arc(corner, inner, 1.0, t)
            }
            arc.pen.unwrap_or(corner.origin)
        };
        (end(false), end(true))
    };
    let arcs: Vec<Vec<Station>> = corners
        .iter()
        .map(|corner| {
            let extent =
                corner.outer.radii.0.max(corner.outer.radii.1).abs() * if corner.smooth.is_some() { 2.0 } else { 1.0 };
            // Chords within about 0.05 px of the curve.
            let steps = (6.0 * extent).sqrt().ceil().max(1.0) as usize;
            // An inner arc under a pixel round is one point, so its stations fan from it as one
            // slice: slices a pixel thin there would turn inside out under femtovg's fill.
            let tip = (corner.inner.radii.0.max(corner.inner.radii.1).abs() < 1.0).then(|| at(corner, 0.5).1);
            (0..=steps).map(|k| at(corner, k as f32 / steps as f32)).map(|(o, i)| (o, tip.unwrap_or(i))).collect()
        })
        .collect();
    let mut stations = Vec::new();
    for (i, arc) in arcs.iter().enumerate() {
        stations.extend_from_slice(arc);
        stations.extend(square(arc[arc.len() - 1], arcs[(i + 1) % 4][0]).into_iter().flatten());
    }
    vec![stations]
}

/// Along a straight edge from station `a` to `b`, the two stations between which dash ends cut
/// square across the band: the inner points' feet on the outer edge, when both fall on it in order.
/// Past either end the corner is concave on the outside, and the ends stay slanted: a fan from one
/// outer point holds no length to dash along.
fn square((o_a, i_a): Station, (o_b, i_b): Station) -> Option<[Station; 2]> {
    let span = (o_b.0 - o_a.0).hypot(o_b.1 - o_a.1);
    let d = ((o_b.0 - o_a.0) / span, (o_b.1 - o_a.1) / span);
    let along = |p: (f32, f32)| (p.0 - o_a.0) * d.0 + (p.1 - o_a.1) * d.1;
    let (from, to) = (along(i_a), along(i_b));
    let foot = |by: f32| (o_a.0 + d.0 * by, o_a.1 + d.1 * by);
    (0.0 <= from && from < to && to <= span).then(|| [(foot(from), i_a), (foot(to), i_b)])
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
    paint: Option<Paint>,
    width: f32,
    edge_rect: LogicalRect,
    axis: EdgeAxis,
) {
    let Some(paint) = paint else { return };
    if width <= 0.0 {
        return;
    }
    let edge_rect = match axis {
        EdgeAxis::Horizontal => {
            let (y, height) = snap_border_band(edge_rect.y, edge_rect.height);
            LogicalRect { y, height, ..edge_rect }
        }
        EdgeAxis::Vertical => {
            let (x, width) = snap_border_band(edge_rect.x, edge_rect.width);
            LogicalRect { x, width, ..edge_rect }
        }
    };
    let mut path = Path::new();
    path.rect(edge_rect.x, edge_rect.y, edge_rect.width, edge_rect.height);
    canvas.fill_path(&path, &paint);
}

/// One closed subpath, reversed if need be so its area is negative for a hole, positive otherwise.
/// A voided `outline` at `rect` cut by `op` with the band `by` px in from every edge: the band
/// (`Intersection`) or the shape inside it (`Difference`). A band wider than the gap beside a void
/// reaches into the void and past the box, and the cut keeps it to the shape.
pub(super) fn cut_band(
    outline: &crate::layout::node::Shape,
    rect: LogicalRect,
    by: f64,
    op: BinaryOp,
) -> Option<(BezPath, Vec<bool>)> {
    let (shape, voids) = (outline.bez(rect), outline.voids(rect));
    let mut band = shape.clone();
    band.extend(inset(&outline.polygon(rect, 0.1), by));
    let holes = voids.iter().copied().chain(voids.iter().map(|void| !void));
    let wound: BezPath = band.subpaths().zip(holes).flat_map(|(l, hole)| wound(l, hole)).collect();
    boolean(&shape, &wound, op)
}

fn wound(subpath: &[PathEl], hole: bool) -> BezPath {
    let path = BezPath::from_vec(subpath.to_vec());
    if (path.area() < 0.0) == hole { path } else { path.reverse_subpaths() }
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
        let child = r##"rect { width = "fill", height = "fill",
            background = { gradient = "linear", stops = { { 0, "#FF0000" }, { 1, "#0000FF" } } } }"##;
        let Some(px) = paint_points(child, &[(32, 0), (32, 63), (32, 32)]) else { return };
        assert!(px[0].0 > 245 && px[0].2 < 10, "top is the first stop, got {:?}", px[0]);
        assert!(px[1].2 > 245 && px[1].0 < 10, "bottom is the last stop, got {:?}", px[1]);
        assert!((120..=136).contains(&px[2].0), "the middle is halfway, got {:?}", px[2]);

        let turned = |shape: &str| {
            format!(
                r##"rect {{ width = "fill", height = "fill",
                    background = {{ {shape}, stops = {{ {{ 0, "#FF0000" }}, {{ 1, "#0000FF" }} }} }} }}"##
            )
        };
        let Some(px) = paint_points(&turned(r#"gradient = "linear", angle = 90"#), &[(0, 32), (63, 32)]) else {
            return;
        };
        assert!(px[0].0 > 245 && px[1].2 > 245, "90 degrees runs left to right, got {px:?}");
        let Some(px) = paint_points(&turned(r#"gradient = "radial""#), &[(32, 32), (32, 0), (0, 0)]) else { return };
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

    /// A rounded border's outer edge has the box's radius, so it hugs the fill: with `radius = 10` and a
    /// 4 px band, (3, 3) is 0.8 px inside the outer arc, where an arc of `10 + 2` would leave it half out.
    #[test]
    fn a_rounded_borders_outer_edge_has_the_boxs_radius() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, radius = 10, border_width = 4, border_color = "#FF0000",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        let alpha = pixel_at(painter.canvas_mut(), 3, 3).3;
        assert!(alpha > 230, "(3, 3) is inside the outer arc, got alpha {alpha}");
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
                width = 40, height = 40, radius = 12, corner_shape = "scoop", background = "#FF000080",
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

    /// `radius = { .. }` rounds only the corners it names, in fill, border, rounded clip and scoop.
    /// Probes sit 1 px in from each corner, clockwise from the top left.
    #[test]
    fn each_corner_takes_its_own_radius_in_fill_border_clip_and_scoop() {
        let corners = [(1, 1), (38, 1), (38, 38), (1, 38)];
        let filled =
            |child: &str| paint_points(child, &corners).map(|px| px.iter().map(|p| p.3 > 128).collect::<Vec<_>>());
        let rounded = r##"radius = { top_left = 16, bottom_right = 16 }"##;
        let Some(fill) =
            filled(&format!(r##"rect {{ width = 40, height = 40, background = "#000000FF", {rounded} }}"##))
        else {
            return;
        };
        assert_eq!(fill, vec![false, true, false, true], "fill");
        let border =
            format!(r##"rect {{ width = 40, height = 40, border_width = 2, border_color = "#FFFFFFFF", {rounded} }}"##);
        assert_eq!(filled(&border).unwrap(), vec![false, true, false, true], "border");
        let clip = format!(
            r##"rect {{ width = 40, height = 40, clip = "rounded", {rounded},
                children = {{ rect {{ width = "fill", height = "fill", background = "#FF0000FF" }} }} }}"##
        );
        assert_eq!(filled(&clip).unwrap(), vec![false, true, false, true], "clip");
        let scoop = r##"rect { width = 40, height = 40, background = "#000000FF", corner_shape = "scoop",
            radius = { bottom_right = 12 } }"##;
        assert_eq!(filled(scoop).unwrap(), vec![true, true, false, true], "scoop");
    }

    /// A dashed border is the solid one with gaps cut out: never more opaque anywhere, on every
    /// path a border takes, and its dashes do not move when only the colours change.
    #[test]
    fn a_dashed_border_only_removes_gaps_from_the_solid_one() {
        let grid: Vec<_> = (0..64).flat_map(|y| (0..64).map(move |x| (x, y))).collect();
        let alphas = |style: &str| {
            let src = format!("rect {{ width=48,height=48,{style} }}");
            paint_points(&src, &grid).expect("headless EGL required").iter().map(|p| p.3).collect::<Vec<_>>()
        };
        let frame = "shape={melt=0,parts={{left=0,top=0,right=48,bottom=8},{left=0,top=40,right=48,bottom=48},\
            {left=0,top=0,right=8,bottom=48},{left=40,top=0,right=48,bottom=48}}}";
        for style in [
            "border_width=4,border_color='#ffffff'".to_string(),
            "radius=12,border_width=4,border_color='#ffffff'".into(),
            "radius=12,corner_shape='scoop',border_width={top=4},border_color='#ffffff'".into(),
            "radius=12,corner_smoothing=0.6,border_width=4,border_color='#ffffff'".into(),
            "radius=16,border_width={top=12,right=4,bottom=4,left=4},border_color='#ffffff'".into(),
            "radius=2,border_width=8,border_color={top='#ff000080',right='#ffffff80',bottom='#ffffff80',left='#ffffff80'}"
                .into(),
            "border_width={top=4,right=4,bottom=4,left=8},border_color='#ffffff80'".into(),
            "radius=8,border_width=4,border_color={gradient='linear',stops={{0,'#ff0000'},{1,'#0000ff'}}}".into(),
            format!("{frame},border_width=6,border_color='#ffffff'"),
        ] {
            let solid = alphas(&style);
            let dashed = alphas(&format!("{style},border_dash={{6}},border_dash_offset=2"));
            let over = grid.iter().zip(solid.iter().zip(&dashed)).find(|(_, (s, d))| **d > s.saturating_add(2));
            assert!(over.is_none(), "{style}: dashed above solid at {over:?}");
            let sum = |alphas: &[u8]| alphas.iter().map(|a| u32::from(*a)).sum::<u32>();
            assert!(sum(&dashed) > 0 && sum(&dashed) < sum(&solid) * 3 / 4, "{style}: dashes cut gaps");
        }
        // Compared only where both solids are opaque: a colour seam antialiases the solid border itself.
        let widths = "radius=12,border_width={top=4,right=4,bottom=4,left=8}";
        let colours = ["'#ffffff'", "{top='#ff0000',right='#ffffff',bottom='#ffffff',left='#ffffff'}"];
        let [white, red] = colours.map(|colour| {
            let style = format!("{widths},border_color={colour}");
            (alphas(&style), alphas(&format!("{style},border_dash={{6,6}}")))
        });
        let moved = (0..grid.len())
            .filter(|&i| white.0[i] == 255 && red.0[i] == 255 && (white.1[i] < 128) != (red.1[i] < 128))
            .map(|i| grid[i])
            .collect::<Vec<_>>();
        assert!(moved.is_empty(), "a colour change moved dashes at {moved:?}");
        assert!(white.1.iter().filter(|a| **a < 128).count() > white.0.iter().filter(|a| **a < 128).count() + 100);
    }

    /// The pattern runs along the outer edge clockwise from the top-left corner: on a square box,
    /// `{8}` dashes the top edge at x 0-8, 16-24, ... and the left edge's last gap, y 8-0, takes the
    /// corner square's half below its mitre. Offset 4 moves them back to 0-4, 12-20, one dash across
    /// the corner with the left edge's last 4 px.
    #[test]
    fn a_dashed_border_starts_at_the_top_left_corner() {
        let border = "width=48,height=48,border_width=4,border_color='#ffffff',border_dash={8}";
        let alphas = |src: String, points: &[(usize, usize)]| {
            paint_points(&src, points).expect("headless EGL required").iter().map(|p| p.3).collect::<Vec<_>>()
        };
        let points = [(6, 2), (12, 2), (20, 2), (28, 2), (1, 3), (2, 6), (2, 12)];
        assert_eq!(alphas(format!("rect {{ {border} }}"), &points), [255, 0, 255, 0, 0, 0, 255]);
        let points = [(2, 2), (8, 2), (16, 2), (24, 2), (1, 1), (2, 6), (2, 10)];
        assert_eq!(alphas(format!("rect {{ {border},border_dash_offset=4 }}"), &points), [255, 0, 255, 0, 255, 0, 0]);
        // Placed 8 px in, the dashes move with the box.
        let row = format!("row {{ children = {{ rect {{ width=8,height=8 }}, rect {{ {border} }} }} }}");
        assert_eq!(alphas(row, &[(14, 2), (20, 2), (28, 2), (36, 2)]), [255, 0, 255, 0]);
        // `{6,2}` leaves gaps at 6-8 and 14-16; the cut clears a fractional band's last pixel.
        let points = [(3, 2), (7, 2), (10, 2), (15, 2)];
        assert_eq!(alphas(format!("rect {{ {border},border_dash={{6,2}} }}"), &points), [255, 0, 255, 0]);
        let thin = alphas(format!("rect {{ {border},border_width=3.5 }}"), &[(4, 3), (12, 3)]);
        assert!(thin[0] > 64 && thin[1] == 0, "{thin:?}");
        // A thick top edge keeps its first dash, and the thin right edge its own beside it.
        let thick = alphas(
            format!("rect {{ {border},border_width={{top=12,right=4,bottom=4,left=4}} }}"),
            &[(6, 2), (6, 10), (45, 20), (45, 28)],
        );
        assert_eq!(thick, [255, 255, 255, 0]);
        // An edge without a width cuts nothing from its neighbour's dash.
        assert_eq!(alphas(format!("rect {{ {border},border_width={{top=4}} }}"), &[(0, 2)]), [255]);
        // Zero-length dashes draw nothing, as on a path, past the ceiling too.
        for pattern in ["{0,4}", "{0,1/1024}"] {
            let none =
                alphas(format!("rect {{ {border},border_dash={pattern} }}"), &[(2, 2), (12, 2), (2, 24), (45, 30)]);
            assert_eq!(none, [0; 4], "{pattern}");
        }
        // Under a moved, half-opaque parent the offscreen pass lands where the box is, faded once.
        let src =
            format!("rect {{ width=64,height=64,opacity=0.5,translate={{x=8,y=8}},children={{rect {{ {border} }}}} }}");
        let faded = alphas(src, &[(14, 10), (20, 10), (28, 10), (4, 4)]);
        assert!(
            faded[0].abs_diff(128) <= 2 && faded[1] == 0 && faded[2].abs_diff(128) <= 2 && faded[3] == 0,
            "{faded:?}"
        );
    }

    /// Where the gaps fall, which tiling alone can't show: a dash and its complement still add up
    /// when both cut on the same wrong line.
    #[test]
    fn a_dashed_border_cuts_where_its_pattern_says() {
        let alphas = |src: &str, points: &[(usize, usize)]| {
            paint_points(&format!("rect {{ width=48,height=48,{src} }}"), points)
                .expect("headless EGL required")
                .iter()
                .map(|p| p.3)
                .collect::<Vec<_>>()
        };
        let white = "border_color='#ffffff'";
        // Bands past the box's middle meet there: the one gap clears the top edge's triangle.
        let full = alphas(
            &format!("border_width=48,{white},border_dash={{144,48}},border_dash_offset=144"),
            &[(24, 2), (24, 18), (8, 4), (40, 4), (4, 24), (44, 24), (24, 44)],
        );
        assert_eq!(full, [0, 0, 0, 0, 255, 255, 255], "a full-width border's top gap");
        // An edge with a width but no colour lends no depth: the top's gaps cut straight down.
        let lone = alphas(
            "border_width={top=4,right=20},border_color={top='#ffffff'},border_dash={8,8}",
            &[(30, 1), (30, 3), (34, 1), (34, 3), (38, 3), (42, 1), (42, 3), (46, 1), (46, 3)],
        );
        assert_eq!(lone, [0, 0, 255, 255, 255, 0, 0, 0, 0], "a lone top edge's dashes");
        // Two pairs: 0-4 on, 4-6 off, 6-12 on, 12-16 off, along the top edge above its mitre.
        let pairs = alphas(
            &format!("border_width=4,{white},border_dash={{4,2,6,4}}"),
            &[(2, 1), (5, 1), (9, 1), (14, 1), (18, 1), (21, 1)],
        );
        assert_eq!(pairs, [255, 0, 255, 0, 255, 0]);
        // One gap round the whole contour clears it, but for antialiasing on its inner corners.
        let grid: Vec<_> = (0..48).flat_map(|y| (0..48).map(move |x| (x, y))).collect();
        let gone =
            alphas(&format!("radius=8,border_width=4,{white},border_dash={{1,1000}},border_dash_offset=1"), &grid);
        let most = (0..grid.len()).max_by_key(|&i| gone[i]).unwrap_or(0);
        assert!(gone[most] <= 16, "a gap round the whole border left {} at {:?}", gone[most], grid[most]);
        // A zero dash between two gaps joins them into one.
        for (split, whole) in [("{4,4,0,4}", "{4,8}"), ("{4,0.4,0,4}", "{4,4.4}")] {
            let style = |pattern: &str| format!("radius=8,border_width=4,{white},border_dash={pattern}");
            let (a, b) = (alphas(&style(split), &grid), alphas(&style(whole), &grid));
            let worst = (0..grid.len()).max_by_key(|&i| a[i].abs_diff(b[i])).unwrap_or(0);
            assert!(
                a[worst].abs_diff(b[worst]) <= 8,
                "{split} against {whole} at {:?}: {} {}",
                grid[worst],
                a[worst],
                b[worst]
            );
        }
    }

    /// Dashes and the gaps between them tile the solid border: `{8,8}` and the same moved 8 px add
    /// up to it, so no gap leaves an island of band and none reaches into a dash, whatever the
    /// corners, widths, colours, snapping or clip; gaps under a hundredth of a pixel apart leave next
    /// to nothing. femtovg's antialiasing is not exact area where a dash ends on a slant or a
    /// corner's inner point: the first set misses at most 6.5 px in all, one taper pixel by 183 of
    /// 255; the second, pointed or holeless bands, up to 9.4 px and a handful of whole pixels; the
    /// third, bands about a pixel apart, up to 12.1 px, a quarter of a pixel on their inner edges.
    #[test]
    fn dashes_and_their_gaps_tile_the_solid_border() {
        let grid: Vec<_> = (0..64).flat_map(|y| (0..64).map(move |x| (x, y))).collect();
        let alphas = |src: &str| {
            paint_points(src, &grid).expect("headless EGL required").iter().map(|p| i32::from(p.3)).collect::<Vec<_>>()
        };
        let white = "border_color='#ffffff'";
        let frame = "shape={melt=0,parts={{left=0,top=0,right=48,bottom=12},{left=0,top=36,right=48,bottom=48},\
            {left=0,top=0,right=12,bottom=48},{left=36,top=0,right=48,bottom=48}}}";
        let wedge = "shape={commands={{op='M',points={0,0}},{op='L',points={48,0}},{op='L',points={24,40}},{op='Z',points={}}}}";
        let backward = "shape={commands={{op='M',points={0,0}},{op='L',points={24,40}},{op='L',points={48,0}},{op='Z',points={}}}}";
        let thin_frame = "shape={melt=0,parts={{left=0,top=0,right=48,bottom=10},{left=0,top=38,right=48,bottom=48},\
            {left=0,top=0,right=10,bottom=48},{left=38,top=0,right=48,bottom=48}}}";
        let boxed = |style: &str| format!("rect {{ width=48,height=48,{style} }}");
        // Placed 8 px above the surface, so only the band's last rows show.
        let clipped = |style: &str| format!("rect {{ width=48,height=48,translate={{y=-8}},{style} }}");
        // Pixels in whole: total miss, pixels both halves cut or both keep, a near-all-gap's most left.
        let exact = (8.0, 0, 32);
        let pointed = (12.0, 4, 48);
        let crowded = (12.5, 0, 64);
        let mut failed = Vec::new();
        for (src, (most, owned, left)) in [
            (boxed(&format!("border_width=8,{white}")), exact),
            (boxed(&format!("radius=2,border_width=12,{white}")), exact),
            (boxed(&format!("radius=16,border_width={{top=12,right=2,bottom=2,left=2}},{white}")), exact),
            (boxed(&format!("radius=16,border_width={{top=12}},{white}")), exact),
            (boxed(&format!("border_width={{top=12,right=4,bottom=4,left=4}},{white}")), exact),
            (
                boxed(
                    "border_width={top=12,right=4,bottom=4,left=4},border_color={top='#ffffff',bottom='#ffffff',left='#ffffff'}",
                ),
                exact,
            ),
            (boxed(&format!("border_width=0.1,{white}")), exact),
            (boxed(&format!("radius=8,border_width=0.25,{white}")), exact),
            (
                boxed(
                    "radius=2,border_width=8,border_color={top='#ff000080',right='#ffffff80',bottom='#ffffff80',left='#ffffff80'}",
                ),
                exact,
            ),
            (
                boxed("radius=8,border_width=4,border_color={gradient='linear',stops={{0,'#ff0000'},{1,'#0000ff'}}}"),
                exact,
            ),
            (clipped(&format!("border_width={{top=12,right=4,bottom=4,left=4}},{white}")), exact),
            (clipped(&format!("{frame},border_width=6,{white}")), exact),
            // Two contours whose bands meet, one starting partway along an edge.
            (boxed(&format!("{frame},border_width=6,{white}")), exact),
            // Bands a pixel or so apart, which the inward reach must not cross.
            (boxed(&format!("{frame},border_width=5.45,{white}")), crowded),
            (boxed(&format!("{thin_frame},border_width=4.5,{white}")), crowded),
            // Chords under a pixel round a tight corner.
            (boxed(&format!("radius=2,border_width=1,{white}")), exact),
            // An inner corner under a pixel round, whose moved inner points pass each other.
            (boxed(&format!("radius=8.5,border_width=8,{white}")), exact),
            (boxed(&format!("radius=12,corner_shape='scoop',border_width=4,{white}")), pointed),
            (boxed(&format!("radius=12,corner_shape='scoop',border_width={{top=4}},{white}")), pointed),
            (
                boxed(&format!(
                    "radius=12,corner_smoothing=0.6,border_width={{top=8,right=4,bottom=4,left=2}},{white}"
                )),
                pointed,
            ),
            // No hole: the inner points meet in the middle.
            (boxed(&format!("radius=10,border_width=24,{white}")), pointed),
            (boxed(&format!("{wedge},border_width=6,{white}")), pointed),
            // Wound the other way.
            (boxed(&format!("{backward},border_width=6,{white}")), pointed),
        ] {
            let solid = alphas(&src);
            let dashed = |pattern: &str| alphas(&src.replacen(" }", &format!(",{pattern} }}"), 1));
            for offset in [0, 12] {
                let on = dashed(&format!("border_dash={{8,8}},border_dash_offset={offset}"));
                let off = dashed(&format!("border_dash={{8,8}},border_dash_offset={}", offset + 8));
                assert!(on.iter().sum::<i32>() > 0 && off.iter().sum::<i32>() > 0, "{src}: both halves paint");
                let missed = (0..grid.len()).map(|i| (on[i] + off[i] - solid[i]).abs()).sum::<i32>() as f32 / 255.0;
                let wrong: Vec<_> = (0..grid.len())
                    .filter(|&i| solid[i] >= 200 && (on[i] + off[i] <= 55 || on[i].min(off[i]) >= 200))
                    .map(|i| (grid[i], solid[i], on[i], off[i]))
                    .collect();
                if missed > most || wrong.len() > owned {
                    failed.push(format!("{src} at {offset}: {missed:.1} px off, owned wrong at {wrong:?}"));
                }
            }
            let gaps = dashed("border_dash={0.01,7.99}");
            let at = (0..grid.len()).max_by_key(|&i| gaps[i]).unwrap_or(0);
            if gaps[at] > left {
                failed.push(format!("{src}: a gap left {} at {:?}", gaps[at], grid[at]));
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
        // Under the dash ceiling but past it in gaps: solid, not erased.
        let src = boxed(&format!("border_width=4,{white}"));
        let past = alphas(&src.replacen(" }", ",border_dash={1/1024,1/1024,0,1/512} }", 1));
        assert_eq!(past, alphas(&src));
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

    /// A gradient border runs along the whole outline on every path `paint_border` takes: the
    /// stroke (uniform, rounded), the shaped band (per-edge width, smoothed corners, scoop) and the
    /// four rects (radius 0). Opposite edges take opposite end stops and the inside stays empty.
    #[test]
    fn a_gradient_border_follows_the_outline_on_every_border_path() {
        let gradient = r##"border_color = { gradient = "linear", angle = 90,
            stops = { { 0, "#FF0000" }, { 1, "#0000FF" } } }"##;
        for (name, shape) in [
            ("stroke", "radius = 8, border_width = 4"),
            ("rects", "border_width = 4"),
            ("per-edge band", "radius = 8, border_width = { left = 4, right = 4, top = 4, bottom = 2 }"),
            (
                "smoothed band",
                "radius = 12, corner_smoothing = 1, border_width = { left = 4, right = 4, top = 4, bottom = 2 }",
            ),
            ("scoop band", "radius = 8, corner_shape = \"scoop\", border_width = 4"),
        ] {
            let child = format!("rect {{ width = 40, height = 40, {shape}, {gradient} }}");
            let Some(px) = paint_points(&child, &[(1, 20), (38, 20), (20, 20)]) else { return };
            assert!(
                px[0].3 > 245 && px[0].0 > 220 && px[0].2 < 40,
                "{name}: left edge is the first stop, got {:?}",
                px[0]
            );
            assert!(
                px[1].3 > 245 && px[1].2 > 220 && px[1].0 < 40,
                "{name}: right edge is the last stop, got {:?}",
                px[1]
            );
            assert_eq!(px[2].3, 0, "{name}: the inside stays empty");
        }
    }

    /// A scoop's border follows its arcs: the cut corner stays empty, the band runs along the arc,
    /// and a translucent band is painted once. The four-rectangle fallback drew a square frame
    /// across every scoop.
    #[test]
    fn a_scooped_border_follows_the_scoop_and_paints_once() {
        let child = r##"rect { width = 40, height = 40, radius = 12, corner_shape = "scoop",
            border_width = 3, border_color = "#FF000080" }"##;
        let Some(px) = paint_points(child, &[(1, 1), (9, 9), (20, 1), (1, 20), (20, 20)]) else { return };
        assert_eq!(px[0].3, 0, "the cut corner stays empty, got {:?}", px[0]);
        for (at, p) in [((9, 9), px[1]), ((20, 1), px[2]), ((1, 20), px[3])] {
            assert!(p.0 == p.3 && p.1 == 0 && (120..=136).contains(&p.3), "{at:?} is band, painted once, got {p:?}");
        }
        assert_eq!(px[4].3, 0, "inside the band is the hole");
    }

    /// A per-edge border on a rounded box bends round the corners as CSS draws it: a lone bottom
    /// edge curves up both corners and nothing paints outside the rounding, and each edge keeps
    /// its own colour.
    #[test]
    fn a_per_edge_border_on_a_rounded_box_follows_the_corners() {
        let child = r##"rect { width = 40, height = 40, radius = 12,
            border_width = { bottom = 3 }, border_color = "#FF0000" }"##;
        let Some(px) = paint_points(child, &[(0, 39), (20, 38), (20, 1), (39, 39)]) else { return };
        assert_eq!(px[0].3, 0, "outside the rounded corner, got {:?}", px[0]);
        assert_eq!(px[1], (255, 0, 0, 255));
        assert_eq!(px[2].3, 0, "the top declared no width");
        assert_eq!(px[3].3, 0, "outside the other corner, got {:?}", px[3]);

        let child = r##"rect { width = 40, height = 40, radius = 8, border_width = 4,
            border_color = { top = "#FF0000", bottom = "#0000FF" } }"##;
        let Some(px) = paint_points(child, &[(20, 1), (20, 38), (1, 20)]) else { return };
        assert_eq!(px[0], (255, 0, 0, 255));
        assert_eq!(px[1], (0, 0, 255, 255));
        assert_eq!(px[2].3, 0, "the left edge has no colour");
    }

    /// A pill's shape. `radius = side / 2`, and a radius over half the side, both reached femtovg's
    /// half-box clamp, whose fill tessellation collapses to a rectangle: every pill and circle had
    /// a square ground under a round border.
    ///
    /// The degeneracy was size-dependent (40x40 at 20 failed, 32x32 did not), so both sizes stay.
    /// This catches an exact half radius; the companion test catches the just-below-half fold.
    /// The corner must show the parent through the fill while the mid-edge stays border.
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

    /// Regression test: a border at a fractional position must land on exactly its width in physical
    /// pixel rows, not blur across more. `padding.top = 10.3` puts the bordered rect's absolute y at
    /// a fractional offset -- unsnapped, femtovg's own antialiasing fills part of row 10 and part
    /// of row 11 at partial coverage instead of one row at full coverage.
    ///
    /// The 4px row must stay exactly four rows, which pins `snap_border_band` rounding both edges
    /// independently rather than rounding the near edge and adding an unrounded thickness.
    /// `padding.top = 31.3` is the geometry a content-sized surface resolves to.
    ///
    /// This is the filled-edge branch, not the stroke; stroke parity is covered by
    /// `a_uniform_stroked_border_at_a_fractional_position_covers_whole_pixel_columns` below.
    #[test]
    fn a_border_at_a_fractional_position_is_exactly_its_width_in_pixel_rows() {
        // (border width, padding.top, first white row)
        for (width, padding, first) in [(1usize, "10.3", 10usize), (4, "31.3", 31)] {
            let Some(instance) = init_headless_egl(64, 64) else { return };
            let lua = Lua::new();
            let shaping = ShapingHandle::spawn();
            let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

            let root = resolved_surface(
                &lua,
                &format!(
                    r##"return panel {{ id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = {{ top = {padding} }}, child = rect {{
                    width = 30, height = 20, background = "#000000FF",
                    border_width = {{ top = {width} }}, border_color = {{ top = "#FFFFFFFF" }},
                }} }}"##
                ),
                LogicalSize { width: 64.0, height: 64.0 },
            );
            paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

            assert_eq!(pixel_at(painter.canvas_mut(), 15, first - 1), (255, 0, 0, 255), "{width}px: row above");
            for y in first..first + width {
                assert_eq!(
                    pixel_at(painter.canvas_mut(), 15, y),
                    (255, 255, 255, 255),
                    "{width}px: row {y} is not fully white"
                );
            }
            assert_eq!(pixel_at(painter.canvas_mut(), 15, first + width), (0, 0, 0, 255), "{width}px: row below");
        }
    }

    /// The uniform-width-with-radius branch of `paint_border` strokes rather than fills, and a
    /// stroke is where physical-pixel parity actually bites: femtovg centres a stroke on its
    /// path, so a 4-wide stroke centred on a half-integer spreads across five rows at partial
    /// coverage while the same stroke centred on an integer covers exactly four. Snapping the
    /// box span and the thickness as bands is what puts the centreline on the right side of that
    /// split without the caller reasoning about parity at all.
    ///
    /// `padding.left = 10.3` matters: with an integer padding the stroke already lands on whole
    /// pixels and passes without any snapping.
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

    // ---- `clip = "rounded"` ----

    /// A `width = "fill"` circle in a tweening cell lands a rounding error narrower than its
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
            fill_rect(canvas, LogicalRect { x: 8.0, y: 8.0, width: w, height: 32.0 }, &Radii::from(17.0), white);
            canvas.flush();
            assert_eq!(pixel_at(canvas, 9, 9), (0, 0, 0, 255), "{name}: the corner outside the circle stays black");
            assert_eq!(pixel_at(canvas, 24, 24), (255, 255, 255, 255), "{name}: the centre is filled");
        }
    }

    /// Unequal corners at exactly half the box take `rounded_rect_varying`, not the pill special cases.
    #[test]
    fn unequal_corners_at_exactly_half_leave_no_seam() {
        for h in [32, 33] {
            let src = format!(
                r##"rect {{ width = 32, height = {h}, background = "#FFFFFF", radius = {{ top_left = 16, bottom_left = 16 }} }}"##
            );
            let Some(px) = paint_points(&src, &[(1, h / 2), (6, 6), (6, h - 7)]) else { return };
            assert!(px.iter().all(|p| *p == (255, 255, 255, 255)), "h={h}: {px:?}");
        }
    }

    /// A smoothed corner reaches further along the side than the circle and so leaves pixel (12, 0)
    /// of a 64 px box at radius 16, which the circle covers; fill, border band and clip share it.
    #[test]
    fn a_smoothed_corner_cuts_what_the_circle_covers_in_fill_border_and_clip() {
        let opaque = |smoothing: &str, child: &str| {
            let corner = format!("width = 64, height = 64, radius = 16, {smoothing}");
            let src = child.replace("CORNER", &corner);
            paint_points(&src, &[(12, 0), (32, 32)]).map(|px| (px[0].3, px[1].3))
        };
        let fill = r##"rect { CORNER, background = "#000000FF" }"##;
        let border = r##"rect { CORNER, border_width = { top = 4, left = 2, right = 4, bottom = 2 }, border_color = "#FFFFFFFF" }"##;
        let clip = r##"rect { CORNER, clip = "rounded",
            children = { rect { width = "fill", height = "fill", background = "#FF0000FF" } } }"##;
        for (name, child, inside) in [("fill", fill, 255), ("border", border, 0), ("clip", clip, 255)] {
            let Some(circle) = opaque("corner_smoothing = 0", child) else { return };
            let smooth = opaque("corner_smoothing = 1", child).unwrap();
            assert!(circle.0 > 128, "{name}: the circle covers (12, 0), got {circle:?}");
            assert!(smooth.0 < 64, "{name}: the smoothed corner leaves it, got {smooth:?}");
            assert_eq!((circle.1, smooth.1), (inside, inside), "{name}: the middle is unchanged");
        }
        // Wound like `rounded_rect`, a translucent fill blends once across the transitions, the arc and the straights.
        let ghost =
            r##"rect { width = 64, height = 64, radius = 16, corner_smoothing = 0.6, background = "#00000080" }"##;
        let px = paint_points(ghost, &[(6, 6), (14, 2), (24, 1), (32, 32), (1, 24)]).unwrap();
        assert!(px.iter().all(|p| (126..=130).contains(&p.3)), "painted once, got {px:?}");
    }

    /// A `background` list paints bottom-up, so the first layer is on top, through the same box
    /// path: a gradient layer over a colour, and a smoothed outline cutting every layer.
    #[test]
    fn background_layers_composite_first_on_top_inside_the_outline() {
        let at = |background: &str, extra: &str, points: &[(usize, usize)]| {
            let src = format!(r##"rect {{ width = 64, height = 64, background = {background}{extra} }}"##);
            paint_points(&src, points)
        };
        let Some(px) = at(r##"{ "#FF0000BF", "#0000FFFF" }"##, "", &[(32, 32)]) else { return };
        let (red, blue) = (px[0].0 as i32, px[0].2 as i32);
        assert!((red - 191).abs() <= 2 && (blue - 64).abs() <= 2 && px[0].3 == 255, "red over blue: {px:?}");
        let flipped = at(r##"{ "#0000FFFF", "#FF0000BF" }"##, "", &[(32, 32)]).unwrap();
        assert_eq!((flipped[0].0, flipped[0].2), (0, 255), "an opaque first layer hides the rest: {flipped:?}");

        let ramp = r##"{ { gradient = "linear", angle = 90, stops = { { 0, "#FF000000" }, { 1, "#FF0000FF" } } }, { fill = "#0000FFFF" } }"##;
        let px = at(ramp, "", &[(2, 32), (61, 32)]).unwrap();
        assert!(px[0].0 < 40 && px[0].2 > 215 && px[1].0 > 215 && px[1].2 < 40, "ramp over blue: {px:?}");

        let both = r##"{ "#00000080", "#00000080" }"##;
        let px = at(both, ", radius = 16, corner_smoothing = 1", &[(12, 0), (32, 32)]).unwrap();
        assert!(px[0].3 < 64, "the smoothed corner cuts both layers, got {px:?}");
        assert!((187..=195).contains(&px[1].3), "two half layers leave 75%, got {px:?}");
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
        fill_rect(canvas, LogicalRect { x: 24.0, y: 8.0, width: 0.0, height: 32.0 }, &Radii::from(6.0), white);
        canvas.flush();
        for x in 22..27 {
            assert_eq!(pixel_at(canvas, x, 24), (0, 0, 0, 255), "column {x} stays black");
        }
    }

    #[test]
    fn a_gradient_holds_its_last_stop_colour_past_it() {
        let src = r##"return panel { id = "bar", width = 100, height = 10, child = rect { width = 100, height = 10,
            background = { gradient = "linear", angle = 90, stops = { { 0, "#FF0000" }, { 0.5, "#0000FF" } } } } }"##;
        let Some(px) = super::super::tests::paint_with_gl(src, (100, 10), &[(75, 5), (99, 5)]) else { return };
        assert_eq!(px, [(0, 0, 255, 255); 2], "past the 0.5 stop the gradient stays blue");
    }
}

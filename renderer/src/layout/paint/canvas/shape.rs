//! Box paths, fills, gradients and borders.

use std::f32::consts::{FRAC_PI_2, PI};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, Paint, Path, Solidity};

use super::vector_path::femtovg_path;
use crate::layout::node::corner::Squircle;
use crate::layout::node::outline::inset;
use crate::layout::node::{BorderColor, BorderPaint, EdgeInsets, Fill, Gradient, GradientShape, Radii, Rgba};
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
/// still_a_circle`). Tweens produce exactly that: a `width = "fill"` circle inside a cell whose
/// width and padding both ease lands a rounding error either side of its height on different
/// frames, and the narrow frames flashed as squares.
///
/// Unequal corners go to femtovg's `rounded_rect_varying`, which shrinks them by CSS's rule; a
/// scoop and a smoothed corner take the same rule from [`Radii::fit`].
pub(super) fn box_path(rect: LogicalRect, radii: &Radii) -> Path {
    if let Some(outline) = &radii.2 {
        return femtovg_path(&outline.bez(rect), std::iter::empty());
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
    let stops = gradient.stops.iter().map(|(at, c)| (*at, Color::from(*c)));
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
    scale: f32,
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
            let mut bez = outline.bez(rect);
            bez.extend(inset(&outline.polygon(rect, 0.1), f64::from(widths.top)));
            canvas.fill_path(&femtovg_path(&bez, [false, true].into_iter()), &paint);
        }
        return;
    }

    if let Some(mut paint) = uniform_paint
        && uniform_width
        && widths.top > 0.0
        && !radius.is_zero()
        && !radius.scoop()
    {
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
        paint.set_line_width(width);
        canvas.stroke_path(&path, &paint);
        return;
    }

    if !radius.is_zero() {
        shaped_border(canvas, rect, radius, border, widths, scale);
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
        paint_border_edge(canvas, edge_paint(color), thickness, edge_rect, axis, scale);
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
    scale: f32,
) {
    let (x, w) = snap_border_band(rect.x, rect.width, scale);
    let (y, h) = snap_border_band(rect.y, rect.height, scale);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let thick = |width: f32| snap_border_band(rect.x, width, scale).1;
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
    let radius = match radius.scoop() {
        false => radius.fit(w, h),
        true => radius.fit(w - 2.0 * HAIR, h - 2.0 * HAIR),
    };
    let [tl, tr, br, bl] = radius.0;
    let [q_tl, q_tr, q_br, q_bl] = radius.squircles(w, h);
    let [top, right, bottom, left] = edges;
    let corners = [
        Corner::new((x, y), (1.0, 1.0), false, tl, q_tl, left, top),
        Corner::new((x + w, y), (-1.0, 1.0), true, tr, q_tr, right, top),
        Corner::new((x + w, y + h), (-1.0, -1.0), false, br, q_br, right, bottom),
        Corner::new((x, y + h), (1.0, -1.0), true, bl, q_bl, left, bottom),
    ];
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
        let color = drawn[start].expect("the filter keeps drawn edges");
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
    scale: f32,
) {
    let Some(paint) = paint else { return };
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
    canvas.fill_path(&path, &paint);
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
}

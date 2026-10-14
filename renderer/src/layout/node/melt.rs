//! `shape = { melt = k, parts = { .. } }`: rounded boxes joined by a round union, the fillet where
//! two meet about `k` px, traced to closed polylines that paint, hit testing and regions all read.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use kurbo::{Affine, BezPath, Line, ParamCurveNearest, Point, Rect, Shape as _, Vec2};

use super::shape::{Coord, SDF_POINTS, closed, place, winding};
use super::vector_path::COORD_MAX;
use super::{LayoutError, invalid};
use crate::lua::luacats::lua_shape;
use crate::text::snap::LogicalRect;

lua_shape! {
    /// A rounded box the melt joins, each edge a `ShapePoint`; `radius` is held to half the shorter side, so `999` makes a circle or pill.
    #[alias = "ShapePart"]
    pub(crate) struct PartInput {
        left: Coord,
        top: Coord,
        right: Coord,
        bottom: Coord,
        radius: Option<f32>,
    }
}

/// A grid edge by its top or left node and whether it runs across.
type Edge = (i64, i64, bool);

/// One part: left, top, right and bottom in px plus a share of the box's width or height, then
/// its corner radius.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Part {
    px: [f32; 4],
    shares: [f32; 4],
    radius: f32,
}

/// One trace, in the parts' own px with the box at the origin.
#[derive(Debug)]
pub(super) struct Traced {
    size: (f32, f32),
    pub(super) loops: Vec<Vec<Point>>,
    /// Whether each loop winds against the biggest, bounding a void.
    pub(super) voids: Vec<bool>,
    pub(super) path: BezPath,
    pub(super) bounds: Rect,
}

/// Parts joined by a round union of fillet radius `radius`.
#[derive(Debug, Clone)]
pub struct Melt {
    radius: f32,
    parts: Vec<Part>,
    /// Display scaling: a scaled copy keeps the parts in config px, so it shares `traced`.
    scale: f32,
    /// The two sizes last traced, newest first: paint's box and the region's snapped one.
    traced: Rc<RefCell<Vec<Rc<Traced>>>>,
}

impl PartialEq for Melt {
    fn eq(&self, other: &Self) -> bool {
        (self.radius, self.scale) == (other.radius, other.scale) && self.parts == other.parts
    }
}

// ponytail: 8 parts and a 256 px melt; the trace costs one rounded-box distance per part per grid node.
pub(super) const MAX_PARTS: usize = 8;
const MAX_MELT: f32 = 256.0;
/// Grid px, plus a grid line through the middle of each part under twice that thick; a part
/// thinner than twice `FINEST` is left out, so every part drawn holds a grid node and grows from it.
const STEP: f64 = 4.0;
const FINEST: f64 = 0.125;
// ponytail: traced up to 1024 px past the box, a straight cut beyond, so a part flung to the
// coordinate bound costs its visible edge only; a wider reach needs the surface's size here.
const REACH: f64 = 1024.0;
/// How far a chord of the traced outline may stray from it, px.
const TOLERANCE: f64 = 0.02;

impl Melt {
    pub(super) fn read(name: &str, radius: f32, parts: Vec<PartInput>) -> Result<Self, LayoutError> {
        if !(0.0..=MAX_MELT).contains(&radius) {
            return Err(invalid(name, format!("`melt` is px in [0, {MAX_MELT}]")));
        }
        if parts.is_empty() {
            return Err(invalid(name, "`parts` holds at least one box"));
        }
        let parts = parts.into_iter().enumerate().map(|(i, PartInput { left, top, right, bottom, radius })| {
            let name = format!("{name}.parts[{}]", i + 1);
            let edges = [left, top, right, bottom];
            if edges.iter().enumerate().any(|(at, edge)| edge.axis.is_some_and(|axis| axis != at % 2)) {
                return Err(invalid(&name, "left and right are x, top and bottom are y"));
            }
            let radius = radius.unwrap_or(0.0);
            if !(0.0..=COORD_MAX).contains(&radius) {
                return Err(invalid(&name, format!("`radius` is px in [0, {COORD_MAX}]")));
            }
            Ok(Part { px: edges.map(|edge| edge.px), shares: edges.map(|edge| edge.share), radius })
        });
        let parts = parts.collect::<Result<_, _>>()?;
        Ok(Self { radius, parts, scale: 1.0, traced: Rc::default() })
    }

    /// `self` moved toward `to` by `t`, or `None` when their part counts differ and the tween snaps.
    pub fn lerp(&self, to: &Self, t: f32) -> Option<Self> {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        let parts = self.parts.iter().zip(&to.parts).map(|(a, b)| Part {
            px: std::array::from_fn(|i| mix(a.px[i], b.px[i])),
            shares: std::array::from_fn(|i| mix(a.shares[i], b.shares[i])),
            radius: mix(a.radius, b.radius).max(0.0),
        });
        (self.parts.len() == to.parts.len()).then(|| Self {
            radius: mix(self.radius, to.radius).max(0.0),
            parts: parts.collect(),
            scale: self.scale,
            traced: Rc::default(),
        })
    }

    /// `self` drawn `k` times as large, sharing its traces.
    pub fn scaled(&self, k: f32) -> Self {
        Self { scale: self.scale * k, ..self.clone() }
    }

    /// From the parts' px at the origin to a box at `rect`.
    pub(super) fn placed(&self, rect: LogicalRect) -> Affine {
        let s = f64::from(self.scale);
        Affine::new([s, 0.0, 0.0, s, rect.x.into(), rect.y.into()])
    }

    /// The trace for a box at `rect`, from the cache when its size was traced last or the time before.
    pub(super) fn traced(&self, rect: LogicalRect) -> Rc<Traced> {
        let size = (rect.width / self.scale, rect.height / self.scale);
        let mut cache = self.traced.borrow_mut();
        let same = |t: &&Rc<Traced>| (t.size.0 - size.0).abs() < 1e-3 && (t.size.1 - size.1).abs() < 1e-3;
        let traced = cache.iter().find(same).cloned().unwrap_or_else(|| Rc::new(self.trace(size)));
        cache.retain(|t| !Rc::ptr_eq(t, &traced));
        cache.insert(0, traced.clone());
        cache.truncate(2);
        traced
    }

    /// The biggest loop at `rect`, its tolerance doubled until [`SDF_POINTS`] vertices fit.
    pub(super) fn sdf_loop(&self, rect: LogicalRect) -> Vec<Point> {
        let traced = self.traced(rect);
        let area = |l: &&Vec<Point>| closed(l.iter().copied()).area().abs();
        let Some(biggest) = traced.loops.iter().max_by(|a, b| area(a).total_cmp(&area(b))) else { return Vec::new() };
        let (mut out, mut tolerance) = (biggest.clone(), TOLERANCE);
        while out.len() > SDF_POINTS && tolerance < 1e3 {
            tolerance *= 2.0;
            out = simplify(biggest, tolerance);
        }
        out.into_iter().map(|p| self.placed(rect) * p).collect()
    }

    /// Each part as centre, half size and corner radius, placed on a box of `size` at the origin.
    fn boxes(&self, size: (f32, f32)) -> Vec<[f64; 5]> {
        let boxes = self.parts.iter().map(|part| {
            let e: [f64; 4] = std::array::from_fn(|i| {
                let share = if i % 2 == 0 { [part.shares[i], 0.0] } else { [0.0, part.shares[i]] };
                place(part.px[i], share, size).into()
            });
            let (hx, hy) = ((e[2] - e[0]).abs() / 2.0, (e[3] - e[1]).abs() / 2.0);
            [(e[0] + e[2]) / 2.0, (e[1] + e[3]) / 2.0, hx, hy, f64::from(part.radius).min(hx).min(hy)]
        });
        boxes.filter(|b| b[2].min(b[3]) >= FINEST).collect()
    }

    /// Marching squares over the round union, in the cells of a quadtree that culls those too far
    /// from its zero to hold any, each crossing projected onto the zero and each loop flattened to
    /// chords within [`TOLERANCE`].
    fn trace(&self, size: (f32, f32)) -> Traced {
        let (boxes, k) = (self.boxes(size), f64::from(self.radius));
        let clip = Rect::new(-REACH, -REACH, f64::from(size.0) + REACH, f64::from(size.1) + REACH);
        let f = |p: Point| {
            let outside =
                Vec2::new((p.x - clip.center().x).abs(), (p.y - clip.center().y).abs()) - clip.size().to_vec2() / 2.0;
            field(&boxes, k, p).max(outside.x.max(outside.y))
        };
        // A fillet bridging two boxes a hair apart bulges under 0.3 k past them.
        let reach = boxes.iter().fold(Rect::new(0.0, 0.0, -1.0, -1.0), |r, b| {
            let part = Rect::new(b[0] - b[2], b[1] - b[3], b[0] + b[2], b[1] + b[3]);
            if r.width() < 0.0 { part } else { r.union(part) }
        });
        let reach = reach.inflate(0.3 * k + 2.0 * STEP, 0.3 * k + 2.0 * STEP).intersect(clip.inflate(STEP, STEP));
        // A rectilinear grid: every STEP px, and through the middle of each thin part on its thin
        // axis, so a thin part costs its own perimeter at STEP rather than a finer grid everywhere.
        let lines = |lo: f64, hi: f64, axis: usize| {
            let coarse = ((lo / STEP).floor() as i64..=(hi / STEP).ceil() as i64).map(|i| i as f64 * STEP);
            let thin = boxes.iter().filter(|b| b[2 + axis] < STEP).map(|b| b[axis]);
            let mut lines: Vec<f64> = coarse.chain(thin.filter(|c| (lo..=hi).contains(c))).collect();
            lines.sort_by(f64::total_cmp);
            lines.dedup();
            lines
        };
        let (xs, ys) = (lines(reach.x0, reach.x1, 0), lines(reach.y0, reach.y1, 1));
        let at = |x: i64, y: i64| Point::new(xs[(x as usize).min(xs.len() - 1)], ys[(y as usize).min(ys.len() - 1)]);
        let (sx, sy) = (xs.len() as i64 - 1, ys.len() as i64 - 1);
        // The field changes at most sqrt(parts) per px, so a cell whose centre reads further from
        // zero than that times its half diagonal holds none of the outline.
        let lipschitz = (boxes.len() as f64).sqrt();
        // Each crossing edge to its point and the edge the outline leaves its cell by.
        let mut next: BTreeMap<Edge, (Point, Edge)> = BTreeMap::new();
        let mut cells = if boxes.is_empty() {
            Vec::new()
        } else {
            vec![(0, 0, (sx.max(sy).max(2) as u64).next_power_of_two() as i64)]
        };
        while let Some((x, y, n)) = cells.pop() {
            let cell = Rect::from_points(at(x, y), at(x + n, y + n));
            if x >= sx || y >= sy || f(cell.center()).abs() > lipschitz * cell.size().to_vec2().hypot() / 2.0 {
                continue;
            }
            if n > 2 {
                let m = n / 2;
                cells.extend([(x, y, m), (x + m, y, m), (x, y + m, m), (x + m, y + m, m)]);
                continue;
            }
            let values: [f64; 9] = std::array::from_fn(|i| f(at(x + (i % 3) as i64, y + (i / 3) as i64)));
            #[cfg(test)]
            CELLS.set(CELLS.get() + 1);
            for (a, b) in [(0, 0), (1, 0), (0, 1), (1, 1)].into_iter().filter(|&(a, b)| x + a < sx && y + b < sy) {
                let node = |da: i64, db: i64| (x + a + da, y + b + db, values[((b + db) * 3 + a + da) as usize]);
                // Clockwise from the top left, y down; `true` where the walk steps inside.
                let corners = [node(0, 0), node(1, 0), node(1, 1), node(0, 1)];
                let crossings: Vec<_> = (0..4)
                    .map(|e| (corners[e], corners[(e + 1) % 4]))
                    .filter(|(p, q)| (p.2 < 0.0) != (q.2 < 0.0))
                    .map(|(p, q)| {
                        let (lo, hi) = if (p.1, p.0) < (q.1, q.0) { (p, q) } else { (q, p) };
                        ((lo.1, lo.0, lo.1 == hi.1), q.2 < 0.0, lo, hi)
                    })
                    .collect();
                let n = crossings.len();
                let centre_inside = n == 4 && f(at(x + a, y + b).midpoint(at(x + a + 1, y + b + 1))) < 0.0;
                for (i, &(edge, entering, lo, hi)) in crossings.iter().enumerate() {
                    if !entering {
                        continue;
                    }
                    // A saddle pairs round whichever of its corners the centre does not join.
                    let partner = match (n, centre_inside) {
                        (2, _) => 1 - i,
                        (_, true) => (i + 3) % 4,
                        _ => (i + 1) % 4,
                    };
                    let start = at(lo.0, lo.1).lerp(at(hi.0, hi.1), lo.2 / (lo.2 - hi.2));
                    next.insert(edge, (project(&f, start), crossings[partner].0));
                }
            }
        }
        let mut loops = Vec::new();
        while let Some((&start, _)) = next.first_key_value() {
            let (mut points, mut edge) = (Vec::new(), start);
            while let Some((p, to)) = next.remove(&edge) {
                if points.last().is_none_or(|last: &Point| last.distance(p) >= FINEST / 4.0) {
                    points.push(p);
                }
                edge = to;
            }
            if edge == start && points.len() >= 3 {
                let mut fine = Vec::with_capacity(points.len());
                for (i, &a) in points.iter().enumerate() {
                    fine.push(a);
                    split(&f, a, points[(i + 1) % points.len()], 6, &mut fine);
                }
                let simple = simplify(&fine, TOLERANCE);
                if simple.len() >= 3 {
                    loops.push(simple);
                }
            }
        }
        let side = winding(&loops);
        let voids = loops.iter().map(|l| closed(l.iter().copied()).area().signum() != side).collect();
        let path: BezPath = loops.iter().flat_map(|l| closed(l.iter().copied())).collect();
        // No outline, as when every part is too thin to draw: the box itself, not a rect at the origin.
        let bounds =
            if loops.is_empty() { Rect::new(0.0, 0.0, size.0.into(), size.1.into()) } else { path.bounding_box() };
        Traced { size, voids, bounds, path, loops }
    }
}

/// The round union of the parts' exact distances: each part's own edge where no other is within
/// `k`, a fillet tangent to both where two are (hg_sdf's `fOpUnionRound`, over any number).
fn field(boxes: &[[f64; 5]], k: f64, p: Point) -> f64 {
    let (mut nearest, mut reach) = (f64::INFINITY, 0.0);
    for &[cx, cy, hx, hy, r] in boxes {
        let q = Vec2::new((p.x - cx).abs() - hx + r, (p.y - cy).abs() - hy + r);
        let d = Vec2::new(q.x.max(0.0), q.y.max(0.0)).hypot() + q.x.max(q.y).min(0.0) - r;
        nearest = f64::min(nearest, d);
        reach += (k - d).max(0.0).powi(2);
    }
    nearest.max(k) - f64::sqrt(reach)
}

/// `p` moved one Newton step onto the zero of `f`.
fn project(f: &impl Fn(Point) -> f64, p: Point) -> Point {
    const E: f64 = 1e-3;
    let g = Vec2::new(
        f(p + Vec2::new(E, 0.0)) - f(p - Vec2::new(E, 0.0)),
        f(p + Vec2::new(0.0, E)) - f(p - Vec2::new(0.0, E)),
    ) / (2.0 * E);
    if g.hypot2() < 1e-9 { p } else { p - g * f(p) / g.hypot2() }
}

/// The outline between `a` and `b`, both on it, pushed as points until each chord is within
/// [`TOLERANCE`]: the grid finds the outline, this flattens it as finely as a curve needs.
fn split(f: &impl Fn(Point) -> f64, a: Point, b: Point, depth: u32, out: &mut Vec<Point>) {
    let mid = project(f, a.midpoint(b));
    if depth == 0 || Line::new(a, b).nearest(mid, 1e-9).distance_sq <= TOLERANCE * TOLERANCE {
        return;
    }
    split(f, a, mid, depth - 1, out);
    out.push(mid);
    split(f, mid, b, depth - 1, out);
}

/// The closed `points` as the fewest chords each within `tolerance` of what it skips, a straight
/// run cut every 16 points.
fn simplify(points: &[Point], tolerance: f64) -> Vec<Point> {
    let n = points.len();
    let (mut out, mut anchor) = (vec![points[0]], 0);
    for j in 2..=n {
        let chord = Line::new(points[anchor], points[j % n]);
        let strays = |m: &usize| chord.nearest(points[*m], 1e-9).distance_sq > tolerance * tolerance;
        if j - anchor > 16 || (anchor + 1..j).any(|m| strays(&m)) {
            anchor = j - 1;
            out.push(points[anchor]);
        }
    }
    out
}

#[cfg(test)]
thread_local! {
    /// Quadtree leaves traced on this thread, for the cost test.
    static CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Two 40 px bubbles `gap` px apart, melted by 16 px.
#[cfg(test)]
pub(crate) fn bubbles(gap: f64) -> Rc<super::Shape> {
    super::shape::parse(&format!(
        "{{ melt = 16, parts = {{ {{ left = 0, top = 0, right = 40, bottom = 40, radius = 20 }}, \
         {{ left = {0}, top = 0, right = {1}, bottom = 40, radius = 20 }} }} }}",
        40.0 + gap,
        80.0 + gap
    ))
    .unwrap()
    .unwrap()
}

/// Four 8 px bars melted by 2 px round the edges of the box, leaving a void in its middle.
#[cfg(test)]
pub(crate) const RING: &str = "{ melt = 2, parts = { { left = 0, top = 0, right = '100%', bottom = 8 }, \
    { left = 0, top = { from = 'bottom', px = -8 }, right = '100%', bottom = '100%' }, \
    { left = 0, top = 0, right = 8, bottom = '100%' }, \
    { left = { from = 'right', px = -8 }, top = 0, right = '100%', bottom = '100%' } } }";

#[cfg(test)]
mod tests {
    use std::f64::consts::{PI, TAU};

    use super::super::shape::{Shape, parse};
    use super::*;

    const PANEL: LogicalRect = LogicalRect { x: 0.0, y: 0.0, width: 300.0, height: 200.0 };

    fn shape(src: &str) -> Rc<Shape> {
        parse(src).unwrap().unwrap()
    }

    /// A circle tab of radius `r` at the top left, melted into a body from its centre to `w` by `h`.
    fn tab(r: f64, w: f64, h: f64, rb: f64, melt: f64) -> Rc<Shape> {
        shape(&format!(
            "{{ melt = {melt}, parts = {{ {{ left = 0, top = 0, right = {d}, bottom = {d}, radius = {r} }}, \
             {{ left = {r}, top = {r}, right = {w}, bottom = {h}, radius = {rb} }} }} }}",
            d = 2.0 * r
        ))
    }

    /// The same panel built by hand as a config does: arcs for the circle and its two concave
    /// fillets of radius `f`, each centred `r + f` from the circle's centre so both ends are tangent.
    fn hand_built(r: f64, w: f64, h: f64, rb: f64, f: f64) -> Rc<Shape> {
        let s = ((r + f).powi(2) - f * f).sqrt();
        let (top, left) = ((r + s, r - f), (r - f, r + s));
        let deg = |y: f64, x: f64| y.atan2(x).to_degrees().rem_euclid(360.0);
        let (from, to) = (deg(left.1 - r, left.0 - r), deg(top.1 - r, top.0 - r));
        let (mx, my) = (r + r * from.to_radians().cos(), r + r * from.to_radians().sin());
        let into_top = deg(r - top.1, r - top.0);
        let into_left = deg(r - left.1, r - left.0) - 360.0;
        shape(&format!(
            "{{ commands = {{ {{ op = 'M', points = {{ {mx}, {my} }} }}, \
             {{ op = 'A', points = {{ {r}, {r}, {r}, {from}, {} }} }}, \
             {{ op = 'A', points = {{ {}, {}, {f}, {into_top}, {} }} }}, \
             {{ op = 'corner', points = {{ {w}, {r} }}, radius = {rb} }}, \
             {{ op = 'corner', points = {{ {w}, {h} }}, radius = {rb} }}, \
             {{ op = 'corner', points = {{ {r}, {h} }}, radius = {rb} }}, \
             {{ op = 'A', points = {{ {}, {}, {f}, 0, {into_left} }} }}, {{ op = 'Z', points = {{}} }} }} }}",
            to - from,
            top.0,
            top.1,
            90.0 - into_top,
            left.0,
            left.1,
        ))
    }

    /// The sharpest turn between neighbouring chords of any loop, in degrees: a cusp turns near 180.
    fn sharpest_turn(shape: &Shape) -> f64 {
        let turn = |l: &Vec<Point>, i: usize| {
            let n = l.len();
            let (a, b) = (l[i] - l[(i + n - 1) % n], l[(i + 1) % n] - l[i]);
            ((b.atan2() - a.atan2() + PI).rem_euclid(TAU) - PI).abs().to_degrees()
        };
        let loops = shape.polygon(PANEL, TOLERANCE);
        loops.iter().flat_map(|l| (0..l.len()).map(move |i| turn(l, i))).fold(0.0, f64::max)
    }

    fn area(shape: &Shape) -> f64 {
        shape.polygon(PANEL, TOLERANCE).into_iter().map(|l| closed(l).area().abs()).sum()
    }

    /// The collapse a config tweens: the body shrinks into the circle tab. Point by point, the hand-
    /// built fillets leave the circle mid-tween and the outline kinks; the melt stays smooth at
    /// every frame, sweeping the melt radius from 12 px to 4 px with the size.
    #[test]
    fn a_collapsing_melt_stays_smooth_where_hand_built_fillets_kink() {
        let (from, to) = (hand_built(24.0, 300.0, 200.0, 16.0, 12.0), hand_built(24.0, 72.0, 72.0, 8.0, 4.0));
        assert!(sharpest_turn(&from) < 20.0 && sharpest_turn(&to) < 20.0, "both ends are smooth");
        let mid = from.lerp(&to, 0.5).unwrap();
        assert!(sharpest_turn(&mid) > 30.0, "hand-built fillets kink mid-tween: {}", sharpest_turn(&mid));

        let (from, to) = (tab(24.0, 300.0, 200.0, 16.0, 12.0), tab(24.0, 72.0, 72.0, 8.0, 4.0));
        for i in 0..=40 {
            let frame = from.lerp(&to, i as f32 / 40.0).unwrap();
            let turn = sharpest_turn(&frame);
            assert!(turn < 20.0, "frame {i} turns {turn}°");
            assert_eq!(frame.polygon(PANEL, TOLERANCE).len(), 1, "frame {i} is one blob");
        }
    }

    /// Two bubbles drifting together stay two loops until the melt bridges them, then one smooth
    /// loop; the pointer follows the bridge.
    #[test]
    fn bubbles_bridge_into_one_smooth_loop_as_they_approach() {
        let loops = |gap: f64| bubbles(gap).polygon(PANEL, TOLERANCE).len();
        assert_eq!((loops(12.0), loops(1.0), loops(-10.0)), (2, 1, 1));
        for gap in [8.0, 6.0, 4.0, 1.0, 0.0, -5.0, -10.0] {
            assert!(sharpest_turn(&bubbles(gap)) < 20.0, "gap {gap}: {}°", sharpest_turn(&bubbles(gap)));
        }
        let hit = |shape: &Shape, x: f64, y: f64| shape.contains(PANEL, Point::new(x, y));
        // Midway between the circles, 8 px above their centres: outside both, inside the bridge.
        let (apart, joined) = (bubbles(12.0), bubbles(1.0));
        assert!(!hit(&apart, 46.0, 12.0) && hit(&joined, 40.5, 12.0), "the bridge takes the pointer");
        assert!(!hit(&joined, 40.5, 0.5), "above the bridge's waist");
    }

    /// A tab grown from half a pixel shows at every size, never dropping between grid nodes, alone
    /// or melting into a body; a 2 px divider between rows of the coarse grid is traced too.
    #[test]
    fn a_part_shows_and_grows_from_its_first_half_pixel() {
        let lone = |s: f64| {
            shape(&format!(
                "{{ parts = {{ {{ left = 10.3, top = 10.7, right = {0}, bottom = {1}, radius = 999 }} }} }}",
                10.3 + s,
                10.7 + s
            ))
        };
        let melted = |s: f64| {
            shape(&format!(
                "{{ melt = 4, parts = {{ {{ left = 20, top = 20, right = 200, bottom = 120, radius = 12 }}, \
                 {{ left = {0}, top = {0}, right = {1}, bottom = {1}, radius = 999 }} }} }}",
                21.1 - s / 2.0,
                21.1 + s / 2.0
            ))
        };
        let (mut last_lone, mut last_melted) = (0.0, area(&melted(0.0)));
        for s in (1..=16).map(|i| f64::from(i) / 2.0) {
            let (a, b) = (area(&lone(s)), area(&melted(s)));
            assert!(a > last_lone && b > last_melted, "{s} px: {a} after {last_lone}, {b} after {last_melted}");
            (last_lone, last_melted) = (a, b);
        }
        let divider = shape("{ parts = { { left = 0, top = 5, right = 100, bottom = 7 } } }");
        assert!((area(&divider) - 200.0).abs() < 1.0, "{}", area(&divider));
        let rect = LogicalRect { x: 5.0, y: 5.0, width: 30.0, height: 30.0 };
        let none = shape("{ parts = { { left = 0, top = 0, right = 0.2, bottom = 9 } } }");
        assert!(none.polygon(rect, TOLERANCE).is_empty() && none.bounds(rect) == rect);
    }

    /// A thin part adds grid lines through itself only: a 1000 px panel beside a 0.5 px line costs
    /// about what their perimeters do at the 4 px step (6000 px over 8 px leaves, culled to a band
    /// a few leaves wide), where a 0.125 px grid everywhere would cost 32 times as many.
    #[test]
    fn a_thin_part_costs_its_own_perimeter() {
        let thin = shape(
            "{ melt = 8, parts = { { left = 0, top = 0, right = 1000, bottom = 1000, radius = 24 }, \
             { left = 1100, top = 0, right = 1100.5, bottom = 1000 } } }",
        );
        CELLS.set(0);
        let loops = thin.polygon(LogicalRect { x: 0.0, y: 0.0, width: 1000.0, height: 1000.0 }, TOLERANCE);
        assert_eq!(loops.len(), 2, "the line shows");
        assert!(CELLS.get() < 4000, "{} leaves", CELLS.get());
    }

    /// A comb at the 8192 px coordinate bound, 7 teeth on a spine, is one loop of about 32000
    /// grid crossings, which `mantle_shape` takes in at most 256 vertices.
    #[test]
    fn a_long_melt_fits_the_sdf_array() {
        let teeth: Vec<_> = (0..7)
            .map(|i| {
                format!("{{ left = {0}, top = 0, right = {1}, bottom = 8192, radius = 50 }}", i * 1200, i * 1200 + 100)
            })
            .collect();
        let comb = shape(&format!(
            "{{ melt = 16, parts = {{ {{ left = 0, top = 0, right = 8192, bottom = 100 }}, {} }} }}",
            teeth.join(", ")
        ));
        let rect = LogicalRect { x: 0.0, y: 0.0, width: 8192.0, height: 8192.0 };
        let loops = comb.polygon(rect, TOLERANCE);
        assert!(loops.len() == 1 && loops[0].len() > SDF_POINTS, "{}", loops[0].len());
        assert!(comb.sdf_polygon(rect).len() <= SDF_POINTS);
    }

    /// A ring of four bars bounds a void: a second loop, wound the other way, that the pointer
    /// passes through. A part flung to the coordinate bound is cut 1024 px past the box.
    #[test]
    fn a_ring_of_parts_leaves_a_void_and_a_far_part_is_cut() {
        let ring = shape(RING);
        let rect = LogicalRect { x: 0.0, y: 0.0, width: 60.0, height: 60.0 };
        assert_eq!(ring.voids(rect), [false, true]);
        assert!(ring.contains(rect, Point::new(4.0, 30.0)) && !ring.contains(rect, Point::new(30.0, 30.0)));
        let far = shape("{ parts = { { left = -8000, top = 0, right = 8000, bottom = 10 } } }");
        let b = far.bounds(rect);
        assert!((b.x, b.width) == (-1024.0, 60.0 + 2048.0), "{b:?}");
    }

    /// A still melt drawn at 1.5x traces once however often a build scales it, and again on a resize.
    #[test]
    fn a_scaled_melt_shares_its_trace() {
        let Shape::Melt(still) = &*bubbles(1.0) else { unreachable!() };
        let at = |w: f32| LogicalRect { x: 0.0, y: 0.0, width: w, height: 60.0 };
        let (first, second) = (still.scaled(1.5), still.scaled(1.5));
        assert!(Rc::ptr_eq(&first.traced(at(150.0)), &second.traced(at(150.0))), "two builds, one trace");
        assert!(!Rc::ptr_eq(&first.traced(at(150.0)), &second.traced(at(180.0))), "a resize retraces");
    }

    #[test]
    fn a_melt_refuses_mixed_or_empty_forms() {
        let part = "{ left = 0, top = 0, right = 9, bottom = 9 }";
        assert!(parse(&format!("{{ parts = {{ {part} }} }}")).is_ok());
        for bad in [
            format!("{{ melt = 4, commands = {{}}, parts = {{ {part} }} }}"),
            "{ melt = 4 }".into(),
            "{ melt = 4, parts = {} }".into(),
            format!("{{ melt = -1, parts = {{ {part} }} }}"),
            "{ parts = { { left = { from = 'top' }, top = 0, right = 9, bottom = 9 } } }".into(),
            "{ parts = { { left = 0, top = 0, right = 9 } } }".into(),
        ] {
            assert!(parse(&bad).is_err(), "accepted {bad}");
        }
        let bad_radius =
            format!("{{ parts = {{ {part}, {{ left = 0, top = 0, right = 9, bottom = 9, radius = -1 }} }} }}");
        let err = parse(&bad_radius).unwrap_err().to_string();
        assert!(err.contains("parts[2]"), "names the part: {err}");
        let err = parse(&format!("{{ parts = {{ {} }} }}", ["1"; 9].join(", "))).unwrap_err().to_string();
        assert!(err.contains("at most 8"), "counted before any part is read: {err}");
        // A melt snaps against a contour or a different count of parts.
        let one = shape(&format!("{{ melt = 4, parts = {{ {part} }} }}"));
        let two = shape(&format!("{{ melt = 4, parts = {{ {part}, {part} }} }}"));
        assert!(one.lerp(&two, 0.5).is_none() && one.lerp(&crate::layout::node::shape::tail(), 0.5).is_none());
    }
}

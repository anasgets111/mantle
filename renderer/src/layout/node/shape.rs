//! `shape`: a box's shape as one closed contour of path commands, then any `hole` contours, each
//! point placed against the box's size where it is painted and hit.

use std::cell::RefCell;
use std::f64::consts::{PI, TAU};
use std::rc::Rc;

use kurbo::Shape as _;
use kurbo::{Affine, BezPath, CubicBez, ParamCurve, ParamCurveDeriv, PathEl, Point, Rect};
use linesweeper::{BinaryOp, FillRule};
use mlua::Value;

use super::melt::{Melt, PartInput};
use super::prop::Prop;
use super::vector_path::{COORD_MAX, PathCommand, PathData, PathOp, Segment, Tweened, push};
use super::{LayoutError, Property, input, invalid, preview_for_error, style::parse_percent};
use crate::lua::luacats::{LuaType, lua_shape, spelled};
use crate::text::snap::LogicalRect;

lua_shape! {
    /// A point `px` from an edge or the centre of the box, or from `"NN%"` of its width or height.
    #[alias = "ShapeAnchor"]
    pub(crate) struct AnchorInput {
        from: String,
        px: Option<f32>,
    }
}

lua_shape! {
    /// As a `PathCommand`: each coordinate is px from the box's top left, `"NN%"` of its width or
    /// height, or an `ShapeAnchor`; a radius or angle is a number.
    #[alias = "ShapeCommand"]
    pub(crate) struct ShapeCommand {
        op: PathOp,
        points: Vec<Coord>,
        hole: Option<bool>,
        radius: Option<f32>,
        corner_smoothing: Option<f32>,
    }
}

lua_shape! {
    /// Either `commands` as one closed contour (an `M`, at least two more commands, then `Z`) followed by any closed contours whose `M` sets `hole = true`, or `parts` joined by a `melt` px fillet (default `0`).
    #[alias = "Shape"]
    struct ShapeInput {
        commands: Option<Vec<ShapeCommand>>,
        melt: Option<f32>,
        parts: Option<Vec<PartInput>>,
    }
}

/// One number of a command: `px` plus a share of the box's width or height, whichever its slot
/// is; an edge's name holds it to its own axis, `0` for x.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Coord {
    pub(super) px: f32,
    pub(super) share: f32,
    pub(super) axis: Option<usize>,
}

spelled!(Coord => "ShapePoint");

const MAX_SHARE: f32 = 16.0;

impl super::input::Input for Coord {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        // A share past 1600% could only land outside the coordinate bound, where femtovg hangs.
        let percent = |s: &str| {
            parse_percent(s)
                .filter(|share| *share <= MAX_SHARE)
                .ok_or_else(|| invalid(property, format!("`{key}`: {s} is not \"NN%\" up to 1600%")))
        };
        Ok(Some(match value {
            Value::String(s) => Coord { px: 0.0, share: percent(&s.to_string_lossy())?, axis: None },
            Value::Table(table) => {
                let AnchorInput { from, px } = AnchorInput::read(&format!("{property}.{key}"), table)?;
                let (share, axis) = match from.as_str() {
                    "left" => (0.0, Some(0)),
                    "right" => (1.0, Some(0)),
                    "top" => (0.0, Some(1)),
                    "bottom" => (1.0, Some(1)),
                    "center" => (0.5, None),
                    other => (percent(other)?, None),
                };
                Coord { px: px.unwrap_or(0.0), share, axis }
            }
            other => match super::value_as_f32(property, other)? {
                Some(px) => Coord { px, share: 0.0, axis: None },
                None => return Ok(None),
            },
        }))
    }
}

/// A box's own outline, replacing its rounded rectangle.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// Each number's px part; a coordinate also has its share of the box in `shares`.
    Contour {
        path: PathData,
        shares: Vec<[f32; 2]>,
        /// Display scaling, applied where the contour is placed so scaled copies share `holes`.
        scale: f32,
        holes: Holes,
    },
    Melt(Melt),
}

/// A holed contour resolved at the two sizes last asked for, newest first: paint's box and the
/// region's snapped one. Always equal, as it holds only what the contour already says.
#[derive(Debug, Clone, Default)]
pub struct Holes(Rc<RefCell<Vec<Rc<Holed>>>>);

impl PartialEq for Holes {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// A holed contour on a box of `size` at the origin: the outline less the union of its holes.
#[derive(Debug)]
pub struct Holed {
    size: (f32, f32),
    path: BezPath,
    voids: Vec<bool>,
    /// `None` when the holes leave nothing.
    bounds: Option<Rect>,
}

impl Holed {
    /// Each hole by its own non-zero rule first, so a self-crossing hole cannot cancel another.
    fn new(placed: &BezPath, size: (f32, f32)) -> Self {
        let mut loops = placed.subpaths().map(|l| BezPath::from_vec(l.to_vec()));
        let outline = loops.next().unwrap_or_default();
        let holes: BezPath = loops
            .filter_map(|hole| boolean(&hole, &BezPath::new(), BinaryOp::Union))
            .flat_map(|(hole, _)| hole)
            .collect();
        let (path, voids) = boolean(&outline, &holes, BinaryOp::Difference)
            .unwrap_or_else(|| (placed.clone(), placed.subpaths().enumerate().map(|(i, _)| i > 0).collect()));
        let bounds = (!path.is_empty()).then(|| path.bounding_box());
        Self { size, path, voids, bounds }
    }
}

/// `op` of `a` and `b` by the non-zero rule, as loops that never cross and whether each bounds a
/// void; `None` only for a non-finite or unclosed path, which the parser and `place` rule out.
pub fn boolean(a: &BezPath, b: &BezPath, op: BinaryOp) -> Option<(BezPath, Vec<bool>)> {
    let set = linesweeper::binary_op(a, b, FillRule::NonZero, op).ok()?;
    Some((set.contours().flat_map(|c| c.path.iter()).collect(), set.contours().map(|c| !c.outer).collect()))
}

/// `path` with each coordinate placed on a box of `size` at the origin.
fn placed(path: &PathData, shares: &[[f32; 2]], size: (f32, f32)) -> BezPath {
    let mut path = path.clone();
    for (n, share) in path.points.iter_mut().zip(shares) {
        *n = place(*n, *share, size);
    }
    path.bez((0.0, 0.0), 1.0)
}

/// From a box of config px at the origin to one at `rect`, drawn `scale` times as large.
fn to_box(scale: f32, rect: LogicalRect) -> Affine {
    let s = f64::from(scale);
    Affine::new([s, 0.0, 0.0, s, rect.x.into(), rect.y.into()])
}

/// The vertex cap of the polyline `mantle_sdf` measures, two to each `vec4` of `mantle_shape`.
pub const SDF_POINTS: usize = 256;

/// Ends each closed loop of a `mantle_shape` with more than one; `mantle_sdf` skips an edge touching it.
const SDF_BREAK: Point = Point::new(1e9, 1e9);

/// The loops `mantle_shape` holds: four vertices, a closing one and a break each.
const SDF_LOOPS: usize = SDF_POINTS / 6;

impl Shape {
    /// The outline of a box at `rect`, each placed coordinate held to [`COORD_MAX`]. Holes subtract
    /// their union, so where two overlap or one crosses the outline the whole hole is empty, and
    /// the loops left never cross: non-zero, even-odd and paint's solidity all read them alike.
    pub fn bez(&self, rect: LogicalRect) -> BezPath {
        match self {
            Self::Contour { path, shares, scale, .. } => {
                let size = (rect.width / scale, rect.height / scale);
                let local = self.holed(rect).map_or_else(|| placed(path, shares, size), |holed| holed.path.clone());
                to_box(*scale, rect) * local
            }
            Self::Melt(melt) => melt.placed(rect) * melt.traced(rect).path.clone(),
        }
    }

    /// A holed contour resolved for `rect`'s size, from the cache when that size was resolved last
    /// or the time before; `None` for a contour without holes, or a melt.
    pub fn holed(&self, rect: LogicalRect) -> Option<Rc<Holed>> {
        let Self::Contour { path, shares, scale, holes } = self else { return None };
        if !path.segments.iter().any(|s| s.hole) {
            return None;
        }
        let size = (rect.width / scale, rect.height / scale);
        let mut cache = holes.0.borrow_mut();
        let same = |h: &&Rc<Holed>| (h.size.0 - size.0).abs() < 1e-3 && (h.size.1 - size.1).abs() < 1e-3;
        let holed = cache.iter().find(same).cloned();
        let holed = holed.unwrap_or_else(|| Rc::new(Holed::new(&placed(path, shares, size), size)));
        cache.retain(|h| !Rc::ptr_eq(h, &holed));
        cache.insert(0, holed.clone());
        cache.truncate(2);
        Some(holed)
    }

    /// Each loop of the outline flattened to within `tolerance` px; a melt is already chords.
    pub fn polygon(&self, rect: LogicalRect, tolerance: f64) -> Vec<Vec<Point>> {
        if let Self::Melt(melt) = self {
            let placed = melt.placed(rect);
            return melt.traced(rect).loops.iter().map(|l| l.iter().map(|p| placed * *p).collect()).collect();
        }
        let mut loops: Vec<Vec<Point>> = Vec::new();
        kurbo::flatten(self.bez(rect), tolerance, |element| match (element, loops.last_mut()) {
            (PathEl::MoveTo(p), _) => loops.push(vec![p]),
            (PathEl::LineTo(p), Some(l)) => l.push(p),
            _ => {}
        });
        loops.into_iter().map(apart).collect()
    }

    /// Whether each loop of [`polygon`](Self::polygon) bounds a void, which paint fills as a hole.
    pub fn voids(&self, rect: LogicalRect) -> Vec<bool> {
        match self {
            Self::Contour { .. } => self.holed(rect).map_or_else(|| vec![false], |holed| holed.voids.clone()),
            Self::Melt(melt) => melt.traced(rect).voids.clone(),
        }
    }

    /// Whether the outline at `rect` holds `p`, by the non-zero rule paint uses.
    pub fn contains(&self, rect: LogicalRect, p: Point) -> bool {
        match self {
            Self::Contour { scale, .. } => match self.holed(rect) {
                Some(holed) => {
                    let p = to_box(*scale, rect).inverse() * p;
                    holed.bounds.is_some_and(|b| b.contains(p)) && holed.path.contains(p)
                }
                None => self.bez(rect).contains(p),
            },
            Self::Melt(melt) => melt.traced(rect).path.contains(melt.placed(rect).inverse() * p),
        }
    }

    /// At most [`SDF_POINTS`] vertices, all `mantle_shape` holds, whatever the kind: one loop as is,
    /// or each of several closed and ended by [`SDF_BREAK`].
    /// ponytail: a melt gives its biggest loop, so blobs past it and voids read as outside and inside;
    /// more needs its loops passed here.
    pub fn sdf_polygon(&self, rect: LogicalRect) -> Vec<Point> {
        let loops = if let Self::Melt(melt) = self { vec![melt.sdf_loop(rect)] } else { self.contour_sdf(rect) };
        if loops.is_empty() {
            // Holes over the whole outline: a lone break, every edge skipped, so nothing is inside.
            return vec![SDF_BREAK];
        }
        let total: usize = loops.iter().map(Vec::len).sum();
        if let [out] = &loops[..] {
            // ponytail: past 256 at the coarsest pass, every nth vertex; upgrade by merging segments.
            return out.iter().copied().step_by(total.div_ceil(SDF_POINTS).max(1)).collect();
        }
        let fits = total + 2 * loops.len() <= SDF_POINTS;
        // Past the cap, each loop keeps four vertices, so a box keeps its corners, and its share of
        // the rest, evenly spaced: at 42 loops that is 4 vertices to share, so a long loop is coarse.
        let spare = SDF_POINTS - 6 * loops.len();
        let closed = |l: &Vec<Point>| {
            let keep = if fits { l.len() } else { l.len().min(4 + l.len() * spare / total) };
            let thin = (0..keep).map(|j| l[j * l.len() / keep]);
            thin.chain(l.first().copied()).chain([SDF_BREAK]).collect::<Vec<_>>()
        };
        loops.iter().flat_map(closed).collect()
    }

    /// Vertices cut every 3° of turn, or every 6°, 12° and so on until [`SDF_POINTS`] fit beside
    /// each loop's closing vertex and break; a loop to each contour.
    fn contour_sdf(&self, rect: LogicalRect) -> Vec<Vec<Point>> {
        let turn = |c: &CubicBez| -> f64 {
            let d = c.deriv();
            let angles = (0..=16).map(|i| d.eval(f64::from(i) / 16.0).to_vec2()).filter(|v| v.hypot2() > 0.0);
            let angles: Vec<f64> = angles.map(|v| v.atan2()).collect();
            angles.windows(2).map(|w| (w[1] - w[0] + PI).rem_euclid(TAU) - PI).map(f64::abs).sum()
        };
        let bez = self.bez(rect);
        let mut loops: Vec<(f64, &[PathEl])> =
            bez.subpaths().map(|l| (BezPath::from_vec(l.to_vec()).area().abs(), l)).collect();
        // ponytail: holes across the outline can cut it into more loops than `mantle_shape` holds;
        // the [`SDF_LOOPS`] biggest are measured, so a smaller island reads as outside and a smaller
        // hole as inside. Upgrade: a per-loop length array beside `mantle_shape`.
        if loops.len() > SDF_LOOPS {
            loops.sort_by(|a, b| b.0.total_cmp(&a.0));
            loops.truncate(SDF_LOOPS);
        }
        let loops: Vec<Vec<(CubicBez, f64)>> = loops
            .iter()
            .map(|(_, l)| kurbo::segments(l.iter().copied()).map(|s| s.to_cubic()).map(|c| (c, turn(&c))).collect())
            .collect();
        let room = if loops.len() > 1 { SDF_POINTS - 2 * loops.len() } else { SDF_POINTS };
        let mut step = 3f64.to_radians();
        loop {
            let cut = |segments: &Vec<(CubicBez, f64)>| {
                apart(segments.iter().flat_map(|&(c, turn)| {
                    let n = (turn / step).ceil().max(1.0);
                    (0..n as usize).map(move |i| c.eval(i as f64 / n))
                }))
            };
            let out: Vec<Vec<Point>> = loops.iter().map(cut).collect();
            if out.iter().map(Vec::len).sum::<usize>() <= room || step > PI {
                return out;
            }
            step *= 2.0;
        }
    }

    /// `rect` grown to hold the contour, which may reach past it.
    pub fn bounds(&self, rect: LogicalRect) -> LogicalRect {
        let b = match self {
            Self::Contour { scale, .. } => match self.holed(rect) {
                Some(holed) => match holed.bounds {
                    Some(b) => to_box(*scale, rect).transform_rect_bbox(b),
                    // Holes over the whole outline leave nothing: the box itself, not a rect at the origin.
                    None => return rect,
                },
                None => self.bez(rect).bounding_box(),
            },
            Self::Melt(melt) => melt.placed(rect).transform_rect_bbox(melt.traced(rect).bounds),
        };
        let (x0, y0) = ((b.x0 as f32).min(rect.x), (b.y0 as f32).min(rect.y));
        let (x1, y1) = ((b.x1 as f32).max(rect.x + rect.width), (b.y1 as f32).max(rect.y + rect.height));
        LogicalRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
    }

    /// `self` moved toward `to` by `t`, or `None` when their commands differ and the tween snaps.
    /// A point is linear in its px and its share, so one written in px and the other in `"NN%"`
    /// lerps as the two resolved against the box would, at every size.
    pub fn lerp(&self, to: &Self, t: f32) -> Option<Self> {
        match (self, to) {
            (Self::Contour { path, shares, scale, .. }, Self::Contour { path: to, shares: to_shares, .. }) => {
                let path = path.lerp(to, t)?;
                let shares = shares.iter().zip(to_shares).map(|(a, b)| [0, 1].map(|i| a[i] + (b[i] - a[i]) * t));
                Some(Self::Contour { path, shares: shares.collect(), scale: *scale, holes: Holes::default() })
            }
            (Self::Melt(a), Self::Melt(b)) => a.lerp(b, t).map(Self::Melt),
            _ => None,
        }
    }

    /// `self` drawn `k` times as large, as display scaling asks, sharing its resolved holes.
    pub fn scaled(&self, k: f32) -> Self {
        match self {
            Self::Contour { path, shares, scale, holes } => {
                Self::Contour { path: path.clone(), shares: shares.clone(), scale: scale * k, holes: holes.clone() }
            }
            Self::Melt(melt) => Self::Melt(melt.scaled(k)),
        }
    }
}

/// `points` without repeats under 0.001 px, the closing one included: a zero-length edge has no normal.
fn apart(points: impl IntoIterator<Item = Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::new();
    for p in points {
        if out.last().is_none_or(|last| last.distance(p) >= 1e-3) {
            out.push(p);
        }
    }
    while out.len() > 1 && out[0].distance(out[out.len() - 1]) < 1e-3 {
        out.pop();
    }
    out
}

/// Signed distance to the closed `polygon`, negative inside by even-odd: `mantle_sdf` on the CPU.
#[cfg(test)]
pub fn distance(polygon: &[Point], p: Point) -> f64 {
    let Some(&last) = polygon.last() else { return f64::INFINITY };
    let (mut nearest, mut inside, mut a) = (f64::INFINITY, false, last);
    for &b in polygon {
        if a.x.max(b.x) >= SDF_BREAK.x {
            a = b;
            continue;
        }
        let (e, w) = (b - a, p - a);
        let t = if e.hypot2() > 0.0 { (w.dot(e) / e.hypot2()).clamp(0.0, 1.0) } else { 0.0 };
        nearest = nearest.min((w - e * t).hypot2());
        // A crossing of the ray to the right of `p`.
        if (a.y <= p.y) != (b.y <= p.y) && (p.x - a.x < e.x * (p.y - a.y) / e.y) {
            inside = !inside;
        }
        a = b;
    }
    if inside { -nearest.sqrt() } else { nearest.sqrt() }
}

/// `n` px plus its `share` of a box `size`, held to [`COORD_MAX`].
pub(super) fn place(n: f32, share: [f32; 2], (w, h): (f32, f32)) -> f32 {
    (n + share[0] * w + share[1] * h).clamp(-COORD_MAX, COORD_MAX)
}

/// `points` as a closed path.
pub(super) fn closed(points: impl IntoIterator<Item = Point>) -> BezPath {
    let mut path = BezPath::new();
    for (i, p) in points.into_iter().enumerate() {
        if i == 0 { path.move_to(p) } else { path.line_to(p) }
    }
    path.close_path();
    path
}

/// The sign of the biggest loop's area, whose winding says which side of every loop is inside.
pub(super) fn winding(loops: &[Vec<Point>]) -> f64 {
    let areas = loops.iter().map(|l| closed(l.iter().copied()).area());
    areas.max_by(|a, b| a.abs().total_cmp(&b.abs())).unwrap_or(0.0).signum()
}

/// Every loop moved `by` px into the shape (out of it if negative), mitres held to four times `by`;
/// a void's loop moves away from the void. A loop of under 3 points has no edge to move along and
/// stays put, so the loops still pair with `voids`.
pub fn inset(loops: &[Vec<Point>], by: f64) -> BezPath {
    inset_loops(loops, by).into_iter().flat_map(closed).collect()
}

/// [`inset`]'s loops, each point moved from the point of `loops` at the same index.
pub fn inset_loops(loops: &[Vec<Point>], by: f64) -> Vec<Vec<Point>> {
    let side = winding(loops);
    let one = |polygon: &Vec<Point>| {
        let n = polygon.len();
        if n < 3 {
            return polygon.clone();
        }
        let normal = |i: usize| {
            let e = polygon[(i + 1) % n] - polygon[i];
            kurbo::Vec2::new(-e.y, e.x).normalize() * side
        };
        (0..n)
            .map(|i| {
                let m = (normal((i + n - 1) % n) + normal(i)) / 2.0;
                polygon[i] + m * by / m.hypot2().max(1.0 / 16.0)
            })
            .collect()
    };
    loops.iter().map(one).collect()
}

/// `shape = { commands = { .. } }`.
impl LuaType for Shape {
    fn lua() -> String {
        "Shape".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        ShapeInput::classes(out);
    }
}

// ponytail: 256 commands; more needs a measured `mantle_sdf` budget.
const MAX_COMMANDS: usize = 256;

impl Prop for Shape {
    type Out = Option<Rc<Shape>>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let name = row.name;
        let table = match value {
            None => return Ok(None),
            Some(Value::Table(table)) => table,
            Some(Value::UserData(ud)) if let Ok(frame) = ud.borrow::<Tweened<Shape>>() => {
                return Ok(Some(frame.0.clone()));
            }
            Some(value) => return Err(invalid(name, format!("expected a table, got {}", preview_for_error(value)))),
        };
        // Counted before any command is read, as `PathCommands` does.
        for (key, limit) in [("commands", MAX_COMMANDS), ("parts", super::melt::MAX_PARTS)] {
            if let Ok(Value::Table(list)) = table.get::<Value>(key) {
                input::array_len(&format!("{name}.{key}"), &list, limit)?;
            }
        }
        let commands = match ShapeInput::read(name, table)? {
            ShapeInput { commands: Some(commands), melt: None, parts: None } => commands,
            ShapeInput { commands: None, melt, parts: Some(parts) } => {
                return Ok(Some(Rc::new(Shape::Melt(Melt::read(name, melt.unwrap_or(0.0), parts)?))));
            }
            _ => return Err(invalid(name, "either `commands`, or `parts` with an optional `melt`")),
        };
        let (mut segments, mut points, mut shares) = (Vec::new(), Vec::new(), Vec::new());
        for (i, ShapeCommand { op, points: coords, hole, radius, corner_smoothing }) in commands.into_iter().enumerate()
        {
            let name = format!("{name}.commands[{}]", i + 1);
            let coordinates = match op {
                PathOp::A | PathOp::Corner => 2,
                _ => coords.len(),
            };
            for (at, coord) in coords.iter().enumerate() {
                // x then y; a radius or angle is a plain number.
                if at >= coordinates && (coord.share != 0.0 || coord.axis.is_some()) {
                    return Err(invalid(&name, "only a coordinate takes a share of the box"));
                }
                if coord.axis.is_some_and(|axis| axis != at % 2) {
                    return Err(invalid(&name, "left and right are x, top and bottom are y"));
                }
                let mut share = [0.0; 2];
                share[at % 2] = coord.share;
                shares.push(share);
            }
            let numbers = coords.iter().map(|c| c.px).collect();
            let command = PathCommand { op, points: numbers, hole, radius, corner_smoothing };
            push(&name, &mut segments, &mut points, command)?;
            shares.resize(points.len(), [0.0; 2]);
        }
        // `push` begins a contour after each `Z` and refuses `hole` past a contour's first command.
        let starts: Vec<usize> = (0..segments.len()).filter(|&i| segments[i].begins).collect();
        let ends = starts.iter().skip(1).copied().chain([segments.len()]);
        let closed = |(n, (start, end)): (usize, (usize, usize))| {
            let contour: &[Segment] = &segments[start..end];
            contour[0].op == PathOp::M
                && contour[contour.len() - 1].op == PathOp::Z
                && contour.len() >= 4
                && contour[0].hole == (n > 0)
        };
        if starts.is_empty() || !starts.iter().copied().zip(ends).enumerate().all(closed) {
            return Err(invalid(
                name,
                "one closed contour, then any holes: each an M, at least two commands, then Z; only a hole's M sets `hole = true`",
            ));
        }
        if starts.len() > SDF_LOOPS {
            return Err(invalid(name, "at most 41 holes, all `mantle_sdf` holds beside the outline"));
        }
        Ok(Some(Rc::new(Shape::Contour {
            path: PathData { segments: segments.into(), points },
            shares,
            scale: 1.0,
            holes: Holes::default(),
        })))
    }
}

/// A box with a tail 16 px wide and 8 px deep hanging from its bottom centre, its base filleted.
#[cfg(test)]
pub(crate) const TAIL: &str = r#"{ commands = {
    { op = "M", points = { "50%", 0 } },
    { op = "corner", points = { "100%", 0 }, radius = 6, corner_smoothing = 0.6 },
    { op = "corner", points = { "100%", "100%" }, radius = 6 },
    { op = "corner", points = { { from = "center", px = 8 }, "100%" }, radius = 3 },
    { op = "corner", points = { "50%", { from = "bottom", px = 8 } }, radius = 1 },
    { op = "corner", points = { { from = "50%", px = -8 }, { from = "bottom" } }, radius = 3 },
    { op = "corner", points = { 0, "100%" }, radius = 6 },
    { op = "corner", points = { 0, 0 }, radius = 6 },
    { op = "Z", points = {} },
} }"#;

#[cfg(test)]
pub(crate) fn parse(src: &str) -> Result<Option<Rc<Shape>>, LayoutError> {
    let lua = mlua::Lua::new();
    let value: Value = lua.load(src).eval().unwrap();
    Shape::read(&super::fields::paint::shape.row, Some(&value))
}

/// A 32 px box's outline, two holes that overlap and a third across the top edge, each wound as
/// it was written: the box body is at (24, 24), the overlap at (12, 11) and the straddling hole at
/// (27, 2) inside the box and (27, -2) outside it.
#[cfg(test)]
pub(crate) const PUNCHED: &str = r#"{ commands = {
    { op = "M", points = { 0, 0 } }, { op = "L", points = { "100%", 0 } },
    { op = "L", points = { "100%", "100%" } }, { op = "L", points = { 0, "100%" } }, { op = "Z", points = {} },
    { op = "M", points = { 4, 4 }, hole = true }, { op = "L", points = { 14, 4 } },
    { op = "L", points = { 14, 14 } }, { op = "L", points = { 4, 14 } }, { op = "Z", points = {} },
    { op = "M", points = { 10, 8 }, hole = true }, { op = "L", points = { 10, 18 } },
    { op = "L", points = { 20, 18 } }, { op = "L", points = { 20, 8 } }, { op = "Z", points = {} },
    { op = "M", points = { 24, -4 }, hole = true }, { op = "L", points = { 30, -4 } },
    { op = "L", points = { 30, 6 } }, { op = "L", points = { 24, 6 } }, { op = "Z", points = {} },
} }"#;

/// [`TAIL`], parsed.
#[cfg(test)]
pub(crate) fn tail() -> Rc<Shape> {
    parse(TAIL).unwrap().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(w: f32, h: f32) -> LogicalRect {
        LogicalRect { x: 0.0, y: 0.0, width: w, height: h }
    }

    /// Every point form resolves against the box it is painted on, so the tail stays centred and
    /// 8 px deep at any size, and reaches past the box.
    #[test]
    fn points_follow_the_box_size() {
        let tail = tail();
        for (w, h) in [(32.0, 32.0), (100.0, 50.0)] {
            let b = tail.bounds(at(w, h));
            assert_eq!((b.x, b.y, b.width), (0.0, 0.0, w));
            assert!((b.height - (h + 8.0)).abs() < 0.5, "{b:?}");
            assert_eq!(tail.bez(at(w, h)).elements()[0], PathEl::MoveTo(Point::new(f64::from(w) / 2.0, 0.0)));
            let inside =
                |x: f32, y: f32| distance(&tail.polygon(at(w, h), 0.25)[0], Point::new(x.into(), y.into())) < 0.0;
            assert!(inside(w / 2.0, h + 4.0) && !inside(w / 2.0 - 7.0, h + 4.0), "the tail at {w}x{h}");
            assert!(!inside(0.5, 0.5), "a rounded corner");
        }
        let quarter = parse(
            r#"{ commands = { { op = "M", points = { { from = "25%", px = 2 }, 0 } },
            { op = "L", points = { "100%", { from = "center", px = -1 } } }, { op = "L", points = { 0, 9 } },
            { op = "Z", points = {} } } }"#,
        )
        .unwrap()
        .unwrap();
        let bez = quarter.bez(LogicalRect { x: 10.0, y: 10.0, width: 40.0, height: 20.0 });
        assert_eq!(
            bez.elements()[..2],
            [PathEl::MoveTo(Point::new(22.0, 10.0)), PathEl::LineTo(Point::new(50.0, 19.0))]
        );
    }

    /// A share past 1600% is refused where it is read, and a coordinate placed on a huge box is held
    /// to the bound: either would otherwise hand femtovg a coordinate that hangs it.
    #[test]
    fn a_share_of_the_box_cannot_place_a_point_past_the_coordinate_bound() {
        let contour = |x: &str| {
            format!(
                "{{ commands = {{ {{ op = 'M', points = {{ {x}, 0 }} }}, {{ op = 'L', points = {{ 9, 0 }} }}, {{ op = 'L', points = {{ 0, 9 }} }}, {{ op = 'Z', points = {{}} }} }} }}"
            )
        };
        assert!(parse(&contour("'1600%'")).is_ok());
        for bad in ["'1601%'", "'100000000%'", "{ from = '100000000%' }"] {
            assert!(parse(&contour(bad)).is_err(), "accepted {bad}");
        }
        let far = parse(&contour("'1600%'")).unwrap().unwrap();
        let huge = LogicalRect { x: 0.0, y: 0.0, width: 1e9, height: 1e9 };
        assert_eq!(far.bez(huge).elements()[0], PathEl::MoveTo(Point::new(8192.0, 0.0)));
        // A tween's share overshoots the parsed bound the same way.
        let shot = far.lerp(&far, 1.0).unwrap();
        assert_eq!(shot.bez(huge).bounding_box().x1, 8192.0);
    }

    /// The command count is checked before any command is read, so a long list of junk reports the
    /// cap, not the first bad entry.
    #[test]
    fn a_shape_counts_its_commands_before_reading_them() {
        let err = parse(&format!("{{ commands = {{ {} }} }}", "1,".repeat(257))).unwrap_err();
        assert!(err.to_string().contains("at most 256"), "{err}");
    }

    /// `inset` moves either winding inward and returns a path, so a hole built from it needs no
    /// point list; a void wound against the outline moves away from the void, into the shape.
    #[test]
    fn inset_moves_each_winding_of_a_contour_inward() {
        let square = |a: f64, b: f64| vec![Point::new(a, a), Point::new(b, a), Point::new(b, b), Point::new(a, b)];
        let reversed = |l: Vec<Point>| l.into_iter().rev().collect::<Vec<_>>();
        for polygon in [square(0.0, 10.0), reversed(square(0.0, 10.0))] {
            let b = inset(&[polygon], 2.0).bounding_box();
            assert_eq!((b.x0, b.y0, b.x1, b.y1), (2.0, 2.0, 8.0, 8.0));
        }
        let ringed = inset(&[square(0.0, 10.0), reversed(square(4.0, 6.0))], 1.0);
        let void = ringed.elements().iter().skip_while(|e| !matches!(e, PathEl::ClosePath)).skip(1);
        let b = void.copied().collect::<BezPath>().bounding_box();
        assert_eq!((b.x0, b.y0, b.x1, b.y1), (3.0, 3.0, 7.0, 7.0));
    }

    #[test]
    fn a_shape_refuses_what_is_not_one_closed_contour() {
        let contour = |body: &str| format!("{{ commands = {{ {{ op = 'M', points = {{ 0, 0 }} }}, {body} }} }}");
        // `first`, then the rest of a triangle.
        let tailed = |first: &str| {
            contour(&format!("{first}, {{ op = 'L', points = {{ 0, 9 }} }}, {{ op = 'Z', points = {{}} }}"))
        };
        let ok = tailed("{ op = 'L', points = { 9, 0 } }");
        assert!(parse(&ok).is_ok());
        // `ok`, then a contour that begins as `first` and ends as `last`.
        let holed = |first: &str, last: &str| {
            format!(
                "{}, {first}, {{ op = 'L', points = {{ 3, 2 }} }}, {{ op = 'L', points = {{ 2, 3 }} }}, {last} }} }}",
                ok.strip_suffix(" } }").unwrap()
            )
        };
        let (hole, z) = ("{ op = 'M', points = { 2, 2 }, hole = true }", "{ op = 'Z', points = {} }");
        assert!(parse(&holed(hole, z)).is_ok());
        for bad in [
            "{}".to_string(),
            "{ commands = {} }".into(),
            contour("{ op = 'L', points = { 9, 0 } }, { op = 'Z', points = {} }"),
            contour("{ op = 'L', points = { 9, 0 } }, { op = 'L', points = { 0, 9 } }"),
            contour(
                "{ op = 'L', points = { 9, 0 } }, { op = 'M', points = { 0, 9 } }, { op = 'L', points = { 9, 9 } }, { op = 'Z', points = {} }",
            ),
            contour(
                "{ op = 'L', points = { 9, 0 } }, { op = 'Z', points = {} }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }",
            ),
            tailed("{ op = 'L', points = { 9, 0 }, hole = true }"),
            ok.replacen("0 } }", "0 }, hole = true }", 1),
            holed("{ op = 'M', points = { 2, 2 } }", z),
            holed("{ op = 'M', points = { 2, 2 }, hole = false }", z),
            holed(hole, "{ op = 'L', points = { 2, 2 } }"),
            holed("{ op = 'A', points = { 2, 2, 1, 0, 90 }, hole = true }", z),
            tailed("{ op = 'corner', points = { 9, 0 } }"),
            tailed("{ op = 'corner', points = { 9, 0 }, radius = 2, corner_smoothing = 2 }"),
            tailed("{ op = 'L', points = { 9, 0 }, radius = 2 }"),
            tailed("{ op = 'L', points = { { from = 'top' }, 0 } }"),
            tailed("{ op = 'L', points = { { from = 'middle' }, 0 } }"),
            tailed("{ op = 'L', points = { '5px', 0 } }"),
            tailed("{ op = 'A', points = { 9, 9, '50%', 0, 90 } }"),
        ] {
            assert!(parse(&bad).is_err(), "accepted {bad}");
        }
        let lua = mlua::Lua::new();
        let style = |src: &str| {
            let props = super::super::props_from_src(&lua, format!("return {{ kind = 'rect', {src} }}"));
            super::super::paint_style("rect", &props)
        };
        assert!(style(&format!("shape = {TAIL}, border_width = 2, border_color = '#000000'")).is_ok());
        for bad in ["radius = 4", "corner_shape = 'scoop'", "border_width = { top = 2 }"] {
            assert!(style(&format!("shape = {TAIL}, {bad}")).is_err(), "accepted {bad}");
        }
    }

    /// A hole wound either way is a void to paint, hit testing and `mantle_sdf`, and the SDF
    /// measures to each loop's own edges, not to an edge joining two loops.
    #[test]
    fn holes_in_a_shape_are_outside_it_whatever_their_winding() {
        let contour = |hole: &str, points: [&str; 4]| {
            let [m, rest @ ..] = points.map(|p| format!("points = {{ {p} }}"));
            let rest = rest.map(|p| format!("{{ op = 'L', {p} }}, ")).concat();
            format!("{{ op = 'M', {m}{hole} }}, {rest}{{ op = 'Z', points = {{}} }}")
        };
        let card = parse(&format!(
            "{{ commands = {{ {}, {}, {} }} }}",
            contour("", ["0, 0", "'100%', 0", "'100%', '100%'", "0, '100%'"]),
            contour(", hole = true", ["4, 4", "12, 4", "12, 12", "4, 12"]),
            contour(", hole = true", ["20, 4", "20, 12", "28, 12", "28, 4"]),
        ))
        .unwrap()
        .unwrap();
        let rect = at(40.0, 20.0);
        assert_eq!(card.voids(rect), [false, true, true]);
        assert_eq!(card.polygon(rect, 0.25).len(), 3);
        let sdf = card.sdf_polygon(rect);
        for (x, y, inside, d) in
            [(2.0, 2.0, true, 2.0), (8.0, 8.0, false, 4.0), (24.0, 8.0, false, 4.0), (35.0, 10.0, true, 5.0)]
        {
            let p = Point::new(x, y);
            assert_eq!(card.contains(rect, p), inside, "({x}, {y})");
            assert_eq!(distance(&sdf, p), if inside { -d } else { d }, "({x}, {y})");
        }
    }

    /// A glass lens takes its normal from finite differences of `mantle_sdf`, so the distance and
    /// its gradient must not jump where the side meets the filleted tail or round a corner.
    #[test]
    fn the_sdf_and_its_gradient_are_continuous_across_the_joins() {
        let polygon = tail().sdf_polygon(at(32.0, 32.0));
        assert!(polygon.len() <= SDF_POINTS);
        let d = |x: f64, y: f64| distance(&polygon, Point::new(x, y));
        let gradient =
            |x: f64, y: f64| kurbo::Vec2::new(d(x + 0.01, y) - d(x - 0.01, y), d(x, y + 0.01) - d(x, y - 0.01));
        // 1.5 px in along the bottom, over the tail's fillet, the side and a corner; then round another.
        for (x0, y) in [(25.0, 30.5), (1.5, 3.0)] {
            let samples: Vec<_> =
                (0..=60).map(|i| x0 + f64::from(i) * 0.1).map(|x| (d(x, y), gradient(x, y).atan2())).collect();
            for pair in samples.windows(2) {
                let ((d0, a0), (d1, a1)) = (pair[0], pair[1]);
                assert!((d1 - d0).abs() <= 0.11, "the distance jumps: {pair:?}");
                assert!(
                    ((a1 - a0 + PI).rem_euclid(TAU) - PI).abs() < 4f64.to_radians(),
                    "the gradient turns: {pair:?}"
                );
            }
        }
    }

    /// 70 full-turn arcs are 280 cubics, past the vertex cap at any step: the passes still end and
    /// the result is thinned to the cap. Run on a thread so a pass that never ends fails the test.
    #[test]
    fn a_contour_past_the_vertex_cap_at_every_step_is_thinned_to_it() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let arcs: String =
                (0..70).map(|i| format!(r#"{{ op = "A", points = {{ {}, 10, 4, 0, 360 }} }},"#, 10 * i + 5)).collect();
            let src = format!(
                r#"{{ commands = {{ {{ op = "M", points = {{ 0, 0 }} }}, {arcs} {{ op = "Z", points = {{}} }} }} }}"#
            );
            tx.send(parse(&src).unwrap().unwrap().sdf_polygon(at(700.0, 20.0)).len()).unwrap();
        });
        let len = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(len) => len,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the thread panicked, e.g. the shape failed to parse")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("contour_sdf never returned"),
        };
        assert!((SDF_POINTS / 2..=SDF_POINTS).contains(&len), "{len} vertices");
    }

    /// Holes subtract their union: where two overlap, and where one crosses the outline, paint's
    /// contours, hit testing and `mantle_sdf` all leave the whole hole empty.
    #[test]
    fn overlapping_and_straddling_holes_leave_their_union_empty() {
        let card = parse(PUNCHED).unwrap().unwrap();
        let rect = at(32.0, 32.0);
        let (loops, voids) = (card.polygon(rect, 0.25), card.voids(rect));
        assert_eq!(loops.len(), voids.len());
        let sdf = card.sdf_polygon(rect);
        for (x, y, inside) in [
            (24.0, 24.0, true),
            (2.0, 2.0, true),
            (12.0, 11.0, false),
            (27.0, 2.0, false),
            (27.0, -2.0, false),
            (8.0, 8.0, false),
        ] {
            let p = Point::new(x, y);
            assert_eq!(card.contains(rect, p), inside, "contains ({x}, {y})");
            assert_eq!(distance(&sdf, p) < 0.0, inside, "mantle_sdf ({x}, {y})");
        }
    }

    /// Degenerate holes resolve without a panic to a void flag per loop, at any box size and through
    /// a tween's overshoot: one flush with an edge, two alike, and one over the whole box, which
    /// leaves nothing inside to hit testing or `mantle_sdf`, and the box as its bounds.
    #[test]
    fn degenerate_holes_resolve_to_a_flag_per_loop() {
        let square = |hole: &str, [x0, y0, x1, y1]: [&str; 4]| {
            format!(
                "{{ op = 'M', points = {{ {x0}, {y0} }}{hole} }}, {{ op = 'L', points = {{ {x1}, {y0} }} }}, \
                 {{ op = 'L', points = {{ {x1}, {y1} }} }}, {{ op = 'L', points = {{ {x0}, {y1} }} }}, {{ op = 'Z', points = {{}} }}"
            )
        };
        let card = |holes: &[[&str; 4]]| {
            let holes: Vec<String> = holes.iter().map(|h| square(", hole = true", *h)).collect();
            let outline = square("", ["0", "0", "'100%'", "'100%'"]);
            parse(&format!("{{ commands = {{ {outline}, {} }} }}", holes.join(", "))).unwrap().unwrap()
        };
        let flush = card(&[["0", "4", "8", "12"]]);
        let moved = card(&[["20", "20", "40", "40"]]);
        let twins = card(&[["4", "4", "12", "12"], ["4", "4", "12", "12"]]);
        let whole = card(&[["-2", "-2", "'110%'", "'110%'"]]);
        let rect = LogicalRect { x: 10.0, y: 10.0, width: 32.0, height: 32.0 };
        let tweens = [-0.5, 0.3, 1.5].map(|t| flush.lerp(&moved, t).unwrap());
        for shape in [&*flush, &*twins, &*whole].into_iter().chain(&tweens) {
            for r in [rect, LogicalRect { width: 0.0, height: 0.0, ..rect }] {
                assert_eq!(shape.polygon(r, 0.25).len(), shape.voids(r).len(), "{shape:?} at {r:?}");
                assert!(shape.sdf_polygon(r).len() <= SDF_POINTS);
            }
        }
        let inside = |shape: &Shape, x: f64, y: f64| {
            (shape.contains(rect, Point::new(x, y)), distance(&shape.sdf_polygon(rect), Point::new(x, y)) < 0.0)
        };
        assert_eq!([inside(&flush, 12.0, 18.0), inside(&flush, 30.0, 30.0)], [(false, false), (true, true)]);
        assert_eq!(inside(&twins, 18.0, 18.0), (false, false));
        assert_eq!(inside(&whole, 26.0, 26.0), (false, false));
        assert_eq!(whole.bounds(rect), rect);
    }

    /// A holed contour resolves once per size: painting and hitting it again, or at another place,
    /// reads the same resolution; another size resolves anew, the last two kept, and a contour
    /// without holes has none.
    #[test]
    fn a_holed_contour_resolves_once_per_size() {
        let card = parse(PUNCHED).unwrap().unwrap();
        let first = card.holed(at(32.0, 32.0)).unwrap();
        let moved = LogicalRect { x: 9.0, y: 3.0, width: 32.0, height: 32.0 };
        let _ = (card.bez(moved), card.voids(moved), card.bounds(moved), card.contains(moved, Point::new(30.0, 30.0)));
        assert!(Rc::ptr_eq(&first, &card.holed(moved).unwrap()), "one resolution for the size");
        assert!(!Rc::ptr_eq(&first, &card.holed(at(40.0, 32.0)).unwrap()), "a resize resolves anew");
        assert!(Rc::ptr_eq(&first, &card.holed(moved).unwrap()), "and the size before stays");
        assert_eq!(*card, *parse(PUNCHED).unwrap().unwrap(), "the cache is not part of the shape");
        assert!(card.contains(moved, Point::new(33.0, 27.0)) && !card.contains(moved, Point::new(21.0, 14.0)));
        assert_eq!(card.bounds(moved), moved);
        assert!(tail().holed(at(32.0, 32.0)).is_none());
    }

    /// A loop flattened to one point has no edge to move along: it stays put rather than turn NaN,
    /// which would fail the boolean a border on a voided shape cuts with.
    #[test]
    fn inset_leaves_a_collapsed_loop_in_place() {
        let square = vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0), Point::new(10.0, 10.0), Point::new(0.0, 10.0)];
        let path = inset(&[square, vec![Point::new(5.0, 5.0)]], 2.0);
        assert!(path.elements().iter().filter_map(PathEl::end_point).all(|p| p.is_finite()), "{path:?}");
    }

    /// Display scaling shares the resolution, as a melt shares its trace: two builds at 1.5x
    /// resolve once, and every point scales with the box; a shape without holes scales its px too.
    #[test]
    fn a_scaled_holed_contour_shares_its_resolution() {
        let card = parse(PUNCHED).unwrap().unwrap();
        let (first, second) = (card.scaled(1.5), card.scaled(1.5));
        let rect = at(48.0, 48.0);
        assert!(Rc::ptr_eq(&first.holed(rect).unwrap(), &second.holed(rect).unwrap()), "two builds, one resolution");
        assert!(first.contains(rect, Point::new(36.0, 36.0)) && !first.contains(rect, Point::new(18.0, 16.5)));
        assert!(!first.contains(rect, Point::new(40.5, 3.0)), "the hole across the edge, scaled");
        assert_eq!(first.bounds(rect), rect);
        let depth = tail().bounds(at(32.0, 32.0)).height - 32.0;
        let b = tail().scaled(2.0).bounds(at(64.0, 64.0));
        assert!(depth > 1.0 && (b.height - 64.0 - 2.0 * depth).abs() < 1e-3, "the tail's px double at 2x: {b:?}");
        assert_eq!((b.x, b.width), (0.0, 64.0), "and its shares stay on the box");
    }

    /// A self-crossing hole counts by its own non-zero rule, so a bow tie and a box over one of its
    /// lobes both cut, rather than winding -1 and +1 and cancelling.
    #[test]
    fn a_bow_tie_hole_and_a_box_hole_both_cut() {
        let poly = |hole: &str, points: &[(i32, i32)]| {
            let [m, rest @ ..] = points else { unreachable!() };
            let rest: String = rest.iter().map(|(x, y)| format!("{{ op = 'L', points = {{ {x}, {y} }} }}, ")).collect();
            format!("{{ op = 'M', points = {{ {}, {} }}{hole} }}, {rest}{{ op = 'Z', points = {{}} }}", m.0, m.1)
        };
        let card = parse(&format!(
            "{{ commands = {{ {}, {}, {} }} }}",
            poly("", &[(0, 0), (48, 0), (48, 48), (0, 48)]),
            poly(", hole = true", &[(12, 12), (40, 40), (8, 40), (36, 12)]),
            poly(", hole = true", &[(20, 12), (29, 12), (29, 18), (20, 18)]),
        ))
        .unwrap()
        .unwrap();
        let rect = at(48.0, 48.0);
        for (x, y) in [(25.0, 15.0), (24.0, 30.0), (22.0, 17.0)] {
            assert!(!card.contains(rect, Point::new(x, y)), "({x}, {y})");
            assert!(distance(&card.sdf_polygon(rect), Point::new(x, y)) > 0.0, "({x}, {y})");
        }
        assert!(card.contains(rect, Point::new(4.0, 4.0)));
    }

    /// Holes that cut a box into more loops than `mantle_shape` holds: 11 punches each way make 144
    /// islands, and two strips across a 100-tooth comb make 201. `mantle_sdf` measures the 42
    /// biggest within the vertex cap: the grid's biggest island is inside it, while the comb's
    /// 300-vertex spine is thinned to its share of what four vertices a loop leave.
    #[test]
    fn holes_that_cut_past_the_loop_cap_keep_the_biggest_loops() {
        let rect = |x0: i32, y0: &str, x1: i32, y1: &str| {
            format!(
                "{{ op = 'M', points = {{ {x0}, {y0} }}, hole = true }}, {{ op = 'L', points = {{ {x1}, {y0} }} }}, \
                 {{ op = 'L', points = {{ {x1}, {y1} }} }}, {{ op = 'L', points = {{ {x0}, {y1} }} }}, {{ op = 'Z', points = {{}} }}"
            )
        };
        let grid: Vec<String> = (0..11)
            .flat_map(|i| {
                [
                    rect(4 * i + 3, "-1", 4 * i + 4, "49"),
                    rect(-1, &(4 * i + 3).to_string(), 49, &(4 * i + 4).to_string()),
                ]
            })
            .collect();
        let grid = format!(
            "{{ commands = {{ {{ op = 'M', points = {{ 0, 0 }} }}, {{ op = 'L', points = {{ 48, 0 }} }}, \
             {{ op = 'L', points = {{ 48, 48 }} }}, {{ op = 'L', points = {{ 0, 48 }} }}, {{ op = 'Z', points = {{}} }}, {} }} }}",
            grid.join(", ")
        );
        let teeth: String = (0..100)
            .rev()
            .map(|i| {
                format!(
                    "{{ op = 'L', points = {{ {}, 40 }} }}, {{ op = 'L', points = {{ {}, 4 }} }}, ",
                    4 * i + 2,
                    4 * i
                )
            })
            .collect();
        let comb = format!(
            "{{ commands = {{ {{ op = 'M', points = {{ 0, 0 }} }}, {{ op = 'L', points = {{ 400, 0 }} }}, \
             {{ op = 'L', points = {{ 400, 4 }} }}, {teeth}{{ op = 'Z', points = {{}} }}, {}, {} }} }}",
            rect(-1, "10", 401, "12"),
            rect(-1, "20", 401, "22")
        );
        for (src, size, island) in [(grid, (48.0, 48.0), Some((46.0, 46.0))), (comb, (400.0, 40.0), None)] {
            let shape = parse(&src).unwrap().unwrap();
            let rect = at(size.0, size.1);
            assert!(shape.polygon(rect, 0.25).len() > SDF_LOOPS);
            let sdf = shape.sdf_polygon(rect);
            assert!(sdf.len() <= SDF_POINTS, "{} vertices", sdf.len());
            assert_eq!(sdf.iter().filter(|p| **p == SDF_BREAK).count(), SDF_LOOPS);
            assert!(island.is_none_or(|(x, y)| distance(&sdf, Point::new(x, y)) < 0.0));
        }
    }

    /// An outline and 41 holes is all `mantle_sdf` holds and all the parser takes: every loop closes
    /// on itself and ends with a break. Past the vertex cap a long loop takes its share of the
    /// rest, which fills the cap.
    #[test]
    fn a_holed_contour_past_the_vertex_cap_keeps_every_loop() {
        // An outline with `arcs` full circles along its top, then `holes` round holes.
        let card = |arcs: u32, holes: u32| {
            let arcs: String =
                (0..arcs).map(|i| format!("{{ op = 'A', points = {{ {}, 0, 4, 0, 360 }} }},", 10 * i + 5)).collect();
            let holes: String = (0..holes)
                .map(|i| {
                    let (x, r) = (10 * i + 5, 10 * i + 8);
                    format!(
                        "{{ op = 'M', points = {{ {r}, 12 }}, hole = true }}, {{ op = 'A', points = {{ {x}, 12, 3, 0, 360 }} }}, \
                         {{ op = 'L', points = {{ {r}, 12 }} }}, {{ op = 'Z', points = {{}} }},"
                    )
                })
                .collect();
            parse(&format!(
                "{{ commands = {{ {{ op = 'M', points = {{ 0, 0 }} }}, {arcs} {{ op = 'L', points = {{ '100%', 0 }} }}, \
                 {{ op = 'L', points = {{ '100%', '100%' }} }}, {{ op = 'L', points = {{ 0, '100%' }} }}, \
                 {{ op = 'Z', points = {{}} }}, {holes} }} }}"
            ))
            .map(|shape| shape.unwrap().sdf_polygon(at(640.0, 24.0)))
        };
        let err = card(0, 42).unwrap_err().to_string();
        assert!(err.contains("41 holes"), "{err}");
        let sdf = card(0, 41).unwrap();
        let loops: Vec<_> = sdf.split(|p| *p == SDF_BREAK).collect();
        assert_eq!(loops.len(), 43, "42 loops, each ended by a break");
        assert!(loops[..42].iter().all(|l| l.len() >= 5 && l[0] == l[l.len() - 1]), "{loops:?}");
        assert_eq!(loops[42], [], "nothing past the last break");
        assert!(distance(&sdf, Point::new(5.0, 12.0)) > 0.0 && distance(&sdf, Point::new(5.0, 3.0)) < 0.0);
        let long = card(60, 30).unwrap().len();
        assert!(long <= SDF_POINTS, "{long} vertices");
        assert!(long > SDF_POINTS - 32, "{long} vertices leave the cap unspent");
    }

    /// `shape` tweens point by point: px against `"NN%"` lerps as the resolved px would, a corner's
    /// radius and smoothing clamp under overshoot, and other commands snap.
    #[test]
    fn shapes_tween_point_by_point_and_clamp_overshoot() {
        let shape = |x: &str, radius: f32, smoothing: f32| {
            parse(&format!(
                "{{ commands = {{ {{ op = 'M', points = {{ {x}, 0 }} }}, {{ op = 'corner', points = {{ 40, 0 }}, \
                 radius = {radius}, corner_smoothing = {smoothing} }}, {{ op = 'L', points = {{ 0, 20 }} }}, \
                 {{ op = 'Z', points = {{}} }} }} }}"
            ))
            .unwrap()
            .unwrap()
        };
        let (from, to) = (shape("0", 2.0, 0.0), shape("'50%'", 6.0, 1.0));
        let mid = from.lerp(&to, 0.5).unwrap();
        // Half of 0 and half of 50% of 40.
        assert_eq!(mid.bez(at(40.0, 20.0)).elements()[0], PathEl::MoveTo(Point::new(10.0, 0.0)));
        let points = |s: &Shape| match s {
            Shape::Contour { path, .. } => path.points.clone(),
            Shape::Melt(_) => unreachable!(),
        };
        assert_eq!((points(&mid)[4], points(&mid)[5]), (4.0, 0.5));
        let under = from.lerp(&to, -1.0).unwrap();
        let over = from.lerp(&to, 2.0).unwrap();
        assert_eq!((points(&under)[4], points(&under)[5], points(&over)[5]), (0.0, 0.0, 1.0));
        let other = parse(
            "{ commands = { { op = 'M', points = { 0, 0 } }, { op = 'L', points = { 40, 0 } }, \
             { op = 'L', points = { 0, 20 } }, { op = 'Z', points = {} } } }",
        )
        .unwrap()
        .unwrap();
        assert!(from.lerp(&other, 0.5).is_none(), "another command list snaps");
    }
}

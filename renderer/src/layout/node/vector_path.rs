//! Bounded path commands. Parsed once per apply, compared by value in display lists.
use super::corner::Squircle;
use super::prop::{Keyword, Prop, keywords};
use super::{LayoutError, Property, input, invalid, only_keys};
use crate::lua::luacats::{lua_shape, spelled};
use std::f32::consts::FRAC_PI_2;
use std::rc::Rc;

use kurbo::{BezPath, Point, Vec2};
use mlua::{Table, Value};

keywords! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PathOp { M = "M", L = "L", Q = "Q", C = "C", A = "A", Corner = "corner", Z = "Z" }
}

keywords! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StrokeCap { Butt, Round, Square }
}

keywords! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StrokeJoin { Miter, Round, Bevel }
}

lua_shape! {
    /// Node-local logical pixels. Q has one control point, C has two, then the endpoint. A is
    /// centre x, centre y, radius, start and sweep in degrees clockwise from the +x axis. `corner`
    /// is one point, rounded by `radius` and `corner_smoothing`. `hole` on a subpath's first
    /// command cuts it out of the fill, whatever its winding.
    #[alias = "PathCommand"]
    #[derive(Debug, Clone, PartialEq)]
    pub struct PathCommand {
        pub op: PathOp,
        pub points: Vec<f32>,
        pub hole: Option<bool>,
        pub radius: Option<f32>,
        pub corner_smoothing: Option<f32>,
    }
}

/// One command's op and flags; its numbers live in [`PathData::points`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub op: PathOp,
    pub hole: bool,
    /// Opens a subpath: an `M`, or an `A` first or after a `Z`.
    pub begins: bool,
}

impl PathOp {
    /// How many numbers the op takes, and how many of those lead as pixels: an arc's start and
    /// sweep are angles.
    fn arity(self) -> (usize, usize) {
        match self {
            Self::M | Self::L => (2, 2),
            Self::Q => (4, 4),
            Self::A => (5, 3),
            // The point, then `radius` and `corner_smoothing`.
            Self::Corner => (4, 3),
            Self::C => (6, 6),
            Self::Z => (0, 0),
        }
    }
}

keywords! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TrimAxis { Length, X }
}

/// Parsed `commands`. The segments are shared by every frame of a tween between two layouts, so
/// a frame allocates only its numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct PathData {
    pub segments: Rc<[Segment]>,
    pub points: Vec<f32>,
}

/// Every number is finite: the parser rejects the rest and a tween clamps. `Eq` lets `Rc<PathData>`
/// compare by pointer first, so an unchanged path's display list entry skips its numbers.
impl Eq for PathData {}

/// Each segment, the range of its numbers, and how many of those lead as pixels.
fn spans(segments: &[Segment]) -> impl Iterator<Item = (Segment, std::ops::Range<usize>, usize)> {
    let mut at = 0;
    segments.iter().map(move |segment| {
        let (count, pixels) = segment.op.arity();
        at += count;
        (*segment, at - count..at, pixels)
    })
}

impl PathData {
    /// Each segment with its numbers.
    pub fn iter(&self) -> impl Iterator<Item = (Segment, &[f32])> {
        spans(&self.segments).map(|(segment, range, _)| (segment, &self.points[range]))
    }

    /// Every pixel value, which display scaling multiplies and a tween clamps.
    pub fn for_each_pixel(&mut self, mut f: impl FnMut(&mut f32)) {
        for (_, range, pixels) in spans(&self.segments) {
            self.points[range.start..range.start + pixels].iter_mut().for_each(&mut f);
        }
    }

    /// `self` moved toward `to` by `t`, or `None` when their segments differ and the tween snaps.
    pub fn lerp(&self, to: &Self, t: f32) -> Option<Self> {
        if !Rc::ptr_eq(&self.segments, &to.segments) && self.segments != to.segments {
            return None;
        }
        // In f64: arc angles may be any finite f32, and their difference can overflow one.
        let t = f64::from(t);
        let mut points: Vec<f32> = self
            .points
            .iter()
            .zip(&to.points)
            .map(|(&p, &q)| {
                let (p, q) = (f64::from(p), f64::from(q));
                (p + (q - p) * t).clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
            })
            .collect();
        for (segment, range, pixels) in spans(&self.segments) {
            points[range.start..range.start + pixels].iter_mut().for_each(|p| *p = p.clamp(-COORD_MAX, COORD_MAX));
            if matches!(segment.op, PathOp::A | PathOp::Corner) {
                // An overshooting easing must not turn a radius negative.
                points[range.start + 2] = points[range.start + 2].max(0.0);
            }
            if segment.op == PathOp::Corner {
                points[range.start + 3] = points[range.start + 3].clamp(0.0, 1.0);
            }
        }
        Some(Self { segments: self.segments.clone(), points })
    }

    /// The commands as one path with its origin at `origin`. A `corner` rounds its point between
    /// the line in from the pen and the line out to what follows.
    pub fn bez(&self, origin: (f32, f32)) -> BezPath {
        let at = |x: f32, y: f32| Point::new(f64::from(origin.0 + x), f64::from(origin.1 + y));
        let arc_at = |p: &[f32], a: f32| at(p[0] + p[2] * a.cos(), p[1] + p[2] * a.sin());
        let arc_start = |p: &[f32]| p[3].rem_euclid(360.0).to_radians();
        let commands: Vec<_> = self.iter().collect();
        let mut path = BezPath::new();
        // Where the pen is and where its subpath began. A joining line under 0.001 px is dropped: femtovg
        // keeps the repeated point, and anti-aliasing draws the stub's edge as a spike.
        let (mut pen, mut first) = (Point::ZERO, Point::ZERO);
        for (i, &(segment, p)) in commands.iter().enumerate() {
            match segment.op {
                PathOp::M => {
                    (pen, first) = (at(p[0], p[1]), at(p[0], p[1]));
                    path.move_to(pen);
                }
                PathOp::L => {
                    pen = at(p[0], p[1]);
                    path.line_to(pen);
                }
                PathOp::Q => {
                    pen = at(p[2], p[3]);
                    path.quad_to(at(p[0], p[1]), pen);
                }
                PathOp::C => {
                    pen = at(p[4], p[5]);
                    path.curve_to(at(p[0], p[1]), at(p[2], p[3]), pen);
                }
                PathOp::A => {
                    let start = arc_start(p);
                    let sweep = p[4].clamp(-360.0, 360.0).to_radians();
                    let from = arc_at(p, start);
                    if segment.begins {
                        (pen, first) = (from, from);
                        path.move_to(pen);
                    } else {
                        line_to(&mut path, pen, from);
                        pen = from;
                    }
                    // One cubic per quarter turn or less, handles 4/3·tan(step/4) radii long; none for no sweep.
                    let segments = (sweep.abs() / FRAC_PI_2).ceil();
                    let step = sweep / segments;
                    let handle = f64::from(p[2] * 4.0 / 3.0 * (step / 4.0).tan());
                    for i in 1..=segments as usize {
                        let (a0, a1) = (start + step * (i - 1) as f32, start + step * i as f32);
                        let tangent = |a: f32| Vec2::new(f64::from(-a.sin()), f64::from(a.cos())) * handle;
                        let end = arc_at(p, a1);
                        path.curve_to(pen + tangent(a0), end - tangent(a1), end);
                        pen = end;
                    }
                }
                PathOp::Corner => {
                    let vertex = at(p[0], p[1]);
                    // What follows in this subpath; a `Z` closes toward its start, an open end has no turn.
                    let next = commands.get(i + 1).filter(|(next, _)| !next.begins).map(|(next, q)| match next.op {
                        PathOp::Z => first,
                        PathOp::A => arc_at(q, arc_start(q)),
                        _ => at(q[0], q[1]),
                    });
                    match next {
                        Some(next) => {
                            // A side shared with the next corner is half each's.
                            let shared = commands.get(i + 1).is_some_and(|(next, _)| next.op == PathOp::Corner);
                            pen = corner(&mut path, (pen, vertex, next), p[2], p[3], shared);
                        }
                        None => {
                            line_to(&mut path, pen, vertex);
                            pen = vertex;
                        }
                    }
                }
                PathOp::Z => {
                    pen = first;
                    path.close_path();
                }
            }
        }
        path
    }
}

/// A line from `pen` to `to`, unless it is a stub under 0.001 px, which anti-aliasing draws as a spike.
fn line_to(path: &mut BezPath, pen: Point, to: Point) {
    if pen.distance(to) >= 1e-3 {
        path.line_to(to);
    }
}

/// A smoothed corner (`corner.rs`) at `vertex`, in the frame of the line in from `pen` and the
/// line out toward `next`, so it meets both tangentially; the end it leaves the pen at. A turn
/// other than a quarter shears the frame. The radius is cut to the sides it has.
fn corner(
    path: &mut BezPath,
    (pen, vertex, next): (Point, Point, Point),
    radius: f32,
    smoothing: f32,
    shared: bool,
) -> Point {
    let (into, out) = (vertex - pen, next - vertex);
    let budget = into.hypot().min(if shared { out.hypot() / 2.0 } else { out.hypot() }) as f32;
    let radius = radius.min(budget);
    // A straight reversal has no corner to round.
    if radius <= 0.0 || into.normalize().dot(out.normalize()) < -0.999 {
        line_to(path, pen, vertex);
        return vertex;
    }
    let (a, b) = (into.normalize(), out.normalize());
    let squircle = Squircle::new(radius, smoothing, budget);
    let place = |(x, y): (f32, f32)| vertex + b * f64::from(x) - a * f64::from(y);
    let start = place(squircle.points[0]);
    line_to(path, pen, start);
    for i in 0..3 {
        let [from, c1, c2, end] = squircle.cubic(i);
        // Smoothing 0 leaves the outer two as points, whose zero-length edges femtovg draws as spikes.
        if [c1, c2, end] != [from; 3] {
            path.curve_to(place(c1), place(c2), place(end));
        }
    }
    place(squircle.points[9])
}

/// A tween's frame in the property map: the parsed path or outline itself, so the parser takes it
/// back without building and walking a Lua table per command.
pub(super) struct Tweened<T>(pub Rc<T>);

pub(crate) fn tweened<T: 'static>(lua: &mlua::Lua, path: &Rc<T>) -> mlua::Result<Value> {
    lua.create_any_userdata(Tweened(path.clone())).map(Value::UserData)
}

/// The bound on every pixel value, in node-local logical pixels.
pub const COORD_MAX: f32 = 8192.0;

/// A `path` node's paint, shared by its [`PaintStyle`](super::PaintStyle) and its display-list draw.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorPath {
    pub commands: Rc<PathData>,
    pub fill: Option<super::Fill>,
    pub stroke: Option<super::Fill>,
    pub stroke_width: f32,
    pub stroke_cap: StrokeCap,
    pub stroke_join: StrokeJoin,
    /// `trim_start` and `trim_end`: the stroked fraction of the path's length or of the node's width.
    pub trim: (f32, f32),
    pub trim_axis: TrimAxis,
    /// Moves the geometry inside the node box before trimming; logical px, buffer px once built.
    pub shift: (f32, f32),
}

pub(crate) struct PathCommands;
spelled!(PathCommands => "PathCommand[]");

// ponytail: 4096 commands and six coordinates each; larger drawings need a measured tessellation budget.
const MAX_COMMANDS: usize = 4096;

impl Prop for PathCommands {
    type Out = Rc<PathData>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let table = match value {
            None => return Ok(Rc::new(PathData { segments: Rc::from([]), points: Vec::new() })),
            Some(Value::Table(table)) => table,
            Some(Value::UserData(ud)) if let Ok(frame) = ud.borrow::<Tweened<PathData>>() => {
                return Ok(frame.0.clone());
            }
            Some(_) => return Err(invalid(row.name, "expected a command array")),
        };
        let len = input::array_len(row.name, table, MAX_COMMANDS)?;
        let mut path = PathData { segments: Rc::from([]), points: Vec::with_capacity(len * 2) };
        let mut segments = Vec::with_capacity(len);
        for i in 1..=len {
            let name = format!("{}[{i}]", row.name);
            let input: Table = table.raw_get(i).map_err(|e| invalid(&name, e.to_string()))?;
            only_keys(&name, &input, PathCommand::KEYS)?;
            let numbers: Table = input.get("points").map_err(|e| invalid(&name, e.to_string()))?;
            input::array_len(&name, &numbers, 6)?;
            let command = PathCommand {
                op: input::field(&name, &input, "op")?,
                points: input::read(&name, "points", Value::Table(numbers))?,
                hole: input::field(&name, &input, "hole")?,
                radius: input::field(&name, &input, "radius")?,
                corner_smoothing: input::field(&name, &input, "corner_smoothing")?,
            };
            push(&name, &mut segments, &mut path.points, command)?;
        }
        path.segments = segments.into();
        Ok(Rc::new(path))
    }
}

/// Checks one command against its op and the subpath it continues, then appends it; `outline`'s
/// parser shares it. A corner's `radius` and `corner_smoothing` follow its point in `points`.
pub(super) fn push(
    name: &str,
    segments: &mut Vec<Segment>,
    points: &mut Vec<f32>,
    PathCommand { op, points: mut numbers, hole, radius, corner_smoothing }: PathCommand,
) -> Result<(), LayoutError> {
    match (op, radius) {
        (PathOp::Corner, Some(radius)) => numbers.extend([radius, corner_smoothing.unwrap_or(0.0)]),
        (PathOp::Corner, None) => return Err(invalid(name, "corner needs a radius")),
        (_, None) if corner_smoothing.is_none() => {}
        _ => return Err(invalid(name, "radius and corner_smoothing belong on a corner")),
    }
    let (count, pixels) = op.arity();
    if numbers.len() != count
        || numbers.iter().any(|n| !n.is_finite())
        || numbers[..pixels].iter().any(|n| n.abs() > COORD_MAX)
    {
        let given = if op == PathOp::Corner { 2 } else { count };
        return Err(invalid(name, format!("{} needs {given} finite numbers, coordinates in [-8192, 8192]", op.name())));
    }
    if matches!(op, PathOp::A | PathOp::Corner) && numbers[2] < 0.0 {
        return Err(invalid(name, format!("{} needs a radius of at least 0", op.name())));
    }
    if op == PathOp::Corner && !(0.0..=1.0).contains(&numbers[3]) {
        return Err(invalid(name, "corner_smoothing is in [0, 1]"));
    }
    let open = segments.last().is_some_and(|last| last.op != PathOp::Z);
    let begins = !open || op == PathOp::M;
    if hole.is_some() && !begins {
        return Err(invalid(name, "hole belongs on the command that begins a subpath"));
    }
    if begins && !matches!(op, PathOp::M | PathOp::A) {
        return Err(invalid(name, "begin each subpath with M or A"));
    }
    segments.push(Segment { op, hole: hole == Some(true), begins });
    points.extend(numbers);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::fields;
    #[test]
    fn path_commands_validate_limits_structure_and_subpaths() {
        let lua = mlua::Lua::new();
        let parse = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            PathCommands::read(&fields::path::commands.row, Some(&value))
        };
        assert!(parse("{}").unwrap().segments.is_empty());
        let command = parse(
            r#"(function()
            local reads = 0
            return {setmetatable({op = 'M'}, {__index = function(_, key)
                if key == 'points' then
                    reads = reads + 1
                    assert(reads == 1, 'points getter ran twice')
                    return {1, 2}
                end
            end})}
        end)()"#,
        )
        .unwrap();
        assert_eq!(command.points, [1.0, 2.0]);

        assert!(parse(r#"{{op='M',points={1,2}}, {op='Q',points={2,3,4,5}}, {op='C',points={1,2,3,4,5,6}}, {op='Z',points={}}}"#).is_ok());
        let ring = parse(
            "{{op='A',points={8,8,8,0,360}}, {op='Z',points={}}, {op='A',points={8,8,4,0,-360},hole=true}, \
             {op='M',points={0,0},hole=false}, {op='A',points={8,8,8,9000,-9000}}}",
        )
        .unwrap();
        let flags = |f: fn(&Segment) -> bool| ring.segments.iter().map(f).collect::<Vec<_>>();
        assert_eq!(flags(|s| s.hole), [false, false, true, false, false]);
        assert_eq!(flags(|s| s.begins), [true, false, true, true, false]);
        // A tween's frame comes back as the same parsed path, no table walked.
        let frame = tweened(&lua, &ring).unwrap();
        assert!(Rc::ptr_eq(&PathCommands::read(&fields::path::commands.row, Some(&frame)).unwrap(), &ring));
        for bad in [
            "1",
            "{extra=1}",
            "{[2]={op='M',points={1,2}}}",
            "{{op='L',points={1,2}}}",
            "{{op='B',points={1,2}}}",
            "{{op='A',points={1,2,3,4}}}",
            "{{op='A',points={1,2,-1,0,90}}}",
            "{{op='A',points={1,2,8193,0,90}}}",
            "{{op='A',points={1,2,3,0,math.huge}}}",
            "{{op='M',points={1,2}},{op='L',points={2,3},hole=true}}",
            "{{op='M',points={1,2}},{op='A',points={1,2,3,0,90},hole=true}}",
            "{{op='M',points={1,2},hole=1}}",
            "{{op='M',points={1}}}",
            "{{op='M',points={1,2,3}}}",
            "{{op='M',points={1,0/0}}}",
            "{{op='M',points={1,math.huge}}}",
            "{{op='M',points={1,8193}}}",
            "{{op='M',points={1,2},oops=true}}",
            "{{op='M',points={1,2,extra=3}}}",
            "{{op='M',points={1,2}},{op='Z',points={}},{op='L',points={2,3}}}",
            "(function() local a={} for i=1,4097 do a[i]={op='M',points={1,2}} end return a end)()",
        ] {
            assert!(parse(bad).is_err(), "accepted {bad}");
        }
        assert!(
            parse("(function() local a={} for i=1,4096 do a[i]={op='M',points={-8192,8192}} end return a end)()")
                .is_ok()
        );
    }

    /// A `corner` with nothing after it and no `Z` has no outgoing side to round toward: the open
    /// subpath ends in a plain line to its point, not a curve toward its start.
    #[test]
    fn an_open_subpath_ending_in_a_corner_draws_a_line_to_it() {
        let lua = mlua::Lua::new();
        let value: Value = lua
            .load("{{op='M',points={0,0}}, {op='L',points={10,0}}, {op='corner',points={10,10},radius=4}}")
            .eval()
            .unwrap();
        let path = PathCommands::read(&fields::path::commands.row, Some(&value)).unwrap();
        let bez = path.bez((0.0, 0.0));
        assert_eq!(bez.elements().last(), Some(&kurbo::PathEl::LineTo(Point::new(10.0, 10.0))));
        assert!(bez.elements().iter().all(|el| !matches!(el, kurbo::PathEl::CurveTo(..))), "{bez:?}");
    }

    #[test]
    fn stroke_caps_joins_and_trims_parse_and_refuse_out_of_range() {
        let lua = mlua::Lua::new();
        let style = |fields: &str| -> Result<_, LayoutError> {
            let table: Table = lua.load(format!("return {{ kind = 'path', {fields} }}")).eval().unwrap();
            match super::super::paint_style("path", &super::super::props_from_table(&table))? {
                Some(super::super::PaintStyle::Path(path)) => Ok((path.stroke_cap, path.stroke_join, path.trim)),
                other => panic!("a path paints, got {other:?}"),
            }
        };
        assert_eq!(style("").unwrap(), (StrokeCap::Butt, StrokeJoin::Miter, (0.0, 1.0)));
        assert_eq!(
            style("stroke_cap = 'round', stroke_join = 'bevel', trim_start = 0.25, trim_end = 0.5").unwrap(),
            (StrokeCap::Round, StrokeJoin::Bevel, (0.25, 0.5))
        );
        for bad in ["stroke_cap = 'rounded'", "stroke_join = 1", "trim_start = -0.1", "trim_end = 1.5"] {
            assert!(style(bad).is_err(), "accepted {bad}");
        }
    }
}

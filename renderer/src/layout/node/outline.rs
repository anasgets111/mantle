//! `outline`: a box's shape as one closed contour of path commands, each point placed against the
//! box's size where it is painted and hit.

use std::f64::consts::{PI, TAU};
use std::rc::Rc;

use kurbo::{BezPath, ParamCurve, ParamCurveDeriv, PathEl, Point, Shape};
use mlua::Value;

use super::prop::Prop;
use super::vector_path::{PathCommand, PathData, PathOp, Segment, Tweened, push};
use super::{LayoutError, Property, invalid, preview_for_error, style::parse_percent};
use crate::lua::luacats::{LuaType, lua_shape, spelled};
use crate::text::snap::LogicalRect;

lua_shape! {
    /// A point `px` from an edge or the centre of the box, or from `"NN%"` of its width or height.
    #[alias = "OutlineAnchor"]
    pub(crate) struct AnchorInput {
        from: String,
        px: Option<f32>,
    }
}

lua_shape! {
    /// As a `PathCommand` with no `hole`: each coordinate is px from the box's top left, `"NN%"` of
    /// its width or height, or an `OutlineAnchor`; a radius or angle is a number.
    #[alias = "OutlineCommand"]
    pub(crate) struct OutlineCommand {
        op: PathOp,
        points: Vec<Coord>,
        radius: Option<f32>,
        corner_smoothing: Option<f32>,
    }
}

lua_shape! {
    /// One closed contour: an `M`, at least two more commands, then `Z`.
    #[alias = "Outline"]
    struct OutlineInput {
        commands: Vec<OutlineCommand>,
    }
}

/// One number of a command: `px` plus a share of the box's width or height, whichever its slot
/// is; an edge's name holds it to its own axis, `0` for x.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Coord {
    px: f32,
    share: f32,
    axis: Option<usize>,
}

spelled!(Coord => "OutlinePoint");

impl super::input::Input for Coord {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let percent =
            |s: &str| parse_percent(s).ok_or_else(|| invalid(property, format!("`{key}`: {s} is not \"NN%\"")));
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

/// A box's own contour, replacing its rounded rectangle.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    /// Each number's px part; a coordinate also has its share of the box in `shares`.
    path: PathData,
    shares: Vec<[f32; 2]>,
}

/// The vertex cap of the polyline `mantle_sdf` measures, two to each `vec4` of `mantle_outline`.
pub const SDF_POINTS: usize = 256;

impl Outline {
    /// The contour of a box at `rect`.
    pub fn bez(&self, rect: LogicalRect) -> BezPath {
        let mut path = self.path.clone();
        for (n, [w, h]) in path.points.iter_mut().zip(&self.shares) {
            *n += w * rect.width + h * rect.height;
        }
        path.bez((rect.x, rect.y))
    }

    /// The contour flattened to within `tolerance` px.
    pub fn polygon(&self, rect: LogicalRect, tolerance: f64) -> Vec<Point> {
        let mut out = Vec::new();
        kurbo::flatten(self.bez(rect), tolerance, |element| {
            if let PathEl::MoveTo(p) | PathEl::LineTo(p) = element {
                out.push(p);
            }
        });
        apart(out)
    }

    /// The contour as at most [`SDF_POINTS`] vertices, each curve cut where it has turned 3°, or 6°
    /// and so on while that is too many. A distance to it bends its gradient by at most that
    /// angle at a vertex, where a tolerance would leave a small corner's chords tens of degrees
    /// apart; a join the commands meet at smoothly stays smooth.
    pub fn sdf_polygon(&self, rect: LogicalRect) -> Vec<Point> {
        let segments: Vec<_> = self.bez(rect).segments().map(|s| s.to_cubic()).collect();
        let turns: Vec<f64> = segments
            .iter()
            .map(|c| {
                let d = c.deriv();
                let angles = (0..=16).map(|i| d.eval(f64::from(i) / 16.0).to_vec2()).filter(|v| v.hypot2() > 0.0);
                let angles: Vec<f64> = angles.map(|v| v.atan2()).collect();
                angles.windows(2).map(|w| (w[1] - w[0] + PI).rem_euclid(TAU) - PI).map(f64::abs).sum()
            })
            .collect();
        let mut step = 3f64.to_radians();
        loop {
            let out = apart(segments.iter().zip(&turns).flat_map(|(c, turn)| {
                let n = (turn / step).ceil().max(1.0);
                (0..n as usize).map(move |i| c.eval(i as f64 / n))
            }));
            if out.len() <= SDF_POINTS {
                return out;
            }
            if step > PI {
                // More straight commands than the cap: every nth vertex.
                return out.iter().copied().step_by(out.len().div_ceil(SDF_POINTS)).collect();
            }
            step *= 2.0;
        }
    }

    /// `rect` grown to hold the contour, which may reach past it.
    pub fn bounds(&self, rect: LogicalRect) -> LogicalRect {
        let b = self.bez(rect).bounding_box();
        let (x0, y0) = ((b.x0 as f32).min(rect.x), (b.y0 as f32).min(rect.y));
        let (x1, y1) = ((b.x1 as f32).max(rect.x + rect.width), (b.y1 as f32).max(rect.y + rect.height));
        LogicalRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
    }

    /// `self` moved toward `to` by `t`, or `None` when their commands differ and the tween snaps.
    /// A point is linear in its px and its share, so one written in px and the other in `"NN%"`
    /// lerps as the two resolved against the box would, at every size.
    pub fn lerp(&self, to: &Self, t: f32) -> Option<Self> {
        let path = self.path.lerp(&to.path, t)?;
        let shares = self.shares.iter().zip(&to.shares).map(|(a, b)| [0, 1].map(|i| a[i] + (b[i] - a[i]) * t));
        Some(Self { path, shares: shares.collect() })
    }

    /// Every px value, which display scaling multiplies; shares of the box scale with the box.
    pub fn scaled(&self, k: f32) -> Self {
        let mut path = self.path.clone();
        path.for_each_pixel(|n| *n *= k);
        Self { path, shares: self.shares.clone() }
    }
}

/// `points` with each one under 0.001 px from the last kept dropped, the closing one included: f32
/// rounding leaves such stubs where commands meet, and femtovg draws a stub's edge as a spike.
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

/// Signed distance from `p` to the closed `polygon`, negative inside by the even-odd rule. The
/// shader's `mantle_sdf` for an outline is the same function, so this is its CPU mirror.
pub fn distance(polygon: &[Point], p: Point) -> f64 {
    let Some(&last) = polygon.last() else { return f64::INFINITY };
    let (mut nearest, mut inside, mut a) = (f64::INFINITY, false, last);
    for &b in polygon {
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

/// `polygon` moved `by` px inward, outward when negative: each edge along its normal, each vertex
/// where its two moved edges meet, a mitre held to four times `by`.
pub fn inset(polygon: &[Point], by: f64) -> Vec<Point> {
    let n = polygon.len();
    // The shoelace area's sign says which side of each edge is inside.
    let area: f64 = (0..n).map(|i| polygon[i].to_vec2().cross(polygon[(i + 1) % n].to_vec2())).sum();
    let normal = |i: usize| {
        let e = polygon[(i + 1) % n] - polygon[i];
        kurbo::Vec2::new(-e.y, e.x).normalize() * area.signum()
    };
    (0..n)
        .map(|i| {
            let m = (normal((i + n - 1) % n) + normal(i)) / 2.0;
            polygon[i] + m * by / m.hypot2().max(1.0 / 16.0)
        })
        .collect()
}

/// `outline = { commands = { .. } }`.
impl LuaType for Outline {
    fn lua() -> String {
        "Outline".into()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        OutlineInput::classes(out);
    }
}

// ponytail: 256 commands; a longer contour needs a measured flattening and `mantle_sdf` budget.
const MAX_COMMANDS: usize = 256;

impl Prop for Outline {
    type Out = Option<Rc<Outline>>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let name = row.name;
        let table = match value {
            None => return Ok(None),
            Some(Value::Table(table)) => table,
            Some(Value::UserData(ud)) if let Ok(frame) = ud.borrow::<Tweened<Outline>>() => {
                return Ok(Some(frame.0.clone()));
            }
            Some(value) => return Err(invalid(name, format!("expected a table, got {}", preview_for_error(value)))),
        };
        let commands = OutlineInput::read(name, table)?.commands;
        if commands.len() > MAX_COMMANDS {
            return Err(invalid(name, format!("at most {MAX_COMMANDS} commands")));
        }
        let (mut segments, mut points, mut shares) = (Vec::new(), Vec::new(), Vec::new());
        for (i, OutlineCommand { op, points: coords, radius, corner_smoothing }) in commands.into_iter().enumerate() {
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
            let command = PathCommand { op, points: numbers, hole: None, radius, corner_smoothing };
            push(&name, &mut segments, &mut points, command)?;
            shares.resize(points.len(), [0.0; 2]);
        }
        let ops: Vec<PathOp> = segments.iter().map(|s: &Segment| s.op).collect();
        if ops.first() != Some(&PathOp::M)
            || ops.last() != Some(&PathOp::Z)
            || ops.len() < 4
            || ops[1..].contains(&PathOp::M)
            || ops[..ops.len() - 1].contains(&PathOp::Z)
        {
            return Err(invalid(name, "one closed contour: M, at least two commands, then Z, with no other M or Z"));
        }
        Ok(Some(Rc::new(Outline { path: PathData { segments: segments.into(), points }, shares })))
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
mod tests {
    use super::*;
    use crate::layout::node::fields;

    fn parse(src: &str) -> Result<Option<Rc<Outline>>, LayoutError> {
        let lua = mlua::Lua::new();
        let value: Value = lua.load(src).eval().unwrap();
        Outline::read(&fields::paint::outline.row, Some(&value))
    }

    fn at(w: f32, h: f32) -> LogicalRect {
        LogicalRect { x: 0.0, y: 0.0, width: w, height: h }
    }

    /// Every point form resolves against the box it is painted on, so the tail stays centred and
    /// 8 px deep at any size, and reaches past the box.
    #[test]
    fn points_follow_the_box_size() {
        let tail = parse(TAIL).unwrap().unwrap();
        for (w, h) in [(32.0, 32.0), (100.0, 50.0)] {
            let b = tail.bounds(at(w, h));
            assert_eq!((b.x, b.y, b.width), (0.0, 0.0, w));
            assert!((b.height - (h + 8.0)).abs() < 0.5, "{b:?}");
            assert_eq!(tail.bez(at(w, h)).elements()[0], PathEl::MoveTo(Point::new(f64::from(w) / 2.0, 0.0)));
            let inside = |x: f32, y: f32| distance(&tail.polygon(at(w, h), 0.25), Point::new(x.into(), y.into())) < 0.0;
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

    #[test]
    fn an_outline_refuses_what_is_not_one_closed_contour() {
        let contour = |body: &str| format!("{{ commands = {{ {{ op = 'M', points = {{ 0, 0 }} }}, {body} }} }}");
        let ok = contour("{ op = 'L', points = { 9, 0 } }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }");
        assert!(parse(&ok).is_ok());
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
            contour(
                "{ op = 'L', points = { 9, 0 }, hole = true }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }",
            ),
            contour("{ op = 'corner', points = { 9, 0 } }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }"),
            contour(
                "{ op = 'corner', points = { 9, 0 }, radius = 2, corner_smoothing = 2 }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }",
            ),
            contour(
                "{ op = 'L', points = { 9, 0 }, radius = 2 }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }",
            ),
            contour(
                "{ op = 'L', points = { { from = 'top' }, 0 } }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }",
            ),
            contour(
                "{ op = 'L', points = { { from = 'middle' }, 0 } }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }",
            ),
            contour("{ op = 'L', points = { '5px', 0 } }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }"),
            contour(
                "{ op = 'A', points = { 9, 9, '50%', 0, 90 } }, { op = 'L', points = { 0, 9 } }, { op = 'Z', points = {} }",
            ),
        ] {
            assert!(parse(&bad).is_err(), "accepted {bad}");
        }
        let lua = mlua::Lua::new();
        let table = |src: &str| lua.load(format!("return {{ kind = 'rect', {src} }}")).eval::<mlua::Table>().unwrap();
        let style = |src: &str| super::super::paint_style("rect", &super::super::props_from_table(&table(src)));
        assert!(style(&format!("outline = {TAIL}, border_width = 2, border_color = '#000000'")).is_ok());
        for bad in ["radius = 4", "corner_shape = 'scoop'", "border_width = { top = 2 }"] {
            assert!(style(&format!("outline = {TAIL}, {bad}")).is_err(), "accepted {bad}");
        }
    }

    /// A glass lens takes its normal from finite differences of `mantle_sdf`, so the distance and
    /// its gradient must not jump where the side meets the filleted tail or round a corner.
    #[test]
    fn the_sdf_and_its_gradient_are_continuous_across_the_joins() {
        let polygon = parse(TAIL).unwrap().unwrap().sdf_polygon(at(32.0, 32.0));
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

    /// `outline` tweens point by point: px against `"NN%"` lerps as the resolved px would, a corner's
    /// radius and smoothing clamp under overshoot, and other commands snap.
    #[test]
    fn outlines_tween_point_by_point_and_clamp_overshoot() {
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
        assert_eq!((mid.path.points[4], mid.path.points[5]), (4.0, 0.5));
        let under = from.lerp(&to, -1.0).unwrap();
        let over = from.lerp(&to, 2.0).unwrap();
        assert_eq!((under.path.points[4], under.path.points[5], over.path.points[5]), (0.0, 0.0, 1.0));
        let other = parse(
            "{ commands = { { op = 'M', points = { 0, 0 } }, { op = 'L', points = { 40, 0 } }, \
             { op = 'L', points = { 0, 20 } }, { op = 'Z', points = {} } } }",
        )
        .unwrap()
        .unwrap();
        assert!(from.lerp(&other, 0.5).is_none(), "another command list snaps");
    }
}

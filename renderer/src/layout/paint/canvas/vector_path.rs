//! Vector leaves use the same fills, canvas state and subtree effects as box paint.
use super::shape::fill_paint;
use crate::layout::node::{StrokeCap, StrokeJoin, TrimAxis, VectorPath};
use crate::text::snap::LogicalRect;

use femtovg::{Canvas, LineCap, LineJoin, Paint, Path, Solidity, Verb, renderer::OpenGl};
use kurbo::common::solve_cubic;
use kurbo::{BezPath, CubicBez, Line, ParamCurve, ParamCurveArclen, PathEl, PathSeg, Point, Shape, Vec2};

pub(super) fn paint(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    VectorPath { commands, fill, stroke, stroke_width, stroke_cap, stroke_join, taper, trim, trim_axis, shift }: &VectorPath,
    scale: f32,
) {
    if rect.is_empty() || commands.segments.is_empty() {
        return;
    }
    let holes = commands.segments.iter().filter(|segment| segment.begins).map(|segment| segment.hole);
    let path = femtovg_path(&commands.bez((rect.x + shift.0 * scale, rect.y + shift.1 * scale), scale), holes);
    if let Some(fill) = fill {
        canvas.fill_path(&path, &fill_paint(fill, rect));
    }
    if let Some(stroke) = stroke.as_ref().filter(|_| *stroke_width > 0.0) {
        let mut paint = fill_paint(stroke, rect);
        paint.set_line_width(*stroke_width * scale);
        paint.set_line_cap(match stroke_cap {
            StrokeCap::Butt => LineCap::Butt,
            StrokeCap::Round => LineCap::Round,
            StrokeCap::Square => LineCap::Square,
        });
        paint.set_line_join(match stroke_join {
            StrokeJoin::Miter => LineJoin::Miter,
            StrokeJoin::Round => LineJoin::Round,
            StrokeJoin::Bevel => LineJoin::Bevel,
        });
        let taper = (taper.0 * scale, taper.1 * scale);
        let mut stroke_path = |path: &Path| stroke_tapered(canvas, path, &paint, taper);
        if *trim_axis == TrimAxis::X {
            // The band is the node's box, which `shift` does not move.
            let band = |t: f32| f64::from(rect.x + t * rect.width);
            if trim.0 < trim.1 {
                stroke_path(&x_trimmed(&path, (band(trim.0), band(trim.1))));
            }
        } else if *trim == (0.0, 1.0) {
            stroke_path(&path);
        } else if trim.0 < trim.1 {
            stroke_path(&trimmed(&path, *trim));
        }
    }
}

fn stroke_tapered(canvas: &mut Canvas<OpenGl>, path: &Path, paint: &Paint, taper: (f32, f32)) {
    if taper == (0.0, 0.0) {
        canvas.stroke_path(path, paint);
    } else {
        // One device pixel in path units, under the node's own `scale` as well as the buffer's.
        let px = 1.0 / f64::from(canvas.transform().average_scale());
        let (closed, outline) = tapered(path, paint, taper, px);
        canvas.stroke_path(&closed, paint);
        canvas.fill_path(&outline, paint);
    }
}

/// `path`'s subpaths that end where they began, to stroke as they are, and the outline of the
/// others stroked as `paint` says, each narrowing to one device pixel, `px` path units, over
/// `taper` at its ends.
// ponytail: linear ramps only; an eased or arbitrary width profile would take knots here.
fn tapered(path: &Path, paint: &Paint, (start, end): (f32, f32), px: f64) -> (Path, Path) {
    // ponytail: detail stops growing past 64x magnification, bounding flattening and arc steps
    // under nested `scale`; a closer zoom would need tolerances from the visible part alone.
    let px = px.max(1.0 / 64.0);
    let (start, end) = (f64::from(start), f64::from(end));
    let mut runs: Vec<Vec<PathSeg>> = Vec::new();
    for (segment, begins) in segments(path) {
        match runs.last_mut() {
            Some(run) if !begins => run.push(segment),
            _ => runs.push(vec![segment]),
        }
    }
    let (mut closed, mut outline) = (Path::new(), Path::new());
    for run in runs {
        let first = run[0].start();
        // femtovg's own test for a closed contour.
        if first.distance(run[run.len() - 1].end()) < 0.01 * px {
            for (i, segment) in run.into_iter().enumerate() {
                append(&mut closed, segment, i > 0);
            }
            continue;
        }
        let mut points = vec![first];
        for segment in &run {
            kurbo::flatten(segment.path_elements(0.1 * px), 0.1 * px, |el| {
                if let PathEl::LineTo(p) = el {
                    points.push(p);
                }
            });
        }
        let length: f64 = points.windows(2).map(|w| w[0].distance(w[1])).sum();
        let ramp = |d: f64, over: f64| if over > 0.0 { d / over } else { f64::INFINITY };
        let full = f64::from(paint.line_width()) / 2.0;
        // Ends 1 px wide: femtovg's antialiasing fringe insets a fill by half a pixel, and on a
        // thinner sliver the inset crosses the far edge and paints a spike past the tip. Linear
        // from there, so the width between samples is what a sample there would have.
        let tip = full.min(0.5 * px);
        let half = |at: f64| tip + (full - tip) * ramp(at, start).min(ramp(length - at, end)).min(1.0);
        // Where the width's slope changes: where overlapping ramps cross, or else each ramp's end.
        let knots =
            if start + end >= length { vec![length * start / (start + end)] } else { vec![start, length - end] };
        let mut samples = vec![(first, half(0.0))];
        let mut walked = 0.0;
        for w in points.windows(2) {
            let edge = w[0].distance(w[1]);
            let inside = knots.iter().filter(|&&k| k > walked && k < walked + edge);
            samples.extend(inside.map(|&k| (w[0].lerp(w[1], (k - walked) / edge), half(k))));
            walked += edge;
            samples.push((w[1], half(walked)));
        }
        // femtovg's own merge distance: a shorter edge, a repeated point or a knot a rounding error
        // from a vertex, has no reliable direction. The wider of two merged keeps a ramp shorter
        // than that from tapering the whole edge after it.
        samples.dedup_by(|b, a| {
            let merge = a.0.distance(b.0) < 0.01 * px;
            if merge {
                a.1 = a.1.max(b.1);
            }
            merge
        });
        let dirs: Vec<Vec2> = samples.windows(2).map(|w| (w[1].0 - w[0].0).normalize()).collect();
        let Some(&last) = dirs.last() else { continue };
        let side = |sign: f64| {
            let mut out = Vec::new();
            for (i, &(p, h)) in samples.iter().enumerate() {
                let (a, b) = (dirs[i.saturating_sub(1)], dirs.get(i).copied().unwrap_or(last));
                let (na, nb) = (a.turn_90() * sign * h, b.turn_90() * sign * h);
                // An exact reversal turns one way: one side takes the join, the other the centre.
                let cross = if a.cross(b) == 0.0 && a.dot(b) < 0.0 { f64::MIN_POSITIVE } else { a.cross(b) };
                let turn = cross * sign;
                // Half-angle terms, |a + b| = 2cos(θ/2) and |a - b| = 2sin(θ/2): 1 + a·b cancels
                // to rounding noise on a reversal off the axes, where |a + b| is plainly near 0.
                let (sum, diff) = ((a + b).hypot(), (a - b).hypot());
                let miter = || p + (na + nb) * (2.0 / (sum * sum));
                // Inside a bend the offsets meet h * tan(θ/2) short of the vertex. Past half an
                // edge that crosses the next bend's, so a tighter bend goes through the centre,
                // which NonZero fills as the union of both sides. Not every bend: femtovg moves a
                // fill's vertices along their miters, up to 12 px off a centre point doubled back on.
                let edges = || p.distance(samples[i - 1].0).min(p.distance(samples[i + 1].0));
                if turn > 0.0 && 2.0 * h * diff <= edges() * sum {
                    out.push(miter());
                    continue;
                }
                out.push(p + na);
                if turn > 0.0 {
                    out.push(p);
                } else if turn < 0.0 {
                    match paint.line_join() {
                        LineJoin::Round => arc(&mut out, p, na, cross.atan2(a.dot(b)), px),
                        LineJoin::Miter if 2.0 <= f64::from(paint.miter_limit()) * sum => out.push(miter()),
                        LineJoin::Miter | LineJoin::Bevel => {}
                    }
                }
                if turn != 0.0 {
                    out.push(p + nb);
                }
            }
            out
        };
        let mut ring = side(1.0);
        let (p, h) = samples[samples.len() - 1];
        cap(&mut ring, paint.line_cap_end(), p, last.turn_90() * h, px);
        ring.extend(side(-1.0).into_iter().rev());
        cap(&mut ring, paint.line_cap_start(), first, -dirs[0].turn_90() * samples[0].1, px);
        let p = |p: Point| (p.x as f32, p.y as f32);
        let (x, y) = p(ring[0]);
        outline.move_to(x, y);
        for (x, y) in ring[1..].iter().map(|&point| p(point)) {
            outline.line_to(x, y);
        }
        outline.close();
    }
    (closed, outline)
}

/// Points strictly between `centre + from` and its rotation by `sweep` radians, on that circle.
fn arc(out: &mut Vec<Point>, centre: Point, from: Vec2, sweep: f64, px: f64) {
    // A chord's sag is radius * (step / 2)^2 / 2: at most 1/8 px with steps 1/sqrt(radius) radians.
    let steps = (sweep.abs() * (from.length() / px).sqrt()).ceil().max(1.0);
    for k in 1..steps as usize {
        out.push(centre + Vec2::from_angle(from.angle() + sweep * k as f64 / steps) * from.length());
    }
}

/// Finishes an end at `p` from `p + across` to `p - across`, around the side away from the stroke.
fn cap(out: &mut Vec<Point>, cap: LineCap, p: Point, across: Vec2, px: f64) {
    let ahead = -across.turn_90();
    match cap {
        LineCap::Butt => {}
        LineCap::Round => arc(out, p, across, -std::f64::consts::PI, px),
        LineCap::Square => out.extend([p + across + ahead, p - across + ahead]),
    }
}

/// `bez` for femtovg, each subpath a hole as `holes` says in turn, so winding never decides one.
pub(super) fn femtovg_path(bez: &BezPath, mut holes: impl Iterator<Item = bool>) -> Path {
    let p = |p: Point| (p.x as f32, p.y as f32);
    let mut path = Path::new();
    let mut pen = (0.0, 0.0);
    for element in bez.elements() {
        match *element {
            PathEl::MoveTo(to) => {
                let (x, y) = p(to);
                path.move_to(x, y);
                path.solidity(if holes.next() == Some(true) { Solidity::Hole } else { Solidity::Solid });
            }
            PathEl::LineTo(to) => {
                let (x, y) = p(to);
                path.line_to(x, y);
            }
            // femtovg halves a zero-length curve to its depth limit, 2047 calls, and draws nothing.
            PathEl::QuadTo(c, to) if [p(c), p(to)] == [pen; 2] => {}
            PathEl::CurveTo(c1, c2, to) if [p(c1), p(c2), p(to)] == [pen; 3] => {}
            PathEl::QuadTo(c, to) => {
                let ((cx, cy), (x, y)) = (p(c), p(to));
                path.quad_to(cx, cy, x, y);
            }
            PathEl::CurveTo(c1, c2, to) => {
                let ((ax, ay), (bx, by), (x, y)) = (p(c1), p(c2), p(to));
                path.bezier_to(ax, ay, bx, by, x, y);
            }
            PathEl::ClosePath => path.close(),
        }
        // femtovg's pen: `close` does not move it back to the subpath's start.
        if let Some(end) = element.end_point() {
            pen = p(end);
        }
    }
    path
}

/// `path`'s segments as kurbo's, each with whether it begins its subpath.
fn segments(path: &Path) -> Vec<(PathSeg, bool)> {
    let point = |x: f32, y: f32| Point::new(x.into(), y.into());
    let mut segments = Vec::new();
    let (mut pen, mut first, mut begins) = (Point::ZERO, Point::ZERO, false);
    for verb in path.verbs() {
        let segment = match verb {
            Verb::MoveTo(x, y) => {
                (pen, first, begins) = (point(x, y), point(x, y), true);
                continue;
            }
            Verb::LineTo(x, y) => PathSeg::Line(Line::new(pen, point(x, y))),
            Verb::BezierTo(ax, ay, bx, by, x, y) => {
                PathSeg::Cubic(CubicBez::new(pen, point(ax, ay), point(bx, by), point(x, y)))
            }
            Verb::Close => PathSeg::Line(Line::new(pen, first)),
            Verb::Solid | Verb::Hole => continue,
        };
        segments.push((segment, begins));
        (pen, begins) = (segment.end(), false);
    }
    segments
}

/// Appends `part`, continuing the current subpath unless it starts elsewhere.
fn append(out: &mut Path, part: PathSeg, continues: bool) {
    let p = |p: Point| (p.x as f32, p.y as f32);
    if !continues {
        let (x, y) = p(part.start());
        out.move_to(x, y);
    }
    if let PathSeg::Cubic(c) = part {
        let ((ax, ay), (bx, by), (x, y)) = (p(c.p1), p(c.p2), p(c.p3));
        out.bezier_to(ax, ay, bx, by, x, y);
    } else {
        let (x, y) = p(part.end());
        out.line_to(x, y);
    }
}

/// The part of `path` from `start` to `end` of its whole length, closing segments included. A
/// subpath kept whole ends where it began, which femtovg strokes as closed.
fn trimmed(path: &Path, (start, end): (f32, f32)) -> Path {
    const ACCURACY: f64 = 0.01;
    let segments: Vec<_> = segments(path).into_iter().map(|(s, begins)| (s, begins, s.arclen(ACCURACY))).collect();
    let total: f64 = segments.iter().map(|s| s.2).sum();
    let (from, to) = (f64::from(start) * total, f64::from(end) * total);
    let mut out = Path::new();
    // Where this segment begins along the path, and whether `out` continues at the pen.
    let (mut at, mut drawing) = (0.0, false);
    for (segment, begins, length) in segments {
        drawing &= !begins;
        let (a, b) = (from.max(at) - at, to.min(at + length) - at);
        at += length;
        if a < b {
            let part = segment.subsegment(segment.inv_arclen(a, ACCURACY)..segment.inv_arclen(b, ACCURACY));
            append(&mut out, part, drawing);
            drawing = true;
        }
    }
    out
}

/// The parts of `path` with `lo <= x <= hi`, each segment cut where its x crosses an edge. A
/// segment that leaves and re-enters the band keeps every piece inside.
fn x_trimmed(path: &Path, (lo, hi): (f64, f64)) -> Path {
    // Pixels, past an f32 ulp (6e-5 at x = 1000) so an edge the band and the path round apart still matches.
    const EPS: f64 = 1e-3;
    let mut out = Path::new();
    // Where `out`'s last piece ended, if the subpath is still open there.
    let mut end: Option<Point> = None;
    for (segment, begins) in segments(path) {
        if begins {
            end = None;
        }
        let c = segment.to_cubic();
        let (p0, p1, p2, p3) = (c.p0.x, c.p1.x, c.p2.x, c.p3.x);
        // x(t) - edge as a power basis polynomial.
        // A line's cubic form has float-noise c3/c2 that wrecks the solver; its polynomial is linear.
        let (c3, c2, c1) = match segment {
            PathSeg::Line(_) => (0.0, 0.0, p3 - p0),
            _ => (p3 - p0 + 3.0 * (p1 - p2), 3.0 * (p0 - 2.0 * p1 + p2), 3.0 * (p1 - p0)),
        };
        // Cut points: both ends and at most three crossings per edge.
        let mut cuts = [0.0; 8];
        let mut n = 1;
        for edge in [lo, hi] {
            for t in solve_cubic(p0 - edge, c1, c2, c3) {
                if t > 0.0 && t < 1.0 {
                    cuts[n] = t;
                    n += 1;
                }
            }
        }
        cuts[n] = 1.0;
        n += 1;
        cuts[..n].sort_by(f64::total_cmp);
        for w in cuts[..n].windows(2) {
            let mid = segment.eval((w[0] + w[1]) / 2.0).x;
            if w[0] < w[1] && mid >= lo - EPS && mid <= hi + EPS {
                let part = segment.subsegment(w[0]..w[1]);
                append(&mut out, part, end.is_some_and(|e| e.distance(part.start()) < EPS));
                end = Some(part.end());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::{paint_points, paint_with_gl};
    use super::*;
    #[test]
    fn paths_share_ancestor_mask_opacity_and_translation() {
        let src = r##"rect { width=64, height=64, opacity=0.5,
            mask={ gradient='linear', stops={{0,'#ffffff'},{0.5,'#ffffff'},{0.5,'#ffffff00'},{1,'#ffffff00'}} },
            children={path { width=48, height=48, translate={x=8,y=8}, fill='#ff0000', commands={
                {op='M',points={0,0}},{op='L',points={0,48}},{op='L',points={48,48}},{op='L',points={48,0}},{op='Z',points={}}
            } }}
        }"##;
        let px = paint_points(src, &[(16, 16), (16, 48), (2, 16)]).expect("headless EGL required");
        assert!((126..=129).contains(&px[0].3), "{px:?}");
        assert_eq!(px[1].3, 0);
        assert_eq!(px[2].3, 0);
    }
    #[test]
    fn path_chart_strokes_and_curves_render() {
        let src = r##"path { width=64,height=64,stroke='#00ff00',stroke_width=4,commands={
            {op='M',points={8,48}},{op='L',points={24,48}},
            {op='Q',points={32,48,32,32}}, {op='C',points={32,16,48,16,56,16}}
        }}"##;
        let px = paint_points(src, &[(16, 48), (48, 17), (8, 8)]).expect("headless EGL required");
        assert!(px[0].1 > 240 && px[0].3 > 240, "{px:?}");
        assert!(px[1].1 > 200 && px[1].3 > 200, "{px:?}");
        assert_eq!(px[2].3, 0);
    }
    #[test]
    fn paths_clip_centered_strokes_and_skip_empty_geometry() {
        let src = r##"path { width=32,height=32,stroke='#ffffff',stroke_width=8,commands={
            {op='M',points={0,0}},{op='L',points={0,32}}
        }}"##;
        let px = paint_points(src, &[(1, 16), (6, 16), (1, 40)]).expect("headless EGL required");
        assert_eq!(px[0].3, 255);
        assert_eq!(px[1].3, 0);
        assert_eq!(px[2].3, 0);
        for commands in ["{}", "{{op='M',points={16,16}}}", "{{op='M',points={16,16}},{op='L',points={16,16}}}"] {
            let src = format!(r##"path {{ width=32,height=32,fill='#ffffff',stroke='#ffffff',commands={commands} }}"##);
            let px = paint_points(&src, &[(16, 16), (15, 16)]).expect("headless EGL required");
            assert!(px.iter().all(|p| p.3 == 0), "{commands}: {px:?}");
        }
    }

    #[test]
    fn strokes_take_caps_and_joins() {
        // Width 8, from (16,48) up to a corner at (16,16): (12,12) is inside a miter's square
        // corner only, (14,50) inside a round or square cap, (12,51) inside a square cap only.
        for (style, expect) in [
            ("", [255, 0, 0]),
            ("stroke_cap='round',stroke_join='round',", [0, 255, 0]),
            ("stroke_cap='square',stroke_join='bevel',", [0, 255, 255]),
        ] {
            let src = format!(
                r##"path {{ width=64,height=64,stroke='#ffffff',stroke_width=8,{style}commands={{
                {{op='M',points={{16,48}}}},{{op='L',points={{16,16}}}},{{op='L',points={{48,16}}}}
            }} }}"##
            );
            let px = paint_points(&src, &[(12, 12), (14, 50), (12, 51)]).expect("headless EGL required");
            assert_eq!([px[0].3, px[1].3, px[2].3], expect, "{style}: {px:?}");
            // Tapered over its last 8 px, either way round, the far end at (48,16) narrows to a
            // point while the corner and the near end keep their join and cap.
            for (taper, path) in [
                ("taper_end=8", "{op='M',points={16,48}},{op='L',points={16,16}},{op='L',points={48,16}}"),
                ("taper_start=8", "{op='M',points={48,16}},{op='L',points={16,16}},{op='L',points={16,48}}"),
            ] {
                let src = format!(
                    r##"path {{ width=64,height=64,stroke='#ffffff',stroke_width=8,{style}{taper},commands={{{path}}} }}"##
                );
                let px = paint_points(&src, &[(12, 12), (14, 50), (12, 51), (36, 19), (47, 19)])
                    .expect("headless EGL required");
                assert_eq!(
                    px.iter().map(|p| p.3).collect::<Vec<_>>(),
                    [&expect[..], &[255, 0]].concat(),
                    "{style}{taper}"
                );
            }
        }
        // Bent tighter than its half width, the outline folds over itself and still fills solid.
        let src = r##"path { width=64,height=64,stroke='#ffffff',stroke_width=16,taper_end=1,commands={{op='A',points={32,32,4,0,300}}} }"##;
        let px = paint_points(src, &[(32, 32), (28, 32), (32, 36), (32, 28), (26, 26)]).expect("headless EGL required");
        assert!(px.iter().all(|p| p.3 == 255), "{px:?}");
    }

    #[test]
    fn a_tapered_stroke_paints_nothing_inside_a_gentle_curve_and_turns_back_whole() {
        // Radius 50, width 6: the inner edge is at 47. femtovg moves a fill's vertices along their
        // miters, so an outline doubling back through the centre would spike inward.
        let src = r##"return panel { id = "bar", width = 160, height = 160, child = path { width = 160, height = 160,
            stroke = '#ffffff', stroke_width = 6, taper_end = 20, commands = {{op='A',points={80,80,50,0,270}}} } }"##;
        let ring: Vec<_> = (0..160)
            .flat_map(|y| (0..160).map(move |x| (x, y)))
            .filter(|&(x, y)| (41.0..=46.0).contains(&(x as f64 + 0.5 - 80.0).hypot(y as f64 + 0.5 - 80.0)))
            .collect();
        let px = paint_with_gl(src, (160, 160), &ring).expect("headless EGL required");
        let lit: Vec<_> = ring.iter().zip(&px).filter(|(_, p)| p.3 > 0).collect();
        assert!(lit.is_empty(), "{lit:?}");

        // A line that turns straight back keeps both legs and the requested join past the turn.
        for (join, past) in [("round", 255), ("miter", 0), ("bevel", 0)] {
            let src = format!(
                r##"path {{ width=64,height=64,stroke='#ffffff',stroke_width=10,taper_start=1,stroke_join='{join}',commands={{
                {{op='M',points={{8,24}}}},{{op='L',points={{48,24}}}},{{op='L',points={{16,24}}}} }} }}"##
            );
            let px =
                paint_points(&src, &[(30, 24), (44, 24), (47, 20), (51, 24), (54, 24)]).expect("headless EGL required");
            assert_eq!(px.iter().map(|p| p.3).collect::<Vec<_>>(), [255, 255, 255, past, 0], "{join}: {px:?}");
        }

        // Off the axes a reversal's directions sum to rounding noise, not zero: still a reversal.
        for (taper, commands, inside) in [
            (
                "taper_start=1",
                "{op='M',points={10,10}},{op='L',points={50,50}},{op='L',points={20,20}}",
                [(8, 14), (11, 17), (12, 16), (15, 9)],
            ),
            (
                "taper_start=1",
                "{op='M',points={10,10}},{op='L',points={20,60}},{op='L',points={15,35}}",
                [(8, 20), (16, 20), (10, 28), (16, 28)],
            ),
            (
                "taper_end=30",
                "{op='M',points={10,10}},{op='L',points={20,60}},{op='L',points={15,35}}",
                [(8, 20), (16, 20), (10, 28), (16, 28)],
            ),
        ] {
            let src = format!(
                r##"path {{ width=64,height=64,stroke='#ffffff',stroke_width=10,{taper},commands={{{commands}}} }}"##
            );
            let px = paint_points(&src, &inside).expect("headless EGL required");
            assert!(px.iter().all(|p| p.3 == 255), "{commands} {taper}: {px:?}");
        }

        // Scaled 4x, the tip is still one device pixel wide, not four.
        let src = r##"path { width=16,height=16,scale=4,origin={x=0,y=0},stroke='#ffffff',stroke_width=2,taper_end=8,
            commands={{op='M',points={0,8}},{op='L',points={12,8}}} }"##;
        let px = paint_points(src, &[(8, 33), (40, 32), (46, 33)]).expect("headless EGL required");
        assert_eq!(px.iter().map(|p| p.3).collect::<Vec<_>>(), [255, 255, 0], "{px:?}");
    }

    #[test]
    fn a_taper_ramps_each_open_run_to_a_hairline_and_leaves_closed_ones_to_the_stroker() {
        let lines = |points: &[(f32, f32)]| {
            let mut path = Path::new();
            path.move_to(points[0].0, points[0].1);
            for &(x, y) in &points[1..] {
                path.line_to(x, y);
            }
            path
        };
        let mut paint = Paint::default();
        paint.set_line_width(10.0);
        let ring = |path: &Path, taper| {
            let (_, outline) = tapered(path, &paint, taper, 1.0);
            let ring: Vec<_> = outline
                .verbs()
                .filter_map(|verb| match verb {
                    Verb::MoveTo(x, y) | Verb::LineTo(x, y) => Some(Point::new(x.into(), y.into())),
                    _ => None,
                })
                .collect();
            ring
        };
        // One side out along +y, the other back along its mirror.
        let left = |path: &Path, taper| {
            let ring = ring(path, taper);
            let (left, right) = ring.split_at(ring.len() / 2);
            assert!(left.iter().rev().map(|p| (p.x, -p.y)).eq(right.iter().map(|p| (p.x, p.y))), "{ring:?}");
            left.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>()
        };
        let line = lines(&[(0.0, 0.0), (100.0, 0.0)]);
        // Full width from 20 px in to 30 px from the end.
        assert_eq!(left(&line, (20.0, 30.0)), [(0.0, 0.5), (20.0, 5.0), (70.0, 5.0), (100.0, 0.5)]);
        // A vertex on the ramp takes the width the ramp has there.
        let early = lines(&[(0.0, 0.0), (2.0, 0.0), (100.0, 0.0)]);
        let ramped = f64::from((0.5 + 4.5 * 2.0 / 20.0) as f32);
        assert_eq!(left(&early, (20.0, 30.0)), [(0.0, 0.5), (2.0, ramped), (20.0, 5.0), (70.0, 5.0), (100.0, 0.5)]);
        // A ramp shorter than the merge distance leaves the line full width, not tapering its length.
        assert_eq!(left(&line, (0.005, 0.0)), [(0.0, 5.0), (100.0, 5.0)]);
        // A repeated point changes nothing.
        let doubled = lines(&[(0.0, 0.0), (50.0, 0.0), (50.0, 0.0), (100.0, 0.0)]);
        let once = lines(&[(0.0, 0.0), (50.0, 0.0), (100.0, 0.0)]);
        assert_eq!(left(&doubled, (20.0, 30.0)), left(&once, (20.0, 30.0)));
        // Ramps that overlap peak thinner where they cross; ramps that just meet peak at full width.
        assert_eq!(left(&line, (60.0, 60.0)), [(0.0, 0.5), (50.0, 4.25), (100.0, 0.5)]);
        assert_eq!(left(&line, (50.0, 50.0)), [(0.0, 0.5), (50.0, 5.0), (100.0, 0.5)]);

        // A 45 degree turn mitres 5 / cos(22.5) px out; a near reversal passes the limit and bevels.
        let corner = Point::new(50.0, 0.0);
        let mitre = 5.0 / (std::f64::consts::PI / 8.0).cos();
        let bent = ring(&lines(&[(0.0, 0.0), (50.0, 0.0), (100.0, 50.0)]), (1.0, 1.0));
        assert!(bent.iter().any(|p| (p.distance(corner) - mitre).abs() < 1e-3), "{bent:?}");
        let hairpin = ring(&lines(&[(0.0, 0.0), (50.0, 0.0), (0.0, 2.0)]), (1.0, 1.0));
        assert!(hairpin.iter().all(|p| p.distance(corner) < 5.01 || p.x < 50.0), "{hairpin:?}");

        // Inside a 60 degree turn the offsets meet 5 * tan(30) = 2.9 px short of the corner, so
        // edges of at least twice that meet there; a shorter one on either side goes through it.
        let (corner, away) = (Point::new(100.0, 0.0), |d: f32| (100.0 + d / 2.0, d * 0.75_f32.sqrt()));
        for (path, through) in [
            (lines(&[(0.0, 0.0), (100.0, 0.0), away(8.0)]), false),
            (lines(&[(0.0, 0.0), (100.0, 0.0), away(6.0)]), true),
            (lines(&[away(6.0), (100.0, 0.0), (0.0, 0.0)]), true),
        ] {
            let bent = ring(&path, (1.0, 1.0));
            assert_eq!(bent.iter().any(|p| p.distance(corner) < 1e-3), through, "{bent:?}");
        }

        // Magnified past 64x, round caps stop gaining points.
        let mut round = paint.clone();
        round.set_line_cap(LineCap::Round);
        let (_, outline) = tapered(&line, &round, (20.0, 0.0), 1e-30);
        assert!(outline.verbs().count() < 100, "{}", outline.verbs().count());

        // Each subpath is its own run with its own ends.
        let mut two = lines(&[(0.0, 0.0), (40.0, 0.0)]);
        two.move_to(60.0, 0.0);
        two.line_to(100.0, 0.0);
        let (_, outline) = tapered(&two, &paint, (10.0, 10.0), 1.0);
        assert_eq!(outline.verbs().filter(|verb| matches!(verb, Verb::MoveTo(..))).count(), 2);

        let mut closed = lines(&[(0.0, 50.0), (10.0, 50.0), (10.0, 60.0)]);
        closed.close();
        let (closed, outline) = tapered(&closed, &paint, (20.0, 30.0), 1.0);
        assert_eq!(segments(&closed).iter().map(|s| s.1).collect::<Vec<_>>(), [true, false, false]);
        assert_eq!(outline.verbs().count(), 0);
    }

    #[test]
    fn trims_stroke_a_fraction_of_the_whole_path_closing_segments_included() {
        let square = "{op='M',points={16,16}},{op='L',points={48,16}},{op='L',points={48,48}},{op='L',points={16,48}},{op='Z',points={}}";
        // Top, right, bottom, then the closing left side: 32 of the 128 px each.
        for (trim, expect) in [
            ("trim_end=0.5", [255, 255, 255, 0, 0]),
            ("trim_end=0.125", [255, 0, 0, 0, 0]),
            ("trim_start=0.75", [0, 0, 0, 0, 255]),
            ("trim_start=0.5,trim_end=0.5", [0, 0, 0, 0, 0]),
        ] {
            let src = format!(
                r##"path {{ width=64,height=64,stroke='#ffffff',stroke_width=4,{trim},commands={{{square}}} }}"##
            );
            let px =
                paint_points(&src, &[(24, 16), (40, 16), (48, 32), (32, 48), (16, 32)]).expect("headless EGL required");
            assert_eq!(px.iter().map(|p| p.3).collect::<Vec<_>>(), expect, "{trim}: {px:?}");
        }
        // A taper measures the stroke as trimmed: the half square ends in a point at (48,48).
        let src = format!(
            r##"path {{ width=64,height=64,stroke='#ffffff',stroke_width=4,trim_end=0.5,taper_end=8,commands={{{square}}} }}"##
        );
        let px = paint_points(&src, &[(48, 40), (49, 46)]).expect("headless EGL required");
        assert_eq!((px[0].3, px[1].3), (255, 0), "{px:?}");
        // The length runs on across subpaths: three quarters is all of the first and half the second.
        let src = r##"path { width=64,height=64,stroke='#ffffff',stroke_width=4,trim_end=0.75,commands={
            {op='M',points={8,8}},{op='L',points={24,8}},{op='L',points={24,24}},{op='L',points={8,24}},{op='Z',points={}},
            {op='A',points={48,48,8,0,360}}
        } }"##;
        let px = paint_points(src, &[(8, 16), (53, 53), (48, 56), (40, 48), (48, 40)]).expect("headless EGL required");
        assert_eq!(px.iter().map(|p| p.3).collect::<Vec<_>>(), [255, 255, 255, 0, 0], "{px:?}");
    }

    #[test]
    fn an_x_trim_cuts_a_line_and_a_curve_where_x_crosses_the_band() {
        let mut line = Path::new();
        line.move_to(0.0, 10.0);
        line.line_to(100.0, 10.0);
        let cut = segments(&x_trimmed(&line, (20.0, 60.0)));
        assert_eq!(cut.len(), 1);
        assert_eq!((cut[0].0.start(), cut[0].0.end()), (Point::new(20.0, 10.0), Point::new(60.0, 10.0)));

        // x(t) = 100t, so the band edges fall at t = 0.2 and 0.6.
        let mut curve = Path::new();
        curve.move_to(0.0, 0.0);
        curve.bezier_to(100.0 / 3.0, 50.0, 200.0 / 3.0, -50.0, 100.0, 0.0);
        let cut = segments(&x_trimmed(&curve, (20.0, 60.0)));
        assert_eq!(cut.len(), 1);
        let (start, end) = (cut[0].0.start(), cut[0].0.end());
        assert!((start.x - 20.0).abs() < 1e-4 && (end.x - 60.0).abs() < 1e-4, "{start:?} {end:?}");
    }

    #[test]
    fn an_x_trim_keeps_a_vertical_segment_on_a_band_edge_the_path_and_band_round_apart() {
        let mut wall = Path::new();
        wall.move_to(1000.0003, 0.0);
        wall.line_to(1000.0003, 50.0);
        let cut = segments(&x_trimmed(&wall, (1000.0006, 1100.0)));
        assert_eq!(cut.len(), 1, "{cut:?}");
    }

    #[test]
    fn an_x_trim_keeps_every_piece_of_a_segment_that_crosses_the_band_repeatedly() {
        // x runs 0 -> 100 -> 0 -> 100: one cubic crossing a band at 40..60 six times.
        let mut curve = Path::new();
        curve.move_to(0.0, 0.0);
        curve.bezier_to(300.0, 10.0, -200.0, 20.0, 100.0, 30.0);
        let cut = segments(&x_trimmed(&curve, (40.0, 60.0)));
        assert!(cut.iter().filter(|(_, begins)| *begins).count() >= 3, "{}", cut.len());
        for (piece, _) in &cut {
            for x in [piece.start().x, piece.end().x, piece.eval(0.5).x] {
                assert!((39.999..=60.001).contains(&x), "outside the band: {x}");
            }
        }
    }

    #[test]
    fn an_x_trim_caps_its_cut_ends_and_shift_moves_the_geometry_not_the_band() {
        let line = "{op='M',points={0,32}},{op='L',points={64,32}}";
        let alpha = |style: &str, at: &[(usize, usize)]| {
            let src = format!(
                r##"path {{ width=64,height=64,stroke='#ffffff',stroke_width=8,trim_axis='x',{style},commands={{{line}}} }}"##
            );
            paint_points(&src, at).expect("headless EGL required").iter().map(|p| p.3).collect::<Vec<_>>()
        };
        // The band is x 16..48; a round cap reaches 4 px past each cut, a butt end stops at it.
        assert_eq!(
            alpha("trim_start=0.25,trim_end=0.75,stroke_cap='round'", &[(13, 32), (50, 32), (10, 32)])[..2],
            [255, 255]
        );
        assert_eq!(alpha("trim_start=0.25,trim_end=0.75", &[(13, 32), (50, 32), (10, 32)]), [0, 0, 0]);
        // Shifted 20 px right the line spans 20..84; the band 0..32 does not follow it.
        assert_eq!(alpha("trim_end=0.5,shift={x=20,y=0}", &[(10, 32), (25, 32), (40, 32)]), [0, 255, 0]);
    }

    #[test]
    fn paths_share_gradient_fills_shadows_and_the_blur_effect() {
        let shape = r##"commands={{op='M',points={16,8}},{op='L',points={16,24}},{op='L',points={32,24}},{op='L',points={32,8}},{op='Z',points={}}}"##;
        let src = format!(
            r##"path {{width=64,height=64,fill={{gradient='linear',stops={{{{0,'#ff0000'}},{{1,'#0000ff'}}}}}},shadows={{{{color='#00ff00',offset={{x=0,y=24}}}}}}, {shape}}}"##
        );
        let px = paint_points(&src, &[(24, 16), (24, 40), (8, 40)]).expect("headless EGL required");
        assert!(px[0].0 > 150 && px[0].2 > 30, "{px:?}");
        assert!(px[1].1 > 240 && px[1].3 > 240, "{px:?}");
        assert_eq!(px[2].3, 0);
        let src = format!(r##"path {{width=64,height=64,fill='#ffffff',effect={{blur=2}},{shape}}}"##);
        let px = paint_points(&src, &[(24, 16), (14, 16), (4, 16)]).expect("headless EGL required");
        assert!(px[0].3 > 240 && px[1].3 > 10 && px[1].3 < 150, "{px:?}");
        assert_eq!(px[2].3, 0);
    }

    #[test]
    fn arcs_sweep_clockwise_and_holes_ignore_winding() {
        let src = r##"path { width=64,height=64,stroke='#ffffff',stroke_width=4,commands={{op='A',points={32,32,20,0,90}}} }"##;
        let px = paint_points(src, &[(46, 46), (46, 18), (32, 32)]).expect("headless EGL required");
        assert_eq!((px[0].3, px[1].3, px[2].3), (255, 0, 0), "{px:?}");
        // Inside a subpath a line joins the arc's start; anticlockwise from 3 o'clock reaches 12.
        let src = r##"path { width=64,height=64,stroke='#ffffff',stroke_width=4,commands={
            {op='M',points={32,60}},{op='A',points={32,32,20,0,-90}},{op='L',points={4,12}}
        }}"##;
        let px = paint_points(src, &[(42, 46), (46, 18), (46, 46), (18, 12)]).expect("headless EGL required");
        assert_eq!((px[0].3, px[1].3, px[2].3, px[3].3), (255, 255, 0, 255), "{px:?}");
        // A wedge's corners stay sharp: no repeated point for anti-aliasing to draw out as a spike,
        // whether a line or a move reaches the arc's start. A sweep past 360 is the full circle.
        for commands in [
            "{op='M',points={32,32}},{op='A',points={32,32,20,0,90}},{op='Z',points={}}",
            "{op='M',points={32,32}},{op='L',points={52,32}},{op='A',points={32,32,20,0,90}},{op='Z',points={}}",
            "{op='M',points={32,32}},{op='L',points={52.001,32}},{op='A',points={32,32,20,0,90}},{op='Z',points={}}",
        ] {
            let src = format!(r##"path {{ width=64,height=64,fill='#ffffff',commands={{{commands}}} }}"##);
            let px = paint_points(&src, &[(46, 32), (46, 31), (50, 31), (53, 32)]).expect("headless EGL required");
            assert_eq!(px[0].3, 255, "{commands}: {px:?}");
            assert!(px[1..].iter().all(|p| p.3 == 0), "{commands}: {px:?}");
        }
        // A hole outside every solid subpath counts -1 and fills.
        let src =
            r##"path { width=64,height=64,fill='#ffffff',commands={{op='A',points={32,32,8,0,360},hole=true}} }"##;
        assert_eq!(paint_points(src, &[(32, 32)]).expect("headless EGL required")[0].3, 255);
        let src = r##"path { width=64,height=64,fill='#ffffff',commands={{op='A',points={32,32,20,45,720}}} }"##;
        let px = paint_points(src, &[(32, 14), (14, 32), (32, 50), (50, 32)]).expect("headless EGL required");
        assert!(px.iter().all(|p| p.3 == 255), "{px:?}");
        let src = r##"path { width=64,height=64,stroke='#ffffff',stroke_width=4,commands={{op='A',points={32,32,20,0,0}}} }"##;
        let px = paint_points(src, &[(52, 32), (32, 32)]).expect("headless EGL required");
        assert!(px.iter().all(|p| p.3 == 0), "{px:?}");

        let outer = "{op='M',points={8,8}},{op='L',points={56,8}},{op='L',points={56,56}},{op='L',points={8,56}},{op='Z',points={}}";
        for (inner, centre) in [
            (
                "{op='M',points={24,24},hole=true},{op='L',points={40,24}},{op='L',points={40,40}},{op='L',points={24,40}}",
                0,
            ),
            ("{op='M',points={24,24}},{op='L',points={24,40}},{op='L',points={40,40}},{op='L',points={40,24}}", 255),
            ("{op='A',points={32,32,8,0,-360},hole=true}", 0),
            // Two overlapping holes count -2 inside the solid square: nonzero, so it paints again.
            (
                "{op='A',points={32,32,8,0,360},hole=true},{op='Z',points={}},{op='A',points={32,32,8,0,360},hole=true}",
                255,
            ),
        ] {
            let src = format!(r##"path {{ width=64,height=64,fill='#ffffff',commands={{{outer},{inner}}} }}"##);
            let px = paint_points(&src, &[(32, 32), (16, 16), (2, 2)]).expect("headless EGL required");
            assert_eq!((px[0].3, px[1].3, px[2].3), (centre, 255, 0), "{inner}: {px:?}");
        }
    }

    /// A corner placed off its own frame can round to a curve with every point on the pen.
    #[test]
    fn a_zero_length_curve_is_left_out() {
        let mut bez = BezPath::new();
        bez.move_to((0.0, 0.0));
        bez.line_to((10.0, 0.0));
        bez.curve_to((10.0 + 1e-9, 0.0), (10.0, 0.0), (10.0, 0.0));
        bez.quad_to((10.0, 0.0), (10.0, 0.0));
        bez.curve_to((15.0, 0.0), (20.0, 5.0), (20.0, 10.0));
        bez.close_path();
        let path = femtovg_path(&bez, std::iter::empty());
        assert_eq!(path.verbs().filter(|verb| matches!(verb, Verb::BezierTo(..))).count(), 1);
    }
}

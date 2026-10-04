//! Vector leaves use the same fills, canvas state and subtree effects as box paint.
use super::shape::fill_paint;
use crate::layout::node::{PathOp, StrokeCap, StrokeJoin, VectorPath};
use crate::text::snap::LogicalRect;
use std::f32::consts::FRAC_PI_2;

use femtovg::{Canvas, LineCap, LineJoin, Path, Solidity, Verb, renderer::OpenGl};
use kurbo::{CubicBez, Line, ParamCurve, ParamCurveArclen, PathSeg, Point};

pub(super) fn paint(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    VectorPath { commands, fill, stroke, stroke_width, stroke_cap, stroke_join, trim }: &VectorPath,
) {
    if rect.is_empty() || commands.segments.is_empty() {
        return;
    }
    let mut path = Path::new();
    let (x, y) = (rect.x, rect.y);
    // Where the pen is and where its subpath began, so an arc's joining line is drawn only when it
    // moves: femtovg keeps a repeated point, and anti-aliasing draws the zero-length edge as a spike.
    let (mut pen, mut first) = ((0.0, 0.0), (0.0, 0.0));
    for (segment, p) in commands.iter() {
        match segment.op {
            PathOp::M => {
                pen = (x + p[0], y + p[1]);
                first = pen;
                path.move_to(pen.0, pen.1);
            }
            PathOp::L => {
                pen = (x + p[0], y + p[1]);
                path.line_to(pen.0, pen.1);
            }
            PathOp::Q => {
                pen = (x + p[2], y + p[3]);
                path.quad_to(x + p[0], y + p[1], pen.0, pen.1);
            }
            PathOp::C => {
                pen = (x + p[4], y + p[5]);
                path.bezier_to(x + p[0], y + p[1], x + p[2], y + p[3], pen.0, pen.1);
            }
            PathOp::A => {
                let (cx, cy, r) = (x + p[0], y + p[1], p[2]);
                let at = |a: f32| (cx + r * a.cos(), cy + r * a.sin());
                let start = p[3].rem_euclid(360.0).to_radians();
                let sweep = p[4].clamp(-360.0, 360.0).to_radians();
                let from = at(start);
                if segment.begins {
                    (pen, first) = (from, from);
                    path.move_to(pen.0, pen.1);
                } else if from != pen {
                    pen = from;
                    path.line_to(pen.0, pen.1);
                }
                // One cubic per quarter turn or less, handles 4/3·tan(step/4) radii long; none for no sweep.
                let segments = (sweep.abs() / FRAC_PI_2).ceil();
                let step = sweep / segments;
                let handle = r * 4.0 / 3.0 * (step / 4.0).tan();
                for i in 1..=segments as usize {
                    let (a0, a1) = (start + step * (i - 1) as f32, start + step * i as f32);
                    let end = at(a1);
                    path.bezier_to(
                        pen.0 - handle * a0.sin(),
                        pen.1 + handle * a0.cos(),
                        end.0 + handle * a1.sin(),
                        end.1 - handle * a1.cos(),
                        end.0,
                        end.1,
                    );
                    pen = end;
                }
            }
            PathOp::Z => {
                pen = first;
                path.close();
            }
        }
        if segment.begins {
            // Set on every subpath so winding never decides a hole.
            path.solidity(if segment.hole { Solidity::Hole } else { Solidity::Solid });
        }
    }
    if let Some(fill) = fill {
        canvas.fill_path(&path, &fill_paint(fill, rect));
    }
    if let Some(stroke) = stroke.as_ref().filter(|_| *stroke_width > 0.0) {
        let mut paint = fill_paint(stroke, rect);
        paint.set_line_width(*stroke_width);
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
        if *trim == (0.0, 1.0) {
            canvas.stroke_path(&path, &paint);
        } else if trim.0 < trim.1 {
            canvas.stroke_path(&trimmed(&path, *trim), &paint);
        }
    }
}

/// The part of `path` from `start` to `end` of its whole length, closing segments included. A
/// subpath kept whole ends where it began, which femtovg strokes as closed.
fn trimmed(path: &Path, (start, end): (f32, f32)) -> Path {
    const ACCURACY: f64 = 0.01;
    let point = |x: f32, y: f32| Point::new(x.into(), y.into());
    // Each segment, whether it begins its subpath, and its length.
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
        segments.push((segment, begins, segment.arclen(ACCURACY)));
        (pen, begins) = (segment.end(), false);
    }
    let total: f64 = segments.iter().map(|s| s.2).sum();
    let (from, to) = (f64::from(start) * total, f64::from(end) * total);
    let mut out = Path::new();
    // Where this segment begins along the path, and whether `out` continues at the pen.
    let (mut at, mut drawing) = (0.0, false);
    let p = |p: Point| (p.x as f32, p.y as f32);
    for (segment, begins, length) in segments {
        drawing &= !begins;
        let (a, b) = (from.max(at) - at, to.min(at + length) - at);
        at += length;
        if a < b {
            let part = segment.subsegment(segment.inv_arclen(a, ACCURACY)..segment.inv_arclen(b, ACCURACY));
            if !drawing {
                let (x, y) = p(part.start());
                out.move_to(x, y);
                drawing = true;
            }
            if let PathSeg::Cubic(c) = part {
                let ((ax, ay), (bx, by), (x, y)) = (p(c.p1), p(c.p2), p(c.p3));
                out.bezier_to(ax, ay, bx, by, x, y);
            } else {
                let (x, y) = p(part.end());
                out.line_to(x, y);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::paint_points;
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
        }
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
        // The length runs on across subpaths: three quarters is all of the first and half the second.
        let src = r##"path { width=64,height=64,stroke='#ffffff',stroke_width=4,trim_end=0.75,commands={
            {op='M',points={8,8}},{op='L',points={24,8}},{op='L',points={24,24}},{op='L',points={8,24}},{op='Z',points={}},
            {op='A',points={48,48,8,0,360}}
        } }"##;
        let px = paint_points(src, &[(8, 16), (53, 53), (48, 56), (40, 48), (48, 40)]).expect("headless EGL required");
        assert_eq!(px.iter().map(|p| p.3).collect::<Vec<_>>(), [255, 255, 255, 0, 0], "{px:?}");
    }

    #[test]
    fn paths_share_gradient_fills_shadows_and_content_blur() {
        let shape = r##"commands={{op='M',points={16,8}},{op='L',points={16,24}},{op='L',points={32,24}},{op='L',points={32,8}},{op='Z',points={}}}"##;
        let src = format!(
            r##"path {{width=64,height=64,fill={{gradient='linear',stops={{{{0,'#ff0000'}},{{1,'#0000ff'}}}}}},shadow_color='#00ff00',shadow_offset={{x=0,y=24}}, {shape}}}"##
        );
        let px = paint_points(&src, &[(24, 16), (24, 40), (8, 40)]).expect("headless EGL required");
        assert!(px[0].0 > 150 && px[0].2 > 30, "{px:?}");
        assert!(px[1].1 > 240 && px[1].3 > 240, "{px:?}");
        assert_eq!(px[2].3, 0);
        let src = format!(r##"path {{width=64,height=64,fill='#ffffff',content_blur=2,{shape}}}"##);
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
}

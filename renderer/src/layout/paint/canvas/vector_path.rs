//! Vector leaves use the same fills, canvas state and subtree effects as box paint.
use super::shape::fill_paint;
use crate::layout::node::{PathOp, VectorPath};
use crate::text::snap::LogicalRect;
use std::f32::consts::FRAC_PI_2;

use femtovg::{Canvas, Path, Solidity, renderer::OpenGl};

pub(super) fn paint(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    VectorPath { commands, fill, stroke, stroke_width }: &VectorPath,
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
        canvas.stroke_path(&path, &paint);
    }
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

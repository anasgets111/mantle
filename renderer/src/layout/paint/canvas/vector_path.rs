//! Vector leaves use the same fills, canvas state and subtree effects as box paint.
use super::shape::fill_paint;
use crate::layout::node::{PathOp, VectorPath};
use crate::text::snap::LogicalRect;
use femtovg::{Canvas, Path, renderer::OpenGl};

pub(super) fn paint(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    VectorPath { commands, fill, stroke, stroke_width }: &VectorPath,
) {
    if rect.is_empty() || commands.is_empty() {
        return;
    }
    let mut path = Path::new();
    let (x, y) = (rect.x, rect.y);
    for command in commands {
        let p = &command.points;
        match command.op {
            PathOp::M => path.move_to(x + p[0], y + p[1]),
            PathOp::L => path.line_to(x + p[0], y + p[1]),
            PathOp::Q => path.quad_to(x + p[0], y + p[1], x + p[2], y + p[3]),
            PathOp::C => path.bezier_to(x + p[0], y + p[1], x + p[2], y + p[3], x + p[4], y + p[5]),
            PathOp::Z => path.close(),
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
            mask={ gradient='Linear', stops={{0,'#ffffff'},{0.5,'#ffffff'},{0.5,'#ffffff00'},{1,'#ffffff00'}} },
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
            r##"path {{width=64,height=64,fill={{gradient='Linear',stops={{{{0,'#ff0000'}},{{1,'#0000ff'}}}}}},shadow_color='#00ff00',shadow_offset={{x=0,y=24}}, {shape}}}"##
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
}

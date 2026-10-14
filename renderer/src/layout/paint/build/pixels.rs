//! Buffer-pixel scaling, opacity fades and layer bounds the display-list build shares.

use super::*;

pub(super) fn scaled(r: LogicalRect, k: f32) -> LogicalRect {
    LogicalRect { x: r.x * k, y: r.y * k, width: r.width * k, height: r.height * k }
}

/// A transform in buffer pixels: the linear part is the same in either unit; only its translation
/// scales. A pure translation (a `move` mid-slide, or a `translate`) rounds to whole pixels, or its
/// glyph quads land between pixels and drivers sample them differently: frame 4 of
/// `cookbook/notifications-1.png` drew its text 1 px apart on NVIDIA and Intel.
pub(super) fn affine_in_pixels(matrix: node::Affine, scale: f32) -> node::Affine {
    let [a, b, c, d, x, y] = matrix;
    let (x, y) = (x * scale, y * scale);
    if [a, b, c, d] == [1.0, 0.0, 0.0, 1.0] { [a, b, c, d, x.round(), y.round()] } else { [a, b, c, d, x, y] }
}

/// `ring` around the box at `rect`, following its shape; `None` when its inner edge leaves no box.
pub(super) fn ring_cmd(
    rect: LogicalRect,
    radius: &Radii,
    ring: Ring,
    clip: PhysicalRect,
    scale: f32,
) -> Option<DrawCmd> {
    let Ring { width, color, offset } = ring;
    let by = offset + width;
    if rect.width + 2.0 * offset <= 0.0 || rect.height + 2.0 * offset <= 0.0 {
        return None;
    }
    let px = scaled(rect, scale);
    let (px, draw) = if let Some(outline) = &radius.2 {
        // The stroke's centre line is the contour moved out half a band; `inset` moves inward.
        let polygon = outline.polygon(LogicalRect { x: 0.0, y: 0.0, ..rect }, 0.1);
        if polygon.len() < 3 {
            return None;
        }
        let (mut segments, mut points) = (Vec::new(), Vec::new());
        for element in inset(&polygon, -f64::from(offset + width / 2.0)).elements() {
            let (op, at) = match *element {
                kurbo::PathEl::MoveTo(at) => (PathOp::M, Some(at)),
                kurbo::PathEl::LineTo(at) => (PathOp::L, Some(at)),
                _ => (PathOp::Z, None),
            };
            segments.push(Segment { op, hole: false, begins: op == PathOp::M });
            points.extend(at.into_iter().flat_map(|at| [at.x as f32, at.y as f32]));
        }
        let path = node::VectorPath {
            commands: std::rc::Rc::new(PathData { segments: segments.into(), points }),
            fill: None,
            stroke: Some(Fill::Color(color)),
            stroke_width: width,
            stroke_cap: node::StrokeCap::Butt,
            stroke_join: node::StrokeJoin::Round,
            trim: (0.0, 1.0),
            trim_axis: node::TrimAxis::Length,
            shift: (0.0, 0.0),
        };
        (px, Draw::Path(path))
    } else {
        // CSS grows each corner by the offset; a square one stays square, and a scoop is left alone.
        let fitted = radius.fit(rect.width, rect.height);
        let corners = fitted.0.map(|r| if r > 0.0 { (r + by).max(0.0) } else { r });
        let edges = BorderPaint::Edges(BorderColor {
            top: Some(color),
            right: Some(color),
            bottom: Some(color),
            left: Some(color),
        });
        let widths = EdgeInsets { top: width, right: width, bottom: width, left: width };
        let draw = Draw::Box { background: Vec::new(), radius: Radii(corners, fitted.1, None), border: edges, widths };
        (grow(px, by * scale), draw)
    };
    Some(DrawCmd { rect: px, clip, draw: in_buffer_pixels(draw, scale) })
}

/// `draw`'s own geometry in buffer pixels; a group's subtree was converted as it was built.
/// Exhaustive, so a new draw has to say what it scales. Image and icon boxes are cache keys
/// [`draw_for`] rounds itself, and text shapes at its logical size for `canvas::execute` to rasterize at
/// the buffer scale.
pub(super) fn in_buffer_pixels(draw: Draw, scale: f32) -> Draw {
    if scale == 1.0 {
        return draw;
    }
    let shadow = |shadow: node::Shadow| node::Shadow {
        blur: shadow.blur * scale,
        offset: (shadow.offset.0 * scale, shadow.offset.1 * scale),
        spread: shadow.spread * scale,
        ..shadow
    };
    let scale_shader =
        |shader: Option<LayerShader>| shader.map(|shader| LayerShader { radius: shader.radius * scale, ..shader });
    match draw {
        Draw::Box { background, radius, border, widths } => {
            Draw::Box { background, radius: radius * scale, border, widths: widths.scaled(scale) }
        }
        Draw::Path(mut path) => {
            std::rc::Rc::make_mut(&mut path.commands).for_each_pixel(|point| *point *= scale);
            path.stroke_width *= scale;
            path.shift = (path.shift.0 * scale, path.shift.1 * scale);
            Draw::Path(path)
        }
        Draw::Clipped { radius, mask, commands } => Draw::Clipped { radius: radius * scale, mask, commands },
        Draw::NodeMask { invert, radius, split, commands } => {
            Draw::NodeMask { invert, radius: radius * scale, split, commands }
        }
        Draw::Shadow { shadow: cast, radius, knockout } => {
            Draw::Shadow { shadow: shadow(cast), radius: radius * scale, knockout }
        }
        Draw::InsetShadow { shadow: cast, radius, widths } => {
            Draw::InsetShadow { shadow: shadow(cast), radius: radius * scale, widths: widths.scaled(scale) }
        }
        Draw::Layer { effect, shader, silhouette, commands } => Draw::Layer {
            effect: Box::new(node::Effect {
                shadows: effect.shadows.into_iter().map(shadow).collect(),
                blur: effect.blur * scale,
                ..*effect
            }),
            shader: scale_shader(shader),
            silhouette,
            commands,
        },
        Draw::Backdrop { sigma, tone, radius, alpha, shader, mask, desktop } => Draw::Backdrop {
            sigma: sigma * scale,
            tone,
            radius: radius * scale,
            alpha,
            shader: scale_shader(shader),
            mask,
            desktop,
        },
        // `build_node` makes a `Transformed` in buffer pixels already, through `affine_in_pixels`.
        draw @ (Draw::Text { .. }
        | Draw::Transformed { .. }
        | Draw::Icon { .. }
        | Draw::Image { .. }
        | Draw::Capture { .. }
        | Draw::Shader { .. }) => draw,
    }
}

/// Splits a box into fill and border so [`build_node`] can mask children between them. Other draws
/// stay whole; [`rounded_clip`] only returns for boxes.
pub(super) fn split_fill_and_border(draw: Option<Draw>) -> (Option<Draw>, Option<Draw>) {
    let Some(Draw::Box { background, radius, border, widths }) = draw else {
        return (draw, None);
    };
    let fill = (!background.is_empty()).then(|| Draw::Box {
        background,
        radius: radius.clone(),
        border: BorderPaint::default(),
        widths: EdgeInsets::default(),
    });
    let border =
        (widths != EdgeInsets::default()).then_some(Draw::Box { background: Vec::new(), radius, border, widths });
    (fill, border)
}

/// Multiplies `opacity` into an existing alpha: half-transparent inside a half-faded panel is a
/// quarter.
pub(super) fn fade(color: Rgba, opacity: f32) -> Rgba {
    Rgba { a: color.a * opacity, ..color }
}

pub(super) fn fade_fill(fill: &Fill, opacity: f32) -> Fill {
    match fill {
        Fill::Color(color) => Fill::Color(fade(*color, opacity)),
        Fill::Gradient(gradient) => Fill::Gradient(fade_gradient(gradient, opacity)),
    }
}

pub(super) fn fade_gradient(gradient: &node::Gradient, opacity: f32) -> node::Gradient {
    node::Gradient {
        stops: gradient.stops.iter().map(|(at, color)| (*at, fade(*color, opacity))).collect(),
        ..*gradient
    }
}

/// Multiplies border-edge alpha; absent edges stay absent.
pub(super) fn fade_border(border: &BorderPaint, opacity: f32) -> BorderPaint {
    match border {
        BorderPaint::Edges(colors) => BorderPaint::Edges(BorderColor {
            top: colors.top.map(|c| fade(c, opacity)),
            right: colors.right.map(|c| fade(c, opacity)),
            bottom: colors.bottom.map(|c| fade(c, opacity)),
            left: colors.left.map(|c| fade(c, opacity)),
        }),
        BorderPaint::Gradient(gradient) => BorderPaint::Gradient(fade_gradient(gradient, opacity)),
    }
}

/// One logical edge in physical pixels, rounded and floored at 1. `ImageCache` keys on this
/// integer.
pub(super) fn physical_edge(logical: f32, scale: f32) -> u32 {
    let physical = logical * scale;
    if !physical.is_finite() || physical <= 1.0 {
        return 1;
    }
    physical.round() as u32
}

/// `image.source_blur` in physical pixels. Floored at 0, not [`physical_edge`]'s 1: a box always
/// covers some area, but a blur may genuinely be off. the `source_blur` field already rejects
/// negative, infinite and NaN logical values, and `scale` is always positive, so unlike
/// `physical_edge` there is no out-of-range input here to clamp.
pub(super) fn physical_blur(logical: f32, scale: f32) -> u32 {
    (logical * scale).round() as u32
}

/// `command` composited by `blend`: a normal one as it is, any other one as a layer of its own,
/// whose offscreen blends onto what is under it.
pub(super) fn blended(blend: Blend, command: DrawCmd) -> DrawCmd {
    if blend == Blend::Normal {
        return command;
    }
    let (rect, clip) = (command.rect, command.clip);
    let effect = node::Effect { blend, ..node::Effect::default() };
    DrawCmd {
        rect,
        clip,
        draw: Draw::Layer { effect: Box::new(effect), shader: None, silhouette: false, commands: vec![command] },
    }
}

pub(super) fn push_fill(out: &mut Vec<DrawCmd>, fill: DrawCmd, layers: &[(Fill, Blend)]) {
    let DrawCmd { rect, clip, draw: Draw::Box { background, radius, border, widths } } = fill else {
        return out.push(fill);
    };
    if layers.iter().all(|(_, blend)| *blend == Blend::Normal) {
        return out.push(DrawCmd { rect, clip, draw: Draw::Box { background, radius, border, widths } });
    }
    let boxed = |background| DrawCmd {
        rect,
        clip,
        draw: Draw::Box { background, radius: radius.clone(), border: border.clone(), widths },
    };
    let mut run = Vec::new();
    for (fill, (_, blend)) in background.into_iter().zip(layers).rev() {
        if *blend == Blend::Normal {
            run.insert(0, fill);
            continue;
        }
        if !run.is_empty() {
            out.push(boxed(std::mem::take(&mut run)));
        }
        out.push(blended(*blend, boxed(vec![fill])));
    }
    if !run.is_empty() {
        out.push(boxed(run));
    }
}

/// A layer's offscreen: the box padded for the furthest blur or shader, united with where it lands as the shadow.
pub(super) fn layer_bounds(rect: LogicalRect, effect: &node::Effect, scale: f32) -> PhysicalRect {
    let shadow_reach = effect.shadows.iter().fold(0.0_f32, |most, shadow| most.max(reach(shadow.blur / 2.0)));
    let padded = grow(rect, shadow_reach.max(reach(effect.blur)).max(shader_padding(&effect.shader)));
    let own = snap_to_physical(padded, scale);
    effect
        .shadows
        .iter()
        .fold(own, |own, shadow| own.union(snap_to_physical(shadow_rect(rect, padded, *shadow), scale)))
}

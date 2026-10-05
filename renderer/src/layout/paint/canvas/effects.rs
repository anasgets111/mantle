//! Shadows, content blur, backdrop blur and blends: the draws that read or filter pixels through
//! pooled targets.

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, CompositeOperation, ImageId, Paint, Path, RenderTarget, Solidity};
use glow::HasContext;

use crate::layout::image_shader;
use crate::layout::node::{self, EdgeInsets, Radii, Rgba};
use crate::text::atlas::TextPainter;
use crate::text::snap::{LogicalRect, PhysicalRect};

use super::super::{LayerShader, UNCLIPPED, any_draw_matches, grow, reads_under, shadow_rect, transformed, volatile};
use super::shape::{box_path, polygon_into};
use super::{Draw, DrawCmd, Frame, Shaders, Walk, fill_image, flush, offscreen, scratch};
use crate::layout::node::outline::inset;

/// A shadow's gradient: its feather and its colour solid and at zero alpha. A ramp across 3 sigma
/// is within 14/255 of the layer path's Gaussian; matching its slope instead, 22. The feather is
/// floored at NanoVG's 1, since the gradient divides by it.
fn shadow_gradient(shadow: node::Shadow) -> (f32, Color, Color) {
    let Rgba { r, g, b, .. } = shadow.color;
    ((1.5 * shadow.blur).max(1.0), Color::from(shadow.color), Color::rgbaf(r, g, b, 0.0))
}

/// A round box's shadow (ADR-0254), cut out under the box when `knockout` (ADR-0260): femtovg's
/// box gradient fades from the colour to nothing across 3 sigma centred on the spread box's edge.
pub(super) fn paint_shadow(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    shadow: node::Shadow,
    own: &Radii,
    knockout: bool,
) {
    let LogicalRect { x, y, width, height } = shadow_rect(rect, rect, shadow);
    let (feather, color, clear) = shadow_gradient(shadow);
    // A signed distance past half the box is positive everywhere, so the gradient would paint nothing.
    // The gradient has one radius, so unequal corners cast the shadow of their mean.
    let radius = spread_radius(own.0.iter().sum::<f32>() / 4.0, shadow.spread).min(width.min(height) / 2.0);
    let paint = Paint::box_gradient(x, y, width, height, radius, feather, color, clear);
    let reach = grow(LogicalRect { x, y, width, height }, feather / 2.0);
    let path = if knockout { knocked_out(rect, own, reach) } else { box_path(reach, &Radii::default()) };
    canvas.fill_path(&path, &paint);
}

/// An `inset` shadow (ADR-0331): the padding box, filled with `shadow.color` everywhere outside a
/// hole that is the box moved by `offset` and shrunk by `spread`, feathered like [`paint_shadow`].
/// The fill's path is the padding outline, so the shape clips it and smoothed corners follow.
/// ponytail: the hole is one mean radius, so unequal corners and a scoop shade as a round hole.
pub(super) fn paint_inset_shadow(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    shadow: node::Shadow,
    own: &Radii,
    widths: EdgeInsets,
) {
    let pad = LogicalRect {
        x: rect.x + widths.left,
        y: rect.y + widths.top,
        width: rect.width - widths.left - widths.right,
        height: rect.height - widths.top - widths.bottom,
    };
    if pad.width <= 0.0 || pad.height <= 0.0 {
        return;
    }
    // A corner's inner radius is its radius less the wider border it meets; sign keeps a scoop a scoop.
    let [tl, tr, br, bl] = own.0;
    let shrink = |r: f32, a: f32, b: f32| r.signum() * (r.abs() - a.max(b)).max(0.0);
    let inner = Radii(
        [
            shrink(tl, widths.top, widths.left),
            shrink(tr, widths.top, widths.right),
            shrink(br, widths.bottom, widths.right),
            shrink(bl, widths.bottom, widths.left),
        ],
        own.1,
        None,
    );
    let hole = LogicalRect {
        x: pad.x + shadow.offset.0 + shadow.spread,
        y: pad.y + shadow.offset.1 + shadow.spread,
        width: (pad.width - 2.0 * shadow.spread).max(0.0),
        height: (pad.height - 2.0 * shadow.spread).max(0.0),
    };
    let mean = inner.0.iter().map(|r| r.abs()).sum::<f32>() / 4.0;
    let radius = spread_radius(mean, -shadow.spread).min(hole.width.min(hole.height) / 2.0);
    let (feather, color, clear) = shadow_gradient(shadow);
    let paint = Paint::box_gradient(hole.x, hole.y, hole.width, hole.height, radius, feather, clear, color);
    canvas.fill_path(&box_path(pad, &inner), &paint);
}

/// An `inset` shadow inside an `outline`, which no box gradient can draw: everything outside the
/// contour moved by `offset` and in by `spread`, cast like a layer's shadow and cut to the contour
/// moved in by the border. ponytail: one offscreen and blur per shadow per paint, uncached;
/// upgrade: keep the cast as `draw_layer` keeps a layer.
pub(super) fn paint_outline_inset(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    command: &DrawCmd,
    outline: &node::Outline,
    target: RenderTarget,
) {
    let Draw::InsetShadow { shadow, widths, .. } = &command.draw else { return };
    let (rect, clip) = (command.rect, command.clip);
    let polygon = outline.polygon(rect, 0.25);
    let (dx, dy) = (f64::from(shadow.offset.0), f64::from(shadow.offset.1));
    let hole: Vec<_> =
        inset(&polygon, f64::from(widths.top + shadow.spread)).into_iter().map(|p| p + (dx, dy)).collect();
    // Room for the blur to read solid shadow from past the clip's edges.
    let room = (1.5 * shadow.blur + shadow.offset.0.abs().max(shadow.offset.1.abs())).ceil() as i32 + 2;
    let (x0, y0) = (clip.x0 - room, clip.y0 - room);
    let size = ((clip.x1 - clip.x0 + 2 * room) as usize, (clip.y1 - clip.y0 + 2 * room) as usize);
    let Some(content) = scratch(painter, walk, size) else { return };
    let canvas = painter.canvas_mut();
    canvas.save();
    canvas.reset_transform();
    canvas.reset_scissor();
    canvas.set_render_target(RenderTarget::Image(content));
    canvas.clear_rect(0, 0, size.0 as u32, size.1 as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
    canvas.translate(-x0 as f32, -y0 as f32);
    let mut outside = Path::new();
    outside.rect(x0 as f32, y0 as f32, size.0 as f32, size.1 as f32);
    polygon_into(&mut outside, &hole, Solidity::Hole);
    canvas.fill_path(&outside, &Paint::color(Color::black()));
    canvas.restore();
    canvas.set_render_target(target);
    let Some(cast) = cast_shadow(painter, walk, content, size, *shadow, target) else { return };
    let mut pad = Path::new();
    polygon_into(&mut pad, &inset(&polygon, f64::from(widths.top)), Solidity::Solid);
    let (w, h) = (size.0 as f32, size.1 as f32);
    painter.canvas_mut().fill_path(&pad, &Paint::image(cast, x0 as f32, y0 as f32, w, h, 0.0, 1.0));
}

/// CSS's corner radius of a shadow spread from a box's: a square corner stays square.
fn spread_radius(radius: f32, spread: f32) -> f32 {
    let k = if radius < spread { 1.0 + (radius / spread - 1.0).powi(3) } else { 1.0 };
    (radius + spread * k).max(0.0)
}

/// `outside` with the box cut out, which is where a box shadow draws (ADR-0260). femtovg fringes the
/// outer rect's edge over the fill, so it sits 2px clear of both, where the ramp is already zero.
fn knocked_out(rect: LogicalRect, radius: &Radii, outside: LogicalRect) -> Path {
    let (x0, y0) = (outside.x.min(rect.x) - 2.0, outside.y.min(rect.y) - 2.0);
    let x1 = (outside.x + outside.width).max(rect.x + rect.width) + 2.0;
    let y1 = (outside.y + outside.height).max(rect.y + rect.height) + 2.0;
    let mut path = box_path(rect, radius);
    path.solidity(Solidity::Hole);
    path.rect(x0, y0, x1 - x0, y1 - y0);
    path.solidity(Solidity::Solid);
    path
}

/// A subtree under its own `effect.shader`, shadows, `effect.blur` and colour filters (ADR-0254,
/// ADR-0334, ADR-0336), each a pass into pooled targets. An unchanged layer composites what it
/// last finished (ADR-0258).
pub(super) fn draw_layer(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    command: &DrawCmd,
    target: RenderTarget,
    frame: Frame,
) {
    let Draw::Layer { effect, shader, silhouette, commands } = &command.draw else { return };
    let (node::Effect { shadows, blur, tone, .. }, silhouette) = (&**effect, *silhouette);
    let (rect, clip) = (command.rect, command.clip);
    let size = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    let area = LogicalRect { x: clip.x0 as f32, y: clip.y0 as f32, width: size.0 as f32, height: size.1 as f32 };
    let (casts, content) = match painter.layer(walk.surface, command) {
        Some(kept) => kept,
        None => {
            // Whole: the blur and the shadow read past the repaint's edge.
            let Some(content) =
                offscreen(painter, walk, rect, clip, None, None, commands, target, frame, UNCLIPPED, true)
            else {
                return;
            };
            // The shader's output is what the shadows, blur and colour filters see. A failed build
            // leaves the node as painted, and that frame uncached so only a revision retries.
            let shaded = shader
                .as_ref()
                .and_then(|shader| shaded(painter, walk, (content, content), size, command.rect, area, shader));
            let content = shaded.unwrap_or(content);
            let mut casts: Vec<ImageId> =
                shadows.iter().map_while(|shadow| cast_shadow(painter, walk, content, size, *shadow, target)).collect();
            let sharp = *blur < MIN_SIGMA && tone.is_identity();
            let blurred = blurred(painter, walk, content, size, (*blur, *tone));
            // A filter a full pool refused leaves this frame unfiltered, not every frame after.
            let filtered =
                casts.len() == shadows.len() && sharp == blurred.is_none() && shader.is_none() == shaded.is_none();
            // All or none: the layers kept would be the top ones, the bottom ones missing. The partial
            // casts stay in `walk.scratch` and recycle with the frame.
            if casts.len() != shadows.len() {
                casts.clear();
            }
            let content = blurred.unwrap_or(content);
            // A glass reads what is under the layer's box, which this command does not name.
            if filtered && !any_draw_matches(commands, |draw| volatile(draw) || reads_under(draw)) {
                walk.scratch.retain(|(id, _)| !casts.contains(id) && *id != content);
                painter.keep_layer(walk.surface, command, (casts.clone(), content), size);
            }
            (casts, content)
        }
    };
    // CSS paints the first shadow on top, so the last draws first.
    // ponytail: content shadows blend apart from the content; upgrade: composite both, blend once.
    for (shadow, &cast) in shadows.iter().zip(&casts).rev() {
        let at = shadow_rect(rect, area, *shadow);
        let blend = if shadow.blend == node::Blend::Normal { effect.blend } else { shadow.blend };
        match commands.as_slice() {
            // The hole's part outside `at` would take the cast's clamped edge.
            [DrawCmd { draw: Draw::Box { radius, .. }, .. }] if silhouette => {
                painter.canvas_mut().save();
                painter.canvas_mut().intersect_scissor(at.x, at.y, at.width, at.height);
                composite(painter, walk, cast, at, &knocked_out(rect, radius, at), clip, blend);
                painter.canvas_mut().restore();
            }
            _ => composite(painter, walk, cast, at, &box_path(at, &Radii::default()), clip, blend),
        }
    }
    if !silhouette {
        composite(painter, walk, content, area, &box_path(area, &Radii::default()), clip, effect.blend);
    }
}

/// `image` laid over `at` and cut to `path`, onto the target by `blend`. A normal one is drawn
/// over; any other copies what the target holds under `area`, blends `image` with it in one pass
/// and puts the result in place of the copy. Drawn over without a GL context or a target.
fn composite(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    image: ImageId,
    at: LogicalRect,
    path: &Path,
    area: PhysicalRect,
    blend: node::Blend,
) {
    let blended = if blend == node::Blend::Normal {
        None
    } else {
        read_target(painter, walk, area).and_then(|read| {
            let target = scratch(painter, walk, read.size)?;
            // A target texel `(s, t)` to the image's, both with rows bottom up as GL keeps them.
            let to_image = |s: f32, t: f32| {
                let (x, y) = read.point(s, 1.0 - t);
                [(x - at.x) / at.width, 1.0 - (y - at.y) / at.height]
            };
            let ([x, y], [ux, uy], [vx, vy]) = (to_image(0.0, 0.0), to_image(1.0, 0.0), to_image(0.0, 1.0));
            let map = [ux - x, uy - y, 0.0, vx - x, vy - y, 0.0, x, y, 1.0];
            let pass = image_shader::BlendPass { backdrop: read.copy, source: image, target, map, mode: blend };
            let Shaders { gl, stage } = walk.shaders.as_mut()?;
            // SAFETY: `Shaders` is built only with `gl` current on this thread and shared with the canvas.
            unsafe { stage.blend(gl, painter.canvas_mut(), &pass) }.then(|| read.paint(target, 1.0))
        })
    };
    match blended {
        Some(paint) => replace(painter.canvas_mut(), path, &paint, 1.0),
        None => painter.canvas_mut().fill_path(path, &Paint::image(image, at.x, at.y, at.width, at.height, 0.0, 1.0)),
    }
}

/// `content` and its `blurred` copy through `effect.shader` into a scratch the size of `area`, the
/// box `rect` seen through it; `None` when the program or a target is missing.
fn shaded(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    (content, blurred): (ImageId, ImageId),
    size: (usize, usize),
    rect: LogicalRect,
    area: LogicalRect,
    shader: &LayerShader,
) -> Option<ImageId> {
    // A box with no area has no `v_uv` to place.
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return None;
    }
    let target = scratch(painter, walk, size)?;
    let scale = walk.scale;
    let Shaders { gl, stage } = walk.shaders.as_mut()?;
    let images = super::sampler_images(painter.canvas_mut(), walk.images, &shader.images);
    let run = image_shader::ContentRun {
        input: content,
        blurred,
        target,
        rect: [
            (area.x - rect.x) / rect.width,
            (area.y - rect.y) / rect.height,
            area.width / rect.width,
            area.height / rect.height,
        ],
        logical_size: (rect.width / scale, rect.height / scale),
        radii: (shader.radius.clone() * (1.0 / scale)).fit(rect.width / scale, rect.height / scale),
        progress: shader.progress,
        params: &shader.params,
        images: &images,
    };
    // SAFETY: `Shaders` is built only with `gl` current on this thread and shared with the canvas.
    unsafe { stage.content(gl, painter.canvas_mut(), &shader.source, &run) }.then_some(target)
}

/// `image_shader`'s blur divides by `u_sigma`.
const MIN_SIGMA: f32 = 0.01;

/// `source` blurred by `sigma`, then recoloured by `tone` in the same last pass; `None` when both
/// are off, or the filter could not run.
fn blurred(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    source: ImageId,
    size: (usize, usize),
    (sigma, tone): (f32, node::Tone),
) -> Option<ImageId> {
    if sigma < MIN_SIGMA && tone.is_identity() {
        return None;
    }
    let image = scratch(painter, walk, size)?;
    // A power of two keeps the kernel within 4 to 8 texels at any sigma.
    let factor = 1 << (sigma / 4.0).log2().max(0.0) as u32;
    gaussian(painter, walk, source, image, size, (sigma, tone), factor).then_some(image)
}

/// Blurs `source` into `target`, both `size`, at `1 / factor` of that size: halved, blurred
/// across and down, and stretched back (ADR-0262). `false` without a GL context or a target.
fn gaussian(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    source: ImageId,
    target: ImageId,
    size: (usize, usize),
    (sigma, tone): (f32, node::Tone),
    factor: usize,
) -> bool {
    use image_shader::BlurPass;
    if walk.shaders.is_none() {
        return false;
    }
    // Each halving reads whole 2x2 blocks, the last one past an odd edge, so a low texel spans
    // exactly `factor` pixels.
    let (mut passes, mut from, mut at) = (Vec::new(), (source, size), 1);
    while at < factor {
        at *= 2;
        let half = (from.1.0.div_ceil(2), from.1.1.div_ceil(2));
        let Some(image) = scratch(painter, walk, half) else { return false };
        let extent = [(2 * half.0) as f32 / from.1.0 as f32, (2 * half.1) as f32 / from.1.1 as f32];
        passes.push(BlurPass {
            source: from.0,
            target: image,
            extent,
            axis: [0.0; 2],
            sigma: 0.0,
            tone: node::Tone::default(),
        });
        from = (image, half);
    }
    let (from, low) = from;
    let identity = node::Tone::default();
    if sigma < MIN_SIGMA {
        // A colour filter alone is one bilinear read.
        passes.push(BlurPass { source, target, extent: [1.0; 2], axis: [0.0; 2], sigma: 0.0, tone });
    } else {
        let Some(across) = scratch(painter, walk, low) else { return false };
        // The halvings add a `factor`-wide box's variance and the stretch a tent's.
        let f = factor as f32;
        let spread = if factor == 1 { 0.0 } else { (3.0 * f * f - 1.0) / 12.0 };
        let sigma = (sigma * sigma - spread).max(0.0).sqrt() / f;
        let down = if factor == 1 { target } else { from };
        // The colour filter rides the pass that writes `target`.
        let down_tone = if factor == 1 { tone } else { identity };
        passes.push(BlurPass {
            source: from,
            target: across,
            extent: [1.0; 2],
            axis: [1.0, 0.0],
            sigma,
            tone: identity,
        });
        passes.push(BlurPass {
            source: across,
            target: down,
            extent: [1.0; 2],
            axis: [0.0, 1.0],
            sigma,
            tone: down_tone,
        });
        if factor > 1 {
            let extent = [size.0 as f32 / (factor * low.0) as f32, size.1 as f32 / (factor * low.1) as f32];
            passes.push(BlurPass { source: down, target, extent, axis: [0.0; 2], sigma: 0.0, tone });
        }
    }
    let Some(Shaders { gl, stage }) = walk.shaders.as_mut() else { return false };
    // SAFETY: `Shaders` is built only with `gl` current on this thread and shared with the canvas.
    unsafe { stage.blur(gl, painter.canvas_mut(), &passes) }
}

/// `content` blurred, then recoloured by `SourceIn` keeping each pixel's alpha (ADR-0254).
fn cast_shadow(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    content: ImageId,
    size: (usize, usize),
    shadow: node::Shadow,
    target: RenderTarget,
) -> Option<ImageId> {
    let sigma = shadow.blur / 2.0;
    let blurred = blurred(painter, walk, content, size, (sigma, node::Tone::default()));
    let cast = blurred.or_else(|| scratch(painter, walk, size))?;
    let (width, height) = (size.0 as f32, size.1 as f32);
    let mut whole = Path::new();
    whole.rect(0.0, 0.0, width, height);
    let canvas = painter.canvas_mut();
    canvas.save();
    canvas.reset_transform();
    canvas.reset_scissor();
    canvas.set_render_target(RenderTarget::Image(cast));
    if blurred.is_none() {
        canvas.clear_rect(0, 0, size.0 as u32, size.1 as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
        fill_image(canvas, content, LogicalRect { x: 0.0, y: 0.0, width, height }, 1.0);
    }
    canvas.global_composite_operation(CompositeOperation::SourceIn);
    canvas.fill_path(&whole, &Paint::color(shadow.color.into()));
    canvas.restore();
    canvas.set_render_target(target);
    Some(cast)
}

/// Blurs and recolours (sigma and tone) what the current target holds under the command's clip,
/// the 3 sigma the blur reads, into the box (ADR-0256). A `shader` reads the copy and the filtered
/// copy instead and its output replaces the whole area, so it must return the input to leave a pixel.
pub(super) fn draw_backdrop(painter: &mut TextPainter, walk: &mut Walk<'_, '_>, command: &DrawCmd) {
    let Draw::Backdrop { sigma, tone, radius, alpha, shader } = &command.draw else { return };
    let (rect, clip, alpha) = (command.rect, command.clip, *alpha);
    let Some(read) = read_target(painter, walk, clip) else { return };
    let frost = (*sigma > 0.0 || !tone.is_identity())
        .then(|| blurred(painter, walk, read.copy, read.size, (*sigma, *tone)).unwrap_or(read.copy));
    let area = LogicalRect {
        x: clip.x0 as f32,
        y: clip.y0 as f32,
        width: (clip.x1 - clip.x0) as f32,
        height: (clip.y1 - clip.y0) as f32,
    };
    let inputs = (read.copy, frost.unwrap_or(read.copy));
    // A program that fails to build leaves the frost.
    let out = shader.as_ref().and_then(|shader| shaded(painter, walk, inputs, read.size, rect, read.at, shader));
    match (out, frost) {
        (Some(out), _) => {
            replace(painter.canvas_mut(), &box_path(area, &Radii::default()), &read.paint(out, alpha), alpha)
        }
        (None, Some(frost)) => replace(painter.canvas_mut(), &box_path(rect, radius), &read.paint(frost, alpha), alpha),
        (None, None) => {}
    }
}

/// A copy of the target's pixels under an area, and where it lies in the coordinates in force
/// when it was read: `at` turned by `angle` about its corner.
pub(super) struct Read {
    pub(super) copy: ImageId,
    pub(super) size: (usize, usize),
    at: LogicalRect,
    angle: f32,
}

impl Read {
    /// The paint that lays an image of `size` back where the pixels were read.
    pub(super) fn paint(&self, image: ImageId, alpha: f32) -> Paint {
        Paint::image(image, self.at.x, self.at.y, self.at.width, self.at.height, self.angle, alpha)
    }

    /// The point `(u, v)` of the copy, in fractions from its top-left, where it was read.
    fn point(&self, u: f32, v: f32) -> (f32, f32) {
        let (sin, cos) = self.angle.sin_cos();
        let (dx, dy) = (u * self.at.width, v * self.at.height);
        (self.at.x + dx * cos - dy * sin, self.at.y + dx * sin + dy * cos)
    }
}

/// The current target's pixels under `area`, copied to a scratch, under the transform in force
/// now. `None` without a GL context: femtovg cannot read a target.
pub(super) fn read_target(painter: &mut TextPainter, walk: &mut Walk<'_, '_>, area: PhysicalRect) -> Option<Read> {
    let gl = walk.shaders.as_ref()?.gl;
    let canvas = painter.canvas_mut();
    let to_target = canvas.transform();
    let whole = PhysicalRect { x0: 0, y0: 0, x1: canvas.width() as i32, y1: canvas.height() as i32 };
    let region = transformed(to_target.0, area).intersect(whole);
    if region.is_empty() {
        return None;
    }
    let size = ((region.x1 - region.x0) as usize, (region.y1 - region.y0) as usize);
    let copy = scratch(painter, walk, size)?;
    let canvas = painter.canvas_mut();
    let texture = canvas.get_native_texture(copy).ok()?;
    flush(canvas);
    // SAFETY: `paint_surface` made this context current, the flush left the target bound, and
    // femtovg's next flush rebinds every texture unit it uses.
    // A blit, not `glCopyTexSubImage2D`: NVIDIA 615 stages that copy through a CPU buffer the size
    // of the read and keeps it for the context's life, 14.7 MiB after one full-screen frost.
    unsafe {
        let target = gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING);
        let into = gl.create_framebuffer().ok()?;
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(into));
        gl.framebuffer_texture_2d(glow::DRAW_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
        // GL rows run bottom up in a target and in a `FLIP_Y` image alike.
        let (x0, y0, width, rows) = (region.x0, whole.y1 - region.y1, size.0 as i32, size.1 as i32);
        gl.blit_framebuffer(x0, y0, x0 + width, y0 + rows, 0, 0, width, rows, glow::COLOR_BUFFER_BIT, glow::NEAREST);
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, target);
        gl.delete_framebuffer(into);
    }
    // ponytail: approximate where a non-uniform `scale` meets a `rotate`, whose inverse skews and
    // an image paint cannot. Upgrade path: a raw-GL quad, as the shader stage draws.
    let from_target = to_target.inverse();
    let (x, y) = from_target.transform_point(region.x0 as f32, region.y0 as f32);
    let [a, b, c, d, ..] = from_target.0;
    let angle = b.atan2(a);
    let (sin, cos) = angle.sin_cos();
    let (width, height) = (size.0 as f32 * a.hypot(b), size.1 as f32 * (d * cos - c * sin));
    Some(Read { copy, size, at: LogicalRect { x, y, width, height }, angle })
}

/// Fills `path` with `paint` in place of what is there, a lerp by `alpha`: source-over would show
/// a translucent ground through it.
pub(super) fn replace(canvas: &mut Canvas<OpenGl>, path: &Path, paint: &Paint, alpha: f32) {
    canvas.save();
    canvas.global_composite_operation(CompositeOperation::DestinationOut);
    canvas.fill_path(path, &Paint::color(Color::rgbaf(0.0, 0.0, 0.0, alpha)));
    canvas.global_composite_operation(CompositeOperation::Lighter);
    canvas.fill_path(path, paint);
    canvas.restore();
}

#[cfg(test)]
mod tests {
    use super::super::tests::{init_headless_egl, paint_with_gl, pixel_at, surface_96x64, test_gl, text_painter};
    use super::super::*;
    use super::*;

    use mlua::Lua;

    use crate::layout::node::Fill;
    use crate::layout::paint::tests::resolved_surface;
    use crate::layout::scene::LogicalSize;
    use crate::text::shaping::ShapingHandle;

    /// A 32px box at (16, 16) on a white 64x96 panel, painted with `effect` properties and read
    /// down its middle column.
    fn paint_effect(effect: &str) -> Option<[(u8, u8, u8, u8); 8]> {
        let px = paint_effect_at(effect, &[8, 20, 32, 40, 50, 56, 64, 76].map(|y| (32, y)))?;
        Some(std::array::from_fn(|i| px[i]))
    }

    fn paint_effect_at(effect: &str, points: &[(usize, usize)]) -> Option<Vec<(u8, u8, u8, u8)>> {
        let src = format!(
            r##"return panel {{ id = "bar", width = 64, height = 96, background = "#FFFFFFFF",
                padding = {{ top = 16, left = 16 }}, child = rect {{ width = 32, height = 32, {effect} }} }}"##
        );
        paint_with_gl(&src, (64, 96), points)
    }

    fn near(actual: (u8, u8, u8, u8), expected: (u8, u8, u8)) -> bool {
        let close = |a: u8, e: u8| a.abs_diff(e) <= 3;
        close(actual.0, expected.0) && close(actual.1, expected.1) && close(actual.2, expected.2)
    }

    /// A box at (16, 16) whose outline hangs a tail to y = 56 below its centre: the fill, a border, an
    /// inset shadow, a knocked-out shadow, a backdrop and a rounded clip all take the one contour, so
    /// the tail is filled, its base carries no line, and its sides carry the band.
    #[test]
    fn an_outline_with_a_tail_is_one_shape_to_every_paint() {
        use crate::layout::node::outline::TAIL;
        let (red, white, black, pink) = ((255, 0, 0), (255, 255, 255), (0, 0, 0), (255, 127, 127));
        let (tail, base, below_base, edge, side, beside, top) =
            ((32, 51), (32, 47), (32, 48), (47, 32), (34, 52), (37, 54), (32, 20));
        let at =
            |effect: &str, points: &[(usize, usize)]| paint_effect_at(&format!("outline = {TAIL}, {effect}"), points);
        let fill = r##"background = "#FF0000FF""##;
        let border = format!(r##"{fill}, border_width = 2, border_color = "#000000FF""##);
        let inset = format!(r##"{fill}, shadows = {{ {{ inset = true, spread = 2, color = "#000000FF" }} }}"##);
        for (what, effect) in [("border", &border), ("inset", &inset)] {
            let Some(px) = at(effect, &[tail, base, below_base, edge, side, beside]) else { return };
            assert!(near(px[0], red) && near(px[1], red) && near(px[2], red), "{what}: a filled tail, no seam: {px:?}");
            assert!(near(px[3], black) && near(px[4], black), "{what}: the band runs down the tail: {px:?}");
            assert!(near(px[5], white), "{what}: nothing beside it: {px:?}");
        }
        let cast = r##"background = "#FF000080", shadows = { { offset = { y = 16 } } }"##;
        let Some(px) = at(cast, &[tail, base, top, (32, 67), (40, 60)]) else { return };
        assert!(px[..3].iter().all(|&p| near(p, pink)), "the body over no shadow: {px:?}");
        assert!(near(px[3], black) && near(px[4], black), "the tail and box cast: {px:?}");
        let Some(px) = at("effect = { backdrop = { brightness = 0 } }", &[tail, top, beside, (16, 16)]) else { return };
        assert!(near(px[0], black) && near(px[1], black), "the backdrop fills the tail: {px:?}");
        assert!(near(px[2], white) && near(px[3], white), "and stops at the contour: {px:?}");
        let clip = r##"clip = "rounded", children = { rect { width = 32, height = 32, background = "#0000FFFF" } }"##;
        let Some(px) = at(clip, &[top, (16, 16)]) else { return };
        assert!(near(px[0], (0, 0, 255)) && near(px[1], white), "children cut to the contour: {px:?}");
    }

    /// ADR-0331. An inset shadow darkens a band just inside the edge and leaves the centre, the
    /// pixel outside a round corner, and the ground alone; `offset` moves the band, a fractional
    /// `spread` shades by its fraction, and the border draws over it.
    #[test]
    fn an_inset_shadow_darkens_inside_the_edge_and_stays_in_the_shape() {
        let white = r##"background = "#FFFFFFFF""##;
        let ink = |px: (u8, u8, u8, u8)| px.0;
        let Some(px) = paint_effect_at(
            &format!("{white}, radius = 12, shadows = {{ {{ inset = true, spread = 4 }} }}"),
            &[(18, 32), (32, 18), (32, 32), (14, 32), (17, 17), (21, 21)],
        ) else {
            return;
        };
        assert!(ink(px[0]) < 8 && ink(px[1]) < 8, "the band is dark: {px:?}");
        assert!(near(px[2], (255, 255, 255)), "the centre is untouched: {px:?}");
        assert!(near(px[3], (255, 255, 255)), "outside the box is untouched: {px:?}");
        assert!(near(px[4], (255, 255, 255)), "outside the round corner is untouched: {px:?}");
        assert!(ink(px[5]) < 8, "inside the arc is dark: {px:?}");

        let Some(px) = paint_effect_at(
            &format!("{white}, shadows = {{ {{ inset = true, offset = {{ y = 6 }} }} }}"),
            &[(32, 19), (32, 24), (32, 45), (18, 32)],
        ) else {
            return;
        };
        assert!(ink(px[0]) < 8, "an offset down opens the top: {px:?}");
        assert!(near(px[1], (255, 255, 255)) && near(px[2], (255, 255, 255)), "and clears the rest: {px:?}");
        assert!(near(px[3], (255, 255, 255)), "{px:?}");

        let at = |spread: &str| {
            let shadows =
                format!("{white}, shadows = {{ {{ inset = true, spread = {spread}, color = \"#000000\" }} }}");
            paint_effect_at(&shadows, &[(32, 16), (32, 17)]).map(|px| (ink(px[0]), ink(px[1])))
        };
        let (Some(half), Some(shrunk)) = (at("0.5"), at("-0.75")) else { return };
        assert!((90..170).contains(&half.0) && half.1 > 250, "half a pixel of band shades half: {half:?}");
        assert!(shrunk.0 > 250, "a hole grown past the box shades nothing: {shrunk:?}");

        let bordered = format!(
            "{white}, border_width = 2, border_color = \"#FF0000FF\", shadows = {{ {{ inset = true, spread = 6 }} }}"
        );
        let Some(px) = paint_effect_at(&bordered, &[(17, 32), (20, 32), (32, 32)]) else { return };
        assert!(near(px[0], (255, 0, 0)), "the border is over the shadow: {px:?}");
        assert!(ink(px[1]) < 8 && near(px[2], (255, 255, 255)), "{px:?}");
    }

    /// ADR-0334. The colour filters recolour the straight sRGB colour, as CSS's do, in the order
    /// saturate, brightness, contrast: `(192, 96, 64)` through CSS's own matrices.
    #[test]
    fn a_content_colour_filter_recolours_the_straight_colour_in_order() {
        for (effect, background, want) in [
            ("saturate = 2", "#C06040FF", (255, 78, 14)),
            // Straight, then over white: a premultiplied filter would darken the translucent box.
            ("saturate = 2", "#C0604080", (255, 166, 134)),
            ("saturate = 2, brightness = 0.5", "#C06040FF", (128, 39, 7)),
            ("contrast = 0.5", "#C06040FF", (160, 112, 96)),
            ("saturate = 1", "#C06040FF", (192, 96, 64)),
        ] {
            let src = format!(r##"background = "{background}", effect = {{ {effect} }}"##);
            let Some(px) = paint_effect_at(&src, &[(32, 32)]) else { return };
            assert!(near(px[0], want), "{effect} on {background}: {px:?}");
        }
    }

    /// ADR-0334. A backdrop colour filter recolours what the glass covers at blur 0, through a
    /// blur one pass wide and through a stretched one, and leaves the ground outside it alone.
    #[test]
    fn a_backdrop_colour_filter_recolours_the_ground_under_the_glass() {
        for blur in ["", "blur = 2,", "blur = 16,"] {
            let src = format!(
                r##"return panel {{ id = "bar", width = 64, height = 32, background = "#C06040FF",
                    child = rect {{ width = 32, height = 32, effect = {{ backdrop = {{ {blur} saturate = 2 }} }} }} }}"##
            );
            let Some(px) = paint_with_gl(&src, (64, 32), &[(16, 16), (48, 16)]) else { return };
            assert!(near(px[0], (255, 78, 14)), "under the glass, `{blur}`: {px:?}");
            assert!(near(px[1], (192, 96, 64)), "beside it, `{blur}`: {px:?}");
        }
    }

    /// A blended background layer, node and shadow layer each blend with the ground under them, by
    /// the W3C formulas: `#808080` multiplies `(192, 96, 64)` darker and screens it lighter.
    #[test]
    fn a_blend_mode_composites_onto_the_ground_under_it() {
        let ground = |child: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 64, height = 64, background = "#C06040FF", padding = 16,
                    child = {child} }}"##
            )
        };
        for (blend, want) in [
            ("normal", (128, 128, 128)),
            ("multiply", (96, 48, 32)),
            ("screen", (224, 176, 160)),
            ("luminosity", (199, 103, 71)),
            ("plus_lighter", (255, 224, 192)),
            ("plus_darker", (65, 0, 0)),
        ] {
            for child in [
                format!(
                    r##"rect {{ width = 32, height = 32, background = {{ {{ fill = "#808080", blend = "{blend}" }} }} }}"##
                ),
                format!(r##"rect {{ width = 32, height = 32, background = "#808080", blend = "{blend}" }}"##),
            ] {
                let Some(px) = paint_with_gl(&ground(&child), (64, 64), &[(32, 32), (8, 8)]) else { return };
                assert!(near(px[0], want) && near(px[1], (192, 96, 64)), "{child}: {px:?}");
            }
        }
        // A blended layer over a normal one blends with it alone; the shadow multiplies the ground.
        let child = r##"rect { width = 32, height = 16, background = { { fill = "#808080", blend = "multiply" }, "#FF0000" },
            shadows = { { color = "#808080", offset = { y = 16 }, blend = "multiply" } } }"##;
        let Some(px) = paint_with_gl(&ground(child), (64, 64), &[(32, 24), (32, 40)]) else { return };
        assert!(near(px[0], (128, 0, 0)) && near(px[1], (96, 48, 32)), "{px:?}");
    }

    /// `effect.shader` with `input = "backdrop"` reads what the surface painted under the node,
    /// `padding` past its box, and the filtered copy as `u_input_blurred`; its output replaces the
    /// ground before the node.
    #[test]
    fn a_backdrop_shader_reads_the_ground_under_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let run = |name: &str, body: &str, keys: &str| {
            let frag = dir.path().join(name);
            std::fs::write(&frag, format!("void main() {{ fragColor = {body}; }}")).unwrap();
            let src = format!(
                r##"return panel {{ id = "bar", width = 64, height = 32, background = {{ gradient = "linear", angle = 90,
                    stops = {{ {{ 0, "#FF0000" }}, {{ 0.5, "#FF0000" }}, {{ 0.5, "#00FF00" }}, {{ 1, "#00FF00" }} }} }},
                    child = rect {{ width = 32, height = 32, effect = {{ {keys} shader = {{ source = "{}", input = "backdrop",
                    padding = 32 }} }} }} }}"##,
                frag.display()
            );
            paint_with_gl(&src, (64, 32), &[(16, 16), (48, 16)])
        };
        let Some(px) = run("swap.frag", "vec4(mantle_input(v_uv).bgr, 1.0)", "") else { return };
        assert!(near(px[0], (0, 0, 255)) && near(px[1], (0, 255, 0)), "the red under the box, swizzled: {px:?}");
        let Some(px) = run("bend.frag", "mantle_input(v_uv + vec2(1.0, 0.0))", "") else { return };
        assert!(near(px[0], (0, 255, 0)), "a box's width to the right, inside the padding: {px:?}");
        let Some(px) = run("grey.frag", "mantle_input_blurred(v_uv)", "backdrop = { saturate = 0 },") else { return };
        assert!(near(px[0], (54, 54, 54)), "the filtered copy: {px:?}");
    }

    /// The output replaces the ground, so a program returning its input leaves a translucent ground
    /// as it was, inside the box and in the padding; source-over would add the ground to itself.
    #[test]
    fn an_identity_backdrop_shader_leaves_a_translucent_ground_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let frag = dir.path().join("same.frag");
        std::fs::write(&frag, "void main() { fragColor = mantle_input(v_uv); }").unwrap();
        let paint = |effect: String| {
            let src = format!(
                r##"return panel {{ id = "bar", width = 64, height = 32, background = "#FF000080",
                    child = rect {{ width = 32, height = 32, {effect} }} }}"##
            );
            paint_with_gl(&src, (64, 32), &[(16, 16), (48, 16)])
        };
        let shader =
            format!(r#"effect = {{ shader = {{ source = "{}", input = "backdrop", padding = 16 }} }}"#, frag.display());
        let (Some(bare), Some(shaded)) = (paint(String::new()), paint(shader)) else { return };
        assert!(bare[0].3 < 200, "the ground is translucent: {bare:?}");
        for (bare, shaded) in bare.iter().zip(&shaded) {
            assert!(
                [
                    bare.0.abs_diff(shaded.0),
                    bare.1.abs_diff(shaded.1),
                    bare.2.abs_diff(shaded.2),
                    bare.3.abs_diff(shaded.3)
                ]
                .iter()
                .all(|d| *d <= 2),
                "{bare:?} vs {shaded:?}"
            );
        }
    }

    /// ADR-0254's gradient path: an opaque box's shadow lands offset under it, sharp without a
    /// blur and fading over a layer's `blur` either side of its edge with one.
    #[test]
    fn an_opaque_boxs_shadow_is_painted_offset_under_it() {
        let Some(px) = paint_effect(r##"background = "#FF0000FF", shadows = { { offset = { y = 16 } } }"##) else {
            return;
        };
        assert!(near(px[0], (255, 255, 255)) && near(px[1], (255, 0, 0)), "{px:?}");
        assert!(near(px[5], (0, 0, 0)), "the shadow shows below the box: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "and ends 16px below it: {px:?}");

        let Some(px) = paint_effect(r##"background = "#FF0000FF", shadows = { { offset = { y = 16 }, blur = 8 } }"##)
        else {
            return;
        };
        assert!((90..170).contains(&px[6].0), "half dark at the shadow's edge: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "and gone `blur` past it: {px:?}");
    }

    /// Both `shadows` layers paint, the first over the second where they overlap, on the gradient
    /// path and the layer path alike.
    #[test]
    fn every_shadow_layer_paints_and_the_first_is_on_top() {
        let layers = r##"shadows = { { color = "#0000FFFF", offset = { y = 16 } }, { color = "#00FF00FF", offset = { y = 32 } } }"##;
        for body in [r##"background = "#FF0000FF""##, r##"background = "#FF0000FE", shadow_mode = "content""##] {
            let points = [(32, 32), (32, 56), (32, 72), (32, 88)];
            let Some(px) = paint_effect_at(&format!("{body}, {layers}"), &points) else { return };
            assert!(near(px[0], (255, 0, 0)), "{body}: the box over both: {px:?}");
            assert!(near(px[1], (0, 0, 255)), "{body}: the first over the second: {px:?}");
            assert!(near(px[2], (0, 255, 0)), "{body}: the second past the first: {px:?}");
            assert!(near(px[3], (255, 255, 255)), "{body}: {px:?}");
        }
    }

    /// A radius past half the box is a circle, and its shadow is that circle's, not nothing.
    #[test]
    fn a_circle_past_its_half_radius_still_casts_a_shadow() {
        let effect = r##"background = "#FF0000FF", radius = 999, shadows = { { offset = { y = 16 } } }"##;
        let Some(px) = paint_effect(effect) else { return };
        assert!(near(px[5], (0, 0, 0)), "the circle's shadow below it: {px:?}");
    }

    /// CSS: a square box's spread shadow keeps square corners.
    #[test]
    fn a_square_boxs_spread_shadow_keeps_square_corners() {
        let effect = r##"background = "#FF0000FF", shadows = { { offset = { y = 16 }, spread = 4 } }"##;
        let Some(px) = paint_effect_at(effect, &[(12, 67)]) else { return };
        assert!(near(px[0], (0, 0, 0)), "the spread shadow's corner pixel: {px:?}");
    }

    /// Switching paths, as a background alpha tween reaching 1 does, keeps the shadow's softness.
    #[test]
    fn the_gradient_and_the_layer_cast_the_same_blurred_shadow() {
        let column = [58, 60, 62, 64, 66, 68, 70].map(|y| (32, y));
        let shadow = r##"shadows = { { offset = { y = 16 }, blur = 8 } }"##;
        let Some(gradient) = paint_effect_at(&format!(r##"background = "#FF0000FF", {shadow}"##), &column) else {
            return;
        };
        let Some(layer) =
            paint_effect_at(&format!(r##"background = "#FF0000FE", shadow_mode = "content", {shadow}"##), &column)
        else {
            return;
        };
        let worst = gradient.iter().zip(&layer).map(|(g, l)| g.0.abs_diff(l.0)).max().unwrap();
        assert!(worst <= 14, "gradient {gradient:?} against layer {layer:?}");
    }

    /// ADR-0254's layer path: a half-transparent box casts a half-strength shadow, and the box
    /// composites over it rather than beside it.
    #[test]
    fn a_translucent_box_casts_a_shadow_at_its_own_alpha_under_itself() {
        let content = r##"background = "#FF000080", shadow_mode = "content", shadows = { { offset = { y = 16 } } }"##;
        let Some(px) = paint_effect(content) else { return };
        assert!(near(px[0], (255, 255, 255)), "{px:?}");
        assert!(near(px[1], (255, 127, 127)), "the box alone: {px:?}");
        assert!(near(px[3], (191, 63, 63)), "the box over its shadow: {px:?}");
        assert!(near(px[5], (127, 127, 127)), "the shadow alone, at the box's alpha: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "{px:?}");

        let Some(px) = paint_effect(&content.replace("y = 16 }", "y = 16 }, blur = 8")) else { return };
        assert!((180..205).contains(&px[6].0), "a quarter dark at the blurred shadow's edge: {px:?}");
        assert!(px[7].0 >= 250, "and gone 3 sigma past it: {px:?}");
    }

    /// ADR-0254: `effect.blur` spreads the box past its edge, premultiplied: a red edge fades to
    /// pink over white, never through a dark fringe.
    #[test]
    fn an_effect_blur_spreads_the_box_past_its_edge_without_darkening_it() {
        let Some(px) = paint_effect(r##"background = "#FF0000FF", effect = { blur = 4 }"##) else { return };
        assert!(near(px[2], (255, 0, 0)), "the middle stays red: {px:?}");
        assert!(px[4].1 > 30 && px[4].1 < 240, "2px past the edge is pink: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "and 3 sigma past it is white: {px:?}");
        assert!(px.iter().all(|p| p.0 >= 250), "no pixel darkens: {px:?}");
    }

    /// A mask cuts the pixels the shadow is cast from: the masked-away half casts nothing.
    #[test]
    fn a_masked_box_casts_the_shadow_of_what_its_mask_keeps() {
        let effect = r##"background = "#FF0000FF", shadows = { { offset = { y = 16 } } }, shadow_mode = "content",
            mask = { gradient = "linear", angle = 90,
                stops = { { 0, "#FFFFFFFF" }, { 0.5, "#FFFFFFFF" }, { 0.5, "#FFFFFF00" }, { 1, "#FFFFFF00" } } }"##;
        let Some(px) = paint_effect_at(effect, &[(20, 30), (44, 30), (20, 56), (44, 56)]) else { return };
        assert!(near(px[0], (255, 0, 0)) && near(px[1], (255, 255, 255)), "the mask keeps the left half: {px:?}");
        assert!(near(px[2], (0, 0, 0)) && near(px[3], (255, 255, 255)), "and only it casts: {px:?}");
    }

    /// ADR-0260. A translucent box's shadow stops at its edge: the body shows the ground, not the
    /// shadow, and past the edge it is the opaque box's shadow, pixel for pixel.
    #[test]
    fn a_box_shadow_is_knocked_out_under_a_translucent_box() {
        let shadow = r##"shadows = { { offset = { y = 16 }, blur = 8 } }"##;
        let column: Vec<(usize, usize)> = (50..80).map(|y| (32, y)).collect();
        let Some(opaque) = paint_effect_at(&format!(r##"background = "#FF0000FF", {shadow}"##), &column) else {
            return;
        };
        let Some(glass) = paint_effect_at(&format!(r##"background = "#FF000080", {shadow}"##), &column) else { return };
        assert_eq!(glass, opaque, "the shadow outside the box");
        let Some(px) = paint_effect(r##"background = "#FF000080", shadows = { { offset = { y = 16 } } }"##) else {
            return;
        };
        assert!(near(px[3], (255, 127, 127)), "the box alone over its shadow: {px:?}");
        assert!(near(px[5], (0, 0, 0)), "the shadow at full strength: {px:?}");
    }

    /// ADR-0260. A shadow inside a translucent box's edge, normal or blended, leaves every pixel
    /// under the box as the box alone paints it: the cut-out is by the box, not by the shadow's reach.
    #[test]
    fn a_shadow_inside_the_box_leaves_no_ring_under_a_translucent_box() {
        let inside: Vec<(usize, usize)> =
            (16..48).map(|y| (32, y)).chain((16..48).map(|x| (x, 32))).chain((16..48).map(|x| (x, 17))).collect();
        for (radius, shadow) in [
            (16, r##"{ color = "#000000FF", spread = -2 }"##),
            (0, r##"{ color = "#000000FF", offset = { x = 2 }, spread = -1 }"##),
            (0, r##"{ color = "#A6A6A6FF", offset = { x = 1.25 }, spread = -0.75 }"##),
            (0, r##"{ color = "#A6A6A6FF", offset = { x = 1.25 }, spread = -0.75, blend = "plus_darker" }"##),
        ] {
            let body = format!(r##"radius = {radius}, background = "#99999940""##);
            let Some(bare) = paint_effect_at(&body, &inside) else { return };
            let Some(cast) = paint_effect_at(&format!("{body}, shadows = {{ {shadow} }}"), &inside) else { return };
            assert_eq!(cast, bare, "{shadow}");
        }
    }

    /// A zero-blur shadow's outer rim covers the pixel by its spread's fraction, cut out or not.
    #[test]
    fn a_zero_blur_shadow_rim_covers_by_its_fraction_under_a_translucent_box() {
        for (spread, ink) in [(0.25, 191), (0.5, 127), (0.75, 64)] {
            let e =
                format!(r##"background = "#FF000040", shadows = {{ {{ color = "#000000FF", spread = {spread} }} }}"##);
            let Some(px) = paint_effect_at(&e, &[(15, 32), (32, 15)]) else { return };
            assert!(px.iter().all(|p| p.0.abs_diff(ink) <= 2), "spread {spread}: {px:?}");
        }
    }

    /// Twelve alternating white and black stops, `stops` for a linear gradient `background`.
    const STRIPES: &str = r##"local stops = {}
        for i = 0, 11 do
            local colour = i % 2 == 0 and "#FFFFFFFF" or "#000000FF"
            stops[#stops + 1] = { i / 12, colour }
            stops[#stops + 1] = { (i + 1) / 12, colour }
        end
        "##;

    /// ADR-0260. A box whose shadow lands outside its parent, or collapses, still draws.
    #[test]
    fn a_box_draws_when_its_shadow_does_not() {
        for shadow in ["shadows = { { offset = { y = 100 } } }", "shadows = { { spread = -20 } }"] {
            let Some(px) = paint_effect_at(&format!(r##"background = "#FF000080", {shadow}"##), &[(32, 32)]) else {
                return;
            };
            assert!(near(px[0], (255, 127, 127)), "{shadow}: {px:?}");
        }
    }

    /// CSS: a spread past a small radius sharpens it by `1 + (r / spread - 1)^3`.
    #[test]
    fn a_spread_sharpens_a_radius_smaller_than_itself() {
        assert_eq!(
            [(4.0, 8.0), (0.0, 8.0), (12.0, 8.0), (6.0, -2.0), (2.0, -4.0)].map(|(r, s)| spread_radius(r, s)),
            [11.0, 0.0, 20.0, 4.0, 0.0]
        );
    }

    /// ADR-0260. Inside its edge a box shadow changes no pixel: not the label's, not a frosted
    /// body's, whose blur reads the ground before its own shadow is drawn.
    #[test]
    fn a_box_shadow_leaves_a_frosted_labelled_pill_as_it_was() {
        let src = |shadow: &str| {
            format!(
                r##"{STRIPES} return panel {{ id = "bar", width = 96, height = 48, child = rect {{ width = "fill", height = "fill",
                    background = {{ gradient = "linear", angle = 90, stops = stops }}, padding = 8,
                    children = {{ rect {{ width = 80, height = 32, radius = 16, effect = {{ backdrop = {{ blur = 4 }} }},
                        background = "#FFFFFF33", padding = 8, {shadow}
                        children = {{ text {{ content = "hi", foreground = "#FF0000FF" }} }} }} }} }} }}"##
            )
        };
        // The pill is 8..88 x 8..40, rounded 16; a pixel's inset from its arc.
        let inside = |x: usize, y: usize| {
            let (dx, dy) = ((x as f32 + 0.5 - 48.0).abs() - 24.0, (y as f32 + 0.5 - 24.0).abs());
            dx.max(0.0).hypot(dy) <= 14.5
        };
        let body: Vec<(usize, usize)> =
            (8..88).flat_map(|x| (8..40).map(move |y| (x, y))).filter(|&(x, y)| inside(x, y)).collect();
        let Some(plain) = paint_with_gl(&src(""), (96, 48), &body) else { return };
        let Some(boxed) = paint_with_gl(&src("shadows = { { blur = 8, offset = { y = 4 } } },"), (96, 48), &body)
        else {
            return;
        };
        assert!(plain == boxed, "the body unchanged by its box shadow");
        let content = src(r#"shadows = { { blur = 8, offset = { y = 4 } } }, shadow_mode = "content","#);
        let Some(cast) = paint_with_gl(&content, (96, 48), &body) else { return };
        assert!(cast != plain, "a content shadow shows through the glass");
    }

    /// ADR-0260. A scoop's box shadow is its silhouette: its own notches show the shadow under
    /// them, the shadow's notches stay clear, and the body is knocked out.
    #[test]
    fn a_scooped_box_shadow_is_its_silhouette_knocked_out() {
        let effect =
            r##"background = "#FF000080", radius = 12, corner_shape = "scoop", shadows = { { offset = { y = 16 } } }"##;
        let strip: Vec<(usize, usize)> = (18..31).map(|y| (32, y)).collect();
        let Some(px) = paint_effect_at(effect, &[&[(32, 40), (32, 56), (17, 46), (17, 63)], &strip[..]].concat())
        else {
            return;
        };
        assert!(near(px[0], (255, 127, 127)), "the body over no shadow: {px:?}");
        assert!(px[4..].iter().all(|&p| near(p, (255, 127, 127))), "none above the cast either: {px:?}");
        assert!(near(px[1], (0, 0, 0)), "the shadow below: {px:?}");
        assert!(near(px[2], (0, 0, 0)), "the box's notch shows the shadow: {px:?}");
        assert!(near(px[3], (255, 255, 255)), "the shadow's own notch is clear: {px:?}");
    }

    /// ADR-0258. An unchanged layer composites what it last finished, per surface. It stays kept
    /// while its surface's list holds it, however long the region skips it, and goes on the first
    /// paint that no longer holds it. One drawing a texture or a glass is never kept: either can
    /// change under its list.
    #[test]
    fn an_unchanged_layer_composites_what_it_last_finished() {
        let Some(instance) = init_headless_egl(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        let list = |child: &str| {
            surface_96x64(&format!(
                r##"return panel {{ id = "bar", width = 96, height = 64, padding = 16, child = {child} }}"##
            ))
        };
        let (gl, mut stage) = (test_gl(&instance), image_shader::ShaderStage::default());
        let mut paint = |painter: &mut TextPainter, surface: &str, list: &DisplayList, region: PhysicalRect| {
            let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
            let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
            let _ = execute(surface, painter, images, captures, list, 1.0, (96.0, 64.0), &[region], shaders);
        };
        let whole = PhysicalRect { x0: 0, y0: 0, x1: 96, y1: 64 };
        let blurred = list(r##"rect { width = 32, height = 32, background = "#FF0000FF", effect = { blur = 2 } }"##);
        let red = list(r##"rect { width = 32, height = 32, background = "#FF0000FE", effect = { blur = 2 } }"##);
        let layer = blurred.commands.last().unwrap();
        paint(&mut painter, "a", &blurred, whole);
        paint(&mut painter, "b", &red, whole);
        let (_, content) = painter.layer("a", layer).expect("a layer with no texture is kept");
        assert!(painter.layer("b", red.commands.last().unwrap()).is_some(), "the other surface's, in the same place");
        // Only a composite of the kept image could show this.
        let canvas = painter.canvas_mut();
        canvas.set_render_target(RenderTarget::Image(content));
        let (width, height) = ((layer.clip.x1 - layer.clip.x0) as u32, (layer.clip.y1 - layer.clip.y0) as u32);
        canvas.clear_rect(0, 0, width, height, Color::rgbaf(0.0, 0.0, 1.0, 1.0));
        canvas.set_render_target(RenderTarget::Screen);
        paint(&mut painter, "a", &blurred, whole);
        assert_eq!(pixel_at(painter.canvas_mut(), 20, 20), (0, 0, 255, 255));

        let corner = PhysicalRect { x0: 90, y0: 0, x1: 96, y1: 4 };
        for _ in 0..=crate::text::atlas::LAYER_PAINTS {
            paint(&mut painter, "a", &blurred, corner);
        }
        assert!(painter.layer("a", layer).is_some(), "held by its list while the region skips it");
        paint(&mut painter, "a", &DisplayList::default(), whole);
        assert!(painter.layer("a", layer).is_none(), "gone once its list drops it");

        let textured = list(r#"image { source = "/nonexistent.png", width = 32, height = 32, effect = { blur = 2 } }"#);
        paint(&mut painter, "a", &textured, whole);
        assert!(painter.layer("a", textured.commands.last().unwrap()).is_none());

        // A glass inside reads what is under it, which the layer's command does not name (ADR-0256).
        let glass = list(
            r#"rect { width = 32, height = 32, effect = { blur = 2 }, children = { rect { width = 16, height = 16, effect = { backdrop = { blur = 2 } } } } }"#,
        );
        paint(&mut painter, "a", &glass, whole);
        assert!(painter.layer("a", glass.commands.last().unwrap()).is_none(), "{glass:?}");
    }

    #[test]
    fn a_shader_nodes_quad_casts_a_shadow_through_the_layer() {
        let dir = tempfile::tempdir().unwrap();
        let frag = dir.path().join("blue.frag");
        std::fs::write(&frag, "void main() { fragColor = vec4(0.0, 0.0, 1.0, 1.0); }").unwrap();
        let src = format!(
            r##"return panel {{ id = "bar", width = 64, height = 96, background = "#FFFFFFFF",
                padding = {{ top = 16, left = 16 }}, child = shader {{ width = 32, height = 32,
                source = "{}", shadows = {{ {{ offset = {{ y = 16 }} }} }}}} }}"##,
            frag.display()
        );
        let Some(px) = paint_with_gl(&src, (64, 96), &[(32, 32), (32, 56), (32, 76)]) else { return };
        assert_eq!(px[0], (0, 0, 255, 255), "the quad lands where the node is");
        assert_eq!(px[1], (0, 0, 0, 255), "and casts its shadow below");
        assert_eq!(px[2], (255, 255, 255, 255));
    }

    /// ADR-0256. Eight-pixel stripes under a frosted pill blur to grey inside the pill only. The
    /// pill's border and child draw sharp over it, and the corner outside its arc keeps the stripe.
    /// Translucent stripes are replaced by their blur, not shown through it.
    #[test]
    fn an_effect_backdrop_frosts_the_stripes_under_a_pill_and_nothing_else() {
        let src = &(STRIPES.to_owned()
            + r##"return panel { id = "bar", width = 96, height = 48, child = rect { width = "fill", height = "fill",
                background = { gradient = "linear", angle = 90, stops = stops }, padding = 8,
                children = { rect { width = 80, height = 32, radius = 16, effect = { backdrop = { blur = 4 } },
                    border_width = 2, border_color = "#00FF00FF", children = {
                        rect { width = 4, height = 4, margin = { left = 38, top = 14 }, background = "#FF0000FF" } } } } } }"##);
        let row: Vec<(usize, usize)> = (24..72).map(|x| (x, 30)).collect();
        let fixed = [(4, 24), (12, 4), (44, 44), (10, 10), (48, 9), (48, 24)];
        let points = [&row[..], &fixed].concat();
        let translucent = src.replace("#FFFFFFFF", "#FFFFFF80").replace("#000000FF", "#00000000");
        for (src, grey, (white, black)) in [
            (src, 60..=196, ((255, 255, 255, 255), (0, 0, 0, 255))),
            (&translucent, 30..=100, ((128, 128, 128, 128), (0, 0, 0, 0))),
        ] {
            let Some(px) = paint_with_gl(src, (96, 48), &points) else { return };
            let (row, fixed) = px.split_at(row.len());
            assert!(row.iter().all(|p| grey.contains(&p.0) && p.0 == p.2), "inside the pill is grey: {row:?}");
            assert_eq!(fixed[..4], [white, black, black, black], "outside");
            assert_eq!(fixed[4], (0, 255, 0, 255), "the border is sharp");
            assert_eq!(fixed[5], (255, 0, 0, 255), "and so is the child");
        }
    }

    /// ADR-0256. A frosted node fading in crosses from its backdrop to the blur, never through the
    /// surface's transparency, and a box at the surface's edge does not blur in the void past it.
    #[test]
    fn a_fading_backdrop_keeps_an_opaque_ground_opaque_up_to_the_surfaces_edge() {
        let src = r##"return panel { id = "bar", width = 64, height = 32, background = "#FF0000FF",
            child = rect { width = "fill", height = "fill", effect = { backdrop = { blur = 4 } }, opacity = 0.5 } }"##;
        let Some(px) = paint_with_gl(src, (64, 32), &[(32, 16), (0, 0), (63, 31)]) else { return };
        assert_eq!(px, [(255, 0, 0, 255); 3]);
    }

    /// ADR-0256. A glass at a clipping parent's edge reads only inside that parent: a red header
    /// above a blue viewport does not bleed into the glass at the viewport's top.
    #[test]
    fn a_glass_reads_nothing_past_its_parents_clip() {
        let src = r##"return panel { id = "bar", width = 64, height = 48, child = column { width = "fill", children = {
            rect { width = "fill", height = 16, background = "#FF0000FF" },
            rect { width = "fill", height = 32, background = "#0000FFFF", clip = "box",
                children = { rect { width = "fill", height = 16, effect = { backdrop = { blur = 4 } } } } } } } }"##;
        let Some(px) = paint_with_gl(src, (64, 48), &[(32, 16), (32, 8)]) else { return };
        assert_eq!(px, [(0, 0, 255, 255), (255, 0, 0, 255)]);
    }

    /// ADR-0256. A rounded clip is not a backdrop root, as CSS's `overflow: hidden` is not: a
    /// frosted pill inside a translucent rounded card blurs the stripes behind the card, and the
    /// card's own translucent fill still blends over them once.
    #[test]
    fn a_glass_in_a_rounded_clip_blurs_what_is_behind_the_card() {
        let src = &(STRIPES.to_owned()
            + r##"return panel { id = "bar", width = 96, height = 48, padding = 4,
                background = { gradient = "linear", angle = 90, stops = stops },
                child = rect { width = 88, height = 40, radius = 8, clip = "rounded", padding = 4,
                    children = { rect { width = 80, height = 32, radius = 16, effect = { backdrop = { blur = 4 } },
                        background = "#0000FF40" } } } }"##);
        let row: Vec<(usize, usize)> = (24..72).map(|x| (x, 20)).collect();
        let points = [&row[..], &[(2, 2), (12, 6), (20, 6)]].concat();
        let Some(px) = paint_with_gl(src, (96, 48), &points) else { return };
        let (row, fixed) = px.split_at(row.len());
        assert!(row.iter().all(|p| (45..=150).contains(&p.0) && p.2 > p.0), "inside the pill is frosted: {row:?}");
        let (white, black) = ((255, 255, 255, 255), (0, 0, 0, 255));
        assert_eq!(fixed, [white, black, white], "outside the pill the stripes are sharp");
        // A translucent ground under the card composites back once.
        let translucent = src.replace("#FFFFFFFF", "#FFFFFF80").replace("#000000FF", "#00000000");
        let Some(px) = paint_with_gl(&translucent, (96, 48), &points) else { return };
        let (white, black) = ((128, 128, 128, 128), (0, 0, 0, 0));
        assert_eq!(px[row.len()..], [white, black, white]);
    }

    /// ADR-0256. A backdrop split red and blue along the diagonal x + y = 52 across a frosted pill
    /// reads red above it and blue below, blended only across it: the region is read where the pill
    /// is, the right way up and round, on the screen, in a rounded clip's or a mask's offscreen,
    /// from under the node's own layer, and through a transform. The pill sits off its targets'
    /// vertical centres, so an unflipped read lands elsewhere.
    #[test]
    fn a_backdrop_is_read_where_the_box_is_in_every_target() {
        let opaque_mask = r##"mask = { gradient = "linear", stops = { { 0, "#FFFFFFFF" }, { 1, "#FFFFFFFF" } } },"##;
        for (wrapper, blur, pill) in [
            ("", "", ""),
            (r#"radius = 4, clip = "rounded","#, "", ""),
            (opaque_mask, "", ""),
            ("", "blur = 1,", r##"border_width = 1, border_color = "#00FF00FF""##),
            ("translate = { x = 4, y = -4 },", "", ""),
            ("", "", "rotate = 180"),
        ] {
            let src = format!(
                r##"return panel {{ id = "bar", width = 96, height = 96, padding = 8, child = rect {{
                    width = 80, height = 64, {wrapper} children = {{ rect {{ width = "fill", height = "fill",
                        padding = {{ left = 8 }}, background = {{ gradient = "linear", angle = 135, stops = {{ {{ 0, "#FF0000FF" }},
                            {{ 0.25, "#FF0000FF" }}, {{ 0.25, "#0000FFFF" }}, {{ 1, "#0000FFFF" }} }} }},
                        children = {{ rect {{ width = 64, height = 32, radius = 16, effect = {{ {blur} backdrop = {{ blur = 2 }} }}, {pill} }} }} }} }} }} }}"##
            );
            let points = [(26, 14), (30, 22), (26, 26), (56, 28), (13, 12)];
            let Some(px) = paint_with_gl(&src, (96, 96), &points) else { return };
            let case = format!("wrapper {{ {wrapper} }}, effect {{ {blur} }}, pill {{ {pill} }}: {px:?}");
            assert!(px[0].0 > 240 && px[0].2 < 15, "red above the split, {case}");
            assert!(px[3].2 > 240 && px[3].0 < 15, "blue below it, {case}");
            assert!(px[1..3].iter().all(|p| (40..=215).contains(&p.0) && (40..=215).contains(&p.2)), "blended, {case}");
            assert_eq!(px[4], (255, 0, 0, 255), "sharp outside the pill, {case}");
        }
    }

    /// ADR-0262. The engine's blur, run at a fraction of the size from sigma 8, stays within
    /// 3 of 255 of the reference Gaussian: femtovg's up to sigma 8, its own at full size past it.
    /// 250 is no multiple of the factors, so the halvings read past an odd edge.
    #[test]
    fn the_engines_blur_matches_the_reference_gaussian() {
        let Some(instance) = init_headless_egl(250, 250) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 250, 250) else { return };
        let gl = test_gl(&instance);
        let mut stage = image_shader::ShaderStage::default();
        let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
        let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
        let (drawn, split) = (Vec::new(), PaintSplit::default());
        let mut walk = Walk { images, captures, scale: 1.0, scratch: Vec::new(), drawn, shaders, split, surface: "t" };
        let size = (250, 250);
        // A 64px red square striped blue every 8px, in the middle of a transparent target.
        let source = scratch(&mut painter, &mut walk, size).unwrap();
        let canvas = painter.canvas_mut();
        canvas.set_render_target(RenderTarget::Image(source));
        canvas.clear_rect(0, 0, 250, 250, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
        let colour = |r, b| Fill::Color(Rgba { r, g: 0.0, b, a: 1.0 });
        fill_rect(
            canvas,
            LogicalRect { x: 96.0, y: 96.0, width: 64.0, height: 64.0 },
            &Radii::default(),
            &colour(1.0, 0.0),
        );
        for x in (96..160).step_by(8) {
            let stripe = LogicalRect { x: x as f32, y: 96.0, width: 4.0, height: 64.0 };
            fill_rect(canvas, stripe, &Radii::default(), &colour(0.0, 1.0));
        }
        canvas.set_render_target(RenderTarget::Screen);
        let read = |painter: &mut TextPainter, image: ImageId| {
            let canvas = painter.canvas_mut();
            canvas.clear_rect(0, 0, 250, 250, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
            fill_image(canvas, image, LogicalRect { x: 0.0, y: 0.0, width: 250.0, height: 250.0 }, 1.0);
            flush(canvas);
            canvas.screenshot().expect("screenshot reads back the pbuffer")
        };
        for sigma in [2.0, 6.0, 8.0, 16.0, 32.0] {
            let engine = blurred(&mut painter, &mut walk, source, size, (sigma, node::Tone::default())).unwrap();
            let engine = read(&mut painter, engine);
            let reference = scratch(&mut painter, &mut walk, size).unwrap();
            if sigma <= 8.0 {
                painter.canvas_mut().filter_image(reference, femtovg::ImageFilter::GaussianBlur { sigma }, source);
            } else {
                assert!(gaussian(&mut painter, &mut walk, source, reference, size, (sigma, node::Tone::default()), 1));
            }
            let reference = read(&mut painter, reference);
            let diffs = engine
                .buf()
                .iter()
                .zip(reference.buf())
                .flat_map(|(a, b)| [a.r.abs_diff(b.r), a.g.abs_diff(b.g), a.b.abs_diff(b.b), a.a.abs_diff(b.a)]);
            let (worst, squares) = diffs.fold((0, 0.0), |(worst, sum), d| (worst.max(d), sum + f64::from(d).powi(2)));
            let psnr = 10.0 * (255.0_f64.powi(2) / (squares / (250.0 * 250.0 * 4.0))).log10();
            assert!(worst <= 3 && psnr >= 53.0, "sigma {sigma}: worst {worst}, PSNR {psnr:.1} dB");
        }
    }

    /// ADR-0262. Past sigma 8 a blur keeps widening: 20px outside a black box at sigma 16 is
    /// about a tenth dark, where femtovg's capped kernel left it white.
    #[test]
    fn a_blur_past_sigma_8_keeps_widening() {
        let src = r##"return panel { id = "bar", width = 200, height = 160, padding = { left = 68, top = 48 },
            background = "#FFFFFFFF", child = rect { width = 64, height = 64, background = "#000000FF",
                effect = { blur = 16 } } }"##;
        let Some(px) = paint_with_gl(src, (200, 160), &[(152, 80)]) else { return };
        assert!((210..=240).contains(&px[0].0), "{px:?}");
    }

    /// ADR-0262. A frame that blurs again allocates no texture, even when its blurs' chains
    /// outnumber the pool's sizes: a probe image created after ten frames takes the slot and
    /// version the probe before them freed, as nothing else was created in between.
    #[test]
    fn repainting_a_blur_allocates_no_texture() {
        let glass =
            |width: u32| format!("rect {{ width = {width}, height = 30, effect = {{ backdrop = {{ blur = 32 }} }} }},");
        let src = format!(
            r##"return panel {{ id = "bar", width = 800, height = 300, padding = 110, background = "#FF0000FF",
                child = row {{ spacing = 10, children = {{ {} rect {{ width = 30, height = 30, effect = {{ backdrop = {{ blur = 2 }} }} }} }} }} }}"##,
            [10, 20, 30, 40, 50].map(glass).concat()
        );
        let Some(instance) = init_headless_egl(800, 300) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 800, 300) else { return };
        let gl = test_gl(&instance);
        let mut stage = image_shader::ShaderStage::default();
        let list = build(&resolved_surface(&Lua::new(), &src, LogicalSize { width: 800.0, height: 300.0 }), 1.0, None);
        let whole = PhysicalRect { x0: 0, y0: 0, x1: 800, y1: 300 };
        let mut paint = |painter: &mut TextPainter| {
            let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
            let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
            let _ = execute("test", painter, images, captures, &list, 1.0, (800.0, 300.0), &[whole], shaders);
        };
        let probe = |painter: &mut TextPainter| {
            let canvas = painter.canvas_mut();
            let id = canvas.create_image_empty(1, 1, PixelFormat::Rgba8, ImageFlags::empty()).unwrap();
            canvas.delete_image(id);
            format!("{id:?}")
        };
        paint(&mut painter);
        let (first, second) = (probe(&mut painter), probe(&mut painter));
        for _ in 0..10 {
            paint(&mut painter);
        }
        let third = probe(&mut painter);
        // Slot map keys print as `index v version`; each create and delete moves the version by 2.
        let version = |key: &str| key.trim_end_matches(')').rsplit('v').next().unwrap().parse::<u32>().unwrap();
        assert_eq!(version(&third) - version(&second), version(&second) - version(&first), "{first} {second} {third}");
    }

    /// ADR-0258. A layer too big for the budget is dropped alone; older layers under it stay.
    #[test]
    fn a_layer_over_the_budget_evicts_only_itself() {
        let Some(instance) = init_headless_egl(8, 8) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 8, 8) else { return };
        let command = |colour: &str| {
            let src = format!(r#"return panel {{ id = "bar", width = 96, height = 64, background = "{colour}" }}"#);
            surface_96x64(&src).commands[0].clone()
        };
        let (small, big) = (command("#FF0000FF"), command("#0000FFFF"));
        let mut image =
            || painter.canvas_mut().create_image_empty(1, 1, PixelFormat::Rgba8, ImageFlags::empty()).unwrap();
        let (small_id, big_id) = (image(), image());
        painter.keep_layer("other", &small, (Vec::new(), small_id), (8, 8));
        painter.recycle_scratch([]);
        painter.keep_layer("test", &big, (Vec::new(), big_id), (5000, 5000));
        let retired = painter.sweep_layers("test", |_| true);
        assert_eq!(retired, [(big_id, (5000, 5000))]);
        assert!(painter.layer("other", &small).is_some());
    }

    /// A 32px box at (16, 16) on a white 64x96 panel with `effect.shader` running `frag`, read at
    /// `points`. `shader` holds the keys after `source`, `body` the box's own.
    fn paint_shader(frag: &str, shader: &str, body: &str, points: &[(usize, usize)]) -> Option<Vec<(u8, u8, u8, u8)>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("effect.frag");
        std::fs::write(&path, frag).unwrap();
        let src = format!(
            r##"return panel {{ id = "bar", width = 64, height = 96, background = "#FFFFFFFF",
                padding = {{ top = 16, left = 16 }}, child = rect {{ width = 32, height = 32,
                effect = {{ shader = {{ source = "{}", {shader} }} }}, {body} }} }}"##,
            path.display()
        );
        paint_with_gl(&src, (64, 96), points)
    }

    /// ADR-0336. The program reads the subtree and its output replaces it: an inversion turns a
    /// red box cyan and the ground around it stays, and a pass-through keeps a two-colour box the
    /// way up it was drawn.
    #[test]
    fn an_effect_shader_replaces_the_subtree_with_its_output() {
        let invert = "void main() { vec4 c = mantle_input(v_uv); fragColor = vec4(vec3(c.a) - c.rgb, c.a); }";
        let Some(px) = paint_shader(invert, "", r##"background = "#FF0000FF""##, &[(32, 32), (8, 8)]) else { return };
        assert!(near(px[0], (0, 255, 255)), "inverted: {px:?}");
        assert!(near(px[1], (255, 255, 255)), "outside the box: {px:?}");

        let same = "void main() { fragColor = mantle_input(v_uv); }";
        let halves = r##"children = { column { children = {
            rect { width = 32, height = 16, background = "#FF0000FF" },
            rect { width = 32, height = 16, background = "#0000FFFF" } } } }"##;
        let Some(px) = paint_shader(same, "", halves, &[(32, 20), (32, 44)]) else { return };
        assert!(near(px[0], (255, 0, 0)) && near(px[1], (0, 0, 255)), "top red, bottom blue: {px:?}");
    }

    /// `u_progress` reaches an `effect.shader` program, and a different value is a different pixel.
    #[test]
    fn an_effect_shader_reads_u_progress() {
        let frag = "void main() { fragColor = vec4(u_progress, 0.0, 0.0, 1.0); }";
        for (progress, red) in [(0.25, 64), (1.0, 255)] {
            let Some(px) = paint_shader(frag, &format!("progress = {progress}"), "", &[(32, 32)]) else { return };
            assert!(near(px[0], (red, 0, 0)), "progress {progress}: {px:?}");
        }
    }

    /// ADR-0336. `padding` is room past the box the program draws into, and nothing without it.
    #[test]
    fn effect_shader_padding_lets_the_program_draw_past_the_box() {
        let fill = "void main() { fragColor = vec4(0.0, 1.0, 0.0, 1.0); }";
        let points = [(12, 32), (32, 32), (4, 32)];
        let Some(px) = paint_shader(fill, "padding = 8", "", &points) else { return };
        assert!(near(px[0], (0, 255, 0)) && near(px[1], (0, 255, 0)), "the padding and the box: {px:?}");
        assert!(near(px[2], (255, 255, 255)), "and no further: {px:?}");
        let Some(px) = paint_shader(fill, "", "", &points) else { return };
        assert!(near(px[0], (255, 255, 255)) && near(px[1], (0, 255, 0)), "the box alone: {px:?}");
    }

    /// ADR-0335, ADR-0336. `mantle_sdf` is negative inside the outline and positive outside, at a
    /// rounded corner and at a smoothed one, which cuts more of the corner away from its diagonal.
    #[test]
    fn mantle_sdf_is_negative_inside_the_outline_and_positive_outside() {
        let frag = "void main() { fragColor = mantle_sdf(v_uv * u_size) < 0.0 ? vec4(1.0, 0.0, 0.0, 1.0) : vec4(0.0, 0.0, 0.0, 1.0); }";
        let inside = |p: (u8, u8, u8, u8)| p == (255, 0, 0, 255);
        // A 48px box at (16, 16), radius 16: (17, 17) is 1.5px in along the diagonal, (22, 22) 6.5px;
        // (17, 25) is 1.5px in from the left edge and 9.5px down, where the smoothed curve is further out.
        let points = [(17, 17), (22, 22), (17, 25), (40, 40), (14, 40), (16, 40)];
        let body = "width = 48, height = 48, radius = 16";
        let Some(px) = paint_shader(frag, "padding = 4", body, &points) else { return };
        assert_eq!(
            px.iter().copied().map(inside).collect::<Vec<_>>(),
            [false, true, true, true, false, true],
            "circular: {px:?}"
        );
        let smooth = format!("{body}, corner_smoothing = 0.6");
        let Some(px) = paint_shader(frag, "padding = 4", &smooth, &points) else { return };
        assert_eq!(
            px.iter().copied().map(inside).collect::<Vec<_>>(),
            [false, true, false, true, false, true],
            "smoothed: {px:?}"
        );
        // An outline's tail, past the box, is inside; beside it and a cut corner are not.
        let tail = format!("outline = {}", crate::layout::node::outline::TAIL);
        let Some(px) = paint_shader(frag, "padding = 8", &tail, &[(32, 51), (37, 54), (16, 16), (32, 32)]) else {
            return;
        };
        assert_eq!(px.iter().copied().map(inside).collect::<Vec<_>>(), [true, false, false, true], "outline: {px:?}");
    }

    /// ADR-0336. A shader that does not build leaves the node as painted.
    #[test]
    fn an_effect_shader_that_fails_to_build_draws_the_node_plain() {
        let Some(px) = paint_shader(
            "void main() { nonsense }",
            "padding = 8",
            r##"background = "#FF0000FF""##,
            &[(32, 32), (12, 32)],
        ) else {
            return;
        };
        assert!(near(px[0], (255, 0, 0)) && near(px[1], (255, 255, 255)), "{px:?}");
    }

    /// ADR-0336. A `params` change or a saved `.frag` repaints the layer instead of compositing the
    /// one kept from the last paint.
    #[test]
    fn a_param_change_or_a_saved_file_replaces_the_kept_layer() {
        let Some(instance) = init_headless_egl(64, 96) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 96) else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("effect.frag");
        std::fs::write(&path, "uniform float k; void main() { fragColor = vec4(k, 0.0, 0.0, 1.0); }").unwrap();
        let list = |k: u32| {
            let src = format!(
                r##"return panel {{ id = "bar", width = 64, height = 96, background = "#FFFFFFFF", padding = 16,
                    child = rect {{ width = 32, height = 32, effect = {{ shader = {{ source = "{}",
                    params = {{ k = {k} }} }} }} }} }}"##,
                path.display()
            );
            build(&resolved_surface(&Lua::new(), &src, LogicalSize { width: 64.0, height: 96.0 }), 1.0, None)
        };
        let (gl, mut stage) = (test_gl(&instance), image_shader::ShaderStage::default());
        let whole = PhysicalRect { x0: 0, y0: 0, x1: 64, y1: 96 };
        let mut paint = |painter: &mut TextPainter, list: &DisplayList| {
            let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
            let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
            let _ = execute("a", painter, images, captures, list, 1.0, (64.0, 96.0), &[whole], shaders);
            pixel_at(painter.canvas_mut(), 32, 32)
        };
        let one = list(1);
        assert!(near(paint(&mut painter, &one), (255, 0, 0)));
        let layer = one.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Layer { .. })).unwrap();
        assert!(painter.layer("a", layer).is_some(), "kept after the first paint");
        assert!(near(paint(&mut painter, &one), (255, 0, 0)), "and composited again");
        assert!(near(paint(&mut painter, &list(0)), (0, 0, 0)), "a new param repaints it");
        std::fs::write(&path, "uniform float k; void main() { fragColor = vec4(0.0, 0.0, 1.0 - k, 1.0); }").unwrap();
        assert!(near(paint(&mut painter, &list(0)), (0, 0, 255)), "so does a saved file");
    }
}

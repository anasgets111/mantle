//! Shadows, content blur and backdrop blur: the draws that read or filter pixels through pooled
//! targets.

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, CompositeOperation, ImageId, Paint, Path, RenderTarget, Solidity};
use glow::HasContext;

use crate::layout::image_shader;
use crate::layout::node::{self, Rgba};
use crate::text::atlas::TextPainter;
use crate::text::snap::{LogicalRect, PhysicalRect};

use super::super::{UNCLIPPED, any_draw_matches, grow, is_empty, shadow_rect, transformed, volatile};
use super::shape::box_path;
use super::{Draw, DrawCmd, Frame, Shaders, Walk, fill_image, flush, offscreen, scratch};

/// A round box's shadow (ADR-0254), cut out under the box when `knockout` (ADR-0260): femtovg's
/// box gradient fades from the colour to nothing across 3 sigma centred on the spread box's edge.
pub(super) fn paint_shadow(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    shadow: node::Shadow,
    own: f32,
    knockout: bool,
) {
    let LogicalRect { x, y, width, height } = shadow_rect(rect, rect, shadow);
    let Rgba { r, g, b, a } = shadow.color;
    // A ramp across 3 sigma is within 14/255 of the layer path's Gaussian; matching its slope
    // instead, 22. Floored at NanoVG's 1, since the gradient divides by it.
    let feather = (1.5 * shadow.blur).max(1.0);
    // A signed distance past half the box is positive everywhere, so the gradient would paint nothing.
    let radius = spread_radius(own, shadow.spread).min(width.min(height) / 2.0);
    let color = Color::rgbaf(r, g, b, a);
    let paint = Paint::box_gradient(x, y, width, height, radius, feather, color, Color::rgbaf(r, g, b, 0.0));
    let reach = grow(LogicalRect { x, y, width, height }, feather / 2.0);
    let path = if knockout { knocked_out(rect, own, reach) } else { box_path(reach, 0.0) };
    canvas.fill_path(&path, &paint);
}

/// CSS's corner radius of a shadow spread from a box's: a square corner stays square.
fn spread_radius(radius: f32, spread: f32) -> f32 {
    let k = if radius < spread { 1.0 + (radius / spread - 1.0).powi(3) } else { 1.0 };
    (radius + spread * k).max(0.0)
}

/// `outside` with the box cut out, which is where a box shadow draws (ADR-0260).
fn knocked_out(rect: LogicalRect, radius: f32, outside: LogicalRect) -> Path {
    let mut path = box_path(rect, radius);
    path.solidity(Solidity::Hole);
    path.rect(outside.x, outside.y, outside.width, outside.height);
    path.solidity(Solidity::Solid);
    path
}

/// A subtree under its own shadow and `content_blur` (ADR-0254). Both are blurs into
/// pooled targets, and an unchanged layer composites what it last finished (ADR-0258).
pub(super) fn draw_layer(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    command: &DrawCmd,
    target: RenderTarget,
    frame: Frame,
) {
    let Draw::Layer { effect, silhouette, commands } = &command.draw else { return };
    let (node::Effect { shadow, blur, .. }, silhouette) = (*effect, *silhouette);
    let (rect, clip) = (command.rect, command.clip);
    let size = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    let area = LogicalRect { x: clip.x0 as f32, y: clip.y0 as f32, width: size.0 as f32, height: size.1 as f32 };
    let (cast, content) = match painter.layer(walk.surface, command) {
        Some(kept) => kept,
        None => {
            // Whole: the blur and the shadow read past the repaint's edge.
            let Some(content) = offscreen(painter, walk, rect, clip, None, None, commands, target, frame, UNCLIPPED)
            else {
                return;
            };
            let cast = shadow.and_then(|shadow| cast_shadow(painter, walk, content, size, shadow, target));
            let sharp = blur * walk.scale < MIN_SIGMA;
            let blurred = blurred(painter, walk, content, size, blur * walk.scale);
            // A filter a full pool refused leaves this frame unfiltered, not every frame after.
            let filtered = shadow.is_none() == cast.is_none() && sharp == blurred.is_none();
            let content = blurred.unwrap_or(content);
            // A glass reads what is under the layer's box, which this command does not name.
            if filtered && !any_draw_matches(commands, |draw| volatile(draw) || matches!(draw, Draw::Backdrop { .. })) {
                walk.scratch.retain(|(id, _)| Some(*id) != cast && *id != content);
                painter.keep_layer(walk.surface, command, (cast, content), size);
            }
            (cast, content)
        }
    };
    let canvas = painter.canvas_mut();
    if let (Some(shadow), Some(cast)) = (shadow, cast) {
        let at = shadow_rect(rect, area, shadow);
        match commands.as_slice() {
            // The hole's part outside `at` would take the cast's clamped edge.
            [DrawCmd { draw: Draw::Box { radius, .. }, .. }] if silhouette => {
                canvas.save();
                canvas.intersect_scissor(at.x, at.y, at.width, at.height);
                let paint = Paint::image(cast, at.x, at.y, at.width, at.height, 0.0, 1.0);
                canvas.fill_path(&knocked_out(rect, *radius, at), &paint);
                canvas.restore();
            }
            _ => fill_image(canvas, cast, at, 1.0),
        }
    }
    if !silhouette {
        fill_image(canvas, content, area, 1.0);
    }
}

/// `image_shader`'s blur divides by `u_sigma`.
const MIN_SIGMA: f32 = 0.01;

fn blurred(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    source: ImageId,
    size: (usize, usize),
    sigma: f32,
) -> Option<ImageId> {
    if sigma < MIN_SIGMA {
        return None;
    }
    let image = scratch(painter, walk, size)?;
    // A power of two keeps the kernel within 4 to 8 texels at any sigma.
    let factor = 1 << (sigma / 4.0).log2().max(0.0) as u32;
    gaussian(painter, walk, source, image, size, sigma, factor).then_some(image)
}

/// Blurs `source` into `target`, both `size`, at `1 / factor` of that size: halved, blurred
/// across and down, and stretched back (ADR-0262). `false` without a GL context or a target.
fn gaussian(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    source: ImageId,
    target: ImageId,
    size: (usize, usize),
    sigma: f32,
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
        passes.push(BlurPass { source: from.0, target: image, extent, axis: [0.0; 2], sigma: 0.0 });
        from = (image, half);
    }
    let (from, low) = from;
    let Some(across) = scratch(painter, walk, low) else { return false };
    // The halvings add a `factor`-wide box's variance and the stretch a tent's.
    let f = factor as f32;
    let spread = if factor == 1 { 0.0 } else { (3.0 * f * f - 1.0) / 12.0 };
    let sigma = (sigma * sigma - spread).max(0.0).sqrt() / f;
    let down = if factor == 1 { target } else { from };
    passes.push(BlurPass { source: from, target: across, extent: [1.0; 2], axis: [1.0, 0.0], sigma });
    passes.push(BlurPass { source: across, target: down, extent: [1.0; 2], axis: [0.0, 1.0], sigma });
    if factor > 1 {
        let extent = [size.0 as f32 / (factor * low.0) as f32, size.1 as f32 / (factor * low.1) as f32];
        passes.push(BlurPass { source: down, target, extent, axis: [0.0; 2], sigma: 0.0 });
    }
    let Some(Shaders { gl, stage }) = walk.shaders.as_mut() else { return false };
    // SAFETY: `paint_surface` made this context current, and the canvas shares it.
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
    let sigma = shadow.blur / 2.0 * walk.scale;
    let blurred = blurred(painter, walk, content, size, sigma);
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
    let Rgba { r, g, b, a } = shadow.color;
    canvas.global_composite_operation(CompositeOperation::SourceIn);
    canvas.fill_path(&whole, &Paint::color(Color::rgbaf(r, g, b, a)));
    canvas.restore();
    canvas.set_render_target(target);
    Some(cast)
}

/// Blurs what the current target holds under `clip`, the 3 sigma the blur reads, into the box
/// (ADR-0256).
pub(super) fn draw_backdrop(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    clip: PhysicalRect,
    sigma: f32,
    radius: f32,
    alpha: f32,
) {
    let Some((copy, size, paint)) = read_target(painter, walk, clip) else { return };
    let blurred = blurred(painter, walk, copy, size, sigma * walk.scale).unwrap_or(copy);
    replace(painter.canvas_mut(), &box_path(rect, radius), &paint(blurred, alpha), alpha);
}

/// The current target's pixels under `area`, copied to a scratch of the returned size, and the
/// paint that lays an image of that size back where they were read, under the transform in force
/// now. `None` without a GL context: femtovg cannot read a target.
#[allow(clippy::type_complexity)]
pub(super) fn read_target(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    area: PhysicalRect,
) -> Option<(ImageId, (usize, usize), impl Fn(ImageId, f32) -> Paint + use<>)> {
    let gl = walk.shaders.as_ref()?.gl;
    let canvas = painter.canvas_mut();
    let to_target = canvas.transform();
    let whole = PhysicalRect { x0: 0, y0: 0, x1: canvas.width() as i32, y1: canvas.height() as i32 };
    let region = transformed(to_target.0, area).intersect(whole);
    if is_empty(region) {
        return None;
    }
    let size = ((region.x1 - region.x0) as usize, (region.y1 - region.y0) as usize);
    let copy = scratch(painter, walk, size)?;
    let canvas = painter.canvas_mut();
    let texture = canvas.get_native_texture(copy).ok()?;
    flush(canvas);
    // SAFETY: `paint_surface` made this context current, the flush left the target bound, and
    // femtovg's next flush rebinds every texture unit it uses.
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        // GL rows run bottom up in a target and in a `FLIP_Y` image alike.
        let (width, rows) = (size.0 as i32, size.1 as i32);
        gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, region.x0, whole.y1 - region.y1, width, rows);
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
    // ponytail: approximate where a non-uniform `scale` meets a `rotate`, whose inverse skews and
    // an image paint cannot. Upgrade path: a raw-GL quad, as the shader stage draws.
    let from_target = to_target.inverse();
    let (x, y) = from_target.transform_point(region.x0 as f32, region.y0 as f32);
    let [a, b, c, d, ..] = from_target.0;
    let angle = b.atan2(a);
    let (sin, cos) = angle.sin_cos();
    let (width, height) = (size.0 as f32 * a.hypot(b), size.1 as f32 * (d * cos - c * sin));
    Some((copy, size, move |image, alpha| Paint::image(image, x, y, width, height, angle, alpha)))
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

    /// ADR-0254's gradient path: an opaque box's shadow lands offset under it, sharp without a
    /// blur and fading over `shadow_blur` either side of its edge with one.
    #[test]
    fn an_opaque_boxs_shadow_is_painted_offset_under_it() {
        let Some(px) = paint_effect(r##"background = "#FF0000FF", shadow_offset = { y = 16 }"##) else { return };
        assert!(near(px[0], (255, 255, 255)) && near(px[1], (255, 0, 0)), "{px:?}");
        assert!(near(px[5], (0, 0, 0)), "the shadow shows below the box: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "and ends 16px below it: {px:?}");

        let Some(px) = paint_effect(r##"background = "#FF0000FF", shadow_offset = { y = 16 }, shadow_blur = 8"##)
        else {
            return;
        };
        assert!((90..170).contains(&px[6].0), "half dark at the shadow's edge: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "and gone `shadow_blur` past it: {px:?}");
    }

    /// A radius past half the box is a circle, and its shadow is that circle's, not nothing.
    #[test]
    fn a_circle_past_its_half_radius_still_casts_a_shadow() {
        let effect = r##"background = "#FF0000FF", radius = 999, shadow_offset = { y = 16 }"##;
        let Some(px) = paint_effect(effect) else { return };
        assert!(near(px[5], (0, 0, 0)), "the circle's shadow below it: {px:?}");
    }

    /// CSS: a square box's spread shadow keeps square corners.
    #[test]
    fn a_square_boxs_spread_shadow_keeps_square_corners() {
        let effect = r##"background = "#FF0000FF", shadow_offset = { y = 16 }, shadow_spread = 4"##;
        let Some(px) = paint_effect_at(effect, &[(12, 67)]) else { return };
        assert!(near(px[0], (0, 0, 0)), "the spread shadow's corner pixel: {px:?}");
    }

    /// Switching paths, as a background alpha tween reaching 1 does, keeps the shadow's softness.
    #[test]
    fn the_gradient_and_the_layer_cast_the_same_blurred_shadow() {
        let column = [58, 60, 62, 64, 66, 68, 70].map(|y| (32, y));
        let shadow = r##"shadow_offset = { y = 16 }, shadow_blur = 8"##;
        let Some(gradient) = paint_effect_at(&format!(r##"background = "#FF0000FF", {shadow}"##), &column) else {
            return;
        };
        let Some(layer) =
            paint_effect_at(&format!(r##"background = "#FF0000FE", shadow_mode = "Content", {shadow}"##), &column)
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
        let content = r##"background = "#FF000080", shadow_mode = "Content", shadow_offset = { y = 16 }"##;
        let Some(px) = paint_effect(content) else { return };
        assert!(near(px[0], (255, 255, 255)), "{px:?}");
        assert!(near(px[1], (255, 127, 127)), "the box alone: {px:?}");
        assert!(near(px[3], (191, 63, 63)), "the box over its shadow: {px:?}");
        assert!(near(px[5], (127, 127, 127)), "the shadow alone, at the box's alpha: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "{px:?}");

        let Some(px) = paint_effect(&format!("{content}, shadow_blur = 8")) else { return };
        assert!((180..205).contains(&px[6].0), "a quarter dark at the blurred shadow's edge: {px:?}");
        assert!(px[7].0 >= 250, "and gone 3 sigma past it: {px:?}");
    }

    /// ADR-0254: `content_blur` spreads the box past its edge, premultiplied: a red edge fades to
    /// pink over white, never through a dark fringe.
    #[test]
    fn a_content_blur_spreads_the_box_past_its_edge_without_darkening_it() {
        let Some(px) = paint_effect(r##"background = "#FF0000FF", content_blur = 4"##) else { return };
        assert!(near(px[2], (255, 0, 0)), "the middle stays red: {px:?}");
        assert!(px[4].1 > 30 && px[4].1 < 240, "2px past the edge is pink: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "and 3 sigma past it is white: {px:?}");
        assert!(px.iter().all(|p| p.0 >= 250), "no pixel darkens: {px:?}");
    }

    /// A mask cuts the pixels the shadow is cast from: the masked-away half casts nothing.
    #[test]
    fn a_masked_box_casts_the_shadow_of_what_its_mask_keeps() {
        let effect = r##"background = "#FF0000FF", shadow_offset = { y = 16 }, shadow_mode = "Content",
            mask = { gradient = "Linear", angle = 90,
                stops = { { 0, "#FFFFFFFF" }, { 0.5, "#FFFFFFFF" }, { 0.5, "#FFFFFF00" }, { 1, "#FFFFFF00" } } }"##;
        let Some(px) = paint_effect_at(effect, &[(20, 30), (44, 30), (20, 56), (44, 56)]) else { return };
        assert!(near(px[0], (255, 0, 0)) && near(px[1], (255, 255, 255)), "the mask keeps the left half: {px:?}");
        assert!(near(px[2], (0, 0, 0)) && near(px[3], (255, 255, 255)), "and only it casts: {px:?}");
    }

    /// ADR-0260. A translucent box's shadow stops at its edge: the body shows the ground, not the
    /// shadow, and past the edge it is the opaque box's shadow, pixel for pixel.
    #[test]
    fn a_box_shadow_is_knocked_out_under_a_translucent_box() {
        let shadow = r##"shadow_offset = { y = 16 }, shadow_blur = 8"##;
        let column: Vec<(usize, usize)> = (50..80).map(|y| (32, y)).collect();
        let Some(opaque) = paint_effect_at(&format!(r##"background = "#FF0000FF", {shadow}"##), &column) else {
            return;
        };
        let Some(glass) = paint_effect_at(&format!(r##"background = "#FF000080", {shadow}"##), &column) else { return };
        assert_eq!(glass, opaque, "the shadow outside the box");
        let Some(px) = paint_effect(r##"background = "#FF000080", shadow_offset = { y = 16 }"##) else { return };
        assert!(near(px[3], (255, 127, 127)), "the box alone over its shadow: {px:?}");
        assert!(near(px[5], (0, 0, 0)), "the shadow at full strength: {px:?}");
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
        for shadow in ["shadow_offset = { y = 100 }", "shadow_spread = -20"] {
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
                r##"{STRIPES} return panel {{ id = "bar", width = 96, height = 48, child = rect {{ width = "Fill", height = "Fill",
                    background = {{ gradient = "Linear", angle = 90, stops = stops }}, padding = 8,
                    children = {{ rect {{ width = 80, height = 32, radius = 16, backdrop_blur = 4,
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
        let Some(boxed) = paint_with_gl(&src("shadow_blur = 8, shadow_offset = { y = 4 },"), (96, 48), &body) else {
            return;
        };
        assert!(plain == boxed, "the body unchanged by its box shadow");
        let content = src(r#"shadow_blur = 8, shadow_offset = { y = 4 }, shadow_mode = "Content","#);
        let Some(cast) = paint_with_gl(&content, (96, 48), &body) else { return };
        assert!(cast != plain, "a content shadow shows through the glass");
    }

    /// ADR-0260. A scoop's box shadow is its silhouette: its own notches show the shadow under
    /// them, the shadow's notches stay clear, and the body is knocked out.
    #[test]
    fn a_scooped_box_shadow_is_its_silhouette_knocked_out() {
        let effect = r##"background = "#FF000080", radius = 12, corner_shape = "Scoop", shadow_offset = { y = 16 }"##;
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
        let blurred = list(r##"rect { width = 32, height = 32, background = "#FF0000FF", content_blur = 2 }"##);
        let red = list(r##"rect { width = 32, height = 32, background = "#FF0000FE", content_blur = 2 }"##);
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

        let textured = list(r#"image { source = "/nonexistent.png", width = 32, height = 32, content_blur = 2 }"#);
        paint(&mut painter, "a", &textured, whole);
        assert!(painter.layer("a", textured.commands.last().unwrap()).is_none());

        // A glass inside reads what is under it, which the layer's command does not name (ADR-0256).
        let glass = list(
            r#"rect { width = 32, height = 32, content_blur = 2, children = { rect { width = 16, height = 16, backdrop_blur = 2 } } }"#,
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
                source = "{}", shadow_offset = {{ y = 16 }} }} }}"##,
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
    fn a_backdrop_blur_frosts_the_stripes_under_a_pill_and_nothing_else() {
        let src = &(STRIPES.to_owned()
            + r##"return panel { id = "bar", width = 96, height = 48, child = rect { width = "Fill", height = "Fill",
                background = { gradient = "Linear", angle = 90, stops = stops }, padding = 8,
                children = { rect { width = 80, height = 32, radius = 16, backdrop_blur = 4,
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
            child = rect { width = "Fill", height = "Fill", backdrop_blur = 4, opacity = 0.5 } }"##;
        let Some(px) = paint_with_gl(src, (64, 32), &[(32, 16), (0, 0), (63, 31)]) else { return };
        assert_eq!(px, [(255, 0, 0, 255); 3]);
    }

    /// ADR-0256. A glass at a clipping parent's edge reads only inside that parent: a red header
    /// above a blue viewport does not bleed into the glass at the viewport's top.
    #[test]
    fn a_glass_reads_nothing_past_its_parents_clip() {
        let src = r##"return panel { id = "bar", width = 64, height = 48, child = column { width = "Fill", children = {
            rect { width = "Fill", height = 16, background = "#FF0000FF" },
            rect { width = "Fill", height = 32, background = "#0000FFFF",
                children = { rect { width = "Fill", height = 16, backdrop_blur = 4 } } } } } }"##;
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
                background = { gradient = "Linear", angle = 90, stops = stops },
                child = rect { width = 88, height = 40, radius = 8, clip = "Rounded", padding = 4,
                    children = { rect { width = 80, height = 32, radius = 16, backdrop_blur = 4,
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
        let opaque_mask = r##"mask = { gradient = "Linear", stops = { { 0, "#FFFFFFFF" }, { 1, "#FFFFFFFF" } } },"##;
        for (wrapper, pill) in [
            ("", ""),
            (r#"radius = 4, clip = "Rounded","#, ""),
            (opaque_mask, ""),
            ("", r##"content_blur = 1, border_width = 1, border_color = "#00FF00FF""##),
            ("translate = { x = 4, y = -4 },", ""),
            ("", "rotate = 180"),
        ] {
            let src = format!(
                r##"return panel {{ id = "bar", width = 96, height = 96, padding = 8, child = rect {{
                    width = 80, height = 64, {wrapper} children = {{ rect {{ width = "Fill", height = "Fill",
                        padding = {{ left = 8 }}, background = {{ gradient = "Linear", angle = 135, stops = {{ {{ 0, "#FF0000FF" }},
                            {{ 0.25, "#FF0000FF" }}, {{ 0.25, "#0000FFFF" }}, {{ 1, "#0000FFFF" }} }} }},
                        children = {{ rect {{ width = 64, height = 32, radius = 16, backdrop_blur = 2, {pill} }} }} }} }} }} }}"##
            );
            let points = [(26, 14), (30, 22), (26, 26), (56, 28), (13, 12)];
            let Some(px) = paint_with_gl(&src, (96, 96), &points) else { return };
            let case = format!("wrapper {{ {wrapper} }}, pill {{ {pill} }}: {px:?}");
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
        fill_rect(canvas, LogicalRect { x: 96.0, y: 96.0, width: 64.0, height: 64.0 }, 0.0, &colour(1.0, 0.0));
        for x in (96..160).step_by(8) {
            let stripe = LogicalRect { x: x as f32, y: 96.0, width: 4.0, height: 64.0 };
            fill_rect(canvas, stripe, 0.0, &colour(0.0, 1.0));
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
            let engine = blurred(&mut painter, &mut walk, source, size, sigma).unwrap();
            let engine = read(&mut painter, engine);
            let reference = scratch(&mut painter, &mut walk, size).unwrap();
            if sigma <= 8.0 {
                painter.canvas_mut().filter_image(reference, femtovg::ImageFilter::GaussianBlur { sigma }, source);
            } else {
                assert!(gaussian(&mut painter, &mut walk, source, reference, size, sigma, 1));
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
                content_blur = 16 } }"##;
        let Some(px) = paint_with_gl(src, (200, 160), &[(152, 80)]) else { return };
        assert!((210..=240).contains(&px[0].0), "{px:?}");
    }

    /// ADR-0262. A frame that blurs again allocates no texture, even when its blurs' chains
    /// outnumber the pool's sizes: a probe image created after ten frames takes the slot and
    /// version the probe before them freed, as nothing else was created in between.
    #[test]
    fn repainting_a_blur_allocates_no_texture() {
        let glass = |width: u32| format!("rect {{ width = {width}, height = 30, backdrop_blur = 32 }},");
        let src = format!(
            r##"return panel {{ id = "bar", width = 800, height = 300, padding = 110, background = "#FF0000FF",
                child = row {{ spacing = 10, children = {{ {} rect {{ width = 30, height = 30, backdrop_blur = 2 }} }} }} }}"##,
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
        painter.keep_layer("other", &small, (None, small_id), (8, 8));
        painter.recycle_scratch([]);
        painter.keep_layer("test", &big, (None, big_id), (5000, 5000));
        let retired = painter.sweep_layers("test", |_| true);
        assert_eq!(retired, [(big_id, (5000, 5000))]);
        assert!(painter.layer("other", &small).is_some());
    }
}

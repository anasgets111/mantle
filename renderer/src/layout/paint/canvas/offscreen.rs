//! Offscreen target pooling, node masking, and rounded clipping.

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, CompositeOperation, ImageFlags, ImageId, Paint, Path, PixelFormat, RenderTarget};

use crate::image::{ImageRequest, Load};
use crate::layout::node::{self, MaskSource};
use crate::text::atlas::TextPainter;
use crate::text::snap::{LogicalRect, PhysicalRect};

use super::super::{Draw, DrawCmd, UNCLIPPED, any_draw_matches, reads_under, volatile};
use super::dashed_border::{Cut, cut};
use super::effects::{read_target, replace};
use super::shape::{border_stations, box_path, gradient_paint, paint_border};
use super::vector_path::Dashes;
use super::{Frame, Walk, run};

/// Draws `commands` into an offscreen image, then fills the node's rounded path with that image.
/// On an 80x32 pill at radius 16 with a 30px child, femtovg's `intersect_rounded_scissor` re-rounded
/// the child and leaked the ground 8% through the pill's straight top edge (measured on 0.26).
/// Giving the child the pill's radius instead draws a lozenge. A `mask` multiplies the target's
/// alpha before that fill, in the same one target (ADR-0255).
/// ponytail: femtovg 0.27's scissor carries one rounded rectangle; clip with it once it intersects two.
///
/// The target comes from `TextPainter`'s pool: creating one per clipping node per repaint was 8.6 ms
/// of an 8.6 ms repaint (ADR-0217). A single solid box under a gradient mask draws directly in one
/// pass, and a stable gradient-masked subtree reuses its kept offscreen across composition-only
/// transforms (#199).
pub(super) fn draw_clipped(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    command: &DrawCmd,
    target: RenderTarget,
    frame: Frame,
) {
    let Draw::Clipped { radius, mask, commands } = &command.draw else { return };
    let (rect, clip) = (command.rect, command.clip);
    if let Some((mask, _)) = mask
        && try_draw_solid_gradient_mask(painter.canvas_mut(), rect, clip, radius, mask, commands)
    {
        return;
    }
    let used = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    if used.0 == 0 || used.1 == 0 {
        return;
    }
    let gradient_mask = matches!(mask, Some((node::Mask { source: MaskSource::Gradient(_), .. }, _)));
    let snug = !aligned(painter.canvas_mut());
    let kept =
        gradient_mask.then(|| painter.layer(walk.surface, command)).flatten().filter(|(_, clamped)| !snug || *clamped);
    let (image, seed) = match kept {
        Some(((_, image), _)) => (image, None),
        None => {
            // Not a backdrop root, as CSS's `overflow: hidden` is not: a glass or a blend inside starts from
            // what is under the group (ADR-0256).
            let glass = mask.is_none() && any_draw_matches(commands, reads_under);
            let seed = if glass { read_target(painter, walk, clip) } else { None };
            let under = seed.as_ref().map(|read| read.paint(read.copy, 1.0));
            let Some(image) = offscreen(
                painter,
                walk,
                rect,
                clip,
                mask.as_ref(),
                under,
                commands,
                target,
                frame,
                frame.region,
                true,
                snug,
            ) else {
                return;
            };
            if gradient_mask
                && frame.region.contains(clip)
                && !any_draw_matches(commands, |draw| volatile(draw) || reads_under(draw))
            {
                walk.scratch.retain(|(id, _)| *id != image);
                let clamped = painter.canvas_mut().image_size(image).ok() == Some(used);
                let retired = painter.keep_layer(walk.surface, command, (Vec::new(), image), clamped);
                walk.scratch.extend(retired);
            }
            (image, seed)
        }
    };
    let path = box_path(rect, radius);
    let at = LogicalRect { x: clip.x0 as f32, y: clip.y0 as f32, width: used.0 as f32, height: used.1 as f32 };
    let paint = pooled_paint(painter.canvas_mut(), image, used, at, 1.0);
    match seed {
        Some(_) => replace(painter.canvas_mut(), &path, &paint, 1.0),
        None => painter.canvas_mut().fill_path(&path, &paint),
    }
}

/// Draws a single borderless solid box under a gradient mask directly in one pass: constant RGB
/// multiplied by a gradient's alpha is the same gradient with each stop's alpha scaled by the
/// box's, matching premultiplied `DestinationIn`/`DestinationOut` without an offscreen target.
fn try_draw_solid_gradient_mask(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    clip: PhysicalRect,
    radius: &node::Radii,
    mask: &node::Mask,
    commands: &[DrawCmd],
) -> bool {
    let MaskSource::Gradient(gradient) = &mask.source else { return false };
    let [inner] = commands else { return false };
    if inner.rect != rect || inner.clip != clip {
        return false;
    }
    let Draw::Box { background, radius: box_radius, widths, .. } = &inner.draw else { return false };
    // An unclipped `shape` outline can reach past `rect`, where `draw_clipped`'s composite cuts it to `rect`.
    if *widths != node::EdgeInsets::default() || (radius != box_radius && !(radius.is_zero() && box_radius.2.is_none()))
    {
        return false;
    }
    let [node::Fill::Color(color)] = background.as_slice() else { return false };
    if rect.is_empty() {
        return true;
    }
    let stops = gradient
        .stops
        .iter()
        .map(|&(at, stop)| {
            let alpha = if mask.invert { 1.0 - stop.a } else { stop.a };
            (at, node::Rgba { a: color.a * alpha, ..*color })
        })
        .collect();
    let masked = node::Gradient { shape: gradient.shape, stops };
    canvas.fill_path(&box_path(rect, box_radius), &gradient_paint(&masked, rect));
    true
}

/// Both lists use the ordinary executor, so transforms, shaders, nested masks and effects share
/// their existing implementation. The alpha texture covers the whole clip, including empty pixels.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_node_mask(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    clip: PhysicalRect,
    invert: bool,
    radius: &node::Radii,
    split: usize,
    commands: &[DrawCmd],
    target: RenderTarget,
    frame: Frame,
) {
    let (alpha, content) = commands.split_at(split);
    let snug = !aligned(painter.canvas_mut());
    let Some(alpha) = offscreen(painter, walk, rect, clip, None, None, alpha, target, frame, UNCLIPPED, false, snug)
    else {
        return;
    };
    let Some(content) =
        offscreen(painter, walk, rect, clip, None, None, content, target, frame, UNCLIPPED, false, snug)
    else {
        return;
    };
    let used = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    let (width, height) = (used.0 as f32, used.1 as f32);
    let canvas = painter.canvas_mut();
    let cut = pooled_paint(canvas, alpha, used, LogicalRect { x: 0.0, y: 0.0, width, height }, 1.0);
    draw_into(canvas, content, None);
    canvas.global_composite_operation(if invert {
        CompositeOperation::DestinationOut
    } else {
        CompositeOperation::DestinationIn
    });
    let mut whole = Path::new();
    whole.rect(0.0, 0.0, width, height);
    canvas.fill_path(&whole, &cut.with_anti_alias(false));
    canvas.restore();
    canvas.set_render_target(target);
    let at = LogicalRect { x: clip.x0 as f32, y: clip.y0 as f32, width, height };
    canvas.fill_path(&box_path(rect, radius), &pooled_paint(canvas, content, used, at, 1.0));
}

/// A dashed border: the solid border drawn offscreen, less a mask of the slices of its band between
/// `dash`'s dashes. The dashes keep the solid border's corners, widths, colours and silhouette.
/// ponytail: two scratch targets per dashed border, held until the frame flushes and pooled for a
/// frame that repeats (ten 4K-sized borders near 640 MiB); femtovg draws at the flush, so borders
/// can't share a pair without one between them. Upgrade: stroke directly when width and paint are
/// uniform.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_dashed_border(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    clip: PhysicalRect,
    radius: &node::Radii,
    border: &node::BorderPaint,
    widths: node::EdgeInsets,
    dash: &Dashes,
    target: RenderTarget,
) {
    let slices = match cut(&border_stations(rect, radius, border, widths), dash) {
        Cut::Nothing => return,
        Cut::Solid => return paint_border(painter.canvas_mut(), rect, radius, border, widths),
        Cut::Slices(slices) => slices,
    };
    let used = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    if used.0 == 0 || used.1 == 0 {
        return;
    }
    let snug = !aligned(painter.canvas_mut());
    let (Some(mask), Some(image)) = (scratch(painter, walk, used, snug), scratch(painter, walk, used, snug)) else {
        return paint_border(painter.canvas_mut(), rect, radius, border, widths);
    };
    let canvas = painter.canvas_mut();
    let at = LogicalRect { x: clip.x0 as f32, y: clip.y0 as f32, width: used.0 as f32, height: used.1 as f32 };
    let mut whole = Path::new();
    whole.rect(at.x, at.y, at.width, at.height);
    // Added, not layered, so where two pieces meet their edges' coverage sums to the whole pixel.
    draw_into(canvas, mask, Some(used));
    canvas.translate(-at.x, -at.y);
    canvas.global_composite_operation(CompositeOperation::Lighter);
    let opaque = Paint::color(Color::white());
    for slice in &slices {
        canvas.fill_path(slice, &opaque);
    }
    canvas.restore();
    draw_into(canvas, image, Some(used));
    canvas.translate(-at.x, -at.y);
    paint_border(canvas, rect, radius, border, widths);
    canvas.global_composite_operation(CompositeOperation::DestinationOut);
    let gaps = pooled_paint(canvas, mask, used, at, 1.0);
    canvas.fill_path(&whole, &gaps.with_anti_alias(false));
    canvas.restore();
    canvas.set_render_target(target);
    let paint = pooled_paint(canvas, image, used, at, 1.0);
    canvas.fill_path(&whole, &paint);
}

/// A pooled render target whose top-left `used` part the caller draws in, held until [`super::execute`]
/// flushes; `None` when out of texture memory or past the context's largest texture. Each side
/// rounds up to 64 px, or stays `used` when `snug`, for an image femtovg will sample off its texel
/// centres: past the used part it holds whatever was there, where `CLAMP_TO_EDGE` would repeat the
/// edge (ADR-0370).
pub(super) fn scratch(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    used: (usize, usize),
    snug: bool,
) -> Option<ImageId> {
    let size = if snug { used } else { (used.0.next_multiple_of(64), used.1.next_multiple_of(64)) };
    if size.0.max(size.1) > painter.max_texture() {
        return None;
    }
    // `PREMULTIPLIED` prevents a second alpha multiplication; `FLIP_Y` maps canvas y=0 to the last
    // GL texture row. Both match femtovg 0.27's drop-shadow flags (`src/lib.rs`).
    let flags = ImageFlags::PREMULTIPLIED | ImageFlags::FLIP_Y;
    let image = match painter.take_scratch(walk.surface, size) {
        Some(image) => image,
        None => painter.canvas_mut().create_image_empty(size.0, size.1, PixelFormat::Rgba8, flags).ok()?,
    };
    walk.scratch.push((image, size));
    Some(image)
}

/// Returns `image` to the pool mid-frame once nothing queued in femtovg still reads or writes it.
pub(super) fn release_scratch(painter: &mut TextPainter, walk: &mut Walk<'_, '_>, image: ImageId) {
    if let Some(at) = walk.scratch.iter().position(|(id, _)| *id == image) {
        let (id, size) = walk.scratch.swap_remove(at);
        painter.return_scratch(walk.surface, id, size);
    }
}

/// Saves the canvas and points it at `image` with no transform or scissor, cleared to transparent
/// over its top-left `clear` when given. The caller restores.
pub(super) fn draw_into(canvas: &mut Canvas<OpenGl>, image: ImageId, clear: Option<(usize, usize)>) {
    canvas.save();
    canvas.set_global_alpha(1.0);
    canvas.set_render_target(RenderTarget::Image(image));
    canvas.reset_transform();
    canvas.reset_scissor();
    if let Some((width, height)) = clear {
        canvas.clear_rect(0, 0, width as u32, height as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
    }
}

/// Whether femtovg's image paints placed on whole pixels land texel on pixel under the canvas
/// transform, so they never sample a pooled image's slack (ADR-0370).
pub(super) fn aligned(canvas: &Canvas<OpenGl>) -> bool {
    let [a, b, c, d, e, f] = canvas.transform().0;
    [a, b, c, d] == [1.0, 0.0, 0.0, 1.0] && e.fract() == 0.0 && f.fract() == 0.0
}

/// A paint laying the `used` top-left of the pooled `image` on `at`: its extent is the whole
/// texture's (ADR-0370).
pub(super) fn pooled_paint(
    canvas: &Canvas<OpenGl>,
    image: ImageId,
    used: (usize, usize),
    at: LogicalRect,
    alpha: f32,
) -> Paint {
    let (width, height) = canvas.image_size(image).unwrap_or(used);
    // Divided first, so a used part the texture's size stays exact.
    let (width, height) = (at.width / used.0 as f32 * width as f32, at.height / used.1 as f32 * height as f32);
    Paint::image(image, at.x, at.y, width, height, 0.0, alpha)
}

/// `fill_image` for the `used` top-left of a pooled `image`, at the origin and its used size.
pub(super) fn fill_pooled(canvas: &mut Canvas<OpenGl>, image: ImageId, used: (usize, usize)) {
    let at = LogicalRect { x: 0.0, y: 0.0, width: used.0 as f32, height: used.1 as f32 };
    canvas.fill_path(&box_path(at, &node::Radii::default()), &pooled_paint(canvas, image, used, at, 1.0));
}

/// Draws `commands` into a scratch target covering `clip` over `under`, masked over the node's box
/// `rect`, returned for the caller to composite at `clip`. `None` when there is nothing to
/// composite. `region` is the part of the commands to draw, and cuts nothing when it is `UNCLIPPED`.
/// If no scratch image can be had, `unmasked_fallback` draws `commands` straight into `target`.
#[allow(clippy::too_many_arguments)]
pub(super) fn offscreen(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    clip: PhysicalRect,
    mask: Option<&(node::Mask, (u32, u32))>,
    under: Option<Paint>,
    commands: &[DrawCmd],
    target: RenderTarget,
    frame: Frame,
    region: PhysicalRect,
    unmasked_fallback: bool,
    snug: bool,
) -> Option<ImageId> {
    let (width, height) = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    // A box with no area shows nothing, and asking for a 0xN render target leaves GL with an
    // incomplete framebuffer that the next composite on this canvas paints as a full square. A
    // cell tweening its width through zero hits this on its first and last frame.
    if width == 0 || height == 0 {
        return None;
    }
    let Some(image) = scratch(painter, walk, (width, height), snug) else {
        // A mask source never falls back into its parent; clips and effects keep the subtree
        // unmasked, in the parent's target and frame.
        if unmasked_fallback {
            run(painter, walk, commands, target, frame);
        }
        return None;
    };

    let canvas = painter.canvas_mut();
    let size = canvas.image_size(image).unwrap_or((width, height));
    canvas.save();
    // A fallback's fade (`draw_layer`) is for the parent's target, not this one.
    canvas.set_global_alpha(1.0);
    canvas.set_render_target(RenderTarget::Image(image));
    canvas.clear_rect(0, 0, width as u32, height as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
    // Set, rather than accumulate, the translation; nested clips otherwise sum offsets. Inner
    // scissors transform with it, so absolute command coordinates need no extra math.
    canvas.reset_transform();
    canvas.translate(-clip.x0 as f32, -clip.y0 as f32);
    let mut whole = Path::new();
    whole.rect(clip.x0 as f32, clip.y0 as f32, width as f32, height as f32);
    if let Some(under) = under {
        canvas.reset_scissor();
        canvas.fill_path(&whole, &under.with_anti_alias(false));
    }
    // The offscreen's whole texture (a shader quad inside a rounded clip places itself in that target,
    // ADR-0184), its origin at the clip's corner, and no transform: `draw_clipped` reset the canvas
    // transform above, and composites the result under the outer one afterwards.
    let inner = Frame {
        size: (size.0 as f32, size.1 as f32),
        origin: (clip.x0 as f32, clip.y0 as f32),
        transform: None,
        region,
        fade: 1.0,
    };
    run(painter, walk, commands, RenderTarget::Image(image), inner);

    if let Some(mask) = mask {
        apply_mask(painter, walk, rect, &whole, mask);
    }

    let canvas = painter.canvas_mut();
    canvas.restore();
    canvas.set_render_target(target);
    Some(image)
}

/// Multiplies the current target's alpha by `mask` over `rect`, which keeps it premultiplied, and so
/// masks a shader quad drawn into it too. `whole` covers the target: a pixel it misses keeps its alpha.
pub(super) fn apply_mask(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    whole: &Path,
    (mask, box_px): &(node::Mask, (u32, u32)),
) {
    let paint = match &mask.source {
        MaskSource::Gradient(gradient) => Some(gradient_paint(gradient, rect)),
        MaskSource::Node(_) => None, // Lowered to Draw::NodeMask by build_node.
        // A missing mask image leaves the subtree unmasked, the answer an allocation failure
        // in `offscreen` gets too.
        MaskSource::Image(file) => {
            let request = ImageRequest::mask(std::path::Path::new(file), *box_px);
            walk.images
                .image(painter.canvas_mut(), &request, Load::Inline)
                .map(|id| Paint::image(id, rect.x, rect.y, rect.width, rect.height, 0.0, 1.0))
        }
    };
    if let Some(paint) = paint {
        let canvas = painter.canvas_mut();
        canvas.reset_scissor();
        canvas.global_composite_operation(match mask.invert {
            false => CompositeOperation::DestinationIn,
            true => CompositeOperation::DestinationOut,
        });
        canvas.fill_path(whole, &paint.with_anti_alias(false));
    }
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    use super::super::tests::{
        assert_partial_repaint_matches, init_headless_egl, paint_points, pixel_at, surface_96x64, test_gl, text_painter,
    };
    use super::super::{Shaders, execute, paint_tree};
    use super::{Draw, DrawCmd};
    use crate::image::ImageCache;
    use crate::image::capture::CaptureCache;
    use crate::layout::image_shader;
    use crate::layout::paint::tests::resolved_surface;
    use crate::layout::scene::LogicalSize;
    use crate::text::shaping::ShapingHandle;
    use crate::text::snap::PhysicalRect;

    #[test]
    fn a_fractional_rounded_clip_uses_the_physical_corner() {
        let Some(instance) = init_headless_egl(96, 96) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 96) else { return };
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64,
                child = rect { width = 32, height = 32, radius = 12, clip = "rounded",
                    children = { rect { width = 32, height = 32, background = "#FF0000FF" } } } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.5);
        assert_eq!(pixel_at(painter.canvas_mut(), 1, 1).3, 0);
        assert_eq!(pixel_at(painter.canvas_mut(), 24, 24), (255, 0, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 55, 24).3, 0);
    }

    /// A mask multiplies alpha, so the node's own fill and its child fade together: an opaque stop
    /// keeps them, a clear one leaves the untouched ground.
    #[test]
    fn a_gradient_mask_fades_the_nodes_fill_and_subtree_and_invert_flips_it() {
        let masked = |invert: bool| {
            format!(
                r##"rect {{ width = "fill", height = "fill", background = "#FFFFFFFF",
                    mask = {{ gradient = "linear", invert = {invert},
                        stops = {{ {{ 0, "#FFFFFFFF" }}, {{ 0.5, "#FFFFFFFF" }}, {{ 0.5, "#FFFFFF00" }}, {{ 1, "#FFFFFF00" }} }} }},
                    children = {{ rect {{ width = 8, height = "fill", background = "#FF0000FF" }} }} }}"##
            )
        };
        let points = [(32, 8), (32, 56), (4, 8), (4, 56)];
        let Some(kept) = paint_points(&masked(false), &points) else { return };
        assert_eq!(kept, [(255, 255, 255, 255), (0, 0, 0, 0), (255, 0, 0, 255), (0, 0, 0, 0)]);
        let Some(inverted) = paint_points(&masked(true), &points) else { return };
        assert_eq!(inverted, [(0, 0, 0, 0), (255, 255, 255, 255), (0, 0, 0, 0), (255, 0, 0, 255)]);
    }

    /// A layer past the context's largest texture paints its subtree unblurred, still cut to its
    /// parent's box: femtovg 0.27 would fail the texture silently and draw the subtree unclipped.
    #[test]
    fn a_layer_past_the_texture_limit_paints_unfiltered_inside_its_parent() {
        let src = r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF", padding = 8,
            child = rect { width = 64, height = 64, clip = "box", children = {
                rect { width = 96, height = 64, background = "#0000FFFF", effect = { blur = 4 },
                    children = { rect { width = 32, height = 64, background = "#FFFFFFFF" } } } } } }"##;
        let paint = |src: &str, cap: usize, points: &[(usize, usize)]| {
            let instance = init_headless_egl(96, 96)?;
            let shaping = ShapingHandle::spawn();
            let mut painter = text_painter(&instance, &shaping, 96, 96)?;
            painter.cap_texture(cap);
            let root = resolved_surface(&Lua::new(), src, LogicalSize { width: 96.0, height: 96.0 });
            let gl = test_gl(&instance);
            let mut stage = image_shader::ShaderStage::default();
            let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
            let whole = PhysicalRect { x0: 0, y0: 0, x1: 96, y1: 96 };
            let (images, captures) = (&mut ImageCache::inline(), &mut CaptureCache::default());
            let list = super::super::build(&root, 1.0, None);
            let _ = execute("test", &mut painter, images, captures, &list, 1.0, (96.0, 96.0), &[whole], shaders);
            Some(points.iter().map(|&(x, y)| pixel_at(painter.canvas_mut(), x, y)).collect::<Vec<_>>())
        };
        // The layer is the parent's 64 px box: a cap of 64 holds it, 32 does not.
        for cap in [64, 32] {
            let Some(px) = paint(src, cap, &[(38, 40), (60, 40), (80, 40)]) else { return };
            assert_eq!(
                px[0] == (255, 255, 255, 255),
                cap == 32,
                "cap {cap}: the white child's edge is sharp unblurred"
            );
            assert_eq!(px[1], (0, 0, 255, 255), "cap {cap}: the subtree paints");
            assert_eq!(px[2], (255, 0, 0, 255), "cap {cap}: cut to the parent at x = 72");
        }
        // #117. A faded shader layer that falls back still fades, and the rounded clip inside it,
        // which a cap of 64 still holds, fades once.
        let faded = r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF", padding = 8,
            child = rect { width = 80, height = 80, opacity = 0.5, effect = { shader = { source = "/s.frag" } },
                children = { rect { width = 32, height = 32, radius = 4, clip = "rounded",
                    children = { rect { width = 32, height = 32, background = "#0000FFFF" } } } } } }"##;
        for cap in [128, 64] {
            let Some(px) = paint(faded, cap, &[(24, 24)]) else { return };
            let (r, g, b, _) = px[0];
            assert!(r.abs_diff(128) <= 3 && g == 0 && b.abs_diff(128) <= 3, "cap {cap}: {px:?}");
        }
        // #147. Nested faded layers that both fall back multiply their fades: blue at 0.25 over red.
        let nested = r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF", padding = 8,
            child = rect { width = 80, height = 80, opacity = 0.5, effect = { shader = { source = "/s.frag" } },
                children = { rect { width = 72, height = 72, opacity = 0.5, effect = { shader = { source = "/s.frag" } },
                    children = { rect { width = 32, height = 32, background = "#0000FFFF" } } } } } }"##;
        for cap in [128, 64] {
            let Some(px) = paint(nested, cap, &[(24, 24)]) else { return };
            let (r, g, b, _) = px[0];
            assert!(r.abs_diff(191) <= 3 && g == 0 && b.abs_diff(64) <= 3, "cap {cap}: {px:?}");
        }
        // A target allocated inside a fallback starts unfaded: at cap 96 the rotated middle layer
        // allocates snug, and the aligned one inside it rounds to 128 and falls back again.
        let reset = r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF", padding = 8,
            child = rect { width = 80, height = 80, opacity = 0.5, effect = { shader = { source = "/s.frag" } },
                children = { rect { width = 72, height = 72, rotate = 1, effect = { shader = { source = "/s.frag" } },
                    children = { rect { width = 72, height = 72, effect = { shader = { source = "/s.frag" } },
                        children = { rect { width = 32, height = 32, background = "#0000FFFF" } } } } } } } }"##;
        for cap in [128, 96] {
            let Some(px) = paint(reset, cap, &[(24, 24)]) else { return };
            let (r, g, b, _) = px[0];
            assert!(r.abs_diff(128) <= 3 && g == 0 && b.abs_diff(128) <= 3, "cap {cap}: {px:?}");
        }
    }

    /// An SVG mask is how a config cuts a subtree to a shape no `radius` draws.
    #[test]
    fn an_image_mask_keeps_only_what_its_alpha_covers() {
        let dir = tempfile::tempdir().unwrap();
        let svg = dir.path().join("left-half.svg");
        std::fs::write(
            &svg,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="32" height="64" fill="black"/></svg>"#,
        )
        .unwrap();
        let child = format!(
            r##"rect {{ width = "fill", height = "fill", background = "#00FF00FF", mask = {{ source = "{}" }} }}"##,
            svg.display()
        );
        let Some(px) = paint_points(&child, &[(8, 32), (56, 32)]) else { return };
        assert_eq!(px, [(0, 255, 0, 255), (0, 0, 0, 0)]);
    }

    /// A pill with a child filling its left third. Without the rounded clip that child is a
    /// square-cornered block poking out of the left cap, and giving it the pill's radius draws a
    /// lozenge instead.
    #[test]
    fn a_rounded_clip_cuts_a_child_by_the_parents_arc() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16, clip = "rounded",
                    children = { rect { width = 30, height = "fill", background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        // The pill spans (8, 8) to (88, 40) with radius 16, so its corner arcs are centred at
        // (24, 24) and the child covers x in [8, 38).
        let canvas = painter.canvas_mut();
        assert_eq!(
            pixel_at(canvas, 10, 10),
            (255, 0, 0, 255),
            "the child's own top-left corner is outside the pill's arc, so the panel shows through"
        );
        assert_eq!(pixel_at(canvas, 10, 24), (0, 0, 255, 255), "at the pill's waist the child reaches its edge");
        assert_eq!(
            pixel_at(canvas, 30, 10),
            (0, 0, 255, 255),
            "past the arc the pill's top edge is straight, so nothing may round the child there"
        );
        assert_eq!(pixel_at(canvas, 50, 24), (255, 0, 0, 255), "the child ends at x = 38 and nothing extends it");
    }

    /// The default costs nothing: no `clip` means square corners.
    #[test]
    fn without_the_property_a_radius_still_clips_square() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16,
                    children = { rect { width = 30, height = "fill", background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        assert_eq!(pixel_at(painter.canvas_mut(), 10, 10), (0, 0, 255, 255));
    }

    /// A child that would escape the parent entirely is still bound by the rectangle, so the
    /// rounded pass narrows the clip rather than replacing it.
    #[test]
    fn a_rounded_clip_still_holds_a_child_to_the_parents_box() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 40, height = 32, radius = 16, clip = "rounded",
                    children = { rect { width = 90, height = 90, background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 60, 24), (255, 0, 0, 255), "the pill ends at x = 48");
        assert_eq!(pixel_at(canvas, 24, 44), (255, 0, 0, 255), "and at y = 40");
        assert_eq!(pixel_at(canvas, 24, 24), (0, 0, 255, 255));
    }

    /// One rounded clip inside another. The inner pass has to put the render target back to its
    /// parent's image rather than to the screen, and this is the assertion that catches it: were
    /// the inner subtree sent to the framebuffer it would paint unmasked and outside the outer arc.
    #[test]
    fn a_rounded_clip_nests_inside_another_one() {
        let Some(instance) = init_headless_egl(96, 96) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 96) else { return };

        // A 64x64 circle at (16, 16), holding a 64x64 child that is itself a rounded clip holding a
        // square block covering the whole box. Both arcs have to survive.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF",
                padding = { top = 16, left = 16 }, child = rect {
                    width = 64, height = 64, radius = 32, clip = "rounded",
                    children = { rect {
                        width = 64, height = 64, radius = 32, clip = "rounded",
                        children = { rect { width = 64, height = 64, background = "#0000FFFF" } },
                    } },
                } }"##,
            LogicalSize { width: 96.0, height: 96.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 48, 48), (0, 0, 255, 255), "the middle of the disc");
        assert_eq!(pixel_at(canvas, 20, 20), (255, 0, 0, 255), "the corner of the box is outside the disc");
    }

    /// A translucent child blends with what is behind it once, not twice. The offscreen pass is
    /// where this can go wrong: femtovg stores premultiplied results in a render target, and
    /// compositing without `PREMULTIPLIED` multiplies the alpha in a second time.
    #[test]
    fn a_translucent_child_under_a_rounded_clip_blends_once() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        // Blue at 50% over the panel's red: one blend is 128 of each, two would be 64 blue.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16, clip = "rounded",
                    children = { rect { width = 30, height = "fill", background = "#0000FF80" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let (r, _, b, _) = pixel_at(painter.canvas_mut(), 20, 24);
        assert!((126..=129).contains(&r) && (126..=129).contains(&b), "blended to ({r}, _, {b}, _), expected ~128");
    }

    /// The border paints over the clipped subtree. A fill
    /// reaching the arc otherwise covers the border exactly where the arc is, which is the half of
    /// a pill's outline most worth seeing.
    #[test]
    fn a_rounded_clips_border_paints_over_its_children() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16, clip = "rounded",
                    border_width = 4, border_color = "#00FF00FF",
                    children = { rect { width = 30, height = "fill", background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        assert_eq!(
            pixel_at(painter.canvas_mut(), 10, 24),
            (0, 255, 0, 255),
            "the left cap's border is where the child reaches, so it has to be on top"
        );
    }

    /// The group's own clip can be narrower than the node's box, when an ancestor cuts it. The
    /// offscreen image is sized to that clip while the mask path is drawn on the full box, so this
    /// is where the two coordinate systems have to agree.
    #[test]
    fn a_rounded_clip_hanging_off_its_parent_still_lands_where_it_belongs() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        // A 64-wide pill starting 40px into a 60px-wide parent, so its right 44px are cut away by
        // the parent's box and the group's clip runs (48, 8) to (108, 40) intersected to x < 68.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 60, height = 32, clip = "box", children = { rect {
                        margin = { left = 40 }, width = 64, height = 32, radius = 16, clip = "rounded",
                        children = { rect { width = 64, height = "fill", background = "#0000FFFF" } },
                    } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 60, 24), (0, 0, 255, 255), "inside the pill and inside the parent");
        assert_eq!(pixel_at(canvas, 50, 10), (255, 0, 0, 255), "the pill's left cap still rounds");
        assert_eq!(pixel_at(canvas, 72, 24), (255, 0, 0, 255), "and the parent's box still ends at x = 68");
    }

    /// ADR-0256. femtovg opens a flush on the program its last one ended on without setting that
    /// program's view, which an offscreen of another size left at its own: a gradient ground
    /// drifted between frames. A glass in the offscreen blurs without a flush of its own (#196) and
    /// must leave the next one opening on the same switch.
    #[test]
    fn a_gradient_over_an_offscreen_paints_the_same_every_frame() {
        for glass in ["", r#"rect { width = 20, height = 20, effect = { backdrop = { blur = 2 } } },"#] {
            let src = format!(
                r##"return panel {{ id = "bar", width = 96, height = 64, padding = 16,
                background = {{ gradient = "linear", angle = 90, stops = {{ {{ 0, "#FF0000FF" }}, {{ 1, "#0000FFFF" }} }} }},
                child = rect {{ width = 40, height = 20, radius = 10, clip = "rounded", children = {{
                    rect {{ width = 40, height = 10,
                        background = {{ gradient = "linear", stops = {{ {{ 0, "#00FF00FF" }}, {{ 1, "#000000FF" }} }} }} }},
                    rect {{ width = 40, height = 10, background = "#FFFFFFFF" }}, {glass} }} }} }}"##
            );
            let Some(instance) = init_headless_egl(96, 64) else { return };
            let shaping = ShapingHandle::spawn();
            let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
            let gl = test_gl(&instance);
            let mut stage = image_shader::ShaderStage::default();
            let list = surface_96x64(&src);
            let whole = PhysicalRect { x0: 0, y0: 0, x1: 96, y1: 64 };
            let mut frames = Vec::new();
            for _ in 0..2 {
                let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
                let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
                let _ = execute("test", &mut painter, images, captures, &list, 1.0, (96.0, 64.0), &[whole], shaders);
                frames.push(painter.canvas_mut().screenshot().expect("screenshot reads back the pbuffer"));
            }
            let worst =
                frames[0].buf().iter().zip(frames[1].buf()).map(|(a, b)| a.r.abs_diff(b.r).max(a.a.abs_diff(b.a)));
            assert!(worst.max().unwrap() <= 2, "the second frame drifts from the first: {glass}");
        }
    }

    #[test]
    fn node_mask_alpha_invert_hidden_and_nested_transform() {
        // Alpha 254 at opacity 0.5 is 127.0; 255 lands on the 127.5 tie GL may round either way.
        let child = |extra: &str, invert: bool| {
            format!(
                r##"rect {{ width = 64, height = 64,
            opacity = 0.5, background = "#00FF00FE", mask = {{ node = "shape", invert = {invert} }},
            children = {{ rect {{ id = "shape", width = 32, height = 64,
                background = "#FF0000FF", {extra} }} }} }}"##
            )
        };
        let Some(px) = paint_points(&child("", false), &[(8, 32), (48, 32)]) else { return };
        assert_eq!(px, [(0, 127, 0, 127), (0, 0, 0, 0)], "mask colour does not paint and parent opacity applies once");
        let Some(px) = paint_points(&child("", true), &[(8, 32), (48, 32)]) else { return };
        assert_eq!(px, [(0, 0, 0, 0), (0, 127, 0, 127)]);
        for invert in [false, true] {
            let Some(px) = paint_points(&child("visible = false,", invert), &[(8, 32), (48, 32)]) else { return };
            assert_eq!(px, if invert { vec![(0, 127, 0, 127); 2] } else { vec![(0, 0, 0, 0); 2] });
        }
        let nested = r##"opacity = 0.5, translate = { x = 16 }, effect = { blur = 1 },
            mask = { node = "inner" }, children = {
                rect { id = "inner", width = 16, height = 64, background = "#FFFFFF" }
            },"##;
        let Some(px) = paint_points(&child(nested, false), &[(4, 32), (24, 32), (48, 32)]) else { return };
        assert_eq!(px[0], (0, 0, 0, 0));
        assert!((62..=65).contains(&px[1].3), "both opacities multiply: {px:?}");
        assert_eq!(px[2], (0, 0, 0, 0));
    }

    #[test]
    fn node_mask_reuses_two_targets_and_partial_damage_keeps_rounded_transform() {
        let src = |width| {
            format!(
                r##"return panel {{ id = "bar", width = 96, height = 64,
            background = "#FFFFFF", child = rect {{ width = 48, height = 48,
                translate = {{ x = 12, y = 4 }}, radius = 16, clip = "rounded", background = "#00FF00",
                mask = {{ node = "shape" }}, children = {{ rect {{ id = "shape", width = {width}, height = 48,
                    background = "#FFFFFF" }} }} }} }}"##
            )
        };
        let (before, after) = (surface_96x64(&src(24)), surface_96x64(&src(40)));
        let damage = after.damage_since(&before, false);
        let regions: Vec<_> = damage.into_iter().map(|r| after.repaint_region(r)).collect();
        assert_partial_repaint_matches(&before, &after, &regions);
        let Some(instance) = init_headless_egl(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        let root = resolved_surface(&Lua::new(), &src(40), LogicalSize { width: 96.0, height: 64.0 });
        let mut images = ImageCache::new();
        paint_tree(&mut painter, &mut images, &root, 1.0);
        let first = painter.take_scratch("test", (64, 64)).unwrap();
        let second = painter.take_scratch("test", (64, 64)).unwrap();
        assert!(painter.take_scratch("test", (64, 64)).is_none(), "exactly two offscreens even with rounded clipping");
        painter.recycle_scratch("test", [(first, (64, 64)), (second, (64, 64))]);
        paint_tree(&mut painter, &mut images, &root, 1.0);
        let reused = [painter.take_scratch("test", (64, 64)).unwrap(), painter.take_scratch("test", (64, 64)).unwrap()];
        assert!(reused.contains(&first) && reused.contains(&second));
    }

    /// ADR-0370. A rounded clip tweening its width within one size class draws every frame into
    /// the one pooled target, and paints as it did in an exact one; under a `rotate` its target is
    /// snug, so femtovg sampling between texels never reaches a slack.
    #[test]
    fn a_tweening_clip_reuses_one_pooled_target() {
        let src = |width: u32, angle: f32| {
            format!(
                r##"return panel {{ id = "bar", width = 96, height = 64, background = "#FFFFFF",
                child = rect {{ width = {width}, height = 40, radius = 8, clip = "rounded", rotate = {angle},
                    children = {{ rect {{ width = 96, height = 40, background = "#00FF00" }} }} }} }}"##
            )
        };
        let Some(instance) = init_headless_egl(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        let (mut images, lua, size) = (ImageCache::new(), Lua::new(), LogicalSize { width: 96.0, height: 64.0 });
        for width in [41, 47] {
            let root = resolved_surface(&lua, &src(width, 0.0), size);
            paint_tree(&mut painter, &mut images, &root, 1.0);
            let canvas = painter.canvas_mut();
            let (edge, past) = (pixel_at(canvas, width as usize - 1, 20), pixel_at(canvas, width as usize, 20));
            assert_eq!((edge, past), ((0, 255, 0, 255), (255, 255, 255, 255)), "width {width}");
            assert_eq!(painter.census().0, 1, "width {width}: one pooled target");
        }
        assert!(painter.take_scratch("test", (64, 64)).is_some());
        paint_tree(&mut painter, &mut images, &resolved_surface(&lua, &src(41, 5.0), size), 1.0);
        assert!(painter.take_scratch("test", (41, 40)).is_some(), "snug under a rotation");
    }

    /// #199. A single borderless solid box under a gradient mask draws directly in one pass with
    /// zero offscreen targets, matching the offscreen path on premultiplied midpoints, inversion,
    /// clipping, rotation, HiDPI scale, and parent opacity, while falling back for borders and
    /// unclipped out-of-box shapes.
    #[test]
    fn a_solid_box_gradient_mask_draws_directly_without_an_offscreen_target() {
        let Some(instance) = init_headless_egl(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        let (mut images, lua, size) = (ImageCache::new(), Lua::new(), LogicalSize { width: 96.0, height: 64.0 });

        // Compare direct fast-path pixels against a forced-offscreen reference (same fill + invisible child)
        // across linear and radial masks, normal and inverted, with semi-transparent fill over blue.
        for (shape, invert) in [("linear", false), ("linear", true), ("radial", false), ("radial", true)] {
            let angle = if shape == "linear" { "angle = 90, " } else { "" };
            let make = |extra: &str| {
                format!(
                    r##"return panel {{ id = "bar", width = 96, height = 64, background = "#0000FFFF",
                        child = rect {{ width = 64, height = 64, radius = 8, background = "#FF000080",
                            mask = {{ gradient = "{shape}", {angle}invert = {invert},
                                stops = {{ {{ 0, "#FFFFFFFF" }}, {{ 1, "#FFFFFF00" }} }} }}{extra} }} }}"##
                )
            };
            paint_tree(&mut painter, &mut images, &resolved_surface(&lua, &make(""), size), 1.0);
            assert_eq!(painter.census(), (0, 0, 0, 0), "{shape} invert={invert}: zero offscreen targets");
            let direct = painter.canvas_mut().screenshot().expect("direct screenshot");

            let forced = make(r##", children = { rect { width = 1, height = 1, background = "#00000000" } }"##);
            paint_tree(&mut painter, &mut images, &resolved_surface(&lua, &forced, size), 1.0);
            let offscreen = painter.canvas_mut().screenshot().expect("offscreen screenshot");
            // Interior samples (endpoints + midpoint) match the offscreen path within 2 LSB.
            for (x, y) in [(4, 32), (32, 32), (60, 32)] {
                let a = direct.buf()[y * 96 + x];
                let b = offscreen.buf()[y * 96 + x];
                let diff = a.r.abs_diff(b.r).max(a.g.abs_diff(b.g)).max(a.b.abs_diff(b.b)).max(a.a.abs_diff(b.a));
                assert!(diff <= 2, "{shape} invert={invert} at ({x},{y}): direct={a:?} offscreen={b:?}");
            }
            painter.release_surface("test");
        }

        // Clipped by parent box and rotated: zero offscreen targets, parent clip still cuts.
        // CSS angle 0 runs bottom-to-top: stop 0 (opaque) is at the bottom, stop 1 (clear) at the top.
        let clipped_rot = r##"return panel { id = "bar", width = 96, height = 64, background = "#0000FFFF",
            child = rect { width = 48, height = 64, clip = "box", children = {
                rect { width = 64, height = 48, radius = 6, rotate = 12, background = "#FF0000FF",
                    mask = { gradient = "linear", angle = 0, stops = { { 0, "#FFFFFFFF" }, { 1, "#FFFFFF00" } } } }
            } } }"##;
        paint_tree(&mut painter, &mut images, &resolved_surface(&lua, clipped_rot, size), 1.0);
        assert_eq!(painter.census(), (0, 0, 0, 0), "rotated clipped solid masked box skips offscreen");
        assert_eq!(pixel_at(painter.canvas_mut(), 60, 24), (0, 0, 255, 255), "parent clip still cuts past x=48");
        assert!(pixel_at(painter.canvas_mut(), 24, 40).0 > 180, "bottom of rotated box stays red");
        assert!(pixel_at(painter.canvas_mut(), 24, 8).2 > 180, "top of rotated box fades to blue");

        // Bordered box falls back to offscreen (and keeps a layer).
        let bordered = r##"return panel { id = "bar", width = 96, height = 64, background = "#000000FF",
            child = rect { width = 48, height = 48, background = "#FF0000FF", border_width = 2, border_color = "#FFFFFFFF",
                mask = { gradient = "linear", angle = 90, stops = { { 0, "#FFFFFFFF" }, { 1, "#FFFFFF00" } } } } }"##;
        paint_tree(&mut painter, &mut images, &resolved_surface(&lua, bordered, size), 1.0);
        assert_eq!(painter.census().2, 1, "bordered masked box uses cached offscreen path");
        painter.release_surface("test");

        // Unclipped `shape` reaching outside `rect` (TAIL) falls back so `draw_clipped` still cuts to `rect`.
        let tail = crate::layout::node::shape::TAIL;
        let protruding = format!(
            r##"return panel {{ id = "bar", width = 96, height = 64, background = "#0000FFFF",
                child = rect {{ width = 32, height = 32, margin = {{ left = 16, top = 16 }}, background = "#FF0000FF",
                    shape = {tail},
                    mask = {{ gradient = "linear", angle = 90, stops = {{ {{ 0, "#FFFFFFFF" }}, {{ 1, "#FFFFFFFF" }} }} }} }} }}"##
        );
        paint_tree(&mut painter, &mut images, &resolved_surface(&lua, &protruding, size), 1.0);
        assert_eq!(painter.census().2, 1, "out-of-box shape falls back to offscreen");
        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (255, 0, 0, 255), "inside rect paints red");
        assert_eq!(pixel_at(painter.canvas_mut(), 32, 51), (0, 0, 255, 255), "tail past rect is clipped");
        painter.release_surface("test");

        // HiDPI 1.5x rounded-clipped solid box with gradient mask.
        let Some(hidpi_instance) = init_headless_egl(96, 96) else { return };
        let Some(mut hidpi) = text_painter(&hidpi_instance, &shaping, 96, 96) else { return };
        let hidpi_src = r##"return panel { id = "bar", width = 64, height = 64, background = "#0000FFFF",
            child = rect { width = 64, height = 64, radius = 12, clip = "rounded", background = "#00FF00FF",
                mask = { gradient = "linear", angle = 180, stops = { { 0, "#FFFFFFFF" }, { 1, "#FFFFFF00" } } } } }"##;
        paint_tree(
            &mut hidpi,
            &mut images,
            &resolved_surface(&lua, hidpi_src, LogicalSize { width: 64.0, height: 64.0 }),
            1.5,
        );
        assert_eq!(hidpi.census(), (0, 0, 0, 0), "HiDPI solid masked box skips offscreen");
        assert_eq!(pixel_at(hidpi.canvas_mut(), 1, 1), (0, 0, 255, 255), "rounded corner stays blue");
        assert!(pixel_at(hidpi.canvas_mut(), 48, 8).1 > 200, "top edge is green");
        assert!(pixel_at(hidpi.canvas_mut(), 48, 88).2 > 200, "bottom edge fades to blue");
    }

    /// #199. A stable gradient-masked subtree reuses its kept offscreen across composition-only
    /// transforms, and redraws when a child moves or the mask changes; image masks stay uncached.
    #[test]
    fn a_stable_gradient_masked_subtree_reuses_its_kept_layer_and_invalidates_on_changes() {
        let Some(instance) = init_headless_egl(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        let (mut images, lua, size) = (ImageCache::new(), Lua::new(), LogicalSize { width: 96.0, height: 64.0 });

        let card = |angle: f32, child_x: u32, invert: bool| {
            format!(
                r##"return panel {{ id = "bar", width = 96, height = 64, background = "#000000FF",
                    child = rect {{ width = 48, height = 48, margin = {{ left = 24, top = 8 }}, rotate = {angle},
                        background = "#0000FFFF",
                        mask = {{ gradient = "linear", angle = 90, invert = {invert},
                            stops = {{ {{ 0, "#FFFFFFFF" }}, {{ 0.5, "#FFFFFFFF" }}, {{ 0.5, "#FFFFFF00" }}, {{ 1, "#FFFFFF00" }} }} }},
                        children = {{ rect {{ width = 16, height = 48, margin = {{ left = {child_x} }}, background = "#FF0000FF" }} }} }} }}"##
            )
        };
        let clipped_cmd = |list: &super::super::DisplayList| -> DrawCmd {
            fn find(cmds: &[DrawCmd]) -> Option<DrawCmd> {
                for cmd in cmds {
                    if matches!(cmd.draw, Draw::Clipped { .. }) {
                        return Some(cmd.clone());
                    }
                    if let Some(nested) = cmd.draw.nested()
                        && let Some(found) = find(nested)
                    {
                        return Some(found);
                    }
                }
                None
            }
            find(&list.commands).expect("Draw::Clipped in card list")
        };

        // Frame 1 (rotated 4 deg): renders once snug and keeps 1 layer with zero pooled scratch.
        let root1 = resolved_surface(&lua, &card(4.0, 0, false), size);
        let cmd1 = clipped_cmd(&super::super::build(&root1, 1.0, None));
        paint_tree(&mut painter, &mut images, &root1, 1.0);
        let kept1 = painter.layer("test", &cmd1).expect("layer kept on frame 1").1;
        assert_eq!((painter.census().0, painter.census().2), (0, 1));
        assert!(pixel_at(painter.canvas_mut(), 32, 32).0 > 200, "red child visible on left half");
        assert_eq!(pixel_at(painter.canvas_mut(), 60, 32), (0, 0, 0, 255), "right half masked out");

        // Frame 2 (rotated 25 deg, same subtree and mask): exact same ImageId reused, zero pool growth.
        let root2 = resolved_surface(&lua, &card(25.0, 0, false), size);
        paint_tree(&mut painter, &mut images, &root2, 1.0);
        let kept2 = painter.layer("test", &cmd1).expect("same layer reused on frame 2").1;
        assert_eq!(kept2, kept1, "composition-only rotation reuses the kept masked image");
        assert_eq!((painter.census().0, painter.census().2), (0, 1));
        assert!(pixel_at(painter.canvas_mut(), 32, 32).0 > 200, "rotated cached subtree still paints red child");

        // Frame 3 (child moves under stable mask): old layer retired and new image painted.
        let root3 = resolved_surface(&lua, &card(0.0, 16, false), size);
        let cmd3 = clipped_cmd(&super::super::build(&root3, 1.0, None));
        paint_tree(&mut painter, &mut images, &root3, 1.0);
        assert!(painter.layer("test", &cmd1).is_none(), "stale layer evicted");
        assert!(painter.layer("test", &cmd3).is_some(), "new layer kept");
        assert_eq!(pixel_at(painter.canvas_mut(), 30, 32), (0, 0, 255, 255), "blue background exposed at x=30");
        assert_eq!(pixel_at(painter.canvas_mut(), 44, 32), (255, 0, 0, 255), "moved red child now at x=44");

        // Frame 4 (mask inverts): cache misses and redraws with flipped mask.
        let root4 = resolved_surface(&lua, &card(0.0, 0, true), size);
        paint_tree(&mut painter, &mut images, &root4, 1.0);
        assert_eq!(pixel_at(painter.canvas_mut(), 30, 32), (0, 0, 0, 255), "left half masked out when inverted");
        assert_eq!(pixel_at(painter.canvas_mut(), 60, 32), (0, 0, 255, 255), "right half kept when inverted");

        // Image masks stay uncached (outside #199's scope).
        let dir = tempfile::tempdir().unwrap();
        let svg = dir.path().join("half.svg");
        std::fs::write(
            &svg,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><rect width="24" height="48" fill="black"/></svg>"#,
        )
        .unwrap();
        let img_src = format!(
            r##"return panel {{ id = "bar", width = 96, height = 64, background = "#000000FF",
                child = rect {{ width = 48, height = 48, background = "#00FF00FF", mask = {{ source = "{}" }} }} }}"##,
            svg.display()
        );
        paint_tree(&mut painter, &mut images, &resolved_surface(&lua, &img_src, size), 1.0);
        assert_eq!(painter.census().2, 0, "image masks are not kept as cached layers");
    }
}

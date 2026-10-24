//! Offscreen target pooling, node masking, and rounded clipping.

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, CompositeOperation, ImageFlags, ImageId, Paint, Path, PixelFormat, RenderTarget};

use crate::image::{ImageRequest, Load};
use crate::layout::node::{self, MaskSource};
use crate::text::atlas::TextPainter;
use crate::text::snap::{LogicalRect, PhysicalRect};

use super::super::{DrawCmd, UNCLIPPED, any_draw_matches, reads_under};
use super::effects::{read_target, replace};
use super::shape::{box_path, gradient_paint};
use super::{Frame, Walk, run};

/// Draws `commands` into an offscreen image, then fills the node's rounded path with that image.
/// On an 80x32 pill at radius 16 with a 30px child, femtovg's `intersect_rounded_scissor` re-rounded
/// the child and leaked the ground 8% through the pill's straight top edge (measured on 0.26).
/// Giving the child the pill's radius instead draws a lozenge. A `mask` multiplies the target's
/// alpha before that fill, in the same one target (ADR-0255).
/// ponytail: femtovg 0.27's scissor carries one rounded rectangle; clip with it once it intersects two.
///
/// The target comes from `TextPainter`'s pool: creating one per clipping node per repaint was 8.6 ms
/// of an 8.6 ms repaint (ADR-0217).
// ponytail: keep the nine scalar/context arguments; passing `DrawCmd` would require a second match.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_clipped(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    clip: PhysicalRect,
    radius: &node::Radii,
    mask: Option<&(node::Mask, (u32, u32))>,
    commands: &[DrawCmd],
    target: RenderTarget,
    frame: Frame,
) {
    // Not a backdrop root, as CSS's `overflow: hidden` is not: a glass or a blend inside starts from
    // what is under the group (ADR-0256).
    let glass = mask.is_none() && any_draw_matches(commands, reads_under);
    let seed = if glass { read_target(painter, walk, clip) } else { None };
    let under = seed.as_ref().map(|read| read.paint(read.copy, 1.0));
    let Some(image) = offscreen(painter, walk, rect, clip, mask, under, commands, target, frame, frame.region, true)
    else {
        return;
    };
    let path = box_path(rect, radius);
    let used = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    let at = LogicalRect { x: clip.x0 as f32, y: clip.y0 as f32, width: used.0 as f32, height: used.1 as f32 };
    let paint = pooled_paint(painter.canvas_mut(), image, used, at, 1.0);
    match seed {
        Some(_) => replace(painter.canvas_mut(), &path, &paint, 1.0),
        None => painter.canvas_mut().fill_path(&path, &paint),
    }
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
    let Some(alpha) = offscreen(painter, walk, rect, clip, None, None, alpha, target, frame, UNCLIPPED, false) else {
        return;
    };
    let Some(content) = offscreen(painter, walk, rect, clip, None, None, content, target, frame, UNCLIPPED, false)
    else {
        return;
    };
    let used = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    let (width, height) = (used.0 as f32, used.1 as f32);
    let canvas = painter.canvas_mut();
    let cut = pooled_paint(canvas, alpha, used, LogicalRect { x: 0.0, y: 0.0, width, height }, 1.0);
    canvas.save();
    canvas.set_render_target(RenderTarget::Image(content));
    canvas.reset_transform();
    canvas.reset_scissor();
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

/// A pooled render target whose top-left `used` part the caller draws in, held until [`super::execute`]
/// flushes; `None` when out of texture memory. Each side rounds up to 64 px, or stays `used` when
/// `snug`, for an image femtovg will sample off its texel centres: past the used part it holds
/// whatever was there, where `CLAMP_TO_EDGE` would repeat the edge (ADR-0370).
pub(super) fn scratch(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    used: (usize, usize),
    snug: bool,
) -> Option<ImageId> {
    let size = if snug { used } else { (used.0.next_multiple_of(64), used.1.next_multiple_of(64)) };
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
) -> Option<ImageId> {
    let (width, height) = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    // A box with no area shows nothing, and asking for a 0xN render target leaves GL with an
    // incomplete framebuffer that the next composite on this canvas paints as a full square. A
    // cell tweening its width through zero hits this on its first and last frame.
    if width == 0 || height == 0 {
        return None;
    }
    // Composited at whole pixels under the transform in force now.
    let snug = !aligned(painter.canvas_mut());
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
    /// drifted between frames.
    #[test]
    fn a_gradient_over_an_offscreen_paints_the_same_every_frame() {
        let src = r##"return panel { id = "bar", width = 96, height = 64, padding = 16,
            background = { gradient = "linear", angle = 90, stops = { { 0, "#FF0000FF" }, { 1, "#0000FFFF" } } },
            child = rect { width = 40, height = 20, radius = 10, clip = "rounded", children = {
                rect { width = 40, height = 10,
                    background = { gradient = "linear", stops = { { 0, "#00FF00FF" }, { 1, "#000000FF" } } } },
                rect { width = 40, height = 10, background = "#FFFFFFFF" } } } }"##;
        let Some(instance) = init_headless_egl(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        let gl = test_gl(&instance);
        let mut stage = image_shader::ShaderStage::default();
        let list = surface_96x64(src);
        let whole = PhysicalRect { x0: 0, y0: 0, x1: 96, y1: 64 };
        let mut frames = Vec::new();
        for _ in 0..2 {
            let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
            let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
            let _ = execute("test", &mut painter, images, captures, &list, 1.0, (96.0, 64.0), &[whole], shaders);
            frames.push(painter.canvas_mut().screenshot().expect("screenshot reads back the pbuffer"));
        }
        let worst = frames[0].buf().iter().zip(frames[1].buf()).map(|(a, b)| a.r.abs_diff(b.r).max(a.a.abs_diff(b.a)));
        assert!(worst.max().unwrap() <= 2, "the second frame drifts from the first");
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
}

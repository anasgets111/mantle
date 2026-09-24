//! Sends a built [`DisplayList`] to femtovg: boxes, borders, text, icons, images, rounded clips and
//! transforms.

use std::f32::consts::{FRAC_PI_2, PI};
use std::time::{Duration, Instant};

use femtovg::renderer::OpenGl;
use femtovg::{
    Canvas, Color, CompositeOperation, ImageFilter, ImageFlags, ImageId, Paint, Path, PixelFormat, RenderTarget,
    Solidity,
};
use glow::HasContext;

use crate::image::capture::CaptureCache;
use crate::image::{self, Fit, ImageCache, Load};
use crate::layout::image_shader;
use crate::layout::node::{self, BorderColor, EdgeInsets, Fill, Gradient, GradientShape, MaskSource, Rgba};
use crate::layout::scene::NodeId;
#[cfg(test)]
use crate::layout::scene::ResolvedNode;
use crate::text::atlas::{TextDraw, TextPainter};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_border_band};

#[cfg(test)]
use super::build;
use super::{DisplayList, Draw, DrawCmd};

/// Timing breakdown of what [`execute`] drew.
#[derive(Clone, Copy, Default, Debug)]
pub struct PaintSplit {
    pub text: Duration,
    pub icons: Duration,
    pub boxes: Duration,
    pub flush: Duration,
}

/// Test helper that paints a tree through [`build`] and [`execute`] at the caller's scale.
/// The production caller is `wayland::App::paint_surface`.
#[cfg(test)]
pub fn paint_tree(painter: &mut TextPainter, images: &mut ImageCache, root: &ResolvedNode, scale: f32) {
    let canvas = painter.canvas_mut();
    let whole = PhysicalRect { x0: 0, y0: 0, x1: canvas.width() as i32, y1: canvas.height() as i32 };
    // No GL context reaches this harness, so a config shader falls back to the dissolve. A fresh
    // `CaptureCache` is fine here too: no test builds a tree with pixels already staged for one.
    let mut captures = CaptureCache::default();
    let size = (whole.x1 as f32, whole.y1 as f32);
    let _ = execute("test", painter, images, &mut captures, &build(root, scale, None), scale, size, &[whole], None);
}

/// One `image` node's source that this paint had a texture for. `layout::scene` moves the node onto
/// it, which is how a `retain` cover ends and a `transition` begins (ADR-0183).
///
/// Reported from the draw rather than inferred from `ImageCache::poll`, because only the draw asks
/// the cache with the node's exact key: a path-keyed cue fires for a failed decode, never fires for
/// an already-cached source, and cannot tell a thumbnail's landing from the full-size image's.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawnImage {
    pub node: NodeId,
    pub source: String,
}

/// The GL context and the compiled config shaders, for the runs that need them (ADR-0184). Absent
/// wherever there is no context to draw with -- a test harness, a paint before a surface has bound
/// one -- and every cross then falls back to the built-in dissolve.
pub struct Shaders<'a> {
    pub gl: &'a glow::Context,
    pub stage: &'a mut image_shader::ShaderStage,
}

/// Everything a paint walk carries besides the commands and the target it is drawing into: the
/// cache it asks for textures, what it has produced so far, and the shaders it may use. Bundled
/// because these are one context threaded whole through a recursion, and passing them apart made
/// `run` and `draw_clipped` a wall of positional arguments.
struct Walk<'a, 'g> {
    images: &'a mut ImageCache,
    captures: &'a mut CaptureCache,
    scale: f32,
    /// Offscreen targets, with their sizes, held until [`execute`] flushes and returns them to
    /// `TextPainter`'s pool.
    scratch: Vec<(ImageId, (usize, usize))>,
    drawn: Vec<DrawnImage>,
    shaders: Option<Shaders<'g>>,
    split: PaintSplit,
    /// Whose kept layers this walk reads and sweeps (ADR-0258).
    surface: &'a str,
}

/// The framebuffer a walk is drawing into and the transform in force there. Both are the screen's
/// until `draw_clipped` opens an offscreen or `Draw::Transformed` sets a matrix, and both are
/// things a shader quad needs that femtovg's own draws get from the canvas (ADR-0184).
#[derive(Clone, Copy)]
struct Frame {
    /// Size of the target, and where its top-left sits in surface coordinates.
    size: (f32, f32),
    origin: (f32, f32),
    transform: Option<node::Affine>,
    /// What this paint redraws, in surface pixels (ADR-0258); nothing outside it is touched.
    region: PhysicalRect,
}

/// Executes an already-built list; keeping canvas work separate makes the list comparable and
/// [`build`](super::build) EGL-free. Clears and redraws `regions` alone, each grown by
/// [`DisplayList::repaint_region`] and cut to the target here.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    surface: &str,
    painter: &mut TextPainter,
    images: &mut ImageCache,
    captures: &mut CaptureCache,
    list: &DisplayList,
    scale: f32,
    target_size: (f32, f32),
    regions: &[PhysicalRect],
    shaders: Option<Shaders<'_>>,
) -> (Vec<DrawnImage>, PaintSplit) {
    // Before recording draws, after the previous flush: evicted textures cannot be queued draws.
    images.release_evicted(painter.canvas_mut());
    // Upload before any draw names the texture.
    images.upload_landed(painter.canvas_mut());
    captures.upload_landed(painter.canvas_mut());
    let (scratch, drawn, split) = (Vec::new(), Vec::new(), PaintSplit::default());
    let mut walk = Walk { images, captures, scale, scratch, drawn, shaders, split, surface };
    let (width, height) = target_size;
    for region in regions {
        let region = region.intersect(PhysicalRect { x0: 0, y0: 0, x1: width as i32, y1: height as i32 });
        if super::is_empty(region) {
            continue;
        }
        let PhysicalRect { x0, y0, x1, y1 } = region;
        // femtovg's stencil fills and strokes assume the buffer starts at zero, and a reused back
        // buffer's stencil is undefined.
        let (x, y, w, h) = (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
        painter.canvas_mut().clear_rect(x, y, w, h, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
        let frame = Frame { size: target_size, origin: (0.0, 0.0), transform: None, region };
        run(painter, &mut walk, &list.commands, RenderTarget::Screen, frame);
    }
    painter.canvas_mut().reset_scissor();
    let timing = crate::layout::scene::timing_on();
    let t_flush = timing.then(Instant::now);
    flush(painter.canvas_mut());
    if let Some(t_flush) = t_flush {
        walk.split.flush += t_flush.elapsed();
    }
    // Recycle scratch targets only after flush; femtovg still executes queued calls at flush, as
    // `release_shadow_images` does for drop-shadow targets.
    let retired = painter.sweep_layers(surface, |kept| holds(&list.commands, kept));
    painter.recycle_scratch(walk.scratch.drain(..).chain(retired));
    (walk.drawn, walk.split)
}

/// Whether `layer` is in `commands`, at any depth.
fn holds(commands: &[DrawCmd], layer: &DrawCmd) -> bool {
    commands.iter().any(|command| {
        command == layer
            || match &command.draw {
                Draw::Clipped { commands, .. } | Draw::Transformed { commands, .. } | Draw::Layer { commands, .. } => {
                    holds(commands, layer)
                }
                _ => false,
            }
    })
}

/// Flushes, then queues a fill so the next flush opens on a program switch (ADR-0256). Zero area,
/// so it writes no pixel, in or out of a partial update's damage.
/// ponytail: works around femtovg 0.27 opening each flush on program 0 without setting its view,
/// stale after any offscreen of another size; drop it for `canvas.flush()` once upstream sets the
/// view on reuse.
pub fn flush(canvas: &mut Canvas<OpenGl>) {
    canvas.flush();
    canvas.save();
    canvas.reset();
    let mut nothing = Path::new();
    nothing.rect(0.0, 0.0, 0.0, 0.0);
    canvas.fill_path(&nothing, &Paint::color(Color::rgbaf(0.0, 0.0, 0.0, 0.0)).with_anti_alias(false));
    canvas.restore();
}

/// Runs commands against `target`, recursively restoring parent images for nested clips. `scratch`
/// holds offscreen images until [`execute`] flushes.
fn run(painter: &mut TextPainter, walk: &mut Walk<'_, '_>, commands: &[DrawCmd], target: RenderTarget, frame: Frame) {
    let scale = walk.scale;
    let timing = crate::layout::scene::timing_on();
    let mut current_clip: Option<PhysicalRect> = None;
    for command in commands {
        // `command.clip` already contains every ancestor intersection, so set the final scissor.
        let clip = command.clip;
        let scissor = clip.intersect(frame.region);
        // A transform's clip is untransformed; `repaint_region` took its drawn bounds whole or not at all.
        let bounds = if let Draw::Transformed { .. } = command.draw { super::command_bounds(command) } else { scissor };
        if super::is_empty(bounds.intersect(frame.region)) {
            continue;
        }
        if current_clip != Some(scissor) {
            painter.canvas_mut().scissor(
                scissor.x0 as f32,
                scissor.y0 as f32,
                (scissor.x1 - scissor.x0) as f32,
                (scissor.y1 - scissor.y0) as f32,
            );
            current_clip = Some(scissor);
        }
        let rect = command.rect;
        match &command.draw {
            Draw::Box { background, radius, colors, widths } => {
                let t0 = timing.then(Instant::now);
                // `None` skips the fill; alpha 0 remains an explicit transparent rect.
                if let Some(fill) = background {
                    fill_rect(painter.canvas_mut(), rect, *radius, fill);
                }
                paint_border(painter.canvas_mut(), rect, *radius, *colors, *widths, scale);
                if let Some(t0) = t0 {
                    walk.split.boxes += t0.elapsed();
                }
            }
            Draw::Text { content, runs, font_size, font, color, align, centered, caret, caret_on } => {
                let t0 = timing.then(Instant::now);
                let mut rect = rect;
                if *centered {
                    rect.y += ((rect.height - crate::text::shaping::line_height(*font_size)) / 2.0).max(0.0);
                }
                painter.draw_text(
                    TextDraw {
                        text: content,
                        runs,
                        font_size: *font_size,
                        font: font.as_ref(),
                        color: *color,
                        align: *align,
                        caret: *caret,
                        caret_on: *caret_on,
                    },
                    rect,
                    scale,
                );
                if let Some(t0) = t0 {
                    walk.split.text += t0.elapsed();
                }
            }
            Draw::Icon { name, px, alpha, color } => {
                let t0 = timing.then(Instant::now);
                // `freedesktop-icons` uses `u16`; themes have no directory above 512.
                if let Some(path) = image::icons::resolve(name, (*px).min(512) as u16) {
                    let draw = FileDraw {
                        fit: Fit::Contain,
                        rect,
                        box_px: (*px, *px),
                        alpha: *alpha,
                        tint: *color,
                        load: Load::Inline,
                        blur_px: 0,
                    };
                    let _ = draw_file(painter.canvas_mut(), walk.images, &path, draw);
                }
                if let Some(t0) = t0 {
                    walk.split.icons += t0.elapsed();
                }
            }
            Draw::Image { node, source, fit, box_px, alpha, load, retained, dissolve, shader, blur_px } => {
                let draw = FileDraw {
                    fit: *fit,
                    rect,
                    box_px: *box_px,
                    alpha: *alpha,
                    tint: None,
                    load: *load,
                    blur_px: *blur_px,
                };
                let under = retained.as_deref().map(std::path::Path::new);
                match dissolve {
                    Some(progress) => {
                        // Asked for whether or not a pixel of it is visible yet: the answer is
                        // about the texture, and it is what starts the run (ADR-0183).
                        let to = file_texture(painter.canvas_mut(), walk.images, std::path::Path::new(source), draw);
                        if to.is_some() {
                            walk.drawn.push(DrawnImage { node: *node, source: source.clone() });
                        }
                        let from = under.and_then(|under| file_texture(painter.canvas_mut(), walk.images, under, draw));

                        // The stage takes the whole cross, both endpoints at once, which is the
                        // only way an effect can be anything but a fade (ADR-0184) -- and, with no
                        // effect named, the only way a fade composes exactly (ADR-0186). It needs
                        // both textures and a context; without either, the two draws below take
                        // the frame, which is why they were built first and why they stay.
                        let crossed = match (walk.shaders.as_mut(), from, to) {
                            (Some(shaders), Some((from, from_rect)), Some((to, to_rect))) => {
                                let params = shader.as_ref().map_or(&[][..], |(_, params)| params.as_slice());
                                let run = image_shader::Run {
                                    cross: Some(image_shader::Cross { from, to, from_rect, to_rect }),
                                    rect,
                                    transform: frame.transform,
                                    clip: scissor,
                                    target_size: frame.size,
                                    target_origin: frame.origin,
                                    opacity: *alpha,
                                    progress: *progress,
                                    params,
                                };
                                let effect = shader.as_ref().map(|(path, _)| path.as_path());
                                // SAFETY: `paint_surface` made this context current before calling
                                // `execute`, and it is the one every GL object here belongs to.
                                unsafe { shaders.stage.draw(shaders.gl, painter.canvas_mut(), effect, &run) }
                            }
                            _ => false,
                        };
                        // Last resort, and an approximation: the outgoing at its own full alpha
                        // with the incoming fading in over it. Exact for opaque endpoints at full
                        // opacity, and wrong otherwise -- at `alpha` 0.5 and `progress` 0.5 these
                        // two draws compose to 0.625 where 0.5 is right, showing the surface's
                        // ground through the middle (ADR-0181, corrected by ADR-0186).
                        //
                        // Reached when there is no GL context, when either endpoint has no texture
                        // yet, or when even the engine's own shader would not build. The first is
                        // the test harness; the rest are real and are why this stays.
                        if !crossed {
                            if let Some((id, fitted)) = from {
                                fill_image(painter.canvas_mut(), id, fitted, *alpha);
                            }
                            if let Some((id, fitted)) = to {
                                fill_image(painter.canvas_mut(), id, fitted, *alpha * *progress);
                            }
                        }
                    }
                    // The named source has no texture: still decoding, or a failure the cache has
                    // already logged once. Either way the node keeps its last picture rather than
                    // showing the surface behind it (ADR-0180). A cover that is itself gone --
                    // evicted despite the pin, or deleted from disk -- draws nothing.
                    None => {
                        if draw_file(painter.canvas_mut(), walk.images, std::path::Path::new(source), draw) {
                            walk.drawn.push(DrawnImage { node: *node, source: source.clone() });
                        } else if let Some(under) = under {
                            draw_file(painter.canvas_mut(), walk.images, under, draw);
                        }
                    }
                }
            }
            // `wayland::capture` staged this node's pixels, if any landed; `execute`'s
            // `captures.upload_landed` above already put them on the GPU this turn. Nothing yet
            // (unknown output, or no frame has arrived) draws nothing, matching `image`'s empty
            // `source`.
            Draw::Capture { node, fit, alpha, .. } => {
                if let Some((id, width, height)) = walk.captures.get(*node) {
                    let fitted = image::fitted_rect(rect, width as f32, height as f32, *fit);
                    fill_image(painter.canvas_mut(), id, fitted, *alpha);
                }
            }
            Draw::Shader { source, progress, params, alpha, .. } => {
                if let Some(shaders) = walk.shaders.as_mut() {
                    let run = image_shader::Run {
                        cross: None,
                        rect,
                        transform: frame.transform,
                        clip: scissor,
                        target_size: frame.size,
                        target_origin: frame.origin,
                        opacity: *alpha,
                        progress: *progress,
                        params,
                    };
                    // SAFETY: as for `Draw::Image` above.
                    unsafe { shaders.stage.draw(shaders.gl, painter.canvas_mut(), Some(source), &run) };
                }
            }
            Draw::Clipped { radius, mask, commands } => {
                draw_clipped(painter, walk, rect, clip, *radius, mask.as_ref(), commands, target, frame);
                current_clip = None;
            }
            Draw::Transformed { matrix, commands } => {
                let canvas = painter.canvas_mut();
                canvas.save();
                canvas.set_transform(&femtovg::Transform2D(*matrix));
                let inner = Frame { transform: Some(*matrix), region: super::UNCLIPPED, ..frame };
                run(painter, walk, commands, target, inner);
                painter.canvas_mut().restore();
                current_clip = None;
            }
            Draw::Shadow { shadow, radius } => paint_shadow(painter.canvas_mut(), rect, *shadow, *radius),
            Draw::Layer { effect, commands } => {
                draw_layer(painter, walk, command, *effect, commands, target, frame);
                current_clip = None;
            }
            Draw::Backdrop { sigma, radius, alpha } => {
                draw_backdrop(painter, walk, rect, clip, *sigma, *radius, *alpha)
            }
        }
    }
}

/// Draws `commands` into an offscreen image, then fills the node's rounded path with that image.
/// femtovg 0.26's `intersect_rounded_scissor` carries one rounded rectangle; on an 80x32 pill at
/// radius 16 with a 30px child it re-rounded the child and leaked the ground 8% through the pill's
/// straight top edge. Giving the child the pill's radius instead draws a lozenge. A `mask`
/// multiplies the target's alpha before that fill, in the same one target (ADR-0255).
///
/// The target comes from `TextPainter`'s pool: creating one per clipping node per repaint was 8.6 ms
/// of an 8.6 ms repaint (ADR-0217).
///
/// ponytail: the pool matches on exact size, so a clip tweening its width reuses none. Upgrade path
/// is a size class and a sub-rect composite, handling `FLIP_Y` about the target's height.
// ponytail: keep the nine scalar/context arguments; passing `DrawCmd` would require a second match.
#[allow(clippy::too_many_arguments)]
fn draw_clipped(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    clip: PhysicalRect,
    radius: f32,
    mask: Option<&(node::Mask, (u32, u32))>,
    commands: &[DrawCmd],
    target: RenderTarget,
    frame: Frame,
) {
    // Not a backdrop root, as CSS's `overflow: hidden` is not: a glass inside starts from what is
    // under the group (ADR-0256).
    let glass = mask.is_none() && super::any_draw_matches(commands, |draw| matches!(draw, Draw::Backdrop { .. }));
    let seed = if glass { read_target(painter, walk, clip) } else { None };
    let under = seed.as_ref().map(|(copy, _, paint)| paint(*copy, 1.0));
    let Some(image) = offscreen(painter, walk, rect, clip, mask, under, commands, target, frame, frame.region) else {
        return;
    };
    let path = box_path(rect, radius);
    let (width, height) = ((clip.x1 - clip.x0) as f32, (clip.y1 - clip.y0) as f32);
    let paint = Paint::image(image, clip.x0 as f32, clip.y0 as f32, width, height, 0.0, 1.0);
    match seed {
        Some(_) => replace(painter.canvas_mut(), &path, &paint, 1.0),
        None => painter.canvas_mut().fill_path(&path, &paint),
    }
}

/// A pooled render target of `size`, held until [`execute`] flushes; `None` when out of texture
/// memory.
fn scratch(painter: &mut TextPainter, walk: &mut Walk<'_, '_>, size: (usize, usize)) -> Option<ImageId> {
    // `PREMULTIPLIED` prevents a second alpha multiplication; `FLIP_Y` maps canvas y=0 to the last
    // GL texture row. Both match femtovg 0.27's drop-shadow flags (`src/lib.rs`).
    let flags = ImageFlags::PREMULTIPLIED | ImageFlags::FLIP_Y;
    let image = match painter.take_scratch(size) {
        Some(image) => image,
        None => painter.canvas_mut().create_image_empty(size.0, size.1, PixelFormat::Rgba8, flags).ok()?,
    };
    walk.scratch.push((image, size));
    Some(image)
}

/// Draws `commands` into a scratch target covering `clip` over `under`, masked over the node's box
/// `rect`, returned for the caller to composite at `clip`. `None` when there is nothing to
/// composite.
/// Draws only `region` of the commands, and cuts nothing when that is `UNCLIPPED`.
#[allow(clippy::too_many_arguments)]
fn offscreen(
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
) -> Option<ImageId> {
    let (width, height) = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    // A box with no area shows nothing, and asking for a 0xN render target leaves GL with an
    // incomplete framebuffer that the next composite on this canvas paints as a full square. A
    // cell tweening its width through zero hits this on its first and last frame.
    if width == 0 || height == 0 {
        return None;
    }
    let Some(image) = scratch(painter, walk, (width, height)) else {
        // Out of texture memory: preserve the subtree unmasked rather than drop it.
        // Into the parent's target, so it keeps the parent's frame: the clip this could not
        // allocate is not where these commands are going.
        run(painter, walk, commands, target, frame);
        return None;
    };

    let canvas = painter.canvas_mut();
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
    // The offscreen's own size (a shader quad inside a rounded clip places itself in that target,
    // ADR-0184), its origin at the clip's corner, and no transform: `draw_clipped`
    // reset the canvas transform above, and composites the result under the outer one afterwards.
    let inner = Frame {
        size: (width as f32, height as f32),
        origin: (clip.x0 as f32, clip.y0 as f32),
        transform: None,
        region,
    };
    run(painter, walk, commands, RenderTarget::Image(image), inner);

    // Multiplying alpha keeps the target premultiplied, and so masks a shader quad drawn into it
    // too. The fill covers the whole target: a pixel it misses keeps its alpha.
    if let Some((mask, box_px)) = mask {
        let paint = match &mask.source {
            MaskSource::Gradient(gradient) => Some(gradient_paint(gradient, rect)),
            // A missing mask image leaves the subtree unmasked, the answer an allocation failure
            // above gets too.
            MaskSource::Image(file) => walk
                .images
                .image(painter.canvas_mut(), std::path::Path::new(file), *box_px, None, Load::Inline, Fit::Stretch, 0)
                .map(|id| Paint::image(id, rect.x, rect.y, rect.width, rect.height, 0.0, 1.0)),
        };
        if let Some(paint) = paint {
            let canvas = painter.canvas_mut();
            canvas.reset_scissor();
            canvas.global_composite_operation(match mask.invert {
                false => CompositeOperation::DestinationIn,
                true => CompositeOperation::DestinationOut,
            });
            canvas.fill_path(&whole, &paint.with_anti_alias(false));
        }
    }

    let canvas = painter.canvas_mut();
    canvas.restore();
    canvas.set_render_target(target);
    Some(image)
}

/// An opaque box's shadow (ADR-0254): femtovg's box gradient fades from the colour to nothing
/// across 3 sigma centred on the spread box's edge, one quad and no render target.
fn paint_shadow(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, shadow: node::Shadow, radius: f32) {
    let LogicalRect { x, y, width, height } = super::shadow_rect(rect, rect, shadow);
    let Rgba { r, g, b, a } = shadow.color;
    // A ramp across 3 sigma is within 14/255 of the layer path's Gaussian; matching its slope
    // instead, 22. Floored at NanoVG's 1, since the gradient divides by it.
    let feather = (1.5 * shadow.blur).max(1.0);
    // CSS: a square corner stays square under spread. A signed distance past half the box is
    // positive everywhere, so the gradient would paint nothing.
    let radius = if radius > 0.0 { (radius + shadow.spread).clamp(0.0, width.min(height) / 2.0) } else { 0.0 };
    let color = Color::rgbaf(r, g, b, a);
    let paint = Paint::box_gradient(x, y, width, height, radius, feather, color, Color::rgbaf(r, g, b, 0.0));
    let reach = super::grow(LogicalRect { x, y, width, height }, feather / 2.0);
    let mut path = Path::new();
    path.rect(reach.x, reach.y, reach.width, reach.height);
    canvas.fill_path(&path, &paint);
}

/// A subtree under its own shadow and `content_blur` (ADR-0254). Both are femtovg filters over
/// pooled targets, and an unchanged layer composites what it last finished (ADR-0258).
fn draw_layer(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    command: &DrawCmd,
    node::Effect { shadow, blur, .. }: node::Effect,
    commands: &[DrawCmd],
    target: RenderTarget,
    frame: Frame,
) {
    let (rect, clip) = (command.rect, command.clip);
    let size = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    let area = LogicalRect { x: clip.x0 as f32, y: clip.y0 as f32, width: size.0 as f32, height: size.1 as f32 };
    let (cast, content) = match painter.layer(walk.surface, command) {
        Some(kept) => kept,
        None => {
            // Whole: the blur and the shadow read past the repaint's edge.
            let Some(content) =
                offscreen(painter, walk, rect, clip, None, None, commands, target, frame, super::UNCLIPPED)
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
            if filtered
                && !super::any_draw_matches(commands, |draw| {
                    super::volatile(draw) || matches!(draw, Draw::Backdrop { .. })
                })
            {
                walk.scratch.retain(|(id, _)| Some(*id) != cast && *id != content);
                painter.keep_layer(walk.surface, command, (cast, content), size);
            }
            (cast, content)
        }
    };
    if let (Some(shadow), Some(cast)) = (shadow, cast) {
        fill_image(painter.canvas_mut(), cast, super::shadow_rect(rect, area, shadow), 1.0);
    }
    fill_image(painter.canvas_mut(), content, area, 1.0);
}

/// femtovg's blur divides by sigma, and its own shadow skips one below this.
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
    painter.canvas_mut().filter_image(image, ImageFilter::GaussianBlur { sigma }, source);
    Some(image)
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
    let cast = blurred(painter, walk, content, size, sigma).or_else(|| scratch(painter, walk, size))?;
    let (width, height) = (size.0 as f32, size.1 as f32);
    let mut whole = Path::new();
    whole.rect(0.0, 0.0, width, height);
    let canvas = painter.canvas_mut();
    canvas.save();
    canvas.reset_transform();
    canvas.reset_scissor();
    canvas.set_render_target(RenderTarget::Image(cast));
    if sigma < MIN_SIGMA {
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
fn draw_backdrop(
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
fn read_target(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    area: PhysicalRect,
) -> Option<(ImageId, (usize, usize), impl Fn(ImageId, f32) -> Paint + use<>)> {
    let gl = walk.shaders.as_ref()?.gl;
    let canvas = painter.canvas_mut();
    let to_target = canvas.transform();
    let whole = PhysicalRect { x0: 0, y0: 0, x1: canvas.width() as i32, y1: canvas.height() as i32 };
    let region = super::transformed(to_target.0, area).intersect(whole);
    if super::is_empty(region) {
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
fn replace(canvas: &mut Canvas<OpenGl>, path: &Path, paint: &Paint, alpha: f32) {
    canvas.save();
    canvas.global_composite_operation(CompositeOperation::DestinationOut);
    canvas.fill_path(path, &Paint::color(Color::rgbaf(0.0, 0.0, 0.0, alpha)));
    canvas.global_composite_operation(CompositeOperation::Lighter);
    canvas.fill_path(path, paint);
    canvas.restore();
}

/// File-draw parameters shared by icon and image commands.
#[derive(Debug, Clone, Copy)]
struct FileDraw {
    fit: Fit,
    rect: LogicalRect,
    box_px: (u32, u32),
    alpha: f32,
    /// `None` for an `image`, which names a file the config chose rather than a themed icon.
    tint: Option<Rgba>,
    load: Load,
    /// `0` for an icon, which has no `source_blur` (ADR-0240).
    blur_px: u32,
}

/// Cache lookup and one `fill_path` over the fitted rect. Filling the full box with a `Contain`
/// paint would let femtovg clamp the outer pixel row into the letterbox; `Cover` is cropped by the
/// run's scissor.
/// Answers whether it drew, which is how an `image` learns its source has no texture yet and its
/// `retain` cover should take the frame (ADR-0180).
fn draw_file(canvas: &mut Canvas<OpenGl>, images: &mut ImageCache, file: &std::path::Path, draw: FileDraw) -> bool {
    let Some((id, fitted)) = file_texture(canvas, images, file, draw) else {
        return false;
    };
    fill_image(canvas, id, fitted, draw.alpha);
    true
}

/// The texture for `file` and the rect its `fit` puts it in, without drawing it. Split out because
/// a shader cross needs both endpoints' textures and rects and draws neither itself (ADR-0184).
fn file_texture(
    canvas: &mut Canvas<OpenGl>,
    images: &mut ImageCache,
    file: &std::path::Path,
    draw: FileDraw,
) -> Option<(ImageId, LogicalRect)> {
    let FileDraw { fit, rect, box_px, alpha: _, tint, load, blur_px } = draw;
    let id = images.image(canvas, file, box_px, tint, load, fit, blur_px)?;
    let (width, height) = canvas.image_size(id).ok()?;
    Some((id, image::fitted_rect(rect, width as f32, height as f32, fit)))
}

fn fill_image(canvas: &mut Canvas<OpenGl>, id: ImageId, fitted: LogicalRect, alpha: f32) {
    let mut path = Path::new();
    path.rect(fitted.x, fitted.y, fitted.width, fitted.height);
    canvas.fill_path(&path, &Paint::image(id, fitted.x, fitted.y, fitted.width, fitted.height, 0.0, alpha));
}

/// Below this the two sides of a box count as equal (`box_path`): the 0.05 px band the sweep in
/// its doc comment found clear of femtovg's bevel fold, and far above any layout rounding error.
const HAIR: f32 = 0.05;

/// The path a box with `radius` asks for: a rectangle, rounded rectangle, or stadium. femtovg
/// clamps radius with `rad.min(halfw)`; near that clamp, `rounded_rect` fails in two bands. At
/// exactly half, its zero-length straight segments collapse the fill to a square. Just below,
/// `path::cache`'s half-pixel `woff` bevel inset folds the fill fan back at each join: opaque fill
/// hides it, translucent fill blends folded slivers twice (a one-pixel chord at 1.6x alpha on a
/// 42%-alpha ground).
///
/// A sweep of square boxes from 24 to 43.5 logical pixels, at two sub-pixel offsets and 80
/// geometries per row, found:
///
/// | radius below half | filled square | interior seams |
/// | ----------------- | ------------- | -------------- |
/// | 0 (exactly half)  | 80            | 0              |
/// | 0.0001 to 0.01 px | 0             | 29             |
/// | 0.05 px and more  | 0             | 0              |
///
/// A shortfall clears both bands. An epsilon of 0.01 sits in the second, and so does Qt's
/// `qMin(w, h) * 0.4999f` (`qsgbasicinternalrectanglenode.cpp`) at every size. Any epsilon is a
/// constant tuned to one tessellator, so this builds the shape instead.
///
/// Half the smaller side is how config spells a pill (`radius = side / 2`), and a radius scaled
/// apart from the height can exceed half of it. Equal sides use femtovg's circle (four beziers,
/// no straight segments); unequal sides use two semicircular caps joined by `|width - height|`.
/// Both wind like `rounded_rect` (left, bottom, right, top), which controls the fill-fan inset.
///
/// A box whose sides differ by a hair is a circle. A hair-length straight run between the two
/// caps is worse than a bevel: for a box 2 µm narrower than it is tall, the vertical-cap path's
/// fill fan folds over and paints the whole bounding square (`a_box_a_hair_narrower_than_tall_is_
/// still_a_circle`). Tweens produce exactly that: a `width = "Fill"` circle inside a cell whose
/// width and padding both ease lands a rounding error either side of its height on different
/// frames, and the narrow frames flashed as squares.
fn box_path(rect: LogicalRect, radius: f32) -> Path {
    let LogicalRect { x, y, width: w, height: h } = rect;
    let mut path = Path::new();

    if radius == 0.0 || w <= 0.0 || h <= 0.0 {
        path.rect(x, y, w, h);
    } else if radius < 0.0 {
        // Each arc is centred on a corner point and swept inward; `arc` joins them with the
        // straight edges. Below half the shorter side, so neighbouring arcs never meet and fold.
        // Wound left, bottom, right, top like the shapes below: the other way, femtovg's
        // antialiasing inset pushes the edge up to 3px into the scoop.
        let r = (-radius).min(w.min(h) / 2.0 - HAIR).max(0.0);
        path.arc(x, y + h, r, -FRAC_PI_2, 0.0, Solidity::Hole);
        path.arc(x + w, y + h, r, PI, 3.0 * FRAC_PI_2, Solidity::Hole);
        path.arc(x + w, y, r, FRAC_PI_2, PI, Solidity::Hole);
        path.arc(x, y, r, 0.0, FRAC_PI_2, Solidity::Hole);
        path.close();
    } else if radius < w.min(h) / 2.0 {
        path.rounded_rect(x, y, w, h, radius);
    } else if (w - h).abs() <= HAIR {
        path.circle(x + w / 2.0, y + h / 2.0, w.min(h) / 2.0);
    } else if w > h {
        let r = h / 2.0;
        let (cy, right) = (y + r, x + w - r);
        // Top of the left cap, round the left to its bottom; the bottom edge; the right cap, round
        // to its top; `close` walks the top edge back. `Solidity::Solid` sweeps by *decreasing*
        // angle, which with y pointing down is the left-bottom-right-top direction wanted here.
        path.arc(x + r, cy, r, 3.0 * FRAC_PI_2, FRAC_PI_2, Solidity::Solid);
        path.arc(right, cy, r, FRAC_PI_2, -FRAC_PI_2, Solidity::Solid);
        path.close();
    } else {
        let r = w / 2.0;
        let (cx, bottom) = (x + r, y + h - r);
        path.arc(cx, y + r, r, 0.0, -PI, Solidity::Solid);
        path.arc(cx, bottom, r, PI, 0.0, Solidity::Solid);
        path.close();
    }

    path
}

/// The background fill, rounded when the node asked for it. See [`box_path`] for why a radius at
/// half the box is its own shape rather than a `rounded_rect` argument.
fn fill_rect(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, radius: f32, fill: &Fill) {
    // femtovg's antialias fringe paints an empty path as a 1px line.
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let paint = match fill {
        Fill::Color(color) => Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)),
        Fill::Gradient(gradient) => gradient_paint(gradient, rect),
    };
    canvas.fill_path(&box_path(rect, radius), &paint);
}

/// `gradient` laid over `rect` with CSS's geometry (ADR-0255).
fn gradient_paint(gradient: &Gradient, rect: LogicalRect) -> Paint {
    let stops = gradient.stops.iter().map(|(at, c)| (*at, Color::rgbaf(c.r, c.g, c.b, c.a)));
    let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    match gradient.shape {
        GradientShape::Linear { angle } => {
            // CSS's gradient line: long enough that the corners it points between take the end stops.
            let (sin, cos) = angle.to_radians().sin_cos();
            let half = (rect.width * sin.abs() + rect.height * cos.abs()) / 2.0;
            let (dx, dy) = (sin * half, -cos * half);
            Paint::linear_gradient_stops(cx - dx, cy - dy, cx + dx, cy + dy, stops)
        }
        GradientShape::Radial => {
            Paint::elliptical_gradient_stops(cx, cy, 0.0, 0.0, rect.width / 2.0, rect.height / 2.0, stops)
        }
        // femtovg starts a turn at three o'clock, CSS at twelve.
        GradientShape::Conic { angle } => {
            Paint::conic_gradient_stops_with_angle(cx, cy, (angle - 90.0).to_radians(), stops)
        }
    }
}

/// femtovg has no per-edge border primitive, so this covers exactly two cases. Uniform borders
/// (all four edges same width and colour) with `radius` above 0 get one `stroke_path` over the
/// rounded rect, inset by half the stroke width: femtovg strokes centred on the path, so drawing on
/// `rect`'s own edge would straddle it, half inside and half outside. Everything else, any edge
/// differing or radius 0, fills each edge that declares both a non-zero width and a colour as its
/// own rectangle.
///
/// ponytail: the per-edge-rectangle fallback ignores `radius`, giving square corners where a
/// rounded background shows through. Upgrade path: four corner arcs plus four mitred edge
/// segments, once a real config needs a rounded per-edge border.
fn paint_border(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    radius: f32,
    colors: BorderColor,
    widths: EdgeInsets,
    scale: f32,
) {
    let uniform_width = widths.top == widths.right && widths.right == widths.bottom && widths.bottom == widths.left;
    let uniform_color = matches!(
        (colors.top, colors.right, colors.bottom, colors.left),
        (Some(t), Some(r), Some(b), Some(l)) if t == r && r == b && b == l
    );

    if uniform_width && uniform_color && widths.top > 0.0 && radius > 0.0 {
        let color = colors.top.expect("uniform_color's match arm above guarantees Some on every edge");
        // `snap_border_band` rounds a box's own two edges to nearest, so it snaps the node's span
        // on each axis, not only a hairline's thickness. The stroke's thickness is snapped the same
        // way (band-of-one starting at `rect.x`, only the thickness half kept), so with integer box
        // edges and an integer thickness the centerline lands on an integer for an even width and a
        // half-integer for an odd one, the parity femtovg actually rasterizes (this module's
        // `snap_border_band` doc comment).
        let (box_x, box_width) = snap_border_band(rect.x, rect.width, scale);
        let (box_y, box_height) = snap_border_band(rect.y, rect.height, scale);
        let (_, width) = snap_border_band(rect.x, widths.top, scale);
        let inset = width / 2.0;
        let path = box_path(
            LogicalRect {
                x: box_x + inset,
                y: box_y + inset,
                width: (box_width - width).max(0.0),
                height: (box_height - width).max(0.0),
            },
            radius,
        );
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        paint.set_line_width(width);
        canvas.stroke_path(&path, &paint);
        return;
    }

    // Corners overlap here rather than mitre: each edge is its own filled rect spanning the node's
    // full width or height, so two adjacent non-zero edges both cover the corner they share.
    let LogicalRect { x, y, width: w, height: h } = rect;
    for (color, thickness, edge_rect, axis) in [
        (colors.top, widths.top, LogicalRect { x, y, width: w, height: widths.top }, EdgeAxis::Horizontal),
        (
            colors.bottom,
            widths.bottom,
            LogicalRect { x, y: y + h - widths.bottom, width: w, height: widths.bottom },
            EdgeAxis::Horizontal,
        ),
        (colors.left, widths.left, LogicalRect { x, y, width: widths.left, height: h }, EdgeAxis::Vertical),
        (
            colors.right,
            widths.right,
            LogicalRect { x: x + w - widths.right, y, width: widths.right, height: h },
            EdgeAxis::Vertical,
        ),
    ] {
        paint_border_edge(canvas, color, thickness, edge_rect, axis, scale);
    }
}

/// Which dimension of an edge rect is the thin one: top/bottom are thin in y, left/right in x.
/// `paint_border_edge` needs this to know which axis to hand `snap_border_band`; inferring it from
/// the rect's own width/height would be ambiguous whenever a node's height equals its border width.
enum EdgeAxis {
    Horizontal,
    Vertical,
}

/// One border edge: paints only where both a colour and a non-zero width say so
/// (`node::parse_border_color`'s doc comment: `border_width` alone is documented behaviour,
/// not a bug). Snaps the edge's thin axis with `snap_border_band` first, the same whole-physical-
/// pixel treatment as the uniform-radius stroke above; the long axis is left alone, since only the
/// thin axis can straddle a pixel boundary and blur.
fn paint_border_edge(
    canvas: &mut Canvas<OpenGl>,
    color: Option<Rgba>,
    width: f32,
    edge_rect: LogicalRect,
    axis: EdgeAxis,
    scale: f32,
) {
    let Some(color) = color else { return };
    if width <= 0.0 {
        return;
    }
    let edge_rect = match axis {
        EdgeAxis::Horizontal => {
            let (y, height) = snap_border_band(edge_rect.y, edge_rect.height, scale);
            LogicalRect { y, height, ..edge_rect }
        }
        EdgeAxis::Vertical => {
            let (x, width) = snap_border_band(edge_rect.x, edge_rect.width, scale);
            LogicalRect { x, width, ..edge_rect }
        }
    };
    let mut path = Path::new();
    path.rect(edge_rect.x, edge_rect.y, edge_rect.width, edge_rect.height);
    canvas.fill_path(&path, &Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)));
}

#[cfg(test)]
mod tests {
    use super::super::tests::resolved_surface;
    use super::*;

    use std::ffi::c_void;

    use khronos_egl as egl;
    use mlua::Lua;

    use crate::layout::scene::LogicalSize;
    use crate::text::shaping::{ShapeRequest, ShapingHandle};

    const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;

    /// `None` on any failure, with an `eprintln!` naming which step -- "EGL init failed, skip" is
    /// the gate a driverless CI box takes; this machine has a working Mesa/Iris (and llvmpipe
    /// under `LIBGL_ALWAYS_SOFTWARE=1`) and is expected to actually run every test below.
    ///
    /// Returns just the `Instance`: `Display`/`Surface`/`Context` are bare handle newtypes with no
    /// `Drop` impl, so they need no further Rust-side ownership once `make_current` below has
    /// bound them to this thread; only `instance` is read again, for `get_proc_address` in
    /// [`text_painter`]. Binds a pbuffer surface current before returning.
    fn init_headless_egl(width: i32, height: i32) -> Option<egl::Instance<egl::Static>> {
        init_headless_egl_two_surfaces(width, height).map(|(instance, ..)| instance)
    }

    /// Builds a `TextPainter` against `instance`'s already-current context, from the same
    /// `font_chain_data` `paint_surface` registers (ADR-0211).
    fn text_painter(
        instance: &egl::Instance<egl::Static>,
        shaping: &ShapingHandle,
        width: u32,
        height: u32,
    ) -> Option<TextPainter> {
        TextPainter::new(
            |s| instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
            width,
            height,
            shaping.clone(),
        )
        .map_err(|e| eprintln!("EGL init failed, skip: FemtoVG init: {e}"))
        .ok()
    }

    /// `#RRGGBBAA` at logical `(x, y)` from `canvas.screenshot()` -- femtovg's own `screenshot`
    /// already does the GL readback and the row flip, so this harness needs no raw `glReadPixels`.
    /// `scale` here is always `1.0`, so logical and physical pixel coordinates coincide.
    fn pixel_at(canvas: &mut Canvas<OpenGl>, x: usize, y: usize) -> (u8, u8, u8, u8) {
        let image = canvas.screenshot().expect("screenshot reads back the pbuffer's own framebuffer");
        let px = image[(x, y)];
        (px.r, px.g, px.b, px.a)
    }

    /// The EGL harness returning the pieces [`init_headless_egl`] hides, so one context can be made
    /// current against two different draw surfaces.
    #[allow(clippy::type_complexity)]
    fn init_headless_egl_two_surfaces(
        width: i32,
        height: i32,
    ) -> Option<(egl::Instance<egl::Static>, egl::Display, egl::Context, egl::Surface, egl::Surface)> {
        let instance = egl::Instance::new(egl::Static);

        // SAFETY: `eglGetPlatformDisplay` with `EGL_PLATFORM_SURFACELESS_MESA` takes no native
        // handle -- `EGL_DEFAULT_DISPLAY` is the null sentinel the extension defines -- and the
        // attribute list is a `EGL_NONE`-terminated slice, which is the contract for this call.
        let display = match unsafe {
            instance.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE])
        } {
            Ok(d) => d,
            Err(e) => {
                eprintln!("EGL init failed, skip: eglGetPlatformDisplay(SURFACELESS_MESA): {e}");
                return None;
            }
        };
        if let Err(e) = instance.initialize(display) {
            eprintln!("EGL init failed, skip: eglInitialize: {e}");
            return None;
        }
        if let Err(e) = instance.bind_api(egl::OPENGL_ES_API) {
            eprintln!("EGL init failed, skip: eglBindAPI(OPENGL_ES_API): {e}");
            return None;
        }
        let attribs = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::STENCIL_SIZE,
            8,
            egl::NONE,
        ];
        let config = match instance.choose_first_config(display, &attribs) {
            Ok(Some(c)) => c,
            Ok(None) => {
                eprintln!("EGL init failed, skip: no EGL config satisfies PBUFFER+GLES3+8-bit-RGBA");
                return None;
            }
            Err(e) => {
                eprintln!("EGL init failed, skip: eglChooseConfig: {e}");
                return None;
            }
        };
        let pbuffer_attribs = [egl::WIDTH, width, egl::HEIGHT, height, egl::NONE];
        let mut surfaces = Vec::new();
        for _ in 0..2 {
            match instance.create_pbuffer_surface(display, config, &pbuffer_attribs) {
                Ok(s) => surfaces.push(s),
                Err(e) => {
                    eprintln!("EGL init failed, skip: eglCreatePbufferSurface: {e}");
                    return None;
                }
            }
        }
        let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE];
        let context = match instance.create_context(display, config, None, &context_attribs) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("EGL init failed, skip: eglCreateContext: {e}");
                return None;
            }
        };
        if let Err(e) = instance.make_current(display, Some(surfaces[0]), Some(surfaces[0]), Some(context)) {
            eprintln!("EGL init failed, skip: eglMakeCurrent: {e}");
            return None;
        }
        Some((instance, display, context, surfaces[0], surfaces[1]))
    }

    #[test]
    fn one_canvas_draws_correctly_across_two_surfaces_sharing_one_context() {
        let Some((instance, display, context, first, second)) = init_headless_egl_two_surfaces(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let red = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect { width = "Fill", height = "Fill", background = "#FF0000FF" } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        painter.resize(64, 64);
        paint_tree(&mut painter, &mut ImageCache::new(), &red, 1.0);
        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (255, 0, 0, 255));

        instance.make_current(display, Some(second), Some(second), Some(context)).expect("switching the draw surface");
        let green = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 32, height = 32, child = rect { width = "Fill", height = "Fill", background = "#00FF00FF" } }"##,
            LogicalSize { width: 32.0, height: 32.0 },
        );
        painter.resize(32, 32);
        paint_tree(&mut painter, &mut ImageCache::new(), &green, 1.0);
        assert_eq!(
            pixel_at(painter.canvas_mut(), 16, 16),
            (0, 255, 0, 255),
            "the shared canvas must still draw correctly after eglMakeCurrent moved it to another surface"
        );

        instance.make_current(display, Some(first), Some(first), Some(context)).expect("switching back");
        painter.resize(64, 64);
        assert_eq!(
            pixel_at(painter.canvas_mut(), 32, 32),
            (255, 0, 0, 255),
            "the first surface's own framebuffer must be untouched by what was drawn into the second"
        );
    }

    #[test]
    fn a_background_fills_the_surface_with_the_exact_colour() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect { width = "Fill", height = "Fill", background = "#FF0000FF" } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (255, 0, 0, 255));
    }

    /// Paints `child` inside a 64x64 panel with no fill and reads back the pixels at `points`.
    fn paint_points(child: &str, points: &[(usize, usize)]) -> Option<Vec<(u8, u8, u8, u8)>> {
        let src = format!(r#"return panel {{ id = "bar", width = 64, height = 64, child = {child} }}"#);
        paint_with_gl(&src, (64, 64), points)
    }

    /// CSS geometry: a linear gradient runs top to bottom unless turned, a radial one from the centre
    /// out to the edges.
    #[test]
    fn a_gradient_background_follows_css_geometry() {
        let child = r##"rect { width = "Fill", height = "Fill",
            background = { gradient = "Linear", stops = { { 0, "#FF0000" }, { 1, "#0000FF" } } } }"##;
        let Some(px) = paint_points(child, &[(32, 0), (32, 63), (32, 32)]) else { return };
        assert!(px[0].0 > 245 && px[0].2 < 10, "top is the first stop, got {:?}", px[0]);
        assert!(px[1].2 > 245 && px[1].0 < 10, "bottom is the last stop, got {:?}", px[1]);
        assert!((120..=136).contains(&px[2].0), "the middle is halfway, got {:?}", px[2]);

        let turned = |shape: &str| {
            format!(
                r##"rect {{ width = "Fill", height = "Fill",
                    background = {{ {shape}, stops = {{ {{ 0, "#FF0000" }}, {{ 1, "#0000FF" }} }} }} }}"##
            )
        };
        let Some(px) = paint_points(&turned(r#"gradient = "Linear", angle = 90"#), &[(0, 32), (63, 32)]) else {
            return;
        };
        assert!(px[0].0 > 245 && px[1].2 > 245, "90 degrees runs left to right, got {px:?}");
        let Some(px) = paint_points(&turned(r#"gradient = "Radial""#), &[(32, 32), (32, 0), (0, 0)]) else { return };
        assert!(px[0].0 > 245 && px[1].2 > 245 && px[2].2 > 245, "centre out to the edges, got {px:?}");
    }

    /// A mask multiplies alpha, so the node's own fill and its child fade together: an opaque stop
    /// keeps them, a clear one leaves the untouched ground.
    #[test]
    fn a_gradient_mask_fades_the_nodes_fill_and_subtree_and_invert_flips_it() {
        let masked = |invert: bool| {
            format!(
                r##"rect {{ width = "Fill", height = "Fill", background = "#FFFFFFFF",
                    mask = {{ gradient = "Linear", invert = {invert},
                        stops = {{ {{ 0, "#FFFFFFFF" }}, {{ 0.5, "#FFFFFFFF" }}, {{ 0.5, "#FFFFFF00" }}, {{ 1, "#FFFFFF00" }} }} }},
                    children = {{ rect {{ width = 8, height = "Fill", background = "#FF0000FF" }} }} }}"##
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
            r##"rect {{ width = "Fill", height = "Fill", background = "#00FF00FF", mask = {{ source = "{}" }} }}"##,
            svg.display()
        );
        let Some(px) = paint_points(&child, &[(8, 32), (56, 32)]) else { return };
        assert_eq!(px, [(0, 255, 0, 255), (0, 0, 0, 0)]);
    }

    #[test]
    fn a_window_or_popup_root_paints_its_own_box_exactly_as_a_panel_root_does() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        for (kind, colour, expected) in
            [("window", "#FF0000FF", (255, 0, 0, 255)), ("popup", "#0000FFFF", (0, 0, 255, 255))]
        {
            let lua = Lua::new();
            let root = resolved_surface(
                &lua,
                &format!(r#"return {kind} {{ id = "bar", width = 64, height = 64, background = "{colour}" }}"#),
                LogicalSize { width: 64.0, height: 64.0 },
            );
            paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
            assert_eq!(
                pixel_at(painter.canvas_mut(), 32, 32),
                expected,
                "a `{kind}` root must paint its own box like a `panel` root"
            );
        }
    }

    #[test]
    fn a_lock_root_paints_its_own_background_over_the_whole_output_it_covers() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let lua = Lua::new();
        let root = resolved_surface(
            &lua,
            r##"return lock { id = "bar", background = "#00FF00FF" }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (0, 255, 0, 255));
        assert_eq!(
            pixel_at(painter.canvas_mut(), 1, 1),
            (0, 255, 0, 255),
            "the fill reaches the corner of the output the surface covers"
        );
    }

    #[test]
    fn a_later_child_paints_over_its_parent_at_the_overlap_and_the_parent_shows_outside_it() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect { width = "Fill", height = "Fill", background = "#0000FFFF", children = {
                rect { background = "#00FF00FF", width = 20, height = 20 },
            } } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 5, 5), (0, 255, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 40, 40), (0, 0, 255, 255));
    }

    #[test]
    fn a_childs_padded_offset_position_is_honoured_across_two_levels_of_nesting() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, padding = { top = 5, left = 5 }, child = rect {
                width = 50, height = 50, background = "#0000FFFF", padding = { top = 15, left = 15 },
                children = { rect { background = "#00FF00FF", width = 10, height = 10 } },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 10, 10), (0, 0, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 27, 27), (0, 255, 0, 255));
    }

    /// femtovg strokes through the stencil buffer; a context without one paints a translucent
    /// border's overlapping segments twice, which read as 75% where 50% was asked.
    #[test]
    fn a_translucent_rounded_border_paints_its_alpha_once() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, radius = 8, border_width = 4, border_color = "#FF000080",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        for (x, y) in [(20, 1), (1, 20), (38, 20), (20, 38)] {
            let alpha = pixel_at(painter.canvas_mut(), x, y).3;
            assert!((126..=130).contains(&alpha), "({x}, {y}) is painted once, got alpha {alpha}");
        }
    }

    /// A scoop cuts each corner out along a circle centred on the corner point, and a translucent
    /// fill covers everything else exactly once: no fold where an arc meets a straight edge.
    #[test]
    fn a_scooped_box_cuts_each_corner_in_and_fills_the_rest_once() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, radius = 12, corner_shape = "Scoop", background = "#FF000080",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        for (x, y) in [(1, 1), (38, 1), (1, 38), (38, 38), (5, 5)] {
            assert_eq!(pixel_at(painter.canvas_mut(), x, y).3, 0, "({x}, {y}) is inside a scoop");
        }
        for (x, y) in [(20, 1), (1, 20), (20, 20), (10, 10), (38, 20)] {
            let alpha = pixel_at(painter.canvas_mut(), x, y).3;
            assert!((126..=130).contains(&alpha), "({x}, {y}) is filled once, got alpha {alpha}");
        }
        // Row 3's centre crosses the arc at x = sqrt(12^2 - 3.5^2) = 11.5: the edge sits there,
        // not pushed into the scoop.
        assert_eq!(pixel_at(painter.canvas_mut(), 10, 3).3, 0, "the pixel before the arc is empty");
        assert!(pixel_at(painter.canvas_mut(), 12, 3).3 >= 100, "the pixel after the arc is filled");
    }

    #[test]
    fn a_per_edge_border_paints_only_the_edge_that_declared_both_a_colour_and_a_width() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, background = "#000000FF",
                border_width = { top = 4, bottom = 4 },
                border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 20, 1), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 20, 38), (0, 0, 0, 255));
    }

    #[test]
    fn text_foreground_colour_puts_non_background_pixels_inside_its_rect() {
        let Some(instance) = init_headless_egl(120, 40) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 120, 40) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 120, height = 40, child = rect { width = "Fill", height = "Fill", background = "#000000FF", children = {
                text { content = "Mantle", font_size = 24, foreground = "#00FF00FF" },
            } } }"##,
            LogicalSize { width: 120.0, height: 40.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        let mut lit_pixels = 0usize;
        for y in 0..30usize {
            for x in 0..100usize {
                let (r, g, b, _) = pixel_at(painter.canvas_mut(), x, y);
                if (r, g, b) == (0, 0, 0) {
                    continue;
                }
                lit_pixels += 1;
                assert!(
                    g > r && g > b,
                    "a glyph pixel at ({x}, {y}) is {:?}, which is not the green `foreground` asked for -- white here means `foreground` never reached `draw_text`",
                    (r, g, b)
                );
            }
        }
        assert!(
            lit_pixels > 0,
            "text with a foreground colour must paint at least one non-background pixel inside its rect"
        );
    }

    /// A pill's shape, and the bug it hid. `radius = side / 2`, and a radius over half the side,
    /// both reached femtovg's half-box clamp, whose fill tessellation collapses to a rectangle:
    /// every pill and circle had a square ground under a round border.
    ///
    /// 40x40 at 20 rounded before the fix while
    /// 32x32 did not, showing size-dependent degeneracy rather than a clean threshold; keep both
    /// so one passing size cannot hide it. Both use [`box_path`]'s stadium branch: this catches an
    /// exact half radius, while the companion test catches the just-below-half fill-fold case.
    ///
    /// Both halves are asserted: the corner must show the parent through the round fill, while the
    /// mid-edge must remain border. Squaring the border to match a broken fill would satisfy only
    /// one of those checks.
    #[test]
    fn a_radius_of_half_the_box_fills_a_stadium_not_a_square() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        // side, radius. The third asks for more than half, which must clamp to the same stadium
        // rather than square off.
        for (side, radius) in [(32.0_f32, 16.0_f32), (40.0, 20.0), (32.0, 17.0)] {
            let src = format!(
                r##"return panel {{ id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = {{ top = 4, left = 4 }}, child = rect {{
                    width = {side}, height = {side}, background = "#000000FF", radius = {radius},
                    border_width = 2, border_color = "#FFFFFFFF",
                }} }}"##
            );
            let root = resolved_surface(&lua, &src, LogicalSize { width: 64.0, height: 64.0 });
            paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

            let case = format!("{side}x{side} radius {radius}");
            // Well outside the inscribed circle: `side/2 * (sqrt(2) - 1)` clear, about 6px at 32
            // and 8px at 40, so this is not an antialiasing read.
            assert_eq!(
                pixel_at(painter.canvas_mut(), 5, 5),
                (255, 0, 0, 255),
                "{case}: the corner of a stadium is outside it, so the panel behind must show through"
            );
            assert_eq!(
                pixel_at(painter.canvas_mut(), 4 + side as usize / 2, 4 + side as usize / 2),
                (0, 0, 0, 255),
                "{case}: and the middle is still filled"
            );
            assert_eq!(
                pixel_at(painter.canvas_mut(), 5, 4 + side as usize / 2),
                (255, 255, 255, 255),
                "{case}: the border was always round here and must stay so"
            );
        }
    }

    /// A translucent ground at radius half the box must blend exactly once everywhere inside it.
    ///
    /// The shape being right is not enough, and that is the point of this test sitting beside
    /// [`a_radius_of_half_the_box_fills_a_stadium_not_a_square`]: `rounded_rect` at exactly half
    /// draws a correct outline and then folds its fill fan over itself at each of the four
    /// collapsed straight segments, so a translucent ground gets a second helping of itself along
    /// a one-pixel chord out of each cap. Opaque fills hide it; a 42%-alpha ground does not.
    ///
    /// 33x33 at offset 20 rather than a round 32, because the fold is erratic in the size: a sweep
    /// of square boxes from 24 to 43.5 at radius exactly half found seams at 30 of the 80
    /// geometries tried, and 32x32 at an integer offset was one of the clean ones. This case is one
    /// of the dirty ones, so it fails against a plain `rounded_rect`.
    #[test]
    fn a_translucent_ground_at_half_radius_blends_once_not_twice() {
        let Some(instance) = init_headless_egl(96, 96) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 96) else { return };

        // Black at 42% over the panel's red: one blend is 255 * 0.58, two is 255 * 0.58^2 = 86.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF", padding = { top = 20, left = 20 }, child = rect {
                width = 33, height = 33, background = "#0000006B", radius = 16.5,
            } }"##,
            LogicalSize { width: 96.0, height: 96.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let image = painter.canvas_mut().screenshot().expect("screenshot reads back the pbuffer's own framebuffer");

        // The inscribed disc less four pixels, which clears the antialiased rim on every side.
        let centre = 20.0 + 33.0 / 2.0;
        let radius = 33.0 / 2.0 - 4.0;
        let mut doubled = Vec::new();
        for y in 0..96usize {
            for x in 0..96usize {
                let (dx, dy) = (x as f32 + 0.5 - centre, y as f32 + 0.5 - centre);
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let pixel = image[(x, y)];
                if (pixel.r, pixel.g, pixel.b) != (148, 0, 0) {
                    doubled.push((x, y, pixel.r));
                }
            }
        }
        assert!(doubled.is_empty(), "the ground blended twice at {doubled:?}");

        // And the shape is still a circle, so nothing above can be satisfied by drawing less.
        assert_eq!(
            pixel_at(painter.canvas_mut(), 21, 21),
            (255, 0, 0, 255),
            "the corner of a circle is outside it, so the panel behind must show through"
        );
    }

    /// The uniform-border-with-radius branch of [`paint_border`] had no test at all: the per-edge
    /// case above takes the fallback path, so the whole `stroke_path` arm, including the half-width
    /// inset its doc comment reasons carefully about, went unexercised. That inset is the part
    /// worth pinning: femtovg strokes centred on the path, so an uninset stroke straddles the
    /// node's own edge with half of it painted outside the box.
    #[test]
    fn a_uniform_border_with_a_radius_strokes_inside_the_nodes_own_box() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10, left = 10 }, child = rect {
                width = 40, height = 40, background = "#000000FF",
                radius = 8, border_width = 4, border_color = "#FFFFFFFF",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 12, 30), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 16, 30), (0, 0, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 9, 30), (255, 0, 0, 255));
    }

    /// The face the first glyph of "hello" is shaped in: in `family`, or the declared chain.
    fn first_face(shaping: &ShapingHandle, family: Option<&str>) -> fontdb::ID {
        shaping
            .shape_glyphs(ShapeRequest {
                text: "hello".into(),
                font_size: 20.0,
                line_height: crate::text::shaping::line_height(20.0),
                max_width: None,
                runs: Vec::new(),
                font: family.map(std::sync::Arc::from),
            })
            .shaped[0]
            .glyphs[0]
            .face
    }

    /// A family first named after the painter was built reaches femtovg through `sync`, and every
    /// sync keeps the ids it already minted: `add_shared_font_with_index` never dedups, so
    /// re-adding a face would strand its old `Font` and change its id.
    #[test]
    fn a_family_named_later_reaches_femtovg_through_sync_and_held_faces_keep_their_ids() {
        let Some(instance) = init_headless_egl(400, 60) else { return };
        let (declared, named) = ("Noto Sans", "Noto Sans Mono");
        if !crate::text::fonts::fc_match_available()
            || !crate::text::fonts::fc_lists(declared)
            || !crate::text::fonts::fc_lists(named)
        {
            eprintln!("skip: need two installed families to tell apart");
            return;
        }
        let shaping = ShapingHandle::spawn();
        shaping.set_chain(&[declared.to_string()]);
        let Some(mut painter) = text_painter(&instance, &shaping, 400, 60) else { return };
        let declared_face = first_face(&shaping, None);
        let declared_id = painter.font_id(declared_face);
        assert!(declared_id.is_some(), "the declared face is registered from the start");

        // What a text node does: measure first, which is what resolves the family.
        let named_face = first_face(&shaping, Some(named));
        assert_eq!(painter.font_id(named_face), None, "the painter was built before the family loaded");
        assert_ne!(shaping.font_generation(), painter.font_generation(), "the painter should now be behind");
        painter.sync();
        let named_id = painter.font_id(named_face);
        assert!(named_id.is_some(), "the named family's face should now be drawable");
        assert_eq!(painter.font_generation(), shaping.font_generation());
        assert_eq!(painter.font_id(declared_face), declared_id);

        shaping.ensure_family(&std::sync::Arc::from("ZZ No Such Family 9184"));
        assert_ne!(shaping.font_generation(), painter.font_generation(), "the second sync must not be a no-op");
        painter.sync();
        assert_eq!(painter.font_id(declared_face), declared_id);
        assert_eq!(painter.font_id(named_face), named_id);
    }

    /// A name nothing on the system answers shapes in the declared chain, whose faces the painter
    /// holds -- a typo draws the text in the wrong face, never as nothing.
    #[test]
    fn a_family_nothing_answers_shapes_in_a_face_the_painter_holds() {
        let Some(instance) = init_headless_egl(400, 60) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 400, 60) else { return };
        let face = first_face(&shaping, Some("ZZ No Such Family 9184"));
        painter.sync();
        assert!(painter.font_id(face).is_some());
    }

    /// A variable family ships bold as one file's `wght` axis: cosmic-text shapes a bold run at 700,
    /// so paint must draw that instance, not the file's default, or bold spacing holds regular ink.
    #[test]
    fn a_bold_run_in_a_variable_family_draws_the_bold_instance() {
        let Some(instance) = init_headless_egl(240, 40) else { return };
        let family = "Inter Variable";
        if !crate::text::fonts::fc_match_available() || !crate::text::fonts::fc_lists(family) {
            eprintln!("skip: {family} is not installed");
            return;
        }
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        shaping.ensure_family(&std::sync::Arc::from(family));
        let Some(mut painter) = text_painter(&instance, &shaping, 240, 40) else { return };
        let mut ink = |bold: bool| {
            let root = resolved_surface(
                &lua,
                &format!(
                    r##"return panel {{ id = "bar", width = 240, height = 40, background = "#000000FF", child = text {{
                        font = "{family}", font_size = 24, foreground = "#FFFFFFFF",
                        content = {{ {{ text = "Mantle", bold = {bold} }} }} }} }}"##
                ),
                LogicalSize { width: 240.0, height: 40.0 },
            );
            paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
            let mut sum = 0u32;
            for y in 0..40 {
                for x in 0..240 {
                    sum += u32::from(pixel_at(painter.canvas_mut(), x, y).1);
                }
            }
            sum as f32
        };
        let (regular, bold) = (ink(false), ink(true));
        assert!(bold > regular * 1.2, "bold ink {bold} should clearly exceed regular ink {regular}");
    }

    /// A `text` node's content wider than the box layout gave it must stop at that box's edge, not
    /// paint over whatever sits to its right. The live MPRIS-title-through-two-cells bug the doc
    /// comment describes.
    ///
    /// Proved this test is real, not just a green test: with `run`'s `save`/
    /// `intersect_scissor`/`restore` temporarily removed, this failed at the first scanned pixel
    /// row with a mix of white (glyph) and black-background pixels found past the box, exactly
    /// the escape this test exists to catch. Restored before finishing.
    #[test]
    fn a_text_wider_than_its_box_paints_nothing_outside_it() {
        let Some(instance) = init_headless_egl(200, 50) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 200, 50) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 200, height = 50, background = "#0000FFFF", child = rect {
                width = 40, height = 50, background = "#000000FF", children = {
                    text { content = "Mantle Engine Renderer Overflow", font_size = 24, foreground = "#FFFFFFFF" },
                } } }"##,
            LogicalSize { width: 200.0, height: 50.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        for y in 0..50usize {
            for x in 45..200usize {
                assert_eq!(
                    pixel_at(painter.canvas_mut(), x, y),
                    (0, 0, 255, 255),
                    "pixel ({x}, {y}) outside the text's 40px-wide box is not the surface's plain blue -- \
                     the overflowing glyph escaped its clip"
                );
            }
        }
    }

    /// Regression test: a 1px border at a fractional position must land on exactly one physical
    /// pixel row, not blur across two. `padding.top = 10.3` puts the bordered rect's absolute y at
    /// a fractional offset -- unsnapped, femtovg's own antialiasing fills part of row 10 and part
    /// of row 11 at partial coverage instead of one row at full coverage.
    ///
    /// Proved this catches the bug it exists for: with `snap_border_band` removed from
    /// `paint_border_edge`'s horizontal branch (using the raw, unsnapped `edge_rect` instead),
    /// row 10 came back `(201, 178, 178, 255)`, a red/white blend rather than white, and the
    /// row-10 assertion failed. Restored before finishing.
    #[test]
    fn a_1px_border_at_a_fractional_position_is_exactly_one_pixel_row() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10.3 }, child = rect {
                width = 30, height = 20, background = "#000000FF",
                border_width = { top = 1 }, border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 9), (255, 0, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 10), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 11), (0, 0, 0, 255));
    }

    /// The same snap as the 1px test above, at a width wide enough that "one row either way"
    /// is not the whole story: a 4px band must stay exactly four rows, neither growing to five
    /// nor losing one, which is what pins `snap_border_band` rounding both edges independently
    /// rather than rounding the near edge and adding an unrounded thickness. `padding.top =
    /// 31.3` is the fractional geometry a content-sized surface resolves to, so this is the real
    /// shape of the bug, not a contrived one.
    ///
    /// Note this is the filled-edge branch, not the stroke: `border_width = { top = 4 }` leaves
    /// the other three edges at zero, so `paint_border`'s `uniform_width` test fails and it
    /// takes the per-edge path. Stroke parity, the thing the deleted `snap_border_to_physical`
    /// actually got wrong, is covered by
    /// `a_uniform_stroked_border_at_a_fractional_position_covers_whole_pixel_columns` below.
    ///
    /// Proved this catches the bug: with the same unsnapped `edge_rect` change as the 1px test
    /// above, row 31 came back `(201, 178, 178, 255)` instead of full white -- the loop below
    /// fails on the first row it checks, so this only pins that one value, not all four rows'
    /// worth of blend. Restored before finishing.
    #[test]
    fn a_4px_border_at_a_fractional_position_stays_exactly_four_rows() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 31.3 }, child = rect {
                width = 30, height = 20, background = "#000000FF",
                border_width = { top = 4 }, border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 30), (255, 0, 0, 255));
        for y in 31..35usize {
            assert_eq!(pixel_at(painter.canvas_mut(), 15, y), (255, 255, 255, 255), "row {y} is not fully white");
        }
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 35), (0, 0, 0, 255));
    }

    /// The uniform-width-with-radius branch of `paint_border` strokes rather than fills, and a
    /// stroke is where physical-pixel parity actually bites: femtovg centres a stroke on its
    /// path, so a 4-wide stroke centred on a half-integer spreads across five rows at partial
    /// coverage while the same stroke centred on an integer covers exactly four. Snapping the
    /// box span and the thickness as bands is what puts the centreline on the right side of that
    /// split without the caller reasoning about parity at all.
    ///
    /// `padding.left = 10.3` is what makes this a real test: with an integer padding the stroke
    /// already lands on whole pixels and passes without any snapping, which is why the existing
    /// `a_uniform_border_with_a_radius_strokes_inside_the_nodes_own_box` test above cannot see
    /// this. Measured with the `snap_border_band` calls in that branch removed: column 10 came
    /// back `(201, 178, 178, 255)`, a red/white blend, instead of full white.
    #[test]
    fn a_uniform_stroked_border_at_a_fractional_position_covers_whole_pixel_columns() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10, left = 10.3 }, child = rect {
                width = 40, height = 40, background = "#000000FF",
                radius = 8, border_width = 4, border_color = "#FFFFFFFF",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(
            pixel_at(painter.canvas_mut(), 9, 30),
            (255, 0, 0, 255),
            "column 9 should be the surface's red, outside the border"
        );
        for x in 10..14usize {
            assert_eq!(pixel_at(painter.canvas_mut(), x, 30), (255, 255, 255, 255), "column {x} is not fully white");
        }
        assert_eq!(
            pixel_at(painter.canvas_mut(), 14, 30),
            (0, 0, 0, 255),
            "column 14 should be the rect's own black fill, past the border"
        );
    }

    /// A `row`/`column`/`rect` container clips its children just as much as a `text` node clips its
    /// glyphs. A `row` whose children overflow is the same defect as an overflowing `text`, not a
    /// separate case. This is the non-text half of that claim: a child rect explicitly larger than
    /// its parent must not paint past the parent's own box.
    ///
    /// Proved this test is real the same way: with the clip removed, the assertion at (50, 50)
    /// failed, reading the child's green instead of the surface's magenta. Restored before
    /// finishing.
    #[test]
    fn an_oversized_child_rect_is_clipped_to_its_parents_box() {
        let Some(instance) = init_headless_egl(80, 80) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 80, 80) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 80, height = 80, background = "#FF00FFFF", padding = { top = 10, left = 10 }, child = rect {
                width = 30, height = 30, background = "#000000FF", children = {
                    rect { background = "#00FF00FF", width = 60, height = 60 },
                } } }"##,
            LogicalSize { width: 80.0, height: 80.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 15), (0, 255, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 50, 50), (255, 0, 255, 255));
    }

    // ---- `clip = "Rounded"` ----

    /// A `width = "Fill"` circle in a tweening cell lands a rounding error narrower than its
    /// height on some frames. That box took the vertical-cap branch, whose hair-length straight run
    /// folded the fill fan over the whole square; the wider case never did, which is why it showed
    /// on some frames and not others.
    #[test]
    fn a_box_a_hair_narrower_than_tall_is_still_a_circle() {
        let Some(instance) = init_headless_egl(64, 48) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 48) else { return };
        let white = &Fill::Color(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        for (name, w) in [("a hair narrower", 31.999_998), ("a hair wider", 32.000_004), ("square", 32.0)] {
            let canvas = painter.canvas_mut();
            canvas.clear_rect(0, 0, 64, 48, Color::rgbaf(0.0, 0.0, 0.0, 1.0));
            fill_rect(canvas, LogicalRect { x: 8.0, y: 8.0, width: w, height: 32.0 }, 17.0, white);
            canvas.flush();
            assert_eq!(pixel_at(canvas, 9, 9), (0, 0, 0, 255), "{name}: the corner outside the circle stays black");
            assert_eq!(pixel_at(canvas, 24, 24), (255, 255, 255, 255), "{name}: the centre is filled");
        }
    }

    /// A slider's fill at 0% is a zero-width box; it once drew a 1px line.
    #[test]
    fn a_zero_width_box_paints_nothing() {
        let Some(instance) = init_headless_egl(64, 48) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 48) else { return };
        let canvas = painter.canvas_mut();
        canvas.clear_rect(0, 0, 64, 48, Color::rgbaf(0.0, 0.0, 0.0, 1.0));
        let white = &Fill::Color(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        fill_rect(canvas, LogicalRect { x: 24.0, y: 8.0, width: 0.0, height: 32.0 }, 6.0, white);
        canvas.flush();
        for x in 22..27 {
            assert_eq!(pixel_at(canvas, x, 24), (0, 0, 0, 255), "column {x} stays black");
        }
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
                    width = 80, height = 32, radius = 16, clip = "Rounded",
                    children = { rect { width = 30, height = "Fill", background = "#0000FFFF" } },
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
                    children = { rect { width = 30, height = "Fill", background = "#0000FFFF" } },
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
                    width = 40, height = 32, radius = 16, clip = "Rounded",
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
                    width = 64, height = 64, radius = 32, clip = "Rounded",
                    children = { rect {
                        width = 64, height = 64, radius = 32, clip = "Rounded",
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
                    width = 80, height = 32, radius = 16, clip = "Rounded",
                    children = { rect { width = 30, height = "Fill", background = "#0000FF80" } },
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
                    width = 80, height = 32, radius = 16, clip = "Rounded",
                    border_width = 4, border_color = "#00FF00FF",
                    children = { rect { width = 30, height = "Fill", background = "#0000FFFF" } },
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

    /// ADR-0149: `scale` paints a node bigger without moving its layout box. A 16px white square
    /// centred in a 64x48 black panel, scaled 2x, covers 32px around the same centre.
    #[test]
    fn a_scaled_node_paints_about_its_origin_and_its_layout_box_is_unchanged() {
        let Some(instance) = init_headless_egl(64, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 48) else { return };
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 48, background = "#000000ff", child = rect {
                width = 16, height = 16, margin = { left = 24, top = 16 }, background = "#ffffffff", scale = 2 } }"##,
            LogicalSize { width: 64.0, height: 48.0 },
        );
        assert_eq!(root.children[0].rect.width, 16.0, "the solver never sees the scale");
        assert_eq!(root.children[0].transform.scale, (2.0, 2.0));
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 18, 10), (255, 255, 255, 255), "inside the painted 16..48 x 8..40 box");
        assert_eq!(pixel_at(canvas, 45, 38), (255, 255, 255, 255));
        assert_eq!(pixel_at(canvas, 12, 10), (0, 0, 0, 255), "outside it");
        assert_eq!(pixel_at(canvas, 32, 24), (255, 255, 255, 255), "the centre stays put");
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
                    width = 60, height = 32, children = { rect {
                        margin = { left = 40 }, width = 64, height = 32, radius = 16, clip = "Rounded",
                        children = { rect { width = 64, height = "Fill", background = "#0000FFFF" } },
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

    /// A 32px box at (16, 16) on a white 64x96 panel, painted with `effect` properties and read
    /// down its middle column.
    fn paint_effect(effect: &str) -> Option<[(u8, u8, u8, u8); 8]> {
        let px = paint_effect_at(effect, &[8, 20, 32, 40, 50, 56, 64, 76].map(|y| (32, y)))?;
        Some(std::array::from_fn(|i| px[i]))
    }

    fn paint_effect_at(effect: &str, points: &[(usize, usize)]) -> Option<Vec<(u8, u8, u8, u8)>> {
        let instance = init_headless_egl(64, 96)?;
        let shaping = ShapingHandle::spawn();
        let mut painter = text_painter(&instance, &shaping, 64, 96)?;
        let src = format!(
            r##"return panel {{ id = "bar", width = 64, height = 96, background = "#FFFFFFFF",
                padding = {{ top = 16, left = 16 }}, child = rect {{ width = 32, height = 32, {effect} }} }}"##
        );
        let root = resolved_surface(&Lua::new(), &src, LogicalSize { width: 64.0, height: 96.0 });
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        Some(points.iter().map(|&(x, y)| pixel_at(canvas, x, y)).collect())
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
        let Some(layer) = paint_effect_at(&format!(r##"background = "#FF0000FE", {shadow}"##), &column) else {
            return;
        };
        let worst = gradient.iter().zip(&layer).map(|(g, l)| g.0.abs_diff(l.0)).max().unwrap();
        assert!(worst <= 14, "gradient {gradient:?} against layer {layer:?}");
    }

    /// ADR-0254's layer path: a half-transparent box casts a half-strength shadow, and the box
    /// composites over it rather than beside it.
    #[test]
    fn a_translucent_box_casts_a_shadow_at_its_own_alpha_under_itself() {
        let Some(px) = paint_effect(r##"background = "#FF000080", shadow_offset = { y = 16 }"##) else { return };
        assert!(near(px[0], (255, 255, 255)), "{px:?}");
        assert!(near(px[1], (255, 127, 127)), "the box alone: {px:?}");
        assert!(near(px[3], (191, 63, 63)), "the box over its shadow: {px:?}");
        assert!(near(px[5], (127, 127, 127)), "the shadow alone, at the box's alpha: {px:?}");
        assert!(near(px[7], (255, 255, 255)), "{px:?}");

        let Some(px) = paint_effect(r##"background = "#FF000080", shadow_offset = { y = 16 }, shadow_blur = 8"##)
        else {
            return;
        };
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
        let effect = r##"background = "#FF0000FF", shadow_offset = { y = 16 },
            mask = { gradient = "Linear", angle = 90,
                stops = { { 0, "#FFFFFFFF" }, { 0.5, "#FFFFFFFF" }, { 0.5, "#FFFFFF00" }, { 1, "#FFFFFF00" } } }"##;
        let Some(px) = paint_effect_at(effect, &[(20, 30), (44, 30), (20, 56), (44, 56)]) else { return };
        assert!(near(px[0], (255, 0, 0)) && near(px[1], (255, 255, 255)), "the mask keeps the left half: {px:?}");
        assert!(near(px[2], (0, 0, 0)) && near(px[3], (255, 255, 255)), "and only it casts: {px:?}");
    }

    /// Paints the surface `src` at `size` through [`execute`] with a GL context, which a shader
    /// quad and a backdrop need, and reads back `points`.
    fn paint_with_gl(src: &str, size: (u32, u32), points: &[(usize, usize)]) -> Option<Vec<(u8, u8, u8, u8)>> {
        let instance = init_headless_egl(size.0 as i32, size.1 as i32)?;
        let shaping = ShapingHandle::spawn();
        let mut painter = text_painter(&instance, &shaping, size.0, size.1)?;
        let root = resolved_surface(&Lua::new(), src, LogicalSize { width: size.0 as f32, height: size.1 as f32 });
        // SAFETY: `init_headless_egl` made this context current on this thread.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        };
        let mut stage = image_shader::ShaderStage::default();
        let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
        let list = build(&root, 1.0, None);
        let (target, whole) =
            ((size.0 as f32, size.1 as f32), PhysicalRect { x0: 0, y0: 0, x1: size.0 as i32, y1: size.1 as i32 });
        let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
        let _ = execute("test", &mut painter, images, captures, &list, 1.0, target, &[whole], shaders);
        let canvas = painter.canvas_mut();
        Some(points.iter().map(|&(x, y)| pixel_at(canvas, x, y)).collect())
    }

    /// Paints `after` whole on one 96x64 pbuffer, and `before` whole then `after` in `region` on
    /// another, and asserts the second matches the first inside `region` and `before` outside it.
    fn assert_partial_repaint_matches(before: &DisplayList, after: &DisplayList, regions: &[PhysicalRect]) {
        let Some((instance, display, context, _, partial)) = init_headless_egl_two_surfaces(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        // SAFETY: `init_headless_egl_two_surfaces` made this context current on this thread.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        };
        let mut stage = image_shader::ShaderStage::default();
        let mut paint = |painter: &mut TextPainter, list: &DisplayList, regions: &[PhysicalRect]| {
            let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
            let shaders = Some(Shaders { gl: &gl, stage: &mut stage });
            let _ = execute("test", painter, images, captures, list, 1.0, (96.0, 64.0), regions, shaders);
            painter.canvas_mut().screenshot().expect("screenshot reads back the pbuffer's own framebuffer")
        };
        let whole = PhysicalRect { x0: 0, y0: 0, x1: 96, y1: 64 };
        let expected = paint(&mut painter, after, &[whole]);
        instance
            .make_current(display, Some(partial), Some(partial), Some(context))
            .expect("switching the draw surface");
        let previous = paint(&mut painter, before, &[whole]);
        let repainted = paint(&mut painter, after, regions);
        for (x, y) in (0..64usize).flat_map(|y| (0..96usize).map(move |x| (x, y))) {
            let (x_, y_) = (x as i32, y as i32);
            let inside = regions.iter().any(|r| (r.x0..r.x1).contains(&x_) && (r.y0..r.y1).contains(&y_));
            let want = if inside { expected[(x, y)] } else { previous[(x, y)] };
            assert_eq!(repainted[(x, y)], want, "({x}, {y}), inside the region: {inside}");
        }
    }

    fn surface_96x64(src: &str) -> DisplayList {
        build(&resolved_surface(&Lua::new(), src, LogicalSize { width: 96.0, height: 64.0 }), 1.0, None)
    }

    /// ADR-0258. A partial repaint rewrites its region alone: through a masked group, a shader quad
    /// and a blurred box it cuts across, it matches a whole paint, and outside it the last frame
    /// stays.
    #[test]
    fn a_partial_repaint_matches_a_whole_one_inside_its_region_and_keeps_the_last_frame_outside() {
        let dir = tempfile::tempdir().unwrap();
        let frag = dir.path().join("tint.frag");
        std::fs::write(&frag, "uniform vec4 tint; void main() { fragColor = tint; }").unwrap();
        let list = |ground: &str, corner: &str, tint: &str| {
            surface_96x64(&format!(
                r##"return panel {{ id = "bar", width = 96, height = 64, background = "{ground}", padding = 8,
                    child = row {{ spacing = 8, children = {{
                        rect {{ width = 24, height = 48, background = "#FF0000FF", mask = {{ gradient = "Linear",
                            angle = 90, stops = {{ {{ 0, "#FFFFFFFF" }}, {{ 1, "#FFFFFF40" }} }} }},
                            children = {{ rect {{ width = 16, height = 16, background = "{corner}" }} }} }},
                        shader {{ width = 20, height = 48, source = "{}", params = {{ tint = {{ {tint} }} }} }},
                        rect {{ width = 20, height = 20, background = "#0000FFFF", radius = 6, content_blur = 3 }} }} }} }}"##,
                frag.display()
            ))
        };
        let after = list("#FFFFFFFF", "#FFFF00FF", "0.5, 0, 0, 0.5");
        // Into the blurred box's 3-sigma reach, which starts at 59.
        let region = PhysicalRect { x0: 20, y0: 12, x1: 64, y1: 20 };
        assert_eq!(after.repaint_region(region), region);
        let before = list("#00FF00FF", "#000000FF", "0, 0, 0.5, 0.5");
        assert_partial_repaint_matches(&before, &after, &[region]);
        // Two apart, one of them through the blurred box's composite alone.
        assert_partial_repaint_matches(
            &before,
            &after,
            &[PhysicalRect { x0: 2, y0: 2, x1: 12, y1: 60 }, PhysicalRect { x0: 70, y0: 30, x1: 94, y1: 50 }],
        );
    }

    /// ADR-0258. A box changing within a frosted pill's reach, on the screen and inside a rounded
    /// card that seeds its offscreen from the screen, grows the repaint over the pill's whole read,
    /// so the pill blurs this frame's pixels only.
    #[test]
    fn a_partial_repaint_beside_a_frosted_pill_matches_a_whole_one() {
        // Solid grounds: a gradient's femtovg texture does not survive a glass's mid-frame flush
        // into the next paint, which is ADR-0256's to fix.
        let pair = |colour: &str| {
            format!(
                r##"row {{ spacing = 2, children = {{ rect {{ width = 10, height = 20, background = "{colour}" }},
                    rect {{ width = 40, height = 20, radius = 10, backdrop_blur = 4 }},
                    rect {{ width = 10, height = 20, background = "#0000FFFF" }} }} }}"##
            )
        };
        let list = |colour: &str| {
            surface_96x64(&format!(
                r##"return panel {{ id = "bar", width = 96, height = 64, padding = 4, background = "#FF0000FF",
                    child = column {{ spacing = 4, children = {{ {},
                        rect {{ width = 88, height = 32, radius = 8, clip = "Rounded", padding = 4,
                            background = "#FFFFFF40", children = {{ {} }} }} }} }} }}"##,
                pair(colour),
                pair(colour)
            ))
        };
        let (before, after) = (list("#00FF00FF"), list("#FFFF00FF"));
        // Both changed boxes, as a buffer-age union hands them over, before any backdrop growth.
        let damage = PhysicalRect { x0: 4, y0: 4, x1: 22, y1: 56 };
        let region = after.repaint_region(damage).intersect(PhysicalRect { x0: 0, y0: 0, x1: 96, y1: 64 });
        assert!(region.x1 > 60, "grown over the pills' reads: {region:?}");
        assert_partial_repaint_matches(&before, &after, &[region]);
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
        let paint = |painter: &mut TextPainter, surface: &str, list: &DisplayList, region: PhysicalRect| {
            let (images, captures) = (&mut ImageCache::new(), &mut CaptureCache::default());
            let _ = execute(surface, painter, images, captures, list, 1.0, (96.0, 64.0), &[region], None);
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

    /// ADR-0258. A region past the surface's edge still clears and redraws the part inside it.
    #[test]
    fn a_region_past_the_surfaces_edge_repaints_the_part_inside() {
        let list = |colour: &str| {
            surface_96x64(&format!(
                r##"return panel {{ id = "bar", width = 96, height = 64, background = "{colour}" }}"##
            ))
        };
        let region = PhysicalRect { x0: -8, y0: -8, x1: 40, y1: 30 };
        assert_partial_repaint_matches(&list("#00FF0080"), &list("#FF000080"), &[region]);
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

    /// A transform moves the quad and its scissor together: a translated shader draws where it
    /// slid to, a scaled one past its layout box.
    #[test]
    fn a_transformed_shader_node_draws_where_it_is_painted() {
        let dir = tempfile::tempdir().unwrap();
        let frag = dir.path().join("blue.frag");
        std::fs::write(&frag, "void main() { fragColor = vec4(0.0, 0.0, 1.0, 1.0); }").unwrap();
        let src = format!(
            r##"return panel {{ id = "bar", width = 64, height = 48, child = row {{ children = {{
                shader {{ width = 16, height = 16, source = "{0}", translate = {{ x = 16 }} }},
                shader {{ width = 16, height = 16, margin = {{ left = 8, top = 24 }}, source = "{0}", scale = 2 }} }} }} }}"##,
            frag.display()
        );
        let Some(px) = paint_with_gl(&src, (64, 48), &[(20, 8), (8, 8), (18, 18), (38, 38)]) else { return };
        assert_eq!(px, [(0, 0, 255, 255), (0, 0, 0, 0), (0, 0, 255, 255), (0, 0, 255, 255)]);
    }

    /// ADR-0256. Eight-pixel stripes under a frosted pill blur to grey inside the pill only. The
    /// pill's border and child draw sharp over it, and the corner outside its arc keeps the stripe.
    /// Translucent stripes are replaced by their blur, not shown through it.
    #[test]
    fn a_backdrop_blur_frosts_the_stripes_under_a_pill_and_nothing_else() {
        let src = r##"local stops = {}
            for i = 0, 11 do
                local colour = i % 2 == 0 and "#FFFFFFFF" or "#000000FF"
                stops[#stops + 1] = { i / 12, colour }
                stops[#stops + 1] = { (i + 1) / 12, colour }
            end
            return panel { id = "bar", width = 96, height = 48, child = rect { width = "Fill", height = "Fill",
                background = { gradient = "Linear", angle = 90, stops = stops }, padding = 8,
                children = { rect { width = 80, height = 32, radius = 16, backdrop_blur = 4,
                    border_width = 2, border_color = "#00FF00FF", children = {
                        rect { width = 4, height = 4, margin = { left = 38, top = 14 }, background = "#FF0000FF" } } } } } }"##;
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
        let src = r##"local stops = {}
            for i = 0, 11 do
                local colour = i % 2 == 0 and "#FFFFFFFF" or "#000000FF"
                stops[#stops + 1] = { i / 12, colour }
                stops[#stops + 1] = { (i + 1) / 12, colour }
            end
            return panel { id = "bar", width = 96, height = 48, padding = 4,
                background = { gradient = "Linear", angle = 90, stops = stops },
                child = rect { width = 88, height = 40, radius = 8, clip = "Rounded", padding = 4,
                    children = { rect { width = 80, height = 32, radius = 16, backdrop_blur = 4,
                        background = "#0000FF40" } } } }"##;
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

    /// ADR-0256. femtovg opens a flush on the program its last one ended on without setting that
    /// program's view, which an offscreen of another size left at its own: a gradient ground
    /// drifted between frames.
    #[test]
    fn a_gradient_over_an_offscreen_paints_the_same_every_frame() {
        let src = r##"return panel { id = "bar", width = 96, height = 64, padding = 16,
            background = { gradient = "Linear", angle = 90, stops = { { 0, "#FF0000FF" }, { 1, "#0000FFFF" } } },
            child = rect { width = 40, height = 20, radius = 10, clip = "Rounded", children = {
                rect { width = 40, height = 10,
                    background = { gradient = "Linear", stops = { { 0, "#00FF00FF" }, { 1, "#000000FF" } } } },
                rect { width = 40, height = 10, background = "#FFFFFFFF" } } } }"##;
        let Some(instance) = init_headless_egl(96, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 64) else { return };
        // SAFETY: `init_headless_egl` made this context current on this thread.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        };
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

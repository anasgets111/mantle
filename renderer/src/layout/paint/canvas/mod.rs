//! Sends a built [`DisplayList`] to femtovg: boxes, borders, text, icons, images, rounded clips and
//! transforms.

mod effects;
mod shape;

use std::time::{Duration, Instant};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, CompositeOperation, ImageFlags, ImageId, Paint, Path, PixelFormat, RenderTarget};

use crate::image::capture::CaptureCache;
use crate::image::{self, Fit, ImageCache, Load};
use crate::layout::image_shader;
use crate::layout::node::{self, MaskSource, Rgba};
use crate::layout::scene::NodeId;
#[cfg(test)]
use crate::layout::scene::ResolvedNode;
use crate::text::atlas::{TextDraw, TextPainter};
use crate::text::snap::{LogicalRect, PhysicalRect};

#[cfg(test)]
use super::build;
use super::{DisplayList, Draw, DrawCmd};
use effects::{draw_backdrop, draw_layer, paint_shadow, read_target, replace};
use shape::{box_path, fill_rect, gradient_paint, paint_border};

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
/// [`build`](super::build()) EGL-free. Clears and redraws `regions` alone, each grown by
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
                if let Some((id, width, height)) = walk.captures.get(*node)
                    && let Some((fill, at)) =
                        image::capture::placement(rect, width, height, walk.captures.crop(*node), *fit)
                {
                    let mut path = Path::new();
                    path.rect(fill.x, fill.y, fill.width, fill.height);
                    let paint = Paint::image(id, at.x, at.y, at.width, at.height, 0.0, *alpha);
                    painter.canvas_mut().fill_path(&path, &paint);
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
            Draw::Shadow { shadow, radius, knockout } => {
                paint_shadow(painter.canvas_mut(), rect, *shadow, *radius, *knockout)
            }
            Draw::Layer { .. } => {
                draw_layer(painter, walk, command, target, frame);
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
    pub(super) fn init_headless_egl(width: i32, height: i32) -> Option<egl::Instance<egl::Static>> {
        init_headless_egl_two_surfaces(width, height).map(|(instance, ..)| instance)
    }

    /// A `glow` context over `instance`'s current one.
    pub(super) fn test_gl(instance: &egl::Instance<egl::Static>) -> glow::Context {
        // SAFETY: the caller's `init_headless_egl` made this context current on this thread.
        unsafe {
            glow::Context::from_loader_function(|s| {
                instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        }
    }

    /// Builds a `TextPainter` against `instance`'s already-current context, from the same
    /// `font_chain_data` `paint_surface` registers (ADR-0211).
    pub(super) fn text_painter(
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
    pub(super) fn pixel_at(canvas: &mut Canvas<OpenGl>, x: usize, y: usize) -> (u8, u8, u8, u8) {
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
    pub(super) fn paint_points(child: &str, points: &[(usize, usize)]) -> Option<Vec<(u8, u8, u8, u8)>> {
        let src = format!(r#"return panel {{ id = "bar", width = 64, height = 64, child = {child} }}"#);
        paint_with_gl(&src, (64, 64), points)
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

    /// Paints the surface `src` at `size` through [`execute`] with a GL context, which a shader
    /// quad and a backdrop need, and reads back `points`.
    pub(super) fn paint_with_gl(
        src: &str,
        size: (u32, u32),
        points: &[(usize, usize)],
    ) -> Option<Vec<(u8, u8, u8, u8)>> {
        let instance = init_headless_egl(size.0 as i32, size.1 as i32)?;
        let shaping = ShapingHandle::spawn();
        let mut painter = text_painter(&instance, &shaping, size.0, size.1)?;
        let root = resolved_surface(&Lua::new(), src, LogicalSize { width: size.0 as f32, height: size.1 as f32 });
        let gl = test_gl(&instance);
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
        let gl = test_gl(&instance);
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

    pub(super) fn surface_96x64(src: &str) -> DisplayList {
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
}

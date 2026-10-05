//! Flattens a resolved tree into a [`DisplayList`], without a canvas or GL context.
//!
//! `node::paint_style` parses during `Scene::apply`; [`build_node`] reads typed data only. Drawing
//! is parent-then-child tree order (ADR-0023), and invisible subtrees draw nothing.
//!
//! `ResolvedNode.rect` is parent-relative, so [`build_node`] accumulates an absolute origin as it
//! descends instead of trusting `rect.x`/`rect.y` as already-absolute.

use crate::layout::node::{
    self, Blend, BorderColor, BorderPaint, CaretStyle, ClipShape, EdgeInsets, Fill, PaintStyle, Radii, Rgba, StyleRun,
};
use crate::layout::scene::{NodeId, ResolvedNode};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

use super::{DisplayList, Draw, DrawCmd, LayerShader, UNCLIPPED, command_bounds, grow, grow_y, shadow_rect};

/// Focused field and draw-safe content. Masked fields carry only destination and character count,
/// never secret bytes (`shared::SecureBuffer::expose_secret`, ADR-0005). Plain fields use `NodeId`,
/// not a box: when a notification moved a field, box-keyed focus lost its caret while keystrokes
/// continued landing in the buffer; a replacement field could also inherit its text (ADR-0099).
pub enum FieldFocus<'a> {
    Masked {
        id: NodeId,
        target: &'a node::SecureSubmitTarget,
        /// `shared::SecureBuffer::grapheme_count`.
        filled: usize,
    },
    Plain {
        /// Focused node, stable across movement, resizing, and inserted siblings.
        id: NodeId,
        text: &'a str,
        /// `(anchor, caret)` byte offsets into `text` while the field takes keys, `None` when it
        /// does not, which is what hides the caret (ADR-0108).
        caret: Option<(usize, usize)>,
        /// The blink's phase: off hides the bar alone, so the selection and the line's scroll to
        /// the caret hold still.
        caret_on: bool,
    },
    Composing {
        id: NodeId,
        text: &'a str,
        selection: (usize, usize),
        preedit: &'a str,
        cursor: (i32, i32),
        caret_on: bool,
    },
}

/// Flattens `root` without touching a canvas or GL context.
#[cfg(test)]
pub fn build(root: &ResolvedNode, scale: f32, focus: Option<&FieldFocus>) -> DisplayList {
    build_with_control(root, scale, focus.map(std::slice::from_ref).unwrap_or_default(), None)
}

pub fn build_with_control(
    root: &ResolvedNode,
    scale: f32,
    focus: &[FieldFocus],
    control: Option<NodeId>,
) -> DisplayList {
    let mut commands = Vec::new();
    build_node(root, 0.0, 0.0, scale, (UNCLIPPED, root.rect), 1.0, focus, control, &mut commands);
    DisplayList { commands }
}

/// One node, then its children in paint order. Origins accumulate parent-relative rects to an
/// absolute position.
// ponytail: keep the eight scalar/context arguments; a wrapper would only bag them for one caller.
#[allow(clippy::too_many_arguments)]
fn build_node(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    (clip, surface): (PhysicalRect, LogicalRect),
    inherited_opacity: f32,
    focus: &[FieldFocus],
    control: Option<NodeId>,
    out: &mut Vec<DrawCmd>,
) {
    if !node.visible {
        return;
    }

    let rect = node.at(origin_x, origin_y);
    // The list is in buffer pixels; everything this walk reasons with stays logical until here.
    let px =
        LogicalRect { x: rect.x * scale, y: rect.y * scale, width: rect.width * scale, height: rect.height * scale };
    let cmd = |clip: PhysicalRect, draw: Draw| DrawCmd { rect: px, clip, draw: in_buffer_pixels(draw, scale) };

    // Snap this box and intersect it with ancestor clips. Wrapped text is already rewritten by
    // `fit_text_to_box`; this remains a backstop for unwrapped overflow. Clips stay rectangular
    // here; `clip = "rounded"` creates a grouped mask below.
    //
    // ponytail: `layout::hit` intersects the same rectangles but knows nothing about the arc, so a
    // pill's corner is outside its fill yet still takes a click (four pixels on a 34px control),
    // and a scoop's cut-out still takes the click and counts as input.
    // Upgrade path: hit testing should share this walk instead of a second copy of the rule.
    // The group matrix moves what is inside it, not the ancestors' clip, so the clip enters pre-matrix.
    // ponytail: a layer or shader inside a group gets only the rounded mapped clip, so a rotated child is cut by a bounding box.
    let outer = clip;
    let (parent_clip, surface) = if let Some(matrix) = node.paint_matrix(rect) {
        let physical = [matrix[0], matrix[1], matrix[2], matrix[3], matrix[4] * scale, matrix[5] * scale];
        let inverse = node::invert_affine(physical);
        // An empty clip stays empty: `transformed` would flip its inverted corners into a real rect.
        let clip = match inverse {
            Some(inverse) if !clip.is_empty() => super::transformed(inverse, clip),
            _ => clip,
        };
        // The linear part is the movement-free transform's, so its inverse shifts the effect target.
        let surface = match (node.movement.as_ref(), inverse) {
            (Some(moving), Some(inverse)) => LogicalRect {
                x: surface.x - (inverse[0] * moving.offset.0 + inverse[2] * moving.offset.1),
                y: surface.y - (inverse[1] * moving.offset.0 + inverse[3] * moving.offset.1),
                ..surface
            },
            _ => surface,
        };
        (clip, surface)
    } else {
        (clip, surface)
    };
    // An outline reaching past the box paints there too; children stay cut to the box.
    let bounds = match &node.paint {
        Some(PaintStyle::Box { radius, .. }) => radius.bounds(rect),
        _ => rect,
    };
    let clip = parent_clip.intersect(snap_to_physical(bounds, scale));
    let child_clip =
        if node.clips_children() { parent_clip.intersect(snap_to_physical(rect, scale)) } else { parent_clip };
    let effect = &node.effect;
    let read =
        snap_to_physical(grow(bounds, reach(effect.backdrop).max(shader_padding(&effect.backdrop_shader))), scale);
    let opacity = inherited_opacity * node.opacity;
    let layers = match &node.paint {
        Some(PaintStyle::Box { background, .. }) => background.as_slice(),
        _ => &[],
    };
    // ADR-0254 decision 2, ADR-0260. An opaque box draws as it did in either mode.
    let (radius, opaque, boxed) = match &node.paint {
        Some(PaintStyle::Box { radius, mask, .. }) => {
            // One opaque normal layer anywhere leaves the box opaque: a blended pixel over an
            // opaque one stays opaque. A blended node shows what is under it.
            let opaque = layers
                .iter()
                .any(|(fill, blend)| *blend == Blend::Normal && matches!(fill, Fill::Color(c) if c.a >= 1.0))
                && effect.blend == Blend::Normal
                && mask.is_none()
                && effect.blur == 0.0
                && effect.shader.is_none()
                && opacity >= 1.0;
            (radius.clone(), opaque, !opaque && !effect.content_shadow)
        }
        _ => (Radii::default(), false, false),
    };
    // A gradient cannot draw a scoop or an outline, so their box shadow is their silhouette's.
    let casts = !effect.shadows.is_empty() && (boxed || (opaque && radius.analytic()));
    let mut layered = if casts { node::Effect { shadows: Vec::new(), ..effect.clone() } } else { effect.clone() };
    let own = layer_bounds(bounds, &layered, scale);
    let reach = if casts && radius.analytic() {
        effect.shadows.iter().fold(own, |reach, shadow| {
            reach.union(snap_to_physical(grow(shadow_rect(rect, rect, *shadow), 1.5 * shadow.blur), scale))
        })
    } else if effect.layers() {
        layer_bounds(bounds, effect, scale)
    } else {
        child_clip
    };
    // A box just scrolled out still casts the shadow reaching back in; one whose shadow is out still draws.
    if parent_clip.intersect(reach).is_empty() {
        return;
    }

    // `node::paint_style` already decided the draw. An unrecognised kind stays transparent, which
    // avoids the passwordless black lock screen ADR-0052 decision 3 rejects. Opacity is baked into
    // the list because ADR-0063 skips unchanged lists; applying it in `execute` would be invisible.
    // A fully clipped node draws nothing, and its children cut to its box return on their own.
    let draw = if clip.is_empty() { None } else { draw_for(node, rect, scale, opacity, focus) };

    // A transformed node paints itself and its subtree as one group under its matrix
    // (ADR-0149), so the group is built into `out` and lifted out of it afterwards. Coordinates
    // inside stay the untransformed absolute ones this walk computes; the matrix is about the
    // node's absolute origin, so the canvas maps them at draw time. Scissors inside follow the
    // matrix too, femtovg's own rule, which is right for the node's own box.
    let start = out.len();
    let (x, y) = (rect.x, rect.y);
    let mask = match &node.paint {
        Some(PaintStyle::Box { mask: Some(mask), .. }) => Some(mask),
        _ => None,
    };
    // A backdrop shader's padding can reach back into the clip from a box that is itself outside it.
    let padded_read = shader_padding(&effect.backdrop_shader) > 0.0 && !parent_clip.intersect(read).is_empty();
    // Outside the node's own offscreen, which holds nothing to read (ADR-0256).
    if let Some(PaintStyle::Box { radius, .. }) = &node.paint
        && (effect.backdrop > 0.0 || !effect.backdrop_tone.is_identity() || effect.backdrop_shader.is_some())
        && opacity > 0.0
        && (!clip.is_empty() || padded_read)
    {
        let shader = effect.backdrop_shader.clone().map(|shader| layer_shader(shader, radius.clone()));
        let draw = Draw::Backdrop {
            sigma: effect.backdrop,
            tone: effect.backdrop_tone,
            radius: radius.clone(),
            alpha: opacity,
            shader,
        };
        out.push(cmd(parent_clip.intersect(read), draw));
    }
    // CSS `mix-blend-mode` blends the element's own shadows with it, never its backdrop filter.
    let group = out.len();
    // After the backdrop: CSS's backdrop is what precedes the element, and its shadow is part of it.
    if casts {
        let faded = effect.shadows.iter().map(|shadow| node::Shadow { color: fade(shadow.color, opacity), ..*shadow });
        if radius.analytic() {
            // CSS paints the first layer on top, so the last draws first.
            for shadow in faded.rev() {
                let draw = Draw::Shadow { shadow, radius: radius.clone(), knockout: boxed };
                out.push(blended(shadow.blend, cmd(parent_clip.intersect(reach), draw)));
            }
        } else {
            let effect = node::Effect { shadows: faded.collect(), ..node::Effect::default() };
            let black = vec![Fill::Color(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 })];
            let fill = Draw::Box {
                background: black,
                radius: radius.clone(),
                border: BorderPaint::default(),
                widths: EdgeInsets::default(),
            };
            let draw = Draw::Layer {
                effect: Box::new(effect),
                shader: None,
                silhouette: true,
                commands: vec![cmd(clip, fill)],
            };
            out.push(cmd(parent_clip.intersect(reach), draw));
        }
    }
    let body = if effect.blend == Blend::Normal { out.len() } else { group };
    // Above every background layer, under the children and the border (ADR-0331); the first layer is on top.
    let insets: Vec<DrawCmd> = match &node.paint {
        Some(PaintStyle::Box { radius, widths, .. }) if !clip.is_empty() => effect
            .inset
            .iter()
            .rev()
            .map(|shadow| {
                let shadow = node::Shadow { color: fade(shadow.color, opacity), ..*shadow };
                blended(shadow.blend, cmd(clip, Draw::InsetShadow { shadow, radius: radius.clone(), widths: *widths }))
            })
            .collect(),
        _ => Vec::new(),
    };
    let blends_layers = layers.iter().any(|(_, blend)| *blend != Blend::Normal);
    match rounded_clip(node) {
        // A mask covers the node's own paint too, as Qt's `OpacityMask` covers its item (ADR-0255).
        radius if mask.is_some() => {
            let (fill, border) = split_fill_and_border(draw);
            let mut inner = Vec::new();
            if let Some(fill) = fill {
                push_fill(&mut inner, cmd(clip, fill), layers);
            }
            inner.extend(insets);
            for child in node.painted_children() {
                build_node(child, x, y, scale, (clip, surface), opacity, focus, control, &mut inner);
            }
            inner.extend(border.map(|draw| cmd(clip, draw)));
            if !inner.is_empty() {
                let draw = if let Some(mask_node) = node.mask_child() {
                    let mut commands = Vec::new();
                    build_node(mask_node, x, y, scale, (clip, surface), 1.0, &[], None, &mut commands);
                    let split = commands.len();
                    commands.extend(inner);
                    Draw::NodeMask {
                        invert: mask.is_some_and(|mask| mask.invert),
                        radius: radius.unwrap_or_default(),
                        split,
                        commands,
                    }
                } else {
                    let box_px = (physical_edge(rect.width, scale), physical_edge(rect.height, scale));
                    let mask = mask.cloned().map(|mask| (mask, box_px));
                    Draw::Clipped { radius: radius.unwrap_or_default(), mask, commands: inner }
                };
                out.push(cmd(clip, draw));
            }
        }
        None => {
            // The border draws over an inset shadow and a blended layer, so such a box paints fill, inset, border.
            let (draw, border) =
                if insets.is_empty() && !blends_layers { (draw, None) } else { split_fill_and_border(draw) };
            if let Some(draw) = draw {
                // ponytail: ink can pass a tight line box, so text clips to one em of vertical room; upgrade: the shaped ink extents.
                let clip = match &draw {
                    Draw::Text { face, .. } if !clip.is_empty() => {
                        parent_clip.intersect(snap_to_physical(grow_y(rect, face.font_size), scale))
                    }
                    _ => clip,
                };
                push_fill(out, cmd(clip, draw), layers);
            }
            out.extend(insets);
            out.extend(border.map(|draw| cmd(clip, draw)));
            for child in node.painted_children() {
                build_node(child, x, y, scale, (child_clip, surface), opacity, focus, control, out);
            }
        }
        // Rounded order: fill, masked subtree, border. A child reaching the arc would
        // cover a border painted first.
        Some(radius) => {
            let (fill, border) = split_fill_and_border(draw);
            if let Some(fill) = fill {
                push_fill(out, cmd(clip, fill), layers);
            }
            out.extend(insets);
            let mut inner = Vec::new();
            for child in node.painted_children() {
                build_node(child, x, y, scale, (clip, surface), opacity, focus, control, &mut inner);
            }
            // A leaf has nothing to clip, so avoid the render target and composite.
            if !inner.is_empty() {
                out.push(cmd(clip, Draw::Clipped { radius, mask: None, commands: inner }));
            }
            if let Some(border) = border {
                out.push(cmd(clip, border));
            }
        }
    }
    if control == Some(node.id)
        && node::fields::common::focus_ring.read(&node.properties).unwrap_or(true)
        && rect.width >= 4.0
        && rect.height >= 4.0
        && !clip.is_empty()
    {
        let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let border = |color| {
            BorderPaint::Edges(BorderColor {
                top: Some(color),
                right: Some(color),
                bottom: Some(color),
                left: Some(color),
            })
        };
        let widths = EdgeInsets { top: 2.0, right: 2.0, bottom: 2.0, left: 2.0 };
        out.push(cmd(
            clip,
            Draw::Box { background: Vec::new(), radius: Radii::default(), border: border(white), widths },
        ));
        let inner = LogicalRect {
            x: px.x + 2.0 * scale,
            y: px.y + 2.0 * scale,
            width: px.width - 4.0 * scale,
            height: px.height - 4.0 * scale,
        };
        out.push(DrawCmd {
            rect: inner,
            clip,
            draw: in_buffer_pixels(
                Draw::Box { background: Vec::new(), radius: Radii::default(), border: border(black), widths },
                scale,
            ),
        });
    }
    if layered.layers() && out.len() > body {
        let commands: Vec<DrawCmd> = out.drain(body..).collect();
        // A transformed child overflowing the box keeps the overflow it has without the layer.
        let bounds = commands.iter().map(command_bounds).filter(|r| !r.is_empty()).fold(own, PhysicalRect::union);
        // ponytail: a negative spread pulls in content from further out than this. Upgrade path:
        // invert `shadow_rect` about the box.
        let pad = layered.shadows.iter().fold(0.0_f32, |pad, shadow| {
            pad.max(self::reach(shadow.blur / 2.0) + shadow.offset.0.abs().max(shadow.offset.1.abs()))
        });
        let target = snap_to_physical(
            grow(surface, pad.max(self::reach(layered.blur)).max(shader_padding(&layered.shader))),
            scale,
        );
        let shader = layered.shader.take().map(|shader| layer_shader(shader, radius));
        let draw = Draw::Layer { effect: Box::new(layered), shader, silhouette: false, commands };
        out.push(cmd(parent_clip.intersect(bounds).intersect(target), draw));
    }
    if let Some(matrix) = node.paint_matrix(rect) {
        let commands: Vec<DrawCmd> = out.drain(start..).collect();
        out.push(cmd(outer, Draw::Transformed { matrix, commands }));
    }
}

/// `draw`'s own geometry in buffer pixels; a group's subtree was converted as it was built.
/// Exhaustive, so a new draw has to say what it scales. Image and icon boxes are cache keys
/// [`draw_for`] rounds itself, and text shapes at its logical size for `canvas::execute` to rasterize at
/// the buffer scale.
fn in_buffer_pixels(draw: Draw, scale: f32) -> Draw {
    let shadow = |shadow: node::Shadow| node::Shadow {
        blur: shadow.blur * scale,
        offset: (shadow.offset.0 * scale, shadow.offset.1 * scale),
        spread: shadow.spread * scale,
        ..shadow
    };
    match draw {
        Draw::Box { background, radius, border, widths } => {
            Draw::Box { background, radius: radius * scale, border, widths: widths.scaled(scale) }
        }
        Draw::Path(mut path) => {
            if scale != 1.0 {
                std::rc::Rc::make_mut(&mut path.commands).for_each_pixel(|point| *point *= scale);
            }
            path.stroke_width *= scale;
            path.shift = (path.shift.0 * scale, path.shift.1 * scale);
            Draw::Path(path)
        }
        Draw::Clipped { radius, mask, commands } => Draw::Clipped { radius: radius * scale, mask, commands },
        Draw::NodeMask { invert, radius, split, commands } => {
            Draw::NodeMask { invert, radius: radius * scale, split, commands }
        }
        // The affine's linear part is the same in either unit; only its translation scales.
        Draw::Transformed { mut matrix, commands } => {
            matrix[4] *= scale;
            matrix[5] *= scale;
            Draw::Transformed { matrix, commands }
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
            shader: shader.map(|shader| LayerShader { radius: shader.radius * scale, ..shader }),
            silhouette,
            commands,
        },
        Draw::Backdrop { sigma, tone, radius, alpha, shader } => Draw::Backdrop {
            sigma: sigma * scale,
            tone,
            radius: radius * scale,
            alpha,
            shader: shader.map(|shader| LayerShader { radius: shader.radius * scale, ..shader }),
        },
        draw @ (Draw::Text { .. }
        | Draw::Icon { .. }
        | Draw::Image { .. }
        | Draw::Capture { .. }
        | Draw::Shader { .. }) => draw,
    }
}

/// The non-zero radii of a node whose children use a rounded clip.
fn rounded_clip(node: &ResolvedNode) -> Option<Radii> {
    match &node.paint {
        Some(PaintStyle::Box { clip: ClipShape::Rounded, radius, .. }) if !radius.is_zero() => Some(radius.clone()),
        _ => None,
    }
}

/// Splits a box into fill and border so [`build_node`] can mask children between them. Other draws
/// stay whole; [`rounded_clip`] only returns for boxes.
fn split_fill_and_border(draw: Option<Draw>) -> (Option<Draw>, Option<Draw>) {
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
fn fade(color: Rgba, opacity: f32) -> Rgba {
    Rgba { a: color.a * opacity, ..color }
}

fn fade_fill(fill: &Fill, opacity: f32) -> Fill {
    match fill {
        Fill::Color(color) => Fill::Color(fade(*color, opacity)),
        Fill::Gradient(gradient) => Fill::Gradient(fade_gradient(gradient, opacity)),
    }
}

fn fade_gradient(gradient: &node::Gradient, opacity: f32) -> node::Gradient {
    node::Gradient {
        stops: gradient.stops.iter().map(|(at, color)| (*at, fade(*color, opacity))).collect(),
        ..*gradient
    }
}

/// Multiplies border-edge alpha; absent edges stay absent.
fn fade_border(border: &BorderPaint, opacity: f32) -> BorderPaint {
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

/// Converts a node's parsed paint to a draw. `scale` supplies physical image size and `focus`
/// supplies field content; malformed properties already failed `Scene::apply`.
///
/// Takes the node rather than its `paint`, because an `image` reads three things off it (the
/// paint, the source it last had a texture for, and any dissolve crossing between them), and the
/// pass supplies only the geometry.
fn draw_for(node: &ResolvedNode, rect: LogicalRect, scale: f32, opacity: f32, focus: &[FieldFocus]) -> Option<Draw> {
    let node_id = node.id;
    let retained = node.displayed_source.as_deref();
    let dissolve = node.dissolve.as_deref();
    match node.paint.as_ref()? {
        PaintStyle::Path(path) => Some(Draw::Path(node::VectorPath {
            commands: path.commands.clone(),
            fill: path.fill.as_ref().map(|fill| fade_fill(fill, opacity)),
            stroke: path.stroke.as_ref().map(|fill| fade_fill(fill, opacity)),
            stroke_width: path.stroke_width,
            stroke_cap: path.stroke_cap,
            stroke_join: path.stroke_join,
            trim: path.trim,
            trim_axis: path.trim_axis,
            shift: path.shift,
        })),
        // The shared paint of `rect`/`row`/`column` and all four surface roles: background
        // fill, then borders. `clip` is not read here: it decides what this node's *children* are
        // cut to, `build_node`'s question, not this one's.
        PaintStyle::Box { background, radius, border, widths, clip: _, mask: _ } => Some(Draw::Box {
            background: background.iter().map(|(fill, _)| fade_fill(fill, opacity)).collect(),
            radius: radius.clone(),
            border: fade_border(border, opacity),
            widths: *widths,
        }),

        // `text`: `content` through `TextPainter`, at `rect`, coloured by `foreground`. `elide`,
        // `wrap` and `max_lines` are absent on purpose: `Scene::apply` already rewrote `content` to
        // the string that fits (ellipsized, or line-broken with `\n`) in the only place the box
        // width and the shaping worker are both in reach.
        PaintStyle::Text { content, runs, face, color, align, elide: _, wrap: _, max_lines: _, elided: _ } => {
            Some(Draw::Text {
                content: content.clone(),
                runs: runs
                    .iter()
                    .map(|run| StyleRun { color: run.color.map(|c| fade(c, opacity)), ..run.clone() })
                    .collect(),
                face: face.clone(),
                color: fade(*color, opacity),
                align: *align,
                centered: false,
                caret: None,
                caret_on: false,
                caret_style: CaretStyle::plain(face.font_size, fade(*color, opacity)),
            })
        }

        // Icons use `Contain` and the shorter edge: `size` is a bounding-box diameter.
        PaintStyle::Icon { name, color } => Some(Draw::Icon {
            name: name.clone(),
            px: physical_edge(rect.width.min(rect.height), scale),
            alpha: opacity,
            color: *color,
        }),

        // Image source and fit (ADR-0054 decision 3). Empty source draws nothing; both physical
        // edges enter the cache because `Cover` may scale an SVG past the shorter edge (ADR-0122).
        // `retained` is what goes *under* the draw, and it is one of two things: mid-dissolve the
        // picture being crossed away from, otherwise the one the node is still covering a decoding
        // source with. One field because the node is never doing both: `displayed_source` has
        // already moved on to `source` by the time a dissolve starts.
        PaintStyle::Image { source, fit, load, retain, transition, source_blur, radius } => {
            (!source.is_empty()).then(|| {
                // What goes under the draw. Dropped once the node draws what it names: an equal pair in
                // the list would be one more thing to compare, and its disappearance ends the cover.
                let cover = match dissolve {
                    Some(dissolve) => Some(dissolve.from.clone()),
                    None => retained.filter(|_| *retain).filter(|last| *last != source.as_str()).map(str::to_string),
                };
                let has_cover = cover.is_some();
                Draw::Image {
                    node: node_id,
                    // Mid-dissolve the node draws the run's own destination, not whatever a later pass
                    // has since resolved: a third source arriving would otherwise drop the picture this
                    // run is halfway to and cross to one with no texture yet (ADR-0183).
                    source: dissolve.map_or_else(|| source.clone(), |dissolve| dissolve.to.clone()),
                    fit: *fit,
                    box_px: (physical_edge(rect.width, scale), physical_edge(rect.height, scale)),
                    alpha: opacity,
                    load: *load,
                    retained: cover,
                    shader: dissolve.and_then(|dissolve| {
                        dissolve
                            .spec
                            .shader
                            .clone()
                            .map(|path| (path, dissolve.spec.params.clone(), sampler_files(&dissolve.spec.images)))
                    }),
                    dissolve: match dissolve {
                        Some(dissolve) => Some(dissolve.progress),
                        // A declared transition still covering a gap opens its cross *here*, at zero,
                        // before anything has proved the incoming texture exists, because asking for
                        // the draw is the only way to prove it (ADR-0183). Drawing the incoming at full
                        // alpha on that frame and starting the cross on the next one shows it whole,
                        // snaps back to the outgoing, and only then crosses.
                        None => (transition.is_some() && has_cover).then_some(0.0),
                    },
                    blur_px: physical_blur(*source_blur, scale),
                    radius: radius.clone() * scale,
                }
            })
        }

        // A `textfield` shows its placeholder until focused, then one mask character per typed
        // character. Wrong-password feedback costs a two-second `pam_fail_delay`; three failures
        // trigger `pam_faillock` and a ten-minute lockout. `retarget_secure_submit` zeroizes the
        // buffer on focus changes, so only the focused field can show typed state.
        PaintStyle::TextField {
            target, placeholder, placeholder_color, caret: bar, mask, face, color, align, ..
        } => {
            // The first entry for this node wins: the focused field, then any parked draft. Another
            // node's focus, masked or not, leaves this one to its parked draft or placeholder.
            let (content, caret, caret_on, runs, is_placeholder) = focus
                .iter()
                .find_map(|field| match field {
                    // An empty masked field remains a prompt.
                    FieldFocus::Masked { id, target: focused, filled }
                        if *filled > 0
                            && *id == node_id
                            && target.as_ref().is_some_and(|declared| declared == *focused) =>
                    {
                        Some((mask.repeat(*filled), None, false, Vec::new(), false))
                    }
                    // Empty focused fields show the placeholder rather than a bare caret (ADR-0135):
                    // the caret-only rule hid the prompt of every `autofocus` field, which holds the
                    // keyboard from the first frame. Keep `target.is_none()` beside the id: the same
                    // node may gain `secure_submit`, and a masked field must never draw plain text.
                    FieldFocus::Composing { id, text, selection, preedit, cursor, caret_on }
                        if *id == node_id && target.is_none() =>
                    {
                        let (content, preedit_range, caret) =
                            super::compose_preedit(text, *selection, preedit, *cursor);
                        let scroll_caret = Some(caret.unwrap_or((preedit_range.end, preedit_range.end)));
                        let runs = vec![StyleRun {
                            range: preedit_range,
                            bold: false,
                            italic: false,
                            underline: true,
                            color: None,
                            href: None,
                        }];
                        Some((content, scroll_caret, *caret_on && caret.is_some(), runs, false))
                    }
                    FieldFocus::Plain { id, text, caret, caret_on } if *id == node_id && target.is_none() => {
                        Some(match text.is_empty() && !placeholder.is_empty() {
                            true => (placeholder.clone(), None, false, Vec::new(), true),
                            // The draft remains visible without a caret (ADR-0108).
                            false => (text.to_string(), *caret, *caret_on, Vec::new(), false),
                        })
                    }
                    _ => None,
                })
                .unwrap_or_else(|| (placeholder.clone(), None, false, Vec::new(), true));
            let color = if is_placeholder { placeholder_color } else { color };
            // An empty field with no placeholder still draws, for the caret alone (ADR-0135
            // decision 2).
            (!content.is_empty() || caret.is_some()).then_some(Draw::Text {
                content: content.into(),
                runs,
                face: face.clone(),
                color: fade(*color, opacity),
                align: *align,
                centered: true,
                caret,
                caret_on,
                caret_style: CaretStyle { color: fade(bar.color, opacity), ..*bar },
            })
        }

        // An empty capture target draws nothing, as an empty image source does.
        PaintStyle::Capture { target, fit, live, paint_cursor, region } => {
            (!target.name().is_empty()).then(|| Draw::Capture {
                node: node_id,
                target: target.clone(),
                fit: *fit,
                alpha: opacity,
                live: *live,
                paint_cursor: *paint_cursor,
                region: *region,
            })
        }

        PaintStyle::Shader { source, progress, params, images } => (!source.is_empty()).then(|| Draw::Shader {
            source: source.into(),
            version: crate::image::FileVersion::read(source.as_ref()),
            progress: *progress,
            params: params.clone(),
            images: sampler_files(images),
            alpha: opacity,
        }),
    }
}

/// One logical edge in physical pixels, rounded and floored at 1. `ImageCache` keys on this
/// integer.
fn physical_edge(logical: f32, scale: f32) -> u32 {
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
fn physical_blur(logical: f32, scale: f32) -> u32 {
    (logical * scale).round() as u32
}

/// How far a Gaussian of `sigma` spreads: the 3 sigma its kernel samples (ADR-0262).
fn reach(sigma: f32) -> f32 {
    3.0 * sigma
}

/// How far past the box `effect.shader` reads and draws, logical px.
fn shader_padding(shader: &Option<node::EffectShader>) -> f32 {
    shader.as_ref().map_or(0.0, |shader| shader.padding)
}

/// `shader` as a draw carries it: the file's version, so an edit repaints, and the outline.
fn layer_shader(shader: node::EffectShader, radius: Radii) -> LayerShader {
    LayerShader {
        version: crate::image::FileVersion::read(&shader.source),
        source: shader.source,
        params: shader.params,
        images: sampler_files(&shader.images),
        progress: shader.progress,
        radius,
    }
}

fn sampler_files(images: &[node::ShaderImage]) -> Vec<super::SamplerFile> {
    let version = |path: &String| crate::image::FileVersion::read(path.as_ref());
    images.iter().map(|(name, path)| (name.clone(), path.clone(), version(path))).collect()
}

/// `command` composited by `blend`: a normal one as it is, any other one as a layer of its own,
/// whose offscreen blends onto what is under it.
fn blended(blend: Blend, command: DrawCmd) -> DrawCmd {
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

/// Pushes a box's `fill`, its blended `layers` split out bottom-up: a run of normal layers stays
/// one box, and each blended layer blends onto everything drawn under it.
// ponytail: one offscreen and blend pass per blended layer; upgrade: one pass over one copy.
fn push_fill(out: &mut Vec<DrawCmd>, fill: DrawCmd, layers: &[(Fill, Blend)]) {
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
fn layer_bounds(rect: LogicalRect, effect: &node::Effect, scale: f32) -> PhysicalRect {
    let shadow_reach = effect.shadows.iter().fold(0.0_f32, |most, shadow| most.max(reach(shadow.blur / 2.0)));
    let padded = grow(rect, shadow_reach.max(reach(effect.blur)).max(shader_padding(&effect.shader)));
    let own = snap_to_physical(padded, scale);
    effect
        .shadows
        .iter()
        .fold(own, |own, shadow| own.union(snap_to_physical(shadow_rect(rect, padded, *shadow), scale)))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{IMAGE_MASKED, effect_surface, effect_surface_src, masked, pins, resolved_surface};
    use super::*;

    use mlua::Lua;

    use crate::layout::node::{MoveTween, TextAlign};
    use crate::layout::scene::LogicalSize;

    // display list (`build`), the seam that needs no EGL context

    #[test]
    fn a_move_paints_into_a_parent_from_outside_the_final_layout_clip() {
        for (child, offset, blurred) in [
            (r##"rect { width = 10, height = 10, margin = { left = 100 }, background = "#ffffff" }"##, -20.0, false),
            (
                r##"rect { width = 10, height = 10, margin = { left = 150 }, background = "#ffffff", effect = { blur = 4 } }"##,
                -150.0,
                true,
            ),
            (
                r##"rect { width = 10, height = 10, margin = { left = 150 },
                children = { rect { width = 10, height = 10, background = "#ffffff", effect = { blur = 4 } } } }"##,
                -150.0,
                true,
            ),
        ] {
            let src = format!("return panel {{ id = 'bar', width = 100, height = 20, child = {child} }}");
            let mut tree = resolved_surface(&Lua::new(), &src, LogicalSize { width: 100.0, height: 20.0 });
            let before = build(&tree, 1.0, None);
            tree.children[0].movement = Some(Box::new(MoveTween::test((offset, 0.0))));
            let list = build(&tree, 1.0, None);
            let moved =
                list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Transformed { .. })).expect("moving subtree");
            let Draw::Transformed { commands, .. } = &moved.draw else { unreachable!() };
            if blurred {
                let layer = commands.iter().find(|cmd| matches!(cmd.draw, Draw::Layer { .. })).expect("blurred layer");
                assert!(!layer.clip.is_empty(), "blurred content remains in the moved surface: {src}");
            } else {
                assert!(commands.iter().any(|cmd| matches!(cmd.draw, Draw::Box { .. })), "the box paints at x=80..90");
                assert!(list.damage_since(&before, true).iter().any(|rect| rect.x0 <= 80 && rect.x1 >= 90));
            }
        }
    }

    fn text_align_of(list: &DisplayList) -> TextAlign {
        list.commands
            .iter()
            .find_map(|cmd| match &cmd.draw {
                Draw::Text { align, .. } => Some(*align),
                _ => None,
            })
            .expect("expected a text draw")
    }

    /// ADR-0183. The frame that first has the incoming texture must already be drawing the cross,
    /// or it shows the incoming at full alpha for one frame and the cross then starts by jumping
    /// back to the outgoing. Readiness is only knowable by asking for the draw, so the ask happens
    /// at zero.
    #[test]
    fn a_transition_waiting_on_its_incoming_texture_draws_it_at_zero_rather_than_at_full_alpha() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true,
                transition = { duration = 400, easing = "linear" },
                width = "fill", height = "fill" } }"##;
        let mut tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let image_draw = |tree: &ResolvedNode| {
            build(tree, 1.0, None).commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Image { retained, dissolve, .. } => Some((retained.clone(), *dissolve)),
                _ => None,
            })
        };

        // Holding the old picture, the new one not yet drawn: no dissolve has started, because
        // nothing has proved the texture exists.
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(
            image_draw(&tree),
            Some((Some("/tmp/old.png".to_string()), Some(0.0))),
            "the incoming is asked for at zero, so the frame that first has it still shows the outgoing"
        );

        // `retain` with no transition keeps the plain cover: it has no cross to open on.
        let plain = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true, retain = true,
                width = "fill", height = "fill" } }"##;
        let mut tree = resolved_surface(&lua, plain, LogicalSize { width: 200.0, height: 40.0 });
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(image_draw(&tree), Some((Some("/tmp/old.png".to_string()), None)));
    }

    /// ADR-0181. Mid-dissolve the list carries the outgoing picture and the alpha the incoming is
    /// drawn over it at, in the same field the gap cover uses: the node is never doing both.
    #[test]
    fn a_dissolving_image_carries_the_outgoing_picture_and_the_alpha_to_draw_the_incoming_at() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true,
                transition = { duration = 400, easing = "linear" },
                width = "fill", height = "fill" } }"##;
        let mut tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let image_draw = |tree: &ResolvedNode| {
            build(tree, 1.0, None).commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Image { retained, dissolve, .. } => Some((retained.clone(), *dissolve)),
                _ => None,
            })
        };
        assert_eq!(image_draw(&tree), Some((None, None)), "nothing has landed, so nothing is crossing");

        // What `note_landed_images` leaves behind: the source moved on, the outgoing on the run.
        tree.children[0].displayed_source = Some("/tmp/new.png".to_string());
        tree.children[0].dissolve = Some(Box::new(node::Dissolve {
            from: "/tmp/old.png".to_string(),
            to: "/tmp/new.png".to_string(),
            started: std::time::Instant::now(),
            spec: node::TransitionSpec {
                duration: std::time::Duration::from_millis(400),
                easing: Default::default(),
                shader: None,
                params: Vec::new(),
                images: Vec::new(),
            },
            progress: 0.25,
        }));
        assert_eq!(image_draw(&tree), Some((Some("/tmp/old.png".to_string()), Some(0.25))));

        // Both are drawn, so both are pinned; losing the outgoing mid-cross is a hole in the frame.
        let pinned = pins(&build(&tree, 1.0, None));
        assert_eq!(
            pinned,
            vec![
                (std::path::PathBuf::from("/tmp/new.png"), (200, 40)),
                (std::path::PathBuf::from("/tmp/old.png"), (200, 40)),
            ]
        );

        // `transition` implies `retain`, so the same node covers a gap without the property being
        // written twice, and covering with a transition declared opens the cross at zero, which
        // is the subject of its own test below.
        tree.children[0].dissolve = None;
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(image_draw(&tree), Some((Some("/tmp/old.png".to_string()), Some(0.0))));
    }

    /// ADR-0180. The cover only reaches the list while the node is behind its own source, it is
    /// pinned so `trim` cannot free the texture it is covering with, and `retain` is what turns it
    /// on: without the property the same stale `displayed_source` says nothing.
    #[test]
    fn a_retaining_image_carries_the_source_it_still_shows_until_the_named_one_catches_up() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true, retain = true,
                width = "fill", height = "fill" } }"##;
        let mut tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });

        // Nothing has landed yet, so there is nothing to cover the gap with.
        let cover_of = |tree: &ResolvedNode| {
            build(tree, 1.0, None).commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Image { retained, .. } => Some(retained.clone()),
                _ => None,
            })
        };
        assert_eq!(cover_of(&tree), Some(None), "an image that never drew has no cover");

        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(cover_of(&tree), Some(Some("/tmp/old.png".to_string())));
        let list = build(&tree, 1.0, None);
        let pinned = pins(&list);
        assert_eq!(
            pinned,
            vec![
                (std::path::PathBuf::from("/tmp/new.png"), (200, 40)),
                (std::path::PathBuf::from("/tmp/old.png"), (200, 40)),
            ],
            "both are pinned on the box they are drawn at, or the cover is evicted mid-cover"
        );
        assert!(list.draws_any_of(&[std::path::PathBuf::from("/tmp/old.png")]));

        // Caught up: the pair is equal, so the list settles instead of carrying a second copy.
        tree.children[0].displayed_source = Some("/tmp/new.png".to_string());
        assert_eq!(cover_of(&tree), Some(None));

        // The same stale state without the property draws nothing while the source decodes.
        let plain = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true,
                width = "fill", height = "fill" } }"##;
        let mut tree = resolved_surface(&lua, plain, LogicalSize { width: 200.0, height: 40.0 });
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(cover_of(&tree), Some(None), "`retain` is what carries the cover, not the state");
    }

    #[test]
    fn a_text_run_is_left_aligned_in_its_box_unless_it_says_otherwise() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = text { content = "hi", foreground = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        assert_eq!(text_align_of(&build(&tree, 1.0, None)), TextAlign::Start);
    }

    #[test]
    fn a_declared_text_align_reaches_the_display_list() {
        for (declared, expected) in
            [("center", TextAlign::Center), ("end", TextAlign::End), ("start", TextAlign::Start)]
        {
            let lua = Lua::new();
            let src = format!(
                r##"return panel {{ id = "bar", width = 200, height = 40,
                    child = text {{ content = "hi", foreground = "#ffffffff", text_align = "{declared}" }} }}"##
            );
            let tree = resolved_surface(&lua, &src, LogicalSize { width: 200.0, height: 40.0 });
            assert_eq!(text_align_of(&build(&tree, 1.0, None)), expected, "text_align = {declared:?}");
        }
    }

    /// A masked field aligns the same way a `text` does, because both produce a `Draw::Text` and a
    /// password prompt that centres its dots is a normal thing to want.
    #[test]
    fn a_textfield_carries_its_own_alignment_too() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = 180, height = 24, placeholder = "password", foreground = "#ffffffff", text_align = "center" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        assert_eq!(text_align_of(&build(&tree, 1.0, None)), TextAlign::Center);
    }

    /// The alignment is in the list, so switching it repaints. Same argument as the opacity case:
    /// ADR-0063 skips a repaint when the list compares equal.
    #[test]
    fn changing_only_the_text_alignment_changes_the_display_list() {
        let size = LogicalSize { width: 200.0, height: 40.0 };
        let src = |align: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 200, height = 40,
                    child = text {{ content = "hi", foreground = "#ffffffff", text_align = "{align}" }} }}"##
            )
        };
        let a = build(&resolved_surface(&Lua::new(), &src("start"), size), 1.0, None);
        let b = build(&resolved_surface(&Lua::new(), &src("center"), size), 1.0, None);
        assert_ne!(a, b);
    }

    fn box_alpha(cmd: &DrawCmd) -> f32 {
        match &cmd.draw {
            Draw::Box { background, .. } if let [Fill::Color(color)] = background.as_slice() => color.a,
            other => panic!("expected a filled box, got {other:?}"),
        }
    }

    /// The whole point of the property: one `opacity` on a container fades everything under it,
    /// rather than each descendant needing its own.
    #[test]
    fn a_parents_opacity_reaches_every_descendant() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            child = rect { width = 100, height = 20, background = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert_eq!(box_alpha(&list.commands[1]), 0.5, "the child fades with the panel it sits in");
    }

    /// Multiplied down the chain rather than replaced, so nothing inside a faded panel can come
    /// back solid.
    #[test]
    fn a_nested_opacity_multiplies_with_its_ancestors_rather_than_replacing_them() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            child = rect { width = 100, height = 20, opacity = 0.5, background = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert_eq!(box_alpha(&list.commands[1]), 0.25, "half of a half");
    }

    /// A colour that was already translucent keeps its own alpha as a factor: a config writing
    /// both meant both.
    #[test]
    fn an_opacity_multiplies_the_alpha_a_colour_already_carried() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            background = "#ffffff80" }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        let expected = (0x80 as f32 / 255.0) * 0.5;
        assert!((box_alpha(&list.commands[0]) - expected).abs() < 1e-6);
    }

    /// ADR-0063 skips a repaint when the new list equals the last one, so a fade that did not
    /// change the list would be a change the surface never painted.
    #[test]
    fn changing_only_the_opacity_changes_the_display_list() {
        let solid = Lua::new();
        let faded = Lua::new();
        let src = |opacity: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 200, height = 40, opacity = {opacity},
                    child = text {{ content = "12:00", foreground = "#ffffffff" }} }}"##
            )
        };
        let size = LogicalSize { width: 200.0, height: 40.0 };
        let a = build(&resolved_surface(&solid, &src("1.0"), size), 1.0, None);
        let b = build(&resolved_surface(&faded, &src("0.4"), size), 1.0, None);
        assert_ne!(a, b, "the alpha is in the list, not applied on the way to the canvas");
    }

    /// Blitted draws carry the alpha separately, because `Paint::image` takes it as an argument
    /// where a fill can bake it into the colour.
    #[test]
    fn an_icon_carries_the_faded_alpha_rather_than_a_tinted_colour() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.25,
            child = icon { name = "network-wireless", size = 16 } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        let icon = list.commands.iter().find_map(|cmd| match &cmd.draw {
            Draw::Icon { alpha, .. } => Some(*alpha),
            _ => None,
        });
        assert_eq!(icon, Some(0.25));
    }

    /// Every border edge fades, and an edge with no colour stays absent rather than becoming a
    /// transparent one.
    #[test]
    fn a_border_fades_edge_by_edge_and_an_absent_edge_stays_absent() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            border_color = { top = "#ff0000ff" }, border_width = { top = 2 } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        let Draw::Box { border: BorderPaint::Edges(colors), .. } = &list.commands[0].draw else {
            panic!("expected a box with edge colours");
        };
        assert_eq!(colors.top.unwrap().a, 0.5);
        assert!(colors.bottom.is_none(), "an edge the config never coloured is not faded into existence");
    }

    /// A gradient border fades through its stops like a gradient background.
    #[test]
    fn a_gradient_border_fades_every_stop() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5, border_width = 2,
            border_color = { gradient = "linear", stops = { { 0, "#ff0000ff" }, { 1, "#0000ffff" } } } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        let Draw::Box { border: BorderPaint::Gradient(gradient), .. } = &list.commands[0].draw else {
            panic!("expected a gradient border");
        };
        assert_eq!(gradient.stops.iter().map(|(_, c)| c.a).collect::<Vec<_>>(), [0.5, 0.5]);
    }

    /// `opacity = 0` and `visible = false` are different, deliberately: a transparent node still
    /// lays out and still takes pointer events, which is what lets a fade run without the layout
    /// jumping. So it still produces a draw, at zero alpha.
    #[test]
    fn a_fully_transparent_node_still_draws_rather_than_vanishing_from_the_list() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0,
            background = "#ffffffff" }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert_eq!(box_alpha(&list.commands[0]), 0.0);
    }

    /// The property this whole optimisation rests on: same tree in, same list out. If this can
    /// ever fail for an unchanged scene, `paint_surface`'s skip repaints every frame anyway and
    /// the wallpaper is back to redrawing at the clock's cadence.
    #[test]
    fn the_same_tree_builds_an_equal_list_so_an_unchanged_surface_can_be_skipped() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, background = "#112233ff",
            child = text { content = "12:00:00", foreground = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert!(!list.commands.is_empty(), "an empty list would make this pass for the wrong reason");
        assert_eq!(list, build(&tree, 1.0, None));
    }

    /// The other half, and the one that would make a skip dangerous if it failed: a changed
    /// string has to change the list, or the surface would keep showing a stale clock forever.
    #[test]
    fn changing_only_a_texts_content_changes_the_list() {
        let lua = Lua::new();
        let panel = |content: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 200, height = 40,
                child = text {{ content = "{content}", foreground = "#ffffffff" }} }}"##
            )
        };
        let size = LogicalSize { width: 200.0, height: 40.0 };
        let before = build(&resolved_surface(&lua, &panel("12:00:00"), size), 1.0, None);
        let after = build(&resolved_surface(&Lua::new(), &panel("12:00:01"), size), 1.0, None);
        assert_ne!(before, after, "a new seconds digit must reach the list, or the paint gets skipped");
    }

    /// `visible = false` collapses the node and everything under it, the same rule the tree walk
    /// this replaced applied, so an invisible subtree costs nothing to compare, not just
    /// nothing to draw.
    #[test]
    fn an_invisible_node_and_its_children_contribute_nothing() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, background = "#112233ff",
            child = rect { visible = false, background = "#ff0000ff", width = 50, height = 20,
                   children = { text { content = "hidden", foreground = "#ffffffff" } } } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        assert!(
            !list.commands.iter().any(|c| matches!(&c.draw, Draw::Text { content, .. } if &**content == "hidden")),
            "an invisible node's child reached the list: {list:?}"
        );
    }

    /// Draw order is tree order, which is what makes ADR-0023's stacking model come
    /// out right: a child is painted after the parent it covers.
    #[test]
    fn a_parents_box_is_listed_before_its_childs() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, background = "#112233ff",
            child = rect { background = "#445566ff", width = 50, height = 20 } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        let backgrounds: Vec<_> = list
            .commands
            .iter()
            .filter_map(|c| match &c.draw {
                Draw::Box { background, .. } => match background.as_slice() {
                    [Fill::Color(color)] => Some(*color),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(backgrounds.len(), 2, "both boxes should be listed: {list:?}");
        // #112233 then #445566: the root's own fill is listed first, the child that covers it
        // second, so replaying the list in order reproduces the stacking.
        assert_eq!(
            (backgrounds[0].b * 255.0).round() as u8,
            0x33,
            "the panel root's own background must be listed first, got {backgrounds:?}"
        );
        assert_eq!(
            (backgrounds[1].b * 255.0).round() as u8,
            0x66,
            "the child must be listed after the parent it paints over"
        );
    }

    /// A tight `line_height` leaves ink outside the line box: the clip reaches past it vertically only, within ancestors' clips.
    #[test]
    fn a_texts_clip_reaches_past_a_tight_line_box_but_only_vertically() {
        let src = |clip: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 100, height = 60, padding = 20, clip = "{clip}",
                    child = text {{ content = "Hg", width = 40, font_size = 20, line_height = 0.5 }} }}"##
            )
        };
        let list =
            build(&resolved_surface(&Lua::new(), &src("none"), LogicalSize { width: 100.0, height: 60.0 }), 1.0, None);
        let text = list.commands.iter().find(|c| matches!(c.draw, Draw::Text { .. })).unwrap();
        assert_eq!((text.rect.y, text.rect.height), (20.0, 10.0));
        assert_eq!((text.clip.x0, text.clip.x1), (20, 60), "width still bounds overflow");
        assert!(text.clip.y0 <= 0 && text.clip.y1 >= 40, "the ink above and below the box: {:?}", text.clip);
        let changed = |content: &str| {
            let src = src("none").replace("Hg", content);
            build(&resolved_surface(&Lua::new(), &src, LogicalSize { width: 100.0, height: 60.0 }), 1.0, None)
        };
        let damage = changed("Hy").damage_since(&list, true);
        assert!(damage.iter().any(|r| r.y1 >= 40 && r.y0 <= 0), "damage covers the ink: {damage:?}");
        let boxed =
            build(&resolved_surface(&Lua::new(), &src("box"), LogicalSize { width: 100.0, height: 60.0 }), 1.0, None);
        let text = boxed.commands.iter().find(|c| matches!(c.draw, Draw::Text { .. })).unwrap();
        assert!(text.clip.y0 >= 0 && text.clip.y1 <= 60, "a clipping ancestor still cuts the ink: {:?}", text.clip);
    }

    /// A child's clip is its own box intersected with its parent's, never wider. This is the
    /// invariant that lets [`execute`] call `scissor` outright instead of rebuilding an
    /// `intersect_scissor` nest.
    #[test]
    fn a_childs_clip_never_escapes_its_parents_box() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { background = "#445566ff", width = 40, height = 10, clip = "box",
                children = { rect { background = "#778899ff", width = 500, height = 500 } } } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        let clips: Vec<_> = list.commands.iter().map(|c| c.clip).collect();
        for pair in clips.windows(2) {
            let (outer, inner) = (pair[0], pair[1]);
            assert!(
                inner.x0 >= outer.x0 && inner.y0 >= outer.y0 && inner.x1 <= outer.x1 && inner.y1 <= outer.y1,
                "a descendant clip {inner:?} escaped its ancestor {outer:?}"
            );
        }
    }

    /// A parent's clip stays in the parent's space: the child's group matrix must not carry it along.
    #[test]
    fn a_transformed_child_stays_cut_to_its_parents_box() {
        // The painted bounds of every fill: mapped out through each group's matrix, cut by its clip.
        fn painted(commands: &[DrawCmd], groups: &mut Vec<(node::Affine, PhysicalRect)>, out: &mut Vec<PhysicalRect>) {
            for command in commands {
                match &command.draw {
                    Draw::Transformed { matrix, commands } => {
                        groups.push((*matrix, command.clip));
                        painted(commands, groups, out);
                        groups.pop();
                    }
                    Draw::Box { .. } => {
                        out.push(groups.iter().rev().fold(command.clip, |r, (m, clip)| {
                            crate::layout::paint::transformed(*m, r).intersect(*clip)
                        }))
                    }
                    _ => {}
                }
            }
        }
        let nested = r##"rect { width = 100, height = 20, translate = { x = 30 }, children = {
            rect { width = 100, height = 20, background = "#445566ff", scale = 2 } } }"##;
        for child in [
            r##"rect { width = 100, height = 20, background = "#445566ff", translate = { x = 30 } }"##,
            r##"rect { width = 100, height = 20, background = "#445566ff", scale = 2 }"##,
            r##"rect { width = 100, height = 20, background = "#445566ff", scale = 6 }"##,
            r##"rect { width = 100, height = 20, background = "#445566ff", scale = 6, origin = { x = 0, y = 1 } }"##,
            r##"rect { width = 100, height = 20, background = "#445566ff", translate = { x = 30 }, scale = 0.5 }"##,
            nested,
        ] {
            let src = format!(r##"return panel {{ id = "bar", width = 100, height = 20, child = {child} }}"##);
            let list =
                build(&resolved_surface(&Lua::new(), &src, LogicalSize { width: 100.0, height: 20.0 }), 1.0, None);
            let mut fills = Vec::new();
            painted(&list.commands, &mut Vec::new(), &mut fills);
            assert!(!fills.is_empty(), "{child}: nothing painted");
            for drawn in fills {
                assert!(
                    drawn.x0 >= 0 && drawn.x1 <= 100 && drawn.y0 >= 0 && drawn.y1 <= 20,
                    "{child}: paints over {drawn:?}, past its parent"
                );
            }
        }
        // A rotated child gets the parent's bounding box in its own space, so x narrows from the
        // 100px child's rotated 120 px bounds but y keeps the bounds' overshoot.
        let src = r##"return panel { id = "bar", width = 100, height = 20,
            child = rect { width = 100, height = 20, background = "#445566ff", rotate = 45 } }"##;
        let list = build(&resolved_surface(&Lua::new(), src, LogicalSize { width: 100.0, height: 20.0 }), 1.0, None);
        let mut fills = Vec::new();
        painted(&list.commands, &mut Vec::new(), &mut fills);
        let drawn = fills.last().unwrap();
        assert!(drawn.x0 > 0 && drawn.x1 < 100, "{drawn:?}");
    }

    /// A clip emptied by an ancestor stays empty under a transformed child, even when the parent
    /// still recurses for its shadow.
    #[test]
    fn a_transformed_child_under_an_empty_clip_draws_nothing() {
        fn blue(commands: &[DrawCmd]) -> bool {
            commands.iter().any(|c| {
                let fill = matches!(&c.draw, Draw::Box { background, .. } if matches!(background.as_slice(), [Fill::Color(c)] if (c.b * 255.0).round() as u8 == 0x66));
                fill || c.draw.nested().is_some_and(blue)
            })
        }
        let src = r##"return panel { id = "bar", width = 100, height = 20,
            child = rect { width = 40, height = 20, margin = { left = 110 }, background = "#102030ff", clip = "box",
                shadows = { { blur = 30, color = "#000000ff" } }, children = {
                    rect { width = 40, height = 20, background = "#445566ff", translate = { x = -30 } } } } }"##;
        let list = build(&resolved_surface(&Lua::new(), src, LogicalSize { width: 100.0, height: 20.0 }), 1.0, None);
        assert!(!blue(&list.commands), "{list:?}");
    }

    /// A node scrolled or positioned entirely outside its parent draws nothing, so it earns no
    /// entry, and, more usefully, moving it around off-screen produces no list change and so no
    /// repaint.
    #[test]
    fn a_subtree_clipped_to_nothing_is_left_out_entirely() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { background = "#445566ff", width = 0, height = 0, clip = "box",
                children = { text { content = "offscreen", foreground = "#ffffffff" } } } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        assert!(
            !list.commands.iter().any(|c| matches!(&c.draw, Draw::Text { content, .. } if &**content == "offscreen")),
            "a zero-area parent clips its child to nothing, so neither belongs in the list: {list:?}"
        );
    }

    // textfield masking

    fn lock_target() -> node::SecureSubmitTarget {
        node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string(), name: None }
    }

    /// One surface holding a password field.
    fn password_surface(lua: &Lua) -> ResolvedNode {
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "fill", height = 28, placeholder = "password",
                mask_character = "*",
                secure_submit = { capability = "lock", action = "authenticate" } } }"##;
        resolved_surface(lua, src, LogicalSize { width: 200.0, height: 40.0 })
    }

    fn drawn_text(list: &DisplayList) -> Vec<String> {
        list.commands
            .iter()
            .filter_map(|c| match &c.draw {
                Draw::Text { content, .. } => Some(content.to_string()),
                _ => None,
            })
            .collect()
    }

    fn reply_surface(lua: &Lua) -> ResolvedNode {
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "fill", height = 28, placeholder = "Reply",
                on_submit = function(text) end } }"##;
        resolved_surface(lua, src, LogicalSize { width: 200.0, height: 40.0 })
    }

    #[test]
    fn keyboard_focus_draws_an_outline_for_the_selected_node() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        let idle = build_with_control(&tree, 1.0, &[], None);
        let focused = build_with_control(&tree, 1.0, &[], Some(id));
        assert_eq!(focused.commands.len(), idle.commands.len() + 2);
        assert!(focused.commands.iter().rev().take(2).all(|cmd| matches!(cmd.draw, Draw::Box { .. })));

        let mut ringless = tree.clone();
        std::rc::Rc::make_mut(&mut ringless.children[0].properties).insert("focus_ring", mlua::Value::Boolean(false));
        assert_eq!(build_with_control(&ringless, 1.0, &[], Some(id)).commands.len(), idle.commands.len());
    }

    /// The plain half of `textfield` (ADR-0092). Unfocused it is a placeholder like any other
    /// field; focused it shows what has been typed, with a caret after it.
    #[test]
    fn a_plain_textfield_shows_its_placeholder_until_it_is_focused() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        assert_eq!(drawn_text(&build(&tree, 1.0, None)), vec!["Reply".to_string()]);

        let id = tree.children[0].id;
        let typed =
            build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "on my way", caret: Some((9, 9)), caret_on: true }));
        assert_eq!(drawn_text(&typed), vec!["on my way".to_string()]);
    }

    #[test]
    fn the_placeholder_and_caret_take_their_own_colors_and_typed_text_takes_foreground() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "fill", height = 28, placeholder = "Reply", foreground = "#ff0000",
                placeholder_color = "#00ff00", caret = { color = "#0000ff" }, on_submit = function(text) end } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let color = |list: &DisplayList| {
            list.commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Text { color, .. } => Some((color.r, color.g)),
                _ => None,
            })
        };
        assert_eq!(color(&build(&tree, 1.0, None)), Some((0.0, 1.0)));
        let id = tree.children[0].id;
        let typed = build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "x", caret: Some((1, 1)), caret_on: true }));
        assert_eq!(color(&typed), Some((1.0, 0.0)));
        let caret = typed.commands.iter().find_map(|cmd| match &cmd.draw {
            Draw::Text { caret_style, .. } => Some((caret_style.color.r, caret_style.color.b)),
            _ => None,
        });
        assert_eq!(caret, Some((0.0, 1.0)));
    }

    #[test]
    fn a_fields_placeholder_and_draft_draw_in_its_typography() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 60,
            child = textfield { width = "fill", placeholder = "Reply", font = "Mono", font_size = 20, line_height = 2,
                letter_spacing = 3, font_weight = 700, italic = true, font_variations = { wght = 650 },
                on_submit = function(text) end } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 60.0 });
        assert_eq!(tree.children[0].rect.height, 40.0, "one line of `font_size * line_height`");
        let id = tree.children[0].id;
        let typed = build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "x", caret: Some((1, 1)), caret_on: true }));
        for list in [build(&tree, 1.0, None), typed] {
            let Some(Draw::Text {
                face: node::Typeface { font, font_size, line_height, letter_spacing, font_weight, italic, variations },
                ..
            }) = list.commands.iter().find_map(|cmd| matches!(cmd.draw, Draw::Text { .. }).then(|| cmd.draw.clone()))
            else {
                panic!("the field draws text")
            };
            assert_eq!((font.as_deref(), font_size, line_height), (Some("Mono"), 20.0, 40.0));
            assert_eq!((letter_spacing, font_weight, italic), (3.0, 700.0, true));
            assert_eq!(&*variations, &[(*b"wght", 650.0_f32.to_bits())]);
        }
    }

    #[test]
    fn an_unfocused_field_draws_its_parked_draft() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        let draft = FieldFocus::Plain { id, text: "half a sentence", caret: None, caret_on: false };
        let list = build_with_control(&tree, 1.0, std::slice::from_ref(&draft), None);
        assert_eq!(drawn_text(&list), vec!["half a sentence".to_string()]);
        // A password being typed elsewhere in the form must not hide it.
        let target = lock_target();
        let typing = FieldFocus::Masked { id: NodeId::test(999), target: &target, filled: 4 };
        assert_eq!(drawn_text(&build_with_control(&tree, 1.0, &[typing, draft], None)), vec!["half a sentence"]);
    }

    #[test]
    fn preedit_is_visible_and_underlined() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        let list = build(
            &tree,
            1.0,
            Some(&FieldFocus::Composing {
                id,
                text: "ab",
                selection: (1, 1),
                preedit: "語",
                cursor: (3, 3),
                caret_on: true,
            }),
        );
        let (content, runs, caret) = list
            .commands
            .iter()
            .find_map(|cmd| match &cmd.draw {
                Draw::Text { content, runs, caret, .. } => Some((content, runs, caret)),
                _ => None,
            })
            .unwrap();
        assert_eq!(content.as_ref(), "a語b");
        assert_eq!(*caret, Some((4, 4)));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].range, 1..4);
        assert!(runs[0].underline);

        let hidden = build(
            &tree,
            1.0,
            Some(&FieldFocus::Composing {
                id,
                text: "ab",
                selection: (1, 1),
                preedit: "語",
                cursor: (-1, -1),
                caret_on: true,
            }),
        );
        assert!(
            hidden
                .commands
                .iter()
                .any(|cmd| { matches!(&cmd.draw, Draw::Text { caret: Some((4, 4)), caret_on: false, .. }) })
        );
    }

    /// An empty focused field shows its placeholder, the same as an empty idle one and the same as
    /// an empty masked one (ADR-0135). This asserted the opposite until `autofocus` proved the
    /// distinction unreachable: a field that holds the keyboard from its first frame has no idle
    /// state to be confused with, and the placeholder was text nothing could ever display.
    #[test]
    fn a_focused_but_empty_plain_field_still_shows_its_placeholder() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        assert_eq!(
            drawn_text(&build(
                &tree,
                1.0,
                Some(&FieldFocus::Plain { id, text: "", caret: Some((0, 0)), caret_on: true })
            )),
            vec!["Reply".to_string()]
        );
    }

    /// The caret alone is what a field with no placeholder to show falls back to, which is the one
    /// case left where an empty focused field still says it is live by drawing something. It is a
    /// rect the painter fills at the caret, so the command carries no text to draw (ADR-0236).
    #[test]
    fn a_focused_empty_field_that_declared_no_placeholder_draws_the_caret_alone() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "fill", height = 28, on_submit = function(text) end } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let id = tree.children[0].id;

        let list = build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "", caret: Some((0, 0)), caret_on: true }));
        assert!(
            list.commands
                .iter()
                .any(|c| matches!(&c.draw, Draw::Text { content, caret, .. } if content.is_empty() && caret.is_some())),
            "an empty field with the keyboard says so with its caret"
        );
        assert!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "", caret: None, caret_on: true })))
                .is_empty(),
            "with no keyboard and nothing to say, an empty field draws nothing at all"
        );
    }

    /// Focus names one node, so a focus naming another leaves this field alone. The case it guards
    /// is two reply fields in one card: only the one clicked into fills.
    /// ADR-0108: the keyboard left, the draft did not.
    #[test]
    fn a_plain_field_without_the_keyboard_draws_its_draft_with_no_caret_and_its_placeholder_when_empty() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        assert_eq!(
            drawn_text(&build(
                &tree,
                1.0,
                Some(&FieldFocus::Plain { id, text: "on my way", caret: None, caret_on: true })
            )),
            vec!["on my way".to_string()]
        );
        assert_eq!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "", caret: None, caret_on: true }))),
            vec!["Reply".to_string()]
        );
    }

    #[test]
    fn a_plain_field_that_is_not_the_focused_node_keeps_its_placeholder() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let elsewhere = crate::layout::scene::NodeId::test(9999);
        let list = build(
            &tree,
            1.0,
            Some(&FieldFocus::Plain { id: elsewhere, text: "not mine", caret: Some((0, 0)), caret_on: true }),
        );
        assert_eq!(drawn_text(&list), vec!["Reply".to_string()]);
    }

    /// The bug the box stood in the way of (ADR-0099). A notification arriving above the card being
    /// replied to re-lays the surface out and the field lands somewhere else, and under a
    /// box-keyed focus paint stopped finding it: the caret and the typed text vanished from a
    /// field that was still receiving every keystroke. Here the same tree is drawn at two different
    /// geometries and the focus follows the node.
    #[test]
    fn a_focused_plain_field_keeps_its_caret_when_the_layout_moves_it() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        assert_eq!(
            drawn_text(&build(
                &tree,
                1.0,
                Some(&FieldFocus::Plain { id, text: "on my way", caret: Some((9, 9)), caret_on: true })
            )),
            vec!["on my way".to_string()]
        );

        // The same node, pushed down and narrowed the way a re-resolve would. Kept inside the
        // surface, since a node clipped out entirely draws nothing for reasons unrelated to focus.
        let mut moved = tree.clone();
        moved.children[0].rect.y += 6.0;
        moved.children[0].rect.width -= 40.0;
        assert_eq!(
            drawn_text(&build(
                &moved,
                1.0,
                Some(&FieldFocus::Plain { id, text: "on my way", caret: Some((9, 9)), caret_on: true })
            )),
            vec!["on my way".to_string()],
            "the caret follows the node, not the box it used to occupy"
        );
    }

    /// The other direction, and the reason the id has to be the node's own rather than anything
    /// positional: a *different* field that comes to sit where the focused one was must not
    /// inherit its text.
    #[test]
    fn a_different_field_that_takes_the_focused_ones_box_draws_nothing_of_its_text() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let vacated = tree.children[0].rect;

        let mut other = tree.clone();
        other.children[0].id = crate::layout::scene::NodeId::test(4242);
        other.children[0].rect = vacated;

        let list = build(
            &other,
            1.0,
            Some(&FieldFocus::Plain {
                id: tree.children[0].id,
                text: "on my way",
                caret: Some((9, 9)),
                caret_on: true,
            }),
        );
        assert_eq!(drawn_text(&list), vec!["Reply".to_string()]);
    }

    /// A masked field ignores a plain focus outright: the two are different kinds, and a
    /// `secure_submit` field must never draw text a `FieldFocus::Plain` is carrying.
    #[test]
    fn a_masked_field_never_draws_a_plain_focuss_text() {
        let lua = Lua::new();
        let tree = password_surface(&lua);
        let id = tree.children[0].id;
        let list =
            build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "hunter2", caret: Some((0, 0)), caret_on: true }));
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    #[test]
    fn an_unfocused_password_field_shows_its_placeholder() {
        let lua = Lua::new();
        let list = build(&password_surface(&lua), 1.0, None);
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    /// The fix for typing blind: four keystrokes are four glyphs on screen.
    #[test]
    fn a_focused_password_field_draws_one_mask_character_per_typed_character() {
        let lua = Lua::new();
        let target = lock_target();
        let tree = password_surface(&lua);
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &target, filled: 4 }));
        assert_eq!(drawn_text(&list), vec!["****".to_string()]);
    }

    #[test]
    fn a_focused_but_empty_password_field_still_shows_its_placeholder() {
        let lua = Lua::new();
        let target = lock_target();
        let tree = password_surface(&lua);
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &target, filled: 0 }));
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    /// Focus is a `{ capability, action }` pair, so a field addressed somewhere else must not
    /// fill just because some other field is focused on the same surface. This is the same
    /// routing rule `input::keyboard::retarget_secure_submit` enforces for the bytes themselves.
    #[test]
    fn a_field_addressed_to_another_capability_does_not_draw_the_focused_fields_characters() {
        let lua = Lua::new();
        let elsewhere =
            node::SecureSubmitTarget { capability: "network".to_string(), action: "connect".to_string(), name: None };
        let tree = password_surface(&lua);
        let list =
            build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &elsewhere, filled: 9 }));
        assert_eq!(
            drawn_text(&list),
            vec!["password".to_string()],
            "a PSK's length must not leak onto the lock screen's field"
        );
    }

    #[test]
    fn another_node_with_the_same_secure_target_does_not_show_the_password_length() {
        let lua = Lua::new();
        let tree = password_surface(&lua);
        let target = lock_target();
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { id: NodeId::test(999), target: &target, filled: 9 }));
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    /// The count is all paint ever gets (see [`FieldFocus`]), so there is no path by which a
    /// typed character reaches the list. Asserted because a display list is cloned, compared and
    /// retained in `last_painted`, exactly the places ADR-0005 keeps a secret out of.
    #[test]
    fn a_masked_field_draws_only_the_mask_character() {
        let lua = Lua::new();
        let target = lock_target();
        let tree = password_surface(&lua);
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &target, filled: 6 }));
        let drawn = drawn_text(&list);
        assert_eq!(drawn, vec!["******".to_string()]);
        assert!(drawn[0].chars().all(|c| c == '*'), "nothing but the mask glyph may reach the list");
    }

    /// `mask_character` is optional, and a field that omits it should still look
    /// like a password field rather than draw nothing.
    #[test]
    fn a_field_without_a_mask_character_falls_back_to_a_bullet() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "fill", height = 28,
                secure_submit = { capability = "lock", action = "authenticate" } } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let target = lock_target();
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &target, filled: 3 }));
        assert_eq!(drawn_text(&list), vec!["\u{2022}\u{2022}\u{2022}".to_string()]);
    }

    /// The dots are shaped in the field's own typeface, and no caret position is ever derived from them.
    #[test]
    fn a_masked_fields_dots_carry_its_typeface_and_no_caret() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "fill", height = 28, font_size = 20, letter_spacing = 3,
                secure_submit = { capability = "lock", action = "authenticate" } } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let target = lock_target();
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &target, filled: 3 }));
        let Some(Draw::Text { face, caret, .. }) =
            list.commands.iter().map(|cmd| &cmd.draw).find(|d| matches!(d, Draw::Text { .. }))
        else {
            panic!("the dots are drawn: {list:?}")
        };
        assert_eq!((face.font_size, face.letter_spacing, *caret), (20.0, 3.0, None));
        let point = crate::layout::hit::LogicalPoint { x: 5.0, y: 5.0 };
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        assert_eq!(crate::layout::hit::caret_at(&[&tree, &tree.children[0]], point, "", 0, &shaping), None);
    }

    /// The property this feature needs from the display list: typing has to change it, or
    /// `paint_surface` skips the repaint and the dots never appear.
    #[test]
    fn each_typed_character_changes_the_list_so_the_repaint_is_not_skipped() {
        let lua = Lua::new();
        let tree = password_surface(&lua);
        let target = lock_target();
        let three =
            build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &target, filled: 3 }));
        let four = build(&tree, 1.0, Some(&FieldFocus::Masked { id: tree.children[0].id, target: &target, filled: 4 }));
        assert_ne!(three, four);
    }

    /// A leaf that asks for a rounded clip has nothing to clip, so it buys no offscreen pass.
    #[test]
    fn a_childless_rounded_clip_builds_no_group() {
        let lua = Lua::new();
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, child = rect {
                width = 80, height = 32, radius = 16, clip = "rounded", background = "#0000FFFF",
            } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        let list = build(&root, 1.0, None);
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Clipped { .. })));
    }

    /// The property is in the display list, so turning it on repaints: ADR-0063 skips the frame
    /// when the list compares equal, and a clip that changed shape without changing the list would
    /// never be drawn.
    #[test]
    fn changing_only_the_clip_changes_the_display_list() {
        let size = LogicalSize { width: 96.0, height: 48.0 };
        let src = |clip: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 96, height = 48, child = rect {{
                    width = 80, height = 32, radius = 16, clip = "{clip}",
                    children = {{ rect {{ width = 30, height = "fill", background = "#0000FFFF" }} }},
                }} }}"##
            )
        };
        let boxed = build(&resolved_surface(&Lua::new(), &src("box"), size), 1.0, None);
        let rounded = build(&resolved_surface(&Lua::new(), &src("rounded"), size), 1.0, None);
        assert_ne!(boxed, rounded);
    }

    /// ADR-0253. The stage recompiles on a new file version, but only a paint reaches it, and a
    /// list naming the path alone compares equal after the edit and skips that paint.
    #[test]
    fn editing_a_shader_file_changes_the_display_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.frag");
        std::fs::write(&path, "void main() {}").unwrap();
        let src = format!(
            r#"return panel {{ id = "bar", child = shader {{ width = 10, height = 10, source = "{}" }} }}"#,
            path.display()
        );
        let size = LogicalSize { width: 100.0, height: 100.0 };
        let lua = Lua::new();
        let tree = resolved_surface(&lua, &src, size);
        let before = build(&tree, 1.0, None);
        assert_eq!(build(&tree, 1.0, None), before, "an untouched file repaints nothing");
        std::fs::write(&path, "void main() { fragColor = vec4(1.0); }").unwrap();
        assert_ne!(build(&tree, 1.0, None), before);
    }

    /// ADR-0336. A node with `effect.shader` is one layer carrying the program, and the repaint it
    /// causes reaches every input: `padding` grows its clip, and a `params` change or a saved file
    /// changes the command, which is what keeps `TextPainter` from compositing the old layer.
    #[test]
    fn an_effect_shader_layer_carries_its_padding_params_and_file_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.frag");
        std::fs::write(&path, "void main() {}").unwrap();
        let src = |padding: u32, k: u32| {
            format!(
                r##"return panel {{ id = "bar", width = 200, height = 100, padding = 40, child = rect {{ width = 20,
                height = 20, radius = 5, background = "#ffffff", effect = {{ shader = {{ source = "{}",
                padding = {padding}, params = {{ k = {k} }} }} }} }} }}"##,
                path.display()
            )
        };
        let layer = |padding, k| {
            let list = effect_surface_src(&src(padding, k));
            list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Layer { .. })).expect("a layer").clone()
        };
        let plain = layer(0, 1);
        let Draw::Layer { shader: Some(shader), effect, .. } = &plain.draw else { panic!("a shader layer: {plain:?}") };
        assert_eq!(
            (shader.params.clone(), shader.radius.clone()),
            (vec![("k".to_string(), vec![1.0])], Radii::from(5.0))
        );
        assert!(effect.shader.is_none(), "the program travels in the layer's `shader` alone");
        assert!(plain.clip.x0 >= 38 && plain.clip.x1 <= 62, "the box and its outline's edge: {:?}", plain.clip);
        assert_eq!(layer(12, 1).clip, PhysicalRect { x0: 28, y0: 28, x1: 72, y1: 72 }, "padding grows the clip");
        assert_ne!(layer(0, 2), plain, "a param change is a different command");
        std::fs::write(&path, "void main() { fragColor = vec4(1.0); }").unwrap();
        assert_ne!(layer(0, 1), plain, "so is a saved file");
    }

    /// ADR-0336 item 5: a node cut out by a clipping ancestor is still built when its shader padding reaches back in.
    #[test]
    fn shader_padding_decides_whether_a_clipped_out_node_is_built() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.frag");
        std::fs::write(&path, "void main() {}").unwrap();
        let layer = |padding: u32, shadow: &str| {
            let src = format!(
                r##"return panel {{ id = "bar", width = 200, height = 100, padding = 40, child = row {{ width = 30,
                height = 20, clip = "box", children = {{ rect {{ width = 20, height = 20, margin = {{ left = 40 }}, {shadow}
                effect = {{ shader = {{ source = "{}", padding = {padding} }} }}, children = {{
                rect {{ width = 20, height = 20, margin = {{ left = -30 }}, background = "#ffffff" }} }} }} }} }} }}"##,
                path.display()
            );
            let list = effect_surface_src(&src);
            list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Layer { silhouette: false, .. })).cloned()
        };
        for shadow in ["", "shadows = { { blur = 1, color = \"#000000ff\" } },"] {
            assert!(layer(0, shadow).is_none(), "no padding: nothing reaches the clip ({shadow})");
            let drawn = layer(30, shadow).unwrap_or_else(|| panic!("padding reaches the clip ({shadow})"));
            assert!(drawn.clip.x1 <= 70, "inside the ancestor's clip: {:?}", drawn.clip);
        }
    }

    /// A backdrop shader's padding reaches back into an ancestor's clip from a box outside it.
    #[test]
    fn backdrop_shader_padding_decides_whether_a_clipped_out_box_is_built() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.frag");
        std::fs::write(&path, "void main() {}").unwrap();
        let drawn = |padding: u32| {
            let src = format!(
                r##"return panel {{ id = "bar", width = 200, height = 100, padding = 40, child = row {{ width = 30,
                height = 20, clip = "box", children = {{ rect {{ width = 20, height = 20, margin = {{ left = 40 }},
                effect = {{ shader = {{ source = "{}", input = "backdrop", padding = {padding} }} }} }} }} }} }}"##,
                path.display()
            );
            effect_surface_src(&src).commands.iter().any(|cmd| matches!(cmd.draw, Draw::Backdrop { .. }))
        };
        assert!(!drawn(0), "no padding: nothing reaches the clip");
        assert!(drawn(30), "padding reaches the clip");
    }

    /// Qt's `OpacityMask` covers the item, not only its children, so the node's own fill and border
    /// are drawn inside the masked group, in the order an unmasked box draws them.
    #[test]
    fn a_mask_groups_the_nodes_fill_subtree_and_border_in_paint_order() {
        let list = masked(IMAGE_MASKED);
        let [_, group] = list.commands.as_slice() else { panic!("the panel's box, then one group: {list:?}") };
        let Draw::Clipped { radius, mask: Some((mask, box_px)), commands } = &group.draw else {
            panic!("expected a masked group, got {:?}", group.draw)
        };
        assert_eq!((radius.clone(), *box_px, mask.invert), (Radii::default(), (80, 32), false));
        let order: Vec<_> = commands
            .iter()
            .map(|cmd| match &cmd.draw {
                Draw::Box { background, widths, .. } if !background.is_empty() && *widths == EdgeInsets::default() => {
                    "fill"
                }
                Draw::Box { background, .. } if background.is_empty() => "border",
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(order, ["fill", "fill", "border"], "own fill, child, own border");
    }

    #[test]
    fn a_masked_rounded_box_composites_through_its_radius_and_a_leaf_still_groups() {
        let list = masked(
            r##"rect { width = 80, height = 32, radius = 8, clip = "rounded", background = "#0000FFFF",
                mask = { source = "/nonexistent/mask.svg" } }"##,
        );
        let Draw::Clipped { radius, mask: Some(_), commands } = &list.commands[1].draw else { panic!("{list:?}") };
        assert_eq!((radius.clone(), commands.len()), (Radii::from(8.0), 1));
    }

    /// `corner_smoothing` rides the radii into every draw that cuts or reads through the outline.
    #[test]
    fn corner_smoothing_reaches_a_mask_and_a_backdrop() {
        let smooth = Radii([8.0; 4], 0.6, None);
        let list = masked(
            r##"rect { width = 80, height = 32, radius = 8, corner_smoothing = 0.6, clip = "rounded",
                background = "#0000FFFF", mask = { source = "/nonexistent/mask.svg" } }"##,
        );
        let Draw::Clipped { radius, mask: Some(_), .. } = &list.commands[1].draw else { panic!("{list:?}") };
        assert_eq!(*radius, smooth);
        let list = effect_surface(
            r##"rect { width = 40, height = 20, radius = 8, corner_smoothing = 0.6, effect = { backdrop = { blur = 4 } } }"##,
        );
        let radius = list.commands.iter().find_map(|cmd| match &cmd.draw {
            Draw::Backdrop { radius, .. } => Some(radius.clone()),
            _ => None,
        });
        assert_eq!(radius, Some(smooth));
    }

    /// Opacity is baked into the list (ADR-0063), so a gradient fades stop by stop like a colour.
    #[test]
    fn opacity_fades_every_gradient_stop() {
        let list = masked(
            r##"rect { width = 80, height = 32, opacity = 0.5,
                background = { gradient = "radial", stops = { { 0, "#ffffff" }, { 1, "#ffffff80" } } } }"##,
        );
        let Draw::Box { background, .. } = &list.commands[1].draw else { panic!("{list:?}") };
        let [Fill::Gradient(gradient)] = background.as_slice() else { panic!("{list:?}") };
        let alphas: Vec<f32> = gradient.stops.iter().map(|(_, color)| color.a).collect();
        assert_eq!(alphas, [0.5, 0.5 * 128.0 / 255.0]);
    }

    /// Three overlapping 20px siblings, red channel 1, 2, 3, under a parent with `radius` and a
    /// rounded clip; `zs` are their `z`, bound to a signal.
    fn stacked(zs: [&str; 3], radius: f32) -> DisplayList {
        let lua = Lua::new();
        let sibling = |n: i32| {
            format!(
                r##"rect {{ width = 20, height = 20, background = "#0{n}0000", z = state("z{n}", {}) }}"##,
                zs[n as usize - 1]
            )
        };
        let src = format!(
            r##"return panel {{ id = "bar", width = 200, height = 40, child = row {{ spacing = -10, radius = {radius},
                clip = "rounded", children = {{ {}, {}, {} }} }} }}"##,
            sibling(1),
            sibling(2),
            sibling(3)
        );
        build(&resolved_surface(&lua, &src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None)
    }

    /// The red channels of every box fill in paint order, into groups.
    fn fill_order(commands: &[DrawCmd]) -> Vec<u8> {
        commands
            .iter()
            .flat_map(|c| match &c.draw {
                Draw::Box { background, .. } if let [Fill::Color(color)] = background.as_slice() => {
                    vec![(color.r * 255.0).round() as u8]
                }
                draw => draw.nested().map_or_else(Vec::new, fill_order),
            })
            .collect()
    }

    /// ADR-0259: ascending `z`, declaration order among equals, inside a rounded clip too.
    #[test]
    fn siblings_paint_in_ascending_z_and_declaration_order_among_equals() {
        for radius in [0.0, 6.0] {
            let order = |zs| fill_order(&stacked(zs, radius).commands);
            assert_eq!(order(["0", "0", "0"]), [1, 2, 3], "radius {radius}");
            assert_eq!(order(["1", "0", "0"]), [2, 3, 1], "radius {radius}");
            assert_eq!(order(["0", "0", "-1"]), [3, 1, 2], "radius {radius}");
            assert_eq!(order(["1", "0", "-0.0"]), [2, 3, 1], "-0.0 equals 0, radius {radius}");
        }
    }

    /// ADR-0254. An opaque box's silhouette is its own shape, so its shadow is one gradient quad
    /// drawn first, faded with the node, and clipped to its offset, spread and blurred extent
    /// rather than to the box.
    #[test]
    fn an_opaque_box_casts_its_shadow_as_one_gradient_under_its_fill() {
        let card = |rest: &str| {
            effect_surface(&format!(
                r##"rect {{ width = 40, height = 20, radius = 6, background = "#ffffff", {rest}
                    shadows = {{ {{ color = "#00000080", blur = 8, offset = {{ y = 4 }}, spread = 2 }} }}}}"##
            ))
        };
        let list = card("opacity = 0.5,");
        let at = list.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow");
        let Draw::Shadow { shadow, radius, knockout } = list.commands[at].draw.clone() else { unreachable!() };
        assert_eq!((radius, knockout), (Radii::from(6.0), true), "a fading card shows no shadow through its body");
        assert!((shadow.color.a - 0.5 * 128.0 / 255.0).abs() < 1e-6, "faded with the node: {shadow:?}");
        assert!(matches!(list.commands[at + 1].draw, Draw::Box { .. }), "the fill covers the shadow");
        // The box is 40..80 x 40..60; the shadow's box is 38..82 x 42..66, blurred 3 sigma, 12, further out.
        assert_eq!(list.commands[at].clip, PhysicalRect { x0: 26, y0: 30, x1: 94, y1: 78 });
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Layer { .. })), "no offscreen");
        let opaque = card("");
        assert!(opaque.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Shadow { knockout: false, .. })));
        assert_eq!(card(r#"shadow_mode = "content","#), opaque, "the same in either mode (ADR-0260)");
    }

    /// One opaque colour layer, on top or below, makes a box opaque: its shadow is not knocked out.
    #[test]
    fn a_box_with_an_opaque_background_layer_is_opaque() {
        let knockout = |background: &str| {
            let list = effect_surface(&format!(
                r##"rect {{ width = 40, height = 20, background = {background}, shadows = {{ {{ blur = 4 }} }} }}"##
            ));
            let Some(Draw::Shadow { knockout, .. }) =
                list.commands.iter().map(|cmd| &cmd.draw).find(|draw| matches!(draw, Draw::Shadow { .. })).cloned()
            else {
                panic!("a shadow");
            };
            knockout
        };
        assert!(!knockout(r##"{ "#ffffffff", "#ffffff80" }"##), "an opaque top layer");
        assert!(!knockout(r##"{ "#ffffff80", "#000000ff" }"##), "an opaque bottom layer");
        assert!(knockout(
            r##"{ "#ffffff80", { gradient = "radial", stops = { { 0, "#000000" }, { 1, "#ffffff" } } } }"##
        ));
        // Only a normal layer counts: a blended one shows what is under it.
        assert!(knockout(r##"{ { fill = "#ffffffff", blend = "multiply" } }"##), "a blended opaque layer");
        assert!(!knockout(r##"{ { fill = "#ffffff80", blend = "screen" }, "#000000ff" }"##), "over an opaque one");
    }

    /// A blended node is not opaque, and blends its own shadow with it; a blended layer is a layer
    /// of its own between the normal runs, bottom-up; `"normal"` builds what no `blend` does.
    #[test]
    fn blend_splits_layers_and_groups_a_blended_nodes_shadow() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, background = "#ffffffff", blend = "multiply", shadows = { { blur = 4 } } }"##,
        );
        let [.., DrawCmd { draw: Draw::Layer { effect, commands, .. }, .. }] = list.commands.as_slice() else {
            panic!("{list:?}")
        };
        assert_eq!(effect.blend, Blend::Multiply);
        assert!(matches!(commands[0].draw, Draw::Shadow { knockout: true, .. }), "{commands:?}");
        let stack = |background: &str| {
            let list = effect_surface(&format!(r##"rect {{ width = 40, height = 20, background = {background} }}"##));
            list.commands.into_iter().skip(1).map(|cmd| cmd.draw).collect::<Vec<_>>()
        };
        let boxed = |fills: &[(f32, f32, f32)]| {
            let background = fills.iter().map(|&(r, g, b)| Fill::Color(Rgba { r, g, b, a: 1.0 })).collect();
            Draw::Box {
                background,
                radius: Radii::default(),
                border: BorderPaint::default(),
                widths: EdgeInsets::default(),
            }
        };
        let split = stack(r##"{ "#ff0000", "#00ff00", { fill = "#ffffff", blend = "screen" }, "#0000ff" }"##);
        let [bottom, Draw::Layer { effect, commands, .. }, top] = split.as_slice() else { panic!("{split:?}") };
        let (red, green, blue) = ((1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0));
        assert_eq!((bottom, effect.blend, top), (&boxed(&[blue]), Blend::Screen, &boxed(&[red, green])));
        assert_eq!(commands[0].draw, boxed(&[(1.0, 1.0, 1.0)]));
        assert_eq!(stack(r##"{ { fill = "#808080", blend = "normal" } }"##), stack(r##""#808080""##));
    }

    /// `shadows` paints its layers bottom first, so the first is on top: one gradient each under a
    /// box, every layer cast from one offscreen in content mode.
    #[test]
    fn shadow_layers_paint_bottom_first() {
        let layers = r##"shadows = { { color = "#0000ffff", offset = { y = 4 } }, { color = "#00000000", blur = 9 },
            { color = "#00ff00ff", blur = 8, offset = { y = 12 } } }"##;
        let colors = |shadows: &[node::Shadow]| shadows.iter().map(|shadow| shadow.color.g).collect::<Vec<_>>();
        let list =
            effect_surface(&format!(r##"rect {{ width = 40, height = 20, background = "#ffffff", {layers} }}"##));
        let drawn: Vec<_> = list
            .commands
            .iter()
            .filter_map(|cmd| match cmd.draw {
                Draw::Shadow { shadow, .. } => Some(shadow),
                _ => None,
            })
            .collect();
        assert_eq!(colors(&drawn), [1.0, 0.0], "the transparent layer drops; the last draws first");
        let last = list.commands.iter().rposition(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow");
        assert!(matches!(list.commands[last + 1].draw, Draw::Box { .. }), "the fill over both");

        let list = effect_surface(&format!(r##"text {{ content = "hi", {layers} }}"##));
        let Some(Draw::Layer { effect, .. }) = list.commands.last().map(|cmd| &cmd.draw) else { panic!("a layer") };
        assert_eq!(colors(&effect.shadows), [0.0, 1.0], "listed first on top");
    }

    /// ADR-0254. Anything but an opaque box casts the shadow of its pixels, so its subtree goes
    /// offscreen as one group. Its content already carries the opacity, so the shadow colour
    /// does not fade twice.
    #[test]
    fn text_or_a_translucent_box_casts_its_shadow_through_one_offscreen_layer() {
        for child in [
            r##"text { content = "hi", opacity = 0.5, shadows = { { blur = 4, offset = { x = 3 } } } }"##,
            r##"rect { width = 40, height = 20, background = "#ffffff80", opacity = 0.5,
                shadows = { { blur = 4, offset = { x = 3 } } }, shadow_mode = "content", children = { text { content = "hi" } } }"##,
        ] {
            let list = effect_surface(child);
            let layer = list.commands.last().unwrap();
            let Draw::Layer { effect, commands, .. } = &layer.draw else {
                panic!("{child}: expected a layer, got {:?}", layer.draw)
            };
            let (shadows, blur) = (&effect.shadows, &effect.blur);
            let [shadow] = shadows.as_slice() else { panic!("{child}: one shadow, got {shadows:?}") };
            assert_eq!((shadow.color.a, *blur), (1.0, 0.0), "{child}");
            assert!(commands.iter().any(|cmd| matches!(cmd.draw, Draw::Text { .. })), "{child}");
            // The blur reaches 3 sigma, 6px, around the box, and the offset shifts its right edge 3.
            let node = snap_to_physical(layer.rect, 1.0);
            // A text's glyph ink also reaches past its box vertically, so the layer may be taller.
            assert_eq!((layer.clip.x0, layer.clip.x1), (node.x0 - 6, node.x1 + 9), "{child}");
            assert!(layer.clip.y0 <= node.y0 - 6 && layer.clip.y1 >= node.y1 + 6, "{child}");
        }
    }

    /// ADR-0260. A box's shadow is its own shape whatever it holds: one gradient knocked out under
    /// the box, faded with the node, and nothing offscreen, so the label casts nothing.
    #[test]
    fn a_translucent_box_casts_its_box_shadow_as_a_knocked_out_gradient() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, radius = 6, background = "#ffffff40", opacity = 0.5,
                shadows = { { blur = 4, offset = { y = 4 } } }, children = { text { content = "hi" } } }"##,
        );
        let at = list.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow");
        let Draw::Shadow { shadow, radius, knockout } = list.commands[at].draw.clone() else { unreachable!() };
        assert_eq!((radius, knockout, shadow.color.a), (Radii::from(6.0), true, 0.5));
        assert!(matches!(list.commands[at + 1].draw, Draw::Box { .. }), "the fill over it");
        assert!(
            list.commands[at + 2..].iter().any(|cmd| matches!(cmd.draw, Draw::Text { .. })),
            "the label unshadowed"
        );
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Layer { .. })), "no offscreen");
    }

    /// ADR-0331. An inset shadow draws above every background layer and under the children; the
    /// border follows it, and the children only in the rounded clip's group. It opens no layer.
    #[test]
    fn an_inset_shadow_paints_between_the_fill_and_the_children_with_the_border_last() {
        fn kinds(commands: &[DrawCmd]) -> Vec<&'static str> {
            commands
                .iter()
                .flat_map(|cmd| match &cmd.draw {
                    Draw::Box { background, .. } if !background.is_empty() => vec!["fill"],
                    Draw::Box { .. } => vec!["border"],
                    Draw::InsetShadow { .. } => vec!["inset"],
                    Draw::Text { .. } => vec!["text"],
                    draw => draw.nested().map_or_else(Vec::new, kinds),
                })
                .collect()
        }
        for (body, want) in [
            ("", ["fill", "inset", "border", "text"]),
            (", radius = 6", ["fill", "inset", "border", "text"]),
            (r#", radius = 6, clip = "rounded""#, ["fill", "inset", "text", "border"]),
        ] {
            let list = effect_surface(&format!(
                r##"rect {{ width = 40, height = 20, background = {{ "#ff0000", "#00ff00" }}, border_width = 2,
                    border_color = "#0000ff", shadows = {{ {{ blur = 4, inset = true, offset = {{ y = 2 }} }} }}{body},
                    children = {{ text {{ content = "hi" }} }} }}"##
            ));
            let all = kinds(&list.commands);
            let from = all.iter().position(|k| *k == "fill").unwrap();
            assert_eq!(all[from..], want, "{body}");
            assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Layer { .. } | Draw::Shadow { .. })));
        }
    }

    /// ADR-0260. `effect.blur` still takes a layer, and the box shadow stays a gradient outside it.
    /// A scoop, which a gradient cannot draw, casts its fill's silhouette alone through a layer.
    #[test]
    fn a_box_shadow_never_rides_the_bodys_layer() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, background = "#ffffff40", effect = { blur = 2 }, shadows = { { offset = { y = 4 } } }}"##,
        );
        assert!(matches!(list.commands[1].draw, Draw::Shadow { knockout: true, .. }), "{:?}", list.commands);
        assert!(matches!(&list.commands[2].draw, Draw::Layer { effect, .. } if effect.shadows.is_empty()));
        assert_eq!(list.commands[2].clip, PhysicalRect { x0: 34, y0: 34, x1: 86, y1: 66 }, "the blur's reach alone");

        let list = effect_surface(
            r##"rect { width = 40, height = 20, radius = 6, corner_shape = "scoop", background = "#ffffff40",
                opacity = 0.5, shadows = { { offset = { y = 4 } } }, children = { text { content = "hi" } } }"##,
        );
        let Draw::Layer { effect, silhouette: true, commands, .. } = &list.commands[1].draw else {
            panic!("{:?}", list.commands)
        };
        assert_eq!(
            effect.shadows.iter().map(|shadow| shadow.color.a).collect::<Vec<_>>(),
            [0.5],
            "faded with the node"
        );
        let [DrawCmd { draw: Draw::Box { background, radius, .. }, .. }] = commands.as_slice() else {
            panic!("the silhouette alone: {commands:?}")
        };
        let [node::Fill::Color(fill)] = background.as_slice() else { panic!("a colour: {background:?}") };
        assert_eq!((fill.a, radius.clone()), (1.0, Radii::from(-6.0)));
        assert!(list.commands[2..].iter().any(|cmd| matches!(cmd.draw, Draw::Text { .. })), "the label outside it");
    }

    /// ADR-0254, ADR-0262. `effect.blur` spreads the subtree's pixels 3 sigma past its box, at
    /// any sigma.
    #[test]
    fn an_effect_blur_groups_the_subtree_and_reaches_three_sigma() {
        for (blur, reach) in [(2, 6), (12, 36)] {
            let list = effect_surface(&format!(
                r##"rect {{ width = 40, height = 20, background = "#ffffff", effect = {{ blur = {blur} }} }}"##
            ));
            let layer = list.commands.last().unwrap();
            assert!(
                matches!(&layer.draw, Draw::Layer { effect, .. } if effect.shadows.is_empty()),
                "got {:?}",
                layer.draw
            );
            assert_eq!(layer.clip, PhysicalRect { x0: 40 - reach, y0: 40 - reach, x1: 80 + reach, y1: 60 + reach });
        }
    }

    /// A box scrolled just out of its parent still casts the shadow that reaches back in, rather
    /// than popping it in once its own edge crosses back.
    #[test]
    fn a_box_just_outside_its_parent_still_casts_the_shadow_reaching_in() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, clip = "box", children = { rect { width = 40, height = 20, margin = { top = 24 },
                background = "#ffffff", shadows = { { offset = { y = -10 } } }} } }"##,
        );
        let shadow = list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow");
        assert_eq!((shadow.clip.y0, shadow.clip.y1), (54, 60), "the part of it inside the parent");
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Box { .. } if cmd.rect.y == 64.0)));
    }

    /// `clip = "none"` hands a node's children its parent's clip: a wrapper exactly its child's size
    /// no longer cuts the child's shadow, nor a child laid out past it once the wrapper scrolls away.
    #[test]
    fn an_unclipped_wrapper_leaves_its_childs_shadow_and_overflow_whole() {
        let wrapped = |clip: &str| {
            effect_surface(&format!(
                r##"column {{ clip = "{clip}", children = {{ rect {{ width = 40, height = 20, background = "#ffffff",
                    shadows = {{ {{ blur = 8, offset = {{ y = 4 }} }} }}}} }} }}"##
            ))
        };
        let shadow = |list: &DisplayList| {
            list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow").clip
        };
        assert_eq!(shadow(&wrapped("box")), PhysicalRect { x0: 40, y0: 40, x1: 80, y1: 60 }, "cut to the wrapper");
        assert_eq!(shadow(&wrapped("none")), PhysicalRect { x0: 28, y0: 32, x1: 92, y1: 76 });

        let list = effect_surface(
            r##"rect { width = 40, height = 20, clip = "none", children = { rect { width = 40, height = 20,
                margin = { top = 24 }, background = "#ffffff" } } }"##,
        );
        assert!(list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Box { .. }) && cmd.rect.y == 64.0));
    }

    /// Without `clip`, a child laid out past its parent paints there, as CSS `overflow: visible`,
    /// and its change damages where it paints; a scroll viewport, a mask and the surface still cut.
    #[test]
    fn only_a_scroll_viewport_a_mask_or_the_surface_cuts_by_default() {
        fn at(commands: &[DrawCmd], y: f32) -> Option<PhysicalRect> {
            commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Box { .. } if cmd.rect.y == y => Some(cmd.clip),
                draw => draw.nested().and_then(|inner| at(inner, y)),
            })
        }
        // `effect_surface` pads by 40, so the 20px parent ends at 60 and the child starts at 64.
        let child = |color: &str| {
            format!(r##"rect {{ width = 40, height = 20, margin = {{ top = 24 }}, background = "{color}" }}"##)
        };
        let list = |parent: &str, color: &str| effect_surface(&parent.replace("CHILD", &child(color)));
        let spill = PhysicalRect { x0: 40, y0: 64, x1: 80, y1: 84 };
        for parent in [
            "rect { width = 40, height = 20, children = { CHILD } }",
            "column { width = 40, height = 20, children = { CHILD } }",
            "list { width = 40, height = 20, source = { 1 }, itemfn = function() return CHILD end }",
        ] {
            assert_eq!(at(&list(parent, "#ffffff").commands, 64.0), Some(spill), "{parent}");
            let damage = list(parent, "#000000").damage_since(&list(parent, "#ffffff"), true);
            assert!(damage.iter().any(|d| d.intersect(spill) == spill), "{parent}: {damage:?}");
        }
        for parent in [
            r#"rect { width = 40, height = 20, clip = "box", children = { CHILD } }"#,
            r#"column { width = 40, height = 20, scroll = scroll("s"), children = { CHILD } }"#,
            r#"list { width = 40, height = 20, scroll = scroll("l"), source = { 1 }, itemfn = function() return CHILD end }"#,
            r#"rect { width = 40, height = 20, mask = { source = "/nonexistent/mask.svg" }, children = { CHILD } }"#,
        ] {
            assert_eq!(at(&list(parent, "#ffffff").commands, 64.0), None, "{parent}");
        }
        let past_the_panel = effect_surface(
            r##"rect { width = 40, height = 20, children = { rect { width = 40, height = 20, margin = { top = 80 },
                background = "#ffffff" } } }"##,
        );
        assert_eq!(at(&past_the_panel.commands, 120.0), None, "the 100px panel cuts a child at 120");
    }

    /// Under a chain of `clip = "none"` to the surface, a layer's offscreen stops at the surface
    /// grown by what its blur and shadow reach back in from: 3 sigma plus the offset.
    #[test]
    fn a_layer_under_unclipped_ancestors_stops_near_the_surface() {
        let src = r##"return panel { id = "bar", width = 200, height = 100, clip = "none", child = rect {
            width = 40, height = 20, clip = "none", effect = { blur = 1 },
            shadows = { { blur = 4, offset = { x = 5 } } }, shadow_mode = "content",
            children = { rect { width = 8000, height = 8000, margin = { left = -4000 }, background = "#ffffff" } } } }"##;
        let list = build(&resolved_surface(&Lua::new(), src, LogicalSize { width: 200.0, height: 100.0 }), 1.0, None);
        let layer = list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Layer { .. })).expect("a layer");
        assert_eq!(layer.clip, PhysicalRect { x0: -11, y0: -6, x1: 211, y1: 111 });
    }

    /// One conversion puts every draw's geometry in buffer pixels; text metrics stay logical.
    #[test]
    fn a_list_built_at_scale_two_doubles_every_draws_geometry() {
        let src = r##"return panel { id = "bar", width = 200, height = 100, padding = 10, child = rect {
            width = 40, height = 20, radius = 4, border_width = 1, border_color = "#ffffff",
            shadows = { { blur = 2, offset = { x = 3 }, spread = 1 } },
            children = { text { content = "a", font_size = 12 } } } }"##;
        let list = build(&resolved_surface(&Lua::new(), src, LogicalSize { width: 200.0, height: 100.0 }), 2.0, None);
        let shadow = list.commands.iter().find_map(|cmd| match &cmd.draw {
            Draw::Shadow { shadow, radius, .. } => Some((cmd.rect, *shadow, radius.clone())),
            _ => None,
        });
        let (rect, shadow, radius) = shadow.expect("a box shadow");
        assert_eq!(rect, LogicalRect { x: 20.0, y: 20.0, width: 80.0, height: 40.0 });
        assert_eq!((shadow.blur, shadow.offset, shadow.spread, radius), (4.0, (6.0, 0.0), 2.0, Radii::from(8.0)));
        let border = list.commands.iter().find_map(|cmd| match &cmd.draw {
            Draw::Box { radius, widths, .. } if widths.top > 0.0 => Some((radius.clone(), widths.top)),
            _ => None,
        });
        assert_eq!(border, Some((Radii::from(8.0), 2.0)));
        let text = list.commands.iter().find_map(|cmd| match &cmd.draw {
            Draw::Text { face, .. } => Some(face.font_size),
            _ => None,
        });
        assert_eq!(text, Some(12.0), "shaping reads the logical size");
    }

    /// A child scaled past its layered parent's box keeps the overflow it would have without the
    /// layer.
    #[test]
    fn a_layer_covers_a_transformed_child_overflowing_its_box() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, effect = { blur = 1 }, clip = "none",
                children = { rect { width = 40, height = 20, background = "#ffffff", scale = 2 } } }"##,
        );
        let layer = list.commands.last().unwrap();
        assert!(matches!(layer.draw, Draw::Layer { .. }));
        assert!(layer.clip.x0 <= 20 && layer.clip.x1 >= 100, "the child's scaled box: {:?}", layer.clip);
    }

    /// ADR-0256. The backdrop is read before the node paints anything and outside the offscreen a
    /// shadow draws the node into, faded with it, and its clip covers the 3 sigma the blur reads.
    #[test]
    fn an_effect_backdrop_draws_first_outside_the_nodes_layer_and_reaches_three_sigma() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, radius = 6, background = "#ffffff40", opacity = 0.5,
                effect = { backdrop = { blur = 4 } }, shadows = { { offset = { y = 4 } } }}"##,
        );
        let at = list.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })).expect("a backdrop");
        assert_eq!(
            list.commands[at].draw,
            Draw::Backdrop {
                sigma: 4.0,
                tone: node::Tone::default(),
                radius: Radii::from(6.0),
                alpha: 0.5,
                shader: None
            }
        );
        assert_eq!(list.commands[at].clip, PhysicalRect { x0: 28, y0: 28, x1: 92, y1: 72 });
        // CSS: the backdrop is what precedes the element, and its own box shadow is part of it.
        assert!(matches!(list.commands[at + 1].draw, Draw::Shadow { .. }), "the box shadow draws after");
        let content = effect_surface(
            r##"rect { width = 40, height = 20, background = "#ffffff40", effect = { backdrop = { blur = 4 } }, shadows = { { offset = { y = 4 } } },
                shadow_mode = "content" }"##,
        );
        let at = content.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })).unwrap();
        assert!(matches!(content.commands[at + 1].draw, Draw::Layer { .. }), "the layer draws over it");
        let plain = effect_surface(r##"rect { width = 40, height = 20, background = "#ffffff40" }"##);
        assert!(!plain.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })));
    }

    /// ADR-0334. A colour filter forces a layer or a backdrop read like a blur does, and every
    /// factor at `1` costs neither.
    #[test]
    fn a_colour_filter_forces_a_layer_or_backdrop_and_identity_costs_nothing() {
        let kinds = |src: &str| {
            let list = effect_surface(&format!(
                r##"rect {{ width = 40, height = 20, background = "#ffffff", effect = {src} }}"##
            ));
            let layer = list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Layer { .. }));
            (
                layer,
                list.commands.iter().find_map(|cmd| match cmd.draw {
                    Draw::Backdrop { sigma, tone, .. } => Some((sigma, tone)),
                    _ => None,
                }),
            )
        };
        let same =
            "{ saturate = 1, brightness = 1, contrast = 1, backdrop = { saturate = 1, brightness = 1, contrast = 1 } }";
        assert_eq!(kinds(same), (false, None));
        assert_eq!(kinds("{ brightness = 1.5 }"), (true, None));
        let tone = node::Tone { saturate: 2.0, ..node::Tone::default() };
        assert_eq!(kinds("{ backdrop = { saturate = 2 } }"), (false, Some((0.0, tone))));
        assert_eq!(kinds("{ backdrop = { blur = 3, saturate = 2 } }"), (false, Some((3.0, tone))));
    }

    /// Nothing to show at opacity 0, so nothing to read.
    #[test]
    fn a_fully_faded_glass_reads_no_backdrop() {
        let list =
            effect_surface(r##"rect { width = 40, height = 20, effect = { backdrop = { blur = 4 } }, opacity = 0 }"##);
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })), "{list:?}");
    }
    #[test]
    fn node_mask_images_and_captures_keep_resource_and_damage_tracking() {
        let list = masked(
            r##"rect { width = 80, height = 32, background = "#ffffff",
            mask = { node = "shape" }, children = { rect { id = "shape", width = 80, height = 32,
                children = { image { source = "/tmp/mask.png", width = 20, height = 20 },
                    capture { output = "TEST", width = 20, height = 20 } } } } }"##,
        );
        let images = pins(&list);
        assert_eq!(images, [(std::path::PathBuf::from("/tmp/mask.png"), (20, 20))]);
        assert!(list.draws_any_of(&[std::path::PathBuf::from("/tmp/mask.png")]));
        let mut captures = Vec::new();
        list.capture_nodes(&mut captures);
        assert_eq!(captures.len(), 1);
        assert!(list.captures_any_of(&[captures[0].node]));
        assert!(!list.damage_since(&list, true).is_empty());
    }
}

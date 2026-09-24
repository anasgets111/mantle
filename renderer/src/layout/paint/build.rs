//! Flattens a resolved tree into a [`DisplayList`], without a canvas or GL context.
//!
//! `node::paint_style` parses during `Scene::apply`; [`build_node`] reads typed data only. Drawing
//! is parent-then-child tree order (ADR-0023), and invisible subtrees draw nothing.
//!
//! `ResolvedNode.rect` is parent-relative, so [`build_node`] accumulates an absolute origin as it
//! descends instead of trusting `rect.x`/`rect.y` as already-absolute.

use crate::layout::node::{self, BorderColor, ClipShape, EdgeInsets, Fill, PaintStyle, Rgba, StyleRun};
use crate::layout::scene::{NodeId, ResolvedNode};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

use super::{DisplayList, Draw, DrawCmd, UNCLIPPED, command_bounds, grow, is_empty, shadow_rect, union};

/// Focused field and draw-safe content. Masked fields carry only destination and character count,
/// never secret bytes (`shared::SecureBuffer::expose_secret`, ADR-0005). Plain fields use `NodeId`,
/// not a box: when a notification moved a field, box-keyed focus lost its caret while keystrokes
/// continued landing in the buffer; a replacement field could also inherit its text (ADR-0099).
pub enum FieldFocus<'a> {
    Masked {
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
}

/// Flattens `root` without touching a canvas or GL context.
pub fn build(root: &ResolvedNode, scale: f32, focus: Option<&FieldFocus>) -> DisplayList {
    let mut commands = Vec::new();
    build_node(root, 0.0, 0.0, scale, (UNCLIPPED, root.rect), 1.0, focus, &mut commands);
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
    focus: Option<&FieldFocus>,
    out: &mut Vec<DrawCmd>,
) {
    if !node.visible {
        return;
    }

    let x = origin_x + node.rect.x;
    let y = origin_y + node.rect.y;
    let rect = LogicalRect { x, y, width: node.rect.width, height: node.rect.height };

    // Snap this box and intersect it with ancestor clips. Wrapped text is already rewritten by
    // `fit_text_to_box`; this remains a backstop for unwrapped overflow. Clips stay rectangular
    // here; `clip = "Rounded"` creates a grouped mask below.
    //
    // ponytail: `layout::hit` intersects the same rectangles but knows nothing about the arc, so a
    // pill's corner is outside its fill yet still takes a click (four pixels on a 34px control),
    // and a scoop's cut-out still takes the click and counts as input.
    // Upgrade path: hit testing should share this walk instead of a second copy of the rule.
    let (parent_clip, clip) = (clip, clip.intersect(snap_to_physical(rect, scale)));
    let child_clip = if node.clips_children() { clip } else { parent_clip };
    let effect = node.effect;
    let read = snap_to_physical(grow(rect, reach(effect.backdrop)), scale);
    let opacity = inherited_opacity * node.opacity;
    // ADR-0254 decision 2, ADR-0260. An opaque box draws as it did in either mode.
    let (radius, opaque, boxed) = match &node.paint {
        Some(PaintStyle::Box { background, radius, mask, .. }) => {
            let opaque = matches!(background, Some(Fill::Color(fill)) if fill.a >= 1.0)
                && mask.is_none()
                && effect.blur == 0.0
                && opacity >= 1.0;
            (*radius, opaque, !opaque && !effect.content_shadow)
        }
        _ => (0.0, false, false),
    };
    // A gradient cannot draw a scoop, so a scoop's box shadow is its silhouette's.
    let cast = effect.shadow.filter(|_| boxed || (opaque && radius >= 0.0));
    let layered = node::Effect { shadow: effect.shadow.filter(|_| cast.is_none()), ..effect };
    let own = layer_bounds(rect, layered, scale);
    let reach = match cast.filter(|_| radius >= 0.0) {
        Some(shadow) => union(
            snap_to_physical(grow(shadow_rect(rect, rect, shadow), 1.5 * shadow.blur), scale),
            if effect.blur > 0.0 { own } else { snap_to_physical(rect, scale) },
        ),
        None if effect.shadow.is_some() || effect.blur > 0.0 => layer_bounds(rect, effect, scale),
        None => child_clip,
    };
    // A box just scrolled out still casts the shadow reaching back in; one whose shadow is out still draws.
    if is_empty(parent_clip.intersect(reach)) {
        return;
    }

    // `node::paint_style` already decided the draw. An unrecognised kind stays transparent, which
    // avoids the passwordless black lock screen ADR-0052 decision 3 rejects. Opacity is baked into
    // the list because ADR-0063 skips unchanged lists; applying it in `execute` would be invisible.
    // A fully clipped node draws nothing, and its children cut to its box return on their own.
    let draw = if is_empty(clip) { None } else { draw_for(node, rect, scale, opacity, focus) };

    // A transformed node paints itself and its subtree as one group under its matrix
    // (ADR-0149), so the group is built into `out` and lifted out of it afterwards. Coordinates
    // inside stay the untransformed absolute ones this walk computes; the matrix is about the
    // node's absolute origin, so the canvas maps them at draw time. Scissors inside follow the
    // matrix too, femtovg's own rule, which is right for the node's own box.
    // ponytail: an ancestor's clip is carried along as well, so a scaled child overflowing its
    // parent is cut by the parent's box scaled with it, not the box itself. Upgrade path: set the
    // parent clip once outside the group and `intersect_scissor` inside.
    let start = out.len();
    let (x, y) = (rect.x, rect.y);
    let mask = match &node.paint {
        Some(PaintStyle::Box { mask: Some(mask), .. }) => Some(mask),
        _ => None,
    };
    // Outside the node's own offscreen, which holds nothing to read (ADR-0256).
    if let Some(PaintStyle::Box { radius, .. }) = node.paint
        && effect.backdrop > 0.0
        && opacity > 0.0
        && !is_empty(clip)
    {
        let draw = Draw::Backdrop { sigma: effect.backdrop, radius, alpha: opacity };
        out.push(DrawCmd { rect, clip: parent_clip.intersect(read), draw });
    }
    // After the backdrop: CSS's backdrop is what precedes the element, and its shadow is part of it.
    if let Some(shadow) = cast {
        let shadow = node::Shadow { color: fade(shadow.color, opacity), ..shadow };
        let draw = if radius >= 0.0 {
            Draw::Shadow { shadow, radius, knockout: boxed }
        } else {
            let effect = node::Effect { shadow: Some(shadow), ..node::Effect::default() };
            let black = Some(Fill::Color(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }));
            let fill =
                Draw::Box { background: black, radius, colors: BorderColor::default(), widths: EdgeInsets::default() };
            Draw::Layer { effect, silhouette: true, commands: vec![DrawCmd { rect, clip, draw: fill }] }
        };
        out.push(DrawCmd { rect, clip: parent_clip.intersect(reach), draw });
    }
    let body = out.len();
    match rounded_clip(node) {
        // A mask covers the node's own paint too, as Qt's `OpacityMask` covers its item (ADR-0255).
        radius if mask.is_some() => {
            let (fill, border) = split_fill_and_border(draw);
            let mut inner: Vec<DrawCmd> = fill.map(|draw| DrawCmd { rect, clip, draw }).into_iter().collect();
            for child in node.painted_children() {
                build_node(child, x, y, scale, (clip, surface), opacity, focus, &mut inner);
            }
            inner.extend(border.map(|draw| DrawCmd { rect, clip, draw }));
            if !inner.is_empty() {
                let box_px = (physical_edge(rect.width, scale), physical_edge(rect.height, scale));
                let mask = mask.cloned().map(|mask| (mask, box_px));
                out.push(DrawCmd {
                    rect,
                    clip,
                    draw: Draw::Clipped { radius: radius.unwrap_or(0.0), mask, commands: inner },
                });
            }
        }
        None => {
            if let Some(draw) = draw {
                out.push(DrawCmd { rect, clip, draw });
            }
            for child in node.painted_children() {
                build_node(child, x, y, scale, (child_clip, surface), opacity, focus, out);
            }
        }
        // Rounded order: fill, masked subtree, border. A child reaching the arc would
        // cover a border painted first.
        Some(radius) => {
            let (fill, border) = split_fill_and_border(draw);
            if let Some(fill) = fill {
                out.push(DrawCmd { rect, clip, draw: fill });
            }
            let mut inner = Vec::new();
            for child in node.painted_children() {
                build_node(child, x, y, scale, (clip, surface), opacity, focus, &mut inner);
            }
            // A leaf has nothing to clip, so avoid the render target and composite.
            if !inner.is_empty() {
                out.push(DrawCmd { rect, clip, draw: Draw::Clipped { radius, mask: None, commands: inner } });
            }
            if let Some(border) = border {
                out.push(DrawCmd { rect, clip, draw: border });
            }
        }
    }
    if (layered.shadow.is_some() || layered.blur > 0.0) && out.len() > body {
        let commands: Vec<DrawCmd> = out.drain(body..).collect();
        // A transformed child overflowing the box keeps the overflow it has without the layer.
        let bounds = commands.iter().map(command_bounds).filter(|r| !is_empty(*r)).fold(own, union);
        // ponytail: a negative spread pulls in content from further out than this. Upgrade path:
        // invert `shadow_rect` about the box.
        let pad = layered
            .shadow
            .map_or(0.0, |shadow| self::reach(shadow.blur / 2.0) + shadow.offset.0.abs().max(shadow.offset.1.abs()));
        let target = snap_to_physical(grow(surface, pad.max(self::reach(layered.blur))), scale);
        out.push(DrawCmd {
            rect,
            clip: parent_clip.intersect(bounds).intersect(target),
            draw: Draw::Layer { effect: layered, silhouette: false, commands },
        });
    }
    if !node.transform.is_identity() {
        let matrix = node.transform.matrix(rect);
        let commands: Vec<DrawCmd> = out.drain(start..).collect();
        out.push(DrawCmd { rect, clip: child_clip, draw: Draw::Transformed { matrix, commands } });
    }
}

/// The non-zero radius of a node whose children use a rounded clip.
fn rounded_clip(node: &ResolvedNode) -> Option<f32> {
    match node.paint {
        Some(PaintStyle::Box { clip: ClipShape::Rounded, radius, .. }) if radius != 0.0 => Some(radius),
        _ => None,
    }
}

/// Splits a box into fill and border so [`build_node`] can mask children between them. Other draws
/// stay whole; [`rounded_clip`] only returns for boxes.
fn split_fill_and_border(draw: Option<Draw>) -> (Option<Draw>, Option<Draw>) {
    let Some(Draw::Box { background, radius, colors, widths }) = draw else {
        return (draw, None);
    };
    let fill = background.map(|color| Draw::Box {
        background: Some(color),
        radius,
        colors: BorderColor::default(),
        widths: EdgeInsets::default(),
    });
    let border = (widths != EdgeInsets::default()).then_some(Draw::Box { background: None, radius, colors, widths });
    (fill, border)
}

/// Multiplies `opacity` into an existing alpha: half-transparent inside a half-faded panel is a
/// quarter.
fn fade(color: Rgba, opacity: f32) -> Rgba {
    Rgba { a: color.a * opacity, ..color }
}

/// Multiplies border-edge alpha; absent edges stay absent.
fn fade_border(colors: BorderColor, opacity: f32) -> BorderColor {
    BorderColor {
        top: colors.top.map(|c| fade(c, opacity)),
        right: colors.right.map(|c| fade(c, opacity)),
        bottom: colors.bottom.map(|c| fade(c, opacity)),
        left: colors.left.map(|c| fade(c, opacity)),
    }
}

/// Converts a node's parsed paint to a draw. `scale` supplies physical image size and `focus`
/// supplies field content; malformed properties already failed `Scene::apply`.
///
/// Takes the node rather than its `paint`, because an `image` reads three things off it (the
/// paint, the source it last had a texture for, and any dissolve crossing between them), and the
/// pass supplies only the geometry.
fn draw_for(
    node: &ResolvedNode,
    rect: LogicalRect,
    scale: f32,
    opacity: f32,
    focus: Option<&FieldFocus>,
) -> Option<Draw> {
    let node_id = node.id;
    let retained = node.displayed_source.as_deref();
    let dissolve = node.dissolve.as_deref();
    match node.paint.as_ref()? {
        // The shared paint of `rect`/`row`/`column`/`button` and all four surface roles: background
        // fill, then borders. `clip` is not read here: it decides what this node's *children* are
        // cut to, `build_node`'s question, not this one's.
        PaintStyle::Box { background, radius, colors, widths, clip: _, mask: _ } => Some(Draw::Box {
            background: background.as_ref().map(|fill| match fill {
                Fill::Color(color) => Fill::Color(fade(*color, opacity)),
                Fill::Gradient(gradient) => Fill::Gradient(node::Gradient {
                    stops: gradient.stops.iter().map(|(at, color)| (*at, fade(*color, opacity))).collect(),
                    ..*gradient
                }),
            }),
            radius: *radius,
            colors: fade_border(*colors, opacity),
            widths: *widths,
        }),

        // `text`: `content` through `TextPainter`, at `rect`, coloured by `foreground`. `elide`,
        // `wrap` and `max_lines` are absent on purpose: `Scene::apply` already rewrote `content` to
        // the string that fits (ellipsized, or line-broken with `\n`) in the only place the box
        // width and the shaping worker are both in reach.
        PaintStyle::Text { content, runs, font_size, font, color, align, elide: _, wrap: _, max_lines: _ } => {
            Some(Draw::Text {
                content: content.clone(),
                runs: runs
                    .iter()
                    .map(|run| StyleRun { color: run.color.map(|c| fade(c, opacity)), ..run.clone() })
                    .collect(),
                font_size: *font_size,
                font: font.clone(),
                color: fade(*color, opacity),
                align: *align,
                centered: false,
                caret: None,
                caret_on: false,
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
        PaintStyle::Image { source, fit, load, retain, transition, source_blur } => (!source.is_empty()).then(|| {
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
                shader: dissolve
                    .and_then(|dissolve| dissolve.spec.shader.clone().map(|path| (path, dissolve.spec.params.clone()))),
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
            }
        }),

        // A `textfield` shows its placeholder until focused, then one mask character per typed
        // character. Wrong-password feedback costs a two-second `pam_fail_delay`; three failures
        // trigger `pam_faillock` and a ten-minute lockout. `retarget_secure_submit` zeroizes the
        // buffer on focus changes, so only the focused field can show typed state.
        PaintStyle::TextField { target, placeholder, mask, font_size, color, align } => {
            let (content, caret, caret_on) = match focus {
                // An empty masked field remains a prompt.
                Some(FieldFocus::Masked { target: focused, filled }) if *filled > 0 => {
                    if target.as_ref().is_some_and(|declared| declared == *focused) {
                        (mask.repeat(*filled), None, false)
                    } else {
                        (placeholder.clone(), None, false)
                    }
                }
                // Empty focused fields show the placeholder rather than a bare caret (ADR-0135):
                // the caret-only rule hid the prompt of every `autofocus` field, which holds the
                // keyboard from the first frame. Keep `target.is_none()` beside the id: the same
                // node may gain `secure_submit`, and a masked field must never draw plain text.
                Some(FieldFocus::Plain { id, text, caret, caret_on }) if *id == node_id && target.is_none() => {
                    match text.is_empty() && !placeholder.is_empty() {
                        true => (placeholder.clone(), None, false),
                        // The draft remains visible without a caret (ADR-0108).
                        false => (text.to_string(), *caret, *caret_on),
                    }
                }
                _ => (placeholder.clone(), None, false),
            };
            // An empty field with no placeholder still draws, for the caret alone (ADR-0135
            // decision 2).
            (!content.is_empty() || caret.is_some()).then_some(Draw::Text {
                content: content.into(),
                runs: Vec::new(),
                font_size: *font_size,
                // A `textfield` draws its placeholder and its masked content in the declared
                // chain; nothing lets one name a family.
                font: None,
                color: fade(*color, opacity),
                align: *align,
                centered: true,
                caret,
                caret_on,
            })
        }

        // `capture` (ADR-0248): empty `output` draws nothing, the same rule `image`'s empty
        // `source` follows.
        PaintStyle::Capture { output, fit, live, paint_cursor, region } => {
            (!output.is_empty()).then(|| Draw::Capture {
                node: node_id,
                output: output.clone(),
                fit: *fit,
                alpha: opacity,
                live: *live,
                paint_cursor: *paint_cursor,
                region: *region,
            })
        }

        PaintStyle::Shader { source, progress, params } => (!source.is_empty()).then(|| Draw::Shader {
            source: source.into(),
            version: crate::image::FileVersion::read(source.as_ref()),
            progress: *progress,
            params: params.clone(),
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
/// covers some area, but a blur may genuinely be off. `parse_source_blur` already rejects
/// negative, infinite and NaN logical values, and `scale` is always positive, so unlike
/// `physical_edge` there is no out-of-range input here to clamp.
fn physical_blur(logical: f32, scale: f32) -> u32 {
    (logical * scale).round() as u32
}

/// How far a Gaussian of `sigma` spreads: the 3 sigma its kernel samples (ADR-0262).
fn reach(sigma: f32) -> f32 {
    3.0 * sigma
}

/// A layer's offscreen: the box padded for the further-reaching blur, and where that padded box
/// lands as the shadow.
fn layer_bounds(rect: LogicalRect, effect: node::Effect, scale: f32) -> PhysicalRect {
    let shadow_reach = effect.shadow.map_or(0.0, |shadow| reach(shadow.blur / 2.0));
    let padded = grow(rect, shadow_reach.max(reach(effect.blur)));
    let own = snap_to_physical(padded, scale);
    effect.shadow.map_or(own, |shadow| union(own, snap_to_physical(shadow_rect(rect, padded, shadow), scale)))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{IMAGE_MASKED, effect_surface, masked, resolved_surface};
    use super::*;

    use mlua::Lua;

    use crate::layout::node::TextAlign;
    use crate::layout::scene::LogicalSize;

    // display list (`build`), the seam that needs no EGL context

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
                transition = { duration = 400, easing = "Linear" },
                width = "Fill", height = "Fill" } }"##;
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
                width = "Fill", height = "Fill" } }"##;
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
                transition = { duration = 400, easing = "Linear" },
                width = "Fill", height = "Fill" } }"##;
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
            },
            progress: 0.25,
        }));
        assert_eq!(image_draw(&tree), Some((Some("/tmp/old.png".to_string()), Some(0.25))));

        // Both are drawn, so both are pinned; losing the outgoing mid-cross is a hole in the frame.
        let mut pinned = Vec::new();
        build(&tree, 1.0, None).drawn_images(&mut pinned);
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
                width = "Fill", height = "Fill" } }"##;
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
        let mut pinned = Vec::new();
        list.drawn_images(&mut pinned);
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
                width = "Fill", height = "Fill" } }"##;
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
            [("Center", TextAlign::Center), ("End", TextAlign::End), ("Start", TextAlign::Start)]
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
            child = textfield { width = 180, height = 24, placeholder = "password", foreground = "#ffffffff", text_align = "Center" } }"##;
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
        let a = build(&resolved_surface(&Lua::new(), &src("Start"), size), 1.0, None);
        let b = build(&resolved_surface(&Lua::new(), &src("Center"), size), 1.0, None);
        assert_ne!(a, b);
    }

    fn box_alpha(cmd: &DrawCmd) -> f32 {
        match &cmd.draw {
            Draw::Box { background: Some(Fill::Color(color)), .. } => color.a,
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
        let Draw::Box { colors, .. } = &list.commands[0].draw else {
            panic!("expected a box");
        };
        assert_eq!(colors.top.unwrap().a, 0.5);
        assert!(colors.bottom.is_none(), "an edge the config never coloured is not faded into existence");
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
                Draw::Box { background: Some(Fill::Color(color)), .. } => Some(*color),
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

    /// A child's clip is its own box intersected with its parent's, never wider. This is the
    /// invariant that lets [`execute`] call `scissor` outright instead of rebuilding an
    /// `intersect_scissor` nest.
    #[test]
    fn a_childs_clip_never_escapes_its_parents_box() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { background = "#445566ff", width = 40, height = 10,
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

    /// A node scrolled or positioned entirely outside its parent draws nothing, so it earns no
    /// entry, and, more usefully, moving it around off-screen produces no list change and so no
    /// repaint.
    #[test]
    fn a_subtree_clipped_to_nothing_is_left_out_entirely() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { background = "#445566ff", width = 0, height = 0,
                children = { text { content = "offscreen", foreground = "#ffffffff" } } } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        assert!(
            !list.commands.iter().any(|c| matches!(&c.draw, Draw::Text { content, .. } if &**content == "offscreen")),
            "a zero-area parent clips its child to nothing, so neither belongs in the list: {list:?}"
        );
    }

    // textfield masking

    fn lock_target() -> node::SecureSubmitTarget {
        node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string() }
    }

    /// One surface holding a password field.
    fn password_surface(lua: &Lua) -> ResolvedNode {
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "Fill", height = 28, placeholder = "password",
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
            child = textfield { width = "Fill", height = 28, placeholder = "Reply",
                on_submit = function(text) end } }"##;
        resolved_surface(lua, src, LogicalSize { width: 200.0, height: 40.0 })
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
            child = textfield { width = "Fill", height = 28, on_submit = function(text) end } }"##;
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
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &target, filled: 4 }));
        assert_eq!(drawn_text(&list), vec!["****".to_string()]);
    }

    #[test]
    fn a_focused_but_empty_password_field_still_shows_its_placeholder() {
        let lua = Lua::new();
        let target = lock_target();
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &target, filled: 0 }));
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    /// Focus is a `{ capability, action }` pair, so a field addressed somewhere else must not
    /// fill just because some other field is focused on the same surface. This is the same
    /// routing rule `input::keyboard::retarget_secure_submit` enforces for the bytes themselves.
    #[test]
    fn a_field_addressed_to_another_capability_does_not_draw_the_focused_fields_characters() {
        let lua = Lua::new();
        let elsewhere = node::SecureSubmitTarget { capability: "network".to_string(), action: "connect".to_string() };
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &elsewhere, filled: 9 }));
        assert_eq!(
            drawn_text(&list),
            vec!["password".to_string()],
            "a PSK's length must not leak onto the lock screen's field"
        );
    }

    /// The count is all paint ever gets (see [`FieldFocus`]), so there is no path by which a
    /// typed character reaches the list. Asserted because a display list is cloned, compared and
    /// retained in `last_painted`, exactly the places ADR-0005 keeps a secret out of.
    #[test]
    fn a_masked_field_draws_only_the_mask_character() {
        let lua = Lua::new();
        let target = lock_target();
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &target, filled: 6 }));
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
            child = textfield { width = "Fill", height = 28,
                secure_submit = { capability = "lock", action = "authenticate" } } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let target = lock_target();
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { target: &target, filled: 3 }));
        assert_eq!(drawn_text(&list), vec!["\u{2022}\u{2022}\u{2022}".to_string()]);
    }

    /// The property this feature needs from the display list: typing has to change it, or
    /// `paint_surface` skips the repaint and the dots never appear.
    #[test]
    fn each_typed_character_changes_the_list_so_the_repaint_is_not_skipped() {
        let lua = Lua::new();
        let tree = password_surface(&lua);
        let target = lock_target();
        let three = build(&tree, 1.0, Some(&FieldFocus::Masked { target: &target, filled: 3 }));
        let four = build(&tree, 1.0, Some(&FieldFocus::Masked { target: &target, filled: 4 }));
        assert_ne!(three, four);
    }

    /// A leaf that asks for a rounded clip has nothing to clip, so it buys no offscreen pass.
    #[test]
    fn a_childless_rounded_clip_builds_no_group() {
        let lua = Lua::new();
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, child = rect {
                width = 80, height = 32, radius = 16, clip = "Rounded", background = "#0000FFFF",
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
                    children = {{ rect {{ width = 30, height = "Fill", background = "#0000FFFF" }} }},
                }} }}"##
            )
        };
        let boxed = build(&resolved_surface(&Lua::new(), &src("Box"), size), 1.0, None);
        let rounded = build(&resolved_surface(&Lua::new(), &src("Rounded"), size), 1.0, None);
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

    /// Qt's `OpacityMask` covers the item, not only its children, so the node's own fill and border
    /// are drawn inside the masked group, in the order an unmasked box draws them.
    #[test]
    fn a_mask_groups_the_nodes_fill_subtree_and_border_in_paint_order() {
        let list = masked(IMAGE_MASKED);
        let [_, group] = list.commands.as_slice() else { panic!("the panel's box, then one group: {list:?}") };
        let Draw::Clipped { radius, mask: Some((mask, box_px)), commands } = &group.draw else {
            panic!("expected a masked group, got {:?}", group.draw)
        };
        assert_eq!((*radius, *box_px, mask.invert), (0.0, (80, 32), false));
        let order: Vec<_> = commands
            .iter()
            .map(|cmd| match &cmd.draw {
                Draw::Box { background: Some(_), widths, .. } if *widths == EdgeInsets::default() => "fill",
                Draw::Box { background: None, .. } => "border",
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(order, ["fill", "fill", "border"], "own fill, child, own border");
    }

    #[test]
    fn a_masked_rounded_box_composites_through_its_radius_and_a_leaf_still_groups() {
        let list = masked(
            r##"rect { width = 80, height = 32, radius = 8, clip = "Rounded", background = "#0000FFFF",
                mask = { source = "/nonexistent/mask.svg" } }"##,
        );
        let Draw::Clipped { radius, mask: Some(_), commands } = &list.commands[1].draw else { panic!("{list:?}") };
        assert_eq!((*radius, commands.len()), (8.0, 1));
    }

    /// Opacity is baked into the list (ADR-0063), so a gradient fades stop by stop like a colour.
    #[test]
    fn opacity_fades_every_gradient_stop() {
        let list = masked(
            r##"rect { width = 80, height = 32, opacity = 0.5,
                background = { gradient = "Radial", stops = { { 0, "#ffffff" }, { 1, "#ffffff80" } } } }"##,
        );
        let Draw::Box { background: Some(Fill::Gradient(gradient)), .. } = &list.commands[1].draw else {
            panic!("{list:?}")
        };
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
                clip = "Rounded", children = {{ {}, {}, {} }} }} }}"##,
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
                Draw::Box { background: Some(Fill::Color(color)), .. } => vec![(color.r * 255.0).round() as u8],
                Draw::Clipped { commands, .. } | Draw::Layer { commands, .. } | Draw::Transformed { commands, .. } => {
                    fill_order(commands)
                }
                _ => Vec::new(),
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
                    shadow_color = "#00000080", shadow_blur = 8, shadow_offset = {{ y = 4 }}, shadow_spread = 2 }}"##
            ))
        };
        let list = card("opacity = 0.5,");
        let at = list.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow");
        let Draw::Shadow { shadow, radius, knockout } = list.commands[at].draw else { unreachable!() };
        assert_eq!((radius, knockout), (6.0, true), "a fading card shows no shadow through its body");
        assert!((shadow.color.a - 0.5 * 128.0 / 255.0).abs() < 1e-6, "faded with the node: {shadow:?}");
        assert!(matches!(list.commands[at + 1].draw, Draw::Box { .. }), "the fill covers the shadow");
        // The box is 40..80 x 40..60; the shadow's box is 38..82 x 42..66, blurred 3 sigma, 12, further out.
        assert_eq!(list.commands[at].clip, PhysicalRect { x0: 26, y0: 30, x1: 94, y1: 78 });
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Layer { .. })), "no offscreen");
        let opaque = card("");
        assert!(opaque.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Shadow { knockout: false, .. })));
        assert_eq!(card(r#"shadow_mode = "Content","#), opaque, "the same in either mode (ADR-0260)");
    }

    /// ADR-0254. Anything but an opaque box casts the shadow of its pixels, so its subtree goes
    /// offscreen as one group. Its content already carries the opacity, so the shadow colour
    /// does not fade twice.
    #[test]
    fn text_or_a_translucent_box_casts_its_shadow_through_one_offscreen_layer() {
        for child in [
            r##"text { content = "hi", opacity = 0.5, shadow_blur = 4, shadow_offset = { x = 3 } }"##,
            r##"rect { width = 40, height = 20, background = "#ffffff80", opacity = 0.5, shadow_blur = 4,
                shadow_offset = { x = 3 }, shadow_mode = "Content", children = { text { content = "hi" } } }"##,
        ] {
            let list = effect_surface(child);
            let layer = list.commands.last().unwrap();
            let Draw::Layer { effect: node::Effect { shadow: Some(shadow), blur, .. }, commands, .. } = &layer.draw
            else {
                panic!("{child}: expected a layer, got {:?}", layer.draw)
            };
            assert_eq!((shadow.color.a, *blur), (1.0, 0.0), "{child}");
            assert!(commands.iter().any(|cmd| matches!(cmd.draw, Draw::Text { .. })), "{child}");
            // The blur reaches 3 sigma, 6px, around the box, and the offset shifts its right edge 3.
            let node = snap_to_physical(layer.rect, 1.0);
            assert_eq!((layer.clip.x0, layer.clip.y0), (node.x0 - 6, node.y0 - 6), "{child}");
            assert_eq!((layer.clip.x1, layer.clip.y1), (node.x1 + 9, node.y1 + 6), "{child}");
        }
    }

    /// ADR-0260. A box's shadow is its own shape whatever it holds: one gradient knocked out under
    /// the box, faded with the node, and nothing offscreen, so the label casts nothing.
    #[test]
    fn a_translucent_box_casts_its_box_shadow_as_a_knocked_out_gradient() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, radius = 6, background = "#ffffff40", opacity = 0.5,
                shadow_blur = 4, shadow_offset = { y = 4 }, children = { text { content = "hi" } } }"##,
        );
        let at = list.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow");
        let Draw::Shadow { shadow, radius, knockout } = list.commands[at].draw else { unreachable!() };
        assert_eq!((radius, knockout, shadow.color.a), (6.0, true, 0.5));
        assert!(matches!(list.commands[at + 1].draw, Draw::Box { .. }), "the fill over it");
        assert!(
            list.commands[at + 2..].iter().any(|cmd| matches!(cmd.draw, Draw::Text { .. })),
            "the label unshadowed"
        );
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Layer { .. })), "no offscreen");
    }

    /// ADR-0260. `content_blur` still takes a layer, and the box shadow stays a gradient outside it.
    /// A scoop, which a gradient cannot draw, casts its fill's silhouette alone through a layer.
    #[test]
    fn a_box_shadow_never_rides_the_bodys_layer() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, background = "#ffffff40", content_blur = 2, shadow_offset = { y = 4 } }"##,
        );
        assert!(matches!(list.commands[1].draw, Draw::Shadow { knockout: true, .. }), "{:?}", list.commands);
        assert!(matches!(list.commands[2].draw, Draw::Layer { effect: node::Effect { shadow: None, .. }, .. }));
        assert_eq!(list.commands[2].clip, PhysicalRect { x0: 34, y0: 34, x1: 86, y1: 66 }, "the blur's reach alone");

        let list = effect_surface(
            r##"rect { width = 40, height = 20, radius = 6, corner_shape = "Scoop", background = "#ffffff40",
                opacity = 0.5, shadow_offset = { y = 4 }, children = { text { content = "hi" } } }"##,
        );
        let Draw::Layer { effect, silhouette: true, commands } = &list.commands[1].draw else {
            panic!("{:?}", list.commands)
        };
        assert_eq!(effect.shadow.map(|shadow| shadow.color.a), Some(0.5), "faded with the node");
        let [DrawCmd { draw: Draw::Box { background: Some(node::Fill::Color(fill)), radius, .. }, .. }] =
            commands.as_slice()
        else {
            panic!("the silhouette alone: {commands:?}")
        };
        assert_eq!((fill.a, *radius), (1.0, -6.0));
        assert!(list.commands[2..].iter().any(|cmd| matches!(cmd.draw, Draw::Text { .. })), "the label outside it");
    }

    /// ADR-0254, ADR-0262. `content_blur` spreads the subtree's pixels 3 sigma past its box, at
    /// any sigma.
    #[test]
    fn a_content_blur_groups_the_subtree_and_reaches_three_sigma() {
        for (blur, reach) in [(2, 6), (12, 36)] {
            let list = effect_surface(&format!(
                r##"rect {{ width = 40, height = 20, background = "#ffffff", content_blur = {blur} }}"##
            ));
            let layer = list.commands.last().unwrap();
            assert!(
                matches!(layer.draw, Draw::Layer { effect: node::Effect { shadow: None, .. }, .. }),
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
            r##"rect { width = 40, height = 20, children = { rect { width = 40, height = 20, margin = { top = 24 },
                background = "#ffffff", shadow_offset = { y = -10 } } } }"##,
        );
        let shadow = list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow");
        assert_eq!((shadow.clip.y0, shadow.clip.y1), (54, 60), "the part of it inside the parent");
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Box { .. } if cmd.rect.y == 64.0)));
    }

    /// `clip = "None"` hands a node's children its parent's clip: a wrapper exactly its child's size
    /// no longer cuts the child's shadow, nor a child laid out past it once the wrapper scrolls away.
    #[test]
    fn an_unclipped_wrapper_leaves_its_childs_shadow_and_overflow_whole() {
        let wrapped = |clip: &str| {
            effect_surface(&format!(
                r##"column {{ clip = "{clip}", children = {{ rect {{ width = 40, height = 20, background = "#ffffff",
                    shadow_blur = 8, shadow_offset = {{ y = 4 }} }} }} }}"##
            ))
        };
        let shadow = |list: &DisplayList| {
            list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Shadow { .. })).expect("a shadow").clip
        };
        assert_eq!(shadow(&wrapped("Box")), PhysicalRect { x0: 40, y0: 40, x1: 80, y1: 60 }, "cut to the wrapper");
        assert_eq!(shadow(&wrapped("None")), PhysicalRect { x0: 28, y0: 32, x1: 92, y1: 76 });

        let list = effect_surface(
            r##"rect { width = 40, height = 20, clip = "None", children = { rect { width = 40, height = 20,
                margin = { top = 24 }, background = "#ffffff" } } }"##,
        );
        assert!(list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Box { .. }) && cmd.rect.y == 64.0));
    }

    /// Under a chain of `clip = "None"` to the surface, a layer's offscreen stops at the surface
    /// grown by what its blur and shadow reach back in from: 3 sigma plus the offset.
    #[test]
    fn a_layer_under_unclipped_ancestors_stops_near_the_surface() {
        let src = r##"return panel { id = "bar", width = 200, height = 100, clip = "None", child = rect {
            width = 40, height = 20, clip = "None", content_blur = 1,
            shadow_blur = 4, shadow_offset = { x = 5 }, shadow_mode = "Content",
            children = { rect { width = 8000, height = 8000, margin = { left = -4000 }, background = "#ffffff" } } } }"##;
        let list = build(&resolved_surface(&Lua::new(), src, LogicalSize { width: 200.0, height: 100.0 }), 1.0, None);
        let layer = list.commands.iter().find(|cmd| matches!(cmd.draw, Draw::Layer { .. })).expect("a layer");
        assert_eq!(layer.clip, PhysicalRect { x0: -11, y0: -6, x1: 211, y1: 111 });
    }

    /// A child scaled past its layered parent's box keeps the overflow it would have without the
    /// layer.
    #[test]
    fn a_layer_covers_a_transformed_child_overflowing_its_box() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, content_blur = 1,
                children = { rect { width = 40, height = 20, background = "#ffffff", scale = 2 } } }"##,
        );
        let layer = list.commands.last().unwrap();
        assert!(matches!(layer.draw, Draw::Layer { .. }));
        assert!(layer.clip.x0 <= 20 && layer.clip.x1 >= 100, "the child's scaled box: {:?}", layer.clip);
    }

    /// ADR-0256. The backdrop is read before the node paints anything and outside the offscreen a
    /// shadow draws the node into, faded with it, and its clip covers the 3 sigma the blur reads.
    #[test]
    fn a_backdrop_blur_draws_first_outside_the_nodes_layer_and_reaches_three_sigma() {
        let list = effect_surface(
            r##"rect { width = 40, height = 20, radius = 6, background = "#ffffff40", opacity = 0.5,
                backdrop_blur = 4, shadow_offset = { y = 4 } }"##,
        );
        let at = list.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })).expect("a backdrop");
        assert_eq!(list.commands[at].draw, Draw::Backdrop { sigma: 4.0, radius: 6.0, alpha: 0.5 });
        assert_eq!(list.commands[at].clip, PhysicalRect { x0: 28, y0: 28, x1: 92, y1: 72 });
        // CSS: the backdrop is what precedes the element, and its own box shadow is part of it.
        assert!(matches!(list.commands[at + 1].draw, Draw::Shadow { .. }), "the box shadow draws after");
        let content = effect_surface(
            r##"rect { width = 40, height = 20, background = "#ffffff40", backdrop_blur = 4, shadow_offset = { y = 4 },
                shadow_mode = "Content" }"##,
        );
        let at = content.commands.iter().position(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })).unwrap();
        assert!(matches!(content.commands[at + 1].draw, Draw::Layer { .. }), "the layer draws over it");
        let plain = effect_surface(r##"rect { width = 40, height = 20, background = "#ffffff40" }"##);
        assert!(!plain.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })));
    }

    /// Nothing to show at opacity 0, so nothing to read.
    #[test]
    fn a_fully_faded_glass_reads_no_backdrop() {
        let list = effect_surface(r##"rect { width = 40, height = 20, backdrop_blur = 4, opacity = 0 }"##);
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Backdrop { .. })), "{list:?}");
    }
}

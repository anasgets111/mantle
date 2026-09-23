//! Draws a resolved layout tree onto a shared femtovg canvas.
//!
//! [`build`] turns a resolved tree into plain Rust [`DisplayList`] data; [`execute`] sends it to
//! femtovg. `paint_surface` skips drawing and `eglSwapBuffers` when the list is unchanged, while
//! `build` stays testable without EGL. A full-surface commit recomposites the whole screen behind
//! it, so unchanged lists skip that cost. On an idle bar with a clock, a 1920x1200 wallpaper went
//! from repainting twice a second to never and niri CPU fell about a third.
//!
//! `node::paint_style` parses during `Scene::apply`; [`build_node`] reads typed data only. Drawing
//! is parent-then-child tree order (ADR-0023), and invisible subtrees draw nothing.
//!
//! `ResolvedNode.rect` is parent-relative, so [`build_node`] accumulates an absolute origin as it
//! descends instead of trusting `rect.x`/`rect.y` as already-absolute.
//!
//! The canvas half, [`execute`] and everything it draws with, is in `canvas`.

mod canvas;

pub use canvas::{DrawnImage, Shaders, execute};

use crate::image::{self, Fit, Load};
use crate::layout::node::{self, BorderColor, ClipShape, EdgeInsets, PaintStyle, Rgba, StyleRun, TextAlign};
use crate::layout::scene::{NodeId, ResolvedNode};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

/// Typed draw data. `textfield` and unrecognised kinds contribute no [`DrawCmd`]. No Lua values are
/// kept: mlua table identity would make a signal-resolved table unequal every pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Draw {
    /// Box fill, then border, for containers and all surface roles.
    Box { background: Option<Rgba>, radius: f32, colors: BorderColor, widths: EdgeInsets },
    Text {
        content: std::sync::Arc<str>,
        /// Byte ranges drawn in another face, underlined, or recoloured (ADR-0104).
        runs: Vec<StyleRun>,
        font_size: f32,
        /// The family this was measured and drawn in, or `None` for the declared chain
        /// (ADR-0144).
        font: Option<std::sync::Arc<str>>,
        color: Rgba,
        align: TextAlign,
        /// Center a `textfield` line; ordinary text starts at the top of its content box.
        centered: bool,
        /// A focused plain `textfield`'s `(anchor, caret)` byte offsets into `content` (ADR-0236).
        /// Here rather than in a second command because the rects are glyph positions, and paint
        /// is where the glyphs are. `None` for every other node.
        caret: Option<(usize, usize)>,
        /// The blink's phase; see [`FieldFocus::Plain`].
        caret_on: bool,
    },
    /// Theme name, resolved in [`execute`]. Icons carry alpha separately because `Paint::image`
    /// takes it as an argument.
    Icon {
        name: String,
        px: u32,
        alpha: f32,
        /// `currentColor` tint (ADR-0072); `ImageCache` keys on it.
        color: Option<Rgba>,
    },
    /// Node box in physical pixels, used as the `ImageCache` key; the cache downscales a raster to
    /// cover it (ADR-0122).
    /// `retained` is the source this node last had a texture for, carried when `retain` is set
    /// and `source` has not caught up to it yet (ADR-0180); `canvas::run` draws it if `source` has no
    /// texture. Present only while the two differ, so a settled node's list stops changing.
    Image {
        /// Which retained node this is, so [`execute`] can report back the source it actually drew
        /// (ADR-0183). Readiness is not knowable anywhere else: only the draw has the exact cache
        /// key, and only it can tell a decode that landed from one that failed or was never asked.
        node: NodeId,
        source: String,
        fit: Fit,
        box_px: (u32, u32),
        alpha: f32,
        load: Load,
        retained: Option<String>,
        /// Mid-cross-dissolve (ADR-0181), the alpha `source` is drawn at over `retained`. `None`
        /// when the node is showing one picture, which is when `retained` is a gap cover rather
        /// than the source being crossed away from.
        dissolve: Option<f32>,
        /// The config shader this cross is drawn with and the `params` it is given (ADR-0184).
        /// `None` is the built-in dissolve, and so is a shader that would not build.
        shader: Option<(std::path::PathBuf, Vec<(String, f32)>)>,
        /// `image.source_blur` in physical pixels (ADR-0240). `0` for no blur, which `ImageCache`
        /// never distinguishes from a request it decided not to run.
        blur_px: u32,
    },
    /// An output's live contents (ADR-0248). `wayland::capture` owns the texture, keyed by `node`;
    /// this carries what a draw places it with and what the capture registry paces a source by,
    /// the same split `Draw::Image` makes between pixels and policy.
    Capture { node: NodeId, output: String, fit: Fit, alpha: f32, live: bool, paint_cursor: bool },
    /// A subtree masked by the declaring node's rounded arc. Rectangular clips flatten into each
    /// command; rounded clips stay grouped for [`execute`].
    Clipped { radius: f32, commands: Vec<DrawCmd> },
    /// The subtree of a node with a `scale`/`rotate`/`translate` (ADR-0149), drawn under its
    /// affine. Coordinates inside are the untransformed absolute ones.
    Transformed { matrix: node::Affine, commands: Vec<DrawCmd> },
}

/// One drawable node: what, where, and its precomputed ancestor clip. Intersections are axis
/// aligned and associative, so [`execute`] can set one scissor instead of rebuilding a nest.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawCmd {
    pub rect: LogicalRect,
    pub clip: PhysicalRect,
    pub draw: Draw,
}

/// One `capture` node as `DisplayList::capture_nodes` reports it: identity plus everything
/// `wayland::App::sync_captures` paces a source by. No `fit`/`alpha`/`box_px`: those are draw
/// concerns the registry never reads.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureNode {
    pub node: NodeId,
    pub output: String,
    pub live: bool,
    pub paint_cursor: bool,
}

/// One surface's draw order, flattened for equality. Before this, the single dirty flag repainted
/// every mapped surface on every re-resolve (ADR-0044 decision 2). Float equality is safe because
/// identical inputs produce identical bits; `NaN` repaints forever rather than leaving stale
/// pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayList {
    pub commands: Vec<DrawCmd>,
}

/// Walks `commands` for a leaf `matches`, descending into `Clipped`/`Transformed` subtrees.
/// Shared by [`DisplayList::draws_any_of`] and [`DisplayList::captures_any_of`], which differ only
/// in which `Draw` variant and field they compare.
fn any_draw_matches(commands: &[DrawCmd], matches: impl Fn(&Draw) -> bool + Copy) -> bool {
    commands.iter().any(|command| match &command.draw {
        Draw::Clipped { commands, .. } | Draw::Transformed { commands, .. } => any_draw_matches(commands, matches),
        draw => matches(draw),
    })
}

impl DisplayList {
    /// Whether any `image` in this list draws one of `files` (ADR-0122). A background decode
    /// landing changes no list, since a list names the file and not the texture, so this is how
    /// `wayland::App` tells which surfaces' skipped repaint is now stale.
    pub fn draws_any_of(&self, files: &[std::path::PathBuf]) -> bool {
        any_draw_matches(&self.commands, |draw| {
            matches!(draw, Draw::Image { source, retained, .. } if files.iter().any(|file| {
                file.as_os_str() == source.as_str()
                    || retained.as_ref().is_some_and(|cover| file.as_os_str() == cover.as_str())
            }))
        })
    }

    /// Whether any `capture` in this list is one of `nodes` (ADR-0248): a landed frame names the
    /// node it uploads for, mirroring [`Self::draws_any_of`]'s file-name lookup for `image`.
    pub fn captures_any_of(&self, nodes: &[NodeId]) -> bool {
        any_draw_matches(&self.commands, |draw| matches!(draw, Draw::Capture { node, .. } if nodes.contains(node)))
    }

    /// Every `capture` node in this list, for `wayland::App::sync_captures` (ADR-0248):
    /// which sources to keep requesting, which to stop, and by what pacing and output.
    pub fn capture_nodes(&self, out: &mut Vec<CaptureNode>) {
        fn walk(commands: &[DrawCmd], out: &mut Vec<CaptureNode>) {
            for command in commands {
                match &command.draw {
                    Draw::Capture { node, output, live, paint_cursor, .. } => {
                        out.push(CaptureNode {
                            node: *node,
                            output: output.clone(),
                            live: *live,
                            paint_cursor: *paint_cursor,
                        });
                    }
                    Draw::Clipped { commands, .. } | Draw::Transformed { commands, .. } => walk(commands, out),
                    _ => {}
                }
            }
        }
        walk(&self.commands, out)
    }

    /// Images as `(path, box)` cache keys for `ImageCache::trim` pins (ADR-0123). A mapped surface
    /// still shows what it last painted, so that entry must not be evicted underneath it.
    pub fn drawn_images(&self, out: &mut Vec<(std::path::PathBuf, (u32, u32))>) {
        fn walk(commands: &[DrawCmd], out: &mut Vec<(std::path::PathBuf, (u32, u32))>) {
            for command in commands {
                match &command.draw {
                    Draw::Image { source, box_px, retained, .. } => {
                        // The box the *entry* is under, not the box it is drawn into: a vector's
                        // key is squared, and a pin that names the drawn box misses it (ADR-0183).
                        // Both endpoints are pinned, or `trim` frees the very texture covering the
                        // gap and the node blinks after all (ADR-0180).
                        for path in std::iter::once(source).chain(retained.iter()) {
                            let path = std::path::PathBuf::from(path);
                            let key_box = image::cache_box(&path, *box_px);
                            out.push((path, key_box));
                        }
                    }
                    Draw::Clipped { commands, .. } | Draw::Transformed { commands, .. } => walk(commands, out),
                    _ => {}
                }
            }
        }
        walk(&self.commands, out)
    }

    /// The pixels that can differ from `previous`, empty if none can. Commands outside the common
    /// prefix and suffix are the only ones whose list entry differs, so their bounds, old and new,
    /// cover those. A texture changes under an unchanged entry (a decode or capture landing, a GIF
    /// frame), so every command drawing one adds its own bounds too.
    /// ponytail: the changed commands merge into one rect, so two changes at opposite corners
    /// damage everything between. Upgrade path is a rect per changed command, capped.
    pub fn damage_since(&self, previous: &DisplayList) -> Vec<PhysicalRect> {
        let (old, new) = (&previous.commands, &self.commands);
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let (old, new) = (&old[prefix..], &new[prefix..]);
        let suffix = old.iter().rev().zip(new.iter().rev()).take_while(|(a, b)| a == b).count();
        let changed = old[..old.len() - suffix].iter().chain(&new[..new.len() - suffix]).map(command_bounds);
        let textured = self
            .commands
            .iter()
            .filter(|command| {
                any_draw_matches(std::slice::from_ref(command), |draw| {
                    matches!(draw, Draw::Image { .. } | Draw::Icon { .. } | Draw::Capture { .. })
                })
            })
            .map(command_bounds);
        changed
            .filter(|rect| !is_empty(*rect))
            .reduce(union)
            .into_iter()
            .chain(textured)
            .filter(|r| !is_empty(*r))
            .collect()
    }
}

/// Every pixel `command` can touch. `build_node` cuts each node's clip to its own box and
/// `execute` scissors every draw to it, so a leaf's clip bounds it, padded because femtovg
/// antialiases its scissor edge. A transformed group covers its commands' bounds under its matrix,
/// which femtovg composes with an outer one as the recursion does, and also those bounds
/// untransformed: the shader stage scissors by the raw clip under the innermost matrix alone.
fn command_bounds(command: &DrawCmd) -> PhysicalRect {
    const PAD: i32 = 2;
    let Draw::Transformed { matrix, commands } = &command.draw else {
        let clip = command.clip;
        return PhysicalRect {
            x0: clip.x0.saturating_sub(PAD),
            y0: clip.y0.saturating_sub(PAD),
            x1: clip.x1.saturating_add(PAD),
            y1: clip.y1.saturating_add(PAD),
        };
    };
    let Some(inner) = commands.iter().map(command_bounds).reduce(union) else {
        return PhysicalRect { x0: 0, y0: 0, x1: 0, y1: 0 };
    };
    if !matrix.iter().all(|n| n.is_finite()) {
        return UNCLIPPED;
    }
    let (x, y) = (inner.x0 as f32, inner.y0 as f32);
    let rect = LogicalRect { x, y, width: inner.x1 as f32 - x, height: inner.y1 as f32 - y };
    union(inner, snap_to_physical(super::region::transformed_bounds(*matrix, rect), 1.0))
}

fn union(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    PhysicalRect { x0: a.x0.min(b.x0), y0: a.y0.min(b.y0), x1: a.x1.max(b.x1), y1: a.y1.max(b.y1) }
}

/// Identity clip before any scissor is pushed.
const UNCLIPPED: PhysicalRect = PhysicalRect { x0: i32::MIN, y0: i32::MIN, x1: i32::MAX, y1: i32::MAX };

fn is_empty(clip: PhysicalRect) -> bool {
    clip.x1 <= clip.x0 || clip.y1 <= clip.y0
}

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
    build_node(root, 0.0, 0.0, scale, UNCLIPPED, 1.0, focus, &mut commands);
    DisplayList { commands }
}

/// One node, then its children in tree order. Origins accumulate parent-relative rects to an
/// absolute position.
// ponytail: keep the eight scalar/context arguments; a wrapper would only bag them for one caller.
#[allow(clippy::too_many_arguments)]
fn build_node(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    clip: PhysicalRect,
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
    let clip = clip.intersect(snap_to_physical(rect, scale));
    // Fully clipped children cannot draw.
    if is_empty(clip) {
        return;
    }

    // `node::paint_style` already decided the draw. An unrecognised kind stays transparent, which
    // avoids the passwordless black lock screen ADR-0052 decision 3 rejects. Opacity is baked into
    // the list because ADR-0063 skips unchanged lists; applying it in `execute` would be invisible.
    let opacity = inherited_opacity * node.opacity;
    let draw = draw_for(node, rect, scale, opacity, focus);

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
    match rounded_clip(node) {
        None => {
            if let Some(draw) = draw {
                out.push(DrawCmd { rect, clip, draw });
            }
            for child in &node.children {
                build_node(child, x, y, scale, clip, opacity, focus, out);
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
            for child in &node.children {
                build_node(child, x, y, scale, clip, opacity, focus, &mut inner);
            }
            // A leaf has nothing to clip, so avoid the render target and composite.
            if !inner.is_empty() {
                out.push(DrawCmd { rect, clip, draw: Draw::Clipped { radius, commands: inner } });
            }
            if let Some(border) = border {
                out.push(DrawCmd { rect, clip, draw: border });
            }
        }
    }
    if !node.transform.is_identity() {
        let matrix = node.transform.matrix(rect);
        let commands: Vec<DrawCmd> = out.drain(start..).collect();
        out.push(DrawCmd { rect, clip, draw: Draw::Transformed { matrix, commands } });
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
        PaintStyle::Box { background, radius, colors, widths, clip: _ } => Some(Draw::Box {
            background: background.map(|color| fade(color, opacity)),
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
        PaintStyle::Capture { output, fit, live, paint_cursor } => (!output.is_empty()).then(|| Draw::Capture {
            node: node_id,
            output: output.clone(),
            fit: *fit,
            alpha: opacity,
            live: *live,
            paint_cursor: *paint_cursor,
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

#[cfg(test)]
mod tests {
    use super::*;

    use mlua::Lua;

    use crate::layout::instance::SurfaceInstance;
    use crate::layout::scene::{LogicalSize, Scene};
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};
    use crate::lua::signal;
    use crate::text::shaping::ShapingHandle;

    /// Evaluates `lua_src` as one surface's tree, applies it, and returns the resolved root at
    /// `size`. Panics on any layout error: every fixture below is a config this harness controls,
    /// so a rejection is this test's own bug, not something to assert on.
    pub(super) fn resolved_surface(lua: &Lua, lua_src: &str, size: LogicalSize) -> ResolvedNode {
        register_node_constructors(lua).unwrap();
        signal::register(lua, signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        let instances = [SurfaceInstance {
            instance_id: "bar@TEST".to_string(),
            declared_id: "bar".to_string(),
            output: "TEST".to_string(),
            available: size,
            measured_axes: (false, false),
        }];
        scene.apply(&[surface], &instances, &shaping, lua).unwrap();
        scene.surface("bar@TEST").unwrap().clone()
    }

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

        // The same stale state without the property draws nothing while the source decodes, which
        // is the behaviour every image had before this.
        let plain = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true,
                width = "Fill", height = "Fill" } }"##;
        let mut tree = resolved_surface(&lua, plain, LogicalSize { width: 200.0, height: 40.0 });
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(cover_of(&tree), Some(None), "`retain` is what carries the cover, not the state");
    }

    #[test]
    fn a_list_knows_which_files_it_draws_through_a_rounded_clip_too() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { width = 100, height = 40, radius = 8, clip = "Rounded",
                children = { image { source = "/tmp/a.png", async = true, width = "Fill", height = "Fill" } } } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert!(list.draws_any_of(&[std::path::PathBuf::from("/tmp/a.png")]));
        assert!(!list.draws_any_of(&[std::path::PathBuf::from("/tmp/b.png")]));
        assert!(!list.draws_any_of(&[]));
        // The same walk names the pin `ImageCache::trim` keeps: the path with the box the image
        // was keyed on, the 100x40 rect it fills.
        let mut pinned = Vec::new();
        list.drawn_images(&mut pinned);
        assert_eq!(pinned, vec![(std::path::PathBuf::from("/tmp/a.png"), (100, 40))]);
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
            Draw::Box { background: Some(color), .. } => color.a,
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
                Draw::Box { background: Some(color), .. } => Some(*color),
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

    #[test]
    fn damage_is_the_changed_commands_old_and_new_bounds_and_nothing_when_unchanged() {
        let cmd = |x: f32| {
            let rect = LogicalRect { x, y: 10.0, width: 20.0, height: 20.0 };
            let draw = Draw::Box {
                background: Some(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                radius: 0.0,
                colors: BorderColor::default(),
                widths: EdgeInsets::default(),
            };
            DrawCmd { rect, clip: snap_to_physical(rect, 1.0), draw }
        };
        let before = DisplayList { commands: vec![cmd(0.0), cmd(100.0), cmd(500.0)] };
        let after = DisplayList { commands: vec![cmd(0.0), cmd(140.0), cmd(500.0)] };
        assert_eq!(after.damage_since(&before), [PhysicalRect { x0: 98, y0: 8, x1: 162, y1: 32 }]);
        assert!(before.damage_since(&before).is_empty());
        let transformed = |scale: f32| DisplayList {
            commands: vec![DrawCmd {
                draw: Draw::Transformed { matrix: [scale, 0.0, 0.0, scale, 0.0, 0.0], commands: vec![cmd(100.0)] },
                ..cmd(100.0)
            }],
        };
        assert_eq!(transformed(2.0).damage_since(&transformed(1.0)), [PhysicalRect { x0: 98, y0: 8, x1: 244, y1: 64 }]);
    }
}

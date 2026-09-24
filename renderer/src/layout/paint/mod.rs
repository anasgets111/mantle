//! Draws a resolved layout tree onto a shared femtovg canvas.
//!
//! [`build()`] turns a resolved tree into plain Rust [`DisplayList`] data; [`execute`] sends it to
//! femtovg. `paint_surface` skips drawing and `eglSwapBuffers` when the list is unchanged, while
//! `build` stays testable without EGL. A full-surface commit recomposites the whole screen behind
//! it, so unchanged lists skip that cost. On an idle bar with a clock, a 1920x1200 wallpaper went
//! from repainting twice a second to never and niri CPU fell about a third.
//!
//! The canvas half, [`execute`] and everything it draws with, is in `canvas`.

mod build;
mod canvas;

pub use build::{FieldFocus, build};
pub use canvas::{DrawnImage, Shaders, execute, flush};

use crate::image::{self, Fit, Load};
use crate::layout::node::{self, BorderColor, EdgeInsets, Fill, Rgba, StyleRun, TextAlign};
use crate::layout::scene::NodeId;
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

/// Typed draw data. A kind with no [`PaintStyle`](node::PaintStyle) contributes no [`DrawCmd`]. No Lua values are
/// kept: mlua table identity would make a signal-resolved table unequal every pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Draw {
    /// Box fill, then border, for containers and all surface roles.
    Box { background: Option<Fill>, radius: f32, colors: BorderColor, widths: EdgeInsets },
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
        shader: Option<(std::path::PathBuf, Vec<node::ShaderParam>)>,
        /// `image.source_blur` in physical pixels (ADR-0240). `0` for no blur, which `ImageCache`
        /// never distinguishes from a request it decided not to run.
        blur_px: u32,
    },
    /// An output's live contents (ADR-0248). `wayland::capture` owns the texture, keyed by `node`;
    /// this carries what a draw places it with and what the capture registry paces a source by,
    /// the same split `Draw::Image` makes between pixels and policy.
    Capture {
        node: NodeId,
        output: String,
        fit: Fit,
        alpha: f32,
        live: Option<f32>,
        paint_cursor: bool,
        region: Option<LogicalRect>,
    },
    /// A `shader` node (ADR-0253). `progress` in the list is what makes a tween repaint and damage it,
    /// and `version` is what makes an edited file reach the stage that recompiles it.
    Shader {
        source: std::path::PathBuf,
        version: crate::image::FileVersion,
        progress: f32,
        params: Vec<node::ShaderParam>,
        alpha: f32,
    },
    /// A subtree masked by the declaring node's rounded arc and, if it has one, its `mask` with
    /// the physical box an image mask is cached under (ADR-0255). Rectangular clips flatten into
    /// each command; rounded clips and masks stay grouped for [`execute`].
    Clipped { radius: f32, mask: Option<(node::Mask, (u32, u32))>, commands: Vec<DrawCmd> },
    /// The subtree of a node with a `scale`/`rotate`/`translate` (ADR-0149), drawn under its
    /// affine. Coordinates inside are the untransformed absolute ones.
    Transformed { matrix: node::Affine, commands: Vec<DrawCmd> },
    /// A box's shadow as one gradient quad under its fill (ADR-0254), cut out under the box when
    /// `knockout` (ADR-0260). `shadow.color` carries the inherited opacity.
    Shadow { shadow: node::Shadow, radius: f32, knockout: bool },
    /// A subtree drawn offscreen, then composited over its own shadow and through `content_blur`
    /// (ADR-0254). `rect` is the node's box; `clip` covers everything the effect reaches. A
    /// `silhouette` is a scoop's fill, and only its shadow draws, cut out under the box (ADR-0260).
    Layer { effect: node::Effect, silhouette: bool, commands: Vec<DrawCmd> },
    /// What the target already holds under the node's box, blurred by `sigma` logical pixels and
    /// drawn through its `radius` at `alpha` (ADR-0256). `clip` covers the 3 sigma the blur reads.
    Backdrop { sigma: f32, radius: f32, alpha: f32 },
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
    pub live: Option<f32>,
    pub paint_cursor: bool,
    pub region: Option<LogicalRect>,
}

/// One surface's draw order, flattened for equality so a re-resolve repaints only the surfaces
/// whose list changed (ADR-0044 decision 2). Float equality is safe because
/// identical inputs produce identical bits; `NaN` repaints forever rather than leaving stale
/// pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayList {
    pub commands: Vec<DrawCmd>,
}

/// Walks `commands` for a draw `matches`, a group before its `Clipped`/`Transformed`/`Layer` subtree.
/// Shared by [`DisplayList::draws_any_of`] and [`DisplayList::captures_any_of`], which differ only
/// in which `Draw` variant and field they compare.
fn any_draw_matches(commands: &[DrawCmd], matches: impl Fn(&Draw) -> bool + Copy) -> bool {
    commands.iter().any(|command| {
        matches(&command.draw)
            || match &command.draw {
                Draw::Clipped { commands, .. } | Draw::Transformed { commands, .. } | Draw::Layer { commands, .. } => {
                    any_draw_matches(commands, matches)
                }
                _ => false,
            }
    })
}

/// Whether `draw`'s pixels can change under an unchanged command: a decode or capture landing, a
/// GIF frame (ADR-0258).
fn volatile(draw: &Draw) -> bool {
    matches!(draw, Draw::Image { .. } | Draw::Icon { .. } | Draw::Capture { .. }) || mask_file(draw).is_some()
}

/// The file an image `mask` reads, which changes under an unchanged list as an `image`'s does.
fn mask_file(draw: &Draw) -> Option<&str> {
    match draw {
        Draw::Clipped { mask: Some((node::Mask { source: node::MaskSource::Image(file), .. }, _)), .. } => Some(file),
        _ => None,
    }
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
            })) || mask_file(draw).is_some_and(|mask| files.iter().any(|file| file.as_os_str() == mask))
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
                    Draw::Capture { node, output, live, paint_cursor, region, .. } => {
                        out.push(CaptureNode {
                            node: *node,
                            output: output.clone(),
                            live: *live,
                            paint_cursor: *paint_cursor,
                            region: *region,
                        });
                    }
                    Draw::Clipped { commands, .. }
                    | Draw::Transformed { commands, .. }
                    | Draw::Layer { commands, .. } => walk(commands, out),
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
            // The box the *entry* is under, not the box it is drawn into: a vector's key is
            // squared, and a pin that names the drawn box misses it (ADR-0183).
            let pin = |out: &mut Vec<_>, path: &str, box_px| {
                let path = std::path::PathBuf::from(path);
                let key_box = image::cache_box(&path, box_px);
                out.push((path, key_box));
            };
            for command in commands {
                match &command.draw {
                    Draw::Image { source, box_px, retained, .. } => {
                        // Both endpoints are pinned, or `trim` frees the very texture covering the
                        // gap and the node blinks after all (ADR-0180).
                        for path in std::iter::once(source).chain(retained.iter()) {
                            pin(out, path, *box_px);
                        }
                    }
                    Draw::Clipped { mask, commands, .. } => {
                        if let (Some(file), Some((_, box_px))) = (mask_file(&command.draw), mask) {
                            pin(out, file, *box_px);
                        }
                        walk(commands, out)
                    }
                    Draw::Transformed { commands, .. } | Draw::Layer { commands, .. } => walk(commands, out),
                    _ => {}
                }
            }
        }
        walk(&self.commands, out)
    }

    /// The pixels that can differ from `previous`, empty if none can. Commands outside the common
    /// prefix and suffix are the only ones whose list entry differs, so their bounds, old and new,
    /// cover those. A texture changes under an unchanged entry (a decode or capture landing, a GIF
    /// frame) only on a paint its surface owes for that (ADR-0182, ADR-0233), and then `textures`
    /// adds the bounds of every command drawing one. A rounded clip whose own fields match
    /// composites pixel for pixel, so both look inside it (ADR-0258). One rect per changed
    /// command, and one for a run whose length changed.
    pub fn damage_since(&self, previous: &DisplayList, textures: bool) -> Vec<PhysicalRect> {
        fn merged(rects: impl Iterator<Item = PhysicalRect>) -> Option<PhysicalRect> {
            rects.filter(|rect| !is_empty(*rect)).reduce(union)
        }
        fn changed(old: &[DrawCmd], new: &[DrawCmd], out: &mut Vec<PhysicalRect>) {
            let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
            let (old, new) = (&old[prefix..], &new[prefix..]);
            let suffix = old.iter().rev().zip(new.iter().rev()).take_while(|(a, b)| a == b).count();
            let (old, new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
            if old.len() != new.len() {
                out.extend(merged(old.iter().chain(new).map(command_bounds)));
                return;
            }
            for (a, b) in old.iter().zip(new).filter(|(a, b)| a != b) {
                match (&a.draw, &b.draw) {
                    (
                        Draw::Clipped { radius, mask, commands },
                        Draw::Clipped { radius: r, mask: m, commands: other },
                    ) if (a.rect, a.clip, radius, mask) == (b.rect, b.clip, r, m) => changed(commands, other, out),
                    _ => out.extend(merged([a, b].into_iter().map(command_bounds))),
                }
            }
        }
        fn textured(commands: &[DrawCmd], out: &mut Vec<PhysicalRect>) {
            for command in commands {
                match &command.draw {
                    Draw::Clipped { commands, .. } if mask_file(&command.draw).is_none() => textured(commands, out),
                    _ if any_draw_matches(std::slice::from_ref(command), volatile) => out.push(command_bounds(command)),
                    _ => {}
                }
            }
        }
        let mut damage = Vec::new();
        changed(&previous.commands, &self.commands, &mut damage);
        if textures {
            textured(&self.commands, &mut damage);
        }
        damage.retain(|r| !is_empty(*r));
        self.expand_backdrops(&mut damage);
        damage
    }

    /// `damage` grown over every transformed group it touches (ADR-0258): their scissors follow
    /// their matrix, so they draw whole. Rounded clips and masks draw in part, so the walk goes
    /// into them.
    pub fn repaint_region(&self, damage: PhysicalRect) -> PhysicalRect {
        fn grow(commands: &[DrawCmd], region: PhysicalRect) -> PhysicalRect {
            commands.iter().fold(region, |region, command| match &command.draw {
                Draw::Clipped { commands, .. } => grow(commands, region),
                Draw::Transformed { .. } => {
                    let bounds = command_bounds(command);
                    if is_empty(bounds.intersect(region)) { region } else { union(region, bounds) }
                }
                _ => region,
            })
        }
        // Grown until stable: a transform may reach a backdrop and a read area a transform.
        let mut grown = vec![grow(&self.commands, damage)];
        self.expand_backdrops(&mut grown);
        let grown = grown.into_iter().reduce(union).unwrap_or(damage);
        if grown == damage { damage } else { self.repaint_region(grown) }
    }

    /// Adds every backdrop's read area that `damage` reaches, until none is left, so a repainted
    /// backdrop reads only pixels drawn this frame (ADR-0256).
    pub fn expand_backdrops(&self, damage: &mut Vec<PhysicalRect>) {
        fn reads(commands: &[DrawCmd], matrices: &mut Vec<node::Affine>, out: &mut Vec<PhysicalRect>) {
            for command in commands {
                match &command.draw {
                    Draw::Backdrop { .. } => {
                        out.push(matrices.iter().rev().fold(command_bounds(command), |read, m| transformed(*m, read)))
                    }
                    Draw::Transformed { matrix, commands } => {
                        matrices.push(*matrix);
                        reads(commands, matrices, out);
                        matrices.pop();
                    }
                    Draw::Clipped { commands, .. } | Draw::Layer { commands, .. } => reads(commands, matrices, out),
                    _ => {}
                }
            }
        }
        let mut pending = Vec::new();
        reads(&self.commands, &mut Vec::new(), &mut pending);
        while let Some(at) = pending.iter().position(|read| {
            !damage.iter().any(|rect| rect.intersect(*read) == *read)
                && damage.iter().any(|rect| !is_empty(rect.intersect(*read)))
        }) {
            damage.push(pending.swap_remove(at));
        }
    }
}

/// The most rects one paint repaints apart (ADR-0258); past it the cheapest pairs merge.
const MAX_REGIONS: usize = 4;

/// `rects` merged wherever a pair's union covers under 1.5x their summed area, which takes every
/// overlap, then pairwise, cheapest first, down to [`MAX_REGIONS`] (ADR-0258).
pub fn coalesce(mut rects: Vec<PhysicalRect>) -> Vec<PhysicalRect> {
    rects.retain(|rect| !is_empty(*rect));
    // A relayout damages every command; the pairing below is quadratic per merge.
    if rects.len() > 32 {
        return rects.into_iter().reduce(union).into_iter().collect();
    }
    let area = |r: PhysicalRect| i64::from(r.x1 - r.x0) * i64::from(r.y1 - r.y0);
    loop {
        let pairs = (0..rects.len()).flat_map(|i| (i + 1..rects.len()).map(move |j| (i, j)));
        let cost = |&(i, j): &(usize, usize)| {
            area(union(rects[i], rects[j])) as f64 / (area(rects[i]) + area(rects[j])) as f64
        };
        match pairs.map(|pair| (cost(&pair), pair)).min_by(|a, b| a.0.total_cmp(&b.0)) {
            Some((cost, (i, j))) if cost < 1.5 || rects.len() > MAX_REGIONS => {
                rects[i] = union(rects[i], rects[j]);
                rects.swap_remove(j);
            }
            _ => return rects,
        }
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
    union(inner, transformed(*matrix, inner))
}

/// `rect`'s bounds under `matrix`.
pub(crate) fn transformed(matrix: node::Affine, rect: PhysicalRect) -> PhysicalRect {
    if !matrix.iter().all(|n| n.is_finite()) {
        return UNCLIPPED;
    }
    let (x, y) = (rect.x0 as f32, rect.y0 as f32);
    let rect = LogicalRect { x, y, width: rect.x1 as f32 - x, height: rect.y1 as f32 - y };
    snap_to_physical(super::region::transformed_bounds(matrix, rect), 1.0)
}

pub(crate) fn union(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    PhysicalRect { x0: a.x0.min(b.x0), y0: a.y0.min(b.y0), x1: a.x1.max(b.x1), y1: a.y1.max(b.y1) }
}

fn grow(rect: LogicalRect, by: f32) -> LogicalRect {
    LogicalRect { x: rect.x - by, y: rect.y - by, width: rect.width + 2.0 * by, height: rect.height + 2.0 * by }
}

/// Where `area`, painted around a node's box `rect`, lands as that node's shadow: offset, and
/// scaled about the box's centre until the box has grown by `spread` a side. For a box that is
/// CSS's spread; for other content it is Qt's `shadowScale` (ADR-0254).
fn shadow_rect(rect: LogicalRect, area: LogicalRect, shadow: node::Shadow) -> LogicalRect {
    let axis = |start: f32, size: f32, from: f32, span: f32, offset: f32| {
        let k = if size > 0.0 { ((size + 2.0 * shadow.spread) / size).max(0.0) } else { 1.0 };
        let centre = start + size / 2.0;
        (centre + (from - centre) * k + offset, span * k)
    };
    let (x, width) = axis(rect.x, rect.width, area.x, area.width, shadow.offset.0);
    let (y, height) = axis(rect.y, rect.height, area.y, area.height, shadow.offset.1);
    LogicalRect { x, y, width, height }
}

/// Identity clip before any scissor is pushed.
const UNCLIPPED: PhysicalRect = PhysicalRect { x0: i32::MIN, y0: i32::MIN, x1: i32::MAX, y1: i32::MAX };

fn is_empty(clip: PhysicalRect) -> bool {
    clip.x1 <= clip.x0 || clip.y1 <= clip.y0
}

#[cfg(test)]
mod tests {
    use super::*;

    use mlua::Lua;

    use crate::layout::instance::SurfaceInstance;
    use crate::layout::scene::{LogicalSize, ResolvedNode, Scene};
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

    /// `child` in a 96x48 panel, built at scale 1.
    pub(super) fn masked(child: &str) -> DisplayList {
        let src = format!(r#"return panel {{ id = "bar", width = 96, height = 48, child = {child} }}"#);
        build(&resolved_surface(&Lua::new(), &src, LogicalSize { width: 96.0, height: 48.0 }), 1.0, None)
    }

    pub(super) const IMAGE_MASKED: &str = r##"rect { width = 80, height = 32, background = "#0000FFFF",
        border_width = 1, border_color = "#FFFFFFFF", mask = { source = "/tmp/m.png" },
        children = { rect { width = 30, height = "Fill", background = "#FF0000FF" } } }"##;

    /// A mask texture changes under an unchanged list like an `image`'s: a decode landing, a GIF
    /// frame, the file rewritten. So it is pinned, marks its surface stale, and damages its group.
    #[test]
    fn an_image_mask_is_a_texture_for_pinning_staleness_and_damage() {
        let list = masked(IMAGE_MASKED);
        let mut pinned = Vec::new();
        list.drawn_images(&mut pinned);
        assert_eq!(pinned, [(std::path::PathBuf::from("/tmp/m.png"), (80, 32))]);
        assert!(list.draws_any_of(&[std::path::PathBuf::from("/tmp/m.png")]));
        assert!(!list.draws_any_of(&[std::path::PathBuf::from("/tmp/other.png")]));
        assert_eq!(list.damage_since(&list, true), [PhysicalRect { x0: -2, y0: -2, x1: 82, y1: 34 }]);
        assert!(list.damage_since(&list, false).is_empty(), "no texture moved: the surface owes no paint");
    }

    #[test]
    fn damage_is_the_changed_commands_old_and_new_bounds_and_nothing_when_unchanged() {
        let cmd = |x: f32| {
            let rect = LogicalRect { x, y: 10.0, width: 20.0, height: 20.0 };
            let draw = Draw::Box {
                background: Some(Fill::Color(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 })),
                radius: 0.0,
                colors: BorderColor::default(),
                widths: EdgeInsets::default(),
            };
            DrawCmd { rect, clip: snap_to_physical(rect, 1.0), draw }
        };
        let before = DisplayList { commands: vec![cmd(0.0), cmd(100.0), cmd(500.0)] };
        let after = DisplayList { commands: vec![cmd(0.0), cmd(140.0), cmd(500.0)] };
        assert_eq!(after.damage_since(&before, true), [PhysicalRect { x0: 98, y0: 8, x1: 162, y1: 32 }]);
        assert!(before.damage_since(&before, true).is_empty());
        let transformed = |scale: f32| DisplayList {
            commands: vec![DrawCmd {
                draw: Draw::Transformed { matrix: [scale, 0.0, 0.0, scale, 0.0, 0.0], commands: vec![cmd(100.0)] },
                ..cmd(100.0)
            }],
        };
        assert_eq!(
            transformed(2.0).damage_since(&transformed(1.0), true),
            [PhysicalRect { x0: 98, y0: 8, x1: 244, y1: 64 }]
        );
    }

    pub(super) fn effect_surface(child: &str) -> DisplayList {
        let src =
            format!(r##"return panel {{ id = "bar", width = 200, height = 100, padding = 40, child = {child} }}"##);
        build(&resolved_surface(&Lua::new(), &src, LogicalSize { width: 200.0, height: 100.0 }), 1.0, None)
    }

    /// ADR-0256. A frosted node shows what is under it, so a change its blur reaches repaints it,
    /// and one out of its reach does not.
    #[test]
    fn a_change_within_a_backdrops_reach_damages_the_frosted_node() {
        let surface = |spacing: i32, colour: &str| {
            effect_surface(&format!(
                r##"row {{ spacing = {spacing}, children = {{ rect {{ width = 10, height = 20, background = "{colour}" }},
                    rect {{ width = 40, height = 20, backdrop_blur = 4 }} }} }}"##
            ))
        };
        for (spacing, reached) in [(4, true), (30, false)] {
            let damage = surface(spacing, "#ff0000").damage_since(&surface(spacing, "#00ff00"), true);
            let frosted = 50 + spacing + 40;
            assert_eq!(damage.iter().any(|r| r.x1 >= frosted), reached, "spacing {spacing}: {damage:?}");
        }
    }

    fn glass(x0: i32, x1: i32) -> DrawCmd {
        let clip = PhysicalRect { x0, y0: 0, x1, y1: 10 };
        let rect = LogicalRect { x: x0 as f32, y: 0.0, width: (x1 - x0) as f32, height: 10.0 };
        DrawCmd { rect, clip, draw: Draw::Backdrop { sigma: 1.0, radius: 0.0, alpha: 1.0 } }
    }

    /// ADR-0256. A later glass repainting reaches an earlier one whose read it covers, and a read
    /// already inside the damage adds nothing.
    #[test]
    fn backdrop_damage_expands_to_a_fixpoint_and_skips_covered_reads() {
        let list = DisplayList { commands: vec![glass(0, 40), glass(30, 100), glass(92, 94)] };
        let mut damage = vec![PhysicalRect { x0: 90, y0: 0, x1: 95, y1: 10 }];
        list.expand_backdrops(&mut damage);
        let pad = |x0, x1| PhysicalRect { x0: x0 - 2, y0: -2, x1: x1 + 2, y1: 12 };
        assert_eq!(damage[1..], [pad(30, 100), pad(0, 40)]);
    }

    /// ADR-0256. A glass nested in groups damages its own read area mapped through the enclosing
    /// matrices, not the whole group.
    #[test]
    fn a_nested_glass_damages_its_own_read_area_through_its_matrices() {
        let group = |draw| DrawCmd { draw, ..glass(0, 200) };
        let scaled = Draw::Transformed { matrix: [2.0, 0.0, 0.0, 2.0, 0.0, 0.0], commands: vec![glass(10, 20)] };
        let clipped = Draw::Clipped { radius: 4.0, mask: None, commands: vec![group(scaled), glass(150, 160)] };
        let list = DisplayList { commands: vec![group(clipped)] };
        let mut damage = vec![PhysicalRect { x0: 30, y0: 0, x1: 31, y1: 1 }];
        list.expand_backdrops(&mut damage);
        assert_eq!(damage[1..], [PhysicalRect { x0: 16, y0: -4, x1: 44, y1: 24 }]);
    }

    /// ADR-0258. Rects merge where their union costs under 1.5x their summed area, overlapping
    /// ones always, and the cheapest pairs merge until at most four are left.
    #[test]
    fn repaint_rects_merge_when_cheap_and_down_to_four() {
        let rect = |x0, y0, side| PhysicalRect { x0, y0, x1: x0 + side, y1: y0 + side };
        let (shader, clock) = (rect(10, 10, 100), rect(1300, 10, 20));
        assert_eq!(coalesce(vec![shader, clock]), [shader, clock], "far apart stay apart");
        assert_eq!(coalesce(vec![shader, rect(50, 50, 100)]), [rect(10, 10, 140)], "overlapping merge");
        assert_eq!(coalesce(vec![rect(0, 0, 10), rect(10, 0, 10)]), [PhysicalRect { x0: 0, y0: 0, x1: 20, y1: 10 }]);
        let five: Vec<_> = (0..5).map(|i| rect(i * 300, 0, 10)).collect();
        let merged = coalesce(five);
        assert_eq!(merged.len(), 4, "{merged:?}");
    }

    /// ADR-0258. Each changed command damages its own bounds, so a shader at one end and a clock at
    /// the other are two rects, not the panel between them.
    #[test]
    fn two_far_changes_damage_two_rects() {
        let list = |colour: &str| {
            effect_surface(&format!(
                r##"row {{ spacing = 60, children = {{ rect {{ width = 10, height = 10, background = "{colour}" }},
                    rect {{ width = 10, height = 10, background = "#ffffff" }},
                    rect {{ width = 10, height = 10, background = "{colour}" }} }} }}"##
            ))
        };
        let damage = list("#ff0000").damage_since(&list("#00ff00"), true);
        assert_eq!(damage.len(), 2, "{damage:?}");
    }

    /// ADR-0258. A rounded clip composites pixel for pixel, so a change or a texture inside one
    /// damages that child alone rather than the whole group.
    #[test]
    fn a_change_inside_a_rounded_clip_damages_that_child_alone() {
        let list = |color: &str| {
            effect_surface(&format!(
                r##"rect {{ width = 120, height = 40, radius = 8, clip = "Rounded", children = {{ row {{ children = {{
                    rect {{ width = 20, height = 20, background = "{color}" }},
                    rect {{ width = 20, height = 20, background = "#ffffff" }},
                    image {{ source = "/tmp/i.png", width = 20, height = 20 }} }} }} }} }}"##
            ))
        };
        let damage = list("#ff0000").damage_since(&list("#00ff00"), true);
        assert_eq!(
            damage,
            [PhysicalRect { x0: 38, y0: 38, x1: 62, y1: 62 }, PhysicalRect { x0: 78, y0: 38, x1: 102, y1: 62 }]
        );
    }

    /// ADR-0258. A repaint touching a transformed group grows to take it whole, inside a rounded
    /// clip too, and on through whatever that growth then touches. A layer draws its offscreen
    /// whole anyway, so it only clips the repaint, as a plain box does.
    #[test]
    fn a_repaint_takes_every_transformed_group_it_touches_whole() {
        let list = effect_surface(
            r##"row { spacing = 4, children = {
                rect { width = 20, height = 20, background = "#ffffff", content_blur = 1 },
                rect { width = 30, height = 20, radius = 4, clip = "Rounded", children = {
                    rect { width = 20, height = 20, background = "#ffffff", scale = 2 } } },
                rect { width = 20, height = 20, background = "#ffffff", scale = 2 } } }"##,
        );
        let clipped = list.commands.iter().find(|c| matches!(c.draw, Draw::Clipped { .. })).unwrap();
        let Draw::Clipped { commands, .. } = &clipped.draw else { unreachable!() };
        let inner = command_bounds(&commands[0]);
        let outer = command_bounds(list.commands.last().unwrap());
        assert!(!is_empty(inner.intersect(outer)), "the two scaled boxes overlap: {inner:?} {outer:?}");
        let blurred = PhysicalRect { x0: 44, y0: 44, x1: 48, y1: 48 };
        assert_eq!(list.repaint_region(blurred), blurred);
        // Past the clipped one in list order, so only a second pass reaches it.
        let touching = PhysicalRect { x0: 125, y0: 44, x1: 127, y1: 48 };
        assert_eq!(list.repaint_region(touching), union(inner, outer));
    }

    /// A shadow moving repaints where it was and where it lands, not only the node's box.
    #[test]
    fn a_moved_shadow_damages_both_its_old_and_new_extent() {
        let at = |y: i32| {
            effect_surface(&format!(
                r##"rect {{ width = 40, height = 20, background = "#ffffff", shadow_offset = {{ y = {y} }} }}"##
            ))
        };
        let damage = at(30).damage_since(&at(10), true);
        assert!(
            damage.iter().any(|r| r.y0 <= 50 && r.y1 >= 90 && r.x0 <= 40 && r.x1 >= 80),
            "the old shadow at 50..70 and the new one at 70..90: {damage:?}"
        );
    }
}

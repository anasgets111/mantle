//! Where a resolved surface takes input and where it asks for blur (ADR-0109, ADR-0195).

use crate::layout::node::{self, PaintStyle};
use crate::layout::scene::ResolvedNode;
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

/// The starting clip: a root intersects itself on the first step, so this only has to not be the limit.
const EVERYTHING: LogicalRect = LogicalRect { x: -1e9, y: -1e9, width: 2e9, height: 2e9 };

/// The input-region scan: what this surface draws and what it can click, as surface-local
/// physical rects (ADR-0038 decision 5, ADR-0109). Pure; the `wl_region`/
/// `wl_surface::set_input_region` push it feeds lives in `crate::wayland::App::apply_input_region`,
/// the only place a Wayland object exists to push to.
///
/// A visible node claims its whole box when it is *solid*: it paints something (a box with a
/// background or a border, or any text, icon, image or field), or it has a pointer handler
/// (`on_click`, `on_press`, `on_drag`, `on_wheel`) or `submit`, which may be invisible by design but is still
/// pressable (a full-surface click-outside catcher). Transparent containers claim nothing and are walked
/// into, so a full-surface `column` holding two cards yields the cards. Everything else is
/// click-through and, under focus-follows-mouse, focus-through; the popup's empty space below its
/// cards therefore takes neither clicks nor keyboard focus.
///
/// A claiming box that `clips_children` ends the walk. One that does not also claims the
/// descendants sticking out of it; the walk carries the clip so each is cut to its clipping ancestors.
///
/// Painting claims input on every layer but `Background`, where a handler is the only thing that
/// does (ADR-0204). Paint stands in for interactivity because a drawn overlay must not leak a click
/// to the window behind it; nothing is behind the bottom layer, so there the proxy claims a whole
/// output for a wallpaper that cannot use it and takes the desktop's focus-through with it.
///
/// Applies to any surface whose visible content is smaller than the surface itself: empty space
/// (clicks pass through) with nothing visible, a no-op for a tightly-sized bar whose child fills
/// it.
pub fn overlay_input_regions(surface_root: &ResolvedNode, scale: f32) -> Vec<PhysicalRect> {
    // A role with no `layer` at all -- window, popup, lock -- keeps the paint proxy.
    let paint_claims =
        !matches!(node::fields::panel::layer.read(&surface_root.properties), Ok(node::LayerKind::Background));
    let mut regions = Vec::new();
    let scan = (scale, paint_claims);
    // A root's paint claims nothing, but its handler asks for the whole surface.
    if surface_root.takes_pointer() {
        collect_input_regions(surface_root, 0.0, 0.0, scan, node::IDENTITY_AFFINE, EVERYTHING, true, &mut regions);
        return regions;
    }
    let root_matrix = surface_root.paint_matrix(surface_root.at(0.0, 0.0)).unwrap_or(node::IDENTITY_AFFINE);
    let hittable = surface_root.hittable(true);
    for child in surface_root.content_children() {
        collect_input_regions(child, 0.0, 0.0, scan, root_matrix, EVERYTHING, hittable, &mut regions);
    }
    regions
}

/// Every `behind_blur = true` node in one surface, as the physical rects the compositor is handed
/// (ADR-0195). Pure; the `ext_background_effect_surface_v1::set_blur_region` push it feeds lives
/// in `crate::wayland::App::apply_blur_region`.
///
/// Opt-in, never inferred. The sibling walk above answers "what can be clicked", which has one
/// correct answer and so needs no config input; "what should be blurred" is an aesthetic with no
/// correct answer, and inferring it from `background` alpha would have been a guess: a control may
/// be deliberately invisible at `#00000000`, and border-only or image-backed glass carries no
/// background alpha to read.
///
/// Three differences from [`overlay_input_regions`], because blur follows painted content:
///
/// 1. **Leaving nodes count.** They still paint while their exit runs, but take no input.
/// 2. **Ancestor clips intersect.** `layout::paint::build_node` cuts a child to a parent that
///    `clips_children`, so a card scrolled out of a `max_height` list is not drawn and must not
///    blur either.
/// 3. **The surface root is included**, because a root may paint its own box.
///
/// A claiming node does not stop the walk: a marked child inside a marked parent unions into it,
/// and a rounded parent that does not clip can have children painting outside its corners.
pub fn blur_regions(surface_root: &ResolvedNode, scale: f32) -> Vec<PhysicalRect> {
    let mut regions = Vec::new();
    collect_blur_regions(surface_root, 0.0, 0.0, scale, node::IDENTITY_AFFINE, EVERYTHING, 1.0, &mut regions);
    regions
}

#[allow(clippy::too_many_arguments)]
fn collect_blur_regions(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    matrix: node::Affine,
    clip: LogicalRect,
    opacity: f32,
    out: &mut Vec<PhysicalRect>,
) {
    // `visible`, not `in_flow`: a leaving node is still painted (ADR-0150) and its glass is still
    // on the glass. A fully faded subtree paints nothing, so it asks for nothing.
    if !node.visible || opacity * node.opacity <= 0.0 {
        return;
    }
    let rect = node.at(origin_x, origin_y);
    let matrix = node.paint_matrix(rect).map_or(matrix, |own| node::compose_affine(matrix, own));
    // In the node's pre-matrix space, as `layout::paint::build_node` carries it: the group's matrix
    // moves the node and what it paints, but not the ancestors' clip it is cut by.
    let parent_clip = match node.paint_matrix(rect).and_then(node::invert_affine) {
        // An empty clip stays empty: mapping would flip its inverted corners into a real rect.
        Some(inverse) if !clip.is_empty() => node::transformed_bounds(inverse, clip),
        _ => clip,
    };
    let clip = parent_clip.intersect(rect);
    let child_clip = if node.clips_children() { clip } else { parent_clip };
    if child_clip.is_empty() {
        return;
    }
    if node.behind_blur {
        let radius = match &node.paint {
            Some(PaintStyle::Box { radius, .. }) => *radius,
            _ => node::Radii::default(),
        };
        // The radius travels with the box, so scale it the way the box was scaled. An axis-aligned
        // matrix scales x and y alike here; a rotation would not, and a rounded rotated box is
        // approximated by the larger of the two.
        let grow = ((matrix[0] * matrix[0] + matrix[1] * matrix[1]).sqrt())
            .max((matrix[2] * matrix[2] + matrix[3] * matrix[3]).sqrt());
        // Round the node's own box and *then* cut it to the clip, never the other way round: a
        // card scrolled halfway out of a list is cut by a straight edge, and rounding the cut
        // rectangle would round that edge too, pulling blur off the straight sides still on screen.
        let mut rounded = Vec::new();
        push_rounded_rect(
            snap_to_physical(node::transformed_bounds(matrix, rect), scale),
            radius * scale * grow,
            &mut rounded,
        );
        let visible = snap_to_physical(node::transformed_bounds(matrix, clip), scale);
        for strip in rounded {
            let cut = strip.intersect(visible);
            if !cut.is_empty() {
                out.push(cut);
            }
        }
    }
    for child in node.content_children() {
        collect_blur_regions(child, rect.x, rect.y, scale, matrix, child_clip, opacity * node.opacity, out);
    }
}

/// A rounded rectangle as the axis-aligned rectangles a `wl_region` is made of, since the protocol
/// carries no radius.
///
/// The middle is one rectangle and only the two corner bands are cut into strips, so an ordinary
/// card costs about `radius` rectangles rather than its height in them: at `radius.md` that is a
/// couple of dozen, against the ~600 a scanline-per-row rasterisation would have sent every frame.
/// Rows sharing an inset merge into one strip, which is most of them near the middle of a band.
///
/// A negative radius is a scoop: the circle centres on the corner point, which is a rounded band
/// mirrored top to bottom and side to side. A smoothed corner reads its inset off the same chain
/// the painter draws.
fn push_rounded_rect(rect: PhysicalRect, radii: node::Radii, out: &mut Vec<PhysicalRect>) {
    if rect.is_empty() {
        return;
    }
    let height = rect.y1 - rect.y0;
    let width = rect.x1 - rect.x0;
    // Corners shrink together, the same rule the painter's arcs follow.
    let radii = radii.fit(width as f32, height as f32);
    let squircles = radii.squircles(width as f32, height as f32);
    // How far `row` rows from corner `i`'s own edge is inset from the side: none past its band.
    let inset = |i: usize, row: i32| {
        if let Some(squircle) = squircles[i] {
            return if (row as f32) < squircle.reach.round() {
                squircle.inset(row as f32 + 0.5).round() as i32
            } else {
                0
            };
        }
        let radius = radii.0[i];
        let r = radius.abs().round() as i32;
        match row < r {
            false => 0,
            true if radius < 0.0 => r - inset_at(r, r - 1 - row),
            true => inset_at(r, row),
        }
    };
    // Rows sharing both insets merge into one strip, so the straight middle is a single rectangle.
    let insets = |row: i32| {
        let up = row;
        let down = height - 1 - row;
        (inset(0, up).max(inset(3, down)), inset(1, up).max(inset(2, down)))
    };
    let mut row = 0;
    while row < height {
        let (left, right) = insets(row);
        let mut last = row + 1;
        while last < height && insets(last) == (left, right) {
            last += 1;
        }
        if rect.x0 + left < rect.x1 - right {
            out.push(PhysicalRect { x0: rect.x0 + left, y0: rect.y0 + row, x1: rect.x1 - right, y1: rect.y0 + last });
        }
        row = last;
    }
}

/// How far row `row` of a corner band is inset from the side, for a corner of radius `r`.
///
/// Sampled at the row's centre rather than its outer edge. The edge is where the arc is furthest
/// in, so sampling there insets the outermost row by the whole radius and a box only as tall as
/// its rounding loses every rectangle it had -- a 2x2 at radius 1 produced one empty rect and
/// nothing else.
fn inset_at(r: i32, row: i32) -> i32 {
    let dy = (r - row) as f32 - 0.5;
    let r = r as f32;
    (r - (r * r - dy * dy).max(0.0).sqrt()).round() as i32
}

/// `scan` is the per-surface `(scale, paint_claims)`, unchanged down the recursion. `clip` is the
/// ancestors' cut in this node's pre-matrix space, as in [`collect_blur_regions`]: hit testing
/// refuses what a clipping ancestor hides, so a region must too.
#[allow(clippy::too_many_arguments)]
fn collect_input_regions(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    scan: (f32, bool),
    matrix: node::Affine,
    clip: LogicalRect,
    inherited: bool,
    out: &mut Vec<PhysicalRect>,
) {
    if !node.in_flow() {
        return;
    }
    let (scale, paint_claims) = scan;
    let rect = node.at(origin_x, origin_y);
    let own_matrix = node.paint_matrix(rect);
    let matrix = own_matrix.map_or(matrix, |own| node::compose_affine(matrix, own));
    let parent_clip = match own_matrix.and_then(node::invert_affine) {
        Some(inverse) if !clip.is_empty() => node::transformed_bounds(inverse, clip),
        _ => clip,
    };
    let own_clip = parent_clip.intersect(rect);
    let child_clip = if node.clips_children() { own_clip } else { parent_clip };
    if child_clip.is_empty() {
        return;
    }
    let hittable = node.hittable(inherited);
    let mut claimed = None;
    // An empty cut stays empty: mapping it would flip its inverted corners into a real rect.
    if hittable && !own_clip.is_empty() && takes_input_as_a_box(node, paint_claims) {
        let bounds = snap_to_physical(node::transformed_bounds(matrix, own_clip), scale);
        if !bounds.is_empty() {
            claimed = Some(bounds);
            out.push(bounds);
        }
        if node.clips_children() {
            return;
        }
    }
    let start = out.len();
    for child in node.content_children() {
        collect_input_regions(child, rect.x, rect.y, scan, matrix, child_clip, hittable, out);
    }
    // Only an unclipped box's overflow adds to it; one rect per descendant inside it would bloat the region.
    if let Some(own) = claimed {
        let mut at = 0;
        out.retain(|region| {
            at += 1;
            at <= start || !own.contains(*region)
        });
    }
}

/// [`overlay_input_regions`]'s "solid" test. A `background` of `#00000000` counts: the IDL says it
/// draws a transparent rectangle where an absent one draws nothing, and a config that wrote it
/// asked for a box.
fn takes_input_as_a_box(node: &ResolvedNode, paint_claims: bool) -> bool {
    let paints = paint_claims
        && match &node.paint {
            Some(PaintStyle::Box { background, widths, .. }) => {
                background.is_some() || [widths.top, widths.right, widths.bottom, widths.left].iter().any(|w| *w > 0.0)
            }
            Some(PaintStyle::Path(path)) => {
                (path.fill.is_some() || path.stroke.is_some() && path.stroke_width > 0.0)
                    && path.commands.segments.iter().any(|segment| {
                        matches!(segment.op, node::PathOp::L | node::PathOp::Q | node::PathOp::C | node::PathOp::A)
                    })
            }
            // Its alpha is the GPU's to know; a config gives it a pointer handler for a hit area (ADR-0253).
            Some(PaintStyle::Shader { .. }) | None => false,
            Some(_) => true,
        };
    paints || node.takes_pointer()
}

#[cfg(test)]
mod tests {
    use mlua::Value;

    use super::*;
    use crate::layout::node::MoveTween;
    use crate::layout::node::PropMap;
    use crate::layout::scene::NodeId;

    /// A `rect` with a background, the way the region scan sees one.
    fn solid_paint() -> Option<PaintStyle> {
        let lua = mlua::Lua::new();
        let mut properties = PropMap::default();
        properties.insert("background", Value::String(lua.create_string("#112233").unwrap()));
        node::paint_style("rect", &properties).unwrap()
    }

    fn region_node(
        id: u64,
        kind: &'static str,
        rect: (f32, f32, f32, f32),
        paint: Option<PaintStyle>,
        children: Vec<ResolvedNode>,
    ) -> ResolvedNode {
        let node = ResolvedNode { id: NodeId::test(id), paint, ..ResolvedNode::test(kind, rect, children) };
        // A surface root paints as a box that cuts, as `paint_style` makes it.
        if kind == "panel" && node.paint.is_none() { node.with_clip(node::ClipShape::Box) } else { node }
    }

    #[test]
    fn moves_claim_painted_input_and_blur_regions() {
        for (parent_moves, rect, offset, moved) in [
            (true, (5.0, 2.0, 10.0, 10.0), 20.0, PhysicalRect { x0: 35, y0: 12, x1: 45, y1: 22 }),
            (false, (100.0, 0.0, 10.0, 10.0), -20.0, PhysicalRect { x0: 80, y0: 0, x1: 90, y1: 10 }),
        ] {
            let mut glass = region_node(1, "rect", rect, solid_paint(), Vec::new());
            glass.behind_blur = true;
            let children = if parent_moves {
                vec![region_node(2, "column", (10.0, 10.0, 30.0, 20.0), None, vec![glass])]
            } else {
                vec![glass]
            };
            let mut root = region_node(3, "panel", (0.0, 0.0, 100.0, 50.0), None, children);
            root.children[0].movement = Some(Box::new(MoveTween::test((offset, 0.0))));
            assert_eq!(overlay_input_regions(&root, 1.0), [moved]);
            assert_eq!(blur_regions(&root, 1.0), [moved]);
        }

        let mut glass = region_node(4, "rect", (110.0, 0.0, 40.0, 20.0), solid_paint(), Vec::new());
        glass.behind_blur = true;
        glass.transform.scale = (2.0, 2.0);
        glass.transform.origin = (0.0, 0.0);
        glass.movement = Some(Box::new(MoveTween::test((-40.0, 0.0))));
        let root = region_node(5, "panel", (0.0, 0.0, 100.0, 40.0), None, vec![glass]);
        assert_eq!(blur_regions(&root, 1.0), [PhysicalRect { x0: 70, y0: 0, x1: 100, y1: 40 }]);
    }

    /// ADR-0253. A shader's alpha is unknown on the CPU, so its box claims no input; a morph drawn
    /// inside its fully open box would otherwise swallow clicks meant for what is behind it.
    #[test]
    fn a_shader_node_claims_no_input() {
        let paint = Some(PaintStyle::Shader { source: "/s.frag".into(), progress: 0.0, params: Vec::new() });
        assert!(!takes_input_as_a_box(&region_node(1, "shader", (0.0, 0.0, 10.0, 10.0), paint, Vec::new()), true));
    }

    /// A box no bigger than its own rounding still has pixels, and every rectangle handed to
    /// `wl_region` must be a real one: a degenerate middle strip is a request for nothing.
    #[test]
    fn a_box_as_small_as_its_radius_still_produces_a_region_and_never_an_empty_rect() {
        for (w, h, r) in [(2, 2, 1.0), (4, 4, 2.0), (10, 4, 2.0), (3, 9, 1.0)] {
            let mut strips = Vec::new();
            push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: w, y1: h }, node::Radii::from(r), &mut strips);
            for s in &strips {
                assert!(!s.is_empty(), "{w}x{h} r{r} emitted the empty rect {s:?}");
            }
            let covered: i32 = strips.iter().map(|s| (s.x1 - s.x0) * (s.y1 - s.y0)).sum();
            assert!(covered > 0, "{w}x{h} r{r} asked for no blur at all");
        }
    }

    /// The catcher case the whole design exists for: a full-screen surface whose only glass is one
    /// card must hand the compositor the card, not the screen. Proven empirically first -- a niri
    /// `layer-rule` blurring the surface rect flattened a striped backdrop across the whole output.
    #[test]
    fn blur_regions_claim_only_the_marked_node_inside_a_full_screen_surface() {
        let lua = mlua::Lua::new();
        let mut card = region_node(1, "rect", (200.0, 260.0, 620.0, 260.0), solid_paint(), Vec::new());
        card.behind_blur = true;
        let mut catcher = region_node(2, "rect", (0.0, 0.0, 1920.0, 1161.0), None, Vec::new());
        std::rc::Rc::make_mut(&mut catcher.properties)
            .insert("on_click", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        let root = region_node(3, "panel", (0.0, 0.0, 1920.0, 1161.0), None, vec![catcher, card]);

        assert_eq!(
            blur_regions(&root, 1.0),
            [PhysicalRect { x0: 200, y0: 260, x1: 820, y1: 520 }],
            "the card only; the catcher takes the whole screen for input and none of it for blur"
        );
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 0, y0: 0, x1: 1920, y1: 1161 }, PhysicalRect { x0: 200, y0: 260, x1: 820, y1: 520 }],
            "and the input region still takes the whole screen, which is the point of the catcher"
        );
    }

    /// A notification card slides in under `translate` and its glass is the card itself, so the
    /// blur has to travel with the paint, including its ancestors' transforms.
    #[test]
    fn blur_regions_follow_an_ancestors_transform_and_an_ancestors_clip() {
        let mut card = region_node(1, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        card.behind_blur = true;
        let build_slider = |id: u64, card: ResolvedNode| {
            let mut slider = region_node(id, "column", (10.0, 10.0, 100.0, 40.0), None, vec![card]);
            slider.transform = node::Transform { translate: (300.0, 0.0), ..node::Transform::default() };
            slider
        };
        let slider = build_slider(2, card.clone());
        let root = region_node(3, "panel", (0.0, 0.0, 500.0, 100.0), None, vec![slider]);
        assert_eq!(
            blur_regions(&root, 1.0),
            [PhysicalRect { x0: 310, y0: 10, x1: 410, y1: 50 }],
            "the card blurs where its parent's translate paints it, not where the solver left it"
        );

        // A narrower surface cuts it where paint does: the card's own translate cannot carry the
        // root's clip along (`layout::paint::build_node`).
        let narrow = region_node(7, "panel", (0.0, 0.0, 400.0, 100.0), None, vec![build_slider(8, card)]);
        assert_eq!(
            blur_regions(&narrow, 1.0),
            [PhysicalRect { x0: 310, y0: 10, x1: 400, y1: 50 }],
            "the root's box stays put while the slider's translate moves what it holds"
        );

        // The same card scrolled halfway out of a shorter list: paint clips it to the parent box
        // (`layout::paint::build_node`), so blur stops at the same edge.
        let mut card = region_node(4, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        card.behind_blur = true;
        let list = region_node(5, "list", (0.0, 0.0, 100.0, 20.0), None, vec![card]).with_clip(node::ClipShape::Box);
        let root = region_node(6, "panel", (0.0, 0.0, 400.0, 100.0), None, vec![list]);
        assert_eq!(
            blur_regions(&root, 1.0),
            [PhysicalRect { x0: 0, y0: 0, x1: 100, y1: 20 }],
            "clipped to the list, not the card's own height"
        );
    }

    /// `layout::paint::build_node` intersects ancestor boxes *before* any transform and hands the
    /// whole group to the canvas under one matrix, so an ancestor's translate carries the clip
    /// with it. A walk that intersected transformed boxes instead would drop a child its parent
    /// moves back into view, and ask for no blur where paint draws pixels.
    #[test]
    fn a_child_its_parents_translate_carries_into_view_still_asks_for_blur() {
        let mut card = region_node(1, "rect", (80.0, 0.0, 40.0, 20.0), solid_paint(), Vec::new());
        card.behind_blur = true;
        let mut parent =
            region_node(2, "column", (0.0, 0.0, 100.0, 20.0), None, vec![card]).with_clip(node::ClipShape::Box);
        parent.transform = node::Transform { translate: (50.0, 0.0), ..node::Transform::default() };
        let root = region_node(3, "panel", (0.0, 0.0, 300.0, 100.0), None, vec![parent]);

        // The card is clipped to its parent's box untransformed (80..100), then the whole group
        // moves +50, so paint draws 130..150 and the blur must follow it there.
        assert_eq!(
            blur_regions(&root, 1.0),
            [PhysicalRect { x0: 130, y0: 0, x1: 150, y1: 20 }],
            "the clip is applied before the transform, as paint applies it"
        );
    }

    /// The input walk maps a clipping ancestor's cut back through a transformed node, as paint does:
    /// the card is cut at the grandparent's edge, then the group moves.
    #[test]
    fn a_clip_above_a_translated_parent_cuts_the_child_in_its_own_space() {
        let card = region_node(1, "rect", (140.0, 0.0, 40.0, 20.0), solid_paint(), Vec::new());
        let mut parent = region_node(2, "column", (0.0, 0.0, 100.0, 20.0), None, vec![card]);
        parent.transform = node::Transform { translate: (50.0, 0.0), ..node::Transform::default() };
        let cut = region_node(4, "column", (0.0, 0.0, 200.0, 20.0), None, vec![parent]).with_clip(node::ClipShape::Box);
        let root = region_node(3, "panel", (0.0, 0.0, 300.0, 100.0), None, vec![cut]);

        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 190, y0: 0, x1: 200, y1: 20 }]);
    }

    /// A card scrolled halfway out of a list is cut by a straight edge, so rounding must happen on
    /// the node's own box and the cut applied after. Rounding the already-cut rectangle would put
    /// corners on the cut edge and pull blur off the straight sides still on screen.
    #[test]
    fn a_clipped_rounded_card_keeps_square_corners_where_it_was_cut() {
        let mut card = region_node(1, "rect", (0.0, 0.0, 100.0, 100.0), solid_paint(), Vec::new());
        card.behind_blur = true;
        card.paint = Some(PaintStyle::Box {
            background: Some(node::Fill::Color(node::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.8 })),
            radius: node::Radii::from(20.0),
            border: node::BorderPaint::default(),
            widths: crate::layout::node::EdgeInsets::default(),
            clip: node::ClipShape::Box,
            mask: None,
        });
        // Only the top half is inside the list.
        let list = region_node(2, "list", (0.0, 0.0, 100.0, 50.0), None, vec![card]).with_clip(node::ClipShape::Box);
        let root = region_node(3, "panel", (0.0, 0.0, 300.0, 300.0), None, vec![list]);
        let regions = blur_regions(&root, 1.0);

        assert!(!regions.is_empty(), "a half-visible card still blurs");
        let bottom = regions.iter().map(|r| r.y1).max().unwrap();
        assert_eq!(bottom, 50, "nothing reaches past the list");
        // The cut edge is straight: the row just above it spans the card's full width, which a
        // second rounding would have pulled in.
        let widest_at_cut = regions.iter().filter(|r| r.y1 == 50).map(|r| r.x1 - r.x0).max().unwrap();
        assert_eq!(widest_at_cut, 100, "the cut edge is square, not rounded a second time");
    }

    /// Opt-in and nothing else: translucency is not a request, and a faded-out subtree asks for
    /// nothing because it paints nothing.
    #[test]
    fn blur_is_opt_in_and_a_faded_subtree_asks_for_nothing() {
        let glass = region_node(1, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        let root = region_node(2, "panel", (0.0, 0.0, 400.0, 100.0), None, vec![glass]);
        assert!(blur_regions(&root, 1.0).is_empty(), "a painted box that never asked does not blur");

        let mut card = region_node(3, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        card.behind_blur = true;
        let mut faded = region_node(4, "column", (0.0, 0.0, 100.0, 40.0), None, vec![card]);
        faded.opacity = 0.0;
        let root = region_node(5, "panel", (0.0, 0.0, 400.0, 100.0), None, vec![faded]);
        assert!(blur_regions(&root, 1.0).is_empty(), "nothing is painted at zero opacity, so nothing is blurred");
    }

    /// `wl_region` carries no radius, so a rounded card is sent as strips. The cost matters: this
    /// is pushed every time the region changes, so a card must not cost its height in rectangles.
    #[test]
    fn a_rounded_box_becomes_a_middle_and_two_corner_bands_costing_about_its_radius() {
        let mut strips = Vec::new();
        push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 600, y1: 300 }, node::Radii::from(12.0), &mut strips);

        // The band's last rows have no inset left, so they join the middle.
        assert_eq!(strips.iter().filter(|s| s.x0 == 0 && s.x1 == 600).count(), 1, "the straight middle is one rect");
        assert!(strips.len() < 30, "about the radius in strips, not the height: {}", strips.len());
        // Every strip is inside the box, and none of them reaches a corner pixel.
        for s in &strips {
            assert!(s.x0 >= 0 && s.y0 >= 0 && s.x1 <= 600 && s.y1 <= 300, "{s:?} escapes the box");
            assert!(!s.is_empty(), "{s:?} is empty");
        }
        let corner = strips.iter().any(|s| s.x0 == 0 && s.y0 == 0);
        assert!(!corner, "the top-left pixel belongs to the rounding, not to the region");

        let mut square = Vec::new();
        push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 10, y1: 10 }, node::Radii::default(), &mut square);
        assert_eq!(square, [PhysicalRect { x0: 0, y0: 0, x1: 10, y1: 10 }], "no radius is one rectangle");
    }

    /// A smoothed corner reaches further along the top edge than the circle, so the first row of a
    /// blur region starts further in, and the band is `reach` rows tall.
    #[test]
    fn a_smoothed_corner_cuts_a_wider_band_into_the_region() {
        let first_row = |smoothing: f32| {
            let mut strips = Vec::new();
            let radii = node::Radii([16.0; 4], smoothing);
            push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 64, y1: 64 }, radii, &mut strips);
            (strips[0].x0, strips.iter().find(|s| s.x0 == 0).map(|s| s.y0))
        };
        let (circle, smooth) = (first_row(0.0), first_row(1.0));
        assert_eq!(circle.0, 12, "the circle's first row");
        assert!((13..=18).contains(&smooth.0), "the smoothed first row is further in, got {smooth:?}");
        assert!(circle.1.unwrap() < smooth.1.unwrap(), "and the full width starts lower: {circle:?} {smooth:?}");
    }

    /// Each corner cuts its own band; a square one keeps its pixel, and radii too big for a side
    /// shrink together.
    #[test]
    fn each_corner_of_a_region_takes_its_own_radius() {
        let mut strips = Vec::new();
        let radii = node::Radii([12.0, 0.0, 12.0, 0.0], 0.0);
        push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 40, y1: 40 }, radii, &mut strips);
        let covers = |x: i32, y: i32| strips.iter().any(|s| s.x0 <= x && x < s.x1 && s.y0 <= y && y < s.y1);
        assert!(!covers(0, 0) && !covers(39, 39), "rounded corners are cut");
        assert!(covers(39, 0) && covers(0, 39), "square corners are whole");
        let mut small = Vec::new();
        push_rounded_rect(
            PhysicalRect { x0: 0, y0: 0, x1: 40, y1: 10 },
            node::Radii([8.0, 8.0, 0.0, 0.0], 0.0),
            &mut small,
        );
        assert!(!small.iter().any(|s| s.x0 == 0 && s.y0 == 0), "8 + 8 on 10 px of height shrinks to 5 + 5");
    }

    /// A scoop's region is the box less a quarter disc at each corner point: the corner pixel is
    /// out, a pixel just outside the disc is in, and the middle of an edge is whole.
    #[test]
    fn a_scooped_box_region_leaves_out_a_quarter_disc_at_each_corner() {
        let mut strips = Vec::new();
        push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 40, y1: 40 }, node::Radii::from(-12.0), &mut strips);
        let covers = |x: i32, y: i32| strips.iter().any(|s| s.x0 <= x && x < s.x1 && s.y0 <= y && y < s.y1);
        for (x, y) in [(0, 0), (39, 0), (0, 39), (39, 39), (7, 7), (11, 0)] {
            assert!(!covers(x, y), "({x}, {y}) is inside a scoop");
        }
        for (x, y) in [(20, 0), (0, 20), (20, 20), (10, 10), (12, 0)] {
            assert!(covers(x, y), "({x}, {y}) is outside every scoop");
        }
    }

    /// Hit testing cuts a child at a clipping ancestor, so a card scrolled out of a transparent
    /// viewport must not keep a region there: it would catch clicks meant for what is behind.
    #[test]
    fn a_child_outside_a_clipping_ancestor_claims_no_input() {
        let card = |id, y| region_node(id, "rect", (0.0, y, 100.0, 40.0), solid_paint(), Vec::new());
        for clip in [node::ClipShape::Box, node::ClipShape::Rounded] {
            let viewport = region_node(3, "column", (0.0, 0.0, 100.0, 50.0), None, vec![card(1, 0.0), card(2, 200.0)])
                .with_clip(clip);
            let root = region_node(4, "panel", (0.0, 0.0, 300.0, 300.0), None, vec![viewport]);
            assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 100, y1: 40 }]);
        }
        let half = region_node(5, "column", (0.0, 0.0, 100.0, 50.0), None, vec![card(6, 30.0)])
            .with_clip(node::ClipShape::Box);
        let root = region_node(7, "panel", (0.0, 0.0, 300.0, 300.0), None, vec![half]);
        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 30, x1: 100, y1: 50 }]);
    }

    /// An unclipped solid box claims itself and what sticks out of it, not every descendant inside.
    #[test]
    fn an_unclipped_solid_box_claims_its_box_and_only_the_overflow() {
        let grandchild = region_node(1, "rect", (0.0, 0.0, 10.0, 10.0), solid_paint(), Vec::new());
        let inside = region_node(2, "rect", (10.0, 10.0, 20.0, 20.0), solid_paint(), vec![grandchild]);
        let outside = region_node(3, "rect", (80.0, 0.0, 40.0, 20.0), solid_paint(), Vec::new());
        let parent = region_node(4, "rect", (0.0, 0.0, 100.0, 50.0), solid_paint(), vec![inside, outside]);
        let root = region_node(5, "panel", (0.0, 0.0, 300.0, 300.0), None, vec![parent]);
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 0, y0: 0, x1: 100, y1: 50 }, PhysicalRect { x0: 80, y0: 0, x1: 120, y1: 20 }]
        );
    }

    /// ADR-0109: a transparent container is walked into; a solid child claims its box; a node
    /// with a handler claims its box with nothing painted; a transparent leaf claims nothing.
    #[test]
    fn overlay_input_regions_come_from_what_is_drawn_and_what_is_clickable() {
        let lua = mlua::Lua::new();
        let card_a = region_node(1, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        let card_b = region_node(2, "rect", (0.0, 50.0, 100.0, 40.0), solid_paint(), Vec::new());
        let column = region_node(3, "column", (10.0, 10.0, 100.0, 500.0), None, vec![card_a, card_b]);
        let root = region_node(4, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![column]);
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 10, y0: 10, x1: 110, y1: 50 }, PhysicalRect { x0: 10, y0: 60, x1: 110, y1: 100 }],
            "the cards, at their surface-local positions, and not the column"
        );

        let mut catcher = region_node(5, "column", (0.0, 0.0, 120.0, 520.0), None, Vec::new());
        std::rc::Rc::make_mut(&mut catcher.properties)
            .insert("on_click", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        let root = region_node(6, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![catcher]);
        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 120, y1: 520 }]);

        let idle = region_node(7, "row", (0.0, 0.0, 120.0, 520.0), None, Vec::new());
        let root = region_node(8, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![idle]);
        assert!(overlay_input_regions(&root, 1.0).is_empty(), "a row with no handler is as transparent as a rect");

        let label = region_node(9, "rect", (10.0, 10.0, 50.0, 20.0), solid_paint(), Vec::new());
        let mut submit = region_node(10, "row", (0.0, 0.0, 120.0, 40.0), None, vec![label]);
        std::rc::Rc::make_mut(&mut submit.properties).insert("submit", Value::Boolean(true));
        let root = region_node(11, "panel", (0.0, 0.0, 120.0, 40.0), None, vec![submit]);
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 0, y0: 0, x1: 120, y1: 40 }],
            "a submit row claims its box, not only its label"
        );

        let card = region_node(12, "rect", (10.0, 10.0, 50.0, 20.0), solid_paint(), Vec::new());
        let mut root = region_node(13, "panel", (0.0, 0.0, 120.0, 40.0), solid_paint(), vec![card]);
        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 10, y0: 10, x1: 60, y1: 30 }]);
        std::rc::Rc::make_mut(&mut root.properties)
            .insert("on_wheel", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 0, y0: 0, x1: 120, y1: 40 }],
            "a painted root claims nothing, a root with a handler the whole surface"
        );
    }

    /// ADR-0149: the region follows the painted box, and a node scaled to nothing paints nothing.
    #[test]
    fn overlay_input_regions_follow_the_transform_and_vanish_at_zero_scale() {
        let mut card = region_node(1, "rect", (10.0, 10.0, 100.0, 40.0), solid_paint(), Vec::new());
        card.transform.scale = (0.5, 0.5);
        let root = region_node(2, "panel", (0.0, 0.0, 120.0, 60.0), None, vec![card]);
        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 35, y0: 20, x1: 85, y1: 40 }]);

        let mut gone = region_node(3, "rect", (10.0, 10.0, 100.0, 40.0), solid_paint(), Vec::new());
        gone.transform.scale = (0.0, 0.0);
        let root = region_node(4, "panel", (0.0, 0.0, 120.0, 60.0), None, vec![gone]);
        assert!(overlay_input_regions(&root, 1.0).is_empty(), "nothing painted, nothing to press");
    }

    #[test]
    fn overlay_input_regions_includes_only_visible_direct_children() {
        let visible_child = region_node(102, "rect", (0.0, 0.0, 10.0, 10.0), solid_paint(), Vec::new());
        let mut hidden_child = region_node(103, "rect", (20.0, 20.0, 10.0, 10.0), None, Vec::new());
        hidden_child.visible = false;
        let root = region_node(104, "panel", (0.0, 0.0, 100.0, 100.0), None, vec![visible_child, hidden_child]);

        let regions = overlay_input_regions(&root, 1.0);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0], PhysicalRect { x0: 0, y0: 0, x1: 10, y1: 10 });
    }

    #[test]
    fn paths_claim_input_only_when_painted_or_interactive() {
        let lua = mlua::Lua::new();
        for (fields, painted) in [
            ("", false),
            ("fill = '#ffffff', commands = {{ op = 'M', points = {1, 1} }}", false),
            ("commands = {{ op = 'M', points = {1, 1} }, { op = 'L', points = {10, 10} }}", false),
            (
                "stroke = '#ffffff', stroke_width = 0, commands = {{ op = 'M', points = {1, 1} }, { op = 'L', points = {10, 10} }}",
                false,
            ),
            ("stroke = '#ffffff', commands = {{ op = 'M', points = {1, 1} }, { op = 'L', points = {10, 10} }}", true),
            ("fill = '#ffffff', commands = {{ op = 'A', points = {10, 10, 5, 0, 360} }}", true),
        ] {
            let table: mlua::Table = lua.load(format!("return {{ kind = 'path', {fields} }}")).eval().unwrap();
            let properties = node::props_from_table(&table);
            let paint = node::paint_style("path", &properties).unwrap();
            let child = region_node(1, "path", (0.0, 0.0, 20.0, 20.0), paint, Vec::new());
            let mut root = region_node(2, "panel", (0.0, 0.0, 20.0, 20.0), None, vec![child]);
            assert_eq!(!overlay_input_regions(&root, 1.0).is_empty(), painted, "{fields}");
            std::rc::Rc::make_mut(&mut root.children[0].properties)
                .insert("on_click", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
            assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 20, y1: 20 }]);
        }
    }

    #[test]
    fn a_hittable_false_subtree_claims_no_input_until_a_descendant_re_enables_it() {
        let set = |node: &mut ResolvedNode, value| {
            std::rc::Rc::make_mut(&mut node.properties).insert("hittable", Value::Boolean(value));
        };
        let back = region_node(1, "rect", (0.0, 0.0, 20.0, 20.0), solid_paint(), Vec::new());
        let mut front = region_node(2, "rect", (30.0, 0.0, 20.0, 20.0), solid_paint(), Vec::new());
        let mut layer = region_node(3, "column", (0.0, 0.0, 50.0, 20.0), solid_paint(), vec![back]);
        set(&mut layer, false);
        let root = region_node(4, "panel", (0.0, 0.0, 50.0, 20.0), None, vec![layer.clone()]);
        assert!(overlay_input_regions(&root, 1.0).is_empty());
        set(&mut front, true);
        layer.children.push(front);
        let root = region_node(4, "panel", (0.0, 0.0, 50.0, 20.0), None, vec![layer]);
        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 30, y0: 0, x1: 50, y1: 20 }]);
        let card = region_node(5, "rect", (0.0, 0.0, 20.0, 20.0), solid_paint(), Vec::new());
        let mut root = region_node(6, "panel", (0.0, 0.0, 50.0, 20.0), None, vec![card]);
        set(&mut root, false);
        assert!(overlay_input_regions(&root, 1.0).is_empty());
    }

    #[test]
    fn a_surface_with_nothing_visible_in_it_claims_no_input_at_all() {
        let mut hidden_child = region_node(105, "rect", (0.0, 0.0, 100.0, 100.0), None, Vec::new());
        hidden_child.visible = false;
        let mut root = region_node(106, "panel", (0.0, 0.0, 1920.0, 1080.0), None, vec![hidden_child]);
        assert!(overlay_input_regions(&root, 1.0).is_empty());

        root.children.clear();
        assert!(overlay_input_regions(&root, 1.0).is_empty());
    }

    /// `hit.rs` hits a default box's overflowing child, so the region has to hold it too.
    #[test]
    fn an_unclipped_solid_box_still_claims_its_overflowing_child() {
        let child = region_node(1, "rect", (60.0, 0.0, 10.0, 10.0), solid_paint(), Vec::new());
        let parent = region_node(2, "rect", (0.0, 0.0, 50.0, 20.0), solid_paint(), vec![child]);
        let root = region_node(3, "panel", (0.0, 0.0, 100.0, 20.0), None, vec![parent]);
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 0, y0: 0, x1: 50, y1: 20 }, PhysicalRect { x0: 60, y0: 0, x1: 70, y1: 10 }]
        );
    }

    #[test]
    fn a_child_that_fills_its_surface_claims_the_whole_surface() {
        let filling = region_node(120, "row", (0.0, 0.0, 1920.0, 32.0), solid_paint(), Vec::new());
        let root = region_node(107, "panel", (0.0, 0.0, 1920.0, 32.0), None, vec![filling]);

        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 1920, y1: 32 }]);
    }
}

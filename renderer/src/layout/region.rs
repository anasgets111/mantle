//! Where a resolved surface takes input and where it asks for blur (ADR-0109, ADR-0195).

use crate::layout::node::{self, PaintStyle};
use crate::layout::scene::ResolvedNode;
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

/// The input-region scan: what this surface draws and what it can click, as surface-local
/// physical rects (ADR-0038 decision 5, ADR-0109). Pure; the `wl_region`/
/// `wl_surface::set_input_region` push it feeds lives in `crate::wayland::App::apply_input_region`,
/// the only place a Wayland object exists to push to.
///
/// A visible node claims its whole box when it is *solid*: it paints something (a box with a
/// background or a border, or any text, icon, image or field), or it is a `button` with a pointer
/// handler (`on_click`, `on_drag`, `on_wheel`), which is invisible by design but still pressable
/// (a full-surface click-outside catcher). Transparent containers claim nothing and are walked
/// into, so a full-surface `column` holding two cards yields the cards. Everything else is
/// click-through and, under focus-follows-mouse, focus-through; the popup's empty space below its
/// cards therefore takes neither clicks nor keyboard focus.
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
    let paint_claims = !matches!(node::parse_layer(&surface_root.properties), Ok(node::LayerKind::Background));
    let mut regions = Vec::new();
    for child in &surface_root.children {
        collect_input_regions(child, 0.0, 0.0, scale, paint_claims, &mut regions);
    }
    regions
}

/// Every `blur = true` node in one surface, as the physical rects the compositor is handed
/// (ADR-0195). Pure; the `ext_background_effect_surface_v1::set_blur_region` push it feeds lives
/// in `crate::wayland::App::apply_blur_region`.
///
/// Opt-in, never inferred. The sibling walk above answers "what can be clicked", which has one
/// correct answer and so needs no config input; "what should be blurred" is an aesthetic with no
/// correct answer, and inferring it from `background` alpha would have been a guess: a control may
/// be deliberately invisible at `#00000000`, and border-only or image-backed glass carries no
/// background alpha to read.
///
/// Three things this does that [`overlay_input_regions`] does not, each because blur is about
/// where a node is *painted* rather than where it can be pressed:
///
/// 1. **Ancestor transforms compose.** `painted_bounds` reads one node's own transform and says so;
///    a notification card slides in under `translate` while its own children are the glass, so a
///    walk that missed the ancestor's shift would blur where the card is not. Exact for the
///    translation every animation here uses; a rotated or scaled node contributes its bounding box.
/// 2. **Ancestor clips intersect.** `layout::paint::build_node` clips every child to its parent's
///    box, so a card scrolled out of a `max_height` list is not drawn and must not blur either.
/// 3. **The surface root is included**, because a root may paint its own box.
///
/// A claiming node does not stop the walk: a marked child inside a marked parent unions into it,
/// and a rounded parent that does not clip can have children painting outside its corners.
pub fn blur_regions(surface_root: &ResolvedNode, scale: f32) -> Vec<PhysicalRect> {
    let mut regions = Vec::new();
    // The root intersects itself on the first step, so this only has to not be the limit.
    let everything =
        LogicalRect { x: f32::MIN / 4.0, y: f32::MIN / 4.0, width: f32::MAX / 2.0, height: f32::MAX / 2.0 };
    collect_blur_regions(surface_root, 0.0, 0.0, scale, IDENTITY_AFFINE, everything, 1.0, &mut regions);
    regions
}

const IDENTITY_AFFINE: node::Affine = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `outer` applied after `inner`, which is the order `layout::paint` nests its `Draw::Transformed`
/// groups in.
fn compose_affine(outer: node::Affine, inner: node::Affine) -> node::Affine {
    let [a1, b1, c1, d1, e1, f1] = outer;
    let [a2, b2, c2, d2, e2, f2] = inner;
    [
        a1 * a2 + c1 * b2,
        b1 * a2 + d1 * b2,
        a1 * c2 + c1 * d2,
        b1 * c2 + d1 * d2,
        a1 * e2 + c1 * f2 + e1,
        b1 * e2 + d1 * f2 + f1,
    ]
}

/// The axis-aligned bounds of `rect`'s four corners under `matrix`.
pub(super) fn transformed_bounds(matrix: node::Affine, rect: LogicalRect) -> LogicalRect {
    let corners = [
        node::apply_affine(matrix, rect.x, rect.y),
        node::apply_affine(matrix, rect.x + rect.width, rect.y),
        node::apply_affine(matrix, rect.x, rect.y + rect.height),
        node::apply_affine(matrix, rect.x + rect.width, rect.y + rect.height),
    ];
    let (x0, y0) = corners.iter().fold((f32::MAX, f32::MAX), |(x, y), &(cx, cy)| (x.min(cx), y.min(cy)));
    let (x1, y1) = corners.iter().fold((f32::MIN, f32::MIN), |(x, y), &(cx, cy)| (x.max(cx), y.max(cy)));
    LogicalRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
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
    let rect = LogicalRect { x: origin_x + node.rect.x, y: origin_y + node.rect.y, ..node.rect };
    let matrix =
        if node.transform.is_identity() { matrix } else { compose_affine(matrix, node.transform.matrix(rect)) };
    // Untransformed, exactly as `layout::paint::build_node` accumulates it: that walk intersects
    // boxes before any transform and hands the whole group to the canvas under one matrix, so a
    // node's painted area is its ancestors' clip *and then* the composed transform. Intersecting
    // transformed boxes instead loses a child that its parent's translate carries back into view.
    let clip = intersect_logical(clip, rect);
    if clip.width <= 0.0 || clip.height <= 0.0 {
        return;
    }
    if node.blur {
        let radius = match &node.paint {
            Some(PaintStyle::Box { radius, .. }) => *radius,
            _ => 0.0,
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
            snap_to_physical(transformed_bounds(matrix, rect), scale),
            radius * scale * grow,
            &mut rounded,
        );
        let visible = snap_to_physical(transformed_bounds(matrix, clip), scale);
        for strip in rounded {
            let cut = strip.intersect(visible);
            if cut.x1 > cut.x0 && cut.y1 > cut.y0 {
                out.push(cut);
            }
        }
    }
    for child in &node.children {
        collect_blur_regions(child, rect.x, rect.y, scale, matrix, clip, opacity * node.opacity, out);
    }
}

/// The overlap of two untransformed absolute boxes, zero-sized when they miss.
fn intersect_logical(a: LogicalRect, b: LogicalRect) -> LogicalRect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    LogicalRect { x, y, width: right - x, height: bottom - y }
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
/// mirrored top to bottom and side to side.
fn push_rounded_rect(rect: PhysicalRect, radius: f32, out: &mut Vec<PhysicalRect>) {
    if rect.x1 <= rect.x0 || rect.y1 <= rect.y0 {
        return;
    }
    let height = rect.y1 - rect.y0;
    let width = rect.x1 - rect.x0;
    // A radius cannot exceed half the box in either axis, the same clamp the painter's arcs use.
    let r = (radius.abs().round() as i32).min(width / 2).min(height / 2);
    if r <= 0 {
        out.push(rect);
        return;
    }
    // The straight middle, full width, between the two corner bands. A box exactly twice its own
    // radius tall has no middle, and an empty rectangle is a request for nothing.
    if rect.y1 - r > rect.y0 + r {
        out.push(PhysicalRect { x0: rect.x0, y0: rect.y0 + r, x1: rect.x1, y1: rect.y1 - r });
    }
    // One band walked once, mirrored top and bottom.
    let inset_of = |row| if radius < 0.0 { r - inset_at(r, r - 1 - row) } else { inset_at(r, row) };
    let mut row = 0;
    while row < r {
        let inset = inset_of(row);
        let mut last = row + 1;
        while last < r && inset_of(last) == inset {
            last += 1;
        }
        if rect.x0 + inset < rect.x1 - inset {
            out.push(PhysicalRect { x0: rect.x0 + inset, y0: rect.y0 + row, x1: rect.x1 - inset, y1: rect.y0 + last });
            out.push(PhysicalRect { x0: rect.x0 + inset, y0: rect.y1 - last, x1: rect.x1 - inset, y1: rect.y1 - row });
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

fn collect_input_regions(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    paint_claims: bool,
    out: &mut Vec<PhysicalRect>,
) {
    if !node.in_flow() {
        return;
    }
    let rect = LogicalRect { x: origin_x + node.rect.x, y: origin_y + node.rect.y, ..node.rect };
    if takes_input_as_a_box(node, paint_claims) {
        let bounds = painted_bounds(node, rect);
        if bounds.width > 0.0 && bounds.height > 0.0 {
            out.push(snap_to_physical(bounds, scale));
        }
        return;
    }
    for child in &node.children {
        collect_input_regions(child, rect.x, rect.y, scale, paint_claims, out);
    }
}

/// Where a node is on screen: its box, or the bounds of that box under its own transform
/// (ADR-0149), so a scaled tile takes input where it is painted.
/// ponytail: ancestors' transforms are not composed in; a transformed node inside a transformed
/// node reports its own box's bounds only. Upgrade path: carry the matrix down this walk.
fn painted_bounds(node: &ResolvedNode, rect: LogicalRect) -> LogicalRect {
    if node.transform.is_identity() {
        return rect;
    }
    transformed_bounds(node.transform.matrix(rect), rect)
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
            // Its alpha is the GPU's to know; a config adds a `button` for a hit area (ADR-0253).
            Some(PaintStyle::Shader { .. }) | None => false,
            Some(_) => true,
        };
    paints || node.takes_pointer()
}

#[cfg(test)]
mod tests {
    use mlua::Value;

    use super::*;
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
        ResolvedNode { id: NodeId::test(id), paint, ..ResolvedNode::test(kind, rect, children) }
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
            push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: w, y1: h }, r, &mut strips);
            for s in &strips {
                assert!(s.x1 > s.x0 && s.y1 > s.y0, "{w}x{h} r{r} emitted the empty rect {s:?}");
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
        card.blur = true;
        let mut catcher = region_node(2, "button", (0.0, 0.0, 1920.0, 1161.0), None, Vec::new());
        catcher.properties.insert("on_click", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
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
    /// blur has to travel with the paint. `painted_bounds` reads one node's own transform only
    /// (its comment says so); this walk composes ancestors' too.
    #[test]
    fn blur_regions_follow_an_ancestors_transform_and_an_ancestors_clip() {
        let mut card = region_node(1, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        card.blur = true;
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

        // A narrower surface does not cut it, and neither does paint: the clip is intersected
        // before any transform (`layout::paint::build_node`), so a card whose ancestor translate
        // carries it past the surface edge is still drawn there, and the compositor clips the
        // region to the surface itself. Cutting here would disagree with the pixels.
        let narrow = region_node(7, "panel", (0.0, 0.0, 400.0, 100.0), None, vec![build_slider(8, card)]);
        assert_eq!(
            blur_regions(&narrow, 1.0),
            [PhysicalRect { x0: 310, y0: 10, x1: 410, y1: 50 }],
            "the untransformed clip is what paint uses, so the translate is not cut by the surface box"
        );

        // The same card scrolled halfway out of a shorter list: paint clips it to the parent box
        // (`layout::paint::build_node`), so blur stops at the same edge.
        let mut card = region_node(4, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        card.blur = true;
        let list = region_node(5, "list", (0.0, 0.0, 100.0, 20.0), None, vec![card]);
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
        card.blur = true;
        let mut parent = region_node(2, "column", (0.0, 0.0, 100.0, 20.0), None, vec![card]);
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

    /// A card scrolled halfway out of a list is cut by a straight edge, so rounding must happen on
    /// the node's own box and the cut applied after. Rounding the already-cut rectangle would put
    /// corners on the cut edge and pull blur off the straight sides still on screen.
    #[test]
    fn a_clipped_rounded_card_keeps_square_corners_where_it_was_cut() {
        let mut card = region_node(1, "rect", (0.0, 0.0, 100.0, 100.0), solid_paint(), Vec::new());
        card.blur = true;
        card.paint = Some(PaintStyle::Box {
            background: Some(node::Fill::Color(node::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.8 })),
            radius: 20.0,
            colors: node::BorderColor::default(),
            widths: crate::layout::node::EdgeInsets::default(),
            clip: node::ClipShape::Box,
            mask: None,
        });
        // Only the top half is inside the list.
        let list = region_node(2, "list", (0.0, 0.0, 100.0, 50.0), None, vec![card]);
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
        card.blur = true;
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
        push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 600, y1: 300 }, 12.0, &mut strips);

        assert_eq!(strips[0], PhysicalRect { x0: 0, y0: 12, x1: 600, y1: 288 }, "the straight middle is one rect");
        assert!(strips.len() < 30, "about the radius in strips, not the height: {}", strips.len());
        // Every strip is inside the box, and none of them reaches a corner pixel.
        for s in &strips {
            assert!(s.x0 >= 0 && s.y0 >= 0 && s.x1 <= 600 && s.y1 <= 300, "{s:?} escapes the box");
            assert!(s.x1 > s.x0 && s.y1 > s.y0, "{s:?} is empty");
        }
        let corner = strips.iter().any(|s| s.x0 == 0 && s.y0 == 0);
        assert!(!corner, "the top-left pixel belongs to the rounding, not to the region");

        let mut square = Vec::new();
        push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 10, y1: 10 }, 0.0, &mut square);
        assert_eq!(square, [PhysicalRect { x0: 0, y0: 0, x1: 10, y1: 10 }], "no radius is one rectangle");
    }

    /// A scoop's region is the box less a quarter disc at each corner point: the corner pixel is
    /// out, a pixel just outside the disc is in, and the middle of an edge is whole.
    #[test]
    fn a_scooped_box_region_leaves_out_a_quarter_disc_at_each_corner() {
        let mut strips = Vec::new();
        push_rounded_rect(PhysicalRect { x0: 0, y0: 0, x1: 40, y1: 40 }, -12.0, &mut strips);
        let covers = |x: i32, y: i32| strips.iter().any(|s| s.x0 <= x && x < s.x1 && s.y0 <= y && y < s.y1);
        for (x, y) in [(0, 0), (39, 0), (0, 39), (39, 39), (7, 7), (11, 0)] {
            assert!(!covers(x, y), "({x}, {y}) is inside a scoop");
        }
        for (x, y) in [(20, 0), (0, 20), (20, 20), (10, 10), (12, 0)] {
            assert!(covers(x, y), "({x}, {y}) is outside every scoop");
        }
    }

    /// ADR-0109: a transparent container is walked into; a solid child claims its box; a `button`
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

        let mut catcher = region_node(5, "button", (0.0, 0.0, 120.0, 520.0), None, Vec::new());
        catcher.properties.insert("on_click", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        let root = region_node(6, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![catcher]);
        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 120, y1: 520 }]);

        let idle_button = region_node(7, "button", (0.0, 0.0, 120.0, 520.0), None, Vec::new());
        let root = region_node(8, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![idle_button]);
        assert!(overlay_input_regions(&root, 1.0).is_empty(), "a button with no handler is as transparent as a rect");

        let label = region_node(9, "rect", (10.0, 10.0, 50.0, 20.0), solid_paint(), Vec::new());
        let mut submit = region_node(10, "button", (0.0, 0.0, 120.0, 40.0), None, vec![label]);
        submit.properties.insert("submit", Value::Boolean(true));
        let root = region_node(11, "panel", (0.0, 0.0, 120.0, 40.0), None, vec![submit]);
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 0, y0: 0, x1: 120, y1: 40 }],
            "a submit button claims its box, not only its label"
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
    fn a_surface_with_nothing_visible_in_it_claims_no_input_at_all() {
        let mut hidden_child = region_node(105, "rect", (0.0, 0.0, 100.0, 100.0), None, Vec::new());
        hidden_child.visible = false;
        let mut root = region_node(106, "panel", (0.0, 0.0, 1920.0, 1080.0), None, vec![hidden_child]);
        assert!(overlay_input_regions(&root, 1.0).is_empty());

        root.children.clear();
        assert!(overlay_input_regions(&root, 1.0).is_empty());
    }

    #[test]
    fn a_child_that_fills_its_surface_claims_the_whole_surface() {
        let filling = region_node(120, "row", (0.0, 0.0, 1920.0, 32.0), solid_paint(), Vec::new());
        let root = region_node(107, "panel", (0.0, 0.0, 1920.0, 32.0), None, vec![filling]);

        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 1920, y1: 32 }]);
    }
}

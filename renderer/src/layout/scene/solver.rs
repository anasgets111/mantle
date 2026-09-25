use super::{LayoutStyle, LogicalSize};
use crate::layout::node::{self, Align, LayoutError, PaintStyle, PropMap, SizeMode, Tween};
use crate::text::shaping::{self, ShapeRequest, ShapingHandle};
use taffy::prelude::{length, line, span};

/// A per-pass solver tree. Geometry stays fractional until `text::snap` applies the
/// surface scale at paint; taffy otherwise rounds layouts to whole numbers.
pub(super) fn new_solver_tree() -> taffy::TaffyTree<Measure> {
    let mut tree = taffy::TaffyTree::new();
    tree.disable_rounding();
    tree
}

/// The parent's flow axis. Stacking parents have none; each child gets the whole content box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainAxis {
    Horizontal,
    Vertical,
}

/// Which axis `kind` flows along, or `None` when it does not flow at all; a `list` borrows its
/// `direction` from [`flow_kind`], so a horizontal `list` takes a horizontal wheel.
///
/// `pub(crate)` for `wayland::input`'s wheel handler, which has to know whether a node under the
/// pointer takes a horizontal or a vertical wheel before it writes anything.
pub(crate) fn main_axis_of(kind: &str, properties: &PropMap) -> Result<Option<MainAxis>, LayoutError> {
    Ok(match flow_kind(kind, properties)? {
        "row" => Some(MainAxis::Horizontal),
        "column" => Some(MainAxis::Vertical),
        _ => None,
    })
}

/// Leaf sizes taffy cannot derive from style alone. Parsed in [`prepare`] so malformed icon sizes
/// can return [`LayoutError`]; the measure callback returns only `Size<f32>`.
///
/// [`prepare`]: super::pass::prepare
pub(super) enum Measure {
    /// Shaped extent; `wrap` and `max_lines` change geometry, not only paint.
    Text {
        content: std::sync::Arc<str>,
        runs: Vec<shaping::FontRun>,
        font_size: f32,
        /// The family the box is measured against, so the reserved width is the one the same
        /// family will paint into (ADR-0144).
        font: Option<std::sync::Arc<str>>,
        wrap: node::Wrap,
        max_lines: Option<usize>,
        /// The last `(max_width, size)` this node measured; see [`solve`].
        memo: Option<(Option<f32>, taffy::Size<f32>)>,
    },
    /// `icon`'s `size`, the same number on both axes.
    Square(f32),
}

/// `Content` and `Fill` map to taffy's `auto`; `Fill` gets its meaning from parent flow and
/// [`taffy_style`]'s grow/stretch rules.
fn taffy_dimension(mode: SizeMode) -> taffy::Dimension {
    match mode {
        SizeMode::Content | SizeMode::Fill => taffy::Dimension::auto(),
        SizeMode::Pixels(n) => taffy::Dimension::length(n),
        SizeMode::Percent(p) => taffy::Dimension::percent(p),
    }
}

/// `Align` as an item's alignment inside its parent slot.
fn item_align(align: Align) -> taffy::AlignSelf {
    match align {
        Align::Start => taffy::AlignItems::START,
        Align::Center => taffy::AlignItems::CENTER,
        Align::End => taffy::AlignItems::END,
        Align::Stretch => taffy::AlignItems::STRETCH,
    }
}

/// `Align` as a flow container's packing. `Stretch` packs as `Start`: a main axis has nothing to
/// stretch into that `Fill` does not already claim.
fn main_align(align: Align) -> taffy::JustifyContent {
    match align {
        Align::Start | Align::Stretch => taffy::JustifyContent::START,
        Align::Center => taffy::JustifyContent::CENTER,
        Align::End => taffy::JustifyContent::END,
    }
}

/// One node's taffy style, combining its container and item roles. `parent_axis` decides whether a
/// `Fill` shares a flow remainder or takes the whole slot; `None` means stacking or surface root.
fn taffy_style(
    kind: &str,
    properties: &PropMap,
    style: &LayoutStyle,
    parent_axis: Option<MainAxis>,
) -> Result<taffy::Style, LayoutError> {
    // Invisible nodes get no size, position, or spacing gap; all readers filter on `visible`.
    if !style.visible {
        return Ok(taffy::Style { display: taffy::Display::None, ..taffy::Style::DEFAULT });
    }

    let mut out = taffy::Style {
        // No shrink: fixed children keep their stated size, even when siblings overflow. Disable
        // taffy's automatic minimum so a `Fill` item can collapse to zero.
        // On a flex cross axis, leave `min_size` as `auto`: taffy 0.14 otherwise adds the
        // container's margin to each child's minimum (`constants.margin` instead of `child.margin`
        // in `determine_flex_base_size`/`determine_container_main_size`). `Some(0) + margin` floors
        // it; `None + margin` does not. The bug measured a panel body at 1521px wide/one line,
        // then drew it 378px wide/two lines, making every card a line short
        // (`a_containers_own_margin_does_not_widen_what_its_children_are_measured_at`).
        flex_shrink: 0.0,
        // A declared `min_width`/`min_height` takes that axis over; the other keeps the default.
        min_size: {
            let automatic = match parent_axis {
                Some(MainAxis::Horizontal) => {
                    taffy::Size { width: length(0.0), height: taffy::LengthPercentageAuto::auto() }
                }
                Some(MainAxis::Vertical) => {
                    taffy::Size { width: taffy::LengthPercentageAuto::auto(), height: length(0.0) }
                }
                None => taffy::Size { width: length(0.0), height: length(0.0) },
            };
            taffy::Size {
                width: style.min_width.map_or(automatic.width, taffy::LengthPercentageAuto::length),
                height: style.min_height.map_or(automatic.height, taffy::LengthPercentageAuto::length),
            }
        },
        padding: taffy::Rect {
            left: length(style.padding.left),
            right: length(style.padding.right),
            top: length(style.padding.top),
            bottom: length(style.padding.bottom),
        },
        margin: taffy::Rect {
            left: length(style.margin.left),
            right: length(style.margin.right),
            top: length(style.margin.top),
            bottom: length(style.margin.bottom),
        },
        size: taffy::Size { width: taffy_dimension(style.width_mode), height: taffy_dimension(style.height_mode) },
        // The ceiling is taffy's own `max-height`: the node's auto height is measured from its
        // children and then capped, and the children keep the height they were given, which is
        // what leaves `finish`'s `extent_along` a remainder for the scroll offset to be clamped to.
        max_size: taffy::Size {
            width: style.max_width.map_or_else(taffy::LengthPercentageAuto::auto, taffy::LengthPercentageAuto::length),
            height: style
                .max_height
                .map_or_else(taffy::LengthPercentageAuto::auto, taffy::LengthPercentageAuto::length),
        },
        ..taffy::Style::DEFAULT
    };

    // Container half.
    match main_axis_of(kind, properties)? {
        Some(axis) => {
            out.display = taffy::Display::Flex;
            out.flex_direction = match axis {
                MainAxis::Horizontal => taffy::FlexDirection::Row,
                MainAxis::Vertical => taffy::FlexDirection::Column,
            };
            // A flow container packs along its own axis; children control the other axis.
            out.justify_content = Some(main_align(match axis {
                MainAxis::Horizontal => style.align_h,
                MainAxis::Vertical => style.align_v,
            }));
            // Adjacent-child spacing is a flex gap; set both axes because there is one flex line.
            out.gap = taffy::Size { width: length(style.spacing), height: length(style.spacing) };
        }
        // ADR-0023's stacking model is one auto-sized grid cell: children overlap and align
        // independently, while `Content` is their bounding union.
        None => out.display = taffy::Display::Grid,
    }

    // Item half. `Fill` off the parent's flow axis means the whole slot and outranks alignment.
    let fills_h = style.width_mode == SizeMode::Fill && parent_axis != Some(MainAxis::Horizontal);
    let fills_v = style.height_mode == SizeMode::Fill && parent_axis != Some(MainAxis::Vertical);
    let align_h = if fills_h { taffy::AlignItems::STRETCH } else { item_align(style.align_h) };
    let align_v = if fills_v { taffy::AlignItems::STRETCH } else { item_align(style.align_v) };

    // Flex parents govern the main axis with `justify_content`; grid items state both axes.
    let (governed_h, governed_v) = match parent_axis {
        Some(MainAxis::Horizontal) => (None, Some(align_v)),
        Some(MainAxis::Vertical) => (Some(align_h), None),
        None => (Some(align_h), Some(align_v)),
    };

    // Taffy's `align-self: stretch` applies only to an `auto` cross size, so `height = 5` would
    // normally win. Stretch takes precedence here
    // (`row_child_stretch_alignment_fills_the_cross_axis`); blank the size to make taffy do that.
    if governed_h == Some(taffy::AlignItems::STRETCH) {
        out.size.width = taffy::Dimension::auto();
    }
    if governed_v == Some(taffy::AlignItems::STRETCH) {
        out.size.height = taffy::Dimension::auto();
    }

    match parent_axis {
        // Flex item: parent packs the main axis; cross alignment is local. A zero basis makes
        // main-axis `Fill` share the whole remainder
        // (`two_fill_siblings_split_the_remainder_equally`).
        Some(MainAxis::Horizontal) => {
            out.align_self = Some(align_v);
            if style.width_mode == SizeMode::Fill {
                out.flex_grow = 1.0;
                out.flex_basis = taffy::Dimension::length(0.0);
            }
        }
        Some(MainAxis::Vertical) => {
            out.align_self = Some(align_h);
            if style.height_mode == SizeMode::Fill {
                out.flex_grow = 1.0;
                out.flex_basis = taffy::Dimension::length(0.0);
            }
        }
        // Grid item in the shared cell; both alignments are local.
        None => {
            out.justify_self = Some(align_h);
            out.align_self = Some(align_v);
            out.grid_row = taffy::Line { start: line(1), end: span(1) };
            out.grid_column = taffy::Line { start: line(1), end: span(1) };
        }
    }

    Ok(out)
}

/// Builds one node's [`taffy_style`] and hands back the solver node holding it, with no children
/// attached yet.
///
/// Split out of [`prepare`] purely for the stack: a `taffy::Style` is 552 bytes, and [`prepare`]
/// recurses one frame per tree level with `MAX_TREE_DEPTH` of them allowed, so a `Style` built
/// there is 552 bytes multiplied by the depth cap. Built here, it lives in a frame that returns
/// before the recursion descends.
///
/// [`prepare`]: super::pass::prepare
pub(super) fn new_solver_node(
    tree: &mut taffy::TaffyTree<Measure>,
    kind: &str,
    properties: &PropMap,
    style: &LayoutStyle,
    parent_axis: Option<MainAxis>,
    measure: Option<Measure>,
) -> Result<taffy::NodeId, LayoutError> {
    let solver_style = taffy_style(kind, properties, style, parent_axis)?;
    match measure {
        Some(measure) => tree.new_leaf_with_context(solver_style, measure),
        None => tree.new_leaf(solver_style),
    }
    .map_err(taffy_failed)
}

/// A content-sized axis of `id` grows to span its leavers' last rects (ADR-0150): they take no room
/// in the flow, but a parent collapsing under a lone leaver would clip its exit away, and a
/// content-sized surface with it. Fixed and `Fill` axes already had room for them.
///
/// Its own frame, like [`new_solver_node`], for the `taffy::Style` it clones.
pub(super) fn hold_leavers(
    tree: &mut taffy::TaffyTree<Measure>,
    id: taffy::NodeId,
    style: &LayoutStyle,
    leaving: &[super::ResolvedNode],
) -> Result<(), LayoutError> {
    let content = (style.width_mode == SizeMode::Content, style.height_mode == SizeMode::Content);
    if leaving.is_empty() || content == (false, false) {
        return Ok(());
    }
    let (right, bottom) = leaving.iter().fold((0.0_f32, 0.0_f32), |(right, bottom), leaver| {
        let rect = leaver.rect;
        (right.max(rect.x + rect.width + leaver.margin.right), bottom.max(rect.y + rect.height + leaver.margin.bottom))
    });
    let mut solver_style = tree.style(id).map_err(taffy_failed)?.clone();
    if content.0 {
        solver_style.min_size.width = length(style.min_width.unwrap_or(0.0).max(right + style.padding.right));
    }
    if content.1 {
        solver_style.min_size.height = length(style.min_height.unwrap_or(0.0).max(bottom + style.padding.bottom));
    }
    tree.set_style(id, solver_style).map_err(taffy_failed)
}

pub(super) const TEXT_MEASURE_KEYS: &[&str] = &["content", "font_size", "font", "wrap", "max_lines"];

pub(super) fn text_measure_matches(fresh: &PropMap, retained: &PropMap) -> bool {
    TEXT_MEASURE_KEYS.iter().all(|k| fresh.get(k) == retained.get(k))
}

/// Whether a running tween moves what this `text` measures from, so advancing it voids the memo.
pub(super) fn text_measure_tweening(kind: &str, tweens: &[Tween]) -> bool {
    kind == "text" && tweens.iter().any(|t| !t.resting && TEXT_MEASURE_KEYS.contains(&t.property))
}

/// What the solver asks a leaf for its size with, for the two kinds whose size is their content.
pub(super) fn measure_for(
    kind: &str,
    paint: Option<&PaintStyle>,
    properties: &PropMap,
    memo: Option<(Option<f32>, taffy::Size<f32>)>,
) -> Result<Option<Measure>, LayoutError> {
    Ok(match flow_kind(kind, properties)? {
        // `node::paint_style` gives every `text` a `PaintStyle::Text` and `flow_kind` cannot route
        // another kind here, so the arm is total, the same shape as `pass::children_of`'s
        // `unreachable!` arm.
        "text" => {
            let Some(PaintStyle::Text { content, runs, font_size, font, wrap, max_lines, .. }) = paint else {
                unreachable!("paint_style produces PaintStyle::Text for text nodes");
            };
            Some(Measure::Text {
                content: content.clone(),
                runs: node::font_runs(runs),
                font_size: *font_size,
                font: font.clone(),
                wrap: *wrap,
                max_lines: *max_lines,
                memo,
            })
        }
        "icon" => Some(Measure::Square(node::fields::icon::size.read(properties)?)),
        // `image` has no intrinsic size, unlike `icon`: knowing a file's own dimensions means
        // decoding it, and this pass has no canvas to decode against and runs on every
        // `Scene::apply`. So an `image` takes the box `width`/`height` give it, measuring
        // nothing without one, the same as an empty `rect`.
        _ => None,
    })
}

/// taffy's own errors are all "you handed me a node id I do not have", which the scene pass cannot do:
/// every id comes from the tree it is used against, and the tree lives no longer than the pass. So
/// this is the `unreachable!` equivalent for a `Result` that has to be handled anyway, reported as
/// a pass failure rather than a panic on the Wayland dispatch thread.
pub(super) fn taffy_failed(err: taffy::TaffyError) -> LayoutError {
    node::invalid("layout", format!("the layout solver refused a node built by this pass: {err}"))
}

/// Runs taffy's own layout passes over the tree: sizes up, positions down, and the constraints
/// resolved between them. Given a prepared tree and the room its root has, fills in every node's
/// geometry.
///
/// The measure callback is the one place this crate is still asked a geometry question, only for
/// the two kinds whose size is their content: a `text`'s shaped extent and an `icon`'s square.
/// taffy asks each `text` about ten times a pass at one `max_width`, always so for a bar's
/// non-wrapping labels, so `Measure::Text` answers a repeat from its last `(max_width, size)`.
/// `ShapingHandle`'s memo answers one only after hashing a `ShapeRequest` that owns a copy of the
/// string: 10,000 of those are 1.1ms of a 7.4ms pass on a 500-row list. One entry, since a miss
/// falls back on that memo, and no invalidation, since [`new_solver_tree`] builds the context
/// fresh each pass. A `NaN` width never equals itself, so it re-measures rather than going stale.
pub(super) fn solve(
    tree: &mut taffy::TaffyTree<Measure>,
    root: taffy::NodeId,
    available: LogicalSize,
    shaping: &ShapingHandle,
) -> Result<(), LayoutError> {
    let space = taffy::Size {
        width: taffy::AvailableSpace::Definite(available.width),
        height: taffy::AvailableSpace::Definite(available.height),
    };
    tree.compute_layout_with_measure(root, space, |input, _node, context, style| {
        taffy::compute_leaf_layout(
            input,
            style,
            |_, _| 0.0,
            |known, offered| {
                let Some(measure) = context else {
                    return taffy::Size::ZERO;
                };
                match measure {
                    Measure::Square(size) => taffy::Size { width: *size, height: *size },
                    Measure::Text { content, runs, font_size, font, wrap, max_lines, memo } => {
                        // The wrap boundary: the width this box is already known to have, or the
                        // width on offer when it is not. `MaxContent`/`MinContent` mean taffy is
                        // asking what the string wants rather than offering it a box, and an
                        // unconstrained measurement is the honest answer to that.
                        //
                        // `None` for a node that does not wrap, so it measures the one line it
                        // will paint, not the wrapped height of a box it draws one clipped line in.
                        let max_width = match wrap {
                            node::Wrap::None => None,
                            node::Wrap::Word => known.width.or(match offered.width {
                                taffy::AvailableSpace::Definite(width) => Some(width),
                                taffy::AvailableSpace::MinContent | taffy::AvailableSpace::MaxContent => None,
                            }),
                        };
                        if let Some((key, size)) = memo
                            && *key == max_width
                        {
                            return *size;
                        }
                        let line_height = shaping::line_height(*font_size);
                        let shaped = shaping.shape(ShapeRequest {
                            text: content.to_string(),
                            font_size: *font_size,
                            line_height,
                            max_width,
                            runs: runs.clone(),
                            font: font.clone(),
                        });
                        let lines = max_lines.map_or(shaped.lines.len(), |cap| shaped.lines.len().min(cap));
                        let size = taffy::Size { width: shaped.width, height: lines as f32 * line_height };
                        *memo = Some((max_width, size));
                        size
                    }
                }
            },
        )
    })
    .map_err(taffy_failed)
}

/// The kind whose layout `kind` actually uses. Every kind is itself except `list`, which borrows a
/// `row`'s or a `column`'s arm depending on its `direction`.
///
/// A `list` is a repeater, not a third layout: it reconciles children by key, then stacks them,
/// and "stacks them" is a `column` or a `row` and nothing else. Routing to the existing arms keeps
/// a horizontal list identical to a hand-built `row`, rather than a second implementation that
/// agrees with it until it does not.
fn flow_kind<'a>(kind: &'a str, properties: &PropMap) -> Result<&'a str, LayoutError> {
    if kind == "list" { Ok(node::fields::list::direction.read(properties)?.kind()) } else { Ok(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::scene::tests::{apply_at, full, surface_from};
    use crate::layout::scene::*;

    fn direction_props(lua: &mlua::Lua, direction: Option<&str>) -> PropMap {
        let mut properties = PropMap::default();
        if let Some(direction) = direction {
            properties.insert("direction", Value::String(lua.create_string(direction).unwrap()));
        }
        properties
    }

    #[test]
    fn a_list_lays_out_as_a_column_unless_it_says_otherwise() {
        // The default is what every config written before `direction` existed relies on.
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("list", &direction_props(&lua, None)).unwrap(), "column");
        assert_eq!(flow_kind("list", &direction_props(&lua, Some("Vertical"))).unwrap(), "column");
    }

    #[test]
    fn a_horizontal_list_lays_out_as_a_row() {
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("list", &direction_props(&lua, Some("Horizontal"))).unwrap(), "row");
    }

    #[test]
    fn direction_on_anything_that_is_not_a_list_is_ignored_rather_than_obeyed() {
        // `row` and `column` already say which way they go in their own name, so a `direction` on
        // one is a config confusing itself, not a second way to spell the kind.
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("column", &direction_props(&lua, Some("Horizontal"))).unwrap(), "column");
        assert_eq!(flow_kind("row", &direction_props(&lua, Some("Vertical"))).unwrap(), "row");
    }

    #[test]
    fn an_unknown_direction_is_refused_by_name() {
        let lua = mlua::Lua::new();
        let err = flow_kind("list", &direction_props(&lua, Some("sideways"))).unwrap_err().to_string();
        assert!(err.contains("sideways"), "the message has to name what was written: {err}");
        assert!(err.contains("Horizontal"), "and what was expected: {err}");
    }

    #[test]
    fn a_pixels_sized_rect_resolves_to_its_explicit_size() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = rect { width = 40, height = 20 } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        let child = &root.children[0];
        assert_eq!(child.rect.width, 40.0);
        assert_eq!(child.rect.height, 20.0);
    }

    #[test]
    fn a_childless_rect_with_no_explicit_size_resolves_to_zero() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = rect {} }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 0.0);
        assert_eq!(child.rect.height, 0.0);
    }

    #[test]
    fn a_fill_child_takes_its_parents_available_bounds() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 1000, height = 500, child = rect { width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 1000.0);
        assert_eq!(child.rect.height, 500.0);
    }

    #[test]
    fn a_percent_child_scales_against_its_parents_available_bounds() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(r#"panel { id = "bar", width = 1000, height = 500, child = rect { width = "50%" } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 500.0);
    }

    #[test]
    fn row_intrinsic_width_sums_children_plus_spacing_gaps() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { spacing = 5, children = { rect { width = 10, height = 8 }, rect { width = 10, height = 4 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.rect.width, 25.0, "10 + 10 + 5 spacing");
        assert_eq!(row.rect.height, 8.0, "max of children's heights");
    }

    #[test]
    fn column_intrinsic_height_sums_children_plus_spacing_gaps() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { spacing = 3, children = { rect { width = 6, height = 10 }, rect { width = 9, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.height, 23.0, "10 + 10 + 3 spacing");
        assert_eq!(column.rect.width, 9.0, "max of children's widths");
    }

    #[test]
    fn a_childs_own_margin_pushes_it_inward_and_widens_the_rows_footprint() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 10, height = 10, margin = { left = 4, right = 4 } } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.rect.width, 18.0, "10 + 4 + 4 margin");
        assert_eq!(row.children[0].rect.x, 4.0, "the child's own margin.left offsets it inward");
    }

    #[test]
    fn a_margined_row_child_pushes_its_sibling_apart_instead_of_overlapping() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10, margin = { right = 5 } },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[1].rect.x, 15.0, "10 (first child) + 5 (its margin.right) = 15, not overlapping at 10");
    }

    #[test]
    fn stretching_a_child_that_itself_has_children_repositions_its_descendants_too() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = row { height = "Fill", children = {
                rect { width = 20, align_v = "Stretch", children = {
                    rect { width = 6, height = 6, align_h = "Center", align_v = "Center" },
                } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let stretched = &row.children[0];
        assert_eq!(stretched.rect.height, 50.0, "stretched to the row's full height");
        let inner = &stretched.children[0];
        assert_eq!(
            inner.rect.y,
            (50.0 - 6.0) / 2.0,
            "centered against the stretched (post-fix) height, not the pre-stretch intrinsic height"
        );
    }

    /// A `Stretch` child of a `Content`-sized row: the solver closes this, not anything written
    /// here (ADR-0023).
    ///
    /// Same shape as the test above, minus the row's `height = "Fill"`, which is the whole
    /// difference: the row is now `Content`-sized, so its own height is not known until after its
    /// children resolve. The hand-written pass could not pre-force a `Stretch` child's size in that
    /// case, so it patched the child's own `rect` afterwards and left the child's descendants
    /// positioned against the pre-stretch height -- the grandchild centred in 6 rather than in 50.
    /// ADR-0023's upgrade path (g) named "a second constraint pass" as the fix. That is what a
    /// solver is.
    #[test]
    fn a_stretch_child_of_a_content_sized_row_repositions_its_descendants_too() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = row { children = {
                rect { width = 20, height = 50 },
                rect { width = 20, align_v = "Stretch", children = {
                    rect { width = 6, height = 6, align_h = "Center", align_v = "Center" },
                } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.rect.height, 50.0, "the row measures itself from the fixed sibling");
        let stretched = &row.children[1];
        assert_eq!(stretched.rect.height, 50.0, "and the stretched sibling takes that whole height");
        assert_eq!(
            stretched.children[0].rect.y,
            (50.0 - 6.0) / 2.0,
            "centered against the stretched height, which the old stacking model did not do"
        );
    }

    /// The one thing the solver swap narrows, pinned so it stays a decision rather than a surprise.
    ///
    /// An invisible node leaves the layout entirely, so its whole subtree resolves to zero geometry
    /// instead of being sized and then declined a position. Nothing outside this module can tell:
    /// `layout::paint`, `layout::hit` and `overlay_input_regions` all filter on `visible` before
    /// they read a rect, and the two readers that do look at a hidden node -- `layout::hover`, and
    /// `wayland::surface`'s `panel_spec` re-derive -- read `properties`, which is still here in
    /// full. What was always specified is the part that still holds: a hidden child reserves no
    /// space and no `spacing` gap, which the two tests above this pin.
    #[test]
    fn an_invisible_subtree_resolves_to_no_geometry_at_all() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = rect { width = 40, height = 30,
                visible = false, children = { rect { width = 10, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let hidden = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((hidden.rect.width, hidden.rect.height), (0.0, 0.0), "the hidden node itself");
        // A subtree hidden from the start was never built (ADR-0124): there is nothing under it
        // to have geometry until it is shown.
        assert!(hidden.children.is_empty(), "and nothing under it yet");
        assert_eq!(
            hidden.properties.get("width").map(|w| w.to_string().unwrap()),
            Some("40".to_string()),
            "its properties survive in full, which is what `panel_spec` and `hover` read"
        );
    }

    #[test]
    fn row_start_alignment_packs_children_at_the_beginning() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 10, height = 10 }, rect { width = 10, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, 0.0);
        assert_eq!(row.children[1].rect.x, 10.0);
    }

    #[test]
    fn row_end_alignment_packs_children_against_the_far_edge() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 20, child = row { width = "Fill", align_h = "End", children = { rect { width = 10, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, 90.0);
    }

    #[test]
    fn row_child_stretch_alignment_fills_the_cross_axis() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = row { height = "Fill", children = { rect { width = 10, height = 5, align_v = "Stretch" } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.height, 50.0);
    }

    #[test]
    fn stacking_container_aligns_each_child_independently_and_they_can_overlap() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 100, child = rect { width = "Fill", height = "Fill", children = {
                rect { width = 20, height = 20, align_h = "Start", align_v = "Start" },
                rect { width = 20, height = 20, align_h = "End", align_v = "End" },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let outer = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(outer.children[0].rect.x, 0.0);
        assert_eq!(outer.children[1].rect.x, 80.0);
        assert_eq!(outer.children[1].rect.y, 80.0);
    }

    #[test]
    fn an_invisible_child_does_not_consume_row_space() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10, visible = false },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.rect.width, 10.0, "the invisible child must not widen the row or add a spacing gap");
        assert_eq!(
            row.children[1].rect.x, 0.0,
            "the visible child packs at the start as if the hidden one weren't there"
        );
    }

    /// `width = "Fill"` shares the main axis with its siblings: in a 600px row a `Fill` child
    /// resolved against the whole content width would push its fixed sibling to x=600, outside
    /// the row.
    #[test]
    fn a_fill_child_takes_only_the_room_its_siblings_leave() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 500.0, "the fill child takes 600 less its sibling's 100");
        assert_eq!(row.children[1].rect.x, 500.0, "and its sibling lands inside the row, not past its edge");
    }

    #[test]
    fn two_fill_siblings_split_the_remainder_equally() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = "Fill", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((row.children[0].rect.width, row.children[1].rect.width), (300.0, 300.0));
        assert_eq!(row.children[1].rect.x, 300.0);
    }

    /// A share is a footprint, and the positioning advances by a footprint including
    /// margin. Forcing the share as the child's *size* instead would push every later sibling out
    /// by exactly the margin.
    #[test]
    fn a_fill_childs_margin_comes_out_of_its_own_share() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10, margin = 25 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 450.0, "500 of footprint less 25 of margin on each side");
        assert_eq!(row.children[1].rect.x, 500.0, "so the sibling still starts one footprint in");
    }

    /// The mirror of `a_fill_childs_margin_comes_out_of_its_own_share`, and the case that test
    /// missed: the margin on the *sibling* rather than on the `Fill` child. The positioning
    /// advances its cursor by a footprint that includes margin, so a remainder that counts only the
    /// sibling's box hands the `Fill` child exactly that margin too much and pushes it off the end.
    #[test]
    fn a_fixed_siblings_margin_is_counted_against_the_remainder() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = 100, height = 10, margin = 20 },
                rect { width = "Fill", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let fill = &row.children[1];
        assert_eq!(fill.rect.width, 460.0, "600 less the sibling's 100 box and its 40 of margin");
        assert_eq!(
            fill.rect.x + fill.rect.width,
            row.rect.width,
            "and the fill child ends exactly at the row's edge, not past it"
        );
    }

    #[test]
    fn spacing_is_reserved_before_a_fill_child_is_sized() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, spacing = 20, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 480.0, "600 less the sibling's 100 and the one 20px gap");
        assert_eq!(row.children[1].rect.x, 500.0);
    }

    #[test]
    fn a_column_fills_its_main_axis_the_same_way_a_row_does() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { width = 200, height = 600, children = {
                rect { height = "Fill", width = 10 },
                rect { height = 100, width = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.children[0].rect.height, 500.0);
        assert_eq!(column.children[1].rect.y, 500.0);
    }

    /// Clamped rather than negative, and the overflow stays visible. This is flexbox without
    /// `flex-shrink`: the engine will not silently shrink a size the config stated in pixels to
    /// make a `Fill` sibling fit.
    #[test]
    fn fixed_children_that_already_overflow_collapse_a_fill_sibling_to_nothing() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 100, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 300, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 0.0);
        assert_eq!(row.children[1].rect.width, 300.0, "the stated size is kept, not shrunk to fit");
    }

    /// An invisible sibling takes no space when positioned, so it must reserve none here
    /// either -- otherwise hiding a node would shrink the one beside it.
    #[test]
    fn an_invisible_sibling_reserves_nothing_from_a_fill_childs_share() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        for hidden in [
            r#"rect { width = 100, height = 10, visible = false }"#,
            r#"rect { width = "Fill", height = 10, visible = false }"#,
        ] {
            let mut scene_for_case = std::mem::replace(&mut scene, Scene::new());
            let (lua, surface) = surface_from(&format!(
                r#"panel {{ id = "bar", child = row {{ width = 600, height = 40, children = {{
                    rect {{ width = "Fill", height = 10 }}, {hidden},
                }} }} }}"#
            ));
            apply_at(&mut scene_for_case, &[surface], full(), &shaping, &lua).unwrap();
            let row = &scene_for_case.surface("bar@TEST").unwrap().children[0];
            assert_eq!(row.children[0].rect.width, 600.0, "the visible fill child takes the whole row");
        }
    }

    /// A row's *cross* axis hands every child the row's full height, because on that axis there is nothing to share.
    #[test]
    fn fill_on_a_rows_cross_axis_is_still_the_whole_row() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 200, children = {
                rect { width = 100, height = "Fill" },
                rect { width = 100, height = 50 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.height, 200.0);
    }

    /// A stacking parent has no main axis, its children
    /// may overlap by design (ADR-0023), and `Fill` there means the whole box.
    #[test]
    fn fill_under_a_stacking_parent_is_still_the_whole_box() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = rect { width = 600, height = 200, children = {
                rect { width = "Fill", height = 10 },
                rect { width = "Fill", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let stack = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((stack.children[0].rect.width, stack.children[1].rect.width), (600.0, 600.0));
        assert_eq!(stack.children[1].rect.x, 0.0, "stacked, not flowed");
    }

    /// A percentage still resolves against the parent, not against the remainder, which is what a
    /// CSS percentage width does. Deliberately left alone by this pass: changing it would be a
    /// second behaviour change riding along inside a bug fix.
    #[test]
    fn a_percentage_sibling_still_resolves_against_the_parent() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = "50%", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[1].rect.width, 300.0, "half of the parent, not half of what is left");
        assert_eq!(row.children[0].rect.width, 300.0, "and the fill child takes what that leaves");
    }

    /// Unchanged, and deliberate: a row that states no width has no remainder to divide, so a
    /// `Fill` child of it resolves to zero. See ADR-0077's own note on item 10
    /// for the one-pass reasoning behind it and the upgrade path.
    #[test]
    fn a_fill_child_of_a_content_sized_row_still_resolves_to_zero() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 0.0);
    }

    /// `min_width` shares a taffy field with the disabled automatic minimum above, so the two have
    /// to be checked together: a declared floor, content that already clears it, and no floor.
    #[test]
    fn a_min_width_floors_a_content_sized_row_without_widening_what_already_clears_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();

        let measure = |scene: &mut Scene, source: &str| {
            let (lua, surface) = surface_from(source);
            apply_at(scene, &[surface], full(), &shaping, &lua).unwrap();
            let width = scene.surface("bar@TEST").unwrap().children[0].rect.width;
            drop(lua);
            width
        };

        assert_eq!(
            measure(
                &mut scene,
                r#"panel { id = "bar", child = row { min_width = 220, children = {
                rect { width = 40, height = 10 },
            } } }"#
            ),
            220.0,
            "40 of content in a box floored at 220 is 220 wide"
        );
        assert_eq!(
            measure(
                &mut scene,
                r#"panel { id = "bar", child = row { min_width = 220, children = {
                rect { width = 400, height = 10 },
            } } }"#
            ),
            400.0,
            "content that already clears the floor is untouched by it"
        );
        assert_eq!(
            measure(
                &mut scene,
                r#"panel { id = "bar", child = row { children = {
                rect { width = 40, height = 10 },
            } } }"#
            ),
            40.0,
            "and no floor leaves the automatic minimum exactly as it was"
        );
    }

    /// A floor above a ceiling is the size, the way CSS resolves the pair, rather than a tree the
    /// engine refuses: the two are separate properties and nothing stops a config carrying both.
    #[test]
    fn a_min_width_above_a_max_width_wins_instead_of_being_refused() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { min_width = 300, max_width = 120, children = {
                rect { width = 40, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].rect.width, 300.0);
    }

    /// Found live: a content-sized `column` with 8px of padding reported the
    /// bare height of its one child, and reported the same height with 50px of padding. Padding
    /// insets the box children are laid out in (they are offset by exactly this
    /// much), so a content-sized container that does not also grow by it positions its children
    /// past its own edge.
    #[test]
    fn a_content_sized_container_grows_by_its_own_padding() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column {
                padding = { top = 8, right = 10, bottom = 8, left = 10 },
                children = { rect { width = 20, height = 20 } },
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.width, 40.0, "20 wide child plus 10 of padding on each side");
        assert_eq!(column.rect.height, 36.0, "20 tall child plus 8 of padding top and bottom");
        assert_eq!(column.children[0].rect.x, 10.0);
        assert_eq!(column.children[0].rect.y, 8.0);
    }

    /// The surface root resolves through the same `unwrap_or`, and a popup sized to its contents
    /// is the case this actually bites.
    #[test]
    fn padding_grows_a_content_sized_surface_too() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", padding = { top = 6, right = 6, bottom = 6, left = 6 },
                child = rect { width = 20, height = 20 } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert_eq!(root.rect.width, 32.0);
        assert_eq!(root.rect.height, 32.0);
    }

    /// Padding must not double-count on an axis whose size the config stated: there it correctly
    /// shrinks the child budget and leaves the parent alone.
    #[test]
    fn padding_does_not_grow_an_explicitly_sized_container() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column {
                width = 100,
                padding = { top = 8, right = 10, bottom = 8, left = 10 },
                children = { rect { width = 20, height = 20 } },
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.width, 100.0, "stated width wins, padding already inset the child");
        assert_eq!(column.rect.height, 36.0, "the Content axis still grows by its padding");
    }

    #[test]
    fn text_content_size_comes_from_a_real_shaping_round_trip() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = text { content = "Mantle" } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let text = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(text.rect.width > 0.0);
        assert_eq!(text.rect.height, 12.0 * 1.2, "default font_size 12 * the 1.2 line-height multiplier");
    }

    /// taffy 0.14 adds a flex container's own margin to its children's minimum cross size when it
    /// measures them (see [`taffy_style`]'s `min_size`). The card here is a column
    /// with a left margin of most of the output, holding a body that wraps at the card's width.
    /// Its height has to be the wrapped body's, whatever the margin.
    #[test]
    fn a_containers_own_margin_does_not_widen_what_its_children_are_measured_at() {
        let long = "have a look at this: https://example.com/project/mantle/pull/12345 and tell me what you think about it all";
        let heights = |margin: u32| {
            let src = format!(
                r#"panel {{ id = "bar", child = column {{ width = "Fill", height = "Fill", children = {{
                column {{ width = 392, margin = {{ left = {margin}, top = 4 }}, padding = {{ top = 7, right = 7, bottom = 7, left = 7 }}, children = {{
                    column {{ width = "Fill", children = {{
                        text {{ content = "Sender", font_size = 14 }},
                        text {{ content = "{long}", font_size = 12, wrap = "Word", width = "Fill" }},
                    }} }},
                }} }},
            }} }} }}"#
            );
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&src);
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let card = &scene.surface("bar@TEST").unwrap().children[0].children[0];
            let inner = &card.children[0];
            (card.rect.height, inner.rect.height, inner.children[0].rect.height + inner.children[1].rect.height)
        };
        let (card, inner, lines) = heights(1521);
        assert_eq!(inner, lines, "the column is as tall as its two texts, one of them wrapped");
        assert_eq!(card, inner + 14.0, "and the card is that plus its padding");
        assert_eq!((card, inner), (heights(0).0, heights(0).1), "the margin moves the card, it does not resize it");
    }

    #[test]
    fn font_size_change_invalidates_text_memo() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = text { content = "Hello World", font_size = state("fs", 12) } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let width_12 = scene.surface("bar@TEST").unwrap().children[0].rect.width;

        lua.load(r#"state("fs", 12):set(24)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let width_24 = scene.surface("bar@TEST").unwrap().children[0].rect.width;

        assert!(width_24 > width_12 * 1.5, "larger font_size must produce larger box: {width_12} vs {width_24}");
    }
}

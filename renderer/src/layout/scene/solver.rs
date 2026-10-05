use super::{LayoutStyle, LogicalSize, ResolvedNode};
use crate::layout::node::{self, Align, LayoutError, PaintStyle, PropMap, SizeMode};
use crate::text::shaping::{self, ShapingHandle};
use taffy::TraversePartialTree;
use taffy::prelude::{length, line, span, zero};

/// A solver tree, one per surface instance, kept across passes and ticks so a node that did not
/// change keeps taffy's layout cache (ADR-0294). Geometry stays fractional until `text::snap`
/// applies the surface scale at paint; taffy otherwise rounds layouts to whole numbers.
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
#[derive(PartialEq)]
pub(super) enum Measure {
    /// Shaped extent; `wrap` and `max_lines` change geometry, not only paint.
    Text {
        content: std::sync::Arc<str>,
        runs: Vec<shaping::FontRun>,
        /// The face the box is measured against, so the reserved width is the one it paints into.
        face: node::Typeface,
        wrap: node::Wrap,
        max_lines: Option<usize>,
        /// The last `(max_width, size)` this node measured; see [`solve`].
        memo: Option<(Option<f32>, taffy::Size<f32>)>,
    },
    /// `icon`'s `size`, the same number on both axes.
    Square(f32),
    /// A `textfield`'s one text line: height only, since its width is the config's to give.
    Line(f32),
    /// Marks a `homogeneous` container: [`fit_slots`] sizes its slot track before each solve.
    /// `content` is whether its main size is the content's; only then, or under `wrap`, does the slot decide.
    Slots { axis: MainAxis, content: bool, wrap: bool },
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

fn content_sized(style: &LayoutStyle, axis: MainAxis) -> bool {
    match axis {
        MainAxis::Horizontal => style.width_mode == SizeMode::Content,
        MainAxis::Vertical => style.height_mode == SizeMode::Content,
    }
}

/// Whether a flow container wraps; read here because `wrap` is also a `text` property of another type.
fn wraps(properties: &PropMap) -> Result<bool, LayoutError> {
    node::fields::flow_layout::wrap.read(properties)
}

/// A `homogeneous` container's tracks: `1fr` each (at least `slot` when content-sized), or fixed `slot` cells when wrapped.
fn set_slot(out: &mut taffy::Style, axis: MainAxis, slot: f32, content: bool, wrap: bool) {
    use taffy::style_helpers::{fr, minmax, repeat};
    let track = || {
        if content { minmax(length(slot), fr(1.0)) } else { minmax(length(0.0), fr(1.0)) }
    };
    let cells = || vec![repeat(taffy::RepetitionCount::AutoFill, vec![minmax(length(slot), length(slot))])];
    // Items fill along the main axis first: columns for a row, rows for a column.
    (out.grid_auto_flow, (out.grid_template_columns, out.grid_template_rows)) = match (axis, wrap) {
        (MainAxis::Horizontal, true) => (taffy::GridAutoFlow::Row, (cells(), Vec::new())),
        (MainAxis::Vertical, true) => (taffy::GridAutoFlow::Column, (Vec::new(), cells())),
        (MainAxis::Horizontal, false) => (taffy::GridAutoFlow::Column, (Vec::new(), Vec::new())),
        (MainAxis::Vertical, false) => (taffy::GridAutoFlow::Row, (Vec::new(), Vec::new())),
    };
    match axis {
        MainAxis::Horizontal if !wrap => out.grid_auto_columns = vec![track()],
        MainAxis::Vertical if !wrap => out.grid_auto_rows = vec![track()],
        _ => {}
    }
}

/// One node's taffy style, combining its container and item roles. `parent_axis` decides whether a
/// `Fill` shares a flow remainder or takes the whole slot; `None` means stacking or surface root.
pub(super) fn taffy_style(
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
        // TODO(DioxusLabs/taffy#1081): give the cross axis a zero `min_size` too once that PR ships.
        // taffy 0.14 adds the container's margin to each child's cross minimum (`constants.margin`
        // for `child.margin` in `determine_flex_base_size`); `Some(0) + margin` floors it, `None +
        // margin` does not (ADR-0111).
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
            let (main, cross) = match axis {
                MainAxis::Horizontal => (style.align_h, style.align_v),
                MainAxis::Vertical => (style.align_v, style.align_h),
            };
            let wrap = wraps(properties)?;
            // Adjacent-child spacing is a flex gap; set both axes because there is one flex line.
            out.gap = taffy::Size { width: length(style.spacing), height: length(style.spacing) };
            if wrap {
                out.flex_wrap = taffy::FlexWrap::Wrap;
                out.gap = match axis {
                    MainAxis::Horizontal => {
                        taffy::Size { width: length(style.spacing), height: length(style.line_spacing) }
                    }
                    MainAxis::Vertical => {
                        taffy::Size { width: length(style.line_spacing), height: length(style.spacing) }
                    }
                };
                if properties.contains_key("scroll") {
                    return Err(node::invalid("wrap", "`wrap` cannot scroll: remove `scroll` or `wrap`"));
                }
            }
            // `main` packs a line and `cross` the lines, unset (stretch) without `wrap`.
            let (main, cross) = (Some(main_align(main)), wrap.then(|| main_align(cross)));
            (out.justify_content, out.align_content) = (main, cross);
            if style.homogeneous {
                out.display = taffy::Display::Grid;
                set_slot(&mut out, axis, 0.0, content_sized(style, axis), wrap);
                // A grid's `justify_*` is always horizontal.
                if axis == MainAxis::Vertical {
                    (out.justify_content, out.align_content) = (cross, main);
                }
            }
        }
        // ADR-0023's stacking model is one auto-sized grid cell: children overlap and align
        // independently, while `Content` is their bounding union. `minmax(0, auto)` keeps the cell
        // at the content box when a child is larger; an `auto` minimum would grow to that child.
        // Not `1fr`: taffy 0.14 leaves item margins out of its max-content size (DioxusLabs/taffy#1177).
        None => {
            out.display = taffy::Display::Grid;
            // An auto-size, uncapped axis floors the track at the children's min-content (CSS
            // fit-content); a fixed or capped one keeps `0` so the cell stays the box.
            let cell = |floor: bool| {
                let min = if floor { taffy::MinTrackSizingFunction::min_content() } else { zero() };
                vec![taffy::style_helpers::minmax(min, taffy::MaxTrackSizingFunction::auto())]
            };
            out.grid_template_columns = cell(style.width_mode == SizeMode::Content && style.max_width.is_none());
            out.grid_template_rows = cell(style.height_mode == SizeMode::Content && style.max_height.is_none());
        }
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

/// [`new_solver_node`] for a node the cached tree already holds, writing only what changed: a
/// write dirties the node and its ancestors, and a pass that dirtied nothing is a pass taffy
/// answers from its cache.
///
/// A parentless node is a surface root, whose size [`solve_instance`] patches in afterwards;
/// compared here, that patch would dirty every such root on every pass. A `text` measured from the
/// same inputs keeps the context it has, whose memo is the size the solver last got for them.
///
/// [`solve_instance`]: super::pass::solve_instance
// ponytail: an unchanged node still rebuilds and compares its taffy style and measure, about
// 0.3 us a node (ADR-0294). Upgrade: skip nodes the resolve kept whose parent kept its axis.
pub(super) fn update_solver_node(
    tree: &mut taffy::TaffyTree<Measure>,
    id: taffy::NodeId,
    kind: &str,
    properties: &PropMap,
    style: &LayoutStyle,
    parent_axis: Option<MainAxis>,
    mut measure: Option<Measure>,
) -> Result<(), LayoutError> {
    let mut solver_style = taffy_style(kind, properties, style, parent_axis)?;
    let current = tree.style(id).map_err(taffy_failed)?;
    if tree.parent(id).is_none() {
        solver_style.size = current.size;
    }
    // The slot [`fit_slots`] wrote is not rebuilt here; taking it back would dirty the node every pass.
    if let Some(Measure::Slots { wrap, .. }) = measure {
        if wrap {
            solver_style.grid_template_columns.clone_from(&current.grid_template_columns);
            solver_style.grid_template_rows.clone_from(&current.grid_template_rows);
        } else {
            solver_style.grid_auto_columns.clone_from(&current.grid_auto_columns);
            solver_style.grid_auto_rows.clone_from(&current.grid_auto_rows);
        }
    }
    if *current != solver_style {
        tree.set_style(id, solver_style).map_err(taffy_failed)?;
    }
    // Compared with the kept memo swapped in, so only the inputs decide; any other change drops it.
    if let (Some(Measure::Text { memo: kept, .. }), Some(Measure::Text { memo, .. })) =
        (tree.get_node_context(id), measure.as_mut())
    {
        *memo = *kept;
    }
    if tree.get_node_context(id) != measure.as_ref() {
        if let Some(Measure::Text { memo, .. }) = measure.as_mut() {
            *memo = None;
        }
        tree.set_node_context(id, measure).map_err(taffy_failed)?;
    }
    Ok(())
}

/// Sets `parent`'s solver children unless it already has exactly these.
pub(super) fn set_solver_children(
    tree: &mut taffy::TaffyTree<Measure>,
    parent: taffy::NodeId,
    children: &[taffy::NodeId],
) -> Result<(), LayoutError> {
    if !tree.child_ids(parent).eq(children.iter().copied()) {
        tree.set_children(parent, children).map_err(taffy_failed)?;
    }
    Ok(())
}

/// Takes `node`'s subtree out of the cached solver tree, for a node the scene drops or sends
/// leaving: out of the solver for good, so its ids go with it.
pub(super) fn release_solver_nodes(tree: &mut taffy::TaffyTree<Measure>, node: &mut ResolvedNode) {
    if let Some(id) = node.taffy.take() {
        // Ignored: the id is this tree's own, so `remove` cannot miss.
        let _ = tree.remove(id);
    }
    node.children.iter_mut().for_each(|child| release_solver_nodes(tree, child));
}

/// Clears every solver id under `node`, for a retained tree about to be laid out in a new solver
/// tree: the ids it holds name nodes of one that is gone, and one could name a live node of the
/// new one. Recursive because a hidden node's frozen children keep theirs until they thaw.
pub(super) fn forget_solver_nodes(node: &mut ResolvedNode) {
    node.taffy = None;
    node.children.iter_mut().for_each(forget_solver_nodes);
}

/// An unallocated content axis grows to span its leavers' last rects (ADR-0150): they take no room
/// in the flow, but a parent collapsing under a lone leaver would clip its exit away. Allocated,
/// fixed and `Fill` axes already have their own bounds.
///
/// Its own frame, like [`new_solver_node`], for the `taffy::Style` it clones.
pub(super) fn hold_leavers(
    tree: &mut taffy::TaffyTree<Measure>,
    id: taffy::NodeId,
    style: &LayoutStyle,
    allocated_axes: (bool, bool),
    leaving: &[super::ResolvedNode],
) -> Result<(), LayoutError> {
    let content = (
        style.width_mode == SizeMode::Content && !allocated_axes.0,
        style.height_mode == SizeMode::Content && !allocated_axes.1,
    );
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

/// What the solver asks a leaf for its size with, for the kinds whose size is their content.
pub(super) fn measure_for(
    kind: &str,
    paint: Option<&PaintStyle>,
    properties: &PropMap,
    style: &LayoutStyle,
) -> Result<Option<Measure>, LayoutError> {
    let kind = flow_kind(kind, properties)?;
    if let Some(axis) = main_axis_of(kind, properties)?
        && style.homogeneous
    {
        return Ok(Some(Measure::Slots { axis, content: content_sized(style, axis), wrap: wraps(properties)? }));
    }
    Ok(match kind {
        // `node::paint_style` gives every `text` a `PaintStyle::Text` and `flow_kind` cannot route
        // another kind here, so the arm is total, the same shape as `pass::children_of`'s
        // `unreachable!` arm.
        "text" => {
            let Some(PaintStyle::Text { content, runs, face, wrap, max_lines, .. }) = paint else {
                unreachable!("paint_style produces PaintStyle::Text for text nodes");
            };
            Some(Measure::Text {
                content: content.clone(),
                runs: node::font_runs(runs),
                face: face.clone(),
                wrap: *wrap,
                max_lines: *max_lines,
                memo: None,
            })
        }
        "icon" => Some(Measure::Square(node::fields::icon::size.read(properties)?)),
        "textfield" => {
            let Some(PaintStyle::TextField { face, .. }) = paint else {
                unreachable!("paint_style produces PaintStyle::TextField for textfield nodes");
            };
            Some(Measure::Line(face.line_height))
        }
        // `image` has no intrinsic size, unlike `icon`: knowing a file's own dimensions means
        // decoding it, and this pass has no canvas to decode against and runs on every
        // `Scene::apply`. So an `image` takes the box `width`/`height` give it, measuring
        // nothing without one, the same as an empty `rect`.
        _ => None,
    })
}

/// taffy's own errors are all "you handed me a node id I do not have", which the scene pass cannot do:
/// every id comes from the tree it is used against, and `forget_solver_nodes` clears any other. So
/// this is the `unreachable!` equivalent for a `Result` that has to be handled anyway, reported as
/// a pass failure rather than a panic on the Wayland dispatch thread.
pub(super) fn taffy_failed(err: taffy::TaffyError) -> LayoutError {
    node::invalid("layout", format!("the layout solver refused a node built by this pass: {err}"))
}

/// The measure callback of [`solve`] and [`fit_slots`].
fn measure_leaf(
    shaping: &ShapingHandle,
    input: taffy::LayoutInput,
    context: Option<&mut Measure>,
    style: &taffy::Style,
) -> taffy::LayoutOutput {
    // TODO(DioxusLabs/taffy#1166): remove `ceiling` and `clamp` once that PR ships in a release.
    // taffy 0.14 lets a `flex_shrink = 0` item's flex basis beat its own max size in an
    // intrinsic contribution (css-flexbox §9.9.3 clamps by min/max last), so a leaf measured
    // past `max_width` widened a content-sized row. Answering at the ceiling keeps the basis
    // under it. Ceilings are border-box pixels; the measure answers for the content box inside.
    let inset = |start: taffy::LengthPercentage, end: taffy::LengthPercentage| {
        start.into_raw().value() + end.into_raw().value()
    };
    let ceiling = taffy::Size {
        width: style
            .max_size
            .width
            .resolve_to_option(0.0, |_, _| 0.0)
            .map(|max| (max - inset(style.padding.left, style.padding.right)).max(0.0)),
        height: style
            .max_size
            .height
            .resolve_to_option(0.0, |_, _| 0.0)
            .map(|max| (max - inset(style.padding.top, style.padding.bottom)).max(0.0)),
    };
    let clamp = |size: taffy::Size<f32>| taffy::Size {
        width: ceiling.width.map_or(size.width, |max| size.width.min(max)),
        height: ceiling.height.map_or(size.height, |max| size.height.min(max)),
    };
    taffy::compute_leaf_layout(
        input,
        style,
        |_, _| 0.0,
        |known, offered| {
            let Some(measure) = context else {
                return taffy::Size::ZERO;
            };
            match measure {
                Measure::Square(size) => clamp(taffy::Size { width: *size, height: *size }),
                Measure::Line(height) => clamp(taffy::Size { width: 0.0, height: *height }),
                Measure::Slots { .. } => taffy::Size::ZERO,
                Measure::Text { content, runs, face, wrap, max_lines, memo } => {
                    // The wrap width: the known width, else the one on offer; `None` when unwrapped or taffy asks
                    // for max-content, and 0 for min-content, which breaks at every chance: the longest word.
                    let max_width = match wrap {
                        node::Wrap::None => None,
                        node::Wrap::Word => known
                            .width
                            .or(match offered.width {
                                taffy::AvailableSpace::Definite(width) => Some(width),
                                taffy::AvailableSpace::MinContent => Some(0.0),
                                taffy::AvailableSpace::MaxContent => None,
                            })
                            .map(|width| ceiling.width.map_or(width, |max| width.min(max)))
                            .or(ceiling.width),
                    };
                    if let Some((key, size)) = memo
                        && *key == max_width
                    {
                        return clamp(*size);
                    }
                    let shaped = shaping.shape(face.request(content.to_string(), max_width, runs.clone()));
                    let lines = max_lines.map_or(shaped.lines.len(), |cap| shaped.lines.len().min(cap));
                    let size = taffy::Size { width: shaped.width, height: lines as f32 * face.line_height };
                    *memo = Some((max_width, size));
                    clamp(size)
                }
            }
        },
    )
}

/// Sets each content-sized or wrapping `homogeneous` slot to its widest child's margin box, innermost first.
// ponytail: walks the whole tree each dirty solve; upgrade: collect the `Measure::Slots` ids in [`update_solver_node`].
fn fit_slots(
    tree: &mut taffy::TaffyTree<Measure>,
    node: taffy::NodeId,
    shaping: &ShapingHandle,
) -> Result<(), LayoutError> {
    for child in tree.children(node).map_err(taffy_failed)? {
        fit_slots(tree, child, shaping)?;
    }
    let Some(&Measure::Slots { axis, content, wrap }) = tree.get_node_context(node) else {
        return Ok(());
    };
    if !content && !wrap {
        return Ok(());
    }
    let mut slot = 0.0_f32;
    for child in tree.children(node).map_err(taffy_failed)? {
        if tree.style(child).map_err(taffy_failed)?.display == taffy::Display::None {
            continue;
        }
        let space = taffy::Size { width: taffy::AvailableSpace::MaxContent, height: taffy::AvailableSpace::MaxContent };
        tree.compute_layout_with_measure(child, space, |input, _node, context, style| {
            measure_leaf(shaping, input, context, style)
        })
        .map_err(taffy_failed)?;
        let size = tree.layout(child).map_err(taffy_failed)?.size;
        // Measuring as a root left it at the origin and cached; dirty so the real solve re-places it.
        tree.mark_dirty(child).map_err(taffy_failed)?;
        let margin = tree.style(child).map_err(taffy_failed)?.margin;
        let (extent, margin) = match axis {
            MainAxis::Horizontal => (size.width, margin.left.into_raw().value() + margin.right.into_raw().value()),
            MainAxis::Vertical => (size.height, margin.top.into_raw().value() + margin.bottom.into_raw().value()),
        };
        slot = slot.max(extent + margin);
    }
    write_slot(tree, node, (axis, content, wrap), slot)
}

/// Its own frame, for the `taffy::Style` it clones, as [`hold_leavers`].
fn write_slot(
    tree: &mut taffy::TaffyTree<Measure>,
    node: taffy::NodeId,
    (axis, content, wrap): (MainAxis, bool, bool),
    slot: f32,
) -> Result<(), LayoutError> {
    let mut style = tree.style(node).map_err(taffy_failed)?.clone();
    set_slot(&mut style, axis, slot, content, wrap);
    if *tree.style(node).map_err(taffy_failed)? != style {
        tree.set_style(node, style).map_err(taffy_failed)?;
    }
    Ok(())
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
/// falls back on that memo, and [`update_solver_node`] replaces the context, memo and all, once an
/// input changes. A `NaN` width never equals itself, so it re-measures rather than going stale.
pub(super) fn solve(
    tree: &mut taffy::TaffyTree<Measure>,
    root: taffy::NodeId,
    available: LogicalSize,
    shaping: &ShapingHandle,
) -> Result<(), LayoutError> {
    #[cfg(test)]
    if tree.dirty(root).map_err(taffy_failed)? {
        tests::SOLVES.set(tests::SOLVES.get() + 1);
    }
    let space = taffy::Size {
        width: taffy::AvailableSpace::Definite(available.width),
        height: taffy::AvailableSpace::Definite(available.height),
    };
    if tree.dirty(root).map_err(taffy_failed)? {
        fit_slots(tree, root, shaping)?;
    }
    tree.compute_layout_with_measure(root, space, |input, _node, context, style| {
        measure_leaf(shaping, input, context, style)
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
pub(super) mod tests {
    use super::*;
    use crate::layout::scene::tests::{apply_at, full, surface_from};
    use crate::layout::scene::*;

    thread_local! {
        /// Solves on this thread that found something to lay out: [`solve`] on a clean tree is
        /// taffy's cache answering at the root.
        pub(in crate::layout::scene) static SOLVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

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
        assert_eq!(flow_kind("list", &direction_props(&lua, Some("vertical"))).unwrap(), "column");
    }

    #[test]
    fn a_horizontal_list_lays_out_as_a_row() {
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("list", &direction_props(&lua, Some("horizontal"))).unwrap(), "row");
    }

    #[test]
    fn direction_on_anything_that_is_not_a_list_is_ignored_rather_than_obeyed() {
        // `row` and `column` already say which way they go in their own name, so a `direction` on
        // one is a config confusing itself, not a second way to spell the kind.
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("column", &direction_props(&lua, Some("horizontal"))).unwrap(), "column");
        assert_eq!(flow_kind("row", &direction_props(&lua, Some("vertical"))).unwrap(), "row");
    }

    #[test]
    fn an_unknown_direction_is_refused_by_name() {
        let lua = mlua::Lua::new();
        let err = flow_kind("list", &direction_props(&lua, Some("sideways"))).unwrap_err().to_string();
        assert!(err.contains("sideways"), "the message has to name what was written: {err}");
        assert!(err.contains("horizontal"), "and what was expected: {err}");
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
    fn a_textfield_without_a_height_is_one_line_tall() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { textfield { font_size = 20, width = 80, on_change = function() end }, textfield { height = 7, on_change = function() end } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let fields = &scene.surface("bar@TEST").unwrap().children[0].children;
        assert_eq!((fields[0].rect.width, fields[0].rect.height), (80.0, 20.0 * 1.2));
        assert_eq!(fields[1].rect.height, 7.0, "an explicit height still wins");
    }

    #[test]
    fn a_fill_child_takes_its_parents_available_bounds() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = 1000, height = 500, child = rect { width = "fill", height = "fill" } }"#,
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

    /// A content-sized parent's box counts its child's margin box, as CSS does, so the margined
    /// child is not cut by the parent's clip.
    #[test]
    fn a_content_sized_parent_counts_its_childs_margin() {
        let margin = "margin = { left = 12, right = 3, top = 5, bottom = 7 }";
        let children = [
            format!("rect {{ width = 10, height = 10, {margin} }}"),
            format!("text {{ content = 'x', {margin} }}"),
            format!("rect {{ {margin}, children = {{ rect {{ width = 10, height = 10 }} }} }}"),
        ];
        for parent in ["rect", "row", "column"] {
            for child in &children {
                let mut scene = Scene::new();
                let shaping = ShapingHandle::spawn();
                let (lua, surface) =
                    surface_from(&format!("panel {{ id = 'bar', child = {parent} {{ children = {{ {child} }} }} }}"));
                apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
                let outer = &scene.surface("bar@TEST").unwrap().children[0];
                let inner = &outer.children[0];
                assert!(inner.rect.width > 0.0 && inner.rect.height > 0.0, "{parent} > {child}");
                assert_eq!((inner.rect.x, inner.rect.y), (12.0, 5.0), "{parent} > {child}: offset by the margin");
                assert_eq!(
                    (outer.rect.width, outer.rect.height),
                    (inner.rect.width + 15.0, inner.rect.height + 12.0),
                    "{parent} > {child}: the parent holds the child's margin box"
                );
            }
        }
    }

    /// The stack track was `minmax(0, 1fr)`; for one track `minmax(0, auto)` must size a definite
    /// parent's children the same, whatever the content.
    #[test]
    fn a_definite_stack_parent_sizes_its_children_as_it_did_with_a_fr_track() {
        let cases = [
            ("rect { width = 50, height = 20 }", (50.0, 20.0)),
            ("rect { width = 50, height = 20, align_h = 'stretch', align_v = 'stretch' }", (200.0, 100.0)),
            ("rect { width = 'fill', height = 'fill' }", (200.0, 100.0)),
            ("rect { width = '50%', height = '25%' }", (100.0, 25.0)),
            ("rect { width = 300, height = 150 }", (300.0, 150.0)),
        ];
        for (child, (width, height)) in cases {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&format!(
                "panel {{ id = 'bar', child = rect {{ width = 200, height = 100, children = {{ {child} }} }} }}"
            ));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let outer = &scene.surface("bar@TEST").unwrap().children[0];
            assert_eq!((outer.rect.width, outer.rect.height), (200.0, 100.0), "{child}");
            let inner = &outer.children[0];
            assert_eq!((inner.rect.width, inner.rect.height), (width, height), "{child}");
        }
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
            r#"panel { id = "bar", width = 100, height = 50, child = row { height = "fill", children = {
                rect { width = 20, align_v = "stretch", children = {
                    rect { width = 6, height = 6, align_h = "center", align_v = "center" },
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
    /// Same shape as the test above, minus the row's `height = "fill"`, which is the whole
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
                rect { width = 20, align_v = "stretch", children = {
                    rect { width = 6, height = 6, align_h = "center", align_v = "center" },
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
            r#"panel { id = "bar", width = 100, height = 20, child = row { width = "fill", align_h = "end", children = { rect { width = 10, height = 10 } } } }"#,
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
            r#"panel { id = "bar", width = 100, height = 50, child = row { height = "fill", children = { rect { width = 10, height = 5, align_v = "stretch" } } } }"#,
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
            r#"panel { id = "bar", width = 100, height = 100, child = rect { width = "fill", height = "fill", children = {
                rect { width = 20, height = 20, align_h = "start", align_v = "start" },
                rect { width = 20, height = 20, align_h = "end", align_v = "end" },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let outer = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(outer.children[0].rect.x, 0.0);
        assert_eq!(outer.children[1].rect.x, 80.0);
        assert_eq!(outer.children[1].rect.y, 80.0);
    }

    /// The shared cell is the parent's content box, not its largest child: a child wider than a
    /// fixed or capped parent overflows alone and does not drag its siblings' alignment out.
    #[test]
    fn a_stack_slot_stays_the_parents_size_when_a_child_is_larger() {
        for parent in ["rect { width = 50, height = 50", "rect { max_width = 50"] {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&format!(
                "panel {{ id = 'bar', child = {parent}, children = {{
                    rect {{ width = 20, height = 20, align_h = 'center' }},
                    rect {{ width = 80, height = 80 }},
                }} }} }}"
            ));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let stack = &scene.surface("bar@TEST").unwrap().children[0];
            assert_eq!(stack.children[0].rect.x, 15.0, "{parent}: centred in 50, not in the 80 child");
        }
    }

    /// CSS fit-content: an auto-size stack is at least its children's min-content, so a fixed child
    /// wider than a narrow parent centres by overflowing both sides, wrapper or not.
    #[test]
    fn an_auto_stack_is_no_narrower_than_its_fixed_child_in_a_narrow_parent() {
        for wrapped in [true, false] {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let leaf = "rect { width = 230, height = 20 }";
            let inner = if wrapped { format!("rect {{ children = {{ {leaf} }} }}") } else { leaf.to_string() };
            let (lua, surface) = surface_from(&format!(
                "panel {{ id = 'bar', child = rect {{ width = 40, height = 40, margin = {{ left = 300 }}, children = {{
                    column {{ align_h = 'center', children = {{ {inner} }} }},
                }} }} }}"
            ));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let outer = &scene.surface("bar@TEST").unwrap().children[0];
            let column = &outer.children[0];
            assert_eq!(column.children[0].rect.width, 230.0, "wrapped = {wrapped}");
            assert_eq!(outer.rect.x + column.rect.x, 205.0, "wrapped = {wrapped}: centred on the 40 box at 300..340");
        }
    }

    /// A wrapping text's min-content is its longest word: in a narrow parent an auto wrapper is that
    /// wide, no narrower, and the text still wraps at the wrapper's width.
    #[test]
    fn wrapping_text_in_an_auto_stack_keeps_its_longest_word_whole() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let word = "supercalifragilisticexpialidocious";
        let (lua, surface) = surface_from(&format!(
            "panel {{ id = 'bar', child = rect {{ width = 40, height = 100, children = {{
                rect {{ children = {{ text {{ content = 'a {word} b', font_size = 12, wrap = 'word' }} }} }},
                text {{ content = '{word}', font_size = 12 }},
            }} }} }}"
        ));
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let outer = &scene.surface("bar@TEST").unwrap().children[0];
        let (wrapper, lone) = (&outer.children[0], &outer.children[1]);
        assert!(lone.rect.width > 40.0);
        assert_eq!(wrapper.rect.width, lone.rect.width, "as wide as the word, not squeezed to the 40 parent");
        assert!(wrapper.children[0].rect.height > 2.0 * 12.0, "the phrase wraps around the word");
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

    /// `width = "fill"` shares the main axis with its siblings: in a 600px row a `Fill` child
    /// resolved against the whole content width would push its fixed sibling to x=600, outside
    /// the row.
    #[test]
    fn a_fill_child_takes_only_the_room_its_siblings_leave() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "fill", height = 10 },
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
                rect { width = "fill", height = 10 },
                rect { width = "fill", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((row.children[0].rect.width, row.children[1].rect.width), (300.0, 300.0));
        assert_eq!(row.children[1].rect.x, 300.0);
    }

    fn bar(source: &str) -> (Scene, mlua::Lua, ShapingHandle, VirtualNode) {
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(source);
        let mut scene = Scene::new();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        (scene, lua, shaping, surface)
    }

    fn flow_of(scene: &Scene) -> &ResolvedNode {
        &scene.surface("bar@TEST").unwrap().children[0]
    }

    /// A segmented control: labels of different widths share the widest one's slot, its margins
    /// included, and a content-sized row is that many slots wide.
    #[test]
    fn a_content_sized_homogeneous_row_gives_every_child_the_widest_slot() {
        let alone = |text: &str, margin: &str| {
            let (scene, ..) = bar(&format!(
                "panel {{ id = 'bar', child = row {{ children = {{ text {{ content = '{text}', {margin} }} }} }} }}"
            ));
            flow_of(&scene).children[0].rect.width
        };
        let (short, long) = (alone("a", ""), alone("mmmmmmmm", ""));
        assert!(short + 20.0 < long, "fixture: the margined label is still narrower than the long one");
        let (scene, ..) = bar("panel { id = 'bar', child = row { homogeneous = true, spacing = 4, children = {
                text { content = 'a', margin = { left = 10, right = 10 } },
                text { content = 'mmmmmmmm' },
                text { content = 'a' },
            } } }");
        let row = flow_of(&scene);
        assert!((row.rect.width - (3.0 * long + 8.0)).abs() < 0.01, "three slots of the widest label and two gaps");
        let widths: Vec<f32> = row.children.iter().map(|c| c.rect.width).collect();
        for (got, want) in widths.iter().zip([long - 20.0, long, long]) {
            assert!((got - want).abs() < 0.01, "each fills its slot less its own margins: {widths:?}");
        }
        assert_eq!(row.children[0].rect.x, 10.0);
        assert!((row.children[1].rect.x - (long + 4.0)).abs() < 0.01);
        assert!((row.children[2].rect.x - 2.0 * (long + 4.0)).abs() < 0.01);
    }

    /// The widest margin box decides, not the widest child: taffy's `1fr` alone drops margins.
    #[test]
    fn a_homogeneous_slot_counts_the_largest_margin_box() {
        let (scene, ..) = bar("panel { id = 'bar', child = column { homogeneous = true, children = {
                rect { height = 10, width = 5 },
                rect { height = 10, width = 5, margin = { top = 7, bottom = 9 } },
            } } }");
        let column = flow_of(&scene);
        assert_eq!(column.rect.height, 52.0, "two slots of 10 + 7 + 9");
        assert_eq!(column.children[1].rect.y, 26.0 + 7.0);
    }

    /// A sized container shares its axis equally whatever the content, like `"fill"` children; a
    /// child with a pixel size keeps it at the start of its slot.
    #[test]
    fn a_sized_homogeneous_row_splits_its_width_equally() {
        let (scene, ..) = bar(
            "panel { id = 'bar', child = row { homogeneous = true, width = 310, height = 20, spacing = 5, children = {
                text { content = 'mmmmmmmmmmmmmmmmmmmm' },
                rect { width = 'fill', height = 10 },
                rect { width = 30, height = 10 },
            } } }",
        );
        let row = flow_of(&scene);
        let [a, b, c] = [&row.children[0], &row.children[1], &row.children[2]];
        assert_eq!((a.rect.x, a.rect.width), (0.0, 100.0), "narrower than its text, so the text is cut");
        assert_eq!((b.rect.x, b.rect.width), (105.0, 100.0));
        assert_eq!((c.rect.x, c.rect.width), (210.0, 30.0));
    }

    /// The slot track is written by the solve, so a pass that changes a label rewrites it and a
    /// pass that changes nothing rebuilds no style and lays nothing out.
    #[test]
    fn a_homogeneous_slot_follows_its_content_and_stays_put_when_nothing_changes() {
        let (mut scene, lua, shaping, surface) = bar("label = state('label', 'a')
            return panel { id = 'bar', child = row { homogeneous = true, children = {
                text { content = label }, text { content = 'a' } } } }");
        let width = |scene: &Scene| flow_of(scene).rect.width;
        let before = width(&scene);
        SOLVES.set(0);
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!((SOLVES.get(), width(&scene)), (0, before));
        lua.load("label:set('mmmmmmmm')").exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(width(&scene) > before * 2.0, "both slots grew to the longer label");
    }

    /// An unrelated change re-solving around a slot container must not leave its measured children at the origin.
    #[test]
    fn an_unrelated_change_keeps_homogeneous_children_in_their_slots() {
        let (mut scene, lua, shaping, surface) = bar("label = state('label', 'a')
            return panel { id = 'bar', child = column { children = {
                text { content = label },
                row { homogeneous = true, children = { text { content = 'a' }, text { content = 'mmmm' } } },
            } } }");
        let xs = |scene: &Scene| flow_of(scene).children[1].children.iter().map(|c| c.rect.x).collect::<Vec<_>>();
        let before = xs(&scene);
        assert!(before[1] > 0.0, "fixture: the second slot is not at the origin");
        lua.load("label:set('mmmmmmmm')").exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(xs(&scene), before);
    }

    fn corners(node: &ResolvedNode) -> Vec<(f32, f32)> {
        node.children.iter().map(|c| (c.rect.x, c.rect.y)).collect()
    }

    fn rects(n: usize, extra: &str) -> String {
        (0..n).map(|_| format!("rect {{ width = 40, height = 10, {extra} }}")).collect::<Vec<_>>().join(", ")
    }

    /// Lines break where the next child no longer fits, `spacing` sits in a line and
    /// `line_spacing` between lines, and a content-sized cross axis grows with the lines.
    #[test]
    fn a_wrapping_row_breaks_at_its_width_and_grows_with_its_lines() {
        let (scene, ..) = bar(&format!(
            "panel {{ id = 'bar', child = row {{ wrap = true, width = 100, spacing = 5, line_spacing = 7,
                children = {{ {} }} }} }}",
            rects(3, "")
        ));
        let row = flow_of(&scene);
        assert_eq!(corners(row), [(0.0, 0.0), (45.0, 0.0), (0.0, 17.0)]);
        assert_eq!(row.rect.height, 27.0, "two lines of 10 and a gap of 7");
    }

    #[test]
    fn a_wrapping_column_flows_into_columns() {
        let (scene, ..) = bar(
            "panel { id = 'bar', child = column { wrap = true, height = 100, spacing = 5, line_spacing = 7,
                children = { rect { width = 10, height = 40 }, rect { width = 10, height = 40 }, rect { width = 10, height = 40 } } } }",
        );
        let column = flow_of(&scene);
        assert_eq!(corners(column), [(0.0, 0.0), (0.0, 45.0), (17.0, 0.0)]);
        assert_eq!(column.rect.width, 27.0);
    }

    /// A child's margins count in its line: the margin box is what breaks, and the child sits inside it.
    #[test]
    fn a_wrapping_row_breaks_on_margin_boxes() {
        let (scene, ..) = bar(&format!(
            "panel {{ id = 'bar', child = row {{ wrap = true, width = 100, children = {{ {} }} }} }}",
            rects(3, "margin = { left = 5, right = 5 }")
        ));
        assert_eq!(corners(flow_of(&scene)), [(5.0, 0.0), (55.0, 0.0), (5.0, 10.0)]);
    }

    /// `wrap`, `line_spacing` and `homogeneous` each reach the solver when a signal flips them.
    #[test]
    fn flipping_wrap_line_spacing_or_homogeneous_re_solves() {
        let (mut scene, lua, shaping, surface) =
            bar("wrap = state('wrap', false) gap = state('gap', 0) same = state('same', false)
            return panel { id = 'bar', child = row { wrap = wrap, line_spacing = gap, homogeneous = same, width = 100,
                children = { rect { width = 40, height = 10 }, rect { width = 40, height = 10 },
                    rect { width = 20, height = 10 }, rect { width = 40, height = 10 } } } }");
        let mut seen = vec![corners(flow_of(&scene))];
        for flip in ["wrap:set(true)", "gap:set(7)", "same:set(true)"] {
            lua.load(flip).exec().unwrap();
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            let now = corners(flow_of(&scene));
            assert!(!seen.contains(&now), "{flip} left the rects at {now:?}, as before: {seen:?}");
            seen.push(now);
        }
    }

    /// `align_h` packs each line on its own and `align_v` packs the lines in the container.
    #[test]
    fn a_wrapping_row_packs_each_line_and_the_lines() {
        let (scene, ..) = bar(&format!(
            "panel {{ id = 'bar', child = row {{ wrap = true, width = 100, height = 50, spacing = 5,
                align_h = 'center', align_v = 'end', children = {{ {} }} }} }}",
            rects(3, "")
        ));
        assert_eq!(corners(flow_of(&scene)), [(7.5, 30.0), (52.5, 30.0), (30.0, 40.0)]);
    }

    /// A `"fill"` child counts as zero when lines break, then takes what its own line leaves.
    #[test]
    fn a_fill_child_in_a_wrapping_row_takes_the_slack_of_its_line() {
        let (scene, ..) = bar("panel { id = 'bar', child = row { wrap = true, width = 100, children = {
                rect { width = 40, height = 10 }, rect { width = 'fill', height = 10 },
                rect { width = 40, height = 10 }, rect { width = 40, height = 10 } } } }");
        let row = flow_of(&scene);
        assert_eq!(row.children[1].rect.width, 20.0);
        assert_eq!(corners(row), [(0.0, 0.0), (40.0, 0.0), (60.0, 0.0), (0.0, 10.0)]);
    }

    /// Without a bound there is nothing to break at: one line, as CSS; `max_width` is a bound.
    #[test]
    fn a_wrapping_row_without_a_bound_is_one_line_and_max_width_bounds_it() {
        let (scene, ..) =
            bar(&format!("panel {{ id = 'bar', child = row {{ wrap = true, children = {{ {} }} }} }}", rects(3, "")));
        assert_eq!((flow_of(&scene).rect.width, flow_of(&scene).rect.height), (120.0, 10.0));
        let (scene, ..) = bar(&format!(
            "panel {{ id = 'bar', child = row {{ wrap = true, max_width = 100, children = {{ {} }} }} }}",
            rects(3, "")
        ));
        assert_eq!((flow_of(&scene).rect.width, flow_of(&scene).rect.height), (80.0, 20.0));
        let (scene, ..) = bar(&format!(
            "panel {{ id = 'bar', child = row {{ wrap = true, homogeneous = true, children = {{ {} }} }} }}",
            rects(2, "")
        ));
        assert_eq!(corners(flow_of(&scene)), [(0.0, 0.0), (0.0, 10.0)], "no bound: one cell per line");
    }

    /// Every cell is the largest child's size, across all lines: a real grid.
    #[test]
    fn a_homogeneous_wrapping_row_is_a_grid_of_the_largest_cell() {
        let (scene, ..) = bar(
            "panel { id = 'bar', child = row { wrap = true, homogeneous = true, width = 100, spacing = 5, line_spacing = 7,
                children = { rect { width = 30, height = 10 }, rect { width = 45, height = 10 },
                             rect { width = 20, height = 10 }, rect { width = 20, height = 10 } } } }",
        );
        let row = flow_of(&scene);
        assert_eq!(corners(row), [(0.0, 0.0), (50.0, 0.0), (0.0, 17.0), (50.0, 17.0)]);
        assert_eq!(row.rect.height, 27.0);
    }

    #[test]
    fn a_homogeneous_wrapping_column_is_a_grid_of_the_largest_cell() {
        let (scene, ..) =
            bar("panel { id = 'bar', child = column { wrap = true, homogeneous = true, height = 100, line_spacing = 3,
                children = { rect { width = 10, height = 30 }, rect { width = 25, height = 45 },
                             rect { width = 10, height = 30 } } } }");
        assert_eq!(corners(flow_of(&scene)), [(0.0, 0.0), (0.0, 45.0), (28.0, 0.0)]);
    }

    #[test]
    fn wrap_and_scroll_are_refused_together() {
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from("panel { id = 'bar', child = row { wrap = true, scroll = scroll('s') } }");
        let err = apply_at(&mut Scene::new(), &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(err.to_string().contains("wrap"), "{err}");
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
                rect { width = "fill", height = 10, margin = 25 },
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
                rect { width = "fill", height = 10 },
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
                rect { width = "fill", height = 10 },
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
                rect { height = "fill", width = 10 },
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
                rect { width = "fill", height = 10 },
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
            r#"rect { width = "fill", height = 10, visible = false }"#,
        ] {
            let mut scene_for_case = std::mem::replace(&mut scene, Scene::new());
            let (lua, surface) = surface_from(&format!(
                r#"panel {{ id = "bar", child = row {{ width = 600, height = 40, children = {{
                    rect {{ width = "fill", height = 10 }}, {hidden},
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
                rect { width = 100, height = "fill" },
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
                rect { width = "fill", height = 10 },
                rect { width = "fill", height = 10 },
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
                rect { width = "fill", height = 10 },
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
                rect { width = "fill", height = 10 },
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

    /// A content-sized row floored by `min_width` is that wide and its `Fill` child takes the slack,
    /// whatever flow, stack or alignment holds the row.
    #[test]
    fn a_fill_child_of_a_row_with_a_min_width_takes_the_slack() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let kids = r#"children = { rect { width = 40, height = 10 }, rect { width = "fill", height = 10 }, rect { width = 40, height = 10 } }"#;
        let cases = [
            format!(r#"row {{ min_width = 220, {kids} }}"#),
            format!(r#"column {{ children = {{ row {{ min_width = 220, {kids} }} }} }}"#),
            format!(r#"row {{ align_h = "end", children = {{ row {{ min_width = 220, {kids} }} }} }}"#),
            format!(r#"rect {{ children = {{ row {{ min_width = 220, align_h = "center", {kids} }} }} }}"#),
        ];
        for body in &cases {
            let (lua, surface) = surface_from(&format!(r#"panel {{ id = "bar", child = {body} }}"#));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let mut row = &scene.surface("bar@TEST").unwrap().children[0];
            while row.children.len() == 1 {
                row = &row.children[0];
            }
            assert_eq!(row.rect.width, 220.0, "{body}");
            assert_eq!(row.children[1].rect.width, 140.0, "{body}");
            assert_eq!(row.children[2].rect.x, 180.0, "{body}");
        }
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

    /// Found live: a capped title in a centred content-sized row sized the row to the whole string,
    /// so the elided caption sat at the left of a box twice its width.
    #[test]
    fn a_content_sized_row_takes_a_capped_text_at_its_ceiling() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { align_h = "center", children = { row { children = {
                rect { width = 10, height = 10 },
                text { content = string.rep("wide title ", 40), max_width = 120, elide = "end" },
            } } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert_eq!(row.children[1].rect.width, 120.0);
        assert_eq!(row.rect.width, 130.0, "the row holds the 10 wide rect and the capped text");
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
    fn padding_larger_than_an_explicit_size_keeps_the_content_box_nonnegative() {
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { width = 20, height = 20, padding = 30,
                children = { rect { width = 10, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let node = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((node.rect.width, node.rect.height), (60.0, 60.0));
        assert_eq!((node.children[0].rect.x, node.children[0].rect.y), (30.0, 30.0));
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

    #[test]
    fn text_spacing_and_line_height_change_the_measured_box() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { children = {
                text { content = "ABCD", font_size = 20 },
                text { content = "ABCD", font_size = 20, line_height = 2, letter_spacing = 5 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let children = &scene.surface("bar@TEST").unwrap().children[0].children;
        assert_eq!(children[0].rect.height, 24.0);
        assert_eq!(children[1].rect.height, 40.0);
        assert!(children[1].rect.width > children[0].rect.width + 5.0);
    }

    /// taffy 0.14 adds a flex container's own margin to its children's minimum cross size when it
    /// measures them (see [`taffy_style`]'s `min_size`; fixed by DioxusLabs/taffy#1081). The card here is a column
    /// with a left margin of most of the output, holding a body that wraps at the card's width.
    /// Its height has to be the wrapped body's, whatever the margin.
    #[test]
    fn a_containers_own_margin_does_not_widen_what_its_children_are_measured_at() {
        let long = "have a look at this: https://example.com/project/mantle/pull/12345 and tell me what you think about it all";
        let heights = |margin: u32| {
            let src = format!(
                r#"panel {{ id = "bar", child = column {{ width = "fill", height = "fill", children = {{
                column {{ width = 392, margin = {{ left = {margin}, top = 4 }}, padding = {{ top = 7, right = 7, bottom = 7, left = 7 }}, children = {{
                    column {{ width = "fill", children = {{
                        text {{ content = "Sender", font_size = 14 }},
                        text {{ content = "{long}", font_size = 12, wrap = "word", width = "fill" }},
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

    /// Inter's `opsz` changes advances, so a memo kept across the change would keep the old box.
    #[test]
    fn font_variations_change_invalidates_text_memo() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn_variable_fixture();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = text { content = "Mantle", font = "Inter Variable",
                font_variations = state("axes", { opsz = 14 }) } }"#,
        );
        let mut width = || {
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            scene.surface("bar@TEST").unwrap().children[0].rect.width
        };
        let text = width();
        lua.load(r#"state("axes", {}):set({ opsz = 32 })"#).exec().unwrap();
        assert_ne!(width(), text);
    }

    /// A table compares by address, so an axis changed in place must be seen in the parsed axes.
    #[test]
    fn font_variations_mutated_in_place_invalidate_text_memo() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn_variable_fixture();
        let (lua, surface) = surface_from(
            r##"axes = { opsz = 14 }
            return panel { id = "bar", child = text { content = "Mantle", font = "Inter Variable",
                font_variations = axes, foreground = state("fg", "#FFFFFFFF") } }"##,
        );
        let mut width = || {
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            scene.surface("bar@TEST").unwrap().children[0].rect.width
        };
        let text = width();
        lua.load(r##"axes.opsz = 32 state("fg", "#FFFFFFFF"):set("#000000FF")"##).exec().unwrap();
        assert_ne!(width(), text);
    }

    /// A runs table compares by address, so the parsed runs must tell an in-place edit.
    #[test]
    fn runs_mutated_in_place_invalidate_text_memo() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"runs = { { text = "ab" } }
            return panel { id = "bar", child = text { content = state("c", runs) } }"#,
        );
        let mut width = || {
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            scene.surface("bar@TEST").unwrap().children[0].rect.width
        };
        let short = width();
        lua.load(r#"runs[1].text = "abcdefgh" state("c", runs):set(runs)"#).exec().unwrap();
        assert!(width() > short * 2.0);
    }

    #[test]
    fn an_unchanged_text_keeps_its_memo() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = text { content = "Hello", font_size = state("fs", 12) } }"#,
        );
        let mut memo = || {
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            let id = scene.surface("bar@TEST").unwrap().children[0].taffy.unwrap();
            let tree = scene.solver_trees.values().next().unwrap();
            match tree.get_node_context(id) {
                Some(Measure::Text { memo, .. }) => *memo,
                _ => None,
            }
        };
        let first = memo();
        assert!(first.is_some());
        assert_eq!(memo(), first);
        lua.load(r#"state("fs", 12):set(24)"#).exec().unwrap();
        assert_ne!(memo(), first);
    }
}

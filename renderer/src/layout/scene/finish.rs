use std::time::Instant;

use mlua::Value;

use super::field::FieldDrafts;
use super::fit::fit_text_to_box;
use super::scroll::{ease_request, outruns_window, scroll_children, viewport};
use super::solver::{MainAxis, Measure, main_axis_of, taffy_failed};
use super::{LogicalSize, PreparedNode, ResolvedNode};
use crate::layout::node::{self, LayoutError};
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

/// The second walk: solved geometry back out of the tree and into retained nodes.
///
/// taffy hands out a location relative to the parent's border box, the same frame `layout::paint`
/// and `layout::hit` already accumulate down, so the rect goes across untouched. Two things still
/// `scene`'s own happen here, both needing a size that only exists once the solve is done: a
/// scrolled container shifts its children, and an over-wide `text` is cut to the box it ended up
/// in. Also whether a glyph 0 went into its size: a text in it measured one (#125).
pub(super) fn finish(
    tree: &taffy::TaffyTree<Measure>,
    prepared: PreparedNode,
    (shaping, drafts, fonts): (&ShapingHandle, &FieldDrafts, (u64, u64)),
    now: Instant,
) -> Result<(ResolvedNode, bool), LayoutError> {
    let PreparedNode {
        id,
        kind,
        allocated_axes,
        style,
        properties,
        mut paint,
        displayed_source,
        dissolve,
        taffy: taffy_id,
        children,
        frozen,
        mut tweens,
        move_spec,
        movement,
        prior_position,
        prior_size: _,
        leaving,
        mut list_memo,
        child_table,
        resolve_memo,
        site,
        scrolled: kept_scroll,
        field_top,
    } = prepared;
    let layout = tree.layout(taffy_id).map_err(taffy_failed)?;
    let size = LogicalSize { width: layout.size.width, height: layout.size.height };
    let (unconstrained_width, mut boxed) = match tree.get_node_context(taffy_id) {
        Some(Measure::Text { memo: Some((width, size, boxed)), .. }) => (width.is_none().then_some(size.width), *boxed),
        _ => (None, false),
    };

    // Frozen children come back as they were (see `prepare`): no scroll offset applied again to
    // rects that already carry one, no text refitted to a box that was not laid out.
    let (children, scrolled) = if style.visible {
        // Sized up front: a `Result` collect cannot, and grows by doubling, moving every node again.
        let mut built = Vec::with_capacity(children.len() + leaving.len());
        let windowed = list_memo.as_deref().is_some_and(node::ListMemo::windowed);
        let mut bits = Vec::with_capacity(if windowed { children.len() } else { 0 });
        for child in children {
            let (child, bit) = finish(tree, child, (shaping, drafts, fonts), now)?;
            boxed |= bit;
            bits.extend(windowed.then_some(bit));
            built.push(child);
        }
        let mut children: Vec<ResolvedNode> = built;
        if let Some(memo) = list_memo.as_deref_mut()
            && let Some(axis) = main_axis_of(kind, &properties)?
        {
            let cross = if axis == MainAxis::Vertical { size.width } else { size.height };
            let lengths =
                children.iter().zip(bits).map(|(child, bit)| (child.in_flow().then(|| child.extent_on(axis)), bit));
            memo.measured(lengths, (viewport(&style, size, axis).0, cross, style.spacing), fonts);
        }

        // ADR-0069 decision 4.
        let scrolled = match main_axis_of(kind, &properties)? {
            Some(axis) if properties.contains_key("scroll") => {
                let memo = list_memo.as_deref();
                ease_request(kind, &properties, &style, size, axis, &children, &mut tweens, now, memo)?;
                scroll_children(&properties, &style, size, axis, &mut children, 0.0, memo)
            }
            None if kind == "textfield" => kept_scroll,
            _ => 0.0,
        };

        // After sizing, because the width it fits into is this node's own, and before the node is
        // built, because what it rewrites is the string the display list will carry.
        fit_text_to_box(&mut paint, (size.width - style.padding.horizontal()).max(0.0), unconstrained_width, shaping);

        // Last, so an exit paints over what took its place -- and after the scroll loop above, which
        // is why a leaving child keeps the offset it was dropped at rather than travelling with the
        // list.
        // ponytail: a scroll mid-exit drifts the leaver; upgrade: subtract its stored drop offset.
        children.extend(leaving);
        (children, scrolled)
    } else {
        (frozen, 0.0)
    };

    let mask_target = match &paint {
        Some(node::PaintStyle::Box { mask: Some(node::Mask { source: node::MaskSource::Node(id), .. }), .. }) => {
            let target = children.iter().find(|child| {
                !child.leaving && matches!(child.properties.get("id"), Some(mlua::Value::String(name)) if name.to_str().is_ok_and(|name| &*name == id))
            }).map(|child| child.id);
            if style.visible && target.is_none() {
                return Err(node::invalid("mask", format!("no direct child with id `{id}`")));
            }
            target
        }
        _ => None,
    };
    // A hidden field's rows are back at the top, so its anchor goes.
    let field_top = field_top.filter(|_| style.visible);
    let mut node = ResolvedNode {
        mask_target,
        allocated_axes,
        taffy: Some(taffy_id),
        id,
        kind,
        rect: LogicalRect { x: layout.location.x, y: layout.location.y, width: size.width, height: size.height },
        margin: style.margin,
        scrolled,
        visible: style.visible,
        opacity: style.opacity,
        z: style.z,
        transform: style.transform,
        compositor_blur: style.compositor_blur,
        layout_style: style,
        properties,
        paint,
        displayed_source,
        dissolve,
        children,
        tweens,
        move_spec,
        movement,
        leaving: false,
        list_memo,
        child_table,
        resolve_memo,
        site,
        field_top,
    };
    super::field::refit(&mut node, drafts, shaping, fonts.0);
    retarget_move(&mut node, prior_position, now);
    // A window the pass came up short of builds again this turn (`Scene::read_by_lua`): the
    // viewport outgrew the one it was planned for, or its new items were shorter than estimated.
    if node.visible
        && node.list_memo.as_ref().is_some_and(|memo| memo.window.is_some())
        && let Some(axis) = main_axis_of(kind, &node.properties)?
        && outruns_window(&node, axis)
        && let Some(handle) = node::signal_at(&node.properties, "scroll").and_then(|scroll| scroll.scroll_handle())
    {
        handle.set_quiet(Value::Number(f64::from(node.scrolled)));
    }
    Ok((node, boxed))
}

/// Compare solver positions before scroll rounds either rect; a scroll change stays immediate.
fn retarget_move(node: &mut ResolvedNode, prior_position: Option<(f32, f32)>, now: Instant) {
    let Some(spec) = node.move_spec.as_deref().copied().filter(|_| node.visible) else {
        node.movement = None;
        return;
    };
    let Some(prior) = prior_position else {
        return;
    };
    let delta = (prior.0 - node.rect.x, prior.1 - node.rect.y);
    if delta != (0.0, 0.0) {
        let offset = node.movement.as_ref().map_or((0.0, 0.0), |moving| moving.offset);
        let from = (delta.0 + offset.0, delta.1 + offset.1);
        node.movement = (from != (0.0, 0.0)).then(|| Box::new(super::MoveTween::new(from, spec, now)));
    }
}

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use mlua::{Lua, Value};

use super::fit::fit_text_to_box;
use super::scroll::{extent_along, reveal_child, scroll_offset};
use super::solver::{
    MainAxis, Measure, main_axis_of, measure_for, new_solver_node, solve, taffy_failed, text_measure_matches,
};
use super::tick::{advance_leaving, advanced_dissolve};
use super::{
    LayoutStyle, LogicalSize, PreparedNode, ResolvedNode, Scene, close, ensure_node_admissible, open_span, tween_state,
};
use crate::layout::node::{self, LayoutError, PropMap, SizeMode, Tween};
use crate::lua::nodes::VirtualNode;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

/// Selects `child`, `children`, generated list children, or no children. Surface roles share one
/// `child`; `textfield` is a leaf. Its callbacks and secure-submit fields remain in
/// `ResolvedNode.properties`; the keyboard path reads the latter from the scene while the secret
/// buffer stays on `App` (ADR-0005).
fn children_of(kind: &str, properties: &PropMap) -> Result<Vec<VirtualNode>, LayoutError> {
    match kind {
        "panel" | "window" | "popup" | "lock" => Ok(node::parse_single_child(properties)?.into_iter().collect()),
        "rect" | "row" | "column" | "button" => node::parse_children(properties),
        // ADR-0045 decision 3: list children are generated from `source`, not a literal table.
        "list" => node::parse_list_children(properties),
        "text" | "icon" | "image" | "capture" | "shader" | "textfield" => Ok(Vec::new()),
        other => unreachable!("ensure_supported_kind already rejected `{other}`"),
    }
}

/// `child = function(output)` on a `panel`/`lock` (ADR-0121) runs per instance and pass, with the
/// output name, before the ordinary child walk. Per-pass calls preserve registry-stable state such
/// as `state("wallpaper_" .. output)`. `window`/`popup` and a `monitor = "Active"` panel (ADR-0246)
/// have no output name, so function children are refused rather than called with `""`.
pub(super) fn build_child_for_output(
    mut properties: PropMap,
    kind: &str,
    output: &str,
) -> Result<PropMap, LayoutError> {
    let Some(Value::Function(builder)) = properties.get("child") else {
        return Ok(properties);
    };
    if !matches!(kind, "panel" | "lock") || output.is_empty() {
        return Err(node::invalid(
            "child",
            format!(
                "a function child is for a `panel` or `lock`, which have one instance per output to hand it; \
                 this `{kind}` has one instance wherever the compositor places it"
            ),
        ));
    }
    let built = builder.call::<Value>(output).map_err(|e| node::invalid("child", e.to_string()))?;
    match built {
        Value::Table(_) => {
            properties.insert("child", built);
        }
        // A nil result maps this output empty, like an absent `child`.
        Value::Nil => {
            properties.remove("child");
        }
        other => {
            return Err(node::invalid(
                "child",
                format!("expected the function to return a node table, got {}", node::preview_for_error(&other)),
            ));
        }
    }
    Ok(properties)
}

/// Resolves only a surface root's `Fill`/`Percent` against available room. `Content` stays `auto`
/// until children are known; descendants use the solver.
fn resolve_non_content(mode: SizeMode, available: f32) -> Option<f32> {
    match mode {
        SizeMode::Content => None,
        SizeMode::Pixels(n) => Some(n),
        SizeMode::Fill => Some(available),
        SizeMode::Percent(p) => Some(available * p),
    }
}

/// The root's size, the solve and the read-back, shared by a pass and a tick.
///
/// Patch the root after the walk: its `Fill`/`Percent` resolve against configured room because it
/// has no parent; descendants take size from the solver.
pub(super) fn solve_instance(
    tree: &mut taffy::TaffyTree<Measure>,
    prepared: PreparedNode,
    available: LogicalSize,
    shaping: &ShapingHandle,
) -> Result<ResolvedNode, LayoutError> {
    let style = prepared.style;
    let forced = forced_root_size(prepared.kind, &style, available);
    let mut root_style = tree.style(prepared.taffy).map_err(taffy_failed)?.clone();
    root_style.size = taffy::Size {
        width: match forced.0.or_else(|| resolve_non_content(style.width_mode, available.width)) {
            Some(width) => taffy::Dimension::length(width),
            None => taffy::Dimension::auto(),
        },
        height: match forced.1.or_else(|| resolve_non_content(style.height_mode, available.height)) {
            Some(height) => taffy::Dimension::length(height),
            None => taffy::Dimension::auto(),
        },
    };
    tree.set_style(prepared.taffy, root_style).map_err(taffy_failed)?;
    solve(tree, prepared.taffy, available, shaping)?;
    finish(tree, prepared, shaping)
}

/// A `window` or `lock` root with no size of its own is its configured surface, on the `Content`
/// axes only. `available` is the compositor's `xdg_toplevel` configure size, already converted by
/// `set_instance_size`. A `Content` default would give children a zero budget: a 0x0 tree in a
/// configured tile, and for a lock (no width/height, `lock_spec` refuses both) a transparent buffer
/// over a locked session, the passwordless black screen ADR-0052 decision 3 rejects.
fn forced_root_size(kind: &str, style: &LayoutStyle, available: LogicalSize) -> (Option<f32>, Option<f32>) {
    if matches!(kind, "window" | "lock") {
        (
            (style.width_mode == SizeMode::Content).then_some(available.width),
            (style.height_mode == SizeMode::Content).then_some(available.height),
        )
    } else {
        (None, None)
    }
}

/// Pairs children by identity (ADR-0045 decisions 1-2): an `id` matches only the same `id`, while
/// id-less children match positionally among other id-less children. An id miss is new, never a
/// positional fallback, so it cannot inherit an unrelated `NodeId`/subtree. Unclaimed retained
/// nodes come back second, for `prepare` to see off. The linear match uses one `HashMap<&str,
/// usize>` per parent; sibling
/// count is unbounded (`1..10000` is legal Lua), and this runs on the Wayland dispatch thread at
/// capability-push cadence (ADR-0044 decision 2).
fn pair_children_by_id_then_position(
    fresh_children: &[VirtualNode],
    old_children: Vec<ResolvedNode>,
) -> Result<(Vec<Option<ResolvedNode>>, Vec<ResolvedNode>), LayoutError> {
    if !fresh_children.iter().any(|c| c.properties.contains_key("id"))
        && !old_children.iter().any(|c| c.properties.contains_key("id"))
    {
        let mut old_iter = old_children.into_iter();
        let matched = (0..fresh_children.len()).map(|_| old_iter.next()).collect();
        return Ok((matched, old_iter.collect()));
    }

    // Not `collect`: a `Result` collect drops the size hint, and a list is as long as its data.
    let mut fresh_ids: Vec<Option<String>> = Vec::with_capacity(fresh_children.len());
    for child in fresh_children {
        fresh_ids.push(node::parse_node_id(&child.properties)?);
    }

    // Decision 1: reject duplicate sibling ids before matching `old_children`.
    let mut seen: HashSet<&str> = HashSet::with_capacity(fresh_ids.len());
    for id in fresh_ids.iter().flatten() {
        if !seen.insert(id.as_str()) {
            return Err(LayoutError::InvalidProperty {
                property: "id".to_string(),
                detail: format!("duplicate id `{id}` among siblings"),
            });
        }
    }

    // Retained ids were already validated and cannot hold signals, so `.ok().flatten()` safely
    // treats absent and validated-no-id alike.
    let old_ids: Vec<Option<String>> =
        old_children.iter().map(|c| node::parse_node_id(&c.properties).ok().flatten()).collect();
    let mut old_slots: Vec<Option<ResolvedNode>> = old_children.into_iter().map(Some).collect();

    let mut retained_by_id: HashMap<&str, usize> = HashMap::with_capacity(old_ids.len());
    for (index, id) in old_ids.iter().enumerate() {
        if let Some(id) = id {
            retained_by_id.insert(id.as_str(), index);
        }
    }

    // Identified subsequence: a miss stays `None`, not a positional slot.
    let mut matched: Vec<Option<ResolvedNode>> = Vec::with_capacity(fresh_children.len());
    for fresh_id in &fresh_ids {
        let claimed = fresh_id
            .as_deref()
            .and_then(|id| retained_by_id.get(id).copied())
            .and_then(|index| old_slots[index].take());
        matched.push(claimed);
    }

    // Id-less subsequences zip in order (ADR-0023); identified retained children are excluded.
    let mut unidentified_old = old_ids.iter().enumerate().filter(|(_, id)| id.is_none()).map(|(index, _)| index);
    for (slot, fresh_id) in matched.iter_mut().zip(&fresh_ids) {
        if fresh_id.is_none()
            && let Some(index) = unidentified_old.next()
        {
            *slot = old_slots[index].take();
        }
    }

    Ok((matched, old_slots.into_iter().flatten().collect()))
}

/// The first of the two walks: identity, resolution, parsing and tree construction, in the order
/// the config wrote the nodes.
///
/// Everything that can run Lua or fail happens here, depth-first in declaration order, guaranteeing
/// every getter fires exactly once, in source order. The hand-written pass had to work to keep
/// that, recursing into `Fill` children after their siblings and splitting resolution out of the
/// recursion; here there are no rounds, so declaration and recursion order are the same one.
///
/// `properties` and `style` arrive already resolved and parsed, done by the parent:
/// `taffy_style` needs a child's `margin` and size modes to build the node, so the parse happens
/// in the parent's loop. `Scene::apply_one_instance` does it for a surface root, which has none.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare(
    scene: &mut Scene,
    tree: &mut taffy::TaffyTree<Measure>,
    retained: Option<ResolvedNode>,
    kind: &'static str,
    properties: PropMap,
    style: LayoutStyle,
    tweens: Vec<Tween>,
    parent_axis: Option<MainAxis>,
    thawing: bool,
    lua: &Lua,
    now: Instant,
    depth: u32,
) -> Result<PreparedNode, LayoutError> {
    // Already run by whoever resolved `properties` (that is the ordering `ensure_node_admissible`
    // exists to enforce), repeated here so this function holds its own preconditions rather than
    // trusting a call site, notably `children_of`'s `unreachable!` arm.
    ensure_node_admissible(kind, depth)?;
    // Removed while hidden means removed off screen: no exit plays anywhere under a thaw.
    let thawing = thawing || retained.as_ref().is_some_and(|r| !r.visible);

    let (id, displayed_source, dissolve, old_children, text_memo) = match retained {
        Some(r) => {
            let memo =
                if kind == "text" && text_measure_matches(&properties, &r.properties) { r.text_memo } else { None };
            (r.id, r.displayed_source, r.dissolve, r.children, memo)
        }
        None => (scene.alloc_id(), None, None, Vec::new(), None),
    };
    // Already leaving children are not paired again: a re-added id is a new node beside the one
    // still fading.
    // Checked first because nothing is usually leaving.
    let (leaving, old_children): (Vec<ResolvedNode>, Vec<ResolvedNode>) =
        if old_children.iter().any(|child| child.leaving) {
            old_children.into_iter().partition(|child| child.leaving)
        } else {
            (Vec::new(), old_children)
        };

    // Before the children, because a `text`'s measurement reads the `content` and `font_size`
    // parsed here rather than parsing them a second time.
    let paint = node::paint_style(kind, &properties)?;
    let measure = measure_for(kind, paint.as_ref(), &properties, text_memo)?;

    // Before the children, so their ids attach afterwards, and so the `taffy::Style` behind it is
    // gone from the stack by the time this frame recurses (see `new_solver_node`).
    let taffy_id = new_solver_node(tree, kind, &properties, &style, parent_axis, measure)?;

    // A hidden node's subtree is frozen, not rebuilt (ADR-0124): the children it had keep their
    // ids, properties and last geometry, and none of their signals is read, no `list` item
    // function called, no text measured, until the node is visible again. `taffy_style` already
    // gave the node `Display::None`, so nothing below it could have reached the layout anyway,
    // and `paint`, `hit` and `region` stop at a hidden node. A closed picker of fifty tiles is not
    // rebuilt on every capability push.
    let mut node = PreparedNode {
        id,
        kind,
        style,
        properties,
        paint,
        displayed_source,
        dissolve: advanced_dissolve(dissolve, now),
        taffy: taffy_id,
        children: Vec::new(),
        frozen: old_children,
        tweens,
        leaving: Vec::new(),
    };
    if !node.style.visible {
        node.frozen.extend(leaving);
        return Ok(node);
    }

    let fresh_children = if kind == "list" {
        let mut at = open_span();
        let res = children_of(kind, &node.properties);
        close(&mut at, &mut scene.resolve_split.list);
        res?
    } else {
        children_of(kind, &node.properties)?
    };
    let (matched_candidates, mut unclaimed) =
        pair_children_by_id_then_position(&fresh_children, std::mem::take(&mut node.frozen))?;
    let own_axis = main_axis_of(kind, &node.properties)?;

    node.children.reserve(fresh_children.len());
    for (index, (fresh_child, candidate)) in fresh_children.into_iter().zip(matched_candidates).enumerate() {
        let VirtualNode { kind: child_kind, properties: child_raw } = fresh_child;
        // Every failure below names this child, so the message that reaches a human is the path
        // down to the node rather than a property name and a surface (`LayoutError::in_child`).
        let here = |err: LayoutError| err.in_child(index, child_kind);

        // Before this child's own getters run, not after: resolving its property map calls back
        // into Lua, and a child the walk is about to refuse must not execute anything on the way
        // to being refused. `depth + 1` is the level this child would occupy, so the error is the
        // same variant, kind and level the recursive call raises (see `ensure_node_admissible`).
        ensure_node_admissible(child_kind, depth + 1)?;

        let reusable = match candidate {
            Some(candidate) if candidate.kind != child_kind => {
                unclaimed.push(candidate);
                None
            }
            candidate => candidate,
        };

        // This child's one resolve and one parse for this pass, both here rather than inside the
        // recursive call, because the style the call is handed is built from them and a second
        // read of an impure `margin` could answer differently. Tweens go between the two: the
        // parse must see the displayed value, not the target (ADR-0145).
        let mut child_properties = node::resolve_properties(child_raw, child_kind, lua).map_err(here)?;
        let child_tweens =
            node::retarget(child_kind, reusable.as_ref().map(tween_state), &mut child_properties, now, lua)
                .map_err(here)?;
        let child_style = LayoutStyle::parse(&child_properties).map_err(here)?;
        node.children.push(
            prepare(
                scene,
                tree,
                reusable,
                child_kind,
                child_properties,
                child_style,
                child_tweens,
                own_axis,
                thawing,
                lua,
                now,
                depth + 1,
            )
            .map_err(here)?,
        );
    }

    // The ones on their way out: those already leaving move on, those the tree just dropped
    // start their exit (ADR-0150). A hidden one, or one with no exit block, is simply gone.
    for child in leaving {
        if let Some(child) = advance_leaving(child, now, lua)? {
            node.leaving.push(child);
        }
    }
    for mut child in unclaimed {
        if !thawing && child.visible && node::depart(child.kind, &mut child.tweens, &mut child.properties, now, lua)? {
            child.leaving = true;
            node.leaving.push(child);
        }
    }

    let child_ids: Vec<taffy::NodeId> = node.children.iter().map(|child| child.taffy).collect();
    tree.set_children(taffy_id, &child_ids).map_err(taffy_failed)?;
    Ok(node)
}

/// The second walk: solved geometry back out of the tree and into retained nodes.
///
/// taffy hands out a location relative to the parent's border box, the same frame `layout::paint`
/// and `layout::hit` already accumulate down, so the rect goes across untouched. Two things still
/// `scene`'s own happen here, both needing a size that only exists once the solve is done: a
/// scrolled container shifts its children, and an over-wide `text` is cut to the box it ended up
/// in.
fn finish(
    tree: &taffy::TaffyTree<Measure>,
    prepared: PreparedNode,
    shaping: &ShapingHandle,
) -> Result<ResolvedNode, LayoutError> {
    let PreparedNode {
        id,
        kind,
        style,
        properties,
        mut paint,
        displayed_source,
        dissolve,
        taffy: taffy_id,
        children,
        frozen,
        tweens,
        leaving,
    } = prepared;
    let layout = tree.layout(taffy_id).map_err(taffy_failed)?;
    let size = LogicalSize { width: layout.size.width, height: layout.size.height };
    let (text_memo, unconstrained_width) = match tree.get_node_context(taffy_id) {
        Some(Measure::Text { memo: Some((max_width, size)), .. }) => {
            (Some((*max_width, *size)), if max_width.is_none() { Some(size.width) } else { None })
        }
        _ => (None, None),
    };

    // Frozen children come back as they were (see `prepare`): no scroll offset applied again to
    // rects that already carry one, no text refitted to a box that was not laid out.
    let (children, text_memo) = if style.visible {
        let mut children: Vec<ResolvedNode> =
            children.into_iter().map(|child| finish(tree, child, shaping)).collect::<Result<_, _>>()?;

        // ADR-0069 decision 4. Subtracted from every child's main coordinate, so a scrolled child sits
        // before the content box and the clip `layout::paint` computes per node cuts it. Summed here
        // rather than read off taffy's `scrollable_overflow_rect`, since the two disagree: CSS
        // scrollable overflow is the union of the children's border boxes, while this engine's
        // `spacing`-and-margin footprint is what `Fill` was sized against, which the tests pin.
        if let Some(axis) = main_axis_of(kind, &properties)? {
            let padding = style.padding;
            let (content_main, total_main) = match axis {
                MainAxis::Horizontal => (
                    (size.width - padding.horizontal()).max(0.0),
                    extent_along(&children, MainAxis::Horizontal, style.spacing),
                ),
                MainAxis::Vertical => (
                    (size.height - padding.vertical()).max(0.0),
                    extent_along(&children, MainAxis::Vertical, style.spacing),
                ),
            };
            let padding_start = match axis {
                MainAxis::Horizontal => padding.left,
                MainAxis::Vertical => padding.top,
            };
            reveal_child(&properties, &children, axis, padding_start, content_main);
            let offset = scroll_offset(&properties, content_main, total_main);
            if offset != 0.0 {
                for child in &mut children {
                    match axis {
                        MainAxis::Horizontal => child.rect.x -= offset,
                        MainAxis::Vertical => child.rect.y -= offset,
                    }
                }
            }
        }

        // After sizing, because the width it fits into is this node's own, and before the node is
        // built, because what it rewrites is the string the display list will carry.
        fit_text_to_box(&mut paint, (size.width - style.padding.horizontal()).max(0.0), unconstrained_width, shaping);

        // Last, so an exit paints over what took its place -- and after the scroll loop above, which
        // is why a leaving child keeps the offset it was dropped at rather than travelling with the
        // list.
        // ponytail: a reader who scrolls during a 150 ms exit sees the leaver drift out of place. The
        // upgrade is to carry the offset each leaver was dropped at on the node and subtract the
        // difference here; nothing has asked for it, and lists here scroll far slower than they fade.
        children.extend(leaving);
        (children, text_memo)
    } else {
        (frozen, None)
    };

    Ok(ResolvedNode {
        id,
        kind,
        rect: LogicalRect { x: layout.location.x, y: layout.location.y, width: size.width, height: size.height },
        margin: style.margin,
        visible: style.visible,
        opacity: style.opacity,
        z: style.z,
        transform: style.transform,
        blur: style.blur,
        effect: style.effect,
        properties,
        paint,
        displayed_source,
        dissolve,
        children,
        tweens,
        leaving: false,
        text_memo,
    })
}

/// Writes every visible `geometry(name)` node's absolute rect into its signal (ADR-0147), the same
/// space `hover_writes` reports. No dirty flag: a pass write that changed a rect notes it for
/// `RendererClient` to turn into one follow-up pass, and a `quiet` tick write notes nothing, so no
/// frame ever schedules a pass. Hidden nodes keep their last rect, the way a hidden subtree keeps
/// everything else.
pub(super) fn publish_geometry(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    lua: &Lua,
    quiet: bool,
) -> mlua::Result<()> {
    if !node.in_flow() {
        return Ok(());
    }
    let (x, y) = (origin_x + node.rect.x, origin_y + node.rect.y);
    if let Some((id, cell)) = node::signal_at(&node.properties, "geometry").and_then(|signal| signal.geometry_cell()) {
        const KEYS: [&str; 4] = ["x", "y", "width", "height"];
        let fresh = [x, y, node.rect.width, node.rect.height];
        let same = match &*cell.borrow() {
            Value::Table(old) => KEYS.into_iter().zip(fresh).all(|(key, v)| old.get::<f32>(key).ok() == Some(v)),
            _ => false,
        };
        if !same {
            let rect = lua.create_table_with_capacity(0, 4)?;
            for (key, v) in KEYS.into_iter().zip(fresh) {
                rect.set(key, v)?;
            }
            *cell.borrow_mut() = Value::Table(rect);
            if !quiet {
                crate::lua::signal::note_geometry_moved(lua, id);
            }
        }
    }
    for child in &node.children {
        publish_geometry(child, x, y, lua, quiet)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::scene::tests::{apply_at, full, surface_from};
    use crate::layout::scene::*;
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};

    /// Two outputs, `"LEFT"` and `"RIGHT"`, for the per-output child fixtures (ADR-0121).
    fn apply_on_two_outputs(
        scene: &mut Scene,
        surface: &VirtualNode,
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        let declared_id = node::parse_surface_id(&surface.properties).expect("the fixture declares an `id`");
        let instances: Vec<SurfaceInstance> = ["LEFT", "RIGHT"]
            .into_iter()
            .map(|output| SurfaceInstance {
                instance_id: format!("{declared_id}@{output}"),
                declared_id: declared_id.clone(),
                output: output.to_string(),
                available: full(),
                measured_axes: (false, false),
            })
            .collect();
        scene.apply(std::slice::from_ref(surface), &instances, shaping, lua)
    }

    #[test]
    fn a_function_child_is_built_once_per_output_with_that_outputs_name() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel {
                    id = "wall",
                    child = function(output)
                        return rect { width = output == "LEFT" and 10 or 20, height = 5 }
                    end,
                }"#,
        );

        apply_on_two_outputs(&mut scene, &surface, &shaping, &lua).unwrap();

        assert_eq!(scene.surface("wall@LEFT").unwrap().children[0].rect.width, 10.0);
        assert_eq!(scene.surface("wall@RIGHT").unwrap().children[0].rect.width, 20.0);
    }

    #[test]
    fn a_function_child_returning_nil_maps_the_instance_empty() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel {
                    id = "wall",
                    child = function(output)
                        if output == "LEFT" then return rect { width = 10, height = 5 } end
                    end,
                }"#,
        );

        apply_on_two_outputs(&mut scene, &surface, &shaping, &lua).unwrap();

        assert_eq!(scene.surface("wall@LEFT").unwrap().children.len(), 1);
        assert!(scene.surface("wall@RIGHT").unwrap().children.is_empty());
    }

    #[test]
    fn a_function_child_on_a_window_is_refused_and_a_non_node_return_names_child() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table =
            lua.load(r#"return window { id = "w", child = function() return rect {} end }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err().to_string();
        assert!(err.contains("child") && err.contains("window"), "{err}");

        let table: mlua::Table =
            lua.load(r#"return panel { id = "p", child = function() return 4 end }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err().to_string();
        assert!(err.contains("child") && err.contains("node table"), "{err}");
    }

    #[test]
    fn a_geometry_signal_reads_the_nodes_absolute_rect_after_the_pass_without_dirtying_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"g = geometry("card")
            return panel { id = "bar", child = column { padding = { left = 10, top = 5 }, children = {
                rect { width = 30, height = 20, geometry = g } } } }"#,
        );
        let dirty = crate::lua::signal::DirtyFlag::new();
        crate::lua::signal::register(&lua, dirty.clone()).unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let (x, y, h): (f32, f32, f32) = lua.load("local r = g:get() return r.x, r.y, r.height").eval().unwrap();
        assert_eq!((x, y, h), (10.0, 5.0, 20.0));
        assert!(!dirty.take(), "the pass itself does not dirty; the client decides on one follow-up");
        assert!(!crate::lua::signal::take_geometry_moved(&lua).is_empty(), "the first measurement is a change");
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(crate::lua::signal::take_geometry_moved(&lua).is_empty(), "an unchanged rect notes nothing");
        let err = lua.load("g:set(1)").exec().unwrap_err().to_string();
        assert!(err.contains("a geometry"), "{err}");
    }

    #[test]
    fn a_hidden_subtree_is_frozen_rather_than_rebuilt_and_thaws_with_its_ids() {
        // The closed picker case (ADR-0124): while `visible` is false no item function runs and
        // the retained children stay, ids included, so showing it again pairs against them.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                built = 0
                return panel { id = "bar", child = column { visible = state("open", true),
                    children = { list { source = state("items", { "a", "b" }), itemfn = function(name)
                        built = built + 1
                        return rect { width = 10, height = 10 }
                    end } } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let built = || lua.globals().get::<i64>("built").unwrap();

        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let shown = scene.surface("bar@TEST").unwrap();
        let ids_before: Vec<NodeId> = shown.children[0].children[0].children.iter().map(|c| c.id).collect();
        assert_eq!(ids_before.len(), 2);
        assert_eq!(built(), 2);

        lua.load(r#"state("open", true):set(false)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let hidden = scene.surface("bar@TEST").unwrap();
        assert!(!hidden.children[0].visible);
        let frozen: Vec<NodeId> = hidden.children[0].children[0].children.iter().map(|c| c.id).collect();
        assert_eq!(frozen, ids_before, "the hidden subtree keeps what it had");
        assert_eq!(built(), 2, "no item function ran for a hidden list");

        lua.load(r#"state("open", true):set(true)"#).exec().unwrap();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let thawed = scene.surface("bar@TEST").unwrap();
        let ids_after: Vec<NodeId> = thawed.children[0].children[0].children.iter().map(|c| c.id).collect();
        assert_eq!(ids_after, ids_before, "showing it again pairs the fresh items with the frozen nodes");
        assert_eq!(built(), 4);
        assert_eq!(thawed.children[0].children[0].children[1].rect.y, 10.0, "and lays them out again");
    }

    #[test]
    fn a_child_removed_while_hidden_plays_no_exit_on_thaw() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { visible = state("open", true),
                children = { list { source = state("items", { "a", "b" }), itemfn = function(name)
                    return rect { id = name, width = 10, height = 10,
                        animate = { exit = { duration = 100, opacity = 0 } } }
                end } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("open", true):set(false) state("items", {}):set({ "b" })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("open", true):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let list = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert_eq!(
            list.children.len(),
            1,
            "a is gone, not leaving: {:?}",
            list.children.iter().map(|c| c.leaving).collect::<Vec<_>>()
        );
    }

    /// The 2026-09-08 lock screen: `on \`lock_screen@eDP-1\`` and a property name, on a surface
    /// holding a dozen `text` nodes, named none of them.
    #[test]
    fn a_bad_property_names_the_walk_that_reached_it_not_just_the_surface() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { children = {
                   text { content = "fine" },
                   row { children = { text { content = "also fine" }, text { content = 5 } } },
               } } }"#,
        );

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();

        let LayoutError::InvalidProperty { property, detail } = &err else { panic!("got {err:?}") };
        assert_eq!(property, "content");
        assert_eq!(
            detail,
            "on `bar@TEST`: column[0] > row[1] > text[1] > expected a string or an array of runs, got Integer(5)",
            "the path must lead to the guilty node, and neither sibling text node is on it"
        );
    }

    #[test]
    fn reapplying_the_same_shape_at_the_same_index_reuses_the_node_id() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let first_id = {
            let key = "bar@TEST";
            scene.surfaces.get(key).unwrap().children[0].id
        };

        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = rect { width = 99, height = 99 } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let second_id = scene.surfaces.get("bar@TEST").unwrap().children[0].id;

        assert_eq!(first_id, second_id, "same kind at the same position must reuse the retained node's identity");
        assert_eq!(
            scene.surface("bar@TEST").unwrap().children[0].rect.width,
            99.0,
            "but its geometry must still refresh"
        );
    }

    #[test]
    fn a_kind_change_at_the_same_index_allocates_a_new_identity() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let old_id = scene.surface("bar@TEST").unwrap().children[0].id;

        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = text { content = "hi" } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].kind, "text");
        assert_ne!(scene.surface("bar@TEST").unwrap().children[0].id, old_id);
    }

    #[test]
    fn a_shrinking_child_list_removes_the_tail() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 }, rect { width = 2, height = 2 } } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();

        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 } } } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children.len(), 1);
        assert_eq!(scene.census().1, 3, "only the panel, row, and remaining rect survive");
    }

    #[test]
    fn removed_subtree_values_survive_rollback_and_are_reclaimed_on_success() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, original) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "gone", children = { rect {} } },
            } } }"#,
        );
        apply_at(&mut scene, &[original], full(), &shaping, &lua).unwrap();

        // Only the retained properties own these tables. No VirtualNode, resolved snapshot,
        // or Lua closure keeps them alive; the observer has weak values.
        let weak: mlua::Table = lua.load(r#"return setmetatable({}, { __mode = "v" })"#).eval().unwrap();
        let outer = &mut scene.surfaces.get_mut("bar@TEST").unwrap().children[0].children[0];
        for (index, node) in
            std::iter::once(&mut outer.properties).chain(std::iter::once(&mut outer.children[0].properties)).enumerate()
        {
            let value = lua.create_table().unwrap();
            weak.set(index + 1, value.clone()).unwrap();
            node.insert("lifetime_probe", Value::Table(value));
        }
        let next_id_before = scene.next_id;
        let instances = [SurfaceInstance {
            instance_id: "bar@TEST".to_string(),
            declared_id: "bar".to_string(),
            output: "TEST".to_string(),
            available: full(),
            measured_axes: (false, false),
        }];
        let parse = |source: &str| {
            let table: mlua::Table = lua.load(source).eval().unwrap();
            deserialize_lua_table(&table).unwrap()
        };

        // Matching drops the old subtree before the second fresh child fails its parse.
        let invalid = parse(
            r#"return panel { id = "bar", child = row { children = {
            rect { id = "new" }, rect { width = "invalid" },
        } } }"#,
        );
        assert!(matches!(
            scene.apply(&[invalid], &instances, &shaping, &lua),
            Err(LayoutError::InvalidProperty { property, .. }) if property == "width"
        ));
        lua.gc_collect().unwrap();
        assert!(weak.get::<Option<mlua::Table>>(1).unwrap().is_some());
        assert!(weak.get::<Option<mlua::Table>>(2).unwrap().is_some());
        assert_eq!(scene.next_id, next_id_before);

        let replacement = parse(
            r#"return panel { id = "bar", child = row { children = {
            rect { id = "new" },
        } } }"#,
        );
        let error = scene
            .apply_admitting(std::slice::from_ref(&replacement), &instances, &shaping, &lua, |_| {
                lua.gc_collect().unwrap();
                assert!(weak.get::<Option<mlua::Table>>(1).unwrap().is_some());
                assert!(weak.get::<Option<mlua::Table>>(2).unwrap().is_some());
                Err(node::invalid("child", "veto"))
            })
            .unwrap_err();
        assert!(matches!(error, LayoutError::InvalidProperty { property, .. } if property == "child"));
        lua.gc_collect().unwrap();
        assert!(weak.get::<Option<mlua::Table>>(1).unwrap().is_some());
        assert!(weak.get::<Option<mlua::Table>>(2).unwrap().is_some());
        assert_eq!(scene.next_id, next_id_before);
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children[0].children.len(), 1);

        scene.apply(&[replacement], &instances, &shaping, &lua).unwrap();
        lua.gc_collect().unwrap();
        assert!(weak.get::<Option<mlua::Table>>(1).unwrap().is_none());
        assert!(weak.get::<Option<mlua::Table>>(2).unwrap().is_none());
        assert_eq!(scene.census().1, 3);
    }

    #[test]
    fn an_identified_child_keeps_its_node_id_across_applies_when_a_sibling_is_inserted_above_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "keep", width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let keep_id_before = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 5, height = 5 },
                rect { id = "keep", width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0];

        assert_eq!(
            row.children[1].id, keep_id_before,
            "the id-matched sibling must keep its NodeId despite the insertion above it"
        );
    }

    #[test]
    fn an_unidentified_child_list_keeps_node_ids_across_applies_by_position_only() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let original_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 5, height = 5 },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0];

        assert_eq!(
            row.children[0].id, original_id,
            "position 0 still reuses the retained identity, since matching stays positional"
        );
        assert_ne!(
            row.children[1].id, original_id,
            "position 1 is a fresh allocation, not a reused identity -- matching today's rule with no ids present"
        );
    }

    #[test]
    fn a_mixed_child_list_keeps_identified_node_ids_across_applies_and_the_rest_only_by_position() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { id = "anchor", width = 2, height = 2 },
                rect { width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (b_id_before, anchor_id_before, c_id_before) = {
            let root = scene.surfaces.get("bar@TEST").unwrap();
            let row = &root.children[0].children;
            (row[0].id, row[1].id, row[2].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { id = "anchor", width = 2, height = 2 },
                rect { width = 9, height = 9 },
                rect { width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let root = scene.surfaces.get("bar@TEST").unwrap();
        let row2 = &root.children[0].children;

        assert_eq!(row2.len(), 4);
        assert_eq!(row2[0].id, b_id_before, "B sits ahead of the insertion, so it keeps its slot either way");
        assert_eq!(row2[1].id, anchor_id_before, "the identified sibling keeps its id regardless of position");
        assert_ne!(
            row2[3].id, c_id_before,
            "C is unidentified, so the inserted node claims its retained slot positionally -- same rule an id-less list already had"
        );
    }

    #[test]
    fn duplicate_sibling_ids_are_rejected_as_a_layout_error() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "dup", width = 1, height = 1 },
                rect { id = "dup", width = 2, height = 2 },
            } } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "id" && detail.contains("dup")),
            "duplicate sibling ids must be rejected, naming the offending id: {err:?}"
        );
    }

    #[test]
    fn the_same_id_under_two_different_parents_does_not_collide() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                column { children = { rect { id = "inner", width = 1, height = 1 } } },
                column { children = { rect { id = "inner", width = 2, height = 2 } } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(
            row.children.len(),
            2,
            "two columns, each with its own 'inner' child, must not collide across parents"
        );
        assert_eq!(row.children[0].children[0].rect.width, 1.0);
        assert_eq!(row.children[1].children[0].rect.width, 2.0);
    }

    #[test]
    fn a_signal_valued_id_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("x").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("sig", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { id = sig, width = 1, height = 1 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::UnsupportedSignalProperty(p) if p == "id"),
            "a Signal-valued id must be rejected outright, not resolved: {err:?}"
        );
    }

    #[test]
    fn an_empty_child_list_removes_the_identified_subtree() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "gone", width = 1, height = 1, children = { rect { width = 1, height = 1 } } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = row { children = {} } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert!(scene.surface("bar@TEST").unwrap().children[0].children.is_empty());
        assert_eq!(scene.census().1, 2);
    }

    #[test]
    fn a_fresh_id_gets_a_new_node_id_and_the_vanished_id_is_removed() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "a", width = 1, height = 1 },
                rect { id = "b", width = 2, height = 2 },
                rect { id = "c", width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (a_id, b_id, c_id) = {
            let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (row[0].id, row[1].id, row[2].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "b", width = 2, height = 2 },
                rect { id = "c", width = 3, height = 3 },
                rect { id = "d", width = 4, height = 4 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(row[0].id, b_id, "b keeps its identity across the apply");
        assert_eq!(row[1].id, c_id, "c keeps its identity across the apply");
        assert!(
            row[2].id != a_id && row[2].id != b_id && row[2].id != c_id,
            "d declared a new id, so it must get a fresh NodeId rather than adopt a retained one"
        );
        assert!(row.iter().all(|child| child.id != a_id));
    }

    #[test]
    fn an_anonymous_fresh_child_never_inherits_an_identified_retained_node() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "x", width = 1, height = 1 },
                rect { width = 2, height = 2 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (x_id, anon_id) = {
            let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (row[0].id, row[1].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { width = 2, height = 2 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(
            row[0].id, anon_id,
            "the unidentified subsequence pairs among itself, so the retained anonymous node lands in slot 0"
        );
        assert!(row[1].id != x_id && row[1].id != anon_id, "slot 1 has no unidentified counterpart left, so it is new");
        assert!(row.iter().all(|child| child.id != x_id));
    }

    #[test]
    fn removing_an_id_replaces_the_old_node_with_a_new_identity() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { id = "x", width = 1, height = 1 } } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let x_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 } } } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_ne!(row[0].id, x_id, "dropping the id allocates a new node rather than silently preserving identity");
        assert!(row.iter().all(|child| child.id != x_id));
    }

    #[test]
    fn a_vanished_id_removes_its_subtree_even_when_fresh_slots_are_unmatched() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "gone", width = 1, height = 1, children = { rect { width = 1, height = 1 } } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (outer_id, inner_id) = {
            let outer = &scene.surfaces.get("bar@TEST").unwrap().children[0].children[0];
            (outer.id, outer.children[0].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "other", width = 1, height = 1 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_ne!(row[0].id, outer_id, "`other` must not adopt `gone`'s retained node");
        assert_ne!(row[0].id, inner_id);
        assert!(row[0].children.is_empty());
        assert_eq!(scene.census().1, 3);
    }

    #[test]
    fn many_identified_siblings_reconcile_in_reversed_order_without_a_quadratic_scan() {
        const N: usize = 2000;
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();

        let mut v1_children = String::new();
        for i in 0..N {
            v1_children.push_str(&format!("rect {{ id = \"n{i}\", width = 1, height = 1 }},\n"));
        }
        let (_lua1, surface_v1) =
            surface_from(&format!("panel {{ id = \"bar\", child = row {{ children = {{ {v1_children} }} }} }}"));
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let before: Vec<NodeId> =
            scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        let mut v2_children = String::new();
        v2_children.push_str("rect { id = \"fresh\", width = 1, height = 1 },\n");
        for i in (1..N).rev() {
            v2_children.push_str(&format!("rect {{ id = \"n{i}\", width = 1, height = 1 }},\n"));
        }
        let (_lua2, surface_v2) =
            surface_from(&format!("panel {{ id = \"bar\", child = row {{ children = {{ {v2_children} }} }} }}"));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let after: Vec<NodeId> =
            scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        assert_eq!(after.len(), N);
        for (slot, i) in (1..N).rev().enumerate() {
            assert_eq!(after[slot + 1], before[i], "n{i} must keep its NodeId across the reversal");
        }
        assert!(!before.contains(&after[0]), "the never-seen `fresh` id must allocate rather than inherit n0's node");
        assert!(!after.contains(&before[0]), "n0 left the config");
    }

    /// The three tests below share one shape: a `margin` that is a `computed` signal counting its
    /// own reads into a Lua global, so what a single `Scene::apply` does with that property is
    /// observable from the config's side.
    fn surface_with_a_read_counting_margin(lua: &mlua::Lua) -> VirtualNode {
        register_node_constructors(lua).unwrap();
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                reads = 0
                local m = computed({}, function() reads = reads + 1; return { left = reads } end)
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10, margin = m },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        deserialize_lua_table(&table).unwrap()
    }

    /// The shape resolving-once does not close: a plain Lua table with an `__index`, no `Signal`
    /// anywhere. Every metamethod-aware `Table::get` re-runs the metamethod, so `margin` must be
    /// parsed once per child per pass or its answers are free to differ.
    fn surface_with_an_index_counting_margin(lua: &mlua::Lua) -> VirtualNode {
        register_node_constructors(lua).unwrap();
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                indexes = 0
                local m = setmetatable({}, { __index = function(_, key)
                    indexes = indexes + 1
                    if key == "left" then return indexes end
                    return 0
                end })
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10, margin = m },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        deserialize_lua_table(&table).unwrap()
    }

    /// One parse, four keys. It was 16 invocations before `LayoutStyle`: the parent's child loop,
    /// both sizing folds and the positioning pass, each reading `top`, `right`,
    /// `bottom` and `left` off the same table.
    #[test]
    fn a_margin_table_is_read_exactly_once_per_node_per_pass() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_an_index_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        assert_eq!(
            lua.globals().get::<u32>("indexes").unwrap(),
            4,
            "one `parse_edge_insets` over four keys, not one per reader"
        );
    }

    /// The defect that count caused, pinned directly. An `__index` answering `left` with a fresh
    /// number each read made the sizing pass and the positioning pass disagree: measured before
    /// this fix, a row measured itself 18 wide and then placed its 10-wide child spanning 16..26,
    /// eight pixels outside the parent it had just been sized to fit.
    #[test]
    fn an_index_metamethod_cannot_make_the_two_passes_disagree() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_an_index_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let child = &row.children[0];
        assert_eq!(
            child.rect.x + child.rect.width,
            row.rect.width,
            "the margin the row was measured with must be the margin its child was positioned with: \
             the child spans {}..{} inside a {}-wide row",
            child.rect.x,
            child.rect.x + child.rect.width,
            row.rect.width
        );
    }

    #[test]
    fn an_impure_margin_closure_positions_a_child_inside_the_size_its_parent_was_measured_at() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_a_read_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        let row = &root.children[0];
        let child = &row.children[0];
        assert_eq!(
            child.rect.x + child.rect.width,
            row.rect.width,
            "the margin the row was measured with must be the margin its child was positioned with: the child spans {}..{} inside a {}-wide row",
            child.rect.x,
            child.rect.x + child.rect.width,
            row.rect.width
        );
    }

    #[test]
    fn a_signal_valued_property_resolves_exactly_once_per_apply() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_a_read_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        assert_eq!(
            lua.globals().get::<i64>("reads").unwrap(),
            1,
            "one Scene::apply must read a property's Signal exactly once"
        );
    }

    /// The guarantee most likely to be lost by a later edit: every getter fires exactly once, in
    /// the order the config wrote it. An impure closure like this one is what can observe it.
    ///
    /// The `Fill` child is declared first and resolves first: the solver does its own sizing,
    /// so there is one walk in declaration order, `aAbB`, the tree read top to bottom.
    #[test]
    fn every_getter_fires_exactly_once_in_the_order_the_config_wrote_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                order = ""
                local function mark(name, value)
                    return computed({}, function() order = order .. name; return value end)
                end
                return panel { id = "bar", child = row { width = 600, height = 40, children = {
                    rect { width = "Fill", height = 10, margin = mark("a", 0),
                        children = { rect { width = 1, height = 1, margin = mark("A", 0) } } },
                    rect { width = 100, height = 10, margin = mark("b", 0),
                        children = { rect { width = 1, height = 1, margin = mark("B", 0) } } },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        // Four characters for four getters is the "exactly once" half. Their order is the other
        // half: depth-first, declaration order, which is the order the Lua above reads.
        assert_eq!(
            lua.globals().get::<String>("order").unwrap(),
            "aAbB",
            "every getter fires exactly once, in the order the config wrote it"
        );
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 500.0, "and the fill child was still sized from the remainder");
    }

    #[test]
    fn a_second_apply_resolves_the_property_again_rather_than_reusing_the_first_passes_answer() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_a_read_counting_margin(&lua);

        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        assert_eq!(lua.globals().get::<i64>("reads").unwrap(), 2, "each apply resolves afresh");
    }

    #[test]
    fn the_resolved_tree_holds_a_signals_current_value_not_the_handle() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let white = lua.create_string("#FFFFFF").unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(white), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("bg", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { background = bg, width = 4, height = 4 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let child = &scene.surface("bar@TEST").unwrap().children[0];
        let background = child.properties.get("background").expect("background must survive into the resolved tree");
        assert_eq!(
            background.as_string().map(|s| s.to_string_lossy()),
            Some("#FFFFFF".to_string()),
            "the resolved tree must hold the value, not the Signal handle: {background:?}"
        );
    }

    #[test]
    fn a_child_whose_kind_is_rejected_never_runs_its_property_getters() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                ran = false
                local w = computed({}, function() ran = true; return 10 end)
                return panel { id = "bar", child = row { children = {
                    { kind = "banana", width = w },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();

        assert!(
            matches!(&err, LayoutError::UnsupportedNodeKind(kind) if kind == "banana"),
            "the error must be unchanged by moving the check earlier: {err:?}"
        );
        assert!(!lua.globals().get::<bool>("ran").unwrap(), "a rejected child's property getters must not run");
    }

    #[test]
    fn a_list_resolves_one_child_per_source_element_in_source_order() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                source = { 10, 20, 30 },
                itemfn = function(item) return rect { width = item, height = 5 } end,
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let list = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(list.children.len(), 3);
        assert_eq!(list.children[0].rect.width, 10.0);
        assert_eq!(list.children[1].rect.width, 20.0);
        assert_eq!(list.children[2].rect.width, 30.0);
    }

    #[test]
    fn a_list_with_key_keeps_existing_items_node_ids_when_a_new_element_is_inserted_at_the_front() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let itemfn = r#"function(item) return rect { width = item.n, height = 1 } end"#;
        let key = r#"function(item) return item.id end"#;
        let (_lua1, surface_v1) = surface_from(&format!(
            r#"panel {{ id = "bar", child = list {{
                source = {{ {{ id = "a", n = 1 }}, {{ id = "b", n = 2 }}, {{ id = "c", n = 3 }} }},
                key = {key},
                itemfn = {itemfn},
            }} }}"#
        ));
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (a_id, b_id, c_id) = {
            let list = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (list[0].id, list[1].id, list[2].id)
        };

        let (_lua2, surface_v2) = surface_from(&format!(
            r#"panel {{ id = "bar", child = list {{
                source = {{ {{ id = "z", n = 9 }}, {{ id = "a", n = 1 }}, {{ id = "b", n = 2 }}, {{ id = "c", n = 3 }} }},
                key = {key},
                itemfn = {itemfn},
            }} }}"#
        ));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let list = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(list.len(), 4);
        assert_eq!(list[1].id, a_id, "a kept its retained node despite z inserted above it");
        assert_eq!(list[2].id, b_id, "b kept its retained node despite z inserted above it");
        assert_eq!(list[3].id, c_id, "c kept its retained node despite z inserted above it");
        assert!(
            list[0].id != a_id && list[0].id != b_id && list[0].id != c_id,
            "z is a genuinely new key, so it must get a freshly allocated node, not one borrowed from a's old slot"
        );
    }

    #[test]
    fn a_list_without_key_rebuilds_every_item_from_the_insertion_point_on() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let itemfn = r#"function(item) return rect { width = item, height = 1 } end"#;
        let (_lua1, surface_v1) =
            surface_from(&format!(r#"panel {{ id = "bar", child = list {{ source = {{ 1 }}, itemfn = {itemfn} }} }}"#));
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let x_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(&format!(
            r#"panel {{ id = "bar", child = list {{ source = {{ 2, 1 }}, itemfn = {itemfn} }} }}"#
        ));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let list = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].id, x_id,
            "position 0 reuses the old retained node regardless of which logical item now occupies it"
        );
        assert_ne!(
            list[1].id, x_id,
            "the item that used to be first is now at position 1, a position with no retained counterpart, so it gets a fresh node instead of keeping x's"
        );
    }

    #[test]
    fn a_duplicate_list_key_is_rejected_naming_key_not_id() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                source = { { id = "dup" }, { id = "dup" } },
                key = function(item) return item.id end,
                itemfn = function(item) return rect { width = 1, height = 1 } end,
            } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "key" && detail.contains("dup")),
            "duplicate list keys must be rejected naming `key`, not `id`: {err:?}"
        );
    }

    #[test]
    fn a_list_with_no_source_lays_out_empty() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(r#"panel { id = "bar", child = list { itemfn = function(item) return rect {} end } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        assert!(scene.surface("bar@TEST").unwrap().children[0].children.is_empty());
    }

    #[test]
    fn a_list_source_that_is_not_a_table_is_a_layout_error_naming_source() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = 5, itemfn = function(item) return rect {} end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "source"), "{err:?}");
    }

    #[test]
    fn a_list_with_no_itemfn_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = list { source = { 1 } } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "itemfn"), "{err:?}");
    }

    #[test]
    fn a_list_itemfn_that_is_not_a_function_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = list { source = { 1 }, itemfn = "nope" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "itemfn"), "{err:?}");
    }

    #[test]
    fn a_list_itemfn_raising_a_lua_error_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = { 1 }, itemfn = function(item) error("boom") end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "itemfn" && detail.contains("boom")),
            "{err:?}"
        );
    }

    #[test]
    fn a_list_itemfn_returning_a_non_node_table_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = { 1 }, itemfn = function(item) return 5 end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "itemfn"), "{err:?}");
    }

    #[test]
    fn a_list_key_that_is_not_a_function_is_a_layout_error_naming_key() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = { 1 }, key = "nope", itemfn = function(item) return rect {} end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "key"), "{err:?}");
    }

    #[test]
    fn a_list_key_returning_a_non_string_is_a_layout_error_naming_key() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                source = { 1 },
                key = function(item) return 5 end,
                itemfn = function(item) return rect {} end,
            } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "key"), "{err:?}");
    }

    #[test]
    fn a_list_with_an_empty_source_has_no_children_and_does_not_error() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = {}, itemfn = function(item) return rect {} end } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let list = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(list.children.len(), 0);
    }

    #[test]
    fn a_list_sizes_and_positions_like_a_column() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                spacing = 3,
                source = { 1, 2 },
                itemfn = function(item) return rect { width = 6, height = 10 } end,
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let list = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(list.rect.width, 6.0, "own width is the widest child, same formula as column");
        assert_eq!(list.rect.height, 23.0, "10 + 10 + 3 spacing, same formula as column");
        assert_eq!(list.children[0].rect.y, 0.0);
        assert_eq!(list.children[1].rect.y, 13.0, "second item stacks below the first plus spacing");
    }

    #[test]
    fn window_and_popup_are_supported_kinds_carrying_a_single_child() {
        for kind in ["window", "popup"] {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&format!(
                r#"{{ kind = "{kind}", id = "s", child = rect {{ width = 40, height = 20 }} }}"#
            ));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let root = scene.surface("s@TEST").unwrap();
            assert_eq!(root.kind, kind);
            assert_eq!(root.children.len(), 1, "`{kind}` must carry its `child`");
            assert_eq!(root.children[0].rect.width, 40.0);
            assert_eq!(root.children[0].rect.height, 20.0);
        }
    }

    #[test]
    fn a_window_or_popup_root_stacks_and_stretches_its_child_exactly_as_a_panel_does() {
        for kind in ["window", "popup"] {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&format!(
                r#"{{ kind = "{kind}", id = "s", width = 100, height = 50,
                       child = rect {{ align_h = "Stretch", align_v = "Stretch" }} }}"#
            ));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let child = &scene.surface("s@TEST").unwrap().children[0];
            assert_eq!(child.rect.width, 100.0, "`{kind}` must stretch its child like `panel`");
            assert_eq!(child.rect.height, 50.0);
        }
    }

    #[test]
    fn an_unsized_window_root_is_the_surface_so_a_fill_child_actually_fills_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(r#"{ kind = "window", id = "settings", child = rect { width = "Fill", height = "Fill" } }"#);
        apply_at(&mut scene, &[surface], LogicalSize { width: 1920.0, height: 1168.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("settings@TEST").unwrap();
        assert_eq!((root.rect.width, root.rect.height), (1920.0, 1168.0));
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (1920.0, 1168.0));
    }

    #[test]
    fn a_window_root_that_does_write_a_size_still_gets_the_size_it_wrote() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"{ kind = "window", id = "settings", width = 400, child = rect { width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], LogicalSize { width: 1920.0, height: 1168.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("settings@TEST").unwrap();
        assert_eq!(
            (root.rect.width, root.rect.height),
            (400.0, 1168.0),
            "the written width stands; the unwritten height fills"
        );
    }

    #[test]
    fn a_popup_root_with_both_sizes_needs_no_forcing() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"{ kind = "popup", id = "menu", parent = "bar", width = 200, height = 120,
                 anchor_rect = { x = 0, y = 0, width = 86, height = 24 },
                 child = rect { width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], LogicalSize { width: 200.0, height: 120.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("menu@TEST").unwrap();
        assert_eq!((root.rect.width, root.rect.height), (200.0, 120.0));
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (200.0, 120.0));
    }

    #[test]
    fn a_lock_is_a_supported_kind_carrying_a_single_child_and_stretching_it_like_a_panel() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"{ kind = "lock", id = "screen-lock",
                 child = rect { align_h = "Stretch", align_v = "Stretch" } }"#,
        );
        apply_at(&mut scene, &[surface], LogicalSize { width: 1920.0, height: 1080.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("screen-lock@TEST").unwrap();
        assert_eq!(root.kind, "lock");
        assert_eq!(root.children.len(), 1, "a lock's `child`, read through the same `parse_single_child` a panel's is");
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (1920.0, 1080.0));
    }

    #[test]
    fn an_unsized_lock_root_is_its_output_so_a_fill_child_covers_the_locked_screen() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(r#"{ kind = "lock", id = "screen-lock", child = rect { width = "Fill", height = "Fill" } }"#);
        apply_at(&mut scene, &[surface], LogicalSize { width: 2560.0, height: 1440.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("screen-lock@TEST").unwrap();
        assert_eq!((root.rect.width, root.rect.height), (2560.0, 1440.0));
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (2560.0, 1440.0));
    }
}

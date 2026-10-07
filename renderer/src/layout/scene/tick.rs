use std::time::{Duration, Instant};

use mlua::{Lua, Value};
use shared::debug;

use super::pass::{prior_position, publish_geometry, solve_instance};
use super::resolve::drop_injected_sizes;
use super::solver::{
    MainAxis, Measure, hold_leavers, main_axis_of, measure_for, new_solver_node, set_solver_children, taffy_failed,
    taffy_style, update_solver_node,
};
use super::{LayoutStyle, LogicalSize, PreparedNode, ResolvedNode, Scene, close, open_span};
use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{self, Dissolve, LayoutError, PaintStyle, SizeMode};
use crate::text::shaping::ShapingHandle;

/// Where a tick's relayouts spend themselves, for `--profile`. A paint-only tick is in none of
/// these, so `ms tick` less their sum is that path plus the geometry writes.
#[derive(Clone, Copy, Default)]
pub struct TickSplit {
    pub clone: Duration,
    pub prepare: Duration,
    pub solve: Duration,
}

impl Scene {
    /// Drained like [`Scene::take_resolve_split`].
    pub fn take_tick_split(&mut self) -> TickSplit {
        std::mem::take(&mut self.tick_split)
    }

    /// Advances the tweens of `instances` to `now` and lays the affected ones out again from their retained
    /// property maps, without running Lua (ADR-0145): the only Lua the retained walk touches is a
    /// plain table read. Returns the instances it advanced, which is what the caller owes the
    /// screen this frame. An instance whose relayout fails keeps its last tree and loses its
    /// tweens, so a failure is one log line and a snap rather than a log line per frame.
    ///
    /// A tree whose every running tween only changes what it paints skips the relayout entirely
    /// and is advanced where it stands ([`advance_paint_only`]). That is most of what a config
    /// animates -- a fade, a hover colour, a border lighting up -- and none of it can move a rect.
    pub fn tick<'a>(
        &mut self,
        instances: impl IntoIterator<Item = &'a SurfaceInstance>,
        shaping: &ShapingHandle,
        lua: &Lua,
        now: Instant,
    ) -> Vec<String> {
        // Animated edge tables can still run `__index` during parsing, so ticks share the pass budget.
        let budget = match crate::lua::signal::LayoutPassBudget::enter(lua) {
            Ok(budget) => budget,
            Err(err) => {
                debug!("tick: no pass budget, skipping the frame: {err}");
                return Vec::new();
            }
        };
        let mut relaid = Vec::new();
        let mut measured = false;
        for instance in instances {
            let key = instance.instance_id.as_str();
            let Some(retained) = self.surfaces.get_mut(key).filter(|tree| tree.animating()) else { continue };
            // Recorded before the advance, not after: the frame that ends a tween is the one that
            // shows its target, and by the time it has been written the tree is no longer
            // `animating`. Selecting afterwards would drop exactly that frame.
            relaid.push(key.to_string());
            if retained.tick_is_paint_only() {
                let advanced = advance_paint_only(retained, now, lua)
                    .and_then(|()| if budget.exceeded() { Err(LayoutError::PassBudgetExceeded) } else { Ok(()) });
                if let Err(err) = advanced {
                    debug!("{key}: advancing a paint-only tween failed, stopping it: {err}");
                    strip_tweens(retained, lua);
                }
                continue;
            }
            // ponytail: the clone is the rollback. A relayout consumes the tree and can fail
            // partway: an animated edge table's `__index` can run during re-parse, and a tweened
            // value can be refused. About 18us on the 162-node `tick_cost` tree; an undo
            // path through `prepare_retained` and `finish` is the upgrade if it ever dominates.
            let mut at = open_span();
            let mut root = retained.clone();
            close(&mut at, &mut self.tick_split.clone);
            let mut solver = Scene::take_solver_tree(&mut self.solver_trees, key, [&mut root]);
            let outcome = relayout_retained(
                &mut solver,
                root,
                instance.available,
                (shaping, &self.field_drafts),
                lua,
                now,
                &mut at,
                &mut self.tick_split,
            )
            .and_then(|tree| if budget.exceeded() { Err(LayoutError::PassBudgetExceeded) } else { Ok(tree) });
            match outcome {
                Ok(tree) => {
                    // Geometry readers update when the tween settles (ADR-0131).
                    if let Err(err) = publish_geometry(&tree, 0.0, 0.0, lua, true) {
                        debug!("{key}: writing a geometry signal failed: {err}");
                    }
                    if !tree.animating() {
                        note_settled_geometry(&tree, lua);
                    }
                    self.solver_trees.insert(key.to_string(), solver);
                    *retained = tree;
                    measured = true;
                }
                Err(err) => {
                    debug!("{key}: relaying out a tween failed, snapping it: {err}");
                    strip_tweens(retained, lua);
                }
            }
        }
        if measured {
            self.publish_elision(lua);
        }
        relaid
    }
}

/// Every in-flow `geometry` rect under `node` as moved: the quiet ticks wrote them, and a reader
/// the last pass resolved has not seen the value they settled on.
fn note_settled_geometry(node: &ResolvedNode, lua: &Lua) {
    if !node.in_flow() {
        return;
    }
    if let Some((id, _)) = node::signal_at(&node.properties, "geometry").and_then(|signal| signal.geometry_cell()) {
        crate::lua::signal::note_layout_changed(lua, id);
    }
    node.children.iter().for_each(|child| note_settled_geometry(child, lua));
}

fn strip_tweens(node: &mut ResolvedNode, lua: &Lua) {
    // What a typed tween left on screen goes into the map before the tween, its only record, is dropped.
    if let Err(err) = sync_shown(node, lua) {
        debug!("tween strip: {err}");
    }
    node.tweens.clear();
    node.children.iter_mut().for_each(|child| strip_tweens(child, lua));
}

/// [`node::sync`] for a node's own map, copied only when a typed tween is ahead of it.
fn sync_shown(node: &mut ResolvedNode, lua: &Lua) -> Result<(), LayoutError> {
    if node.tweens.iter().all(|tween| tween.shown.is_none()) {
        return Ok(());
    }
    node::sync(&mut node.tweens, std::rc::Rc::make_mut(&mut node.properties), lua)
}

/// One instance laid out again from what it retained, its tweens advanced to `now`. The Lua-free
/// twin of `Scene::apply_one_instance` plus [`prepare`]: no signal is read, no item function
/// called, no id allocated; every node keeps its identity and its resolved values, and only the
/// properties a tween carries move.
///
/// [`prepare`]: super::pass::prepare
#[allow(clippy::too_many_arguments)]
fn relayout_retained(
    tree: &mut taffy::TaffyTree<Measure>,
    root: ResolvedNode,
    available: LogicalSize,
    (shaping, drafts): (&ShapingHandle, &super::field::FieldDrafts),
    lua: &Lua,
    now: Instant,
    at: &mut Option<Instant>,
    split: &mut TickSplit,
) -> Result<ResolvedNode, LayoutError> {
    let prepared = prepare_retained(tree, root, (None, 0.0), lua, now, false, false)?;
    close(at, &mut split.prepare);
    let solved = solve_instance(tree, prepared, available, (shaping, drafts), now);
    close(at, &mut split.solve);
    solved
}

/// [`prepare`] over a retained tree: for a tick, and for a pass over a `list` item that would
/// build the same (ADR-0269). Unchanged nodes reuse their parsed style and solver node; animated
/// nodes update both, and a node with no solver node yet gets one. Hidden children stay frozen and
/// tweens advance without reconciliation.
///
/// [`prepare`]: super::pass::prepare
pub(super) fn prepare_retained(
    tree: &mut taffy::TaffyTree<Measure>,
    mut node: ResolvedNode,
    parent_flow: (Option<MainAxis>, f32),
    lua: &Lua,
    now: Instant,
    move_on_solve: bool,
    thawing: bool,
) -> Result<PreparedNode, LayoutError> {
    let (parent_axis, _) = parent_flow;
    let prior_position = (move_on_solve && !thawing).then(|| prior_position(&node, tree, parent_flow)).flatten();
    let prior_size = (move_on_solve && !thawing).then_some((node.rect.width, node.rect.height));
    let old_scroll = node.scrolled;
    if thawing {
        node.movement = None;
    }
    if !move_on_solve && node.visible {
        node.movement.take_if(|movement| !movement.advance(now));
    }
    let changed = node.tweens.iter().any(|tween| !tween.resting);
    // Paint-only motion leaves the box as it was: the geometry parse and the solver node stand.
    let paint_only = node.tweens.iter().all(|tween| tween.resting || node::is_paint_only(tween.property));
    let painted = paints(&node.tweens);
    let samples = node::step(&mut node.tweens, &mut node.properties, now, lua, paint_only)?;
    // A tick keeps the size its running tween pins; a pass measures again.
    let injected = move_on_solve
        && node.resolve_memo.as_ref().is_some_and(|memo| drop_injected_sizes(&mut node.properties, memo.raw()));
    let relayout = injected || (changed && !paint_only);
    // `fit_text_to_box` rewrites a text's content only to wrap or cut it; such a paint is read fresh below.
    let refit = matches!(
        &node.paint,
        Some(PaintStyle::Text { wrap: node::Wrap::Word, .. } | PaintStyle::Text { elided: true, .. })
    );
    let changed = changed || injected;
    let style = if relayout {
        // The parse reads the map, which a typed sample has to reach first.
        node::commit(&mut node.tweens, &mut node.properties, samples, now, lua)?;
        sync_shown(&mut node, lua)?;
        std::rc::Rc::new(LayoutStyle::parse(&node.properties)?)
    } else {
        let mut style = std::rc::Rc::clone(&node.layout_style);
        if changed {
            let style = std::rc::Rc::make_mut(&mut style);
            reread(style, &mut node.paint, node.kind, &node.properties, painted && !refit, &samples)?;
        }
        node::commit(&mut node.tweens, &mut node.properties, samples, now, lua)?;
        style
    };
    let ResolvedNode {
        id,
        layout_style: _,
        taffy: old_taffy,
        kind,
        allocated_axes,
        properties,
        paint: old_paint,
        children,
        tweens,
        move_spec,
        movement,
        displayed_source,
        dissolve,
        list_memo,
        child_table,
        resolve_memo,
        ..
    } = node;
    let paint = if relayout || refit { node::paint_style(kind, &properties)? } else { old_paint };
    let taffy_id = match old_taffy {
        Some(taffy_id) => {
            if relayout {
                let measure = measure_for(id, kind, paint.as_ref(), &properties, &style)?;
                update_solver_node(tree, taffy_id, kind, &properties, &style, parent_axis, measure)?;
            }
            taffy_id
        }
        None => {
            let measure = measure_for(id, kind, paint.as_ref(), &properties, &style)?;
            new_solver_node(tree, kind, &properties, &style, parent_axis, measure)?
        }
    };
    let node = PreparedNode {
        id,
        kind,
        allocated_axes,
        children: Vec::with_capacity(if style.visible { children.len() } else { 0 }),
        style,
        properties,
        paint,
        displayed_source,
        dissolve: advanced_dissolve(dissolve, now),
        taffy: taffy_id,
        frozen: children,
        tweens,
        move_spec,
        movement,
        prior_position,
        prior_size,
        leaving: Vec::new(),
        list_memo,
        child_table,
        resolve_memo,
        scrolled: old_scroll,
    };
    if !node.style.visible {
        return Ok(node);
    }
    prepare_retained_children(tree, node, (parent_axis, old_scroll), lua, now, move_on_solve, thawing)
}

/// `node`'s retained children, in `frozen`, laid out again as they are.
fn prepare_retained_children(
    tree: &mut taffy::TaffyTree<Measure>,
    mut node: PreparedNode,
    parent_flow: (Option<MainAxis>, f32),
    lua: &Lua,
    now: Instant,
    move_on_solve: bool,
    thawing: bool,
) -> Result<PreparedNode, LayoutError> {
    let (parent_axis, old_scroll) = parent_flow;
    let own_axis = main_axis_of(node.kind, &node.properties)?;
    let had_leavers = node.frozen.iter().any(|child| child.leaving);
    for child in std::mem::take(&mut node.frozen) {
        if child.leaving {
            if let Some(child) = advance_leaving(child, now, lua)? {
                node.leaving.push(child);
            }
        } else {
            node.children.push(prepare_retained(
                tree,
                child,
                (own_axis, old_scroll),
                lua,
                now,
                move_on_solve,
                thawing,
            )?);
        }
    }
    if had_leavers {
        tree.set_style(node.taffy, taffy_style(node.kind, &node.properties, &node.style, parent_axis)?)
            .map_err(taffy_failed)?;
    }
    hold_leavers(tree, node.taffy, &node.style, node.allocated_axes, &node.leaving)?;
    set_solver_children(tree, node.taffy, &node.children)?;
    Ok(node)
}

/// A node's paint re-read from the values it now displays, keeping the text it was fitted to.
///
/// No pass measures a node this is called for, so the ellipsized prefix or the wrapped lines that
/// `finish` wrote still describe the box the node has. Everything else about the paint is
/// re-read like any other node's, which is what lets a tween move a label's `foreground` rather
/// than freeze it at the colour it last laid out with.
fn repainted_keeping_fitted_text(old: Option<PaintStyle>, fresh: Option<PaintStyle>) -> Option<PaintStyle> {
    match (old, fresh) {
        (Some(PaintStyle::Text { content, runs, elided, .. }), Some(mut fresh)) => {
            if let PaintStyle::Text { content: c, runs: r, elided: e, .. } = &mut fresh {
                (*c, *r, *e) = (content, runs, elided);
            }
            Some(fresh)
        }
        (_, fresh) => fresh,
    }
}

/// Whether a moving tween changes the paint through the map; the [`node::TYPED`] ones do not.
/// Taken before `node::step`, which updates the `resting` flags.
fn paints(tweens: &[node::Tween]) -> bool {
    tweens.iter().any(|tween| !tween.resting && !node::TYPED.contains(&tween.property))
}

/// `paint` re-read into `style` and `paint` when `painted`, then the typed `samples` written over
/// `style`. Every read runs before anything is assigned, so a refusal leaves both as they were.
fn reread(
    style: &mut LayoutStyle,
    paint: &mut Option<PaintStyle>,
    kind: &str,
    properties: &node::PropMap,
    painted: bool,
    samples: &[(&str, node::Animatable)],
) -> Result<(), LayoutError> {
    let fresh = painted.then(|| node::paint_style(kind, properties)).transpose()?;
    overlaid(style, samples)?;
    if let Some(fresh) = fresh {
        *paint = repainted_keeping_fitted_text(paint.take(), fresh);
    }
    Ok(())
}

/// `samples` in the parsed style, which is what its parser would read from the map once `to_value`
/// wrote them there. A sample of a shape the parser refuses (a keyframe list that mixes shapes)
/// is refused the same way, with nothing assigned.
fn overlaid(style: &mut LayoutStyle, samples: &[(&str, node::Animatable)]) -> Result<(), LayoutError> {
    use node::Animatable::{Effect, Fields, Number, Shadows};
    let (mut opacity, mut transform) = (style.opacity, style.transform);
    let (mut layers, mut levels) = (None, None);
    for (property, sample) in samples {
        match (*property, sample) {
            ("opacity", &Number(n)) => opacity = n,
            ("rotate", &Number(n)) => transform.rotate = n,
            ("scale", &Number(n)) => transform.scale = (n, n),
            ("scale", &Fields { values: [x, y, ..], .. }) => transform.scale = (x, y),
            ("translate", &Fields { values: [x, y, ..], .. }) => transform.translate = (x, y),
            ("origin", &Fields { values: [x, y, ..], .. }) => transform.origin = (x, y),
            ("shadows", Shadows(list)) => layers = Some(list),
            ("effect", Effect(list, _)) => levels = Some(list),
            _ => return Err(node::invalid(property, "a value of this shape cannot be tweened here")),
        }
    }
    if let Some(list) = layers {
        style.effect.set_shadows(list)?;
    }
    if let Some(list) = levels {
        style.effect.set_levels(list);
    }
    (style.opacity, style.transform) = (opacity, transform);
    Ok(())
}

/// One frame of a tree whose every running tween is paint-only, advanced where it stands.
///
/// [`relayout_retained`] answers one question -- what size is everything now -- and
/// `node::is_paint_only` is the set of properties that cannot change the answer. So this walks the
/// tree the tweens are already in, advances them, and re-derives the node's parsed state. No clone,
/// no solver tree, no measurement, and no geometry to publish, because no rect moved.
fn advance_paint_only(node: &mut ResolvedNode, now: Instant, lua: &Lua) -> Result<(), LayoutError> {
    // Frozen, tweens included (ADR-0124): `animating` does not count a hidden subtree, and
    // `tick_is_paint_only` passes over it for the same reason.
    if !node.visible {
        return Ok(());
    }
    // Outside the tween gate below, and before it: a dissolve is the only motion on a node that
    // has no `animate` block at all, which is every `image` that declares one.
    node.dissolve = advanced_dissolve(node.dissolve.take(), now);
    node.movement.take_if(|movement| !movement.advance(now));
    // A played-out sequence rests on its last frame and moves nothing; `advance_scrolls` moves a scroll.
    if node.tweens.iter().any(|tween| !tween.resting && tween.property != "scroll") {
        advance_paint_only_node(node, now, lua)?;
    }
    for child in &mut node.children {
        advance_paint_only(child, now, lua)?;
    }
    Ok(())
}

/// Advances a dissolve to `now` and drops it once it is over (ADR-0181). Unlike a tween it writes
/// nothing into the property map and cannot be refused, so it needs none of
/// [`advance_paint_only_node`]'s save-and-restore: the only thing it moves is a number
/// `layout::paint` reads.
pub(super) fn advanced_dissolve(dissolve: Option<Box<Dissolve>>, now: Instant) -> Option<Box<Dissolve>> {
    let mut dissolve = dissolve?;
    dissolve.advance(now).then_some(dissolve)
}

/// One node of [`advance_paint_only`], all-or-nothing.
///
/// `node::advance` writes into the retained map, and a value it writes can still be refused: a
/// spring overshoots its target, and the `opacity` field rejects anything outside `[0, 1]` rather than
/// clamping it (ADR-0068). A refused value left in the map would fail the next pass's re-read too,
/// turning one refused frame into a scene that stops updating. So the values about to move are
/// kept and put back on refusal, bounded by this node's tweens rather than its subtree's
/// properties. A typed tween writes nothing until the re-read has succeeded (`node::commit`), so a
/// refusal leaves its `shown` and the map on the last frame, which the strip then writes back.
fn advance_paint_only_node(node: &mut ResolvedNode, now: Instant, lua: &Lua) -> Result<(), LayoutError> {
    let restore: Vec<(&'static str, Value)> = node
        .tweens
        .iter()
        .filter(|tween| !tween.resting && !node::TYPED.contains(&tween.property))
        .filter_map(|tween| node.properties.get_key_value(tween.property))
        .map(|(property, value)| (*property, value.clone()))
        .collect();
    // Nothing is assigned to the node until every step has succeeded, so a refusal leaves its
    // `opacity`, `transform`, `effect` and `paint` describing the same frame its properties do.
    let painted = paints(&node.tweens);
    let advanced = node::step(&mut node.tweens, &mut node.properties, now, lua, true).and_then(|samples| {
        let style = std::rc::Rc::make_mut(&mut node.layout_style);
        reread(style, &mut node.paint, node.kind, &node.properties, painted, &samples)?;
        node::commit(&mut node.tweens, &mut node.properties, samples, now, lua)
    });
    match advanced {
        Ok(()) => {
            node.opacity = node.layout_style.opacity;
            node.transform = node.layout_style.transform;
            Ok(())
        }
        Err(err) => {
            for (property, value) in restore {
                std::rc::Rc::make_mut(&mut node.properties).insert(property, value);
            }
            Err(err)
        }
    }
}

/// One pass of a leaving node (ADR-0150): its tweens move, the paint and the box they name
/// follow, and the node is gone once nothing is in flight. Its subtree is frozen the way a hidden
/// node's is (ADR-0124).
// ponytail: no solver pass. A `width`/`height` in the exit block resizes the box the subtree is
// clipped to, it does not reflow the subtree; a leaving `text` keeps the string it was fitted to.
// A relayout under an absolute-positioned solver node is the upgrade if a config needs the reflow.
pub(super) fn advance_leaving(
    mut node: ResolvedNode,
    now: Instant,
    lua: &Lua,
) -> Result<Option<ResolvedNode>, LayoutError> {
    node::advance(&mut node.tweens, &mut node.properties, now, lua)?;
    node.dissolve = advanced_dissolve(node.dissolve.take(), now);
    if node.tweens.is_empty() {
        return Ok(None);
    }
    let style = LayoutStyle::parse(&node.properties)?;
    let fresh = node::paint_style(node.kind, &node.properties)?;
    node.paint = repainted_keeping_fitted_text(node.paint.take(), fresh);
    node.opacity = style.opacity;
    node.transform = style.transform;
    std::rc::Rc::make_mut(&mut node.layout_style).effect = style.effect;
    node.margin = style.margin;
    if let SizeMode::Pixels(width) = style.width_mode {
        node.rect.width = width;
    }
    if let SizeMode::Pixels(height) = style.height_mode {
        node.rect.height = height;
    }
    Ok(Some(node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::scene::tests::{apply_at, full, instance_at, surface_from};
    use crate::layout::scene::*;

    /// A panel whose child's `width` follows `state("w")` and eases over 100 ms. Returns the
    /// scene, the Lua state and the surface, applied once at `40`.
    fn animated_width(easing: &str) -> (Scene, mlua::Lua, VirtualNode) {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(&format!(
            r#"panel {{ id = "bar", child = rect {{ width = state("w", 40), height = 20,
                animate = {{ width = {{ duration = 100, easing = "{easing}" }} }} }} }}"#
        ));
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        (scene, lua, surface)
    }

    fn child_width(scene: &Scene) -> f32 {
        scene.surface("bar@TEST").unwrap().children[0].rect.width
    }

    fn child_tween(scene: &Scene) -> Tween {
        scene.surface("bar@TEST").unwrap().children[0].tweens[0].clone()
    }

    #[test]
    fn a_content_sized_width_and_height_ease_between_measured_sizes_and_move_their_parent() {
        // The pill resolves nothing: its content change reaches it only through layout. The
        // explicit-width rect inside it must keep its own `width`.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column {
                animate = { width = { duration = 100, easing = "linear" },
                            height = { duration = 100, easing = "linear" } },
                children = {
                    rect { width = state("w", 40), height = state("h", 20) },
                    rect { width = 30, height = 5, animate = { width = { duration = 100, easing = "linear" } } },
                } } }"#,
        );
        let instances = [instance_at(&surface, full())];
        let layout = |scene: &Scene| {
            let root = scene.surface("bar@TEST").unwrap();
            let pill = &root.children[0];
            (pill.rect.width, pill.rect.height, root.rect.width, pill.children[1].rect.width)
        };
        let ms = std::time::Duration::from_millis;
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(layout(&scene), (40.0, 25.0, 40.0, 30.0));
        assert!(!scene.surface("bar@TEST").unwrap().animating(), "a first layout is taken as it is");

        lua.load(r#"state("w", 40):set(90) state("h", 20):set(60)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(layout(&scene), (40.0, 25.0, 40.0, 30.0), "the pass starts from the size on screen");
        // An unrelated pass keeps the run.
        let started = child_tween(&scene).started;
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(child_tween(&scene).started, started);
        scene.tick(&instances, &shaping, &lua, started + ms(50));
        assert_eq!(layout(&scene), (65.0, 45.0, 65.0, 30.0), "the parent follows the eased box");

        // A new size starts from where the pill is.
        lua.load(r#"state("w", 40):set(140)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let retargeted = child_tween(&scene);
        assert_eq!((retargeted.from, retargeted.to), (node::Animatable::Number(65.0), node::Animatable::Number(140.0)));

        scene.tick(&instances, &shaping, &lua, retargeted.started + ms(100));
        assert_eq!(layout(&scene), (140.0, 65.0, 140.0, 30.0));
        assert!(!scene.surface("bar@TEST").unwrap().animating());

        // Settled, the pill follows its content again instead of holding the last eased size.
        lua.load(r#"state("w", 40):set(60)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(layout(&scene), (140.0, 65.0, 140.0, 30.0));
        assert!(scene.surface("bar@TEST").unwrap().animating());
    }

    #[test]
    fn a_text_whose_content_changes_mid_run_keeps_easing_from_its_size_on_screen() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = text { content = state("t", "a"),
                animate = { width = { duration = 100, easing = "linear" } } } }"#,
        );
        let apply =
            |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        apply(&mut scene);
        lua.load(r#"state("t", "a"):set("aaaaaaaaaaaa")"#).exec().unwrap();
        apply(&mut scene);
        let started = child_tween(&scene).started;
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50));
        let shown = child_width(&scene);

        lua.load(r#"state("t", "a"):set("aaaaaaaaaaaaaaaaaaaaaaaa")"#).exec().unwrap();
        apply(&mut scene);
        let tween = child_tween(&scene);
        assert_eq!(tween.from, node::Animatable::Number(shown));
        assert_ne!(tween.to, node::Animatable::Number(shown));
        assert_eq!(child_width(&scene), shown);
    }

    #[test]
    fn a_reused_list_item_eases_its_content_width_and_drops_the_pin_when_it_settles() {
        // The item stretches to the column; the list reads nothing the column's width writes,
        // so every pass past the first reuses it.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { width = state("cw", 100), children = {
                list { width = "fill", source = { "a" }, itemfn = function(name)
                    return rect { height = 10, align_h = "stretch", animate = { width = { duration = 100, easing = "linear" } } }
                end } } } }"#,
        );
        let instances = [instance_at(&surface, full())];
        let item = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].children[0].children[0].clone();
        let apply =
            |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let ms = std::time::Duration::from_millis;
        apply(&mut scene);
        assert_eq!(item(&scene).rect.width, 100.0);

        lua.load(r#"state("cw", 100):set(200)"#).exec().unwrap();
        apply(&mut scene);
        let node = item(&scene);
        assert_eq!((node.rect.width, node.tweens.len()), (100.0, 1), "a reused item eases too");
        scene.tick(&instances, &shaping, &lua, node.tweens[0].started + ms(100));
        assert_eq!(item(&scene).rect.width, 200.0);

        lua.load(r#"state("cw", 100):set(150)"#).exec().unwrap();
        apply(&mut scene);
        let node = item(&scene);
        assert_eq!((node.rect.width, node.tweens.len()), (200.0, 1), "the settled pin is gone");
        scene.tick(&instances, &shaping, &lua, node.tweens[0].started + ms(100));
        assert_eq!(item(&scene).rect.width, 150.0);
    }

    #[test]
    fn a_keyed_sibling_moves_from_its_painted_position_and_an_exit_freezes_mid_move() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local a = rect { id = "a", width = 10, height = 10, background = "#ff0000" }
               local b = rect { id = "b", width = 10, height = 10, background = "#00ff00",
                   animate = { move = { duration = 100, easing = "linear" },
                               exit = { duration = 100, easing = "linear", opacity = 0 } } }
               local c = rect { id = "c", width = 10, height = 10, background = "#0000ff",
                   animate = { move = { duration = 100, easing = "linear" } } }
               return panel { id = "bar", child = row { width = 100, height = 10,
                   children = state("kids", { a, b, c }) } }"##,
        );
        let instances = [instance_at(&surface, full())];
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"local k = state("kids", {}):get(); state("kids", {}):set({ k[2], k[3] })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((row.children[0].rect.x, row.children[1].rect.x), (0.0, 10.0));
        assert_eq!(
            (row.children[0].movement.as_ref().unwrap().offset.0, row.children[1].movement.as_ref().unwrap().offset.0),
            (10.0, 10.0)
        );
        let started = row.children[0].movement.as_ref().unwrap().started;
        scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(50));
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(
            (row.children[0].movement.as_ref().unwrap().offset.0, row.children[1].movement.as_ref().unwrap().offset.0),
            (5.0, 5.0)
        );

        lua.load(r#"local k = state("kids", {}):get(); state("kids", {}):set({ k[2] })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, 0.0);
        assert_eq!(row.children[0].movement.as_ref().unwrap().offset.0, 15.0, "c retargets from its painted x=15");
        assert!(row.children[1].leaving);
        assert_eq!(row.children[1].rect.x, 5.0, "b exits from its painted x=5");
        assert!(row.children[1].movement.is_none());
        let path = crate::layout::hit::hit_path(
            scene.surface("bar@TEST").unwrap(),
            crate::layout::hit::LogicalPoint { x: 17.0, y: 5.0 },
        );
        assert_eq!(path.last().map(|node| node.id), Some(row.children[0].id));
        scene.tick(
            &instances,
            &shaping,
            &lua,
            row.children[0].movement.as_ref().unwrap().started + Duration::from_millis(100),
        );
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(row.children[0].movement.is_none());
        assert_eq!(row.children[0].rect.x, 0.0);
        assert!(!scene.surface("bar@TEST").unwrap().animating());
    }

    #[test]
    fn a_veto_preserves_an_active_move_and_removing_its_spec_cancels_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"spacing = state("spacing", 0)
                motion = state("motion", { move = { duration = 100, easing = "linear" } })
                return panel { id = "bar", child = row { spacing = spacing, children = {
                    rect { width = 10, height = 10 },
                    rect { width = 10, height = 10, animate = motion }
                } } }"#,
        );
        let apply =
            |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        fn moved(scene: &Scene) -> &ResolvedNode {
            &scene.surface("bar@TEST").unwrap().children[0].children[1]
        }
        apply(&mut scene);
        lua.load("spacing:set(10)").exec().unwrap();
        apply(&mut scene);
        let before = moved(&scene).movement.as_ref().unwrap().clone();
        assert_eq!(before.offset.0, -10.0);

        lua.load("spacing:set(20)").exec().unwrap();
        let err = scene.apply_admitting(
            std::slice::from_ref(&surface),
            &[instance_at(&surface, full())],
            &shaping,
            &lua,
            |_, _| Err(node::invalid("child", "veto")),
        );
        assert!(err.is_err());
        let after = moved(&scene).movement.as_ref().unwrap();
        assert_eq!((after.offset, after.started, moved(&scene).rect.x), (before.offset, before.started, 20.0));

        apply(&mut scene);
        assert_eq!(moved(&scene).rect.x, 30.0);
        assert_eq!(moved(&scene).movement.as_ref().unwrap().offset.0, -20.0);

        lua.load("spacing:set(10) motion:set({})").exec().unwrap();
        apply(&mut scene);
        assert!(moved(&scene).movement.is_none());
        assert_eq!(moved(&scene).rect.x, 20.0);
    }

    #[test]
    fn a_departing_parent_freezes_its_moving_child_at_the_painted_position() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"gap = state("gap", 0)
                local group = row { id = "group", spacing = gap,
                    animate = { exit = { duration = 100, opacity = 0 } }, children = {
                        rect { width = 10, height = 10 },
                        rect { width = 10, height = 10, animate = { move = 100 } }
                    } }
                groups = state("groups", { group })
                return panel { id = "bar", child = row { children = groups } }"#,
        );
        let apply =
            |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        apply(&mut scene);
        lua.load("gap:set(10)").exec().unwrap();
        apply(&mut scene);
        let child = &scene.surface("bar@TEST").unwrap().children[0].children[0].children[1];
        assert_eq!((child.rect.x, child.movement.as_ref().unwrap().offset.0), (20.0, -10.0));

        lua.load("groups:set({})").exec().unwrap();
        apply(&mut scene);
        let group = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert!(group.leaving);
        assert_eq!(group.children[1].rect.x, 10.0);
        assert!(group.children[1].movement.is_none());
    }

    #[test]
    fn a_layout_tween_tick_does_not_start_a_siblings_move() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = row { width = 100, children = {
                rect { width = state("w", 10), height = 10,
                       animate = { width = { duration = 100, easing = "linear" } } },
                rect { width = 10, height = 10, animate = { move = 100 } } } } }"#,
        );
        let instances = [instance_at(&surface, full())];
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("w", 10):set(20)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(row.children[1].movement.is_none());
        let started = row.children[0].tweens[0].started;
        scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(50));
        let sibling = &scene.surface("bar@TEST").unwrap().children[0].children[1];
        assert_eq!(sibling.rect.x, 15.0);
        assert!(sibling.movement.is_none());
    }

    #[test]
    fn path_commands_tween_between_shapes_with_the_same_ops() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local on = state("on", false)
            return panel { id = "bar", child = path { width = 20, height = 20, fill = "#ffffff",
                stroke = on:map(function(o) return o and "#ffffff" or "#000000" end),
                stroke_width = on:map(function(o) return o and 4 or 2 end),
                commands = on:map(function(o) return {{ op = "A", points = { 10, 10, o and 8 or 2, 0, 360 } }} end),
                trim_end = on:map(function(o) return o and 0 or 1 end),
                animate = { commands = { duration = 100 }, stroke = { duration = 100 },
                            stroke_width = { duration = 100 }, trim_end = { duration = 100 } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("on", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(scene.surface("bar@TEST").unwrap().tick_is_paint_only(), "a path's paint asks the solver nothing");
        let started = child_tween(&scene).started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let Some(node::PaintStyle::Path(path)) = &scene.surface("bar@TEST").unwrap().children[0].paint else {
            panic!("a path paints");
        };
        assert_eq!(path.commands.points, [10.0, 10.0, 5.0, 0.0, 360.0], "halfway is the midpoint radius");
        assert_eq!(path.stroke_width, 3.0);
        assert_eq!(path.trim, (0.0, 0.5));
        let Some(node::Fill::Color(stroke)) = path.stroke else { panic!("a colour stroke, got {:?}", path.stroke) };
        assert!(stroke.r > 0.1 && stroke.r < 0.9, "the stroke is between black and white, got {}", stroke.r);
    }

    /// A wave's phase loops on `translate` while its amplitude eases on `commands`: one tick
    /// advances both, so the wave flattens without stopping.
    #[test]
    fn a_commands_tween_runs_beside_a_looping_translate() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local amp = state("amp", 8)
            return panel { id = "bar", child = path { width = 40, height = 20, stroke = "#ffffff",
                commands = amp:map(function(a) return {
                    { op = "M", points = { 0, 10 } }, { op = "Q", points = { 10, 10 - a, 20, 10 } },
                    { op = "Q", points = { 30, 10 + a, 40, 10 } } } end),
                animate = { commands = { duration = 100, easing = "linear" },
                            translate = { duration = 200, easing = "linear", loops = "infinite",
                                          keyframes = { { x = 0, y = 0 }, { x = -20, y = 0 } } } } } }"##,
        );
        let instances = [instance_at(&surface, full())];
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let looped = child_tween(&scene).started;
        scene.tick(&instances, &shaping, &lua, looped + Duration::from_millis(100));
        lua.load(r#"state("amp", 8):set(0)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let node = &scene.surface("bar@TEST").unwrap().children[0];
        let eased = node.tweens.iter().find(|t| t.property == "commands").expect("a commands tween").started;
        scene.tick(&instances, &shaping, &lua, eased + Duration::from_millis(50));

        let node = &scene.surface("bar@TEST").unwrap().children[0];
        let Some(node::PaintStyle::Path(path)) = &node.paint else { panic!("a path paints") };
        assert_eq!(path.commands.points[2..6], [10.0, 6.0, 20.0, 10.0], "halfway is half the amplitude");
        let phase = (eased + Duration::from_millis(50) - looped).as_secs_f32() * 1000.0 % 200.0;
        assert!((node.transform.translate.0 + phase / 10.0).abs() < 0.01, "the loop kept its phase");
    }

    /// `outline` eases between two contours of one command list on the paint-only tick, a point
    /// written in px against one in `"NN%"` crossing as their resolved px would, a spring past its end.
    #[test]
    fn an_outline_tweens_paint_only_under_a_spring() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local open = state("open", false)
            return panel { id = "bar", child = rect { width = 40, height = 20, background = "#ffffff",
                outline = open:map(function(o) return { commands = {
                    { op = "M", points = { o and "50%" or 0, 0 } },
                    { op = "corner", points = { 40, 0 }, radius = o and 0 or 8 },
                    { op = "L", points = { 0, 20 } }, { op = "Z", points = {} } } } end),
                animate = { outline = { spring = { stiffness = 400, damping = 4 } } } } }"##,
        );
        let instances = [instance_at(&surface, full())];
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("open", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(scene.surface("bar@TEST").unwrap().tick_is_paint_only(), "an outline asks the solver nothing");
        let started = child_tween(&scene).started;
        let (mut crossed, mut overshot) = (false, false);
        for ms in (10..400).step_by(10) {
            scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(ms));
            let node = &scene.surface("bar@TEST").unwrap().children[0];
            let Some(node::PaintStyle::Box { radius, .. }) = &node.paint else { panic!("a box paints") };
            let bez = radius.2.as_ref().expect("an outline").bez(node.rect);
            let kurbo::PathEl::MoveTo(start) = bez.elements()[0] else { panic!("{bez:?}") };
            crossed |= start.x > 2.0 && start.x < 18.0;
            overshot |= start.x > 20.5;
        }
        assert!(crossed && overshot, "a spring through the middle and past the end");
    }

    #[test]
    fn a_looping_shift_advances_paint_only() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"return panel { id = "bar", child = path { width = 40, height = 20, stroke = "#ffffff",
                trim_axis = "x", commands = { { op = "M", points = { 0, 10 } }, { op = "L", points = { 40, 10 } } },
                animate = { shift = { duration = 200, easing = "linear", loops = "infinite",
                                      keyframes = { { x = 0, y = 0 }, { x = -20, y = 0 } } } } } }"##,
        );
        let instances = [instance_at(&surface, full())];
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(scene.surface("bar@TEST").unwrap().tick_is_paint_only(), "a shift asks the solver nothing");
        let started = child_tween(&scene).started;
        scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(100));
        let Some(node::PaintStyle::Path(path)) = &scene.surface("bar@TEST").unwrap().children[0].paint else {
            panic!("a path paints");
        };
        assert_eq!(path.shift, (-10.0, 0.0));
        assert_eq!(path.trim_axis, node::TrimAxis::X);
    }

    #[test]
    fn a_changed_target_starts_a_tween_from_the_value_on_screen_and_a_tick_carries_it() {
        // ADR-0145: the pass that sees `90` lays out `40` and a tween; the ticks do the rest
        // without Lua.
        let (mut scene, lua, surface) = animated_width("linear");
        let shaping = ShapingHandle::spawn();
        assert_eq!(child_width(&scene), 40.0);
        assert!(!scene.surface("bar@TEST").unwrap().animating(), "a first value is taken as it is");

        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!((child_width(&scene) - 40.0).abs() < 0.5, "the pass paints from where the node was");
        let tween = child_tween(&scene);
        assert_eq!(tween.to, node::Animatable::Number(90.0));

        let instances = [instance_at(&surface, full())];
        assert!(
            !scene.tick(&instances, &shaping, &lua, tween.started + std::time::Duration::from_millis(50)).is_empty()
        );
        assert_eq!(child_width(&scene), 65.0, "halfway through a linear tween is the midpoint");
        assert!(scene.surface("bar@TEST").unwrap().animating());

        scene.tick(&instances, &shaping, &lua, tween.started + std::time::Duration::from_millis(100));
        assert_eq!(child_width(&scene), 90.0);
        assert!(!scene.surface("bar@TEST").unwrap().animating(), "an arrived tween is dropped");
        assert!(scene.solver_trees.contains_key("bar@TEST"), "the next pass starts from the settled tree");
        assert!(scene.tick(&instances, &shaping, &lua, tween.started + std::time::Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn growing_parent_reveals_text_again_on_cached_layout_ticks() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = rect { width = state("w", 30), height = 20,
                animate = { width = { duration = 100, easing = "linear" } },
                children = { text { width = "fill", content = "A long message that should expand", elide = "end" } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("w", 30):set(300)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let shown =
            |scene: &Scene| match scene.surface("bar@TEST").unwrap().children[0].children[0].paint.as_ref().unwrap() {
                PaintStyle::Text { content, .. } => content.len(),
                other => panic!("{other:?}"),
            };
        let initial = shown(&scene);
        let started = scene.surface("bar@TEST").unwrap().children[0].tweens[0].started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(100));
        assert!(shown(&scene) > initial);
    }

    #[test]
    fn a_percent_child_follows_its_growing_parent_through_the_tween_and_after() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local e = state("e", false)
            local w = e:map(function(on) return on and 220 or 34 end)
            local o = e:map(function(on) return on and 1 or 0 end)
            local v = state("v", 50)
            local function bar() return rect { width = v:map(function(x) return x .. "%" end), height = "fill",
                opacity = o, animate = { opacity = { duration = 100, easing = "linear" } },
                children = { row { width = w, height = "fill", animate = { width = { duration = 100, easing = "linear" } } } } } end
            return panel { id = "bar", child = rect { width = w, height = 20, clip = "rounded", radius = 4,
                animate = { width = { duration = 100, easing = "linear" } },
                children = { bar(), row { width = "fill", height = "fill", children = { rect { width = "100%" } } } } } }"#,
        );
        let fill = |scene: &Scene| {
            let parent = &scene.surface("bar@TEST").unwrap().children[0];
            (parent.rect.width, parent.children[0].rect.width)
        };
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("e", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = scene.surface("bar@TEST").unwrap().children[0].tweens[0].started;
        let instances = [instance_at(&surface, full())];
        for ms in [30, 60, 100, 200] {
            scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(ms));
            let (parent, child) = fill(&scene);
            assert!((child - parent / 2.0).abs() < 0.5, "at {ms} ms: {child} in {parent}");
        }
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(fill(&scene), (220.0, 110.0), "a settled pass keeps it");
    }

    #[test]
    fn only_the_tick_that_settles_owes_its_geometry_readers_a_pass() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = rect { width = state("w", 40), height = 20, geometry = geometry("g"),
                animate = { width = { duration = 100, easing = "linear" } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        crate::lua::signal::take_layout_changed(&lua);
        let started = child_tween(&scene).started;
        let instances = [instance_at(&surface, full())];

        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50));
        assert!(crate::lua::signal::take_layout_changed(&lua).is_empty(), "a mid-tween frame is quiet");
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(100));
        assert_eq!(crate::lua::signal::take_layout_changed(&lua).len(), 1, "the settling frame is not");
        let rect: mlua::Table = lua.load(r#"return geometry("g"):get()"#).eval().unwrap();
        assert_eq!(rect.get::<f32>("width").unwrap(), 90.0);
    }

    #[test]
    fn a_tween_frozen_under_a_hidden_ancestor_does_not_keep_asking_for_frames() {
        // The closed-picker stall: hidden subtrees are never advanced, so a tween caught inside
        // one must not count, or the frame-callback chain runs until the picker opens again.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { visible = state("open", true), children = {
                    rect { width = state("w", 40), height = 10, animate = { width = 100 } } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("w", 40):set(90) state("open", true):set(false)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert!(!scene.surface("bar@TEST").unwrap().animating());
        assert!(scene.tick(&[instance_at(&surface, full())], &shaping, &lua, Instant::now()).is_empty());

        lua.load(r#"state("open", true):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let thawed = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert!(thawed.tweens.is_empty() || thawed.rect.width < 90.0, "the thaw settles or resumes, never stalls");
    }

    /// Stable edge and anchor tables belong to the last pass. Only the width tween changes on ticks.
    #[test]
    fn a_tick_does_not_reparse_an_unchanged_edge_or_anchor_table() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local m = setmetatable({}, { __index = function() reads = (reads or 0) + 1 return 2 end })
            local a = setmetatable({}, { __index = function(_, edge)
                anchor_reads = (anchor_reads or 0) + 1 return edge == "left" or edge == "right" end })
            return panel { id = "bar", anchor = a, child = row { children = {
                rect { width = state("w", 40), height = 10, animate = { width = 100 } },
                rect { width = 10, height = 10, margin = m } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = scene.surface("bar@TEST").unwrap().children[0].children[0].tweens[0].started;
        let reads: usize = lua.globals().get("reads").unwrap();
        let anchor_reads: usize = lua.globals().get("anchor_reads").unwrap();
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        assert_eq!(lua.globals().get::<usize>("reads").unwrap(), reads);
        assert_eq!(lua.globals().get::<usize>("anchor_reads").unwrap(), anchor_reads);
        assert!(scene.surface("bar@TEST").unwrap().animating());
    }

    #[test]
    fn a_new_node_with_a_from_enters_from_it_and_an_edge_table_tweens_per_edge() {
        // ADR-0146: `from` is the entry animation; the margin table eases edge by edge.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = rect { width = 10, height = 10,
                margin = state("m", { left = 0 }), opacity = 1,
                animate = { opacity = { duration = 100, from = 0 },
                            margin = { duration = 100, easing = "linear", from = { left = 40 } } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.opacity, 0.0, "a first value starts at `from`, not at the target");
        assert_eq!(child.rect.x, 40.0);
        let started = child.tweens[0].started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.x, 20.0, "halfway along a linear edge tween");
        assert!((child.opacity - 0.5).abs() < 0.01, "got {}", child.opacity);
    }

    /// A nested signal's change is a new target for the whole table, eased from the value on screen.
    #[test]
    fn a_nested_margin_edge_changed_mid_tween_eases_on_from_where_it_is() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = rect { width = 10, height = 10,
                margin = { left = state("l", 0) }, animate = { margin = { duration = 100, easing = "linear" } } } }"#,
        );
        let halfway = |scene: &mut Scene, value: i32| {
            lua.load(format!(r#"state("l", 0):set({value})"#)).exec().unwrap();
            apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            let started = scene.surface("bar@TEST").unwrap().children[0].tweens[0].started;
            scene.tick(
                &[instance_at(&surface, full())],
                &shaping,
                &lua,
                started + std::time::Duration::from_millis(50),
            );
            scene.surface("bar@TEST").unwrap().children[0].rect.x
        };
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(halfway(&mut scene, 40), 20.0);
        assert_eq!(halfway(&mut scene, 80), 50.0, "from 20 on screen, not from 0 or 40");
    }

    /// A leaving `text` is not relaid out, so it keeps the string it was fitted to -- but its
    /// colour is paint, not layout, and freezing the whole of its paint left a label unable to
    /// fade on the way out while every other node could.
    #[test]
    fn a_leaving_text_animates_its_colour_while_keeping_the_string_it_was_fitted_to() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        // Elided on purpose: the fitted string differs from the one the config wrote, so a paint
        // re-read that forgot to carry it would hand back the whole untruncated message.
        let source = "a long message that will not fit";
        let (lua, surface) = surface_from(
            r##"local label = text { id = "label", width = 60, font_size = 14, elide = "end",
                   content = "a long message that will not fit", foreground = "#000000",
                   animate = { exit = { duration = 100, easing = "linear", foreground = "#ff0000" } } }
               return panel { id = "bar", child = row { children = state("kids", { label }) } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("kids", {}):set({})"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();

        let leaver = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert!(leaver.leaving, "the label is on its way out");
        let started = leaver.tweens[0].started;
        let instances = [instance_at(&surface, full())];
        assert!(!scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50)).is_empty());

        let leaver = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        let Some(PaintStyle::Text { content, color, .. }) = leaver.paint.as_ref() else { panic!("{leaver:?}") };
        assert!(content.ends_with('\u{2026}'), "still the string it was fitted to, got {content:?}");
        assert_ne!(&**content, source, "not the one the config wrote, re-read from the properties");
        assert!((color.r - 0.5).abs() < 0.05 && color.g < 0.05, "halfway to red, got {color:?}");
    }

    /// ADR-0150: a child the tree drops stays as a leaving node while its exit tweens run, out of
    /// flow and out of reach, painted after its live siblings; one with no exit block is gone at
    /// once; the pass that finds nothing in flight drops it.
    #[test]
    fn a_dropped_child_with_an_exit_block_leaves_over_its_tweens_and_one_without_is_gone_at_once() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local a = rect { id = "a", width = 10, height = 10, background = "#ff0000",
                   children = { rect { id = "inner", width = 4, height = 4, background = "#ffffff" } },
                   animate = { exit = { duration = 100, easing = "linear", opacity = 0, translate = { y = 8 } } } }
               local b = rect { id = "b", width = 10, height = 10, background = "#00ff00" }
               local c = rect { id = "c", width = 10, height = 10, background = "#0000ff" }
               return panel { id = "bar", child = row { spacing = 0, children = state("kids", { a, b, c }) } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children.len(), 3);
        let a_id = row.children[0].id;

        lua.load(r#"local k = state("kids", {}):get(); state("kids", {}):set({ k[3] })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let ids: Vec<String> =
            row.children.iter().map(|c| node::fields::common::id.read(&c.properties).unwrap().unwrap()).collect();
        assert_eq!(ids, ["c", "a"], "b had no exit block; a leaves after the live child");
        let (c, a) = (&row.children[0], &row.children[1]);
        assert_eq!(c.rect.x, 0.0, "the flow closed over the leaver's slot at once");
        assert!(a.leaving && a.id == a_id && a.rect.x == 0.0, "a keeps its identity and its last rect");
        // "Out of reach" includes the question a held draft asks of the tree it was typed into
        // (ADR-0108). A leaver is back among `children` so it paints, and answering that question
        // from there would keep a removed textfield focused: keys would still reach a node the
        // tree has already dropped, and its `on_submit` would run for a card that is gone.
        assert!(!crate::layout::hit::contains_node(row, a_id), "a leaving node is no longer in the tree");
        // The whole subtree goes with it. `inner` never left on its own account -- it is only
        // under something that did -- so a check that read one `leaving` flag would still find it.
        let inner = &a.children[0];
        assert!(!inner.leaving, "the child is not itself a leaver");
        assert!(!crate::layout::hit::contains_node(row, inner.id), "and it is out of reach under one");
        assert!(crate::layout::hit::contains_node(row, c.id), "its live sibling still is");
        assert_eq!(a.opacity, 1.0, "an absent opacity departs from 1");
        assert_eq!(row.rect.width, 10.0, "a leaving child takes no room");

        let started = a.tweens[0].started;
        let instances = [instance_at(&surface, full())];
        assert!(!scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50)).is_empty());
        let a = &scene.surface("bar@TEST").unwrap().children[0].children[1];
        assert!((a.opacity - 0.5).abs() < 0.01 && a.transform.translate == (0.0, 4.0), "halfway out: {a:?}");
        let root = scene.surface("bar@TEST").unwrap();
        let path = crate::layout::hit::hit_path(root, crate::layout::hit::LogicalPoint { x: 5.0, y: 5.0 });
        assert_eq!(path.last().map(|n| n.id), Some(root.children[0].children[0].id), "the pointer lands on c, under a");

        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(100));
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children.len(), 1, "a is gone");
        assert!(!scene.surface("bar@TEST").unwrap().animating());
    }

    /// A lone leaver in a content-sized parent: the parent holds the leaver's last rect for the
    /// exit, so neither it nor the surface collapses to 0x0 and clips the exit away, then lets go.
    #[test]
    fn a_content_sized_parent_holds_its_lone_leavers_rect_until_the_exit_ends() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local card = rect { id = "card", width = 40, height = 20, margin = { bottom = 2 },
                   animate = { exit = { duration = 100, easing = "linear", opacity = 0 } } }
               return panel { id = "bar", child = column { padding = 4, children = state("kids", { card }) } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();

        lua.load(r#"state("kids", {}):set({})"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(column.children[0].leaving);
        assert_eq!((column.rect.width, column.rect.height), (48.0, 30.0), "4 + 40 + 4 by 4 + 20 + 2 + 4");

        let started = column.children[0].tweens[0].started;
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50));
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((column.rect.width, column.rect.height), (48.0, 30.0), "held through the exit");

        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(100));
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((column.rect.width, column.rect.height), (8.0, 8.0), "only its padding once the card is gone");
    }

    #[test]
    fn a_departing_fill_child_cannot_enlarge_an_allocated_root_past_its_new_size_or_max() {
        let shaping = ShapingHandle::spawn();
        for (kind, anchor) in [
            ("panel", "anchor = { top = true, bottom = true, left = true, right = true },"),
            ("window", ""),
            ("lock", ""),
        ] {
            let (lua, surface) = surface_from(&format!(
                r#"{{ kind = "{kind}", id = "root", {anchor}
                    max_width = state("max_w", 200), max_height = state("max_h", 200),
                    child = state("kid", rect {{ width = "fill", height = "fill",
                        animate = {{ exit = {{ duration = 100, easing = "linear", opacity = 0 }} }} }}) }}"#
            ));
            let mut scene = Scene::new();
            apply_at(
                &mut scene,
                std::slice::from_ref(&surface),
                LogicalSize { width: 100.0, height: 80.0 },
                &shaping,
                &lua,
            )
            .unwrap();
            lua.load(
                r#"state("kid", false):set(nil)
                state("max_w", 200):set(35)
                state("max_h", 200):set(15)"#,
            )
            .exec()
            .unwrap();
            let available = LogicalSize { width: 40.0, height: 20.0 };
            apply_at(&mut scene, std::slice::from_ref(&surface), available, &shaping, &lua).unwrap();
            let root = scene.surface("root@TEST").unwrap();
            assert!(root.children[0].leaving, "{kind}");
            assert_eq!((root.rect.width, root.rect.height), (35.0, 15.0), "{kind} pass");
            let started = root.children[0].tweens[0].started;
            scene.tick(
                &[instance_at(&surface, available)],
                &shaping,
                &lua,
                started + std::time::Duration::from_millis(50),
            );
            let root = scene.surface("root@TEST").unwrap();
            assert_eq!((root.rect.width, root.rect.height), (35.0, 15.0), "{kind} tick");
        }
    }

    /// ADR-0150: an exit block runs when the tree drops the node, not when it hides one. A hidden
    /// child's subtree is frozen rather than advanced (ADR-0124), so there is nothing to ease and
    /// dropping it later is immediate; `delay(signal, ms)` is the answer for a close-hold.
    #[test]
    fn a_hidden_child_is_dropped_at_once_however_it_leaves() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local a = rect { id = "a", width = 10, height = 10, background = "#ff0000",
                   visible = state("shown", true),
                   animate = { exit = { duration = 100, opacity = 0 } } }
               local b = rect { id = "b", width = 10, height = 10, background = "#00ff00" }
               return panel { id = "bar", child = row { spacing = 0, children = state("kids", { a, b }) } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children.len(), 2);

        // Hiding it alone changes nothing about how many nodes there are: it is still in the tree.
        lua.load(r#"state("shown", true):set(false)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children.len(), 2, "hiding is not leaving");
        assert!(!row.children[0].visible && !row.children[0].leaving);

        lua.load(r#"local k = state("kids", {}):get(); state("kids", {}):set({ k[2] })"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert_eq!(root.children[0].children.len(), 1, "a hidden node has nothing showing to ease out");
        assert!(!root.animating());
    }

    /// A leaving node is out of the reconciliation, so putting its `id` back builds a second node
    /// beside it rather than pulling the first back out of its exit.
    /// The one still fading keeps its own identity until its tweens end.
    #[test]
    fn re_adding_a_leaving_id_builds_a_new_node_beside_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local a = rect { id = "a", width = 10, height = 10, background = "#ff0000",
                   animate = { exit = { duration = 100, easing = "linear", opacity = 0 } } }
               return panel { id = "bar", child = row { spacing = 0, children = state("kids", { a }) } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let first = scene.surface("bar@TEST").unwrap().children[0].children[0].id;

        let kids = |src: &str| lua.load(src).exec().unwrap();
        kids(r#"state("kids", {}):set({})"#);
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = scene.surface("bar@TEST").unwrap().children[0].children[0].tweens[0].started;

        // The fixture's own `a` is not reachable from Lua, so rebuild an identical child instead.
        lua.load(
            r##"state("kids", {}):set({ rect { id = "a", width = 10, height = 10, background = "#ff0000",
                   animate = { exit = { duration = 100, easing = "linear", opacity = 0 } } } })"##,
        )
        .exec()
        .unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children.len(), 2, "the new `a` and the old one still fading");
        assert!(!row.children[0].leaving && row.children[0].id != first, "the live one is a fresh node");
        assert!(row.children[1].leaving && row.children[1].id == first, "the leaver kept its identity");
        assert_eq!(row.rect.width, 10.0, "only the live one is measured");

        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(100));
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children.len(), 1);
        assert!(!row.children[0].leaving);
    }

    /// ADR-0152: an endless sequence drives its property between passes and keeps asking for
    /// frames; a counted one plays out, rests on its last frame, and is not started again by a
    /// pass that resolves for some unrelated reason.
    #[test]
    fn a_counted_sequence_rests_when_it_is_done_and_an_endless_one_never_does() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"return panel { id = "bar", child = row { spacing = 0, children = {
                   rect { id = "pulse", width = 10, height = 10, background = "#ff0000", opacity = 1,
                     animate = { opacity = { duration = 100, easing = "linear", loops = "infinite",
                                             keyframes = { 1, 0.2, 1 } } } },
                   rect { id = "flash", width = state("w", 10), height = 10, background = "#00ff00", opacity = 1,
                     animate = { opacity = { duration = 100, easing = "linear", loops = 1,
                                             keyframes = { 1, 0 } } } } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let (pulse, flash) = (&row.children[0], &row.children[1]);
        assert_eq!(pulse.opacity, 1.0, "both start on their first frame, not on the resolved value");
        assert_eq!(flash.opacity, 1.0);
        let started = pulse.tweens[0].started;
        let instances = [instance_at(&surface, full())];

        assert!(!scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50)).is_empty());
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert!((row.children[0].opacity - 0.6).abs() < 0.01, "halfway down the pulse");
        assert!((row.children[1].opacity - 0.5).abs() < 0.01, "halfway through the flash");

        // Past the counted one's single loop: it rests, the endless one carries on wrapping.
        assert!(!scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(250)).is_empty());
        let root = scene.surface("bar@TEST").unwrap();
        let (pulse, flash) = (&root.children[0].children[0], &root.children[0].children[1]);
        assert!((pulse.opacity - 0.6).abs() < 0.01, "a quarter of the way round again");
        assert_eq!(flash.opacity, 0.0, "resting on its last frame");
        assert!(flash.tweens[0].resting && !pulse.tweens[0].resting);
        assert!(root.animating(), "the endless one still wants frames");

        // A pass for something else entirely must not restart the run that finished. What proves
        // that is the run's own start instant: `apply` reads the real clock while the ticks above
        // are told a time, so an opacity a wall-clock `at` would compute says nothing here.
        //
        // `resting` is not asserted alongside it, because a pass re-derives it against the clock
        // it ran on rather than carrying the flag the last tick left. Under this test's two
        // clocks those disagree -- the ticks are 250ms in, the pass is microseconds in -- while in
        // a live shell both read the same monotonic clock and a counted sequence that is done
        // stays done. Carrying the flag instead is what left a re-delayed sequence resting so
        // hard that `animating` never asked for the frame that would start it.
        let began = flash.tweens[0].started;
        lua.load(r#"state("w", 10):set(20)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let flash = &scene.surface("bar@TEST").unwrap().children[0].children[1];
        assert_eq!(flash.rect.width, 20.0, "the pass did land");
        assert_eq!(flash.tweens[0].started, began, "the same run, not a new one");
    }

    /// A re-delayed sequence must drop the `resting` flag its last tick left, or `animating` never
    /// asks for the frame that starts it and it sits on its old last frame forever. `delay` lives on the spec beside the motion, not in it, so the run still matches
    /// as "the same list going round again" and is carried across.
    #[test]
    fn a_played_out_sequence_handed_a_fresh_delay_stops_resting_and_asks_for_frames_again() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local held = state("held", 0)
            return panel { id = "bar", child = rect { width = 10, height = 10, opacity = 1,
              animate = held:map(function(d)
                  return { opacity = { duration = 100, easing = "linear", loops = 1,
                                       keyframes = { 1, 0 }, delay = d } }
              end) } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let instances = [instance_at(&surface, full())];
        let started = scene.surface("bar@TEST").unwrap().children[0].tweens[0].started;

        // Play it out, then confirm it has stopped asking for frames.
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(200));
        let node = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(node.tweens[0].resting && node.opacity == 0.0, "played out and resting on its last frame");
        assert!(!scene.surface("bar@TEST").unwrap().animating(), "a finished run wants no more frames");

        // The same list, now with a lead-in in front of it: that is a run still to come.
        lua.load(r#"state("held", 0):set(200)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let node = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(!node.tweens[0].resting, "the delay put the run back in front of the clock");
        assert!(scene.surface("bar@TEST").unwrap().animating(), "so the surface asks for frames again");
    }

    /// The paint-only tick has to leave a tree indistinguishable from the relayout it replaces,
    /// including the values a `text` node paints, and it has to name the instance it advanced so
    /// the poll loop knows which surface to repaint.
    #[test]
    fn a_paint_only_tick_moves_the_same_values_a_relayout_would_and_names_its_instance() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local lit = state("lit", false)
            return panel { id = "bar", child = row { width = 100, height = 20, children = {
              rect { width = 10, height = 10,
                     background = lit:map(function(o) return o and "#ffffff" or "#000000" end),
                     opacity = lit:map(function(o) return o and 1 or 0.2 end),
                     animate = { background = { duration = 100, easing = "linear" },
                                 opacity = { duration = 100, easing = "linear" } } },
              text { content = "abc", foreground = lit:map(function(o) return o and "#ffffff" or "#000000" end),
                     animate = { foreground = { duration = 100, easing = "linear" } } } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let before = scene.surface("bar@TEST").unwrap().children[0].children[0].rect;
        lua.load(r#"state("lit", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        assert!(root.tick_is_paint_only(), "a colour and an opacity ask the solver nothing");
        let started = root.children[0].children[0].tweens[0].started;
        let instances = [instance_at(&surface, full())];
        assert_eq!(
            scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50)),
            vec!["bar@TEST".to_string()],
            "the tick names the instance it advanced"
        );

        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let (block, label) = (&row.children[0], &row.children[1]);
        assert!((block.opacity - 0.6).abs() < 0.01, "halfway from 0.2 to 1, got {}", block.opacity);
        let Some(PaintStyle::Box { background, .. }) = &block.paint else {
            panic!("a rect paints a box, got {:?}", block.paint)
        };
        let [(node::Fill::Color(background), _)] = background.as_slice() else { panic!("one colour: {background:?}") };
        assert!((background.r - 0.5).abs() < 0.02, "halfway from black to white, got {}", background.r);
        let Some(PaintStyle::Text { content, color, .. }) = &label.paint else {
            panic!("a text paints text, got {:?}", label.paint)
        };
        assert_eq!(&**content, "abc", "the string it was fitted to survives a tick that never measured it");
        assert!((color.r - 0.5).abs() < 0.02, "the label's colour moved too, got {}", color.r);
        assert_eq!(block.rect, before, "nothing a paint-only tick writes can move a rect");
    }

    /// `corner_smoothing` only reshapes the outline, so its tween ticks without the solver and the
    /// tick writes the eased value back into the box's radii.
    #[test]
    fn a_corner_smoothing_tween_runs_on_the_paint_only_tick() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local up = state("up", false)
            return panel { id = "bar", child = rect { width = 10, height = 10, background = "#ffffff", radius = 4,
                corner_smoothing = up:map(function(u) return u and 1 or 0 end),
                animate = { corner_smoothing = { duration = 100, easing = "linear" } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("up", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert!(root.tick_is_paint_only(), "an outline asks the solver nothing");
        let started = root.children[0].tweens[0].started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let Some(PaintStyle::Box { radius, .. }) = &scene.surface("bar@TEST").unwrap().children[0].paint else {
            panic!("a rect paints a box")
        };
        assert_eq!(radius.0, [4.0; 4]);
        assert!((radius.1 - 0.5).abs() < 0.01, "halfway to 1, got {}", radius.1);
    }

    /// ADR-0254, ADR-0256: a shadow and both blurs tween on the paint-only tick, and the tick re-derives
    /// the node's `effect` from the values it wrote.
    #[test]
    fn a_shadow_and_both_blurs_tween_on_the_paint_only_tick() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local up = state("up", false)
            return panel { id = "bar", child = rect { width = 10, height = 10, background = "#ffffff",
                shadows = up:map(function(u) return { { blur = u and 8 or 0, offset = { y = u and 4 or 0 } } } end),
                effect = { blur = up:map(function(u) return u and 2 or 0 end),
                           backdrop = { blur = up:map(function(u) return u and 8 or 0 end) } },
                animate = { shadows = { duration = 100, easing = "linear" },
                            effect = { duration = 100, easing = "linear" } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("up", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert!(root.tick_is_paint_only(), "an effect asks the solver nothing");
        let started = root.children[0].tweens[0].started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let effect = &scene.surface("bar@TEST").unwrap().children[0].layout_style.effect;
        let shadow = effect.shadows.first().expect("halfway, the shadow shows");
        assert!((shadow.blur - 4.0).abs() < 0.01 && (shadow.offset.1 - 2.0).abs() < 0.01, "got {shadow:?}");
        assert!((effect.blur - 1.0).abs() < 0.01, "got {}", effect.blur);
        assert!((effect.backdrop - 4.0).abs() < 0.01, "got {}", effect.backdrop);
    }

    /// `width` is not paint-only, so a tree carrying one has to take the relayout path even when
    /// every other tween in it is a colour.
    #[test]
    fn a_tween_the_solver_reads_keeps_the_tree_on_the_relayout_path() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local wide = state("wide", false)
            return panel { id = "bar", child = row { width = 100, height = 20, spacing = 0, children = {
              rect { height = 10, background = "#ffffff",
                     width = wide:map(function(w) return w and 40 or 10 end),
                     animate = { width = { duration = 100, easing = "linear" } } },
              rect { width = 10, height = 10,
                     background = wide:map(function(w) return w and "#ffffff" or "#000000" end),
                     opacity = wide:map(function(w) return w and 1 or 0.2 end),
                     animate = { background = { duration = 100, easing = "linear" },
                                 opacity = { duration = 100, easing = "linear" } } } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("wide", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        assert!(!root.tick_is_paint_only(), "a width is the solver's business");
        let started = root.children[0].children[0].tweens[0].started;
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50));
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let (block, beside) = (&row.children[0], &row.children[1]);
        assert!((block.rect.width - 25.0).abs() < 0.5, "halfway from 10 to 40, got {}", block.rect.width);
        // The sibling's tweens are paint-only: they move on the relayout tick, and it is still placed.
        assert!(
            (beside.rect.x - block.rect.x - block.rect.width).abs() < 0.5,
            "placed after the block: {:?}",
            beside.rect
        );
        assert!((beside.opacity - 0.6).abs() < 0.01, "halfway from 0.2 to 1, got {}", beside.opacity);
        let Some(PaintStyle::Box { background, .. }) = &beside.paint else { panic!("a box, got {:?}", beside.paint) };
        let [(node::Fill::Color(background), _)] = background.as_slice() else { panic!("one colour: {background:?}") };
        assert!((background.r - 0.5).abs() < 0.02, "halfway from black to white, got {}", background.r);
    }

    #[test]
    fn a_width_mapped_through_a_lingering_computed_collapses_once_the_delay_lands() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local h = state("h", false)
            local linger = computed({ h, delay(h, 20) }, function(now, was) return now == true or was == true end)
            local open = computed({ linger, state("held", false) }, function(o, held) return o or held end)
            return panel { id = "bar", child = row { children = {
                row { width = computed({ open, state("kept", false) }, function(o, k) return (o or k) and 34 or 0 end), height = 20 } } } }"#,
        );
        let width = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].children[0].rect.width;
        let pass = |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let land = |scene: &mut Scene| {
            std::thread::sleep(std::time::Duration::from_millis(40));
            crate::lua::signal::take_due_wake(&lua, std::time::Instant::now())
                .into_iter()
                .for_each(crate::lua::signal::note_write);
            pass(scene);
        };
        pass(&mut scene);
        assert_eq!(width(&scene), 0.0);
        lua.load(r#"state("h", false):set(true)"#).exec().unwrap();
        pass(&mut scene);
        assert_eq!(width(&scene), 34.0);
        land(&mut scene);
        lua.load(r#"state("h", false):set(false)"#).exec().unwrap();
        pass(&mut scene);
        assert_eq!(width(&scene), 34.0, "the delay holds it open");
        land(&mut scene);
        assert_eq!(width(&scene), 0.0, "and lets go once it lands");
    }

    #[test]
    fn a_collapsed_width_stays_collapsed_through_the_pass_after_its_tween() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local open = state("open", true)
            return panel { id = "bar", child = row { geometry = geometry("g"), children = {
                row { width = open:map(function(o) return o and 34 or 0 end), height = 20,
                    opacity = open:map(function(o) return o and 1 or 0 end),
                    animate = { width = 100, opacity = 100 } },
                text { content = state("other", "a") } } } }"#,
        );
        let width = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].children[0].rect.width;
        let pass = |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        pass(&mut scene);
        lua.load(r#"state("open", true):set(false)"#).exec().unwrap();
        pass(&mut scene);
        let started = scene.surface("bar@TEST").unwrap().children[0].children[0].tweens[0].started;
        let instances = [instance_at(&surface, full())];
        for ms in [50, 100, 150] {
            scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(ms));
        }
        assert_eq!(width(&scene), 0.0, "the tween lands");
        pass(&mut scene);
        assert_eq!(width(&scene), 0.0, "a kept node keeps its landed width");
        lua.load(r#"state("other", "a"):set("b")"#).exec().unwrap();
        pass(&mut scene);
        assert_eq!(width(&scene), 0.0, "and so does a pass another write asked for");
    }

    #[test]
    fn a_lingering_surface_keeps_its_card_tweening_through_the_close() {
        // The root stays visible through `delay` while the card fades and lifts.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local open = state("open", true)
            local linger = computed({ open, delay(open, 147) }, function(now, was) return now or was end)
            return panel { id = "host", visible = linger, child = rect { width = "fill", height = "fill", children = {
                rect { width = "fill", height = "fill" },
                column { width = 100, margin = open:map(function(o) return { left = 30, top = o and 4 or -44 } end),
                    opacity = open:map(function(o) return o and 1 or 0 end),
                    animate = { opacity = { duration = 147, from = 0 }, margin = { duration = 147, easing = "out_quad" } },
                    children = { rect { width = 10, height = 10 } } } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let card = &scene.surface("host@TEST").unwrap().children[0].children[1];
        assert_eq!(card.tweens.len(), 1, "opacity enters from 0; margin has no from and snaps");
        let entered = card.tweens[0].started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, entered + std::time::Duration::from_secs(1));

        lua.load(r#"state("open", true):set(false)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let root = scene.surface("host@TEST").unwrap();
        assert!(root.visible, "delay keeps the surface mapped");
        let card = &root.children[0].children[1];
        assert_eq!(card.tweens.len(), 2, "opacity and margin both leave: {:?}", card.tweens);
        assert!(
            (card.opacity - 1.0).abs() < 0.01 && card.rect.x == 30.0 && card.rect.y == 4.0,
            "the close pass paints from where it was"
        );
        assert!(root.animating());
    }

    #[test]
    fn a_pass_that_does_not_move_the_target_keeps_the_running_tween() {
        let (mut scene, lua, surface) = animated_width("linear");
        let shaping = ShapingHandle::spawn();
        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = child_tween(&scene).started;
        // Neither a pass that resolves the node again with the same target nor one that keeps it
        // (ADR-0270) restarts it.
        crate::lua::signal::begin_evaluation(&lua);
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(child_tween(&scene).started, started, "resolved again");
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(child_tween(&scene).started, started, "kept");
    }

    #[test]
    fn a_retarget_mid_flight_starts_from_the_displayed_value_not_the_old_target() {
        let (mut scene, lua, surface) = animated_width("linear");
        let shaping = ShapingHandle::spawn();
        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let first = child_tween(&scene);
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, first.started + std::time::Duration::from_millis(50));
        assert_eq!(child_width(&scene), 65.0);

        // The pointer left: back to 40, from wherever the box is now.
        lua.load(r#"state("w", 40):set(40)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let second = child_tween(&scene);
        assert_eq!(second.to, node::Animatable::Number(40.0));
        assert_eq!(second.from, node::Animatable::Number(65.0), "from is the value the last tick displayed");
    }

    #[test]
    fn a_colour_tween_reaches_the_paint_style_between_passes() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"panel { id = "bar", child = rect { width = 10, height = 10,
                    background = state("bg", "#000000"),
                    animate = { background = { duration = 100, easing = "linear" } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r##"state("bg", "#000000"):set("#ffffff")"##).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = child_tween(&scene).started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let Some(PaintStyle::Box { background, .. }) = &scene.surface("bar@TEST").unwrap().children[0].paint else {
            panic!("a rect paints a box")
        };
        let [(node::Fill::Color(grey), _)] = background.as_slice() else { panic!("one colour: {background:?}") };
        assert!((grey.r - 0.5).abs() < 0.01, "halfway from black to white is mid grey, got {grey:?}");
    }

    /// ADR-0253. A `progress` tween re-derives the paint without a relayout, and the list it
    /// changes damages the shader's box and nothing else.
    #[test]
    fn a_progress_tween_repaints_only_the_shader_box() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"panel { id = "bar", child = row { width = 200, height = 40, children = {
                  rect { width = 50, height = 20, background = "#ffffff" },
                  shader { width = 40, height = 30, source = "/s.frag", progress = state("p", 0),
                           animate = { progress = { duration = 100, easing = "linear" } } } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("p", 0):set(1)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        assert!(root.tick_is_paint_only(), "`progress` asks the solver nothing");
        let before = crate::layout::paint::build(root, 1.0, None);
        let node = &root.children[0].children[1];
        let (rect, started) = (node.rect, node.tweens[0].started);
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50));

        let root = scene.surface("bar@TEST").unwrap();
        let node = &root.children[0].children[1];
        assert_eq!(node.rect, rect, "a paint-only tick moves no rect");
        let Some(PaintStyle::Shader { progress, .. }) = node.paint else { panic!("got {:?}", node.paint) };
        assert!((progress - 0.5).abs() < 0.01, "halfway from 0 to 1, got {progress}");
        let after = crate::layout::paint::build(root, 1.0, None);
        assert_ne!(after, before);
        let clip = crate::text::snap::snap_to_physical(rect, 1.0);
        let damage = after.damage_since(&before, true);
        assert_eq!(damage.len(), 1, "{damage:?}");
        let [d] = damage[..] else { unreachable!() };
        assert!(
            d.x0 >= clip.x0 - 2 && d.x1 <= clip.x1 + 2 && d.y0 >= clip.y0 - 2 && d.y1 <= clip.y1 + 2,
            "damage {d:?} outside the shader's box {clip:?}"
        );
    }

    /// ADR-0336. An endless `effect.shader.progress` loop advances on tick with no relayout, the
    /// shader's file and the rest of its table carried, and every step repaints the layer.
    #[test]
    fn an_effect_shader_progress_loop_advances_without_a_resolve() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local at = function(p) return { shader = { source = "/s.frag", padding = 3, progress = p } } end
            return panel { id = "bar", child = rect { width = 40, height = 30, background = "#ffffff", effect = at(0),
                animate = { effect = { keyframes = { at(0), at(1) }, duration = 100, easing = "linear", loops = "infinite" } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert!(root.tick_is_paint_only(), "`effect` asks the solver nothing");
        let before = crate::layout::paint::build(root, 1.0, None);
        let started = root.children[0].tweens[0].started;
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(25));
        let root = scene.surface("bar@TEST").unwrap();
        let shader = root.children[0].layout_style.effect.shader.as_ref().expect("the shader is carried");
        assert!((shader.progress - 0.25).abs() < 0.01, "a quarter along, got {}", shader.progress);
        assert_eq!((shader.padding, shader.source.to_str()), (3.0, Some("/s.frag")));
        assert_ne!(crate::layout::paint::build(root, 1.0, None), before, "the layer repaints");
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(125));
        let shader = scene.surface("bar@TEST").unwrap().children[0].layout_style.effect.shader.clone().unwrap();
        assert!((shader.progress - 0.25).abs() < 0.01, "and wraps, got {}", shader.progress);
    }

    /// ADR-0261. A `translate` and `scale` loop ticks without a relayout, and the input region
    /// follows it.
    #[test]
    fn a_transform_loop_ticks_paint_only_and_its_input_region_follows() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"panel { id = "bar", child = row { width = 400, height = 40, children = {
                  rect { width = 20, height = 20, background = "#ffffff",
                         animate = { translate = { duration = 100, easing = "linear", loops = "infinite",
                                                   keyframes = { { x = 0, y = 0 }, { x = 200, y = 0 } } },
                                     scale = { duration = 100, easing = "linear", loops = "infinite",
                                               keyframes = { 1, 2 } } } } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert!(root.tick_is_paint_only(), "a transform asks the solver nothing");
        let (rect, started) = (root.children[0].children[0].rect, root.children[0].children[0].tweens[0].started);
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));

        // Halfway: translate 100, scale 1.5 about the centre, so the box spans x 95..125.
        let root = scene.surface("bar@TEST").unwrap();
        let node = &root.children[0].children[0];
        assert_eq!(node.rect, rect, "a paint-only tick moves no rect");
        assert_eq!((node.transform.translate, node.transform.scale), ((100.0, 0.0), (1.5, 1.5)));
        assert_eq!(
            crate::layout::overlay_input_regions(root, 1.0),
            [crate::text::snap::PhysicalRect { x0: 95, y0: -5, x1: 125, y1: 25 }]
        );
    }

    /// What one tick costs on `read_seam_cost`'s 162-node tree, one node tweening:
    /// `MANTLE_PROFILE=1 cargo test -p renderer --release tick_cost -- --ignored --nocapture`.
    /// Ignored for the same reasons; the profile variable adds the relayout's split. `state` is the
    /// protocol state a ticked surface re-derives: 1.5us against a 2.6us paint-only tick, which is
    /// why a tick that cannot move a region still re-derives it rather than tracking which can.
    #[test]
    #[ignore]
    fn tick_cost() {
        let shaping = ShapingHandle::spawn();
        for (tweened, from, to) in [("width", "8", "40"), ("background", r##""#000000""##, r##""#ffffff""##)] {
            let (lua, surface) = surface_from(&format!(
                r##"local v = state("v", {from})
                local kids = {{}}
                for i = 1, 40 do
                  kids[i] = row {{ spacing = 2, background = "#204080FF", children = {{
                    rect {{ width = 8, height = 8, background = "#FFFFFFFF" }},
                    text {{ content = "item " .. i, font_size = 12 }},
                    rect {{ width = 8, height = 8, background = "#00FF00FF" }} }} }}
                end
                kids[1] = rect {{ width = 8, height = 8, background = "#FFFFFFFF", {tweened} = v,
                  animate = {{ {tweened} = {{ duration = 60000, easing = "linear" }} }} }}
                return panel {{ id = "bar", layer = "top", child = row {{ spacing = 4, children = kids }} }}"##
            ));
            let mut scene = Scene::new();
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            lua.load(format!(r#"state("v", {from}):set({to})"#)).exec().unwrap();
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            let started = scene.surface("bar@TEST").unwrap().children[0].children[0].tweens[0].started;
            let instances = [instance_at(&surface, full())];
            let ticks = 2_000u32;
            scene.take_tick_split();
            let clock = Instant::now();
            for frame in 0..ticks {
                let now = started + std::time::Duration::from_micros(u64::from(frame) * 100);
                assert!(!scene.tick(&instances, &shaping, &lua, now).is_empty());
            }
            let per = |d: Duration| d.as_secs_f64() * 1e6 / f64::from(ticks);
            let (elapsed, split) = (clock.elapsed(), scene.take_tick_split());
            // What `App::apply_resolved_state` derives from the tree a tick left.
            let tree = scene.surface("bar@TEST").unwrap();
            let clock = Instant::now();
            for _ in 0..ticks {
                std::hint::black_box((
                    node::panel_spec(&tree.properties).unwrap(),
                    crate::layout::overlay_input_regions(tree, 1.0),
                    crate::layout::blur_regions(tree, 1.0),
                ));
            }
            println!(
                "TICK tweened={tweened} per_tick={:.1}us (clone={:.1} prepare={:.1} solve={:.1}) state={:.1}us",
                per(elapsed),
                per(split.clone),
                per(split.prepare),
                per(split.solve),
                per(clock.elapsed()),
            );
        }
    }

    /// Two instances of one surface, one per output, as `output = "all"` makes them.
    fn two_outputs(surface: &VirtualNode) -> [SurfaceInstance; 2] {
        ["A", "B"].map(|output| SurfaceInstance {
            instance_id: format!("bar@{output}"),
            output: output.to_string(),
            ..instance_at(surface, full())
        })
    }

    /// A tick advances only the instances it is handed, and one ticked less often lands where the
    /// clock says rather than where its missed frames would have left it.
    #[test]
    fn a_tick_advances_only_the_instances_whose_frame_is_due() {
        let (_, lua, surface) = animated_width("linear");
        let (mut scene, shaping) = (Scene::new(), ShapingHandle::spawn());
        let [a, b] = two_outputs(&surface);
        let both = [a.clone(), b.clone()];
        scene.apply(std::slice::from_ref(&surface), &both, &shaping, &lua).unwrap();
        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        scene.apply(std::slice::from_ref(&surface), &both, &shaping, &lua).unwrap();
        let width = |scene: &Scene, id: &str| scene.surface(id).unwrap().children[0].rect.width;
        let started = scene.surface("bar@A").unwrap().children[0].tweens[0].started;

        let ticked = scene.tick([&a], &shaping, &lua, started + std::time::Duration::from_millis(50));
        assert_eq!(ticked, ["bar@A"]);
        assert_eq!((width(&scene, "bar@A"), width(&scene, "bar@B")), (65.0, 40.0), "B's frame was not due");

        scene.tick([&b], &shaping, &lua, started + std::time::Duration::from_millis(75));
        assert_eq!(width(&scene, "bar@B"), 77.5, "B catches up to the clock in one frame");
        assert!(scene.surface("bar@A").unwrap().animating(), "A still owes its own frames");
    }

    /// `tick_cost`'s tree on two outputs, one at 165 Hz and one at 60 Hz, for one second: ticking
    /// every animating instance on each callback, against ticking only the one whose callback it
    /// was. `cargo test -p renderer --release mixed_refresh_tick_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn mixed_refresh_tick_cost() {
        let (lua, surface) = surface_from(
            r##"local v = state("v", 8)
            local kids = {}
            for i = 1, 40 do
              kids[i] = row { spacing = 2, background = "#204080FF", children = {
                rect { width = 8, height = 8, background = "#FFFFFFFF" },
                text { content = "item " .. i, font_size = 12 },
                rect { width = 8, height = 8, background = "#00FF00FF" } } }
            end
            kids[1] = rect { width = v, height = 8, animate = { width = { duration = 60000, easing = "linear" } } }
            return panel { id = "bar", child = row { spacing = 4, children = kids } }"##,
        );
        let shaping = ShapingHandle::spawn();
        let [a, b] = two_outputs(&surface);
        let mut callbacks: Vec<(u64, &SurfaceInstance)> = (0..165u64).map(|i| (i * 1_000_000 / 165, &a)).collect();
        callbacks.extend((0..60u64).map(|i| (i * 1_000_000 / 60, &b)));
        callbacks.sort_by_key(|(at, _)| *at);
        for per_surface in [false, true] {
            let mut scene = Scene::new();
            let both = [a.clone(), b.clone()];
            scene.apply(std::slice::from_ref(&surface), &both, &shaping, &lua).unwrap();
            lua.load(r#"state("v", 8):set(40)"#).exec().unwrap();
            scene.apply(std::slice::from_ref(&surface), &both, &shaping, &lua).unwrap();
            lua.load(r#"state("v", 8):set(8)"#).exec().unwrap();
            let started = scene.surface("bar@A").unwrap().children[0].children[0].tweens[0].started;
            let (clock, mut relaid) = (Instant::now(), 0);
            for (at, due) in &callbacks {
                let now = started + std::time::Duration::from_micros(*at);
                let ticked = if per_surface {
                    scene.tick([*due], &shaping, &lua, now)
                } else {
                    scene.tick(&both, &shaping, &lua, now)
                };
                relaid += ticked.len();
            }
            println!(
                "MIXED per_surface={per_surface} relayouts={relaid} total={:.1}ms",
                clock.elapsed().as_secs_f64() * 1e3
            );
        }
    }

    #[test]
    fn a_fill_endpoint_snaps_instead_of_tweening() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = rect { width = state("w", 40), height = 10,
                    animate = { width = 100 } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("w", 40):set("fill")"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        // `Fill` under a content-sized panel is zero (see
        // `a_fill_child_of_a_content_sized_row_...`);
        // the point is that it got there in one pass with nothing left in flight.
        assert_eq!(child_width(&scene), 0.0);
        assert!(!scene.surface("bar@TEST").unwrap().animating());
    }

    #[test]
    fn font_size_tween_invalidates_text_memo() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = text { content = "Hello World",
                font_size = state("fs", 12),
                animate = { font_size = { duration = 100, easing = "linear" } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let width_12 = scene.surface("bar@TEST").unwrap().children[0].rect.width;

        lua.load(r#"state("fs", 12):set(36)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = scene.surface("bar@TEST").unwrap().children[0].tweens[0].started;

        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let width_mid = scene.surface("bar@TEST").unwrap().children[0].rect.width;
        assert!(width_mid > width_12, "box must grow as font_size tweens: {width_12} vs {width_mid}");

        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(150));
        let width_36 = scene.surface("bar@TEST").unwrap().children[0].rect.width;
        assert!(width_36 > width_mid, "completed tween must reach full size: {width_mid} vs {width_36}");

        // Tick after completion: memo is locked in and text retains the final shaped width.
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(200));
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].rect.width, width_36);
    }

    /// Ticks leave a typed tween's map entry behind. The pass after them reads what the tick
    /// displayed, a dropped tween's strip writes it back, and a settled tween leaves the target.
    #[test]
    fn a_pass_and_a_strip_after_typed_ticks_see_what_the_tick_displayed() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = rect { width = 10, height = 10, opacity = state("o", 0.2),
                animate = { opacity = { duration = 100, easing = "linear" } } } }"#,
        );
        let apply =
            |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let rect = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].clone();
        apply(&mut scene);
        lua.load(r#"state("o", 0.2):set(1)"#).exec().unwrap();
        apply(&mut scene);
        let started = rect(&scene).tweens[0].started;
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(50));

        let node = rect(&scene);
        assert!((node.opacity - 0.6).abs() < 1e-6, "halfway, got {}", node.opacity);
        assert_eq!(node.tweens[0].shown, Some(node::Animatable::Number(node.opacity)), "the tick's sample");
        assert_ne!(node.properties.get("opacity"), None, "the map holds an older value");
        let behind = node::Animatable::from_value("opacity", node.properties.get("opacity")).unwrap();
        assert_ne!(behind, Some(node::Animatable::Number(node.opacity)), "and it is not the displayed one");

        // Not back to 0.2, which would be a reversal timed by the pass's own clock.
        lua.load(r#"state("o", 0.2):set(0.5)"#).exec().unwrap();
        apply(&mut scene);
        let again = rect(&scene);
        assert_eq!(again.tweens[0].from, node::Animatable::Number(node.opacity), "a pass starts from the tick's value");
        assert_eq!(again.tweens[0].shown, None, "a pass writes the map itself");

        scene.tick(&instances, &shaping, &lua, again.tweens[0].started + Duration::from_millis(20));
        let mut stripped = rect(&scene);
        let shown = stripped.tweens[0].shown.clone();
        strip_tweens(&mut stripped, &lua);
        let held = node::Animatable::from_value("opacity", stripped.properties.get("opacity")).unwrap();
        assert!(
            stripped.tweens.is_empty() && held == shown,
            "a strip writes the sample back before it drops the tween"
        );
        scene.tick(&instances, &shaping, &lua, again.tweens[0].started + Duration::from_millis(200));
        assert_eq!(rect(&scene).opacity, 0.5, "a settled tween shows its target");
        let settled = rect(&scene);
        assert_eq!(
            node::Animatable::from_value("opacity", settled.properties.get("opacity")).unwrap(),
            Some(node::Animatable::Number(0.5))
        );
    }

    /// A typed tween leaves the map behind and writes its sample into the style. It has to land
    /// where the map route's parse does, at the start, mid-flight and at the end, for every kind
    /// the typed route covers, eased, sequenced or sprung; a retarget mid-flight has to start from
    /// the same value either way; and a `sync` has to bring the map to what the map route holds.
    #[test]
    fn a_typed_tween_lands_in_the_style_as_the_map_route_parses_it_and_retargets_from_it() {
        let lua = mlua::Lua::new();
        let now = Instant::now();
        let ms = Duration::from_millis;
        let eased = |from: &str| format!(r#"{{ duration = 100, easing = "linear", from = {from} }}"#);
        let sprung = |from: &str| format!("{{ spring = {{ stiffness = 200, damping = 8 }}, from = {from} }}");
        let shadows = r##"{ { color = "#336699cc", blur = 12, offset = { x = 1.5, y = 2.5 } }, { color = "#12345680", blur = 3 } }"##;
        let shadow_from = r##"{ { color = "#33669900", blur = 0 } }"##;
        // The property, its target, how it animates and the target a retarget moves to.
        let cases = [
            ("opacity", "1", eased("0.2"), "0.5"),
            ("rotate", "90", eased("0"), "-30"),
            ("scale", "2", eased("1"), "0.5"),
            ("scale", "{ x = 2, y = 0.5 }", eased("{ x = 1 }"), "{ y = 3 }"),
            ("translate", "{ x = 20, y = -8 }", eased("{ x = 0 }"), "{ x = 3, y = 3 }"),
            ("origin", "{ x = 0, y = 1 }", eased("{ x = 0.5, y = 0.5 }"), "{ x = 1 }"),
            ("shadows", shadows, eased(shadow_from), r##"{ { color = "#ff0000", blur = 6 } }"##),
            ("effect", "{ blur = 6, saturate = 1.5, backdrop = { blur = 3 } }", eased("{ blur = 0 }"), "{ blur = 2 }"),
            (
                "effect",
                r#"{ blur = 6, backdrop = { blur = 3, mask = { source = "/tmp/m.svg" } } }"#,
                eased("{ blur = 0 }"),
                "{ blur = 2 }",
            ),
            (
                "effect",
                "{ blur = 2, shader = { source = \"/tmp/x.frag\", progress = 1 } }",
                eased("{ blur = 0 }"),
                "{ blur = 3 }",
            ),
            (
                "effect",
                "{ blur = 2, shader = { source = \"/tmp/x.frag\", input = \"backdrop\", progress = 1 } }",
                eased("{ blur = 0 }"),
                "{ blur = 3 }",
            ),
            ("opacity", "1", "{ duration = 100, keyframes = { 0.2, 1, 0.5 } }".into(), "0.5"),
            (
                "translate",
                "{ x = 1 }",
                "{ duration = 100, keyframes = { { x = 0 }, { x = 10, y = 4 }, { x = -5 } } }".into(),
                "{ x = 3 }",
            ),
            (
                "effect",
                "{ blur = 6 }",
                "{ duration = 100, keyframes = { { blur = 0 }, { blur = 6 } } }".into(),
                "{ blur = 2 }",
            ),
            ("opacity", "1", sprung("0.2"), "0.5"),
            ("scale", "{ x = 2, y = 0.5 }", sprung("{ x = 1 }"), "{ y = 3 }"),
        ];
        let parsed = |map: &node::PropMap| {
            let style = LayoutStyle::parse(map).unwrap();
            (style.opacity, style.transform, style.effect)
        };
        for (property, to, entry, next) in &cases {
            let source = |to: &str| format!("return {{ {property} = {to}, animate = {{ {property} = {entry} }} }}");
            let mut props = node::rect_props(&lua, &source(to));
            let tweens = node::retarget("rect", None, &mut props, now, &lua).unwrap().0;
            assert_eq!(tweens.len(), 1, "{property} starts a tween");
            let next_props = node::rect_props(&lua, &source(next));
            for at in [0, 37, 100, 5000] {
                let when = now + ms(at);
                let [old, typed] = [false, true].map(|typed| {
                    let (mut tweens, mut map) = (tweens.clone(), std::rc::Rc::new(props.clone()));
                    let samples = node::step(&mut tweens, &mut map, when, &lua, typed).unwrap();
                    let mut style = LayoutStyle::parse(&props).unwrap();
                    overlaid(&mut style, &samples).unwrap();
                    node::commit(&mut tweens, &mut map, samples, when, &lua).unwrap();
                    (tweens, map, (style.opacity, style.transform, style.effect))
                });
                let expected = parsed(&old.1);
                assert_eq!(typed.2, expected, "{property} at {at}ms");
                assert_eq!(old.0.len(), typed.0.len(), "{property} at {at}ms: the same tweens arrive");
                let mut synced = typed.0.clone();
                let mut map = (*typed.1).clone();
                node::sync(&mut synced, &mut map, &lua).unwrap();
                assert_eq!(parsed(&map), expected, "{property} at {at}ms: a sync catches the map up");
                if typed.0.iter().any(|tween| tween.shown.is_some()) {
                    assert_eq!(
                        typed.1.get(property),
                        props.get(property),
                        "{property} at {at}ms: the map stays behind"
                    );
                }
                if at == 37 {
                    // Compared as text: an effect's carried shader is a table each route holds its own copy of.
                    let retarget = |tweens: &[node::Tween], map: &node::PropMap| {
                        let mut props = next_props.clone();
                        let tweens =
                            node::retarget("rect", Some((tweens, map)), &mut props, when + ms(3), &lua).unwrap().0;
                        let mut text = format!("{tweens:?}");
                        while let Some(at) = text.find("Ref(0x") {
                            let end = at + text[at..].find(')').unwrap();
                            text.replace_range(at..=end, "Ref");
                        }
                        text
                    };
                    assert_eq!(
                        retarget(&typed.0, &typed.1),
                        retarget(&old.0, &old.1),
                        "{property}: a retarget starts from the same value"
                    );
                }
            }
        }
        let mut named: Vec<_> = cases.iter().map(|case| case.0).collect();
        named.sort_unstable();
        named.dedup();
        let mut typed = node::TYPED.to_vec();
        typed.sort_unstable();
        assert_eq!(named, typed, "every typed property is covered here, and this covers no other");
        let mut style = LayoutStyle::parse(&node::rect_props(&lua, "return {}")).unwrap();
        for sample in [("background", node::Animatable::Number(1.0)), ("translate", node::Animatable::Number(1.0))] {
            assert!(overlaid(&mut style, &[sample]).is_err(), "a sample the style cannot take is refused");
        }
    }

    /// An animated `effect` still carries what it does not tween: `backdrop.mask` survives the
    /// pass that starts the tween, a typed tick, a relayout tick beside a `width` tween, and the
    /// settle.
    #[test]
    fn an_animated_effect_keeps_its_backdrop_mask() {
        for width in ["", r#"width = lit:map(function(o) return o and 30 or 10 end),"#] {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&format!(
                r#"local lit = state("lit", false)
                return panel {{ id = "bar", child = rect {{ height = 10, {width}
                    effect = lit:map(function(o)
                        return {{ backdrop = {{ blur = o and 8 or 0, mask = {{ source = "/tmp/m.svg" }} }} }}
                    end),
                    animate = {{ effect = {{ duration = 100, easing = "linear" }},
                                width = {{ duration = 100, easing = "linear" }} }} }} }}"#
            ));
            let apply = |scene: &mut Scene| {
                apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
            };
            let masked = |scene: &Scene| {
                let node = &scene.surface("bar@TEST").unwrap().children[0];
                (node.layout_style.effect.backdrop_mask.is_some(), node.layout_style.effect.backdrop)
            };
            apply(&mut scene);
            assert_eq!(masked(&scene), (true, 0.0), "declared");
            lua.load(r#"state("lit", false):set(true)"#).exec().unwrap();
            apply(&mut scene);
            assert!(masked(&scene).0, "the pass that starts the tween ({width:?})");
            let started = scene.surface("bar@TEST").unwrap().children[0].tweens[0].started;
            let instances = [instance_at(&surface, full())];
            scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(50));
            assert_eq!(masked(&scene), (true, 4.0), "mid-tween ({width:?})");
            scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(200));
            assert_eq!(masked(&scene), (true, 8.0), "settled ({width:?})");
        }
    }

    /// The exit starts from what the typed ticks displayed, not from the map they left behind.
    #[test]
    fn an_exit_starts_from_the_sample_a_typed_tick_displayed() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"local a = rect { id = "a", width = 10, height = 10, opacity = state("o", 0.2),
                   animate = { opacity = { duration = 100, easing = "linear" },
                               exit = { duration = 100, easing = "linear", opacity = 0 } } }
               return panel { id = "bar", child = row { children = state("kids", { a }) } }"##,
        );
        let apply =
            |scene: &mut Scene| apply_at(scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        apply(&mut scene);
        lua.load(r#"state("o", 0.2):set(1)"#).exec().unwrap();
        apply(&mut scene);
        let started = scene.surface("bar@TEST").unwrap().children[0].children[0].tweens[0].started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + Duration::from_millis(50));
        let shown = scene.surface("bar@TEST").unwrap().children[0].children[0].opacity;
        assert!((shown - 0.6).abs() < 1e-6, "halfway, got {shown}");

        lua.load(r#"state("kids", {}):set({})"#).exec().unwrap();
        apply(&mut scene);
        let leaver = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert!(leaver.leaving, "the exit block keeps it");
        assert_eq!(leaver.tweens[0].from, node::Animatable::Number(shown), "the exit leaves from the sample");
    }

    /// A refusal on the frame that ends a typed tween leaves the frame before it, in the style and
    /// in the map the strip writes back, not the target the refused frame was bound for.
    #[test]
    fn a_refusal_on_a_typed_tweens_last_frame_leaves_the_frame_before_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        // The last keyframe is a number where a `translate` takes a table, which the style refuses.
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = rect { width = 10, height = 10,
                animate = { translate = { duration = 100, keyframes = { { x = 0 }, { x = 10 }, 5 } } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let rect = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].clone();
        let started = rect(&scene).tweens[0].started;
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(30));
        let before = rect(&scene);
        let shown = before.tweens[0].shown.clone();
        assert!(shown.is_some() && before.transform.translate.0 > 0.0, "a typed frame on screen");

        scene.tick(&instances, &shaping, &lua, started + Duration::from_millis(10_000));
        let after = rect(&scene);
        assert!(after.tweens.is_empty(), "the refusal stops the tween");
        assert_eq!(after.transform, before.transform, "the style stays on the frame before");
        assert_eq!(
            node::Animatable::from_value("translate", after.properties.get("translate")).unwrap(),
            shown,
            "and the map is written back to it"
        );
    }
}

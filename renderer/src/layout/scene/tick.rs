use std::time::{Duration, Instant};

use mlua::{Lua, Value};
use shared::debug;

use super::pass::{publish_geometry, solve_instance};
use super::solver::{
    MainAxis, Measure, TEXT_MEASURE_KEYS, hold_leavers, main_axis_of, measure_for, new_solver_node, new_solver_tree,
    taffy_failed,
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

    /// Advances every tween to `now` and lays the affected instances out again from their retained
    /// property maps, without running Lua (ADR-0145): the only Lua the retained walk touches is a
    /// plain table read. Returns the instances it advanced, which is what the caller owes the
    /// screen this frame. An instance whose relayout fails keeps its last tree and loses its
    /// tweens, so a bug there is one log line and a snap rather than a log line per frame; the
    /// values a tick writes are ones a pass already accepted, so that should not happen.
    ///
    /// A tree whose every running tween only changes what it paints skips the relayout entirely
    /// and is advanced where it stands ([`advance_paint_only`]). That is most of what a config
    /// animates -- a fade, a hover colour, a border lighting up -- and none of it can move a rect.
    pub fn tick(
        &mut self,
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
        now: Instant,
    ) -> Vec<String> {
        // The retained maps keep a resolved edge table's handle, so its `__index` runs on the
        // parse a tick repeats; the pass budget is what bounds it here as in `apply_admitting`.
        let budget = match crate::lua::signal::LayoutPassBudget::enter(lua) {
            Ok(budget) => budget,
            Err(err) => {
                debug!("tick: no pass budget, skipping the frame: {err}");
                return Vec::new();
            }
        };
        let mut relaid = Vec::new();
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
                    strip_tweens(retained);
                }
                continue;
            }
            // ponytail: the clone is the rollback for a failure that should not happen; the same
            // shape `apply_admitting` uses per pass. Drop it once a tick has never failed in use.
            let mut at = open_span();
            let root = retained.clone();
            close(&mut at, &mut self.tick_split.clone);
            let outcome = relayout_retained(root, instance.available, shaping, lua, now, &mut at, &mut self.tick_split)
                .and_then(|tree| if budget.exceeded() { Err(LayoutError::PassBudgetExceeded) } else { Ok(tree) });
            match outcome {
                Ok(tree) => {
                    // Quiet: a tick never schedules a pass (ADR-0131).
                    if let Err(err) = publish_geometry(&tree, 0.0, 0.0, lua, true) {
                        debug!("{key}: writing a geometry signal failed: {err}");
                    }
                    *retained = tree;
                }
                Err(err) => {
                    debug!("{key}: relaying out a tween failed, snapping it: {err}");
                    strip_tweens(retained);
                }
            }
        }
        relaid
    }
}

fn strip_tweens(node: &mut ResolvedNode) {
    node.tweens.clear();
    node.children.iter_mut().for_each(strip_tweens);
}

/// One instance laid out again from what it retained, its tweens advanced to `now`. The Lua-free
/// twin of `Scene::apply_one_instance` plus [`prepare`]: no signal is read, no item function
/// called, no id allocated; every node keeps its identity and its resolved values, and only the
/// properties a tween carries move.
///
/// [`prepare`]: super::pass::prepare
fn relayout_retained(
    root: ResolvedNode,
    available: LogicalSize,
    shaping: &ShapingHandle,
    lua: &Lua,
    now: Instant,
    at: &mut Option<Instant>,
    split: &mut TickSplit,
) -> Result<ResolvedNode, LayoutError> {
    let mut tree = new_solver_tree();
    let prepared = prepare_retained(&mut tree, root, None, lua, now)?;
    close(at, &mut split.prepare);
    let solved = solve_instance(&mut tree, prepared, available, shaping);
    close(at, &mut split.solve);
    solved
}

/// [`prepare`] over a retained tree instead of a fresh one: same parse, same solver node, same
/// frozen-when-hidden rule, but the children are the ones the node already has and the tweens
/// are advanced rather than reconciled.
///
/// [`prepare`]: super::pass::prepare
fn prepare_retained(
    tree: &mut taffy::TaffyTree<Measure>,
    mut node: ResolvedNode,
    parent_axis: Option<MainAxis>,
    lua: &Lua,
    now: Instant,
) -> Result<PreparedNode, LayoutError> {
    // Checked before `advance`: on the frame a tween lands, `resting` is still false here,
    // so `text_memo` is cleared and the final layout size is measured before `resting` locks in
    // the memo on subsequent frames.
    let text_tweening =
        node.kind == "text" && node.tweens.iter().any(|t| !t.resting && TEXT_MEASURE_KEYS.contains(&t.property));
    node::advance(&mut node.tweens, &mut node.properties, now, lua)?;
    let style = LayoutStyle::parse(&node.properties)?;
    let ResolvedNode {
        id,
        kind,
        properties,
        children,
        tweens,
        displayed_source,
        dissolve,
        text_memo,
        list_memo,
        child_table,
        ..
    } = node;
    let text_memo = if text_tweening { None } else { text_memo };
    let paint = node::paint_style(kind, &properties)?;
    let measure = measure_for(kind, paint.as_ref(), &properties, text_memo)?;
    let taffy_id = new_solver_node(tree, kind, &properties, &style, parent_axis, measure)?;
    let node = PreparedNode {
        id,
        kind,
        style,
        properties,
        paint,
        displayed_source,
        dissolve: advanced_dissolve(dissolve, now),
        taffy: taffy_id,
        children: Vec::with_capacity(if style.visible { children.len() } else { 0 }),
        frozen: children,
        tweens,
        leaving: Vec::new(),
        list_memo,
        child_table,
    };
    if !node.style.visible {
        return Ok(node);
    }
    prepare_retained_children(tree, node, lua, now)
}

/// `node`'s retained children, in `frozen`, laid out again as they are: for a tick, and for a pass
/// over a `list` whose items would build the same (ADR-0269).
pub(super) fn prepare_retained_children(
    tree: &mut taffy::TaffyTree<Measure>,
    mut node: PreparedNode,
    lua: &Lua,
    now: Instant,
) -> Result<PreparedNode, LayoutError> {
    let own_axis = main_axis_of(node.kind, &node.properties)?;
    for child in std::mem::take(&mut node.frozen) {
        if child.leaving {
            if let Some(child) = advance_leaving(child, now, lua)? {
                node.leaving.push(child);
            }
        } else {
            node.children.push(prepare_retained(tree, child, own_axis, lua, now)?);
        }
    }
    hold_leavers(tree, node.taffy, &node.style, &node.leaving)?;
    let child_ids: Vec<taffy::NodeId> = node.children.iter().map(|child| child.taffy).collect();
    tree.set_children(node.taffy, &child_ids).map_err(taffy_failed)?;
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
        (
            Some(PaintStyle::Text { content, runs, .. }),
            Some(PaintStyle::Text { font_size, font, color, align, elide, wrap, max_lines, .. }),
        ) => Some(PaintStyle::Text { content, runs, font_size, font, color, align, elide, wrap, max_lines }),
        (_, fresh) => fresh,
    }
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
    // A played-out sequence rests on its last frame and moves nothing.
    if node.tweens.iter().any(|tween| !tween.resting) {
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
/// properties.
fn advance_paint_only_node(node: &mut ResolvedNode, now: Instant, lua: &Lua) -> Result<(), LayoutError> {
    let restore: Vec<(&'static str, Value)> = node
        .tweens
        .iter()
        .filter(|tween| !tween.resting)
        .filter_map(|tween| node.properties.get_key_value(tween.property))
        .map(|(property, value)| (*property, value.clone()))
        .collect();
    // Nothing is assigned to the node until every step has succeeded, so a refusal leaves its
    // `opacity`, `transform`, `effect` and `paint` describing the same frame its properties do.
    let advanced = node::advance(&mut node.tweens, &mut node.properties, now, lua).and_then(|()| {
        let properties = &node.properties;
        Ok((
            node::fields::common::opacity.read(properties)?,
            node::parse_transform(properties)?,
            node::parse_effect(properties)?,
            node::paint_style(node.kind, properties)?,
        ))
    });
    match advanced {
        Ok((opacity, transform, effect, fresh)) => {
            node.opacity = opacity;
            node.transform = transform;
            node.effect = effect;
            node.paint = repainted_keeping_fitted_text(node.paint.take(), fresh);
            Ok(())
        }
        Err(err) => {
            for (property, value) in restore {
                node.properties.insert(property, value);
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
    node.effect = style.effect;
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
    fn a_changed_target_starts_a_tween_from_the_value_on_screen_and_a_tick_carries_it() {
        // ADR-0145: the pass that sees `90` lays out `40` and a tween; the ticks do the rest
        // without Lua.
        let (mut scene, lua, surface) = animated_width("Linear");
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
        assert!(scene.tick(&instances, &shaping, &lua, tween.started + std::time::Duration::from_secs(1)).is_empty());
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

    /// The same runaway `__index` as
    /// `a_runaway_index_metamethod_fails_the_pass_instead_of_hanging_it`,
    /// armed only once the passes are done: a tick re-parses the retained edge table, so it runs
    /// the metamethod outside any `apply`. Slow on purpose, roughly `LAYOUT_PASS_CAP`.
    #[test]
    fn a_runaway_index_metamethod_snaps_the_tween_instead_of_hanging_the_tick() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local m = setmetatable({}, { __index = function() if hang then while true do end end return 2 end })
            return panel { id = "bar", child = rect { width = state("w", 40), height = 10, margin = m, animate = { width = 100 } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = child_tween(&scene).started;
        lua.load("hang = true").exec().unwrap();

        let clock = std::time::Instant::now();
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        assert!(clock.elapsed() < std::time::Duration::from_secs(20), "must be bounded, took {:?}", clock.elapsed());
        assert!(!scene.surface("bar@TEST").unwrap().animating(), "the tween is dropped, not retried next frame");
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
                            margin = { duration = 100, easing = "Linear", from = { left = 40 } } } } }"#,
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
            r##"local label = text { id = "label", width = 60, font_size = 14, elide = "End",
                   content = "a long message that will not fit", foreground = "#000000",
                   animate = { exit = { duration = 100, easing = "Linear", foreground = "#ff0000" } } }
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
                   animate = { exit = { duration = 100, easing = "Linear", opacity = 0, translate = { y = 8 } } } }
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
                   animate = { exit = { duration = 100, easing = "Linear", opacity = 0 } } }
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
                   animate = { exit = { duration = 100, easing = "Linear", opacity = 0 } } }
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
                   animate = { exit = { duration = 100, easing = "Linear", opacity = 0 } } } })"##,
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
                     animate = { opacity = { duration = 100, easing = "Linear", loops = "Infinite",
                                             keyframes = { 1, 0.2, 1 } } } },
                   rect { id = "flash", width = state("w", 10), height = 10, background = "#00ff00", opacity = 1,
                     animate = { opacity = { duration = 100, easing = "Linear", loops = 1,
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
                  return { opacity = { duration = 100, easing = "Linear", loops = 1,
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
                     animate = { background = { duration = 100, easing = "Linear" },
                                 opacity = { duration = 100, easing = "Linear" } } },
              text { content = "abc", foreground = lit:map(function(o) return o and "#ffffff" or "#000000" end),
                     animate = { foreground = { duration = 100, easing = "Linear" } } } } } }"##,
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
        let Some(PaintStyle::Box { background: Some(node::Fill::Color(background)), .. }) = &block.paint else {
            panic!("a rect paints a box, got {:?}", block.paint)
        };
        assert!((background.r - 0.5).abs() < 0.02, "halfway from black to white, got {}", background.r);
        let Some(PaintStyle::Text { content, color, .. }) = &label.paint else {
            panic!("a text paints text, got {:?}", label.paint)
        };
        assert_eq!(&**content, "abc", "the string it was fitted to survives a tick that never measured it");
        assert!((color.r - 0.5).abs() < 0.02, "the label's colour moved too, got {}", color.r);
        assert_eq!(block.rect, before, "nothing a paint-only tick writes can move a rect");
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
                shadow_color = "#000000", shadow_blur = up:map(function(u) return u and 8 or 0 end),
                shadow_offset = up:map(function(u) return u and { x = 0, y = 4 } or { x = 0, y = 0 } end),
                content_blur = up:map(function(u) return u and 2 or 0 end),
                backdrop_blur = up:map(function(u) return u and 8 or 0 end),
                animate = { shadow_blur = { duration = 100, easing = "Linear" },
                            shadow_offset = { duration = 100, easing = "Linear" },
                            content_blur = { duration = 100, easing = "Linear" },
                            backdrop_blur = { duration = 100, easing = "Linear" } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("up", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert!(root.tick_is_paint_only(), "an effect asks the solver nothing");
        let started = root.children[0].tweens[0].started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let effect = scene.surface("bar@TEST").unwrap().children[0].effect;
        let shadow = effect.shadow.expect("halfway, the shadow shows");
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
            return panel { id = "bar", child = row { width = 100, height = 20, children = {
              rect { height = 10, background = "#ffffff",
                     width = wide:map(function(w) return w and 40 or 10 end),
                     animate = { width = { duration = 100, easing = "Linear" } } } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r#"state("wide", false):set(true)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        assert!(!root.tick_is_paint_only(), "a width is the solver's business");
        let started = root.children[0].children[0].tweens[0].started;
        let instances = [instance_at(&surface, full())];
        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(50));
        let block = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert!((block.rect.width - 25.0).abs() < 0.5, "halfway from 10 to 40, got {}", block.rect.width);
    }

    #[test]
    fn a_lingering_surface_keeps_its_card_tweening_through_the_close() {
        // The root stays visible through `delay` while the card fades and lifts.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"local open = state("open", true)
            local linger = computed({ open, delay(open, 147) }, function(now, was) return now or was end)
            return panel { id = "host", visible = linger, child = rect { width = "Fill", height = "Fill", children = {
                button { width = "Fill", height = "Fill" },
                column { width = 100, margin = open:map(function(o) return { left = 30, top = o and 4 or -44 } end),
                    opacity = open:map(function(o) return o and 1 or 0 end),
                    animate = { opacity = { duration = 147, from = 0 }, margin = { duration = 147, easing = "OutQuad" } },
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
        let (mut scene, lua, surface) = animated_width("Linear");
        let shaping = ShapingHandle::spawn();
        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = child_tween(&scene).started;
        // An unrelated re-resolve (any signal write dirties the whole scene) must not restart it.
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(child_tween(&scene).started, started);
    }

    #[test]
    fn a_retarget_mid_flight_starts_from_the_displayed_value_not_the_old_target() {
        let (mut scene, lua, surface) = animated_width("Linear");
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
                    animate = { background = { duration = 100, easing = "Linear" } } } }"##,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        lua.load(r##"state("bg", "#000000"):set("#ffffff")"##).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let started = child_tween(&scene).started;
        scene.tick(&[instance_at(&surface, full())], &shaping, &lua, started + std::time::Duration::from_millis(50));
        let Some(PaintStyle::Box { background: Some(node::Fill::Color(grey)), .. }) =
            &scene.surface("bar@TEST").unwrap().children[0].paint
        else {
            panic!("a rect paints a box")
        };
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
                           animate = { progress = { duration = 100, easing = "Linear" } } } } } }"##,
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

    /// ADR-0261. A `translate` and `scale` loop ticks without a relayout, and the input region
    /// follows it.
    #[test]
    fn a_transform_loop_ticks_paint_only_and_its_input_region_follows() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r##"panel { id = "bar", child = row { width = 400, height = 40, children = {
                  rect { width = 20, height = 20, background = "#ffffff",
                         animate = { translate = { duration = 100, easing = "Linear", loops = "Infinite",
                                                   keyframes = { { x = 0, y = 0 }, { x = 200, y = 0 } } },
                                     scale = { duration = 100, easing = "Linear", loops = "Infinite",
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
    /// Ignored for the same reasons; the profile variable adds the relayout's split.
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
                  animate = {{ {tweened} = {{ duration = 60000, easing = "Linear" }} }} }}
                return panel {{ id = "bar", child = row {{ spacing = 4, children = kids }} }}"##
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
            let split = scene.take_tick_split();
            println!(
                "TICK tweened={tweened} per_tick={:.1}us (clone={:.1} prepare={:.1} solve={:.1})",
                per(clock.elapsed()),
                per(split.clone),
                per(split.prepare),
                per(split.solve),
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
        lua.load(r#"state("w", 40):set("Fill")"#).exec().unwrap();
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
                animate = { font_size = { duration = 100, easing = "Linear" } } } }"#,
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
        assert!(scene.surface("bar@TEST").unwrap().children[0].text_memo.is_some());
    }
}

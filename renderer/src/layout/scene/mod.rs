//! Resolve/reconcile transaction for the retained scene. [`prepare`] resolves and parses in
//! declaration order, matches ids within each parent (id-less children remain positional), and
//! drops removed subtrees. [`solve`] delegates layout to taffy; `finish` reads
//! geometry back and performs scroll writeback and text elision. The seam that defines `row` and
//! `Fill` is `taffy_style` (ADR-0077).
//!
//! [`solve`]: solver::solve

mod fit;
mod pass;
mod resolve;
mod scroll;
mod solver;
mod tick;
pub use tick::TickSplit;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use mlua::{Lua, Value};

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{self, Align, Dissolve, EdgeInsets, LayoutError, PaintStyle, PropMap, SizeMode, Tween};
use crate::layout::paint::DrawnImage;
use crate::layout::secure_submit::lock_stays_authenticatable;
use crate::lua::nodes::VirtualNode;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;
use pass::{build_child_for_output, prepare, publish_geometry, solve_instance};
use resolve::{ResolveMemo, resolve};
use solver::new_solver_tree;
pub(crate) use solver::{MainAxis, main_axis_of};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

/// [`prepare`] admits 64 levels and refuses the next, matching
/// `lua::signal::budget::MAX_SIGNAL_NESTING_DEPTH`'s boundary. This catches literal cycles and
/// depth-generating `children` signals; the two limits are sized together because each node can
/// nest signal evaluation.
///
/// Measured end to end on a 2 MiB debug thread with a 31-deep `computed` chain on every level; the
/// compound worst case reaches the abort boundary and counts all frames, including Lua and refusal
/// error-formatting frames. A 64-level tree without signals uses 590 KiB, about 8,960 B/level;
/// 1,040 KiB with signals, the extra 450 KiB paid once as the chain unwinds. The 64-level case is a
/// 1.97x margin, versus 1,400 KiB/1.44x for the old hand-written solver (ADR-0077). Production uses
/// an 8 MiB main thread; real configs are 10-15 levels deep. Signal nesting remains 32 for
/// dependency chains.
const MAX_TREE_DEPTH: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

#[cfg(test)]
impl NodeId {
    /// A hand-picked id for a `ResolvedNode` built by hand in a test. Production ids come from
    /// [`Scene::alloc_id`] and nothing else, which is what makes them unique; a test that builds a
    /// tree without a `Scene` still has to say which nodes are the same node and which are not.
    pub(crate) const fn test(raw: u64) -> Self {
        NodeId(raw)
    }
}

#[cfg(test)]
impl ResolvedNode {
    /// A visible, unpainted node built by hand in a test, id 0; the test sets any other field.
    pub(crate) fn test(
        kind: &'static str,
        (x, y, width, height): (f32, f32, f32, f32),
        children: Vec<ResolvedNode>,
    ) -> Self {
        ResolvedNode {
            id: NodeId::test(0),
            kind,
            rect: LogicalRect { x, y, width, height },
            margin: EdgeInsets::default(),
            layout_style: std::rc::Rc::new(LayoutStyle::parse(&PropMap::default()).unwrap()),
            taffy: None,
            visible: true,
            opacity: 1.0,
            z: 0.0,
            transform: node::Transform::default(),
            blur: false,
            effect: node::Effect::default(),
            properties: PropMap::default(),
            paint: None,
            displayed_source: None,
            dissolve: None,
            children,
            tweens: Vec::new(),
            leaving: false,
            text_memo: None,
            list_memo: None,
            child_table: None,
            resolve_memo: None,
        }
    }
}

/// Geometry parsed once per node/pass. A resolved table's `__index` still runs on each access, so
/// this is separate from reading a `Signal`: one read per field is what keeps a child's margin one
/// answer for the measure and the placement. The parent parses a
/// child before recursing because it needs the margin and size for the solver (ADR-0077); ignored
/// fields are still validated so a later kind change cannot hide a malformed property (ADR-0068).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LayoutStyle {
    margin: EdgeInsets,
    padding: EdgeInsets,
    width_mode: SizeMode,
    height_mode: SizeMode,
    /// Ceilings for `Content` growth; overflow goes to `scroll`.
    max_width: Option<f32>,
    max_height: Option<f32>,
    /// Floors for `Content` growth; a declared one replaces taffy's disabled automatic minimum.
    min_width: Option<f32>,
    min_height: Option<f32>,
    align_h: Align,
    align_v: Align,
    spacing: f32,
    visible: bool,
    opacity: f32,
    z: f32,
    transform: node::Transform,
    blur: bool,
    effect: node::Effect,
}

impl LayoutStyle {
    /// The one parse of one node's geometry for one pass. `properties` must already be a
    /// [`node::resolve_properties`] result: this reads values, it does not resolve signals.
    fn parse(properties: &PropMap) -> Result<Self, LayoutError> {
        // Validated and not kept: the pointer path reads the name back off `properties` when it
        // needs it (`layout::hit::cursor_under`), and a pass is the place a misspelling fails.
        use node::fields::{common, flow, paint};
        common::cursor.read(properties)?;
        Ok(Self {
            margin: common::margin.read(properties)?,
            padding: common::padding.read(properties)?,
            width_mode: common::width.read(properties)?,
            height_mode: common::height.read(properties)?,
            max_width: common::max_width.read(properties)?,
            max_height: common::max_height.read(properties)?,
            min_width: common::min_width.read(properties)?,
            min_height: common::min_height.read(properties)?,
            align_h: common::align_h.read(properties)?,
            align_v: common::align_v.read(properties)?,
            // `row`/`column`'s row; a `list`'s agrees, and no other kind has one.
            spacing: flow::spacing.read(properties)?,
            visible: common::visible.read(properties)?,
            opacity: common::opacity.read(properties)?,
            // -0.0 would sort below its z = 0 siblings.
            z: common::z.read(properties)? + 0.0,
            transform: crate::layout::node::parse_transform(properties)?,
            blur: paint::blur.read(properties)?,
            effect: crate::layout::node::parse_effect(properties)?,
        })
    }
}

/// The one node type: what a pass produces, what [`Scene`] retains for the next reconcile, and
/// what every reader borrows. Geometry, parsed paint, and the resolved property map retained for
/// `hover`, callbacks, and surface specs (`wayland::surface::apply_resolved_state` re-derives
/// those at configure cadence). Used by `Scene::surface` and `region::overlay_input_regions`.
///
/// `Clone` exists for `Scene::apply`'s rollback snapshot. Readers take `&` from `Scene::surface`;
/// nothing outside this module builds one.
///
/// `resolve_properties` ran over this node's raw map exactly once, so this snapshot lets later
/// readers take values without resolving anything. `properties` holds resolved values, never a
/// `Signal` handle. Structural keys are copied raw by `node::is_structural_property`, whose parsers
/// reject signals.
///
/// ponytail: absent and nil are one state here (`node::resolve_properties` omits a key whose
/// signal resolved to `Value::Nil`, ADR-0044 decision 1's amendment), so a paint-only property
/// bound to a still-unresolved capability signal reads as its parser's default. Upgrade path: a
/// third state, `Value::Nil` retained as "bound but unresolved".
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    /// Cached for layout ticks; a pass replaces it after resolving properties.
    pub(crate) layout_style: std::rc::Rc<LayoutStyle>,
    /// The solver node in `Scene::solver_trees`, valid only while that instance's tree is cached.
    pub(crate) taffy: Option<taffy::NodeId>,
    /// The identity its retained counterpart was reconciled under, carried so a later reader can
    /// say "this node, again" across passes. Stable by construction: `reconcile_node` keeps the
    /// retained node's id and only allocates when there was nothing to match, so an id survives
    /// the node moving, resizing, or gaining siblings ahead of it (ADR-0099).
    ///
    /// Not addressable from Lua and not the `id` property, which is a reconciliation *hint*
    /// a config writes and this is the answer the engine reached.
    pub id: NodeId,
    pub kind: &'static str,
    pub rect: LogicalRect,
    /// This node's own margin, kept because a parent measures its children's footprint after they
    /// are built ([`extent_along`]).
    ///
    /// [`extent_along`]: scroll::extent_along
    pub margin: EdgeInsets,
    pub visible: bool,
    /// This node's own `opacity`, before any ancestor's. `layout::paint::build_node` multiplies
    /// the chain descending, the same way it intersects a clip, so a panel fades with everything
    /// in it from one property. 1.0 is the default and contributes nothing.
    pub opacity: f32,
    /// Paint and hit order among siblings (ADR-0259); [`Self::painted_children`] reads it.
    pub z: f32,
    /// This node's own paint-only affine (ADR-0149), applied about its box after layout; `rect`
    /// and everything the solver produced are untransformed. `layout::paint` composes it down
    /// the subtree, `layout::hit` maps the pointer back through its inverse.
    pub transform: node::Transform,
    /// This node asked for the desktop behind it to be blurred (`blur`, ADR-0195).
    /// `layout::blur_regions` turns every one of these in a surface into the one region the
    /// compositor is given; nothing else reads it, and a compositor without the protocol ignores the
    /// lot.
    pub blur: bool,
    /// This node's own `shadow_*` and `content_blur` (ADR-0254), over its whole painted subtree.
    pub effect: node::Effect,
    pub properties: PropMap,
    /// This node's paint properties, parsed here rather than by `layout::paint` on every frame
    /// (`node::paint_style`'s module doc comment says why). `None` for a kind that draws nothing.
    pub paint: Option<PaintStyle>,
    /// The `source` this node last had a texture for, for an `image` declaring `retain`
    /// (ADR-0180). `layout::paint` draws it while a newly named source is still decoding, so the
    /// node holds its last picture instead of going blank; [`Scene::note_drawn_images`] moves it
    /// forward as paint proves it has each texture. `None` for every other kind, and until the
    /// first source is drawn.
    pub displayed_source: Option<String>,
    /// The cross-dissolve this `image` is in the middle of (ADR-0181), from the source it was
    /// covering the gap with to the one `displayed_source` has just moved to. `None` whenever the
    /// node is showing one picture, which is nearly always.
    pub dissolve: Option<Box<Dissolve>>,
    pub children: Vec<ResolvedNode>,
    /// Properties in flight between two resolved targets (ADR-0145). `properties` holds what is
    /// displayed this frame, each tween the target it is heading for; `Scene::tick` advances them
    /// between passes and `node::retarget` reconciles them against the next pass's values.
    pub tweens: Vec<Tween>,
    /// The tree no longer holds this node; it stays, at the rect it last had, while its
    /// `animate.exit` tweens run (ADR-0150). Out of flow (siblings have already closed over its
    /// slot) and out of reach (no hit, no input region, no geometry), painted after its live
    /// siblings until the last tween ends, when the next pass drops it.
    pub leaving: bool,
    /// The last `(max_width, size)` a `text` node measured, carried so unchanged text skips shaping.
    pub text_memo: Option<(Option<f32>, taffy::Size<f32>)>,
    /// What a `list` built its items from, so a pass that finds it unchanged keeps them.
    pub list_memo: Option<node::ListMemo>,
    /// What its `children` or `child` table last read as, so a pass holding the same table skips
    /// reading it again.
    pub child_table: Option<pass::ChildTable>,
    /// What `properties` were resolved from, so a pass that finds it unchanged keeps them. Shared,
    /// so the rollback copy of the tree does not copy the raw values it holds.
    pub resolve_memo: Option<std::rc::Rc<ResolveMemo>>,
}

impl ResolvedNode {
    /// Visible and laid out this pass: what a flow measures and a reader may address. A leaving
    /// node is visible and neither.
    pub(super) fn in_flow(&self) -> bool {
        self.visible && !self.leaving
    }

    /// `clip = "None"` hands children the parent's clip instead of cutting them to this box.
    pub(super) fn clips_children(&self) -> bool {
        !matches!(self.paint, Some(PaintStyle::Box { clip: node::ClipShape::None, .. }))
    }

    /// Children bottom to top: ascending `z`, declaration order among equals (ADR-0259).
    /// Allocates only when `z` reorders something.
    pub(super) fn painted_children(&self) -> impl DoubleEndedIterator<Item = &ResolvedNode> {
        let sorted = self.children.is_sorted_by(|a, b| a.z <= b.z);
        let mut resorted: Vec<&ResolvedNode> = if sorted { Vec::new() } else { self.children.iter().collect() };
        resorted.sort_by(|a, b| a.z.total_cmp(&b.z));
        self.children.iter().filter(move |_| sorted).chain(resorted)
    }

    /// A button with `submit = true` or a pointer handler (ADR-0214).
    pub fn takes_pointer(&self) -> bool {
        self.kind == "button"
            && (matches!(self.properties.get("submit"), Some(Value::Boolean(true)))
                || ["on_click", "on_drag", "on_wheel"]
                    .iter()
                    .any(|handler| matches!(self.properties.get(*handler), Some(Value::Function(_)))))
    }

    /// Whether any visible node in this tree is mid-tween, which is what asks the compositor for
    /// another frame callback (`wayland::surface::App::paint_surface`). A hidden node's subtree is
    /// frozen (ADR-0124), tweens included: nothing advances them, so counting them would arm a
    /// callback chain that never ends. They wait there and the thaw's resolve settles them.
    pub fn animating(&self) -> bool {
        self.visible
            && (self.tweens.iter().any(|tween| !tween.resting)
                || self.dissolve.is_some()
                || self.children.iter().any(ResolvedNode::animating))
    }

    /// Whether every tween still running in this tree moves a paint-only property, which is what
    /// lets [`Scene::tick`] advance it in place instead of laying it out again
    /// (`node::is_paint_only`).
    ///
    /// A hidden subtree answers `true` because nothing advances it either way: it is frozen
    /// (ADR-0124) and `animating` does not count it. A leaving node answers `false` whatever its
    /// exit moves, because the end of that exit drops the node, and changing the shape of the tree
    /// is `prepare_retained`'s to do (ADR-0150).
    fn tick_is_paint_only(&self) -> bool {
        if !self.visible {
            return true;
        }
        // The dissolve is not tested: all it moves is the alpha the incoming source is drawn at,
        // which is as paint-only as a property gets.
        !self.leaving
            && self.tweens.iter().all(|tween| tween.resting || node::is_paint_only(tween.property))
            && self.children.iter().all(ResolvedNode::tick_is_paint_only)
    }

    /// This node's margin along `axis`, both edges.
    fn margin_on(&self, axis: MainAxis) -> f32 {
        match axis {
            MainAxis::Horizontal => self.margin.horizontal(),
            MainAxis::Vertical => self.margin.vertical(),
        }
    }
}

/// Persistent trees keyed by surface instance (`"{id}@{output}"`), not declared id. A panel on
/// `monitor = "All"` needs separate trees for a laptop and 4K output because their geometry
/// differs. Surface `id` remains reconcile identity; the output suffix distinguishes instances
/// (ADR-0045). Descendants use per-parent id matching, with positional fallback for id-less nodes.
#[derive(Default)]
pub struct Scene {
    surfaces: HashMap<String, ResolvedNode>,
    /// Kept only while an instance has layout animation in flight.
    solver_trees: HashMap<String, taffy::TaffyTree<solver::Measure>>,
    next_id: u64,
    resolve_split: ResolveSplit,
    tick_split: TickSplit,
}

/// Where one resolve pass spends itself, split the three ways [`Scene::apply_one_instance`]
/// divides into: saving the retained tree for rollback, running Lua and building the solver tree,
/// then solving and measuring. `ms resolve` is one number for all three plus the tween tick, which
/// says the phase is expensive without saying which part is.
///
/// Sums to less than `ms resolve`: a tick-only turn resolves nothing, and the per-pass work
/// outside `apply_one_instance` (the budget, `start_secure_submit_capabilities`) is in neither.
/// Never past it: the turn loop drops what `apply_instances` and `handle_apply_pending` accumulate
/// from dispatch, which `ms resolve` does not cover either.
#[derive(Clone, Copy, Default)]
pub struct ResolveSplit {
    pub clone: Duration,
    pub list: Duration,
    pub resolve: Duration,
    pub solve: Duration,
}

/// One getenv for the process, like the profilers this feeds: `--profile` cannot come and go while
/// the Renderer runs. Off, [`open_span`] reads no clock, the switch `Phases` already uses.
pub(crate) fn timing_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| shared::profile_interval().is_some())
}

/// Opens a timing span, or `None` with the profile off.
fn open_span() -> Option<Instant> {
    timing_on().then(Instant::now)
}

/// Closes `at` into `total` and reopens, so three spans cost three clock reads, not six.
fn close(at: &mut Option<Instant>, total: &mut Duration) {
    if let Some(started) = *at {
        let now = Instant::now();
        *total += now.duration_since(started);
        *at = Some(now);
    }
}

/// Nodes in one retained tree: the half of [`census_walk`] [`Scene::census_by_surface`] wants.
fn count_nodes(node: &ResolvedNode) -> usize {
    1 + node.children.iter().map(count_nodes).sum::<usize>()
}

/// Adds `node` and its descendants to the running node and property totals. Shared by
/// [`Scene::census`] and [`Scene::census_by_surface`] so the per-surface figures always sum to the
/// total the same report prints beside them.
fn census_walk(node: &ResolvedNode, nodes: &mut usize, properties: &mut usize) {
    *nodes += 1;
    *properties += node.properties.len();
    for child in &node.children {
        census_walk(child, nodes, properties);
    }
}

struct InstanceResolveGuard<'a>(&'a Lua);

impl<'a> InstanceResolveGuard<'a> {
    fn enter(lua: &'a Lua, instance_id: &str) -> Self {
        crate::lua::signal::begin_instance_resolve(lua, instance_id);
        Self(lua)
    }
}

impl Drop for InstanceResolveGuard<'_> {
    fn drop(&mut self) {
        crate::lua::signal::end_instance_resolve(self.0);
    }
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drained, not read: a turn that only ticked resolves nothing and must report zero rather
    /// than whichever pass ran last.
    pub fn take_resolve_split(&mut self) -> ResolveSplit {
        std::mem::take(&mut self.resolve_split)
    }

    fn alloc_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Test-only unguarded apply; production uses the admitting path that the lock veto depends on.
    #[cfg(test)]
    pub fn apply(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        self.apply_admitting(fresh_surfaces, instances, shaping, lua, |_| Ok(()))
    }

    /// Every production apply. While the session is locked the same rule vetoes each one, so no
    /// path can drop the password field (ADR-0052 decision 3).
    pub fn apply_locked(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
        locked: bool,
    ) -> Result<(), LayoutError> {
        self.apply_admitting(fresh_surfaces, instances, shaping, lua, |scene| {
            lock_stays_authenticatable(scene, instances, locked)
        })
    }

    /// Reconciles one retained tree per mapped instance against `fresh_surfaces`, using each
    /// instance's `available` size. Missing declared instances are skipped for unplugged outputs;
    /// an instance naming no declaration is an `InvalidProperty`. Retained instances absent from
    /// this cycle stay for topology handling, not in-place apply.
    ///
    /// `admit` vetoes the finished apply after all instances, asking whether the whole resolved
    /// lock tree remains authenticatable; it rolls back on error. The snapshot restores exactly the
    /// pre-call state because a failing getter may already have changed `next_id` or
    /// the trees (`CONTEXT.md`, Rollback; `socket/client/mod.rs::reevaluate`).
    ///
    /// ponytail: every visited instance's tree is deep-cloned as rollback, even on success; the
    /// dirty flag limits this to capability-push cadence. The structural clone is O(nodes), not
    /// O(Lua heap).
    fn apply_admitting(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
        admit: impl Fn(&Scene) -> Result<(), LayoutError>,
    ) -> Result<(), LayoutError> {
        let next_id_snapshot = self.next_id;
        // One budget for the whole pass: the hook covers gaps where a resolved table's `__index`
        // runs, and individually legal 5ms getters cannot add up without a pass deadline.
        let budget = match crate::lua::signal::LayoutPassBudget::enter(lua) {
            Ok(budget) => budget,
            Err(err) => return Err(node::invalid("layout", err.to_string())),
        };
        let mut rollback = Vec::new();
        let outcome = self.apply_visiting(fresh_surfaces, instances, shaping, lua, &budget, &mut rollback, admit);
        if outcome.is_err() {
            for (key, tree) in rollback {
                self.solver_trees.remove(&key);
                match tree {
                    Some(tree) => self.surfaces.insert(key, tree),
                    None => self.surfaces.remove(&key),
                };
            }
            self.next_id = next_id_snapshot;
        }
        outcome
    }

    /// [`Self::apply_admitting`] minus the snapshot and rollback, so the three failure exits are
    /// one `?` each rather than three copies of the restore.
    #[allow(clippy::too_many_arguments)]
    fn apply_visiting(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
        budget: &crate::lua::signal::LayoutPassBudget,
        rollback: &mut Vec<(String, Option<ResolvedNode>)>,
        admit: impl Fn(&Scene) -> Result<(), LayoutError>,
    ) -> Result<(), LayoutError> {
        // One clock reading for the pass, so every tween it starts shares a start.
        let now = Instant::now();
        // A hook interruption can look like an arbitrary `InvalidProperty`; report the pass budget
        // instead whenever the deadline was exceeded.
        let blame_the_budget =
            |outcome: LayoutError| if budget.exceeded() { LayoutError::PassBudgetExceeded } else { outcome };

        // Every instance runs even after one fails, so the report covers the whole scene. One
        // declaration on three outputs is one mistake, reported once under its first instance.
        let mut failed = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for instance in instances {
            if let Err(err) = self.apply_one_instance(fresh_surfaces, instance, shaping, lua, now, rollback) {
                // Blame first: a hook interruption is about the pass, not this instance.
                if budget.exceeded() {
                    return Err(LayoutError::PassBudgetExceeded);
                }
                for err in err.into_each() {
                    if seen.insert((instance.declared_id.as_str(), err.to_string())) {
                        failed.push(err.on_surface(&instance.instance_id));
                    }
                }
            }
        }
        if !failed.is_empty() {
            return Err(LayoutError::many(failed));
        }
        admit(self).map_err(blame_the_budget)?;
        // Lua can catch the hook error with `pcall`; the final deadline check cannot be caught.
        if budget.exceeded() {
            return Err(LayoutError::PassBudgetExceeded);
        }
        Ok(())
    }

    /// One instance's worth of `apply`'s loop body, split out so `apply` can wrap it in a single
    /// early-return-on-error site instead of duplicating the rollback at every `?`.
    fn apply_one_instance(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instance: &SurfaceInstance,
        shaping: &ShapingHandle,
        lua: &Lua,
        now: Instant,
        rollback: &mut Vec<(String, Option<ResolvedNode>)>,
    ) -> Result<(), LayoutError> {
        let _guard = InstanceResolveGuard::enter(lua, &instance.instance_id);
        // Match the declared id, then key the retained tree by instance id (ADR-0045 decision 1).
        let mut fresh = None;
        for candidate in fresh_surfaces {
            if node::fields::surface::id.read(&candidate.properties)? == instance.declared_id {
                fresh = Some(candidate);
                break;
            }
        }
        let Some(fresh) = fresh else {
            return Err(node::invalid(
                "id",
                format!(
                    "surface instance `{}` names a surface `{}` this evaluation did not declare -- \
                     instances and declarations come from the same evaluation, so this is a caller bug, not a config error",
                    instance.instance_id, instance.declared_id
                ),
            ));
        };
        let key = instance.instance_id.clone();
        let available = instance.available;
        let mut at = open_span();
        let mut existing = self.surfaces.remove(&key);
        rollback.push((key.clone(), existing.clone()));
        close(&mut at, &mut self.resolve_split.clone);
        // Check admissibility before resolution runs Lua. Children get the same check in the loop
        // that parses their margin before recursing.
        ensure_node_admissible(fresh.kind, 0)?;
        // Cloned, not moved: one declaration is resolved once per instance of it, one per output.
        // The root has no parent, so its resolve happens here; children resolve in the parent's loop.
        let resolved = resolve(fresh.kind, fresh.properties.clone(), existing.as_mut(), now, lua, |properties| {
            build_child_for_output(properties, fresh.kind, &instance.output)
        })?;

        // A failed walk drops its temporary solver tree without extra rollback state.
        let mut tree = new_solver_tree();
        let prepared = prepare(self, &mut tree, existing, fresh.kind, resolved, None, false, lua, now, 0)?;
        close(&mut at, &mut self.resolve_split.resolve);
        let solved = solve_instance(&mut tree, prepared, available, shaping)?;
        publish_geometry(&solved, 0.0, 0.0, lua, false).map_err(|e| node::invalid("geometry", e.to_string()))?;
        if solved.animating() && !solved.tick_is_paint_only() {
            self.solver_trees.insert(key.clone(), tree);
        } else {
            self.solver_trees.remove(&key);
        }
        self.surfaces.insert(key, solved);
        close(&mut at, &mut self.resolve_split.solve);
        Ok(())
    }

    /// Moves every `image` named in `drawn` onto the source that paint had a texture for, and
    /// starts a cross-dissolve where one is declared (ADR-0183). `layout::paint::execute` reports
    /// these after drawing the surface, which is the only place the answer is knowable: it holds
    /// the node's exact cache key, and it can tell a decode that landed from one that failed and
    /// from one that was already cached and never landed at all.
    ///
    /// The dissolve captures both of its endpoints here. A pass resolving a third source while it
    /// runs does not move them; that source waits, and crosses from wherever this run leaves the
    /// node.
    pub fn note_drawn_images(&mut self, instance_id: &str, drawn: &[DrawnImage], now: Instant) {
        fn walk(node: &mut ResolvedNode, drawn: &[DrawnImage], now: Instant) {
            if let Some(PaintStyle::Image { retain: true, transition, .. }) = &node.paint
                && let Some(shown) = drawn.iter().find(|image| image.node == node.id)
                && node.displayed_source.as_deref() != Some(shown.source.as_str())
            {
                let previous = node.displayed_source.replace(shown.source.clone());
                // A node that has just drawn its first picture has nothing to cross from: it
                // appears. A dissolve already running keeps its endpoints -- this draw is that
                // run's own incoming, not a new change.
                if let (Some(spec), Some(previous)) = (transition, previous)
                    && node.dissolve.is_none()
                {
                    node.dissolve = Some(Box::new(Dissolve::start(previous, shown.source.clone(), spec.clone(), now)));
                }
            }
            node.children.iter_mut().for_each(|child| walk(child, drawn, now));
        }
        if let Some(tree) = self.surfaces.get_mut(instance_id) {
            walk(tree, drawn, now);
        }
    }

    /// One surface instance's resolved tree, by its `"{id}@{output}"` instance id
    /// (`layout::instance::SurfaceInstance::instance_id`), not by the declared `id` a config
    /// writes. `crate::wayland::App::paint_surface` looks a tree up with exactly the id its
    /// `TrackedSurface` carries, which is what makes the two id spaces one (ADR-0038).
    pub fn surface(&self, instance_id: &str) -> Option<&ResolvedNode> {
        self.surfaces.get(instance_id)
    }

    /// Drops the retained tree for an instance that no longer exists.
    ///
    /// [`Self::apply_admitting`] visits only the instances it is given, and says so: retained
    /// instances absent from a cycle "stay for topology handling". This is that handling, called
    /// from `crate::wayland::App::destroy_surface_by_id`, and it is the only thing that removes a
    /// surface from this map. Without it an unplugged output stays resident for the life of the
    /// process, one tree per output name ever seen.
    pub fn forget(&mut self, instance_id: &str) {
        self.surfaces.remove(instance_id);
        self.solver_trees.remove(instance_id);
    }

    /// Node count per retained surface, largest first, for `crate::wayland::memory_profile`. The
    /// total from [`Self::census`] says the scene is growing; only this says which of eighteen
    /// trees is doing it, which is the difference between a finding and a number.
    pub fn census_by_surface(&self) -> Vec<(String, usize)> {
        let mut per_surface: Vec<(String, usize)> =
            self.surfaces.iter().map(|(key, tree)| (key.clone(), count_nodes(tree))).collect();
        // Biggest first: a growing tree is the one worth naming, and the report prints only the
        // head of this list.
        per_surface.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        per_surface
    }

    /// Surfaces, total nodes across every retained tree, and live `properties` values, for
    /// `crate::wayland::memory_profile`. Counts `properties` because that map is the one place a
    /// retained tree holds `mlua::Value`s, so it is where scene growth shows up in the Lua heap
    /// rather than the Rust one. Walks every tree, which is why the profile calls it once per
    /// report window and never per turn.
    pub fn census(&self) -> (usize, usize, usize) {
        let mut nodes = 0;
        let mut properties = 0;
        for tree in self.surfaces.values() {
            census_walk(tree, &mut nodes, &mut properties);
        }
        (self.surfaces.len(), nodes, properties)
    }
}

/// Admits all four root roles as containers with one `child` tree, including a lock tree
/// before the compositor has handed out a surface (ADR-0040 decision 1, ADR-0052 decision 2).
fn ensure_supported_kind(kind: &str) -> Result<(), LayoutError> {
    match kind {
        "panel" | "window" | "popup" | "lock" | "rect" | "row" | "column" | "text" | "icon" | "image" | "capture"
        | "shader" | "button" | "list" | "textfield" => Ok(()),
        other => Err(LayoutError::UnsupportedNodeKind(other.to_string())),
    }
}

/// Checks kind and depth before any `resolve_properties` call. Resolution runs Lua (ADR-0044), so
/// checking afterward once ran a self-generating `children` getter 64 times against the 64-level
/// cap, and ran every getter on unsupported kinds before refusing them. Children are checked at
/// `depth + 1`.
fn ensure_node_admissible(kind: &str, depth: u32) -> Result<(), LayoutError> {
    ensure_supported_kind(kind)?;
    // `>=` admits levels 0..63, exactly 64. `>` would admit 65 while claiming 64 and disagree with
    // `MAX_SIGNAL_NESTING_DEPTH`; the reported depth is the 1-based refused level.
    if depth >= MAX_TREE_DEPTH {
        return Err(LayoutError::TreeTooDeep { kind: kind.to_string(), depth: depth + 1, max: MAX_TREE_DEPTH });
    }
    Ok(())
}

/// One node after identity, resolution and parsing, and before geometry: everything a
/// [`ResolvedNode`] needs except the rect, plus the taffy node that rect will come out of.
///
/// The tree of these is what [`prepare`] builds walking the fresh `VirtualNode` tree in
/// declaration order, and what `finish` walks again to read the solved geometry back.
struct PreparedNode {
    id: NodeId,
    kind: &'static str,
    style: LayoutStyle,
    properties: PropMap,
    paint: Option<PaintStyle>,
    /// Carried across the pass untouched; see [`ResolvedNode::displayed_source`].
    displayed_source: Option<String>,
    /// Advanced to the pass's instant before it is carried; see [`ResolvedNode::dissolve`].
    dissolve: Option<Box<Dissolve>>,
    taffy: taffy::NodeId,
    children: Vec<PreparedNode>,
    /// The retained children of a node that is not `visible` this pass, carried through untouched
    /// (ADR-0124): not rebuilt, not laid out, not dropped. `children` is empty whenever this is
    /// not.
    frozen: Vec<ResolvedNode>,
    tweens: Vec<Tween>,
    /// Children on their way out (ADR-0150), already advanced this pass. Not in the solver.
    leaving: Vec<ResolvedNode>,
    /// Carried across the pass, or replaced by the build that ran; see [`ResolvedNode::list_memo`].
    list_memo: Option<node::ListMemo>,
    /// Carried across the pass, or replaced by the read that ran; see [`ResolvedNode::child_table`].
    child_table: Option<pass::ChildTable>,
    resolve_memo: Option<std::rc::Rc<ResolveMemo>>,
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};

    #[test]
    fn a_closed_timing_span_accumulates_and_reopens_for_the_next_region() {
        let mut at = Some(Instant::now());
        let mut total = Duration::ZERO;
        close(&mut at, &mut total);
        let after_first = total;
        assert!(at.is_some(), "the span reopens, so three regions cost three clock reads and not six");
        close(&mut at, &mut total);
        assert!(total >= after_first, "the second region adds to the first rather than replacing it");

        // Off is the production default, and the whole point of the switch: no clock, no total.
        let (mut off, mut total) = (None, Duration::ZERO);
        close(&mut off, &mut total);
        assert_eq!(total, Duration::ZERO);
        assert!(off.is_none(), "an unopened span stays closed");
    }

    /// Returns the `Lua` alongside the parsed node: an `mlua::Value` (every string/table
    /// property) is tied to the state that created it and panics on use once that state drops,
    /// so callers must keep the returned `Lua` alive for as long as the `VirtualNode` (and
    /// anything resolved from it) is used.
    pub(super) fn surface_from(lua_src: &str) -> (mlua::Lua, VirtualNode) {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(lua_src).set_name("@shell.lua").eval().unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        (lua, node)
    }

    pub(super) fn full() -> LogicalSize {
        LogicalSize { width: 1000.0, height: 500.0 }
    }

    /// The single-output shorthand every fixture below uses: one instance per declared surface,
    /// against one output named `"TEST"`, so a fixture declaring `id = "bar"` reads back as
    /// `scene.surface("bar@TEST")`. Deliberately not `layout::instance::expand_instances` -- that
    /// function takes `SurfaceSpec`s, whose `panel` arm requires a `layer`, and these fixtures
    /// test layout rather than topology; `expand_instances` has its own direct tests in
    /// `layout::instance`.
    pub(super) fn apply_at(
        scene: &mut Scene,
        surfaces: &[VirtualNode],
        available: LogicalSize,
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        let instances: Vec<SurfaceInstance> = surfaces.iter().map(|surface| instance_at(surface, available)).collect();
        scene.apply(surfaces, &instances, shaping, lua)
    }

    pub(super) fn instance_at(surface: &VirtualNode, available: LogicalSize) -> SurfaceInstance {
        let declared_id = node::fields::surface::id.read(&surface.properties).expect("every fixture declares an `id`");
        SurfaceInstance {
            instance_id: format!("{declared_id}@TEST"),
            declared_id,
            output: "TEST".to_string(),
            available,
            measured_axes: (false, false),
        }
    }

    #[test]
    fn a_signal_valued_width_resolves_to_its_current_value_in_the_resolved_node() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Integer(40), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("w", signal).unwrap();
        let table: mlua::Table =
            lua.load(r#"return panel { id = "bar", child = rect { width = w, height = 20 } }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 40.0, "a Signal-valued width must resolve at layout time");
    }

    /// ADR-0180, ADR-0183. A drawn source moves a retaining image onto it, and reaches only images
    /// that asked to retain: everything else keeps drawing what a pass resolved.
    #[test]
    fn a_landed_decode_moves_a_retaining_image_onto_the_source_it_named() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return panel { id = "bar", child = rect { children = {
                    image { id = "held", source = "/tmp/new.png", async = true, retain = true },
                    image { id = "plain", source = "/tmp/new.png", async = true },
                } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let image = |scene: &Scene, index: usize| {
            scene.surface("bar@TEST").unwrap().children[0].children[index].displayed_source.clone()
        };
        let held = |scene: &Scene| image(scene, 0);
        let plain = |scene: &Scene| image(scene, 1);
        let id = |scene: &Scene, index: usize| scene.surface("bar@TEST").unwrap().children[0].children[index].id;
        assert_eq!(held(&scene), None, "nothing has been drawn yet");

        // A report for another node moves nothing. Node identity, not path: two images may draw
        // one file, and only the one that drew it has caught up to it.
        let (held_id, plain_id) = (id(&scene, 0), id(&scene, 1));
        let drew = |node, source: &str| DrawnImage { node, source: source.to_string() };
        scene.note_drawn_images("bar@TEST", &[drew(plain_id, "/tmp/new.png")], Instant::now());
        assert_eq!(held(&scene), None);
        assert_eq!(plain(&scene), None, "an image that did not ask to retain holds nothing");

        scene.note_drawn_images("bar@TEST", &[drew(held_id, "/tmp/new.png")], Instant::now());
        assert_eq!(held(&scene), Some("/tmp/new.png".to_string()));

        // A report for another surface never reaches this tree.
        scene.note_drawn_images("other@TEST", &[drew(held_id, "/tmp/later.png")], Instant::now());
        assert_eq!(held(&scene), Some("/tmp/new.png".to_string()));
    }

    /// ADR-0181, ADR-0183. A dissolve starts when paint has the incoming texture and not on the
    /// pass that named it, it starts at zero, it needs somewhere to come from, it keeps both of its
    /// endpoints when a third source arrives, and while it runs the node owes the compositor frames
    /// without owing it a layout.
    #[test]
    fn a_dissolve_holds_its_two_endpoints_from_the_draw_that_starts_it_until_it_ends() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let apply = |scene: &mut Scene, path: &str| {
            let table: mlua::Table = lua
                .load(format!(
                    r#"return panel {{ id = "bar", child = image {{ id = "wp", source = "{path}",
                        async = true, transition = {{ duration = 400, easing = "InOutCubic" }} }} }}"#
                ))
                .eval()
                .unwrap();
            let surface = deserialize_lua_table(&table).unwrap();
            apply_at(scene, &[surface], full(), &shaping, &lua).unwrap();
        };
        let node = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].clone();
        let drew = |scene: &Scene, source: &str| {
            [DrawnImage { node: scene.surface("bar@TEST").unwrap().children[0].id, source: source.to_string() }]
        };

        apply(&mut scene, "/tmp/a.png");
        assert!(node(&scene).dissolve.is_none(), "naming a source starts nothing; drawing it does");
        assert!(!scene.surface("bar@TEST").unwrap().animating());

        // First draw: the node had no picture, so it appears rather than crosses.
        scene.note_drawn_images("bar@TEST", &drew(&scene, "/tmp/a.png"), Instant::now());
        assert!(node(&scene).dissolve.is_none(), "the first picture has nothing to cross from");

        apply(&mut scene, "/tmp/b.png");
        assert!(node(&scene).dissolve.is_none(), "still nothing until a paint has b's texture");
        let started = Instant::now();
        scene.note_drawn_images("bar@TEST", &drew(&scene, "/tmp/b.png"), started);
        let dissolve = node(&scene).dissolve.expect("b replaced a, so it crosses");
        assert_eq!(dissolve.from, "/tmp/a.png", "it crosses from the picture that was up");
        assert_eq!(dissolve.to, "/tmp/b.png", "and to the one the draw proved it has");
        assert_eq!(dissolve.progress, 0.0, "and it opens on the outgoing, not part way across");
        assert_eq!(
            node(&scene).displayed_source.as_deref(),
            Some("/tmp/b.png"),
            "while `displayed_source` has already moved on, which is why the dissolve carries `from`"
        );

        let tree = scene.surface("bar@TEST").unwrap();
        assert!(tree.animating(), "a dissolve owes the compositor its next frame");
        assert!(tree.tick_is_paint_only(), "and owes it no layout: all it moves is an alpha");

        // Every frame of the run redraws b and reports it again; that is this run's own incoming,
        // not a change, and it must not restart anything.
        scene.note_drawn_images("bar@TEST", &drew(&scene, "/tmp/b.png"), started);
        assert_eq!(node(&scene).dissolve.map(|d| d.from), Some("/tmp/a.png".to_string()), "unchanged, not restarted");

        // A third source arriving mid-run leaves both endpoints alone, so b stays on screen and
        // keeps its cache pin (ADR-0183).
        apply(&mut scene, "/tmp/c.png");
        let running = node(&scene).dissolve.expect("still crossing a to b");
        assert_eq!((running.from.as_str(), running.to.as_str()), ("/tmp/a.png", "/tmp/b.png"));

        // The tick has to reach it. This node has no `animate` block, so the walk's tween gate
        // skips it entirely, and a dissolve advanced behind that gate never moves -- which is what
        // the first live run of this showed: one frame at progress zero and then a stuck picture.
        let instances = [SurfaceInstance {
            instance_id: "bar@TEST".to_string(),
            declared_id: "bar".to_string(),
            output: "TEST".to_string(),
            available: full(),
            measured_axes: (false, false),
        }];
        let ticked = scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(100));
        assert_eq!(ticked, ["bar@TEST"], "the surface it advanced is what the frame is owed to");
        let progress = node(&scene).dissolve.expect("a quarter through, still crossing").progress;
        assert!(progress > 0.0 && progress < 1.0, "a quarter of the way across, got {progress}");

        scene.tick(&instances, &shaping, &lua, started + std::time::Duration::from_millis(400));
        assert!(node(&scene).dissolve.is_none(), "and it is dropped the moment its duration is up");
        assert!(!scene.surface("bar@TEST").unwrap().animating(), "so the node stops asking for frames");
    }

    #[test]
    fn a_state_signal_in_a_property_resolves_at_layout_time_and_a_set_between_applies_moves_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { width = state("w", 40), height = 20 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].rect.width, 40.0);

        lua.load(r#"state("w", 40):set(90)"#).exec().unwrap();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        assert_eq!(
            scene.surface("bar@TEST").unwrap().children[0].rect.width,
            90.0,
            "a re-resolve after :set() must lay out the written value, not the initial one"
        );
    }

    /// ADR-0259: `z` reorders paint alone; the tree, and the focus order read off it, keep
    /// declaration order, for literal and generated children alike.
    #[test]
    fn z_leaves_the_resolved_children_in_declaration_order() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = row { children = {
                   rect { id = "a", width = 10, height = 10, z = 2 },
                   rect { id = "b", width = 10, height = 10, z = -1 },
                   list { source = { "c", "d" }, itemfn = function(name)
                       return rect { id = name, width = 10, height = 10, z = name == "c" and 5 or 0 }
                   end } } } }"#,
        );
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let id = |c: &ResolvedNode| node::fields::common::id.read(&c.properties).unwrap().unwrap();
        assert_eq!(row.children[..2].iter().map(id).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(row.children[2].children.iter().map(id).collect::<Vec<_>>(), ["c", "d"]);
        assert_eq!((row.children[0].rect.x, row.children[1].rect.x), (0.0, 10.0), "layout ignores z");
        assert_eq!(row.children[1].z, -1.0);
    }

    /// `z` snaps; a tween would reorder mid-flight at an arbitrary frame.
    #[test]
    fn animating_z_is_refused() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = rect { width = 10, height = 10, z = 1, animate = { z = 100 } } }"#,
        );
        let err = apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap_err();
        assert!(err.to_string().contains("`z`"), "{err}");
    }

    #[test]
    fn an_unsupported_top_level_kind_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = { kind = "banana" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(err, LayoutError::UnsupportedNodeKind(k) if k == "banana"));
    }

    #[test]
    fn an_unsupported_kind_nested_inside_children_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(r#"panel { id = "bar", child = row { children = { { kind = "banana" } } } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(err, LayoutError::UnsupportedNodeKind(k) if k == "banana"));
    }

    #[test]
    fn an_unknown_elide_fails_the_pass_naming_the_property() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = text { content = "hi", elide = "Middle" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "elide"), "got {err:?}");
    }

    /// ADR-0068: a bad value fails the apply rather than being clamped or defaulted, so
    /// `opacity = 50` meaning percent is heard about immediately.
    #[test]
    fn an_opacity_outside_zero_to_one_fails_the_pass() {
        let shaping = ShapingHandle::spawn();
        for bad in ["50", "-0.5", "1.5", r#""half""#] {
            let mut scene = Scene::new();
            let (lua, surface) =
                surface_from(&format!(r#"panel {{ id = "bar", child = rect {{ opacity = {bad} }} }}"#));
            let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "opacity"),
                "`opacity = {bad}` must be refused by name, got {err:?}"
            );
        }
    }

    #[test]
    fn an_unknown_cursor_name_fails_the_pass() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = rect { cursor = "hand" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "cursor"), "got {err:?}");
    }

    #[test]
    fn an_absent_opacity_is_fully_opaque() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].opacity, 1.0);
    }

    /// ADR-0068's coverage: paint properties parse on an invisible node too, so a bad one fails at
    /// boot rather than once something makes the node visible.
    #[test]
    fn a_malformed_paint_property_on_an_invisible_node_still_fails_the_pass() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = rect { visible = false, background = 5 } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(matches!(err, LayoutError::InvalidProperty { property, .. } if property == "background"));
    }

    #[test]
    fn image_is_a_supported_leaf_with_no_intrinsic_size_and_icon_still_has_one() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { image { source = "/tmp/w.png", fit = "contain" }, icon { name = "audio-volume-high", size = 24 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let image = &row.children[0];
        assert_eq!(image.kind, "image");
        assert!(image.children.is_empty(), "image is a leaf, never a container");
        assert_eq!((image.rect.width, image.rect.height), (0.0, 0.0));

        let icon = &row.children[1];
        assert_eq!((icon.rect.width, icon.rect.height), (24.0, 24.0));
    }

    #[test]
    fn capture_is_a_supported_leaf_with_no_intrinsic_size() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", child = capture { output = "DP-1", live = true } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let capture = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(capture.kind, "capture");
        assert!(capture.children.is_empty(), "capture is a leaf, never a container");
        assert_eq!((capture.rect.width, capture.rect.height), (0.0, 0.0));
    }

    #[test]
    fn an_image_given_a_box_takes_that_box() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", width = "Fill", height = "Fill", child = image { source = "/tmp/w.png", width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let image = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(
            image.rect.width > 0.0 && image.rect.height > 0.0,
            "a Fill image should take the panel, got {:?}",
            image.rect
        );
    }

    #[test]
    fn textfield_is_a_supported_leaf_kind_carrying_its_properties_unvalidated() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { textfield { mask_character = "*", secure_submit = { capability = "polkit", action = "authenticate" } } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let field = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert_eq!(field.kind, "textfield");
        assert!(field.children.is_empty(), "textfield is a leaf, never a container");
        assert_eq!(field.properties.get("mask_character").unwrap().as_string().unwrap().to_string_lossy(), "*");
    }

    #[test]
    fn one_declared_surface_resolves_one_tree_per_instance_each_against_its_own_size() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", width = "Fill", height = "Fill" }"#);
        let instances = vec![
            SurfaceInstance {
                instance_id: "bar@eDP-1".to_string(),
                declared_id: "bar".to_string(),
                output: "eDP-1".to_string(),
                available: LogicalSize { width: 1920.0, height: 1080.0 },
                measured_axes: (false, false),
            },
            SurfaceInstance {
                instance_id: "bar@DP-1".to_string(),
                declared_id: "bar".to_string(),
                output: "DP-1".to_string(),
                available: LogicalSize { width: 3840.0, height: 2160.0 },
                measured_axes: (false, false),
            },
        ];

        scene.apply(&[surface], &instances, &shaping, &lua).unwrap();

        assert_eq!(scene.surface("bar@eDP-1").unwrap().rect.width, 1920.0);
        assert_eq!(scene.surface("bar@DP-1").unwrap().rect.width, 3840.0);
        assert!(scene.surface("bar").is_none(), "the declared id alone is not a key any more");
    }

    #[test]
    fn a_declared_surface_with_no_instance_resolves_not_at_all() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", width = 10, height = 10 }"#);

        scene.apply(&[surface], &[], &shaping, &lua).unwrap();

        assert!(
            scene.surface("bar@TEST").is_none(),
            "no instance means no output to resolve against, which is not an error"
        );
    }

    #[test]
    fn an_instance_naming_a_surface_the_evaluation_did_not_declare_is_a_caller_bug() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"panel { id = "bar", width = 10, height = 10 }"#);
        let instances = vec![SurfaceInstance {
            instance_id: "ghost@TEST".to_string(),
            declared_id: "ghost".to_string(),
            output: "TEST".to_string(),
            available: full(),
            measured_axes: (false, false),
        }];

        let err = scene.apply(&[surface], &instances, &shaping, &lua).unwrap_err();

        assert!(err.to_string().contains("ghost@TEST"), "the message must name the offending instance: {err}");
        assert!(scene.surface("ghost@TEST").is_none());
    }

    #[test]
    fn a_veto_from_admit_takes_the_same_rollback_road_a_failed_walk_takes() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, v1) = surface_from(r#"panel { id = "bar", width = 10, height = 10 }"#);
        apply_at(&mut scene, &[v1], full(), &shaping, &_lua1).unwrap();
        let next_id_before = scene.next_id;

        let (_lua2, v2) = surface_from(r#"panel { id = "bar", width = 99, height = 99 }"#);
        let instances = [instance_at(&v2, full())];
        let err = scene
            .apply_admitting(&[v2], &instances, &shaping, &_lua2, |_| {
                Err(node::invalid("child", "the finished scene is not admissible"))
            })
            .unwrap_err();

        assert!(err.to_string().contains("not admissible"), "the veto's own message must reach the caller: {err}");
        assert_eq!(
            scene.surface("bar@TEST").unwrap().rect.width,
            10.0,
            "a vetoed apply must leave the prior tree on screen"
        );
        assert_eq!(scene.next_id, next_id_before, "and must not leak the ids the vetoed walk allocated");
    }

    #[test]
    fn a_failed_apply_leaves_the_scene_exactly_as_it_was() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10 },
                rect { width = 20, height = 20 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();

        let ids_before: Vec<NodeId> = {
            let root = scene.surfaces.get("bar@TEST").unwrap();
            let row = &root.children[0];
            vec![root.id, row.id, row.children[0].id, row.children[1].id]
        };
        let next_id_before = scene.next_id;

        let lua2 = mlua::Lua::new();
        register_node_constructors(&lua2).unwrap();
        crate::lua::signal::register(&lua2, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua2
            .load(
                r#"
                local bad_width = computed({}, function() error("boom") end)
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10 },
                    rect { width = bad_width, height = 20 },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface_v2 = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface_v2], full(), &shaping, &lua2).unwrap_err();
        assert!(matches!(err, LayoutError::InvalidProperty { property, .. } if property == "width"));

        let root = scene.surfaces.get("bar@TEST").unwrap();
        let row = &root.children[0];
        let ids_after = vec![root.id, row.id, row.children[0].id, row.children[1].id];
        assert_eq!(ids_after, ids_before, "NodeIds must be stable across a failed apply, not reallocated");
        assert_eq!(scene.next_id, next_id_before, "next_id must not be left bumped by the aborted pass");
        assert_eq!(row.children[1].rect.width, 20.0, "the first tree's geometry must still be intact");
    }

    #[test]
    fn a_self_referential_literal_tree_is_rejected_with_a_layout_error() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local r = rect {}
                r.children = { r }
                return panel { id = "bar", child = r }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(err, LayoutError::TreeTooDeep { .. }),
            "a cyclic literal tree must return a LayoutError, not abort the process: {err:?}"
        );
    }

    #[test]
    fn a_computed_children_signal_generating_fresh_depth_is_rejected_with_a_layout_error() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local deep
                deep = computed({}, function()
                    return { rect { width = 1, height = 1, children = deep } }
                end)
                return panel { id = "bar", child = rect { children = deep } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        let capped = matches!(err, LayoutError::TreeTooDeep { .. })
            || matches!(&err, LayoutError::InvalidProperty { detail, .. } if detail.contains("5ms CPU budget"));
        assert!(capped, "a computed children signal generating fresh depth must be capped, not abort: {err:?}");
    }

    /// A `panel` wrapping `rows` nested `row`s around one `rect`, so the deepest level is
    /// `rows + 2`. Built with a Lua loop rather than nested table literals: at these depths the
    /// literal form runs into Lua's own `LUAI_MAXCCALLS` parser nesting limit, which would be
    /// testing the parser rather than this cap.
    fn surface_nested(lua: &mlua::Lua, rows: usize) -> VirtualNode {
        let table: mlua::Table = lua
            .load(format!(
                r#"
                local n = rect {{ width = 1, height = 1 }}
                for _ = 1, {rows} do n = row {{ children = {{ n }} }} end
                return panel {{ id = "bar", child = n }}
                "#
            ))
            .eval()
            .unwrap();
        deserialize_lua_table(&table).unwrap()
    }

    /// What one `list` pass costs at the sizes a wallpaper folder reaches:
    /// `cargo test -p renderer --release list_pass_cost -- --ignored --nocapture`. Ignored for the
    /// same reasons as [`read_seam_cost`]: it reports numbers, and only a release build's mean
    /// anything. `MANTLE_PROFILE=1` adds the per-pass split.
    ///
    /// Six nodes per row, one of them text, beside a `clock` text, over a cached re-apply -- the
    /// per-capability-push shape of ADR-0044 decision 2, not a cold start. `clock` writes only the
    /// clock, so the list keeps its items (ADR-0269); `source` writes the list's source, so it
    /// builds them all. On this machine:
    ///
    /// | rows | `clock` p50 | `source` p50 | note |
    /// |---|---|---|---|
    /// | 12 | 0.17 ms | 0.29 ms | one viewport of a virtualized list |
    /// | 50 | 0.51 ms | 1.15 ms | ADR-0132's fifty tiles |
    /// | 125 | 1.23 ms | 2.79 ms | 500 wallpapers, four to a row |
    /// | 500 | 4.8 ms | 11.2 ms | 2000 wallpapers |
    /// | 125, no text | 0.93 ms | 2.31 ms | |
    ///
    /// Linear in source length either way: about 10us a row kept, 22us a row built. A kept list
    /// still lays every item out; ADR-0191's viewport is the design that would cut it to the first
    /// row of this table, and why it is not built yet.
    #[test]
    #[ignore]
    fn list_pass_cost() {
        let src = |rows: usize, text: bool| {
            let label =
                if text { r###"text { content = e.label, font_size = 14, foreground = "#ffffff" },"### } else { "" };
            format!(
                r##"clock = state("clock", "0")
                local entries = {{}}
                for i = 1, {rows} do entries[i] = {{ id = "e" .. i, label = "wallpaper " .. i }} end
                source = state("source", entries)
                return panel {{
                    id = "picker",
                    child = column {{ width = "Fill", height = "Fill", children = {{
                        text {{ content = clock, font_size = 14 }},
                        list {{
                            width = "Fill", height = "Fill", spacing = 6,
                            source = source,
                            key = function(e) return e.id end,
                            itemfn = function(e)
                                return row {{ width = "Fill", height = 96, spacing = 6, children = {{
                                    rect {{ width = 96, height = 96, background = "#202020", radius = 8 }},
                                    rect {{ width = 96, height = 96, background = "#202020", radius = 8 }},
                                    rect {{ width = 96, height = 96, background = "#202020", radius = 8 }},
                                    rect {{ width = 96, height = 96, background = "#202020", radius = 8 }},
                                    {label}
                                }} }}
                            end,
                        }},
                    }} }},
                }}"##
            )
        };
        let shaping = ShapingHandle::spawn();
        for (rows, text) in [(12usize, true), (50, true), (125, true), (500, true), (125, false)] {
            for (written, write) in [("clock", "clock:set(tostring(tick))"), ("source", "source:set(source:get())")] {
                let (lua, surface) = surface_from(&src(rows, text));
                let mut scene = Scene::new();
                apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
                scene.take_resolve_split();
                let mut samples = Vec::new();
                for tick in 0..40 {
                    lua.globals().set("tick", tick).unwrap();
                    lua.load(write).exec().unwrap();
                    let started = std::time::Instant::now();
                    apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let split = scene.take_resolve_split();
                let per_pass = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / 40.0;
                println!(
                    "{rows:4} rows text={text} write={written}: p50 {:.3} ms  p95 {:.3} ms  (list {:.3} resolve {:.3} solve {:.3})",
                    samples[20],
                    samples[38],
                    per_pass(split.list),
                    per_pass(split.resolve),
                    per_pass(split.solve),
                );
            }
        }
    }

    /// What the non-list part of `prepare` costs per pass on a bar:
    /// `MANTLE_PROFILE=1 cargo test -p renderer --release bar_pass_cost -- --ignored --nocapture`.
    /// Ignored like [`list_pass_cost`]. 50 hover chips of `rect > row > (rect, text)` and a clock
    /// text written each pass, 203 nodes. On this machine, `props` (the resolve span less the list
    /// span) was 0.545 ms a pass reading every `children` table each pass, 0.325 ms keeping what
    /// each table read (`pass::ChildTable`), 0.240 ms also keeping every node the clock write did
    /// not reach (ADR-0270).
    #[test]
    #[ignore]
    fn bar_pass_cost() {
        let (lua, surface) = surface_from(
            r##"clock = state("clock", "0")
            local chips = { text { content = clock, font_size = 13 } }
            for i = 1, 50 do
                local over = hover("chip" .. i)
                chips[#chips + 1] = rect {
                    hover = over, radius = 6, padding = 4,
                    background = over:map(function(on) return on and "#313244" or "#1E1E2E" end),
                    children = { row { spacing = 4, children = {
                        rect { width = 8, height = 8, background = "#89B4FA" },
                        text { content = "chip " .. i, font_size = 12, foreground = "#CDD6F4" },
                    } } },
                }
            end
            return panel { id = "bar", child = row { width = "Fill", height = 32, spacing = 4, children = chips } }"##,
        );
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let (_, nodes, _) = scene.census();
        scene.take_resolve_split();
        let passes = 200;
        for tick in 0..passes {
            lua.load(format!("clock:set('{tick}')")).exec().unwrap();
            apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        }
        let split = scene.take_resolve_split();
        let per_pass = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / passes as f64;
        println!(
            "BAR nodes={nodes}: props {:.3} ms/pass (resolve {:.3} list {:.3} solve {:.3} clone {:.3})",
            per_pass(split.resolve - split.list),
            per_pass(split.resolve),
            per_pass(split.list),
            per_pass(split.solve),
            per_pass(split.clone),
        );
    }

    /// What one production read of a retained tree costs:
    /// `cargo test -p renderer --release read_seam -- --ignored --nocapture`. Ignored because it
    /// reports a number rather than asserting one, and only a release build's number means
    /// anything.
    ///
    /// `Scene::surface` lends its tree rather than rebuilding it: on this 162-node fixture a
    /// rebuild costs 43.4us per read against 134ns lent. The guard if a clone ever comes back.
    /// `hit_path` stands in for the pointer path, which is what pays this per motion event.
    #[test]
    #[ignore]
    fn read_seam_cost() {
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua
            .load(
                r##"
                local kids = {}
                for i = 1, 40 do
                  kids[i] = row {
                    spacing = 2,
                    background = "#204080FF",
                    children = {
                      rect { width = 8, height = 8, background = "#FFFFFFFF" },
                      text { content = "item " .. i, font_size = 12 },
                      rect { width = 8, height = 8, background = "#00FF00FF" },
                    },
                  }
                end
                return panel { id = "bar", child = row { spacing = 4, children = kids } }
                "##,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let mut scene = Scene::new();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let (_, nodes, properties) = scene.census();
        let point = crate::layout::hit::LogicalPoint { x: 120.0, y: 5.0 };
        let iterations = 20_000;
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let mut sink = 0usize;
            for _ in 0..iterations {
                let tree = scene.surface("bar@TEST").unwrap();
                sink += crate::layout::hit::hit_path(tree, point).len();
            }
            std::hint::black_box(sink);
            best = best.min(start.elapsed());
        }
        println!(
            "READ_SEAM nodes={nodes} properties={properties} iterations={iterations} best={:?} per_read={:?}",
            best,
            best / iterations
        );
    }

    #[test]
    fn a_tree_at_the_depth_cap_is_accepted_and_one_level_past_it_is_rejected() {
        let deepest = MAX_TREE_DEPTH as usize;
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();

        let mut scene = Scene::new();
        apply_at(&mut scene, &[surface_nested(&lua, deepest - 2)], full(), &shaping, &lua).unwrap();

        let mut scene = Scene::new();
        let err = apply_at(&mut scene, &[surface_nested(&lua, deepest - 1)], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(err, LayoutError::TreeTooDeep { depth, max, .. }
                if depth == MAX_TREE_DEPTH + 1 && max == MAX_TREE_DEPTH),
            "one level past the cap must be refused, reporting the limit actually enforced: {err:?}"
        );
    }

    #[test]
    fn a_legitimately_deep_but_reasonable_tree_still_applies() {
        const NESTING: usize = 20;
        let mut lua_src = String::from(r#"panel { id = "bar", child = "#);
        for _ in 0..NESTING {
            lua_src.push_str(r#"row { children = { "#);
        }
        lua_src.push_str(r#"rect { width = 4, height = 4 }"#);
        for _ in 0..NESTING {
            lua_src.push_str(" } }");
        }
        lua_src.push('}');

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(&lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let mut node = scene.surface("bar@TEST").unwrap();
        for _ in 0..=NESTING {
            assert_eq!(node.children.len(), 1);
            node = &node.children[0];
        }
        assert_eq!(node.kind, "rect");
        assert_eq!(node.rect.width, 4.0, "the innermost rect's own geometry must have resolved");
    }

    /// The leak this closes: `apply` visits only the instances it is handed, so an instance that
    /// stops existing is never revisited and its tree is never dropped. Nothing but `forget`
    /// removes one, or an unplugged output stays resident for the life of the process.
    #[test]
    fn a_departed_instance_is_forgotten_rather_than_left_resident() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();

        let declared: mlua::Table =
            lua.load(r#"return panel { id = "bar", child = rect { width = 40, height = 10 } }"#).eval().unwrap();
        apply_at(&mut scene, &[deserialize_lua_table(&declared).unwrap()], full(), &shaping, &lua).unwrap();
        assert!(scene.surface("bar@TEST").is_some(), "the tree must exist before it can be forgotten");

        // An apply carrying no instances is what an unplugged output produces, and it must NOT be
        // what drops the tree: `apply` cannot tell an instance that departed from one simply absent
        // this cycle, which is why it leaves the question to topology handling.
        scene.apply(&[deserialize_lua_table(&declared).unwrap()], &[], &shaping, &lua).unwrap();
        assert!(scene.surface("bar@TEST").is_some(), "apply must leave it for topology handling");

        scene.forget("bar@TEST");
        assert!(scene.surface("bar@TEST").is_none(), "topology handling is what drops it");
        scene.forget("bar@TEST");
        assert!(scene.surface("bar@TEST").is_none(), "and forgetting one twice is not an error");
    }
}

#[cfg(test)]
mod pass_budget_tests {
    use super::tests::{apply_at, full};
    use super::*;
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};
    use crate::text::shaping::ShapingHandle;

    /// The config here contains no `Signal` at all:
    /// a plain table with an `__index` that never returns is Lua the pass runs outside any signal
    /// evaluation, so ADR-0021's per-getter cap never covered it. Item 5 measured this exact shape
    /// at 26.10 seconds returning `Ok(())`.
    ///
    /// Slow on purpose, and the only test here that is: what it pins is a wall-clock bound, so it
    /// has to spend it. Roughly `LAYOUT_PASS_CAP`.
    #[test]
    fn a_runaway_index_metamethod_fails_the_pass_instead_of_hanging_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local m = setmetatable({}, { __index = function() while true do end end })
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10, margin = m },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let started = std::time::Instant::now();
        let outcome = apply_at(&mut scene, &[surface], full(), &shaping, &lua);
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome, Err(LayoutError::PassBudgetExceeded)),
            "an unbounded metamethod must blame the budget, not whichever property it was reading: {outcome:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(20), "must be bounded, took {elapsed:?}");
    }

    /// The failure rolls back like every other one (`CONTEXT.md`, Rollback). A pass refused
    /// halfway must not leave the scene holding a partly-resolved tree, or the next repaint draws
    /// it.
    #[test]
    fn a_pass_refused_by_the_budget_leaves_the_scene_as_it_was() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();

        let good: mlua::Table =
            lua.load(r#"return panel { id = "bar", child = rect { width = 40, height = 10 } }"#).eval().unwrap();
        apply_at(&mut scene, &[deserialize_lua_table(&good).unwrap()], full(), &shaping, &lua).unwrap();
        let before = scene.surface("bar@TEST").unwrap().children[0].rect.width;

        let runaway: mlua::Table = lua
            .load(
                r#"
                local m = setmetatable({}, { __index = function() while true do end end })
                return panel { id = "bar", child = rect { width = 99, height = 10, margin = m } }
                "#,
            )
            .eval()
            .unwrap();
        let outcome = apply_at(&mut scene, &[deserialize_lua_table(&runaway).unwrap()], full(), &shaping, &lua);

        assert!(matches!(outcome, Err(LayoutError::PassBudgetExceeded)), "{outcome:?}");
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].rect.width, before, "the good tree survives");
    }
}

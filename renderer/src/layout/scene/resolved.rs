//! The resolved tree's node and the layout style parsed beside it.

use super::*;
use crate::layout::node::{self as node};

#[cfg(test)]
impl ResolvedNode {
    /// A visible, unpainted node built by hand in a test, id 0; the test sets any other field.
    pub(crate) fn test(
        kind: &'static str,
        (x, y, width, height): (f32, f32, f32, f32),
        children: Vec<ResolvedNode>,
    ) -> Self {
        ResolvedNode {
            allocated_axes: (false, false),
            id: NodeId::test(0),
            kind,
            rect: LogicalRect { x, y, width, height },
            margin: EdgeInsets::default(),
            scrolled: 0.0,
            layout_style: std::rc::Rc::new(LayoutStyle::parse(&PropMap::default()).unwrap()),
            taffy: None,
            visible: true,
            opacity: 1.0,
            z: 0.0,
            transform: node::Transform::default(),
            compositor_blur: false,
            properties: std::rc::Rc::default(),
            paint: None,
            mask_target: None,
            displayed_source: None,
            dissolve: None,
            children,
            tweens: Vec::new(),
            move_spec: None,
            movement: None,
            leaving: false,
            list_memo: None,
            child_table: None,
            resolve_memo: None,
            site: None,
        }
    }

    /// This node with `clip` set, painting as a box if it did not already.
    pub(crate) fn with_clip(mut self, shape: node::ClipShape) -> Self {
        match &mut self.paint {
            Some(PaintStyle::Box { clip, .. }) => *clip = shape,
            paint => {
                *paint = Some(PaintStyle::Box {
                    background: Vec::new(),
                    radius: Default::default(),
                    border: Default::default(),
                    widths: Default::default(),
                    clip: shape,
                    mask: None,
                    ring: None,
                })
            }
        }
        self
    }

    /// This node with `padding` on every edge.
    pub(crate) fn with_padding(mut self, padding: f64) -> Self {
        let props = PropMap::from_iter([("padding", Value::Number(padding))]);
        self.layout_style = std::rc::Rc::new(LayoutStyle::parse(&props).unwrap());
        self
    }
}

/// Geometry parsed once per node/pass. A resolved table's `__index` still runs on each access, so
/// this is separate from reading a `Signal`: one read per field is what keeps a child's margin one
/// answer for the measure and the placement. The parent parses a
/// child before recursing because it needs the margin and size for the solver (ADR-0077); ignored
/// fields are still validated so a later kind change cannot hide a malformed property (ADR-0068).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LayoutStyle {
    pub(super) margin: EdgeInsets,
    pub(super) padding: EdgeInsets,
    pub(super) width_mode: SizeMode,
    pub(super) height_mode: SizeMode,
    /// Ceilings for `Content` growth; overflow goes to `scroll`.
    pub(super) max_width: Option<f32>,
    pub(super) max_height: Option<f32>,
    /// Floors for `Content` growth; a declared one replaces taffy's disabled automatic minimum.
    pub(super) min_width: Option<f32>,
    pub(super) min_height: Option<f32>,
    pub(super) align_h: Align,
    pub(super) align_v: Align,
    pub(super) spacing: f32,
    pub(super) homogeneous: bool,
    pub(super) line_spacing: f32,
    pub(super) visible: bool,
    pub(super) opacity: f32,
    pub(super) z: f32,
    pub(super) transform: node::Transform,
    pub(super) compositor_blur: bool,
    /// This node's own `shadows` and `effect.blur` (ADR-0254), over its whole painted subtree.
    pub(crate) effect: node::Effect,
}

impl LayoutStyle {
    /// The one parse of one node's geometry for one pass. `properties` must already be a
    /// [`node::resolve_declared`] result: this reads values, it does not resolve signals.
    pub(super) fn parse(properties: &PropMap) -> Result<Self, LayoutError> {
        // Validated and not kept: the pointer path reads the name back off `properties` when it
        // needs it (`layout::hit::cursor_under`), and a pass is the place a misspelling fails.
        use node::fields::{common, flow, flow_layout, paint};
        common::cursor.read(properties)?;
        common::accessible_name.read(properties)?;
        common::focus_ring.read(properties)?;
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
            homogeneous: flow_layout::homogeneous.read(properties)?,
            line_spacing: flow_layout::line_spacing.read(properties)?,
            visible: common::visible.read(properties)?,
            opacity: common::opacity.read(properties)?,
            // -0.0 would sort below its z = 0 siblings.
            z: common::z.read(properties)? + 0.0,
            transform: crate::layout::node::parse_transform(properties)?,
            compositor_blur: paint::compositor_blur.read(properties)?,
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
/// `resolve_declared` ran over this node's raw map exactly once, so this snapshot lets later
/// readers take values without resolving anything. `properties` holds resolved values, never a
/// `Signal` handle. Structural keys are copied raw by `node::is_structural_property`, whose parsers
/// reject signals.
///
/// ponytail: absent and nil are one state here (`node::resolve_declared` omits a key whose
/// signal resolved to `Value::Nil`, ADR-0044 decision 1's amendment), so a paint-only property
/// bound to a still-unresolved capability signal reads as its parser's default. Upgrade path: a
/// third state, `Value::Nil` retained as "bound but unresolved".
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    /// Surface axes allocated by its role and anchors, kept across animation ticks.
    pub(crate) allocated_axes: (bool, bool),
    /// Cached for layout ticks; a pass replaces it after resolving properties.
    pub(crate) layout_style: std::rc::Rc<LayoutStyle>,
    /// The solver node. `Some` only for a node of the instance's cached tree.
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
    /// The scroll offset its children carry, so a wheel moves them on from it without a pass.
    pub(crate) scrolled: f32,
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
    /// This node asked for the desktop behind it to be blurred (`compositor_blur`, ADR-0195).
    /// `layout::blur_regions` turns every one of these in a surface into the one region the
    /// compositor is given; nothing else reads it, and a compositor without the protocol ignores the
    /// lot.
    pub compositor_blur: bool,
    /// Shared with the rollback copy until a tween writes it.
    pub properties: std::rc::Rc<PropMap>,
    /// This node's paint properties, parsed here rather than by `layout::paint` on every frame
    /// (`node::paint_style`'s module doc comment says why). `None` for a kind that draws nothing.
    pub paint: Option<PaintStyle>,
    /// Resolved while Lua is live; paint and input never inspect a Lua id string.
    pub(crate) mask_target: Option<NodeId>,
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
    /// Parsed `animate.move`, kept when a resolve memo skips the node.
    pub(crate) move_spec: Option<Box<MoveSpec>>,
    /// Paint displacement from the last matched position to this pass's laid-out rect.
    pub(crate) movement: Option<Box<MoveTween>>,
    /// The tree no longer holds this node; it stays, at the rect it last had, while its
    /// `animate.exit` tweens run (ADR-0150). Out of flow (siblings have already closed over its
    /// slot) and out of reach (no hit, no input region, no geometry), painted after its live
    /// siblings until the last tween ends, when the next pass drops it.
    pub leaving: bool,
    /// What a `list` built its items from, so a pass that finds it unchanged keeps them.
    pub list_memo: Option<Box<node::ListMemo>>,
    /// What its `children` or `child` table last read as, so a pass holding the same table skips
    /// reading it again.
    pub child_table: Option<children::ChildTable>,
    /// What `properties` were resolved from, so a pass that finds it unchanged keeps them. Shared,
    /// so the rollback copy of the tree does not copy the raw values it holds.
    pub resolve_memo: Option<std::rc::Rc<ResolveMemo>>,
    /// The line that built it, so a pass can tell a re-render from a different node taking its place.
    pub(crate) site: Option<Site>,
}

impl ResolvedNode {
    /// This field's `initial_text` now, `""` when unset, not a string or not settable. A signal is
    /// read now, not as of the last resolve, which a write alone never repeats (the seed read does
    /// not subscribe).
    pub(crate) fn current_initial_text(&self, lua: &Lua) -> String {
        let text = match self.resolve_memo.as_ref().and_then(|memo| node::signal_at(memo.raw(), "initial_text")) {
            Some(signal) => match crate::lua::signal::untracked(lua, || signal.get_value(lua)) {
                Ok(Value::String(text)) => text.to_str().map(|text| text.to_string()).unwrap_or_default(),
                _ => String::new(),
            },
            None => node::fields::textfield::initial_text.read(&self.properties).unwrap_or_default(),
        };
        let multiline = matches!(self.paint, Some(node::PaintStyle::TextField { multiline: Some(_), .. }));
        if crate::lua::focus::is_settable(&text, multiline) {
            return text;
        }
        shared::warn!("`initial_text` takes at most 64 KiB without control characters; autofocus starts empty");
        String::new()
    }

    /// A disabled `textfield` takes no keyboard focus of either kind.
    pub(crate) fn is_disabled_field(&self) -> bool {
        matches!(self.paint, Some(node::PaintStyle::TextField { disabled: true, .. }))
    }

    pub(crate) fn paint_matrix(&self, rect: LogicalRect) -> Option<node::Affine> {
        if self.transform.is_identity() && self.movement.is_none() {
            return None;
        }
        let mut matrix = self.transform.matrix(rect);
        if let Some(movement) = self.movement.as_ref() {
            matrix[4] += movement.offset.0;
            matrix[5] += movement.offset.1;
        }
        Some(matrix)
    }

    pub(super) fn freeze_move_for_exit(&mut self) {
        if let Some(movement) = self.movement.take() {
            self.rect.x += movement.offset.0;
            self.rect.y += movement.offset.1;
        }
        self.children.iter_mut().for_each(Self::freeze_move_for_exit);
    }

    /// Visible and laid out this pass: what a flow measures and a reader may address. A leaving
    /// node is visible and neither.
    pub(in crate::layout) fn in_flow(&self) -> bool {
        self.visible && !self.leaving
    }

    /// The node's box in surface coordinates, given the absolute origin of its parent.
    pub(crate) fn at(&self, origin_x: f32, origin_y: f32) -> LogicalRect {
        LogicalRect { x: origin_x + self.rect.x, y: origin_y + self.rect.y, ..self.rect }
    }

    /// `border_box` less this node's `padding`: where a `text` or `textfield` lays out its glyphs.
    pub(crate) fn content_box(&self, border_box: LogicalRect) -> LogicalRect {
        let pad = self.layout_style.padding;
        LogicalRect {
            x: border_box.x + pad.left,
            y: border_box.y + pad.top,
            width: (border_box.width - pad.horizontal()).max(0.0),
            height: (border_box.height - pad.vertical()).max(0.0),
        }
    }

    /// Whether children are cut to this box; paint, hit testing and regions all ask here (ADR-0328).
    /// A mask composites through the box, so it cuts whatever `clip` says.
    pub(in crate::layout) fn clips_children(&self) -> bool {
        match &self.paint {
            Some(PaintStyle::Box { clip, mask, .. }) => *clip != node::ClipShape::None || mask.is_some(),
            None => false,
            Some(_) => true,
        }
    }

    /// The mask stays owned, reconciled and laid out among these children, but only paints as alpha.
    pub(crate) fn mask_child(&self) -> Option<&ResolvedNode> {
        self.children.iter().find(|child| Some(child.id) == self.mask_target)
    }

    /// Input and compositor regions exclude the mask, while lifetime walks retain all children.
    pub(crate) fn content_children(&self) -> impl DoubleEndedIterator<Item = &ResolvedNode> {
        let mask = self.mask_target;
        self.children.iter().filter(move |child| Some(child.id) != mask)
    }

    /// Children bottom to top: ascending `z`, declaration order among equals (ADR-0259).
    /// Allocates only when `z` reorders something.
    pub(in crate::layout) fn painted_children(&self) -> impl DoubleEndedIterator<Item = &ResolvedNode> {
        let sorted = self.children.is_sorted_by(|a, b| a.z <= b.z);
        let mut resorted: Vec<&ResolvedNode> = if sorted { Vec::new() } else { self.content_children().collect() };
        resorted.sort_by(|a, b| a.z.total_cmp(&b.z));
        self.content_children().filter(move |_| sorted).chain(resorted)
    }

    /// Whether the pointer can land on this node: its own `hittable`, else `inherited` from the parent.
    pub fn hittable(&self, inherited: bool) -> bool {
        match self.properties.get("hittable") {
            Some(Value::Boolean(own)) => *own,
            _ => inherited,
        }
    }

    /// A node with `submits_secure = true` or a pointer handler (ADR-0214).
    pub fn takes_pointer(&self) -> bool {
        matches!(self.properties.get("submits_secure"), Some(Value::Boolean(true)))
            || ["on_click", "on_double_click", "on_press", "on_drag", "on_wheel"]
                .iter()
                .any(|handler| matches!(self.properties.get(*handler), Some(Value::Function(_))))
    }

    /// Whether this node's own `animate` tweens or `move` are running, not its children's.
    pub(in crate::layout) fn tweening(&self) -> bool {
        self.tweens.iter().any(|tween| !tween.resting) || self.movement.is_some()
    }

    /// Whether any visible node in this tree is mid-tween, which is what asks the compositor for
    /// another frame callback (`wayland::surface::App::paint_surface`). A hidden node's subtree is
    /// frozen (ADR-0124), tweens included: nothing advances them, so counting them would arm a
    /// callback chain that never ends. They wait there and the thaw's resolve settles them.
    pub fn animating(&self) -> bool {
        self.visible
            && (self.tweening() || self.dissolve.is_some() || self.children.iter().any(ResolvedNode::animating))
    }

    /// [`animating`](Self::animating) less the tweens that hold one value from `now`: only those
    /// ask for frame callbacks. `hold` takes the earliest instant a holding tween moves again, which
    /// is meaningful only when this answers `false`.
    pub fn needs_frames(&self, now: std::time::Instant, hold: &mut Option<std::time::Instant>) -> bool {
        self.visible
            && (self.movement.is_some()
                || self.dissolve.is_some()
                || self.tweens.iter().filter(|tween| !tween.resting).any(|tween| match tween.holds_until(now) {
                    Some(at) => {
                        *hold = Some(hold.map_or(at, |held| held.min(at)));
                        false
                    }
                    None => true,
                })
                || self.children.iter().any(|child| child.needs_frames(now, hold)))
    }

    /// Whether every tween still running in this tree moves a paint-only property, which is what
    /// lets [`Scene::tick`] advance it in place instead of laying it out again
    /// (`node::is_paint_only`).
    ///
    /// A hidden subtree answers `true` because nothing advances it either way: it is frozen
    /// (ADR-0124) and `animating` does not count it. A leaving node answers `false` whatever its
    /// exit moves, because the end of that exit drops the node, and changing the shape of the tree
    /// is `prepare_retained`'s to do (ADR-0150).
    pub(super) fn tick_is_paint_only(&self) -> bool {
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
    pub(super) fn margin_on(&self, axis: MainAxis) -> f32 {
        match axis {
            MainAxis::Horizontal => self.margin.horizontal(),
            MainAxis::Vertical => self.margin.vertical(),
        }
    }
}

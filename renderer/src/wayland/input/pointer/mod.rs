//! Pointer input: `on_click` (ADR-0050), `on_drag` and `on_wheel` (ADR-0116), links, hover and the
//! cursor.

use shared::warn;

use super::keyboard::{FieldTarget, Span, Unit, extend_by_unit, focused_field, unit_range};
use super::*;
use crate::layout::node::fields::pointer;
use crate::layout::node::prop::{Keyword, keywords};
use crate::lua::call_logged;
use crate::lua::marshal::{modifiers_table, rect_table};

mod callbacks;
mod position;
mod press;
mod wheel;

pub(crate) use callbacks::apply_hover_write;
pub(in crate::wayland::input) use callbacks::call_on_click;
use callbacks::{call_on_drag, call_on_press, local_pointer_table};
pub(in crate::wayland) use press::{ArmedClick, ArmedSerial, ClickedNode, LastClick};
use press::{double_click, press_chooses_focus, release_completes_click, release_ends_press};

enum HoverUpdate {
    Pointer,
    Layout,
}

impl HoverUpdate {
    fn fires_on_hover(&self) -> bool {
        matches!(self, Self::Pointer)
    }
}

/// Innermost node with callable `on_click` or `submits_secure = true` in a hit path (ADR-0050 decision 1).
/// Scan inward: the deepest node is often an unhandled `text` child, and a node without a handler
/// is transparent. `layout::node::resolve_declared` refuses an `on_click` that is not a function.
fn click_target(path: &[&layout::ResolvedNode], point: layout::hit::LogicalPoint, floor: usize) -> Option<Clickable> {
    (floor..path.len()).rev().find_map(|depth| {
        let node = path[depth];
        let on_click = pointer::on_click.read(&node.properties).ok().flatten();
        let submits_secure = pointer::submits_secure.read(&node.properties).is_ok_and(|on| on);
        if on_click.is_none() && !submits_secure {
            return None;
        }
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        let at = local_pointer(&path[..=depth], rect, point);
        Some(Clickable { rect, handler: on_click, link: None, submits_secure, at })
    })
}

/// `point` in the untransformed space of `path`'s last node, relative to its box. An empty path or a
/// degenerate transform has no inverse; the raw point minus `rect` then stands, as before transforms.
fn local_pointer(
    path: &[&layout::ResolvedNode],
    rect: LogicalRect,
    point: layout::hit::LogicalPoint,
) -> layout::hit::LogicalPoint {
    layout::hit::node_local(path, point)
        .unwrap_or(layout::hit::LogicalPoint { x: point.x - rect.x, y: point.y - rect.y })
}

/// A held drag's current rect and [`local_pointer`], through the node's current path, so a node
/// that moves under its own drag reports one frame. A node gone from `root` keeps the press rect.
fn drag_local(
    root: Option<&layout::ResolvedNode>,
    drag: &DragTarget,
    point: layout::hit::LogicalPoint,
) -> (LogicalRect, layout::hit::LogicalPoint) {
    let path = root.and_then(|root| layout::hit::path_to_node(root, drag.id)).unwrap_or_default();
    let rect = layout::hit::absolute_rect(&path).unwrap_or(drag.rect);
    (rect, local_pointer(&path, rect, point))
}

/// Innermost node with callable `handler` (`on_press` or `on_double_click`); unhandled nodes are
/// transparent, like `on_click`'s scan, and each handler has its own target.
fn press_target(
    path: &[&layout::ResolvedNode],
    point: layout::hit::LogicalPoint,
    floor: usize,
    handler: fn(&layout::ResolvedNode) -> Option<Function>,
) -> Option<PressTarget> {
    (floor..path.len()).rev().find_map(|depth| {
        let node = path[depth];
        let handler = handler(node)?;
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        Some(PressTarget { rect, handler, at: local_pointer(&path[..=depth], rect, point), id: node.id })
    })
}

fn on_press(node: &layout::ResolvedNode) -> Option<Function> {
    pointer::on_press.read(&node.properties).ok().flatten()
}

fn on_double_click(node: &layout::ResolvedNode) -> Option<Function> {
    pointer::on_double_click.read(&node.properties).ok().flatten()
}

/// Click, press and double-click targets for a hit. A `textfield` is a leaf that answers only to
/// its own handlers, never an ancestor's (ADR-0092 decision 7); a masked one answers to none.
fn handlers_under(
    path: &[&layout::ResolvedNode],
    field: Option<&FieldTarget>,
    point: layout::hit::LogicalPoint,
    shaping: &ShapingHandle,
) -> (Option<Clickable>, Option<PressTarget>, Option<PressTarget>) {
    let floor = match field {
        None => 0,
        // ponytail: a textfield is always the deepest hit node, so the leaf is the field.
        Some(FieldTarget::Plain { .. }) => path.len().saturating_sub(1),
        Some(FieldTarget::Masked { .. }) => return (None, None, None),
    };
    let click = if field.is_none() { clickable(path, point, shaping) } else { click_target(path, point, floor) };
    (click, press_target(path, point, floor, on_press), press_target(path, point, floor, on_double_click))
}

/// Innermost node with callable `on_drag` (ADR-0116 decision 1); unhandled nodes are transparent,
/// so a handle inside a draggable track leaves the track draggable.
fn drag_target(path: &[&layout::ResolvedNode]) -> Option<DragTarget> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        let handler = pointer::on_drag.read(&node.properties).ok().flatten()?;
        Some(DragTarget { rect: layout::hit::absolute_rect(&path[..=depth])?, handler, id: node.id })
    })
}

/// Innermost node with callable `on_wheel` (ADR-0116 decision 2).
fn wheel_target(path: &[&layout::ResolvedNode]) -> Option<(usize, LogicalRect, Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        let on_wheel = pointer::on_wheel.read(&node.properties).ok().flatten()?;
        Some((depth, layout::hit::absolute_rect(&path[..=depth])?, on_wheel))
    })
}

/// Left press on a node with `on_drag`, held until release (ADR-0116 decision 1). Every Motion
/// calls it wherever the pointer goes; config clamping keeps a slider pinned at its end.
pub(in crate::wayland) struct ActiveDrag {
    instance_id: String,
    target: DragTarget,
    motion: Motion,
}

const VELOCITY_WINDOW_MS: u32 = 100;
/// Shorter spans divide sensor jitter by almost nothing: 1 px 1 ms after a rest is not 1000 px/s.
const VELOCITY_MIN_SPAN_MS: u32 = 8;

/// The press and motions of a held drag, for `on_drag`'s surface-logical velocity in px/s.
#[derive(Default)]
struct Motion {
    /// `(time, position)` within [`VELOCITY_WINDOW_MS`] of the newest, by the compositor's `u32`
    /// millisecond clock, which wraps.
    samples: std::collections::VecDeque<(u32, (f64, f64))>,
    /// The last velocity measured over at least [`VELOCITY_MIN_SPAN_MS`].
    velocity: (f64, f64),
}

impl Motion {
    /// The velocity after the sample at `time`, if any. Zero when it is the window's only sample,
    /// as for a pointer still that long; the last measured one over a span too short to measure,
    /// and for an event with no time (a leave), so a flick off the edge keeps its speed.
    fn velocity(&mut self, sample: Option<(u32, (f64, f64))>) -> (f64, f64) {
        let Some((time, at)) = sample else { return self.velocity };
        self.samples.push_back((time, at));
        while self.samples.front().is_some_and(|(then, _)| time.wrapping_sub(*then) > VELOCITY_WINDOW_MS) {
            self.samples.pop_front();
        }
        let Some(&(then, from)) = self.samples.front() else { return self.velocity };
        let span = time.wrapping_sub(then);
        if self.samples.len() == 1 {
            self.velocity = (0.0, 0.0);
        } else if span >= VELOCITY_MIN_SPAN_MS {
            let seconds = f64::from(span) / 1000.0;
            self.velocity = ((at.0 - from.0) / seconds, (at.1 - from.1) / seconds);
        }
        self.velocity
    }
}

/// The `on_drag` node under a press; `id` is re-found each event so the pointer is mapped through
/// the node's current transform.
#[derive(Clone)]
struct DragTarget {
    rect: LogicalRect,
    handler: Function,
    id: layout::scene::NodeId,
}

/// The `on_press` or `on_double_click` node under the pointer, with its rect and the node-local pointer.
struct PressTarget {
    rect: LogicalRect,
    handler: Function,
    at: layout::hit::LogicalPoint,
    id: layout::scene::NodeId,
}

/// Press/release target: handler, click-identity rect (see [`ArmedClick`]), and link `href`.
#[derive(Clone)]
struct Clickable {
    rect: LogicalRect,
    handler: Option<Function>,
    link: Option<String>,
    /// `submits_secure = true` sends the scope's armed `secure_submit` field on release, like Enter
    /// (ADR-0114); it is the only pointer path to a password, with no Lua callback.
    submits_secure: bool,
    /// Pointer in the node's own coordinates ([`local_pointer`]).
    at: layout::hit::LogicalPoint,
}

/// Links precede `on_click` (ADR-0106): a text `on_link` with an `href` under `point` wins over any
/// `on_click`, the text's own included, while plain words pass through. A `textfield` similarly
/// arms a click only for right and middle (ADR-0092 decision 7).
fn clickable(
    path: &[&layout::ResolvedNode],
    point: layout::hit::LogicalPoint,
    shaping: &ShapingHandle,
) -> Option<Clickable> {
    let link = path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "text" {
            return None;
        }
        let on_link = node::fields::text_flow::on_link.read(&node.properties).ok().flatten()?;
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        let local = layout::hit::node_local(&path[..=depth], point)?;
        let href = layout::hit::link_under(node, local, shaping)?;
        Some(Clickable { rect, handler: Some(on_link), link: Some(href), submits_secure: false, at: local })
    });
    link.or_else(|| click_target(path, point, 0))
}

/// Both targets from decision 1's single [`layout::hit::hit_path`] traversal. Two walks could
/// re-resolve between them and give one event two answers.
struct PointerHit {
    click: Option<Clickable>,
    field: Option<FieldTarget>,
    control: Option<layout::scene::NodeId>,
    /// Byte offset under the point in the plain `textfield` it landed in (ADR-0236), decided on
    /// this same walk so one event cannot get two answers.
    caret: Option<usize>,
    /// `on_drag` node under the press, with its rect (ADR-0116 decision 1).
    drag: Option<DragTarget>,
    press: Option<PressTarget>,
    double: Option<PressTarget>,
}

/// A masked field, or the `submits_secure` node that sends one.
fn hits_a_secret(field: Option<&FieldTarget>, click: Option<&Clickable>) -> bool {
    matches!(field, Some(FieldTarget::Masked { .. })) || click.is_some_and(|click| click.submits_secure)
}

fn focusable_hit(path: &[&layout::ResolvedNode]) -> Option<layout::scene::NodeId> {
    path.iter()
        .rev()
        .find(|node| layout::scene::is_named_control(node) || focused_field(&[node]).is_some())
        .map(|node| node.id)
}

/// Lua name for an evdev button, or `None` when this engine ignores it. Use strings, not raw 273 or
/// normalized 1/2/3, matching other categorical values. `None` means no arm or release: exposing
/// `BTN_TASK` as `"other"` would make it indistinguishable from `BTN_EXTRA` and could run a left
/// handler. ponytail: only left/right/middle of SCTK's eight names are handled. Upgrade:
/// `BTN_SIDE`/`BTN_EXTRA` (0x113/0x114), `BTN_BACK`/`BTN_FORWARD` (0x116/0x115).
fn pointer_button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some(MouseButton::Left),
        BTN_RIGHT => Some(MouseButton::Right),
        BTN_MIDDLE => Some(MouseButton::Middle),
        _ => None,
    }
    .map(Keyword::name)
}

keywords! {
    /// The mouse buttons `on_click` reports.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum MouseButton {
        Left,
        Right,
        Middle,
    }
}

keywords! {
    /// Where an `on_drag` call falls in its drag (ADR-0116).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DragPhase {
        Start,
        Move,
        End,
    }
}

/// Pointer input to `on_click` (ADR-0050); dispatch is delegated by `delegate_dispatch2!(App)`.
impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for (at, event) in events.iter().enumerate() {
            // `wl_pointer` is per seat; an event may name a surface destroyed by `visible` or
            // output change.
            let Some(index) = self.index_of_surface(&event.surface) else {
                continue;
            };
            self.pointer_event(index, event.position, &event.kind, false);
            let moves = |kind: &PointerEventKind| {
                !matches!(
                    kind,
                    PointerEventKind::Press { .. } | PointerEventKind::Release { .. } | PointerEventKind::Axis { .. }
                )
            };
            // A later move on the same surface in this batch supersedes this one's position.
            let superseded = events
                .get(at + 1..)
                .is_some_and(|rest| rest.iter().any(|next| next.surface == event.surface && moves(&next.kind)));
            if moves(&event.kind) && !superseded {
                self.sync_pointer(index);
            }
        }
    }
}

impl App {
    /// One pointer event on surface `index`: the path real `wl_pointer` events and `mantle input`
    /// share. An `injected` press carries no compositor serial, so it arms none and a popup grab or
    /// window move that wants one is refused.
    pub(in crate::wayland::input) fn pointer_event(
        &mut self,
        index: usize,
        position: (f64, f64),
        kind: &PointerEventKind,
        injected: bool,
    ) {
        match *kind {
            // Left/right/middle only (ADR-0050 second amendment); other codes cannot arm or
            // fire a config handler.
            PointerEventKind::Press { button, serial, time } => {
                if pointer_button_name(button).is_none() {
                    return;
                }
                let instance_id = self.surfaces[index].surface_id.clone();
                // ADR-0049 amendment: this turn's re-resolve consumes it; `run` clears it at
                // turn end. Both click edges arm it (see [`ArmedSerial`]).
                // An injected press has no serial; clearing a stale one keeps it from authorizing a
                // popup grab or window move.
                self.input_serial = (!injected).then(|| ArmedSerial { serial, instance_id: instance_id.clone() });
                // ADR-0051 amendment: preserve the "user asked again" stamp past disarm.
                self.pointer_input_count += 1;
                self.cancel_text_input_composition();
                let hit = self.hit_under(index, position);
                // Read before the call consumes `hit.field`; a held draft is still a plain
                // field (ADR-0108).
                let pressed_a_field = hit.field.is_some();
                let kind = match &hit.field {
                    Some(FieldTarget::Plain { .. }) => super::keyboard::ControlKind::Plain,
                    Some(FieldTarget::Masked { .. }) => super::keyboard::ControlKind::Masked,
                    None => super::keyboard::ControlKind::Button,
                };
                // Borrowed, not taken: the seams below compare what arrives against the focus
                // still held to decide whether to zeroize and what to repaint.
                let (masked, plain) = press_chooses_focus(
                    hit.field,
                    &instance_id,
                    hit.caret,
                    self.held.shift,
                    (button == BTN_LEFT).then(|| (std::time::Instant::now(), position)),
                    self.focused_secure_submit.as_ref(),
                    self.focused_text_field.as_ref(),
                );
                // Reassign through the zeroizing transition seam.
                self.focus_secure_submit(masked);
                self.focus_text_field(plain);
                let next =
                    hit.control.map(|id| super::keyboard::FocusedControl { surface_id: instance_id.clone(), id, kind });
                if pressed_a_field {
                    self.set_control_focus(next);
                } else {
                    self.focus_control(next);
                }
                // A press on the control Tab already focused still hides the outline.
                self.set_focus_visible(false);
                // `hit` holds a field's own handlers only, so a reply box never activates its card;
                // a field's left press is caret and selection, so only right and middle run them (ADR-0092).
                let runs = !pressed_a_field || button != BTN_LEFT;
                self.armed = hit.click.filter(|_| runs).map(|clickable| ArmedClick {
                    instance_id: instance_id.clone(),
                    rect: clickable.rect,
                    link: clickable.link,
                    button,
                });
                self.armed_double = hit.double.as_ref().filter(|_| runs).map(|target| ClickedNode {
                    instance_id: instance_id.clone(),
                    id: target.id,
                    button,
                });
                // Both press-time handlers run inside one window so `toplevel(id)` requests
                // queued by either are sent with this press's serial.
                crate::lua::toplevel::begin_press(self.client.lua(), position);
                if runs
                    && let (Some(target), Some(name)) = (hit.press, pointer_button_name(button))
                    && let Err((what, e)) =
                        call_on_press(self.client.lua(), &target.handler, &target, name, self.modifiers_held())
                {
                    crate::lua::report_raised(&target.handler, format!("{instance_id}: {what}"), &e);
                }
                // Left `on_drag` holds until release (ADR-0116 decision 1); other buttons stay
                // free for clicks, and fields drag nothing just as they click nothing.
                if button == BTN_LEFT
                    && !pressed_a_field
                    && let Some(target) = hit.drag
                {
                    let motion = Motion::default();
                    self.drag = Some(ActiveDrag { instance_id: instance_id.clone(), target, motion });
                    self.fire_on_drag(&instance_id, position, DragPhase::Start, Some(time));
                }
                // The compositor owns the pointer after a move or resize and may send no release,
                // so a held drag ends here and nothing stays armed for one.
                if self.send_toplevel_requests() {
                    self.fire_on_drag(&instance_id, position, DragPhase::End, None);
                    self.armed = None;
                    self.armed_double = None;
                }
            }
            PointerEventKind::Release { button, serial, time } => {
                let Some(name) = pointer_button_name(button) else {
                    return;
                };
                let instance_id = self.surfaces[index].surface_id.clone();
                // Clicks fire on release (ADR-0050 decision 2), so this serial arms a popup
                // opened by `on_click`.
                self.input_serial = (!injected).then(|| ArmedSerial { serial, instance_id: instance_id.clone() });
                self.pointer_input_count += 1;
                // The pointer is up, so motion stops growing a selection (ADR-0236).
                if let Some(field) = self.focused_text_field.as_mut() {
                    field.selecting = false;
                }
                // Release does not change focus; drag-off must not un-focus a textfield. End
                // drag before click so a combined control commits before its click handler.
                if button == BTN_LEFT {
                    self.fire_on_drag(&instance_id, position, DragPhase::End, Some(time));
                }
                let hit = self.hit_under(index, position);
                // A field's own `submits_secure` never sends: a right or middle click is no Enter.
                let submits = hit.field.is_none();
                let hit_double = hit.double;
                // `on_double_click` has its own target, armed by its own press: a node and button, not a rect.
                let clicked = hit_double.as_ref().map(|target| ClickedNode {
                    instance_id: instance_id.clone(),
                    id: target.id,
                    button,
                });
                let completed = clicked.filter(|clicked| self.armed_double.as_ref() == Some(clicked));
                if self.armed_double.as_ref().is_some_and(|armed| armed.button == button) {
                    self.armed_double = None;
                }
                let (doubles, last) = match completed {
                    Some(clicked) => {
                        double_click(self.last_click.as_ref(), (clicked, (std::time::Instant::now(), position)))
                    }
                    None => (false, None),
                };
                self.last_click = last;
                let hit = hit.click;
                let fires = release_completes_click(
                    self.armed.as_ref(),
                    &instance_id,
                    hit.as_ref().map(|clickable| (clickable.rect, clickable.link.as_deref())),
                    button,
                );
                // Clear before callback re-entry; only the matching button ends the slot.
                if release_ends_press(self.armed.as_ref(), button) {
                    self.armed = None;
                }
                if let Some(clickable) = hit.filter(|_| fires) {
                    match (clickable.link, clickable.handler) {
                        // Links take `href`, not the paragraph rect; a link has no `submits_secure`.
                        (Some(href), Some(handler)) => {
                            call_logged(&handler, href, format_args!("{instance_id}: on_link"));
                        }
                        (_, handler) => {
                            if clickable.submits_secure && submits {
                                self.prune_secure_focus();
                                self.finish_secure_submit();
                            }
                            if let Some(handler) = handler {
                                self.fire_on_click(&instance_id, clickable.rect, name, clickable.at, &handler, false);
                            }
                        }
                    }
                }
                if let Some(target) = hit_double.filter(|_| doubles) {
                    self.fire_on_click(&instance_id, target.rect, name, target.at, &target.handler, false);
                }
            }
            // Release will land elsewhere: cancel the armed click. `None` clears all hover
            // (ADR-0062), since no later Motion may arrive to close a tooltip.
            PointerEventKind::Leave { .. } => {
                // End held drag at the last position; no release reaches this surface.
                if let Some(&(_, position)) = self.pointer_at.as_ref() {
                    let instance_id = self.surfaces[index].surface_id.clone();
                    self.fire_on_drag(&instance_id, position, DragPhase::End, None);
                }
                self.forget_pointer();
                let tree = self.client.scene().surface(&self.surfaces[index].surface_id);
                self.sync_hover(index, tree, &[], HoverUpdate::Pointer);
            }
            // Motion off the armed rect does not disarm; return and release still click. Enter
            // and Motion update hover and may fire `on_hover` on crossings.
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                let moved = match *kind {
                    PointerEventKind::Motion { time } => Some(time),
                    _ => None,
                };
                let instance_id = self.surfaces[index].surface_id.clone();
                // Fires before the write: no `on_drag` handler can read `pointer_at`.
                if moved.is_some() {
                    self.fire_on_drag(&instance_id, position, DragPhase::Move, moved);
                    self.drag_selection(index, &instance_id, position);
                }
                self.pointer_at = Some((instance_id, position));
                // One lookup and one hit path serve both; `Scene::surface` lends its tree.
                let tree = self.client.scene().surface(&self.surfaces[index].surface_id);
                let point = layout::hit::LogicalPoint::from_position(position);
                let path = tree.map(|tree| layout::hit::hit_path(tree, point)).unwrap_or_default();
                self.sync_hover(index, tree, &path, HoverUpdate::Pointer);
                // Chosen while the tree is still borrowed, shown once that borrow has ended.
                let shape = layout::hit::cursor_under(&path, point, &self.shaping);
                // The real pointer's cursor is not an injected event's to set.
                if !injected {
                    self.show_cursor(shape);
                }
            }
            // Wheel (ADR-0069); use the event's own position.
            PointerEventKind::Axis { horizontal, vertical, .. } => {
                self.scroll_at(
                    index,
                    position,
                    horizontal.absolute,
                    horizontal.value120,
                    vertical.absolute,
                    vertical.value120,
                );
            }
        }
    }

    /// One hit-test answers click, field, and drag (ADR-0050 decisions 1/4). Coordinates are
    /// logical surface-local while `paint_surface` remains scale `1.0`; a future HiDPI change must
    /// move this conversion with `paint_surface` and `apply_input_region`. Handlers and targets are
    /// cloned out of the lent tree; the tree itself is not.
    fn hit_under(&self, index: usize, position: (f64, f64)) -> PointerHit {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else {
            return PointerHit {
                click: None,
                field: None,
                control: None,
                caret: None,
                drag: None,
                press: None,
                double: None,
            };
        };
        let point = layout::hit::LogicalPoint::from_position(position);
        let path = layout::hit::hit_path(tree, point);
        let field = focused_field(&path);
        // `None`, not `""`, for a field this does not hold: an empty draft measures to offset 0,
        // and handing that to `drag_selection` moves the held field's head into another field's
        // text. Only the draft `press_chooses_focus` would resume has a caret (ADR-0108).
        let (click, press, double) = handlers_under(&path, field.as_ref(), point, &self.shaping);
        let held = self
            .focused_text_field
            .as_ref()
            .filter(|held| matches!(&field, Some(FieldTarget::Plain { id, .. }) if *id == held.id));
        PointerHit {
            click,
            control: focusable_hit(&path),
            // Its own caret too, not just its draft: a draft too wide to fit is drawn slid to
            // follow the caret, and a press reads the byte under where it actually landed.
            caret: held
                .and_then(|held| layout::hit::caret_at(&path, point, &held.buffer, held.selection.1, &self.shaping)),
            field,
            drag: drag_target(&path),
            press,
            double,
        }
    }

    /// Whether a press or release at `position` would reach a masked field or a `submits_secure` node, which
    /// `mantle input` refuses to touch.
    pub(in crate::wayland::input) fn reaches_a_secret(&self, index: usize, position: (f64, f64)) -> bool {
        let hit = self.hit_under(index, position);
        hits_a_secret(hit.field.as_ref(), hit.click.as_ref())
    }

    /// Grow the focused field's selection to the pointer, keeping its anchor: a press inside a
    /// plain field that has since moved (ADR-0236). Off its own surface the drag does nothing,
    /// rather than reading a caret out of some other field the pointer crossed.
    fn drag_selection(&mut self, index: usize, instance_id: &str, position: (f64, f64)) {
        let selecting =
            self.focused_text_field.as_ref().is_some_and(|field| field.selecting && field.surface_id == instance_id);
        let Some(caret) = selecting.then(|| self.hit_under(index, position).caret).flatten() else {
            return;
        };
        if let Some(field) = self.focused_text_field.as_mut()
            && let span = field.span.live(&field.buffer, field.selection)
            && let selection = extend_by_unit(&field.buffer, span, caret)
            && selection != field.selection
        {
            (field.selection, field.span, field.goal_x) = (selection, Span { produced: selection, ..span }, None);
            field.history.break_typing();
            self.text_input.note_other_change();
            self.mark_field_input_changed(instance_id);
            self.fit_focused_field();
        }
    }

    /// Fire one held drag edge for `instance_id` (ADR-0116 decision 1); `end` clears it before the
    /// callback so re-entry finds nothing held. Handler raises are logged and swallowed. `time` is the
    /// event's; an end without one (a leave, a closed surface, a compositor move) reports the last
    /// motion's velocity.
    fn fire_on_drag(&mut self, instance_id: &str, position: (f64, f64), phase: DragPhase, time: Option<u32>) {
        let Some(drag) = self.drag.as_mut().filter(|drag| drag.instance_id == instance_id) else {
            return;
        };
        let velocity = drag.motion.velocity(time.map(|time| (time, position)));
        let point = layout::hit::LogicalPoint::from_position(position);
        let (rect, local) = drag_local(self.client.scene().surface(instance_id), &drag.target, point);
        let handler = drag.target.handler.clone();
        if phase == DragPhase::End {
            self.drag = None;
        }
        if let Err((what, e)) =
            call_on_drag(self.client.lua(), &handler, rect, local, phase, self.modifiers_held(), velocity)
        {
            crate::lua::report_raised(&handler, format!("{instance_id}: {what}"), &e);
        }
    }

    /// Set the cursor only when it changes; motion arrives per pixel and each `set_shape` would add
    /// compositor work. `Leave` clears the cache because the shape is bound to the next enter
    /// serial.
    fn show_cursor(&mut self, shape: cursor_icon::CursorIcon) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        if self.cursor_shown == Some(shape) {
            return;
        }
        match pointer.set_cursor(&self.conn, shape) {
            Ok(()) => self.cursor_shown = Some(shape),
            // Remember failure as shown; a missing themed cursor is reported once, not per pixel.
            Err(err) => {
                warn!("could not set the cursor to {}: {err}", shape.name());
                self.cursor_shown = Some(shape);
            }
        }
    }

    /// Re-run hover writes after layout moves rows under a stationary pointer, without `on_hover`
    /// (ADR-0112 amendment): the row that slid away stops reading hovered and the row now under
    /// the pointer starts, or stale tint follows the old row off the viewport. No user crossing
    /// occurred.
    pub(in crate::wayland) fn refresh_hover_after_layout(&mut self) {
        let Some((surface_id, position)) = &self.pointer_at else {
            return;
        };
        let Some(index) = self.surfaces.iter().position(|tracked| &tracked.surface_id == surface_id) else {
            return;
        };
        let tree = self.client.scene().surface(surface_id);
        let point = layout::hit::LogicalPoint::from_position(*position);
        let path = tree.map(|tree| layout::hit::hit_path(tree, point)).unwrap_or_default();
        self.sync_hover(index, tree, &path, HoverUpdate::Layout);
        self.sync_pointer(index);
    }

    /// The pointer is gone from its surface: no release, no cached cursor, no position.
    pub(in crate::wayland) fn forget_pointer(&mut self) {
        self.armed = None;
        self.armed_double = None;
        self.last_click = None;
        self.cursor_shown = None;
        self.pointer_at = None;
    }

    /// The pointer half of `App::drop_role_object`'s scrub, for the one leave the compositor never
    /// sends.
    ///
    /// Every teardown destroys its role object from this side (ADR-0088), so no `wl_pointer`
    /// leave follows and the `Leave` arm below never runs: `pointer_at` keeps naming a surface
    /// that is gone, every hover signal inside it stays true, and `on_hover(false)` is never
    /// called. Closing the notification history with the pointer over its list left
    /// `hold_expiry(300)` standing, pausing every countdown until ADR-0094's deadline released it
    /// on its own, and the panel reopened pre-tinted.
    ///
    /// Same work as `Leave`, because the surface is equally gone: end a held drag at its last
    /// position, drop the armed click, and write every hover off with `on_hover` firing.
    pub(in crate::wayland) fn pointer_left_destroyed_surface(&mut self, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(&(_, position)) = self.pointer_at.as_ref().filter(|(at, _)| *at == surface_id) else {
            return;
        };
        self.fire_on_drag(&surface_id, position, DragPhase::End, None);
        self.forget_pointer();
        let tree = self.client.scene().surface(&surface_id);
        self.sync_hover(index, tree, &[], HoverUpdate::Pointer);
        self.sync_pointer(index);
    }

    /// Write all `hover` signals from the pointer's hit `path`, empty off the surface (ADR-0062). Collect writes before
    /// `set_changed` because the tree borrow must end; only moved values dirty the scene (decision
    /// 4), so a stationary pointer inside one node re-resolves nothing while device-rate motion
    /// continues (ADR-0044 decision 2). Pointer updates enable `on_hover`; layout refreshes do not.
    ///
    /// Takes the tree and path rather than finding them so one lookup serves this and the cursor.
    fn sync_hover(
        &self,
        index: usize,
        tree: Option<&layout::ResolvedNode>,
        path: &[&layout::ResolvedNode],
        update: HoverUpdate,
    ) {
        // Skip the expensive tree walk when no config registered `hover(name)`.
        if !crate::lua::signal::any_hover_registered(self.client.lua()) {
            return;
        }
        let Some(tree) = tree else {
            return;
        };
        let writes = layout::hover::hover_writes(tree, path);
        let lua = self.client.lua();
        for write in writes {
            apply_hover_write(lua, write, update.fires_on_hover(), &self.surfaces[index].surface_id);
        }
    }

    /// Call a node's `on_click` with its rect (ADR-0050 decision 3); swallow handler raises.
    /// ADR-0046 rescue is for failed evaluation, not a misbehaving callback.
    pub(in crate::wayland::input) fn fire_on_click(
        &mut self,
        instance_id: &str,
        rect: LogicalRect,
        button: &str,
        local: layout::hit::LogicalPoint,
        on_click: &Function,
        keyboard: bool,
    ) {
        // A pointer click after Enter or Space is a first click.
        if keyboard {
            self.last_click = None;
        }
        // `signal:set()` marks its own dirty flag (ADR-0044 decision 5); this call need not.
        crate::lua::focus::begin_callback(self.client.lua(), instance_id, keyboard);
        if let Err((what, e)) = call_on_click(self.client.lua(), on_click, rect, button, local, self.modifiers_held()) {
            crate::lua::report_raised(on_click, format!("{instance_id}: {what}"), &e);
        }
        crate::lua::focus::end_callback(self.client.lua());
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::hit_node;
    use super::*;
    use crate::wayland::input::keyboard::tests::{plain_textfield, secure_submit_table, textfield, with_property};
    use mlua::Table;

    pub(super) fn pt(x: f32, y: f32) -> layout::hit::LogicalPoint {
        layout::hit::LogicalPoint { x, y }
    }

    #[test]
    fn pointer_focus_chooses_a_field_or_named_button_from_the_hit_path() {
        let lua = Lua::new();
        let field = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let mut button = hit_node(&lua, "rect", (0.0, 0.0, 30.0, 20.0), true);
        std::rc::Rc::make_mut(&mut button.properties)
            .insert("accessible_name", Value::String(lua.create_string("Unlock").unwrap()));
        let unnamed = hit_node(&lua, "rect", (0.0, 0.0, 30.0, 20.0), true);
        assert_eq!(focusable_hit(&[&field]), Some(field.id));
        assert_eq!(focusable_hit(&[&button]), Some(button.id));
        assert_eq!(focusable_hit(&[&button, &unnamed]), Some(button.id));
        assert_eq!(focusable_hit(&[&unnamed]), None);
    }

    #[test]
    fn a_masked_field_or_a_submit_node_is_a_secret_and_a_plain_field_is_not() {
        let lua = Lua::new();
        let point = pt(1.0, 1.0);
        let masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        assert!(hits_a_secret(focused_field(&[&masked]).as_ref(), None));
        let plain = plain_textfield(&lua);
        assert!(!hits_a_secret(focused_field(&[&plain]).as_ref(), None));
        let submit =
            with_property(hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false), "submits_secure", Value::Boolean(true));
        let click = click_target(&[&submit], point, 0).expect("a submit node is clickable");
        assert!(hits_a_secret(None, Some(&click)));
        let inert = hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), true);
        assert!(!hits_a_secret(None, click_target(&[&inert], point, 0).as_ref()));
    }

    #[test]
    fn pointer_hover_updates_fire_callbacks_but_layout_refreshes_do_not() {
        assert!(HoverUpdate::Pointer.fires_on_hover(), "Enter, Motion, and Leave are user input");
        assert!(!HoverUpdate::Layout.fires_on_hover(), "layout movement under a resting pointer is silent");
    }

    #[test]
    fn the_innermost_on_drag_or_on_wheel_node_takes_it_and_a_bare_node_is_transparent() {
        let lua = Lua::new();
        let handler = || Value::Function(lua.create_function(|_, ()| Ok(())).unwrap());
        let mut thumb = hit_node(&lua, "icon", (5.0, 2.0, 20.0, 20.0), true);
        std::rc::Rc::make_mut(&mut thumb.properties).insert("on_wheel", handler());
        let mut track = hit_node(&lua, "row", (10.0, 4.0, 40.0, 24.0), false);
        std::rc::Rc::make_mut(&mut track.properties).insert("on_drag", handler());
        std::rc::Rc::make_mut(&mut track.properties).insert("on_wheel", handler());
        track.children.push(thumb);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(track);

        let on_thumb = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let DragTarget { rect, .. } = drag_target(&on_thumb).expect("the track carries the on_drag");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
        let (depth, rect, _) = wheel_target(&on_thumb).expect("the thumb carries an on_wheel");
        assert_eq!((depth, rect), (2, LogicalRect { x: 15.0, y: 6.0, width: 20.0, height: 20.0 }), "innermost");
        let beside = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 45.0, y: 12.0 });
        assert_eq!(wheel_target(&beside).map(|(depth, ..)| depth), Some(1), "the track, past the thumb");
        let bare = hit_node(&lua, "column", (0.0, 0.0, 100.0, 32.0), false);
        assert!(drag_target(&[&bare]).is_none() && wheel_target(&[&bare]).is_none());
    }

    #[test]
    fn on_press_takes_the_innermost_handler_and_gets_rect_button_and_local_pointer() {
        let lua = Lua::new();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&seen);
        let handler = Value::Function(
            lua.create_function(move |_, (rect, button, pointer): (Table, String, Table)| {
                sink.borrow_mut().push((rect.get::<f32>("width").unwrap(), button, pointer.get::<f32>("x").unwrap()));
                Ok(())
            })
            .unwrap(),
        );
        let mut bar = hit_node(&lua, "row", (10.0, 4.0, 40.0, 24.0), false);
        std::rc::Rc::make_mut(&mut bar.properties).insert("on_press", handler);
        bar.children.push(hit_node(&lua, "icon", (5.0, 2.0, 20.0, 20.0), true));
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(bar);

        let at = layout::hit::LogicalPoint { x: 20.0, y: 12.0 };
        let path = layout::hit::hit_path(&root, at);
        let target =
            press_target(&path, at, 0, on_press).expect("the bar carries on_press; its icon child is transparent");
        call_on_press(&lua, &target.handler, &target, "right", [false; 4]).unwrap();
        assert_eq!(*seen.borrow(), vec![(40.0, "right".to_string(), 10.0)]);
        let bare = hit_node(&lua, "column", (0.0, 0.0, 100.0, 32.0), false);
        assert!(press_target(&[&bare], at, 0, on_press).is_none());
    }

    #[test]
    fn the_innermost_handled_node_of_any_kind_under_the_pointer_is_the_one_that_would_fire() {
        // The shape decision 1 exists for: the deepest node is an unhandled `text`, and the outer
        // `column` has no handler, so only the `row` between them answers.
        let lua = Lua::new();
        let mut row = hit_node(&lua, "row", (10.0, 4.0, 40.0, 24.0), true);
        row.children.push(hit_node(&lua, "text", (6.0, 5.0, 28.0, 14.0), false));
        let mut column = hit_node(&lua, "column", (0.0, 0.0, 100.0, 32.0), false);
        column.children.push(row);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(column.clone());

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let Clickable { rect, .. } = click_target(&path, layout::hit::LogicalPoint { x: 20.0, y: 12.0 }, 0)
            .expect("the row carries an on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });

        // The same `text` leaf with its own `on_click` is now the innermost handler.
        column.children[0].children[0] = hit_node(&lua, "text", (6.0, 5.0, 28.0, 14.0), true);
        root.children[0] = column;
        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let Clickable { rect, .. } = click_target(&path, layout::hit::LogicalPoint { x: 20.0, y: 12.0 }, 0)
            .expect("the text carries an on_click");
        assert_eq!(rect, LogicalRect { x: 16.0, y: 9.0, width: 28.0, height: 14.0 });
    }

    #[test]
    fn a_node_with_no_on_click_is_transparent_rather_than_a_barrier() {
        // An unhandled node nested inside a handled one must not swallow the click: the scan
        // keeps walking outwards past it.
        let lua = Lua::new();
        let inner = hit_node(&lua, "rect", (5.0, 2.0, 20.0, 20.0), false);
        let mut outer = hit_node(&lua, "rect", (10.0, 4.0, 40.0, 24.0), true);
        outer.children.push(inner);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(outer);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        assert_eq!(path.len(), 3, "the inner rect is still on the path");
        let Clickable { rect, .. } = click_target(&path, layout::hit::LogicalPoint { x: 20.0, y: 12.0 }, 0)
            .expect("the outer rect carries the on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
    }

    /// `pointer` is where the node's own box was hit, whatever its drawn transform.
    #[test]
    fn on_click_pointer_is_node_local_through_translate_scale_and_rotate() {
        use crate::wayland::node::apply_affine;
        let lua = Lua::new();
        type Case = (&'static str, fn(&mut layout::ResolvedNode));
        let cases: [Case; 3] = [
            ("translate", |n| n.transform.translate = (1785.0, 0.0)),
            ("scale", |n| n.transform.scale = (2.0, 2.0)),
            ("rotate", |n| n.transform.rotate = 90.0),
        ];
        for (name, transform) in cases {
            let mut node = hit_node(&lua, "rect", (10.0, 4.0, 40.0, 24.0), true);
            transform(&mut node);
            let mut root = hit_node(&lua, "panel", (0.0, 0.0, 3000.0, 400.0), false);
            root.children.push(node);
            // The node's own point (15, 4) in its box, found where it is drawn.
            let drawn = apply_affine(layout::hit::path_transform(&[&root, &root.children[0]]), 25.0, 8.0);
            let point = layout::hit::LogicalPoint { x: drawn.0, y: drawn.1 };
            let path = layout::hit::hit_path(&root, point);
            let Clickable { at, .. } = click_target(&path, point, 0).expect(name);
            assert!((at.x - 15.0).abs() < 1e-3 && (at.y - 4.0).abs() < 1e-3, "{name}: {}, {}", at.x, at.y);
        }
    }

    /// A held drag maps through the node's path now and reports its current rect, not the
    /// press-time one; with no path the raw point minus the press `rect` stands.
    #[test]
    fn on_drag_pointer_follows_the_node_and_falls_back_to_the_raw_point() {
        let lua = Lua::new();
        let build = |mutate: fn(&mut layout::ResolvedNode)| {
            let mut node = hit_node(&lua, "rect", (10.0, 4.0, 40.0, 24.0), true);
            mutate(&mut node);
            let id = node.id;
            let mut root = hit_node(&lua, "panel", (0.0, 0.0, 3000.0, 400.0), false);
            root.children.push(node);
            (root, id)
        };
        let drag = |id| DragTarget {
            rect: LogicalRect { x: 2.0, y: 4.0, width: 40.0, height: 24.0 },
            handler: lua.create_function(|_, ()| Ok(())).unwrap(),
            id,
        };
        let point = pt(25.0, 8.0);
        type Case = (&'static str, fn(&mut layout::ResolvedNode), layout::hit::LogicalPoint);
        let cases: [Case; 3] = [
            ("moved since the press", |_| {}, pt(15.0, 4.0)),
            ("translated", |n| n.transform.translate = (100.0, 0.0), pt(-85.0, 4.0)),
            ("degenerate transform", |n| n.transform.scale = (0.0, 0.0), pt(15.0, 4.0)),
        ];
        let now = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        for (name, mutate, want) in cases {
            let (root, id) = build(mutate);
            assert_eq!(drag_local(Some(&root), &drag(id), point), (now, want), "{name}");
        }
        let (root, _) = build(|_| {});
        let gone = drag(layout::scene::NodeId::test(u64::MAX));
        let pressed = (drag(gone.id).rect, pt(23.0, 4.0));
        assert_eq!(drag_local(Some(&root), &gone, point), pressed, "removed mid-drag");
        assert_eq!(drag_local(None, &gone, point), pressed, "surface gone");
    }

    #[test]
    fn drag_velocity_spans_the_last_100_ms_of_event_time_and_a_still_pointer_reads_zero() {
        let mut motion = Motion::default();
        let mut at = |time, x: f64, y: f64| motion.velocity(Some((time, (x, y))));
        assert_eq!(at(1000, 10.0, 10.0), (0.0, 0.0), "the press alone");
        assert_eq!(at(1050, 20.0, 5.0), (200.0, -100.0));
        assert_eq!(at(1100, 40.0, 5.0), (300.0, -50.0), "from the press, 100 ms back");
        assert_eq!(at(1150, 40.0, 5.0), (200.0, 0.0), "the press fell out");
        assert_eq!(at(1260, 40.0, 5.0), (0.0, 0.0), "still for 100 ms before release");
        assert_eq!(at(1261, 41.0, 5.0), (0.0, 0.0), "1 px 1 ms after a rest is too short to measure");
        assert_eq!(at(1270, 50.0, 5.0), (1000.0, 0.0));
        assert_eq!(at(1272, 50.0, 5.0), (833.3333333333334, 0.0), "the span reaches back to the rest");
        assert_eq!(motion.velocity(None), (833.3333333333334, 0.0), "a leave keeps the last motion's velocity");
        let mut pruned = Motion::default();
        pruned.velocity(Some((0, (0.0, 0.0))));
        assert_eq!(pruned.velocity(Some((95, (95.0, 0.0)))), (1000.0, 0.0));
        assert_eq!(pruned.velocity(Some((101, (140.0, 0.0)))), (1000.0, 0.0), "a 6 ms span keeps the last measured");
        let mut wrapping = Motion::default();
        wrapping.velocity(Some((u32::MAX - 9, (0.0, 0.0))));
        assert_eq!(wrapping.velocity(Some((10, (20.0, 0.0)))), (1000.0, 0.0), "the u32 clock wraps");
    }

    /// ADR-0114: `submits_secure = true` alone makes a node a click target, with no Lua handler to call.
    #[test]
    fn submit_alone_makes_any_node_a_click_target() {
        let lua = Lua::new();
        let mut row = hit_node(&lua, "row", (0.0, 0.0, 40.0, 24.0), false);
        std::rc::Rc::make_mut(&mut row.properties).insert("submits_secure", Value::Boolean(true));
        let clickable =
            click_target(&[&row], layout::hit::LogicalPoint { x: 1.0, y: 1.0 }, 0).expect("submit arms a click");
        assert!(clickable.handler.is_none() && clickable.submits_secure);
    }

    #[test]
    fn on_double_click_and_on_click_take_their_own_innermost_targets() {
        let lua = Lua::new();
        let mut icon = hit_node(&lua, "icon", (0.0, 0.0, 20.0, 20.0), false);
        let properties = std::rc::Rc::make_mut(&mut icon.properties);
        properties.insert("on_double_click", handler(&lua));
        properties.insert("accessible_name", Value::String(lua.create_string("Open").unwrap()));
        let mut row = hit_node(&lua, "row", (0.0, 0.0, 40.0, 24.0), false);
        std::rc::Rc::make_mut(&mut row.properties).insert("on_click", handler(&lua));
        let point = layout::hit::LogicalPoint { x: 1.0, y: 1.0 };
        let path = [&row, &icon];
        let click = click_target(&path, point, 0).expect("the row's on_click");
        assert_eq!(click.rect.width, 40.0, "the icon does not swallow the row's single click");
        let double = press_target(&path, point, 0, on_double_click).expect("the icon's on_double_click");
        assert_eq!(double.id, icon.id);
        let shaping = ShapingHandle::spawn();
        assert_eq!(layout::hit::cursor_under(&[&icon], point, &shaping), cursor_icon::CursorIcon::Pointer);
        assert!(!layout::scene::is_named_control(&icon), "keyboard activation never double-clicks");
    }

    #[test]
    fn only_the_three_buttons_a_config_can_name_are_handled_at_all() {
        assert_eq!(pointer_button_name(BTN_LEFT), Some("left"));
        assert_eq!(pointer_button_name(BTN_RIGHT), Some("right"));
        assert_eq!(pointer_button_name(BTN_MIDDLE), Some("middle"));
        // A side button, a browser-back button, and a code off the end of the mouse range: each
        // answers `None`, since a config cannot tell them apart.
        assert_eq!(pointer_button_name(0x113), None);
        assert_eq!(pointer_button_name(0x116), None);
        assert_eq!(pointer_button_name(0), None);
    }

    fn handler(lua: &Lua) -> Value {
        Value::Function(lua.create_function(|_, ()| Ok(())).unwrap())
    }

    /// A card with both handlers around `field`, which gets its own pair when `own`.
    fn card_around(lua: &Lua, field: layout::ResolvedNode, own: bool) -> (layout::ResolvedNode, layout::ResolvedNode) {
        let card = with_property(
            with_property(hit_node(lua, "column", (0.0, 0.0, 99.0, 99.0), false), "on_click", handler(lua)),
            "on_press",
            handler(lua),
        );
        let field = if own {
            with_property(with_property(field, "on_click", handler(lua)), "on_press", handler(lua))
        } else {
            field
        };
        (card, field)
    }

    #[test]
    fn a_plain_field_answers_to_its_own_handlers_and_never_a_cards_and_a_masked_one_to_none() {
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let at = pt(5.0, 5.0);
        let (card, own) = card_around(&lua, plain_textfield(&lua), true);
        let field = focused_field(&[&own]);
        let (click, press, _) = handlers_under(&[&card, &own], field.as_ref(), at, &shaping);
        assert_eq!(
            click.expect("the field's on_click").rect,
            LogicalRect { x: 0.0, y: 0.0, width: 40.0, height: 24.0 }
        );
        assert!(press.is_some_and(|press| press.rect.width == 40.0), "the field's on_press, not the card's");

        let (card, bare) = card_around(&lua, plain_textfield(&lua), false);
        let field = focused_field(&[&bare]);
        let (click, press, _) = handlers_under(&[&card, &bare], field.as_ref(), at, &shaping);
        assert!(click.is_none() && press.is_none(), "no own handler: the card's never fires");
        // Outside a field the card still wins, as before.
        let (click, press, _) = handlers_under(&[&card], None, at, &shaping);
        assert!(click.is_some() && press.is_some());

        let masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let (card, masked) = card_around(&lua, masked, true);
        let field = focused_field(&[&masked]);
        let (click, press, _) = handlers_under(&[&card, &masked], field.as_ref(), at, &shaping);
        assert!(click.is_none() && press.is_none());
    }
}

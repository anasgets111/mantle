//! Pointer input: `on_click` (ADR-0050), `on_drag` and `on_wheel` (ADR-0116), links, hover and the
//! cursor.

use shared::warn;
use smithay_client_toolkit::seat::pointer::{BTN_BACK, BTN_EXTRA, BTN_FORWARD, BTN_SIDE};

use super::keyboard::{FieldTarget, Span, Unit, extend_by_unit, unit_range};
use super::*;
use crate::layout::node::fields::pointer;
use crate::layout::node::prop::{Keyword, keywords};
use crate::lua::call_logged;
use crate::lua::marshal::{modifiers_table, rect_table};

mod callbacks;
mod drag;
mod position;
mod press;
mod target;
mod wheel;
pub(in crate::wayland) use wheel::Fingers;

pub(crate) use callbacks::apply_hover_write;
pub(in crate::wayland::input) use callbacks::call_on_click;
use callbacks::{call_on_drag, call_on_press, local_pointer_table};
use drag::Motion;
pub(in crate::wayland) use press::{ArmedClick, ArmedSerial, ClickedNode, LastClick};
use press::{double_click, press_chooses_focus, release_completes_click, release_ends_press};
use target::{PressTarget, drag_local, wheel_target};

/// Left press on a node with `on_drag`, held until release (ADR-0116 decision 1). Every Motion
/// calls it wherever the pointer goes; config clamping keeps a slider pinned at its end.
pub(in crate::wayland) struct ActiveDrag {
    instance_id: String,
    target: PressTarget,
    motion: Motion,
}

/// Lua name for an evdev button, or `None` when this engine ignores it. Use strings, not raw 273 or
/// normalized 1/2/3, matching other categorical values. Mice send their thumb buttons as
/// `BTN_SIDE`/`BTN_EXTRA` and some send `BTN_BACK`/`BTN_FORWARD`, so each pair is one name. `None`
/// means no arm or release: exposing `BTN_TASK` as `"other"` would make it indistinguishable from
/// the rest and could run a left handler.
fn pointer_button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some(MouseButton::Left),
        BTN_RIGHT => Some(MouseButton::Right),
        BTN_MIDDLE => Some(MouseButton::Middle),
        BTN_SIDE | BTN_BACK => Some(MouseButton::Back),
        BTN_EXTRA | BTN_FORWARD => Some(MouseButton::Forward),
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
        Back,
        Forward,
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
            // Named buttons only (ADR-0050 second amendment); other codes cannot arm or fire a
            // config handler.
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
                self.stop_glide_under(index, position);
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
                    self.focused_secure_submit.as_deref(),
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
                // a field's left press is caret and selection, so only the other buttons run them (ADR-0092).
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
                // A field's own `submits_secure` never sends: a non-left click is no Enter.
                let submits = hit.field.is_none();
                // `on_double_click` has its own target, armed by its own press: a node and button, not a rect.
                let hit_double = hit.double;
                let completed = self.armed_double.clone().filter(|armed| {
                    armed.button == button
                        && armed.instance_id == instance_id
                        && hit_double.as_ref().is_some_and(|target| target.id == armed.id)
                });
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
                self.sync_hover(index, tree, &[], true);
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
                self.sync_hover(index, tree, &path, true);
                // Chosen while the tree is still borrowed, shown once that borrow has ended.
                let shape = layout::hit::cursor_under(&path, point, &self.shaping);
                // The real pointer's cursor is not an injected event's to set.
                if !injected {
                    self.show_cursor(shape);
                }
            }
            // Wheel (ADR-0069); use the event's own position. Finger scrolls coast on lift.
            PointerEventKind::Axis { time, horizontal, vertical, source } => {
                let moved = self.scroll_at(
                    index,
                    position,
                    horizontal.absolute,
                    horizontal.value120,
                    vertical.absolute,
                    vertical.value120,
                );
                self.coast_after_fingers(time, source, moved, horizontal.stop || vertical.stop);
            }
        }
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
        self.sync_hover(index, tree, &path, false);
        self.sync_pointer(index);
    }

    /// The pointer is gone from its surface: no release, no cached cursor, no position.
    pub(in crate::wayland) fn forget_pointer(&mut self) {
        self.armed = None;
        self.armed_double = None;
        self.last_click = None;
        self.cursor_shown = None;
        self.pointer_at = None;
        self.finger_scroll = None;
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
        self.sync_hover(index, tree, &[], true);
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
        fires_on_hover: bool,
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
            apply_hover_write(lua, write, fires_on_hover, &self.surfaces[index].surface_id);
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
    use super::*;

    pub(super) fn pt(x: f32, y: f32) -> layout::hit::LogicalPoint {
        layout::hit::LogicalPoint { x, y }
    }

    #[test]
    fn only_the_buttons_a_config_can_name_are_handled_at_all() {
        assert_eq!(pointer_button_name(BTN_LEFT), Some("left"));
        assert_eq!(pointer_button_name(BTN_RIGHT), Some("right"));
        assert_eq!(pointer_button_name(BTN_MIDDLE), Some("middle"));
        assert_eq!(pointer_button_name(BTN_SIDE), Some("back"));
        assert_eq!(pointer_button_name(BTN_BACK), Some("back"));
        assert_eq!(pointer_button_name(BTN_EXTRA), Some("forward"));
        assert_eq!(pointer_button_name(BTN_FORWARD), Some("forward"));
        // `BTN_TASK` and a code off the end of the mouse range answer `None`: a config could not
        // tell them apart.
        assert_eq!(pointer_button_name(0x117), None);
        assert_eq!(pointer_button_name(0), None);
    }
}

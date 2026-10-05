//! Keyboard focus for named controls, using the retained scene's node identity.

use super::*;
use crate::layout::node::fields::{common, pointer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::wayland) struct FocusedControl {
    pub surface_id: String,
    pub id: layout::scene::NodeId,
    pub kind: ControlKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::wayland) enum ControlKind {
    Plain,
    Masked,
    Button,
}

#[derive(Clone)]
struct Control {
    focus: FocusedControl,
    masked: Option<node::SecureSubmitTarget>,
}

fn controls(surface_id: &str, node: &layout::ResolvedNode, out: &mut Vec<Control>) {
    if !node.visible || node.leaving {
        return;
    }
    let target = focused_field(&[node]);
    let field = matches!(&target, Some(FieldTarget::Plain { .. }));
    let masked = match target {
        Some(FieldTarget::Masked { target, .. }) => Some(target),
        _ => None,
    };
    let button = layout::scene::is_named_control(node);
    if node.rect.width > 0.0 && node.rect.height > 0.0 && (field || masked.is_some() || button) {
        let kind = if field {
            ControlKind::Plain
        } else if masked.is_some() {
            ControlKind::Masked
        } else {
            ControlKind::Button
        };
        out.push(Control { focus: FocusedControl { surface_id: surface_id.to_owned(), id: node.id, kind }, masked });
    }
    for child in node.content_children() {
        controls(surface_id, child, out);
    }
}

fn next_index(len: usize, current: Option<usize>, backwards: bool) -> usize {
    match (current, backwards) {
        (Some(index), true) => (index + len - 1) % len,
        (Some(index), false) => (index + 1) % len,
        (None, true) => len - 1,
        (None, false) => 0,
    }
}

fn tab_target(controls: &[Control], current: Option<&FocusedControl>, backwards: bool) -> Option<usize> {
    if controls.is_empty() || (controls.len() == 1 && controls[0].focus.kind == ControlKind::Plain) {
        return None;
    }
    let current = current.and_then(|focus| controls.iter().position(|control| &control.focus == focus));
    Some(next_index(controls.len(), current, backwards))
}

fn dispatch_tab(
    controls: &[Control],
    current: Option<&FocusedControl>,
    backwards: bool,
    mut focus: impl FnMut(Control),
) -> bool {
    let Some(index) = tab_target(controls, current, backwards) else { return false };
    focus(controls[index].clone());
    true
}

/// Enter and Space are consumed on repeat too, but activate once per press. The press serial
/// arms a popup grab as a pointer press would.
fn dispatch_activation(
    focus: &FocusedControl,
    keysym: Keysym,
    repeat: bool,
    serial: Option<u32>,
    mut activate: impl FnMut(Option<super::super::pointer::ArmedSerial>),
) -> bool {
    if !matches!(keysym, Keysym::Return | Keysym::KP_Enter | Keysym::space) {
        return false;
    }
    if !repeat {
        activate(
            serial.map(|serial| super::super::pointer::ArmedSerial { serial, instance_id: focus.surface_id.clone() }),
        );
    }
    true
}

fn centre(rect: LogicalRect) -> layout::hit::LogicalPoint {
    layout::hit::LogicalPoint { x: rect.width / 2.0, y: rect.height / 2.0 }
}

fn find(node: &layout::ResolvedNode, id: layout::scene::NodeId) -> Option<(&layout::ResolvedNode, LogicalRect)> {
    let path = layout::hit::path_to_node(node, id)?;
    Some((*path.last()?, layout::hit::absolute_rect(&path)?))
}

pub(in crate::wayland) fn secure_id(
    node: &layout::ResolvedNode,
    target: &node::SecureSubmitTarget,
) -> Option<layout::scene::NodeId> {
    if !node.visible || node.leaving {
        return None;
    }
    if let Some(node::PaintStyle::TextField { target: Some(current), .. }) = &node.paint
        && current == target
        && !node.is_disabled_field()
    {
        return Some(node.id);
    }
    node.content_children().find_map(|child| secure_id(child, target))
}

pub(in crate::wayland) fn secure_target_at(
    root: &layout::ResolvedNode,
    id: layout::scene::NodeId,
) -> Option<&node::SecureSubmitTarget> {
    let (node, _) = find(root, id)?;
    match node.paint.as_ref()? {
        node::PaintStyle::TextField { target: Some(target), .. } if !node.is_disabled_field() => Some(target),
        _ => None,
    }
}

fn secure_focus_accepts_keys(control: Option<&FocusedControl>, field: Option<&FocusedField>) -> bool {
    let Some(field) = field else { return false };
    let Some(control) = control else { return true };
    control.surface_id == field.surface_id && control.id == field.id && control.kind == ControlKind::Masked
}

/// Whether this Escape press reaches the surface: a fresh press without Ctrl, and no focused field with
/// something to do. Plain `(has text, composing, has on_cancel)`; secure `(buffer non-empty, has on_cancel)`.
pub(super) fn surface_gets_escape(
    repeat: bool,
    ctrl: bool,
    plain: Option<(bool, bool, bool)>,
    secure: Option<(bool, bool)>,
) -> bool {
    !repeat
        && !ctrl
        && !plain.is_some_and(|(text, composing, cancels)| text || composing || cancels)
        && !secure.is_some_and(|(filled, cancels)| filled || cancels)
}

/// [`surface_gets_escape`]'s `plain` tuple for a field whose Escape follows `escape`: `Blur` always
/// acts, `Pass` never does, so the key reaches `on_key` and `on_escape` with the draft intact.
pub(super) fn plain_escape(escape: Escape, text: bool, composing: bool, cancels: bool) -> (bool, bool, bool) {
    match escape {
        Escape::Clear => (text, composing, cancels),
        Escape::Blur => (true, composing, cancels),
        Escape::Pass => (false, composing, false),
    }
}

/// `on_escape` of the first popup in `scope` (focused surface first, then popups deepest-first) that
/// declares it, else the focused surface's. Siblings resolve in tracked order.
pub(super) fn escape_handler<'a>(trees: &[(&'a str, &layout::ResolvedNode)]) -> Option<(&'a str, Function)> {
    trees.iter().skip(1).chain(trees.first()).find_map(|(id, tree)| {
        crate::layout::node::fields::closable::on_escape.read(&tree.properties).ok().flatten().map(|f| (*id, f))
    })
}

/// A button holding focus blocks typing into the retained password, not Escape.
pub(super) fn secure_key_reaches_field(
    control: Option<&FocusedControl>,
    field: Option<&FocusedField>,
    clears: bool,
) -> bool {
    field.is_some() && (clears || secure_focus_accepts_keys(control, field))
}

fn retained_typing_control(
    field: Option<&FocusedTextField>,
    scope: &[String],
    live: bool,
    root: Option<&layout::ResolvedNode>,
) -> Option<FocusedControl> {
    let field = field.filter(|field| field.typing && live && scope.contains(&field.surface_id))?;
    let (node, _) = find(root?, field.id)?;
    matches!(focused_field(&[node]), Some(FieldTarget::Plain { id, .. }) if id == field.id).then(|| FocusedControl {
        surface_id: field.surface_id.clone(),
        id: field.id,
        kind: ControlKind::Plain,
    })
}

/// The node the engine outlines: control focus that Tab or an AT action moved, never focus from a
/// press, `autofocus` or `focus_target(name)`.
fn outline(
    focused: Option<&FocusedControl>,
    visible: bool,
    surface_id: &str,
    in_scope: bool,
) -> Option<layout::scene::NodeId> {
    focused.filter(|focused| visible && in_scope && focused.surface_id == surface_id).map(|focused| focused.id)
}

pub(super) fn should_arm_autofocus(
    secure_armed: bool,
    typing_restored: bool,
    control: Option<&FocusedControl>,
) -> bool {
    !secure_armed && !typing_restored && control.is_none()
}

impl App {
    pub(in crate::wayland) fn restore_typing_control_on_enter(&mut self, scope: &[String]) -> bool {
        let held = self.focused_text_field.as_ref();
        let live = held.is_some_and(|field| self.surface_is_live(&field.surface_id));
        let root = held.and_then(|field| self.client.scene().surface(&field.surface_id));
        let Some(control) = retained_typing_control(held, scope, live, root) else { return false };
        self.set_control_focus(Some(control));
        true
    }

    pub(in crate::wayland) fn outline_control(&self, surface_id: &str) -> Option<layout::scene::NodeId> {
        let in_scope = self.keyboard_focus_scope().iter().any(|scoped| scoped == surface_id);
        outline(self.focused_control.as_ref(), self.focus_visible, surface_id, in_scope)
    }

    pub(in crate::wayland::input) fn secure_field_takes_keys(&self) -> bool {
        secure_focus_accepts_keys(self.focused_control.as_ref(), self.focused_secure_submit.as_ref())
    }
    fn scoped_controls(&self) -> Vec<Control> {
        let mut result = Vec::new();
        for surface_id in self.keyboard_focus_scope() {
            if let Some(tree) = self.client.scene().surface(&surface_id) {
                controls(&surface_id, tree, &mut result);
            }
        }
        result
    }

    /// Runs `inject`, then puts back any focus it moved off another surface: an injected event may
    /// focus what it lands on, never take focus from where the real keyboard is.
    pub(in crate::wayland::input) fn keeping_foreign_focus<T>(
        &mut self,
        surface_id: &str,
        inject: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let saved = (self.focused_control.clone(), self.focused_text_field.clone(), self.focused_secure_submit.clone());
        let visible = self.focus_visible;
        let result = inject(self);
        let taken = |saved: Option<&str>, now: Option<&str>| focus_taken(saved, now, surface_id);
        if taken(saved.0.as_ref().map(|c| &*c.surface_id), self.focused_control.as_ref().map(|c| &*c.surface_id))
            && saved.0 != self.focused_control
        {
            self.set_control_focus(saved.0);
            self.set_focus_visible(visible);
        }
        let text_state = |f: &FocusedTextField| (f.surface_id.clone(), f.id, f.selection, f.typing, f.buffer.len());
        if taken(saved.1.as_ref().map(|f| &*f.surface_id), self.focused_text_field.as_ref().map(|f| &*f.surface_id))
            && saved.1.as_ref().map(text_state) != self.focused_text_field.as_ref().map(text_state)
        {
            self.focus_text_field(saved.1);
        }
        if taken(saved.2.as_ref().map(|f| &*f.surface_id), self.focused_secure_submit.as_ref().map(|f| &*f.surface_id))
            && saved.2 != self.focused_secure_submit
        {
            self.focus_secure_submit(saved.2);
        }
        result
    }

    pub(in crate::wayland) fn prune_control_focus(&mut self) {
        if self
            .focused_control
            .as_ref()
            .is_some_and(|focused| !self.scoped_controls().iter().any(|control| &control.focus == focused))
        {
            self.set_control_focus(None);
        }
    }

    pub(in crate::wayland) fn set_control_focus(&mut self, next: Option<FocusedControl>) {
        if self.focused_control == next {
            return;
        }
        if let Some(old) = self.focused_control.as_ref().map(|old| old.surface_id.clone()) {
            self.mark_field_input_changed(&old);
        }
        if let Some(new) = next.as_ref().map(|new| new.surface_id.clone()) {
            self.mark_field_input_changed(&new);
        }
        self.focused_control = next;
        self.focus_visible = false;
        self.sync_focused();
    }

    /// Repaints only on a flip: Tab onto the sole, already focused control shows the outline.
    pub(in crate::wayland) fn set_focus_visible(&mut self, visible: bool) {
        if self.focus_visible == visible {
            return;
        }
        self.focus_visible = visible;
        if let Some(surface_id) = self.focused_control.as_ref().map(|focus| focus.surface_id.clone()) {
            self.mark_field_input_changed(&surface_id);
        }
        self.sync_focused();
    }

    /// Writes every `focused` and `focus_visible` signal from the current control focus, the latter
    /// from the node the engine outlines; values that did not move dirty nothing.
    pub(in crate::wayland) fn sync_focused(&self) {
        if !crate::lua::signal::any_focused_registered(self.client.lua()) {
            return;
        }
        for surface in &self.surfaces {
            let Some(tree) = self.client.scene().surface(&surface.surface_id) else { continue };
            let focus = self.focused_control.as_ref().filter(|focus| focus.surface_id == surface.surface_id);
            let outlined = self.outline_control(&surface.surface_id);
            let writes = [
                layout::hover::focused_writes(tree, focus.map(|focus| focus.id), |node| {
                    common::focused.read(&node.properties).ok().flatten()
                }),
                layout::hover::focused_writes(tree, outlined, |node| {
                    common::focus_visible.read(&node.properties).ok().flatten()
                }),
            ];
            for (signal, on) in writes.into_iter().flatten() {
                if let Some(handle) = signal.focused_handle() {
                    handle.set_changed(Value::Boolean(on));
                }
            }
        }
    }

    pub(in crate::wayland) fn focus_control(&mut self, next: Option<FocusedControl>) {
        if self.focused_control == next {
            return;
        }
        if let Some(field) = self.focused_text_field.as_mut() {
            field.typing = false;
            field.history.clear();
        }
        self.invalidate_text_input_focus();
        self.mark_focused_text_field_changed();
        self.set_control_focus(next.clone());
        if let Some(next) = next {
            let target = self
                .client
                .scene()
                .surface(&next.surface_id)
                .and_then(|root| find(root, next.id))
                .and_then(|(node, _)| focused_field(&[node]));
            if let Some(target @ FieldTarget::Plain { .. }) = target {
                self.focus_text_field(Some(plain::requested_focus(
                    next.surface_id.clone(),
                    target,
                    self.focused_text_field.as_ref(),
                )));
            }
        }
    }

    fn activate_control(&mut self, focus: &FocusedControl) {
        let Some((node, rect)) = self.client.scene().surface(&focus.surface_id).and_then(|root| find(root, focus.id))
        else {
            return;
        };
        let on_click = pointer::on_click.read(&node.properties).ok().flatten();
        let submit = pointer::submit.read(&node.properties).is_ok_and(|yes| yes);
        if submit {
            self.prune_secure_focus();
            self.finish_secure_submit();
        }
        if let Some(on_click) = on_click {
            // No pointer: report the node's centre, node-local like a click.
            self.fire_on_click(&focus.surface_id, rect, "left", centre(rect), &on_click);
        }
    }

    pub(in crate::wayland) fn accessibility_action(&mut self, surface_id: &str, raw_id: u64, click: bool) {
        let id = layout::scene::NodeId::from_raw(raw_id);
        let mut list = Vec::new();
        if let Some(tree) = self.client.scene().surface(surface_id) {
            controls(surface_id, tree, &mut list);
        }
        let Some(control) = list.into_iter().find(|control| control.focus.id == id) else {
            return;
        };
        // Focus outside the keyboard scope would be pruned before a key could use it, so a bar
        // button there is only clicked, while the keyboard stays where it is.
        if self.keyboard_focus_scope().iter().any(|scoped| scoped == surface_id) {
            if let Some(target) = control.masked.clone() {
                self.focus_secure_submit(Some(FocusedField { surface_id: surface_id.to_owned(), id, target }));
                self.set_control_focus(Some(control.focus));
                self.set_focus_visible(true);
                return;
            }
            if control.focus.kind == ControlKind::Plain {
                self.focus_secure_submit(None);
            }
            self.focus_control(Some(control.focus.clone()));
            self.set_focus_visible(true);
        }
        if click && control.focus.kind == ControlKind::Button {
            // AT-SPI supplies no Wayland serial. An unrelated pointer press in this turn cannot
            // authorize a popup opened by this action.
            self.input_serial = None;
            self.activate_control(&control.focus);
        }
    }

    /// Tab moves control focus; whether it did, else the key goes on to the field or `on_key`.
    pub(in crate::wayland) fn apply_tab_key(&mut self, event: &KeyEvent) -> bool {
        if self.ctrl_held {
            return false;
        }
        let backwards = event.keysym == Keysym::ISO_Left_Tab || (event.keysym == Keysym::Tab && self.shift_held);
        if matches!(event.keysym, Keysym::Tab | Keysym::KP_Tab | Keysym::ISO_Left_Tab) {
            let list = self.scoped_controls();
            let current = self
                .focused_control
                .as_ref()
                .and_then(|focus| list.iter().position(|control| &control.focus == focus))
                .or_else(|| {
                    self.focused_secure_submit.as_ref().and_then(|field| {
                        list.iter().position(|control| {
                            control.focus.surface_id == field.surface_id
                                && control.focus.id == field.id
                                && control.masked.as_ref() == Some(&field.target)
                        })
                    })
                });
            let moved = dispatch_tab(&list, current.map(|index| &list[index].focus), backwards, |next| {
                if let Some(target) = next.masked {
                    self.focus_secure_submit(Some(FocusedField {
                        surface_id: next.focus.surface_id.clone(),
                        id: next.focus.id,
                        target,
                    }));
                    self.set_control_focus(Some(next.focus));
                } else {
                    if next.focus.kind == ControlKind::Plain {
                        self.focus_secure_submit(None);
                    }
                    self.focus_control(Some(next.focus));
                }
            });
            if moved {
                self.set_focus_visible(true);
            }
            return moved;
        }
        false
    }

    /// Enter or Space on a focused button.
    pub(in crate::wayland) fn apply_activation(&mut self, event: &KeyEvent, repeat: bool, serial: Option<u32>) -> bool {
        if self.ctrl_held {
            return false;
        }
        let Some(focused) = self.focused_control.clone() else { return false };
        let eligible = focused.kind == ControlKind::Button && self.keyboard_focus_scope().contains(&focused.surface_id);
        if !eligible {
            return false;
        }
        dispatch_activation(&focused, event.keysym, repeat, serial, |armed| {
            if let Some(armed) = armed {
                self.input_serial = Some(armed);
            }
            self.activate_control(&focused);
        })
    }
}

/// Whether an injection aimed at `injected` moved focus off another surface: the saved focus sat
/// elsewhere and what is held now is not something the injection itself focused.
fn focus_taken(saved: Option<&str>, now: Option<&str>, injected: &str) -> bool {
    saved.is_some_and(|saved| saved != injected) && now != Some(injected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_injection_may_focus_its_own_surface_but_never_take_focus_from_another() {
        assert!(focus_taken(Some("a"), None, "b"), "pruned away");
        assert!(focus_taken(Some("a"), Some("a"), "b"), "same surface, state may have changed");
        assert!(!focus_taken(Some("a"), Some("b"), "b"), "the press focused a field of its own");
        assert!(!focus_taken(Some("b"), None, "b"), "its own surface's focus is the injection's to change");
        assert!(!focus_taken(None, Some("b"), "b"));
    }
    use crate::wayland::input::keyboard::tests::{plain_textfield, secure_submit_table, textfield};
    use crate::wayland::input::tests::hit_node;

    fn named_button(lua: &Lua, name: &str) -> layout::ResolvedNode {
        let mut node = hit_node(lua, "rect", (0.0, 0.0, 30.0, 20.0), true);
        std::rc::Rc::make_mut(&mut node.properties)
            .insert("accessible_name", Value::String(lua.create_string(name).unwrap()));
        node
    }

    #[test]
    fn escape_goes_to_the_innermost_popup_declaring_on_escape_else_the_surface() {
        let lua = Lua::new();
        let with = |declares: bool| {
            let mut node = hit_node(&lua, "panel", (0.0, 0.0, 10.0, 10.0), false);
            if declares {
                let f = lua.create_function(|_, ()| Ok(())).unwrap();
                std::rc::Rc::make_mut(&mut node.properties).insert("on_escape", Value::Function(f));
            }
            node
        };
        let (bar, inner, outer, tip) = (with(true), with(true), with(true), with(false));
        fn id<'a>(trees: &[(&'a str, &layout::ResolvedNode)]) -> Option<&'a str> {
            escape_handler(trees).map(|(id, _)| id)
        }
        // `shown_popups_under` lists a popup's own popups before it.
        assert_eq!(id(&[("bar", &bar), ("inner", &inner), ("outer", &outer)]), Some("inner"));
        assert_eq!(id(&[("bar", &bar), ("inner", &tip), ("outer", &outer)]), Some("outer"));
        assert_eq!(id(&[("bar", &bar), ("tip", &tip)]), Some("bar"));
        assert!(id(&[("tip", &tip)]).is_none());
        assert!(id(&[]).is_none(), "no keyboard focus, no scope");
    }

    #[test]
    fn a_field_keeps_escape_only_while_it_has_something_to_do() {
        // (repeat, ctrl, plain, secure, reaches the surface)
        let cases = [
            (false, false, None, None, true),
            (true, false, None, None, false),
            (false, true, None, None, false),
            (false, false, Some((true, false, false)), None, false),
            (false, false, Some((false, true, false)), None, false),
            (false, false, Some((false, false, true)), None, false),
            (false, false, Some((false, false, false)), None, true),
            (false, false, None, Some((true, false)), false),
            (false, false, None, Some((false, true)), false),
            (false, false, None, Some((false, false)), true),
        ];
        for (repeat, ctrl, plain, secure, want) in cases {
            assert_eq!(surface_gets_escape(repeat, ctrl, plain, secure), want, "{repeat} {ctrl} {plain:?} {secure:?}");
        }
    }

    #[test]
    fn keyboard_activation_reports_the_node_centre() {
        let rect = LogicalRect { x: 10.0, y: 20.0, width: 30.0, height: 8.0 };
        assert_eq!(centre(rect), layout::hit::LogicalPoint { x: 15.0, y: 4.0 });
    }

    #[test]
    fn tab_wraps_in_both_directions_and_starts_at_the_requested_edge() {
        assert_eq!(next_index(3, None, false), 0);
        assert_eq!(next_index(3, None, true), 2);
        assert_eq!(next_index(3, Some(2), false), 0);
        assert_eq!(next_index(3, Some(0), true), 2);
    }

    #[test]
    fn document_order_skips_hidden_leaving_unnamed_and_zero_sized_controls() {
        let lua = Lua::new();
        let button = |name: Option<&str>| {
            let mut node = hit_node(&lua, "rect", (0.0, 0.0, 30.0, 20.0), true);
            if let Some(name) = name {
                std::rc::Rc::make_mut(&mut node.properties)
                    .insert("accessible_name", Value::String(lua.create_string(name).unwrap()));
            }
            node
        };
        let a = button(Some("A"));
        let mut hidden = button(Some("hidden"));
        hidden.visible = false;
        let mut leaving = button(Some("leaving"));
        leaving.leaving = true;
        let unnamed = button(None);
        let mut zero = button(Some("zero"));
        zero.rect.width = 0.0;
        let b = button(Some("B"));
        let ids = [a.id, b.id];
        let mut root = hit_node(&lua, "column", (0.0, 0.0, 200.0, 40.0), false);
        root.children = vec![a, hidden, leaving, unnamed, zero, b];
        let mut found = Vec::new();
        controls("panel@TEST", &root, &mut found);
        assert_eq!(found.iter().map(|control| control.focus.id).collect::<Vec<_>>(), ids);
    }

    #[test]
    fn tab_skips_a_disabled_field() {
        let lua = Lua::new();
        let off = crate::wayland::input::keyboard::tests::with_property(
            plain_textfield(&lua),
            "disabled",
            mlua::Value::Boolean(true),
        );
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 30.0), false);
        root.children = vec![off, plain_textfield(&lua)];
        let mut list = Vec::new();
        controls("panel@TEST", &root, &mut list);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn tab_focuses_a_lone_button_and_cycles_across_fields_and_buttons() {
        let lua = Lua::new();
        let button = named_button(&lua, "Open");
        let mut list = Vec::new();
        controls("panel@TEST", &button, &mut list);
        let mut focused = None;
        assert!(dispatch_tab(&list, None, false, |control| focused = Some(control.focus)));
        assert_eq!(focused.as_ref().map(|focus| focus.id), Some(button.id));

        let field = plain_textfield(&lua);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 30.0), false);
        root.children = vec![field, button];
        list.clear();
        controls("panel@TEST", &root, &mut list);
        focused = Some(list[0].focus.clone());
        let current = focused.clone();
        assert!(dispatch_tab(&list, current.as_ref(), false, |control| focused = Some(control.focus)));
        assert_eq!(focused, Some(list[1].focus.clone()));
        let current = focused.clone();
        assert!(dispatch_tab(&list, current.as_ref(), false, |control| focused = Some(control.focus)));
        assert_eq!(focused, Some(list[0].focus.clone()), "Tab wraps to the first field");
        let current = focused.clone();
        assert!(dispatch_tab(&list, current.as_ref(), true, |control| focused = Some(control.focus)));
        assert_eq!(focused, Some(list[1].focus.clone()), "Shift+Tab wraps backward");

        list.truncate(1);
        assert!(!dispatch_tab(&list, None, false, |_| panic!("one plain field keeps Tab")));
    }

    #[test]
    fn only_keyboard_moved_focus_is_outlined() {
        let focus = FocusedControl {
            surface_id: "panel@TEST".into(),
            id: layout::scene::NodeId::test(3),
            kind: ControlKind::Plain,
        };
        assert_eq!(outline(Some(&focus), false, "panel@TEST", true), None, "press, autofocus or focus_target(name)");
        assert_eq!(outline(Some(&focus), true, "panel@TEST", true), Some(focus.id), "after Tab or an AT action");
        assert_eq!(outline(Some(&focus), true, "popup@TEST", true), None);
        assert_eq!(outline(Some(&focus), true, "panel@TEST", false), None, "outside the keyboard scope");
    }

    #[test]
    fn reenter_restores_control_focus_from_a_retained_typing_draft() {
        let lua = Lua::new();
        let field = plain_textfield(&lua);
        let id = field.id;
        let target = focused_field(&[&field]).unwrap();
        let mut held = super::plain::requested_focus("panel@TEST".into(), target, None);
        held.buffer = "draft".into();
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 30.0), false);
        root.children.push(field);
        let scope = vec!["panel@TEST".to_string()];

        assert_eq!(
            retained_typing_control(Some(&held), &scope, true, Some(&root)),
            Some(FocusedControl { surface_id: "panel@TEST".into(), id, kind: ControlKind::Plain })
        );
        assert_eq!(held.buffer, "draft", "reentry keeps the draft");
        let button = FocusedControl {
            surface_id: "panel@TEST".into(),
            id: layout::scene::NodeId::test(999),
            kind: ControlKind::Button,
        };
        assert!(!should_arm_autofocus(false, false, Some(&button)), "Enter after a button press keeps its focus");
        assert!(!should_arm_autofocus(false, true, None), "the restored draft keeps focus");
        assert!(should_arm_autofocus(false, false, None), "an empty scope admits autofocus");
        assert!(retained_typing_control(Some(&held), &[], true, Some(&root)).is_none());
        assert!(retained_typing_control(Some(&held), &scope, false, Some(&root)).is_none());
        root.children[0].visible = false;
        assert!(retained_typing_control(Some(&held), &scope, true, Some(&root)).is_none());
    }

    #[test]
    fn enter_and_space_activate_once_per_press() {
        let lua = Lua::new();
        let seen = std::rc::Rc::new(std::cell::Cell::new(0));
        let sink = seen.clone();
        let on_click = lua
            .create_function(move |_, (_rect, button): (mlua::Table, String)| {
                assert_eq!(button, "left");
                sink.set(sink.get() + 1);
                Ok(())
            })
            .unwrap();
        let focused = FocusedControl {
            surface_id: "popup@TEST".into(),
            id: layout::scene::NodeId::test(7),
            kind: ControlKind::Button,
        };
        let rect = LogicalRect { x: 0.0, y: 0.0, width: 30.0, height: 20.0 };
        let mut armed = None;
        for key in [Keysym::Return, Keysym::KP_Enter, Keysym::space] {
            assert!(dispatch_activation(&focused, key, false, Some(41), |serial| {
                armed = serial;
                super::super::super::pointer::call_on_click(
                    &lua,
                    &on_click,
                    rect,
                    "left",
                    layout::hit::LogicalPoint { x: 15.0, y: 10.0 },
                )
                .unwrap();
            }));
            assert!(dispatch_activation(&focused, key, true, Some(41), |_| panic!("repeat activated")));
        }
        assert_eq!(seen.get(), 3);
        assert_eq!(
            armed,
            Some(super::super::super::pointer::ArmedSerial { serial: 41, instance_id: "popup@TEST".into() })
        );
        assert!(!dispatch_activation(&focused, Keysym::Escape, false, Some(41), |_| panic!("Escape activated")));
        assert!(dispatch_activation(&focused, Keysym::space, false, None, |serial| assert_eq!(serial, None)));
    }

    #[test]
    fn button_focus_blocks_secure_edits_but_keeps_the_submit_buffer() {
        let lua = Lua::new();
        let secure = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let second = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let button = named_button(&lua, "Unlock");
        let field = FocusedField {
            surface_id: "lock@TEST".into(),
            id: secure.id,
            target: node::SecureSubmitTarget { capability: "lock".into(), action: "authenticate".into(), name: None },
        };
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 30.0), false);
        let mut root = root;
        root.children = vec![secure, second, button];
        let field_control =
            FocusedControl { surface_id: field.surface_id.clone(), id: root.children[0].id, kind: ControlKind::Masked };
        let second_control =
            FocusedControl { surface_id: field.surface_id.clone(), id: root.children[1].id, kind: ControlKind::Masked };
        let button_control =
            FocusedControl { surface_id: field.surface_id.clone(), id: root.children[2].id, kind: ControlKind::Button };
        assert!(secure_focus_accepts_keys(Some(&field_control), Some(&field)));
        assert!(!secure_focus_accepts_keys(Some(&second_control), Some(&field)));
        let second_field = FocusedField { id: second_control.id, ..field.clone() };
        assert!(secure_focus_accepts_keys(Some(&second_control), Some(&second_field)));
        assert!(!secure_focus_accepts_keys(Some(&button_control), Some(&field)));
        assert!(!secure_key_reaches_field(Some(&button_control), Some(&field), false));
        assert!(secure_key_reaches_field(Some(&button_control), Some(&field), true), "Escape still cancels");
        assert!(!secure_key_reaches_field(Some(&button_control), None, true));
        assert!(secure_focus_accepts_keys(None, Some(&field)), "sole secure field auto-arms");
        let mut held = Some(field.clone());
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("secret");
        super::secure::retarget_secure_submit(&mut held, &mut buffer, Some(field));
        assert_eq!(buffer.expose_secret(), b"secret", "button focus keeps the pending submit");
        super::secure::retarget_secure_submit(&mut held, &mut buffer, Some(second_field));
        assert!(buffer.is_empty(), "a second node with the same target cannot inherit the first password");
    }
}

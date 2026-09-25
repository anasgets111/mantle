//! `secure_submit` fields: which one is armed, the keystrokes that fill its `SecureBuffer`, and
//! the `SecureSubmit` frame they become without a Lua value holding plaintext (ADR-0005/0027).

use shared::{debug, error, warn};

use super::*;
use crate::layout::secure_submit::{sole_secure_submit_in_scope, typable_secure_submit_targets};

/// `on_cancel` of the reachable field addressing `target`. Escape reaches Lua; the secret never does
/// (ADR-0005).
fn secure_on_cancel(tree: &layout::ResolvedNode, target: &node::SecureSubmitTarget) -> Option<Function> {
    let mut stack = vec![tree];
    while let Some(node) = stack.pop() {
        if !node.visible || node.leaving {
            continue;
        }
        if let Some(node::PaintStyle::TextField { target: Some(found), .. }) = &node.paint
            && found == target
        {
            return match node.properties.get("on_cancel") {
                Some(Value::Function(f)) => Some(f.clone()),
                _ => None,
            };
        }
        stack.extend(node.children.iter().rev());
    }
    None
}

/// A secure submit frame, or `None` without a destination (ADR-0050 decision 4). Do not send to
/// `"unknown"/"unknown"`; the buffer is zeroized on every branch.
///
/// Every refusal says so. Dropping a submit here is indistinguishable from a lock screen that has
/// stopped accepting the password: nothing reaches the Supervisor, so nothing downstream can
/// report it, and a session that will not unlock leaves no line anywhere. No branch names the
/// secret or its length.
fn submit_frame_for(
    generation_id: u32,
    target: Option<&node::SecureSubmitTarget>,
    buffer: &mut shared::SecureBuffer,
) -> Option<RendererFrame> {
    // Empty joins an open network (ADR-0029); anywhere else it is no password, and to lock or
    // polkit it would spend a PAM attempt and the `pam_unix` failure delay.
    let empty = buffer.is_empty()
        && target.is_none_or(|target| (&*target.capability, &*target.action) != ("network", "connect"));
    let Some(target) = target.filter(|_| !empty) else {
        match target {
            None => debug!(
                2; "secure submit dropped: no field is focused to send it to, so a password typed here reaches nothing"
            ),
            Some(target) => {
                debug!(2; "secure submit to {}/{} dropped: the field is empty", target.capability, target.action)
            }
        }
        buffer.zeroize();
        return None;
    };
    let Some(capability) = shared::Capability::from_name(&target.capability) else {
        warn!("secure submit to {}/{} dropped: no such capability", target.capability, target.action);
        buffer.zeroize();
        return None;
    };
    Some(secure_submit_frame(generation_id, capability, &target.action, buffer))
}

/// Sole writer of `focused_secure_submit`: `SecureBuffer` belongs to the field typed into, not its
/// transport. The three transitions (`KeyboardHandler::leave`, keyboard capability removal, and
/// pointer retarget) all scrub; any new caller inherits that guarantee. A destination change
/// scrubs, but re-arming the same field does not (ADR-0050 decision 4). Free for unit-testing the
/// read/zeroize contract without Wayland, as with [`secure_submit_frame`].
fn retarget_secure_submit(
    focused: &mut Option<FocusedField>,
    buffer: &mut shared::SecureBuffer,
    next: Option<FocusedField>,
) {
    if *focused != next {
        buffer.zeroize();
    }
    *focused = next;
}

/// Reconcile secure focus on keyboard enter from the scoped trees. Empty/untracked scopes and
/// scopes without one `secure_submit` return `None` through [`App::focus_secure_submit`], so moving
/// focus cannot leave keys addressed to the old field. Keep a current field only if still declared
/// in scope, and still reachable there: a prompt that hides while focused is as gone as one a
/// reload deleted. The compositor's `enter` commonly follows a press, so discarding current focus
/// would make a multi-field surface untypable by clicking. Otherwise,
/// [`sole_secure_submit_in_scope`] refuses to guess among several fields; reloads cannot keep
/// deleted targets.
fn focus_on_enter(scope: &[(&str, &layout::ResolvedNode)], current: Option<&FocusedField>) -> Option<FocusedField> {
    let still_declared = |field: &&FocusedField| {
        scope
            .iter()
            .any(|(id, tree)| *id == field.surface_id && typable_secure_submit_targets(tree).contains(&field.target))
    };
    if let Some(current) = current.filter(still_declared) {
        return Some(current.clone());
    }
    let (surface_id, target) = sole_secure_submit_in_scope(scope)?;
    Some(FocusedField { surface_id: surface_id.to_string(), target })
}

/// A field is armed only if its surface is in the current key scope and still has a live
/// `wl_surface`. Both clauses are required: defect 2 left a field on a `keyboard_interactivity =
/// none` panel armed, and defect 3 left a destroyed lock-screen field armed because no `leave` was
/// guaranteed. Use the same parent-plus-popup `scope` as [`sole_secure_submit_in_scope`], or enter
/// could arm a field the next key prunes.
fn focus_is_still_armed(field: &FocusedField, scope: &[String], its_surface_is_live: bool) -> bool {
    scope.contains(&field.surface_id) && its_surface_is_live
}

/// Build a `RendererFrame::SecureSubmit` (ADR-0005/ADR-0027). Read once with `expose_secret`, then
/// zeroize before the frame leaves this thread; the socket thread scrubs its copy after its wire
/// write in `crate::socket::pump`. Kept free for unit-testing without a live `wl_surface`.
fn secure_submit_frame(
    generation_id: u32,
    capability: shared::Capability,
    action: &str,
    buffer: &mut shared::SecureBuffer,
) -> RendererFrame {
    let frame = RendererFrame::SecureSubmit(SecureSubmit {
        generation_id,
        capability,
        action: action.to_string(),
        secret: buffer.expose_secret().to_vec(),
    });
    buffer.zeroize();
    frame
}

impl App {
    /// Every write to `focused_secure_submit` in this file, funnelled so [`retarget_secure_submit`]
    /// enforces the buffer's lifetime; assigning the field directly anywhere else reopens the leak
    /// that function closes.
    pub(in crate::wayland) fn focus_secure_submit(&mut self, next: Option<FocusedField>) {
        self.mark_focused_secure_submit_changed();
        if let Some(ref next_field) = next {
            self.mark_field_input_changed(&next_field.surface_id);
        }
        retarget_secure_submit(&mut self.focused_secure_submit, &mut self.secure_buffer, next);
    }

    /// Ask [`focus_on_enter`] over scoped trees; called both on `enter` and when trees change under
    /// an existing focus.
    pub(super) fn field_the_scope_declares(
        &self,
        scope: &[String],
        current: Option<FocusedField>,
    ) -> Option<FocusedField> {
        let trees = self.scoped_trees(scope);
        focus_on_enter(&trees, current.as_ref())
    }

    /// Arm a newly visible sole `secure_submit` when the tree changes under existing focus. Enter
    /// alone misses the network prompt: the bar receives its one `enter` when the panel opens, then
    /// a click sets `network.password_ssid` and reveals the field without moving focus. Changing
    /// layer `keyboard_interactivity` instead breaks the popup grab; niri dismissed that popup in
    /// the same frame the field armed. Only arm when empty; [`sole_secure_submit_in_scope`] refuses
    /// to guess among several fields (ADR-0050 decision 4).
    pub(in crate::wayland) fn arm_secure_focus_if_the_scope_now_declares_one(&mut self, scope: &[String]) {
        if self.focused_secure_submit.is_some() || self.keyboard_focus.is_none() {
            return;
        }
        let Some(field) = self.field_the_scope_declares(scope, None) else {
            return;
        };
        // Destroyed surfaces may retain `keyboard_focus` without a `leave`; arming would scrub and
        // re-arm on every pass.
        if !self.surface_is_live(&field.surface_id) {
            return;
        }
        debug!(
            "{}'s `secure_submit` field ({}/{}) became typable under the keyboard focus already held",
            field.surface_id, field.target.capability, field.target.action
        );
        self.focus_secure_submit(Some(field));
    }

    /// Drop stale secure focus and scrub its half-typed secret before every keystroke; all
    /// transitions use [`App::focus_secure_submit`], even when no `leave` followed.
    pub(in crate::wayland::input) fn prune_secure_focus(&mut self) {
        let Some(field) = self.focused_secure_submit.as_ref() else {
            return;
        };
        if focus_is_still_armed(field, &self.keyboard_focus_scope(), self.surface_is_live(&field.surface_id)) {
            return;
        }
        debug!(
            2; "the focused secure_submit field is no longer the one receiving keys; dropping it and scrubbing its buffer"
        );
        self.focus_secure_submit(None);
    }

    /// Poll-turn cleanup for a destroyed surface. Only liveness is checked here: checking routing
    /// would disarm a multi-field press before its matching `enter`, which `sole_secure_submit`
    /// cannot re-choose. This bounds plaintext residency when lock teardown gets no `leave`.
    pub(in crate::wayland) fn drop_secure_focus_if_its_surface_is_gone(&mut self) {
        let gone = self.focused_secure_submit.as_ref().is_some_and(|field| !self.surface_is_live(&field.surface_id));
        if gone {
            debug!(2; "the surface holding the focused secure_submit field is gone; dropping it and scrubbing its buffer");
            self.focus_secure_submit(None);
        }
    }

    /// Apply one secure key (ADR-0005). Focus is the destination gate; masked fields without one
    /// are never focused. Bytes go `KeyEvent` → native `SecureBuffer` → Supervisor, never Lua.
    pub(super) fn apply_secure_key(&mut self, event: &KeyEvent, repeat: bool) {
        let action = key_action(event, repeat, self.ctrl_held);
        if self.focused_secure_submit.is_none() {
            // Enter with nothing focused is the shape a stuck lock screen takes: the keys went
            // nowhere, `finish_secure_submit` is never reached, and every refusal log lives below
            // this return. Only the submit key says so, or an unfocused keyboard would log per
            // keystroke.
            //
            // A plain field taking keys is not that shape: both focuses see every key, so Enter in
            // a notification reply or a search box arrives here with an `on_submit` waiting for it.
            if matches!(action, KeyAction::Submit)
                && !self.focused_text_field.as_ref().is_some_and(|field| self.text_field_takes_keys(field))
            {
                debug!(
                    2; "submit pressed while no secure field holds focus; nothing was typed into one and nothing was sent"
                );
            }
            return;
        }
        // Append/backspace/clear all change the drawn character count.
        self.mark_focused_secure_submit_changed();
        match action {
            KeyAction::Append(text) => self.secure_buffer.push_str(text),
            // `pop_grapheme` zeroizes dropped bytes, not just the length. Only the backwards one:
            // every other reach needs a caret, and a secret holds none (ADR-0064).
            KeyAction::Erase(Motion::Left) => {
                self.secure_buffer.pop_grapheme();
            }
            KeyAction::Erase(_) => {}
            // Scrub through the transition seam, then re-arm: `on_cancel` may keep the prompt open.
            KeyAction::Clear => {
                let cleared = !self.secure_buffer.is_empty();
                let field = self.focused_secure_submit.clone();
                self.focus_secure_submit(None);
                self.focus_secure_submit(field.clone());
                if let Some(field) = field
                    && let Some(on_cancel) = self
                        .client
                        .scene()
                        .surface(&field.surface_id)
                        .and_then(|tree| secure_on_cancel(tree, &field.target))
                    && let Err(e) = on_cancel.call::<()>(cleared)
                {
                    warn!("{}: on_cancel raised, ignoring it: {}", field.surface_id, crate::lua::describe(&e));
                }
            }
            KeyAction::Submit => self.finish_secure_submit(),
            // Password prompts have no navigation, and a masked field has no caret to move: a
            // position in a secret is a position the tree must never hold (ADR-0064).
            KeyAction::Navigate(_) | KeyAction::Move(_) | KeyAction::SelectAll | KeyAction::Ignore => {}
        }
    }

    /// Build and queue a completed `secure_submit`; [`submit_frame_for`] reads once and scrubs on
    /// both the destination-missing and empty-buffer paths (ADR-0050 decision 4).
    pub(in crate::wayland::input) fn finish_secure_submit(&mut self) {
        // Which of the two, not "one of these". A lock screen that will not open needs the log to
        // separate "the password went nowhere" from "you submitted an empty field", and the old
        // line named both and settled neither.
        let addressed = self.focused_secure_submit.as_ref().map(|field| field.target.clone());
        let nothing_typed = self.secure_buffer.is_empty();
        let target = self.focused_secure_submit.as_ref().map(|field| &field.target);
        let Some(frame) = submit_frame_for(self.generation_id, target, &mut self.secure_buffer) else {
            match addressed {
                None => debug!(
                    2; "secure_submit dropped: no focused textfield named a capability and action to address it to, so nothing was sent"
                ),
                Some(target) if nothing_typed => {
                    debug!(2; "secure_submit to {}/{} dropped: nothing had been typed", target.capability, target.action)
                }
                Some(target) => error!(
                    "secure_submit to {}/{} dropped for no recorded reason; this is a bug",
                    target.capability, target.action
                ),
            }
            return;
        };
        if let Err(e) = self.outbound_tx.send(frame) {
            error!("failed to queue SecureSubmit for the socket thread: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::hit_node;
    use super::super::tests::{field, secure_submit_table, target, textfield, tree_with};
    use super::*;

    #[test]
    fn secure_submit_frame_carries_the_accumulated_secret_and_zeroizes_the_buffer_it_read() {
        // ADR-0005, ADR-0027: the frame carries the exact secret this thread accumulated, tagged
        // with this process's own generation_id, and the source buffer is scrubbed in the same
        // breath as the read rather than left live.
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        let frame = secure_submit_frame(4, shared::Capability::Polkit, "authenticate", &mut buffer);

        assert_eq!(
            frame,
            RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: shared::Capability::Polkit,
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            })
        );
        assert!(buffer.is_empty(), "the source SecureBuffer must be zeroized as soon as it has been read");
    }

    #[test]
    fn a_secure_fields_on_cancel_is_found_by_its_destination_while_reachable() {
        let lua = Lua::new();
        let polkit = node::SecureSubmitTarget { capability: "polkit".to_string(), action: "authenticate".to_string() };
        let mut field = textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")));
        let on_cancel = lua.create_function(|_, _cleared: bool| Ok(())).unwrap();
        field.properties.insert("on_cancel", Value::Function(on_cancel));
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(field);
        assert!(secure_on_cancel(&root, &polkit).is_some());
        let other = node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string() };
        assert!(secure_on_cancel(&root, &other).is_none(), "another destination's field");
        root.children[0].visible = false;
        assert!(secure_on_cancel(&root, &polkit).is_none(), "a hidden prompt was not the one dismissed");
    }

    #[test]
    fn moving_focus_between_two_secure_submit_fields_zeroizes_what_the_first_accumulated() {
        // The credential leak this seam closes: the lock screen's `("lock", "authenticate")` field
        // accumulates a login password, focus moves to the bar's `("network", "connect")` field
        // without an Enter in between, and the next submit used to carry `<login password><psk>`
        // to the network capability.
        let mut focused = Some(field("screen@DP-1", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, Some(field("bar@DP-1", "network", "connect")));

        assert_eq!(focused, Some(field("bar@DP-1", "network", "connect")));
        assert!(buffer.is_empty(), "a password typed for one destination must not reach the next one's capability");
    }

    #[test]
    fn the_same_destination_on_a_different_surface_is_a_different_field() {
        // The surface half of the identity is load-bearing: two surfaces may both declare
        // `("lock", "authenticate")` -- a lock screen on each of two monitors does. With the
        // destination alone as the identity, focus moving between them compared equal and the
        // scrub was skipped.
        let mut focused = Some(field("screen@eDP-1", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, Some(field("screen@DP-1", "lock", "authenticate")));

        assert!(buffer.is_empty(), "a field is its surface as well as its destination");
    }

    #[test]
    fn clearing_focus_zeroizes_the_buffer_and_re_focusing_the_same_field_does_not() {
        // Two halves of the same rule. Clearing is `leave`/`capability_lost`, where no submit is
        // ever coming for the bytes. Re-arming the same destination is a press landing in the field
        // already being typed into (ADR-0050 decision 4), and wiping there would delete half
        // a password mid-entry.
        let mut focused = Some(field("screen@TEST", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, None);
        assert_eq!(focused, None);
        assert!(buffer.is_empty(), "focus leaving with no submit must scrub what it accumulated");

        focused = Some(field("screen@TEST", "lock", "authenticate"));
        buffer.push_str("hunter2");
        retarget_secure_submit(&mut focused, &mut buffer, Some(field("screen@TEST", "lock", "authenticate")));
        assert_eq!(buffer.expose_secret(), b"hunter2", "re-focusing the same field must not eat the entry in progress");
    }

    #[test]
    fn a_submit_with_a_focused_target_is_addressed_to_that_capability_and_action() {
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");
        let target = node::SecureSubmitTarget { capability: "polkit".to_string(), action: "authenticate".to_string() };

        let frame = submit_frame_for(4, Some(&target), &mut buffer);

        assert_eq!(
            frame,
            Some(RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: shared::Capability::Polkit,
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            }))
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_submit_with_no_focused_target_sends_nothing_and_still_zeroizes_the_buffer() {
        // ADR-0050 decision 4: addressing this to `"unknown"/"unknown"` would put a password
        // on the wire for no one. The scrub is the half that is not optional.
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        assert_eq!(submit_frame_for(4, None, &mut buffer), None);
        assert!(buffer.is_empty(), "a dropped submit must still leave the accumulated secret scrubbed");
    }

    #[test]
    fn an_enter_on_an_empty_field_sends_nothing() {
        // Not free: the Supervisor routes a `("lock", "authenticate")` submit straight into PAM, so
        // an Enter that said nothing spends one of the user's counted attempts.
        let mut buffer = shared::SecureBuffer::new();
        assert_eq!(submit_frame_for(4, Some(&target("lock", "authenticate")), &mut buffer), None);
    }

    #[test]
    fn an_enter_on_an_empty_network_prompt_joins_it_as_open() {
        // ADR-0029: a hidden network's security is unknown, so it prompts; Enter with nothing typed
        // is how an open one is joined.
        let mut buffer = shared::SecureBuffer::new();
        assert_eq!(
            submit_frame_for(4, Some(&target("network", "connect")), &mut buffer),
            Some(RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: shared::Capability::Network,
                action: "connect".to_string(),
                secret: Vec::new(),
            }))
        );
    }

    #[test]
    fn keyboard_focus_arriving_on_nothing_typable_disarms_whatever_was_armed() {
        // Both of `KeyboardHandler::enter`'s "nothing to arm" cases, once early returns that left
        // the previous surface's field armed while keystrokes kept accumulating into it. `enter`
        // now pushes this answer through `App::focus_secure_submit` whatever it is.
        let lua = Lua::new();
        let untypable = tree_with(&lua, vec![textfield(&lua, None)]);
        let armed = field("screen@TEST", "lock", "authenticate");
        assert_eq!(focus_on_enter(&[], Some(&armed)), None, "an `enter` on a surface this process already destroyed");
        assert_eq!(
            focus_on_enter(&[("bar@TEST", &untypable)], Some(&armed)),
            None,
            "a surface whose tree names no destination"
        );

        let typable = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert_eq!(focus_on_enter(&[("screen@TEST", &typable)], None), Some(armed.clone()));

        // What an `enter` must *not* undo: a press on a surface declaring two fields picked one the
        // sole-field rule refuses to pick, and the compositor's `enter` for that surface commonly
        // follows the press that caused it.
        let two_fields = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        let pressed = field("screen@TEST", "polkit", "authenticate");
        assert_eq!(focus_on_enter(&[("screen@TEST", &two_fields)], Some(&pressed)), Some(pressed));
        assert_eq!(
            focus_on_enter(&[("screen@TEST", &two_fields)], Some(&field("bar@TEST", "network", "connect"))),
            None,
            "a field belonging to no surface in scope is not this scope's to keep"
        );
    }

    #[test]
    fn keyboard_focus_on_a_panel_takes_the_field_on_the_popup_shown_under_it() {
        // A config raises the parent's `keyboard_interactivity` from a click inside a popup that is
        // already open, and niri only hands a popup the keyboard if its parent held it when the
        // popup mapped. So the keys arrive on the parent while the only field in reach is on the
        // popup. Scoped to the entering surface alone this armed nothing.
        let lua = Lua::new();
        let bar = tree_with(&lua, vec![]);
        let panel = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "network", "connect")))]);

        assert_eq!(
            focus_on_enter(&[("bar@TEST", &bar), ("panel@TEST", &panel)], None),
            Some(field("panel@TEST", "network", "connect")),
            "the field is armed on the surface that declares it, not on the one holding the keyboard"
        );

        // And the sole-field rule still spans the whole scope rather than each tree separately:
        // one field on the bar and one on its popup is still two destinations to guess between.
        let typable_bar =
            tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert_eq!(focus_on_enter(&[("bar@TEST", &typable_bar), ("panel@TEST", &panel)], None), None);
    }

    #[test]
    fn a_field_is_armed_only_while_its_own_surface_holds_the_keyboard_and_still_exists() {
        // One per-keystroke question replaces clearing calls at five or six teardown sites. The
        // liveness half is the traced leak: type a login password on the lock screen, the
        // compositor sends `finished`, `teardown_lock_surfaces` destroys the `wl_surface` with no
        // `leave` required to follow, so the plaintext used to stay live in `App::secure_buffer`.
        let armed = field("screen@TEST", "lock", "authenticate");
        let scope = |ids: &[&str]| ids.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();
        assert!(focus_is_still_armed(&armed, &scope(&["screen@TEST"]), true));
        assert!(
            !focus_is_still_armed(&armed, &scope(&["screen@TEST"]), false),
            "its `wl_surface` is gone, whether or not a `leave` ever came"
        );
        assert!(!focus_is_still_armed(&armed, &scope(&["bar@TEST"]), true), "another surface is receiving the keys");
        assert!(!focus_is_still_armed(&armed, &[], true), "the keyboard is on a surface this process does not own");
        // The half `keyboard_focus_scope` buys: the keyboard sits on the bar, the field is on the
        // popup shown under it, and a keystroke reaches it. Without this clause every key pruned
        // the focus `enter` had just armed.
        let on_popup = field("panel@TEST", "network", "connect");
        assert!(focus_is_still_armed(&on_popup, &scope(&["bar@TEST", "panel@TEST"]), true));
        assert!(!focus_is_still_armed(&on_popup, &scope(&["bar@TEST"]), true), "the popup is no longer shown");
    }
}

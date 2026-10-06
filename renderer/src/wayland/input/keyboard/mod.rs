//! Keyboard input: which `textfield` takes the keys, plain edits delivered to Lua (ADR-0092), and
//! `secure_submit` keystrokes, which become a `SecureSubmit` frame without a Lua value holding
//! plaintext (ADR-0005/0027).

use shared::debug;

use super::*;
use crate::layout::node::prop::keywords;
use crate::lua::call_logged;

mod focus;
mod on_key;
mod plain;
mod rows;
mod secure;
pub(in crate::wayland) use focus::{ControlKind, FocusedControl, secure_target_at};
pub(crate) use on_key::KeyPress;
pub(in crate::wayland::input) use plain::{EditHistory, Span, Unit, extend_by_unit, unit_range};

keywords! {
    /// What Escape does in a plain `textfield`; a `secure_submit` field always scrubs and stays armed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Escape {
        /// Empty the draft, then leave the field if `on_cancel` is set.
        Clear,
        /// Keep the draft and leave the field.
        Blur,
        /// Keep the draft and the focus; the key goes up through `on_key` and `on_escape`.
        Pass,
    }
}

keywords! {
    /// The chord that submits a `multiline` field; the other Return inserts a newline.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SubmitKey {
        /// Ctrl+Return submits; Return inserts a newline.
        CtrlReturn = "ctrl+return",
        /// Return submits; Shift+Return inserts a newline.
        Return,
    }
}

/// Innermost pressed `textfield` (ADR-0092): masked fields address a capability and never Lua
/// (ADR-0005); plain fields send edits to Lua.
#[derive(Debug)]
pub(super) enum FieldTarget {
    Masked {
        id: layout::scene::NodeId,
        target: node::SecureSubmitTarget,
    },
    Plain {
        /// Node identity lets paint find it across passes that move it (ADR-0099).
        id: layout::scene::NodeId,
        on_change: Option<Function>,
        on_submit: Option<Function>,
        on_cancel: Option<Function>,
        escape: Escape,
    },
}

/// Innermost pressed `textfield` (ADR-0050 decision 4, ADR-0092). A `secure_submit`
/// table is masked and targets the whole capability/action identity, since that is where its bytes
/// go; otherwise callbacks make it plain. `paint_style`
/// parses the destination during `Scene::apply`, so malformed secure targets fail the pass.
/// Masked fields without a destination and plain fields without callbacks return `None` rather
/// than taking a keyboard they cannot use. Plain fields use `ResolvedNode`'s stable `NodeId`
/// (ADR-0099); [`ArmedClick`] still uses a rect because press/release trees rarely move.
pub(super) fn focused_field(path: &[&layout::ResolvedNode]) -> Option<FieldTarget> {
    field_target(path, false)
}

/// [`focused_field`], optionally reading a disabled field too: `set_text` never takes focus.
fn field_target(path: &[&layout::ResolvedNode], disabled_too: bool) -> Option<FieldTarget> {
    let field = path.iter().rev().find(|node| node.kind == "textfield")?;
    let node::PaintStyle::TextField { target, escape, .. } = field.paint.as_ref()? else {
        return None;
    };
    if field.is_disabled_field() && !disabled_too {
        return None;
    }
    if let Some(target) = target {
        return Some(FieldTarget::Masked { id: field.id, target: target.clone() });
    }
    // Refused by `resolve_declared` unless a function, so an error here cannot happen.
    use node::fields::textfield;
    let properties = &field.properties;
    let on_change = textfield::on_change.read(properties).ok().flatten();
    let on_submit = textfield::on_submit.read(properties).ok().flatten();
    if on_change.is_none() && on_submit.is_none() {
        return None;
    }
    Some(FieldTarget::Plain {
        id: field.id,
        on_change,
        on_submit,
        on_cancel: textfield::on_cancel.read(properties).ok().flatten(),
        escape: *escape,
    })
}

/// Focused plain `textfield`, its Lua callbacks, and readable draft (ADR-0092).
/// The `String` outlives keyboard focus while the node exists (ADR-0108); `typing` records whether
/// a press selected it, while `keyboard_focus` controls current keys and caret drawing.
#[derive(Debug, Clone)]
pub(in crate::wayland) struct FocusedTextField {
    pub(in crate::wayland) surface_id: String,
    pub(in crate::wayland) id: layout::scene::NodeId,
    pub(in crate::wayland) buffer: String,
    /// `(anchor, caret)` byte offsets into `buffer`; equal means a bare caret. Held here beside
    /// the draft rather than in the resolved tree, for the reason the draft is (ADR-0236).
    pub(super) selection: (usize, usize),
    pub(in crate::wayland::input) history: EditHistory,
    /// A press selected it; off keeps text without a caret and sends keys nowhere.
    pub(super) typing: bool,
    /// The pointer is down inside it, so motion extends the selection (ADR-0236).
    pub(super) selecting: bool,
    /// What a drag or Shift+press grows by, and the span it keeps selected.
    pub(super) span: Span,
    /// The left press that last selected here, which the next may continue as a double or triple click.
    pub(super) click: Option<(std::time::Instant, (f64, f64))>,
    /// The x a run of Up and Down in a `multiline` field keeps to, field-local.
    pub(super) goal_x: Option<f32>,
    pub(super) on_change: Option<Function>,
    pub(super) on_submit: Option<Function>,
    pub(super) on_cancel: Option<Function>,
    pub(super) escape: Escape,
}

/// Focused `secure_submit` field and declaring surface. The node id distinguishes fields with the
/// same destination; the surface id distinguishes live keyboard focus from a client-destroyed
/// surface, which need not receive `wl_keyboard.leave`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::wayland) struct FocusedField {
    /// `"{id}@{output}"` instance id declaring the field.
    pub(in crate::wayland) surface_id: String,
    pub(in crate::wayland) id: layout::scene::NodeId,
    pub(in crate::wayland) target: node::SecureSubmitTarget,
}

/// One key event's action for a focused `secure_submit`; borrow the SCTK `KeyEvent` text, avoiding
/// another allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyAction<'a> {
    Append(&'a str),
    /// How far one erase reaches, from the caret. A selection outranks it: what is highlighted is
    /// what goes, whichever key asked.
    Erase(Motion),
    /// Caret motion on a plain field; masked fields ignore it (ADR-0064).
    Move(Motion),
    /// Escape clears and stays in the field.
    Clear,
    Submit,
    /// Ctrl+A on a plain field; masked fields ignore it, having no selection to make.
    SelectAll,
    Undo,
    Redo,
    Ignore,
}

/// Where an arrow or Home/End puts the caret.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Start,
    End,
    /// A `multiline` field's visual rows, resolved against its layout into [`Motion::To`].
    Up,
    Down,
    RowStart,
    RowEnd,
    To(usize),
}

/// The clipboard action Ctrl plus the key at evdev `raw_code` (X, C, V) asks for; a masked field has
/// no selection to cut or copy, so only paste reaches it.
fn clipboard_chord(raw_code: u32, masked: bool) -> Option<crate::lua::focus::FieldAction> {
    use crate::lua::focus::FieldAction;
    match raw_code {
        45 if !masked => Some(FieldAction::Cut),
        46 if !masked => Some(FieldAction::Copy),
        47 => Some(FieldAction::Paste),
        _ => None,
    }
}

/// Convert one `wl_keyboard` key for `secure_submit`. Use xkb, not `zwp_text_input_v3`: without an
/// IME, text-input-v3 emits no `commit_string`; a dormant binding could also let the compositor
/// route an IME into the buffer and create two writers (ADR-0027 amendment). No IDL is added: secure
/// bytes go to the native buffer and Supervisor (ADR-0005); misses are [`KeyAction::Ignore`].
///
/// Filter control characters by text, not keysym: xkbcommon returns C0 text for Escape, Tab, and
/// Return, and appending it would put an invisible ESC in a PAM password. Ignore repeated Enter;
/// `secure_submit_frame` zeroizes on submit, so repeat would send an empty PAM attempt.
fn key_action<'a>(event: &'a KeyEvent, repeat: bool, ctrl: bool, shift: bool) -> KeyAction<'a> {
    /// evdev's code for the key `a` sits on, which the Wayland key event carries verbatim
    /// (linux/input-event-codes.h).
    const KEY_A: u32 = 30;
    const KEY_Z: u32 = 44;
    const KEY_Y: u32 = 21;

    // The key `a` sits on, by what it types or by where it is (ADR-0238 decision 2). Under an
    // Arabic or Cyrillic layout the keysym is that layout's own letter, and asking only what it
    // types loses the chord to exactly the people most likely to be using one.
    let latin = event.keysym.key_char().is_some_and(|c| c.is_ascii_alphabetic());
    let selects_all = matches!(event.keysym, Keysym::a | Keysym::A) || (!latin && event.raw_code == KEY_A);
    let z = matches!(event.keysym, Keysym::z | Keysym::Z) || (!latin && event.raw_code == KEY_Z);
    let y = matches!(event.keysym, Keysym::y | Keysym::Y) || (!latin && event.raw_code == KEY_Y);
    // Ctrl reaches editing, word motion and select-all. Every other chord belongs to the
    // compositor, and swallowing it here would take it from them.
    if ctrl {
        return match event.keysym {
            Keysym::BackSpace => KeyAction::Erase(Motion::WordLeft),
            Keysym::Delete | Keysym::KP_Delete => KeyAction::Erase(Motion::WordRight),
            Keysym::Left | Keysym::KP_Left => KeyAction::Move(Motion::WordLeft),
            Keysym::Right | Keysym::KP_Right => KeyAction::Move(Motion::WordRight),
            _ if selects_all => KeyAction::SelectAll,
            _ if z && !repeat => {
                if shift {
                    KeyAction::Redo
                } else {
                    KeyAction::Undo
                }
            }
            _ if y && !repeat => KeyAction::Redo,
            _ => KeyAction::Ignore,
        };
    }
    match event.keysym {
        Keysym::Return | Keysym::KP_Enter => {
            if repeat {
                KeyAction::Ignore
            } else {
                KeyAction::Submit
            }
        }
        Keysym::BackSpace => KeyAction::Erase(Motion::Left),
        Keysym::Delete | Keysym::KP_Delete => KeyAction::Erase(Motion::Right),
        // PAM counts wrong attempts; Escape clears a mistyped password without Backspace-per-char.
        Keysym::Escape => {
            if repeat {
                KeyAction::Ignore
            } else {
                KeyAction::Clear
            }
        }
        Keysym::Left | Keysym::KP_Left => KeyAction::Move(Motion::Left),
        Keysym::Right | Keysym::KP_Right => KeyAction::Move(Motion::Right),
        Keysym::Home | Keysym::KP_Home => KeyAction::Move(Motion::Start),
        Keysym::End | Keysym::KP_End => KeyAction::Move(Motion::End),
        _ => match event.utf8.as_deref() {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => KeyAction::Append(text),
            _ => KeyAction::Ignore,
        },
    }
}

/// Keyboard focus selected by the compositor; `keyboard_interactivity` controls eligibility, and
/// `wl_keyboard` enter/leave reports the result. Dispatch is delegated by `delegate_dispatch2!`.
impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        // Ignore already-held `raw`/`keysyms`. An enter may name a surface destroyed after the
        // compositor sent it (`visible` flip or output change); `None` is not an error.
        self.keyboard_focus = self.surface_id_for(surface).map(str::to_string);
        self.prune_control_focus();
        self.sync_focused();
        // Redraw the caret of a field whose keyboard returned (ADR-0108; see `leave`).
        self.mark_focused_text_field_changed();
        // Include shown child popups, where a panel password prompt lives
        // (see [`App::keyboard_focus_scope`]).
        let scope = self.keyboard_focus_scope();
        // A scope with exactly one `secure_submit` becomes typable without a click.
        let next = self.field_the_scope_declares(&scope, self.focused_secure_submit.clone());
        match (&self.keyboard_focus, &next) {
            (None, _) => debug!(2; "keyboard focus entered an untracked surface; not tracking it"),
            (Some(id), Some(field)) => debug!(
                2; "keyboard focus entered {id} and takes {}'s `secure_submit` field ({}/{})",
                field.surface_id, field.target.capability, field.target.action
            ),
            // Report the searched popup scope so "no field" distinguishes out-of-reach from hidden.
            (Some(id), None) => debug!(
                2; "keyboard focus entered {id}, and neither it nor its shown popups {:?} declare a sole `secure_submit` field",
                &scope[1..]
            ),
        }
        // Always disarm when nothing is found, or keys remain addressed to the previous field.
        let secure_armed = next.is_some();
        self.focus_secure_submit(next);
        // ADR-0112: absent masked focus or an already-typing plain field, scope `autofocus` takes
        // keys; focus-follows-mouse may enter repeatedly, so a typing field keeps its draft.
        let typing_restored = !secure_armed && self.restore_typing_control_on_enter(&scope);
        self.armed_control = None;
        if focus::should_arm_autofocus(secure_armed, typing_restored, self.focused_control.as_ref()) {
            self.arm_autofocus(&scope);
        }
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        // Clear unconditionally: protocol orders old-surface leave before new-surface enter.
        let left = self.keyboard_focus.take().unwrap_or_else(|| "an untracked surface".to_string());
        self.invalidate_text_input_focus();
        self.set_control_focus(None);
        // ADR-0050 decision 4: elsewhere means no submit will arrive; clear secure focus and the
        // armed press like pointer Leave.
        self.focus_secure_submit(None);
        if let Some(field) = self.focused_text_field.as_mut() {
            field.history.clear();
        }
        // Keep the plain draft (ADR-0108): OnDemand/focus-follows-mouse temporarily removes the
        // keyboard, not the reply. It stops keys/caret until focus returns.
        self.mark_focused_text_field_changed();
        self.armed = None;
        // A Shift released while someone else holds the keyboard sends no `modifiers` here, and a
        // stale one turns the next press into a selection the user never made (ADR-0236). The
        // release that would stop a repeat does not arrive either.
        self.shift_held = false;
        self.ctrl_held = false;
        self.alt_held = false;
        self.super_held = false;
        self.repeating = None;
        debug!(2; "keyboard focus left {left}");
    }

    // Keys reach Lua through `on_key` and `on_escape`; `secure_submit` (ADR-0005) sends `KeyEvent`
    // bytes through native `SecureBuffer` to Supervisor, never Lua. See [`key_action`].
    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        if self.ctrl_held {
            self.prune_secure_focus();
            self.prune_text_field_focus();
            // Only a field that takes the chord keeps it; with none focused it bubbles to `on_key`.
            let field_takes = self.secure_field_takes_keys()
                || self.focused_text_field.as_ref().is_some_and(|field| self.text_field_takes_keys(field));
            if field_takes && let Some(action) = clipboard_chord(event.raw_code, self.secure_field_takes_keys()) {
                self.run_field_action(action, Some(serial));
                return;
            }
        }
        self.key_serial = Some(serial);
        self.apply_key(&event, false, Some(serial));
        self.arm_repeat(event);
    }

    /// The compositor's rate and delay. SCTK offers this for a repeat outside calloop, which is
    /// what [`App::fire_due_repeat`] is: taking the seat's own numbers rather than picking any.
    fn update_repeat_info(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        self.repeat_info = match info {
            RepeatInfo::Repeat { rate, delay } => Some((
                std::time::Duration::from_millis(u64::from(delay)),
                std::time::Duration::from_secs(1) / rate.get(),
            )),
            RepeatInfo::Disable => None,
        };
        self.repeating = None;
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.apply_key(&event, true, None);
    }

    // SCTK release events have no `utf8` and edit no buffer; all a release does is stop the
    // repeat it started, and only if a newer press has not already taken it over.
    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        if self.repeating.as_ref().is_some_and(|(held, _)| held.raw_code == event.raw_code) {
            self.repeating = None;
        }
    }

    /// Shift turns a caret motion into a selection and Ctrl reaches editing bindings (ADR-0236); all
    /// four reach `on_key`.
    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
        self.shift_held = modifiers.shift;
        self.ctrl_held = modifiers.ctrl;
        self.alt_held = modifiers.alt;
        self.super_held = modifiers.logo;
    }
}

impl App {
    /// Whether `instance_id` is still a surface this process has a live `wl_surface` for.
    /// `TrackedRole::wl_surface` is the right test: it answers `None` for both shapes a gone
    /// surface takes, the entry removed outright ([`App::destroy_surface_by_id`]) or kept with its
    /// role object dropped ([`App::drop_role_object`]).
    fn surface_is_live(&self, instance_id: &str) -> bool {
        self.surfaces.iter().any(|tracked| tracked.surface_id == instance_id && tracked.role.wl_surface().is_some())
    }

    /// The surfaces a keystroke arriving now can reach: whichever surface holds keyboard focus,
    /// followed by every popup currently shown under it. Empty when nothing here holds the
    /// keyboard, or when the compositor's focus names a surface this process no longer tracks.
    ///
    /// The popups are the point. `wl_keyboard` focus is one surface, but an `xdg_popup` is only
    /// handed it by niri when its parent already held the keyboard at the moment the popup mapped.
    /// A config that raises a surface's `keyboard_interactivity` from a click inside a popup already
    /// shown under it gets the keys on that surface while the field that wants them is on the popup.
    /// Asking the parent alone left the field untypable until the popup was closed and reopened.
    ///
    /// Reuses `xdg_shell`'s [`App::shown_popups_under`], the same walk `hide_popup` destroys by, so
    /// "shown under this surface" has one definition. Ids rather than trees: the per-keystroke
    /// caller ([`App::prune_secure_focus`]) needs only the ids.
    pub(in crate::wayland) fn keyboard_focus_scope(&self) -> Vec<String> {
        let Some(focused) = self.keyboard_focus.as_deref() else {
            return Vec::new();
        };
        let Some(index) = self.surfaces.iter().position(|tracked| tracked.surface_id == focused) else {
            return Vec::new();
        };
        let mut popups = Vec::new();
        self.shown_popups_under(index, &mut popups);
        let mut scope = vec![focused.to_string()];
        scope.extend(popups.into_iter().map(|popup| self.surfaces[popup].surface_id.clone()));
        scope
    }

    fn scoped_trees<'a>(&'a self, scope: &'a [String]) -> Vec<(&'a str, &'a layout::ResolvedNode)> {
        scope.iter().filter_map(|id| self.client.scene().surface(id).map(|tree| (id.as_str(), tree))).collect()
    }

    /// Focus for `surface_id` in `layout::paint::FieldFocus` form. Keep masked `{ capability,
    /// action }` routing here; paint receives only its filled count, never secret bytes.
    pub(in crate::wayland) fn field_focus_for(&self, surface_id: &str) -> Option<layout::paint::FieldFocus<'_>> {
        if let Some(focused) = self.focused_secure_submit.as_ref().filter(|f| f.surface_id == surface_id) {
            return Some(layout::paint::FieldFocus::Masked {
                id: focused.id,
                target: &focused.target,
                filled: self.secure_buffer.grapheme_count(),
            });
        }
        let focused = self.focused_text_field.as_ref().filter(|f| f.surface_id == surface_id)?;
        if self.text_field_takes_keys(focused)
            && let Some(preedit) = self.text_input.composing(surface_id, focused.id)
        {
            return Some(layout::paint::FieldFocus::Composing {
                id: focused.id,
                text: &focused.buffer,
                selection: focused.selection,
                preedit: &preedit.text,
                cursor: preedit.cursor,
                caret_on: self.caret_on(std::time::Instant::now()),
            });
        }
        Some(layout::paint::FieldFocus::Plain {
            id: focused.id,
            text: &focused.buffer,
            caret: self.text_field_takes_keys(focused).then_some(focused.selection),
            caret_on: self.caret_on(std::time::Instant::now()),
        })
    }

    /// Holds `event` for repeat when repeating it would do anything: a modifier or an Enter would
    /// only wake the loop to reach [`KeyAction::Ignore`]. A newer press takes the timer over.
    fn arm_repeat(&mut self, event: KeyEvent) {
        let repeats = !matches!(self.field_key_action(&event, true), KeyAction::Ignore)
            || !self.key_handlers_now(&event).1.is_empty();
        self.repeating =
            self.repeat_info.filter(|_| repeats).map(|(delay, _)| (event, std::time::Instant::now() + delay));
    }

    /// When the held key next repeats, for `poll`'s timeout.
    pub(in crate::wayland) fn next_repeat_deadline(&self) -> Option<std::time::Instant> {
        self.repeating.as_ref().map(|(_, due)| *due)
    }

    /// Delivers the held key if its moment has come. One per turn, counted from now rather than
    /// from the moment missed: a loop that slept through several owes one keystroke, not a burst.
    pub(in crate::wayland) fn fire_due_repeat(&mut self) {
        let now = std::time::Instant::now();
        let Some((_, interval)) = self.repeat_info else { return };
        if !self.repeating.as_ref().is_some_and(|(_, due)| *due <= now) {
            return;
        }
        let Some((event, _)) = self.repeating.take() else { return };
        self.apply_key(&event, true, None);
        self.repeating = Some((event, now + interval));
    }

    /// Apply one key to either field kind (ADR-0092), pruning both focuses once before dispatch. A key
    /// no plain field takes bubbles through `on_key`; an armed secure field never lets Lua hear one.
    fn apply_key(&mut self, event: &KeyEvent, repeat: bool, serial: Option<u32>) {
        self.prune_secure_focus();
        self.prune_text_field_focus();
        self.prune_control_focus();
        if self.apply_tab_key(event) {
            return;
        }
        let handler = (event.keysym == Keysym::Escape).then(|| self.surface_escape_handler(repeat)).flatten();
        if self.focused_secure_submit.is_none() {
            let taken = self.apply_plain_key(event, repeat);
            if !taken && self.deliver_on_key(event, repeat) {
                return;
            }
        }
        if self.apply_activation(event, repeat, serial) {
            return;
        }
        self.apply_secure_key(event, repeat);
        if let Some((id, on_escape)) = handler {
            call_logged(&on_escape, (), format_args!("{id}: on_escape"));
        }
    }

    /// Runs `deliver` as though `surface_id` held the keyboard and `ctrl`/`shift` were the only
    /// modifiers down, then puts the real keyboard focus and modifiers back. Refuses while a masked
    /// field holds focus: injection never reaches a secret.
    fn with_injected_keyboard<T>(
        &mut self,
        surface_id: &str,
        (ctrl, shift): (bool, bool),
        deliver: impl FnOnce(&mut Self) -> T,
    ) -> Result<T, String> {
        if self.focused_secure_submit.is_some() {
            return Err("a `secure_submit` field holds the keyboard".to_string());
        }
        let focus = self.keyboard_focus.replace(surface_id.to_string());
        let held = (self.ctrl_held, self.shift_held, self.alt_held, self.super_held);
        (self.ctrl_held, self.shift_held, self.alt_held, self.super_held) = (ctrl, shift, false, false);
        let result = self.keeping_foreign_focus(surface_id, deliver);
        self.keyboard_focus = focus;
        // The scope moved back, and `focus_visible` follows it.
        self.sync_focused();
        (self.ctrl_held, self.shift_held, self.alt_held, self.super_held) = held;
        Ok(result)
    }

    /// `mantle input key`: [`Self::apply_key`] with no serial and no repeat, so the clipboard chords
    /// that need the compositor's selection are refused.
    pub(in crate::wayland::input) fn inject_key(
        &mut self,
        surface_id: &str,
        event: &KeyEvent,
        modifiers: (bool, bool),
    ) -> Result<(), String> {
        if modifiers.0 && matches!(event.keysym, Keysym::c | Keysym::C | Keysym::x | Keysym::X | Keysym::v | Keysym::V)
        {
            return Err("copy, cut and paste need the compositor's clipboard".to_string());
        }
        self.with_injected_keyboard(surface_id, modifiers, |app| app.apply_key(event, false, None))
    }

    /// `mantle input type`: the commit path of an input method, so `max_length` and `on_change`
    /// apply. Fails when no plain field is typing, which a real keystroke would drop silently.
    pub(in crate::wayland::input) fn inject_text(&mut self, surface_id: &str, text: &str) -> Result<(), String> {
        check_injected_text(text)?;
        self.with_injected_keyboard(surface_id, (false, false), |app| {
            app.prune_text_field_focus();
            if !app.focused_text_field.as_ref().is_some_and(|field| app.text_field_takes_keys(field)) {
                return Err(format!("no textfield is focused in {surface_id}; press one first"));
            }
            app.apply_ime_edit((0, 0), Some(text));
            Ok(())
        })?
    }

    /// The `on_escape` this Escape press fires, resolved before the fields clear or drop focus.
    fn surface_escape_handler(&self, repeat: bool) -> Option<(String, Function)> {
        let plain = self.focused_text_field.as_ref().filter(|field| self.text_field_takes_keys(field)).map(|field| {
            let composing = self.text_input.composing(&field.surface_id, field.id).is_some();
            focus::plain_escape(field.escape, !field.buffer.is_empty(), composing, field.on_cancel.is_some())
        });
        let secure = self.focused_secure_submit.as_ref().map(|field| {
            let cancels = self
                .client
                .scene()
                .surface(&field.surface_id)
                .is_some_and(|tree| secure::secure_on_cancel(tree, field).is_some());
            (!self.secure_buffer.is_empty(), cancels)
        });
        if !focus::surface_gets_escape(repeat, self.ctrl_held, plain, secure) {
            return None;
        }
        let scope = self.keyboard_focus_scope();
        focus::escape_handler(&self.scoped_trees(&scope)).map(|(id, f)| (id.to_string(), f))
    }
}

/// The text `ime_change` would drop silently, named so `mantle input type` can fail.
fn check_injected_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("`type` needs text".to_string());
    }
    if text.chars().any(char::is_control) {
        return Err("text with a control character is dropped; send those through `key`".to_string());
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::wayland) mod tests {
    use super::super::tests::hit_node;
    use super::*;

    #[test]
    fn ctrl_x_c_v_map_to_cut_copy_paste_and_a_masked_field_only_pastes() {
        use crate::lua::focus::FieldAction::{Copy, Cut, Paste};
        assert_eq!([45, 46, 47].map(|code| clipboard_chord(code, false)), [Some(Cut), Some(Copy), Some(Paste)]);
        assert_eq!([45, 46, 47].map(|code| clipboard_chord(code, true)), [None, None, Some(Paste)]);
        assert_eq!(clipboard_chord(30, false), None);
    }

    #[test]
    fn injected_text_the_input_method_path_would_drop_is_an_error() {
        assert!(check_injected_text("héllo ").is_ok());
        assert!(check_injected_text("").is_err());
        assert!(check_injected_text("a\nb").is_err());
        assert!(check_injected_text("\u{7f}").is_err());
    }
    use crate::layout::secure_submit::sole_secure_submit;
    use crate::layout::secure_submit::tree_can_authenticate;

    /// One `secure_submit` destination, as the parsers hand it back.
    pub(super) fn target(capability: &str, action: &str) -> node::SecureSubmitTarget {
        node::SecureSubmitTarget { capability: capability.to_string(), action: action.to_string(), name: None }
    }

    /// A focused field as [`App::focus_secure_submit`] stores one: a destination *and* the instance
    /// id of the surface it was declared on.
    pub(super) fn field(surface_id: &str, capability: &str, action: &str) -> FocusedField {
        FocusedField {
            surface_id: surface_id.to_string(),
            id: layout::scene::NodeId::test(1),
            target: target(capability, action),
        }
    }

    /// A `textfield` node carrying whatever the config wrote under `secure_submit`; `None` writes
    /// nothing, since `secure_submit` is optional even on a masked field.
    pub(in crate::wayland) fn textfield(lua: &Lua, secure_submit: Option<Value>) -> layout::ResolvedNode {
        let mut node = hit_node(lua, "textfield", (0.0, 0.0, 40.0, 24.0), false);
        if let Some(value) = secure_submit {
            std::rc::Rc::make_mut(&mut node.properties).insert("secure_submit", value);
        }
        // Re-derived rather than hand-written, because `layout::secure_submit` reads the parsed
        // style now and `Scene::apply` is what fills it in production: a fixture that set it by
        // hand could declare a destination the parser would never have found.
        node.paint = node::paint_style(node.kind, &node.properties).unwrap();
        node
    }

    /// `node` with one more property written, its style re-derived like [`textfield`]'s.
    pub(in crate::wayland) fn with_property(
        mut node: layout::ResolvedNode,
        key: &'static str,
        value: Value,
    ) -> layout::ResolvedNode {
        std::rc::Rc::make_mut(&mut node.properties).insert(key, value);
        node.paint = node::paint_style(node.kind, &node.properties).unwrap();
        node
    }

    /// A named `on_click` control, so a keyboard-focusable one.
    pub(in crate::wayland) fn button(lua: &Lua) -> layout::ResolvedNode {
        let on_click = lua.create_function(|_, ()| Ok(())).unwrap();
        let node =
            with_property(hit_node(lua, "rect", (0.0, 0.0, 9.0, 9.0), false), "on_click", Value::Function(on_click));
        with_property(node, "accessible_name", Value::String(lua.create_string("Go").unwrap()))
    }

    #[test]
    fn a_disabled_field_takes_no_focus_of_either_kind() {
        let lua = Lua::new();
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        let plain = with_property(plain_textfield(&lua), "disabled", Value::Boolean(true));
        assert!(focused_field(&[&root, &plain]).is_none());
        let secure = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let (id, target) = (secure.id, masked_target(&[&secure]).unwrap());
        let secure = with_property(secure, "disabled", Value::Boolean(true));
        assert!(focused_field(&[&root, &secure]).is_none());
        let tree = tree_with(&lua, vec![secure]);
        assert_eq!(sole_secure_submit(&tree), None, "a disabled prompt is not the scope's destination");
        assert_eq!(focus::secure_target_at(&tree, id), None);
        assert_eq!(focus::secure_id(&tree, &target), None);
    }

    pub(in crate::wayland) fn secure_submit_table(lua: &Lua, capability: &str, action: &str) -> Value {
        let table = lua.create_table().unwrap();
        table.set("capability", capability).unwrap();
        table.set("action", action).unwrap();
        Value::Table(table)
    }

    /// The masked destination [`focused_field`] found, or `None` for anything else. Most of these
    /// tests only care about that half.
    fn masked_target(path: &[&layout::ResolvedNode]) -> Option<node::SecureSubmitTarget> {
        match focused_field(path)? {
            FieldTarget::Masked { target, .. } => Some(target),
            FieldTarget::Plain { .. } => None,
        }
    }

    /// A `textfield` carrying `on_submit`, the plain half's minimum for being worth focusing.
    pub(in crate::wayland) fn plain_textfield(lua: &Lua) -> layout::ResolvedNode {
        let mut node = textfield(lua, None);
        let on_submit = lua.create_function(|_, _text: String| Ok(())).unwrap();
        std::rc::Rc::make_mut(&mut node.properties).insert("on_submit", Value::Function(on_submit));
        node
    }

    pub(in crate::wayland) fn draft(id: u64, buffer: &str) -> FocusedTextField {
        FocusedTextField {
            surface_id: "calendar@eDP-1".to_string(),
            id: layout::scene::NodeId::test(id),
            selection: (buffer.len(), buffer.len()),
            buffer: buffer.to_string(),
            history: EditHistory::default(),
            typing: false,
            selecting: false,
            span: Default::default(),
            click: None,
            goal_x: None,
            on_change: None,
            on_submit: None,
            on_cancel: None,
            escape: Escape::Clear,
        }
    }

    #[test]
    fn a_press_landing_on_no_textfield_leaves_no_destination_focused() {
        let lua = Lua::new();
        let clickable = hit_node(&lua, "rect", (0.0, 0.0, 40.0, 24.0), true);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(focused_field(&[&root, &clickable]).is_none());
    }

    #[test]
    fn the_innermost_textfield_on_the_path_is_the_one_that_owns_the_next_secret() {
        // Same deep-end scan `click_target` makes, and for the same reason (ADR-0050
        // decision 1): one traversal, two questions.
        let lua = Lua::new();
        let outer = textfield(&lua, Some(secure_submit_table(&lua, "network", "connect")));
        let inner = textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);

        assert_eq!(
            masked_target(&[&root, &outer, &inner]),
            Some(node::SecureSubmitTarget {
                capability: "polkit".to_string(),
                action: "authenticate".to_string(),
                name: None
            })
        );
    }

    /// Neither a destination nor a callback: nothing downstream could read a keystroke, so taking
    /// the keyboard for it would only strand the user in a field that swallows keys (ADR-0092).
    #[test]
    fn a_textfield_that_can_report_nothing_is_not_worth_focusing() {
        let lua = Lua::new();
        let field = textfield(&lua, None);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(focused_field(&[&root, &field]).is_none());
    }

    /// The unmasked half of `textfield` (ADR-0092): no `secure_submit`, a callback, so the press
    /// focuses it as a plain field carrying the node identity paint will find it by (ADR-0099).
    #[test]
    fn a_textfield_with_a_callback_and_no_secure_submit_focuses_as_a_plain_field() {
        let lua = Lua::new();
        let field = plain_textfield(&lua);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        match focused_field(&[&root, &field]) {
            Some(FieldTarget::Plain { id, on_change, on_submit, on_cancel, escape }) => {
                assert_eq!(id, field.id, "the field's own node, not the root it was reached through");
                assert_eq!(escape, Escape::Clear);
                assert!(on_change.is_none());
                assert!(on_submit.is_some());
                assert!(on_cancel.is_none());
            }
            other => panic!("expected a plain field, got {}", if other.is_some() { "masked" } else { "nothing" }),
        }
    }

    /// A `secure_submit` beats a callback on the same node, and it has to: the masked path is the
    /// one that keeps bytes out of the Lua VM (ADR-0005), so a field declaring both must not have
    /// its keystrokes handed to a config.
    #[test]
    fn a_field_declaring_both_a_destination_and_a_callback_stays_masked() {
        let lua = Lua::new();
        let mut field = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let on_submit = lua.create_function(|_, _text: String| Ok(())).unwrap();
        std::rc::Rc::make_mut(&mut field.properties).insert("on_submit", Value::Function(on_submit));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(matches!(focused_field(&[&root, &field]), Some(FieldTarget::Masked { .. })));
    }

    /// A scene `lock` tree with a password field somewhere under its root.
    pub(super) fn tree_with(lua: &Lua, fields: Vec<layout::ResolvedNode>) -> layout::ResolvedNode {
        let mut root = hit_node(lua, "column", (0.0, 0.0, 1920.0, 1080.0), false);
        let mut inner = hit_node(lua, "column", (0.0, 0.0, 360.0, 200.0), false);
        inner.children = fields;
        root.children = vec![hit_node(lua, "label", (0.0, 0.0, 100.0, 20.0), false), inner];
        root
    }

    #[test]
    fn keyboard_focus_takes_the_one_secure_submit_field_a_surface_declares() {
        // The rule that makes a lock screen typable without a click: `focused_secure_submit` used
        // to be set only by a pointer press.
        let lua = Lua::new();
        let tree = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);

        assert_eq!(
            sole_secure_submit(&tree),
            Some(node::SecureSubmitTarget {
                capability: "lock".to_string(),
                action: "authenticate".to_string(),
                name: None
            })
        );
    }

    #[test]
    fn two_secure_submit_fields_on_one_surface_focus_neither() {
        // Deliberately not "the first one": with two destinations there is no non-arbitrary answer
        // to "whose password is this?", which is the same question `submit_frame_for` refuses to
        // guess at (ADR-0050 decision 4). A press still picks one, because a press names a node.
        let lua = Lua::new();
        let tree = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        assert_eq!(sole_secure_submit(&tree), None);

        // A field with no destination is not a candidate either -- it names nowhere to send to.
        let bare = tree_with(&lua, vec![textfield(&lua, None)]);
        assert_eq!(sole_secure_submit(&bare), None);
    }

    #[test]
    fn a_hidden_secure_submit_field_neither_takes_the_keyboard_nor_hides_the_shown_one() {
        // A surface can declare several prompts and show one at a time. A hidden secure field, such
        // as a network password field waiting on `password_ssid`, must not be the scope's sole
        // destination: it would swallow keys meant for what is shown and keep an `autofocus` plain
        // field beside it from arming.
        let lua = Lua::new();
        let mut hidden = textfield(&lua, Some(secure_submit_table(&lua, "network", "connect")));
        hidden.visible = false;
        let mut leaving = textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")));
        leaving.leaving = true;
        let shown = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let tree = tree_with(&lua, vec![hidden, leaving, shown]);

        assert_eq!(
            sole_secure_submit(&tree),
            Some(target("lock", "authenticate")),
            "the one field a key can arrive at is the sole one, whatever the hidden siblings declare"
        );
        // The unfiltered reading still sees all three, because that is the one capability startup
        // wants: a prompt has to register its agent before the capability can ask it for anything.
        assert_eq!(crate::layout::secure_submit::secure_submit_targets(&tree).len(), 3);
    }

    #[test]
    fn the_lock_admission_guard_and_the_keyboard_focus_rule_are_one_predicate() {
        // Defect D: `lock_command`'s `can_authenticate` asked whether any field unlocks, while
        // keyboard focus arms only a surface's sole field. A lock tree with two `secure_submit`
        // fields passed the guard, took the lock, and armed nothing on `enter` -- a keyboard-only
        // machine could then only leave the session by a VT switch.
        let lua = Lua::new();
        let typable = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert!(tree_can_authenticate(&typable));
        assert_eq!(sole_secure_submit(&typable), Some(target("lock", "authenticate")));

        let two_fields = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        assert!(!tree_can_authenticate(&two_fields), "a lock the keyboard cannot arm must not be granted the lock");
        assert_eq!(sole_secure_submit(&two_fields), None);

        // One field, but pointed somewhere the Supervisor does not route an unlock through.
        let wrong_destination =
            tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")))]);
        assert!(!tree_can_authenticate(&wrong_destination));
    }

    pub(super) fn key(keysym: Keysym, utf8: Option<&str>) -> KeyEvent {
        KeyEvent { time: 0, raw_code: 0, keysym, utf8: utf8.map(str::to_string) }
    }

    #[test]
    fn a_focused_secure_field_reads_the_keyboard_directly() {
        // `zwp_text_input_v3` alone did not deliver this: it only produces a `commit_string` when
        // the compositor has an input method bound, so on a session with no IME not one byte
        // reached `SecureBuffer`.
        assert_eq!(key_action(&key(Keysym::a, Some("a")), false, false, false), KeyAction::Append("a"));
        assert_eq!(key_action(&key(Keysym::Return, Some("\r")), false, false, false), KeyAction::Submit);
        assert_eq!(key_action(&key(Keysym::KP_Enter, Some("\r")), false, false, false), KeyAction::Submit);
        assert_eq!(
            key_action(&key(Keysym::BackSpace, Some("\u{8}")), false, false, false),
            KeyAction::Erase(Motion::Left)
        );
    }

    #[test]
    fn a_control_key_never_becomes_a_character_of_the_password() {
        // `utf8` is not empty for Escape, Tab or Return -- xkbcommon hands back the C0 control
        // character for each -- so an unfiltered append would silently put an ESC byte in the
        // middle of a secret that PAM then rejects with no visible reason.
        assert_eq!(key_action(&key(Keysym::Tab, Some("\t")), false, false, false), KeyAction::Ignore);
        assert_eq!(key_action(&key(Keysym::Shift_L, None), false, false, false), KeyAction::Ignore);
        assert_eq!(key_action(&key(Keysym::Control_L, Some("\u{1b}")), false, false, false), KeyAction::Ignore);
    }

    #[test]
    fn escape_throws_the_entry_away_instead_of_being_ignored() {
        // Escape used to reach the control-character filter above and be dropped, which left one
        // Backspace per character as the only way to abandon a mistyped password -- on the surface
        // where a wrong guess costs a counted PAM attempt and a `pam_unix` failure delay.
        assert_eq!(key_action(&key(Keysym::Escape, Some("\u{1b}")), false, false, false), KeyAction::Clear);
        assert_eq!(key_action(&key(Keysym::Escape, Some("\u{1b}")), true, false, false), KeyAction::Ignore);
    }

    /// Editing chords under an Arabic layout use the physical key when the keysym is non-Latin.
    #[test]
    fn ctrl_editing_keys_work_under_a_non_latin_layout() {
        for (raw_code, action, shift_action) in [
            (30, KeyAction::SelectAll, KeyAction::SelectAll),
            (44, KeyAction::Undo, KeyAction::Redo),
            (21, KeyAction::Redo, KeyAction::Redo),
        ] {
            let mut arabic = key(Keysym::Arabic_sheen, Some("ش"));
            arabic.raw_code = raw_code;
            assert_eq!(key_action(&arabic, false, true, false), action);
            assert_eq!(key_action(&arabic, false, true, true), shift_action);
            assert_eq!(key_action(&arabic, false, false, false), KeyAction::Append("ش"));
        }
    }

    #[test]
    fn latin_keysym_wins_over_conflicting_physical_key() {
        let mut qwertz_y = key(Keysym::y, Some("y"));
        qwertz_y.raw_code = 44;
        assert_eq!(key_action(&qwertz_y, false, true, false), KeyAction::Redo);
        let mut azerty_q = key(Keysym::q, Some("q"));
        azerty_q.raw_code = 30;
        assert_eq!(key_action(&azerty_q, false, true, false), KeyAction::Ignore);
        let mut qwertz_z = key(Keysym::z, Some("z"));
        qwertz_z.raw_code = 21;
        assert_eq!(key_action(&qwertz_z, false, true, false), KeyAction::Undo);
    }

    #[test]
    fn holding_enter_down_does_not_resubmit_an_already_scrubbed_buffer() {
        // A submit zeroizes the buffer as it reads it, so the second submit of a key repeat would
        // send an *empty* password to PAM and burn one of the user's attempts. Backspace and
        // ordinary characters repeat normally, which is what every text field does.
        assert_eq!(key_action(&key(Keysym::Return, Some("\r")), true, false, false), KeyAction::Ignore);
        assert_eq!(
            key_action(&key(Keysym::BackSpace, Some("\u{8}")), true, false, false),
            KeyAction::Erase(Motion::Left)
        );
        assert_eq!(key_action(&key(Keysym::a, Some("a")), true, false, false), KeyAction::Append("a"));
    }
}

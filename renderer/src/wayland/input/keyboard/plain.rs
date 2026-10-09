//! Plain `textfield`s: which one `autofocus` arms, the draft and selection a key edits, the
//! caret's blink, and the edits delivered to Lua (ADR-0092).

use shared::{debug, warn};

use super::edit::{PlainEdit, field_took, fit_to_limit, ime_change, ime_commit, limited_append};
use super::*;
use crate::lua::call_logged;
use crate::lua::focus::FieldAction;

/// First `autofocus = true` control in scope document order (ADR-0112); duplicate search boxes are a
/// config mistake, so deterministic order beats refusing both. A hidden subtree is skipped whole (ADR-0124).
fn autofocus_in_scope(scope: &[(&str, &layout::ResolvedNode)]) -> Option<super::focus::FocusedControl> {
    let on = |node: &layout::ResolvedNode| node::fields::common::autofocus.read(&node.properties).is_ok_and(|on| on);
    scope.iter().find_map(|(surface_id, tree)| super::focus::first_focusable(surface_id, tree, on))
}

/// The node `id`'s `initial_text` now, `""` when unset or gone.
fn initial_text_of(tree: Option<&layout::ResolvedNode>, id: layout::scene::NodeId, lua: &mlua::Lua) -> String {
    let path = tree.and_then(|tree| layout::hit::path_to_node(tree, id));
    path.and_then(|path| path.last().map(|node| node.current_initial_text(lua))).unwrap_or_default()
}

/// The `focus_target` name of `field` when it is focused, takes keys and (if `need_selection`) has
/// text selected: the one field `has_selection` and the handle's actions mean.
fn focus_holder(
    field: Option<&FocusedTextField>,
    takes_keys: bool,
    need_selection: bool,
    name_of: impl Fn(&FocusedTextField) -> Option<String>,
) -> Option<String> {
    field.filter(|field| takes_keys && (!need_selection || field.selection.0 != field.selection.1)).and_then(name_of)
}

/// Plain fields passing `matches`, in document order; hidden subtrees and disabled fields only when `all`.
fn plain_fields<'a>(
    tree: &'a layout::ResolvedNode,
    all: bool,
    matches: impl Fn(&layout::ResolvedNode) -> bool + 'a,
) -> impl Iterator<Item = FieldTarget> + 'a {
    let mut stack = vec![tree];
    std::iter::from_fn(move || {
        while let Some(node) = stack.pop() {
            if (!node.visible && !all) || node.leaving {
                continue;
            }
            stack.extend(node.content_children().rev());
            if node.kind == "textfield"
                && matches(node)
                && let Some(target @ FieldTarget::Plain { .. }) = field_target(&[node], all)
            {
                return Some(target);
            }
        }
        None
    })
}

/// Whether a field on a live surface can no longer hold focus: its node is gone or disabled.
fn field_unusable(tree: Option<&layout::ResolvedNode>, id: layout::scene::NodeId) -> bool {
    let Some(tree) = tree.filter(|tree| layout::hit::contains_node(tree, id)) else { return true };
    layout::hit::path_to_node(tree, id).is_some_and(|path| path.last().is_some_and(|node| node.is_disabled_field()))
}

type Parked = std::collections::HashMap<(String, layout::scene::NodeId), (String, (usize, usize))>;

/// The one rule for a parked draft: non-empty text is kept with its selection, empty drops it.
fn park(parked: &mut Parked, surface_id: &str, id: layout::scene::NodeId, text: &str, selection: (usize, usize)) {
    let key = (surface_id.to_owned(), id);
    if text.is_empty() {
        parked.remove(&key);
    } else {
        parked.insert(key, (text.to_owned(), selection));
    }
}

/// Moves focus from `old` to `next`: `old`'s text is parked unless `next` is that same field, and
/// `next` takes back what it parked. Only plain fields get here, so no secret is ever held.
fn swap_drafts(parked: &mut Parked, old: Option<&FocusedTextField>, next: Option<&mut FocusedTextField>) {
    if let Some(old) =
        old.filter(|old| next.as_ref().is_none_or(|next| (&old.surface_id, old.id) != (&next.surface_id, next.id)))
    {
        park(parked, &old.surface_id, old.id, &old.buffer, old.selection);
    }
    let Some(next) = next else { return };
    if let Some((buffer, selection)) = parked.remove(&(next.surface_id.clone(), next.id))
        && next.buffer.is_empty()
    {
        (next.buffer, next.selection) = (buffer, selection);
    }
}

/// Replaces the draft of `(surface_id, id)`: in place when it is the focused field, else in the
/// parked map. Returns whether the focused field took it.
fn store_draft(
    parked: &mut Parked,
    focused: Option<&mut FocusedTextField>,
    surface_id: &str,
    id: layout::scene::NodeId,
    text: &str,
) -> bool {
    let end = (text.len(), text.len());
    if let Some(field) = focused.filter(|field| field.surface_id == surface_id && field.id == id) {
        (field.buffer, field.selection, field.goal_x) = (text.to_owned(), end, None);
        field.history.clear();
        return true;
    }
    park(parked, surface_id, id, text, end);
    false
}

/// A parked draft lives only as long as its node and surface.
fn forget_gone_drafts(parked: &mut Parked, alive: impl Fn(&str, layout::scene::NodeId) -> bool) {
    parked.retain(|(surface_id, id), _| alive(surface_id, *id));
}

pub(super) fn requested_focus(
    surface_id: String,
    target: FieldTarget,
    previous: Option<&FocusedTextField>,
) -> FocusedTextField {
    let FieldTarget::Plain { id, on_change, on_submit, on_cancel, escape } = target else {
        unreachable!("requested_focus takes plain targets")
    };
    let retained = previous.filter(|field| field.surface_id == surface_id && field.id == id);
    let (buffer, selection) = retained.map_or((String::new(), (0, 0)), |field| (field.buffer.clone(), field.selection));
    FocusedTextField {
        surface_id,
        id,
        buffer,
        selection,
        history: EditHistory::default(),
        typing: true,
        selecting: false,
        span: Default::default(),
        click: None,
        goal_x: None,
        on_change,
        on_submit,
        on_cancel,
        escape,
    }
}

/// Whether a plain field takes the keys arriving now. A masked field armed anywhere in scope takes
/// them all: the two focuses are held independently, and `apply_key` offers a key to both, so a
/// prompt revealed while a plain field was already typing would otherwise put every character of a
/// password through that field's `on_change` -- into Lua, which is the one place a `secure_submit`
/// secret must never reach (ADR-0005). "Masked focus wins" is the rule
/// `arm_autofocus_if_unfocused`
/// already states for arming; this is the same rule for the keys themselves.
///
/// The draft survives, exactly as it does when the surface loses the keyboard (ADR-0108): the field
/// stops taking keys and stops drawing a caret, and is typable again when the prompt is answered.
fn plain_field_takes_keys(typing: bool, its_surface_is_in_scope: bool, a_masked_field_is_armed: bool) -> bool {
    typing && its_surface_is_in_scope && !a_masked_field_is_armed
}

/// The three callbacks one plain-field edit may deliver. Grouped so [`deliver_plain_edit`] takes an
/// argument per idea rather than one per callback.
struct PlainCallbacks {
    on_change: Option<Function>,
    on_submit: Option<Function>,
    on_cancel: Option<Function>,
}

/// Delivers one plain-field edit's callbacks in the order a config can rely on (ADR-0189), with
/// `on_cancel` told whether the Escape it reports cleared any text.
///
/// `on_submit` before `on_change`. A submit clears the buffer, and the `on_change` that reports the
/// clearing carries the empty string; delivering it first hands a config the empty field before the
/// text that filled it. A list that derives its selection from the query then resolves against an
/// empty needle and submits the first unfiltered row rather than the one on screen. Submit carries the user's intent and goes first;
/// the clear is bookkeeping and follows.
///
/// Free rather than a method so the ordering can be tested with recording closures; the dispatch it
/// came out of needs a whole `App`, which is why this was never covered.
fn deliver_plain_edit(lua: &Lua, surface_id: &str, edit: PlainEdit, text: String, callbacks: PlainCallbacks) {
    let PlainCallbacks { on_change, on_submit, on_cancel } = callbacks;
    crate::lua::focus::begin_callback(lua, surface_id, true);
    if edit.submitted
        && let Some(on_submit) = on_submit
    {
        call_logged(&on_submit, text.clone(), format_args!("{surface_id}: on_submit"));
    }
    if edit.changed
        && let Some(on_change) = on_change
    {
        let text = if edit.submitted { String::new() } else { text };
        call_logged(&on_change, text, format_args!("{surface_id}: on_change"));
    }
    // `edit.changed` is the one thing a config cannot work out for itself: the autofocus arm fires
    // `on_change("")` too, so counting empty changes cannot tell a cleared field from an opened one.
    if edit.cancelled
        && let Some(on_cancel) = on_cancel
    {
        call_logged(&on_cancel, edit.changed, format_args!("{surface_id}: on_cancel"));
    }
    crate::lua::focus::end_callback(lua);
}

/// The caret's phase `elapsed` after the last input, and when it next flips, both measured from
/// that input. On for the first half of each cycle, then on for good once `timeout` passes, so an
/// idle focused field arms no wake.
fn caret_phase(
    blink: Option<(std::time::Duration, std::time::Duration)>,
    elapsed: std::time::Duration,
) -> (bool, Option<std::time::Duration>) {
    let Some((half, timeout)) = blink else { return (true, None) };
    if elapsed >= timeout {
        return (true, None);
    }
    let flips = elapsed.as_millis() / half.as_millis();
    let next = u32::try_from(flips + 1).ok().map(|n| (half * n).min(timeout));
    (flips.is_multiple_of(2), next)
}

/// The `focus_target` name a node is bound to.
fn focus_name(node: &layout::ResolvedNode) -> Option<String> {
    node::fields::common::focus_target.read(&node.properties).ok().flatten()
}

impl App {
    /// A pointer press already stopped typing. Apply the callback's `:request()` after its state has
    /// resolved, before autofocus and repaint, so a newly shown field or control can receive the next key.
    pub(in crate::wayland) fn apply_focus_request(&mut self) {
        let Some(crate::lua::focus::Request { surface: surface_id, name, ring }) =
            crate::lua::focus::take_request(self.client.lua())
        else {
            return;
        };
        if !self.keyboard_focus_scope().contains(&surface_id)
            || !self.surface_is_live(&surface_id)
            || self.focused_secure_submit.is_some()
        {
            return;
        }
        let Some(tree) = self.client.scene().surface(&surface_id) else { return };
        let Some(control) =
            super::focus::first_focusable(&surface_id, tree, |node| focus_name(node).as_deref() == Some(&*name))
        else {
            return;
        };
        self.focus_control(Some(control));
        self.set_focus_visible(ring);
    }

    /// Applies `focus_target(name):set_text` to the fields bound to `name`, hidden ones too: the
    /// focused one is rewritten in place, any other has its parked draft replaced (or dropped for `""`).
    pub(in crate::wayland) fn apply_text_requests(&mut self) {
        for (name, text) in crate::lua::focus::take_texts(self.client.lua()) {
            let named = |node: &layout::ResolvedNode| focus_name(node).as_deref() == Some(&*name);
            let hits: Vec<_> = self
                .client
                .scene()
                .surfaces()
                .flat_map(|(surface_id, tree)| {
                    plain_fields(tree, true, named).filter_map(move |target| match target {
                        FieldTarget::Plain { id, .. } => Some((surface_id.to_string(), id)),
                        FieldTarget::Masked { .. } => None,
                    })
                })
                .collect();
            for (surface_id, id) in hits {
                if crate::lua::focus::refuses(&text, self.field_multiline(&surface_id, id).is_some()) {
                    warn!("{surface_id}: a single-line field refuses focus_target(\"{name}\"):set_text's newline");
                    continue;
                }
                self.set_draft(surface_id, id, &text);
            }
        }
    }

    /// The `focus_target` name of the textfield `field` is.
    fn field_focus_name(&self, field: &FocusedTextField) -> Option<String> {
        let path = layout::hit::path_to_node(self.client.scene().surface(&field.surface_id)?, field.id)?;
        focus_name(path.last()?)
    }

    fn holder(&self, need_selection: bool) -> Option<String> {
        let field = self.focused_text_field.as_ref();
        let takes_keys = field.is_some_and(|field| self.text_field_takes_keys(field));
        focus_holder(field, takes_keys, need_selection, |field| self.field_focus_name(field))
    }

    /// The action a chord or a `focus_target` method asks for, on the focused plain field; a cut or
    /// copy needs the serial of the key or pointer event that caused it.
    pub(in crate::wayland) fn run_field_action(&mut self, action: FieldAction, serial: Option<u32>) {
        match (action, serial) {
            (FieldAction::Cut, Some(serial)) => self.cut_selection(serial),
            (FieldAction::Copy, Some(serial)) => {
                self.copy_selection(serial);
            }
            (FieldAction::Cut | FieldAction::Copy, None) => {
                debug!("{action:?} dropped: no key press or pointer click this turn to copy with");
            }
            (FieldAction::Paste, _) => self.start_paste(),
            (FieldAction::SelectAll, _) => {
                self.apply_plain_action_inner(KeyAction::SelectAll, None, false);
            }
        }
    }

    /// Applies `focus_target(name):cut/copy/paste/select_all` to the field that holds the keyboard
    /// now and is bound to `name`; any other is left alone.
    pub(in crate::wayland) fn apply_field_actions(&mut self) {
        let serial = self.input_serial.as_ref().map(|armed| armed.serial).or(self.key_serial);
        for (name, action) in crate::lua::focus::take_actions(self.client.lua()) {
            self.prune_text_field_focus();
            if self.holder(false).as_deref() == Some(&*name) {
                self.run_field_action(action, serial);
            }
        }
    }

    /// Writes the `has_selection` signals from the field that holds the keyboard.
    pub(in crate::wayland) fn sync_selection(&self) {
        crate::lua::signal::write_selection(self.client.lua(), self.holder(true).as_deref());
    }

    /// Seeds the draft of each field created since the last turn with its `initial_text`, through
    /// the same path as `set_text`. A field whose surface is not live yet waits; a gone one is dropped.
    pub(in crate::wayland) fn apply_seeds(&mut self) {
        self.pending_seeds.extend(self.client.take_seeds());
        for (id, text) in std::mem::take(&mut self.pending_seeds) {
            let surface = self
                .client
                .scene()
                .surfaces()
                .find(|(_, tree)| layout::hit::contains_node(tree, id))
                .map(|(surface_id, _)| surface_id.to_string());
            match surface {
                Some(surface_id) if self.surface_is_live(&surface_id) => self.set_draft(surface_id, id, &text),
                Some(_) => self.pending_seeds.push((id, text)),
                None => {}
            }
        }
    }

    /// The `max_length` of the node `(surface_id, id)`, if it is a field that sets one.
    pub(in crate::wayland) fn field_max_length(&self, surface_id: &str, id: layout::scene::NodeId) -> Option<usize> {
        let path = layout::hit::path_to_node(self.client.scene().surface(surface_id)?, id)?;
        match path.last()?.paint.as_ref()? {
            node::PaintStyle::TextField { max_length, .. } => *max_length,
            _ => None,
        }
    }

    fn set_draft(&mut self, surface_id: String, id: layout::scene::NodeId, text: &str) {
        let text = fit_to_limit(text, self.field_max_length(&surface_id, id), 0);
        if store_draft(&mut self.parked_drafts, self.focused_text_field.as_mut(), &surface_id, id, text) {
            self.cancel_text_input_composition();
            self.text_input.note_other_change();
        }
        self.mark_field_input_changed(&surface_id);
        self.fit_field(&surface_id, id);
    }

    /// Give the keyboard to `autofocus` (ADR-0112).
    pub(super) fn arm_autofocus(&mut self, scope: &[String]) {
        let trees = self.scoped_trees(scope);
        let Some(control) = autofocus_in_scope(&trees) else { return };
        drop(trees);
        // A closed launcher can retain its tree and focus id without a `leave`; require its live
        // `wl_surface` or every turn would arm then prune the same field.
        if !self.surface_is_live(&control.surface_id) {
            return;
        }
        if control.kind != super::focus::ControlKind::Plain {
            // Once per appearance or keyboard enter: a control the user left must not pull focus back.
            let nothing_focused = self.focused_control.is_none();
            if super::focus::should_arm_control(&mut self.armed_control, &control, nothing_focused) {
                self.focus_control(Some(control));
            }
            return;
        }
        let (surface_id, id) = (control.surface_id.clone(), control.id);
        let seed = initial_text_of(self.client.scene().surface(&surface_id), id, self.client.lua());
        debug!("{surface_id}'s `autofocus` textfield takes the keyboard");
        // Autofocus starts from the seed, so a parked draft must not come back.
        self.parked_drafts.remove(&(surface_id.clone(), id));
        self.focus_control(Some(control));
        self.set_draft(surface_id.clone(), id, &seed);
        let opened = self.focused_text_field.as_ref().and_then(|field| field.on_change.clone());
        if let Some(on_change) = opened {
            let text = self.focused_text_field.as_ref().map(|field| field.buffer.clone()).unwrap_or_default();
            call_logged(&on_change, text, format_args!("{surface_id}: on_change"));
        }
    }

    /// Arm a newly appearing `autofocus` field or control under existing focus (ADR-0112), unless a plain
    /// field is typing or a press just stopped that same field. A different field is new; masked
    /// focus wins as on `enter`.
    pub(in crate::wayland) fn arm_autofocus_if_unfocused(&mut self, scope: &[String]) {
        if self.focused_secure_submit.is_some() || self.keyboard_focus.is_none() {
            return;
        }
        self.prune_text_field_focus();
        if let Some(field) = self.focused_text_field.as_ref() {
            if field.typing {
                return;
            }
            let trees = self.scoped_trees(scope);
            let same_field = matches!(
                autofocus_in_scope(&trees),
                Some(control) if control.kind == super::focus::ControlKind::Plain && control.id == field.id
            );
            if same_field {
                return;
            }
        }
        self.arm_autofocus(scope);
    }

    pub(super) fn caret_on(&self, now: std::time::Instant) -> bool {
        caret_phase(self.caret_blink, now.saturating_duration_since(self.caret_epoch)).0
    }

    /// The next phase flip while a field that takes keys is blinking, for the poll timeout.
    /// ponytail: an empty draft arms none, since it shows its placeholder and no caret (ADR-0135);
    /// the rare field with no placeholder keeps a steady caret until the first key.
    pub(in crate::wayland) fn next_caret_deadline(&self) -> Option<std::time::Instant> {
        self.focused_text_field
            .as_ref()
            .filter(|field| !field.buffer.is_empty() && self.text_field_takes_keys(field))?;
        let elapsed = std::time::Instant::now().saturating_duration_since(self.caret_epoch);
        caret_phase(self.caret_blink, elapsed).1.map(|flip| self.caret_epoch + flip)
    }

    /// Queues the focused field's surface when the phase changed since it was last queued.
    pub(in crate::wayland) fn repaint_caret_if_it_flipped(&mut self) {
        let on = self.caret_on(std::time::Instant::now());
        if on == self.caret_painted_on {
            return;
        }
        self.caret_painted_on = on;
        if let Some(id) = self.focused_text_field.as_ref().map(|field| field.surface_id.clone())
            && !self.field_input_surfaces.contains(&id)
        {
            self.field_input_surfaces.push(id);
        }
    }

    /// Every write to `focused_text_field`, funnelled the way [`App::focus_secure_submit`] is --
    /// for repainting rather than for scrubbing. There is no secret here to zeroize; what the two
    /// share is that the field they leave must stop drawing a caret and the field they arrive at
    /// must start.
    pub(in crate::wayland::input) fn focus_text_field(&mut self, mut next: Option<FocusedTextField>) {
        if self.focused_text_field.is_none() && next.is_none() {
            return;
        }
        // A field whose node is gone has nowhere to show its draft, so it is not parked.
        let scene = self.client.scene();
        let alive = self
            .focused_text_field
            .as_ref()
            .filter(|old| scene.surface(&old.surface_id).is_some_and(|tree| layout::hit::contains_node(tree, old.id)));
        swap_drafts(&mut self.parked_drafts, alive, next.as_mut());
        self.mark_focused_text_field_changed();
        if let Some(ref next_field) = next {
            self.mark_field_input_changed(&next_field.surface_id);
        }
        let moved = self.focused_text_field.as_ref().zip(next.as_ref()).is_some_and(|(old, next)| {
            old.surface_id == next.surface_id && old.id == next.id && old.selection != next.selection
        });
        self.focused_text_field = next;
        self.invalidate_text_input_focus();
        if moved {
            self.text_input.note_other_change();
        }
        if let Some(field) = self.focused_text_field.as_mut().filter(|field| !field.typing) {
            field.history.clear();
        }
        self.fit_focused_field();
    }

    /// After a pass, a focused field that became disabled or whose node left a live surface gives
    /// up focus; a surface merely not live keeps its draft (see [`App::prune_text_field_focus`]).
    pub(in crate::wayland) fn drop_unusable_text_field_focus(&mut self) {
        let Some(field) = self.focused_text_field.as_ref() else { return };
        if self.surface_is_live(&field.surface_id)
            && field_unusable(self.client.scene().surface(&field.surface_id), field.id)
        {
            self.focus_text_field(None);
        }
    }

    /// [`App::prune_secure_focus`]'s counterpart. The same two clauses -- the surface is still
    /// alive, and it is still one the keyboard can reach -- because a plain field goes stale for
    /// exactly the reasons a masked one does. What it does not share is the urgency: dropping a
    /// half-typed reply loses a sentence, not a secret, so there is no once-a-turn sweep matching
    /// [`App::drop_secure_focus_if_its_surface_is_gone`]; the check before each keystroke is
    /// enough, and a `leave` clears it anyway.
    pub(in crate::wayland::input) fn prune_text_field_focus(&mut self) {
        let scene = self.client.scene();
        let mut parked = std::mem::take(&mut self.parked_drafts);
        forget_gone_drafts(&mut parked, |surface_id, id| {
            self.surface_is_live(surface_id)
                && scene.surface(surface_id).is_some_and(|tree| layout::hit::contains_node(tree, id))
        });
        self.parked_drafts = parked;
        let Some(field) = self.focused_text_field.as_ref() else {
            return;
        };
        // Liveness of the surface and of the node, not of the keyboard (ADR-0108): a field whose
        // surface lost the keyboard keeps its draft and simply takes no keys until it is back
        // ([`App::text_field_takes_keys`]). A field whose node is gone -- the reply was sent or
        // closed and the row removed -- has nowhere to show a draft, and its callbacks belong to a
        // card that no longer exists, so the next key is what finally lets it go.
        let node_exists = self
            .client
            .scene()
            .surface(&field.surface_id)
            .is_some_and(|tree| layout::hit::contains_node(tree, field.id));
        if self.surface_is_live(&field.surface_id) && node_exists {
            return;
        }
        debug!(2; "the focused textfield is gone; dropping what was typed");
        self.focus_text_field(None);
    }

    /// Whether a key arriving now belongs to `field`: a press chose it, and its surface is one the
    /// keyboard is on (ADR-0108). The same question decides the caret, so what is drawn as live is
    /// what a key would land in.
    pub(in crate::wayland::input) fn text_field_takes_keys(&self, field: &FocusedTextField) -> bool {
        plain_field_takes_keys(
            field.typing,
            self.keyboard_focus_scope().contains(&field.surface_id),
            self.focused_secure_submit.is_some(),
        )
    }

    /// Apply one plain `textfield` key (ADR-0092). Callbacks receive whole text, not
    /// deltas: state bindings want the snapshot, and reassembling deltas is caller work. Submit
    /// leaves the field focused and empty. Escape clears; without `on_cancel`, focus stays because
    /// config cannot observe focus and a silent key stop has no visible signal. With `on_cancel`,
    /// drop focus first, then call it (ADR-0102), so a callback changing the surface finds no stale
    /// focus.
    /// Whether the field took the key; one it does not use bubbles to `on_key`.
    pub(super) fn apply_plain_key(&mut self, event: &KeyEvent, repeat: bool) -> bool {
        let action = self.field_key_action(event, repeat);
        if matches!(action, KeyAction::Append(_)) && self.text_input.owns_text() {
            return true;
        }
        let mut composing = false;
        if matches!(action, KeyAction::Clear) {
            composing = self.focused_text_field.as_ref().is_some_and(|field| {
                self.text_field_takes_keys(field) && self.text_input.composing(&field.surface_id, field.id).is_some()
            });
            self.cancel_text_input_composition();
        }
        self.apply_plain_action_inner(action, None, true) || composing
    }

    pub(in crate::wayland::input) fn apply_ime_edit(&mut self, delete: (u32, u32), commit: Option<&str>) {
        let Some(field) = self.focused_text_field.as_ref().filter(|field| self.text_field_takes_keys(field)) else {
            return;
        };
        let multiline = self.field_multiline(&field.surface_id, field.id).is_some();
        let commit = match commit.map(|text| ime_commit(text, multiline)) {
            Some(None) => return,
            commit => commit.flatten(),
        };
        let Some((range, text)) = ime_change(&field.buffer, field.selection, delete, commit.as_deref()) else { return };
        if let Some(text) = text {
            self.apply_plain_action_inner(KeyAction::Append(text), Some(range), false);
        } else if let Some(field) = self.focused_text_field.as_mut() {
            let edit = field.delete_surrounding(range);
            self.finish_plain_edit(edit, false);
        }
    }

    pub(in crate::wayland::input) fn apply_plain_action_inner(
        &mut self,
        action: KeyAction<'_>,
        ime_range: Option<(usize, usize)>,
        from_key: bool,
    ) -> bool {
        if !self.focused_text_field.as_ref().is_some_and(|field| self.text_field_takes_keys(field)) {
            return false;
        }
        // Read before the borrow below: shift turns a caret motion into a selection.
        let shift = self.shift_held;
        let action = self.resolve_row_motion(action);
        let Some(field) = self.focused_text_field.as_ref() else {
            return false;
        };
        let mut cut = false;
        let action = match action {
            KeyAction::Append(text) => {
                let limited = limited_append(
                    &field.buffer,
                    ime_range.unwrap_or(field.selection),
                    text,
                    self.field_max_length(&field.surface_id, field.id),
                );
                cut = !matches!(limited, KeyAction::Append(t) if t == text);
                limited
            }
            action => action,
        };
        // A cut commit leaves the input method believing all of it landed; resend the surrounding text.
        if cut && ime_range.is_some() {
            self.text_input.note_other_change();
        }
        let Some(field) = self.focused_text_field.as_mut() else {
            return false;
        };
        let edit = field.edit(action, shift, ime_range, from_key);
        let taken = field_took(action, &edit);
        self.finish_plain_edit(edit, ime_range.is_none());
        taken
    }

    fn finish_plain_edit(&mut self, edit: PlainEdit, local: bool) {
        if edit == PlainEdit::NONE {
            return;
        }
        if local && (edit.changed || edit.moved) {
            self.text_input.note_other_change();
        }
        // A caret move repaints and tells the config nothing: no text changed.
        if edit.moved {
            self.mark_focused_text_field_changed();
            self.fit_focused_field();
            return;
        }
        // Clone before callbacks can write a signal and re-resolve the scene.
        let (text, on_change, on_submit, on_cancel, surface_id, field_id) = {
            let field = self.focused_text_field.as_ref().expect("the focus was Some a moment ago");
            (
                field.buffer.clone(),
                field.on_change.clone(),
                field.on_submit.clone(),
                field.on_cancel.clone(),
                field.surface_id.clone(),
                field.id,
            )
        };
        // A passed-up key changes neither text nor caret, so it needs no repaint.
        if edit.passed {
            return;
        }
        if edit.submitted {
            // Empty before the callback can open a popup or re-resolve the scene.
            if let Some(field) = self.focused_text_field.as_mut() {
                field.buffer.clear();
                field.selection = (0, 0);
            }
        }
        if edit.cancelled {
            self.focus_text_field(None);
        }
        self.mark_field_input_changed(&surface_id);
        if edit.changed {
            self.fit_field(&surface_id, field_id);
        }
        deliver_plain_edit(
            self.client.lua(),
            &surface_id,
            edit,
            text,
            PlainCallbacks { on_change, on_submit, on_cancel },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{draft, key, plain_textfield, secure_submit_table, textfield, tree_with};
    use super::*;

    #[test]
    fn ctrl_z_shift_z_and_y_are_plain_history_keys() {
        assert_eq!(key_action(&key(Keysym::z, Some("z")), false, true, false), KeyAction::Undo);
        assert_eq!(key_action(&key(Keysym::Z, Some("Z")), false, true, true), KeyAction::Redo);
        assert_eq!(key_action(&key(Keysym::y, Some("y")), false, true, false), KeyAction::Redo);
        assert_eq!(key_action(&key(Keysym::z, Some("z")), true, true, false), KeyAction::Ignore);
    }

    /// GTK's default: 600 ms on, 600 ms off, solid from 10 s on, and an idle field arms no wake.
    #[test]
    fn the_caret_blinks_in_half_cycles_then_holds_on_past_the_timeout() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        let blink = Some((ms(600), Duration::from_secs(10)));
        assert_eq!(caret_phase(blink, ms(0)), (true, Some(ms(600))));
        assert_eq!(caret_phase(blink, ms(700)), (false, Some(ms(1200))));
        assert_eq!(caret_phase(blink, ms(9_700)), (true, Some(ms(10_000))));
        assert_eq!(caret_phase(blink, ms(10_000)), (true, None));
        assert_eq!(caret_phase(None, ms(700)), (true, None));
    }

    /// ADR-0189. A submit clears the buffer and reports that clearing through `on_change("")`. If
    /// that lands before `on_submit`, a config reading its own query at submit time sees an empty
    /// one -- which launched the first entry of an unfiltered list instead of the row on screen.
    #[test]
    fn a_submit_reaches_on_submit_before_the_on_change_that_reports_the_clearing() {
        let lua = mlua::Lua::new();
        lua.load("log = {}").exec().unwrap();
        let record = |name: &'static str| {
            let lua_ref = &lua;
            lua_ref
                .create_function(move |lua, text: mlua::Value| {
                    let seen = match text {
                        mlua::Value::String(s) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    let log: mlua::Table = lua.globals().get("log")?;
                    log.push(format!("{name}({seen})"))?;
                    Ok(())
                })
                .unwrap()
        };
        let callbacks =
            PlainCallbacks { on_change: Some(record("change")), on_submit: Some(record("submit")), on_cancel: None };
        let edit = PlainEdit { changed: true, submitted: true, ..PlainEdit::NONE };
        deliver_plain_edit(&lua, "launcher@eDP-1", edit, "calc".to_string(), callbacks);

        let order: Vec<String> =
            lua.globals().get::<mlua::Table>("log").unwrap().sequence_values().collect::<mlua::Result<_>>().unwrap();
        assert_eq!(
            order,
            vec!["submit(calc)".to_string(), "change()".to_string()],
            "submit must carry the text, and the empty change must follow it"
        );
    }

    #[test]
    fn a_request_from_an_edit_callback_is_queued_with_the_ring() {
        let lua = mlua::Lua::new();
        crate::lua::focus::register(&lua).unwrap();
        lua.globals().set("target", lua.load("return focus_target('month')").eval::<mlua::Value>().unwrap()).unwrap();
        let request: Function = lua.load("return function() target:request() end").eval().unwrap();
        let edit = PlainEdit { changed: true, ..PlainEdit::NONE };
        let callbacks = PlainCallbacks { on_change: Some(request), on_submit: None, on_cancel: None };
        deliver_plain_edit(&lua, "form@TEST", edit, "12".into(), callbacks);
        let request = crate::lua::focus::take_request(&lua).expect("queued");
        assert_eq!((request.surface.as_str(), request.name.as_str(), request.ring), ("form@TEST", "month", true));
    }

    /// An ordinary keystroke is unaffected: `on_change` carries the text and nothing else fires.
    #[test]
    fn a_plain_keystroke_reports_the_text_through_on_change_alone() {
        let lua = mlua::Lua::new();
        lua.load("log = {}").exec().unwrap();
        let on_change = lua
            .create_function(|lua, text: String| {
                let log: mlua::Table = lua.globals().get("log")?;
                log.push(format!("change({text})"))?;
                Ok(())
            })
            .unwrap();
        let mut field = draft(1, "");
        field.on_change = Some(on_change);
        for action in [KeyAction::Append("a"), KeyAction::Append("b"), KeyAction::Undo, KeyAction::Redo] {
            let edit = field.edit(action, false, None, true);
            deliver_plain_edit(
                &lua,
                &field.surface_id,
                edit,
                field.buffer.clone(),
                PlainCallbacks { on_change: field.on_change.clone(), on_submit: None, on_cancel: None },
            );
        }
        assert_eq!(field.history.undo.len(), 1, "plain typing is one undo step");
        assert!(field.edit(KeyAction::Submit, false, None, true).submitted);
        assert!(field.history.undo.is_empty() && field.history.redo.is_empty());

        let order: Vec<String> =
            lua.globals().get::<mlua::Table>("log").unwrap().sequence_values().collect::<mlua::Result<_>>().unwrap();
        assert_eq!(order, ["change(a)", "change(ab)", "change()", "change(ab)"]);
    }

    #[test]
    fn an_armed_password_prompt_takes_the_keys_away_from_a_plain_field_that_was_typing() {
        // The two focuses are independent and `apply_key` offers a key to both. A password prompt
        // can appear while a plain reply field is already typing, so without this every character
        // of that password would also arrive at the reply field's `on_change`.
        assert!(plain_field_takes_keys(true, true, false));
        assert!(!plain_field_takes_keys(true, true, true), "the password is not also typed into the reply box");
        // Unchanged either way: a field no press chose, and one on a surface the keyboard left.
        assert!(!plain_field_takes_keys(false, true, false));
        assert!(!plain_field_takes_keys(true, false, false));
    }

    #[test]
    fn the_holder_is_a_focused_key_taking_field_and_has_text_selected_when_asked() {
        let named = |field: &FocusedTextField| Some(format!("f{}", field.buffer.len()));
        let selected = FocusedTextField { selection: (1, 3), ..draft(1, "abcd") };
        let collapsed = draft(1, "abcd");
        assert_eq!(focus_holder(None, true, false, named), None, "no field");
        assert_eq!(focus_holder(Some(&selected), false, false, named), None, "not taking keys");
        assert_eq!(focus_holder(Some(&collapsed), true, true, named), None, "a collapsed selection");
        assert_eq!(focus_holder(Some(&collapsed), true, false, named).as_deref(), Some("f4"), "paste needs none");
        assert_eq!(focus_holder(Some(&selected), true, true, named).as_deref(), Some("f4"));
        assert_eq!(focus_holder(Some(&selected), true, true, |_| None), None, "a field without a focus_target");
    }

    #[test]
    fn a_passed_escape_reaches_on_escape_with_the_draft_intact() {
        use super::super::focus::{plain_escape, surface_gets_escape};
        let reaches = |escape| surface_gets_escape(false, false, Some(plain_escape(escape, true, false, true)), None);
        assert!(reaches(Escape::Pass), "a field that passes Escape leaves it to the surface");
        assert!(!reaches(Escape::Clear), "a field with text keeps its Escape");
        assert!(!reaches(Escape::Blur), "blurring is the field's Escape even with text kept");
    }

    /// Arrows, paging and Tab are no edit, so a field passes them up. Tab arrives as `"\t"`, which
    /// the control filter must not turn into text.
    #[test]
    fn arrow_paging_and_tab_keys_are_not_edits() {
        for (keysym, utf8) in
            [(Keysym::Up, None), (Keysym::Down, None), (Keysym::Page_Down, None), (Keysym::Tab, Some("\t"))]
        {
            assert_eq!(key_action(&key(keysym, utf8), false, false, false), KeyAction::Ignore);
        }
    }

    fn autofocus_textfield(lua: &Lua) -> layout::ResolvedNode {
        let mut node = plain_textfield(lua);
        std::rc::Rc::make_mut(&mut node.properties).insert("autofocus", Value::Boolean(true));
        node
    }

    /// ADR-0112: the control the keyboard is handed to unasked. With two the first in document order
    /// does, since two search boxes on one surface is a mistake to pick through, not a secret to refuse.
    #[test]
    fn the_first_autofocus_field_or_control_in_the_scope_is_the_one_armed() {
        use super::super::tests::{button, with_property};
        let lua = Lua::new();
        let on = || Value::Boolean(true);
        let first = autofocus_textfield(&lua);
        let first_id = first.id;
        let tree = tree_with(&lua, vec![plain_textfield(&lua), first, autofocus_textfield(&lua)]);
        let found = autofocus_in_scope(&[("launcher@eDP-1", &tree)]).expect("armed");
        assert_eq!((found.surface_id.as_str(), found.id), ("launcher@eDP-1", first_id));
        assert_eq!(found.kind, super::focus::ControlKind::Plain);

        let control = with_property(button(&lua), "autofocus", on());
        let control_id = control.id;
        let tree = tree_with(&lua, vec![control, autofocus_textfield(&lua)]);
        let found = autofocus_in_scope(&[("dialog@TEST", &tree)]).expect("armed");
        assert_eq!((found.id, found.kind), (control_id, super::focus::ControlKind::Button));

        // Hidden, masked, callback-less, disabled, zero-size, or unnamed: never candidates, whatever they say.
        let mut hidden = autofocus_textfield(&lua);
        hidden.visible = false;
        let mut masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        std::rc::Rc::make_mut(&mut masked.properties).insert("autofocus", on());
        let key = || Value::Function(lua.create_function(|_, ()| Ok(())).unwrap());
        let name = || Value::String(lua.create_string("Field").unwrap());
        let masked = with_property(with_property(masked, "on_key", key()), "accessible_name", name());
        let mute = with_property(with_property(textfield(&lua, None), "on_key", key()), "accessible_name", name());
        let mute = with_property(mute, "autofocus", on());
        let disabled = with_property(autofocus_textfield(&lua), "disabled", on());
        let disabled = with_property(with_property(disabled, "on_key", key()), "accessible_name", name());
        let mut flat = with_property(button(&lua), "autofocus", on());
        flat.rect.width = 0.0;
        let unnamed = with_property(
            crate::wayland::input::tests::hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false),
            "autofocus",
            on(),
        );
        let none = tree_with(&lua, vec![hidden, masked, mute, disabled, flat, unnamed, plain_textfield(&lua)]);
        assert!(autofocus_in_scope(&[("launcher@eDP-1", &none)]).is_none());
    }

    #[test]
    fn autofocus_resets_a_typed_draft_to_the_fields_seed_or_empty() {
        let lua = Lua::new();
        let mut seeded = plain_textfield(&lua);
        std::rc::Rc::make_mut(&mut seeded.properties)
            .insert("initial_text", Value::String(lua.create_string("hi").unwrap()));
        let plain = plain_textfield(&lua);
        let (seeded_id, plain_id) = (seeded.id, plain.id);
        let tree = tree_with(&lua, vec![seeded, plain]);
        for (id, expected) in [(seeded_id, "hi"), (plain_id, "")] {
            let mut field = FocusedTextField { id, ..draft(1, "typed") };
            let seed = initial_text_of(Some(&tree), id, &lua);
            assert!(store_draft(&mut Parked::default(), Some(&mut field), "calendar@eDP-1", id, &seed));
            assert_eq!((field.buffer.as_str(), field.selection), (expected, (expected.len(), expected.len())));
        }
    }

    #[test]
    fn request_finds_only_a_visible_focusable_field_or_control_and_restores_a_field_caret() {
        use super::super::tests::{button, with_property};
        let lua = Lua::new();
        crate::lua::focus::register(&lua).unwrap();
        let handle = Value::UserData(lua.load("return focus_target('search')").eval().unwrap());
        let mut hidden = plain_textfield(&lua);
        hidden.visible = false;
        std::rc::Rc::make_mut(&mut hidden.properties).insert("focus_target", handle.clone());
        let mut masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        std::rc::Rc::make_mut(&mut masked.properties).insert("focus_target", handle.clone());
        let mut shown = plain_textfield(&lua);
        std::rc::Rc::make_mut(&mut shown.properties).insert("focus_target", handle);
        let id = shown.id;
        let tree = tree_with(&lua, vec![hidden, masked, shown]);
        let requested = |tree: &layout::ResolvedNode, name: &str| {
            let named = |node: &layout::ResolvedNode| {
                node::fields::common::focus_target.read(&node.properties).ok().flatten().as_deref() == Some(name)
            };
            super::focus::first_focusable("panel@TEST", tree, named)
        };
        assert_eq!(requested(&tree, "search").map(|control| control.id), Some(id));
        assert!(requested(&tree, "missing").is_none());
        let shown_field = || focused_field(&[&tree.children[1].children[2]]).expect("the shown field");

        // A control answers the same handle; a 0x0 one does not.
        let handle = Value::UserData(lua.load("return focus_target('go')").eval().unwrap());
        let shown = with_property(button(&lua), "focus_target", handle.clone());
        let shown_id = shown.id;
        let mut flat = with_property(button(&lua), "focus_target", handle);
        flat.rect.height = 0.0;
        let buttons = tree_with(&lua, vec![flat, shown]);
        assert_eq!(requested(&buttons, "go").map(|control| control.id), Some(shown_id));

        let mut previous = FocusedTextField {
            surface_id: "panel@TEST".into(),
            id,
            buffer: "draft".into(),
            selection: (2, 4),
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
        };
        previous.history.record((String::new(), (0, 0)), None);
        let target = shown_field();
        let resumed = requested_focus("panel@TEST".into(), target, Some(&previous));
        assert_eq!((resumed.buffer.as_str(), resumed.selection, resumed.typing), ("draft", (2, 4), true));
        assert!(resumed.history.undo.is_empty(), "a new focus request starts fresh history");
        let target = shown_field();
        let other = requested_focus("other@TEST".into(), target, Some(&previous));
        assert_eq!((other.buffer.as_str(), other.selection), ("", (0, 0)));
        assert!(other.history.undo.is_empty());
        let target = shown_field();
        let fresh_autofocus = requested_focus("panel@TEST".into(), target, None);
        assert!(fresh_autofocus.buffer.is_empty() && fresh_autofocus.history.undo.is_empty());
        let changed = requested_focus(
            "panel@TEST".into(),
            FieldTarget::Plain {
                id: layout::scene::NodeId::test(999),
                on_change: None,
                on_submit: None,
                on_cancel: None,
                escape: Escape::Clear,
            },
            Some(&previous),
        );
        assert!(changed.buffer.is_empty() && changed.history.undo.is_empty());
    }

    #[test]
    fn a_field_keeps_its_own_draft_while_another_has_focus() {
        let mut parked = Parked::new();
        let mut one = draft(1, "abc");
        one.selection = (1, 1);
        let mut two = draft(2, "");
        swap_drafts(&mut parked, Some(&one), Some(&mut two));
        assert!(two.buffer.is_empty(), "a field with no draft starts empty");
        two.buffer = "xy".into();
        two.selection = (2, 2);
        let mut back = draft(1, "");
        swap_drafts(&mut parked, Some(&two), Some(&mut back));
        assert_eq!((back.buffer.as_str(), back.selection), ("abc", (1, 1)));
        let mut again = draft(2, "");
        swap_drafts(&mut parked, Some(&back), Some(&mut again));
        assert_eq!((again.buffer.as_str(), again.selection), ("xy", (2, 2)));
        assert_eq!(parked.len(), 1, "the focused field's entry is consumed");
    }

    #[test]
    fn a_parked_draft_goes_with_its_node() {
        let lua = Lua::new();
        let kept = plain_textfield(&lua);
        let kept_id = kept.id;
        let tree = tree_with(&lua, vec![kept]);
        let mut parked = Parked::new();
        parked.insert(("panel@TEST".into(), kept_id), ("a".into(), (1, 1)));
        parked.insert(("panel@TEST".into(), layout::scene::NodeId::test(999)), ("b".into(), (1, 1)));
        forget_gone_drafts(&mut parked, |_, id| layout::hit::contains_node(&tree, id));
        assert_eq!(parked.keys().map(|(_, id)| *id).collect::<Vec<_>>(), vec![kept_id]);
    }

    #[test]
    fn set_text_rewrites_the_focused_draft_and_parks_for_any_other() {
        let mut parked = Parked::new();
        let mut field = draft(1, "old");
        field.history.record(("o".into(), (0, 0)), None);
        field.goal_x = Some(12.0);
        let id = field.id;
        assert!(store_draft(&mut parked, Some(&mut field), "calendar@eDP-1", id, "héllo"));
        assert_eq!((field.buffer.as_str(), field.selection, field.goal_x), ("héllo", (6, 6), None));
        assert!(field.history.undo.is_empty() && parked.is_empty());

        let other = layout::scene::NodeId::test(2);
        assert!(!store_draft(&mut parked, Some(&mut field), "calendar@eDP-1", other, "prefill"));
        assert_eq!(parked[&("calendar@eDP-1".to_string(), other)], ("prefill".to_string(), (7, 7)));
        assert!(!store_draft(&mut parked, None, "calendar@eDP-1", other, ""));
        assert!(parked.is_empty(), "an empty text clears a parked draft");
    }

    #[test]
    fn plain_fields_lists_plain_fields_with_the_name_and_hidden_ones_on_request() {
        let lua = Lua::new();
        crate::lua::focus::register(&lua).unwrap();
        let handle = Value::UserData(lua.load("return focus_target('q')").eval().unwrap());
        let named = |mut node: layout::ResolvedNode| {
            std::rc::Rc::make_mut(&mut node.properties).insert("focus_target", handle.clone());
            node
        };
        let (a, b) = (named(plain_textfield(&lua)), named(plain_textfield(&lua)));
        let mut hidden = named(plain_textfield(&lua));
        hidden.visible = false;
        let masked = named(textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))));
        let off = named(super::super::tests::with_property(plain_textfield(&lua), "disabled", Value::Boolean(true)));
        let (a_id, hidden_id, off_id, b_id) = (a.id, hidden.id, off.id, b.id);
        let tree = tree_with(&lua, vec![a, hidden, masked, off, b]);
        let ids = |hidden| -> Vec<_> {
            plain_fields(&tree, hidden, |_| true)
                .map(|target| match target {
                    FieldTarget::Plain { id, .. } => id,
                    FieldTarget::Masked { .. } => unreachable!(),
                })
                .collect()
        };
        assert_eq!(ids(false), vec![a_id, b_id], "focus skips hidden and disabled fields");
        assert_eq!(ids(true), vec![a_id, hidden_id, off_id, b_id], "set_text reaches both");
    }

    #[test]
    fn a_field_is_unusable_once_its_node_is_gone_or_disabled() {
        let lua = Lua::new();
        crate::lua::focus::register(&lua).unwrap();
        let (live, gone) = (plain_textfield(&lua), plain_textfield(&lua));
        let mut hidden = plain_textfield(&lua);
        hidden.visible = false;
        let off = super::super::tests::with_property(plain_textfield(&lua), "disabled", Value::Boolean(true));
        let (live_id, gone_id, hidden_id, off_id) = (live.id, gone.id, hidden.id, off.id);
        let tree = tree_with(&lua, vec![live, hidden, off]);
        assert!(!field_unusable(Some(&tree), live_id));
        assert!(!field_unusable(Some(&tree), hidden_id), "a hidden field keeps its parked draft");
        assert!(field_unusable(Some(&tree), off_id));
        assert!(field_unusable(Some(&tree), gone_id), "a removed row");
        assert!(field_unusable(None, live_id));
    }

    #[test]
    fn leaving_for_no_field_parks_the_text_but_a_cleared_field_parks_nothing() {
        let mut parked = Parked::new();
        swap_drafts(&mut parked, Some(&draft(1, "abc")), None);
        assert_eq!(parked.len(), 1, "focus dropped with text in the field keeps it");
        swap_drafts(&mut parked, Some(&draft(1, "")), None);
        assert!(parked.is_empty(), "Enter or Escape emptied it, so a leave must not bring the text back");
    }

    #[test]
    fn node_mask_subtree_cannot_take_plain_or_secure_keyboard_focus() {
        let lua = Lua::new();
        let mut plain = plain_textfield(&lua);
        std::rc::Rc::make_mut(&mut plain.properties).insert("autofocus", Value::Boolean(true));
        let secure = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let shape = layout::ResolvedNode {
            id: layout::scene::NodeId::test(987),
            ..layout::ResolvedNode::test("rect", (0.0, 0.0, 20.0, 20.0), vec![plain, secure])
        };
        let root = layout::ResolvedNode {
            mask_target: Some(shape.id),
            ..layout::ResolvedNode::test("rect", (0.0, 0.0, 20.0, 20.0), vec![shape])
        };
        assert!(plain_fields(&root, false, |_| true).next().is_none());
        assert!(autofocus_in_scope(&[("bar", &root)]).is_none());
        assert!(layout::secure_submit::typable_secure_submit_targets(&root).is_empty());
    }
}

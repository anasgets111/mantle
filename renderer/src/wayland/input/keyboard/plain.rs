//! Plain `textfield`s: which one `autofocus` arms, the draft and selection a key edits, the
//! caret's blink, and the edits delivered to Lua (ADR-0092).

use shared::debug;
use unicode_segmentation::UnicodeSegmentation;

use super::*;
use crate::layout::node::prop::Keyword;
use crate::lua::call_logged;

#[derive(Debug, Clone, Default)]
pub(in crate::wayland::input) struct EditHistory {
    undo: Vec<(String, (usize, usize))>,
    redo: Vec<(String, (usize, usize))>,
    typing_end: Option<usize>,
}

impl EditHistory {
    const LIMIT: usize = 1_048_576;

    pub(in crate::wayland::input) fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.typing_end = None;
    }

    pub(in crate::wayland::input) fn break_typing(&mut self) {
        self.typing_end = None;
    }

    fn trim(&mut self) {
        while self.undo.len() + self.redo.len() > 100
            || self.undo.iter().chain(&self.redo).map(|(text, _)| text.len()).sum::<usize>() > Self::LIMIT
        {
            if !self.undo.is_empty() {
                self.undo.remove(0);
            } else {
                self.redo.remove(0);
            }
        }
    }

    fn record(&mut self, snapshot: (String, (usize, usize)), typed_to: Option<usize>) {
        self.redo.clear();
        if typed_to.is_some() && self.typing_end == Some(snapshot.1.1) && snapshot.1.0 == snapshot.1.1 {
            self.typing_end = typed_to;
            return;
        }
        self.typing_end = typed_to;
        // ponytail: 100 snapshots or 1 MiB of text; use edit deltas if long drafts need deeper history.
        if snapshot.0.len() > Self::LIMIT {
            self.undo.clear();
            self.typing_end = None;
            return;
        }
        self.undo.push(snapshot);
        self.trim();
    }

    fn restore(&mut self, text: &mut String, selection: &mut (usize, usize), redo: bool) -> bool {
        self.typing_end = None;
        let (from, to) = if redo { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        let Some((previous, previous_selection)) = from.pop() else { return false };
        let current = (std::mem::replace(text, previous), std::mem::replace(selection, previous_selection));
        if current.0.len() > Self::LIMIT {
            self.clear();
            return true;
        }
        to.push(current);
        self.trim();
        true
    }
}

/// First plain `autofocus = true` field in scope document order (ADR-0112). Skip masked fields and
/// fields without callbacks; unlike two `secure_submit` fields, duplicate search boxes are a config
/// mistake, so deterministic order beats refusing both. A hidden subtree is skipped whole: it is
/// frozen (ADR-0124) and cannot take keys, and one surface that holds several modals' cards keeps
/// the closed ones hidden beside the open one.
fn autofocus_field_in_scope(scope: &[(&str, &layout::ResolvedNode)]) -> Option<(String, FieldTarget)> {
    for (surface_id, tree) in scope {
        if let Some(target) =
            first_plain_field(tree, |node| node::fields::textfield::autofocus.read(&node.properties).is_ok_and(|on| on))
        {
            return Some((surface_id.to_string(), target));
        }
    }
    None
}

/// First visible plain field bound to `name` on this one surface.
fn requested_field(tree: &layout::ResolvedNode, name: &str) -> Option<FieldTarget> {
    first_plain_field(tree, |node| {
        node::fields::textfield::focus_target.read(&node.properties).ok().flatten().as_deref() == Some(name)
    })
}

fn first_plain_field(
    tree: &layout::ResolvedNode,
    matches: impl Fn(&layout::ResolvedNode) -> bool,
) -> Option<FieldTarget> {
    plain_fields(tree, false, matches).next()
}

/// Plain fields passing `matches`, in document order; hidden subtrees only when `hidden`.
fn plain_fields<'a>(
    tree: &'a layout::ResolvedNode,
    hidden: bool,
    matches: impl Fn(&layout::ResolvedNode) -> bool + 'a,
) -> impl Iterator<Item = FieldTarget> + 'a {
    let mut stack = vec![tree];
    std::iter::from_fn(move || {
        while let Some(node) = stack.pop() {
            if (!node.visible && !hidden) || node.leaving {
                continue;
            }
            stack.extend(node.content_children().rev());
            if node.kind == "textfield"
                && matches(node)
                && let Some(target @ FieldTarget::Plain { .. }) = focused_field(&[node])
            {
                return Some(target);
            }
        }
        None
    })
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
        (field.buffer, field.selection) = (text.to_owned(), end);
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
    let FieldTarget::Plain { id, on_change, on_submit, on_cancel, on_navigate } = target else {
        unreachable!("requested_field only returns plain targets")
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
        on_change,
        on_submit,
        on_cancel,
        on_navigate,
    }
}

/// One plain-field key edit before callbacks (ADR-0092, ADR-0102); split from
/// [`App::apply_plain_key`] so Escape is testable without a Wayland seat.
#[derive(Debug, PartialEq, Eq)]
struct PlainEdit {
    /// Buffer text changed, so `on_change` fires.
    changed: bool,
    /// Enter submits the text and empties the buffer.
    submitted: bool,
    /// Escape with `on_cancel` drops focus and fires it.
    cancelled: bool,
    /// Up, down, Tab, or paging key: buffer stays; `on_navigate` hears the name.
    navigated: Option<NavigateKey>,
    /// Caret or selection moved with the text unchanged: repaint, tell the config nothing.
    moved: bool,
}

impl PlainEdit {
    /// No-op edit, allowing [`App::apply_plain_key`] to return early.
    const NONE: PlainEdit =
        PlainEdit { changed: false, submitted: false, cancelled: false, navigated: None, moved: false };
}

/// `text` cut on a grapheme boundary to what `max_length` leaves after `kept` clusters stay.
/// Counting clusters is how the caret and erase already see characters (ADR-0236).
pub(in crate::wayland::input) fn fit_to_limit(text: &str, max: Option<usize>, kept: usize) -> &str {
    let Some(max) = max else { return text };
    text.grapheme_indices(true).nth(max.saturating_sub(kept)).map_or(text, |(at, _)| &text[..at])
}

/// An insert of `text` over `replaced` in `buffer`, cut to `max_length`. Over a selection a fully
/// cut insert would delete it, so that case is refused whole.
fn limited_append<'a>(buffer: &str, replaced: (usize, usize), text: &'a str, max: Option<usize>) -> KeyAction<'a> {
    let (from, to) = (replaced.0.min(replaced.1), replaced.0.max(replaced.1));
    let kept = buffer[..from].graphemes(true).count() + buffer[to..].graphemes(true).count();
    match fit_to_limit(text, max, kept) {
        "" if !text.is_empty() => KeyAction::Ignore,
        fitted => KeyAction::Append(fitted),
    }
}

/// Byte offset one word before `at`, taking the run of spaces before that word with it so a single
/// Ctrl+Backspace crosses the gap and the word together.
fn previous_word(text: &str, at: usize) -> usize {
    text[..at].split_word_bound_indices().rfind(|(_, word)| !word.trim().is_empty()).map_or(0, |(start, _)| start)
}

/// Byte offset one word after `at`, taking the spaces before that word with it.
fn next_word(text: &str, at: usize) -> usize {
    text[at..]
        .split_word_bound_indices()
        .find(|(_, word)| !word.trim().is_empty())
        .map_or(text.len(), |(start, word)| at + start + word.len())
}

/// Byte offset of the grapheme cluster boundary before `at`, or the start of `text`.
fn previous_boundary(text: &str, at: usize) -> usize {
    text[..at].grapheme_indices(true).next_back().map_or(0, |(start, _)| start)
}

/// Byte offset of the grapheme cluster boundary after `at`, or `at` at the end of `text`.
fn next_boundary(text: &str, at: usize) -> usize {
    text[at..].graphemes(true).next().map_or(at, |cluster| at + cluster.len())
}

fn ime_range(text: &str, selection: (usize, usize), before: u32, after: u32) -> Option<(usize, usize)> {
    let (from, to) = (selection.0.min(selection.1), selection.0.max(selection.1));
    let start = from.checked_sub(before as usize)?;
    let end = to.checked_add(after as usize)?;
    (end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)).then_some((start, end))
}

fn ime_change<'a>(
    text: &str,
    selection: (usize, usize),
    delete: (u32, u32),
    commit: Option<&'a str>,
) -> Option<((usize, usize), Option<&'a str>)> {
    if delete == (0, 0) && commit.is_none_or(str::is_empty) {
        return None;
    }
    if commit.is_some_and(|text| text.chars().any(char::is_control)) {
        return None;
    }
    let range = ime_range(text, selection, delete.0, delete.1)?;
    Some((range, commit.filter(|text| !text.is_empty())))
}

/// Apply `action` to `buffer` at `selection`. Insertion and deletion act on the selection, or at
/// the caret when there is none; `shift` extends the selection instead of collapsing it.
/// Escape always clears; with `on_cancel` it also leaves (ADR-0092 decision 6), otherwise the
/// config cannot know the field stopped taking keys.
fn edit_plain_buffer(
    buffer: &mut String,
    selection: &mut (usize, usize),
    action: KeyAction<'_>,
    shift: bool,
    cancels: bool,
) -> PlainEdit {
    let (anchor, caret) = *selection;
    let (from, to) = (anchor.min(caret), anchor.max(caret));
    match action {
        KeyAction::Append(text) => {
            let changed = from < to || !text.is_empty();
            buffer.replace_range(from..to, text);
            *selection = (from + text.len(), from + text.len());
            PlainEdit { changed, ..PlainEdit::NONE }
        }
        // A selection is what one erase removes; without one it reaches as far as the key asked,
        // and one cluster is what the user sees as one character (ADR-0236).
        KeyAction::Erase(reach) => {
            let (from, to) = match from < to {
                true => (from, to),
                false => match reach {
                    Motion::Left => (previous_boundary(buffer, caret), caret),
                    Motion::Right => (caret, next_boundary(buffer, caret)),
                    Motion::WordLeft => (previous_word(buffer, caret), caret),
                    Motion::WordRight => (caret, next_word(buffer, caret)),
                    Motion::Start => (0, caret),
                    Motion::End => (caret, buffer.len()),
                },
            };
            buffer.replace_range(from..to, "");
            *selection = (from, from);
            PlainEdit { changed: from < to, ..PlainEdit::NONE }
        }
        KeyAction::Clear => {
            let had = !buffer.is_empty();
            buffer.clear();
            *selection = (0, 0);
            PlainEdit { changed: had, cancelled: cancels, ..PlainEdit::NONE }
        }
        KeyAction::Submit => PlainEdit { changed: true, submitted: true, ..PlainEdit::NONE },
        KeyAction::Move(motion) => {
            let moved_to = match motion {
                // An unshifted arrow with a selection lands on its edge instead of stepping past it.
                Motion::Left if from < to && !shift => from,
                Motion::Right if from < to && !shift => to,
                Motion::Left => previous_boundary(buffer, caret),
                Motion::Right => next_boundary(buffer, caret),
                Motion::WordLeft => previous_word(buffer, caret),
                Motion::WordRight => next_word(buffer, caret),
                Motion::Start => 0,
                Motion::End => buffer.len(),
            };
            let next = (if shift { anchor } else { moved_to }, moved_to);
            let moved = next != *selection;
            *selection = next;
            // An arrow the caret cannot take goes to the config, so a grid under the field can use it.
            let navigated = match motion {
                Motion::Left if !moved && !shift => Some(NavigateKey::Left),
                Motion::Right if !moved && !shift => Some(NavigateKey::Right),
                _ => None,
            };
            PlainEdit { moved, navigated, ..PlainEdit::NONE }
        }
        KeyAction::SelectAll => {
            let next = (0, buffer.len());
            let moved = next != *selection;
            *selection = next;
            PlainEdit { moved, ..PlainEdit::NONE }
        }
        KeyAction::Navigate(key) => PlainEdit { navigated: Some(key), ..PlainEdit::NONE },
        KeyAction::Undo | KeyAction::Redo | KeyAction::Ignore => PlainEdit::NONE,
    }
}

impl FocusedTextField {
    fn edit(
        &mut self,
        action: KeyAction<'_>,
        shift: bool,
        ime_range: Option<(usize, usize)>,
        from_key: bool,
    ) -> PlainEdit {
        match action {
            action @ (KeyAction::Undo | KeyAction::Redo) => PlainEdit {
                changed: self.history.restore(&mut self.buffer, &mut self.selection, matches!(action, KeyAction::Redo)),
                ..PlainEdit::NONE
            },
            action => {
                let typed = from_key
                    && matches!(&action, KeyAction::Append(text) if text.chars().count() == 1)
                    && self.selection.0 == self.selection.1;
                let before = matches!(&action, KeyAction::Append(_) | KeyAction::Erase(_) | KeyAction::Clear)
                    .then(|| (self.buffer.clone(), self.selection));
                if let Some(range) = ime_range {
                    self.selection = range;
                }
                let edit =
                    edit_plain_buffer(&mut self.buffer, &mut self.selection, action, shift, self.on_cancel.is_some());
                if edit.changed
                    && !edit.cancelled
                    && let Some(snapshot) = before
                {
                    self.history.record(snapshot, typed.then_some(self.selection.1));
                }
                if !typed {
                    self.history.break_typing();
                }
                if edit.submitted || edit.cancelled {
                    self.history.clear();
                }
                edit
            }
        }
    }

    fn delete_surrounding(&mut self, (start, end): (usize, usize)) -> PlainEdit {
        let (from, to) = (self.selection.0.min(self.selection.1), self.selection.0.max(self.selection.1));
        let before = (self.buffer.clone(), self.selection);
        self.buffer.replace_range(to..end, "");
        self.buffer.replace_range(start..from, "");
        let removed_before = from - start;
        self.selection = (self.selection.0 - removed_before, self.selection.1 - removed_before);
        self.history.record(before, None);
        PlainEdit { changed: true, ..PlainEdit::NONE }
    }
}

/// Whether a plain field takes the keys arriving now. A masked field armed anywhere in scope takes
/// them all: the two focuses are held independently, and `apply_key` offers a key to both, so a
/// prompt revealed while a plain field was already typing would otherwise put every character of a
/// password through that field's `on_change` -- into Lua, which is the one place a `secure_submit`
/// secret must never reach (ADR-0005). "Masked focus wins" is the rule
/// `arm_autofocus_if_nothing_is_typing`
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
fn deliver_plain_edit(surface_id: &str, edit: PlainEdit, text: String, callbacks: PlainCallbacks) {
    let PlainCallbacks { on_change, on_submit, on_cancel } = callbacks;
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

impl App {
    /// A pointer press already stopped typing. Restore it after the click's state has resolved,
    /// before autofocus and repaint, so a newly shown field can receive the next key.
    pub(in crate::wayland) fn apply_focus_request(&mut self) {
        let Some((surface_id, name)) = crate::lua::focus::take_request(self.client.lua()) else {
            return;
        };
        if !self.keyboard_focus_scope().contains(&surface_id)
            || !self.surface_is_live(&surface_id)
            || self.focused_secure_submit.is_some()
        {
            return;
        }
        let Some(target) = self.client.scene().surface(&surface_id).and_then(|tree| requested_field(tree, &name))
        else {
            return;
        };
        self.focus_text_field(Some(requested_focus(surface_id, target, self.focused_text_field.as_ref())));
        let control = self.focused_text_field.as_ref().map(|field| super::focus::FocusedControl {
            surface_id: field.surface_id.clone(),
            id: field.id,
            kind: super::focus::ControlKind::Plain,
        });
        self.set_control_focus(control);
    }

    /// Applies `focus_target(name):set_text` to the fields bound to `name`, hidden ones too: the
    /// focused one is rewritten in place, any other has its parked draft replaced (or dropped for `""`).
    pub(in crate::wayland) fn apply_text_requests(&mut self) {
        for (name, text) in crate::lua::focus::take_texts(self.client.lua()) {
            let named = |node: &layout::ResolvedNode| {
                node::fields::textfield::focus_target.read(&node.properties).ok().flatten().as_deref() == Some(&*name)
            };
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
                self.set_draft(surface_id, id, &text);
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
    }

    /// Give keys to `autofocus` with a fresh empty buffer (ADR-0112). ADR-0108 preserves drafts
    /// when the user returns manually; automatic handoff must not append to a forgotten search.
    /// Fire `on_change("")` on every arm so launchers reset selection/scroll and state clears.
    pub(super) fn arm_autofocus_field(&mut self, scope: &[String]) {
        let trees = self.scoped_trees(scope);
        let Some((surface_id, FieldTarget::Plain { id, on_change, on_submit, on_cancel, on_navigate })) =
            autofocus_field_in_scope(&trees)
        else {
            return;
        };
        drop(trees);
        // A closed launcher can retain its tree and focus id without a `leave`; require its live
        // `wl_surface` or every turn would arm then prune the same field.
        if !self.surface_is_live(&surface_id) {
            return;
        }
        let opened = on_change.clone();
        debug!("{surface_id}'s `autofocus` textfield takes the keyboard");
        // Autofocus starts empty, so a parked draft must not come back.
        self.parked_drafts.remove(&(surface_id.clone(), id));
        self.focus_text_field(Some(requested_focus(
            surface_id.clone(),
            FieldTarget::Plain { id, on_change, on_submit, on_cancel, on_navigate },
            None,
        )));
        self.set_control_focus(Some(super::focus::FocusedControl {
            surface_id: surface_id.clone(),
            id,
            kind: super::focus::ControlKind::Plain,
        }));
        if let Some(on_change) = opened {
            call_logged(&on_change, String::new(), format_args!("{surface_id}: on_change"));
        }
    }

    /// Arm a newly appearing `autofocus` field under existing focus (ADR-0112), unless a plain
    /// field is typing or a press just stopped that same field. A different field is new; masked
    /// focus wins as on `enter`.
    pub(in crate::wayland) fn arm_autofocus_if_nothing_is_typing(&mut self, scope: &[String]) {
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
                autofocus_field_in_scope(&trees),
                Some((_, FieldTarget::Plain { id, .. })) if id == field.id
            );
            if same_field {
                return;
            }
        }
        self.arm_autofocus_field(scope);
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
    }

    /// [`App::prune_secure_focus`]'s counterpart. The same two clauses -- the surface is still
    /// alive, and it is still one the keyboard can reach -- because a plain field goes stale for
    /// exactly the reasons a masked one does. What it does not share is the urgency: dropping a
    /// half-typed reply loses a sentence, not a secret, so there is no once-a-turn sweep matching
    /// [`App::drop_secure_focus_if_its_surface_is_gone`]; the check before each keystroke is
    /// enough, and a `leave` clears it anyway.
    pub(in crate::wayland) fn prune_text_field_focus(&mut self) {
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
        // A disabled field is let go like a removed one, except that its draft stays parked.
        let node_exists = self.client.scene().surface(&field.surface_id).is_some_and(|tree| {
            layout::hit::contains_node(tree, field.id)
                && layout::hit::path_to_node(tree, field.id)
                    .and_then(|path| path.last().copied())
                    .is_some_and(|node| !node.paint.as_ref().is_some_and(node::PaintStyle::is_disabled_field))
        });
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
    pub(super) fn apply_plain_key(&mut self, event: &KeyEvent, repeat: bool) {
        let action = key_action(event, repeat, self.ctrl_held, self.shift_held);
        if matches!(action, KeyAction::Append(_)) && self.text_input.owns_text() {
            return;
        }
        if matches!(action, KeyAction::Clear) {
            self.cancel_text_input_composition();
        }
        self.apply_plain_action_inner(action, None, true);
    }

    pub(in crate::wayland::input) fn apply_ime_edit(&mut self, delete: (u32, u32), commit: Option<&str>) {
        let Some(field) = self.focused_text_field.as_ref().filter(|field| self.text_field_takes_keys(field)) else {
            return;
        };
        let Some((range, text)) = ime_change(&field.buffer, field.selection, delete, commit) else { return };
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
    ) {
        if !self.focused_text_field.as_ref().is_some_and(|field| self.text_field_takes_keys(field)) {
            return;
        }
        // Read before the borrow below: shift turns a caret motion into a selection.
        let shift = self.shift_held;
        let Some(field) = self.focused_text_field.as_ref() else {
            return;
        };
        let action = match action {
            KeyAction::Append(text) => limited_append(
                &field.buffer,
                ime_range.unwrap_or(field.selection),
                text,
                self.field_max_length(&field.surface_id, field.id),
            ),
            action => action,
        };
        let Some(field) = self.focused_text_field.as_mut() else {
            return;
        };
        let edit = field.edit(action, shift, ime_range, from_key);
        self.finish_plain_edit(edit, ime_range.is_none());
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
            return;
        }
        // Clone before callbacks can write a signal and re-resolve the scene.
        let (text, on_change, on_submit, on_cancel, on_navigate, surface_id) = {
            let field = self.focused_text_field.as_ref().expect("the focus was Some a moment ago");
            (
                field.buffer.clone(),
                field.on_change.clone(),
                field.on_submit.clone(),
                field.on_cancel.clone(),
                field.on_navigate.clone(),
                field.surface_id.clone(),
            )
        };
        // Navigation changes neither text nor caret, so it needs no repaint.
        if let Some(key) = edit.navigated {
            if let Some(on_navigate) = on_navigate {
                call_logged(&on_navigate, key.name(), format_args!("{surface_id}: on_navigate"));
            }
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
        deliver_plain_edit(&surface_id, edit, text, PlainCallbacks { on_change, on_submit, on_cancel });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{draft, key, plain_textfield, secure_submit_table, textfield, tree_with};
    use super::*;

    #[test]
    fn undo_and_redo_restore_text_and_selection_and_new_edit_drops_redo() {
        let mut history = EditHistory::default();
        let mut text = "ab".to_string();
        let mut selection = (1, 1);
        history.record((text.clone(), selection), None);
        edit_plain_buffer(&mut text, &mut selection, KeyAction::Append("X"), false, false);
        assert_eq!((text.as_str(), selection), ("aXb", (2, 2)));
        assert!(history.restore(&mut text, &mut selection, false));
        assert_eq!((text.as_str(), selection), ("ab", (1, 1)));
        assert!(history.restore(&mut text, &mut selection, true));
        assert_eq!((text.as_str(), selection), ("aXb", (2, 2)));
        assert!(history.restore(&mut text, &mut selection, false));
        history.record((text.clone(), selection), None);
        assert!(!history.restore(&mut text, &mut selection, true));
    }

    #[test]
    fn undo_history_has_a_fixed_ceiling_and_ignores_oversized_drafts() {
        let mut history = EditHistory::default();
        for i in 0..101 {
            history.record((i.to_string(), (0, 0)), None);
        }
        assert_eq!(history.undo.len(), 100);
        history.record(("x".repeat(1_048_577), (0, 0)), None);
        assert!(history.undo.is_empty());

        history.record(("small".into(), (0, 0)), None);
        let mut text = "x".repeat(1_048_577);
        let mut selection = (0, 0);
        assert!(history.restore(&mut text, &mut selection, false));
        assert_eq!(text, "small");
        assert!(history.undo.is_empty() && history.redo.is_empty());
    }

    #[test]
    fn undo_and_redo_share_the_byte_ceiling() {
        let mut history = EditHistory::default();
        let mut text = "c".repeat(650_000);
        let mut selection = (0, 0);
        history.record(("a".repeat(450_000), selection), None);
        history.record(("b".repeat(450_000), selection), None);
        assert!(history.restore(&mut text, &mut selection, false));
        assert_eq!(text.len(), 450_000);
        assert!(history.undo.is_empty(), "the transfer evicts the older undo entry");
        assert_eq!(history.redo[0].0.len(), 650_000);
        let bytes = history.undo.iter().chain(&history.redo).map(|(text, _)| text.len()).sum::<usize>();
        assert!(bytes <= EditHistory::LIMIT);
        assert!(history.restore(&mut text, &mut selection, true));
        assert_eq!(text.len(), 650_000);
        history.record(("d".repeat(600_000), selection), None);
        assert_eq!(history.undo.len(), 1, "recording evicts across the aggregate byte ceiling");
        let bytes = history.undo.iter().chain(&history.redo).map(|(text, _)| text.len()).sum::<usize>();
        assert!(bytes <= EditHistory::LIMIT);

        let mut history = EditHistory {
            redo: vec![("a".repeat(450_000), selection), ("b".repeat(450_000), selection)],
            ..EditHistory::default()
        };
        let mut text = "c".repeat(650_000);
        assert!(history.restore(&mut text, &mut selection, true));
        assert!(history.undo.is_empty(), "redo transfer also enforces the aggregate ceiling");
        assert_eq!(history.redo.len(), 1);
    }

    #[test]
    fn empty_ime_transaction_cannot_replace_a_selection() {
        assert_eq!(ime_change("abc", (1, 2), (0, 0), None), None);
        assert_eq!(ime_change("abc", (1, 2), (0, 0), Some("")), None);
        assert_eq!(ime_change("abc", (1, 2), (0, 0), Some("語")), Some(((1, 2), Some("語"))));
    }

    #[test]
    fn consecutive_typing_coalesces_but_a_paste_starts_a_new_snapshot() {
        let mut history = EditHistory::default();
        history.record((String::new(), (0, 0)), Some(1));
        history.record(("a".to_string(), (1, 1)), Some(2));
        assert_eq!(history.undo.len(), 1);
        history.record(("ab".to_string(), (2, 2)), None);
        assert_eq!(history.undo.len(), 2);
        let mut text = "ab!".to_string();
        let mut selection = (3, 3);
        assert!(history.restore(&mut text, &mut selection, false));
        assert_eq!((text.as_str(), selection), ("ab", (2, 2)));
        assert!(history.restore(&mut text, &mut selection, false));
        assert_eq!((text.as_str(), selection), ("", (0, 0)));
    }

    #[test]
    fn ime_deletion_requires_utf8_boundaries_and_replaces_the_selection() {
        let mut text = "aé文z".to_string();
        let mut selection = (3, 6);
        assert_eq!(ime_range(&text, selection, 1, 1), None);
        let (start, end) = ime_range(&text, selection, 2, 1).unwrap();
        assert_eq!((start, end), (1, 7));
        selection = (start, end);
        let edit = edit_plain_buffer(&mut text, &mut selection, KeyAction::Append("語"), false, false);
        assert!(edit.changed);
        assert_eq!((text.as_str(), selection), ("a語", (4, 4)));
    }

    #[test]
    fn delete_only_ime_transaction_preserves_selection() {
        let mut field = draft(1, "abcd");
        field.selection = (1, 3);
        let (range, commit) = ime_change(&field.buffer, field.selection, (1, 0), None).unwrap();
        assert_eq!((range, commit), ((0, 3), None));
        let edit = field.delete_surrounding(range);
        assert!(edit.changed);
        assert_eq!((field.buffer.as_str(), field.selection), ("bcd", (0, 2)));

        let mut field = draft(1, "aé文z");
        field.selection = (3, 3);
        let (range, _) = ime_change(&field.buffer, field.selection, (2, 0), None).unwrap();
        field.delete_surrounding(range);
        assert_eq!((field.buffer.as_str(), field.selection), ("a文z", (1, 1)));
    }

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
        deliver_plain_edit("launcher@eDP-1", edit, "calc".to_string(), callbacks);

        let order: Vec<String> =
            lua.globals().get::<mlua::Table>("log").unwrap().sequence_values().collect::<mlua::Result<_>>().unwrap();
        assert_eq!(
            order,
            vec!["submit(calc)".to_string(), "change()".to_string()],
            "submit must carry the text, and the empty change must follow it"
        );
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

    /// [`edit_plain_buffer`] with the caret at the end of the buffer and no Shift held, which is
    /// where an append-only field always had it.
    fn edit_at_end(buffer: &mut String, action: KeyAction<'_>, cancels: bool) -> PlainEdit {
        let mut selection = (buffer.len(), buffer.len());
        edit_plain_buffer(buffer, &mut selection, action, false, cancels)
    }

    #[test]
    fn escape_on_a_plain_field_without_on_cancel_clears_and_keeps_the_focus() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, false);
        assert_eq!(
            edit,
            PlainEdit { changed: true, submitted: false, cancelled: false, navigated: None, moved: false }
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn escape_on_a_plain_field_with_on_cancel_clears_and_gives_the_field_up() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, true);
        assert_eq!(edit, PlainEdit { changed: true, submitted: false, cancelled: true, navigated: None, moved: false });
        assert!(buffer.is_empty());
    }

    /// An empty field has nothing for `on_change` to report, but Escape is still a cancel: the
    /// field was open and the user asked to leave it.
    #[test]
    fn escape_on_an_empty_field_cancels_without_reporting_a_change() {
        let mut buffer = String::new();
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, true);
        assert_eq!(
            edit,
            PlainEdit { changed: false, submitted: false, cancelled: true, navigated: None, moved: false }
        );
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, false);
        assert_eq!(
            edit,
            PlainEdit { changed: false, submitted: false, cancelled: false, navigated: None, moved: false },
            "nothing at all to do"
        );
    }

    /// One Backspace removes one character as the user sees it, whatever it is made of: a base
    /// letter and its combining acute, or the five scalars and two zero-width joiners of a family
    /// emoji. Deleting a scalar left a bare acute and a lone woman behind (ADR-0236).
    #[test]
    fn backspace_deletes_a_whole_grapheme_cluster() {
        let mut buffer = "e\u{301}\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}".to_string();
        let mut selection = (buffer.len(), buffer.len());
        let edit = |buffer: &mut String, selection: &mut (usize, usize)| {
            edit_plain_buffer(buffer, selection, KeyAction::Erase(Motion::Left), false, false)
        };

        assert_eq!(edit(&mut buffer, &mut selection), PlainEdit { changed: true, ..PlainEdit::NONE });
        assert_eq!(buffer, "e\u{301}", "the whole family goes, joiners and all");
        assert_eq!(edit(&mut buffer, &mut selection), PlainEdit { changed: true, ..PlainEdit::NONE });
        assert!(buffer.is_empty(), "the combining acute leaves with the letter it sits on");
        assert_eq!(edit(&mut buffer, &mut selection), PlainEdit::NONE, "Backspace on an empty field does nothing");
        assert_eq!(selection, (0, 0));
    }

    /// Delete reaches forward, Ctrl reaches a whole word, and a word takes the space before it so
    /// one press crosses the gap and the word together.
    #[test]
    fn an_erase_reaches_as_far_as_the_key_asked() {
        assert_eq!(key_action(&key(Keysym::Delete, None), false, false, false), KeyAction::Erase(Motion::Right));
        assert_eq!(key_action(&key(Keysym::BackSpace, None), false, true, false), KeyAction::Erase(Motion::WordLeft));
        assert_eq!(key_action(&key(Keysym::Delete, None), false, true, false), KeyAction::Erase(Motion::WordRight));

        let (mut buffer, mut selection) = ("on my way".to_string(), (9, 9));
        let word = |buffer: &mut String, selection: &mut (usize, usize), reach| {
            edit_plain_buffer(buffer, selection, KeyAction::Erase(reach), false, false);
        };
        word(&mut buffer, &mut selection, Motion::WordLeft);
        assert_eq!(buffer, "on my ");
        word(&mut buffer, &mut selection, Motion::WordLeft);
        assert_eq!(buffer, "on ", "the second press takes `my` and the space it sat behind");

        let (mut buffer, mut selection) = ("on my way".to_string(), (0, 0));
        word(&mut buffer, &mut selection, Motion::WordRight);
        assert_eq!(buffer, " my way", "forward from the start takes the first word alone");

        // Forward from the end, and backward from the start, have nothing to take.
        let (mut buffer, mut selection) = ("hi".to_string(), (2, 2));
        word(&mut buffer, &mut selection, Motion::WordRight);
        assert_eq!(buffer, "hi");
        selection = (0, 0);
        word(&mut buffer, &mut selection, Motion::WordLeft);
        assert_eq!(buffer, "hi");
    }

    /// Ctrl reaches named editing bindings; every other chord stays the compositor's to bind.
    #[test]
    fn ctrl_a_selects_the_draft_and_no_other_chord_is_taken() {
        assert_eq!(key_action(&key(Keysym::a, Some("a")), false, true, false), KeyAction::SelectAll);
        assert_eq!(key_action(&key(Keysym::c, Some("c")), false, true, false), KeyAction::Ignore, "Ctrl+C is not ours");
        assert_eq!(
            key_action(&key(Keysym::a, Some("a")), false, false, false),
            KeyAction::Append("a"),
            "and plain a types"
        );

        let mut buffer = "on my way".to_string();
        let mut selection = (3, 3);
        let edit = edit_plain_buffer(&mut buffer, &mut selection, KeyAction::SelectAll, false, false);
        assert_eq!(selection, (0, "on my way".len()), "anchor at the start, caret at the end");
        assert!(edit.moved, "and the field repaints");

        // Backspace then takes the whole thing, which is what select-all is for.
        edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Erase(Motion::Left), false, false);
        assert_eq!(buffer, "");
    }

    #[test]
    fn the_caret_steps_over_a_composed_character_in_one_move() {
        let buffer = "ae\u{301}b".to_string();
        let mut selection = (1, 1);
        let move_to = |selection: &mut (usize, usize), motion, shift| {
            edit_plain_buffer(&mut buffer.clone(), selection, KeyAction::Move(motion), shift, false)
        };

        assert_eq!(move_to(&mut selection, Motion::Right, false), PlainEdit { moved: true, ..PlainEdit::NONE });
        assert_eq!(selection, (4, 4), "past the `e` and its acute together");
        move_to(&mut selection, Motion::Left, false);
        assert_eq!(selection, (1, 1), "and back in one press");
        move_to(&mut selection, Motion::End, false);
        assert_eq!(selection, (5, 5));
        assert_eq!(move_to(&mut selection, Motion::End, false), PlainEdit::NONE, "already there");
        // Shift leaves the anchor behind, which is what makes a selection.
        move_to(&mut selection, Motion::Left, true);
        assert_eq!(selection, (5, 4));
        move_to(&mut selection, Motion::Start, true);
        assert_eq!(selection, (5, 0), "Home with Shift selects back to the start");
    }

    /// Typing or deleting with a selection replaces it, and the caret lands after what replaced
    /// it. An unshifted arrow collapses to the selection's edge instead of stepping past it.
    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut buffer = "on my way".to_string();
        let mut selection = (3, 9);
        assert_eq!(
            edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Append("foot"), false, false),
            PlainEdit { changed: true, ..PlainEdit::NONE }
        );
        assert_eq!((buffer.as_str(), selection), ("on foot", (7, 7)));

        let mut selection = (3, 7);
        edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Erase(Motion::Left), false, false);
        assert_eq!((buffer.as_str(), selection), ("on ", (3, 3)), "Backspace takes the selection, not one cluster");

        buffer = "on my way".to_string();
        let mut selection = (3, 9);
        edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Move(Motion::Left), false, false);
        assert_eq!((buffer.as_str(), selection), ("on my way", (3, 3)), "the arrow lands on the near edge");
    }

    #[test]
    fn an_arrow_the_caret_cannot_take_navigates() {
        let mut buffer = "ab".to_string();
        let right = edit_at_end(&mut buffer, KeyAction::Move(Motion::Right), false);
        assert_eq!(right, PlainEdit { navigated: Some(NavigateKey::Right), ..PlainEdit::NONE });
        let left = edit_at_end(&mut buffer, KeyAction::Move(Motion::Left), false);
        assert_eq!(left, PlainEdit { moved: true, ..PlainEdit::NONE }, "the caret takes it");
        let mut selection = (0, 0);
        let shifted = edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Move(Motion::Left), true, false);
        assert_eq!(shifted, PlainEdit::NONE, "Shift at the start is a no-op, not navigation");
    }

    #[test]
    fn typing_and_submitting_a_plain_field_never_cancel() {
        let mut buffer = String::new();
        assert_eq!(
            edit_at_end(&mut buffer, KeyAction::Append("a"), true),
            PlainEdit { changed: true, submitted: false, cancelled: false, navigated: None, moved: false }
        );
        assert_eq!(
            edit_at_end(&mut buffer, KeyAction::Submit, true),
            PlainEdit { changed: true, submitted: true, cancelled: false, navigated: None, moved: false }
        );
        assert_eq!(buffer, "a", "the caller empties the buffer after the submit, not this");
    }

    /// ADR-0112: the keys a single-line field cannot edit with reach the config by name. Tab is the
    /// one that has to be checked, since xkbcommon hands it back as `"\t"` and the `utf8` arm
    /// below it would drop that as a control character.
    #[test]
    fn arrow_paging_and_tab_keys_navigate_instead_of_editing() {
        assert_eq!(key_action(&key(Keysym::Up, None), false, false, false), KeyAction::Navigate(NavigateKey::Up));
        assert_eq!(
            key_action(&key(Keysym::Down, None), true, false, false),
            KeyAction::Navigate(NavigateKey::Down),
            "held Down keeps moving"
        );
        assert_eq!(
            key_action(&key(Keysym::Page_Down, None), false, false, false),
            KeyAction::Navigate(NavigateKey::PageDown)
        );
        assert_eq!(
            key_action(&key(Keysym::Tab, Some("\t")), false, false, false),
            KeyAction::Navigate(NavigateKey::Tab)
        );
        assert_eq!(
            key_action(&key(Keysym::ISO_Left_Tab, None), false, false, false),
            KeyAction::Navigate(NavigateKey::Backtab)
        );

        let mut buffer = "fire".to_string();
        let edit = edit_at_end(&mut buffer, KeyAction::Navigate(NavigateKey::Down), true);
        assert_eq!(edit, PlainEdit { navigated: Some(NavigateKey::Down), ..PlainEdit::NONE });
        assert_eq!(buffer, "fire", "moving through the results is not an edit");
    }

    fn autofocus_textfield(lua: &Lua) -> layout::ResolvedNode {
        let mut node = plain_textfield(lua);
        std::rc::Rc::make_mut(&mut node.properties).insert("autofocus", Value::Boolean(true));
        node
    }

    /// ADR-0112: the field the keyboard is handed to unasked. Only a plain field that could take
    /// keys qualifies, and with two the first in document order does, since two search boxes on one
    /// surface is a mistake to pick through rather than a secret to refuse routing.
    #[test]
    fn the_first_plain_autofocus_field_in_the_scope_is_the_one_armed() {
        let lua = Lua::new();
        let first = autofocus_textfield(&lua);
        let second = autofocus_textfield(&lua);
        let (first_id, second_id) = (first.id, second.id);
        let tree = tree_with(&lua, vec![plain_textfield(&lua), first, second]);
        match autofocus_field_in_scope(&[("launcher@eDP-1", &tree)]) {
            Some((surface, FieldTarget::Plain { id, .. })) => {
                assert_eq!(surface, "launcher@eDP-1");
                assert_eq!(id, first_id, "document order, not {second_id:?}");
            }
            other => panic!("expected the first autofocus field, got {other:?}"),
        }

        // A hidden card's field is out of reach: the next visible one is armed instead.
        let mut hidden = autofocus_textfield(&lua);
        hidden.visible = false;
        let shown = autofocus_textfield(&lua);
        let shown_id = shown.id;
        let tree = tree_with(&lua, vec![hidden, shown]);
        match autofocus_field_in_scope(&[("modal_host@eDP-1", &tree)]) {
            Some((_, FieldTarget::Plain { id, .. })) => assert_eq!(id, shown_id),
            other => panic!("expected the visible field, got {other:?}"),
        }

        // Masked, or declaring nothing that could read the keys: not candidates, whatever they say.
        let mut masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        std::rc::Rc::make_mut(&mut masked.properties).insert("autofocus", Value::Boolean(true));
        let mut mute = textfield(&lua, None);
        std::rc::Rc::make_mut(&mut mute.properties).insert("autofocus", Value::Boolean(true));
        let none = tree_with(&lua, vec![masked, mute, plain_textfield(&lua)]);
        assert!(autofocus_field_in_scope(&[("launcher@eDP-1", &none)]).is_none());
    }

    #[test]
    fn request_finds_only_a_visible_plain_field_and_restores_its_caret() {
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
        let target = requested_field(&tree, "search").expect("visible plain field");
        assert!(matches!(target, FieldTarget::Plain { id: found, .. } if found == id));
        assert!(requested_field(&tree, "missing").is_none());

        let mut previous = FocusedTextField {
            surface_id: "panel@TEST".into(),
            id,
            buffer: "draft".into(),
            selection: (2, 4),
            history: EditHistory::default(),
            typing: false,
            selecting: false,
            on_change: None,
            on_submit: None,
            on_cancel: None,
            on_navigate: None,
        };
        previous.history.record((String::new(), (0, 0)), None);
        let target = requested_field(&tree, "search").unwrap();
        let resumed = requested_focus("panel@TEST".into(), target, Some(&previous));
        assert_eq!((resumed.buffer.as_str(), resumed.selection, resumed.typing), ("draft", (2, 4), true));
        assert!(resumed.history.undo.is_empty(), "a new focus request starts fresh history");
        let target = requested_field(&tree, "search").unwrap();
        let other = requested_focus("other@TEST".into(), target, Some(&previous));
        assert_eq!((other.buffer.as_str(), other.selection), ("", (0, 0)));
        assert!(other.history.undo.is_empty());
        let target = requested_field(&tree, "search").unwrap();
        let fresh_autofocus = requested_focus("panel@TEST".into(), target, None);
        assert!(fresh_autofocus.buffer.is_empty() && fresh_autofocus.history.undo.is_empty());
        let changed = requested_focus(
            "panel@TEST".into(),
            FieldTarget::Plain {
                id: layout::scene::NodeId::test(999),
                on_change: None,
                on_submit: None,
                on_cancel: None,
                on_navigate: None,
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
        let id = field.id;
        assert!(store_draft(&mut parked, Some(&mut field), "calendar@eDP-1", id, "héllo"));
        assert_eq!((field.buffer.as_str(), field.selection), ("héllo", (6, 6)));
        assert!(field.history.undo.is_empty() && parked.is_empty());

        let other = layout::scene::NodeId::test(2);
        assert!(!store_draft(&mut parked, Some(&mut field), "calendar@eDP-1", other, "prefill"));
        assert_eq!(parked[&("calendar@eDP-1".to_string(), other)], ("prefill".to_string(), (7, 7)));
        assert!(!store_draft(&mut parked, None, "calendar@eDP-1", other, ""));
        assert!(parked.is_empty(), "an empty text clears a parked draft");
    }

    #[test]
    fn max_length_counts_grapheme_clusters_and_cuts_inserts_on_a_boundary() {
        let family = "👨‍👩‍👧";
        assert_eq!(fit_to_limit("abcdef", None, 3), "abcdef");
        assert_eq!(fit_to_limit("abcdef", Some(4), 2), "ab");
        assert_eq!(fit_to_limit("e\u{301}xyz", Some(2), 0), "e\u{301}x", "a base with a mark is one character");
        assert_eq!(fit_to_limit(&format!("{family}{family}"), Some(1), 0), family);
        assert_eq!(fit_to_limit("ab", Some(2), 5), "", "a field over its limit takes nothing more");
    }

    #[test]
    fn a_limited_append_counts_what_the_selection_gives_back_and_never_deletes_by_overflow() {
        let append = |buffer: &str, range, text, max| match limited_append(buffer, range, text, max) {
            KeyAction::Append(text) => Some(text.to_owned()),
            KeyAction::Ignore => None,
            _ => unreachable!(),
        };
        assert_eq!(append("abc", (3, 3), "xyz", Some(4)), Some("x".into()));
        assert_eq!(append("abc", (1, 3), "xyz", Some(3)), Some("xy".into()), "the selection makes room");
        assert_eq!(append("abc", (3, 3), "x", Some(3)), None, "a full field ignores typing");
        assert_eq!(append("abcd", (0, 4), "x", Some(2)), Some("x".into()), "an over-long draft can still be replaced");
        assert_eq!(append("abcd", (1, 4), "xyz", Some(2)), Some("x".into()));
        assert_eq!(append("abcd", (0, 4), "", Some(2)), Some(String::new()), "deleting is not limited");
        assert_eq!(append("abc", (3, 3), "xyz", None), Some("xyz".into()));
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
        let (a_id, hidden_id, b_id) = (a.id, hidden.id, b.id);
        let tree = tree_with(&lua, vec![a, hidden, masked, b]);
        let ids = |hidden| -> Vec<_> {
            plain_fields(&tree, hidden, |_| true)
                .map(|target| match target {
                    FieldTarget::Plain { id, .. } => id,
                    FieldTarget::Masked { .. } => unreachable!(),
                })
                .collect()
        };
        assert_eq!(ids(false), vec![a_id, b_id]);
        assert_eq!(ids(true), vec![a_id, hidden_id, b_id]);
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
    fn cancel_clears_history_through_the_field_edit() {
        let lua = Lua::new();
        let mut field = draft(1, "");
        field.on_cancel = Some(lua.create_function(|_, _: bool| Ok(())).unwrap());
        field.edit(KeyAction::Append("draft"), false, None, false);
        assert_eq!(field.history.undo.len(), 1);
        assert!(field.edit(KeyAction::Clear, false, None, false).cancelled);
        assert!(field.history.undo.is_empty());
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
        assert!(first_plain_field(&root, |_| true).is_none());
        assert!(autofocus_field_in_scope(&[("bar", &root)]).is_none());
        assert!(layout::secure_submit::typable_secure_submit_targets(&root).is_empty());
    }
}

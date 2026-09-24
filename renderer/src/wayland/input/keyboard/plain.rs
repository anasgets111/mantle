//! Plain `textfield`s: which one `autofocus` arms, the draft and selection a key edits, the
//! caret's blink, and the edits delivered to Lua (ADR-0092).

use shared::{debug, warn};
use unicode_segmentation::UnicodeSegmentation;

use super::*;

/// First plain `autofocus = true` field in scope document order (ADR-0112). Skip masked fields and
/// fields without callbacks; unlike two `secure_submit` fields, duplicate search boxes are a config
/// mistake, so deterministic order beats refusing both. A hidden subtree is skipped whole: it is
/// frozen (ADR-0124) and cannot take keys, and one surface that holds several modals' cards keeps
/// the closed ones hidden beside the open one.
fn autofocus_field_in_scope(scope: &[(&str, &layout::ResolvedNode)]) -> Option<(String, FieldTarget)> {
    for (surface_id, tree) in scope {
        let mut stack = vec![*tree];
        while let Some(node) = stack.pop() {
            if !node.visible || node.leaving {
                continue;
            }
            if node.kind == "textfield"
                && matches!(node.properties.get("autofocus"), Some(Value::Boolean(true)))
                && let Some(target @ FieldTarget::Plain { .. }) = focused_field(&[node])
            {
                return Some((surface_id.to_string(), target));
            }
            stack.extend(node.children.iter().rev());
        }
    }
    None
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
    navigated: Option<&'static str>,
    /// Caret or selection moved with the text unchanged: repaint, tell the config nothing.
    moved: bool,
}

impl PlainEdit {
    /// No-op edit, allowing [`App::apply_plain_key`] to return early.
    const NONE: PlainEdit =
        PlainEdit { changed: false, submitted: false, cancelled: false, navigated: None, moved: false };
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
            buffer.replace_range(from..to, text);
            *selection = (from + text.len(), from + text.len());
            PlainEdit { changed: true, ..PlainEdit::NONE }
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
                Motion::Left if !moved && !shift => Some("left"),
                Motion::Right if !moved && !shift => Some("right"),
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
        KeyAction::Ignore => PlainEdit::NONE,
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
        && let Err(e) = on_submit.call::<()>(text.clone())
    {
        warn!("{surface_id}: on_submit raised, ignoring it: {e}");
    }
    if edit.changed
        && let Some(on_change) = on_change
        && let Err(e) = on_change.call::<()>(if edit.submitted { String::new() } else { text })
    {
        warn!("{surface_id}: on_change raised, ignoring it: {e}");
    }
    // `edit.changed` is the one thing a config cannot work out for itself: the autofocus arm fires
    // `on_change("")` too, so counting empty changes cannot tell a cleared field from an opened one.
    if edit.cancelled
        && let Some(on_cancel) = on_cancel
        && let Err(e) = on_cancel.call::<()>(edit.changed)
    {
        warn!("{surface_id}: on_cancel raised, ignoring it: {e}");
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
        self.focus_text_field(Some(FocusedTextField {
            surface_id: surface_id.clone(),
            id,
            buffer: String::new(),
            selection: (0, 0),
            typing: true,
            selecting: false,
            on_change,
            on_submit,
            on_cancel,
            on_navigate,
        }));
        if let Some(on_change) = opened
            && let Err(e) = on_change.call::<()>(String::new())
        {
            warn!("{surface_id}: on_change raised, ignoring it: {e}");
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
    pub(in crate::wayland::input) fn focus_text_field(&mut self, next: Option<FocusedTextField>) {
        if self.focused_text_field.is_none() && next.is_none() {
            return;
        }
        self.mark_focused_text_field_changed();
        if let Some(ref next_field) = next {
            self.mark_field_input_changed(&next_field.surface_id);
        }
        self.focused_text_field = next;
    }

    /// [`App::prune_secure_focus`]'s counterpart. The same two clauses -- the surface is still
    /// alive, and it is still one the keyboard can reach -- because a plain field goes stale for
    /// exactly the reasons a masked one does. What it does not share is the urgency: dropping a
    /// half-typed reply loses a sentence, not a secret, so there is no once-a-turn sweep matching
    /// [`App::drop_secure_focus_if_its_surface_is_gone`]; the check before each keystroke is
    /// enough, and a `leave` clears it anyway.
    pub(super) fn prune_text_field_focus(&mut self) {
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
    pub(super) fn text_field_takes_keys(&self, field: &FocusedTextField) -> bool {
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
        if !self.focused_text_field.as_ref().is_some_and(|field| self.text_field_takes_keys(field)) {
            return;
        }
        // Read before the borrow below: shift turns a caret motion into a selection.
        let shift = self.shift_held;
        let Some(field) = self.focused_text_field.as_mut() else {
            return;
        };
        let edit = edit_plain_buffer(
            &mut field.buffer,
            &mut field.selection,
            key_action(event, repeat, self.ctrl_held),
            shift,
            field.on_cancel.is_some(),
        );
        if edit == PlainEdit::NONE {
            return;
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
            if let Some(on_navigate) = on_navigate
                && let Err(e) = on_navigate.call::<()>(key)
            {
                warn!("{surface_id}: on_navigate raised, ignoring it: {e}");
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
    use super::super::tests::{key, plain_textfield, secure_submit_table, textfield, tree_with};
    use super::*;

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
        let callbacks = PlainCallbacks { on_change: Some(on_change), on_submit: None, on_cancel: None };
        deliver_plain_edit(
            "launcher@eDP-1",
            PlainEdit { changed: true, ..PlainEdit::NONE },
            "cal".to_string(),
            callbacks,
        );

        let order: Vec<String> =
            lua.globals().get::<mlua::Table>("log").unwrap().sequence_values().collect::<mlua::Result<_>>().unwrap();
        assert_eq!(order, vec!["change(cal)".to_string()]);
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
        assert_eq!(key_action(&key(Keysym::Delete, None), false, false), KeyAction::Erase(Motion::Right));
        assert_eq!(key_action(&key(Keysym::BackSpace, None), false, true), KeyAction::Erase(Motion::WordLeft));
        assert_eq!(key_action(&key(Keysym::Delete, None), false, true), KeyAction::Erase(Motion::WordRight));

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

    /// Ctrl reaches exactly one binding; every other chord stays the compositor's to bind.
    #[test]
    fn ctrl_a_selects_the_draft_and_no_other_chord_is_taken() {
        assert_eq!(key_action(&key(Keysym::a, Some("a")), false, true), KeyAction::SelectAll);
        assert_eq!(key_action(&key(Keysym::c, Some("c")), false, true), KeyAction::Ignore, "Ctrl+C is not ours");
        assert_eq!(key_action(&key(Keysym::a, Some("a")), false, false), KeyAction::Append("a"), "and plain a types");

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
        assert_eq!(right, PlainEdit { navigated: Some("right"), ..PlainEdit::NONE });
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
        assert_eq!(key_action(&key(Keysym::Up, None), false, false), KeyAction::Navigate("up"));
        assert_eq!(
            key_action(&key(Keysym::Down, None), true, false),
            KeyAction::Navigate("down"),
            "held Down keeps moving"
        );
        assert_eq!(key_action(&key(Keysym::Page_Down, None), false, false), KeyAction::Navigate("page_down"));
        assert_eq!(key_action(&key(Keysym::Tab, Some("\t")), false, false), KeyAction::Navigate("tab"));
        assert_eq!(key_action(&key(Keysym::ISO_Left_Tab, None), false, false), KeyAction::Navigate("backtab"));

        let mut buffer = "fire".to_string();
        let edit = edit_at_end(&mut buffer, KeyAction::Navigate("down"), true);
        assert_eq!(edit, PlainEdit { navigated: Some("down"), ..PlainEdit::NONE });
        assert_eq!(buffer, "fire", "moving through the results is not an edit");
    }

    fn autofocus_textfield(lua: &Lua) -> layout::ResolvedNode {
        let mut node = plain_textfield(lua);
        node.properties.insert("autofocus", Value::Boolean(true));
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
        masked.properties.insert("autofocus", Value::Boolean(true));
        let mut mute = textfield(&lua, None);
        mute.properties.insert("autofocus", Value::Boolean(true));
        let none = tree_with(&lua, vec![masked, mute, plain_textfield(&lua)]);
        assert!(autofocus_field_in_scope(&[("launcher@eDP-1", &none)]).is_none());
    }
}

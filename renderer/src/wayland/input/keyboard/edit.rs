//! Plain-field text editing with no window state: the undo history, one key edit on a buffer, grapheme
//! and word boundaries, `max_length` fitting and input-method offsets (ADR-0092, ADR-0236).

use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation;

use super::*;

#[derive(Debug, Clone, Default)]
pub(in crate::wayland::input) struct EditHistory {
    pub(super) undo: VecDeque<(String, (usize, usize))>,
    pub(super) redo: VecDeque<(String, (usize, usize))>,
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

    pub(super) fn trim(&mut self) {
        let mut bytes: usize = self.undo.iter().chain(&self.redo).map(|(text, _)| text.len()).sum();
        while self.undo.len() + self.redo.len() > 100 || bytes > Self::LIMIT {
            let Some((text, _)) = self.undo.pop_front().or_else(|| self.redo.pop_front()) else { break };
            bytes -= text.len();
        }
    }

    pub(super) fn record(&mut self, snapshot: (String, (usize, usize)), typed_to: Option<usize>) {
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
        self.undo.push_back(snapshot);
        self.trim();
    }

    pub(super) fn restore(&mut self, text: &mut String, selection: &mut (usize, usize), redo: bool) -> bool {
        self.typing_end = None;
        let (from, to) = if redo { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        let Some((previous, previous_selection)) = from.pop_back() else { return false };
        let current = (std::mem::replace(text, previous), std::mem::replace(selection, previous_selection));
        if current.0.len() > Self::LIMIT {
            self.clear();
            return true;
        }
        to.push_back(current);
        self.trim();
        true
    }
}

/// One plain-field key edit before callbacks (ADR-0092, ADR-0102); split from
/// `apply_plain_key` so Escape is testable without a Wayland seat.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PlainEdit {
    /// Buffer text changed, so `on_change` fires.
    pub(super) changed: bool,
    /// Enter submits the text and empties the buffer.
    pub(super) submitted: bool,
    /// Escape with `on_cancel` drops focus and fires it.
    pub(super) cancelled: bool,
    /// An arrow the caret cannot take: the key is not the field's and goes up to `on_key`.
    pub(super) passed: bool,
    /// Caret or selection moved with the text unchanged: repaint, tell the config nothing.
    pub(super) moved: bool,
}

impl PlainEdit {
    /// No-op edit, allowing `apply_plain_key` to return early.
    pub(super) const NONE: PlainEdit =
        PlainEdit { changed: false, submitted: false, cancelled: false, passed: false, moved: false };
}

/// `text` cut on a grapheme boundary to what `max_length` leaves after `kept` clusters stay.
/// Counting clusters is how the caret and erase already see characters (ADR-0236).
pub(super) fn fit_to_limit(text: &str, max: Option<usize>, kept: usize) -> &str {
    let Some(max) = max else { return text };
    text.grapheme_indices(true).nth(max.saturating_sub(kept)).map_or(text, |(at, _)| &text[..at])
}

/// An insert of `text` over `replaced` in `buffer`, cut to `max_length`. Over a selection a fully
/// cut insert would delete it, so that case is refused whole.
pub(super) fn limited_append<'a>(
    buffer: &str,
    replaced: (usize, usize),
    text: &'a str,
    max: Option<usize>,
) -> KeyAction<'a> {
    let (from, to) = (replaced.0.min(replaced.1), replaced.0.max(replaced.1));
    let kept = buffer[..from].graphemes(true).count() + buffer[to..].graphemes(true).count();
    match fit_to_limit(text, max, kept) {
        "" if !text.is_empty() => KeyAction::Ignore,
        fitted => KeyAction::Append(fitted),
    }
}

/// Byte offset one word before `at`, taking the run of spaces before that word with it so a single
/// Ctrl+Backspace crosses the gap and the word together.
pub(super) fn previous_word(text: &str, at: usize) -> usize {
    text[..at].split_word_bound_indices().rfind(|(_, word)| !word.trim().is_empty()).map_or(0, |(start, _)| start)
}

/// Byte offset one word after `at`, taking the spaces before that word with it.
pub(super) fn next_word(text: &str, at: usize) -> usize {
    text[at..]
        .split_word_bound_indices()
        .find(|(_, word)| !word.trim().is_empty())
        .map_or(text.len(), |(start, word)| at + start + word.len())
}

/// How far a press selects: a series of left presses on one field grows it from click to word to line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::wayland::input) enum Unit {
    #[default]
    Char,
    Word,
    Line,
}

impl Unit {
    pub(in crate::wayland::input) fn next(self) -> Self {
        match self {
            Self::Char => Self::Word,
            Self::Word => Self::Line,
            Self::Line => Self::Char,
        }
    }
}

/// A unit, the span its press selected and the selection that left, which a drag or Shift+press keeps.
#[derive(Debug, Clone, Copy, Default)]
pub(in crate::wayland::input) struct Span {
    pub(in crate::wayland::input) unit: Unit,
    pub(in crate::wayland::input) base: (usize, usize),
    pub(in crate::wayland::input) produced: (usize, usize),
}

impl Span {
    /// Self while `selection` is still what it produced in `text`, else per character from the live
    /// anchor; checking the selection catches every edit and keyboard move without a reset at each.
    pub(in crate::wayland::input) fn live(self, text: &str, selection: (usize, usize)) -> Self {
        let valid = self.unit != Unit::Char
            && selection == self.produced
            && text.is_char_boundary(self.base.0)
            && text.is_char_boundary(self.base.1);
        if valid { self } else { Self { unit: Unit::Char, base: (selection.0, selection.0), produced: selection } }
    }
}

/// The `unit` around `at`: a word is the segment Ctrl+Left/Right walk, preferring a word to the
/// space or punctuation it touches; a line runs between newlines, so a single-line field's is all of it.
pub(in crate::wayland::input) fn unit_range(text: &str, at: usize, unit: Unit) -> (usize, usize) {
    match unit {
        Unit::Char => (at, at),
        Unit::Word => {
            let mut segments = text.split_word_bound_indices();
            let wordlike = |(_, word): &(usize, &str)| word.chars().any(char::is_alphanumeric);
            // `at` is a nearest-boundary caret, so it can sit just past a word's last letter.
            let (after, before) = (segments.clone().rfind(|(s, _)| *s <= at), segments.rfind(|(s, _)| *s < at));
            after
                .filter(wordlike)
                .or(before.filter(wordlike))
                .or(after)
                .map_or((at, at), |(start, word)| (start, start + word.len()))
        }
        Unit::Line => {
            (text[..at].rfind('\n').map_or(0, |i| i + 1), text[at..].find('\n').map_or(text.len(), |i| at + i))
        }
    }
}

/// `(anchor, head)` once the pointer reaches `at`: `base` stays selected and the unit there joins it.
pub(in crate::wayland::input) fn extend_by_unit(text: &str, span: Span, at: usize) -> (usize, usize) {
    let (start, end) = unit_range(text, at, span.unit);
    if start < span.base.0 { (span.base.1, start) } else { (span.base.0, end.max(span.base.1)) }
}

/// Byte offset of the grapheme cluster boundary before `at`, or the start of `text`.
pub(super) fn previous_boundary(text: &str, at: usize) -> usize {
    text[..at].grapheme_indices(true).next_back().map_or(0, |(start, _)| start)
}

/// Byte offset of the grapheme cluster boundary after `at`, or `at` at the end of `text`.
pub(super) fn next_boundary(text: &str, at: usize) -> usize {
    text[at..].graphemes(true).next().map_or(at, |cluster| at + cluster.len())
}

pub(super) fn ime_range(text: &str, selection: (usize, usize), before: u32, after: u32) -> Option<(usize, usize)> {
    let (from, to) = (selection.0.min(selection.1), selection.0.max(selection.1));
    let start = from.checked_sub(before as usize)?;
    let end = to.checked_add(after as usize)?;
    (end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)).then_some((start, end))
}

/// An input-method commit as a field takes it, a multiline one's newlines as `\n`; `None` refuses it.
pub(super) fn ime_commit(text: &str, multiline: bool) -> Option<std::borrow::Cow<'_, str>> {
    let text = if multiline { crate::lua::focus::normalize_newlines(text) } else { text.into() };
    (!crate::lua::focus::refuses(&text, multiline)).then_some(text)
}

pub(super) fn ime_change<'a>(
    text: &str,
    selection: (usize, usize),
    delete: (u32, u32),
    commit: Option<&'a str>,
) -> Option<((usize, usize), Option<&'a str>)> {
    if delete == (0, 0) && commit.is_none_or(str::is_empty) {
        return None;
    }
    let range = ime_range(text, selection, delete.0, delete.1)?;
    Some((range, commit.filter(|text| !text.is_empty())))
}

/// Apply `action` to `buffer` at `selection`. Insertion and deletion act on the selection, or at
/// the caret when there is none; `shift` extends the selection instead of collapsing it.
/// Escape follows `escape`; `Clear` empties the draft and, with `on_cancel`, also leaves (ADR-0092
/// decision 6), otherwise the config cannot know the field stopped taking keys.
pub(super) fn edit_plain_buffer(
    buffer: &mut String,
    selection: &mut (usize, usize),
    action: KeyAction<'_>,
    shift: bool,
    escape: Escape,
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
                    Motion::To(to) => (caret.min(to), caret.max(to)),
                    Motion::Up | Motion::Down | Motion::RowStart | Motion::RowEnd => (caret, caret),
                },
            };
            buffer.replace_range(from..to, "");
            *selection = (from, from);
            PlainEdit { changed: from < to, ..PlainEdit::NONE }
        }
        KeyAction::Clear => match escape {
            Escape::Clear => {
                let had = !buffer.is_empty();
                buffer.clear();
                *selection = (0, 0);
                PlainEdit { changed: had, cancelled: cancels, ..PlainEdit::NONE }
            }
            Escape::Blur => PlainEdit { cancelled: true, ..PlainEdit::NONE },
            Escape::Pass => PlainEdit { passed: true, ..PlainEdit::NONE },
        },
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
                Motion::To(to) => to,
                // Resolved by `resolve_row_motion`; one that could not be stays put.
                Motion::Up | Motion::Down | Motion::RowStart | Motion::RowEnd => caret,
            };
            let next = (if shift { anchor } else { moved_to }, moved_to);
            let moved = next != *selection;
            *selection = next;
            // An arrow the caret cannot take goes to the config, so a grid under the field can use it.
            let passed = !moved && !shift && matches!(motion, Motion::Left | Motion::Right);
            PlainEdit { moved, passed, ..PlainEdit::NONE }
        }
        KeyAction::SelectAll => {
            let next = (0, buffer.len());
            let moved = next != *selection;
            *selection = next;
            PlainEdit { moved, ..PlainEdit::NONE }
        }
        KeyAction::Undo | KeyAction::Redo | KeyAction::Ignore => PlainEdit::NONE,
    }
}

/// Whether the field used `action`, else the key is the config's: an arrow the caret cannot take,
/// an Escape with nothing to clear or cancel, and the keys no edit binds.
pub(super) fn field_took(action: KeyAction<'_>, edit: &PlainEdit) -> bool {
    match action {
        KeyAction::Ignore => false,
        KeyAction::Clear => edit.changed || edit.cancelled,
        _ => !edit.passed,
    }
}

impl FocusedTextField {
    pub(super) fn edit(
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
                let edit = edit_plain_buffer(
                    &mut self.buffer,
                    &mut self.selection,
                    action,
                    shift,
                    self.escape,
                    self.on_cancel.is_some(),
                );
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

    pub(super) fn delete_surrounding(&mut self, (start, end): (usize, usize)) -> PlainEdit {
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

#[cfg(test)]
mod tests {
    use super::super::tests::{draft, key};
    use super::*;

    #[test]
    fn undo_and_redo_restore_text_and_selection_and_new_edit_drops_redo() {
        let mut history = EditHistory::default();
        let mut text = "ab".to_string();
        let mut selection = (1, 1);
        history.record((text.clone(), selection), None);
        edit_plain_buffer(&mut text, &mut selection, KeyAction::Append("X"), false, Escape::Clear, false);
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
    fn trim_keeps_100_entries_and_drops_the_oldest_undo_first() {
        let entry = |tag: &str, n: usize| (format!("{tag}{n}"), (0, 0));
        let mut history = EditHistory {
            undo: (0..60).map(|n| entry("u", n)).collect(),
            redo: (0..60).map(|n| entry("r", n)).collect(),
            ..EditHistory::default()
        };
        history.trim();
        assert_eq!((history.undo.len(), history.redo.len()), (40, 60));
        assert_eq!(history.undo[0].0, "u20", "the oldest undo entries went first");
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
            redo: [("a".repeat(450_000), selection), ("b".repeat(450_000), selection)].into(),
            ..EditHistory::default()
        };
        let mut text = "c".repeat(650_000);
        assert!(history.restore(&mut text, &mut selection, true));
        assert!(history.undo.is_empty(), "redo transfer also enforces the aggregate ceiling");
        assert_eq!(history.redo.len(), 1);
    }

    #[test]
    fn an_ime_commit_keeps_newlines_as_lf_only_in_a_multiline_field() {
        assert_eq!(ime_commit("a\r\nb", true).as_deref(), Some("a\nb"));
        assert_eq!(ime_commit("a\tb", true), None);
        assert_eq!(ime_commit("a\nb", false), None);
        assert_eq!(ime_commit("語", false).as_deref(), Some("語"));
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
        let edit = edit_plain_buffer(&mut text, &mut selection, KeyAction::Append("語"), false, Escape::Clear, false);
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

    /// [`edit_plain_buffer`] with the caret at the end of the buffer and no Shift held, which is
    /// where an append-only field always had it.
    fn edit_at_end(buffer: &mut String, action: KeyAction<'_>, cancels: bool) -> PlainEdit {
        let mut selection = (buffer.len(), buffer.len());
        edit_plain_buffer(buffer, &mut selection, action, false, Escape::Clear, cancels)
    }

    #[test]
    fn escape_on_a_plain_field_without_on_cancel_clears_and_keeps_the_focus() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, false);
        assert_eq!(edit, PlainEdit { changed: true, submitted: false, cancelled: false, passed: false, moved: false });
        assert!(buffer.is_empty());
    }

    #[test]
    fn escape_on_a_plain_field_with_on_cancel_clears_and_gives_the_field_up() {
        let mut buffer = "on my wa".to_string();
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, true);
        assert_eq!(edit, PlainEdit { changed: true, submitted: false, cancelled: true, passed: false, moved: false });
        assert!(buffer.is_empty());
    }

    /// An empty field has nothing for `on_change` to report, but Escape is still a cancel: the
    /// field was open and the user asked to leave it.
    #[test]
    fn escape_on_an_empty_field_cancels_without_reporting_a_change() {
        let mut buffer = String::new();
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, true);
        assert_eq!(edit, PlainEdit { changed: false, submitted: false, cancelled: true, passed: false, moved: false });
        let edit = edit_at_end(&mut buffer, KeyAction::Clear, false);
        assert_eq!(
            edit,
            PlainEdit { changed: false, submitted: false, cancelled: false, passed: false, moved: false },
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
            edit_plain_buffer(buffer, selection, KeyAction::Erase(Motion::Left), false, Escape::Clear, false)
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
            edit_plain_buffer(buffer, selection, KeyAction::Erase(reach), false, Escape::Clear, false);
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
        let edit = edit_plain_buffer(&mut buffer, &mut selection, KeyAction::SelectAll, false, Escape::Clear, false);
        assert_eq!(selection, (0, "on my way".len()), "anchor at the start, caret at the end");
        assert!(edit.moved, "and the field repaints");

        // Backspace then takes the whole thing, which is what select-all is for.
        edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Erase(Motion::Left), false, Escape::Clear, false);
        assert_eq!(buffer, "");
    }

    #[test]
    fn the_caret_steps_over_a_composed_character_in_one_move() {
        let buffer = "ae\u{301}b".to_string();
        let mut selection = (1, 1);
        let move_to = |selection: &mut (usize, usize), motion, shift| {
            edit_plain_buffer(&mut buffer.clone(), selection, KeyAction::Move(motion), shift, Escape::Clear, false)
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
            edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Append("foot"), false, Escape::Clear, false),
            PlainEdit { changed: true, ..PlainEdit::NONE }
        );
        assert_eq!((buffer.as_str(), selection), ("on foot", (7, 7)));

        let mut selection = (3, 7);
        edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Erase(Motion::Left), false, Escape::Clear, false);
        assert_eq!((buffer.as_str(), selection), ("on ", (3, 3)), "Backspace takes the selection, not one cluster");

        buffer = "on my way".to_string();
        let mut selection = (3, 9);
        edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Move(Motion::Left), false, Escape::Clear, false);
        assert_eq!((buffer.as_str(), selection), ("on my way", (3, 3)), "the arrow lands on the near edge");
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
    fn a_unit_is_the_word_or_run_under_the_offset_or_the_line_between_newlines() {
        let text = "one  two, three";
        let word = |at| unit_range(text, at, Unit::Word);
        assert_eq!((word(1), word(0)), ((0, 3), (0, 3)));
        assert_eq!(word(3), (0, 3), "just past a word's last letter still takes the word, not the spaces");
        assert_eq!(
            (word(4), word(5)),
            ((3, 5), (5, 8)),
            "inside the spaces the run; at the next word's start that word"
        );
        assert_eq!(word(8), (5, 8), "after a word the comma gives way to it");
        assert_eq!(word(15), (10, 15), "the end takes the last word");
        assert_eq!(unit_range(" ,", 1, Unit::Word), (1, 2), "punctuation with no word beside it is itself");
        assert_eq!(unit_range("", 0, Unit::Word), (0, 0));
        assert_eq!(unit_range(text, 4, Unit::Line), (0, 15), "a single-line field's line is all of it");
        let lines = "ab\ncd ef\n\ngh";
        let line = |at| unit_range(lines, at, Unit::Line);
        assert_eq!((line(1), line(4), line(9)), ((0, 2), (3, 8), (9, 9)), "newline to newline, no wrap rows");
        assert_eq!(line(11), (10, 12));
    }

    #[test]
    fn extending_by_a_unit_keeps_the_original_and_grows_either_way() {
        let text = "one two three";
        let span = Span { unit: Unit::Word, base: (4, 7), produced: (4, 7) };
        assert_eq!(extend_by_unit(text, span, 5), (4, 7), "inside the original nothing changes");
        assert_eq!(extend_by_unit(text, span, 10), (4, 13), "right grows to the whole word");
        assert_eq!(extend_by_unit(text, span, 1), (7, 0), "left grows to the whole word and anchors on the far end");
    }

    #[test]
    fn a_unit_lives_only_while_its_selection_does() {
        let span = Span { unit: Unit::Word, base: (4, 7), produced: (4, 7) };
        assert_eq!(span.live("one two three", (4, 7)).unit, Unit::Word);
        let moved = span.live("one two three", (0, 13));
        assert_eq!((moved.unit, moved.base), (Unit::Char, (0, 0)), "Ctrl+A leaves the word behind");
        // A buffer replaced under a held drag: the stale base would sit inside a character.
        let edited = span.live("\u{e9}\u{e9}\u{e9}\u{e9}", (4, 7));
        assert_eq!(edited.unit, Unit::Char, "base off the new text's boundaries");
        assert_eq!(extend_by_unit("\u{e9}\u{e9}\u{e9}\u{e9}", edited, 2), (4, 2));
    }

    #[test]
    fn an_arrow_the_caret_cannot_take_goes_up() {
        let mut buffer = "ab".to_string();
        let right = edit_at_end(&mut buffer, KeyAction::Move(Motion::Right), false);
        assert_eq!(right, PlainEdit { passed: true, ..PlainEdit::NONE });
        let left = edit_at_end(&mut buffer, KeyAction::Move(Motion::Left), false);
        assert_eq!(left, PlainEdit { moved: true, ..PlainEdit::NONE }, "the caret takes it");
        let mut selection = (0, 0);
        let shifted =
            edit_plain_buffer(&mut buffer, &mut selection, KeyAction::Move(Motion::Left), true, Escape::Clear, false);
        assert_eq!(shifted, PlainEdit::NONE, "Shift at the start is a no-op, not navigation");
    }

    #[test]
    fn a_field_passes_up_the_keys_it_does_not_use_and_takes_the_ones_it_edits_with() {
        let took = |buffer: &str, action: KeyAction<'_>, shift: bool| {
            let (mut text, mut selection) = (buffer.to_string(), (0, 0));
            let edit = edit_plain_buffer(&mut text, &mut selection, action, shift, Escape::Clear, false);
            field_took(action, &edit)
        };
        assert!(took("ab", KeyAction::Append("x"), false));
        assert!(took("ab", KeyAction::Erase(Motion::Left), false), "an edit that does nothing is still the field's");
        assert!(took("ab", KeyAction::Submit, false));
        assert!(took("ab", KeyAction::Undo, false));
        assert!(took("ab", KeyAction::Move(Motion::Right), false), "the caret takes it");
        assert!(!took("ab", KeyAction::Move(Motion::Left), false), "an arrow at the edge is the config's");
        assert!(!took("ab", KeyAction::Ignore, false), "F-keys and Ctrl chords");
        assert!(took("ab", KeyAction::Clear, false), "Escape clears text");
        assert!(!took("", KeyAction::Clear, false), "Escape in an empty field goes up");
    }

    #[test]
    fn escape_follows_the_fields_mode() {
        let run = |escape: Escape, cancels: bool| {
            let (mut text, mut selection) = ("draft".to_string(), (5, 5));
            let edit = edit_plain_buffer(&mut text, &mut selection, KeyAction::Clear, false, escape, cancels);
            (text, edit)
        };
        let (text, edit) = run(Escape::Clear, true);
        assert_eq!((text.as_str(), edit.changed, edit.cancelled), ("", true, true));
        let (text, edit) = run(Escape::Blur, false);
        assert_eq!((text.as_str(), edit.changed, edit.cancelled), ("draft", false, true), "leaves, keeps the text");
        assert!(field_took(KeyAction::Clear, &edit));
        let (text, edit) = run(Escape::Pass, true);
        assert_eq!((text.as_str(), edit.changed, edit.cancelled, edit.passed), ("draft", false, false, true));
        assert!(!field_took(KeyAction::Clear, &edit), "the key goes up to on_key");
    }

    #[test]
    fn typing_and_submitting_a_plain_field_never_cancel() {
        let mut buffer = String::new();
        assert_eq!(
            edit_at_end(&mut buffer, KeyAction::Append("a"), true),
            PlainEdit { changed: true, submitted: false, cancelled: false, passed: false, moved: false }
        );
        assert_eq!(
            edit_at_end(&mut buffer, KeyAction::Submit, true),
            PlainEdit { changed: true, submitted: true, cancelled: false, passed: false, moved: false }
        );
        assert_eq!(buffer, "a", "the caller empties the buffer after the submit, not this");
    }
}

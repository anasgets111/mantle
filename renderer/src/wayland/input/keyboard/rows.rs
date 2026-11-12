//! A `multiline` `textfield`'s keys and view: Return by `submit_key`, Up/Down/Home/End by visual
//! row, the field's size following its draft, and the scroll that keeps the caret in view.

use super::*;
use crate::layout::field_rows;

/// `action` as a multiline field reads the key: Return inserts a newline unless it is `submit`'s
/// chord, and the arrows and Home/End move by visual row, Ctrl+Home/End to the text's ends.
pub(super) fn multiline_action<'a>(
    event: &KeyEvent,
    action: KeyAction<'a>,
    (repeat, ctrl, shift, alt): (bool, bool, bool, bool),
    submit: SubmitKey,
) -> KeyAction<'a> {
    match event.keysym {
        Keysym::Return | Keysym::KP_Enter => match (submit, ctrl, shift) {
            _ if alt => KeyAction::Ignore,
            (SubmitKey::CtrlReturn, true, _) | (SubmitKey::Return, false, false) if !repeat => KeyAction::Submit,
            (SubmitKey::CtrlReturn, false, _) | (SubmitKey::Return, false, true) => KeyAction::Append("\n"),
            _ => KeyAction::Ignore,
        },
        Keysym::Up | Keysym::KP_Up if !ctrl => KeyAction::Move(Motion::Up),
        Keysym::Down | Keysym::KP_Down if !ctrl => KeyAction::Move(Motion::Down),
        Keysym::Home | Keysym::KP_Home => KeyAction::Move(if ctrl { Motion::Start } else { Motion::RowStart }),
        Keysym::End | Keysym::KP_End => KeyAction::Move(if ctrl { Motion::End } else { Motion::RowEnd }),
        _ => action,
    }
}

/// Where `motion` takes a caret at `caret` over `rows` laid out under `align`, `width` wide, and the
/// x Up and Down keep through a run of them, field-local. `None` past the first or last row: the
/// key is then not the field's.
fn row_motion(
    rows: &[field_rows::Row<'_>],
    motion: Motion,
    (caret, upstream): (usize, bool),
    goal: Option<f32>,
    (align, width): (node::TextAlign, f32),
) -> Option<((usize, bool), Option<f32>)> {
    let index = field_rows::row_of(rows, caret, upstream);
    let row = rows.get(index)?;
    let next = match motion {
        Motion::Up => index.checked_sub(1)?,
        Motion::Down => index + 1,
        Motion::RowStart => return Some(((row.start, false), None)),
        _ => return Some(((row.end, true), None)),
    };
    let goal = goal.unwrap_or_else(|| row.left(align, 0.0, width) + row.caret_x(caret));
    let next = rows.get(next)?;
    Some((next.caret_at(goal - next.left(align, 0.0, width)), Some(goal)))
}

/// Where a multiline field's text is laid out: its box and the typography its rows wrap in.
struct View<'a> {
    height: f32,
    face: &'a node::Typeface,
    align: node::TextAlign,
    /// The wrap width.
    width: f32,
    scrolled: f32,
}

impl App {
    /// The `multiline` of the field `(surface_id, id)`, `None` for a single-line or gone one.
    pub(in crate::wayland::input) fn field_multiline(
        &self,
        surface_id: &str,
        id: layout::scene::NodeId,
    ) -> Option<node::Multiline> {
        self.field_view(surface_id, id).map(|(_, multiline)| multiline)
    }

    fn field_view(&self, surface_id: &str, id: layout::scene::NodeId) -> Option<(View<'_>, node::Multiline)> {
        let path = layout::hit::path_to_node(self.client.scene().surface(surface_id)?, id)?;
        let node = *path.last()?;
        let node::PaintStyle::TextField { face, align, caret, multiline: Some(multiline), .. } = node.paint.as_ref()?
        else {
            return None;
        };
        let content = node.content_box(node.rect);
        let width = field_rows::wrap_width(content.width, caret.width);
        Some((View { height: content.height, face, align: *align, width, scrolled: node.scrolled }, *multiline))
    }

    /// The draft of `(surface_id, id)`: the focused field's, else a parked one, else `""`.
    fn draft_of(&self, surface_id: &str, id: layout::scene::NodeId) -> &str {
        let focused = self.focused_text_field.as_ref().filter(|field| field.surface_id == surface_id && field.id == id);
        let parked = || self.parked_drafts.get(&(surface_id.to_owned(), id)).map(|(text, _)| text.as_str());
        focused.map(|field| field.buffer.as_str()).or_else(parked).unwrap_or_default()
    }

    /// The key as the focused field reads it: a multiline field remaps Return and the row keys.
    pub(super) fn field_key_action<'a>(&self, event: &'a KeyEvent, repeat: bool) -> KeyAction<'a> {
        let action = key_action(event, repeat, self.held.ctrl, self.held.shift);
        let multiline =
            self.focused_text_field.as_ref().and_then(|field| self.field_multiline(&field.surface_id, field.id));
        let held = (repeat, self.held.ctrl, self.held.shift, self.held.alt);
        multiline.map_or(action, |lines| multiline_action(event, action, held, lines.submit))
    }

    /// A row motion resolved against the focused field's rows, [`row_motion`].
    pub(super) fn resolve_row_motion<'a>(&mut self, action: KeyAction<'a>) -> KeyAction<'a> {
        let KeyAction::Move(motion @ (Motion::Up | Motion::Down | Motion::RowStart | Motion::RowEnd)) = action else {
            // A bare modifier or Escape moves no caret (a clearing Escape leaves byte 0), so upstream holds.
            if let Some(field) =
                self.focused_text_field.as_mut().filter(|_| !matches!(action, KeyAction::Ignore | KeyAction::Clear))
            {
                (field.goal_x, field.upstream) = (None, false);
            }
            return action;
        };
        let resolved = self.focused_text_field.as_ref().and_then(|field| {
            let (view, _) = self.field_view(&field.surface_id, field.id)?;
            field_rows::with_rows(&field.buffer, view.face, view.width, &self.shaping, |rows| {
                row_motion(rows, motion, (field.selection.1, field.upstream), field.goal_x, (view.align, view.width))
            })
        });
        let (Some(((to, upstream), goal)), Some(field)) = (resolved, self.focused_text_field.as_mut()) else {
            return KeyAction::Ignore;
        };
        (field.goal_x, field.upstream) = (goal, upstream);
        KeyAction::Move(Motion::To(to))
    }

    /// After the draft of `(surface_id, id)` changed, its caret moved, it took focus or it re-wrapped:
    /// a multiline field takes the draft to measure, and its rows scroll to keep a focused caret in view.
    pub(in crate::wayland::input) fn fit_field(&mut self, surface_id: &str, id: layout::scene::NodeId) {
        let Some((view, _)) = self.field_view(surface_id, id) else { return };
        let text = self.draft_of(surface_id, id).to_owned();
        let focused = self.focused_text_field.as_ref().filter(|field| field.surface_id == surface_id && field.id == id);
        let scroll = field_rows::with_rows(&text, view.face, view.width, &self.shaping, |rows| {
            let caret = focused.map(|field| field_rows::row_of(rows, field.selection.1, field.upstream));
            field_rows::fitted(view.scrolled, caret, rows.len(), view.face.line_height, view.height)
        });
        // ponytail: the box resizes in the next pass, so this frame scrolls against the old height; paint clamps it.
        if self.client.set_field_draft(surface_id, id, &text) {
            self.waker.wake();
        }
        self.client.set_field_scroll(surface_id, id, scroll);
    }

    /// [`App::fit_field`] for the focused field.
    pub(in crate::wayland) fn fit_focused_field(&mut self) {
        if let Some((surface_id, id)) =
            self.focused_text_field.as_ref().map(|field| (field.surface_id.clone(), field.id))
        {
            self.fit_field(&surface_id, id);
        }
    }

    /// After a pass, [`App::fit_field`] for a focused multiline field whose width re-wrapped its rows.
    pub(in crate::wayland) fn fit_focused_field_if_rewrapped(&mut self) {
        let Some(field) = self.focused_text_field.as_ref() else { return };
        let Some((view, _)) = self.field_view(&field.surface_id, field.id) else { return };
        if self.client.scene().field_wrap_width(field.id) != Some(view.width) {
            self.fit_focused_field();
        }
    }

    /// Scrolls the multiline field `id`'s rows by `delta` px; whether they moved, else the wheel
    /// goes on outward.
    pub(in crate::wayland::input) fn wheel_field(
        &mut self,
        surface_id: &str,
        id: layout::scene::NodeId,
        delta: f32,
    ) -> bool {
        let Some((view, _)) = self.field_view(surface_id, id) else { return false };
        let rows = field_rows::count(self.draft_of(surface_id, id), view.face, view.width, &self.shaping);
        let Some(scroll) = field_rows::wheeled(view.scrolled, delta, rows, view.face.line_height, view.height) else {
            return false;
        };
        self.client.set_field_scroll(surface_id, id, scroll);
        self.mark_field_input_changed(surface_id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{draft, key};
    use super::*;

    #[test]
    fn return_inserts_a_newline_and_the_submit_key_submits() {
        let read = |keysym, (ctrl, shift, alt), submit| {
            multiline_action(&key(keysym, Some("\r")), KeyAction::Submit, (false, ctrl, shift, alt), submit)
        };
        let (ctrl_return, return_) = (SubmitKey::CtrlReturn, SubmitKey::Return);
        assert_eq!(read(Keysym::Return, (false, false, false), ctrl_return), KeyAction::Append("\n"));
        assert_eq!(read(Keysym::KP_Enter, (true, false, false), ctrl_return), KeyAction::Submit);
        assert_eq!(read(Keysym::Return, (false, false, false), return_), KeyAction::Submit);
        assert_eq!(read(Keysym::Return, (false, true, false), return_), KeyAction::Append("\n"));
        assert_eq!(read(Keysym::Return, (true, true, false), return_), KeyAction::Ignore, "Ctrl+Shift+Return");
        for submit in [ctrl_return, return_] {
            assert_eq!(read(Keysym::Return, (false, false, true), submit), KeyAction::Ignore, "Alt+Return");
        }
        let held =
            multiline_action(&key(Keysym::Return, None), KeyAction::Ignore, (true, true, false, false), ctrl_return);
        assert_eq!(held, KeyAction::Ignore, "a held submit chord sends once");
        let typed =
            multiline_action(&key(Keysym::a, Some("a")), KeyAction::Append("a"), (false, false, false, false), return_);
        assert_eq!(typed, KeyAction::Append("a"), "every other key reads as on one line");
        let home =
            |ctrl| multiline_action(&key(Keysym::Home, None), KeyAction::Ignore, (false, ctrl, false, false), return_);
        assert_eq!((home(false), home(true)), (KeyAction::Move(Motion::RowStart), KeyAction::Move(Motion::Start)));
    }

    #[test]
    fn up_and_down_move_by_visual_row_keeping_the_goal_x_and_home_end_take_the_rows_ends() {
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        let face = crate::layout::hit::tests::face(14.0, 0.0);
        let text = "hello world\nab\nhello there";
        field_rows::with_rows(text, &face, 60.0, &shaping, |rows| {
            let spans: Vec<_> = rows.iter().map(|row| &text[row.start..row.end]).collect();
            assert_eq!(spans, ["hello", "world", "ab", "hello", "there"], "the test needs wrapped rows");
            let at = (node::TextAlign::Start, 60.0);
            // From after "wor" down to the short row's end, then on to "hel" under the goal it kept.
            let (short, goal) = row_motion(rows, Motion::Down, (9, false), None, at).unwrap();
            assert_eq!(short, (14, true), "past a shorter row's end is that end");
            let (deeper, kept) = row_motion(rows, Motion::Down, short, goal, at).unwrap();
            assert_eq!((deeper, kept), ((19, false), goal), "the goal x outlives the short row: \"hell|o\"");
            assert_eq!(
                row_motion(rows, Motion::Up, (6, false), None, at).map(|(to, _)| to.0),
                Some(0),
                "up a soft wrap"
            );
            assert_eq!(
                row_motion(rows, Motion::Up, (3, true), None, at),
                None,
                "up from the first row is not the field's"
            );
            assert_eq!(row_motion(rows, Motion::Down, (25, true), None, at), None, "nor down from the last");
            let home = row_motion(rows, Motion::RowStart, (9, false), Some(4.0), at);
            assert_eq!(home, Some(((6, false), None)), "Home: the row's start");
            let end = row_motion(rows, Motion::RowEnd, (2, false), None, at);
            assert_eq!(end, Some(((5, true), None)), "End: before the break's space");
            assert_eq!(field_rows::row_of(rows, 5, true), 0, "where affinity changes nothing");
            let last = row_motion(rows, Motion::RowEnd, (22, false), None, at).unwrap().0;
            assert_eq!(field_rows::row_of(rows, last.0, last.1), 4, "End on the last row");
        });
    }

    /// A mid-word wrap: one byte ends row 0 and starts row 1. End keeps the caret on row 0 and the
    /// keys after it start from there.
    #[test]
    fn end_on_a_mid_word_wrap_stays_on_its_row_for_the_keys_after_it() {
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        let face = crate::layout::hit::tests::face(14.0, 0.0);
        let text = "w".repeat(40);
        let end = field_rows::with_rows(&text, &face, 60.0, &shaping, |rows| {
            let at = (node::TextAlign::Start, 60.0);
            let row = |(to, upstream): (usize, bool)| field_rows::row_of(rows, to, upstream);
            let end = rows[0].end;
            assert_eq!(end, rows[1].start, "the test needs a mid-word wrap");
            let (to, _) = row_motion(rows, Motion::RowEnd, (1, false), None, at).unwrap();
            assert_eq!((to, row(to)), ((end, true), 0), "End stays on its row");
            assert!(rows[0].caret_x(end) > rows[0].caret_x(end - 1), "at its right edge");
            let home = row_motion(rows, Motion::RowStart, to, None, at);
            assert_eq!(home, Some(((0, false), None)), "Home returns to its start");
            // Up and Down leave from the row the caret is drawn on, at its x.
            assert_eq!(row_motion(rows, Motion::Up, to, None, at), None, "Up from row 0 is not the field's");
            let (down, goal) = row_motion(rows, Motion::Down, to, None, at).unwrap();
            assert_eq!((row(down), goal), (1, Some(rows[0].caret_x(end))), "Down from row 0's end");
            assert_eq!(row(row_motion(rows, Motion::Up, down, goal, at).unwrap().0), 0, "and Up back");
            assert_eq!(
                row(row_motion(rows, Motion::Down, (end, false), None, at).unwrap().0),
                2,
                "unlike row 1's start"
            );
            end
        });
        // Arrows and typing step from that byte; `resolve_row_motion` drops the affinity.
        let edited = |action| {
            let mut field = FocusedTextField { selection: (end, end), upstream: true, ..draft(1, &text) };
            field.edit(action, false, None, true);
            (field.buffer, field.selection.1)
        };
        assert_eq!(edited(KeyAction::Move(Motion::Right)).1, end + 1, "Right: one character on, onto row 1");
        assert_eq!(edited(KeyAction::Move(Motion::Left)).1, end - 1, "Left: one back on row 0");
        let (typed, caret) = edited(KeyAction::Append("w"));
        assert_eq!((typed.len(), caret), (41, end + 1), "typing inserts at that byte");
        field_rows::with_rows(&typed, &face, 60.0, &shaping, |rows| {
            let row = &rows[field_rows::row_of(rows, caret, false)];
            assert_eq!(row.start, end, "the full row 0 sent the new \"w\" to row 1");
            assert!(row.caret_x(caret) > 0.0, "and the caret follows it there");
        });
    }
}

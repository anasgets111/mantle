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
    caret: usize,
    goal: Option<f32>,
    (align, width): (node::TextAlign, f32),
) -> Option<(usize, Option<f32>)> {
    let index = field_rows::row_of(rows, caret);
    let row = rows.get(index)?;
    let next = match motion {
        Motion::Up => index.checked_sub(1)?,
        Motion::Down => index + 1,
        Motion::RowStart => return Some((row.start, None)),
        _ => return Some((row.end, None)),
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
        let width = field_rows::wrap_width(node.rect.width, caret.width);
        Some((View { height: node.rect.height, face, align: *align, width, scrolled: node.scrolled }, *multiline))
    }

    /// The draft of `(surface_id, id)`: the focused field's, else a parked one, else `""`.
    fn draft_of(&self, surface_id: &str, id: layout::scene::NodeId) -> &str {
        let focused = self.focused_text_field.as_ref().filter(|field| field.surface_id == surface_id && field.id == id);
        let parked = || self.parked_drafts.get(&(surface_id.to_owned(), id)).map(|(text, _)| text.as_str());
        focused.map(|field| field.buffer.as_str()).or_else(parked).unwrap_or_default()
    }

    /// The key as the focused field reads it: a multiline field remaps Return and the row keys.
    pub(super) fn field_key_action<'a>(&self, event: &'a KeyEvent, repeat: bool) -> KeyAction<'a> {
        let action = key_action(event, repeat, self.ctrl_held, self.shift_held);
        let multiline =
            self.focused_text_field.as_ref().and_then(|field| self.field_multiline(&field.surface_id, field.id));
        let held = (repeat, self.ctrl_held, self.shift_held, self.alt_held);
        multiline.map_or(action, |lines| multiline_action(event, action, held, lines.submit))
    }

    /// A row motion resolved against the focused field's rows, [`row_motion`].
    pub(super) fn resolve_row_motion<'a>(&mut self, action: KeyAction<'a>) -> KeyAction<'a> {
        let KeyAction::Move(motion @ (Motion::Up | Motion::Down | Motion::RowStart | Motion::RowEnd)) = action else {
            if let Some(field) = self.focused_text_field.as_mut() {
                field.goal_x = None;
            }
            return action;
        };
        let resolved = self.focused_text_field.as_ref().and_then(|field| {
            let (view, _) = self.field_view(&field.surface_id, field.id)?;
            field_rows::with_rows(&field.buffer, view.face, view.width, &self.shaping, |rows| {
                row_motion(rows, motion, field.selection.1, field.goal_x, (view.align, view.width))
            })
        });
        let (Some((to, goal)), Some(field)) = (resolved, self.focused_text_field.as_mut()) else {
            return KeyAction::Ignore;
        };
        field.goal_x = goal;
        KeyAction::Move(Motion::To(to))
    }

    /// After the draft of `(surface_id, id)` changed, its caret moved, it took focus or it re-wrapped:
    /// a multiline field takes the draft to measure, and its rows scroll to keep a focused caret in view.
    pub(in crate::wayland::input) fn fit_field(&mut self, surface_id: &str, id: layout::scene::NodeId) {
        let Some((view, _)) = self.field_view(surface_id, id) else { return };
        let text = self.draft_of(surface_id, id).to_owned();
        let focused = self.focused_text_field.as_ref().filter(|field| field.surface_id == surface_id && field.id == id);
        let scroll = field_rows::with_rows(&text, view.face, view.width, &self.shaping, |rows| {
            let caret = focused.map(|field| field_rows::row_of(rows, field.selection.1));
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
    use super::super::tests::key;
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
            let (short, goal) = row_motion(rows, Motion::Down, 9, None, at).unwrap();
            assert_eq!(short, 14, "past a shorter row's end is that end");
            let (deeper, kept) = row_motion(rows, Motion::Down, short, goal, at).unwrap();
            assert_eq!((deeper, kept), (19, goal), "the goal x outlives the short row: \"hell|o\"");
            assert_eq!(row_motion(rows, Motion::Up, 6, None, at).map(|(to, _)| to), Some(0), "up a soft wrap");
            assert_eq!(row_motion(rows, Motion::Up, 3, None, at), None, "up from the first row is not the field's");
            assert_eq!(row_motion(rows, Motion::Down, 25, None, at), None, "nor down from the last");
            assert_eq!(row_motion(rows, Motion::RowStart, 9, Some(4.0), at), Some((6, None)), "Home: the row's start");
            assert_eq!(row_motion(rows, Motion::RowEnd, 2, None, at), Some((5, None)), "End: before the break's space");
        });
    }
}

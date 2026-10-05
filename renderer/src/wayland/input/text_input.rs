//! Plain field composition through text-input-v3. Secure fields never enable it.

use wayland_client::globals::GlobalList;
use wayland_client::{Dispatch, delegate_noop};
use wayland_protocols::wp::text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3};

use super::*;
use crate::text::shaping;

type TextInputProxy = zwp_text_input_v3::ZwpTextInputV3;
type FieldId = (String, layout::scene::NodeId);
type LastState = (Option<(String, i32, i32)>, Option<(i32, i32, i32, i32)>);

#[derive(Default)]
struct Pending {
    preedit: Option<(String, i32, i32)>,
    commit: Option<String>,
    delete: (u32, u32),
}

pub(super) struct Preedit {
    pub text: String,
    pub cursor: (i32, i32),
}

#[derive(Default)]
pub(in crate::wayland) struct TextInput {
    manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    input: Option<TextInputProxy>,
    entered: Option<String>,
    enabled: Option<FieldId>,
    preedit: Option<Preedit>,
    pending: Pending,
    last: Option<LastState>,
    commits: u32,
    enabled_at: u32,
    other_changed: bool,
    serial_blocked: bool,
}

impl TextInput {
    pub(in crate::wayland) fn bind(globals: &GlobalList, qh: &QueueHandle<App>) -> Self {
        Self { manager: globals.bind(qh, 1..=1, ()).ok(), ..Self::default() }
    }

    pub(super) fn attach(&mut self, qh: &QueueHandle<App>, seat: &wl_seat::WlSeat) {
        if self.input.is_none() {
            self.input = self.manager.as_ref().map(|manager| manager.get_text_input(seat, qh, ()));
        }
    }

    pub(super) fn detach(&mut self) {
        if let Some(input) = self.input.take() {
            input.destroy();
        }
        *self = Self { manager: self.manager.take(), ..Self::default() };
    }

    fn commit(&mut self, input: &TextInputProxy) {
        input.commit();
        self.commits = self.commits.wrapping_add(1);
    }

    fn disable(&mut self, input: &TextInputProxy) -> Option<String> {
        if self.enabled.is_some() {
            input.disable();
            self.commit(input);
        }
        self.reset_disabled_state()
    }

    fn reset_disabled_state(&mut self) -> Option<String> {
        let old = self.enabled.take().map(|(surface, _)| surface);
        self.preedit = None;
        self.pending = Pending::default();
        self.last = None;
        self.other_changed = false;
        self.serial_blocked = false;
        self.enabled_at = 0;
        old
    }

    pub(super) fn composing(&self, surface: &str, id: layout::scene::NodeId) -> Option<&Preedit> {
        self.enabled.as_ref().filter(|(s, node)| s == surface && *node == id)?;
        self.preedit.as_ref()
    }

    pub(super) fn owns_text(&self) -> bool {
        // ponytail: v3 lacks IME activation; suppress raw text only during composition until activation is exposed.
        self.enabled.is_some()
            && (self.preedit.is_some() || self.pending.preedit.is_some() || self.pending.commit.is_some())
    }

    pub(super) fn note_other_change(&mut self) {
        self.other_changed = true;
        self.serial_blocked = false;
    }

    fn stage(&mut self, event: zwp_text_input_v3::Event) {
        use zwp_text_input_v3::Event;
        match event {
            Event::PreeditString { text, cursor_begin, cursor_end } => {
                self.pending.preedit = Some((text.unwrap_or_default(), cursor_begin, cursor_end));
            }
            Event::CommitString { text } => self.pending.commit = Some(text.unwrap_or_default()),
            Event::DeleteSurroundingText { before_length, after_length } => {
                self.pending.delete = (before_length, after_length);
            }
            _ => unreachable!("only staged text-input events reach here"),
        }
    }

    fn finish(&mut self, serial: u32, focused: Option<&FieldId>) -> Option<Pending> {
        let pending = std::mem::take(&mut self.pending);
        let enabled = self.enabled.as_ref()?;
        if serial < self.enabled_at || focused != Some(enabled) {
            return None;
        }
        self.preedit = pending.preedit.as_ref().and_then(|(text, begin, end)| {
            (!text.is_empty() && !text.chars().any(char::is_control))
                .then(|| Preedit { text: text.clone(), cursor: (*begin, *end) })
        });
        self.serial_blocked = serial != self.commits;
        Some(pending)
    }
}

fn surrounding(text: &str, selection: (usize, usize)) -> Option<(String, i32, i32)> {
    let (from, to) = (selection.0.min(selection.1), selection.0.max(selection.1));
    if to > text.len() || to - from > 4000 {
        return None;
    }
    let budget = 4000 - (to - from);
    let start = text.ceil_char_boundary(from.saturating_sub(budget / 2).max(to.saturating_sub(4000)));
    let end = text.floor_char_boundary((start + 4000).min(text.len()));
    Some((text[start..end].to_string(), (selection.1 - start) as i32, (selection.0 - start) as i32))
}

fn advertised_surrounding(
    text: &str,
    selection: (usize, usize),
    preedit: Option<&Preedit>,
) -> Option<(String, i32, i32)> {
    if preedit.is_none() {
        return surrounding(text, selection);
    }
    let (from, to) = (selection.0.min(selection.1), selection.0.max(selection.1));
    let without_selection = [text.get(..from)?, text.get(to..)?].concat();
    surrounding(&without_selection, (from, from))
}

fn cursor_rect(
    root: &layout::ResolvedNode,
    id: layout::scene::NodeId,
    text: &str,
    caret: usize,
    shaping: &ShapingHandle,
) -> Option<(i32, i32, i32, i32)> {
    let path = layout::hit::path_to_node(root, id)?;
    let node = *path.last()?;
    let rect = layout::hit::absolute_rect(&path)?;
    let node::PaintStyle::TextField { target: None, face, align, caret: bar, .. } = node.paint.as_ref()? else {
        return None;
    };
    let shaped = layout::hit::field_line(text, face, shaping);
    let line = shaped.as_ref().and_then(|shaped| shaped.shaped.first());
    let left = layout::hit::field_line_left(line, *align, rect.x, rect.x + rect.width, caret, bar.width, 1.0);
    let bar_height = bar.bar_height(face.line_height);
    let cx = line.map_or(0.0, |line| shaping::caret_x(line, caret));
    let matrix = layout::hit::path_transform(&path);
    let bounds = node::transformed_bounds(
        matrix,
        LogicalRect {
            x: left + cx,
            y: rect.y + ((rect.height - face.line_height) / 2.0).max(0.0) + (face.line_height - bar_height) / 2.0,
            width: bar.width,
            height: bar_height,
        },
    );
    Some((
        bounds.x.round() as i32,
        bounds.y.round() as i32,
        bounds.width.ceil().max(1.0) as i32,
        bounds.height.ceil().max(1.0) as i32,
    ))
}

impl App {
    pub(in crate::wayland::input) fn cancel_text_input_composition(&mut self) {
        if !self.text_input.owns_text()
            && self.text_input.pending.preedit.is_none()
            && self.text_input.pending.commit.is_none()
            && self.text_input.pending.delete == (0, 0)
        {
            return;
        }
        if let Some(input) = self.text_input.input.clone()
            && let Some(old) = self.text_input.disable(&input)
        {
            self.mark_field_input_changed(&old);
        }
    }

    pub(in crate::wayland::input) fn invalidate_text_input_focus(&mut self) {
        let still_focused = self.focused_text_field.as_ref().is_some_and(|field| {
            self.text_field_takes_keys(field)
                && self.text_input.enabled.as_ref() == Some(&(field.surface_id.clone(), field.id))
        });
        if !still_focused
            && let Some(input) = self.text_input.input.clone()
            && let Some(old) = self.text_input.disable(&input)
        {
            self.mark_field_input_changed(&old);
        }
    }

    pub(in crate::wayland) fn sync_text_input(&mut self) {
        let Some(input) = self.text_input.input.clone() else { return };
        // ponytail: IME needs direct surface focus; support parent-focused popups after tracking configured offsets.
        let desired = self.focused_text_field.as_ref().filter(|field| {
            self.text_field_takes_keys(field) && self.text_input.entered.as_deref() == Some(field.surface_id.as_str())
        });
        let desired_id = desired.map(|field| (field.surface_id.clone(), field.id));
        if desired_id != self.text_input.enabled {
            if let Some(old) = self.text_input.disable(&input) {
                self.mark_field_input_changed(&old);
            }
            if let Some(id) = desired_id.clone() {
                input.enable();
                input.set_content_type(zwp_text_input_v3::ContentHint::None, zwp_text_input_v3::ContentPurpose::Normal);
                self.text_input.enabled = Some(id);
                self.text_input.enabled_at = self.text_input.commits.wrapping_add(1);
            }
        }
        if self.text_input.serial_blocked {
            return;
        }
        let Some(field) = self
            .focused_text_field
            .as_ref()
            .filter(|field| desired_id.as_ref() == Some(&(field.surface_id.clone(), field.id)))
        else {
            return;
        };
        let (shown, caret) = if let Some(preedit) = self.text_input.composing(&field.surface_id, field.id) {
            let (shown, range, caret) =
                layout::paint::compose_preedit(&field.buffer, field.selection, &preedit.text, preedit.cursor);
            (shown, caret.map_or(range.end, |(_, caret)| caret))
        } else {
            (field.buffer.clone(), field.selection.1)
        };
        let rect = self
            .client
            .scene()
            .surface(&field.surface_id)
            .and_then(|root| cursor_rect(root, field.id, &shown, caret, &self.shaping));
        let surrounding = advertised_surrounding(
            &field.buffer,
            field.selection,
            self.text_input.composing(&field.surface_id, field.id),
        );
        let next = (surrounding, rect);
        if self.text_input.last.as_ref() == Some(&next) && !self.text_input.other_changed {
            return;
        }
        if self.text_input.other_changed {
            input.set_text_change_cause(zwp_text_input_v3::ChangeCause::Other);
        }
        if let Some((text, cursor, anchor)) = &next.0 {
            input.set_surrounding_text(text.clone(), *cursor, *anchor);
        }
        if let Some((x, y, width, height)) = next.1 {
            input.set_cursor_rectangle(x, y, width, height);
        }
        self.text_input.commit(&input);
        self.text_input.last = Some(next);
        self.text_input.other_changed = false;
    }
}

impl Dispatch<TextInputProxy, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &TextInputProxy,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.text_input.input.as_ref() != Some(proxy) {
            return;
        }
        use zwp_text_input_v3::Event;
        match event {
            Event::Enter { surface } => {
                state.text_input.entered = state.surface_id_for(&surface).map(str::to_owned);
                state.sync_text_input();
            }
            Event::Leave { .. } => {
                state.text_input.entered = None;
                if let Some(old) = state.text_input.disable(proxy) {
                    state.mark_field_input_changed(&old);
                }
            }
            event
            @ (Event::PreeditString { .. } | Event::CommitString { .. } | Event::DeleteSurroundingText { .. }) => {
                state.text_input.stage(event);
            }
            Event::Done { serial } => {
                let had_preedit = state.text_input.preedit.is_some();
                let focused = state
                    .focused_text_field
                    .as_ref()
                    .filter(|field| state.text_field_takes_keys(field))
                    .map(|field| (field.surface_id.clone(), field.id));
                let Some(pending) = state.text_input.finish(serial, focused.as_ref()) else { return };
                let old = focused.expect("finish checked the focused field").0;
                let visible_change = had_preedit || pending.preedit.is_some();
                state.apply_ime_edit(pending.delete, pending.commit.as_deref());
                if visible_change {
                    state.mark_field_input_changed(&old);
                }
                state.sync_text_input();
            }
            _ => {}
        }
    }
}

delegate_noop!(App: zwp_text_input_manager_v3::ZwpTextInputManagerV3);

#[cfg(test)]
mod tests {
    use super::*;

    fn field(n: u64) -> FieldId {
        ("panel@TEST".into(), layout::scene::NodeId::test(n))
    }

    #[test]
    fn surrounding_text_keeps_selection_and_utf8_boundaries_within_the_protocol_limit() {
        let text = format!("{}é{}", "a".repeat(3000), "b".repeat(3000));
        let (window, cursor, anchor) = surrounding(&text, (3000, 3002)).unwrap();
        assert!(window.len() <= 4000);
        assert_eq!(&window[anchor as usize..cursor as usize], "é");
        assert_eq!(surrounding(&text, (0, 5000)), None);
        assert_eq!(surrounding("aéz", (3, 1)), Some(("aéz".to_string(), 1, 3)));
    }

    #[test]
    fn preedit_advertises_the_unselected_text_then_commit_or_cancel_restores_the_draft() {
        let id = field(1);
        let mut input = TextInput { enabled: Some(id.clone()), commits: 1, enabled_at: 1, ..TextInput::default() };
        let selection = (1, 2);
        input.stage(zwp_text_input_v3::Event::PreeditString { text: Some("X".into()), cursor_begin: 1, cursor_end: 1 });
        input.finish(1, Some(&id)).unwrap();
        assert_eq!(advertised_surrounding("abc", selection, input.preedit.as_ref()), Some(("ac".into(), 1, 1)));

        input.stage(zwp_text_input_v3::Event::CommitString { text: Some("X".into()) });
        let committed = input.finish(1, Some(&id)).unwrap();
        assert_eq!(committed.commit.as_deref(), Some("X"));
        assert_eq!(advertised_surrounding("aXc", (2, 2), input.preedit.as_ref()), Some(("aXc".into(), 2, 2)));

        input.stage(zwp_text_input_v3::Event::PreeditString { text: Some("Y".into()), cursor_begin: 1, cursor_end: 1 });
        input.finish(1, Some(&id)).unwrap();
        assert_eq!(advertised_surrounding("abc", selection, input.preedit.as_ref()), Some(("ac".into(), 1, 1)));
        input.finish(1, Some(&id)).unwrap();
        assert_eq!(advertised_surrounding("abc", selection, input.preedit.as_ref()), Some(("abc".into(), 2, 1)));
    }

    #[test]
    fn hidden_preedit_cursor_uses_the_end_of_shown_text() {
        let (shown, range, caret) = layout::paint::compose_preedit("aéb", (1, 3), "語", (-1, -1));
        assert_eq!(shown, "a語b");
        assert_eq!(caret.map_or(range.end, |(_, caret)| caret), 4);
        assert!(shown.is_char_boundary(range.end));
    }

    #[test]
    fn done_batches_preedit_commit_and_delete_for_its_enabled_field() {
        let id = field(1);
        let mut input = TextInput { enabled: Some(id.clone()), commits: 4, enabled_at: 4, ..TextInput::default() };
        input.stage(zwp_text_input_v3::Event::PreeditString {
            text: Some("語".into()),
            cursor_begin: 3,
            cursor_end: 3,
        });
        input.stage(zwp_text_input_v3::Event::CommitString { text: Some("字".into()) });
        input.stage(zwp_text_input_v3::Event::DeleteSurroundingText { before_length: 1, after_length: 2 });
        assert!(input.owns_text());
        let pending = input.finish(4, Some(&id)).unwrap();
        assert_eq!(pending.commit.as_deref(), Some("字"));
        assert_eq!(pending.delete, (1, 2));
        assert_eq!(input.preedit.as_ref().map(|preedit| preedit.text.as_str()), Some("語"));
        assert!(!input.serial_blocked);
    }

    #[test]
    fn stale_done_allows_a_later_local_move_and_ime_delete() {
        let id = field(1);
        let mut input = TextInput { enabled: Some(id.clone()), commits: 5, enabled_at: 4, ..TextInput::default() };
        input.stage(zwp_text_input_v3::Event::CommitString { text: Some("old".into()) });
        assert!(input.finish(3, Some(&id)).is_none());
        input.stage(zwp_text_input_v3::Event::CommitString { text: Some("old".into()) });
        input.finish(4, Some(&id)).unwrap();
        assert!(input.serial_blocked);
        input.note_other_change();
        assert!(!input.serial_blocked);
        assert_eq!(advertised_surrounding("aé文z", (3, 3), None), Some(("aé文z".into(), 3, 3)));
        input.stage(zwp_text_input_v3::Event::DeleteSurroundingText { before_length: 2, after_length: 0 });
        let pending = input.finish(5, Some(&id)).unwrap();
        assert_eq!(pending.delete, (2, 0));
        input.stage(zwp_text_input_v3::Event::CommitString { text: Some("secret".into()) });
        assert!(input.finish(5, None).is_none(), "secure focus cannot receive plain IME commits");
        input.stage(zwp_text_input_v3::Event::PreeditString { text: Some("X".into()), cursor_begin: 1, cursor_end: 1 });
        input.finish(4, Some(&id)).unwrap();
        input.stage(zwp_text_input_v3::Event::DeleteSurroundingText { before_length: 1, after_length: 0 });
        assert!(input.serial_blocked && input.preedit.is_some());
        assert_eq!(input.reset_disabled_state(), Some(id.0));
        assert!(input.pending.preedit.is_none() && input.pending.commit.is_none());
        assert_eq!(input.pending.delete, (0, 0));
        assert!(input.preedit.is_none() && input.last.is_none());
        assert!(!input.serial_blocked && !input.other_changed);
        assert_eq!(input.enabled_at, 0);
    }

    #[test]
    fn empty_done_clears_preedit_and_a_commit_does_not_latch_raw_key_ownership() {
        let id = field(1);
        let mut input = TextInput { enabled: Some(id.clone()), commits: 1, enabled_at: 1, ..TextInput::default() };
        input.stage(zwp_text_input_v3::Event::PreeditString {
            text: Some("文".into()),
            cursor_begin: 3,
            cursor_end: 3,
        });
        let preedit_only = input.finish(1, Some(&id)).unwrap();
        assert!(preedit_only.commit.is_none() && preedit_only.delete == (0, 0));
        assert_eq!(input.preedit.as_ref().map(|preedit| preedit.text.as_str()), Some("文"));
        let empty = input.finish(1, Some(&id)).unwrap();
        assert!(empty.preedit.is_none() && empty.commit.is_none() && empty.delete == (0, 0));
        assert!(input.preedit.is_none(), "done resets pending preedit to its empty initial state");
        input.stage(zwp_text_input_v3::Event::PreeditString {
            text: Some("文".into()),
            cursor_begin: 3,
            cursor_end: 3,
        });
        input.finish(1, Some(&id)).unwrap();
        assert_eq!(input.preedit.as_ref().map(|preedit| preedit.text.as_str()), Some("文"));
        input.stage(zwp_text_input_v3::Event::CommitString { text: Some("字".into()) });
        input.finish(1, Some(&id)).unwrap();
        assert!(input.preedit.is_none());
        assert!(!input.owns_text(), "plain keys resume after the IME commits");
    }

    #[test]
    fn cursor_rectangle_follows_ancestor_and_field_transforms() {
        let id = layout::scene::NodeId::test(1);
        let lua = mlua::Lua::new();
        let mut field = super::keyboard::tests::textfield(&lua, None);
        field.id = id;
        field.rect = LogicalRect { x: 10.0, y: 10.0, width: 100.0, height: 24.0 };
        let mut root = layout::ResolvedNode::test("panel", (0.0, 0.0, 200.0, 100.0), vec![field]);
        let shaping = ShapingHandle::spawn();
        let base = cursor_rect(&root, id, "", 0, &shaping).unwrap();
        root.transform.translate = (30.0, 40.0);
        root.children[0].transform.translate = (5.0, 7.0);
        let moved = cursor_rect(&root, id, "", 0, &shaping).unwrap();
        assert_eq!((moved.0 - base.0, moved.1 - base.1), (35, 47));
        assert_eq!(cursor_rect(&root, layout::scene::NodeId::test(2), "", 0, &shaping), None);
    }

    /// The IME rectangle is the caret bar the field draws, in its own typeface and `caret` size, at a non-empty draft's caret.
    #[test]
    fn cursor_rectangle_is_the_caret_bar_of_the_fields_typeface() {
        let id = layout::scene::NodeId::test(1);
        let lua = mlua::Lua::new();
        let table = |src: &str| mlua::Value::Table(lua.load(src).eval().unwrap());
        let mut field = super::keyboard::tests::textfield(&lua, None);
        for (key, value) in [
            ("font_size", mlua::Value::Number(20.0)),
            ("line_height", mlua::Value::Number(2.0)),
            ("caret", table("return { width = 3, height = 0.5 }")),
        ] {
            field = super::keyboard::tests::with_property(field, key, value);
        }
        field.id = id;
        field.rect = LogicalRect { x: 10.0, y: 10.0, width: 100.0, height: 60.0 };
        let root = layout::ResolvedNode::test("panel", (0.0, 0.0, 200.0, 100.0), vec![field]);
        let shaping = ShapingHandle::spawn();
        let node::PaintStyle::TextField { face, .. } = root.children[0].paint.as_ref().unwrap() else { unreachable!() };
        let line = layout::hit::field_line("ab", face, &shaping).unwrap();
        let cx = shaping::caret_x(&line.shaped[0], 2);
        // A 40px line in a 60px box starts 10 down, and the 20px bar centres in it: 10 + 10 + 10.
        assert_eq!(cursor_rect(&root, id, "ab", 2, &shaping), Some(((10.0 + cx).round() as i32, 30, 3, 20)));
    }
}

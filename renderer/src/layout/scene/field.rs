//! What a `multiline` `textfield` takes from the app that holds its draft: the text it measures
//! and how far its rows scroll.

use std::collections::{HashMap, HashSet};

use super::{NodeId, ResolvedNode, Scene};
use crate::layout::node::{PaintStyle, Typeface};
use crate::layout::{field_rows, hit};
use crate::text::shaping::ShapingHandle;

/// A multiline field's draft and the rows it wrapped to at a width and font generation.
#[derive(Default)]
pub(super) struct FieldDraft {
    pub(super) text: String,
    wrapped: (f32, usize, u64),
}

/// The byte a multiline field's top row starts at, the px past it, and the wrap width, face and font
/// generation its scroll was derived at: a change to any scrolls back to that text (#111, #125).
#[derive(Debug, Clone)]
pub(crate) struct FieldTop {
    at: usize,
    past: f32,
    fit: (f32, Typeface, u64),
}

/// The wrap width a multiline field's rows take now, and its face; `None` for any other node.
fn fit_of(node: &ResolvedNode) -> Option<(f32, &Typeface)> {
    let Some(PaintStyle::TextField { face, caret, multiline: Some(_), .. }) = &node.paint else { return None };
    Some((field_rows::wrap_width(node.content_box(node.rect).width, caret.width), face))
}

/// Scrolls `node`, just laid out, back to its [`FieldTop`] when that was derived at another fit,
/// the px past its row scaled to the new row height.
pub(super) fn refit(node: &mut ResolvedNode, drafts: &FieldDrafts, shaping: &ShapingHandle, fonts: u64) {
    let (Some((width, face)), Some(top)) = (fit_of(node), node.field_top.as_deref()) else { return };
    if (top.fit.0, &top.fit.1, top.fit.2) == (width, face, fonts) {
        return;
    }
    let (past, height) = (top.past * face.line_height / top.fit.1.line_height, node.content_box(node.rect).height);
    let text = drafts.get(&node.id).map_or("", |draft| draft.text.as_str());
    let scrolled = field_rows::with_rows(text, face, width, shaping, |rows| {
        let row = field_rows::row_of(rows, top.at, false) as f32;
        field_rows::clamp_scroll(row * face.line_height + past, rows.len(), face.line_height, height)
    });
    let top = FieldTop { at: top.at, past, fit: (width, face.clone(), fonts) };
    (node.scrolled, node.field_top) = (scrolled, Some(Box::new(top)));
}

/// By node: an id is never reused, so a re-created node starts from nothing, as its draft does.
pub(super) type FieldDrafts = HashMap<NodeId, FieldDraft>;

impl Scene {
    /// Gives the multiline field `id` the draft it measures, held here so a rebuilt solver node
    /// measures it too. Whether its row count changed, so `instance_id` owes a pass to resize it.
    pub fn set_field_draft(&mut self, instance_id: &str, id: NodeId, text: &str, shaping: &ShapingHandle) -> bool {
        let path = self.surfaces.get(instance_id).and_then(|tree| hit::path_to_node(tree, id));
        let Some(&node) = path.as_ref().and_then(|path| path.last()) else { return false };
        let Some(PaintStyle::TextField { face, caret, multiline: Some(lines), .. }) = &node.paint else { return false };
        let width = field_rows::wrap_width(node.content_box(node.rect).width, caret.width);
        // Before shaping: a face landing meanwhile leaves the count behind, not ahead (#125).
        let fonts = shaping.font_generation();
        let old = self.field_drafts.get(&id);
        let current = |old: &FieldDraft| (old.wrapped.0, old.wrapped.2) == (width, fonts);
        if old.map_or(text.is_empty(), |old| old.text == text && current(old)) {
            return false;
        }
        let count = |text: &str| field_rows::count(text, face, width, shaping);
        let before = match old {
            Some(old) if current(old) => old.wrapped.1,
            old => count(old.map_or("", |old| old.text.as_str())),
        };
        let after = count(text);
        match text.is_empty() {
            true => self.field_drafts.remove(&id),
            false => self.field_drafts.insert(id, FieldDraft { text: text.to_owned(), wrapped: (width, after, fonts) }),
        };
        // The measure reads the draft, not its inputs, so taffy's cache must be told.
        if let (Some(taffy), Some(tree)) = (node.taffy, self.solver_trees.get_mut(instance_id)) {
            let _ = tree.mark_dirty(taffy);
        }
        lines.rows(before) != lines.rows(after)
    }

    /// The byte and px of the field `id`'s scroll anchor.
    #[cfg(test)]
    pub fn field_top(&self, id: NodeId) -> Option<(usize, f32)> {
        let node = self.surfaces.values().find_map(|tree| hit::path_to_node(tree, id)?.last().copied())?;
        node.field_top.as_ref().map(|top| (top.at, top.past))
    }

    /// The width and font generation the field `id`'s draft last wrapped at, `None` without one.
    pub fn field_wrapped(&self, id: NodeId) -> Option<(f32, u64)> {
        self.field_drafts.get(&id).map(|draft| (draft.wrapped.0, draft.wrapped.2))
    }

    /// Drops the drafts of fields no surface holds any more.
    pub(super) fn forget_gone_fields(&mut self) {
        let surfaces = &self.surfaces;
        self.field_drafts.retain(|id, _| surfaces.values().any(|tree| hit::contains_node(tree, *id)));
    }

    /// The `focus_target` names the retained text fields declare, hidden ones included.
    pub(super) fn field_targets(&self) -> HashSet<String> {
        let mut stack: Vec<&ResolvedNode> = self.surfaces.values().collect();
        let mut names = HashSet::new();
        while let Some(node) = stack.pop() {
            stack.extend(&node.children);
            if node.kind == "textfield"
                && let Some(name) = node.focus_target()
            {
                names.insert(name);
            }
        }
        names
    }

    /// Scrolls the field `id`'s rows to `scroll` px, `top` its [`field_rows::top`]; whether that
    /// moved them. The anchor takes the last pass's font generation, never newer than the rows'.
    pub fn set_field_scroll(&mut self, instance_id: &str, id: NodeId, scroll: f32, top: Option<(usize, f32)>) -> bool {
        let Some(node) = self.surfaces.get_mut(instance_id).and_then(|tree| find_mut(tree, id)) else { return false };
        let fit = fit_of(node).map(|(width, face)| (width, face.clone(), self.font_generation));
        node.field_top = top.zip(fit).map(|((at, past), fit)| Box::new(FieldTop { at, past, fit }));
        let moved = node.scrolled != scroll;
        node.scrolled = scroll;
        moved
    }
}

fn find_mut(node: &mut ResolvedNode, id: NodeId) -> Option<&mut ResolvedNode> {
    if node.id == id {
        return Some(node);
    }
    node.children.iter_mut().find_map(|child| find_mut(child, id))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{apply_at, full, surface_from};
    use super::*;
    use crate::text::shaping::tests::gated_shots_fixture;

    const FIELD: &str = r#"return panel { id = "bar", child = column { children = { textfield { width = 80,
        font_size = 10, line_height = 1, multiline = true, max_lines = 3, on_change = function() end } } } }"#;

    #[test]
    fn a_multiline_field_grows_with_its_draft_to_max_lines_and_keeps_its_scroll_across_passes() {
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(FIELD);
        let mut scene = Scene::new();
        let surfaces = std::slice::from_ref(&surface);
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        let field = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].children[0].clone();
        let id = field(&scene).id;
        assert_eq!(field(&scene).rect.height, 10.0, "one row empty");
        let mut grow = |text: &str| {
            let resized = scene.set_field_draft("bar@TEST", id, text, &shaping);
            apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
            (resized, field(&scene).rect.height)
        };
        assert_eq!(grow("a\nb"), (true, 20.0));
        assert_eq!(grow("a\nb\nc\nd\ne"), (true, 30.0), "capped at max_lines");
        assert_eq!(grow("a\nb\nc\nd"), (false, 30.0), "still past the cap: no pass owed");
        // An anchor at row 0 that a walk on every pass would scroll back to.
        assert!(scene.set_field_scroll("bar@TEST", id, 10.0, Some((0, 0.0))));
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        assert_eq!(field(&scene).scrolled, 10.0, "a pass keeps the rows where the app scrolled them");
    }

    /// Rows counted with boxes are recounted once the face lands (#55): at 10 px a box is 6 px
    /// wide and `●` 8.94, so ten of them wrap to one row, then two.
    #[test]
    fn a_draft_wrapped_before_a_face_landed_is_recounted() {
        let Some((shaping, open)) = gated_shots_fixture(crate::text::fonts::covering_file) else { return };
        let (lua, surface) = surface_from(FIELD);
        let mut scene = Scene::new();
        let surfaces = std::slice::from_ref(&surface);
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        let id = scene.surface("bar@TEST").unwrap().children[0].children[0].id;
        assert!(!scene.set_field_draft("bar@TEST", id, "●●●●● ●●●●●", &shaping), "one row of boxes");
        crate::text::shaping::tests::land(&shaping, open);
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        assert!(scene.set_field_draft("bar@TEST", id, "●●●●●", &shaping), "two rows shrink to one");
    }

    /// #111, #125: an unfocused field scrolled before a face landed keeps the text its top row
    /// showed, at the width of the pass the face lands in: `x` tops the box at row 2 in boxes; in
    /// the face each `●` line wraps in two at 80 px (row 4) and in none at 200 px (row 2), 5 px down.
    #[test]
    fn a_field_scrolled_before_a_face_landed_keeps_its_top_row_resized_or_not() {
        for (resize, scrolled) in [("", 45.0), ("w:set(200)", 25.0)] {
            let Some((shaping, open)) = gated_shots_fixture(crate::text::fonts::covering_file) else { return };
            let (lua, surface) = surface_from(&format!("w = state(\"w\", 80)\n{}", FIELD.replace("80", "w")));
            let mut scene = Scene::new();
            let surfaces = std::slice::from_ref(&surface);
            apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
            let id = scene.surface("bar@TEST").unwrap().children[0].children[0].id;
            scene.set_field_draft("bar@TEST", id, "●●●●● ●●●●●\n●●●●● ●●●●●\nx\ny\nz\nq", &shaping);
            apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
            // Row 2, `x`, starts past two lines of ten 3-byte `●`, a space and a newline.
            assert!(scene.set_field_scroll("bar@TEST", id, 25.0, Some((64, 5.0))));
            crate::text::shaping::tests::land(&shaping, open);
            lua.load(resize).exec().unwrap();
            apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
            assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children[0].scrolled, scrolled, "{resize}");
            assert_eq!(scene.field_top(id), Some((64, 5.0)), "{resize}");
        }
    }

    /// #125: a field relaid at another line height keeps its top line and the share of it scrolled
    /// past: `x`, byte 4, on row 2, half a row down is 50 px at 20 px rows and 25 at 10.
    #[test]
    fn a_field_relaid_at_another_line_height_keeps_its_top_line() {
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(&format!("lh = state(\"lh\", 2)\n{}", FIELD.replace("line_height = 1", "line_height = lh")));
        let mut scene = Scene::new();
        let surfaces = std::slice::from_ref(&surface);
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        let id = scene.surface("bar@TEST").unwrap().children[0].children[0].id;
        scene.set_field_draft("bar@TEST", id, "a\nb\nx\ny\nz\nq\nr\ns", &shaping);
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        assert!(scene.set_field_scroll("bar@TEST", id, 50.0, Some((4, 10.0))));
        for (lh, scrolled) in [(1, 25.0), (2, 50.0)] {
            lua.load(format!("lh:set({lh})")).exec().unwrap();
            apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
            assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children[0].scrolled, scrolled, "{lh}");
        }
    }

    #[test]
    fn a_rebuilt_or_reshown_field_still_measures_its_draft() {
        let shaping = ShapingHandle::spawn();
        let source = FIELD.replace("textfield {", "textfield { visible = shown,");
        let (lua, surface) = surface_from(&format!("shown = state(\"shown\", true)\n{source}"));
        let mut scene = Scene::new();
        let surfaces = std::slice::from_ref(&surface);
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        let field = |scene: &Scene| scene.surface("bar@TEST").unwrap().children[0].children[0].clone();
        assert!(scene.set_field_draft("bar@TEST", field(&scene).id, "a\nb", &shaping));
        // As a failed pass leaves it: the next pass builds every solver node again.
        scene.solver_trees.clear();
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        assert_eq!(field(&scene).rect.height, 20.0, "a new solver node measures the draft the scene holds");
        for shown in ["false", "true"] {
            lua.load(format!("shown:set({shown})")).exec().unwrap();
            apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        }
        assert_eq!(field(&scene).rect.height, 20.0, "and so does a field shown again");
    }

    /// Node ids are never reused, so a draft left behind by a removed field would never be read again.
    #[test]
    fn a_removed_field_drops_its_draft() {
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"return panel { id = "bar", child = column { children = state("kids", {
                textfield { width = 80, multiline = true, on_change = function() end } }) } }"#,
        );
        let mut scene = Scene::new();
        let surfaces = std::slice::from_ref(&surface);
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        let id = scene.surface("bar@TEST").unwrap().children[0].children[0].id;
        scene.set_field_draft("bar@TEST", id, "a\nb", &shaping);
        assert!(scene.field_wrapped(id).is_some());
        lua.load(r#"state("kids", {}):set({})"#).exec().unwrap();
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        assert!(scene.field_drafts.is_empty(), "a field no surface holds keeps no draft");
    }

    /// `has_selection` names follow the fields' `focus_target`s: a gone target's name goes, and the
    /// live one keeps its written cell for a reader that asks by name and holds nothing between runs.
    #[test]
    fn has_selection_names_last_as_long_as_a_field_declares_them() {
        let shaping = ShapingHandle::spawn();
        let lua = crate::layout::node::scene_lua();
        crate::lua::focus::register(&lua).unwrap();
        let surface = lua.load(
            r#"n = state("n", 1)
            return panel { id = "bar", child = n:map(function(n)
                return textfield { width = 80, focus_target = focus_target("f" .. n), on_change = function() end }
            end) }"#,
        );
        let surface = crate::lua::nodes::deserialize_lua_table(&surface.eval().unwrap()).unwrap();
        let mut scene = Scene::new();
        let surfaces = std::slice::from_ref(&surface);
        let selected = |n: usize| lua.load(format!(r#"return has_selection("f{n}"):get()"#)).eval::<bool>().unwrap();
        for n in 1..=50 {
            lua.load(format!("n:set({n})")).exec().unwrap();
            assert!(!selected(n));
            crate::lua::signal::write_selection(&lua, Some(&format!("f{n}")));
            lua.gc_collect().unwrap();
            apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
            assert!(selected(n), "f{n} kept its written cell through the sweep");
            assert_eq!(crate::lua::signal::selection_names(&lua), 1, "only f{n} has a field");
        }
    }

    #[test]
    fn only_a_multiline_field_is_seeded_with_a_newline() {
        let shaping = ShapingHandle::spawn();
        for (multiline, ok) in [("true", true), ("false", false)] {
            let (lua, surface) = surface_from(&format!(
                r#"return panel {{ id = "bar", child = textfield {{ width = 80, multiline = {multiline},
                    initial_text = "a\nb", on_change = function() end }} }}"#
            ));
            let applied = apply_at(&mut Scene::new(), &[surface], full(), &shaping, &lua);
            assert_eq!(applied.is_ok(), ok, "multiline = {multiline}");
        }
    }
}

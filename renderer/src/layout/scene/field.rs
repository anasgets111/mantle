//! What a `multiline` `textfield` takes from the app that holds its draft: the text it measures
//! and how far its rows scroll.

use std::collections::HashMap;

use super::{NodeId, ResolvedNode, Scene};
use crate::layout::node::PaintStyle;
use crate::layout::{field_rows, hit};
use crate::text::shaping::ShapingHandle;

/// A multiline field's draft, and the rows it wrapped to at a width, so a change re-wraps once.
#[derive(Default)]
pub(super) struct FieldDraft {
    pub(super) text: String,
    wrapped: (f32, usize),
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
        let width = field_rows::wrap_width(node.rect.width, caret.width);
        let old = self.field_drafts.get(&id);
        if old.map_or(text.is_empty(), |old| old.text == text && old.wrapped.0 == width) {
            return false;
        }
        let count = |text: &str| field_rows::count(text, face, width, shaping);
        let before = match old {
            Some(FieldDraft { wrapped: (at, rows), .. }) if *at == width => *rows,
            old => count(old.map_or("", |old| old.text.as_str())),
        };
        let after = count(text);
        match text.is_empty() {
            true => self.field_drafts.remove(&id),
            false => self.field_drafts.insert(id, FieldDraft { text: text.to_owned(), wrapped: (width, after) }),
        };
        // The measure reads the draft, not its inputs, so taffy's cache must be told.
        if let (Some(taffy), Some(tree)) = (node.taffy, self.solver_trees.get_mut(instance_id)) {
            let _ = tree.mark_dirty(taffy);
        }
        lines.rows(before) != lines.rows(after)
    }

    /// The width the field `id`'s draft last wrapped at, `None` without one.
    pub fn field_wrap_width(&self, id: NodeId) -> Option<f32> {
        self.field_drafts.get(&id).map(|draft| draft.wrapped.0)
    }

    /// Drops the drafts of fields no surface holds any more.
    pub(super) fn forget_gone_fields(&mut self) {
        let surfaces = &self.surfaces;
        self.field_drafts.retain(|id, _| surfaces.values().any(|tree| hit::contains_node(tree, *id)));
    }

    /// Scrolls the field `id`'s rows to `scroll` px; whether that moved them.
    pub fn set_field_scroll(&mut self, instance_id: &str, id: NodeId, scroll: f32) -> bool {
        let Some(node) = self.surfaces.get_mut(instance_id).and_then(|tree| find_mut(tree, id)) else { return false };
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
        assert!(scene.set_field_scroll("bar@TEST", id, 10.0));
        apply_at(&mut scene, surfaces, full(), &shaping, &lua).unwrap();
        assert_eq!(field(&scene).scrolled, 10.0, "a pass keeps the rows where the app scrolled them");
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

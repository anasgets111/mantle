//! AT-SPI trees derived from the same resolved nodes that paint and input use.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use accesskit::{
    Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId, Rect, Role, TreeId,
    TreeInfo, TreeUpdate,
};
use accesskit_unix::Adapter;

use super::*;
use crate::layout::node::fields::common;

struct Activate(crate::wake::Waker, Arc<AtomicBool>);
impl ActivationHandler for Activate {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.1.store(true, Ordering::Release);
        self.0.wake();
        None
    }
}

struct Actions {
    surface_id: String,
    tx: Sender<(String, ActionRequest)>,
    wake: crate::wake::Waker,
}
impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        if self.tx.send((self.surface_id.clone(), request)).is_ok() {
            self.wake.wake();
        }
    }
}

struct Deactivate;
impl DeactivationHandler for Deactivate {
    fn deactivate_accessibility(&mut self) {}
}

pub(super) struct Accessibility {
    adapters: HashMap<String, Adapter>,
    tx: Sender<(String, ActionRequest)>,
    rx: Receiver<(String, ActionRequest)>,
    wake: crate::wake::Waker,
    activation: Arc<AtomicBool>,
    sent: HashMap<String, SentState>,
}

/// What a surface's last tree update reflected; an unchanged scene with the same state skips the
/// rebuild.
#[derive(PartialEq)]
struct SentState {
    focus: Option<layout::scene::NodeId>,
    window_focused: bool,
    field_revision: u64,
}

impl Accessibility {
    pub fn new(wake: crate::wake::Waker) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            adapters: HashMap::new(),
            tx,
            rx,
            wake,
            activation: Arc::new(AtomicBool::new(false)),
            sent: HashMap::new(),
        }
    }
}

fn append(
    node: &layout::ResolvedNode,
    origin: (f32, f32),
    out: &mut Vec<(NodeId, Node)>,
    root: bool,
    plain: Option<(layout::scene::NodeId, &str)>,
) {
    if (!node.visible || node.leaving) && !root {
        return;
    }
    let rect = node.at(origin.0, origin.1);
    let target = if node.kind == "textfield" {
        match node.paint.as_ref() {
            Some(node::PaintStyle::TextField { target: Some(_), .. }) => Role::PasswordInput,
            _ => Role::TextInput,
        }
    } else if root {
        Role::Window
    } else if node.kind == "text" {
        Role::Label
    } else if layout::scene::is_named_click_target(node) {
        Role::Button
    } else {
        Role::GenericContainer
    };
    let mut accessible = Node::new(target);
    accessible.set_bounds(Rect::new(
        rect.x as f64,
        rect.y as f64,
        (rect.x + rect.width) as f64,
        (rect.y + rect.height) as f64,
    ));
    if let Ok(name) = common::accessible_name.read(&node.properties)
        && !name.is_empty()
    {
        accessible.set_label(name);
    }
    if let Some(node::PaintStyle::Text { content, .. }) = node.paint.as_ref() {
        accessible.set_value(content.to_string());
    }
    if target == Role::TextInput
        && let Some((id, value)) = plain
        && id == node.id
    {
        accessible.set_value(value.to_owned());
    }
    if target == Role::Button {
        accessible.add_action(Action::Focus);
        accessible.add_action(Action::Click);
    } else if target == Role::TextInput || target == Role::PasswordInput {
        accessible.add_action(Action::Focus);
    }
    let children: Vec<_> = if node.visible && !node.leaving {
        node.content_children().filter(|child| child.visible && !child.leaving).collect()
    } else {
        Vec::new()
    };
    accessible.set_children(children.iter().map(|child| NodeId(child.id.raw())).collect::<Vec<_>>());
    out.push((NodeId(node.id.raw()), accessible));
    for child in children {
        append(child, (rect.x, rect.y), out, false, plain);
    }
}

// ponytail: sends the whole tree on every change; diff against the last update if large trees lag.
fn tree_update(
    root: &layout::ResolvedNode,
    focus: Option<layout::scene::NodeId>,
    plain: Option<(layout::scene::NodeId, &str)>,
) -> TreeUpdate {
    let mut nodes = Vec::new();
    append(root, (0.0, 0.0), &mut nodes, true, plain);
    let root_id = NodeId(root.id.raw());
    let mut tree = TreeInfo::new(root_id);
    tree.toolkit_name = Some("Mantle".into());
    // AccessKit panics on a focus outside the node list; a reload can drop the focused node
    // before the next prune.
    let focus =
        focus.map(|id| NodeId(id.raw())).filter(|id| nodes.iter().any(|(node, _)| node == id)).unwrap_or(root_id);
    TreeUpdate { nodes, tree: Some(tree), tree_id: TreeId::ROOT, focus }
}

impl App {
    pub(in crate::wayland) fn process_accessibility_actions(&mut self) {
        while let Ok((surface_id, request)) = self.accessibility.rx.try_recv() {
            if !self.accessibility.adapters.contains_key(&surface_id) {
                continue;
            }
            match request.action {
                Action::Focus => self.accessibility_action(&surface_id, request.target_node.0, false),
                Action::Click => self.accessibility_action(&surface_id, request.target_node.0, true),
                _ => {}
            }
        }
    }

    pub(in crate::wayland) fn sync_accessibility(&mut self, scene_changed: bool) {
        let activated = self.accessibility.activation.swap(false, Ordering::AcqRel);
        let scope = self.keyboard_focus_scope();
        let focus_surface = self
            .focused_control
            .as_ref()
            .map(|focus| focus.surface_id.as_str())
            .or_else(|| self.focused_secure_submit.as_ref().map(|field| field.surface_id.as_str()))
            .filter(|id| scope.iter().any(|surface| surface == id))
            .or(self.keyboard_focus.as_deref());
        let live: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| surface.role.wl_surface().is_some())
            .map(|surface| surface.surface_id.clone())
            .collect();
        let active: Vec<_> = live
            .iter()
            .filter_map(|id| {
                let root = self.client.scene().surface(id)?;
                let focused = focus_surface == Some(id.as_str());
                let focus = if focused {
                    self.focused_control
                        .as_ref()
                        .filter(|control| &control.surface_id == id)
                        .map(|control| control.id)
                        .or_else(|| {
                            self.focused_secure_submit
                                .as_ref()
                                .filter(|field| &field.surface_id == id)
                                .filter(|field| {
                                    input::keyboard::secure_target_at(root, field.id) == Some(&field.target)
                                })
                                .map(|field| field.id)
                        })
                } else {
                    None
                };
                let stamp = SentState { focus, window_focused: focused, field_revision: self.field_revision };
                if !scene_changed && !activated && self.accessibility.sent.get(id) == Some(&stamp) {
                    return None;
                }
                let plain = self
                    .focused_text_field
                    .as_ref()
                    .filter(|field| &field.surface_id == id)
                    .map(|field| (field.id, field.buffer.as_str()));
                Some((id.clone(), tree_update(root, focus, plain), focused, stamp))
            })
            .collect();
        self.accessibility.adapters.retain(|id, _| live.contains(id));
        self.accessibility.sent.retain(|id, _| live.contains(id));
        for (id, update, focused, stamp) in active {
            let adapter = self.accessibility.adapters.entry(id.clone()).or_insert_with(|| {
                Adapter::new(
                    Activate(self.accessibility.wake.clone(), self.accessibility.activation.clone()),
                    Actions {
                        surface_id: id.clone(),
                        tx: self.accessibility.tx.clone(),
                        wake: self.accessibility.wake.clone(),
                    },
                    Deactivate,
                )
            });
            adapter.update_window_focus_state(focused);
            adapter.update_if_active(|| update);
            self.accessibility.sent.insert(id, stamp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::input::keyboard::tests::{plain_textfield, secure_submit_table, textfield};

    #[test]
    fn hidden_and_leaving_nodes_are_absent_from_the_accessibility_tree() {
        let mut hidden = layout::ResolvedNode::test("text", (0.0, 0.0, 10.0, 10.0), vec![]);
        hidden.visible = false;
        hidden.id = layout::scene::NodeId::test(2);
        let mut leaving = layout::ResolvedNode::test("text", (0.0, 0.0, 10.0, 10.0), vec![]);
        leaving.leaving = true;
        leaving.id = layout::scene::NodeId::test(3);
        let mut root = layout::ResolvedNode::test("panel", (0.0, 0.0, 50.0, 50.0), vec![hidden, leaving]);
        root.id = layout::scene::NodeId::test(1);
        let update = tree_update(&root, None, None);
        assert_eq!(update.nodes.len(), 1);
        assert_eq!(update.focus, NodeId(1));
        let update = tree_update(&root, Some(layout::scene::NodeId::test(2)), None);
        assert_eq!(update.focus, NodeId(1), "a focus the tree omits falls back to the root");
    }

    #[test]
    fn plain_value_is_exposed_and_password_content_is_not() {
        let lua = Lua::new();
        let plain = plain_textfield(&lua);
        let plain_id = plain.id;
        let secure = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let secure_id = secure.id;
        let mut root = layout::ResolvedNode::test("panel", (0.0, 0.0, 200.0, 40.0), vec![plain, secure]);
        root.id = layout::scene::NodeId::test(900);
        let update = tree_update(&root, Some(plain_id), Some((plain_id, "draft")));
        let plain = &update.nodes.iter().find(|(id, _)| *id == NodeId(plain_id.raw())).unwrap().1;
        let secure = &update.nodes.iter().find(|(id, _)| *id == NodeId(secure_id.raw())).unwrap().1;
        assert_eq!(plain.role(), Role::TextInput);
        assert_eq!(plain.value(), Some("draft"));
        assert_eq!(secure.role(), Role::PasswordInput);
        assert_eq!(secure.value(), None);
    }
}

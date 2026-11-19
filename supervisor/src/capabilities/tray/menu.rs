//! DBusMenu `GetLayout` decoding: the `(ia{sv}av)` tree read straight into [`MenuItem`]s
//! (ADR-0031). A `zvariant::Value` holds an `icon-data` array as one `Value` per byte, so the tree
//! is decoded by typed seeds instead, and nodes past the depth or node cap are skipped unread.

use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use shared::debug;
pub use shared::state::tray::{MenuItem, MenuType, ToggleType};
use zbus::zvariant::{Signature, Type};

use super::proxies::DBusMenuProxy;
use super::{MAX_MENU_NODES, MAX_TRAY_TEXT_BYTES};
use crate::capabilities::{Dict, DictFields, Hint, shm_icons, truncate_utf8_bytes};

/// Maximum byte size for a menu item's raw `icon-data` PNG (512 KB).
const MAX_MENU_ICON_BYTES: usize = 512 * 1024;

/// 8-byte PNG file header.
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Recursion cap for [`Node`]. A session-bus peer controls `GetLayout`, so an encoded deep tree
/// could otherwise stack-overflow this task. 32 is generous; real menus rarely nest more than a
/// handful of levels.
const MAX_MENU_DEPTH: u32 = 32;

/// One node: `(ia{sv}av)`, id, properties, children.
const NODE: &Signature = &Signature::static_structure(&[&Signature::I32, Dict::<NodeProps>::SIGNATURE, CHILDREN]);
const CHILDREN: &Signature = &Signature::static_array(&Signature::Variant);

/// The `GetLayout` reply: revision and root node.
const LAYOUT: &Signature = &Signature::static_structure(&[&Signature::U32, NODE]);

/// The node properties [`menu_item`] reads, as the application sent them.
#[derive(Default)]
struct NodeProps {
    kind: Option<String>,
    label: Option<String>,
    enabled: Option<bool>,
    visible: Option<bool>,
    icon_name: Option<String>,
    icon_data: Option<Vec<u8>>,
    toggle_type: Option<String>,
    toggle_state: Option<i32>,
}

impl DictFields for NodeProps {
    fn read<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error> {
        match key {
            "type" => self.kind = map.next_value_seed(Hint::new())?,
            "label" => self.label = map.next_value_seed(Hint::new())?,
            "enabled" => self.enabled = map.next_value_seed(Hint::new())?,
            "visible" => self.visible = map.next_value_seed(Hint::new())?,
            "icon-name" => self.icon_name = map.next_value_seed(Hint::new())?,
            "icon-data" => self.icon_data = map.next_value_seed(Hint::new())?,
            "toggle-type" => self.toggle_type = map.next_value_seed(Hint::new())?,
            "toggle-state" => self.toggle_state = map.next_value_seed(Hint::new())?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// A visible node as decoded, before [`menu_item`] caps its text and spools its icon. Spooling
/// waits for the whole reply: a decode failing part way would otherwise leave files no
/// [`MenuItem`] names, which `reap_menu_icons` never deletes.
struct MenuNode {
    id: i32,
    props: NodeProps,
    children: Vec<MenuNode>,
}

/// With `reply`, the `GetLayout` reply `(u(ia{sv}av))`, else the root node inside it: either way
/// decoded to the root's children, leaving the root's own properties unread.
struct Layout<'a> {
    reply: bool,
    budget: &'a mut usize,
}

impl Type for Layout<'_> {
    const SIGNATURE: &'static Signature = LAYOUT;
}

impl<'de> DeserializeSeed<'de> for Layout<'_> {
    type Value = Vec<MenuNode>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Layout<'_> {
    type Value = Vec<MenuNode>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a GetLayout reply")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        seq.next_element::<IgnoredAny>()?;
        if self.reply {
            return Ok(seq.next_element_seed(Layout { reply: false, budget: self.budget })?.unwrap_or_default());
        }
        seq.next_element::<IgnoredAny>()?;
        Ok(seq.next_element_seed(Children { depth: 1, budget: self.budget })?.unwrap_or_default())
    }
}

/// An `av` of child nodes at `depth`.
struct Children<'a> {
    depth: u32,
    budget: &'a mut usize,
}

impl<'de> DeserializeSeed<'de> for Children<'_> {
    type Value = Vec<MenuNode>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Children<'_> {
    type Value = Vec<MenuNode>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DBusMenu children")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut children = Vec::new();
        while let Some(child) = seq.next_element_seed(Node { depth: self.depth, budget: &mut *self.budget })? {
            children.extend(child);
        }
        Ok(children)
    }
}

/// One child: a variant holding a node at `depth`, `None` when hidden, not a node, or past the
/// shared [`MAX_MENU_NODES`] `budget`. Depth alone leaves breadth unbounded, and it is breadth an
/// application reaches for by accident: one level of a million siblings sits well inside
/// [`MAX_MENU_DEPTH`]. What is not kept is skipped with `IgnoredAny`, allocating nothing.
struct Node<'a> {
    depth: u32,
    budget: &'a mut usize,
}

/// A variant on its own, as a child of `av`.
impl Type for Node<'_> {
    const SIGNATURE: &'static Signature = &Signature::Variant;
}

impl<'de> DeserializeSeed<'de> for Node<'_> {
    type Value = Option<MenuNode>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("Variant", &["signature", "value"], self)
    }
}

impl<'de> Visitor<'de> for Node<'_> {
    type Value = Option<MenuNode>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a DBusMenu node")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let signature: Signature = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        if *self.budget == 0 {
            seq.next_element::<IgnoredAny>()?;
            return Ok(None);
        }
        *self.budget -= 1;
        // ponytail: a child not shaped `(ia{sv}av)` is dropped; no known toolkit sends another. Upgrade: peel variants.
        if signature != *NODE {
            seq.next_element::<IgnoredAny>()?;
            return Ok(None);
        }
        Ok(seq.next_element_seed(NodeFields(self))?.flatten())
    }
}

/// The `(ia{sv}av)` inside a [`Node`]'s variant.
struct NodeFields<'a>(Node<'a>);

impl<'de> DeserializeSeed<'de> for NodeFields<'_> {
    type Value = Option<MenuNode>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for NodeFields<'_> {
    type Value = Option<MenuNode>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a DBusMenu node")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let Node { depth, budget } = self.0;
        let id = seq.next_element()?.unwrap_or_default();
        let props = seq.next_element::<Dict<NodeProps>>()?.map(|Dict(props)| props).unwrap_or_default();
        if props.visible == Some(false) {
            seq.next_element::<IgnoredAny>()?;
            return Ok(None);
        }
        let children = if depth >= MAX_MENU_DEPTH {
            debug!(2; "GetLayout reply exceeded the maximum menu depth ({MAX_MENU_DEPTH}) at node id {id}; truncating its children");
            seq.next_element::<IgnoredAny>()?;
            Vec::new()
        } else {
            seq.next_element_seed(Children { depth: depth + 1, budget })?.unwrap_or_default()
        };
        Ok(Some(MenuNode { id, props, children }))
    }
}

/// Caps `node`'s text, applies DBusMenu's defaults, and spools its `icon-data` PNG when it names
/// no icon.
fn menu_item(node: MenuNode, item_stem: &str) -> MenuItem {
    let MenuNode { id, props, children } = node;
    // DBusMenu bounds none of these and the application owns them all.
    let capped = |text: String| truncate_utf8_bytes(&text, MAX_TRAY_TEXT_BYTES);
    let mut icon_name = props.icon_name.map(capped).filter(|name| !name.is_empty());
    if icon_name.is_none()
        && let Some(bytes) = props.icon_data
        && bytes.len() <= MAX_MENU_ICON_BYTES
        && bytes.starts_with(PNG_MAGIC)
    {
        match shm_icons::write_png(super::icon::SPOOL_SUBDIR, &format!("{item_stem}_menu_{id}.png"), &bytes) {
            Ok(path) => icon_name = Some(path),
            Err(err) => debug!("failed to spool DBusMenu icon for node {id}: {err}"),
        }
    }
    let toggle_type = match props.toggle_type.as_deref() {
        Some("checkmark") => Some(ToggleType::Checkmark),
        Some("radio") => Some(ToggleType::Radio),
        _ => None,
    };
    MenuItem {
        id,
        menu_type: if props.kind.as_deref() == Some("separator") { MenuType::Separator } else { MenuType::Standard },
        label: props.label.map(capped),
        enabled: props.enabled.unwrap_or(true),
        icon_name,
        toggle_type,
        toggle_state: toggle_type.map(|_| props.toggle_state.unwrap_or(-1)),
        children: children.into_iter().map(|child| menu_item(child, item_stem)).collect(),
    }
}

/// Total [`MenuItem`] nodes in `items` and all nested submenus.
pub(super) fn count_menu_nodes(items: &[MenuItem]) -> usize {
    items.iter().map(|item| 1 + count_menu_nodes(&item.children)).sum()
}

/// Truncates `items` in pre-order to at most `*remaining` total nodes (including descendants),
/// appending dropped subtrees to `dropped` so their spooled icons can be deleted.
pub(super) fn truncate_menu_nodes(items: &mut Vec<MenuItem>, remaining: &mut usize, dropped: &mut Vec<MenuItem>) {
    let mut keep = 0;
    for item in items.iter_mut() {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        keep += 1;
        truncate_menu_nodes(&mut item.children, remaining, dropped);
    }
    if keep < items.len() {
        dropped.extend(items.drain(keep..));
    }
}

pub(super) async fn fetch_menu_via(
    menu: &DBusMenuProxy<'static>,
    item_stem: &str,
    max_nodes: usize,
) -> zbus::Result<Vec<MenuItem>> {
    // The root is one of the budgeted nodes, so one GetLayout reply never exceeds MAX_MENU_NODES - 1
    // MenuItems; a smaller `max_nodes` reflects the shared tray-wide remaining node budget.
    let cap = max_nodes.min(MAX_MENU_NODES - 1);
    if cap == 0 {
        return Ok(Vec::new());
    }
    let reply = menu.inner().call_method("GetLayout", &(0i32, -1i32, Vec::<&str>::new())).await?;
    let body = reply.body();
    if body.signature() != LAYOUT {
        return Err(zbus::Error::Failure(format!("GetLayout replied ({}), not {LAYOUT}", body.signature())));
    }
    let mut budget = cap;
    let (nodes, _) = body.data().deserialize_with_seed(Layout { reply: true, budget: &mut budget })?;
    // Reported here rather than at the node that ran out: exhaustion stops every remaining sibling
    // and ancestor alike, so warning inside the recursion means one line per ancestor for one reply.
    if budget == 0 {
        debug!("GetLayout reply hit the {cap}-node cap; the rest of the menu was dropped");
    }
    Ok(nodes.into_iter().map(|node| menu_item(node, item_stem)).collect())
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::serialized::Context;
    use zbus::zvariant::{Array, Dict, LE, Str, StructureBuilder, Value, to_bytes};

    use super::*;
    use crate::capabilities::shm_icons::init_test_spool;

    /// Decodes `value` as one `av` child at `depth`, as `GetLayout` delivers it.
    fn parse_menu_node(value: &Value<'_>, depth: u32, budget: &mut usize, item_stem: &str) -> Option<MenuItem> {
        let encoded = to_bytes(Context::new_dbus(LE, 0), value).unwrap();
        let (node, _) = encoded.deserialize_with_seed(Node { depth, budget }).unwrap();
        node.map(|node| menu_item(node, item_stem))
    }

    /// Parses with the full node allowance, which is what every test but the cap's own wants.
    fn parse(value: &Value<'_>, depth: u32) -> Option<MenuItem> {
        parse_menu_node(value, depth, &mut { MAX_MENU_NODES }, "stem")
    }

    fn menu_node_value<'a>(id: i32, properties: Vec<(&'a str, Value<'a>)>, children: Vec<Value<'a>>) -> Value<'a> {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        for (key, value) in properties {
            dict.append(Value::Str(Str::from(key)), Value::Value(Box::new(value)))
                .expect("dict insert must succeed in this test");
        }
        let mut array = Array::new(&Signature::Variant);
        for child in children {
            array.append(Value::Value(Box::new(child))).expect("array insert must succeed in this test");
        }
        let structure = StructureBuilder::new()
            .add_field(id)
            .append_field(Value::Dict(dict))
            .append_field(Value::Array(array))
            .build()
            .expect("well-formed test structure");
        Value::Structure(structure)
    }

    #[test]
    fn parse_menu_node_reads_the_properties_and_defaults_the_absent_ones() {
        let text = |s: &'static str| Value::Str(Str::from(s));
        // (case, properties, (type, label, enabled, icon name, toggle type, toggle state))
        let cases = [
            (
                "standard leaf",
                vec![("type", text("standard")), ("label", text("Quit")), ("enabled", Value::Bool(true))],
                (MenuType::Standard, Some("Quit"), true, None, None, None),
            ),
            ("defaults", vec![], (MenuType::Standard, None, true, None, None, None)),
            ("separator", vec![("type", text("separator"))], (MenuType::Separator, None, true, None, None, None)),
            ("disabled", vec![("enabled", Value::Bool(false))], (MenuType::Standard, None, false, None, None, None)),
            (
                "toggle type and state",
                vec![("toggle-type", text("checkmark")), ("toggle-state", Value::I32(1))],
                (MenuType::Standard, None, true, None, Some(ToggleType::Checkmark), Some(1)),
            ),
            (
                "toggle state defaults to -1",
                vec![("toggle-type", text("radio"))],
                (MenuType::Standard, None, true, None, Some(ToggleType::Radio), Some(-1)),
            ),
            ("unknown type", vec![("type", text("menu"))], (MenuType::Standard, None, true, None, None, None)),
            (
                "unknown toggle type drops the state",
                vec![("toggle-type", text("switch")), ("toggle-state", Value::I32(1))],
                (MenuType::Standard, None, true, None, None, None),
            ),
            (
                "icon name",
                vec![("icon-name", text("edit-cut"))],
                (MenuType::Standard, None, true, Some("edit-cut"), None, None),
            ),
        ];
        for (case, properties, expected) in cases {
            let item = parse(&menu_node_value(7, properties, vec![]), 0).expect(case);
            let got = (
                item.menu_type,
                item.label.as_deref(),
                item.enabled,
                item.icon_name.as_deref(),
                item.toggle_type,
                item.toggle_state,
            );
            assert_eq!((item.id, got, item.children.is_empty()), (7, expected, true), "{case}");
        }
    }

    #[test]
    fn parse_menu_node_recurses_into_children_at_any_depth() {
        let label = |text: &'static str| vec![("label", Value::Str(Str::from(text)))];
        let grandchild = menu_node_value(21, label("Deep"), vec![]);
        let child_a = menu_node_value(11, label("Copy"), vec![grandchild]);
        let child_b = menu_node_value(12, label("Paste"), vec![]);
        let root = menu_node_value(0, vec![], vec![child_a, child_b]);

        let item = parse(&root, 0).expect("must parse a well-formed node");
        assert_eq!(item.children.len(), 2);
        assert_eq!((item.children[0].id, item.children[0].label.as_deref()), (11, Some("Copy")));
        assert_eq!((item.children[1].id, item.children[1].label.as_deref()), (12, Some("Paste")));
        assert_eq!(
            (item.children[0].children[0].id, item.children[0].children[0].label.as_deref()),
            (21, Some("Deep"))
        );
    }

    #[test]
    fn parse_menu_node_rejects_a_non_structure_value() {
        assert_eq!(parse(&Value::I32(42), 0), None);
    }

    #[test]
    fn parse_menu_node_truncates_at_the_depth_cap_without_panicking_or_overflowing() {
        // A deeper chain models a malicious or buggy `GetLayout`; unbounded recursion would allow a
        // stack-overflow DoS from any peer owning a tray item's `Menu` object.
        fn deep_chain(remaining: u32, id: i32) -> Value<'static> {
            if remaining == 0 {
                menu_node_value(id, vec![], vec![])
            } else {
                menu_node_value(id, vec![], vec![deep_chain(remaining - 1, id + 1)])
            }
        }

        // D-Bus caps nesting at 64 containers, three per level, so start near the cap.
        let start = MAX_MENU_DEPTH - 5;
        let root = deep_chain(10, 0);

        let item = parse(&root, start).expect("the root node itself must still parse");

        let mut current = &item;
        let mut depth = 0;
        while !current.children.is_empty() {
            current = &current.children[0];
            depth += 1;
        }
        assert_eq!(
            depth,
            MAX_MENU_DEPTH - start,
            "parsing must truncate children exactly at the depth cap, not keep recursing into the deeper levels the raw tree actually has"
        );
    }

    /// Depth is not the only way a reply gets large, and breadth is the one an application reaches
    /// for: these siblings all sit at depth 1.
    #[test]
    fn a_reply_wider_than_the_node_budget_is_truncated_rather_than_built() {
        let children: Vec<Value<'_>> =
            (0..MAX_MENU_NODES * 2).map(|id| menu_node_value(id as i32, Vec::new(), Vec::new())).collect();
        let root = menu_node_value(0, Vec::new(), children);

        let mut budget = MAX_MENU_NODES;
        let parsed = parse_menu_node(&root, 0, &mut budget, "stem").expect("the root itself must still parse");

        // The root spends one, so the survivors are the rest of the allowance.
        assert_eq!(parsed.children.len(), MAX_MENU_NODES - 1);
        assert_eq!(budget, 0, "the budget is what stopped it");
    }

    /// The budget spans the whole reply, not one node's children, so nesting cannot spend more than
    /// breadth would.
    #[test]
    fn the_node_budget_is_shared_across_the_whole_tree_not_per_level() {
        let mut node = menu_node_value(0, Vec::new(), Vec::new());
        for id in 1..10 {
            node = menu_node_value(id, Vec::new(), vec![node]);
        }

        let mut budget = 4;
        parse_menu_node(&node, 0, &mut budget, "stem").expect("the root must parse");
        assert_eq!(budget, 0, "ten nested nodes must not spend more than the four allowed");
    }

    /// DBusMenu bounds no property, and the application owns every one of them.
    #[test]
    fn an_over_long_label_is_capped_rather_than_carried() {
        let long = "x".repeat(MAX_TRAY_TEXT_BYTES * 4);
        let value = menu_node_value(
            1,
            vec![("label", Value::Str(Str::from(long.as_str()))), ("icon-name", Value::Str(Str::from(long.as_str())))],
            Vec::new(),
        );

        let item = parse(&value, 0).expect("a well-formed node with long text still parses");
        assert_eq!(item.label.as_deref().map(str::len), Some(MAX_TRAY_TEXT_BYTES));
        assert_eq!(item.icon_name.as_deref().map(str::len), Some(MAX_TRAY_TEXT_BYTES));
    }

    #[test]
    fn parse_menu_node_filters_out_invisible_items() {
        let invisible = menu_node_value(1, vec![("visible", Value::Bool(false))], Vec::new());
        assert_eq!(parse(&invisible, 0), None);

        let visible = menu_node_value(1, vec![("visible", Value::Bool(true))], Vec::new());
        assert!(parse(&visible, 0).is_some());
    }

    fn byte_array_value(bytes: &[u8]) -> Value<'_> {
        let mut array = Array::new(&Signature::U8);
        for &byte in bytes {
            array.append(Value::U8(byte)).expect("append u8");
        }
        Value::Array(array)
    }

    #[test]
    fn parse_menu_node_spools_valid_png_icon_data_when_icon_name_absent() {
        init_test_spool();
        let png_bytes = b"\x89PNG\r\n\x1a\nfake_png_payload";
        let value = menu_node_value(10, vec![("icon-data", byte_array_value(png_bytes))], vec![]);

        let item = parse_menu_node(&value, 0, &mut { MAX_MENU_NODES }, "test_app").expect("must parse");
        let path = item.icon_name.expect("spooled path must be set");
        assert!(path.ends_with("test_app_menu_10.png"), "path was {path}");
        let on_disk = std::fs::read(&path).expect("spooled file must exist");
        assert_eq!(on_disk, png_bytes);
    }

    #[test]
    fn parse_menu_node_rejects_non_png_and_oversized_icon_data() {
        init_test_spool();
        let mut oversized = Vec::with_capacity(MAX_MENU_ICON_BYTES + 1);
        oversized.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        oversized.resize(MAX_MENU_ICON_BYTES + 1, 0);
        for (case, bytes) in [("not a png", b"not-a-png".to_vec()), ("oversized", oversized)] {
            let value = menu_node_value(11, vec![("icon-data", byte_array_value(&bytes))], vec![]);
            let item = parse_menu_node(&value, 0, &mut { MAX_MENU_NODES }, "test_app").expect("must parse");
            assert_eq!(item.icon_name, None, "{case}");
        }
    }

    #[test]
    fn parse_menu_node_prefers_a_non_empty_icon_name_over_icon_data() {
        init_test_spool();
        let png_bytes = b"\x89PNG\r\n\x1a\npayload";
        for (name, expected) in [("document-open", "document-open"), ("", "test_app_menu_13.png")] {
            let value = menu_node_value(
                13,
                vec![("icon-name", Value::Str(Str::from(name))), ("icon-data", byte_array_value(png_bytes))],
                vec![],
            );
            let item = parse_menu_node(&value, 0, &mut { MAX_MENU_NODES }, "test_app").expect("must parse");
            assert!(item.icon_name.expect("an icon").ends_with(expected), "icon-name {name:?}");
        }
    }

    /// The bytes land in one `Vec<u8>`, not one `zvariant::Value` each, and a key no field names
    /// is walked without being kept.
    #[test]
    fn icon_data_decodes_to_bytes_and_an_unknown_key_is_skipped() {
        init_test_spool();
        let huge = vec![7; 1024 * 1024];
        let mut png = PNG_MAGIC.to_vec();
        png.resize(512 * 1024, 1);
        let child = menu_node_value(
            2,
            vec![("x-unknown", byte_array_value(&huge)), ("icon-data", byte_array_value(&png))],
            Vec::new(),
        );
        let root = menu_node_value(1, vec![("icon-data", byte_array_value(&huge))], vec![child]);
        let encoded = to_bytes(Context::new_dbus(LE, 0), &root).unwrap();

        let (node, _) = encoded.deserialize_with_seed(Node { depth: 0, budget: &mut { MAX_MENU_NODES } }).unwrap();
        let node = node.expect("a well-formed node");
        assert_eq!(node.props.icon_data.as_deref(), Some(huge.as_slice()));
        let item = menu_item(node, "big");
        assert_eq!(item.icon_name, None, "past MAX_MENU_ICON_BYTES");
        let path = item.children[0].icon_name.as_deref().expect("the PNG at the cap is spooled");
        assert_eq!(std::fs::read(path).unwrap(), png);
    }

    struct CapturingMenu {
        depths: std::sync::Mutex<Vec<i32>>,
        child_count: i32,
    }

    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl CapturingMenu {
        async fn get_layout(
            &self,
            _parent: i32,
            depth: i32,
            _names: Vec<String>,
        ) -> (u32, super::super::proxies::RawMenuLayout) {
            self.depths.lock().unwrap().push(depth);
            let children: Vec<Value<'static>> = (1..=self.child_count)
                .map(|id| {
                    Value::from((
                        id,
                        std::collections::HashMap::<String, Value<'static>>::new(),
                        Vec::<Value<'static>>::new(),
                    ))
                })
                .collect();
            let raw_children = children.into_iter().map(|v| v.try_into().unwrap()).collect();
            (1, (0, std::collections::HashMap::new(), raw_children))
        }
    }

    #[tokio::test]
    async fn fetch_menu_via_caps_at_max_menu_nodes_minus_one_and_passes_full_depth() {
        let bus = crate::capabilities::test_support::private_bus().await;
        let menu_obj =
            CapturingMenu { depths: std::sync::Mutex::new(Vec::new()), child_count: (MAX_MENU_NODES + 5) as i32 };
        let app = bus.builder().serve_at("/MenuBar", menu_obj).unwrap().build().await.unwrap();
        let conn = bus.connection().await;
        let dest = zbus::names::BusName::from(app.unique_name().unwrap().clone()).into();
        let path = zbus::zvariant::OwnedObjectPath::try_from("/MenuBar").unwrap();
        let proxy = super::super::proxies::bind_dbusmenu(&conn, &dest, &path).await.unwrap();

        assert!(fetch_menu_via(&proxy, "stem", 0).await.unwrap().is_empty());
        let fetched = fetch_menu_via(&proxy, "stem", usize::MAX).await.unwrap();
        assert_eq!(fetched.len(), MAX_MENU_NODES - 1);

        let depths = app
            .object_server()
            .interface::<_, CapturingMenu>("/MenuBar")
            .await
            .unwrap()
            .get()
            .await
            .depths
            .lock()
            .unwrap()
            .clone();
        assert_eq!(depths, vec![-1], "zero budget skips GetLayout; non-zero asks for full depth (-1)");
    }
}

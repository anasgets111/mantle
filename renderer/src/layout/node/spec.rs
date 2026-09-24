//! Surface specs, rebuild fingerprints, child-list value types, and masked `SecureSubmitTarget`.
//! List generation also owns duplicate-key rejection.

use std::collections::HashSet;

use mlua::Value;

use crate::lua::nodes::{DeserializeError, VirtualNode, deserialize_lua_table};

use super::*;
use fields::list;

/// A `lock` is in `BOX_KINDS` (`crate::lua::nodes`) and accepts the common and box properties;
/// [`lock_spec`] refuses only `visible`, `width`, and `height` (ADR-0052 decision 2). `child` is
/// walked into the retained tree, so the spec carries no layout field. It stays a struct rather
/// than `SurfaceSpec::Lock(String)`, giving [`lock_spec`] a place to attach those refusals. There
/// is no `LockTopology`: the protocol exposes only `ack_configure`, with size supplied by configure,
/// so only declaration existence is fingerprinted.
#[derive(Debug, Clone, PartialEq)]
pub struct LockSpec {
    pub id: String,
}

/// Refuses unsupported properties before reading `id`. Ignoring `visible` could tear down a
/// compositor-owned lock at `locked`/`unlock_and_destroy`, causing ADR-0042's solid-color fallback;
/// the error reaches `rescue`'s `error_log` (ADR-0046) while unlocked. `width` and `height` are
/// inert because configure owns geometry and lock surfaces cover every output (ADR-0052 decision
/// 2), but silent no-ops are still errors. A `Signal` under a refused key is refused too.
pub fn lock_spec(properties: &PropMap) -> Result<LockSpec, LayoutError> {
    use fields::lock;
    lock::visible.read(properties)?;
    lock::width.read(properties)?;
    lock::height.read(properties)?;
    Ok(LockSpec { id: fields::surface::id.read(properties)? })
}

/// One declared top-level surface, parsed by its role (ADR-0040 decision 1). Declaration order
/// remains in the roster and fingerprint, so one enum preserves it across roles.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceSpec {
    Panel(PanelSpec),
    Window(WindowSpec),
    Popup(PopupSpec),
    Lock(LockSpec),
}

impl SurfaceSpec {
    /// The declared id used to match a `SurfaceInstance` back to its `VirtualNode`.
    pub fn declared_id(&self) -> &str {
        match self {
            SurfaceSpec::Panel(spec) => &spec.topology.id,
            SurfaceSpec::Window(spec) => &spec.id,
            SurfaceSpec::Popup(spec) => &spec.id,
            SurfaceSpec::Lock(spec) => &spec.id,
        }
    }

    pub fn fingerprint(&self) -> SurfaceFingerprint {
        match self {
            SurfaceSpec::Panel(spec) => SurfaceFingerprint::Panel(spec.topology.clone()),
            SurfaceSpec::Window(spec) => SurfaceFingerprint::Window(spec.id.clone()),
            SurfaceSpec::Popup(spec) => SurfaceFingerprint::Popup(spec.id.clone()),
            SurfaceSpec::Lock(spec) => SurfaceFingerprint::Lock(spec.id.clone()),
        }
    }
}

/// A declaration's creation-time fields: one whose fingerprint changed is rebuilt in place
/// (ADR-0216). A `panel` carries all five topology fields; `window`, `popup`, and `lock` carry only
/// `id` because their other fields update live or rebuild per open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceFingerprint {
    Panel(SurfaceTopology),
    Window(String),
    Popup(String),
    Lock(String),
}

/// [`deserialize_lua_table`] with a refused kind kept as itself, so a child of an unsupported kind
/// still fails the pass as one rather than as a malformed `children` entry.
fn deserialize_child(table: &mlua::Table, property: &str) -> Result<VirtualNode, LayoutError> {
    deserialize_lua_table(table).map_err(|e| match e {
        DeserializeError::UnsupportedKind(kind) => LayoutError::UnsupportedNodeKind(kind),
        other => invalid(property, other.to_string()),
    })
}

/// A surface root's one `child`, converted with `deserialize_lua_table`.
impl Prop for VirtualNode {
    type Out = Option<VirtualNode>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<VirtualNode>, LayoutError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let Value::Table(table) = value else {
            return Err(invalid(row.name, format!("expected a node table, got {}", preview_for_error(value))));
        };
        Ok(Some(deserialize_child(table, row.name)?))
    }
}

/// A `panel`'s or `lock`'s `child`: a node, or a function of the output's connector name that
/// `layout::scene::pass::build_child_for_output` calls before this reads its node (ADR-0121).
pub(crate) struct Root;

impl LuaType for Root {
    fn lua() -> String {
        let builder = crate::lua::luacats::fun(&[("output", String::lua)], Some((VirtualNode::lua, true)));
        format!("{}|{builder}", VirtualNode::lua())
    }
}

impl Prop for Root {
    type Out = Option<VirtualNode>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<VirtualNode>, LayoutError> {
        VirtualNode::read(row, value)
    }
}

/// An array-of-nodes `children` property.
pub(crate) struct Children;

impl LuaType for Children {
    fn lua() -> String {
        Vec::<VirtualNode>::lua()
    }
}

impl Prop for Children {
    type Out = Vec<VirtualNode>;
    fn read(_: &Property, value: Option<&Value>) -> Result<Vec<VirtualNode>, LayoutError> {
        let Some(value) = value else {
            return Ok(Vec::new());
        };
        let Value::Table(table) = value else {
            return Err(invalid("children", format!("expected an array table, got {}", preview_for_error(value))));
        };
        // A config controls `#children`, and a sparse table's border can be enormous, so the hint is
        // capped at what the loop below accepts.
        let mut children = Vec::with_capacity(table.raw_len().min(MAX_ARRAY_ELEMENTS));
        // By index to `#children`, not `sequence_values`: that stopped at the first nil, silently
        // dropping every child after it.
        for index in 1..=table.raw_len() {
            if children.len() == MAX_ARRAY_ELEMENTS {
                return Err(invalid("children", format!("more than {MAX_ARRAY_ELEMENTS} children in one node")));
            }
            let entry: Value = table.raw_get(index).map_err(|e| invalid("children", e.to_string()))?;
            let Value::Table(entry) = entry else {
                return Err(invalid(
                    "children",
                    format!("expected a node table at index {index}, got {}", preview_for_error(&entry)),
                ));
            };
            children.push(deserialize_child(&entry, "children")?);
        }
        Ok(children)
    }
}

/// A `list`'s `source`: absent, or a signal still reading nil before its first push, is an empty
/// list.
pub(crate) struct Items;

impl LuaType for Items {
    fn lua() -> String {
        Vec::<Value>::lua()
    }
}

impl Prop for Items {
    type Out = Option<mlua::Table>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<mlua::Table>, LayoutError> {
        match value {
            None => Ok(None),
            Some(Value::Table(source)) => Ok(Some(source.clone())),
            Some(other) => Err(invalid(row.name, format!("expected an array table, got {}", preview_for_error(other)))),
        }
    }
}

/// A `list`'s `limit`, capped at what one list builds. ponytail: it bounds item construction for
/// search and launchers; viewport windowing is the upgrade (ADR-0191).
pub(crate) struct Limit;

impl LuaType for Limit {
    fn lua() -> String {
        i64::lua()
    }
}

impl Prop for Limit {
    type Out = Option<usize>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<usize>, LayoutError> {
        match value {
            None => Ok(None),
            Some(Value::Integer(n)) if *n >= 0 => Ok(Some((*n as usize).min(MAX_ARRAY_ELEMENTS))),
            Some(Value::Number(n)) if *n >= 0.0 && n.fract() == 0.0 => Ok(Some((*n as usize).min(MAX_ARRAY_ELEMENTS))),
            Some(other) => {
                Err(invalid(row.name, format!("expected a non-negative integer, got {}", preview_for_error(other))))
            }
        }
    }
}

/// A `list`'s children (ADR-0045 decision 3) are generated once per resolved
/// `source` item; it arrives already resolved, so a `Signal` there was read exactly once before
/// `itemfn` runs. Without `key`, reconciliation is positional. With it, `key(element)`
/// is called on the source value, not the built node, and overwrites that node's `id`; duplicate
/// keys fail here before `pair_children_by_id_then_position` sees them.
///
/// ponytail: `key` speeds reconciliation, not evaluation. A 30-item tray still runs `itemfn` 30
/// times and discards 29 fresh nodes on ADR-0044 decision 2's per-poll-turn capability-push
/// cadence. `list` is a "fast-reconciling virtual repeater"; skipping unchanged items
/// needs retained-side data, which `children_of` does not provide. That is worth about 19% of the
/// pass (ADR-0132); the whole of it is a viewport, measured at 22us a row by
/// `layout::scene::tests::list_pass_cost` and designed in ADR-0191.
pub fn parse_list_children(properties: &PropMap) -> Result<Vec<VirtualNode>, LayoutError> {
    let source = list::source.read(properties)?;
    let itemfn = list::itemfn.read(properties)?.expect("`itemfn` is required");
    let key_fn = list::key.read(properties)?;
    let limit = list::limit.read(properties)?;

    let Some(source) = source else {
        return Ok(Vec::new());
    };
    let mut children = Vec::with_capacity(source.raw_len().min(limit.unwrap_or(MAX_ARRAY_ELEMENTS)));
    let mut seen_keys: HashSet<String> = HashSet::new();
    for element in source.sequence_values::<Value>() {
        if limit.is_some_and(|lim| children.len() == lim) {
            break;
        }
        if children.len() == MAX_ARRAY_ELEMENTS {
            return Err(invalid("source", format!("more than {MAX_ARRAY_ELEMENTS} items in one list")));
        }
        let element = element.map_err(|e| invalid("source", e.to_string()))?;

        let built = itemfn.call::<Value>(&element).map_err(|e| invalid("itemfn", e.to_string()))?;
        let Value::Table(built_table) = built else {
            return Err(invalid("itemfn", format!("expected a node table, got {}", preview_for_error(&built))));
        };
        let mut node = deserialize_child(&built_table, "itemfn")?;

        if let Some(key_fn) = &key_fn {
            // Moved, not borrowed: a borrow would keep this item rooted for the rest of the loop
            // body, which a `key` collecting garbage behind a weak table can see.
            let key_value = key_fn.call::<Value>(element).map_err(|e| invalid("key", e.to_string()))?;
            let Value::String(key_str) = key_value else {
                return Err(invalid(
                    "key",
                    format!("expected key(item) to return a string, got {}", preview_for_error(&key_value)),
                ));
            };
            let key_text = key_str.to_str().map(|s| (*s).to_owned()).map_err(|_| {
                invalid(
                    "key",
                    "must be valid UTF-8 -- a key is compared for equality, so it cannot be converted lossily",
                )
            })?;
            if seen_keys.contains(&key_text) {
                return Err(invalid("key", format!("duplicate key `{key_text}` among list items")));
            }
            seen_keys.insert(key_text);
            // List identity wins over any `id` the item function supplied.
            node.properties.insert("id", Value::String(key_str));
        }

        children.push(node);
    }
    Ok(children)
}

/// `textfield.secure_submit` routes a masked field's committed buffer without Lua
/// (ADR-0005, ADR-0027). Enter is read from `wl_keyboard` in `renderer/src/wayland/input/keyboard/secure.rs`, not
/// `zwp_text_input_v3`; the pair keys `RendererFrame::SecureSubmit` (ADR-0050 decision 4). See
/// `secure_key_action` for why a password must bypass the input-method bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureSubmitTarget {
    pub capability: String,
    pub action: String,
}

impl LuaType for SecureSubmitTarget {
    fn lua() -> String {
        let name = String::lua();
        format!("{{ capability: {name}, action: {name}, [string]: \"no such property\" }}")
    }
}

/// `secure_submit` is optional because an unread mask is unreadable from Lua, and it
/// is non-structural, so signal-bound values arrive resolved. `capability`/`action` reject
/// non-UTF-8 rather than collapsing distinct bytes onto one Supervisor capability name, as a
/// node's `id` does.
impl Prop for SecureSubmitTarget {
    type Out = Option<SecureSubmitTarget>;
    fn read(_: &Property, value: Option<&Value>) -> Result<Option<SecureSubmitTarget>, LayoutError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let Value::Table(table) = value else {
            return Err(invalid("secure_submit", format!("expected a table, got {}", preview_for_error(value))));
        };
        only_keys("secure_submit", table, &["capability", "action"])?;
        let field = |key: &str| -> Result<String, LayoutError> {
            let v: Value = table.get(key).map_err(|e| invalid("secure_submit", e.to_string()))?;
            let s = match v {
                Value::Nil => return Err(invalid("secure_submit", format!("`{key}` is required"))),
                Value::String(s) => s,
                other => {
                    return Err(invalid(
                        "secure_submit",
                        format!("`{key}` must be a string, got {}", preview_for_error(&other)),
                    ));
                }
            };
            let s = s.to_str().map(|s| s.to_string()).map_err(|_| {
                invalid(
                    "secure_submit",
                    format!(
                        "`{key}` must be valid UTF-8 -- it addresses a Supervisor capability, so it cannot be converted lossily"
                    ),
                )
            })?;
            if s.is_empty() {
                return Err(invalid("secure_submit", format!("`{key}` must not be empty")));
            }
            Ok(s)
        };
        let (capability, action) = (field("capability")?, field("action")?);
        if !SECURE_SUBMIT_TARGETS.contains(&(capability.as_str(), action.as_str())) {
            let known: Vec<String> = SECURE_SUBMIT_TARGETS.iter().map(|(c, a)| format!("`{c}`/`{a}`")).collect();
            return Err(invalid(
                "secure_submit",
                format!("`{capability}`/`{action}` receives no password; it takes {}", known.join(", ")),
            ));
        }
        Ok(Some(SecureSubmitTarget { capability, action }))
    }
}

/// The pairs `supervisor/src/main.rs` routes a secret to; its fallback arm drops any other one, so
/// a password typed into a field aimed elsewhere would vanish.
const SECURE_SUBMIT_TARGETS: [(&str, &str); 3] =
    [("lock", "authenticate"), ("network", "connect"), ("polkit", "authenticate")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_walks_nested_node_tables() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua
                .load(r#"return { kind = "row", children = { { kind = "text", content = "a" }, { kind = "text", content = "b" } } }"#)
                .eval()
                .unwrap();
        let props = props_from_table(&table);
        let children = fields::stack::children.read(&props).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].kind, "text");
        assert_eq!(children[1].properties.get("content").unwrap().as_string().unwrap().to_string_lossy(), "b");
    }

    #[test]
    fn a_single_child_converts_the_child_table() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "panel", child = { kind = "rect" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let child = fields::root::child.read(&props).unwrap();
        assert_eq!(child.unwrap().kind, "rect");
    }

    #[test]
    fn a_single_child_absent_is_none() {
        let props = PropMap::default();
        assert!(fields::root::child.read(&props).unwrap().is_none());
    }

    #[test]
    fn secure_submit_refuses_an_unrouted_pair_and_an_unknown_key() {
        let lua = mlua::Lua::new();
        for (source, expected) in [
            (
                r#"{ capability = "lock", action = "connect" }"#,
                "`lock`/`connect` receives no password; it takes `lock`/`authenticate`, `network`/`connect`, `polkit`/`authenticate`",
            ),
            (
                r#"{ capability = "lock", action = "authenticate", acton = "x" }"#,
                "unknown key `acton`; it takes `capability`, `action`",
            ),
        ] {
            let table: mlua::Table =
                lua.load(format!("return {{ kind = \"textfield\", secure_submit = {source} }}")).eval().unwrap();
            let err = fields::textfield::secure_submit.read(&props_from_table(&table)).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail == expected),
                "{err}"
            );
        }
    }

    #[test]
    fn secure_submit_absent_is_none() {
        let props = PropMap::default();
        assert_eq!(fields::textfield::secure_submit.read(&props).unwrap(), None);
    }

    #[test]
    fn secure_submit_well_formed_table_parses_capability_and_action() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "network", action = "connect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            fields::textfield::secure_submit.read(&props).unwrap(),
            Some(SecureSubmitTarget { capability: "network".to_string(), action: "connect".to_string() })
        );
    }

    #[test]
    fn secure_submit_missing_capability_is_invalid_property_naming_the_field() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "textfield", secure_submit = { action = "connect" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::textfield::secure_submit.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("capability")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_missing_action_is_invalid_property_naming_the_field() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "textfield", secure_submit = { capability = "network" } }"#).eval().unwrap();
        let props = props_from_table(&table);
        let err = fields::textfield::secure_submit.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("action")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_empty_capability_is_invalid_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "", action = "connect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = fields::textfield::secure_submit.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("capability")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_empty_action_is_invalid_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua
            .load(r#"return { kind = "textfield", secure_submit = { capability = "network", action = "" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let err = fields::textfield::secure_submit.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "secure_submit" && detail.contains("action")),
            "got {err}"
        );
    }

    #[test]
    fn secure_submit_non_table_value_is_invalid_property() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "textfield", secure_submit = "network.connect" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            fields::textfield::secure_submit.read(&props).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "secure_submit"
        ));
    }

    #[test]
    fn secure_submit_non_utf8_capability_is_rejected_rather_than_lossily_converted() {
        let lua = mlua::Lua::new();
        let table = lua.create_table().unwrap();
        table.set("kind", "textfield").unwrap();
        let inner = lua.create_table().unwrap();
        inner.set("capability", lua.create_string(b"\xff").unwrap()).unwrap();
        inner.set("action", "connect").unwrap();
        table.set("secure_submit", inner).unwrap();
        let props = props_from_table(&table);
        let err = fields::textfield::secure_submit.read(&props).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "secure_submit"),
            "a non-UTF-8 secure_submit field must be a LayoutError naming the property: {err:?}"
        );
    }

    #[test]
    fn lock_spec_reads_the_id_and_carries_nothing_else() {
        let lua = mlua::Lua::new();
        let table: mlua::Table =
            lua.load(r#"return { kind = "lock", id = "screen-lock", child = { kind = "rect" } }"#).eval().unwrap();
        assert_eq!(lock_spec(&props_from_table(&table)).unwrap(), LockSpec { id: "screen-lock".to_string() });
    }

    #[test]
    fn a_lock_without_an_id_is_rejected_the_same_way_every_other_role_is() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "lock" }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "id"
        ));
    }

    #[test]
    fn every_property_a_lock_denies_is_refused_by_name_rather_than_ignored() {
        let lua = mlua::Lua::new();
        // `monitor` and `anchor` never reach `lock_spec` from a config any more: they are not on
        // a `lock` row in `nodes::properties::PROPERTIES`, so `deserialize_lua_table` refuses them first
        // (`a_lock_property_that_is_not_even_on_the_kind_is_refused_before_lock_spec_sees_it`).
        // The three left here are ones a lock legitimately has a row for and refuses anyway.
        for property in ["visible", "width", "height"] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "lock", id = "screen-lock", {property} = 1 }}"#)).eval().unwrap();
            let err = lock_spec(&props_from_table(&table)).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property),
                "`{property}` must be refused by name, got {err:?}"
            );
        }
    }

    /// The other half of the lock's denial, one layer up. A name a `lock` has no row for cannot
    /// reach `lock_spec` at all, so the refusal a config author sees is the property gate's.
    #[test]
    fn a_lock_property_that_is_not_even_on_the_kind_is_refused_before_lock_spec_sees_it() {
        let lua = mlua::Lua::new();
        for property in ["monitor", "anchor"] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "lock", id = "screen-lock", {property} = 1 }}"#)).eval().unwrap();
            let err = crate::lua::nodes::deserialize_lua_table(&table).unwrap_err();
            assert!(err.to_string().contains(property), "`{property}` must be refused by name, got {err}");
        }
    }

    #[test]
    fn a_refused_lock_property_wins_over_a_missing_id_because_it_is_the_error_that_teaches() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load(r#"return { kind = "lock", visible = false }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "visible"
        ));
    }

    #[test]
    fn a_signal_bound_lock_property_is_refused_on_the_evaluation_pass_like_a_literal_one() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { kind = "lock", id = "screen-lock", visible = state("v", true) }"#).eval().unwrap();
        assert!(matches!(
            lock_spec(&props_from_table(&table)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "visible"
        ));
    }

    #[test]
    fn a_signal_in_a_lock_id_is_rejected_by_the_universal_structural_arm_with_no_new_carve_out() {
        let lua = mlua::Lua::new();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { kind = "lock", id = state("i", "screen-lock") }"#).eval().unwrap();
        let resolved = resolve_properties(props_from_table(&table), "lock", &lua).unwrap();
        assert!(matches!(
            lock_spec(&resolved).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "id"
        ));
    }

    #[test]
    fn a_lock_fingerprints_on_its_id_alone_so_only_its_existence_is_a_topology_change() {
        let spec = SurfaceSpec::Lock(LockSpec { id: "screen-lock".to_string() });
        assert_eq!(spec.declared_id(), "screen-lock");
        assert_eq!(spec.fingerprint(), SurfaceFingerprint::Lock("screen-lock".to_string()));
        assert_ne!(spec.fingerprint(), SurfaceFingerprint::Window("screen-lock".to_string()));
    }

    /// Depth was capped and breadth was not, and the pass budget cannot cover the gap: filling this
    /// array is a Rust loop with no Lua in it, so the deadline hook never runs.
    #[test]
    fn a_children_array_wider_than_the_cap_is_a_config_error_rather_than_an_allocation() {
        let lua = mlua::Lua::new();
        crate::lua::nodes::register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local kids = {}
                for i = 1, 10001 do kids[i] = rect { width = 1, height = 1 } end
                return kids
                "#,
            )
            .eval()
            .unwrap();
        let mut properties = PropMap::default();
        properties.insert("children", Value::Table(table));

        let err = fields::stack::children.read(&properties).expect_err("past the cap this must be refused");
        assert!(format!("{err:?}").contains("more than"), "the error has to say what to fix: {err:?}");
    }

    /// The cap must not be in the way of anything a real config builds.
    #[test]
    fn an_ordinary_children_array_is_unaffected() {
        let lua = mlua::Lua::new();
        crate::lua::nodes::register_node_constructors(&lua).unwrap();
        let table: mlua::Table =
            lua.load(r#"return { rect { width = 1, height = 1 }, rect { width = 2, height = 2 } }"#).eval().unwrap();
        let mut properties = PropMap::default();
        properties.insert("children", Value::Table(table));

        assert_eq!(fields::stack::children.read(&properties).unwrap().len(), 2);
    }

    #[test]
    fn parse_list_children_bounds_to_limit_and_refuses_invalid_values() {
        let lua = mlua::Lua::new();
        crate::lua::nodes::register_node_constructors(&lua).unwrap();
        let eval = |s: &str| -> Result<usize, LayoutError> {
            let t: mlua::Table = lua.load(s).eval().unwrap();
            parse_list_children(&props_from_table(&t)).map(|c| c.len())
        };
        assert_eq!(
            eval("return list { source = { 1, 2, 3, 4 }, limit = 2, itemfn = function(i) return rect { width = i, height = i } end }").unwrap(),
            2
        );
        assert_eq!(
            eval("return list { source = { 1, 2 }, limit = 0, itemfn = function() error('unreached') end }").unwrap(),
            0
        );
        assert!(matches!(
            eval("return list { source = { 1 }, limit = -1, itemfn = function() return rect {} end }").unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "limit"
        ));
        assert_eq!(eval("return list { itemfn = function() return rect {} end }").unwrap(), 0, "no source is empty");
        assert!(matches!(
            eval("return list { source = {} }").unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "itemfn"
        ));
    }

    /// `sequence_values` stopped at the hole and dropped `b` without a word.
    #[test]
    fn a_nil_hole_in_children_is_refused_by_index() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua
            .load(r#"return { kind = "row", children = { { kind = "rect" }, nil, { kind = "rect" } } }"#)
            .eval()
            .unwrap();
        let err = fields::stack::children.read(&props_from_table(&table)).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "children" && detail == "expected a node table at index 2, got Nil"),
            "{err}"
        );
    }
}

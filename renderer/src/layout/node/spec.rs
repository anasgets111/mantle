//! Surface specs, rebuild fingerprints, child-list value types, and masked `SecureSubmitTarget`.

use mlua::Value;

use crate::lua::nodes::{DeserializeError, VirtualNode, deserialize_lua_table};

use super::*;

/// A `lock` is a box kind (`paint_style`'s `boxed`) and accepts the common and box properties;
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
    /// The role's name, as the config and the logs spell it.
    pub fn role(&self) -> &'static str {
        match self {
            SurfaceSpec::Panel(_) => "panel",
            SurfaceSpec::Window(_) => "window",
            SurfaceSpec::Popup(_) => "popup",
            SurfaceSpec::Lock(_) => "lock",
        }
    }

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
pub(super) fn deserialize_child(
    table: &mlua::Table,
    property: &str,
    index: Option<usize>,
) -> Result<VirtualNode, LayoutError> {
    deserialize_lua_table(table).map_err(|e| match e {
        DeserializeError::UnsupportedKind(kind) => LayoutError::UnsupportedNodeKind(kind),
        other => {
            // The step already names the node, so no header; a kind-less table is labelled by its property.
            let kind = match &other {
                DeserializeError::UnknownProperty { kind, .. } => kind.as_str(),
                _ => property,
            };
            invalid("", other.to_string()).in_node(kind, index, crate::lua::nodes::site_of(table))
        }
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
        Ok(Some(deserialize_child(table, row.name, None)?))
    }
}

/// A `panel`'s or `lock`'s `child`: a node, or a function of the output's connector name that
/// `layout::scene::children::build_child_for_output` calls before this reads its node (ADR-0121).
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

spelled!(Children => Vec::<VirtualNode>::lua());

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
        // A misspelled child does not hide its siblings' mistakes.
        let mut failed = Vec::new();
        // By index to `#children`, not `sequence_values`: that stopped at the first nil, silently
        // dropping every child after it.
        for index in 1..=table.raw_len() {
            if index > MAX_ARRAY_ELEMENTS {
                return Err(invalid("children", format!("more than {MAX_ARRAY_ELEMENTS} children in one node")));
            }
            let entry: Value = table.raw_get(index).map_err(|e| invalid("children", e.to_string()))?;
            let child = match entry {
                Value::Table(entry) => deserialize_child(&entry, "children", Some(index - 1)),
                other => Err(invalid(
                    "children",
                    format!("children[{}]: expected a node table, got {}", index, preview_for_error(&other)),
                )),
            };
            match child {
                Ok(child) => children.push(child),
                Err(err) => failed.push(err),
            }
        }
        if !failed.is_empty() {
            return Err(LayoutError::many(failed));
        }
        Ok(children)
    }
}

/// `textfield.secure_submit` routes a masked field's committed buffer without Lua
/// (ADR-0005, ADR-0027). Enter is read from `wl_keyboard` in `renderer/src/wayland/input/keyboard/secure.rs`, not
/// `zwp_text_input_v3`; the pair keys `RendererFrame::SecureSubmit` (ADR-0050 decision 4). See
/// `secure_key_action` for why a password must bypass the input-method bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureSubmitTarget {
    pub capability: String,
    pub action: String,
    pub name: Option<String>,
}

spelled!(SecureSubmitTarget => r#"{ capability: string, action: string, name?: string, [string]: "no such property" }"#);

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
        only_keys("secure_submit", table, &["capability", "action", "name"])?;
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
            let known: Vec<String> = SECURE_SUBMIT_TARGETS.iter().map(|(c, a)| format!("{c}/{a}")).collect();
            let hint = crate::lua::hint::hint(
                &format!("{capability}/{action}"),
                &known.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            return Err(invalid(
                "secure_submit",
                format!("`capability`/`action` receives no password, got string \"{capability}/{action}\"; {hint}"),
            ));
        }
        let name: Option<String> = table.get("name").map_err(|e| invalid("secure_submit", e.to_string()))?;
        let needs_name = if capability == "secrets" {
            Some("`secrets`/`store` needs a `name` without control characters, at most 128 bytes")
        } else if capability == "bluetooth" {
            Some(
                "`bluetooth`/`pair` needs a request id and MAC in `name`, without control characters, at most 128 bytes",
            )
        } else if action == "vpn_secret" {
            Some(
                "`network`/`vpn_secret` needs a request id and key in `name`, without control characters, at most 128 bytes",
            )
        } else {
            None
        };
        if let Some(message) = needs_name {
            if !name.as_deref().is_some_and(shared::valid_secret_name) {
                return Err(invalid("secure_submit", message));
            }
        } else if name.is_some() {
            return Err(invalid(
                "secure_submit",
                "`name` is only valid for `secrets`/`store`, `bluetooth`/`pair` or `network`/`vpn_secret`",
            ));
        }
        Ok(Some(SecureSubmitTarget { capability, action, name }))
    }
}

/// The pairs `supervisor/src/main.rs` routes a secret to; its fallback arm drops any other one, so
/// a password typed into a field aimed elsewhere would vanish.
const SECURE_SUBMIT_TARGETS: [(&str, &str); 6] = [
    ("lock", "authenticate"),
    ("network", "connect"),
    ("network", "vpn_secret"),
    ("polkit", "authenticate"),
    ("secrets", "store"),
    ("bluetooth", "pair"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::signal_lua;

    #[test]
    fn children_walks_nested_node_tables() {
        let lua = mlua::Lua::new();
        let props = props_from_src(
            &lua,
            r#"return { kind = "row", children = { { kind = "text", content = "a" }, { kind = "text", content = "b" } } }"#,
        );
        let children = fields::stack::children.read(&props).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].kind, "text");
        assert_eq!(children[1].properties.get("content").unwrap().as_string().unwrap().to_string_lossy(), "b");
    }

    #[test]
    fn a_single_child_converts_the_child_table() {
        let lua = mlua::Lua::new();
        let props = props_from_src(&lua, r#"return { kind = "panel", child = { kind = "rect" } }"#);
        let child = fields::root::child.read(&props).unwrap();
        assert_eq!(child.unwrap().kind, "rect");
        assert!(fields::root::child.read(&PropMap::default()).unwrap().is_none());
    }

    #[test]
    fn secure_submit_well_formed_table_parses_capability_and_action() {
        let lua = mlua::Lua::new();
        let props = props_from_src(
            &lua,
            r#"return { kind = "textfield", secure_submit = { capability = "network", action = "connect" } }"#,
        );
        assert_eq!(
            fields::textfield::secure_submit.read(&props).unwrap(),
            Some(SecureSubmitTarget { capability: "network".to_string(), action: "connect".to_string(), name: None })
        );
        assert_eq!(fields::textfield::secure_submit.read(&PropMap::default()).unwrap(), None);
    }

    #[test]
    fn secrets_store_requires_a_valid_name_and_other_targets_refuse_it() {
        let lua = mlua::Lua::new();
        for (target, valid) in [
            (r#"{ capability = "secrets", action = "store", name = "mail" }"#, true),
            (r#"{ capability = "secrets", action = "store" }"#, false),
            (r#"{ capability = "secrets", action = "store", name = "\n" }"#, false),
            (r#"{ capability = "lock", action = "authenticate", name = "mail" }"#, false),
            (r#"{ capability = "bluetooth", action = "pair", name = "1/00:11:22:33:44:55" }"#, true),
            (r#"{ capability = "bluetooth", action = "pair" }"#, false),
            (r#"{ capability = "bluetooth", action = "pair", name = "\n" }"#, false),
            (r#"{ capability = "bluetooth", action = "pair", name = string.rep("a", 129) }"#, false),
            (r#"{ capability = "network", action = "vpn_secret", name = "1/password" }"#, true),
            (r#"{ capability = "network", action = "vpn_secret" }"#, false),
            (r#"{ capability = "network", action = "connect", name = "1/password" }"#, false),
        ] {
            assert_eq!(
                fields::textfield::secure_submit
                    .read(&props_from_src(&lua, format!("return {{ kind = 'textfield', secure_submit = {target} }}")))
                    .is_ok(),
                valid,
                "{target}"
            );
        }
    }

    #[test]
    fn secure_submit_malformed_values_are_rejected() {
        let lua = mlua::Lua::new();
        for (value, needle) in [
            (r#"{ action = "connect" }"#, "capability"),
            (r#"{ capability = "network" }"#, "action"),
            (r#"{ capability = "", action = "connect" }"#, "capability"),
            (r#"{ capability = "network", action = "" }"#, "action"),
            (r#""network.connect""#, ""),
            (
                r#"{ capability = "lock", action = "connect" }"#,
                "`capability`/`action` receives no password, got string \"lock/connect\"; expected one of `lock/authenticate`, `network/connect`, `network/vpn_secret`, `polkit/authenticate`, `secrets/store`, `bluetooth/pair`",
            ),
            (
                r#"{ capability = "lock", action = "authenticate", acton = "x" }"#,
                "unknown key `acton`; did you mean `action`?",
            ),
        ] {
            let props = props_from_src(&lua, format!("return {{ kind = 'textfield', secure_submit = {value} }}"));
            let err = fields::textfield::secure_submit.read(&props).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "secure_submit" && detail.contains(needle)),
                "{value}: {err}"
            );
        }
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
        assert_eq!(
            lock_spec(&props_from_src(
                &lua,
                r#"return { kind = "lock", id = "screen-lock", child = { kind = "rect" } }"#
            ))
            .unwrap(),
            LockSpec { id: "screen-lock".to_string() }
        );
    }

    #[test]
    fn a_lock_without_an_id_or_with_a_property_it_denies_is_refused_by_name() {
        let lua = signal_lua();
        // `output` and `anchor` never reach `lock_spec` from a config: they are not on a `lock`
        // field in `nodes::properties`, so `deserialize_lua_table` refuses them first
        // (`a_lock_property_that_is_not_even_on_the_kind_is_refused_before_lock_spec_sees_it`).
        // The denied ones here are ones a lock legitimately has a row for and refuses anyway.
        for (fields, property) in [
            ("", "id"), // the same refusal every other role gets
            (r#"id = "screen-lock", visible = 1"#, "visible"),
            (r#"id = "screen-lock", width = 1"#, "width"),
            (r#"id = "screen-lock", height = 1"#, "height"),
            (r#"visible = false"#, "visible"), // over a missing id: it is the error that teaches
            (r#"id = "screen-lock", visible = state("v", true)"#, "visible"), // on the evaluation pass, like a literal
        ] {
            let err = lock_spec(&props_from_src(&lua, format!(r#"return {{ kind = "lock", {fields} }}"#))).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property),
                "{fields}: {err:?}"
            );
        }
    }

    /// The other half of the lock's denial, one layer up. A name a `lock` has no row for cannot
    /// reach `lock_spec` at all, so the refusal a config author sees is the property gate's.
    #[test]
    fn a_lock_property_that_is_not_even_on_the_kind_is_refused_before_lock_spec_sees_it() {
        let lua = mlua::Lua::new();
        for property in ["output", "anchor"] {
            let table: mlua::Table =
                lua.load(format!(r#"return {{ kind = "lock", id = "screen-lock", {property} = 1 }}"#)).eval().unwrap();
            let err = crate::lua::nodes::deserialize_lua_table(&table).unwrap_err();
            assert!(err.to_string().contains(property), "`{property}` must be refused by name, got {err}");
        }
    }

    #[test]
    fn a_lock_fingerprints_on_its_id_alone_so_only_its_existence_is_a_topology_change() {
        let spec = SurfaceSpec::Lock(LockSpec { id: "screen-lock".to_string() });
        assert_eq!(spec.declared_id(), "screen-lock");
        assert_eq!(spec.fingerprint(), SurfaceFingerprint::Lock("screen-lock".to_string()));
    }

    /// Depth was capped and breadth was not, and the pass budget cannot cover the gap: filling this
    /// array is a Rust loop with no Lua in it, so the deadline hook never runs.
    #[test]
    fn a_children_array_wider_than_the_cap_is_a_config_error_rather_than_an_allocation() {
        let lua = scene_lua();
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

        // Children that fail count toward the cap too, rather than piling up one error each.
        let broken: mlua::Table =
            lua.load("local kids = {} for i = 1, 10001 do kids[i] = rect { colr = 1 } end return kids").eval().unwrap();
        properties.insert("children", Value::Table(broken));
        let err = fields::stack::children.read(&properties).expect_err("past the cap this must be refused");
        assert!(format!("{err:?}").contains("more than"), "{err:?}");
    }

    /// `sequence_values` stopped at the hole and dropped `b` without a word.
    #[test]
    fn a_nil_hole_in_children_is_refused_by_index() {
        let lua = mlua::Lua::new();
        let err = fields::stack::children
            .read(&props_from_src(
                &lua,
                r#"return { kind = "row", children = { { kind = "rect" }, nil, { kind = "rect" } } }"#,
            ))
            .unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail, .. } if property == "children" && detail == "children[2]: expected a node table, got nil"),
            "{err}"
        );

        // The hole does not hide a sibling's mistake.
        let err = fields::stack::children
            .read(&props_from_src(
                &lua,
                r#"return { kind = "row", children = { { kind = "rect", colr = 1 }, nil, { kind = "rect" } } }"#,
            ))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("2 nodes failed:"), "{err}");
    }
}

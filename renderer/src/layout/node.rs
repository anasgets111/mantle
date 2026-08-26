//! Typed property parsing for the layout engine (build-steps.md Phase 12,
//! `docs/oblisk-idl-api-specs.md` § 5.1). `renderer/src/lua/nodes.rs`'s `VirtualNode` deliberately
//! left every property as a raw `mlua::Value` -- this module is the "actual consumer that needs
//! typed, validated properties" that file's own doc comment named as Phase 12's job.
//!
//! Every parser here treats a `Value::UserData` (an unread `Signal`, § 1.2) as an error rather
//! than resolving it: a `Signal` that lands in a geometry-affecting property slot means
//! `shell.lua` returned the handle itself instead of calling `:get()` on it first. Auto-resolving
//! it here would mean re-evaluating on every layout pass with no cache-invalidation story --
//! that's the Watcher's territory (`CONTEXT.md`, Watcher; Phase 13), not this parser's.

use std::collections::HashMap;

use mlua::Value;

use crate::lua::nodes::{VirtualNode, deserialize_lua_table};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeMode {
    Pixels(f32),
    Percent(f32),
    Content,
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EdgeInsets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl EdgeInsets {
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("unsupported node kind `{0}`")]
    UnsupportedNodeKind(String),
    #[error("invalid value for `{property}`: {detail}")]
    InvalidProperty { property: String, detail: String },
    #[error(
        "`{0}` is a Signal handle, not a plain value -- read it via :get() before returning it from shell.lua"
    )]
    UnsupportedSignalProperty(String),
}

fn invalid(property: &str, detail: impl Into<String>) -> LayoutError {
    LayoutError::InvalidProperty {
        property: property.to_string(),
        detail: detail.into(),
    }
}

fn value_as_f32(value: &Value) -> Option<f32> {
    match value {
        Value::Integer(i) => Some(*i as f32),
        Value::Number(n) => Some(*n as f32),
        _ => None,
    }
}

/// A `Signal` userdata landing in a geometry slot is rejected, not auto-resolved -- see the
/// module doc comment.
fn reject_signal(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

/// `"NN%"` (`^\d+(\.\d+)?%$`) as `SizeMode::Percent`. Not a confirmed spec syntax -- § 5.1's base
/// property table only documents integer/`"Fill"` for width/height even though § 3.1 names
/// `Percent(f32)` as a size class without giving it a literal Lua form. See docs/adr/0023.
fn parse_percent(s: &str) -> Option<f32> {
    let digits = s.strip_suffix('%')?;
    let mut parts = digits.splitn(2, '.');
    let int_part = parts.next()?;
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(frac_part) = parts.next()
        && (frac_part.is_empty() || !frac_part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    digits.parse::<f32>().ok().map(|n| n / 100.0)
}

/// An explicit pixel value is range-checked against § 5.1's base property table
/// (`[0, 8192]`) -- ADR-0021 item 5 named this phase's layout engine as the "actual
/// consumer that needs typed, validated properties" that range check waits on.
pub fn parse_size_mode(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<SizeMode, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(SizeMode::Content);
    };
    reject_signal(property, value)?;
    if let Some(n) = value_as_f32(value) {
        if !(0.0..=8192.0).contains(&n) {
            return Err(invalid(
                property,
                format!("must be within [0, 8192], got {n}"),
            ));
        }
        return Ok(SizeMode::Pixels(n));
    }
    if let Value::String(s) = value {
        let s = s.to_string_lossy();
        if s == "Fill" {
            return Ok(SizeMode::Fill);
        }
        if let Some(pct) = parse_percent(&s) {
            return Ok(SizeMode::Percent(pct));
        }
    }
    Err(invalid(
        property,
        format!("expected a number, \"Fill\", or a \"NN%\" string, got {value:?}"),
    ))
}

pub fn parse_edge_insets(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<EdgeInsets, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(EdgeInsets::default());
    };
    reject_signal(property, value)?;
    let Value::Table(table) = value else {
        return Err(invalid(
            property,
            format!("expected a table, got {value:?}"),
        ));
    };
    let edge = |key: &str| -> Result<f32, LayoutError> {
        let v: Value = table
            .get(key)
            .map_err(|e| invalid(property, e.to_string()))?;
        match v {
            Value::Nil => Ok(0.0),
            other => value_as_f32(&other).ok_or_else(|| {
                invalid(property, format!("`{key}` must be a number, got {other:?}"))
            }),
        }
    };
    Ok(EdgeInsets {
        top: edge("top")?,
        right: edge("right")?,
        bottom: edge("bottom")?,
        left: edge("left")?,
    })
}

pub fn parse_align(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<Align, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(Align::Start);
    };
    reject_signal(property, value)?;
    let Value::String(s) = value else {
        return Err(invalid(
            property,
            format!("expected a string, got {value:?}"),
        ));
    };
    match s.to_string_lossy().as_ref() {
        "Start" => Ok(Align::Start),
        "Center" => Ok(Align::Center),
        "End" => Ok(Align::End),
        "Stretch" => Ok(Align::Stretch),
        other => Err(invalid(property, format!("unknown alignment `{other}`"))),
    }
}

pub fn parse_visible(properties: &HashMap<String, Value>) -> Result<bool, LayoutError> {
    let Some(value) = properties.get("visible") else {
        return Ok(true);
    };
    reject_signal("visible", value)?;
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(invalid(
            "visible",
            format!("expected a boolean, got {other:?}"),
        )),
    }
}

pub fn parse_spacing(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("spacing") else {
        return Ok(0.0);
    };
    reject_signal("spacing", value)?;
    value_as_f32(value)
        .ok_or_else(|| invalid("spacing", format!("expected a number, got {value:?}")))
}

pub fn parse_content(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    let value = properties
        .get("content")
        .ok_or_else(|| invalid("content", "text node requires `content`"))?;
    reject_signal("content", value)?;
    match value {
        Value::String(s) => Ok(s.to_string_lossy()),
        other => Err(invalid(
            "content",
            format!("expected a string, got {other:?}"),
        )),
    }
}

pub fn parse_font_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let Some(value) = properties.get("font_size") else {
        return Ok(12.0);
    };
    reject_signal("font_size", value)?;
    value_as_f32(value)
        .ok_or_else(|| invalid("font_size", format!("expected a number, got {value:?}")))
}

pub fn parse_icon_size(properties: &HashMap<String, Value>) -> Result<f32, LayoutError> {
    let value = properties
        .get("size")
        .ok_or_else(|| invalid("size", "icon node requires `size`"))?;
    reject_signal("size", value)?;
    value_as_f32(value).ok_or_else(|| invalid("size", format!("expected a number, got {value:?}")))
}

/// Shared shape behind [`parse_surface_id`]/[`parse_layer`]/[`parse_monitor`]: fetch `property`,
/// reject a `Signal`, require it to be a string. `default` supplies the value when the property
/// is absent; `None` makes it required, erroring instead (Standards review, docs/adr/0024).
fn parse_string_property(properties: &HashMap<String, Value>, property: &str, default: Option<&str>) -> Result<String, LayoutError> {
    let value = match properties.get(property) {
        Some(value) => value,
        None => match default {
            Some(default) => return Ok(default.to_string()),
            None => return Err(invalid(property, format!("surface node requires `{property}`"))),
        },
    };
    reject_signal(property, value)?;
    match value {
        Value::String(s) => Ok(s.to_string_lossy()),
        other => Err(invalid(property, format!("expected a string, got {other:?}"))),
    }
}

pub fn parse_surface_id(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "id", None)
}

/// § 6.1's `layer` (`"Background"`/`"Bottom"`/`"Top"`/`"Overlay"`). Required, same shape as
/// [`parse_surface_id`] -- every existing fixture in this repo already sets it. Stored as a raw
/// string, not a validated enum: Phase 13 only needs it for topology-diff equality (`CONTEXT.md`,
/// Topology change), not for binding a real `zwlr_layer_surface_v1` yet -- see docs/adr/0024.
pub fn parse_layer(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "layer", None)
}

/// § 6.1's `anchor` table (`{ top, bottom, left, right }` edge booleans). Same default-to-zero
/// shape as [`EdgeInsets`], booleans instead of floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Anchor {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

pub fn parse_anchor(properties: &HashMap<String, Value>) -> Result<Anchor, LayoutError> {
    let Some(value) = properties.get("anchor") else {
        return Ok(Anchor::default());
    };
    reject_signal("anchor", value)?;
    let Value::Table(table) = value else {
        return Err(invalid("anchor", format!("expected a table, got {value:?}")));
    };
    let edge = |key: &str| -> Result<bool, LayoutError> {
        let v: Value = table.get(key).map_err(|e| invalid("anchor", e.to_string()))?;
        match v {
            Value::Nil => Ok(false),
            Value::Boolean(b) => Ok(b),
            other => Err(invalid("anchor", format!("`{key}` must be a boolean, got {other:?}"))),
        }
    };
    Ok(Anchor { top: edge("top")?, right: edge("right")?, bottom: edge("bottom")?, left: edge("left")? })
}

/// § 6.1's `monitor` (a specific output EDID, or `"All"`). Absent defaults to `"All"` -- an
/// unqualified surface targets every monitor, matching the IDL's own documented meaning for that
/// value rather than treating the property as required.
pub fn parse_monitor(properties: &HashMap<String, Value>) -> Result<String, LayoutError> {
    parse_string_property(properties, "monitor", Some("All"))
}

/// A surface's topology-relevant fields (`CONTEXT.md`, Topology change: "adds, removes, or
/// changes the layer, anchor, or monitor target of a top-level `surface` node"). Structural
/// equality on `Vec<SurfaceTopology>` (order-sensitive) is the Renderer's own topology diff --
/// see `renderer/src/socket.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceTopology {
    pub id: String,
    pub layer: String,
    pub anchor: Anchor,
    pub monitor: String,
}

pub fn surface_topology(properties: &HashMap<String, Value>) -> Result<SurfaceTopology, LayoutError> {
    Ok(SurfaceTopology {
        id: parse_surface_id(properties)?,
        layer: parse_layer(properties)?,
        anchor: parse_anchor(properties)?,
        monitor: parse_monitor(properties)?,
    })
}

/// A single-node property (`surface.child`), converted from its raw table via
/// `lua::nodes::deserialize_lua_table` -- not re-implemented here.
pub fn parse_single_child(
    properties: &HashMap<String, Value>,
    property: &str,
) -> Result<Option<VirtualNode>, LayoutError> {
    let Some(value) = properties.get(property) else {
        return Ok(None);
    };
    reject_signal(property, value)?;
    let Value::Table(table) = value else {
        return Err(invalid(
            property,
            format!("expected a node table, got {value:?}"),
        ));
    };
    let node = deserialize_lua_table(table).map_err(|e| invalid(property, e.to_string()))?;
    Ok(Some(node))
}

/// An array-of-nodes property (`rect`/`row`/`column`/`button.children`).
pub fn parse_children(
    properties: &HashMap<String, Value>,
) -> Result<Vec<VirtualNode>, LayoutError> {
    let Some(value) = properties.get("children") else {
        return Ok(Vec::new());
    };
    reject_signal("children", value)?;
    let Value::Table(table) = value else {
        return Err(invalid(
            "children",
            format!("expected an array table, got {value:?}"),
        ));
    };
    let mut children = Vec::new();
    for entry in table.sequence_values::<mlua::Table>() {
        let entry = entry.map_err(|e| invalid("children", e.to_string()))?;
        let node = deserialize_lua_table(&entry).map_err(|e| invalid("children", e.to_string()))?;
        children.push(node);
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> mlua::Lua {
        mlua::Lua::new()
    }

    fn props_from_table(table: &mlua::Table) -> HashMap<String, Value> {
        deserialize_lua_table(table).unwrap().properties
    }

    #[test]
    fn width_absent_is_content() {
        let props = HashMap::new();
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Content);
    }

    #[test]
    fn width_integer_is_pixels() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = 32 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_size_mode(&props, "width").unwrap(),
            SizeMode::Pixels(32.0)
        );
    }

    #[test]
    fn width_fill_string_is_fill() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "Fill" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_size_mode(&props, "width").unwrap(), SizeMode::Fill);
    }

    #[test]
    fn width_percent_string_divides_by_100() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "50%" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert_eq!(
            parse_size_mode(&props, "width").unwrap(),
            SizeMode::Percent(0.5)
        );
    }

    #[test]
    fn width_above_the_8192_ceiling_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = 8193 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width").unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn a_negative_width_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = -5 }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width").unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn width_garbage_string_is_invalid_property() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", width = "banana" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        assert!(matches!(
            parse_size_mode(&props, "width").unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn margin_reads_named_edges_defaulting_absent_ones_to_zero() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "rect", margin = { top = 4, left = 2 } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let insets = parse_edge_insets(&props, "margin").unwrap();
        assert_eq!(
            insets,
            EdgeInsets {
                top: 4.0,
                right: 0.0,
                bottom: 0.0,
                left: 2.0
            }
        );
    }

    #[test]
    fn align_h_parses_all_four_variants() {
        for (text, expected) in [
            ("Start", Align::Start),
            ("Center", Align::Center),
            ("End", Align::End),
            ("Stretch", Align::Stretch),
        ] {
            let lua = lua();
            let table: mlua::Table = lua
                .load(format!(r#"return {{ kind = "rect", align_h = "{text}" }}"#))
                .eval()
                .unwrap();
            let props = props_from_table(&table);
            assert_eq!(parse_align(&props, "align_h").unwrap(), expected);
        }
    }

    #[test]
    fn visible_absent_defaults_true() {
        let props = HashMap::new();
        assert!(parse_visible(&props).unwrap());
    }

    #[test]
    fn a_signal_userdata_in_a_geometry_slot_is_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true)).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "rect").unwrap();
        table.set("visible", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(
            matches!(parse_visible(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "visible")
        );
    }

    #[test]
    fn text_content_is_required() {
        let props = HashMap::new();
        assert!(matches!(
            parse_content(&props).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn font_size_absent_defaults_to_twelve() {
        let props = HashMap::new();
        assert_eq!(parse_font_size(&props).unwrap(), 12.0);
    }

    #[test]
    fn icon_size_is_required() {
        let props = HashMap::new();
        assert!(matches!(
            parse_icon_size(&props).unwrap_err(),
            LayoutError::InvalidProperty { .. }
        ));
    }

    #[test]
    fn parse_children_walks_nested_node_tables() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "row", children = { { kind = "text", content = "a" }, { kind = "text", content = "b" } } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let children = parse_children(&props).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].kind, "text");
        assert_eq!(
            children[1]
                .properties
                .get("content")
                .unwrap()
                .as_string()
                .unwrap()
                .to_string_lossy(),
            "b"
        );
    }

    #[test]
    fn parse_single_child_converts_the_child_table() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "surface", child = { kind = "rect" } }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let child = parse_single_child(&props, "child").unwrap();
        assert_eq!(child.unwrap().kind, "rect");
    }

    #[test]
    fn parse_single_child_absent_is_none() {
        let props = HashMap::new();
        assert!(parse_single_child(&props, "child").unwrap().is_none());
    }

    #[test]
    fn layer_is_required() {
        let props = HashMap::new();
        assert!(matches!(parse_layer(&props).unwrap_err(), LayoutError::InvalidProperty { .. }));
    }

    #[test]
    fn layer_reads_the_string() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", layer = "Top" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_layer(&props).unwrap(), "Top");
    }

    #[test]
    fn anchor_absent_defaults_all_false() {
        let props = HashMap::new();
        assert_eq!(parse_anchor(&props).unwrap(), Anchor::default());
    }

    #[test]
    fn anchor_reads_named_edges_defaulting_absent_ones_to_false() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", anchor = { top = true, left = true } }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_anchor(&props).unwrap(), Anchor { top: true, right: false, bottom: false, left: true });
    }

    #[test]
    fn monitor_absent_defaults_to_all() {
        let props = HashMap::new();
        assert_eq!(parse_monitor(&props).unwrap(), "All");
    }

    #[test]
    fn monitor_reads_the_string() {
        let lua = lua();
        let table: mlua::Table = lua.load(r#"return { kind = "surface", monitor = "eDP-1" }"#).eval().unwrap();
        let props = props_from_table(&table);
        assert_eq!(parse_monitor(&props).unwrap(), "eDP-1");
    }

    #[test]
    fn surface_topology_combines_id_layer_anchor_and_monitor() {
        let lua = lua();
        let table: mlua::Table = lua
            .load(r#"return { kind = "surface", id = "bar", layer = "Top", anchor = { top = true }, monitor = "eDP-1" }"#)
            .eval()
            .unwrap();
        let props = props_from_table(&table);
        let topology = surface_topology(&props).unwrap();
        assert_eq!(
            topology,
            SurfaceTopology {
                id: "bar".to_string(),
                layer: "Top".to_string(),
                anchor: Anchor { top: true, right: false, bottom: false, left: false },
                monitor: "eDP-1".to_string(),
            }
        );
    }

    #[test]
    fn a_signal_userdata_in_layer_is_rejected() {
        let lua = lua();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Boolean(true)).0;
        let table = lua.create_table().unwrap();
        table.set("kind", "surface").unwrap();
        table.set("layer", signal).unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        assert!(matches!(parse_layer(&node.properties).unwrap_err(), LayoutError::UnsupportedSignalProperty(p) if p == "layer"));
    }
}

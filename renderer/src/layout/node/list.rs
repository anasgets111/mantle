//! `list` children: the `source`/`limit` properties, per-item memos and the build that reuses them.

use std::collections::HashSet;

use mlua::{Lua, Value, WeakLua};

use crate::lua::location::Site;
use crate::lua::nodes::VirtualNode;
use crate::lua::signal::{self, CellId, ComputedFrame};

use super::spec::deserialize_child;
use super::*;
use fields::list;

/// A `list`'s `source`: absent, or a signal still reading nil before its first push, is an empty
/// list. Raw past `resolve_declared`: [`list_children`] reads it, and only when the list builds.
pub(crate) struct Items;

spelled!(Items => prop::Bound::<Vec<Value>>::lua());

impl Prop for Items {
    type Out = Option<mlua::Table>;
    const RAW: bool = true;
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

spelled!(Limit => i64::lua());

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

/// A `list`'s items and the build that made them, `source` read here rather than by
/// `resolve_declared` so a list that [`ListMemo::still_holds`] reads nothing at all.
pub fn list_children(properties: &PropMap, lua: &Lua) -> Result<(Vec<VirtualNode>, ListMemo), LayoutError> {
    let mut build = ListMemo {
        inputs: list_inputs(properties),
        limit: list::limit.read(properties).ok().flatten(),
        stamp: signal::write_clock(lua),
        lua: lua.weak(),
        cells: Vec::new(),
        items: Vec::new(),
    };
    let Some(source) = signal_at(properties, "source") else {
        return Ok((parse_list_children(properties, lua, &mut build)?, build));
    };
    let frame = ComputedFrame::enter(lua);
    let value = resolve_signal(&source, "list", "source", lua);
    build.cells = frame.finish();
    let mut resolved = properties.clone();
    match value? {
        Some(value) => resolved.insert("source", value),
        None => resolved.remove("source"),
    };
    Ok((parse_list_children(&resolved, lua, &mut build)?, build))
}

/// What a `list` last built its items from (ADR-0269, ADR-0273): its `source`, `itemfn` and `key`
/// by identity, its `limit`, the cells reading `source` and calling `key` read, and per item, the
/// cells its `itemfn` call read and, apart, the cells its subtree's resolve read. While the list's
/// own inputs hold, the pass lays the retained items out again, calls `itemfn` only for an item
/// whose `itemfn` reads were written, and resolves again one whose subtree reads alone were.
///
/// Exact for signals, and blind to anything else a build reads: `os.time()`, an upvalue, a table
/// mutated in place. That is the contract `docs/nodes/list.md` states.
#[derive(Clone)]
pub struct ListMemo {
    /// Kept to keep them alive: compared by address, so a collected function cannot hand its
    /// address to a new one. By address and not `==`, which asserts both come from one `Lua`.
    inputs: [Value; 3],
    /// Checked first: a value of a dropped VM panics on any read. A scene outlives its VM only in
    /// tests.
    lua: WeakLua,
    limit: Option<usize>,
    stamp: u64,
    cells: Vec<CellId>,
    /// One per item, in the order the list holds them.
    pub items: Vec<ItemMemo>,
}

/// One `list` item's build: the element it came from, the key `key` gave it, where `itemfn` built
/// its node, the write clock taken before it read anything, and what it read.
#[derive(Clone)]
pub struct ItemMemo {
    element: Value,
    key: Option<mlua::LuaString>,
    site: Option<Site>,
    stamp: u64,
    /// What the `itemfn` call read: a write to one builds the item again.
    built: Vec<CellId>,
    /// What resolving its subtree read: a write to one resolves the kept declaration again.
    resolved: Vec<CellId>,
    /// Its subtree dropped a value, so it resolves again each pass to report it.
    dropped: bool,
}

/// What a pass does with one item of a list whose own build still holds.
pub enum ItemPass {
    /// Lay the retained item out again, running no Lua.
    Keep,
    /// Resolve the retained declaration again; each node's memo skips the getters nothing wrote.
    Resolve(Option<Site>),
    /// Call `itemfn` again.
    Build,
}

fn list_inputs(properties: &PropMap) -> [Value; 3] {
    ["source", "itemfn", "key"].map(|key| properties.get(key).cloned().unwrap_or(Value::Nil))
}

impl ListMemo {
    /// Whether a build now would read the same elements and keys the last one did. When it would,
    /// those reads are noted as this pass's, so the instance stays a reader of what the skipped
    /// build read; each kept item notes its own through [`ItemMemo::pass`].
    pub fn still_holds(&self, properties: &PropMap, lua: &Lua) -> Result<bool, LayoutError> {
        let same_inputs = self.lua == lua.weak()
            && self.inputs.iter().zip(&list_inputs(properties)).all(|(a, b)| same_lua_value(a, b))
            && list::limit.read(properties).is_ok_and(|limit| limit == self.limit);
        if !same_inputs {
            return Ok(false);
        }
        if signal::written_since(self.stamp, &self.cells)
            && let Some(source) = signal_at(properties, "source")
        {
            // Settle a mapped source before deciding whether its output changed.
            resolve_signal(&source, "list", "source", lua)?;
        }
        let holds = !signal::written_since(self.stamp, &self.cells);
        if holds {
            signal::note_reads(lua, &self.cells);
        }
        Ok(holds)
    }

    /// Whether the last build read `cell`, for the list or any of its items.
    pub fn read(&self, cell: CellId) -> bool {
        self.cells.contains(&cell)
            || self.items.iter().any(|item| item.built.contains(&cell) || item.resolved.contains(&cell))
    }

    /// Adds what item `index`'s subtree read, and whether it dropped a value.
    pub fn item_read(&mut self, index: usize, cells: &[CellId], dropped: bool) {
        extend_unique(&mut self.items[index].resolved, cells);
        self.items[index].dropped = dropped;
    }
}

impl ItemMemo {
    /// What this item needs this pass. The reads a kept part skips are noted as this pass's. A
    /// resolve starts a fresh subtree read set: every `itemfn` read held up to now, so they hold
    /// since the new stamp too.
    pub fn pass(&mut self, lua: &Lua) -> ItemPass {
        if signal::written_since(self.stamp, &self.built) {
            return ItemPass::Build;
        }
        signal::note_reads(lua, &self.built);
        if !self.dropped && !signal::written_since(self.stamp, &self.resolved) {
            signal::note_reads(lua, &self.resolved);
            return ItemPass::Keep;
        }
        self.stamp = signal::write_clock(lua);
        self.resolved.clear();
        ItemPass::Resolve(self.site)
    }

    /// `element` built through `itemfn` with no key yet, and its fresh memo.
    fn build(lua: &Lua, itemfn: &mlua::Function, element: Value) -> Result<(VirtualNode, ItemMemo), LayoutError> {
        let stamp = signal::write_clock(lua);
        let frame = ComputedFrame::enter(lua);
        let node = build_item(itemfn, &element);
        let built = frame.finish();
        let node = node?;
        let memo = ItemMemo { element, key: None, site: node.site, stamp, built, resolved: Vec::new(), dropped: false };
        Ok((node, memo))
    }

    /// This item built again from the element and key it had, and its fresh memo.
    pub fn rebuild(&self, properties: &PropMap, lua: &Lua) -> Result<(VirtualNode, ItemMemo), LayoutError> {
        let itemfn = list::itemfn.read(properties)?.expect("`itemfn` is required");
        let (mut node, mut memo) = Self::build(lua, &itemfn, self.element.clone())?;
        if let Some(key) = &self.key {
            std::rc::Rc::make_mut(&mut node.properties).insert("id", Value::String(key.clone()));
            memo.key = Some(key.clone());
        }
        Ok((node, memo))
    }
}

/// By hand: `WeakLua` has no `Debug`.
impl std::fmt::Debug for ListMemo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListMemo")
            .field("stamp", &self.stamp)
            .field("cells", &self.cells)
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

fn extend_unique(into: &mut Vec<CellId>, cells: &[CellId]) {
    for cell in cells {
        if !into.contains(cell) {
            into.push(*cell);
        }
    }
}

/// `itemfn(element)` as a node.
fn build_item(itemfn: &mlua::Function, element: &Value) -> Result<VirtualNode, LayoutError> {
    let built = itemfn.call::<Value>(element).map_err(|e| invalid("itemfn", crate::lua::describe(&e)))?;
    let Value::Table(built_table) = built else {
        return Err(invalid("itemfn", format!("expected a node table, got {}", preview_for_error(&built))));
    };
    deserialize_child(&built_table, "itemfn", None)
}

/// A `list`'s children (ADR-0045 decision 3) are generated once per resolved
/// `source` item; it arrives already resolved, so a `Signal` there was read exactly once before
/// `itemfn` runs. Without `key`, reconciliation is positional. With it, `key(element)`
/// is called on the source value, not the built node, and overwrites that node's `id`; duplicate
/// keys fail here before `pair_children_by_id_then_position` sees them.
///
/// Each `itemfn` call reads in its own frame, into `build`'s item; each `key` call into `build`'s
/// own cells, since a key is the item's identity rather than its content.
///
/// ponytail: a list whose [`ListMemo`] still holds still lays every item out. A viewport
/// (ADR-0191) is the upgrade; `layout::scene::tests::list_pass_cost` measures both paths.
fn parse_list_children(properties: &PropMap, lua: &Lua, build: &mut ListMemo) -> Result<Vec<VirtualNode>, LayoutError> {
    let source = list::source.read(properties)?;
    let itemfn = list::itemfn.read(properties)?.expect("`itemfn` is required");
    let key_fn = list::key.read(properties)?;
    let limit = list::limit.read(properties)?;

    let Some(source) = source else {
        return Ok(Vec::new());
    };
    let mut children = Vec::with_capacity(source.raw_len().min(limit.unwrap_or(MAX_ARRAY_ELEMENTS)));
    let mut seen_keys: HashSet<String> = HashSet::new();
    // A broken item does not stop the rest; every item repeating one `itemfn` mistake is one
    // entry after `LayoutError::many`.
    let mut failed = Vec::new();
    for (index, element) in source.sequence_values::<Value>().enumerate() {
        if limit.is_some_and(|lim| index == lim) {
            break;
        }
        if index == MAX_ARRAY_ELEMENTS {
            return Err(invalid("source", format!("more than {MAX_ARRAY_ELEMENTS} items in one list")));
        }
        let element = element.map_err(|e| invalid("source", e.to_string()))?;

        let item = (|| {
            let (mut node, mut memo) = ItemMemo::build(lua, &itemfn, element.clone())?;

            if let Some(key_fn) = &key_fn {
                let frame = ComputedFrame::enter(lua);
                let key_value = key_fn.call::<Value>(element);
                extend_unique(&mut build.cells, &frame.finish());
                let key_value = key_value.map_err(|e| invalid("key", crate::lua::describe(&e)))?;
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
                std::rc::Rc::make_mut(&mut node.properties).insert("id", Value::String(key_str.clone()));
                memo.key = Some(key_str);
            }
            Ok((node, memo))
        })();
        match item {
            Ok((node, memo)) => {
                children.push(node);
                build.items.push(memo);
            }
            Err(err) => failed.push(err),
        }
    }
    if !failed.is_empty() {
        return Err(LayoutError::many(failed));
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_children_bounds_to_limit_and_refuses_invalid_values() {
        let lua = scene_lua();
        let eval = |s: &str| -> Result<usize, LayoutError> {
            list_children(&props_from_src(&lua, s), &lua).map(|(children, _)| children.len())
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
}

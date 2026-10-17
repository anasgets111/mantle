//! `list` children: the `items`/`limit` properties, per-item memos and the build that reuses them.

use std::ops::Range;
use std::rc::Rc;

use mlua::{Lua, Value, WeakLua};
use rustc_hash::FxHashMap;

use crate::lua::location::Site;
use crate::lua::nodes::VirtualNode;
use crate::lua::signal::{self, CellId, ComputedFrame};

use super::spec::deserialize_child;
use super::*;
use fields::list;

/// A `list`'s `items`: absent, or a signal still reading nil before its first push, is an empty
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

/// A `list`'s `limit`, capped at what one list builds. A keyed scrolling list builds only its
/// window once laid out (ADR-0374); `limit` bounds every other list, and every first build.
///
/// ponytail: a keyed scrolling list's first build, with no viewport yet, builds every item. Upgrade:
/// build a bounded prefix and let the same-turn short-window pass correct it, once a startup or
/// reload apply also runs that pass.
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

/// A `list`'s items and the build that made them, `items` read here rather than by
/// `resolve_declared` so a list that [`ListMemo::still_holds`] reads nothing at all.
/// Windowed to `view` when given, carrying `old`'s solved lengths by key.
pub fn list_children(
    properties: &PropMap,
    lua: &Lua,
    view: Option<&View>,
    old: Option<&ListMemo>,
) -> Result<(Vec<VirtualNode>, ListMemo), LayoutError> {
    let mut build = ListMemo {
        inputs: list_inputs(properties),
        limit: list::limit.read(properties).ok().flatten(),
        stamp: signal::write_clock(lua),
        lua: lua.weak(),
        cells: Vec::new(),
        items: Vec::new(),
        catalog: None,
        sizes: Vec::new(),
        typical: old.map_or(f32::NAN, |old| old.typical),
        window: None,
        added: Vec::new(),
        viewport: old.map_or(0.0, |old| old.viewport),
        cross: old.map_or(f32::NAN, |old| old.cross),
        spacing: old.map_or(0.0, |old| old.spacing),
    };
    let Some(source) = signal_at(properties, "items") else {
        return Ok((parse_list_children(properties, lua, &mut build, view, old)?, build));
    };
    let frame = ComputedFrame::enter(lua);
    let value = resolve_signal(&source, "list", "items", lua);
    build.cells = frame.finish();
    let mut resolved = properties.clone();
    match value? {
        Some(value) => resolved.insert("items", value),
        None => resolved.remove("items"),
    };
    Ok((parse_list_children(&resolved, lua, &mut build, view, old)?, build))
}

/// What a `list` last built its items from (ADR-0269, ADR-0273): its `items`, `each` and `key`
/// by identity, its `limit`, the cells reading `items` and calling `key` read, and per item, the
/// cells its `each` call read and, apart, the cells its subtree's resolve read. While the list's
/// own inputs hold, the pass lays the retained items out again, calls `each` only for an item
/// whose `each` reads were written, and resolves again one whose subtree reads alone were.
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
    /// One per item built, in the order the list holds them.
    pub items: Vec<ItemMemo>,
    /// A keyed scrolling list's every element and key; `None` for any other list.
    catalog: Option<Rc<Catalog>>,
    /// Per catalog item, its last solved length with margins: NaN if never built, `HIDDEN` if not shown.
    sizes: Vec<f32>,
    /// The mean known length, carried across builds: an unbuilt item's estimate.
    typical: f32,
    /// The catalog items built, when not all of them.
    pub window: Option<Range<usize>>,
    /// Per catalog item, whether this build added its key: built for the first time, it enters.
    pub added: Vec<bool>,
    /// The viewport length, cross length and spacing last solved while shown; a cross change voids `sizes`.
    pub viewport: f32,
    cross: f32,
    spacing: f32,
}

/// The length of an item that takes no room: hidden, so no gap either.
const HIDDEN: f32 = f32::NEG_INFINITY;

/// Every element and key of a keyed scrolling list; shared, as rollback clones the memo every pass.
struct Catalog {
    all: Vec<(Value, mlua::LuaString)>,
    index: FxHashMap<String, usize>,
}

/// What a keyed `list` scrolling along its own axis windows against (ADR-0374); `reveal` is 1-based.
pub struct View {
    pub length: f32,
    pub offset: f32,
    pub reveal: Option<usize>,
}

/// One `list` item's build: the element it came from, the key `key` gave it, where `each` built
/// its node, the write clock taken before it read anything, and what it read.
#[derive(Clone)]
pub struct ItemMemo {
    element: Value,
    key: Option<mlua::LuaString>,
    site: Option<Site>,
    stamp: u64,
    /// What the `each` call read: a write to one builds the item again.
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
    /// Call `each` again.
    Build,
}

fn list_inputs(properties: &PropMap) -> [Value; 3] {
    ["items", "each", "key"].map(|key| properties.get(key).cloned().unwrap_or(Value::Nil))
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
            && let Some(source) = signal_at(properties, "items")
        {
            // Settle a mapped source before deciding whether its output changed.
            resolve_signal(&source, "list", "items", lua)?;
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

    /// How many items the list holds, built or not.
    pub fn count(&self) -> usize {
        self.catalog.as_ref().map_or(self.items.len(), |catalog| catalog.all.len())
    }

    /// The items built: the window, or all of them.
    pub fn range(&self) -> Range<usize> {
        self.window.clone().unwrap_or(0..self.count())
    }

    /// The catalog index of the item keyed `id`, built or not.
    pub fn at(&self, id: &str) -> Option<usize> {
        self.catalog.as_ref().and_then(|catalog| catalog.index.get(id).copied())
    }

    /// The room `size` takes, with the gap after it when `gap`.
    fn slot(&self, size: f32, gap: bool) -> f32 {
        let spacing = if gap { self.spacing } else { 0.0 };
        if size == HIDDEN {
            0.0
        } else if size.is_nan() {
            self.typical + spacing
        } else {
            size + spacing
        }
    }

    fn room_of(&self, items: Range<usize>) -> f32 {
        self.sizes[items].iter().map(|&size| self.slot(size, true)).sum()
    }

    /// The catalog item `reveal(index)` names, counting shown items, and where it lies.
    fn revealed(&self, index: usize) -> Option<(usize, (f32, f32))> {
        let item = self.sizes.iter().enumerate().filter(|(_, size)| **size != HIDDEN).nth(index - 1)?.0;
        let start = self.room_of(0..item);
        Some((item, (start, start + self.slot(self.sizes[item], false))))
    }

    /// The items to build to show `view` with `overscan` px past each edge; `None` for all of them,
    /// and before any item was measured.
    ///
    /// ponytail: an unbuilt item is estimated at the mean solved length, so rows of very different
    /// lengths move the extent as they are first built. Upgrade: an author-given item length.
    fn plan(&self, view: &View, overscan: f32) -> Option<Range<usize>> {
        if self.catalog.is_none() || view.length <= 0.0 || !self.typical.is_finite() {
            return None;
        }
        let total = self.room_of(0..self.sizes.len()) - self.spacing;
        let reveal = view.reveal.and_then(|index| self.revealed(index));
        let offset = reveal.and_then(|(_, span)| least_move(span, view.offset, view.length)).unwrap_or(view.offset);
        let offset = offset.clamp(0.0, (total - view.length).max(0.0));
        let (from, to) = (offset - overscan, offset + view.length + overscan);
        let (mut at, mut range) = (0.0, 0..0);
        for (index, &size) in self.sizes.iter().enumerate() {
            if at >= to {
                break;
            }
            let length = self.slot(size, false);
            at += length;
            if at <= from {
                range.start = index + 1;
            }
            range.end = index + 1;
            at += self.slot(size, true) - length;
        }
        (range != (0..self.sizes.len())).then_some(range)
    }

    /// The window to build for `view`: the one built while it covers `view` with half a viewport to
    /// spare past each edge, else one a viewport past each, so scrolling builds every half viewport.
    pub fn replan(&self, view: &View) -> Option<Range<usize>> {
        match (&self.window, self.plan(view, view.length / 2.0)) {
            (Some(have), Some(need)) if have.start <= need.start && need.end <= have.end => self.window.clone(),
            (None, None) => None,
            _ => self.plan(view, view.length),
        }
    }

    /// The room before and after the window for the items it does not build, gaps included.
    pub fn room(&self) -> (f32, f32) {
        let Some(range) = self.window.clone() else { return (0.0, 0.0) };
        (self.room_of(0..range.start), self.room_of(range.end..self.sizes.len()))
    }

    /// Notes what the built items solved to, `None` for one not shown, and the viewport length,
    /// cross length and spacing the list solved to while shown. A new cross length voids the others' sizes.
    pub fn measured(
        &mut self,
        lengths: impl Iterator<Item = Option<f32>>,
        (viewport, cross, spacing): (f32, f32, f32),
    ) {
        (self.viewport, self.spacing) = (viewport, spacing);
        if self.catalog.is_none() {
            return;
        }
        if cross != self.cross {
            self.sizes.iter_mut().filter(|size| size.is_finite()).for_each(|size| *size = f32::NAN);
            self.cross = cross;
        }
        for (item, length) in self.range().zip(lengths) {
            self.sizes[item] = length.unwrap_or(HIDDEN);
        }
        let known = self.sizes.iter().filter(|size| size.is_finite());
        let (sum, count) = known.fold((0.0, 0), |(sum, count), size| (sum + size, count + 1));
        if count > 0 {
            self.typical = sum / count as f32;
        }
    }

    /// Catalog item `item` built through `each` under its key, and its fresh memo.
    pub fn build_at(
        &self,
        item: usize,
        each: &mlua::Function,
        lua: &Lua,
    ) -> Result<(VirtualNode, ItemMemo), LayoutError> {
        let (element, key) = self.catalog.as_ref().expect("only a windowed list builds by position").all[item].clone();
        ItemMemo::keyed(lua, each, element, Some(key))
    }

    /// Where `reveal(index)` points in a windowed list: its place among the built children, or,
    /// for an item not built, its estimated span. `None` past the last shown item.
    pub fn reveal(&self, index: usize) -> Option<Result<usize, (f32, f32)>> {
        let range = self.window.clone()?;
        let (item, span) = self.revealed(index)?;
        let before = self.sizes[..range.start].iter().filter(|size| **size != HIDDEN).count();
        Some(if range.contains(&item) { Ok(index - before) } else { Err(span) })
    }
}

/// The least move from `offset` that shows all of `(start, end)` in `length`; `None` when shown.
pub(crate) fn least_move((start, end): (f32, f32), offset: f32, length: f32) -> Option<f32> {
    if start < offset {
        Some(start)
    } else if end > offset + length {
        Some(end - length)
    } else {
        None
    }
}

impl ItemMemo {
    /// What this item needs this pass. The reads a kept part skips are noted as this pass's. A
    /// resolve starts a fresh subtree read set: every `each` read held up to now, so they hold
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

    /// `element` built through `each` with no key yet, and its fresh memo.
    fn build(lua: &Lua, each: &mlua::Function, element: Value) -> Result<(VirtualNode, ItemMemo), LayoutError> {
        let stamp = signal::write_clock(lua);
        let frame = ComputedFrame::enter(lua);
        let node = build_item(each, &element);
        let built = frame.finish();
        let node = node?;
        let memo = ItemMemo { element, key: None, site: node.site, stamp, built, resolved: Vec::new(), dropped: false };
        Ok((node, memo))
    }

    /// `element` built through `each`, its `key` overwriting the node's `id`, and its fresh memo.
    fn keyed(
        lua: &Lua,
        each: &mlua::Function,
        element: Value,
        key: Option<mlua::LuaString>,
    ) -> Result<(VirtualNode, ItemMemo), LayoutError> {
        let (mut node, mut memo) = Self::build(lua, each, element)?;
        if let Some(key) = key {
            // List identity wins over any `id` the item function supplied.
            Rc::make_mut(&mut node.properties).insert("id", Value::String(key.clone()));
            memo.key = Some(key);
        }
        Ok((node, memo))
    }

    /// This item built again from the element and key it had, and its fresh memo.
    pub fn rebuild(&self, properties: &PropMap, lua: &Lua) -> Result<(VirtualNode, ItemMemo), LayoutError> {
        let each = list::each.read(properties)?.expect("`each` is required");
        Self::keyed(lua, &each, self.element.clone(), self.key.clone())
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

/// `each(element)` as a node.
fn build_item(each: &mlua::Function, element: &Value) -> Result<VirtualNode, LayoutError> {
    let built = each.call::<Value>(element).map_err(|e| invalid("each", crate::lua::describe(&e)))?;
    let Value::Table(built_table) = built else {
        return Err(invalid("each", format!("expected a node table, got {}", preview_for_error(&built))));
    };
    deserialize_child(&built_table, "each", None)
}

/// A `list`'s children (ADR-0045 decision 3) are generated once per resolved
/// `items` item; it arrives already resolved, so a `Signal` there was read exactly once before
/// `each` runs. Without `key`, reconciliation is positional. With it, `key(element)`
/// is called on the source value, not the built node, and overwrites that node's `id`; duplicate
/// keys fail here before `pair_children_by_id_then_position` sees them.
///
/// Each `each` call reads in its own frame, into `build`'s item; each `key` call into `build`'s
/// own cells, since a key is the item's identity rather than its content.
///
/// A keyed list with `scroll` calls `each` only for the window `view` needs (ADR-0374).
fn parse_list_children(
    properties: &PropMap,
    lua: &Lua,
    build: &mut ListMemo,
    view: Option<&View>,
    old: Option<&ListMemo>,
) -> Result<Vec<VirtualNode>, LayoutError> {
    let source = list::items.read(properties)?;
    let each = list::each.read(properties)?.expect("`each` is required");
    let key_fn = list::key.read(properties)?;
    let limit = list::limit.read(properties)?;

    let Some(source) = source else {
        return Ok(Vec::new());
    };
    let mut all = Vec::with_capacity(source.raw_len().min(limit.unwrap_or(MAX_ARRAY_ELEMENTS)));
    let mut index: FxHashMap<String, usize> = FxHashMap::default();
    // A broken item does not stop the rest; every item repeating one `each` mistake is one
    // entry after `LayoutError::many`.
    let mut failed = Vec::new();
    for (position, element) in source.sequence_values::<Value>().enumerate() {
        if limit.is_some_and(|lim| position == lim) {
            break;
        }
        if position == MAX_ARRAY_ELEMENTS {
            return Err(invalid("items", format!("more than {MAX_ARRAY_ELEMENTS} items in one list")));
        }
        let element = element.map_err(|e| invalid("items", e.to_string()))?;
        let key = match &key_fn {
            None => None,
            Some(key_fn) => match item_key(key_fn, &element, position, &mut index, &mut build.cells, lua) {
                Ok(key) => Some(key),
                Err(err) => {
                    failed.push(err);
                    continue;
                }
            },
        };
        all.push((element, key));
    }
    let built = if key_fn.is_some() && properties.contains_key("scroll") && failed.is_empty() {
        build.sizes = vec![f32::NAN; all.len()];
        // A new `each` or VM builds other rows: their old lengths would mislead (`typical` stays).
        let carried = old.filter(|old| old.lua == lua.weak() && same_lua_value(&old.inputs[1], &build.inputs[1]));
        let was = old.and_then(|old| old.catalog.as_ref());
        build.added = vec![true; all.len()];
        for (key, &at) in &index {
            let Some(&before) = was.and_then(|was| was.index.get(key)) else { continue };
            build.added[at] = false;
            if let Some(old) = carried {
                build.sizes[at] = old.sizes[before];
            }
        }
        let all = all.into_iter().map(|(element, key)| (element, key.expect("a keyed list keys every item")));
        let catalog = Rc::new(Catalog { all: all.collect(), index });
        build.catalog = Some(Rc::clone(&catalog));
        build.window = view.and_then(|view| build.plan(view, view.length));
        catalog.all[build.range()].iter().map(|(element, key)| (element.clone(), Some(key.clone()))).collect()
    } else {
        all
    };
    let mut children = Vec::with_capacity(built.len());
    for (element, key) in built {
        match ItemMemo::keyed(lua, &each, element, key) {
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

/// `key(element)` for the item at `position`, refused unless a UTF-8 string no other item returned.
fn item_key(
    key_fn: &mlua::Function,
    element: &Value,
    position: usize,
    index: &mut FxHashMap<String, usize>,
    cells: &mut Vec<CellId>,
    lua: &Lua,
) -> Result<mlua::LuaString, LayoutError> {
    let frame = ComputedFrame::enter(lua);
    let key_value = key_fn.call::<Value>(element);
    extend_unique(cells, &frame.finish());
    let key_value = key_value.map_err(|e| invalid("key", crate::lua::describe(&e)))?;
    let Value::String(key_str) = key_value else {
        return Err(invalid(
            "key",
            format!("expected key(item) to return a string, got {}", preview_for_error(&key_value)),
        ));
    };
    let key_text = key_str.to_str().map(|s| (*s).to_owned()).map_err(|_| {
        invalid("key", "must be valid UTF-8 -- a key is compared for equality, so it cannot be converted lossily")
    })?;
    if index.insert(key_text, position).is_some() {
        let key = key_str.to_string_lossy();
        return Err(invalid("key", format!("duplicate key `{key}` among list items")));
    }
    Ok(key_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_children_bounds_to_limit_and_refuses_invalid_values() {
        let lua = scene_lua();
        let eval = |s: &str| -> Result<usize, LayoutError> {
            list_children(&props_from_src(&lua, s), &lua, None, None).map(|(children, _)| children.len())
        };
        assert_eq!(
            eval("return list { items = { 1, 2, 3, 4 }, limit = 2, each = function(i) return rect { width = i, height = i } end }").unwrap(),
            2
        );
        assert_eq!(
            eval("return list { items = { 1, 2 }, limit = 0, each = function() error('unreached') end }").unwrap(),
            0
        );
        assert!(matches!(
            eval("return list { items = { 1 }, limit = -1, each = function() return rect {} end }").unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "limit"
        ));
        assert_eq!(eval("return list { each = function() return rect {} end }").unwrap(), 0, "no items is empty");
        assert!(matches!(
            eval("return list { items = {} }").unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "each"
        ));
    }
}

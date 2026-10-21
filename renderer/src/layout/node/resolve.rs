//! Signal resolution: each ordinary `Signal` on a node is read once per pass and its value stored in
//! its place (ADR-0044 decision 1).

use super::*;

/// One node's raw property map with every `Signal` replaced by its current value (ADR-0044 decision
/// 1). Called at most once per node per pass, and not at all for a node whose last resolve still holds
/// (ADR-0270); everything downstream reads the result, not the raw map. Once is load-bearing:
/// `Signal::get_value` runs a `computed` closure, which may answer differently on every call
/// (`os.clock()`, `math.random`, an accumulator upvalue), so one read per property makes the tree a
/// snapshot of one pass and stops ADR-0021's per-`get_value` evaluation budget being paid four times
/// over for one property. The snapshot covers *signals* only: a plain table with an `__index`
/// metamethod is copied as-is, and each `table.get` a parser makes re-runs it (see `NumberOrEdges`'s read).
///
/// Per entry: a key [`is_structural_property`] names for this `kind` is copied through raw, signal and
/// all. A table value is walked by [`resolve_nested`], which copies the tables that hold a signal. A
/// signal is read through `Signal::get_value` and the *result* stored in its place; a result that is
/// itself a `Signal` is an error naming the property, avoiding an unbounded loop on a cyclic
/// construction. A `Value::Nil` result **omits the key** (ADR-0044 decision 1's amendment), which
/// matters at boot: `run_startup_evaluation` runs before the poll loop drains any frame, so every
/// `shared::Capability::ALL` signal reads `nil` at the first `Scene::apply`, and a bare capability
/// binding must not fail layout there. Every property resolves, including ones no
/// parser reads: the paint stage reads colours and radii straight off the resolved map. A getter that
/// raises fails the whole apply, since deferring it would mean the second read this function prevents.
///
/// When no signal is present, at the top level or in a table, resolution completes in place with no
/// allocations or sorting. `tables_plain` vouches that the walked tables hold no signal.
pub(crate) fn resolve_declared(
    properties: &PropMap,
    kind: &str,
    tables_plain: bool,
    lua: &Lua,
) -> Result<PropMap, LayoutError> {
    let mut properties = properties.clone();
    if holds_signals(&properties, tables_plain) {
        // Sorted, and the sort is the point: unsorted, two failing properties on one node name
        // whichever bucket the hasher put first. `renderer/src/socket/client/resolve.rs` puts this message in the
        // `rescue` global's `error_log` for a human to read (ADR-0024), so which one a broken config
        // names must come from the config. `two_failing_properties_always_report_the_same_one` guards it.
        let mut keys: Vec<&'static str> = properties.keys().copied().collect();
        keys.sort_unstable();
        for property in keys {
            if is_structural_property(kind, property) {
                // The writer of a layout measurement is not its reader: a change re-resolves only
                // the nodes that read it. A `scroll` slot's holder reads nothing of it either; the
                // scene notes it outside the node's own reads (`scene::resolve`).
                if !matches!(property, "geometry" | "scroll" | "elided" | "settled" | "status")
                    && let Some(cell) = signal_at(&properties, property).and_then(|s| s.cell_id())
                {
                    signal::note_read(lua, cell);
                }
                continue;
            }
            // A seed is read once at creation, so a write to its signal must not re-resolve the node.
            let read = || resolved_value(&properties, kind, property, tables_plain, lua);
            match if property == "initial_text" { signal::untracked(lua, read) } else { read() }? {
                Some(Value::Nil) => properties.remove(property),
                Some(value) => properties.insert(property, value),
                None => continue,
            };
        }
    }
    // `effect` is every kind's row but `backdrop` is a box's, and a key's kind is no row's.
    if let Some(Value::Table(effect)) = properties.get("effect")
        && crate::lua::nodes::properties::kind_bit(kind)
            .is_some_and(|bit| bit & crate::lua::nodes::properties::BOX == 0)
    {
        let input = effect.get::<mlua::Table>("shader").and_then(|shader| shader.get::<mlua::LuaString>("input"));
        for (key, reads) in [
            ("effect.backdrop", effect.contains_key("backdrop").unwrap_or(false)),
            (
                "effect.shader.input",
                input.is_ok_and(|input| [b"backdrop".as_slice(), b"desktop"].contains(&&*input.as_bytes())),
            ),
        ] {
            if reads {
                return Err(invalid(key, format!("`{kind}` has no backdrop; it is for box kinds only")));
            }
        }
    }
    // Callbacks and the two input flags have no parser: the input handlers read them where they
    // fire, where a wrong type could only be ignored. Sorted like the loop above.
    let mut typed: Vec<&'static str> = properties
        .keys()
        .copied()
        .filter(|property| property.starts_with("on_") || matches!(*property, "submits_secure" | "autofocus"))
        .collect();
    typed.sort_unstable();
    for property in typed {
        let value = &properties[property];
        let (ok, expected) = if property.starts_with("on_") {
            (matches!(value, Value::Function(_)), "a function")
        } else {
            (matches!(value, Value::Boolean(_)), "a boolean")
        };
        if !ok {
            return Err(invalid(property, format!("expected {expected}, got {}", preview_for_error(value))));
        }
    }
    // `on_hover` fires on the crossing its node's own `hover` signal reports through
    // `Signal::pointer_crossed`, so that signal is where the pointer's record lives (ADR-0095).
    // Without a slot the callback is unreachable, and this is the kind of silence
    // `deserialize_lua_table`'s unknown-key rejection exists to end: a config that declared it
    // would watch a handler never fire with nothing anywhere saying why.
    if properties.contains_key("on_hover") && !properties.contains_key("hover") {
        return Err(invalid(
            "on_hover",
            "declared without a `hover` slot on the same node -- add `hover = hover(\"a-name\")`, which is what remembers whether the pointer was on this node, and so what tells its crossings from another node's",
        ));
    }
    Ok(properties)
}

/// Settle derived property outputs before a retained node checks whether its reads changed.
pub(crate) fn settle_property_signals(
    properties: &PropMap,
    kind: &str,
    tables_plain: bool,
    lua: &Lua,
) -> Result<(), LayoutError> {
    if !holds_signals(properties, tables_plain) {
        return Ok(());
    }
    let mut keys: Vec<_> = properties.keys().copied().collect();
    keys.sort_unstable();
    for property in keys {
        if !is_structural_property(kind, property) {
            resolved_value(properties, kind, property, tables_plain, lua)?;
        }
    }
    Ok(())
}

/// ponytail: 8 tables, past the deepest shape a parser reads (5); deeper, `input::plain` refuses. Upgrade: per-row depth.
const NESTED_SIGNAL_DEPTH: usize = 8;

/// Tables a scan visits before leaving the rest to [`resolve_nested`], whose `seen` map bounds a
/// table reached many ways.
const SCAN_TABLES: usize = 256;

/// The enclosing tables of a walk, so a back-reference keeps the original table.
type TablePath = [*const std::ffi::c_void; NESTED_SIGNAL_DEPTH];

/// Whether a walk leaves `table` (at address `at`) alone: past the depth limit, under a metatable,
/// or already on the `path` of enclosing tables.
fn walk_skips(table: &mlua::Table, at: *const std::ffi::c_void, path: &TablePath, depth: usize) -> bool {
    depth == NESTED_SIGNAL_DEPTH || table.metatable().is_some() || path[..depth].contains(&at)
}

/// Whether `table` under `property` is walked for signals: `child`/`children` hold node tables,
/// each resolved as its own node, and the child-table cache keys them by address.
fn walked_table<'a>(property: &str, value: &'a Value) -> Option<&'a mlua::Table> {
    match value {
        Value::Table(table) if !matches!(property, "child" | "children") => Some(table),
        _ => None,
    }
}

/// Whether no walked table of `properties` may hold a signal. The resolve memo keeps the answer
/// while its declaration holds; a signal put into a table afterwards is not seen, and the parser
/// refuses it.
pub(crate) fn tables_plain(properties: &PropMap) -> bool {
    !properties.iter().any(|(property, value)| walked_table(property, value).is_some_and(may_hold_signal))
}

/// Whether any property holds a userdata, or (unless `tables_plain`) a walked table may hold a signal.
fn holds_signals(properties: &PropMap, tables_plain: bool) -> bool {
    properties.iter().any(|(property, value)| {
        matches!(value, Value::UserData(_))
            || !tables_plain && walked_table(property, value).is_some_and(may_hold_signal)
    })
}

/// Whether a walk of `table` may find a signal, scanned without allocating; a scan past
/// [`SCAN_TABLES`] says yes and leaves the answer to the walk.
/// ponytail: mlua converts each scanned value (~70 ns); upgrade: a raw lua_next scan.
fn may_hold_signal(table: &mlua::Table) -> bool {
    fn scan(table: &mlua::Table, path: &mut TablePath, depth: usize, budget: &mut usize) -> bool {
        let at = table.to_pointer();
        if walk_skips(table, at, path, depth) {
            return false;
        }
        let Some(left) = budget.checked_sub(1) else { return true };
        *budget = left;
        path[depth] = at;
        table.pairs::<Value, Value>().any(|pair| match pair {
            Ok((_, Value::UserData(ud))) => signal::is_signal(&ud),
            Ok((_, Value::Table(inner))) => scan(&inner, path, depth + 1, budget),
            Ok(_) => false,
            Err(_) => true,
        })
    }
    let mut budget = SCAN_TABLES;
    scan(table, &mut [std::ptr::null(); NESTED_SIGNAL_DEPTH], 0, &mut budget)
}

/// `property`'s value with its signals read, top level or nested; `Some(Nil)` makes it absent and
/// `None` means it holds no signal.
fn resolved_value(
    properties: &PropMap,
    kind: &str,
    property: &str,
    tables_plain: bool,
    lua: &Lua,
) -> Result<Option<Value>, LayoutError> {
    let value = &properties[property];
    if let Some(table) = walked_table(property, value).filter(|table| !tables_plain && may_hold_signal(table)) {
        let mut path = [std::ptr::null(); NESTED_SIGNAL_DEPTH];
        let walked = resolve_nested(table, kind, 0, &mut path, &mut Default::default(), lua);
        return Ok(walked.map_err(|e| e.under(property))?.map(Value::Table));
    }
    match value {
        Value::UserData(ud) => read_signal(ud, kind, property, lua),
        _ => Ok(None),
    }
}

/// `ud`'s value when it is a signal, `Nil` when that reads nil; `None` for other userdata.
fn read_signal(ud: &mlua::AnyUserData, kind: &str, property: &str, lua: &Lua) -> Result<Option<Value>, LayoutError> {
    let Some(signal) = signal::from_userdata(ud) else { return Ok(None) };
    Ok(Some(resolve_signal(&signal, kind, property, lua)?.unwrap_or(Value::Nil)))
}

/// A copy of `table` with each signal inside it read, or `None` when it holds none, so a
/// signal-free table is shared, not copied. A signal's result is never walked: resolution stays
/// exactly once. Raw reads; a table with a metatable is left to its parser, which refuses a signal.
/// `seen` is keyed by depth too, since a walk cut at the depth limit depends on where it began.
fn resolve_nested(
    table: &mlua::Table,
    kind: &str,
    depth: usize,
    path: &mut TablePath,
    seen: &mut rustc_hash::FxHashMap<(*const std::ffi::c_void, usize), Option<mlua::Table>>,
    lua: &Lua,
) -> Result<Option<mlua::Table>, LayoutError> {
    let at = table.to_pointer();
    if walk_skips(table, at, path, depth) {
        return Ok(None);
    }
    if let Some(walked) = seen.get(&(at, depth)) {
        return Ok(walked.clone());
    }
    path[depth] = at;
    let lua_err = |e: mlua::Error| invalid("", e.to_string());
    let len = table.raw_len();
    let mut copy: Option<mlua::Table> = None;
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(lua_err)?;
        let read = match &value {
            Value::Table(inner) => resolve_nested(inner, kind, depth + 1, path, seen, lua).map(|t| t.map(Value::Table)),
            Value::UserData(ud) => read_signal(ud, kind, "", lua),
            _ => Ok(None),
        };
        let Some(fresh) = read.map_err(|e| e.under(&key_path(&key)))? else { continue };
        // Compacting would shift every later entry, a gradient stop's colour into its position.
        if fresh.is_nil() && matches!(key, Value::Integer(i) if i >= 1 && i as usize <= len) {
            return Err(invalid(&key_path(&key), "a signal in an array read nil, which would leave a hole"));
        }
        let target = match &copy {
            Some(target) => target.clone(),
            None => {
                let target = lua.create_table().map_err(lua_err)?;
                table.for_each(|key: Value, value: Value| target.raw_set(key, value)).map_err(lua_err)?;
                copy.insert(target).clone()
            }
        };
        target.raw_set(key, fresh).map_err(lua_err)?;
    }
    seen.insert((at, depth), copy.clone());
    Ok(copy)
}

/// `key` as the path segment the parsers name it by: `[2]` or `.top`.
fn key_path(key: &Value) -> String {
    match key {
        Value::Integer(i) => format!("[{i}]"),
        Value::String(s) => format!(".{}", s.display()),
        other => format!("[{}]", preview_for_error(other)),
    }
}

/// `signal`'s value for `property`, `None` for nil: the property is absent.
pub(super) fn resolve_signal(
    signal: &signal::Signal,
    kind: &str,
    property: &str,
    lua: &Lua,
) -> Result<Option<Value>, LayoutError> {
    // Name the node kind: a config has many `background`s, and the bare property left a reader
    // grepping every one of them. `Scene::apply_admitting` adds the surface.
    let value = signal.get_value(lua).map_err(|e| {
        invalid(property, format!("Signal getter on a `{kind}` node failed: {}", crate::lua::describe(&e)))
    })?;
    match value {
        Value::UserData(_) => Err(invalid(
            property,
            "a Signal resolved to another Signal -- resolution happens exactly once, not to a fixed point",
        )),
        Value::Nil => Ok(None),
        value => Ok(Some(value)),
    }
}

/// The carve-outs from decision 1's "parsers resolve a `Signal`" rule, the rows typed
/// [`prop::Structural`]: [`SurfaceTopology`]'s five fields, a popup's `parent`, and every node's
/// optional `id` (ADR-0045 decision 1) keep rejecting one outright.
/// Each is read once per evaluation and a *structural* decision is made on it (where a surface is
/// placed, which retained node a fresh one is), which a `Signal` changing between passes would leave
/// stale. `surface_topology` runs on every `Scene::apply` so `pending_surfaces` can diff it against
/// `applied_topology` (ADR-0216); a surface could otherwise move layer or output with no rebuild. `id`
/// pairs a fresh child against its retained counterpart. ADR-0044 decision 1 leaves both out: a gap,
/// not a rejected case. Every
/// other property only shapes the pass it was read in. This works because [`resolve_declared`] passes
/// the keys [`is_structural_property`] names through raw: a resolved signal is indistinguishable from
/// a literal by the time it reaches a map.
pub(super) fn reject_signal_in_structural_field(property: &str, value: &Value) -> Result<(), LayoutError> {
    if matches!(value, Value::UserData(_)) {
        return Err(LayoutError::UnsupportedSignalProperty(property.to_string()));
    }
    Ok(())
}

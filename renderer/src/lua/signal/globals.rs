use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use mlua::{AnyUserData, IntoLua, Lua, LuaSerdeExt, Table, Value, Variadic};

use crate::lua::luacats::{As, Generic, LuaType, SignalOf, lua_fn};
use crate::lua::marshal::{a_type, list_entries};
use crate::text::snap::LogicalRect;

use super::budget::install_hook;
use super::tracking::next_computed_id;
use super::{
    CellId, DirtyFlag, HELD_SLOT, Signal, SignalKind, from_userdata, is_signal, literal_was_edited, new_derived,
    next_cell_id,
};

/// Applies an `mantle set`/`toggle` to named `state` (ADR-0112), using `set`'s marshalling and
/// dirty checks. Refuses missing state or non-boolean toggle values, the two keybind/config
/// mismatches.
pub fn write_state(lua: &Lua, set: &shared::SetState) -> Result<(), String> {
    let (signal, initial) = lua
        .app_data_ref::<StateRegistry>()
        .and_then(|registry| registry.0.get(&set.name).cloned())
        .ok_or_else(|| format!("this config declares no state({:?}, ...)", set.name))?;
    let value = match &set.write {
        shared::StateWrite::Set(json) => {
            crate::lua::json::to_lua(lua, json).map_err(|err| format!("the value does not convert to Lua: {err}"))?
        }
        shared::StateWrite::Toggle => match signal.get_value(lua) {
            Ok(Value::Boolean(current)) => Value::Boolean(!current),
            Ok(other) => return Err(format!("it holds {}, and only a boolean toggles", other.type_name())),
            Err(err) => return Err(format!("its value could not be read: {err}")),
        },
        // Back to the declared initial when it already holds the value: the scalar comparison
        // `literal_was_edited` makes, so `1` and `1.0` are the same value and a table never is.
        shared::StateWrite::ToggleTo(json) => {
            let wanted = crate::lua::json::to_lua(lua, json)
                .map_err(|err| format!("the value does not convert to Lua: {err}"))?;
            let current = signal.get_value(lua).map_err(|err| format!("its value could not be read: {err}"))?;
            if literal_was_edited(&current, &wanted) == Some(false) { initial } else { wanted }
        }
    };
    signal.reseed(value).map_err(|err| format!("refused at the marshalling boundary: {err}"))
}

/// The evaluation's output reached the screen, so its names are the ones a bare `mantle set` lists.
/// Listing only: `write_state` reaches every name the VM ever declared, before and after this.
pub fn promote_states(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<StateRegistry>() {
        registry.2 = registry.1.clone();
    }
}

/// A bare `mantle set`'s lines, sorted: each promoted state as `name<TAB>value`, the value compact
/// JSON so `mantle set` reads it back. A value JSON cannot hold (a function, a number-keyed table
/// that is not a list) prints the name alone, so one such state does not hide the others.
pub fn declared_states(lua: &Lua) -> Vec<String> {
    let Some(registry) = lua.app_data_ref::<StateRegistry>() else { return Vec::new() };
    let mut lines: Vec<String> = (registry.2.iter())
        .filter_map(|name| Some((name, registry.0.get(name)?)))
        .map(|(name, (signal, _))| {
            let json = signal.get_value(lua).ok().and_then(|value| lua.from_value::<serde_json::Value>(value).ok());
            json.map_or_else(|| name.clone(), |json| format!("{name}\t{json}"))
        })
        .collect();
    lines.sort_unstable();
    lines
}

/// Whether config called `hover(name)`. `crate::wayland` checks first, so configs without tooltip
/// or hover expansion pay no tree clone, walk, or signal writes at pointer-report rate.
pub fn any_hover_registered(lua: &Lua) -> bool {
    lua.app_data_ref::<HoverRegistry>().is_some_and(|registry| !registry.0.is_empty())
}

/// ADR-0044 decision 5 state registry: name preserves last-click values across in-place reloads;
/// the stored literal detects an edited initial, which wins over live state (the wallpaper case).
/// In `Lua::set_app_data`, so ADR-0044 decision 4's persistent VM preserves it and a replaced
/// Renderer starts without it. The first set is the names declared since [`begin_evaluation`], the
/// second the one [`promote_states`] last took, which a bare `mantle set` lists.
#[derive(Default)]
struct StateRegistry(HashMap<String, (Signal, Value)>, HashSet<String>, HashSet<String>);

/// Called by `Loader` before each evaluation: only a second, different seed within one evaluation
/// is a conflict, while one differing from the last evaluation's is an edit.
pub fn begin_evaluation(lua: &Lua) {
    if let Some(mut registry) = lua.app_data_mut::<StateRegistry>() {
        registry.1.clear();
    }
}

/// `hover(name)` registry (ADR-0062 decision 2), name-keyed across reloads so a tooltip stays open
/// through
/// config edits. Separate from [`StateRegistry`], or `state("volume", 0)` and
/// `hover("volume")` would collide and confuse `signal:set()`.
#[derive(Default)]
struct HoverRegistry(HashMap<String, (Signal, Signal)>);

/// Name-keyed `scroll(name)` registry; reload preserves the user's offset and avoids jumping an
/// open panel to top (ADR-0069 decision 2).
#[derive(Default)]
struct ScrollRegistry(HashMap<String, Signal>);

/// Name-keyed `geometry(name)` registry, so a reload keeps the last measured rect instead of
/// answering zero until the next pass.
#[derive(Default)]
struct GeometryRegistry(HashMap<String, Signal>);

/// The cells a pass's geometry write changed (ADR-0147 amendment); the client turns them into one
/// follow-up pass over their readers so a binding on the measurement settles, and only one, so a
/// binding that feeds its own measurement cannot spin the loop.
#[derive(Default)]
struct GeometryMoved(Vec<CellId>);

pub(crate) fn note_geometry_moved(lua: &Lua, id: CellId) {
    if let Some(mut moved) = lua.app_data_mut::<GeometryMoved>() {
        moved.0.push(id);
        return;
    }
    lua.set_app_data(GeometryMoved(vec![id]));
}

/// The cells a pass write moved since the last take.
pub fn take_geometry_moved(lua: &Lua) -> Vec<CellId> {
    lua.app_data_mut::<GeometryMoved>().map(|mut moved| std::mem::take(&mut moved.0)).unwrap_or_default()
}

/// The `ms` a `delay` or a `pulse` is given, as whole milliseconds.
///
/// Bounded on what the caller actually gets rather than on the number it wrote: `0.1` clears a
/// bound written in floats and then rounds to nothing, leaving a `delay` that holds for no time
/// and a `pulse` that is never true, both of them silently.
fn parse_hold(what: &str, millis: f64) -> Result<Duration, mlua::Error> {
    let rounded = millis.round() as u64;
    if !(millis > 0.0 && millis <= 60_000.0) || rounded == 0 {
        return Err(mlua::Error::runtime(format!("{what} must be within [1, 60000] ms, got {millis}")));
    }
    Ok(Duration::from_millis(rounded))
}

/// Registers `computed`, `delay` and `pulse` (ADR-0146, ADR-0153), `state` (ADR-0044 decision 5),
/// `hover`, `hover_rect`, and `scroll`. Dependencies are signal-like userdata. Pass the shared
/// dirty flag explicitly, not via `app_data`: a hidden coupling failing inside a config author's
/// `state()` call is worse than threading one argument through. `set` marks the same flag
/// `new_live` returns and `RendererClient` drains.
pub fn register(lua: &Lua, dirty: DirtyFlag) -> mlua::Result<()> {
    // Before any config code runs, so every coroutine it ever creates inherits the hook.
    install_hook(lua)?;
    let hover_dirty = dirty.clone();
    let rect_dirty = dirty.clone();
    let scroll_dirty = dirty.clone();
    lua_fn!(
        lua,
        /// A signal of `fn` over its dependencies' values, recomputed on read. `fn` must be side-effect free
        /// and runs under the shared 5 ms CPU budget (ADR-0021). ponytail: `fn`'s parameters are untyped,
        /// since typing them needs an overload per arity; prefer `:map` for one source.
        /// [docs](https://anasgets111.github.io/mantle/guide/signals.html#derived-signals)
        fn computed(
            lua,
            /// Signals or capabilities, in `fn`'s argument order; anything else raises.
            dependencies: As<Table, Vec<SignalOf<Value>>>,
            r#fn: fn(values: Variadic<Value>) -> Value,
        ) -> /// Read-only.
        SignalOf<Value, AnyUserData> {
            let entries = list_entries(&dependencies.0)
                .map_err(|detail| mlua::Error::runtime(format!("computed() dependencies: {detail}")))?;
            let collected = (1..)
                .zip(entries)
                .map(|(index, dep)| match dep {
                    // Name the expected type; `borrow`'s error does not.
                    Value::UserData(dep) if is_signal(&dep) => Ok(dep),
                    other => Err(mlua::Error::runtime(format!(
                        "computed() dependency {index} is {}; dependencies must be Signals or `mantle` capabilities",
                        a_type(&other)
                    ))),
                })
                .collect::<mlua::Result<Vec<_>>>()?;
            let kind = SignalKind::Computed { id: next_computed_id(), arity: collected.len() };
            new_derived(lua, kind, Some(r#fn.0), collected).map(SignalOf::new)
        }
    )?;
    lua_fn!(
        lua,
        /// `source`'s value once a new value has held for `ms`; until then, the old one (ADR-0146). A source
        /// that returns to the old value first changes nothing. A trailing debounce, or a close-hold:
        /// `visible = computed({ open, delay(open, ms) }, function(now, was) return now or was end)`.
        /// [docs](https://anasgets111.github.io/mantle/guide/signals.html#delay-hold-a-value)
        fn delay(
            lua,
            /// A signal or capability; anything else raises.
            source: SignalOf<Generic, AnyUserData>,
            /// `[1, 60000]`, rounded to whole milliseconds; outside raises.
            ms: f64,
        ) -> /// Read-only.
        SignalOf<Generic, AnyUserData> {
            let source_ud = source.0;
            let source = from_userdata(&source_ud)
                .ok_or_else(|| mlua::Error::runtime("delay() takes a Signal or an `mantle` capability first"))?;
            let hold = parse_hold("delay() hold", ms)?;
            let held = source.get_value(lua)?;
            let ud = new_derived(lua, SignalKind::Delayed { hold, due: Rc::default() }, None, vec![source_ud])?;
            ud.set_nth_user_value(HELD_SLOT, held)?;
            Ok(SignalOf::new(ud))
        }
    )?;
    lua_fn!(
        lua,
        /// `true` for `ms` after `source` changes value, else `false`; a change inside the window restarts it
        /// (ADR-0153). Fires one-shot animations: `animate = pulse(clicks, 400):map(...)` (ADR-0152). Values
        /// compare with `==`, so a table-valued source changes on every push.
        /// [docs](https://anasgets111.github.io/mantle/guide/signals.html#pulse-mark-a-change)
        fn pulse(
            lua,
            /// A signal or capability; anything else raises.
            source: SignalOf<Value, AnyUserData>,
            /// `[1, 60000]`, rounded to whole milliseconds; outside raises. At least as long as what it drives.
            ms: f64,
        ) -> /// Read-only.
        SignalOf<bool, AnyUserData> {
            let source_ud = source.0;
            let source = from_userdata(&source_ud)
                .ok_or_else(|| mlua::Error::runtime("pulse() takes a Signal or an `mantle` capability first"))?;
            let hold = parse_hold("pulse() window", ms)?;
            let seen = source.get_value(lua)?;
            let ud = new_derived(lua, SignalKind::Pulse { hold, until: Rc::default() }, None, vec![source_ud])?;
            ud.set_nth_user_value(HELD_SLOT, seen)?;
            Ok(SignalOf::new(ud))
        }
    )?;
    lua_fn!(
        lua,
        /// Named writable state that survives reloads. A changed scalar `initial` re-seeds it; a table
        /// `initial` never does (ADR-0044). `mantle set <name> <value>` and `mantle toggle <name> [value]`
        /// write it (ADR-0112): a bare toggle needs a boolean, and toggling to the held value restores `initial`.
        /// [docs](https://anasgets111.github.io/mantle/guide/signals.html#named-state)
        fn state(
            lua,
            /// Its identity: one name, one signal.
            name: String,
            /// The first value, and the signal's type for LuaLS.
            initial: Generic,
        ) -> StateSignal {
            let initial = initial.0;
            let (existing, repeated) = {
                let mut registry = crate::lua::app_data_or_default::<StateRegistry>(lua);
                (registry.0.get(&name).cloned(), !registry.1.insert(name.clone()))
            };
            if let Some((signal, seeded)) = existing {
                // Existing name wins across reload; an edited `initial` is later than `set` and
                // reseeds it (ADR-0044 decision 5 amendment). Within one evaluation, two modules
                // disagreeing would reseed on every reload, so that is refused.
                if literal_was_edited(&initial, &seeded) == Some(true) {
                    if repeated {
                        return Err(mlua::Error::runtime(format!(
                            "state(\"{name}\", ...) is declared twice in this evaluation with different initial values, {seeded:?} then {initial:?}; expected one seed per name"
                        )));
                    }
                    signal.reseed(initial.clone()).map_err(|err| {
                        mlua::Error::runtime(format!(
                            "state(\"{name}\", ...) refused its new initial value at the marshalling boundary: {err}"
                        ))
                    })?;
                    crate::lua::app_data_or_default::<StateRegistry>(lua).0.insert(name, (signal.clone(), initial));
                }
                return Ok(StateSignal(signal));
            }
            let signal = Signal::new_state(initial.clone(), dirty.clone()).map_err(|err| {
                mlua::Error::runtime(format!(
                    "state(\"{name}\", ...) refused its initial value at the marshalling boundary: {err}"
                ))
            })?;
            crate::lua::app_data_or_default::<StateRegistry>(lua).0.insert(name, (signal.clone(), initial));
            Ok(StateSignal(signal))
        }
    )?;
    lua_fn!(
        lua,
        /// Whether the pointer is inside the node whose `hover` is bound to this signal; `false` until it is
        /// (ADR-0062). One name, one signal, across reloads. Read-only.
        /// [docs](https://anasgets111.github.io/mantle/guide/input.html#hover)
        fn hover(lua, name: String) -> SignalOf<bool> {
            Ok(SignalOf::new(hover_slot(lua, &hover_dirty, name)?.0))
        }
    )?;
    lua_fn!(
        lua,
        /// The absolute rect of `hover(name)`'s node, in its surface's logical coordinates, for a `popup`'s
        /// `anchor_rect`. `1x1` at the origin before the first hover; keeps the last rect after the pointer
        /// leaves.
        /// [docs](https://anasgets111.github.io/mantle/guide/input.html#hover)
        fn hover_rect(
            lua,
            /// The `hover` slot. Reading this does not register a region.
            name: String,
        ) -> SignalOf<LogicalRect> {
            Ok(SignalOf::new(hover_slot(lua, &rect_dirty, name)?.1))
        }
    )?;
    lua_fn!(
        lua,
        /// The absolute rect of the node whose `geometry` is bound to this signal, in its surface's logical
        /// coordinates (the space of `on_click` and `hover_rect`); layout writes it (ADR-0147). Zero before
        /// the first layout. A change earns one follow-up pass, so a binding feeding its own measurement cannot loop.
        /// [docs](https://anasgets111.github.io/mantle/guide/signals.html#geometry-read-a-nodes-laid-out-rect)
        fn geometry(
            lua,
            /// One name, one signal, across reloads.
            name: String,
        ) -> SignalOf<LogicalRect> {
            let existing = crate::lua::app_data_or_default::<GeometryRegistry>(lua).0.get(&name).cloned();
            if let Some(signal) = existing {
                return Ok(SignalOf::new(signal));
            }
            let zero = lua.create_table()?;
            for key in ["x", "y", "width", "height"] {
                zero.set(key, 0.0)?;
            }
            let signal = Signal(SignalKind::Geometry(next_cell_id(), Rc::new(RefCell::new(Value::Table(zero)))));
            crate::lua::app_data_or_default::<GeometryRegistry>(lua).0.insert(name, signal.clone());
            Ok(SignalOf::new(signal))
        }
    )?;
    lua_fn!(
        lua,
        /// A viewport's scroll offset along its main axis, in logical pixels from the top or left. The wheel
        /// writes it and layout clamps it (ADR-0069); `:reveal` is the only request Lua makes.
        /// [docs](https://anasgets111.github.io/mantle/guide/input.html#scroll)
        fn scroll(
            lua,
            /// Bind the result as a `row`, `column` or `list`'s `scroll`. One name, one signal, across reloads.
            name: String,
        ) -> ScrollSignal {
            Ok(ScrollSignal(
                crate::lua::app_data_or_default::<ScrollRegistry>(lua)
                    .0
                    .entry(name)
                    .or_insert_with(|| Signal::new_scroll(scroll_dirty.clone()))
                    .clone(),
            ))
        }
    )
}

/// What `state` returns: a [`Signal`] whose `set` works. LuaLS spells the family as classes over a
/// generic `T` that no Rust signature carries (`signals.lua`'s header holds `Signal<T>`), so these
/// class blocks are written here, beside the type.
pub(crate) struct StateSignal(Signal);

impl IntoLua for StateSignal {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

impl LuaType for StateSignal {
    fn lua() -> String {
        "StateSignal<T>".to_string()
    }
    const GENERIC: bool = true;
    fn classes(out: &mut Vec<String>) {
        out.push(
            r#"---@class StateSignal<T>: Signal<T>, userdata
---What `state` returns: the only signal Lua writes.
---@field set fun(self: StateSignal<T>, value: T) Stores `value` and re-resolves its readers. Raises on NaN, infinity, an integer past ±(2^53−1) or a string over 64 KiB; tables are not checked. Types are checked by LuaLS only.
"#
            .to_string(),
        );
    }
}

/// What `scroll` returns: a [`Signal`] whose `reveal` works; see [`StateSignal`] for why its class
/// is written here.
pub(crate) struct ScrollSignal(Signal);

impl IntoLua for ScrollSignal {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

impl LuaType for ScrollSignal {
    fn lua() -> String {
        "ScrollSignal".to_string()
    }
    fn classes(out: &mut Vec<String>) {
        out.push(
            r#"---@class ScrollSignal: Signal<number>, userdata
---What `scroll` returns.
---@field reveal fun(self: ScrollSignal, index: integer) On the next pass, scrolls the least distance that shows the viewport's `index`-th visible child (1-based; a `list`'s items in source order), then the wheel takes over (ADR-0112). An index with no child does nothing; below 1 raises.
"#
            .to_string(),
        );
    }
}

/// Name-keyed hover slot: boolean from `hover(name)`, rect from `hover_rect(name)`
/// (ADR-0062 decision 2).
/// Either global creates the pair, and reloads share it. No marshalling: pointer handler owns both
/// values, not Lua.
fn hover_slot(lua: &Lua, dirty: &DirtyFlag, name: String) -> mlua::Result<(Signal, Signal)> {
    let existing = crate::lua::app_data_or_default::<HoverRegistry>(lua).0.get(&name).cloned();
    if let Some(slot) = existing {
        return Ok(slot);
    }
    let slot = Signal::new_hover(dirty.clone(), Value::Table(unhovered_rect(lua)?));
    crate::lua::app_data_or_default::<HoverRegistry>(lua).0.insert(name, slot.clone());
    Ok(slot)
}

/// Pre-pointer `hover_rect(name)`: real 1x1 origin table. Non-zero because zero `anchor_rect` is
/// rejected; `visible = hover(name)` stays false, so a tooltip waits invisibly at origin until
/// the pointer event supplies the real rect.
fn unhovered_rect(lua: &Lua) -> mlua::Result<mlua::Table> {
    let rect = lua.create_table()?;
    rect.set("x", 0.0)?;
    rect.set("y", 0.0)?;
    rect.set("width", 1.0)?;
    rect.set("height", 1.0)?;
    Ok(rect)
}

#[cfg(test)]
mod tests {
    use super::super::tests::lua_with_state;
    use super::super::*;
    use super::*;

    #[test]
    fn hover_returns_a_read_only_boolean_signal_that_starts_false() {
        // ADR-0062 decision 2: engine-written hover starts false, not nil; `visible` treats nil as
        // absent
        // (ADR-0044 decision 1 amendment).
        let (lua, _dirty) = lua_with_state();
        let started: bool = lua.load(r#"return hover("volume"):get()"#).eval().unwrap();
        assert!(!started);
    }

    #[test]
    fn hover_hands_the_same_name_the_same_signal_so_an_in_place_reload_keeps_it_open() {
        // Name is identity across reload (ADR-0044 decision 5, ADR-0062 decision 2), so the signal
        // is reused, not reset false. Check storage, not userdata `==`, which compares object
        // identity.
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"first = hover("volume") second = hover("volume") other = hover("battery")"#).exec().unwrap();

        let first: mlua::AnyUserData = lua.globals().get("first").unwrap();
        from_userdata(&first).unwrap().hover_handle().unwrap().set(Value::Boolean(true));

        assert!(lua.load("return second:get()").eval::<bool>().unwrap(), "one name is one slot");
        assert!(!lua.load("return other:get()").eval::<bool>().unwrap(), "a different name is a different slot");
    }

    #[test]
    fn hover_rect_reads_a_real_non_zero_rect_before_anything_has_been_hovered() {
        // Live-session bug: nil rect means absent (ADR-0044 decision 1 amendment), while tooltip
        // `anchor_rect` must be non-zero, so from the first frame each capability push made
        // every resolve refuse the popup until something hovered.
        let (lua, _dirty) = lua_with_state();
        let rect: mlua::Table = lua.load(r#"return hover_rect("volume"):get()"#).eval().unwrap();

        assert!(rect.get::<f32>("width").unwrap() > 0.0, "a zero-width anchor_rect is refused by the protocol");
        assert!(rect.get::<f32>("height").unwrap() > 0.0, "and so is a zero-height one");
        assert_eq!(rect.get::<f32>("x").unwrap(), 0.0);
        assert_eq!(rect.get::<f32>("y").unwrap(), 0.0);
    }

    #[test]
    fn state_returns_a_signal_reading_back_the_initial_value_it_was_given() {
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua.load(r#"return state("count", 7):get()"#).eval().unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn the_same_state_name_and_the_same_initial_keeps_the_value_written_since() {
        // ADR-0044 decision 5: reload preserves the user's last click, not the literal.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("open", 0):set(5)
                return state("open", 0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "an unedited literal must keep the current value, not reset to the initial");
    }

    #[test]
    fn a_changed_initial_re_seeds_the_signal_and_marks_dirty() {
        // D5 amendment: changed literal is a later write than `set`; this is the wallpaper path.
        let (lua, dirty) = lua_with_state();
        lua.load(r#"state("open", 0):set(5)"#).exec().unwrap();
        begin_evaluation(&lua);
        let result: i64 = lua.load(r#"return state("open", 99):get()"#).eval().unwrap();
        assert_eq!(result, 99, "an edited literal must win over the value `:set()` left behind");
        assert!(dirty.take(), "a re-seed must mark the scene dirty, or nothing repaints from it");
    }

    #[test]
    fn re_seeding_twice_from_the_same_edited_literal_only_happens_once() {
        // Remember the new literal; the old one would re-seed every later evaluation and clobber
        // `set` forever.
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"state("open", 0)"#).exec().unwrap();
        for _ in 0..2 {
            begin_evaluation(&lua);
            lua.load(r#"state("open", 99):set(7)"#).exec().unwrap();
        }
        let result: i64 = lua.load(r#"return state("open", 99):get()"#).eval().unwrap();
        assert_eq!(result, 7, "the second evaluation of an already-adopted literal is not another edit");
    }

    /// Two modules seeding one name differently would reseed it on every reload, each one
    /// clobbering the other's value.
    #[test]
    fn two_different_seeds_for_one_name_in_one_evaluation_are_refused() {
        let (lua, _dirty) = lua_with_state();
        let err = lua.load(r#"state("mode", 1) state("mode", "s")"#).exec().unwrap_err().to_string();
        assert!(err.contains(r#"state("mode", ...) is declared twice in this evaluation"#), "{err}");
        begin_evaluation(&lua);
        lua.load(r#"state("mode", "s") state("mode", "s")"#).exec().expect("the same seed twice agrees");
    }

    #[test]
    fn a_table_initial_never_counts_as_edited() {
        // A popup's anchor rect: fresh table pointers would make every reload an edit and snap the
        // popup to the corner.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("anchor", { x = 0 }):set(5)
                return state("anchor", { x = 0 }):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "a table literal must keep the live value, since it cannot be compared");
    }

    #[test]
    fn rewriting_an_integer_literal_as_a_float_is_not_an_edit() {
        // Lua `==` says `0 == 0.0`; reformatting a number is not an edit.
        let (lua, _dirty) = lua_with_state();
        let result: i64 = lua
            .load(
                r#"
                state("count", 0):set(5)
                return state("count", 0.0):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, 5, "0 and 0.0 are the same literal");
    }

    #[test]
    fn changing_a_literals_type_is_an_edit() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"state("kind", false):set("clicked")"#).exec().unwrap();
        begin_evaluation(&lua);
        let result: String = lua.load(r#"return state("kind", "waiting"):get()"#).eval().unwrap();
        assert_eq!(result, "waiting", "two scalars of different types are a different literal");
    }

    #[test]
    fn two_state_names_are_two_independent_signals() {
        let (lua, _dirty) = lua_with_state();
        let (a, b): (i64, i64) = lua
            .load(
                r#"
                state("a", 1):set(10)
                return state("a", 1):get(), state("b", 2):get()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!((a, b), (10, 2), "the map is keyed by name, so a write to one name must not reach another");
    }

    /// ADR-0112: keybind writes target config state; refuse missing names and non-boolean toggles.
    #[test]
    fn a_control_clients_write_reaches_a_declared_state_and_is_refused_otherwise() {
        let lua = Lua::new();
        let dirty = DirtyFlag::new();
        register(&lua, dirty.clone()).unwrap();
        lua.load(r#"OPEN = state("launcher_open", false); KIND = state("panel_kind", "none")"#).exec().unwrap();
        dirty.take();

        write_state(&lua, &shared::SetState { name: "launcher_open".into(), write: shared::StateWrite::Toggle })
            .unwrap();
        assert!(dirty.take(), "a write from outside re-resolves the scene like any other");
        assert!(lua.load("return OPEN:get()").eval::<bool>().unwrap());

        let set = shared::StateWrite::Set(serde_json::json!("notifications"));
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: set }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "notifications");
        dirty.take();

        let missing = write_state(&lua, &shared::SetState { name: "nope".into(), write: shared::StateWrite::Toggle });
        assert!(missing.unwrap_err().contains("declares no state"));
        let not_bool =
            write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: shared::StateWrite::Toggle });
        assert!(not_bool.unwrap_err().contains("only a boolean toggles"));
        assert!(!dirty.take(), "a refused write changes nothing");

        // `toggle <name> <value>`: to the value, then back to the declared initial.
        let to_launcher = || shared::StateWrite::ToggleTo(serde_json::json!("launcher"));
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: to_launcher() }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "launcher");
        write_state(&lua, &shared::SetState { name: "panel_kind".into(), write: to_launcher() }).unwrap();
        assert_eq!(lua.load("return KIND:get()").eval::<String>().unwrap(), "none", "already it: back to the initial");
        assert!(dirty.take());
    }

    /// A bare `mantle set` lists the names the shell on screen declared, with values `set` reads back.
    #[test]
    fn the_listing_is_the_applied_evaluations_states_with_their_current_values() {
        let (lua, _dirty) = lua_with_state();
        assert!(declared_states(&lua).is_empty(), "nothing declared lists nothing");
        lua.load(r#"state("open", false):set(true) state("kind", "true") state("tags", { "a" }) state("fn", print)"#)
            .exec()
            .unwrap();
        promote_states(&lua);
        let listed = declared_states(&lua);
        // A string stays quoted, so `mantle set kind '"true"'` restores a string and not a boolean;
        // a function has no JSON, so its name stands alone.
        assert_eq!(listed, ["fn", "kind\t\"true\"", "open\ttrue", "tags\t[\"a\"]"]);

        // A reload that raises part-way is never promoted: the scene on screen, and its names, stay.
        begin_evaluation(&lua);
        lua.load(r#"state("open", false)"#).exec().unwrap();
        assert_eq!(declared_states(&lua), listed);
        promote_states(&lua);
        assert_eq!(declared_states(&lua), ["open\ttrue"], "a dropped name goes");
    }

    #[test]
    fn state_refuses_an_initial_value_that_fails_the_marshalling_boundary() {
        // Lua-authored `state` initial crosses `marshal.rs`.
        let (lua, _dirty) = lua_with_state();
        let err = lua.load(r#"return state("bad", 0/0)"#).eval::<Value>().unwrap_err();
        assert!(err.to_string().contains("finite"), "a NaN initial must be refused by name: {err}");
    }
}

//! `toplevel(id)`: window-frame requests (move, resize, window menu) and the configure state, for an
//! app's own title bar and frame.

use std::collections::HashMap;

use mlua::{IntoLua, Lua, Value};
use shared::warn;

use super::luacats::{LuaType, SignalOf, lua_class, lua_fn, lua_shape};
use super::signal::{DirtyFlag, LiveSignalHandle, Signal};
use crate::layout::node::Decorations;
use crate::layout::node::prop::{Keyword, keywords};

lua_shape! {
    /// The edges a tiling compositor has tiled the window against.
    #[record = "ToplevelTiled"]
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub(crate) struct Tiled {
        pub left: bool,
        pub right: bool,
        pub top: bool,
        pub bottom: bool,
    }
}

lua_shape! {
    /// The most room the compositor suggests for the window, in logical pixels.
    #[record = "ToplevelBounds"]
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub(crate) struct Bounds {
        pub width: u32,
        pub height: u32,
    }
}

lua_shape! {
    /// What the compositor can do for the window; a compositor that never says supports everything.
    #[record = "ToplevelCapabilities"]
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub(crate) struct Capabilities {
        pub window_menu: bool,
        pub maximize: bool,
        pub fullscreen: bool,
        pub minimize: bool,
    }
}

lua_shape! {
    /// What the compositor last configured a `window` to be. Before the first configure and after the window closes it is the default: nothing set, `decoration = "client"`.
    #[record = "ToplevelState"]
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct ToplevelState {
        /// The window has keyboard focus as the compositor sees it.
        pub activated: bool,
        pub maximized: bool,
        pub fullscreen: bool,
        /// An interactive resize is under way.
        pub resizing: bool,
        pub tiled: Tiled,
        /// `nil` until the compositor suggests bounds.
        pub bounds: Option<Bounds>,
        pub capabilities: Capabilities,
        /// Who draws the frame: the mode the compositor chose, which is `"client"` without `zxdg_decoration_manager_v1`.
        pub decoration: Decorations,
    }
}

impl Default for ToplevelState {
    fn default() -> Self {
        Self {
            activated: false,
            maximized: false,
            fullscreen: false,
            resizing: false,
            tiled: Tiled::default(),
            bounds: None,
            capabilities: Capabilities { window_menu: true, maximize: true, fullscreen: true, minimize: true },
            decoration: Decorations::Client,
        }
    }
}

/// `Signal<ToplevelState>`, which also declares the `ToplevelState` class a plain `SignalOf` would not.
pub(crate) struct StateSignal(Signal);

impl LuaType for StateSignal {
    fn lua() -> String {
        SignalOf::<ToplevelState>::lua()
    }
    fn classes(out: &mut Vec<String>) {
        ToplevelState::classes(out);
    }
}

impl IntoLua for StateSignal {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

impl IntoLua for Decorations {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        Ok(Value::String(lua.create_string(self.name())?))
    }
}

/// Per window id: the signal `:state()` hands out, its write end and the last state written.
/// In app data so a reload keeps the signal a config already holds.
#[derive(Default)]
struct States {
    dirty: Option<DirtyFlag>,
    windows: HashMap<String, (Signal, LiveSignalHandle, ToplevelState)>,
}

impl States {
    fn entry(&mut self, lua: &Lua, id: &str) -> mlua::Result<&mut (Signal, LiveSignalHandle, ToplevelState)> {
        if !self.windows.contains_key(id) {
            let state = ToplevelState::default();
            let dirty = self.dirty.clone().unwrap_or_default();
            let (signal, handle) = Signal::new_live(state.clone().into_lua(lua)?, dirty);
            self.windows.insert(id.to_string(), (signal, handle, state));
        }
        Ok(self.windows.get_mut(id).expect("inserted above"))
    }
}

/// Writes `state` to window `id`'s signal and returns whether it changed, so a configure that
/// repeats the last one wakes no reader.
pub(crate) fn publish(lua: &Lua, id: &str, state: ToplevelState) -> bool {
    let mut states = super::app_data_or_default::<States>(lua);
    let Ok(entry) = states.entry(lua, id) else { return false };
    if entry.2 == state {
        return false;
    }
    let Ok(table) = state.clone().into_lua(lua) else { return false };
    entry.1.set(table);
    entry.2 = state;
    true
}

keywords! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Edge { Top, Bottom, Left, Right, TopLeft, TopRight, BottomLeft, BottomRight }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Move,
    Resize(Edge),
    /// Surface-local position of the press.
    Menu((i32, i32)),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) window: String,
    pub(crate) action: Action,
}

#[derive(Default)]
struct Requests {
    /// The press position while a press-time callback runs; compositors honour these requests only for the press serial.
    press: Option<(i32, i32)>,
    queue: Vec<Request>,
}

pub(crate) struct ToplevelHandle(String);

impl ToplevelHandle {
    fn queue(&self, lua: &Lua, action: impl FnOnce((i32, i32)) -> Action) {
        let mut requests = super::app_data_or_default::<Requests>(lua);
        match requests.press {
            Some(at) => requests.queue.push(Request { window: self.0.clone(), action: action(at) }),
            None => warn!(
                "toplevel({:?}): move, resize and show_menu work only inside `on_press` or an `on_drag` \"start\", where the press serial is live, so nothing was sent",
                self.0
            ),
        }
    }
}

lua_class! {
    /// A named `window` the app draws its own frame for. `move`, `resize` and `show_menu` ask the compositor to take over the pointer, and only work inside an `on_press` or `on_drag` `"start"` callback; elsewhere, on a hidden window or on another surface's press, they log a warning and do nothing.
    impl ToplevelHandle {
        /// Start an interactive move of the window, as dragging a title bar does.
        fn r#move(lua, this) {
            this.queue(lua, |_| Action::Move);
            Ok(())
        }

        /// Start an interactive resize from an edge or corner of the window.
        fn resize(
            lua,
            this,
            /// `"top"`, `"bottom"`, `"left"`, `"right"`, `"top_left"`, `"top_right"`, `"bottom_left"` or `"bottom_right"`.
            edge: String,
        ) {
            let Some(edge) = Edge::find(edge.as_bytes()) else {
                return Err(mlua::Error::runtime(format!("resize() takes {}, not {edge:?}", Edge::NAMES.join(", "))));
            };
            this.queue(lua, |_| Action::Resize(edge));
            Ok(())
        }

        /// Open the compositor's window menu at the pointer's press position.
        fn show_menu(lua, this) {
            this.queue(lua, Action::Menu);
            Ok(())
        }

        /// The window's configure state as a read-only signal, rewritten when the compositor changes it, so a frame can follow `activated`, `maximized`, `tiled` and the like. Reading it never requests a change.
        fn state(lua, this) -> StateSignal {
            let mut states = super::app_data_or_default::<States>(lua);
            Ok(StateSignal(states.entry(lua, &this.0)?.0.clone()))
        }
    }
}

pub(crate) fn register(lua: &Lua, dirty: DirtyFlag) -> mlua::Result<()> {
    super::app_data_or_default::<States>(lua).dirty = Some(dirty);
    lua_fn!(
        lua,
        /// Names a `window` whose frame the app draws; call `:move()`, `:resize(edge)` or `:show_menu()` on it from an `on_press`, or `:state()` for its configure state.
        /// [docs](https://anasgets111.github.io/mantle/surfaces/window.html#custom-title-bar)
        fn toplevel(
            _lua,
            /// The `id` of a `window`.
            id: String,
        ) -> ToplevelHandle {
            if id.is_empty() {
                return Err(mlua::Error::runtime("toplevel() requires a nonempty window id"));
            }
            Ok(ToplevelHandle(id))
        }
    )
}

/// Opens the press-time window for callbacks that run on a press at `position`.
pub(crate) fn begin_press(lua: &Lua, position: (f64, f64)) {
    super::app_data_or_default::<Requests>(lua).press = Some((position.0 as i32, position.1 as i32));
}

/// Closes the press-time window and takes what the callbacks queued.
pub(crate) fn end_press(lua: &Lua) -> Vec<Request> {
    let mut requests = super::app_data_or_default::<Requests>(lua);
    requests.press = None;
    std::mem::take(&mut requests.queue)
}

#[cfg(test)]
mod tests {
    use mlua::AnyUserData;

    use super::*;

    fn lua() -> Lua {
        let lua = Lua::new();
        register(&lua, DirtyFlag::new()).unwrap();
        let handle: AnyUserData = lua.load("return toplevel('main')").eval().unwrap();
        lua.globals().set("frame", handle).unwrap();
        lua
    }

    fn request(action: Action) -> Request {
        Request { window: "main".into(), action }
    }

    #[test]
    fn methods_queue_their_request_during_a_press_and_drain_once() {
        let lua = lua();
        begin_press(&lua, (12.7, 3.2));
        lua.load("frame:move() frame:resize('bottom_left') frame:show_menu()").exec().unwrap();
        assert_eq!(
            end_press(&lua),
            vec![request(Action::Move), request(Action::Resize(Edge::BottomLeft)), request(Action::Menu((12, 3)))]
        );
        assert!(end_press(&lua).is_empty());
    }

    #[test]
    fn outside_a_press_nothing_is_queued() {
        let lua = lua();
        lua.load("frame:move()").exec().unwrap();
        begin_press(&lua, (0.0, 0.0));
        end_press(&lua);
        lua.load("frame:show_menu()").exec().unwrap();
        assert!(end_press(&lua).is_empty());
    }

    #[test]
    fn state_is_one_read_only_signal_per_window_that_publish_writes_only_on_a_change() {
        let lua = lua();
        let read = |source: &str| {
            let ud: mlua::AnyUserData = lua.load(source).eval().unwrap();
            let Value::Table(table) = super::super::signal::from_userdata(&ud).unwrap().get_value(&lua).unwrap() else {
                panic!("the state is a table");
            };
            table
        };
        let before = read("return frame:state()");
        assert!(!before.get::<bool>("activated").unwrap());
        assert_eq!(before.get::<String>("decoration").unwrap(), "client");
        assert!(before.get::<mlua::Table>("capabilities").unwrap().get::<bool>("minimize").unwrap());
        assert!(before.get::<Value>("bounds").unwrap().is_nil());

        assert!(!publish(&lua, "main", ToplevelState::default()), "the default is what it already holds");
        let focused = ToplevelState {
            activated: true,
            bounds: Some(Bounds { width: 800, height: 600 }),
            decoration: Decorations::Server,
            ..ToplevelState::default()
        };
        assert!(publish(&lua, "main", focused.clone()));
        assert!(!publish(&lua, "main", focused), "a repeated configure writes nothing");
        let now = read("return toplevel('main'):state()");
        assert!(now.get::<bool>("activated").unwrap());
        assert_eq!(now.get::<String>("decoration").unwrap(), "server");
        assert_eq!(now.get::<mlua::Table>("bounds").unwrap().get::<u32>("width").unwrap(), 800);

        assert!(publish(&lua, "main", ToplevelState::default()), "closing resets it");
        assert!(!read("return frame:state()").get::<bool>("activated").unwrap());
        assert!(lua.load("frame:state():set(1)").exec().is_err(), "read-only");
    }

    #[test]
    fn a_bad_edge_or_empty_id_raises() {
        let lua = lua();
        begin_press(&lua, (0.0, 0.0));
        for edge in ["'middle'", "'TOP'", "''", "nil"] {
            assert!(lua.load(format!("frame:resize({edge})")).exec().is_err(), "{edge}");
        }
        assert!(lua.load("toplevel('')").exec().is_err());
        assert!(end_press(&lua).is_empty());
    }
}

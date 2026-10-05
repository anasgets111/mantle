//! `toplevel(id)`: window-frame requests (move, resize, window menu) for an app's own title bar.

use mlua::Lua;
use shared::warn;

use super::luacats::{lua_class, lua_fn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Edge {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "top" => Self::Top,
            "bottom" => Self::Bottom,
            "left" => Self::Left,
            "right" => Self::Right,
            "top_left" => Self::TopLeft,
            "top_right" => Self::TopRight,
            "bottom_left" => Self::BottomLeft,
            "bottom_right" => Self::BottomRight,
            _ => return None,
        })
    }
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
    /// A named `window` the app draws its own frame for. Each method asks the compositor to take over the pointer, and only works inside an `on_press` or `on_drag` `"start"` callback; elsewhere, on a hidden window or on another surface's press, it logs a warning and does nothing.
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
            let Some(edge) = Edge::parse(&edge) else {
                return Err(mlua::Error::runtime(format!(
                    "resize() takes top, bottom, left, right, top_left, top_right, bottom_left or bottom_right, not {edge:?}"
                )));
            };
            this.queue(lua, |_| Action::Resize(edge));
            Ok(())
        }

        /// Open the compositor's window menu at the pointer's press position.
        fn show_menu(lua, this) {
            this.queue(lua, Action::Menu);
            Ok(())
        }
    }
}

pub(crate) fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// Names a `window` whose frame the app draws; call `:move()`, `:resize(edge)` or `:show_menu()` on it from an `on_press`.
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
        register(&lua).unwrap();
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

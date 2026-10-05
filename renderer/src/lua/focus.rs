//! Named requests to move keyboard focus to a field or control from a click or a key callback.

use mlua::{Lua, Value};

use super::luacats::{lua_class, lua_fn};

#[derive(Default)]
struct Requests {
    /// The surface and whether the running callback was keyboard-delivered.
    callback: Option<(String, bool)>,
    pending: Option<Request>,
    texts: Vec<(String, String)>,
}

/// A `:request()` made inside a callback; `ring` when a key callback made it.
#[derive(Debug, PartialEq)]
pub(crate) struct Request {
    pub surface: String,
    pub name: String,
    pub ring: bool,
}

pub(crate) struct FocusHandle(String);

/// The paste path's limits, so a set or seeded text is one a paste could have typed.
pub(crate) fn is_settable(text: &str) -> bool {
    super::marshal::check_string(text).is_ok() && !text.chars().any(char::is_control)
}

lua_class! {
    /// A named focus target: a plain textfield or a focusable control.
    impl FocusHandle {
        /// Give this field or control the keyboard after the current callback updates its surface. Works from
        /// `on_click`, `on_key` and edits typed or committed into a field (`on_change`, `on_submit`,
        /// `on_cancel`); elsewhere it does nothing, as in the `on_change` an `autofocus` or `set_text` fires.
        /// A key callback's request shows the focus outline.
        fn request(lua, this) {
            let mut requests = super::app_data_or_default::<Requests>(lua);
            if let Some((surface, ring)) = requests.callback.clone() {
                requests.pending = Some(Request { surface, name: this.0.clone(), ring });
            }
            Ok(())
        }

        /// Sets the text of every plain textfield with this name and an `on_change` or `on_submit`, hidden ones
        /// too: caret at the end, undo and composition cleared, no `on_change`. Does nothing on a control. Raises
        /// on control characters or over 64 KiB. Applies when the callback returns.
        fn set_text(lua, this, text: String) {
            if !is_settable(&text) {
                return Err(mlua::Error::runtime("set_text() takes at most 64 KiB without control characters"));
            }
            super::app_data_or_default::<Requests>(lua).texts.push((this.0.clone(), text));
            Ok(())
        }
    }
}

pub(crate) fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// Names a plain textfield or focusable control that `:request()` can focus from a click or key callback.
        /// [docs](https://anasgets111.github.io/mantle/guide/input.html#text-fields)
        fn focus_target(
            _lua,
            /// Shared with the node's `focus_target` property.
            name: String,
        ) -> FocusHandle {
            if name.is_empty() {
                return Err(mlua::Error::runtime("focus_target() requires a nonempty name"));
            }
            Ok(FocusHandle(name))
        }
    )
}

pub(crate) fn name(value: &Value) -> Option<String> {
    let Value::UserData(handle) = value else { return None };
    handle.borrow::<FocusHandle>().ok().map(|handle| handle.0.clone())
}

/// Opens the window in which `:request()` counts: a click or a key callback on `surface`.
pub(crate) fn begin_callback(lua: &Lua, surface: &str, keyboard: bool) {
    super::app_data_or_default::<Requests>(lua).callback = Some((surface.to_owned(), keyboard));
}

pub(crate) fn end_callback(lua: &Lua) {
    super::app_data_or_default::<Requests>(lua).callback = None;
}

/// Drops texts queued for the tree this evaluation replaces.
pub(crate) fn begin_evaluation(lua: &Lua) {
    super::app_data_or_default::<Requests>(lua).texts.clear();
}

pub(crate) fn take_texts(lua: &Lua) -> Vec<(String, String)> {
    std::mem::take(&mut super::app_data_or_default::<Requests>(lua).texts)
}

pub(crate) fn take_request(lua: &Lua) -> Option<Request> {
    super::app_data_or_default::<Requests>(lua).pending.take()
}

#[cfg(test)]
mod tests {
    use mlua::AnyUserData;

    use super::*;

    #[test]
    fn requests_only_inside_a_callback_and_keep_its_surface_and_origin() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let handle: AnyUserData = lua.load("return focus_target('search')").eval().unwrap();
        lua.globals().set("target", handle).unwrap();
        lua.load("target:request()").exec().unwrap();
        assert_eq!(take_request(&lua), None);
        for ring in [false, true] {
            begin_callback(&lua, "panel@TEST", ring);
            lua.load("target:request()").exec().unwrap();
            end_callback(&lua);
            assert_eq!(take_request(&lua), Some(Request { surface: "panel@TEST".into(), name: "search".into(), ring }));
        }
    }

    #[test]
    fn set_text_queues_valid_text_from_anywhere_and_refuses_control_characters() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let handle: AnyUserData = lua.load("return focus_target('search')").eval().unwrap();
        lua.globals().set("target", handle).unwrap();
        lua.load("target:set_text('héllo')").exec().unwrap();
        assert!(lua.load("target:set_text('a\\nb')").exec().is_err());
        assert!(lua.load("target:set_text(('x'):rep(65537))").exec().is_err());
        assert_eq!(take_texts(&lua), vec![("search".to_string(), "héllo".to_string())]);
        assert!(take_texts(&lua).is_empty());
    }
}

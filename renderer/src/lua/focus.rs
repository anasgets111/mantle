//! Named requests to return keyboard input to a plain field after a click.

use mlua::{Lua, Value};

use super::luacats::{lua_class, lua_fn};

#[derive(Default)]
struct Requests {
    click_surface: Option<String>,
    pending: Option<(String, String)>,
    texts: Vec<(String, String)>,
}

/// The longest text `set_text` takes, the same ceiling as a paste.
const MAX_TEXT_BYTES: usize = 64 * 1024;

pub(crate) struct FocusHandle(String);

lua_class! {
    /// A named plain textfield focus target.
    impl FocusHandle {
        /// Give this field the keyboard after the current click updates its surface.
        fn request(lua, this) {
            let mut requests = super::app_data_or_default::<Requests>(lua);
            if let Some(surface) = requests.click_surface.clone() {
                requests.pending = Some((surface, this.0.clone()));
            }
            Ok(())
        }

        /// Replaces the draft of every visible plain textfield bound to this name, as if typed: the caret goes to the end and undo history clears. Calls no `on_change`, and a field being composed in loses the composition. Text with control characters or over 64 KiB raises. Takes effect when the current callback returns.
        fn set_text(lua, this, text: String) {
            if text.len() > MAX_TEXT_BYTES || text.chars().any(char::is_control) {
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
        /// Names a plain textfield that an `on_click` can focus with `:request()`.
        /// [docs](https://anasgets111.github.io/mantle/guide/input.html#text-fields)
        fn focus_target(
            _lua,
            /// Shared with the textfield's `focus_target` property.
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

pub(crate) fn begin_click(lua: &Lua, surface: &str) {
    super::app_data_or_default::<Requests>(lua).click_surface = Some(surface.to_owned());
}

pub(crate) fn end_click(lua: &Lua) {
    super::app_data_or_default::<Requests>(lua).click_surface = None;
}

pub(crate) fn take_texts(lua: &Lua) -> Vec<(String, String)> {
    std::mem::take(&mut super::app_data_or_default::<Requests>(lua).texts)
}

pub(crate) fn take_request(lua: &Lua) -> Option<(String, String)> {
    super::app_data_or_default::<Requests>(lua).pending.take()
}

#[cfg(test)]
mod tests {
    use mlua::AnyUserData;

    use super::*;

    #[test]
    fn requests_only_inside_a_click_keep_the_click_surface() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let handle: AnyUserData = lua.load("return focus_target('search')").eval().unwrap();
        lua.globals().set("target", handle).unwrap();
        lua.load("target:request()").exec().unwrap();
        assert_eq!(take_request(&lua), None);
        begin_click(&lua, "panel@TEST");
        lua.load("target:request()").exec().unwrap();
        end_click(&lua);
        assert_eq!(take_request(&lua), Some(("panel@TEST".into(), "search".into())));
    }

    #[test]
    fn set_text_queues_valid_text_from_anywhere_and_refuses_control_characters() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let handle: AnyUserData = lua.load("return focus_target('search')").eval().unwrap();
        lua.globals().set("target", handle).unwrap();
        lua.load("target:set_text('héllo')").exec().unwrap();
        assert!(lua.load("target:set_text('a\\nb')").exec().is_err());
        assert_eq!(take_texts(&lua), vec![("search".to_string(), "héllo".to_string())]);
        assert!(take_texts(&lua).is_empty());
    }
}

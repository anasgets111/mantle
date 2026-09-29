//! Named requests to return keyboard input to a plain field after a button click.

use mlua::{Lua, Value};

use super::luacats::{lua_class, lua_fn};

#[derive(Default)]
struct Requests {
    click_surface: Option<String>,
    pending: Option<(String, String)>,
}

pub(crate) struct FocusHandle(String);

lua_class! {
    /// A named plain textfield focus target.
    impl FocusHandle {
        /// Give this field the keyboard after the current button click updates its surface.
        fn request(lua, this) {
            let mut requests = super::app_data_or_default::<Requests>(lua);
            if let Some(surface) = requests.click_surface.clone() {
                requests.pending = Some((surface, this.0.clone()));
            }
            Ok(())
        }
    }
}

pub(crate) fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// Names a plain textfield that a button can focus with `:request()`.
        /// [docs](https://anasgets111.github.io/mantle/guide/input.html#text-fields)
        fn focus(
            lua,
            /// Shared with the textfield's `focus` property.
            name: String,
        ) -> FocusHandle {
            if name.is_empty() {
                return Err(mlua::Error::runtime("focus() requires a nonempty name"));
            }
            let _ = lua;
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
        let handle: AnyUserData = lua.load("return focus('search')").eval().unwrap();
        lua.globals().set("target", handle).unwrap();
        lua.load("target:request()").exec().unwrap();
        assert_eq!(take_request(&lua), None);
        begin_click(&lua, "panel@TEST");
        lua.load("target:request()").exec().unwrap();
        end_click(&lua);
        assert_eq!(take_request(&lua), Some(("panel@TEST".into(), "search".into())));
    }
}

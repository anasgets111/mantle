//! `on_key`: the keys a plain field or secure field does not take, bubbled from the keyboard-focused
//! node through its ancestors to the surface root.

use mlua::IntoLua;
use xkbcommon::xkb;

use super::*;
use crate::layout::node::fields::common;
use crate::lua::luacats::spelled;

/// One key press as `on_key` receives it: `---@class KeyPress` in `surfaces.lua`.
pub(crate) struct KeyPress {
    name: String,
    text: Option<String>,
    /// `(ctrl, shift, alt, super)`.
    modifiers: [bool; 4],
    repeat: bool,
}

spelled!(KeyPress => "KeyPress");

impl KeyPress {
    pub(super) fn new(event: &KeyEvent, modifiers: [bool; 4], repeat: bool) -> Self {
        // Text a key types, never a control character: Return and Escape carry C0 text, and so does
        // any Ctrl chord.
        let text = event.utf8.as_deref().filter(|text| !text.is_empty() && !text.chars().any(char::is_control));
        // xkb's canonical names for these four are the X11 `Prior`/`Next`; the usual spelling is kept.
        let name = match event.keysym {
            Keysym::Page_Up => "Page_Up".to_string(),
            Keysym::Page_Down => "Page_Down".to_string(),
            Keysym::KP_Page_Up => "KP_Page_Up".to_string(),
            Keysym::KP_Page_Down => "KP_Page_Down".to_string(),
            keysym => xkb::keysym_get_name(keysym),
        };
        Self { name, text: text.map(str::to_string), modifiers, repeat }
    }
}

impl IntoLua for KeyPress {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        let table = lua.create_table()?;
        table.set("name", self.name)?;
        table.set("text", self.text)?;
        let modifiers = lua.create_table()?;
        for (name, held) in ["ctrl", "shift", "alt", "super"].into_iter().zip(self.modifiers) {
            modifiers.set(name, held)?;
        }
        table.set("modifiers", modifiers)?;
        table.set("repeat", self.repeat)?;
        Ok(Value::Table(table))
    }
}

/// Whether Lua hears this key at all. A secure field being armed silences it for the whole surface
/// (ADR-0005), and a bare modifier is not a key worth a call.
pub(super) fn lua_hears(secure_armed: bool, keysym: Keysym) -> bool {
    !secure_armed && !keysym.is_modifier_key()
}

/// The `on_key` of every node on `path` (root first), innermost first. Empty when the path ends at
/// a masked field, so a destination-bound field never has a handler above it called.
pub(super) fn key_handlers(path: &[&layout::ResolvedNode]) -> Vec<Function> {
    if matches!(path.last().and_then(|node| focused_field(&[node])), Some(FieldTarget::Masked { .. })) {
        return Vec::new();
    }
    path.iter().rev().filter_map(|node| common::on_key.read(&node.properties).ok().flatten()).collect()
}

/// Calls `handlers` in order until one returns `true`; whether one did.
pub(super) fn deliver(lua: &Lua, handlers: &[Function], key: KeyPress, what: &str) -> bool {
    if handlers.is_empty() {
        return false;
    }
    let Ok(key) = key.into_lua(lua) else { return false };
    for handler in handlers {
        match handler.call::<Option<bool>>(&key) {
            Ok(Some(true)) => return true,
            Ok(_) => {}
            Err(err) => crate::lua::warn_raised(Err(err), format_args!("{what}: on_key")),
        }
    }
    false
}

impl App {
    /// The `on_key` handlers a key arriving now reaches: from the focused control, else the focused
    /// surface's root alone.
    pub(super) fn key_handlers_now(&self, event: &KeyEvent) -> (String, Vec<Function>) {
        let none = (String::new(), Vec::new());
        if !lua_hears(self.focused_secure_submit.is_some(), event.keysym) {
            return none;
        }
        let (surface_id, control) = match (&self.focused_control, &self.keyboard_focus) {
            (Some(control), _) => (control.surface_id.clone(), Some(control.id)),
            (None, Some(id)) => (id.clone(), None),
            (None, None) => return none,
        };
        let Some(tree) = self.client.scene().surface(&surface_id) else { return none };
        let path = match control {
            Some(id) => layout::hit::path_to_node(tree, id).unwrap_or_default(),
            None => vec![tree],
        };
        let handlers = key_handlers(&path);
        (surface_id, handlers)
    }

    /// Bubbles `event` through `on_key`; whether a handler took it.
    pub(super) fn deliver_on_key(&mut self, event: &KeyEvent, repeat: bool) -> bool {
        let (surface_id, handlers) = self.key_handlers_now(event);
        let modifiers = [self.ctrl_held, self.shift_held, self.alt_held, self.super_held];
        deliver(self.client.lua(), &handlers, KeyPress::new(event, modifiers, repeat), &surface_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::hit_node;
    use super::super::tests::{key, plain_textfield, secure_submit_table, textfield, with_property};
    use super::*;

    /// A handler that appends `tag` to the global `log` and returns `ret`.
    fn handler(lua: &Lua, tag: &str, ret: &str) -> Value {
        let source = format!("return function(k) log[#log + 1] = '{tag}:' .. k.name; return {ret} end");
        Value::Function(lua.load(source).eval().unwrap())
    }

    fn log(lua: &Lua) -> Vec<String> {
        lua.globals().get::<mlua::Table>("log").unwrap().sequence_values().map(Result::unwrap).collect()
    }

    fn lua_with_log() -> Lua {
        let lua = Lua::new();
        lua.globals().set("log", lua.create_table().unwrap()).unwrap();
        lua
    }

    fn press(keysym: Keysym, utf8: Option<&str>, modifiers: [bool; 4], repeat: bool) -> KeyPress {
        KeyPress::new(&key(keysym, utf8), modifiers, repeat)
    }

    #[test]
    fn the_handler_gets_the_name_text_modifiers_and_repeat() {
        let lua = lua_with_log();
        let seen: Function = lua
            .load(
                "return function(k) return k.name == 'A' and k.text == 'A' and k.modifiers.shift and k.modifiers.ctrl == false
                    and k.modifiers.alt == false and k.modifiers.super and k['repeat'] == true end",
            )
            .eval()
            .unwrap();
        assert!(deliver(&lua, &[seen], press(Keysym::A, Some("A"), [false, true, false, true], true), "t"));
        let nameless: Function = lua
            .load("return function(k) return k.name == 'Return' and k.text == nil and k.modifiers.ctrl and not k['repeat'] end")
            .eval()
            .unwrap();
        let ctrl_return = press(Keysym::Return, Some("\r"), [true, false, false, false], false);
        assert!(deliver(&lua, &[nameless], ctrl_return, "t"), "control text is no text");
        assert_eq!(press(Keysym::Page_Down, None, [false; 4], false).name, "Page_Down");
        assert_eq!(press(Keysym::Escape, Some("\u{1b}"), [false; 4], false).name, "Escape");
    }

    #[test]
    fn a_key_bubbles_from_the_focused_node_to_the_surface_and_true_stops_it() {
        let lua = lua_with_log();
        let node = |tag: &str, ret: &str| {
            with_property(hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false), "on_key", handler(&lua, tag, ret))
        };
        let (root, middle, leaf) = (node("root", "nil"), node("middle", "false"), node("leaf", "nil"));
        let bare = hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false);
        let path = [&root, &bare, &middle, &leaf];
        let handlers = key_handlers(&path);
        assert_eq!(handlers.len(), 3, "a node without on_key is skipped");
        assert!(!deliver(&lua, &handlers, press(Keysym::Down, None, [false; 4], false), "t"));
        assert_eq!(log(&lua), ["leaf:Down", "middle:Down", "root:Down"]);

        lua.globals().set("log", lua.create_table().unwrap()).unwrap();
        let stopper = node("middle", "true");
        let handlers = key_handlers(&[&root, &stopper, &leaf]);
        assert!(deliver(&lua, &handlers, press(Keysym::Down, None, [false; 4], false), "t"));
        assert_eq!(log(&lua), ["leaf:Down", "middle:Down"], "the surface never hears a handled key");
    }

    #[test]
    fn a_surface_root_alone_hears_a_key_nothing_took() {
        let lua = lua_with_log();
        let root = with_property(
            hit_node(&lua, "panel", (0.0, 0.0, 99.0, 99.0), false),
            "on_key",
            handler(&lua, "surface", "true"),
        );
        assert!(deliver(&lua, &key_handlers(&[&root]), press(Keysym::F5, None, [false; 4], false), "t"));
        assert_eq!(log(&lua), ["surface:F5"]);
    }

    #[test]
    fn a_focused_textfield_hands_unused_keys_to_its_ancestors_on_key() {
        let lua = lua_with_log();
        let field = with_property(plain_textfield(&lua), "on_key", handler(&lua, "field", "nil"));
        let parent = with_property(
            hit_node(&lua, "column", (0.0, 0.0, 99.0, 99.0), false),
            "on_key",
            handler(&lua, "parent", "nil"),
        );
        let handlers = key_handlers(&[&parent, &field]);
        assert!(!deliver(&lua, &handlers, press(Keysym::Down, None, [false; 4], false), "t"));
        assert_eq!(log(&lua), ["field:Down", "parent:Down"]);
    }

    #[test]
    fn a_masked_field_and_an_armed_secure_field_never_reach_on_key() {
        let lua = lua_with_log();
        let secret = with_property(
            textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
            "on_key",
            handler(&lua, "secret", "nil"),
        );
        let root = with_property(
            hit_node(&lua, "panel", (0.0, 0.0, 99.0, 99.0), false),
            "on_key",
            handler(&lua, "root", "nil"),
        );
        assert!(key_handlers(&[&root, &secret]).is_empty(), "no handler above a secure field hears its keys");
        assert!(lua_hears(false, Keysym::a));
        assert!(!lua_hears(true, Keysym::a), "an armed secure field silences the surface");
        assert!(!lua_hears(false, Keysym::Shift_L), "a bare modifier is no key");
    }

    #[test]
    fn on_key_makes_a_named_node_focusable_and_a_nameless_one_not() {
        let lua = lua_with_log();
        let keyed =
            with_property(hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false), "on_key", handler(&lua, "k", "nil"));
        assert!(!layout::scene::is_named_control(&keyed));
        let named = with_property(keyed, "accessible_name", Value::String(lua.create_string("Grid").unwrap()));
        assert!(layout::scene::is_named_control(&named));
    }

    #[test]
    fn a_handler_that_raises_does_not_stop_the_ones_above_it() {
        let lua = lua_with_log();
        let raising: Function = lua.load("return function() error('boom') end").eval().unwrap();
        let after: Function = lua.load("return function() return true end").eval().unwrap();
        assert!(deliver(&lua, &[raising, after], press(Keysym::a, Some("a"), [false; 4], false), "t"));
    }
}

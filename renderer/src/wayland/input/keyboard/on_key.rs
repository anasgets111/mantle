//! `on_key`: the keys no field takes, bubbled from the focused node up to the surface root.

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
    /// `None` for a release, which has no `repeat` field.
    repeat: Option<bool>,
}

/// One key release as `on_key_release` receives it: a [`KeyPress`] without `repeat`.
pub(crate) struct KeyRelease(KeyPress);

spelled!(KeyPress => "KeyPress");
spelled!(KeyRelease => "KeyRelease");

impl KeyPress {
    pub(super) fn new(event: &KeyEvent, modifiers: [bool; 4], repeat: Option<bool>) -> Self {
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
        table.set("modifiers", crate::lua::marshal::modifiers_table(lua, self.modifiers)?)?;
        if let Some(repeat) = self.repeat {
            table.set("repeat", repeat)?;
        }
        Ok(Value::Table(table))
    }
}

impl IntoLua for KeyRelease {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

/// Whether Lua hears this key at all. A secure field being armed silences it for the whole surface
/// (ADR-0005), and a bare modifier is not a key worth a call.
pub(super) fn lua_hears(secure_armed: bool, keysym: Keysym) -> bool {
    !secure_armed && !keysym.is_modifier_key()
}

/// A press offered to `on_key` for its `on_key_release` (never one typed into an armed secure
/// field), with the surface and control it was routed from: its release goes there even if the
/// press moved focus.
pub(crate) struct HeldPress {
    event: KeyEvent,
    route: KeyRoute,
}

/// The surface and focused control `on_key` bubbles from; `None` control is the surface root alone.
pub(super) type KeyRoute = (String, Option<layout::scene::NodeId>);

/// The held press a release of `raw_code` ends; `None` for one never offered to `on_key`, or pressed
/// before the keyboard entered (`leave` empties the list). A new press of the key drops a stale one.
pub(super) fn take_press(pressed: &mut Vec<HeldPress>, raw_code: u32) -> Option<HeldPress> {
    let at = pressed.iter().position(|held| held.event.raw_code == raw_code)?;
    Some(pressed.swap_remove(at))
}

/// The `on_key` (or, for a `release`, `on_key_release`) of every node on `path` (root first),
/// innermost first. Empty when the path ends at a masked field, so a destination-bound field never
/// has a handler above it called.
pub(super) fn key_handlers(path: &[&layout::ResolvedNode], release: bool) -> Vec<Function> {
    if matches!(path.last().and_then(|node| focused_field(&[node])), Some(FieldTarget::Masked { .. })) {
        return Vec::new();
    }
    let field = if release { common::on_key_release } else { common::on_key };
    path.iter().rev().filter_map(|node| field.read(&node.properties).ok().flatten()).collect()
}

/// Calls `handlers` in order until one returns `true`; whether one did.
pub(super) fn deliver(lua: &Lua, handlers: &[Function], key: impl IntoLua, what: &str) -> bool {
    if handlers.is_empty() {
        return false;
    }
    let Ok(key) = key.into_lua(lua) else { return false };
    crate::lua::focus::begin_callback(lua, what, true);
    let mut taken = false;
    for handler in handlers {
        match handler.call::<Option<bool>>(&key) {
            Ok(Some(true)) => {
                taken = true;
                break;
            }
            Ok(_) => {}
            Err(err) => crate::lua::warn_raised(handler, Err(err), format_args!("{what}: key handler")),
        }
    }
    crate::lua::focus::end_callback(lua);
    taken
}

impl App {
    /// Where a key arriving now bubbles from: the focused control, else the focused surface's root
    /// alone; `None` when Lua hears no such key or nothing holds the keyboard.
    pub(super) fn key_route(&self, event: &KeyEvent) -> Option<KeyRoute> {
        if !lua_hears(self.focused_secure_submit.is_some(), event.keysym) {
            return None;
        }
        match (&self.focused_control, &self.keyboard_focus) {
            (Some(control), _) => Some((control.surface_id.clone(), Some(control.id))),
            (None, Some(id)) => Some((id.clone(), None)),
            (None, None) => None,
        }
    }

    /// The `on_key` (or `on_key_release`) handlers of `route`.
    pub(super) fn route_handlers(&self, (surface_id, control): &KeyRoute, release: bool) -> Vec<Function> {
        let Some(tree) = self.client.scene().surface(surface_id) else { return Vec::new() };
        let path = match control {
            Some(id) => layout::hit::path_to_node(tree, *id).unwrap_or_default(),
            None => vec![tree],
        };
        key_handlers(&path, release)
    }

    /// Bubbles `event` through `on_key`; whether a handler took it.
    pub(super) fn deliver_on_key(&mut self, event: &KeyEvent, repeat: bool) -> bool {
        let Some(route) = self.key_route(event) else { return false };
        let handlers = self.route_handlers(&route, false);
        let modifiers = self.modifiers_held();
        let key = KeyPress::new(event, modifiers, Some(repeat));
        let taken = deliver(self.client.lua(), &handlers, key, &route.0);
        if !repeat {
            self.pressed_keys.push(HeldPress { event: event.clone(), route });
        }
        taken
    }

    /// Bubbles the release of `press` through `on_key_release` from where its press was routed.
    /// `press` supplies the name and text: an SCTK release carries no text, and its keysym follows
    /// modifiers let go first.
    pub(super) fn deliver_on_key_release(&mut self, press: &HeldPress) {
        let handlers = self.route_handlers(&press.route, true);
        let key = KeyRelease(KeyPress::new(&press.event, self.modifiers_held(), None));
        deliver(self.client.lua(), &handlers, key, &press.route.0);
    }

    /// `(ctrl, shift, alt, super)` on the seat's keyboard, for `on_key` and the pointer handlers.
    // ponytail: unknown while none of our surfaces has keyboard focus (all false); core Wayland has no upgrade.
    pub(in crate::wayland) fn modifiers_held(&self) -> [bool; 4] {
        [self.held.ctrl, self.held.shift, self.held.alt, self.held.logo]
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
        KeyPress::new(&key(keysym, utf8), modifiers, Some(repeat))
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
        let handlers = key_handlers(&path, false);
        assert_eq!(handlers.len(), 3, "a node without on_key is skipped");
        assert!(!deliver(&lua, &handlers, press(Keysym::Down, None, [false; 4], false), "t"));
        assert_eq!(log(&lua), ["leaf:Down", "middle:Down", "root:Down"]);

        lua.globals().set("log", lua.create_table().unwrap()).unwrap();
        let stopper = node("middle", "true");
        let handlers = key_handlers(&[&root, &stopper, &leaf], false);
        assert!(deliver(&lua, &handlers, press(Keysym::Down, None, [false; 4], false), "t"));
        assert_eq!(log(&lua), ["leaf:Down", "middle:Down"], "the surface never hears a handled key");
    }

    #[test]
    fn a_release_bubbles_through_on_key_release_alone_without_repeat() {
        let lua = lua_with_log();
        let node = |field: &'static str, tag: &str, ret: &str| {
            with_property(hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false), field, handler(&lua, tag, ret))
        };
        let root = node("on_key_release", "root", "nil");
        let stopper = node("on_key_release", "middle", "true");
        let pressed_only = node("on_key", "leaf", "nil");
        assert!(key_handlers(&[&root], false).is_empty(), "on_key never hears a release handler");
        let handlers = key_handlers(&[&root, &stopper, &pressed_only], true);
        let seen: Function = lua
            .load("return function(k) return k['repeat'] == nil and k.text == 'a' and k.modifiers.ctrl end")
            .eval()
            .unwrap();
        let release = || KeyRelease(KeyPress::new(&key(Keysym::a, Some("a")), [true, false, false, false], None));
        assert!(deliver(&lua, &[seen], release(), "t"), "a release has no repeat field");
        assert!(deliver(&lua, &handlers, release(), "t"));
        assert_eq!(log(&lua), ["middle:a"], "true stops it before the surface");
    }

    #[test]
    fn a_release_ends_its_press_and_keeps_the_route_the_press_had() {
        let at = |raw_code| KeyEvent { raw_code, ..key(Keysym::Return, Some("\r")) };
        let route = |surface: &str, control| (surface.to_string(), control);
        let mut pressed = vec![
            HeldPress { event: at(28), route: route("bar@TEST", Some(layout::scene::NodeId::test(1))) },
            HeldPress { event: at(30), route: route("bar@TEST", None) },
        ];
        // The press activated control 1, whose `on_click` moved focus to control 2: the release
        // still goes to control 1, never to a control that did not hear the press.
        let held = take_press(&mut pressed, 28).expect("offered");
        assert_eq!(held.route, route("bar@TEST", Some(layout::scene::NodeId::test(1))));
        assert!(take_press(&mut pressed, 28).is_none(), "one release per press");
        assert!(
            take_press(&mut pressed, 50).is_none(),
            "taken by a field or Tab, or pressed before the keyboard entered"
        );
        assert_eq!(take_press(&mut pressed, 30).map(|held| held.event.utf8), Some(Some("\r".to_string())));
    }

    #[test]
    fn a_surface_root_alone_hears_a_key_nothing_took() {
        let lua = lua_with_log();
        let root = with_property(
            hit_node(&lua, "panel", (0.0, 0.0, 99.0, 99.0), false),
            "on_key",
            handler(&lua, "surface", "true"),
        );
        assert!(deliver(&lua, &key_handlers(&[&root], false), press(Keysym::F5, None, [false; 4], false), "t"));
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
        let handlers = key_handlers(&[&parent, &field], false);
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
        assert!(key_handlers(&[&root, &secret], false).is_empty(), "no handler above a secure field hears its keys");
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
        let released = with_property(
            hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false),
            "on_key_release",
            handler(&lua, "r", "nil"),
        );
        assert!(!layout::scene::is_named_control(&released));
        let named = with_property(released, "accessible_name", Value::String(lua.create_string("Grid").unwrap()));
        assert!(layout::scene::is_named_control(&named), "on_key_release alone makes a named node focusable");
    }

    #[test]
    fn a_request_from_on_key_is_queued_with_the_ring_and_only_inside_the_callback() {
        let lua = lua_with_log();
        crate::lua::focus::register(&lua).unwrap();
        lua.globals().set("target", lua.load("return focus_target('next')").eval::<mlua::Value>().unwrap()).unwrap();
        let handler: Function = lua.load("return function() target:request() end").eval().unwrap();
        deliver(&lua, &[handler], press(Keysym::Right, None, [false; 4], false), "bar@TEST");
        let request = crate::lua::focus::take_request(&lua).expect("queued");
        assert_eq!((request.surface.as_str(), request.name.as_str(), request.ring), ("bar@TEST", "next", true));
        lua.load("target:request()").exec().unwrap();
        assert!(crate::lua::focus::take_request(&lua).is_none(), "the window closed with the callback");
    }

    #[test]
    fn a_handler_that_raises_does_not_stop_the_ones_above_it() {
        let lua = lua_with_log();
        let raising: Function = lua.load("return function() error('boom') end").eval().unwrap();
        let after: Function = lua.load("return function() return true end").eval().unwrap();
        assert!(deliver(&lua, &[raising, after], press(Keysym::a, Some("a"), [false; 4], false), "t"));
    }
}

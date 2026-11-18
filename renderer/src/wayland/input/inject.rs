//! `mantle input`: synthetic pointer and keyboard events for one named surface, fed to the handlers
//! real `wl_pointer` and `wl_keyboard` events reach. Never touches another client, the real focus,
//! the cursor, a lock surface or a `secure_submit` field.

use shared::{InputButton, InputStep};
use xkbcommon::xkb;

use super::*;
use crate::layout::instance::is_instance_of;
use crate::wayland::surface::TrackedRole;

/// Index into `names` of the instance `wanted` names: an exact id first, then the one instance of a
/// declared id (`bar` for `bar@eDP-1`). Several instances need `id@output`.
fn resolve(names: &[&str], wanted: &str) -> Result<usize, String> {
    if let Some(exact) = names.iter().position(|name| *name == wanted) {
        return Ok(exact);
    }
    let of_it: Vec<usize> = (0..names.len()).filter(|&i| is_instance_of(names[i], wanted)).collect();
    match of_it.as_slice() {
        [only] => Ok(*only),
        [] => Err(format!("no shown surface {wanted:?}; shown: {}", names.join(", "))),
        many => {
            let named: Vec<&str> = many.iter().map(|&i| names[i]).collect();
            Err(format!("{wanted:?} has {} instances; name one of: {}", many.len(), named.join(", ")))
        }
    }
}

/// A key combo as the event the keyboard path takes, and the (ctrl, shift) it holds down. Only those
/// two modifiers change what a plain field does.
fn parse_combo(combo: &str) -> Result<(KeyEvent, (bool, bool)), String> {
    let (held, name) = if combo == "+" {
        ("", "plus")
    } else if let Some(held) = combo.strip_suffix("++") {
        (held, "plus")
    } else {
        combo.rsplit_once('+').unwrap_or(("", combo))
    };
    let (mut ctrl, mut shift) = (false, false);
    for modifier in held.split('+').filter(|word| !word.is_empty()) {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            other => return Err(format!("{combo:?}: modifier {other:?} is not ctrl or shift")),
        }
    }
    let mut chars = name.chars();
    let keysym = match (chars.next(), chars.next()) {
        // `shift+a` is the keysym `A`, as a layout would send it.
        (Some(c), None) if shift && c.is_ascii_lowercase() => Keysym::from_char(c.to_ascii_uppercase()),
        (Some(c), None) => Keysym::from_char(c),
        _ => {
            let exact = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
            if exact.raw() == 0 { xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE) } else { exact }
        }
    };
    if keysym.raw() == 0 {
        return Err(format!("{combo:?}: no key named {name:?}"));
    }
    let text = xkb::keysym_to_utf8(keysym);
    let utf8 = (!ctrl && !text.is_empty()).then_some(text);
    Ok((KeyEvent { time: 0, raw_code: 0, keysym, utf8 }, (ctrl, shift)))
}

/// Every key step parses, checked before the first step runs so a bad combo delivers nothing.
fn check_combos(steps: &[InputStep]) -> Result<(), String> {
    steps.iter().try_for_each(|step| match step {
        InputStep::Key(combo) => parse_combo(combo).map(drop),
        _ => Ok(()),
    })
}

/// Every shown `textfield` under `node`.
fn shown_fields<'a>(node: &'a crate::layout::ResolvedNode, out: &mut Vec<&'a crate::layout::ResolvedNode>) {
    if node.visible && !node.leaving {
        out.extend((node.kind == "textfield").then_some(node));
        node.content_children().for_each(|child| shown_fields(child, out));
    }
}

fn evdev(button: InputButton) -> u32 {
    match button {
        InputButton::Left => BTN_LEFT,
        InputButton::Right => BTN_RIGHT,
        InputButton::Middle => BTN_MIDDLE,
    }
}

impl App {
    /// Runs every step of `inject` on its surface, stopping at the first refusal; earlier steps have
    /// run. The tree is not re-resolved between steps, as between events of one real frame.
    pub(in crate::wayland) fn inject_input(&mut self, inject: &shared::Inject) -> Result<(), String> {
        let shown: Vec<usize> =
            (0..self.surfaces.len()).filter(|&i| self.surfaces[i].role.wl_surface().is_some()).collect();
        let names: Vec<&str> = shown.iter().map(|&i| self.surfaces[i].surface_id.as_str()).collect();
        let index = shown[resolve(&names, &inject.surface)?];
        let surface_id = self.surfaces[index].surface_id.clone();
        if matches!(self.surfaces[index].role, TrackedRole::Lock { .. }) {
            return Err(format!("{surface_id} is a lock surface"));
        }
        check_combos(&inject.steps)?;
        for step in &inject.steps {
            match step {
                InputStep::Move { x, y } => self.inject_motion(index, (*x, *y)),
                InputStep::Press(button) => self.inject_button(index, *button, true)?,
                InputStep::Release(button) => self.inject_button(index, *button, false)?,
                InputStep::Wheel { x, y, dy } => {
                    self.inject_motion(index, (*x, *y));
                    let value120 = (dy * 120.0).round() as i32;
                    self.scroll_at(index, (*x, *y), 0.0, 0, 0.0, value120);
                }
                InputStep::Leave => self.pointer_left_destroyed_surface(index),
                InputStep::Key(combo) => {
                    let (event, modifiers) = parse_combo(combo)?;
                    self.inject_key(&surface_id, &event, modifiers)?;
                }
                InputStep::Text(text) => self.inject_text(&surface_id, text)?,
            }
        }
        Ok(())
    }

    fn inject_motion(&mut self, index: usize, position: (f64, f64)) {
        let here = self.pointer_at.as_ref().is_some_and(|(id, _)| *id == self.surfaces[index].surface_id);
        if !here
            && let Some(previous) = self
                .pointer_at
                .as_ref()
                .and_then(|(id, _)| self.surfaces.iter().position(|surface| &surface.surface_id == id))
        {
            self.pointer_left_destroyed_surface(previous);
        }
        let kind = if here { PointerEventKind::Motion { time: 0 } } else { PointerEventKind::Enter { serial: 0 } };
        self.pointer_event(index, position, &kind, true);
        self.sync_pointer(index);
    }

    fn inject_button(&mut self, index: usize, button: InputButton, pressed: bool) -> Result<(), String> {
        let surface_id = &self.surfaces[index].surface_id;
        let position = match &self.pointer_at {
            Some((id, position)) if id == surface_id => *position,
            _ => return Err(format!("the pointer is not on {surface_id}; `move` it there first")),
        };
        if self.reaches_a_secret(index, position) {
            return Err(format!("{surface_id} has a `secure_submit` field or submit button there"));
        }
        let (button, serial) = (evdev(button), 0);
        let kind = if pressed {
            PointerEventKind::Press { time: 0, button, serial }
        } else {
            PointerEventKind::Release { time: 0, button, serial }
        };
        let surface_id = surface_id.clone();
        self.keeping_foreign_focus(&surface_id, |app| app.pointer_event(index, position, &kind, true));
        Ok(())
    }

    /// Runs `deliver` as though `surface_id` held the keyboard and `ctrl`/`shift` were the only
    /// modifiers down, then puts the real keyboard focus and modifiers back. Refuses while a masked
    /// field holds focus: injection never reaches a secret.
    fn with_injected_keyboard<T>(
        &mut self,
        surface_id: &str,
        (ctrl, shift): (bool, bool),
        deliver: impl FnOnce(&mut Self) -> T,
    ) -> Result<T, String> {
        if self.focused_secure_submit.is_some() {
            return Err("a `secure_submit` field holds the keyboard".to_string());
        }
        let focus = self.keyboard_focus.replace(surface_id.to_string());
        let held = std::mem::replace(&mut self.held, Held { ctrl, shift, ..Held::default() });
        let result = self.keeping_foreign_focus(surface_id, deliver);
        self.keyboard_focus = focus;
        // The scope moved back, and `focus_visible` follows it.
        self.sync_focused();
        self.held = held;
        Ok(result)
    }

    /// `mantle input key`: [`Self::apply_key`] with no serial and no repeat, so the clipboard chords
    /// that need the compositor's selection are refused, then the key's `on_key_release`.
    fn inject_key(&mut self, surface_id: &str, event: &KeyEvent, modifiers: (bool, bool)) -> Result<(), String> {
        if modifiers.0 && matches!(event.keysym, Keysym::c | Keysym::C | Keysym::x | Keysym::X | Keysym::v | Keysym::V)
        {
            return Err("copy, cut and paste need the compositor's clipboard".to_string());
        }
        self.with_injected_keyboard(surface_id, modifiers, |app| {
            keyboard::take_press(&mut app.pressed_keys, event.raw_code);
            app.apply_key(event, false, None);
            app.release_offered_key(event.raw_code);
        })
    }

    /// `mantle input type`: the commit path of an input method, so `max_length` and `on_change`
    /// apply. Fails when no plain field is typing, which a real keystroke would drop silently.
    fn inject_text(&mut self, surface_id: &str, text: &str) -> Result<(), String> {
        check_injected_text(text)?;
        self.with_injected_keyboard(surface_id, (false, false), |app| {
            app.prune_text_field_focus();
            if !app.focused_text_field.as_ref().is_some_and(|field| app.text_field_takes_keys(field)) {
                let mut fields = Vec::new();
                app.client.scene().surface(surface_id).into_iter().for_each(|tree| shown_fields(tree, &mut fields));
                // `focused_field` refuses only a disabled field or one without a handler.
                if !fields.iter().any(|f| keyboard::focused_field(&[f]).is_some())
                    && fields.iter().any(|f| !f.is_disabled_field())
                {
                    return Err(format!(
                        "{surface_id}'s textfield has neither `on_change` nor `on_submit`, so it takes no focus"
                    ));
                }
                return Err(format!("no textfield is focused in {surface_id}; press one first"));
            }
            app.apply_ime_edit((0, 0), Some(text));
            Ok(())
        })?
    }
}

/// The text `ime_change` would drop silently, named so `mantle input type` can fail.
fn check_injected_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("`type` needs text".to_string());
    }
    if text.chars().any(char::is_control) {
        return Err("text with a control character is dropped; send those through `key`".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_text_the_input_method_path_would_drop_is_an_error() {
        assert!(check_injected_text("héllo ").is_ok());
        assert!(check_injected_text("").is_err());
        assert!(check_injected_text("a\nb").is_err());
        assert!(check_injected_text("\u{7f}").is_err());
    }

    #[test]
    fn a_bad_combo_anywhere_refuses_the_whole_sequence() {
        let steps =
            [InputStep::Move { x: 1.0, y: 1.0 }, InputStep::Key("ctrl+a".into()), InputStep::Key("alt+x".into())];
        assert!(check_combos(&steps).unwrap_err().contains("alt"));
        assert!(check_combos(&steps[..2]).is_ok());
    }

    #[test]
    fn a_name_picks_its_instance_and_several_need_the_output() {
        let names = ["bar@A", "bar@B", "menu", "bars@A"];
        assert_eq!(resolve(&names, "bar@B"), Ok(1));
        assert_eq!(resolve(&names, "menu"), Ok(2));
        assert_eq!(resolve(&names, "bars"), Ok(3), "one instance needs no output");
        let ambiguous = resolve(&names, "bar").unwrap_err();
        assert!(ambiguous.contains("bar@A") && ambiguous.contains("bar@B"), "{ambiguous}");
        let unknown = resolve(&names, "ba").unwrap_err();
        assert!(unknown.contains("no shown surface"), "{unknown}");
    }

    #[test]
    fn a_combo_names_its_key_and_holds_only_ctrl_and_shift() {
        let (event, held) = parse_combo("ctrl+a").unwrap();
        assert_eq!((event.keysym, event.utf8, held), (Keysym::a, None, (true, false)));
        let (event, held) = parse_combo("shift+a").unwrap();
        assert_eq!((event.keysym, event.utf8.as_deref(), held), (Keysym::A, Some("A"), (false, true)));
        let (event, held) = parse_combo("shift+Tab").unwrap();
        assert_eq!((event.keysym, held), (Keysym::Tab, (false, true)));
        assert_eq!(parse_combo("Return").unwrap().0.keysym, Keysym::Return);
        assert_eq!(parse_combo("down").unwrap().0.keysym, Keysym::Down, "names are not case sensitive");
        assert_eq!(parse_combo("é").unwrap().0.utf8.as_deref(), Some("é"));
        assert_eq!(parse_combo("+").unwrap().0.keysym, Keysym::plus);
        let (event, held) = parse_combo("ctrl++").unwrap();
        assert_eq!((event.keysym, held), (Keysym::plus, (true, false)));
        for bad in ["alt+a", "ctrl+Nope", "ctrl+"] {
            assert!(parse_combo(bad).is_err(), "{bad}");
        }
    }

    /// A launcher (`keyboard_interactivity = mode`, `visible = shown`) with fields `a` (shown with `open`) and `b`;
    /// notes with `n`.
    fn launcher(mode: &str) -> (App, tempfile::TempDir) {
        let (app, dir, _queue, _mock) = App::test_app_with_wire(&format!(
            r#"open, log, w, shown = state("open", false), {{}}, state("w", 200), state("shown", true)
            local function field(name) return textfield {{ width = 100, height = 20, autofocus = true, focused = focused(name),
                on_change = function(t) log[#log + 1] = name .. ":" .. t end }} end
            return {{ panel {{ id = "launcher", layer = "top", output = "TEST", width = 200, height = 80,
                keyboard_interactivity = {mode}, visible = shown,
                child = column {{ children = {{ column {{ visible = open, children = {{ field("a") }} }}, field("b") }} }} }},
              panel {{ id = "notes", layer = "top", output = "TEST", width = w, height = 80,
                keyboard_interactivity = "on_demand", child = field("n") }} }}"#
        ));
        (app, dir)
    }

    /// One main-loop turn's focus upkeep and protocol state after `code`.
    fn turn(app: &mut App, code: &str) {
        app.client.lua().load(code).exec().unwrap();
        assert!(app.client.re_resolve_if_dirty());
        app.drop_unusable_text_field_focus();
        app.prune_control_focus();
        app.apply_resolved_surface_state();
    }

    fn is_focused(app: &App, name: &str) -> bool {
        app.client.lua().load(format!("return focused({name:?}):get()")).eval().unwrap()
    }

    #[test]
    fn a_bound_keyboard_interactivity_turning_on_arms_autofocus_without_an_enter() {
        let (mut app, _dir) = launcher(r#"open:map(function(o) return o and "on_demand" or "none" end)"#);
        turn(&mut app, "open:set(true)");
        turn(&mut app, "w:set(300)");
        assert!(is_focused(&app, "a"), "#130: a later re-resolve keeps the armed field until `enter`");
        assert_eq!(app.inject_text("launcher@TEST", "ze"), Ok(()));
        assert_eq!(app.lua_log(), ["a:", "a:ze"]);
        turn(&mut app, "open:set(false)");
        turn(&mut app, "open:set(true)");
        assert_eq!(app.lua_log(), ["a:", "a:ze"], "closing arms nothing, not even `b`; reopening keeps the draft");
    }

    #[test]
    fn keyboard_interactivity_turning_on_never_takes_a_focused_field() {
        let (mut app, _dir) = launcher(r#"open:map(function(o) return o and "on_demand" or "none" end)"#);
        app.keyboard_focus = Some("notes@TEST".to_string());
        app.arm_autofocus_if_unfocused(&app.keyboard_focus_scope());
        turn(&mut app, "open:set(true)");
        assert_eq!(app.lua_log(), ["n:"], "the notes field keeps the keys");
        (app.keyboard_focus, app.focused_control) = (None, None); // `leave` keeps the draft typing
        turn(&mut app, "open:set(false)");
        turn(&mut app, "open:set(true)");
        assert_eq!(app.lua_log(), ["n:", "a:"], "a draft without the keyboard holds nothing");
    }

    #[test]
    fn an_armed_field_waiting_for_the_keyboard_unfocuses_when_interactivity_turns_off() {
        let (mut app, _dir) = launcher(r#"w:map(function(v) return v > 250 and "on_demand" or "none" end)"#);
        for w in [300, 310, 320] {
            turn(&mut app, &format!("w:set({w})"));
        }
        assert!(is_focused(&app, "b"));
        turn(&mut app, "w:set(200)");
        turn(&mut app, "w:set(210)");
        assert!(!is_focused(&app, "b"));
    }

    #[test]
    fn hiding_the_panel_ends_its_wait_for_the_keyboard() {
        let (mut app, _dir) = launcher(r#"open:map(function(o) return o and "on_demand" or "none" end)"#);
        for code in ["open:set(true)", "shown:set(false)", "open:set(false)", "shown:set(true)"] {
            turn(&mut app, code);
        }
        let steps = vec![InputStep::Move { x: 10.0, y: 10.0 }, InputStep::Press(InputButton::Left)];
        assert_eq!(app.inject_input(&shared::Inject { surface: "launcher@TEST".into(), steps }), Ok(()));
        assert!(is_focused(&app, "b"));
        turn(&mut app, "w:set(300)");
        assert!(!is_focused(&app, "b"), "a pressed field waits for the keyboard, as before");
    }

    #[test]
    fn typing_into_a_field_without_handlers_names_them() {
        let (mut app, _dir, _queue, _mock) = App::test_app_with_wire(
            r#"local function p(id, key, on) return panel { id = id, layer = "top", output = "TEST", width = 100,
                height = 40, keyboard_interactivity = "on_demand", child = column { children = {
                    textfield { width = 100, height = 20 }, textfield { width = 100, height = 20, on_submit = print, [key] = on } } } } end
            return { p("p", "visible", false), p("q", "disabled", true) }"#,
        );
        for id in ["p@TEST", "q@TEST"] {
            let refused = app.inject_text(id, "x").unwrap_err();
            assert!(refused.contains("neither `on_change` nor `on_submit`"), "{refused}");
        }
    }

    #[test]
    fn a_fixed_on_demand_panel_still_waits_for_the_keyboard_to_autofocus() {
        let (mut app, _dir) = launcher(r#""on_demand""#);
        turn(&mut app, "open:set(true)");
        assert!(app.focused_text_field.is_none(), "only `enter` or a click arms it, as before");
        assert!(app.inject_text("launcher@TEST", "x").unwrap_err().contains("press one first"));
        let steps = vec![InputStep::Move { x: 10.0, y: 30.0 }, InputStep::Press(InputButton::Left)];
        assert_eq!(app.inject_input(&shared::Inject { surface: "launcher@TEST".into(), steps }), Ok(()));
        assert!(is_focused(&app, "b"));
        turn(&mut app, "w:set(300)");
        assert!(!is_focused(&app, "b"), "a pressed field still waits for the keyboard");
    }
}

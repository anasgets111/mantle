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
        // Parsed up front so a bad combo delivers nothing.
        let combos = inject
            .steps
            .iter()
            .map(|step| match step {
                InputStep::Key(combo) => parse_combo(combo).map(Some),
                _ => Ok(None),
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (step, combo) in inject.steps.iter().zip(combos) {
            match (step, combo) {
                (InputStep::Move { x, y }, _) => self.inject_motion(index, (*x, *y)),
                (InputStep::Press(button), _) => self.inject_button(index, *button, true)?,
                (InputStep::Release(button), _) => self.inject_button(index, *button, false)?,
                (InputStep::Wheel { x, y, dy }, _) => {
                    self.inject_motion(index, (*x, *y));
                    let value120 = (dy * 120.0).round() as i32;
                    self.scroll_at(index, (*x, *y), 0.0, 0, 0.0, value120);
                }
                (InputStep::Key(_), Some((event, modifiers))) => self.inject_key(&surface_id, &event, modifiers)?,
                (InputStep::Text(text), _) => self.inject_text(&surface_id, text)?,
                (InputStep::Key(_), None) => unreachable!("every key step was parsed above"),
            }
        }
        Ok(())
    }

    fn inject_motion(&mut self, index: usize, position: (f64, f64)) {
        let here = self.pointer_at.as_ref().is_some_and(|(id, _)| *id == self.surfaces[index].surface_id);
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
        self.pointer_event(index, position, &kind, true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

//! Press bookkeeping: the armed click and popup serial, release matching, multi-click series and
//! which field a press focuses.

use super::*;

/// One press waiting for its release (ADR-0050 decision 2). The rect stands in for identity:
/// moving the node between press and release cancels the click.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::wayland) struct ArmedClick {
    pub(super) instance_id: String,
    pub(super) rect: LogicalRect,
    /// The `href` under the press inside `text` (ADR-0106), so a paragraph's two links do not share
    /// a click identity.
    pub(super) link: Option<String>,
    /// Evdev code, requiring release of the same button (ADR-0050 second amendment). ponytail: one
    /// armed click, so chording drops both; upgrade to an `ArrayVec` of three or small map for
    /// chords.
    pub(super) button: u32,
}

/// Serial for `xdg_popup.grab` and its surface (ADR-0049 amendment, ADR-0051 decision 1). Set by
/// [`PointerHandler::pointer_frame`], consumed by [`run`]'s poll loop after `dispatch_pending`:
/// resolving inside the callback would nest `Scene::apply` and Lua/Wayland creation while its
/// queue is being dispatched. Press and release both arm it, latest wins. `xdg_shell` only
/// requires a serial from "a real input event", so a stale real-input serial gets normal
/// `popup_done` refusal (ADR-0051 decision 3). Keep `instance_id`, not the tracked index, because
/// hotplug can rename indices before re-resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::wayland) struct ArmedSerial {
    pub(in crate::wayland) serial: u32,
    pub(in crate::wayland) instance_id: String,
}

/// Whether this release ends the armed press. Completion also needs surface, rect, and link; ending
/// needs only the button, so drag-off releases end it. A different release must not clear it: left,
/// right, left must retain the right press.
pub(super) fn release_ends_press(armed: Option<&ArmedClick>, button: u32) -> bool {
    armed.is_some_and(|armed| armed.button == button)
}

/// Whether a release completes `armed` (ADR-0050 decision 2). It must hit a clickable node with the
/// same instance, rect, link, and button; matching coordinates on a re-resolved plain rect is not
/// the original click. `released_on` comes from `target::click_target`, not raw position.
pub(super) fn release_completes_click(
    armed: Option<&ArmedClick>,
    instance_id: &str,
    released_on: Option<(LogicalRect, Option<&str>)>,
    button: u32,
) -> bool {
    armed.zip(released_on).is_some_and(|(armed, (rect, link))| {
        armed.instance_id == instance_id
            && armed.rect == rect
            && armed.link.as_deref() == link
            && armed.button == button
    })
}

/// A left press's time and position, which the next one may continue; for `on_double_click`, a
/// click's release.
pub(super) type Click = (std::time::Instant, (f64, f64));
// ponytail: fixed 400 ms and 4 px for field multi-clicks and `on_double_click`: Wayland has no
// portable double-click setting. Upgrade: a config knob, or the desktop's setting where one exists.
const MULTI_CLICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);
const MULTI_CLICK_SLOP: f64 = 4.0;

/// Whether `now` continues the series that `last` began.
pub(super) fn continues(last: Click, now: Click) -> bool {
    now.0.saturating_duration_since(last.0) <= MULTI_CLICK_INTERVAL
        && (last.1.0 - now.1.0).abs() <= MULTI_CLICK_SLOP
        && (last.1.1 - now.1.1).abs() <= MULTI_CLICK_SLOP
}

/// An `on_double_click` press or click: the node, not its rect, so a node its first click moves or
/// resizes still doubles and another that slides into its place does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::wayland) struct ClickedNode {
    pub(super) instance_id: String,
    pub(super) id: layout::scene::NodeId,
    pub(super) button: u32,
}

/// A click and its release time and position, which `on_double_click` may continue.
pub(in crate::wayland) type LastClick = (ClickedNode, (std::time::Instant, (f64, f64)));

/// Whether the click `now` completes a double click on the node and button of the previous click
/// `last`, and the click the next one may continue: none after a double, so a third starts over.
pub(super) fn double_click(last: Option<&LastClick>, now: LastClick) -> (bool, Option<LastClick>) {
    let double = last.is_some_and(|(click, at)| *click == now.0 && continues(*at, now.1));
    (double, (!double).then_some(now))
}

/// Which field a press focuses (ADR-0050 decision 4). Both halves are rewritten on every press:
/// reply and password fields must displace each other. `caret` is the byte offset under the press
/// point ([`layout::hit::caret_at`]), absent when nothing measured it; `extend` is Shift, which
/// selects from where the caret already was instead of collapsing to the press (ADR-0236). A
/// non-`primary` (non-left) press focuses but leaves a selection it lands in, so a menu can
/// act on it, and starts no drag. `click` is a left (`primary`) press and `None` the others; a held field it continues
/// selects the next unit (click, word, line, click), and Shift keeps the held unit instead.
pub(super) fn press_chooses_focus(
    hit_field: Option<FieldTarget>,
    instance_id: &str,
    caret: Option<usize>,
    extend: bool,
    click: Option<Click>,
    focused_secure_submit: Option<&FocusedField>,
    focused_text_field: Option<&FocusedTextField>,
) -> (Option<FocusedField>, Option<FocusedTextField>) {
    match hit_field {
        Some(FieldTarget::Masked { id, target }) => {
            (Some(FocusedField { surface_id: instance_id.to_string(), id, target }), None)
        }
        // Re-pressing the same field resumes its draft (ADR-0108).
        Some(FieldTarget::Plain { id, on_change, on_submit, on_cancel, escape }) => {
            let resumed = focused_text_field.filter(|field| field.id == id);
            let resumed_selection = resumed.as_ref().map(|field| field.selection);
            let primary = click.is_some();
            // `live` drops a unit that an edit or a keyboard selection has outdated.
            let held = resumed.as_ref().map(|field| (field.span.live(&field.buffer, field.selection), field.click));
            let (buffer, mut history) =
                resumed.map(|field| (field.buffer.clone(), field.history.clone())).unwrap_or_default();
            history.break_typing();
            // Where the press landed; the end of the draft when nothing measured it (ADR-0236).
            let caret = caret.unwrap_or(buffer.len()).min(buffer.len());
            let span = match (held, click) {
                (Some((held, _)), _) if extend => held,
                (held, click) => {
                    let unit = match (held, click) {
                        (Some((held, Some(last))), Some(now)) if continues(last, now) => held.unit.next(),
                        _ => Unit::Char,
                    };
                    Span { unit, base: unit_range(&buffer, caret, unit), produced: (0, 0) }
                }
            };
            let selection = match resumed_selection {
                Some((a, b)) if !primary && (a.min(b)..=a.max(b)).contains(&caret) => (a, b),
                _ => extend_by_unit(&buffer, span, caret),
            };
            let span = Span { produced: selection, ..span };
            (
                None,
                Some(FocusedTextField {
                    surface_id: instance_id.to_string(),
                    id,
                    selection,
                    buffer,
                    history,
                    typing: true,
                    selecting: primary,
                    span,
                    click: click.filter(|_| !extend),
                    goal_x: None,
                    on_change,
                    on_submit,
                    on_cancel,
                    escape,
                }),
            )
        }
        // Elsewhere stops plain typing but keeps its draft (ADR-0108). Masked focus remains until
        // another field takes it (ADR-0114 decision 8), so scrim, card, and Authenticate clicks do
        // not discard a secret before `submit`.
        None => (
            focused_secure_submit.cloned(),
            focused_text_field.cloned().map(|field| FocusedTextField {
                typing: false,
                selecting: false,
                click: None,
                ..field
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::target::clickable;
    use super::*;
    use crate::layout::node::MoveTween;
    use crate::wayland::input::keyboard::tests::draft;
    use crate::wayland::input::tests::hit_node;

    fn left() -> Option<Click> {
        Some((std::time::Instant::now(), (0.0, 0.0)))
    }

    /// A plain field as [`focused_field`] hands one back: a callback is what makes it plain. Each
    /// handler names itself in the `fired` global, so landing in the wrong slot is visible.
    fn plain_field(lua: &Lua, id: u64) -> FieldTarget {
        let handler =
            |name: &'static str| Some(lua.create_function(move |lua, ()| lua.globals().set("fired", name)).unwrap());
        FieldTarget::Plain {
            id: layout::scene::NodeId::test(id),
            on_change: handler("change"),
            on_submit: handler("submit"),
            on_cancel: None,
            escape: Escape::Clear,
        }
    }

    /// One `secure_submit` destination, as the parsers hand it back.
    fn secure_target() -> node::SecureSubmitTarget {
        node::SecureSubmitTarget {
            capability: "session_lock".to_string(),
            action: "authenticate".to_string(),
            name: None,
        }
    }

    #[test]
    fn a_release_fires_only_over_the_same_surface_rect_and_button_the_press_armed() {
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let moved = LogicalRect { x: 11.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, link: None, button: BTN_LEFT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_LEFT));
        // Dragged off the node, then released: the release hits no clickable node at all.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", None, BTN_LEFT));
        // Dragged onto a different clickable node on the same surface.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((moved, None)), BTN_LEFT));
        // Same geometry, different surface -- two panels can resolve identical rects.
        assert!(!release_completes_click(Some(&armed), "notification_area@eDP-1", Some((rect, None)), BTN_LEFT));
        // A release with nothing armed (a press that hit no clickable node, or a `leave` in between).
        assert!(!release_completes_click(None, "bar@eDP-1", Some((rect, None)), BTN_LEFT));

        // Press right, release left, on the same node: two clicks interleaved, neither complete. A
        // mouse can hold more than one button down at a time, so this is a real sequence.
        let right = ArmedClick { button: BTN_RIGHT, ..armed };
        assert!(release_completes_click(Some(&right), "bar@eDP-1", Some((rect, None)), BTN_RIGHT));
        assert!(!release_completes_click(Some(&right), "bar@eDP-1", Some((rect, None)), BTN_LEFT));
        assert!(!release_completes_click(Some(&right), "bar@eDP-1", Some((rect, None)), BTN_MIDDLE));
    }

    #[test]
    fn a_release_ends_only_its_own_buttons_press() {
        // The bug this exists to stop: press left, press right, release left, release right, all
        // on one node. If the left release clears the slot, the right press is thrown away with
        // it and the right click never fires despite being a complete pair.
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, link: None, button: BTN_RIGHT };

        assert!(release_ends_press(Some(&armed), BTN_RIGHT));
        assert!(!release_ends_press(Some(&armed), BTN_LEFT));
        // Ending it does not depend on it having fired: dragging off the node and releasing the
        // same button is over too, and the slot has to go.
        assert!(!release_ends_press(None, BTN_RIGHT));
    }

    /// A paragraph holding two links is one rect (ADR-0106): pressing one and releasing over the
    /// other is not a click on either, and a release on the plain words is not a click on the link.
    #[test]
    fn a_link_click_completes_only_on_the_same_link() {
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 200.0, height: 40.0 };
        let armed = ArmedClick {
            instance_id: "bar@eDP-1".to_string(),
            rect,
            link: Some("https://a/".to_string()),
            button: BTN_LEFT,
        };
        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, Some("https://a/"))), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, Some("https://b/"))), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_LEFT));

        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let mut text = hit_node(&lua, "text", (40.0, 4.0, 100.0, 20.0), false);
        let properties = std::rc::Rc::make_mut(&mut text.properties);
        properties.insert("content", lua.load("return {{ text = 'go', href = 'https://a/' }}").eval().unwrap());
        properties.insert("on_link", Value::Function(lua.create_function(|_, _: String| Ok(())).unwrap()));
        text.paint = node::paint_style("text", &text.properties).unwrap();
        text.movement = Some(Box::new(MoveTween::test((-30.0, 0.0))));
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 160.0, 32.0), false);
        root.children.push(text);
        let painted = layout::hit::LogicalPoint { x: 15.0, y: 10.0 };
        let path = layout::hit::hit_path(&root, painted);
        assert_eq!(clickable(&path, painted, &shaping).and_then(|hit| hit.link), Some("https://a/".to_string()));
        assert_eq!(layout::hit::cursor_under(&path, painted, &shaping), cursor_icon::CursorIcon::Pointer);
        let old = layout::hit::LogicalPoint { x: 45.0, y: 10.0 };
        let path = layout::hit::hit_path(&root, old);
        assert!(clickable(&path, old, &shaping).is_none());
        assert_eq!(layout::hit::cursor_under(&path, old, &shaping), cursor_icon::CursorIcon::Default);
    }

    #[test]
    fn a_second_click_on_the_same_node_and_button_in_time_doubles_and_a_third_starts_over() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let node = |id, button| ClickedNode {
            instance_id: "bar@eDP-1".to_string(),
            id: layout::scene::NodeId::test(id),
            button,
        };
        let click = |node: ClickedNode, ms, x| (node, (t0 + Duration::from_millis(ms), (x, 10.0)));
        let (double, first) = double_click(None, click(node(7, BTN_LEFT), 0, 20.0));
        assert!(!double);
        let (double, after) = double_click(first.as_ref(), click(node(7, BTN_LEFT), 300, 23.0));
        assert!(double, "the node its first click moved or resized is still the same node");
        assert!(!double_click(after.as_ref(), click(node(7, BTN_LEFT), 500, 23.0)).0, "a third click starts over");
        assert!(!double_click(first.as_ref(), click(node(7, BTN_LEFT), 401, 20.0)).0, "too slow");
        assert!(!double_click(first.as_ref(), click(node(7, BTN_LEFT), 100, 25.0)).0, "too far");
        assert!(!double_click(first.as_ref(), click(node(7, BTN_RIGHT), 100, 20.0)).0, "another button");
        assert!(
            !double_click(first.as_ref(), click(node(8, BTN_LEFT), 100, 20.0)).0,
            "another node slid into its place"
        );
    }

    /// ADR-0108 decision 2: the draft outlives focus, so coming back to the same reply box has to
    /// find the half-typed sentence still in it.
    #[test]
    fn re_pressing_a_drafts_field_resumes_it_and_a_different_field_starts_empty() {
        let lua = Lua::new();
        let (masked, plain) = press_chooses_focus(
            Some(plain_field(&lua, 7)),
            "notification_area@eDP-1",
            Some(7),
            false,
            left(),
            None,
            Some(&draft(7, "half a sentence")),
        );
        let plain = plain.expect("the press focused the field it hit");
        assert_eq!(plain.buffer, "half a sentence");
        assert!(plain.typing, "the caret is back");
        assert_eq!(plain.selection, (7, 7), "the caret lands where the press did, not at the end");
        // `prune_text_field_focus` keys liveness off this, so the draft's stale surface would drop
        // what was typed.
        assert_eq!(plain.surface_id, "notification_area@eDP-1", "the press names the surface");
        plain.on_change.unwrap().call::<()>(()).unwrap();
        assert_eq!(lua.globals().get::<String>("fired").unwrap(), "change", "the handlers kept their slots");
        assert!(masked.is_none());

        // Two reply boxes on one surface: the neighbour's text must not appear in this one.
        let lua = Lua::new();
        let (_, plain) = press_chooses_focus(
            Some(plain_field(&lua, 8)),
            "notification_area@eDP-1",
            None,
            false,
            left(),
            None,
            Some(&draft(7, "half a sentence")),
        );
        let plain = plain.expect("the press focused the field it hit");
        assert_eq!(plain.buffer, "");
        assert!(plain.typing);
    }

    /// ADR-0236: Shift selects from where the caret was. `leave` clears `held.shift` so a Shift
    /// released under another client cannot make this happen to a press the user meant as plain.
    #[test]
    fn shift_pressing_a_held_field_selects_from_the_caret_it_already_had() {
        let lua = Lua::new();
        let held = draft(7, "half a sentence");
        let anchor = held.selection.0;
        let (_, plain) = press_chooses_focus(
            Some(plain_field(&lua, 7)),
            "notification_area@eDP-1",
            Some(4),
            true,
            left(),
            None,
            Some(&held),
        );
        let plain = plain.expect("the press focused the field it hit");
        assert_eq!(plain.selection, (anchor, 4), "the anchor stays put and the press moves the head");

        // Without Shift the same press collapses instead, which is the difference a stale
        // `held.shift` would erase.
        let (_, plain) = press_chooses_focus(
            Some(plain_field(&lua, 7)),
            "notification_area@eDP-1",
            Some(4),
            false,
            left(),
            None,
            Some(&draft(7, "half a sentence")),
        );
        assert_eq!(plain.expect("focused").selection, (4, 4));
    }

    #[test]
    fn pressing_away_from_every_field_stops_typing_but_keeps_the_draft() {
        let held = FocusedField {
            surface_id: "lock@eDP-1".to_string(),
            id: layout::scene::NodeId::test(7),
            target: secure_target(),
        };
        let (masked, plain) =
            press_chooses_focus(None, "bar@eDP-1", None, false, left(), Some(&held), Some(&draft(7, "kept")));
        let plain = plain.expect("the draft survives a press elsewhere");
        assert_eq!(plain.buffer, "kept");
        assert!(!plain.typing, "no caret without focus");
        // ADR-0114 decision 8: the scrim and the Authenticate button are both presses elsewhere.
        assert_eq!(masked, Some(held));
    }

    #[test]
    fn pressing_a_masked_field_takes_focus_from_the_plain_one() {
        let (masked, plain) = press_chooses_focus(
            Some(FieldTarget::Masked { id: layout::scene::NodeId::test(8), target: secure_target() }),
            "lock@eDP-1",
            None,
            false,
            left(),
            None,
            Some(&draft(7, "half a sentence")),
        );
        assert_eq!(
            masked,
            Some(FocusedField {
                surface_id: "lock@eDP-1".to_string(),
                id: layout::scene::NodeId::test(8),
                target: secure_target(),
            })
        );
        assert!(plain.is_none(), "one field holds the keys, and it is the password one");
    }

    #[test]
    fn a_right_press_keeps_a_selection_it_lands_in_and_starts_no_drag() {
        let lua = Lua::new();
        let held = || FocusedTextField { selection: (2, 9), ..draft(7, "half a sentence") };
        let press = |caret, primary: bool| {
            let (_, plain) = press_chooses_focus(
                Some(plain_field(&lua, 7)),
                "bar@eDP-1",
                Some(caret),
                false,
                if primary { left() } else { None },
                None,
                Some(&held()),
            );
            plain.expect("focused")
        };
        let inside = press(5, false);
        assert_eq!((inside.selection, inside.selecting), ((2, 9), false));
        let outside = press(12, false);
        assert_eq!((outside.selection, outside.selecting), ((12, 12), false), "outside it places the caret");
        let left = press(5, true);
        assert_eq!((left.selection, left.selecting), ((5, 5), true), "a left press still collapses and drags");
    }

    #[test]
    fn presses_continue_a_series_by_time_and_distance_and_a_right_press_or_blur_ends_it() {
        use std::time::{Duration, Instant};
        let (lua, t0) = (Lua::new(), Instant::now());
        let press = |held: Option<FocusedTextField>, id, at, extend, button: Option<Click>| {
            let click = button.map(|(_, p)| (t0 + Duration::from_millis(at), p));
            press_chooses_focus(Some(plain_field(&lua, id)), "bar@eDP-1", Some(5), extend, click, None, held.as_ref()).1
        };
        let at = |x| Some((t0, (x, 10.0)));
        let first = press(Some(draft(7, "one two three")), 7, 0, false, at(10.0)).unwrap();
        let second = press(Some(first.clone()), 7, 300, false, at(12.0)).unwrap();
        let third = press(Some(second.clone()), 7, 600, false, at(12.0)).unwrap();
        let fourth = press(Some(third.clone()), 7, 900, false, at(12.0)).unwrap();
        let units = [first.span.unit, second.span.unit, third.span.unit, fourth.span.unit];
        assert_eq!(units, [Unit::Char, Unit::Word, Unit::Line, Unit::Char]);
        assert_eq!((second.selection, third.selection), ((4, 7), (0, 13)));
        let unit =
            |held: &FocusedTextField, id, ms, x| press(Some(held.clone()), id, ms, false, at(x)).unwrap().span.unit;
        assert_eq!(unit(&first, 7, 401, 10.0), Unit::Char, "too slow");
        assert_eq!(unit(&first, 7, 100, 15.0), Unit::Char, "too far");
        assert_eq!(unit(&first, 8, 100, 10.0), Unit::Char, "another field starts its own series");
        let right = press(Some(second.clone()), 7, 100, false, None).unwrap();
        assert_eq!(unit(&right, 7, 150, 12.0), Unit::Char, "a right press ended the series");
        let blurred = press_chooses_focus(None, "bar@eDP-1", None, false, None, None, Some(&second)).1.unwrap();
        assert_eq!(unit(&blurred, 7, 100, 12.0), Unit::Char, "a press elsewhere ended it");
    }

    #[test]
    fn shift_press_keeps_the_unit_and_an_edit_or_select_all_drops_it() {
        let lua = Lua::new();
        let shift = |caret, held| {
            press_chooses_focus(Some(plain_field(&lua, 7)), "bar@eDP-1", Some(caret), true, left(), None, Some(&held))
                .1
                .expect("focused")
        };
        let word = FocusedTextField {
            selection: (4, 7),
            span: Span { unit: Unit::Word, base: (4, 7), produced: (4, 7) },
            ..draft(7, "one two three")
        };
        assert_eq!(shift(10, word.clone()).selection, (4, 13), "Shift grows by the held word");
        assert_eq!(shift(1, word.clone()).selection, (7, 0));
        let all = FocusedTextField { selection: (0, 13), ..word.clone() };
        assert_eq!(shift(10, all).selection, (0, 10), "after Ctrl+A the anchor is the live one, not the old word");
        // A first press on a not-yet-focused field has an empty buffer; its drag must still anchor at the live selection.
        let swapped = FocusedTextField { selection: (3, 3), ..draft(7, "one two three") };
        let span = swapped.span.live(&swapped.buffer, swapped.selection);
        assert_eq!(extend_by_unit(&swapped.buffer, span, 9), (3, 9));
    }
}

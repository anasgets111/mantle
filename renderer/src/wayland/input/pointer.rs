//! Pointer input: `on_click` (ADR-0050), `on_drag` and `on_wheel` (ADR-0116), links, hover and the
//! cursor.

use shared::warn;

use super::keyboard::{FieldTarget, focused_field};
use super::*;

/// Mouse-wheel notch size in logical pixels (ADR-0069 decision 6): flat 39, about three
/// lines of 13px text. A per-container step would need a font size. Touchpad pixels divide by it
/// only for `on_wheel` fractions.
const WHEEL_STEP_PIXELS: f32 = 39.0;

const VALUE120_PER_NOTCH: f32 = 120.0;

/// Scroll distance in logical pixels (ADR-0069 decision 6). Use touchpad `pixels` as sent; use
/// `value120` (120 per notch) only without pixels, or compositors sending both double the motion.
fn wheel_delta(pixels: f64, value120: i32) -> f32 {
    if pixels != 0.0 {
        return pixels as f32;
    }
    value120 as f32 / VALUE120_PER_NOTCH * WHEEL_STEP_PIXELS
}

/// `on_wheel`'s notches (ADR-0116 decision 2 amendment), positive away from the user. `value120`
/// wins: a wheel's pixels are not `WHEEL_STEP_PIXELS` per notch.
fn wheel_steps(pixels: f64, value120: i32) -> f32 {
    if value120 != 0 {
        return -(value120 as f32) / VALUE120_PER_NOTCH;
    }
    -(pixels as f32) / WHEEL_STEP_PIXELS
}

/// One press waiting for its release (ADR-0050 decision 2). The rect stands in for identity:
/// moving the button between press and release cancels the click.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::wayland) struct ArmedClick {
    instance_id: String,
    rect: LogicalRect,
    /// The `href` under the press inside `text` (ADR-0106), so a paragraph's two links do not share
    /// a click identity.
    link: Option<String>,
    /// Evdev code, requiring release of the same button (ADR-0050 second amendment). ponytail: one
    /// armed click, so chording drops both; upgrade to an `ArrayVec` of three or small map for
    /// chords.
    button: u32,
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

/// Innermost `button` with callable `on_click` in a hit path (ADR-0050 decision 1). Scan inward:
/// the deepest node is normally the button's `text` child. A button without a handler is
/// transparent, and only `Value::Function` counts; `layout::node` leaves the key opaque.
fn clickable_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(LogicalRect, Option<&'a Function>, bool)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let on_click = match node.properties.get("on_click") {
            Some(Value::Function(f)) => Some(f),
            _ => None,
        };
        let submit = matches!(node.properties.get("submit"), Some(Value::Boolean(true)));
        if on_click.is_none() && !submit {
            return None;
        }
        Some((layout::hit::absolute_rect(&path[..=depth])?, on_click, submit))
    })
}

/// Innermost `button` with callable `on_drag` (ADR-0116 decision 1); unhandled buttons are
/// transparent, so a handle inside a draggable track leaves the track draggable.
fn draggable_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(LogicalRect, &'a Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let Some(Value::Function(on_drag)) = node.properties.get("on_drag") else {
            return None;
        };
        Some((layout::hit::absolute_rect(&path[..=depth])?, on_drag))
    })
}

/// Innermost `button` with callable `on_wheel` (ADR-0116 decision 2).
fn wheel_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(usize, LogicalRect, &'a Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let Some(Value::Function(on_wheel)) = node.properties.get("on_wheel") else {
            return None;
        };
        Some((depth, layout::hit::absolute_rect(&path[..=depth])?, on_wheel))
    })
}

/// Left press on a button with `on_drag`, held until release (ADR-0116 decision 1). Every Motion
/// calls it wherever the pointer goes; config clamping keeps a slider pinned at its end.
pub(in crate::wayland) struct ActiveDrag {
    instance_id: String,
    rect: LogicalRect,
    handler: Function,
}

/// Press/release target: handler, click-identity rect (see [`ArmedClick`]), and link `href`.
#[derive(Clone)]
struct Clickable {
    rect: LogicalRect,
    handler: Option<Function>,
    link: Option<String>,
    /// `submit = true` sends the scope's armed `secure_submit` field on release, like Enter
    /// (ADR-0114); it is the only button path to a password, with no Lua callback.
    submit: bool,
}

/// Links precede buttons (ADR-0106): a text `on_link` with an `href` under `point` wins over an
/// ancestor button, while plain text is transparent. A `textfield` similarly arms no click
/// (ADR-0092 decision 7).
fn clickable(
    path: &[&layout::ResolvedNode],
    point: layout::hit::LogicalPoint,
    shaping: &ShapingHandle,
) -> Option<Clickable> {
    let link = path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "text" {
            return None;
        }
        let Some(Value::Function(on_link)) = node.properties.get("on_link") else {
            return None;
        };
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        let local = layout::hit::LogicalPoint { x: point.x - rect.x, y: point.y - rect.y };
        let href = layout::hit::link_under(node, local, shaping)?;
        Some(Clickable { rect, handler: Some(on_link.clone()), link: Some(href), submit: false })
    });
    link.or_else(|| {
        clickable_button(path).map(|(rect, on_click, submit)| Clickable {
            rect,
            handler: on_click.cloned(),
            link: None,
            submit,
        })
    })
}

/// Both targets from decision 1's single [`layout::hit::hit_path`] traversal. Two walks could
/// re-resolve between them and give one event two answers.
struct PointerHit {
    button: Option<Clickable>,
    field: Option<FieldTarget>,
    /// Byte offset under the point in the plain `textfield` it landed in (ADR-0236), decided on
    /// this same walk so one event cannot get two answers.
    caret: Option<usize>,
    /// `on_drag` button under the press, with its rect (ADR-0116 decision 1).
    drag: Option<(LogicalRect, Function)>,
}

/// Lua name for an evdev button, or `None` when this engine ignores it. Use strings, not raw 273 or
/// normalized 1/2/3, matching other categorical values. `None` means no arm or release: exposing
/// `BTN_TASK` as `"other"` would make it indistinguishable from `BTN_EXTRA` and could run a left
/// handler. ponytail: only left/right/middle of SCTK's eight names are handled. Upgrade:
/// `BTN_SIDE`/`BTN_EXTRA` (0x113/0x114), `BTN_BACK`/`BTN_FORWARD` (0x116/0x115).
fn pointer_button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some("left"),
        BTN_RIGHT => Some("right"),
        BTN_MIDDLE => Some("middle"),
        _ => None,
    }
}

/// Whether this release ends the armed press. Completion also needs surface, rect, and link; ending
/// needs only the button, so drag-off releases end it. A different release must not clear it: left,
/// right, left must retain the right press.
fn release_ends_press(armed: Option<&ArmedClick>, button: u32) -> bool {
    armed.is_some_and(|armed| armed.button == button)
}

/// Whether a release completes `armed` (ADR-0050 decision 2). It must hit a handled button with the
/// same instance, rect, link, and button; matching coordinates on a re-resolved plain rect is not
/// the original click. `released_on` comes from [`clickable_button`], not raw position.
fn release_completes_click(
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

/// Which field a press focuses (ADR-0050 decision 4). Both halves are rewritten on every press:
/// reply and password fields must displace each other. `caret` is the byte offset under the press
/// point ([`layout::hit::caret_at`]), absent when nothing measured it; `extend` is Shift, which
/// selects from where the caret already was instead of collapsing to the press (ADR-0236).
fn press_chooses_focus(
    hit_field: Option<FieldTarget>,
    instance_id: &str,
    caret: Option<usize>,
    extend: bool,
    focused_secure_submit: Option<FocusedField>,
    focused_text_field: Option<FocusedTextField>,
) -> (Option<FocusedField>, Option<FocusedTextField>) {
    match hit_field {
        Some(FieldTarget::Masked(target)) => (Some(FocusedField { surface_id: instance_id.to_string(), target }), None),
        // Re-pressing the same field resumes its draft (ADR-0108).
        Some(FieldTarget::Plain { id, on_change, on_submit, on_cancel, on_navigate }) => {
            let resumed = focused_text_field.filter(|field| field.id == id);
            let anchor = resumed.as_ref().map(|field| field.selection.0);
            let buffer = resumed.map(|field| field.buffer).unwrap_or_default();
            // Where the press landed; the end of the draft when nothing measured it (ADR-0236).
            let caret = caret.unwrap_or(buffer.len()).min(buffer.len());
            (
                None,
                Some(FocusedTextField {
                    surface_id: instance_id.to_string(),
                    id,
                    selection: (anchor.filter(|_| extend).unwrap_or(caret), caret),
                    buffer,
                    typing: true,
                    selecting: true,
                    on_change,
                    on_submit,
                    on_cancel,
                    on_navigate,
                }),
            )
        }
        // Elsewhere stops plain typing but keeps its draft (ADR-0108). Masked focus remains until
        // another field takes it (ADR-0114 decision 8), so scrim, card, and Authenticate clicks do
        // not discard a secret before `submit`.
        None => (
            focused_secure_submit,
            focused_text_field.map(|field| FocusedTextField { typing: false, selecting: false, ..field }),
        ),
    }
}

/// Call `on_click` with its button rect in surface logical coordinates (ADR-0050 decision 3). The
/// rect round-trips to popup `anchor_rect` through Lua. Error labels distinguish building the
/// engine's argument from a raised config handler.
fn call_on_click(
    lua: &Lua,
    on_click: &Function,
    rect: LogicalRect,
    button: &str,
) -> Result<(), (&'static str, mlua::Error)> {
    let argument = rect_table(lua, rect).map_err(|e| ("could not build on_click's rect argument", e))?;
    on_click.call::<()>((argument, button)).map_err(|e| ("on_click raised, ignoring it", e))
}

/// Call `on_drag` with the rect, pointer in button-local coordinates, and gesture phase (ADR-0116
/// decision 1). Local coordinates avoid repeated subtraction in handlers; unclamped coordinates
/// let each config apply its own `min`/`max`.
fn call_on_drag(
    lua: &Lua,
    on_drag: &Function,
    rect: LogicalRect,
    position: (f64, f64),
    phase: &str,
) -> Result<(), (&'static str, mlua::Error)> {
    let rect_argument = rect_table(lua, rect).map_err(|e| ("could not build on_drag's rect argument", e))?;
    let pointer = lua.create_table().map_err(|e| ("could not build on_drag's pointer argument", e))?;
    pointer.set("x", position.0 as f32 - rect.x).map_err(|e| ("could not build on_drag's pointer argument", e))?;
    pointer.set("y", position.1 as f32 - rect.y).map_err(|e| ("could not build on_drag's pointer argument", e))?;
    on_drag.call::<()>((rect_argument, pointer, phase)).map_err(|e| ("on_drag raised, ignoring it", e))
}

/// Build `on_click`'s button rect `{ x, y, width, height }` in surface logical coordinates
/// (ADR-0050 decision 3).
pub(in crate::wayland) fn rect_table(lua: &Lua, rect: LogicalRect) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("x", rect.x)?;
    table.set("y", rect.y)?;
    table.set("width", rect.width)?;
    table.set("height", rect.height)?;
    Ok(table)
}

/// Pointer input to `on_click` (ADR-0050); dispatch is delegated by `delegate_dispatch2!(App)`.
impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // `wl_pointer` is per seat; an event may name a surface destroyed by `visible` or
            // output change.
            let Some(index) = self.index_of_surface(&event.surface) else {
                continue;
            };
            match event.kind {
                // Left/right/middle only (ADR-0050 second amendment); other codes cannot arm or
                // fire a config handler.
                PointerEventKind::Press { button, serial, .. } => {
                    if pointer_button_name(button).is_none() {
                        continue;
                    }
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // ADR-0049 amendment: this turn's re-resolve consumes it; `run` clears it at
                    // turn end. Both click edges arm it (see [`ArmedSerial`]).
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    // ADR-0051 amendment: preserve the "user asked again" stamp past disarm.
                    self.pointer_input_count += 1;
                    let hit = self.hit_under(index, event.position);
                    // Read before the call consumes `hit.field`; a held draft is still a plain
                    // field (ADR-0108).
                    let pressed_a_field = hit.field.is_some();
                    // Clones, not takes: the seams below compare what arrives against the focus
                    // still held to decide whether to zeroize and what to repaint.
                    let (masked, plain) = press_chooses_focus(
                        hit.field,
                        &instance_id,
                        hit.caret,
                        self.shift_held,
                        self.focused_secure_submit.clone(),
                        self.focused_text_field.clone(),
                    );
                    // Reassign through the zeroizing transition seam.
                    self.focus_secure_submit(masked);
                    self.focus_text_field(plain);
                    // A textfield press arms no click, so an ancestor button cannot fire
                    // (ADR-0092); `textfield` is a leaf. This keeps notification reply boxes
                    // from also activating the card.
                    self.armed = hit.button.filter(|_| !pressed_a_field).map(|clickable| ArmedClick {
                        instance_id: instance_id.clone(),
                        rect: clickable.rect,
                        link: clickable.link,
                        button,
                    });
                    // Left `on_drag` holds until release (ADR-0116 decision 1); other buttons stay
                    // free for clicks, and fields drag nothing just as they click nothing.
                    if button == BTN_LEFT
                        && !pressed_a_field
                        && let Some((rect, handler)) = hit.drag
                    {
                        self.drag = Some(ActiveDrag { instance_id: instance_id.clone(), rect, handler });
                        self.fire_on_drag(&instance_id, event.position, "start");
                    }
                }
                PointerEventKind::Release { button, serial, .. } => {
                    let Some(name) = pointer_button_name(button) else {
                        continue;
                    };
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // Clicks fire on release (ADR-0050 decision 2), so this serial arms a popup
                    // opened by `on_click`.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    self.pointer_input_count += 1;
                    // The pointer is up, so motion stops growing a selection (ADR-0236).
                    if let Some(field) = self.focused_text_field.as_mut() {
                        field.selecting = false;
                    }
                    // Release does not change focus; drag-off must not un-focus a textfield. End
                    // drag before click so a combined control commits before its click handler.
                    if button == BTN_LEFT {
                        self.fire_on_drag(&instance_id, event.position, "end");
                    }
                    let hit = self.hit_under(index, event.position).button;
                    let fires = release_completes_click(
                        self.armed.as_ref(),
                        &instance_id,
                        hit.as_ref().map(|clickable| (clickable.rect, clickable.link.as_deref())),
                        button,
                    );
                    // Clear before callback re-entry; only the matching button ends the slot.
                    if release_ends_press(self.armed.as_ref(), button) {
                        self.armed = None;
                    }
                    if let Some(clickable) = hit.filter(|_| fires) {
                        match (clickable.link, clickable.handler) {
                            // Links take `href`, not the paragraph rect; a link is not a button.
                            (Some(href), Some(handler)) => {
                                if let Err(e) = handler.call::<()>(href) {
                                    warn!("{instance_id}: on_link raised, ignoring it: {e}");
                                }
                            }
                            (_, handler) => {
                                if clickable.submit {
                                    self.prune_secure_focus();
                                    self.finish_secure_submit();
                                }
                                if let Some(handler) = handler {
                                    self.fire_on_click(&instance_id, clickable.rect, name, &handler);
                                }
                            }
                        }
                    }
                }
                // Release will land elsewhere: cancel the armed click. `None` clears all hover
                // (ADR-0062), since no later Motion may arrive to close a tooltip.
                PointerEventKind::Leave { .. } => {
                    // End held drag at the last position; no release reaches this surface.
                    if let Some(&(_, position)) = self.pointer_at.as_ref() {
                        let instance_id = self.surfaces[index].surface_id.clone();
                        self.fire_on_drag(&instance_id, position, "end");
                    }
                    self.armed = None;
                    self.cursor_shown = None;
                    self.pointer_at = None;
                    let tree = self.client.scene().surface(&self.surfaces[index].surface_id);
                    self.sync_hover(index, tree, None, true);
                }
                // Motion off the armed rect does not disarm; return and release still click. Enter
                // and Motion update hover, but only Motion fires `on_hover` (ADR-0112 amendment):
                // an Enter over a surface opened under a resting pointer is layout movement, not a
                // user choice. Actual movement sends Motion shortly after.
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let moved = matches!(event.kind, PointerEventKind::Motion { .. });
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // Fires before the write: no `on_drag` handler can read `pointer_at`.
                    if moved {
                        self.fire_on_drag(&instance_id, event.position, "move");
                        self.drag_selection(index, &instance_id, event.position);
                    }
                    self.pointer_at = Some((instance_id, event.position));
                    // One lookup serves both; `Scene::surface` lends its tree.
                    let tree = self.client.scene().surface(&self.surfaces[index].surface_id);
                    self.sync_hover(index, tree, Some(event.position), moved);
                    // Chosen while the tree is still borrowed, shown once that borrow has ended.
                    let shape = self.cursor_for(tree, event.position);
                    self.show_cursor(shape);
                }
                // Wheel (ADR-0069); use the event's own position.
                PointerEventKind::Axis { horizontal, vertical, .. } => {
                    self.scroll_at(
                        index,
                        event.position,
                        horizontal.absolute,
                        horizontal.value120,
                        vertical.absolute,
                        vertical.value120,
                    );
                }
            }
        }
    }
}

impl App {
    /// One hit-test answers button, field, and drag (ADR-0050 decisions 1/4). Coordinates are
    /// logical surface-local while `paint_surface` remains scale `1.0`; a future HiDPI change must
    /// move this conversion with `paint_surface` and `apply_input_region`. Handlers and targets are
    /// cloned out of the lent tree; the tree itself is not.
    fn hit_under(&self, index: usize, position: (f64, f64)) -> PointerHit {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else {
            return PointerHit { button: None, field: None, caret: None, drag: None };
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(tree, point);
        let field = focused_field(&path);
        // `None`, not `""`, for a field this does not hold: an empty draft measures to offset 0,
        // and handing that to `drag_selection` moves the held field's head into another field's
        // text. Only the draft `press_chooses_focus` would resume has a caret (ADR-0108).
        let held = self
            .focused_text_field
            .as_ref()
            .filter(|held| matches!(&field, Some(FieldTarget::Plain { id, .. }) if *id == held.id));
        PointerHit {
            button: clickable(&path, point, &self.shaping),
            // Its own caret too, not just its draft: a draft too wide to fit is drawn slid to
            // follow the caret, and a press reads the byte under where it actually landed.
            caret: held
                .and_then(|held| layout::hit::caret_at(&path, point, &held.buffer, held.selection.1, &self.shaping)),
            field,
            drag: draggable_button(&path).map(|(rect, handler)| (rect, handler.clone())),
        }
    }

    /// Grow the focused field's selection to the pointer, keeping its anchor: a press inside a
    /// plain field that has since moved (ADR-0236). Off its own surface the drag does nothing,
    /// rather than reading a caret out of some other field the pointer crossed.
    fn drag_selection(&mut self, index: usize, instance_id: &str, position: (f64, f64)) {
        let selecting =
            self.focused_text_field.as_ref().is_some_and(|field| field.selecting && field.surface_id == instance_id);
        let Some(caret) = selecting.then(|| self.hit_under(index, position).caret).flatten() else {
            return;
        };
        if let Some(field) = self.focused_text_field.as_mut().filter(|field| field.selection.1 != caret) {
            field.selection.1 = caret;
            self.field_input_changed = true;
        }
    }

    /// Fire one held drag edge for `instance_id` (ADR-0116 decision 1); `end` clears it before the
    /// callback so re-entry finds nothing held. Handler raises are logged and swallowed.
    fn fire_on_drag(&mut self, instance_id: &str, position: (f64, f64), phase: &str) {
        let Some(drag) = self.drag.as_ref().filter(|drag| drag.instance_id == instance_id) else {
            return;
        };
        let (rect, handler) = (drag.rect, drag.handler.clone());
        if phase == "end" {
            self.drag = None;
        }
        if let Err((what, e)) = call_on_drag(self.client.lua(), &handler, rect, position, phase) {
            warn!("{instance_id}: {what}: {e}");
        }
    }

    /// Apply one wheel event to the deepest scrollable or `on_wheel` button (ADR-0116 decision 2).
    /// Use touchpad pixels or `value120` (ADR-0069 decision 6); ignore deprecated `discrete`,
    /// which compositors that still send also accompany with `value120`.
    /// Innermost wins with no parent chaining: a wheel over a list stops there at its end, unlike
    /// browser chaining to the parent; no config needs those edge cases. The offset written is
    /// unclamped: `layout::scene` owns the bound and writes back what it used.
    /// `on_wheel` receives the vertical axis only.
    fn scroll_at(
        &mut self,
        index: usize,
        position: (f64, f64),
        horizontal_px: f64,
        horizontal_steps: i32,
        vertical_px: f64,
        vertical_steps: i32,
    ) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(tree) = self.client.scene().surface(&surface_id) else {
            return;
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(tree, point);
        // Deepest scrollable under the pointer wins.
        let scrollable = path.iter().enumerate().rev().find_map(|(depth, node)| {
            let signal = layout::scene::scroll_signal(&node.properties)?;
            let axis = layout::scene::main_axis_of(node.kind, &node.properties).ok()??;
            Some((depth, signal, axis))
        });
        let wheel = wheel_button(&path);
        if let Some((depth, rect, on_wheel)) = wheel
            && scrollable.as_ref().is_none_or(|(scroll_depth, ..)| depth > *scroll_depth)
        {
            let steps = wheel_steps(vertical_px, vertical_steps);
            if steps == 0.0 {
                return;
            }
            let on_wheel = on_wheel.clone();
            drop(path);
            match rect_table(self.client.lua(), rect) {
                Ok(rect) => {
                    if let Err(e) = on_wheel.call::<()>((rect, steps)) {
                        warn!("{surface_id}: on_wheel raised, ignoring it: {e}");
                    }
                }
                Err(e) => warn!("{surface_id}: could not build on_wheel's rect argument: {e}"),
            }
            return;
        }
        let Some((_, signal, axis)) = scrollable else {
            return;
        };
        let (pixels, steps) = match axis {
            layout::scene::MainAxis::Horizontal => (horizontal_px, horizontal_steps),
            layout::scene::MainAxis::Vertical => (vertical_px, vertical_steps),
        };
        let delta = wheel_delta(pixels, steps);
        if delta == 0.0 {
            return;
        }
        let Some(handle) = signal.scroll_handle() else {
            return;
        };
        let current = signal.scroll_offset().unwrap_or(0.0);
        handle.set_changed(mlua::Value::Number(f64::from(current + delta)));
    }

    /// The cursor [`layout::hit::cursor_under`] chooses under `position` (ADR-0107). Split from
    /// [`Self::show_cursor`] so the choosing ends its borrow of the lent tree before the showing
    /// writes through `&mut self`.
    fn cursor_for(&self, tree: Option<&layout::ResolvedNode>, position: (f64, f64)) -> cursor_icon::CursorIcon {
        let Some(tree) = tree else {
            return cursor_icon::CursorIcon::Default;
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        layout::hit::cursor_under(&layout::hit::hit_path(tree, point), point, &self.shaping)
    }

    /// Set the cursor only when it changes; motion arrives per pixel and each `set_shape` would add
    /// compositor work. `Leave` clears the cache because the shape is bound to the next enter
    /// serial.
    fn show_cursor(&mut self, shape: cursor_icon::CursorIcon) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        if self.cursor_shown == Some(shape) {
            return;
        }
        match pointer.set_cursor(&self.conn, shape) {
            Ok(()) => self.cursor_shown = Some(shape),
            // Remember failure as shown; a missing themed cursor is reported once, not per pixel.
            Err(err) => {
                warn!("could not set the cursor to {}: {err}", shape.name());
                self.cursor_shown = Some(shape);
            }
        }
    }

    /// Re-run hover writes after layout moves rows under a stationary pointer, without `on_hover`
    /// (ADR-0112 amendment): the row that slid away stops reading hovered and the row now under
    /// the pointer starts, or stale tint follows the old row off the viewport. No user crossing
    /// occurred.
    pub(in crate::wayland) fn refresh_hover_after_layout(&mut self) {
        let Some((surface_id, position)) = &self.pointer_at else {
            return;
        };
        let Some(index) = self.surfaces.iter().position(|tracked| &tracked.surface_id == surface_id) else {
            return;
        };
        let tree = self.client.scene().surface(surface_id);
        self.sync_hover(index, tree, Some(*position), false);
    }

    /// The pointer half of `App::drop_role_object`'s scrub, for the one leave the compositor never
    /// sends.
    ///
    /// Every teardown destroys its role object from this side (ADR-0088), so no `wl_pointer`
    /// leave follows and the `Leave` arm below never runs: `pointer_at` keeps naming a surface
    /// that is gone, every hover signal inside it stays true, and `on_hover(false)` is never
    /// called. Closing the notification history with the pointer over its list left
    /// `hold_expiry(300)` standing, pausing every countdown until ADR-0094's deadline released it
    /// on its own, and the panel reopened pre-tinted.
    ///
    /// Same work as `Leave`, because the surface is equally gone: end a held drag at its last
    /// position, drop the armed click, and write every hover off with `on_hover` firing.
    pub(in crate::wayland) fn pointer_left_destroyed_surface(&mut self, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(&(_, position)) = self.pointer_at.as_ref().filter(|(at, _)| *at == surface_id) else {
            return;
        };
        self.fire_on_drag(&surface_id, position, "end");
        self.armed = None;
        self.cursor_shown = None;
        self.pointer_at = None;
        let tree = self.client.scene().surface(&surface_id);
        self.sync_hover(index, tree, None, true);
    }

    /// Write all `hover` signals, or clear them for `None` (ADR-0062). Collect writes before
    /// `set_changed` because the tree borrow must end; only moved values dirty the scene (decision
    /// 4), so a stationary pointer inside one button re-resolves nothing while device-rate motion
    /// continues (ADR-0044 decision 2). `fire` enables `on_hover` only for motion/leave, not enter-
    /// motion or re-layout.
    ///
    /// Takes the tree rather than fetching it so one lookup serves this and the cursor.
    ///
    fn sync_hover(&self, index: usize, tree: Option<&layout::ResolvedNode>, position: Option<(f64, f64)>, fire: bool) {
        // Skip the expensive tree walk when no config registered `hover(name)`.
        if !crate::lua::signal::any_hover_registered(self.client.lua()) {
            return;
        }
        let Some(tree) = tree else {
            return;
        };
        let point = position.map(|(x, y)| layout::hit::LogicalPoint { x: x as f32, y: y as f32 });
        let writes = layout::hover::hover_writes(tree, point);
        let lua = self.client.lua();
        for write in writes {
            // Non-hover signals stay untouched, so `hover = mantle.network` cannot overwrite a
            // capability snapshot (ADR-0062 decision 2).
            let Some(handle) = write.signal.hover_handle() else {
                continue;
            };
            // The boolean gates rect and callback writes.
            let crossed = handle.set_changed(mlua::Value::Boolean(write.hovered));
            // Fire only on edges (ADR-0095): device-rate motion could call a handler hundreds of
            // times across one button. Swallow handler errors like `fire_on_click`.
            if crossed
                && fire
                && let Some(on_hover) = &write.on_hover
                && let Err(err) = on_hover.call::<()>(write.hovered)
            {
                warn!("{}: on_hover handler raised: {err}", self.surfaces[index].surface_id);
            }
            // Rects are edge-only, not merely an optimization: mlua table equality is identity, so
            // a fresh equal table would undo decision 4 on every motion.
            if crossed
                && let Some(rect) = write.rect
                && let Some(rect_handle) = write.signal.hover_rect_handle()
            {
                match rect_table(lua, rect) {
                    Ok(table) => rect_handle.set(mlua::Value::Table(table)),
                    // The boolean landed; keep the last tooltip position on table-build failure.
                    Err(err) => warn!("{}: could not build a hover rect: {err}", self.surfaces[index].surface_id),
                }
            }
        }
    }

    /// Call a button's `on_click` with its rect (ADR-0050 decision 3); swallow handler raises.
    /// ADR-0046 rescue is for failed evaluation, not a misbehaving callback.
    fn fire_on_click(&mut self, instance_id: &str, rect: LogicalRect, button: &str, on_click: &Function) {
        // `signal:set()` marks its own dirty flag (ADR-0044 decision 5); this call need not.
        if let Err((what, e)) = call_on_click(self.client.lua(), on_click, rect, button) {
            warn!("{instance_id}: {what}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::hit_node;
    use super::*;

    /// ADR-0069 decision 6. The rest of `scroll_at` needs a compositor to deliver a notch;
    /// this is the half that does not.
    #[test]
    fn a_touchpads_pixels_are_used_as_sent_and_a_wheels_steps_are_converted() {
        assert_eq!(wheel_delta(17.5, 0), 17.5, "a touchpad reports a distance and it is taken");
        assert_eq!(wheel_delta(-17.5, 0), -17.5, "including upward");
        assert_eq!(wheel_delta(0.0, 120), WHEEL_STEP_PIXELS, "one notch is one step");
        assert_eq!(wheel_delta(0.0, -240), -2.0 * WHEEL_STEP_PIXELS, "two notches up");
        assert_eq!(wheel_delta(0.0, 60), WHEEL_STEP_PIXELS / 2.0, "high-resolution wheels send fractions of a notch");
    }

    /// A compositor that sends both is sending the same motion twice, so the distance wins and the
    /// step count is not added on top of it.
    #[test]
    fn a_step_count_is_ignored_when_a_distance_came_with_it() {
        assert_eq!(wheel_delta(17.5, 120), 17.5);
    }

    #[test]
    fn wheel_steps_are_notches_positive_away_from_the_user() {
        assert_eq!(wheel_steps(0.0, -120), 1.0, "one notch up is +1");
        assert_eq!(wheel_steps(0.0, 240), -2.0, "two notches down are -2");
        assert_eq!(wheel_steps(-WHEEL_STEP_PIXELS as f64 / 2.0, 0), 0.5, "a touchpad swipe is a fraction of a notch");
        assert_eq!(wheel_steps(0.0, 0), 0.0);
        assert_eq!(wheel_steps(15.0, 120), -1.0, "value120 wins over pixels sent with it");
    }

    #[test]
    fn the_innermost_on_drag_button_is_the_one_that_takes_the_drag_and_a_bare_button_is_transparent() {
        let lua = Lua::new();
        let inner = hit_node(&lua, "button", (5.0, 2.0, 20.0, 20.0), true);
        let mut track = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), false);
        track.properties.insert("on_drag", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        track.children.push(inner);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(track);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let (rect, _) = draggable_button(&path).expect("the track carries the on_drag");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
        assert!(wheel_button(&path).is_none(), "nothing on this path declares on_wheel");
    }

    #[test]
    fn on_drag_is_handed_the_pointer_in_the_buttons_own_coordinates() {
        let lua = Lua::new();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&seen);
        let handler = lua
            .create_function(move |_, (rect, pointer, phase): (Table, Table, String)| {
                sink.borrow_mut().push((
                    rect.get::<f32>("width").unwrap(),
                    pointer.get::<f32>("x").unwrap(),
                    pointer.get::<f32>("y").unwrap(),
                    phase,
                ));
                Ok(())
            })
            .unwrap();
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        call_on_drag(&lua, &handler, rect, (30.0, 10.0), "start").unwrap();
        // Past the right edge: unclamped, so the config's own clamp is what pins the slider.
        call_on_drag(&lua, &handler, rect, (60.0, 10.0), "end").unwrap();
        assert_eq!(*seen.borrow(), vec![(40.0, 20.0, 6.0, "start".to_string()), (40.0, 50.0, 6.0, "end".to_string())]);
    }

    #[test]
    fn a_wheel_event_carrying_no_motion_scrolls_nothing() {
        assert_eq!(wheel_delta(0.0, 0), 0.0);
    }

    /// The `value120` that would have been silently dropped by narrowing it to an `i16` first.
    ///
    /// Reachable rather than absurd: `AxisScroll::merge` sums `value120` across every axis event
    /// queued before the next `Frame` (`ret.value120 += other.value120`, smithay-client-toolkit
    /// 0.21.1), so one frame of dispatch lag behind a fast wheel accumulates past `i16::MAX`. The
    /// old narrowing turned that into a delta of zero, which returns early without writing the
    /// signal at all -- a hard flick scrolling nothing, at exactly the moment the client was
    /// already behind.
    #[test]
    fn an_implausibly_large_step_count_still_scrolls_rather_than_becoming_zero() {
        assert!(wheel_delta(0.0, 120 * 400) > 0.0);
    }

    #[test]
    fn the_innermost_handled_button_under_the_pointer_is_the_one_that_would_fire() {
        // The shape decision 1 exists for: the deepest node is the `text`, and the outer `row`
        // is not a button, so only the middle node answers.
        let lua = Lua::new();
        let mut button = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), true);
        button.children.push(hit_node(&lua, "text", (6.0, 5.0, 28.0, 14.0), false));
        let mut row = hit_node(&lua, "row", (0.0, 0.0, 100.0, 32.0), false);
        row.children.push(button);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(row);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let (rect, ..) = clickable_button(&path).expect("the button carries an on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
    }

    #[test]
    fn a_button_with_no_on_click_is_transparent_rather_than_a_barrier() {
        // An unhandled `button` nested inside a handled one must not swallow the click: the scan
        // keeps walking outwards past it.
        let lua = Lua::new();
        let inner = hit_node(&lua, "button", (5.0, 2.0, 20.0, 20.0), false);
        let mut outer = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), true);
        outer.children.push(inner);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(outer);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        assert_eq!(path.len(), 3, "the inner button is still on the path");
        let (rect, ..) = clickable_button(&path).expect("the outer button carries the on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
    }

    #[test]
    fn an_on_click_that_is_not_a_function_is_not_a_click_handler() {
        // Nothing in `layout::node` parses this key (it stays opaque), so a config writing
        // `on_click = "quit"` reaches here as a string and must simply not fire.
        let lua = Lua::new();
        let mut button = hit_node(&lua, "button", (0.0, 0.0, 40.0, 24.0), false);
        button.properties.insert("on_click", Value::String(lua.create_string("quit").unwrap()));
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(button);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        assert!(clickable_button(&path).is_none());
    }

    #[test]
    fn a_release_fires_only_over_the_same_surface_and_the_same_rect_the_press_armed() {
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let moved = LogicalRect { x: 11.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, link: None, button: BTN_LEFT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_LEFT));
        // Dragged off the button, then released: the release hits no button at all.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", None, BTN_LEFT));
        // Dragged onto a different button on the same surface.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((moved, None)), BTN_LEFT));
        // Same button geometry, different surface -- two panels can resolve identical rects.
        assert!(!release_completes_click(Some(&armed), "notification_area@eDP-1", Some((rect, None)), BTN_LEFT));
        // A release with nothing armed (a press that hit no button, or a `leave` in between).
        assert!(!release_completes_click(None, "bar@eDP-1", Some((rect, None)), BTN_LEFT));
    }

    #[test]
    fn only_the_three_buttons_a_config_can_name_are_handled_at_all() {
        assert_eq!(pointer_button_name(BTN_LEFT), Some("left"));
        assert_eq!(pointer_button_name(BTN_RIGHT), Some("right"));
        assert_eq!(pointer_button_name(BTN_MIDDLE), Some("middle"));
        // A side button, a browser-back button, and a code off the end of the mouse range: each
        // answers `None`, since a config cannot tell them apart.
        assert_eq!(pointer_button_name(0x113), None);
        assert_eq!(pointer_button_name(0x116), None);
        assert_eq!(pointer_button_name(0), None);
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

    #[test]
    fn a_release_fires_only_for_the_button_the_press_armed() {
        // Press right, release left, on the same node: two different clicks interleaved, and
        // neither completed. A mouse can hold more than one button down at a time, so this is a
        // real sequence rather than a hypothetical one.
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, link: None, button: BTN_RIGHT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_RIGHT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some((rect, None)), BTN_MIDDLE));
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
    }

    #[test]
    fn on_clicks_argument_is_the_buttons_rect_as_four_named_fields() {
        let lua = Lua::new();
        let table = rect_table(&lua, LogicalRect { x: 10.5, y: 4.0, width: 40.0, height: 24.0 }).unwrap();
        assert_eq!(table.get::<f32>("x").unwrap(), 10.5);
        assert_eq!(table.get::<f32>("y").unwrap(), 4.0);
        assert_eq!(table.get::<f32>("width").unwrap(), 40.0);
        assert_eq!(table.get::<f32>("height").unwrap(), 24.0);
    }

    #[test]
    fn on_click_takes_the_button_name_as_a_second_argument_beside_the_rect() {
        // Second argument, not a fifth field on the rect table: every handler written against the
        // one-argument form keeps working, since Lua drops arguments a function does not declare.
        let lua = Lua::new();
        let seen: Function = lua
            .load(r#"seen = {} return function(rect, button) seen.x, seen.w, seen.button = rect.x, rect.width, button end"#)
            .eval()
            .unwrap();
        call_on_click(&lua, &seen, LogicalRect { x: 12.0, y: 4.0, width: 40.0, height: 24.0 }, "right").unwrap();

        let recorded: Table = lua.globals().get("seen").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 12.0);
        assert_eq!(recorded.get::<f32>("w").unwrap(), 40.0);
        assert_eq!(recorded.get::<String>("button").unwrap(), "right");
    }

    #[test]
    fn a_one_argument_on_click_still_runs_unchanged() {
        // ADR-0050 decision 3's exact worked example, which every config in the tree uses.
        let lua = Lua::new();
        let anchor: Function = lua.load(r#"anchor = nil return function(rect) anchor = rect end"#).eval().unwrap();
        call_on_click(&lua, &anchor, LogicalRect { x: 40.0, y: 0.0, width: 86.0, height: 24.0 }, "left").unwrap();

        let recorded: Table = lua.globals().get("anchor").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 40.0);
        assert_eq!(recorded.get::<f32>("height").unwrap(), 24.0);
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
            on_navigate: None,
        }
    }

    /// One `secure_submit` destination, as the parsers hand it back.
    fn secure_target() -> node::SecureSubmitTarget {
        node::SecureSubmitTarget { capability: "session_lock".to_string(), action: "authenticate".to_string() }
    }

    /// A draft as [`App::focus_text_field`] holds one between presses.
    fn draft(id: u64, buffer: &str) -> FocusedTextField {
        FocusedTextField {
            surface_id: "calendar@eDP-1".to_string(),
            id: layout::scene::NodeId::test(id),
            selection: (buffer.len(), buffer.len()),
            buffer: buffer.to_string(),
            typing: false,
            selecting: false,
            on_change: None,
            on_submit: None,
            on_cancel: None,
            on_navigate: None,
        }
    }

    /// ADR-0108 decision 2: the draft outlives focus, so coming back to the same reply box has to
    /// find the half-typed sentence still in it.
    #[test]
    fn re_pressing_the_field_a_draft_belongs_to_resumes_it_rather_than_starting_empty() {
        let lua = Lua::new();
        let (masked, plain) = press_chooses_focus(
            Some(plain_field(&lua, 7)),
            "notification_area@eDP-1",
            Some(7),
            false,
            None,
            Some(draft(7, "half a sentence")),
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
    }

    #[test]
    fn pressing_a_different_plain_field_starts_its_own_empty_draft() {
        // Two reply boxes on one surface: the neighbour's text must not appear in this one.
        let lua = Lua::new();
        let (_, plain) = press_chooses_focus(
            Some(plain_field(&lua, 8)),
            "notification_area@eDP-1",
            None,
            false,
            None,
            Some(draft(7, "half a sentence")),
        );
        let plain = plain.expect("the press focused the field it hit");
        assert_eq!(plain.buffer, "");
        assert!(plain.typing);
    }

    /// ADR-0236: Shift selects from where the caret was. `leave` clears `shift_held` so a Shift
    /// released under another client cannot make this happen to a press the user meant as plain.
    #[test]
    fn shift_pressing_a_held_field_selects_from_the_caret_it_already_had() {
        let lua = Lua::new();
        let held = draft(7, "half a sentence");
        let anchor = held.selection.0;
        let (_, plain) =
            press_chooses_focus(Some(plain_field(&lua, 7)), "notification_area@eDP-1", Some(4), true, None, Some(held));
        let plain = plain.expect("the press focused the field it hit");
        assert_eq!(plain.selection, (anchor, 4), "the anchor stays put and the press moves the head");

        // Without Shift the same press collapses instead, which is the difference a stale
        // `shift_held` would erase.
        let (_, plain) = press_chooses_focus(
            Some(plain_field(&lua, 7)),
            "notification_area@eDP-1",
            Some(4),
            false,
            None,
            Some(draft(7, "half a sentence")),
        );
        assert_eq!(plain.expect("focused").selection, (4, 4));
    }

    #[test]
    fn pressing_away_from_every_field_stops_typing_but_keeps_the_draft() {
        let held = FocusedField { surface_id: "lock@eDP-1".to_string(), target: secure_target() };
        let (masked, plain) =
            press_chooses_focus(None, "bar@eDP-1", None, false, Some(held.clone()), Some(draft(7, "kept")));
        let plain = plain.expect("the draft survives a press elsewhere");
        assert_eq!(plain.buffer, "kept");
        assert!(!plain.typing, "no caret without focus");
        // ADR-0114 decision 8: the scrim and the Authenticate button are both presses elsewhere.
        assert_eq!(masked, Some(held));
    }

    #[test]
    fn pressing_a_masked_field_takes_focus_from_the_plain_one() {
        let (masked, plain) = press_chooses_focus(
            Some(FieldTarget::Masked(secure_target())),
            "lock@eDP-1",
            None,
            false,
            None,
            Some(draft(7, "half a sentence")),
        );
        assert_eq!(masked, Some(FocusedField { surface_id: "lock@eDP-1".to_string(), target: secure_target() }));
        assert!(plain.is_none(), "one field holds the keys, and it is the password one");
    }
}

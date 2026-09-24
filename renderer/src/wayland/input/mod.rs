//! Seat input for `App`: the `wl_seat` capabilities here, pointer input in `pointer`, and keyboard
//! focus with `secure_submit` typing in `keyboard`.

use shared::{debug, error};

use super::*;
mod keyboard;
pub(crate) use keyboard::NavigateKey;
pub(crate) use pointer::{DragPhase, MouseButton};
mod pointer;

pub(super) use keyboard::{FocusedField, FocusedTextField};

#[cfg(test)]
pub(crate) use pointer::apply_hover_write;
#[cfg(test)]
pub(super) use pointer::rect_table;
pub(super) use pointer::{ActiveDrag, ArmedClick, ArmedSerial};

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    /// Pointer and keyboard only; there is no touch property. `is_none` guards are required:
    /// `wl_seat::capabilities` restates the full set: gaining a keyboard re-announces the pointer,
    /// and duplicate SCTK objects would duplicate events into one armed/focus state.
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            // Themed pointer (ADR-0107): SCTK uses `wp_cursor_shape_v1` when available, otherwise
            // XCursor via `wl_shm`; its cursor surface dies with the pointer.
            Capability::Pointer if self.pointer.is_none() => {
                let cursor_surface = self.compositor_state.create_surface(qh);
                match self.seat_state.get_pointer_with_theme::<Self, ()>(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    cursor_surface,
                    ThemeSpec::default(),
                ) {
                    Ok(pointer) => {
                        debug!("pointer capability acquired");
                        self.pointer = Some(pointer);
                    }
                    // Nonfatal: painting, reload, and keyboard input remain; only `on_click` stops.
                    Err(e) => {
                        error!("wl_seat::get_pointer failed; no button's on_click will ever fire: {e}")
                    }
                }
            }
            // Use the compositor keymap (`None` rmlvo); there is no `on_key` for this shell to
            // interpret, so imposing a layout would serve no policy.
            Capability::Keyboard if self.keyboard.is_none() => match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => {
                    debug!("keyboard capability acquired");
                    self.keyboard = Some(keyboard);
                }
                // Nonfatal, but `enter`/`leave` stop tracking focus and stale textfield focus may
                // outlive the user.
                Err(e) => error!("wl_seat::get_keyboard failed; keyboard focus will never be tracked: {e}"),
            },
            _ => {}
        }
    }

    /// ponytail: `App` holds one pointer and one keyboard, not a map per seat, so `new_capability`
    /// is first-seat-wins. Ceiling: one seat. Upgrade by keying `pointer`, `keyboard` and
    /// `keyboard_focus` on the `wl_seat`.
    ///
    /// Removal is not on that ceiling: it answers only for the seat that owns the object, because
    /// nothing gives input back. A second seat losing a capability, or departing and reaching here
    /// through [`Self::remove_seat`], would otherwise take the working seat's pointer and keyboard
    /// with it, and `new_capability`'s guards refuse a replacement the live seat never re-announces.
    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer_seat().as_ref() == Some(&seat) => {
                // No pointer means no release; clear the press like `leave` (ADR-0050 decision 2).
                self.armed = None;
                self.cursor_shown = None;
                // A held drag has no other end: `fire_on_drag(.., "move")` asks for no button, so
                // a replacement pointer's first Motion would keep dragging.
                self.drag = None;
                self.pointer_at = None;
                // `ThemedPointer::drop` releases `wl_pointer` (`since="3"`), shape device, and
                // cursor surface (src/seat/pointer/mod.rs:567).
                self.pointer = None;
            }
            Capability::Keyboard if self.keyboard_seat().as_ref() == Some(&seat) => {
                // No keyboard means no leave; clear stale focus and its half-typed secret
                // (ADR-0050 decision 4).
                self.keyboard_focus = None;
                self.focus_secure_submit(None);
                if let Some(keyboard) = self.keyboard.take() {
                    // `wl_keyboard::release` is `since="3"` too (wayland.xml).
                    if keyboard.version() >= 3 {
                        keyboard.release();
                    }
                }
            }
            _ => {}
        }
    }

    /// SCTK's `remove_global` calls this and never [`Self::remove_capability`], so a kept
    /// `ThemedPointer` or `wl_keyboard` would make `new_capability`'s `is_none` guards refuse the
    /// replacement seat's, leaving input dead and a focused field holding a half-typed secret.
    fn remove_seat(&mut self, conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.remove_capability(conn, qh, seat.clone(), Capability::Pointer);
        self.remove_capability(conn, qh, seat, Capability::Keyboard);
    }
}

/// GTK's `gtk-cursor-blink`, `-time` (a whole cycle, in ms) and `-timeout` (in s), with GTK's own
/// defaults, as (half a cycle, blink duration). `None` when blinking is off.
/// ponytail: read once, so a changed setting lands on the next Renderer start.
pub(in crate::wayland) fn caret_blink() -> Option<(std::time::Duration, std::time::Duration)> {
    let setting = |key| crate::image::icons::gtk_setting(key);
    if setting("gtk-cursor-blink").is_some_and(|on| matches!(on.as_str(), "false" | "0")) {
        return None;
    }
    let cycle = setting("gtk-cursor-blink-time").and_then(|ms| ms.parse().ok()).unwrap_or(1200u64);
    let timeout = setting("gtk-cursor-blink-timeout").and_then(|s| s.parse().ok()).unwrap_or(10u64);
    (cycle >= 2 && timeout > 0)
        .then(|| (std::time::Duration::from_millis(cycle / 2), std::time::Duration::from_secs(timeout)))
}

impl App {
    fn pointer_seat(&self) -> Option<wl_seat::WlSeat> {
        Some(self.pointer.as_ref()?.pointer().data::<PointerData<()>>()?.seat().clone())
    }

    fn keyboard_seat(&self) -> Option<wl_seat::WlSeat> {
        Some(self.keyboard.as_ref()?.data::<KeyboardData<App, ()>>()?.seat().clone())
    }

    pub(in crate::wayland) fn mark_field_input_changed(&mut self, surface_id: &str) {
        self.caret_epoch = std::time::Instant::now();
        self.caret_painted_on = true;
        if !self.field_input_surfaces.iter().any(|s| s == surface_id) {
            self.field_input_surfaces.push(surface_id.to_string());
        }
    }

    pub(in crate::wayland) fn mark_focused_text_field_changed(&mut self) {
        if let Some(id) = self.focused_text_field.as_ref().map(|f| f.surface_id.clone()) {
            self.mark_field_input_changed(&id);
        }
    }

    pub(in crate::wayland) fn mark_focused_secure_submit_changed(&mut self) {
        if let Some(id) = self.focused_secure_submit.as_ref().map(|f| f.surface_id.clone()) {
            self.mark_field_input_changed(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::PropMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Hands each hand-built `ResolvedNode` its own id. A `Scene` allocates these in production and
    /// these tests have no `Scene`; the only property that matters is that two nodes are never
    /// accidentally the same node.
    static NEXT_TEST_NODE_ID: AtomicU64 = AtomicU64::new(1);

    pub(super) fn hit_node(
        lua: &Lua,
        kind: &'static str,
        (x, y, width, height): (f32, f32, f32, f32),
        on_click: bool,
    ) -> layout::ResolvedNode {
        let mut properties = PropMap::default();
        if on_click {
            properties.insert("on_click", Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        }
        layout::ResolvedNode {
            // Distinct per node: `focused_field` reads an identity off one of these, and a shared id
            // would make every hand-built field the same field.
            id: layout::scene::NodeId::test(NEXT_TEST_NODE_ID.fetch_add(1, Ordering::Relaxed)),
            paint: node::paint_style(kind, &properties).unwrap(),
            properties,
            ..layout::ResolvedNode::test(kind, (x, y, width, height), Vec::new())
        }
    }
}

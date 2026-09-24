//! `popup` (`xdg_popup`): positioners, reposition, nested teardown and the ADR-0049/0051 dismissal
//! latch.

use shared::{debug, error, warn};

use super::*;
use crate::wayland::surface::MapState;
use crate::wayland::surface::Placement;
use crate::wayland::surface::PopupParent;
use crate::wayland::surface::PopupRefusal;
use crate::wayland::surface::TrackedRole;

/// One `popup` visibility decision from object existence and the ADR-0051 latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupAction {
    Create,
    Destroy,
    Nothing,
}
/// ADR-0051 decision 2's pure latch machine for `visible`. `dismissed_at` is the pointer
/// count at dismissal; it holds while that count is unchanged (first amendment), since the `visible
/// = false` edge is otherwise unobservable and a bool would latch forever. The caller clears it on
/// that edge.
///
/// `visible=true` with no object creates unless the count is latched; a moved count permits a fresh
/// click. `visible=false` destroys an object and otherwise does nothing. An existing object stays,
/// and this no-op runs for every surface on every capability push.
fn popup_visibility_action(visible: bool, exists: bool, dismissed_at: Option<u64>, pointer_input: u64) -> PopupAction {
    let latched = dismissed_at == Some(pointer_input);
    match (visible, exists) {
        (true, false) if !latched => PopupAction::Create,
        (false, true) => PopupAction::Destroy,
        _ => PopupAction::Nothing,
    }
}
/// Parent instance for a popup (ADR-0051 decision 1). A declared id can expand to one panel per
/// output (ADR-0038 decision 3), so use the arming click's instance. ponytail: without an armed
/// click, such as a `grab = false` D-Bus popup, use the first parent instance. Upgrade: add popup
/// `monitor` and a third selector argument.
fn parent_instance_index<'a>(
    instance_ids: impl Iterator<Item = &'a str>,
    parent: &str,
    armed: Option<&str>,
) -> Option<usize> {
    let mut first = None;
    for (index, instance_id) in instance_ids.enumerate() {
        if !is_instance_of(instance_id, parent) {
            continue;
        }
        if armed == Some(instance_id) {
            return Some(index);
        }
        first.get_or_insert(index);
    }
    first
}
/// Popup buffer size. Positive configure axes are authoritative because the compositor may slide,
/// flip, or resize for positioner constraints. Non-positive axes use the requested size: SCTK's
/// `PopupInner` starts pending dimensions at `-1`, which would crash `WlEglSurface::new`; clamp to
/// at least 1.
///
/// `requested` is what the positioner was given, not what the spec says: a `Content` axis has no
/// number in the spec at all (`surface::popup_requested_size`).
fn popup_size_for(configured: (i32, i32), requested: (f32, f32)) -> (u32, u32) {
    let axis = |configured: i32, requested: f32| -> u32 {
        if configured > 0 {
            return configured as u32;
        }
        (requested.max(1.0)) as u32
    };
    (axis(configured.0, requested.0), axis(configured.1, requested.1))
}
/// `anchor` to `xdg_positioner`; `Center` is protocol `none`, centered in the anchor rectangle.
fn positioner_anchor(anchor: PopupAnchor) -> xdg_positioner::Anchor {
    match anchor {
        PopupAnchor::Center => xdg_positioner::Anchor::None,
        PopupAnchor::Top => xdg_positioner::Anchor::Top,
        PopupAnchor::Bottom => xdg_positioner::Anchor::Bottom,
        PopupAnchor::Left => xdg_positioner::Anchor::Left,
        PopupAnchor::Right => xdg_positioner::Anchor::Right,
        PopupAnchor::TopLeft => xdg_positioner::Anchor::TopLeft,
        PopupAnchor::TopRight => xdg_positioner::Anchor::TopRight,
        PopupAnchor::BottomLeft => xdg_positioner::Anchor::BottomLeft,
        PopupAnchor::BottomRight => xdg_positioner::Anchor::BottomRight,
    }
}
/// `gravity` to its separate but identical protocol enum; `Center` again maps to `none`,
/// which centers the surface over the anchor point on axes without specified gravity.
fn positioner_gravity(gravity: PopupAnchor) -> xdg_positioner::Gravity {
    match gravity {
        PopupAnchor::Center => xdg_positioner::Gravity::None,
        PopupAnchor::Top => xdg_positioner::Gravity::Top,
        PopupAnchor::Bottom => xdg_positioner::Gravity::Bottom,
        PopupAnchor::Left => xdg_positioner::Gravity::Left,
        PopupAnchor::Right => xdg_positioner::Gravity::Right,
        PopupAnchor::TopLeft => xdg_positioner::Gravity::TopLeft,
        PopupAnchor::TopRight => xdg_positioner::Gravity::TopRight,
        PopupAnchor::BottomLeft => xdg_positioner::Gravity::BottomLeft,
        PopupAnchor::BottomRight => xdg_positioner::Gravity::BottomRight,
    }
}
/// Six independent `constraint_adjustment` booleans to the protocol bitmask; precedence is
/// compositor-defined, so config array order carries no meaning.
fn positioner_constraint(adjustment: ConstraintAdjustment) -> xdg_positioner::ConstraintAdjustment {
    let mut bits = xdg_positioner::ConstraintAdjustment::None;
    bits.set(xdg_positioner::ConstraintAdjustment::SlideX, adjustment.slide_x);
    bits.set(xdg_positioner::ConstraintAdjustment::SlideY, adjustment.slide_y);
    bits.set(xdg_positioner::ConstraintAdjustment::FlipX, adjustment.flip_x);
    bits.set(xdg_positioner::ConstraintAdjustment::FlipY, adjustment.flip_y);
    bits.set(xdg_positioner::ConstraintAdjustment::ResizeX, adjustment.resize_x);
    bits.set(xdg_positioner::ConstraintAdjustment::ResizeY, adjustment.resize_y);
    bits
}
/// Sends all positioner fields in protocol order, from the one value that is also what a live
/// popup remembers being given ([`Placement`]), so a field cannot be sent without being compared.
/// An open popup's positioner is replaced through `xdg_popup.reposition`; a fresh one gets its at
/// `get_popup`, which consumes it.
///
/// Logical pixels round to `i32` (`x=996.6` becomes 997) because the anchor came from `on_click`
/// (ADR-0050 decision 3). The size `ceil`s instead: an anchor rect names a point and a size names
/// room, and a card measuring 252.48 given 252 loses the half pixel to the surface's own edge,
/// which is the clipping content sizing exists to end. Both clamp to 1, since `set_size` raises
/// `invalid_input` on a zero.
fn configure_positioner(positioner: &XdgPositioner, placement: &Placement) {
    let round = |n: f32| n.round() as i32;
    positioner.set_size((placement.size.0.ceil() as i32).max(1), (placement.size.1.ceil() as i32).max(1));
    positioner.set_anchor_rect(
        round(placement.anchor_rect.x),
        round(placement.anchor_rect.y),
        round(placement.anchor_rect.width).max(1),
        round(placement.anchor_rect.height).max(1),
    );
    positioner.set_anchor(positioner_anchor(placement.anchor));
    positioner.set_gravity(positioner_gravity(placement.gravity));
    positioner.set_constraint_adjustment(positioner_constraint(placement.constraint_adjustment));
    positioner.set_offset(round(placement.offset.x), round(placement.offset.y));
}

/// `xdg_popup.reposition` arrived in xdg-shell version 3. SCTK's `Popup::reposition` silently does
/// nothing below it, which would leave a popup quietly the wrong size, so ask first and say so.
const REPOSITION_SINCE: u32 = 3;

impl App {
    /// [`App::create_surfaces`]'s `popup` arm: always track it, but create `xdg_popup` only when
    /// visible (ADR-0049/0051). Twenty declared popups cost twenty retained nodes and zero objects.
    /// A startup-visible default-grab popup is refused and logged once because no click supplied a
    /// serial; an undismissable dropdown is worse than a closed one (ADR-0049 amendment). This is
    /// expected and does not fail startup.
    pub(in crate::wayland) fn create_popup(
        &mut self,
        qh: &QueueHandle<App>,
        spec: &PopupSpec,
        instance: &SurfaceInstance,
        visible: bool,
    ) {
        self.surfaces.push(TrackedSurface::new(
            TrackedRole::Popup {
                popup: None,
                // The declaration's own numbers, which for a `Content` axis is zero until the first
                // resolve measures one. Nothing opens before then; `apply_resolved_state` writes
                // the real pair on every pass.
                requested: crate::wayland::surface::popup_requested_size(spec, LogicalRect::default()),
                // Nothing is open, so no positioner has been given anything yet.
                positioned: None,
                spec: spec.clone(),
                dismissed_at: None,
                refusal_logged: None,
            },
            instance.instance_id.clone(),
        ));
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_popup(qh, index);
        }
    }

    /// Apply [`popup_visibility_action`] (ADR-0049 decision 2, ADR-0051 decision 2). Clear the
    /// latch on every `visible = false`, even if the object is already gone: `on_dismiss` may write
    /// false in the same turn and must reopen without waiting for pointer input.
    pub(in crate::wayland) fn apply_popup_visibility(&mut self, index: usize, visible: bool) {
        let TrackedRole::Popup { popup, dismissed_at, spec, requested, positioned, .. } = &self.surfaces[index].role
        else {
            return;
        };
        // An open popup whose placement has moved since its positioner was given one, so an open
        // popup follows its size instead of keeping the one it opened at.
        let moved = (visible && popup.is_some())
            .then(|| Placement { size: *requested, ..Placement::of(spec, LogicalRect::default()) })
            .filter(|placement| placement.is_measured() && Some(*placement) != *positioned);
        let action = popup_visibility_action(visible, popup.is_some(), *dismissed_at, self.pointer_input_count);
        if !visible && let TrackedRole::Popup { dismissed_at, refusal_logged, .. } = &mut self.surfaces[index].role {
            *dismissed_at = None;
            *refusal_logged = None;
        }
        match action {
            PopupAction::Create => {
                let qh = self.queue_handle.clone();
                self.show_popup(&qh, index);
            }
            PopupAction::Destroy => self.hide_popup(index),
            PopupAction::Nothing => {
                if let Some(placement) = moved {
                    self.reposition_popup(index, placement);
                }
            }
        }
    }

    /// Creates positioner and popup in protocol order (ADR-0040 decision 2, ADR-0049
    /// decisions 1-2, ADR-0051 decisions 1 and 3). `get_popup` consumes every positioner field.
    /// Use [`Popup::from_surface`], not `Popup::new`: the latter commits before a layer parent is
    /// rooted and causes `invalid_popup_parent`. Request grabs before mapping or get
    /// `invalid_grab`.
    /// SCTK's `Dispatch2<XdgSurface, _>` acks `xdg_surface.configure` before
    /// [`PopupHandler::configure`] (`shell/xdg/popup.rs`), so nothing here acks. The grab is the
    /// one request not wrapped by SCTK: `Popup::xdg_popup()` is the raw-object escape hatch for
    /// `wp-text-input-v3` established by ADR-0009, and is used here only for grab.
    /// An unarmed `grab = true` is refused, producing normal immediate `popup_done` rather than an
    /// undismissable popup (ADR-0049 amendment, ADR-0051 decision 3).
    fn show_popup(&mut self, qh: &QueueHandle<App>, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            error!("{surface_id}: this compositor advertises no xdg_wm_base, so no popup can be created for it");
            return;
        };
        let TrackedRole::Popup { spec, requested, .. } = &self.surfaces[index].role else {
            return;
        };
        let spec = spec.clone();
        let placement = Placement::of(&spec, LogicalRect::default());
        // `requested` is what `apply_resolved_state` measured; `Placement::of` above cannot know
        // it, so take the measured pair and keep the placement fields it did read.
        let placement = Placement { size: *requested, ..placement };

        // Nothing measured on a `Content` axis yet, so there is no size to ask for. Decline and
        // let the next pass open it, rather than inventing one the surface would then cut.
        if !placement.is_measured() {
            if self.refusal_is_new(index, PopupRefusal::Unmeasured) {
                warn!(
                    "{surface_id}: sized {:?}, so it is not opened yet. An omitted `width`/`height` \
                     is measured off the resolved tree, and this one has measured nothing on that axis. \
                     Logged once until it opens or `visible` resolves false.",
                    placement.size
                );
            }
            return;
        }

        // Refuse before creating protocol objects.
        let grab = if spec.grab {
            let Some(armed) = self.input_serial.clone() else {
                if self.refusal_is_new(index, PopupRefusal::Unarmed) {
                    warn!(
                        "{surface_id}: `grab = true` and no input event armed a serial this turn, so it is not opened. \
                         A popup may only be opened in response to real user input; open it from an `on_click`, or declare `grab = false`. \
                         Logged once until it opens or `visible` resolves false."
                    );
                }
                return;
            };
            let Some(seat) = self.seat_state.seats().next() else {
                if self.refusal_is_new(index, PopupRefusal::Seatless) {
                    warn!("{surface_id}: `grab = true` and this compositor advertises no seat, so it is not opened");
                }
                return;
            };
            Some((seat, armed))
        } else {
            None
        };

        // Parent selection applies with or without a grab; the arming click still decides it
        // (ADR-0051 decision 1).
        let parent_index = parent_instance_index(
            self.surfaces.iter().map(|tracked| tracked.surface_id.as_str()),
            &spec.parent,
            self.input_serial.as_ref().map(|armed| armed.instance_id.as_str()),
        );
        let Some(parent) = parent_index.and_then(|parent| self.surfaces[parent].role.as_popup_parent()) else {
            if self.refusal_is_new(index, PopupRefusal::HiddenParent) {
                warn!(
                    "{surface_id}: its `parent` {:?} names no surface that is currently shown, so it is not opened",
                    spec.parent
                );
            }
            return;
        };
        // Log the selected parent, observable mainly on multi-monitor sessions.
        let parent_id = parent_index.map_or("<none>", |parent| self.surfaces[parent].surface_id.as_str()).to_string();

        let positioner = match XdgPositioner::new(xdg_shell) {
            Ok(positioner) => positioner,
            Err(err) => {
                log_bind_failure(&surface_id, "xdg_wm_base::create_positioner", err);
                return;
            }
        };
        configure_positioner(&positioner, &placement);

        let surface = self.compositor_state.create_surface(qh);
        let rooted_at_creation = match &parent {
            PopupParent::Xdg(xdg_surface) => Some(xdg_surface),
            PopupParent::Layer(_) => None,
        };
        let popup = match Popup::from_surface(rooted_at_creation, &positioner, qh, surface, xdg_shell) {
            Ok(popup) => popup,
            Err(err) => {
                log_bind_failure(&surface_id, "xdg_surface::get_popup", err);
                return;
            }
        };
        if let PopupParent::Layer(layer) = &parent {
            // Layer-shell roots the raw popup before the commit, or `invalid_popup_parent`.
            layer.get_popup(popup.xdg_popup());
        }
        if let Some((seat, armed)) = &grab {
            popup.xdg_popup().grab(seat, armed.serial);
        }
        // Required initial unbuffered commit, after all rooting/grab requests.
        popup.wl_surface().commit();
        // `get_popup` copied the positioner, so dropping it is correct.

        if let TrackedRole::Popup { popup: slot, refusal_logged, .. } = &mut self.surfaces[index].role {
            *slot = Some(popup);
            *refusal_logged = None;
        }
        self.surfaces[index].map_state = MapState::AwaitingConfigure;
        // What this popup's positioner now holds. Every later pass compares against it.
        if let TrackedRole::Popup { positioned, .. } = &mut self.surfaces[index].role {
            *positioned = Some(placement);
        }
        debug!(
            "{surface_id} creating: visible = true, anchored to {parent_id}, grab {}",
            if grab.is_some() { "taken" } else { "not requested" }
        );
    }

    /// Give an open popup a new positioner (`xdg_popup.reposition`), which is the only way to
    /// change a size or a placement that `get_popup` already consumed.
    ///
    /// The token is ours to choose and comes back on the resulting `PopupConfigure` as
    /// `ConfigureKind::Reposition`; nothing here needs to correlate them, because the ordinary
    /// configure path already takes whatever size arrives and resizes the EGL window to it. It is
    /// sent anyway rather than left at zero so a compositor's own logs can pair request to answer.
    ///
    /// `positioned` moves forward on the request, not on the answer: it records what this popup's
    /// positioner was told, and a second identical request would be no more true for waiting. The
    /// configure that follows is what actually resizes anything.
    fn reposition_popup(&mut self, index: usize, placement: Placement) {
        let surface_id = self.surfaces[index].surface_id.clone();
        // Read the version out before anything wants `&mut self`; a `Popup` borrow of the role
        // would otherwise outlive the throttle check below.
        let TrackedRole::Popup { popup: Some(popup), .. } = &self.surfaces[index].role else {
            return;
        };
        let version = popup.xdg_popup().version();
        if version < REPOSITION_SINCE {
            if self.refusal_is_new(index, PopupRefusal::Unrepositionable) {
                warn!(
                    "{surface_id}: this compositor bound xdg_popup v{version}, and `reposition` needs \
                     v{REPOSITION_SINCE}, so it keeps the size and place it opened at until it closes. \
                     Logged once until it opens again or `visible` resolves false."
                );
            }
            return;
        }
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            return;
        };
        let positioner = match XdgPositioner::new(xdg_shell) {
            Ok(positioner) => positioner,
            Err(err) => {
                log_bind_failure(&surface_id, "xdg_wm_base::create_positioner", err);
                return;
            }
        };
        configure_positioner(&positioner, &placement);
        self.reposition_token = self.reposition_token.wrapping_add(1);
        let token = self.reposition_token;
        let was = if let TrackedRole::Popup { popup: Some(popup), positioned, .. } = &mut self.surfaces[index].role {
            popup.reposition(&positioner, token);
            positioned.replace(placement)
        } else {
            None
        };
        // A popup only repositions when its content or its anchor actually moved, and if that
        // starts happening on every pass this line is the evidence.
        debug!(
            "{surface_id} repositioned to {:?} from {:?} (token {token})",
            placement.size,
            was.map(|placement| placement.size)
        );
    }

    /// Destroys this popup and nested popups, children first because xdg-shell rejects parent-first
    /// teardown, leaving tracking entries so a later `visible = true` builds fresh objects. Latch
    /// children removed with the parent even without their own `popup_done`; their object is gone
    /// while `visible` remains true and their parent is absent. The latch clears on their
    /// `visible = false` edge (ADR-0051 decision 2).
    fn hide_popup(&mut self, index: usize) {
        for child in self.drop_child_popups(index) {
            self.latch_popup(child);
        }
        self.drop_role_object(index);
    }

    /// Returns shown descendants deepest-first and destroys their objects. Every surface teardown
    /// needs this because wlroots rejects a parent xdg-surface with live popups. Do not latch here:
    /// parent teardown is not compositor dismissal, and a child should return when its parent does,
    /// including parents reopened by D-Bus without pointer input. While absent, each re-resolve
    /// retries and emits one throttled [`PopupRefusal`].
    pub(in crate::wayland) fn drop_child_popups(&mut self, index: usize) -> Vec<usize> {
        let mut nested = Vec::new();
        self.shown_popups_under(index, &mut nested);
        for &child in &nested {
            self.drop_role_object(child);
        }
        nested
    }

    /// Whether this is a new refusal for the popup (ADR-0049 amendment). Different reasons each
    /// log, so fixing `grab` and then hitting a hidden parent is visible.
    fn refusal_is_new(&mut self, index: usize, refusal: PopupRefusal) -> bool {
        let TrackedRole::Popup { refusal_logged, .. } = &mut self.surfaces[index].role else {
            return false;
        };
        refusal_logged.replace(refusal) != Some(refusal)
    }

    /// Stamp ADR-0051 decision 2's latch with the current pointer count; it holds until that count
    /// advances, so dismissal followed by nothing stays latched and a later click reopens.
    fn latch_popup(&mut self, index: usize) {
        let stamp = self.pointer_input_count;
        if let TrackedRole::Popup { dismissed_at, .. } = &mut self.surfaces[index].role {
            *dismissed_at = Some(stamp);
        }
    }

    /// Append shown descendants deepest-first, matching [`App::hide_popup`]. Siblings stay in
    /// tracked order, which is safe because xdg-shell constrains a popup against its parent, not
    /// against siblings under one parent. A popup can hold an object only after its parent does, so
    /// parent cycles, including self-parenting, never open and cannot recurse through this walk.
    pub(in crate::wayland) fn shown_popups_under(&self, index: usize, out: &mut Vec<usize>) {
        let parent_id = self.surfaces[index].surface_id.as_str();
        for child in (0..self.surfaces.len()).filter(|&child| child != index).filter(|&child| {
            matches!(&self.surfaces[child].role, TrackedRole::Popup { popup: Some(_), spec, .. } if is_instance_of(parent_id, &spec.parent))
        }) {
            self.shown_popups_under(child, out);
            out.push(child);
        }
    }
}

/// `xdg_popup` handler for `popup`.
impl PopupHandler for App {
    /// SCTK has acked the configure. Ignore compositor placement because config has no
    /// binding for it. Only `Initial` is built; `Reactive` needs `set_reactive` and `Reposition`
    /// needs `xdg_popup.reposition`, while a fresh popup per open handles changing anchors.
    fn configure(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup, configure: PopupConfigure) {
        let Some(index) = self.index_of_surface(popup.wl_surface()) else {
            return;
        };
        let TrackedRole::Popup { requested, .. } = &self.surfaces[index].role else {
            return;
        };
        let (width, height) = popup_size_for((configure.width, configure.height), *requested);
        self.bind_and_clear(index, width, height);
    }

    /// `popup_done` is compositor dismissal, not a request. It is why ADR-0040 uses a real
    /// `xdg_popup` instead of a second `panel`: layer-shell has no compositor-agnostic
    /// click-outside dismissal. Then destroy children/object, latch ADR-0051 decision 2, and call
    /// `on_dismiss` against the already-gone popup. A denied grab arrives here as normal
    /// `popup_done` (ADR-0051 decision 3); clone callbacks and swallow raises. The latch, not this
    /// callback, prevents the re-resolve livelock.
    fn done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup) {
        let Some(index) = self.index_of_surface(popup.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        debug!("{surface_id}: dismissed by the compositor");
        self.hide_popup(index);
        self.latch_popup(index);

        let on_dismiss = self
            .client
            .scene()
            .surface(&surface_id)
            .and_then(|tree| crate::layout::node::fields::popup::on_dismiss.read(&tree.properties).ok().flatten());
        let Some(on_dismiss) = on_dismiss else {
            // No handler is fine; the latch still prevents a livelock.
            return;
        };
        if let Err(e) = on_dismiss.call::<()>(()) {
            warn!("{surface_id}: on_dismiss raised, ignoring it: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_visible_popup_with_no_object_is_created_unless_the_latch_is_set() {
        assert_eq!(popup_visibility_action(true, false, None, 4), PopupAction::Create);
        // ADR-0051 decision 2, and the one row the whole latch exists for: a compositor
        // dismissal leaves the resolved tree still saying `visible = true`, so without this the
        // next re-resolve creates a second popup for the same click-outside to dismiss, forever.
        assert_eq!(popup_visibility_action(true, false, Some(4), 4), PopupAction::Nothing);
    }

    #[test]
    fn a_dismissal_with_no_pointer_input_since_holds_the_latch_for_the_generations_life() {
        // The livelock ADR-0051 decision 2 exists to stop, in the config that has no
        // `on_dismiss` at all. Nothing new arrives, so the counter never moves and no re-resolve
        // ever creates a replacement -- not for one turn, but forever.
        for _ in 0..1000 {
            assert_eq!(popup_visibility_action(true, false, Some(9), 9), PopupAction::Nothing);
        }
    }

    #[test]
    fn a_click_arriving_after_the_dismissal_clears_the_latch_in_the_same_turn() {
        // ADR-0051's first amendment. Under a grab niri delivers the closing click to the
        // parent bar too, so `popup_done` and the button's `on_click` land in one batch and
        // `visible` alone cannot separate the two cases -- but `popup_done` dispatches before the
        // pointer events that follow it, so the counter has already moved.
        assert_eq!(popup_visibility_action(true, false, Some(9), 10), PopupAction::Create);
    }

    #[test]
    fn a_popup_that_is_already_open_is_left_alone_on_every_later_re_resolve() {
        // Not a degenerate case: ADR-0044 decision 2's dirty flag is one flag for the whole scene,
        // so `apply_resolved_state` runs for every surface on every capability push, and an open
        // popup passes through here several times a second.
        assert_eq!(popup_visibility_action(true, true, None, 4), PopupAction::Nothing);
    }

    #[test]
    fn visible_going_false_destroys_an_open_popup_and_asks_for_nothing_from_a_closed_one() {
        assert_eq!(popup_visibility_action(false, true, None, 4), PopupAction::Destroy);
        assert_eq!(popup_visibility_action(false, true, Some(4), 4), PopupAction::Destroy);
        // The row that reopens the path. Nothing is destroyed because the compositor already did
        // it; the caller clears the latch on this same edge, which is what lets an `on_dismiss`
        // writing `visible = false` make the popup openable again immediately.
        assert_eq!(popup_visibility_action(false, false, Some(4), 4), PopupAction::Nothing);
        assert_eq!(popup_visibility_action(false, false, None, 4), PopupAction::Nothing);
    }

    #[test]
    fn a_popup_anchors_to_the_parent_instance_the_arming_click_landed_on() {
        // ADR-0051 decision 1. Two monitors, one declared `bar`, and the click decides.
        let instances = ["bar@eDP-1", "bar@DP-1", "menu"];
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("bar@DP-1")), Some(1));
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("bar@eDP-1")), Some(0));
    }

    #[test]
    fn a_popup_with_nothing_armed_falls_back_to_the_first_instance_of_its_parent() {
        // The `grab = false` popup opened by a D-Bus notification: config has no way to say
        // which monitor it means (see `parent_instance_index`'s ponytail).
        let instances = ["bar@eDP-1", "bar@DP-1"];
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", None), Some(0));
    }

    #[test]
    fn a_click_on_some_other_surface_still_falls_back_to_the_first_parent_instance() {
        // A popup opened by a click on the *notification area* while naming `bar` as its parent.
        // The armed surface is not a candidate at all, so the fallback is the only answer left.
        let instances = ["bar@eDP-1", "bar@DP-1", "notification_area@DP-1"];
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("notification_area@DP-1")), Some(0));
    }

    #[test]
    fn a_popup_whose_parent_is_declared_nowhere_gets_no_index() {
        let instances = ["bar@eDP-1", "settings"];
        assert_eq!(parent_instance_index(instances.into_iter(), "launcher", Some("bar@eDP-1")), None);
    }

    #[test]
    fn a_popup_parents_to_a_window_by_its_bare_instance_id() {
        // A popup parents to either a `panel` or a `window`, and a window's instance carries
        // no `@output` because the compositor places it.
        let instances = ["bar@eDP-1", "settings"];
        assert_eq!(parent_instance_index(instances.into_iter(), "settings", None), Some(1));
    }

    #[test]
    fn a_popup_configure_is_taken_as_given_because_the_compositor_may_have_constrained_it() {
        // `constraint_adjustment` lets the compositor slide, flip or resize the popup to
        // keep it on screen, and the size it lands on is the one that has to be painted.
        assert_eq!(popup_size_for((180, 90), (200.0, 120.0)), (180, 90));
    }

    #[test]
    fn a_popup_configure_with_no_size_falls_back_to_what_the_positioner_asked_for() {
        // `PopupInner` seeds its pending dimensions at `-1` and reports whatever they hold when
        // `xdg_surface.configure` arrives; a `-1` reaching `WlEglSurface::new` is a crash and the
        // requested size is right there.
        assert_eq!(popup_size_for((-1, -1), (200.0, 120.0)), (200, 120));
        assert_eq!(popup_size_for((180, 0), (200.0, 120.0)), (180, 120), "per axis, not all or nothing");
    }

    #[test]
    fn a_popup_never_takes_a_zero_sized_buffer() {
        assert_eq!(popup_size_for((0, 0), (0.0, 0.0)), (1, 1), "a wl_egl_window of 0 is invalid");
    }

    #[test]
    fn every_popup_anchor_maps_to_its_protocol_anchor_and_gravity() {
        for (ours, anchor, gravity) in [
            (PopupAnchor::Top, xdg_positioner::Anchor::Top, xdg_positioner::Gravity::Top),
            (PopupAnchor::Bottom, xdg_positioner::Anchor::Bottom, xdg_positioner::Gravity::Bottom),
            (PopupAnchor::Left, xdg_positioner::Anchor::Left, xdg_positioner::Gravity::Left),
            (PopupAnchor::Right, xdg_positioner::Anchor::Right, xdg_positioner::Gravity::Right),
            (PopupAnchor::TopLeft, xdg_positioner::Anchor::TopLeft, xdg_positioner::Gravity::TopLeft),
            (PopupAnchor::TopRight, xdg_positioner::Anchor::TopRight, xdg_positioner::Gravity::TopRight),
            (PopupAnchor::BottomLeft, xdg_positioner::Anchor::BottomLeft, xdg_positioner::Gravity::BottomLeft),
            (PopupAnchor::BottomRight, xdg_positioner::Anchor::BottomRight, xdg_positioner::Gravity::BottomRight),
        ] {
            assert_eq!(positioner_anchor(ours), anchor);
            assert_eq!(positioner_gravity(ours), gravity);
        }
    }

    #[test]
    fn center_is_the_protocols_none_on_both_requests() {
        // The XML is what makes this a translation, not a fudge: with no edge specified the anchor
        // point is "in the center of the anchor rectangle", and a gravity of `none` centers the
        // surface "over the anchor point on any axis that had no gravity specified".
        assert_eq!(positioner_anchor(PopupAnchor::Center), xdg_positioner::Anchor::None);
        assert_eq!(positioner_gravity(PopupAnchor::Center), xdg_positioner::Gravity::None);
    }

    #[test]
    fn constraint_adjustment_booleans_map_to_the_matching_bitmask() {
        assert_eq!(
            positioner_constraint(ConstraintAdjustment::NONE),
            xdg_positioner::ConstraintAdjustment::None,
            "an explicitly empty array is the protocol's own no-adjustment"
        );
        assert_eq!(
            positioner_constraint(ConstraintAdjustment::default()),
            xdg_positioner::ConstraintAdjustment::FlipY | xdg_positioner::ConstraintAdjustment::SlideX,
            "the config default is dropdown behaviour, not the protocol's"
        );
        assert_eq!(
            positioner_constraint(ConstraintAdjustment {
                slide_x: true,
                slide_y: true,
                flip_x: true,
                flip_y: true,
                resize_x: true,
                resize_y: true,
            }),
            xdg_positioner::ConstraintAdjustment::SlideX
                | xdg_positioner::ConstraintAdjustment::SlideY
                | xdg_positioner::ConstraintAdjustment::FlipX
                | xdg_positioner::ConstraintAdjustment::FlipY
                | xdg_positioner::ConstraintAdjustment::ResizeX
                | xdg_positioner::ConstraintAdjustment::ResizeY
        );
    }
}

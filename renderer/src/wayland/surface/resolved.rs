//! Pushing a resolved root onto its surface: live role fields, input and blur regions, and
//! `visible` as create/destroy.

use super::*;

/// What `visible` owes a panel or window that is in `map_state`. Panels and windows share one
/// table; only the calls differ. Popups add ADR-0051's dismissal latch, so they keep
/// `xdg_shell::popup::popup_visibility_action`, and locks have no say at all.
///
/// Showing rebuilds from the spec because hiding destroyed the role object (ADR-0088). The two
/// `Nothing` cells are the steady states: already shown, or already gone.
fn visibility_action(map_state: MapState, visible: bool) -> VisibilityAction {
    match (map_state, visible) {
        (MapState::Unmapped, true) => VisibilityAction::Show,
        (MapState::AwaitingConfigure | MapState::Mapped, false) => VisibilityAction::Hide,
        _ => VisibilityAction::Nothing,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum VisibilityAction {
    Show,
    Hide,
    Nothing,
}

impl App {
    /// Apply resolved state to every tracked surface after a changed scene; ADR-0044 decision 2
    /// has one dirty flag for the whole scene.
    pub(in crate::wayland) fn apply_resolved_surface_state(&mut self) {
        for index in 0..self.surfaces.len() {
            self.apply_resolved_state(index);
        }
    }

    /// The same, for the instances a tween tick advanced.
    ///
    /// A tick re-derives only the trees it moved, saving a role spec parse per surface.
    pub(in crate::wayland) fn apply_resolved_surface_state_for(&mut self, named: &[&[String]]) {
        for index in 0..self.surfaces.len() {
            if named.iter().any(|ids| ids.contains(&self.surfaces[index].surface_id)) {
                self.apply_resolved_state(index);
            }
        }
    }

    /// The ADR-0051 latch, for the popups this turn's [`turn::StateScope`] did not reach; see
    /// [`turn::surface_state_for_turn`] for why a click owes this and the scene does not.
    ///
    /// Popups only, and visibility only. The role fields and the input region are what a tick has
    /// no business re-deriving; whether the compositor dismissed a popup the config still calls
    /// visible is not something the scene can say.
    pub(in crate::wayland) fn apply_popup_visibility_for_armed_input(&mut self) {
        for index in 0..self.surfaces.len() {
            if !matches!(self.surfaces[index].role, TrackedRole::Popup { .. }) {
                continue;
            }
            let surface_id = self.surfaces[index].surface_id.clone();
            let Some(visible) = self.client.scene().surface(&surface_id).map(|tree| tree.visible) else {
                continue;
            };
            self.apply_popup_visibility(index, visible);
        }
    }

    /// Push a resolved root's live protocol fields, input region, and visibility (ADR-0038 decision
    /// 2, ADR-0049 decisions 1-2). Window fields must come from the resolved `WindowSpec`: raw
    /// evaluation values would freeze signal-bound `title`s. The socket parser
    /// [`crate::lua::surfaces::surface_specs`] still reads unresolved properties, which is right for a
    /// panel's topology fields but wrong for a window's live fields. Push before
    /// `apply_visibility`, so a newly shown window uses this pass's spec; callers commit all staged
    /// state together, while create/destroy/map/unmap commit by definition.
    pub(super) fn apply_resolved_state(&mut self, index: usize) {
        self.surfaces[index].dirty = true;
        let surface_id = self.surfaces[index].surface_id.clone();
        // `Scene::surface` lends its tree, so what the tree is read for is taken here and the
        // borrow ends with this block; the role updates below write through `&mut self`. Exactly
        // one spec is parsed, the one this surface's role calls for.
        let (panel, window, popup, tree_rect, regions, visible, blur) = {
            let Some(tree) = self.client.scene().surface(&surface_id) else {
                // Startup/apply failure or rollback (`Scene::apply` restores its prior state): keep
                // the last applied fields rather than pushing defaults over a working surface.
                return;
            };
            let role = &self.surfaces[index].role;
            // Both regions are tree walks, and the pushes below discard them on exactly these two
            // conditions: no `wl_surface` to set them on, and for blur, no compositor support.
            let live = role.wl_surface().is_some();
            (
                matches!(role, TrackedRole::Panel { .. }).then(|| node::panel_spec(&tree.properties)),
                matches!(role, TrackedRole::Window { .. }).then(|| node::window_spec(&tree.properties)),
                matches!(role, TrackedRole::Popup { .. }).then(|| node::popup_spec(&tree.properties)),
                tree.rect,
                if live { layout::overlay_input_regions(tree, 1.0) } else { Vec::new() },
                tree.visible,
                if live && self.blur_supported { layout::blur_regions(tree, 1.0) } else { Vec::new() },
            )
        };

        match panel {
            // Store the measurement first and unconditionally. `apply_spec_change` returns early
            // on a hidden panel, whose `LayerSurface` is gone (ADR-0088), and that is exactly the
            // panel `show_panel` is about to rebuild from this number.
            Some(Ok(fresh)) => {
                if let TrackedRole::Panel { measured, .. } = &mut self.surfaces[index].role {
                    *measured = layout::LogicalSize { width: tree_rect.width, height: tree_rect.height };
                }
                // Re-derived every pass rather than fixed at creation: `width` and `height` are
                // live layer-shell fields (ADR-0038 decision 2), so a signal can move an axis
                // between a number and its content between one pass and the next.
                let output_size = match &self.surfaces[index].role {
                    TrackedRole::Panel { output_size, .. } => *output_size,
                    _ => layout::LogicalSize::default(),
                };
                let (axes, ceiling) = layer::measurement(&fresh, output_size);
                self.client.set_measured_axes(&surface_id, axes, ceiling);
                self.apply_spec_change(index, fresh, visible);
            }
            Some(Err(err)) => log_invalid_re_resolve(&surface_id, "panel", err),
            None => {}
        }
        match window {
            Some(Ok(fresh)) => self.apply_window_change(index, fresh),
            Some(Err(err)) => log_invalid_re_resolve(&surface_id, "window", err),
            None => {}
        }
        match popup {
            // Store, do not diff: `get_popup` consumes every positioner field and no
            // `xdg_popup.reposition` exists. The next open uses this pass's `anchor_rect`
            // (ADR-0049 amendment), including a click-written state signal.
            Some(Ok(fresh)) => {
                let size = popup_requested_size(&fresh, tree_rect);
                if let TrackedRole::Popup { spec, requested, .. } = &mut self.surfaces[index].role {
                    *spec = fresh;
                    *requested = size;
                }
            }
            Some(Err(err)) => log_invalid_re_resolve(&surface_id, "popup", err),
            None => {}
        }
        // Locks have no config-settable protocol field: only `ack_configure` exists and size
        // arrives in configure. The create path parses a spec only for the role match; input region
        // handling still runs.
        self.apply_input_region(index, regions);
        self.apply_blur_region(index, blur);
        self.apply_visibility(index, visible);
    }

    /// Set the per-surface input region from the resolved tree (ADR-0038 decision 5): no
    /// visible children means pass-through, a full child covers the surface, and intermediate
    /// content gets its visible geometry. Scale is `1.0` because no buffer scale is set. An
    /// unchanged region is not resent: a transform tween re-derives it every frame (ADR-0261).
    /// Skip a hidden window with no `wl_surface`; its first post-show re-resolve sets the region.
    fn apply_input_region(&mut self, index: usize, regions: Vec<crate::text::snap::PhysicalRect>) {
        let Some(surface) = self.surfaces[index].role.wl_surface().cloned() else {
            return;
        };
        if self.surfaces[index].last_input_region.as_ref() == Some(&regions) {
            return;
        }
        let Some(region) = self.region_of(index, &regions) else {
            return;
        };
        surface.set_input_region(Some(region.wl_region()));
        // `set_input_region` copies the contents, so dropping the region here is sufficient.
        self.surfaces[index].last_input_region = Some(regions);
    }

    fn region_of(&self, index: usize, rects: &[crate::text::snap::PhysicalRect]) -> Option<Region> {
        // `CompositorState::bind` already proved the compositor exists; keep the shell up if region
        // creation nevertheless fails.
        let region = Region::new(&self.compositor_state)
            .inspect_err(|e| log_bind_failure(&self.surfaces[index].surface_id, "wl_compositor::create_region", e))
            .ok()?;
        for rect in rects {
            region.add(rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0);
        }
        Some(region)
    }

    /// Hand the compositor the region behind this surface it should blur (`blur`,
    /// ADR-0195). The rects come from `layout::blur_regions`, which is where the policy lives; this
    /// is only the push.
    ///
    /// Lazily created and never created at all for the common surface, because most surfaces never
    /// set `blur` and an `ext_background_effect_surface_v1` per surface would be an object and a
    /// destroy for nothing.
    ///
    /// A compositor with no manager, or one whose `blur` capability is absent or withdrawn, gets
    /// nothing pushed and the config sees no error: an unavailable compositor feature is not a
    /// config mistake.
    fn apply_blur_region(&mut self, index: usize, regions: Vec<crate::text::snap::PhysicalRect>) {
        if !self.blur_supported {
            return;
        }
        let Some(surface) = self.surfaces[index].role.wl_surface().cloned() else {
            return;
        };
        let qh = self.queue_handle.clone();
        if regions == self.surfaces[index].last_blur_region {
            return;
        }
        if self.surfaces[index].blur_effect.is_none() {
            // Nothing to ask for and nothing asked for before: do not create the object at all.
            if regions.is_empty() {
                return;
            }
            // `blur_supported` only ever comes from a `capabilities` event, and the manager
            // global has to exist to send one, so this is unreachable. Logged rather than
            // silently skipped for exactly that reason: firing means that is wrong.
            let effect = match self.background_effect.get_background_effect(&surface, &qh) {
                Ok(effect) => effect,
                Err(e) => {
                    let name = self.surfaces[index].surface_id.clone();
                    log_bind_failure(&name, "ext_background_effect_manager_v1::get_background_effect", e);
                    return;
                }
            };
            self.surfaces[index].blur_effect = Some(effect);
        }
        let Some(region) = self.region_of(index, &regions) else {
            return;
        };
        if let Some(effect) = self.surfaces[index].blur_effect.as_ref() {
            // A null region would remove the effect; an empty one keeps the object and blurs
            // nothing, which is what a surface whose glass is currently hidden wants.
            effect.set_blur_region(Some(region.wl_region()));
            // The region is double-buffered and lands on the next `wl_surface.commit`, and
            // `paint_surface` skips both the draw and the commit when the display list is
            // unchanged. A `blur` that flips with nothing else moving produces exactly that list,
            // so without this the region would sit pending until some unrelated repaint. `stale`
            // is the existing word for "the committed state is behind what this surface should be
            // showing", and it costs one repaint of a surface whose glass just changed.
            self.surfaces[index].stale = Some(std::time::Instant::now());
        }
        self.surfaces[index].last_blur_region = regions;
    }

    /// Empty the blur region and commit it while the surface still exists. Hyprland blurs the
    /// *whole* snapshot of a closing surface that still carries a region (`CLayerFadeout`, and the
    /// same line in `CWindowFadeout`), which on the full-screen modal host is the whole output.
    /// [`App::apply_blur_region`] emptied it on this pass, but the region is double-buffered and
    /// the `stale` repaint that would commit it never runs; the unmap is next. The commit is the
    /// fix; null is only tidier than an empty region. Not `destroy`: that clears the compositor's
    /// `m_hasBackgroundEffect`, handing the surface back to any blanket `layerrule blur`.
    pub(super) fn release_blur_effect(&self, index: usize) {
        let Some(surface) = self.surfaces[index].role.wl_surface() else {
            return;
        };
        let Some(effect) = self.surfaces[index].blur_effect.as_ref() else {
            return;
        };
        effect.set_blur_region(None);
        surface.commit();
    }

    /// Apply `visible` as create/destroy for every role (ADR-0049 decision 1, ADR-0088).
    fn apply_visibility(&mut self, index: usize, visible: bool) {
        let show = match &self.surfaces[index].role {
            TrackedRole::Panel { .. } => Self::show_panel,
            TrackedRole::Window { .. } => Self::show_window,
            // Popup visibility also reads ADR-0051's latch: dismissal leaves it `Unmapped` while
            // `visible` remains true.
            TrackedRole::Popup { .. } => return self.apply_popup_visibility(index, visible),
            // `lock_spec` rejects `visible`; the compositor owns lock-surface lifetime from
            // `locked` through `unlock_and_destroy` (ADR-0042, ADR-0052 decision 2).
            TrackedRole::Lock { .. } => return,
        };
        match visibility_action(self.surfaces[index].map_state, visible) {
            // `QueueHandle` is a cheap refcounted handle; clone it across `&mut self`.
            VisibilityAction::Show => {
                let qh = self.queue_handle.clone();
                show(self, &qh, index);
            }
            VisibilityAction::Hide => self.drop_role_object(index),
            VisibilityAction::Nothing => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0088's transition, which had no coverage: hiding a shown panel destroys its object, and
    /// showing it again rebuilds one. The `Unmapped`/`false` cell is the case that reaches
    /// `apply_visibility` for every hidden panel on every capability push.
    #[test]
    fn visibility_creates_and_destroys_only_on_the_edges() {
        use VisibilityAction::{Hide, Nothing, Show};
        for (state, visible, want) in [
            (MapState::Unmapped, true, Show),
            (MapState::Unmapped, false, Nothing),
            (MapState::AwaitingConfigure, false, Hide),
            (MapState::AwaitingConfigure, true, Nothing),
            (MapState::Mapped, false, Hide),
            (MapState::Mapped, true, Nothing),
        ] {
            assert_eq!(visibility_action(state, visible), want, "{state:?} with visible = {visible}");
        }
    }
}

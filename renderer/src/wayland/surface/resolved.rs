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
    /// Apply resolved state to every tracked surface after a changed scene.
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
        let (panel, window, popup, tree_rect, regions, visible, blur, asks_for_blur) = {
            let Some(tree) = self.client.scene().surface(&surface_id) else {
                // Startup/apply failure or rollback (`Scene::apply` restores its prior state): keep
                // the last applied fields rather than pushing defaults over a working surface.
                return;
            };
            let role = &self.surfaces[index].role;
            // Both regions are tree walks, and the pushes below discard them on exactly these two
            // conditions: no `wl_surface` to set them on, and for blur, no compositor support.
            let live = role.wl_surface().is_some();
            let blur = if live && self.blur_supported { layout::blur_regions(tree, 1.0) } else { Vec::new() };
            // A second walk only while no glass shows and no effect object exists; see `apply_blur_region`.
            let asks = blur.is_empty() && live && self.blur_supported && self.surfaces[index].blur_effect.is_none();
            (
                matches!(role, TrackedRole::Panel { .. }).then(|| node::panel_spec(&tree.properties)),
                matches!(role, TrackedRole::Window { .. }).then(|| node::window_spec(&tree.properties)),
                matches!(role, TrackedRole::Popup { .. }).then(|| node::popup_spec(&tree.properties)),
                tree.rect,
                if live { layout::overlay_input_regions(tree, 1.0) } else { Vec::new() },
                tree.visible,
                blur,
                asks && layout::asks_for_blur(tree),
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
        self.apply_blur_region(index, blur, asks_for_blur);
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

    /// Hand the compositor the region behind this surface it should blur (`compositor_blur`,
    /// ADR-0195). The rects come from `layout::blur_regions`, which is where the policy lives; this
    /// is only the push.
    ///
    /// Lazily created and never created at all for the common surface, because most surfaces never
    /// set `compositor_blur` and an `ext_background_effect_surface_v1` per surface would be an object and a
    /// destroy for nothing.
    ///
    /// A compositor with no manager, or one whose `blur` capability is absent or withdrawn, gets
    /// nothing pushed and the config sees no error: an unavailable compositor feature is not a
    /// config mistake.
    fn apply_blur_region(&mut self, index: usize, regions: Vec<crate::text::snap::PhysicalRect>, asks_for_blur: bool) {
        if !self.blur_supported {
            return;
        }
        let Some(surface) = self.surfaces[index].role.wl_surface().cloned() else {
            return;
        };
        let effect = self.surfaces[index].blur_effect.is_some();
        // Nothing to ask for and nothing asked for before: do not create the object at all. A surface
        // whose glass starts hidden still takes it, so a blanket compositor blur rule does not blur
        // all of it until the glass shows.
        if !effect && !asks_for_blur && regions.is_empty() {
            return;
        }
        // ponytail: Hyprland 0.57.2 blurs a whole popup (`blur_popups`, `decoration:blur:popups`) whose
        // region is empty or off the surface (OpenGL.cpp:1936-1944, 1702-1720; null is empty), so
        // there a popup's hidden glass is one blurred logical pixel. Drop once it honours empty (#144).
        let popup = matches!(self.surfaces[index].role, TrackedRole::Popup { .. });
        let regions = if regions.is_empty() && self.hyprland && popup {
            vec![crate::text::snap::PhysicalRect { x0: 0, y0: 0, x1: 1, y1: 1 }]
        } else {
            regions
        };
        let qh = self.queue_handle.clone();
        if effect && regions == self.surfaces[index].last_blur_region {
            return;
        }
        if !effect {
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
            // nothing (on Hyprland, only on a layer surface): what a surface whose glass is hidden wants.
            effect.set_blur_region(Some(region.wl_region()));
            // The region is double-buffered and lands on the next `wl_surface.commit`, and
            // `paint_surface` skips both the draw and the commit when the display list is
            // unchanged. A `compositor_blur` that flips with nothing else moving produces exactly that list,
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
    ///
    /// Only mapped surfaces run this: unconfigured surfaces (notably `ext_session_lock_surface_v1`
    /// before its first `ack_configure`) cannot commit yet and have no mapped snapshot to clear.
    pub(super) fn release_blur_effect(&self, index: usize) {
        if self.surfaces[index].map_state != MapState::Mapped {
            return;
        }
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
        if !visible && let TrackedRole::Window { refusal_logged, .. } = &mut self.surfaces[index].role {
            *refusal_logged = false;
        }
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

    fn resolve_lua(app: &mut App, code: &str) {
        app.client.lua().load(code).exec().unwrap();
        assert!(app.client.re_resolve_if_dirty());
        app.apply_resolved_surface_state();
    }

    fn ids(app: &App, index: usize) -> (u32, u32) {
        let s = &app.surfaces[index];
        (s.role.wl_surface().unwrap().id().protocol_id(), s.blur_effect.as_ref().unwrap().id().protocol_id())
    }

    #[test]
    fn unconfigured_lock_surface_teardown_never_commits_or_clears_blur() {
        let (mut app, _dir, mut queue, mock) = App::test_app_with_wire(
            r#"return lock { id = "lock", compositor_blur = true,
                child = textfield { secure_submit = { capability = "lock", action = "authenticate" } } }"#,
        );
        assert!(app.blur_supported);
        let qh = app.queue_handle.clone();
        // Stage blur before configure, then deny lock via `finished`.
        app.set_session_lock(&qh, true);
        assert_eq!(app.surfaces[0].map_state, MapState::AwaitingConfigure);
        app.apply_resolved_surface_state();
        let (wl_1, fx_1) = ids(&app, 0);
        queue.roundtrip(&mut app).unwrap();
        assert_eq!(mock.commits_for(wl_1), 0);
        assert!(matches!(mock.blur_regions_for(fx_1).as_slice(), [r] if *r != 0));
        mock.send_lock_finished();
        queue.roundtrip(&mut app).unwrap();
        assert_eq!((mock.commits_for(wl_1), mock.blur_regions_for(fx_1).len()), (0, 1));
        assert!(app.session_lock.is_none() && app.surfaces[0].map_state == MapState::Unmapped);
        // Same on direct unconfigured unlock, followed by relock.
        app.set_session_lock(&qh, true);
        app.apply_resolved_surface_state();
        let (wl_2, fx_2) = ids(&app, 0);
        app.set_session_lock(&qh, false);
        queue.roundtrip(&mut app).unwrap();
        assert_eq!((mock.commits_for(wl_2), mock.blur_regions_for(fx_2).len()), (0, 1));
        app.set_session_lock(&qh, true);
        assert_eq!(app.surfaces[0].map_state, MapState::AwaitingConfigure);
        assert!(app.surfaces[0].role.wl_surface().is_some());
    }

    #[test]
    fn mapped_surfaces_clear_blur_and_commit_on_teardown_while_unconfigured_ones_skip_it() {
        let (mut app, _dir, mut queue, mock) = App::test_app_with_wire(
            r#"show_panel, show_win = state("show_panel", true), state("show_win", true)
            return {
                panel { id = "bar", layer = "top", output = "TEST", width = 100, height = 32, visible = show_panel,
                    child = rect { width = "fill", height = "fill", compositor_blur = true } },
                window { id = "win", visible = show_win,
                    child = rect { width = 100, height = 100, compositor_blur = true } },
                lock { id = "lock", compositor_blur = true,
                    child = textfield { secure_submit = { capability = "lock", action = "authenticate" } } },
            }"#,
        );
        let qh = app.queue_handle.clone();
        // Mapped panel & window: closing clears blur strictly before committing teardown.
        for (name, hide) in [("bar@TEST", "show_panel:set(false)"), ("win", "show_win:set(false)")] {
            let i = app.surfaces.iter().position(|s| s.surface_id == name).unwrap();
            app.surfaces[i].map_state = MapState::Mapped;
            app.apply_resolved_surface_state();
            let (wl, fx) = ids(&app, i);
            queue.roundtrip(&mut app).unwrap();
            let before = mock.commits_for(wl);
            resolve_lua(&mut app, hide);
            queue.roundtrip(&mut app).unwrap();
            assert_eq!((mock.commits_for(wl), mock.blur_regions_for(fx).last()), (before + 1, Some(&0)), "{name}");
            let reqs = mock.requests.lock().unwrap();
            let clr = reqs.iter().rposition(|&(id, op, arg)| id == fx && op == 1 && arg == 0).unwrap();
            let cmt = reqs.iter().rposition(|&(id, op, _)| id == wl && op == 6).unwrap();
            assert!(clr < cmt, "{name}");
        }
        // Unconfigured panel: hiding before configure keeps only the initial commit.
        let panel_idx = app.surfaces.iter().position(|s| s.surface_id == "bar@TEST").unwrap();
        resolve_lua(&mut app, "show_panel:set(true)");
        assert_eq!(app.surfaces[panel_idx].map_state, MapState::AwaitingConfigure);
        app.apply_resolved_surface_state();
        let (pwl, pfx) = ids(&app, panel_idx);
        queue.roundtrip(&mut app).unwrap();
        assert_eq!(mock.commits_for(pwl), 1);
        resolve_lua(&mut app, "show_panel:set(false)");
        queue.roundtrip(&mut app).unwrap();
        assert_eq!(mock.commits_for(pwl), 1);
        assert_ne!(mock.blur_regions_for(pfx).last(), Some(&0));
        // Mapped lock: unlock commits blur clear, then relock starts fresh unconfigured.
        let lock_idx = app.surfaces.iter().position(|s| s.surface_id == "lock@TEST").unwrap();
        app.set_session_lock(&qh, true);
        app.surfaces[lock_idx].map_state = MapState::Mapped;
        app.apply_resolved_surface_state();
        let (lwl, lfx) = ids(&app, lock_idx);
        queue.roundtrip(&mut app).unwrap();
        app.set_session_lock(&qh, false);
        queue.roundtrip(&mut app).unwrap();
        assert_eq!((mock.commits_for(lwl), mock.blur_regions_for(lfx).last()), (1, Some(&0)));
        app.set_session_lock(&qh, true);
        assert_eq!(app.surfaces[lock_idx].map_state, MapState::AwaitingConfigure);
        assert_ne!(app.surfaces[lock_idx].role.wl_surface().unwrap().id().protocol_id(), lwl);
    }

    /// For the fade tests: optionally a Lua edit, then a tick at `ms` into the one tween running on
    /// the card in `bar@TEST`'s column; then (regions sent, whether the last is empty).
    fn fade_step(
        app: &mut App,
        queue: &mut wayland_client::EventQueue<App>,
        mock: &crate::wayland::test_support::MockWayland,
        lua: &str,
        ms: u64,
    ) -> (Option<usize>, bool) {
        assert_eq!(app.surfaces[0].surface_id, "bar@TEST");
        if !lua.is_empty() {
            resolve_lua(app, lua);
        }
        let bar = ["bar@TEST".to_string()];
        let card = &app.client.scene().surface(&bar[0]).unwrap().children[0].children[0];
        let [tween] = &card.tweens[..] else { panic!("one tween at a time: {:?}", card.tweens) };
        let ticked = app.client.tick_animations(&bar, tween.started + std::time::Duration::from_millis(ms));
        app.apply_resolved_surface_state_for(&[&ticked]);
        queue.roundtrip(app).unwrap();
        let sent = app.surfaces[0].blur_effect.as_ref().map(|fx| mock.blur_regions_for(fx.id().protocol_id()));
        (sent.map(|sent| sent.len()), app.surfaces[0].last_blur_region.is_empty())
    }

    /// A keyframe fade is cut at half its highest frame, not half its first or last: `{ 0, 1, 0 }`
    /// gains the blur on the way up and loses it on the way down.
    #[test]
    fn a_keyframe_fade_switches_the_blur_region_at_half_its_highest_frame() {
        let (mut app, _dir, mut queue, mock) = App::test_app_with_wire(
            r#"return panel { id = "bar", layer = "top", output = "TEST", width = 100, height = 32,
                child = column { children = { rect { width = 100, height = 32, compositor_blur = true,
                    animate = { opacity = { duration = 1000, easing = "linear", keyframes = { 0, 1, 0 } } } } } } }"#,
        );
        app.surfaces[0].map_state = MapState::Mapped;
        for (ms, want) in
            [(400, (Some(1), true)), (600, (Some(2), false)), (1400, (Some(2), false)), (1600, (Some(3), true))]
        {
            assert_eq!(fade_step(&mut app, &mut queue, &mock, "", ms), want, "at {ms} ms");
        }
    }

    /// Issue #114. The region is on or off, so a fade from or to 0 sends it once, on the tick that
    /// carries the opacity past half the fade's higher end, and not on the ticks either side of it.
    /// A fade between two values above 0 sends nothing, and an ancestor's fade cuts its subtree the
    /// same. A reversal's span is `Tween::span`'s test: a pass runs on the real clock, so these
    /// synthetic ticks cannot time one.
    #[test]
    fn an_opacity_fade_sends_the_blur_region_once_each_way_on_the_tick_that_crosses_half() {
        // `nested` fades the box's parent instead of the box.
        for nested in [false, true] {
            let (mut app, _dir, mut queue, mock) = App::test_app_with_wire(
                &r#"local nested = NESTED
                local card = rect { width = 100, height = 32, compositor_blur = not nested, opacity = state("o", 1),
                    children = nested and { rect { width = "fill", height = "fill", compositor_blur = true } } or {},
                    border_width = state("bw", 0),
                    animate = { opacity = { duration = 1000, easing = "linear", from = 0 },
                                border_width = { duration = 1000, easing = "linear" },
                                exit = { duration = 1000, easing = "linear", opacity = 0 } } }
                return panel { id = "bar", layer = "top", output = "TEST", width = 100, height = 32,
                    child = column { children = state("kids", false):map(function(on) return on and { card } or {} end) } }"#
                    .replace("NESTED", &nested.to_string()),
            );
            app.surfaces[0].map_state = MapState::Mapped;
            let (on, off) = (false, true);
            for (lua, ms, want, why) in [
                (r#"state("kids", false):set(true)"#, 400, (Some(1), off), "an enter from 0 sends empty at 0.4"),
                ("", 600, (Some(2), on), "and is sent at 0.6"),
                ("", 900, (Some(2), on), "once"),
                (r#"state("o", 1):set(0.3)"#, 800, (Some(2), on), "1 to 0.3 keeps it throughout"),
                (r#"state("o", 1):set(1)"#, 500, (Some(2), on), "and 0.3 to 1"),
                ("", 1000, (Some(2), on), "the box at rest at 1"),
                (r#"state("o", 1):set(0)"#, 400, (Some(2), on), "1 to 0 keeps it at 0.6"),
                ("", 600, (Some(3), off), "and empties it at 0.4, mid-fade"),
                ("", 1000, (Some(3), off), "the end resends nothing"),
                (r#"state("o", 1):set(0.4)"#, 400, (Some(3), off), "0 to 0.4 cuts at 0.2: not at 0.16"),
                ("", 600, (Some(4), on), "but at 0.24"),
                ("", 1000, (Some(4), on), "and at rest at 0.4"),
                (r#"state("o", 1):set(0)"#, 400, (Some(4), on), "0.4 to 0 keeps it at 0.24"),
                ("", 600, (Some(5), off), "and empties it at 0.16"),
                ("", 1000, (Some(5), off), ""),
                (r#"state("o", 1):set(1)"#, 1000, (Some(6), on), ""),
                (r#"state("bw", 0):set(4)"#, 400, (Some(6), on), "a tween on another property cuts nothing"),
                ("", 1000, (Some(6), on), ""),
                (r#"state("kids", false):set(false)"#, 400, (Some(6), on), "an exit at 0.6 keeps it"),
                ("", 600, (Some(7), off), "and at 0.4 empties it before the node goes"),
            ] {
                assert_eq!(fade_step(&mut app, &mut queue, &mock, lua, ms), want, "{why}, nested: {nested}");
            }
        }
    }

    /// #144: `bar` and its popup `menu` each hold a 40x20 card blurred while `glass` is on, the popup's at
    /// opacity `fade`, fading in from 0 under `enter`. Runs `before` the popup opens and `after`.
    fn glass_app(
        hyprland: bool,
        before: &str,
        after: &str,
    ) -> (App, tempfile::TempDir, wayland_client::EventQueue<App>, crate::wayland::test_support::MockWayland) {
        let (mut app, dir, mut queue, mock) = App::test_app_with_wire(
            r#"glass, fade, enter = state("glass", true), state("fade", 1), state("enter", false)
            local function card(o, a) return rect { width = 40, height = 20, compositor_blur = glass, opacity = o, animate = a } end
            local fade_in = enter:map(function(e) return e and { opacity = { duration = 1000, from = 0 } } or {} end)
            return { panel { id = "bar", layer = "top", output = "TEST", width = 100, height = 40, child = card(1, {}) },
                popup { id = "menu", parent = "bar", anchor_rect = { x = 0, y = 0, width = 10, height = 10 },
                    width = 50, height = 50, grab = false, visible = state("open", false), child = card(fade, fade_in) } }"#,
        );
        app.hyprland = hyprland;
        app.surfaces.iter_mut().for_each(|s| s.map_state = MapState::Mapped);
        app.input_serial = Some(crate::wayland::input::ArmedSerial { serial: 1, instance_id: "bar@TEST".into() });
        for lua in [before, r#"state("open", false):set(true)"#, after] {
            app.client.lua().load(lua).exec().unwrap();
            app.client.re_resolve_if_dirty(); // Not when only the hidden popup reads the edit.
            // Twice: the opening pass has no `wl_surface` to blur yet.
            (0..2).for_each(|_| app.apply_resolved_surface_state());
        }
        queue.roundtrip(&mut app).unwrap();
        (app, dir, queue, mock)
    }

    /// The rects each surface's last `set_blur_region` sent, panel first, `None` with no effect object.
    fn glass_rects(hyprland: bool, before: &str, after: &str) -> [Option<Vec<[i32; 4]>>; 2] {
        let (app, _dir, _queue, mock) = glass_app(hyprland, before, after);
        ["bar@TEST", "menu"].map(|name| {
            let i = app.surfaces.iter().position(|s| s.surface_id == name).unwrap();
            app.surfaces[i].blur_effect.as_ref().map(|fx| mock.last_blur_rects(fx.id().protocol_id()))
        })
    }

    /// Hyprland blurs a whole popup on an empty region, so there a popup with its glass hidden sends 1x1:
    /// faded out, no longer blurring, or opening with a fade from 0. Elsewhere, and on a panel, it is empty.
    /// A popup with no `compositor_blur` takes no effect object, and shown glass is sent as is.
    #[test]
    fn a_popup_with_its_glass_hidden_sends_one_pixel_on_hyprland_and_empty_elsewhere() {
        let (card, none, pixel) = (Some(vec![[0, 0, 40, 20]]), Some(vec![]), Some(vec![[0, 0, 1, 1]]));
        for (hyprland, before, after, want) in [
            (true, "", "", [card.clone(), card.clone()]),
            (true, "", "fade:set(0)", [card.clone(), pixel.clone()]),
            (true, "", "glass:set(false)", [none.clone(), pixel.clone()]),
            (true, "enter:set(true)", "", [card.clone(), pixel]),
            (true, "glass:set(false)", "", [None, None]),
            (false, "", "fade:set(0)", [card.clone(), none.clone()]),
            (false, "enter:set(true)", "", [card, none]),
        ] {
            assert_eq!(glass_rects(hyprland, before, after), want, "{hyprland} {before} / {after}");
        }
    }

    /// The blur capability withdrawn and re-announced clears the record: the Hyprland popup whose glass is
    /// hidden sends its 1x1 once more, and nothing else.
    #[test]
    fn a_capability_bounce_resends_the_one_pixel_region_of_a_popup_with_hidden_glass() {
        let (mut app, _dir, mut queue, mock) = glass_app(true, "", "fade:set(0)");
        let fx = ids(&app, app.surfaces.iter().position(|s| s.surface_id == "menu").unwrap()).1;
        // `get_background_effect` is opcode 1 on the manager, its first argument the effect's id.
        let manager =
            mock.requests.lock().unwrap().iter().find(|&&(id, op, arg)| op == 1 && arg == fx && id != fx).unwrap().0;
        let sent = mock.blur_regions_for(fx).len();
        for capabilities in [0u32, 1] {
            mock.send(manager, 0, &capabilities.to_ne_bytes());
            queue.roundtrip(&mut app).unwrap();
            app.apply_resolved_surface_state();
        }
        queue.roundtrip(&mut app).unwrap();
        assert_eq!((mock.blur_regions_for(fx).len(), mock.last_blur_rects(fx)), (sent + 1, vec![[0, 0, 1, 1]]));
    }

    /// #132's panels, all mapped: `flip` and `bar` (reserving 40 px) take `mode`, `fixed` is `on_demand`, and
    /// `vis` is `on_demand` shown by `visible`; `menu` is a popup on `flip`, by its declared id. Then a `wl_surface` id per panel.
    fn modes_app()
    -> (App, tempfile::TempDir, wayland_client::EventQueue<App>, crate::wayland::test_support::MockWayland) {
        let (mut app, dir, queue, mock) = App::test_app_with_wire(
            r#"mode, shown = state("mode", "none"), state("shown", false)
            local function p(id, kb, zone, visible) return panel { id = id, layer = "top", output = "TEST", width = 100,
                height = 40, keyboard_interactivity = kb, exclusive_zone = zone, visible = visible, child = textfield {
                    width = 100, height = 20, autofocus = true, on_change = print } } end
            return { p("flip", mode, false), p("bar", mode, 40), p("fixed", "on_demand"), p("vis", "on_demand", false, shown),
                popup { id = "menu", parent = "flip", anchor_rect = { x = 0, y = 0, width = 10, height = 10 }, width = 50,
                    height = 50, grab = false, visible = state("menu", false) } }"#,
        );
        resolve_lua(&mut app, "shown:set(true)");
        app.surfaces.iter_mut().for_each(|s| s.map_state = MapState::Mapped);
        (app, dir, queue, mock)
    }
    fn wl_ids(app: &App) -> Vec<u32> {
        app.surfaces[..4].iter().map(|s| s.role.wl_surface().unwrap().id().protocol_id()).collect()
    }

    #[test]
    fn a_mapped_panel_keeps_its_surface_across_every_other_interactivity_change() {
        let (mut app, _dir, mut queue, mock) = modes_app();
        let before = wl_ids(&app);
        app.input_serial = Some(crate::wayland::input::ArmedSerial { serial: 1, instance_id: "flip@TEST".into() });
        // Opening, shown, then closing in the flip's pass: a remap would give the popup an unconfigured
        // parent, then close it.
        let (open, none) = (r#"state("menu", false):set(true) mode:set("on_demand")"#, r#"mode:set("none")"#);
        let close = r#"state("menu", false):set(false) mode:set("on_demand")"#;
        for lua in [r#"mode:set("exclusive")"#, none, open, none, open, none, close] {
            resolve_lua(&mut app, lua);
            queue.roundtrip(&mut app).unwrap();
            assert_eq!(wl_ids(&app), before, "{lua}: `flip` is not remapped either");
            assert!(!mock.requests.lock().unwrap().iter().any(|&(id, op, _)| before.contains(&id) && op == 0), "{lua}");
        }
    }

    /// A reload editing `parent` leaves the stored spec behind the resolved tree, which an opening popup
    /// takes, and ahead of where a shown one stays attached: the guard reads those two.
    #[test]
    fn a_popup_keeps_its_parent_configured_whatever_its_stored_spec_names() {
        let (mut app, _dir, _queue, _mock) = modes_app();
        let (before, menu) = (wl_ids(&app), app.surfaces.iter().position(|s| s.surface_id == "menu").unwrap());
        app.input_serial = Some(crate::wayland::input::ArmedSerial { serial: 1, instance_id: "flip@TEST".into() });
        for lua in [
            r#"state("menu", false):set(true) mode:set("on_demand")"#,
            r#"mode:set("none")"#,
            r#"mode:set("on_demand")"#,
        ] {
            if let TrackedRole::Popup { spec, .. } = &mut app.surfaces[menu].role {
                spec.parent = "fixed".into();
            }
            resolve_lua(&mut app, lua);
            assert_eq!(wl_ids(&app), before, "{lua}");
        }
    }

    /// #132: no compositor focuses a mapped surface turning `on_demand`, all focus one at map.
    #[test]
    fn a_mapped_panel_turning_on_demand_is_remapped_so_the_compositor_focuses_it() {
        let (mut app, _dir, mut queue, mock) = modes_app();
        let layer_id = |app: &App, index: usize| {
            let TrackedRole::Panel { layer: Some(layer), .. } = &app.surfaces[index].role else { panic!("unshown") };
            let smithay_client_toolkit::shell::wlr_layer::SurfaceKind::Wlr(wlr) = layer.kind() else { panic!() };
            wlr.id().protocol_id()
        };
        let (old, old_layer, bar_layer) = (wl_ids(&app)[0], layer_id(&app, 0), layer_id(&app, 1));
        resolve_lua(&mut app, r#"mode:set("on_demand")"#);
        queue.roundtrip(&mut app).unwrap();
        let requests = mock.requests.lock().unwrap().clone();
        assert!(requests.contains(&(old, 0, 0)), "the old wl_surface is destroyed");
        assert!(!requests.contains(&(old_layer, 4, 2)), "no `on_demand` request on the layer it destroys");
        assert!(requests.contains(&(bar_layer, 4, 2)), "`bar`, kept for its zone, is sent `on_demand`");
        assert_ne!(wl_ids(&app)[0], old);
        assert_eq!(app.surfaces[0].map_state, MapState::AwaitingConfigure);
        assert_eq!(app.focused_control.as_ref().map(|c| c.surface_id.as_str()), Some("flip@TEST"));
    }

    /// An `"active"` panel closed before its first configure is kept. Under the outputs it was built
    /// for, that is a refusal: no pass recreates it until an output change. After one, it is retried at once.
    #[test]
    fn an_active_panel_refused_before_its_first_configure_waits_for_an_output_change() {
        let (mut app, _dir, _queue, _mock) = App::test_app_with_wire(
            r#"mode = state("mode", "none")
            return panel { id = "p", layer = "top", output = "active", width = 100, height = 40, keyboard_interactivity = mode }"#,
        );
        app.surfaces[0].map_state = MapState::Mapped;
        resolve_lua(&mut app, r#"mode:set("on_demand")"#);
        let (conn, qh) = (app.conn.clone(), app.queue_handle.clone());
        let output = app.output_state.outputs().next().unwrap();
        let close = |app: &mut App| {
            let TrackedRole::Panel { layer: Some(layer), .. } = &app.surfaces[0].role else { panic!("unshown") };
            app.closed(&conn, &qh, &layer.clone());
            assert_eq!((app.surfaces.len(), app.surfaces[0].map_state), (1, MapState::Unmapped));
        };
        // A pass as the main loop runs one: only when something marked the scene dirty.
        let pass = |app: &mut App, dirty: bool| {
            if dirty {
                app.client.mark_dirty();
            }
            if app.client.re_resolve_if_dirty() {
                app.apply_resolved_surface_state();
            }
            app.surfaces[0].map_state
        };
        close(&mut app);
        assert!(!app.client.re_resolve_if_dirty(), "no pass asked for");
        assert_eq!([0; 3].map(|_| pass(&mut app, true)), [MapState::Unmapped; 3], "no retry until the outputs change");
        app.update_output(&conn, &qh, output.clone());
        assert_eq!(pass(&mut app, false), MapState::AwaitingConfigure, "the output change asks for the pass");
        app.update_output(&conn, &qh, output);
        close(&mut app);
        assert_eq!(pass(&mut app, false), MapState::AwaitingConfigure, "its output changed after the build: retried");
        app.surfaces[0].map_state = MapState::Mapped;
        close(&mut app);
        assert_eq!(
            pass(&mut app, false),
            MapState::AwaitingConfigure,
            "closed once mapped: retried at once, as before"
        );
    }
}

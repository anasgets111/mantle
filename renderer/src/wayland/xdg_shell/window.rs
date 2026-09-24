//! `window` (`xdg_toplevel`): creation, size negotiation and live property updates.

use shared::{debug, error, warn};

use super::*;
use crate::wayland::surface::MapState;
use crate::wayland::surface::TrackedRole;

/// Fallback for a compositor-selected window axis without `min_size`. ponytail: fixed 640x480;
/// with no min size, that is the opening size when the first configure leaves an axis zero.
/// Upgrade: an advisory initial size property, or solver-backed `Content` sizing (ADR-0077).
const UNCONFIGURED_WINDOW_SIZE: (f32, f32) = (640.0, 480.0);
/// Toplevel buffer size. `xdg_toplevel::configure` binds maximized and fullscreen sizes, so `Some`
/// axes are authoritative; tiling compositors, including niri, always take this branch. A `None`
/// axis means "the client picks", the ordinary first configure on a floating compositor. Choose
/// `min_size`, then 640x480, then clamp by positive `max_size`; a zero max means unset per
/// `set_max_size`. Clamp both axes to 1 because a zero `wl_egl_window` is invalid.
fn toplevel_size_for(
    new_size: (Option<std::num::NonZeroU32>, Option<std::num::NonZeroU32>),
    spec: &WindowSpec,
) -> (u32, u32) {
    let axis = |configured: Option<std::num::NonZeroU32>, fallback: f32, min: f32, max: f32| -> u32 {
        if let Some(configured) = configured {
            return configured.get();
        }
        let mut picked = if min > 0.0 { min } else { fallback };
        if max > 0.0 {
            picked = picked.min(max);
        }
        (picked.max(1.0)) as u32
    };
    let min = spec.min_size.unwrap_or(SizeHint { width: 0.0, height: 0.0 });
    let max = spec.max_size.unwrap_or(SizeHint { width: 0.0, height: 0.0 });
    (
        axis(new_size.0, UNCONFIGURED_WINDOW_SIZE.0, min.width, max.width),
        axis(new_size.1, UNCONFIGURED_WINDOW_SIZE.1, min.height, max.height),
    )
}
/// Live `xdg_toplevel` changes (ADR-0049 amendment). All protocol fields are diffed because
/// title/app-id and size hints remain double-buffered after mapping; `id` is only reconcile
/// identity. `Option<Option<SizeHint>>` distinguishes unchanged from changed-to-absent, which
/// must reach `set_min_size(None)`/`set_max_size(None)` as protocol unset.
#[derive(Debug, Default, PartialEq)]
struct WindowUpdate {
    title: Option<String>,
    app_id: Option<String>,
    min_size: Option<Option<SizeHint>>,
    max_size: Option<Option<SizeHint>>,
}
fn window_update(applied: &WindowSpec, fresh: &WindowSpec) -> WindowUpdate {
    WindowUpdate {
        title: (fresh.title != applied.title).then(|| fresh.title.clone()),
        app_id: (fresh.app_id != applied.app_id).then(|| fresh.app_id.clone()),
        min_size: (fresh.min_size != applied.min_size).then_some(fresh.min_size),
        max_size: (fresh.max_size != applied.max_size).then_some(fresh.max_size),
    }
}
/// A [`SizeHint`] in protocol units; `None` remains unset, sent as protocol zero.
fn size_hint_pair(hint: Option<SizeHint>) -> Option<(u32, u32)> {
    hint.map(|hint| (hint.width.max(0.0) as u32, hint.height.max(0.0) as u32))
}

impl App {
    /// [`App::create_surfaces`]'s `window` arm: always track it, but create `xdg_toplevel` only
    /// when visible (ADR-0049 decision 1). The entry lets later re-resolves observe `visible`.
    pub(in crate::wayland) fn create_window(
        &mut self,
        qh: &QueueHandle<App>,
        spec: &WindowSpec,
        instance: &SurfaceInstance,
        visible: bool,
    ) {
        self.surfaces.push(TrackedSurface::new(
            TrackedRole::Window { window: None, spec: spec.clone() },
            instance.instance_id.clone(),
        ));
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_window(qh, index);
        }
    }

    /// Diff a fresh `window` spec and send changed fields. Hidden windows have no object to
    /// update, but retain the latest spec so a title changed three times while closed opens with
    /// the third value (ADR-0049 decision 1).
    pub(in crate::wayland) fn apply_window_change(&mut self, index: usize, fresh: WindowSpec) {
        let TrackedRole::Window { window, spec: applied } = &mut self.surfaces[index].role else {
            return;
        };
        let update = window_update(applied, &fresh);
        *applied = fresh;
        let Some(window) = window.as_ref() else {
            return;
        };
        if let Some(title) = update.title {
            window.set_title(title);
        }
        if let Some(app_id) = update.app_id {
            window.set_app_id(app_id);
        }
        // Minimum before maximum avoids a transient inverted pair and `invalid_size`; the parser
        // already rejects the final pairing.
        if let Some(min_size) = update.min_size {
            window.set_min_size(size_hint_pair(min_size));
        }
        if let Some(max_size) = update.max_size {
            window.set_max_size(size_hint_pair(max_size));
        }
    }

    /// Creates the toplevel and its required initial unbuffered commit (ADR-0040 decisions
    /// 4-5, ADR-0049 decision 1), then waits in `AwaitingConfigure`. SCTK acks configure before
    /// the handler. `XdgShell::bind` already picked up `zxdg_decoration_manager_v1` with
    /// `xdg_wm_base`, so `WindowDecorations::RequestServer` plus
    /// [`Window::request_decoration_mode`] is the whole decoration path, with no second global.
    /// No geometry request is needed: the default bounding box fits this edge-to-edge shell, with
    /// no shadow to exclude and no subsurfaces. Missing xdg-shell logs once per attempt and leaves
    /// the window absent, not fatal.
    pub(in crate::wayland) fn show_window(&mut self, qh: &QueueHandle<App>, index: usize) {
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            error!(
                "{}: this compositor advertises no xdg_wm_base, so no window can be created for it",
                self.surfaces[index].surface_id
            );
            return;
        };
        let TrackedRole::Window { spec, .. } = &self.surfaces[index].role else {
            return;
        };
        let spec = spec.clone();

        let surface = self.compositor_state.create_surface(qh);
        let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, qh);
        // The constructor decides whether the decoration object exists; this sets its mode.
        // Accept the compositor's answer; configure logs client-side decoration and remains bare.
        window.request_decoration_mode(Some(DecorationMode::Server));
        window.set_title(spec.title.clone());
        window.set_app_id(spec.app_id.clone());
        // Hints do not clamp layout, but bound the size chosen for `None` configure axes.
        window.set_min_size(size_hint_pair(spec.min_size));
        window.set_max_size(size_hint_pair(spec.max_size));
        // Required initial commit without a buffer; configure then permits attachment.
        window.commit();

        if let TrackedRole::Window { window: slot, .. } = &mut self.surfaces[index].role {
            *slot = Some(window);
        }
        self.surfaces[index].map_state = MapState::AwaitingConfigure;
        debug!("{} creating: visible = true", self.surfaces[index].surface_id);
    }
}

/// `xdg_toplevel` handler for `window`.
impl WindowHandler for App {
    /// `xdg_toplevel::close` is a request, not a command: config may ignore it. Do not destroy
    /// here; doing so would leave `visible=true` describing a missing window and the next resolve
    /// would create a second one (ADR-0049 decision 2). Clone the callback before Lua; log and
    /// swallow raises like `fire_on_click`.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, window: &Window) {
        let Some(index) = self.index_of_surface(window.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        let on_close = self
            .client
            .scene()
            .surface(&surface_id)
            .and_then(|tree| crate::layout::node::fields::window::on_close.read(&tree.properties).ok().flatten());
        let Some(on_close) = on_close else {
            debug!(
                2; "{surface_id}: the compositor asked it to close and no `on_close` declined or accepted; staying open"
            );
            return;
        };
        if let Err(e) = on_close.call::<()>(()) {
            warn!("{surface_id}: on_close raised, ignoring it: {e}");
        }
    }

    /// SCTK has acked this configure. `new_size` may leave axes to the client; client-side
    /// decoration is logged but not drawn (ADR-0040 decision 4); `state`/`capabilities` have no
    /// config binding, while fullscreen/maximized sizes arrive as `Some` axes.
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let Some(index) = self.index_of_surface(window.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        if configure.decoration_mode == DecorationMode::Client
            && self.surfaces[index].map_state == MapState::AwaitingConfigure
        {
            debug!(
                2; "{surface_id}: the compositor granted client-side decorations; carrying on undecorated, since this shell draws no titlebar of its own"
            );
        }
        let TrackedRole::Window { spec, .. } = &self.surfaces[index].role else {
            return;
        };
        let (width, height) = toplevel_size_for(configure.new_size, spec);
        self.bind_and_clear(index, width, height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_window() -> WindowSpec {
        WindowSpec {
            id: "settings".to_string(),
            title: "Mantle settings".to_string(),
            app_id: "mantle.settings".to_string(),
            min_size: None,
            max_size: None,
        }
    }

    fn nz(n: u32) -> Option<std::num::NonZeroU32> {
        std::num::NonZeroU32::new(n)
    }

    #[test]
    fn a_configured_toplevel_axis_is_the_compositors_and_is_taken_as_given() {
        // A tiling compositor sizes every window: `xdg_toplevel::configure` makes a maximized or
        // fullscreen size binding, not advisory. On niri this is the only branch that ever runs.
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        spec.max_size = Some(SizeHint { width: 1280.0, height: 800.0 });
        assert_eq!(
            toplevel_size_for((nz(1920), nz(1168)), &spec),
            (1920, 1168),
            "the hints never override a configure"
        );
    }

    #[test]
    fn an_unconfigured_toplevel_axis_takes_the_min_size_the_config_declared() {
        // "If this value is None, you may set the size of the window as you wish", which is the
        // ordinary first configure on a floating compositor. `min_size` is the only thing a config
        // can say about a window's size, so it is what the client says back.
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        assert_eq!(toplevel_size_for((None, None), &spec), (320, 240));
        // One axis each way, which is the shape a compositor constraining only width produces.
        assert_eq!(toplevel_size_for((nz(800), None), &spec), (800, 240));
    }

    #[test]
    fn an_unconfigured_axis_with_no_min_size_falls_back_to_the_named_constant() {
        // Its `ponytail:` states the ceiling: a config has nothing else to say here, and a
        // toplevel's root is forced to the surface, so no content size exists to prefer instead.
        assert_eq!(toplevel_size_for((None, None), &settings_window()), (640, 480));
    }

    #[test]
    fn the_size_this_client_picks_stays_under_the_max_size_the_config_declared() {
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 900.0, height: 900.0 });
        spec.max_size = Some(SizeHint { width: 400.0, height: 0.0 });
        // A zero `max_size` axis is not a maximum of zero: `set_max_size`'s own "0 means no
        // expected maximum size in the given dimension".
        assert_eq!(toplevel_size_for((None, None), &spec), (400, 900));
    }

    #[test]
    fn a_re_resolve_that_changed_no_window_property_sends_no_requests_at_all() {
        let applied = settings_window();
        assert_eq!(window_update(&applied, &applied.clone()), WindowUpdate::default());
    }

    #[test]
    fn every_window_field_is_pushed_on_its_own_and_only_when_it_moved() {
        // All four, unlike a panel's diff: `xdg-shell.xml` allows `set_app_id`/`set_title` after
        // mapping, and both size hints are ordinary double-buffered requests, so a changed `title`
        // is an in-place update, not a recreate.
        let applied = settings_window();

        let mut renamed = applied.clone();
        renamed.title = "Settings".to_string();
        assert_eq!(
            window_update(&applied, &renamed),
            WindowUpdate { title: Some("Settings".to_string()), ..WindowUpdate::default() }
        );

        let mut rematched = applied.clone();
        rematched.app_id = "mantle.prefs".to_string();
        assert_eq!(
            window_update(&applied, &rematched),
            WindowUpdate { app_id: Some("mantle.prefs".to_string()), ..WindowUpdate::default() }
        );

        let mut bounded = applied.clone();
        bounded.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        assert_eq!(
            window_update(&applied, &bounded),
            WindowUpdate { min_size: Some(Some(SizeHint { width: 320.0, height: 240.0 })), ..WindowUpdate::default() }
        );
    }

    #[test]
    fn a_size_hint_that_moved_to_absent_is_still_a_change_that_has_to_reach_the_wire() {
        // The reason the field is `Option<Option<_>>`: the outer layer is "did it move", the inner
        // one is absent-versus-present, and dropping a `max_size` from a config has to send
        // the protocol's zero (meaning unset) rather than leaving the old maximum standing.
        let mut applied = settings_window();
        applied.max_size = Some(SizeHint { width: 1280.0, height: 800.0 });
        let fresh = settings_window();

        assert_eq!(window_update(&applied, &fresh), WindowUpdate { max_size: Some(None), ..WindowUpdate::default() });
        assert_eq!(size_hint_pair(None), None, "which `Window::set_max_size` sends as the protocol's zero");
        assert_eq!(size_hint_pair(Some(SizeHint { width: 320.0, height: 240.0 })), Some((320, 240)));
    }
}

//! `window` (`xdg_toplevel`): creation, size negotiation and live property updates.

use shared::{debug, error, warn};

use smithay_client_toolkit::reexports::csd_frame::WindowManagerCapabilities;
use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;

use super::*;
use crate::layout::node::{Decorations, EdgeInsets};
use crate::lua::call_logged;
use crate::lua::toplevel::{Action, Bounds, Capabilities, Edge, Request, Tiled, ToplevelState};
use crate::wayland::surface::MapState;
use crate::wayland::surface::TrackedRole;

/// Fallback for a compositor-selected window axis without `min_size`. ponytail: fixed 640x480;
/// with no min size, that is the opening size when the first configure leaves an axis zero.
/// Upgrade: an advisory initial size property, or solver-backed `Content` sizing (ADR-0077).
const UNCONFIGURED_WINDOW_SIZE: (f32, f32) = (640.0, 480.0);
/// Window geometry size (the buffer less `geometry_inset`): a configured axis wins, else
/// `min_size`, else 640x480, clamped by a positive `max_size` and to at least 1.
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
/// The buffer size around a `geometry`-sized window and its `set_window_geometry` rect (x, y,
/// width, height). Whole logical px, because both the surface size and the request are integers.
pub(super) fn window_frame(geometry: (u32, u32), inset: EdgeInsets) -> ((u32, u32), [u32; 4]) {
    let [top, right, bottom, left] = [inset.top, inset.right, inset.bottom, inset.left].map(|n| n.round() as u32);
    ((geometry.0 + left + right, geometry.1 + top + bottom), [left, top, geometry.0, geometry.1])
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
    decorations: Option<Decorations>,
    geometry_inset: Option<EdgeInsets>,
}
fn window_update(applied: &WindowSpec, fresh: &WindowSpec) -> WindowUpdate {
    WindowUpdate {
        geometry_inset: (fresh.geometry_inset != applied.geometry_inset).then_some(fresh.geometry_inset),
        title: (fresh.title != applied.title).then(|| fresh.title.clone()),
        app_id: (fresh.app_id != applied.app_id).then(|| fresh.app_id.clone()),
        min_size: (fresh.min_size != applied.min_size).then_some(fresh.min_size),
        max_size: (fresh.max_size != applied.max_size).then_some(fresh.max_size),
        decorations: (fresh.decorations != applied.decorations).then_some(fresh.decorations),
    }
}

impl From<Decorations> for DecorationMode {
    fn from(decorations: Decorations) -> Self {
        match decorations {
            Decorations::Server => Self::Server,
            Decorations::Client => Self::Client,
        }
    }
}

/// Anything the compositor does not name server-side is drawn by the client.
impl From<DecorationMode> for Decorations {
    fn from(mode: DecorationMode) -> Self {
        match mode {
            DecorationMode::Server => Self::Server,
            _ => Self::Client,
        }
    }
}

/// The `toplevel(id):state()` value for one configure: the compositor's state flags, bounds,
/// capabilities and the decoration mode it chose.
fn toplevel_state(configure: &WindowConfigure) -> ToplevelState {
    let caps = configure.capabilities;
    ToplevelState {
        activated: configure.is_activated(),
        maximized: configure.is_maximized(),
        fullscreen: configure.is_fullscreen(),
        resizing: configure.is_resizing(),
        tiled: Tiled {
            left: configure.is_tiled_left(),
            right: configure.is_tiled_right(),
            top: configure.is_tiled_top(),
            bottom: configure.is_tiled_bottom(),
        },
        bounds: configure.suggested_bounds.map(|(width, height)| Bounds { width, height }),
        capabilities: Capabilities {
            window_menu: caps.contains(WindowManagerCapabilities::WINDOW_MENU),
            maximize: caps.contains(WindowManagerCapabilities::MAXIMIZE),
            fullscreen: caps.contains(WindowManagerCapabilities::FULLSCREEN),
            minimize: caps.contains(WindowManagerCapabilities::MINIMIZE),
        },
        decoration: configure.decoration_mode.into(),
    }
}
/// A [`SizeHint`] in protocol units; `None` remains unset, sent as protocol zero.
fn size_hint_pair(hint: Option<SizeHint>) -> Option<(u32, u32)> {
    hint.map(|hint| (hint.width.max(0.0) as u32, hint.height.max(0.0) as u32))
}

/// The press serial `window`'s frame request may carry, or why it may not: compositors check the
/// serial against the press that started the grab, so one armed on another surface is refused.
fn frame_serial(shown: bool, armed: Option<&ArmedSerial>, window: &str) -> Result<u32, &'static str> {
    match armed {
        _ if !shown => Err("names no shown window"),
        Some(armed) if armed.instance_id == window => Ok(armed.serial),
        Some(_) => Err("is not the surface that was pressed"),
        None => Err("has no press serial armed"),
    }
}

fn resize_edge(edge: Edge) -> ResizeEdge {
    match edge {
        Edge::Top => ResizeEdge::Top,
        Edge::Bottom => ResizeEdge::Bottom,
        Edge::Left => ResizeEdge::Left,
        Edge::Right => ResizeEdge::Right,
        Edge::TopLeft => ResizeEdge::TopLeft,
        Edge::TopRight => ResizeEdge::TopRight,
        Edge::BottomLeft => ResizeEdge::BottomLeft,
        Edge::BottomRight => ResizeEdge::BottomRight,
    }
}

impl App {
    /// Sends the `toplevel(id)` requests the press-time callbacks queued, with this press's serial
    /// (ADR-0049 amendment). Returns whether the compositor took the pointer, so the caller drops
    /// the press state it will never see released. A refused request warns and sends nothing.
    pub(in crate::wayland) fn send_toplevel_requests(&mut self) -> bool {
        let mut sent = false;
        for Request { window: id, action } in crate::lua::toplevel::end_press(self.client.lua()) {
            let window = self.surfaces.iter().find_map(|tracked| match &tracked.role {
                TrackedRole::Window { window: Some(window), .. } if tracked.surface_id == id => Some(window),
                _ => None,
            });
            let serial = frame_serial(window.is_some(), self.input_serial.as_ref(), &id);
            let (Some(window), Ok(serial), Some(seat)) = (window, &serial, self.pointer_seat()) else {
                let why = serial.err().unwrap_or("has no seat");
                warn!("toplevel({id:?}): {why}, so the frame request was not sent");
                continue;
            };
            match action {
                Action::Move => window.move_(&seat, *serial),
                Action::Resize(edge) => window.resize(&seat, *serial, resize_edge(edge)),
                Action::Menu(at) => window.show_window_menu(&seat, *serial, at),
            }
            sent = true;
        }
        sent
    }

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
            TrackedRole::Window { window: None, spec: spec.clone(), geometry: (0, 0) },
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
        let TrackedRole::Window { window, spec: applied, .. } = &mut self.surfaces[index].role else {
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
        if let Some(decorations) = update.decorations {
            window.request_decoration_mode(Some(decorations.into()));
        }
        // No configure follows an inset change; before the first one there is no geometry to frame.
        if update.geometry_inset.is_some()
            && self.surfaces[index].map_state == MapState::Mapped
            && let Some(buffer) = self.frame_window(index)
        {
            self.set_surface_size(index, buffer);
        }
    }

    /// Stages `set_window_geometry` for the last configured geometry inside a buffer grown by
    /// `geometry_inset`, committed by the paint that attaches that buffer; returns its size.
    fn frame_window(&self, index: usize) -> Option<(u32, u32)> {
        let TrackedRole::Window { window: Some(window), spec, geometry } = &self.surfaces[index].role else {
            return None;
        };
        let (buffer, [x, y, width, height]) = window_frame(*geometry, spec.geometry_inset);
        window.set_window_geometry(x, y, width, height);
        Some(buffer)
    }

    /// Creates the toplevel and its required initial unbuffered commit (ADR-0040 decisions
    /// 4-5, ADR-0049 decision 1), then waits in `AwaitingConfigure`. SCTK acks configure before
    /// the handler. `XdgShell::bind` already picked up `zxdg_decoration_manager_v1` with
    /// `xdg_wm_base`, so `WindowDecorations::RequestServer` plus
    /// [`Window::request_decoration_mode`] is the whole decoration path, with no second global.
    /// The window geometry waits for the first configure ([`App::frame_window`]). Missing
    /// xdg-shell logs once per attempt and leaves the window absent, not fatal.
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
        let scale = self.surface_scale(&surface, qh);
        let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, qh);
        // The constructor decides whether the decoration object exists; this sets its mode.
        // The compositor's answer is accepted and published through `toplevel(id):state()`.
        window.request_decoration_mode(Some(spec.decorations.into()));
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
        self.surfaces[index].scale = scale;
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
        call_logged(&on_close, (), format_args!("{surface_id}: on_close"));
    }

    /// SCTK has acked this configure. `new_size` may leave axes to the client; no frame is drawn
    /// for client-side decoration (ADR-0040 decision 4). The rest of the configure is published to
    /// `toplevel(id):state()`; the main loop's pass re-resolves its readers the same turn.
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
        crate::lua::toplevel::publish(self.client.lua(), &surface_id, toplevel_state(&configure));
        let TrackedRole::Window { spec, geometry, .. } = &mut self.surfaces[index].role else {
            return;
        };
        *geometry = toplevel_size_for(configure.new_size, spec);
        if let Some((width, height)) = self.frame_window(index) {
            self.bind_and_clear(index, width, height);
        }
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
            decorations: Default::default(),
            geometry_inset: Default::default(),
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
    fn the_inset_grows_the_buffer_around_the_configured_geometry_and_never_the_geometry() {
        let inset = EdgeInsets { top: 10.0, right: 20.0, bottom: 30.0, left: 40.0 };
        // A maximized configure binds the geometry, so the shadow band is extra buffer.
        let mut spec = settings_window();
        spec.max_size = Some(SizeHint { width: 800.0, height: 600.0 });
        let geometry = toplevel_size_for((nz(1920), nz(1080)), &spec);
        assert_eq!(window_frame(geometry, inset), ((1980, 1120), [40, 10, 1920, 1080]));
        // The client-picked size and its hints are the geometry's, not the buffer's.
        spec.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        assert_eq!(window_frame(toplevel_size_for((None, None), &spec), inset), ((380, 280), [40, 10, 320, 240]));
        // Whole logical px: the surface size and `set_window_geometry` are integers.
        let fractional = EdgeInsets { top: 7.6, right: 7.4, bottom: 0.0, left: 0.5 };
        assert_eq!(window_frame((100, 100), fractional), ((108, 108), [1, 8, 100, 100]));
        assert_eq!(window_frame((100, 100), EdgeInsets::default()), ((100, 100), [0, 0, 100, 100]));
    }

    #[test]
    fn an_inset_change_is_its_own_update() {
        let applied = settings_window();
        let mut fresh = applied.clone();
        fresh.geometry_inset = EdgeInsets { top: 24.0, ..Default::default() };
        assert_eq!(
            window_update(&applied, &fresh),
            WindowUpdate { geometry_inset: Some(fresh.geometry_inset), ..WindowUpdate::default() }
        );
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

    #[test]
    fn a_decorations_change_is_the_one_field_that_reaches_the_wire() {
        let applied = settings_window();
        let mut fresh = applied.clone();
        fresh.decorations = Decorations::Client;
        assert_eq!(
            window_update(&applied, &fresh),
            WindowUpdate { decorations: Some(Decorations::Client), ..WindowUpdate::default() }
        );
        assert_eq!(DecorationMode::from(Decorations::Client), DecorationMode::Client);
        assert_eq!(DecorationMode::from(Decorations::Server), DecorationMode::Server);
    }

    #[test]
    fn a_configure_maps_to_the_state_a_config_reads() {
        use smithay_client_toolkit::reexports::csd_frame::WindowState;
        let mut configure = WindowConfigure::default();
        configure.state =
            WindowState::ACTIVATED | WindowState::MAXIMIZED | WindowState::TILED_LEFT | WindowState::TILED_TOP;
        configure.suggested_bounds = Some((1920, 1080));
        configure.capabilities = WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::MINIMIZE;
        configure.decoration_mode = DecorationMode::Server;
        assert_eq!(
            toplevel_state(&configure),
            ToplevelState {
                activated: true,
                maximized: true,
                fullscreen: false,
                resizing: false,
                tiled: Tiled { left: true, right: false, top: true, bottom: false },
                bounds: Some(Bounds { width: 1920, height: 1080 }),
                capabilities: Capabilities { window_menu: false, maximize: true, fullscreen: false, minimize: true },
                decoration: Decorations::Server,
            }
        );
        assert_eq!(
            toplevel_state(&WindowConfigure::default()),
            ToplevelState::default(),
            "no flags, all capabilities, client-drawn"
        );
    }

    fn armed(on: &str) -> ArmedSerial {
        ArmedSerial { serial: 7, instance_id: on.into() }
    }

    #[test]
    fn a_frame_request_needs_a_shown_window_and_its_own_press_serial() {
        assert_eq!(frame_serial(true, Some(&armed("main")), "main"), Ok(7));
        assert!(frame_serial(false, Some(&armed("main")), "main").is_err(), "hidden or unknown window");
        assert!(frame_serial(true, Some(&armed("other")), "main").is_err(), "another surface's press");
        assert!(frame_serial(true, None, "main").is_err(), "no armed serial");
    }
}

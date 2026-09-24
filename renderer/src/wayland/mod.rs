pub mod egl;

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;

use khronos_egl::Surface as EglSurface;
use mlua::{Function, Lua, Table, Value};
use shared::{LockOutcome, LockReport, RendererFrame, SecureSubmit, Zeroize, error, warn};
use smithay_client_toolkit::background_effect::{BackgroundEffectHandler, BackgroundEffectState};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardData, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerData, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec,
    ThemedPointer,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface, SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_egl::WlEglSurface;
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1;
use wayland_protocols::xdg::shell::client::{xdg_positioner, xdg_surface};

use capture::CaptureRegistry;

use crate::image::ImageCache;
use crate::image::capture::CaptureCache;
use crate::layout;
use crate::layout::instance::{
    OutputGeometry, SurfaceInstance, expand_instances, is_instance_of, reconcile_instances, warn_unmatched_monitors,
};
use crate::layout::node::{
    self, ConstraintAdjustment, LayerKind, PanelSpec, PopupAnchor, PopupSpec, SizeHint, SizeMode, SurfaceSpec,
    WindowSpec,
};
use crate::socket::RendererClient;
use crate::text::atlas::TextPainter;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

mod capture;
mod dmabuf;
mod egl_ext;
mod idle_profile;
mod input;
#[cfg(test)]
pub(crate) use input::apply_hover_write;
pub(crate) use input::{DragPhase, MouseButton, NavigateKey};
mod layer;
mod lock;
mod main_loop;
mod memory_profile;
mod output;
mod surface;
mod turn;
mod xdg_shell;

use input::{ArmedClick, ArmedSerial, FocusedField, FocusedTextField};
use surface::{TrackedSurface, log_bind_failure};

pub use main_loop::run;

pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    seat_state: SeatState,
    layer_shell: LayerShell,
    /// `ext_background_effect_manager_v1` through SCTK's `GlobalProxy` (ADR-0195). A compositor
    /// without the global is not an error: `blur = true` there is silently nothing, the same answer
    /// every other unavailable compositor feature gets here.
    background_effect: BackgroundEffectState,
    /// Whether the manager announces `blur`. [`BackgroundEffectState`] holds the same bit, but only
    /// the current one, and `update_capabilities` needs the previous value to tell a change from a
    /// repeat. Only a change invalidates every surface's pushed region. A compositor may advertise
    /// the global and still not blur, and it may withdraw the bit later.
    blur_supported: bool,
    /// `xdg_wm_base`, plus the `zxdg_decoration_manager_v1` that `XdgShell::bind` picks up beside
    /// it, or `None`; panels still work without xdg-shell, while a declared window logs the missing
    /// global once.
    xdg_shell: Option<XdgShell>,
    /// `ext_session_lock_manager_v1` through SCTK's `GlobalProxy` (ADR-0042). Missing support
    /// surfaces as `GlobalError::MissingGlobal` from `lock` (ADR-0052 decision 4), not startup;
    /// it binds once from the `GlobalList` in [`run`].
    session_lock_state: SessionLockState,
    /// Live `ext_session_lock_v1` from request until Supervisor unlock, denial, or compositor
    /// teardown. `Some` with `is_locked() == false` is the in-flight window; `finished` therefore
    /// has two meanings (ADR-0042).
    session_lock: Option<SessionLock>,
    /// Shared EGL display/config/GLES3 context, lazy because `eglInitialize` loads Mesa,
    /// `libgallium`, and LLVM: 125 MB mapped and 13-35 ms. No-surface configs avoid it
    /// (ADR-0070 decision 7, ADR-0071).
    egl: Option<egl::EglState>,
    gl: Option<glow::Context>,
    /// Config shaders compiled against `gl`, kept for the context's lifetime rather than a
    /// generation's: a reload replaces the scene, not the GL objects (ADR-0184).
    shader_stage: crate::layout::image_shader::ShaderStage,
    conn: Connection,
    /// Process-wide shaping handle; `client` clones it, so content sizing and painting share one
    /// worker and `FontSystem` (ADR-0039 decision 3).
    shaping: ShapingHandle,
    text_painter: Option<TextPainter>,
    /// Process-wide image cache keyed by file path and pixel size, so repeated icons upload once
    /// (`CONTEXT.md`, **Image cache**).
    image_cache: ImageCache,
    /// One texture per live `capture` node, sized by `image_cache`'s budget but counted apart
    /// (ADR-0248 decision 6).
    capture_cache: CaptureCache,
    /// Protocol objects and pacing for every live `capture` node (ADR-0248).
    captures: CaptureRegistry,
    /// Lua VM, `Loader`, retained `Scene`, live signals, and reload state (ADR-0039). `mlua::Lua`
    /// is `!Send`; `wayland-client` imposes no `Send` bound on dispatch state.
    client: RendererClient,
    surfaces: Vec<TrackedSurface>,
    exit: bool,
    /// Set after startup evaluation and surface creation. The initial `wl_output` burst occurs in
    /// [`run`]'s two roundtrips before `screens` seeds evaluation (ADR-0041 decision 2), so output
    /// changes must not reconcile before a spec exists.
    startup_complete: bool,
    /// Frames for the socket thread's `pump`; `UnboundedSender::send` is synchronous and
    /// non-blocking, so dispatch callbacks can use it.
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    /// This Renderer's generation id, stamped into every `SecureSubmit`; read in `main` from
    /// `MANTLE_GENERATION_ID`.
    generation_id: u32,
    /// Clone for paths that create surfaces or request frame callbacks through `&mut self`.
    queue_handle: QueueHandle<App>,
    /// Advertised seat pointer, kept alive because dropping it destroys pointer events. One slot;
    /// [`SeatHandler::new_capability`] stores whichever seat announces the capability.
    pointer: Option<ThemedPointer>,
    /// Last pointer shape over this process's surfaces (ADR-0107). `Leave` clears it because
    /// `wp_cursor_shape_v1` requires a shape on every `Enter`.
    cursor_shown: Option<cursor_icon::CursorIcon>,
    /// Last pointer surface and position (ADR-0112 amendment), set by `Enter` and `Motion`, cleared
    /// by `Leave`, and rewritten against hover signals after a re-resolve because scrolling moves
    /// rows under a still pointer and no `Motion` arrives to say so.
    pointer_at: Option<(String, (f64, f64))>,
    /// `wl_shm` only for SCTK's XCursor fallback when `wp_cursor_shape_v1` is absent; all other
    /// rendering uses EGL.
    shm: Shm,
    /// Advertised keyboard, kept alive and single-seat. Used only for `enter`/`leave`, the only
    /// client-visible result of `keyboard_interactivity`.
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Focused surface instance id (ADR-0050); `input::keyboard::focus_is_still_armed` requires a
    /// `secure_submit` field's declaring surface to match it.
    ///
    /// ponytail: nothing else consumes it (there is no `on_key` property; ADR-0050 declines to
    /// invent one). Upgrade path: an IDL key-handler property, dispatching into this surface's
    /// tree.
    keyboard_focus: Option<String>,
    /// Press waiting for release (ADR-0050 decision 2, [`ArmedClick`]).
    armed: Option<ArmedClick>,
    /// Held left press on an `on_drag` button (ADR-0116 decision 1); `Motion` reports until release
    /// or `Leave`.
    drag: Option<input::ActiveDrag>,
    /// The serial for `xdg_popup.grab`, valid for one poll turn (ADR-0049 amendment).
    input_serial: Option<ArmedSerial>,
    /// Never-reset count of every `BTN_LEFT` press and release (ADR-0051 amendment). Count both
    /// edges so a later turn has something to compare, whatever order the compositor batches
    /// dismissal relative to `popup_done`. It survives `input_serial`'s per-turn lifetime and
    /// tells whether the user asked again.
    pointer_input_count: u64,
    /// Serial for `xdg_popup.reposition`, returned on the configure it causes
    /// (`ConfigureKind::Reposition`). One counter for the process rather than one per popup: it
    /// only has to tell two requests apart, and wrapping is harmless because nothing here waits on
    /// a specific token.
    reposition_token: u32,
    /// Focused `secure_submit` field and declaring surface, set by a textfield press or sole-field
    /// keyboard focus (ADR-0050 decision 4). `None` means no frame; writes go through
    /// [`App::focus_secure_submit`].
    focused_secure_submit: Option<FocusedField>,
    /// Focused plain `textfield` and its draft, the unmasked half (ADR-0092). Only a
    /// press selects it; sole-field `enter` fallback cannot serve multiple reply boxes.
    ///
    /// Mutually exclusive with `focused_secure_submit`; the innermost textfield is one kind.
    focused_text_field: Option<FocusedTextField>,
    /// Shift on the seat's keyboard: it turns a caret motion or a press into a selection
    /// (ADR-0236).
    shift_held: bool,
    /// Ctrl on the seat's keyboard, read for Ctrl+A alone (ADR-0236).
    ctrl_held: bool,
    /// The seat's repeat delay and interval, absent when the compositor turned repeat off. SCTK
    /// drives its own repeat from a calloop timer, which this renderer does not link (ADR-0124),
    /// so `poll`'s deadline carries it instead.
    repeat_info: Option<(std::time::Duration, std::time::Duration)>,
    /// The held key and when it next repeats.
    repeating: Option<(KeyEvent, std::time::Instant)>,
    /// Native, Lua-invisible keystroke buffer until Enter (ADR-0005/ADR-0009/ADR-0027). Its
    /// lifetime follows `focused_secure_submit`; destination changes zeroize it.
    secure_buffer: shared::SecureBuffer,
    /// A keystroke/focus change changed field rendering without dirtying the retained tree: masked
    /// bytes and plain text live outside it (ADR-0005, ADR-0092), so re-resolve misses the update.
    /// Surfaces whose text fields had caret movement or input edits this turn.
    field_input_surfaces: Vec<String>,
    /// GTK's caret blink as (half a cycle, how long it blinks after input), `None` when the
    /// desktop turned it off. Read once at start.
    caret_blink: Option<(std::time::Duration, std::time::Duration)>,
    /// The last field input, which restarts the blink showing.
    caret_epoch: std::time::Instant,
    /// The phase the focused field was last queued to paint.
    caret_painted_on: bool,
    /// A compositor frame callback landed for a surface whose tree was mid-tween (ADR-0145). The
    /// poll loop takes it once per turn and advances every tween; `paint_surface` asks for the
    /// next one while anything is still moving, which is what keeps the chain alive and lets it
    /// die on its own when nothing is (ADR-0130 decision 3).
    animation_frame_due: bool,
    /// Surfaces actually drawn and swapped since the last idle-profile sample; paint walks all
    /// mapped surfaces and declines most, so the aggregate count matters.
    surfaces_drawn: usize,
    repaint_split: surface::RepaintSplit,
    current_egl_surface: Option<EglSurface>,
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// This SCTK (`smithay-client-toolkit-0.21.1`, checked against `src/`) ships exactly two
// `delegate_*` macros: `delegate_dispatch2!` and `delegate_registry!`. `PointerData`,
// `KeyboardData`, `WindowData`, `PopupData`, `GlobalData` and `SessionLockData`/
// `SessionLockSurfaceData` each carry a blanket `Dispatch2` impl, which the line below turns into
// the `Dispatch` half every bind/create call needs. So `PointerHandler`, `KeyboardHandler`,
// `WindowHandler`, `PopupHandler` and `SessionLockHandler` are implemented above with no
// `delegate_pointer!`/`delegate_keyboard!`/`delegate_xdg_shell!`/`delegate_xdg_popup!`/
// `delegate_session_lock!` call to match: none of those macros exist in this SCTK.
/// `ext_background_effect_manager_v1` (ADR-0195). The manager's one event is `capabilities`, a
/// bitfield the compositor sends on bind and again whenever it changes; the `blur` bit going away
/// means the compositor has stopped applying blur even for regions already set, so this tracks the
/// current value rather than the one at startup.
///
/// SCTK does the decode, which is why the manager is routed through it rather than dispatched
/// here. A compositor announcing `blur` beside a bit this build does not know sends a value
/// `Capability::from_bits` rejects. Reading that rejection as unsupported switches blur off over a
/// capability that has nothing to do with blur; `from_bits_retain` keeps the bit instead.
impl BackgroundEffectHandler for App {
    fn background_effect_state(&mut self) -> &mut BackgroundEffectState {
        &mut self.background_effect
    }

    fn update_capabilities(&mut self) {
        let blur = self
            .background_effect
            .capabilities()
            .is_some_and(|caps| caps.contains(ext_background_effect_manager_v1::Capability::Blur));
        if self.blur_supported == blur {
            return;
        }
        self.blur_supported = blur;
        // Every surface's last pushed region is now a lie in both directions: while support was
        // off nothing was sent, and when it goes off the compositor drops what it holds. Clearing
        // the record makes the next resolve push again rather than compare equal and skip.
        for surface in &mut self.surfaces {
            surface.last_blur_region.clear();
        }
    }
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

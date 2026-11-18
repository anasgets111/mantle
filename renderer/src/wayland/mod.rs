pub mod egl;

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;

use khronos_egl::Surface as EglSurface;
use mlua::{Function, Lua, Value};
use shared::{LockOutcome, LockReport, RendererFrame, SecureSubmit, Zeroize, error, warn};
use smithay_client_toolkit::background_effect::{BackgroundEffectHandler, BackgroundEffectState};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, data_device::DataDevice};
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
    OutputGeometry, SurfaceInstance, expand_instances, is_instance_of, reconcile_instances, warn_unmatched_outputs,
};
use crate::layout::node::{
    self, ConstraintAdjustment, DragPhase, Escape, KeyPress, KeyRelease, LayerKind, MouseButton, PanelSpec,
    PopupAnchor, PopupSpec, SizeHint, SizeMode, SubmitKey, SurfaceSpec, WindowSpec,
};
use crate::socket::RendererClient;
use crate::text::atlas::TextPainter;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

mod accessibility;
mod capture;
mod dmabuf;
mod egl_ext;
mod idle_profile;
mod input;
#[cfg(test)]
pub(crate) use input::apply_hover_write;
mod layer;
mod lock;
mod main_loop;
mod memory_profile;
mod output;
mod scale;
mod surface;
mod trim;
mod turn;
mod xdg_shell;

use input::keyboard::FocusedControl;
use input::{ArmedClick, ArmedSerial, ClickedNode, FocusedTextField, LastClick};
use surface::{TrackedSurface, log_bind_failure};

pub use main_loop::run;

/// `ctrl`, `shift`, `alt` and `logo` (Super) as the seat's keyboard last reported them.
#[derive(Clone, Copy, Default)]
pub(in crate::wayland) struct Held {
    ctrl: bool,
    shift: bool,
    alt: bool,
    logo: bool,
}

pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    scale_globals: Option<scale::ScaleGlobals>,
    seat_state: SeatState,
    data_device_manager: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    clipboard_sources: Vec<input::ClipboardSource>,
    clipboard_writers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    paste: Option<input::PendingPaste>,
    field_revision: u64,
    /// The focused field's edits, caret moves, mask and focus changes: what cancels a pending paste.
    field_edits: u64,
    text_input: input::TextInput,
    waker: crate::wake::Waker,
    layer_shell: LayerShell,
    /// `ext_background_effect_manager_v1` through SCTK's `GlobalProxy` (ADR-0195). A compositor
    /// without the global is not an error: `compositor_blur = true` there is silently nothing, the same answer
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
    /// worker and font set (ADR-0039 decision 3).
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
    /// The scroll under a touchpad's fingers, for the fling on lift.
    finger_scroll: Option<input::Fingers>,
    /// `wl_shm` only for SCTK's XCursor fallback when `wp_cursor_shape_v1` is absent; all other
    /// rendering uses EGL.
    shm: Shm,
    /// Advertised keyboard, kept alive and single-seat. Used only for `enter`/`leave`, the only
    /// client-visible result of `keyboard_interactivity`.
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Focused surface instance id (ADR-0050); `input::keyboard::focus_is_still_armed` requires a
    /// `secure_submit` field's declaring surface to match it.
    keyboard_focus: Option<String>,
    /// Press waiting for release (ADR-0050 decision 2, [`ArmedClick`]).
    armed: Option<ArmedClick>,
    /// The `on_double_click` node a press armed, waiting for its release.
    armed_double: Option<ClickedNode>,
    /// The last `on_double_click` node clicked and when and where it was released, which the next click may double.
    last_click: Option<LastClick>,
    /// Held left press on an `on_drag` node (ADR-0116 decision 1); `Motion` reports until release
    /// or `Leave`.
    drag: Option<input::ActiveDrag>,
    /// The serial for `xdg_popup.grab`, valid for one poll turn (ADR-0049 amendment).
    input_serial: Option<ArmedSerial>,
    /// The serial of this turn's key press, which a cut or copy from a callback needs.
    key_serial: Option<u32>,
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
    focused_secure_submit: Option<input::keyboard::SecureFocus>,
    /// Focused plain `textfield` and its draft, the unmasked half (ADR-0092). Only a
    /// press selects it; sole-field `enter` fallback cannot serve multiple reply boxes.
    ///
    /// Mutually exclusive with `focused_secure_submit`; the innermost textfield is one kind.
    focused_text_field: Option<FocusedTextField>,
    /// Text and selection of plain fields that lost focus to another field, until their node goes (ADR-0108).
    parked_drafts: HashMap<(String, layout::scene::NodeId), (String, (usize, usize))>,
    /// `initial_text` seeds of fields whose surface has no `wl_surface` yet; applied once it has one.
    pending_seeds: Vec<(layout::scene::NodeId, String)>,
    focused_control: Option<FocusedControl>,
    /// The `autofocus` control last seen, so one the user left is not re-armed until the keyboard re-enters.
    armed_control: Option<(String, layout::scene::NodeId)>,
    /// The surface `keyboard_interactivity` armed while no surface here held the keyboard; its focus
    /// waits for `enter` (#130).
    awaiting_keyboard: Option<String>,
    /// Bumped on every output event: an `"active"` panel refused since its build waits for the next.
    output_epoch: u64,
    /// Whether `focused_control` was last moved by Tab or an AT action, which alone draw the outline.
    focus_visible: bool,
    accessibility: accessibility::Accessibility,
    /// Modifiers on the seat's keyboard: Shift turns a caret motion or a press into a selection, Ctrl is read
    /// for Ctrl+A (ADR-0236), Alt and Super go to `on_key` alone.
    held: Held,
    /// The seat's repeat delay and interval, absent when the compositor turned repeat off. SCTK
    /// drives its own repeat from a calloop timer, which this renderer does not link (ADR-0124),
    /// so `poll`'s deadline carries it instead.
    repeat_info: Option<(std::time::Duration, std::time::Duration)>,
    /// The held key and when it next repeats.
    repeating: Option<(KeyEvent, std::time::Instant)>,
    /// Keys pressed while Lua could hear them, held for `on_key_release`.
    pressed_keys: Vec<input::keyboard::HeldPress>,
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
    /// Surfaces whose compositor frame callback landed while their tree was mid-tween (ADR-0145).
    /// The poll loop takes them once per turn and advances only their tweens, so each surface
    /// animates at its own output's refresh; `paint_surface` asks for the next one while anything
    /// is still moving, which is what keeps the chain alive and lets it die on its own when
    /// nothing is (ADR-0130 decision 3).
    animation_frames_due: Vec<String>,
    /// Surfaces actually drawn and swapped since the last idle-profile sample; paint walks all
    /// mapped surfaces and declines most, so the aggregate count matters.
    surfaces_drawn: usize,
    repaint_split: surface::RepaintSplit,
    current_egl_surface: Option<EglSurface>,
    /// `Some` for a reload's turn: its repaint swaps after drawing everything, then these ids go.
    reload_removals: Option<Vec<String>>,
    /// Something freed memory in bulk (startup, a reload, a surface's EGL buffers, a shed tree);
    /// the loop collects Lua garbage and trims glibc once things settle (`trim::Trim`).
    trim: trim::Trim,
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// SCTK 0.21 has only `delegate_registry!` and `delegate_dispatch2!`; the latter covers every
// pointer, keyboard, xdg and session-lock `*Data` through blanket `Dispatch2` impls.
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

#[cfg(test)]
mod test_support {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use wayland_client::EventQueue;
    use wayland_client::globals::registry_queue_init;

    use super::*;

    type WireLog = Arc<Mutex<Vec<(u32, u16, u32)>>>;

    pub(in crate::wayland) struct MockWayland {
        pub(in crate::wayland) requests: WireLog,
        lock_id: Arc<AtomicU32>,
        writer: Mutex<UnixStream>,
    }

    impl MockWayland {
        pub(in crate::wayland) fn commits_for(&self, surface_id: u32) -> usize {
            self.requests.lock().unwrap().iter().filter(|&&(id, op, _)| id == surface_id && op == 6).count()
        }

        pub(in crate::wayland) fn blur_regions_for(&self, effect_id: u32) -> Vec<u32> {
            let reqs = self.requests.lock().unwrap();
            reqs.iter().filter_map(|&(id, op, arg)| (id == effect_id && op == 1).then_some(arg)).collect()
        }

        /// Sends one event, `payload` already wire-encoded.
        pub(in crate::wayland) fn send(&self, object_id: u32, opcode: u16, payload: &[u8]) {
            let mut msg = Vec::new();
            push_wire_msg(&mut msg, object_id, opcode, payload);
            self.writer.lock().unwrap().write_all(&msg).unwrap();
        }

        pub(in crate::wayland) fn send_lock_finished(&self) {
            let id = self.lock_id.load(Ordering::Acquire);
            assert_ne!(id, 0);
            self.send(id, 1, &[]);
        }
    }

    fn push_wire_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&((s.len() + 1) as u32).to_ne_bytes());
        buf.extend_from_slice(s.as_bytes());
        buf.resize(buf.len() + 1 + (4 - (buf.len() + 1) % 4) % 4, 0);
    }

    fn push_wire_msg(buf: &mut Vec<u8>, object_id: u32, opcode: u16, payload: &[u8]) {
        let size = ((8 + payload.len()) as u32) << 16 | u32::from(opcode);
        buf.extend_from_slice(&object_id.to_ne_bytes());
        buf.extend_from_slice(&size.to_ne_bytes());
        buf.extend_from_slice(payload);
    }

    fn spawn_mock_wayland_server(mut stream: UnixStream, wire: Option<(WireLog, Arc<AtomicU32>)>) {
        std::thread::spawn(move || {
            let mut req = [0u8; 24]; // `wl_display.get_registry(2)` + `wl_display.sync(3)`
            if stream.read_exact(&mut req).is_err() {
                return;
            }
            let registry_id = u32::from_ne_bytes(req[8..12].try_into().unwrap());
            let callback_id = u32::from_ne_bytes(req[20..24].try_into().unwrap());
            let all_globals = [
                (1u32, "wl_compositor", 4u32),
                (2, "zwlr_layer_shell_v1", 1),
                (3, "wl_shm", 1),
                (4, "xdg_wm_base", 3),
                (5, "wl_output", 4),
                (6, "ext_background_effect_manager_v1", 1),
                (7, "ext_session_lock_manager_v1", 1),
                (8, "wl_seat", 7),
                (9, "wl_data_device_manager", 3),
            ];
            let mut reply = Vec::new();
            for &(name, iface, version) in &all_globals[..if wire.is_some() { all_globals.len() } else { 4 }] {
                let mut payload = name.to_ne_bytes().to_vec();
                push_wire_str(&mut payload, iface);
                payload.extend_from_slice(&version.to_ne_bytes());
                push_wire_msg(&mut reply, registry_id, 0, &payload);
            }
            push_wire_msg(&mut reply, callback_id, 0, &0u32.to_ne_bytes());
            push_wire_msg(&mut reply, 1, 1, &callback_id.to_ne_bytes());
            if stream.write_all(&reply).is_err() {
                return;
            }
            let Some((requests, lock_id)) = wire else {
                let mut discard = [0u8; 256];
                while matches!(stream.read(&mut discard), Ok(n) if n > 0) {}
                return;
            };
            let mut lock_mgr_id = None;
            let mut hdr = [0u8; 8];
            while stream.read_exact(&mut hdr).is_ok() {
                let object_id = u32::from_ne_bytes(hdr[0..4].try_into().unwrap());
                let word = u32::from_ne_bytes(hdr[4..8].try_into().unwrap());
                let opcode = (word & 0xffff) as u16;
                let mut payload = vec![0u8; ((word >> 16) as usize).saturating_sub(8)];
                if !payload.is_empty() && stream.read_exact(&mut payload).is_err() {
                    return;
                }
                let arg0 = payload.get(0..4).map_or(0, |b| u32::from_ne_bytes(b.try_into().unwrap()));
                let mut out = Vec::new();
                if object_id == 1 && opcode == 0 {
                    push_wire_msg(&mut out, arg0, 0, &0u32.to_ne_bytes());
                    push_wire_msg(&mut out, 1, 1, &arg0.to_ne_bytes());
                } else if object_id == registry_id && opcode == 0 && payload.len() >= 16 {
                    let slen = u32::from_ne_bytes(payload[4..8].try_into().unwrap()) as usize;
                    let new_id = u32::from_ne_bytes(payload[payload.len() - 4..].try_into().unwrap());
                    match payload.get(8..8 + slen.saturating_sub(1)).and_then(|b| std::str::from_utf8(b).ok()) {
                        Some("wl_output") => {
                            let mut name = Vec::new();
                            push_wire_str(&mut name, "TEST");
                            push_wire_msg(&mut out, new_id, 4, &name);
                            push_wire_msg(&mut out, new_id, 2, &[]);
                        }
                        Some("ext_background_effect_manager_v1") => {
                            push_wire_msg(&mut out, new_id, 0, &1u32.to_ne_bytes());
                        }
                        Some("ext_session_lock_manager_v1") => lock_mgr_id = Some(new_id),
                        _ => {}
                    }
                } else if lock_mgr_id == Some(object_id) && opcode == 1 {
                    lock_id.store(arg0, Ordering::Release);
                }
                requests.lock().unwrap().push((object_id, opcode, arg0));
                if !out.is_empty() && stream.write_all(&out).is_err() {
                    return;
                }
            }
        });
    }

    impl App {
        fn build_test_app(
            source: &str,
            with_wire: bool,
        ) -> (Self, tempfile::TempDir, EventQueue<App>, Option<MockWayland>) {
            let (client_stream, server_stream) = UnixStream::pair().unwrap();
            let mock = with_wire.then(|| MockWayland {
                requests: Arc::default(),
                lock_id: Arc::default(),
                writer: Mutex::new(server_stream.try_clone().unwrap()),
            });
            spawn_mock_wayland_server(
                server_stream,
                mock.as_ref().map(|m| (Arc::clone(&m.requests), Arc::clone(&m.lock_id))),
            );
            let conn = Connection::from_socket(client_stream).unwrap();
            let (globals, mut event_queue) = registry_queue_init::<App>(&conn).unwrap();
            let qh = event_queue.handle();

            let compositor_state = CompositorState::bind(&globals, &qh).unwrap();
            let layer_shell = LayerShell::bind(&globals, &qh).unwrap();
            let shm = Shm::bind(&globals, &qh).unwrap();
            let background_effect = BackgroundEffectState::new(&globals, &qh);
            let output_state = OutputState::new(&globals, &qh);
            let seat_state = SeatState::new(&globals, &qh);
            let session_lock_state = SessionLockState::new(&globals, &qh);
            let registry_state = RegistryState::new(&globals);
            let captures = CaptureRegistry::bind(&globals, &qh);
            let waker = crate::wake::Waker::new().unwrap();
            let shaping = ShapingHandle::spawn();
            let (dir, _path, client, _rx) = crate::socket::client::tests::client_for(source);
            let (outbound_tx, _app_rx) = tokio::sync::mpsc::unbounded_channel();

            let mut app = App {
                registry_state,
                output_state,
                compositor_state,
                scale_globals: None,
                seat_state,
                data_device_manager: DataDeviceManagerState::bind(&globals, &qh).ok(),
                data_device: None,
                clipboard_sources: Vec::new(),
                clipboard_writers: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                paste: None,
                field_revision: 0,
                field_edits: 0,
                text_input: input::TextInput::default(),
                waker: waker.clone(),
                layer_shell,
                background_effect,
                blur_supported: false,
                xdg_shell: XdgShell::bind(&globals, &qh).ok(),
                session_lock_state,
                session_lock: None,
                egl: None,
                gl: None,
                shader_stage: crate::layout::image_shader::ShaderStage::default(),
                conn,
                shaping: shaping.clone(),
                text_painter: None,
                image_cache: ImageCache::with_waker(waker.clone()).with_fonts(shaping),
                capture_cache: CaptureCache::default(),
                captures,
                client,
                surfaces: Vec::new(),
                exit: false,
                startup_complete: false,
                outbound_tx,
                generation_id: 0,
                queue_handle: qh.clone(),
                pointer: None,
                cursor_shown: None,
                pointer_at: None,
                finger_scroll: None,
                shm,
                keyboard: None,
                keyboard_focus: None,
                armed: None,
                armed_double: None,
                last_click: None,
                drag: None,
                input_serial: None,
                key_serial: None,
                pointer_input_count: 0,
                reposition_token: 0,
                focused_secure_submit: None,
                focused_text_field: None,
                parked_drafts: HashMap::new(),
                pending_seeds: Vec::new(),
                focused_control: None,
                armed_control: None,
                awaiting_keyboard: None,
                output_epoch: 0,
                focus_visible: false,
                accessibility: accessibility::Accessibility::new(waker),
                held: Held::default(),
                repeat_info: None,
                repeating: None,
                pressed_keys: Vec::new(),
                field_input_surfaces: Vec::new(),
                caret_blink: None,
                caret_epoch: std::time::Instant::now(),
                caret_painted_on: true,
                animation_frames_due: Vec::new(),
                surfaces_drawn: 0,
                repaint_split: surface::RepaintSplit::default(),
                current_egl_surface: None,
                reload_removals: None,
                trim: trim::Trim::default(),
            };

            if mock.is_some() {
                event_queue.roundtrip(&mut app).unwrap();
            }
            let outputs = vec![OutputGeometry {
                name: "TEST".to_string(),
                size: layout::LogicalSize { width: 1920.0, height: 1080.0 },
            }];
            let specs = app.client.run_startup_evaluation().unwrap();
            let instances = expand_instances(&specs, &outputs);
            app.client.set_instances(instances.clone());
            assert!(app.client.apply_instances());
            app.create_surfaces(&qh, &specs, &instances);
            app.startup_complete = true;
            (app, dir, event_queue, mock)
        }

        pub(in crate::wayland) fn test_app(source: &str) -> (Self, tempfile::TempDir) {
            let (app, dir, _, _) = Self::build_test_app(source, false);
            (app, dir)
        }

        /// The config's global `log` sequence, which test handlers append to.
        pub(in crate::wayland) fn lua_log(&self) -> Vec<String> {
            self.client.lua().load("return log").eval().unwrap()
        }

        /// Same as [`App::test_app`], with `wl_output`, `ext_background_effect_manager_v1` (Blur),
        /// `ext_session_lock_manager_v1`, a `wl_seat` that announces nothing, `wl_data_device_manager`,
        /// and a wire request log synchronized via `event_queue.roundtrip`.
        pub(in crate::wayland) fn test_app_with_wire(
            source: &str,
        ) -> (Self, tempfile::TempDir, EventQueue<App>, MockWayland) {
            let (app, dir, queue, mock) = Self::build_test_app(source, true);
            (app, dir, queue, mock.unwrap())
        }
    }
}

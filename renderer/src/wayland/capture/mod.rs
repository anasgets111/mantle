//! `capture` node protocol client (ADR-0248): ext-image-copy-capture-v1 first, wlr-screencopy
//! fallback or for a `region`. Hand-dispatched beside SCTK, like ADR-0009's text-input-v3. dma-buf negotiation
//! (ADR-0248 amendment) and texture lifecycle live in `gpu`, using `wayland::dmabuf`.
//!
//! Only the decision functions and teardown are unit tested; the rest is thin
//! protocol translation a mock isn't worth writing.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use femtovg::ImageId;
use khronos_egl as khr;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1, ext_image_copy_capture_session_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1;
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};

use shared::warn;

use crate::image::capture::{CaptureCache, DamageRect, crop_fraction};
use crate::layout::node::CaptureTarget;
use crate::layout::paint::CaptureNode;
use crate::layout::scene::NodeId;
use crate::text::snap::LogicalRect;

use super::App;
use super::dmabuf::{DmabufShape, DmabufSwapchain, FormatModifier};
use super::egl::EglState;

mod ext;
mod gpu;
mod shm;
mod windows;
mod wlr;

pub(super) use gpu::import_ready_dmabufs;
use gpu::{DmabufOffer, DmabufProbe, land_dmabuf_frame};
use shm::NegotiatedBuffer;

/// The capture protocols this compositor offers, bound once at startup (ADR-0248 decision 1).
struct Backend {
    ext: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    outputs: Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    windows: Option<windows::Windows>,
    wlr: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
}

/// Which protocol one source captures through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Ext,
    Wlr,
}

/// ext first (ADR-0248 decision 1), but a `region` prefers wlr, which crops at the source
/// (ADR-0263).
fn pick_protocol(region: bool, ext: bool, wlr: bool) -> Option<Protocol> {
    match (ext, wlr) {
        (_, true) if region || !ext => Some(Protocol::Wlr),
        (true, _) => Some(Protocol::Ext),
        _ => None,
    }
}

/// When a live source may next request: `1 / fps` after its previous request, so the
/// compositor's latency overlaps the wait instead of adding to it (ADR-0263). `None` is due now.
fn next_request_at(last_request: Option<Instant>, fps: f32) -> Option<Instant> {
    last_request.map(|last| last + Duration::from_secs_f32(1.0 / fps))
}

/// The grid slot a request made at `now` takes: its due time, so a late wake does not stretch the
/// period, or `now` once over a period late, so a stall does not burst.
fn request_slot(last_request: Option<Instant>, fps: f32, now: Instant) -> Instant {
    let period = Duration::from_secs_f32(1.0 / fps);
    next_request_at(last_request, fps).filter(|due| now.saturating_duration_since(*due) < period).unwrap_or(now)
}

/// The draw-time crop `region` needs on an ext source (ADR-0263), `None` on a rotated or flipped
/// output, whose buffer its logical coordinates do not map onto.
fn ext_crop(region: LogicalRect, output: (f32, f32), transform: wl_output::Transform) -> Option<LogicalRect> {
    (transform == wl_output::Transform::Normal).then(|| crop_fraction(region, output))
}

/// One `Done` batch, held while a frame is outstanding (ADR-0248 amendment decision 6).
struct DoneOffer {
    width: u32,
    height: u32,
    shm_format: Option<wl_shm::Format>,
    dmabuf_formats: Vec<FormatModifier>,
}

/// Ext protocol state for one source: the session persists across frames; `negotiating` and
/// `damage` are per-frame, set by events and consumed by `Done`/`Ready`.
#[derive(Default)]
struct ExtProto {
    session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    /// The `paint_cursor` this session was created with; a change needs a fresh session.
    paint_cursor: bool,
    /// Awaiting `Ready`/`Failed`; a second `create_frame` meanwhile is `duplicate_frame`.
    frame: Option<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1>,
    negotiating: Option<(u32, u32, Option<wl_shm::Format>)>,
    /// The last `Done`'s shm offer, kept while dma-buf runs so a dma-buf failure can fall back
    /// without a new `Done` (amendment decision 4).
    shm_negotiated: Option<(u32, u32, wl_shm::Format)>,
    buffer: Option<NegotiatedBuffer>,
    /// A `Done` that arrived while `frame` was outstanding, applied once it lands (amendment
    /// decision 6).
    pending_done: Option<DoneOffer>,
    damage: Vec<DamageRect>,
    dmabuf_formats: Vec<FormatModifier>,
}

/// wlr has no persistent session object; `buffer` is this source's own cross-frame state instead.
#[derive(Default)]
struct WlrProto {
    /// Awaiting `Ready`/`Failed`, destroyed with the source.
    frame: Option<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1>,
    negotiating: Option<(u32, u32, u32, wl_shm::Format)>,
    buffer: Option<NegotiatedBuffer>,
    damage: Vec<DamageRect>,
    /// wlr-screencopy names one fourcc per frame, no modifier list (`(width, height, fourcc)`);
    /// consumed at `BufferDone`.
    dmabuf_format: Option<(u32, u32, u32)>,
    /// Whether this frame copies into the dma-buf slot. `dmabuf_shape` is not enough: a frame
    /// with no `linux_dmabuf` offer copies through shm while the shape stays set.
    dmabuf_active: bool,
}

enum Proto {
    Ext(ExtProto),
    Wlr(WlrProto),
}

struct CaptureSource {
    target: CaptureTarget,
    /// Frames per second, `None` one-shot (ADR-0263).
    live: Option<f32>,
    paint_cursor: bool,
    region: Option<LogicalRect>,
    protocol: Protocol,
    /// A frame requested and not yet `Ready`/`Failed`: `sync_captures` requests another only once
    /// this clears (ADR-0248 decision 3).
    in_flight: bool,
    last_request: Option<Instant>,
    /// Wants a frame its cap does not yet allow; `CaptureRegistry::next_request_deadline` wakes for it.
    deferred: bool,
    /// Whether a frame has landed for the current target; a one-shot source reads this to want
    /// no more once it has one.
    captured: bool,
    proto: Option<Proto>,
    /// Stops a persistent failure from retrying every turn. Outputs retry on topology changes;
    /// windows need a new target or node, so a reused address cannot revive a closed source.
    failed: bool,
    warned_missing_output: bool,
    warned_rotated: bool,
    /// This source's dma-buf double buffer (ADR-0248 amendment decision 3). Unused while
    /// `dmabuf_shape` is `None`.
    dmabuf: DmabufSwapchain,
    /// The negotiated dma-buf shape, once one has been picked; `None` means this source draws
    /// through shm.
    dmabuf_shape: Option<DmabufShape>,
    /// Set on any dma-buf failure, permanent for this source (amendment decision 4): no env
    /// escape hatch, no retry short of the source being recreated for a new target.
    dmabuf_failed: bool,
}

impl CaptureSource {
    fn new(node: &CaptureNode, protocol: Protocol) -> Self {
        CaptureSource {
            target: node.target.clone(),
            live: node.live,
            paint_cursor: node.paint_cursor,
            region: node.region,
            protocol,
            in_flight: false,
            last_request: None,
            deferred: false,
            captured: false,
            proto: None,
            failed: false,
            warned_missing_output: false,
            warned_rotated: false,
            dmabuf: DmabufSwapchain::default(),
            dmabuf_shape: None,
            dmabuf_failed: false,
        }
    }

    /// Node IDs survive target replacement; only the current protocol objects may change it.
    fn owns(&self, proxy: &impl Proxy) -> bool {
        let id = proxy.id();
        match &self.proto {
            Some(Proto::Ext(ext)) => {
                ext.session.as_ref().is_some_and(|p| p.id() == id) || ext.frame.as_ref().is_some_and(|p| p.id() == id)
            }
            Some(Proto::Wlr(wlr)) => wlr.frame.as_ref().is_some_and(|p| p.id() == id),
            None => false,
        }
    }
}

/// Per-node capture sources for this generation's live `capture` nodes (ADR-0248).
pub(super) struct CaptureRegistry {
    backend: Backend,
    sources: HashMap<NodeId, CaptureSource>,
    /// Targets already warned as having no capture protocol, once each.
    warned_no_backend: HashSet<CaptureTarget>,
    /// `zwp_linux_dmabuf_v1`, only ever used for `create_params` (ADR-0248 amendment decision 1);
    /// `None` means this compositor cannot build a dma-buf `wl_buffer` at all.
    dmabuf_manager: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    dmabuf_probe: DmabufProbe,
    /// Nodes whose current front buffer has no texture yet; drained once the canvas's GL context
    /// is current (ADR-0039), by `import_ready_dmabufs`.
    pending_import: Vec<NodeId>,
    /// Textures and `EGLImage`s a discarded buffer left behind, freed at the same point.
    pending_free: Vec<(khr::Image, ImageId, glow::NativeTexture)>,
}

impl CaptureRegistry {
    /// Binds each protocol the compositor offers; [`pick_protocol`] chooses per source.
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<App>) -> Self {
        let ext = globals.bind(qh, 1..=1, ()).ok();
        let outputs = globals.bind(qh, 1..=1, ()).ok();
        let windows = windows::Windows::bind(globals, qh);
        let wlr = globals.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(qh, 1..=3, ()).ok();
        let backend = Backend { ext, outputs, windows, wlr };
        let dmabuf_manager = globals.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(qh, 2..=2, ()).ok();
        CaptureRegistry {
            backend,
            sources: HashMap::new(),
            warned_no_backend: HashSet::new(),
            dmabuf_manager,
            dmabuf_probe: DmabufProbe::default(),
            pending_import: Vec::new(),
            pending_free: Vec::new(),
        }
    }

    fn forget(&mut self, id: NodeId, cache: &mut CaptureCache) {
        if let Some(mut source) = self.sources.remove(&id) {
            self.pending_free.extend(source.dmabuf.take_textures());
            destroy_proto(source.proto);
            self.pending_import.retain(|pending| *pending != id);
            cache.forget(id);
        }
    }

    /// Matches `node`'s source to `protocol`, replacing one that differs; `None` when unsupported.
    fn reconcile_source(
        &mut self,
        node: &CaptureNode,
        protocol: Option<Protocol>,
        cache: &mut CaptureCache,
    ) -> Option<Protocol> {
        if self
            .sources
            .get(&node.node)
            .is_some_and(|source| source.target != node.target || Some(source.protocol) != protocol)
        {
            self.forget(node.node, cache);
        }
        let protocol = protocol?;
        let source = self.sources.entry(node.node).or_insert_with(|| CaptureSource::new(node, protocol));
        source.live = node.live;
        source.paint_cursor = node.paint_cursor;
        source.region = node.region;
        cache.set_target(node.node, node.target.clone());
        Some(protocol)
    }

    /// A stopped session. An output restarts on the next sync and keeps its last frame meanwhile;
    /// a closed window clears its texture and stays stopped (ADR-0298).
    fn stop(&mut self, id: NodeId, cache: &mut CaptureCache) {
        let Some(source) = self.sources.get_mut(&id) else { return };
        destroy_proto(source.proto.take());
        source.in_flight = false;
        if source.target.is_output() {
            return;
        }
        self.pending_free.extend(source.dmabuf.take_textures());
        source.dmabuf = DmabufSwapchain::default();
        source.dmabuf_shape = None;
        source.captured = false;
        source.failed = true;
        source.deferred = false;
        self.pending_import.retain(|pending| *pending != id);
        cache.forget(id);
    }

    /// The soonest a deferred source's cap allows its next request, for the loop's poll timeout.
    pub(super) fn next_request_deadline(&self) -> Option<Instant> {
        self.sources
            .values()
            .filter(|source| source.deferred)
            .filter_map(|source| next_request_at(source.last_request, source.live?))
            .min()
    }

    /// Gives a failed source another chance: an output topology change is the case decision 3's
    /// backoff exists for (unplug mid-capture), so it is also the signal to retry.
    pub(super) fn clear_failures(&mut self) {
        for source in self.sources.values_mut().filter(|source| source.target.is_output()) {
            source.failed = false;
            source.warned_missing_output = false;
        }
    }
}

impl App {
    /// Reconciles capture sources against this pass's `capture` nodes: drops ones no surface
    /// currently paints, creates ones newly appearing, and requests a frame for any idle source
    /// that wants one. Called after a repaint that drew, once `last_painted` has settled.
    pub(super) fn sync_captures(&mut self) {
        let mut wanted: HashMap<NodeId, CaptureNode> = HashMap::new();
        for surface in &self.surfaces {
            if let Some((_, list)) = &surface.last_painted {
                let mut nodes = Vec::new();
                list.capture_nodes(&mut nodes);
                for node in nodes {
                    wanted.insert(node.node, node);
                }
            }
        }

        let gone: Vec<NodeId> = self.captures.sources.keys().copied().filter(|id| !wanted.contains_key(id)).collect();
        for id in gone {
            self.captures.forget(id, &mut self.capture_cache);
        }

        let (has_ext, has_wlr) = (
            self.captures.backend.ext.is_some() && self.captures.backend.outputs.is_some(),
            self.captures.backend.wlr.is_some(),
        );
        let has_windows = self.captures.backend.ext.is_some() && self.captures.backend.windows.is_some();
        for (id, node) in wanted {
            let protocol = match &node.target {
                CaptureTarget::Output(_) => pick_protocol(node.region.is_some(), has_ext, has_wlr),
                CaptureTarget::Window(_) => has_windows.then_some(Protocol::Ext),
            };
            let Some(protocol) = self.captures.reconcile_source(&node, protocol, &mut self.capture_cache) else {
                let kind = if node.target.is_output() { "output" } else { "window" };
                if self.captures.warned_no_backend.insert(node.target.clone()) {
                    warn!("capture {kind} `{}` has no supported capture protocol; drawing nothing", node.target.name());
                }
                continue;
            };
            let info = match &node.target {
                CaptureTarget::Output(name) => {
                    self.wl_output_named(name).and_then(|output| self.output_state.info(&output))
                }
                CaptureTarget::Window(_) => None,
            };
            let crop = match (protocol, node.region, info) {
                (Protocol::Ext, Some(region), Some(info)) => {
                    let (width, height) = info.logical_size.unwrap_or_default();
                    let crop = ext_crop(region, (width as f32, height as f32), info.transform);
                    if crop.is_none()
                        && let Some(source) = self.captures.sources.get_mut(&id)
                        && !std::mem::replace(&mut source.warned_rotated, true)
                    {
                        warn!(
                            "capture on `{}` cannot crop a rotated or flipped output; drawing all of it",
                            node.target.name()
                        );
                    }
                    crop
                }
                _ => None,
            };
            self.capture_cache.set_crop(id, crop);
            let source = &self.captures.sources[&id];
            if !source.failed && !source.in_flight && (node.live.is_some() || !source.captured) {
                self.request_when_due(id);
            }
        }
    }

    /// Requests `id`'s next frame now if its cap allows, else defers it to
    /// [`CaptureRegistry::next_request_deadline`].
    fn request_when_due(&mut self, id: NodeId) {
        let Some(source) = self.captures.sources.get_mut(&id) else { return };
        let fps = source.live.unwrap_or(f32::INFINITY);
        source.deferred = next_request_at(source.last_request, fps).is_some_and(|at| at > Instant::now());
        if !source.deferred {
            self.request_frame(id);
        }
    }

    /// Requests every deferred frame whose cap now allows it. Called once per loop turn.
    pub(super) fn request_due_captures(&mut self) {
        let deferred: Vec<NodeId> = self
            .captures
            .sources
            .iter()
            .filter(|(_, source)| source.deferred && source.live.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in deferred {
            self.request_when_due(id);
        }
    }

    /// The output named `name`, resolved like `panel.monitor` (ADR-0246).
    fn wl_output_named(&self, name: &str) -> Option<wl_output::WlOutput> {
        self.output_state.outputs().enumerate().find_map(|(index, output)| {
            let info = self.output_state.info(&output)?;
            (super::output::output_name(index, info.name.as_deref()) == name).then_some(output)
        })
    }

    /// Starts one capture request for `id`, whose pacing already said it wants one. An ext source
    /// with a live session of the same `paint_cursor` reuses it; anything else starts fresh.
    fn request_frame(&mut self, id: NodeId) {
        let Some(target) = self.captures.sources.get(&id).map(|source| source.target.clone()) else { return };
        let (output, window) = match &target {
            CaptureTarget::Output(name) => (self.wl_output_named(name), None),
            CaptureTarget::Window(name) => {
                (None, self.captures.backend.windows.as_ref().and_then(|windows| windows.handle(name)))
            }
        };
        if output.is_none() && window.is_none() {
            if let Some(source) = self.captures.sources.get_mut(&id)
                && !std::mem::replace(&mut source.warned_missing_output, true)
            {
                warn!("capture target `{}` is not available; drawing nothing", target.name());
            }
            return;
        }
        let Some(source) = self.captures.sources.get_mut(&id) else { return };
        source.in_flight = true;
        source.last_request =
            Some(request_slot(source.last_request, source.live.unwrap_or(f32::INFINITY), Instant::now()));
        let (paint_cursor, protocol, region) = (source.paint_cursor, source.protocol, source.region);
        // `in_flight` was false, so no frame holds the session being replaced.
        if matches!(&source.proto, Some(Proto::Ext(ext)) if ext.paint_cursor != paint_cursor)
            && let Some(Proto::Ext(ext)) = source.proto.take()
            && let Some(session) = ext.session
        {
            session.destroy();
        }
        let existing_session = match &source.proto {
            Some(Proto::Ext(ext)) => ext.session.clone(),
            _ => None,
        };
        let qh = self.queue_handle.clone();

        if let Some(session) = existing_session {
            self.create_ext_frame(id, &session);
            return;
        }

        match (protocol, &self.captures.backend) {
            (Protocol::Ext, Backend { ext: Some(manager), outputs, windows, .. }) => {
                let capture_source = match (&output, &window, outputs, windows) {
                    (Some(output), _, Some(outputs), _) => outputs.create_source(output, &qh, id),
                    (_, Some(window), _, Some(windows)) => windows.sources.create_source(window, &qh, id),
                    _ => return,
                };
                let options = if paint_cursor {
                    ext_image_copy_capture_manager_v1::Options::PaintCursors
                } else {
                    ext_image_copy_capture_manager_v1::Options::empty()
                };
                let session = manager.create_session(&capture_source, options, &qh, id);
                capture_source.destroy();
                if let Some(source) = self.captures.sources.get_mut(&id) {
                    source.proto =
                        Some(Proto::Ext(ExtProto { session: Some(session), paint_cursor, ..Default::default() }));
                }
            }
            (Protocol::Wlr, Backend { wlr: Some(manager), .. }) => {
                let Some(output) = output else { return };
                let overlay_cursor = i32::from(paint_cursor);
                let frame = match region {
                    // Output-logical pixels; the compositor scales to buffer pixels and clips.
                    Some(r) => manager.capture_output_region(
                        overlay_cursor,
                        &output,
                        r.x.round() as i32,
                        r.y.round() as i32,
                        r.width.round().max(1.0) as i32,
                        r.height.round().max(1.0) as i32,
                        &qh,
                        id,
                    ),
                    None => manager.capture_output(overlay_cursor, &output, &qh, id),
                };
                if let Some(source) = self.captures.sources.get_mut(&id) {
                    // A fresh `Proto::Wlr` only when none exists yet: overwriting it every
                    // request drops the negotiated buffer and reallocates its pool per frame.
                    if !matches!(source.proto, Some(Proto::Wlr(_))) {
                        source.proto = Some(Proto::Wlr(WlrProto::default()));
                    }
                    if let Some(Proto::Wlr(wlr)) = source.proto.as_mut() {
                        wlr.frame = Some(frame);
                    }
                }
            }
            _ => {}
        }
    }

    fn request_next_if_live(&mut self, id: NodeId) {
        if self.captures.sources.get(&id).is_some_and(|source| source.live.is_some()) {
            self.request_when_due(id);
        }
    }
}

/// Destroys an outstanding frame before its session: left alive, its `Failed` would reach whatever
/// source takes the same `NodeId` next.
fn destroy_proto(proto: Option<Proto>) {
    match proto {
        Some(Proto::Ext(ext)) => {
            ext.frame.iter().for_each(ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1::destroy);
            ext.session.iter().for_each(ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1::destroy);
        }
        Some(Proto::Wlr(wlr)) => wlr.frame.iter().for_each(zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1::destroy),
        None => {}
    }
}

// The three manager globals have no events of their own; request-only.
delegate_noop!(App: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
delegate_noop!(App: ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(App: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

/// Marks `id`'s source failed (ADR-0248 decision 3's backoff): idle and warned once. Outputs retry on topology changes; windows need a new target or node. Shared by the ext and wlr `Failed` handlers.
fn fail_source(captures: &mut CaptureRegistry, id: &NodeId) {
    let Some(source) = captures.sources.get_mut(id) else { return };
    match source.proto.as_mut() {
        Some(Proto::Ext(ext)) => {
            ext.damage.clear();
            ext.frame = None;
            ext.pending_done = None;
        }
        Some(Proto::Wlr(wlr)) => {
            wlr.damage.clear();
            wlr.frame = None;
        }
        None => {}
    }
    source.in_flight = false;
    if !source.failed {
        warn!("capture on `{}` failed; pausing capture", source.target.name());
    }
    source.failed = true;
}

/// A frame failed on `buffer_constraints` (a resize): the `Done` parked behind it is a
/// renegotiation, not a failure. Returns that offer, or `None` after re-arming normal pacing.
fn constraints_changed(captures: &mut CaptureRegistry, id: &NodeId) -> Option<DoneOffer> {
    let source = captures.sources.get_mut(id)?;
    let Some(Proto::Ext(ext)) = source.proto.as_mut() else { return None };
    ext.damage.clear();
    ext.frame = None;
    let offer = ext.pending_done.take();
    // The offer's frame keeps `in_flight` until it lands.
    source.in_flight = offer.is_some();
    offer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> CaptureRegistry {
        CaptureRegistry {
            backend: Backend { ext: None, outputs: None, windows: None, wlr: None },
            sources: HashMap::new(),
            warned_no_backend: HashSet::new(),
            dmabuf_manager: None,
            dmabuf_probe: DmabufProbe::Pending,
            pending_import: Vec::new(),
            pending_free: Vec::new(),
        }
    }

    #[test]
    fn first_dmabuf_frame_requests_a_paint_before_texture_import() {
        let mut registry = registry();
        let mut cache = CaptureCache::default();
        let node = CaptureNode {
            node: NodeId::test(7),
            target: CaptureTarget::Window("0xa11ce".into()),
            live: None,
            paint_cursor: false,
            region: None,
        };
        assert!(registry.reconcile_source(&node, Some(Protocol::Ext), &mut cache).is_some());
        assert!(cache.poll().is_empty());
        land_dmabuf_frame(&mut registry, &mut cache, &node.node);
        assert!(cache.get(node.node).is_none());
        assert_eq!(registry.pending_import, [node.node]);
        assert_eq!(cache.poll(), [node.node], "an idle surface must paint before it can import its first texture");
    }

    #[test]
    fn switching_capture_targets_retires_old_pixels_even_when_the_new_backend_is_missing() {
        let mut registry = registry();
        let mut cache = CaptureCache::default();
        let mut node = CaptureNode {
            node: NodeId::test(7),
            target: CaptureTarget::Output("DP-1".into()),
            live: Some(30.0),
            paint_cursor: false,
            region: None,
        };
        assert!(registry.reconcile_source(&node, Some(Protocol::Wlr), &mut cache).is_some());
        registry.sources.get_mut(&node.node).unwrap().captured = true;
        registry.pending_import.push(node.node);
        node.target = CaptureTarget::Window("0xa11ce".into());
        assert!(registry.reconcile_source(&node, None, &mut cache).is_none());
        assert!(registry.sources.is_empty());
        assert!(registry.pending_import.is_empty());
        assert_eq!(cache.poll(), [node.node]);
        assert!(registry.reconcile_source(&node, None, &mut cache).is_none());
        assert!(cache.poll().is_empty(), "an unsupported target cannot create a repaint loop");
        assert!(registry.reconcile_source(&node, Some(Protocol::Ext), &mut cache).is_some());
        assert!(!registry.sources[&node.node].captured);
        registry.sources.get_mut(&node.node).unwrap().failed = true;
        registry.clear_failures();
        assert!(registry.sources[&node.node].failed, "an output change must not revive a closed window address");
        node.target = CaptureTarget::Window("0xb0b".into());
        assert!(registry.reconcile_source(&node, Some(Protocol::Ext), &mut cache).is_some());
        assert!(!registry.sources[&node.node].failed);
        assert_eq!(registry.sources[&node.node].live, Some(30.0));
    }

    /// A resize fails the in-flight frame on `buffer_constraints`; the parked `Done` renegotiates
    /// instead of pausing, for windows and outputs alike.
    #[test]
    fn a_buffer_constraints_failure_renegotiates_instead_of_failing() {
        for target in [CaptureTarget::Window("0xa11ce".into()), CaptureTarget::Output("DP-1".into())] {
            let mut registry = registry();
            let mut cache = CaptureCache::default();
            let node =
                CaptureNode { node: NodeId::test(7), target, live: Some(30.0), paint_cursor: false, region: None };
            assert!(registry.reconcile_source(&node, Some(Protocol::Ext), &mut cache).is_some());
            let source = registry.sources.get_mut(&node.node).unwrap();
            source.in_flight = true;
            let offer = DoneOffer { width: 640, height: 480, shm_format: None, dmabuf_formats: Vec::new() };
            source.proto = Some(Proto::Ext(ExtProto { pending_done: Some(offer), ..Default::default() }));
            let offer = constraints_changed(&mut registry, &node.node).expect("the parked offer");
            assert_eq!((offer.width, offer.height), (640, 480));
            let source = &registry.sources[&node.node];
            assert!(!source.failed && source.in_flight, "{:?}: the renegotiated frame is in flight", node.target);
            assert!(constraints_changed(&mut registry, &node.node).is_none());
            let source = &registry.sources[&node.node];
            assert!(!source.failed && !source.in_flight, "{:?}: no offer re-arms normal pacing", node.target);
        }
    }

    /// A stopped output session restarts on the next sync and keeps its last frame; a stopped
    /// window clears it and stays stopped (ADR-0298).
    #[test]
    fn a_stopped_output_restarts_and_a_stopped_window_stays_stopped() {
        let mut registry = registry();
        let mut cache = CaptureCache::default();
        let output = NodeId::test(1);
        let window = NodeId::test(2);
        for (node, target) in
            [(output, CaptureTarget::Output("DP-1".into())), (window, CaptureTarget::Window("0xa11ce".into()))]
        {
            let node = CaptureNode { node, target, live: Some(30.0), paint_cursor: false, region: None };
            assert!(registry.reconcile_source(&node, Some(Protocol::Ext), &mut cache).is_some());
            let source = registry.sources.get_mut(&node.node).unwrap();
            source.captured = true;
            source.in_flight = true;
            source.proto = Some(Proto::Ext(ExtProto::default()));
        }
        registry.stop(output, &mut cache);
        let source = &registry.sources[&output];
        assert!(source.proto.is_none() && !source.in_flight);
        assert!(!source.failed && source.captured, "the next sync opens a fresh session");
        assert!(cache.poll().is_empty(), "the last output frame stays on screen");
        registry.stop(window, &mut cache);
        let source = &registry.sources[&window];
        assert!(source.proto.is_none() && !source.in_flight);
        assert!(source.failed && !source.captured);
        assert_eq!(cache.poll(), [window], "a closed window's pixels are retired");
    }

    /// A late wake keeps the request on the 1/fps grid; over a period late, the grid restarts at
    /// the wake rather than bursting to catch up.
    #[test]
    fn a_capped_source_requests_on_a_fixed_grid() {
        let t0 = Instant::now();
        let period = Duration::from_secs_f32(1.0 / 60.0);
        let ms = Duration::from_millis;
        assert_eq!(request_slot(None, 60.0, t0), t0, "first request");
        assert_eq!(request_slot(Some(t0), 60.0, t0 + period + ms(1)), t0 + period, "woke 1 ms late");
        assert_eq!(request_slot(Some(t0), 60.0, t0 + period * 2 + ms(1)), t0 + period * 2 + ms(1), "reset");
        assert_eq!(request_slot(Some(t0), f32::INFINITY, t0 + ms(3)), t0 + ms(3), "`true` has no grid");
    }

    #[derive(Default)]
    struct Probe;
    delegate_noop!(Probe: ignore wayland_client::protocol::wl_registry::WlRegistry);
    delegate_noop!(Probe: ignore wl_output::WlOutput);
    delegate_noop!(Probe: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
    delegate_noop!(Probe: ignore zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1);
    delegate_noop!(Probe: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
    delegate_noop!(Probe: ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
    delegate_noop!(Probe: ext_image_capture_source_v1::ExtImageCaptureSourceV1);
    delegate_noop!(Probe: ignore ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1);
    delegate_noop!(Probe: ignore ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1);

    /// A frame left alive after its source is replaced would deliver `Failed` to the new source
    /// under the same `NodeId`. Reads the requests off the wire as `(object id, opcode)`.
    #[test]
    fn tearing_down_a_proto_destroys_its_outstanding_frames_before_the_session() {
        use std::io::Read;
        use wayland_client::Proxy;
        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let qh = conn.new_event_queue::<Probe>().handle();
        let registry = conn.display().get_registry(&qh, ());
        let output: wl_output::WlOutput = registry.bind(1, 1, &qh, ());
        let wlr: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 = registry.bind(2, 1, &qh, ());
        let ext: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1 = registry.bind(3, 1, &qh, ());
        let sources: ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1 =
            registry.bind(4, 1, &qh, ());
        let session = ext.create_session(
            &sources.create_source(&output, &qh, ()),
            ext_image_copy_capture_manager_v1::Options::empty(),
            &qh,
            (),
        );
        let ext_frame = session.create_frame(&qh, ());
        let wlr_frame = wlr.capture_output(0, &output, &qh, ());
        let (ext_frame_id, session_id, wlr_frame_id) =
            (ext_frame.id().protocol_id(), session.id().protocol_id(), wlr_frame.id().protocol_id());
        conn.flush().unwrap();
        let mut sink = vec![0; 1 << 16];
        server.set_nonblocking(true).unwrap();
        let _ = server.read(&mut sink);

        let node = CaptureNode {
            node: NodeId::test(3),
            target: CaptureTarget::Window("0xa11ce".into()),
            live: None,
            paint_cursor: false,
            region: None,
        };
        let mut source = CaptureSource::new(&node, Protocol::Ext);
        source.proto = Some(Proto::Ext(ExtProto {
            session: Some(session.clone()),
            frame: Some(ext_frame.clone()),
            ..Default::default()
        }));
        assert!(source.owns(&session));
        assert!(source.owns(&ext_frame));
        source.proto = Some(Proto::Wlr(WlrProto { frame: Some(wlr_frame.clone()), ..Default::default() }));
        assert!(!source.owns(&session), "old-session negotiation/stopped events are ignored");
        assert!(!source.owns(&ext_frame), "old-frame ready/failed events are ignored");
        assert!(source.owns(&wlr_frame));
        source.proto = None;
        assert!(!source.owns(&wlr_frame), "closed sources ignore late callbacks");
        let ext_proto = ExtProto { session: Some(session), frame: Some(ext_frame), ..Default::default() };
        destroy_proto(Some(Proto::Ext(ext_proto)));
        destroy_proto(Some(Proto::Wlr(WlrProto { frame: Some(wlr_frame), ..Default::default() })));
        conn.flush().unwrap();
        let read = server.read(&mut sink).unwrap();
        let mut sent = Vec::new();
        let mut at = 0;
        while at + 8 <= read {
            let word = |i: usize| u32::from_ne_bytes(sink[i..i + 4].try_into().unwrap());
            sent.push((word(at), word(at + 4) & 0xffff));
            at += (word(at + 4) >> 16) as usize;
        }
        // ext frame `destroy` is opcode 0, session `destroy` 1, wlr frame `destroy` 1.
        assert_eq!(sent, vec![(ext_frame_id, 0), (session_id, 1), (wlr_frame_id, 1)]);
    }

    #[test]
    fn a_rotated_or_flipped_output_gets_no_ext_crop() {
        let region = LogicalRect { x: 860.0, y: 360.0, width: 1720.0, height: 720.0 };
        let crop = LogicalRect { x: 0.25, y: 0.25, width: 0.5, height: 0.5 };
        assert_eq!(ext_crop(region, (3440.0, 1440.0), wl_output::Transform::Normal), Some(crop));
        for transform in [wl_output::Transform::_90, wl_output::Transform::_180, wl_output::Transform::Flipped] {
            assert_eq!(ext_crop(region, (1440.0, 3440.0), transform), None, "{transform:?}");
        }
    }

    #[test]
    fn a_region_prefers_the_protocol_that_crops_at_the_source() {
        assert_eq!(pick_protocol(true, true, true), Some(Protocol::Wlr));
        assert_eq!(pick_protocol(false, true, true), Some(Protocol::Ext));
        assert_eq!(pick_protocol(true, true, false), Some(Protocol::Ext), "ext crops at draw time");
        assert_eq!(pick_protocol(false, false, true), Some(Protocol::Wlr));
        assert_eq!(pick_protocol(true, false, false), None);
    }
}

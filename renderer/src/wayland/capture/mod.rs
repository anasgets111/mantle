//! `capture` node protocol client (ADR-0248): ext-image-copy-capture-v1 first, wlr-screencopy
//! fallback or for a `region`. Hand-dispatched beside SCTK, like ADR-0009's text-input-v3. dma-buf negotiation
//! (ADR-0248 amendment) lives in `wayland::dmabuf`; this module only decides when to attempt it.
//!
//! Only the decision functions and teardown are unit tested; the rest is thin
//! protocol translation a mock isn't worth writing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use femtovg::ImageId;
use khronos_egl as khr;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, QueueHandle, delegate_noop};
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
use crate::layout::paint::CaptureNode;
use crate::layout::scene::NodeId;
use crate::text::snap::LogicalRect;

use super::App;
use super::dmabuf::{self, DmabufBuffer, DmabufShape, DmabufSupport, DmabufSwapchain, FormatModifier};
use super::egl::EglState;

mod ext;
mod shm;
mod wlr;

use shm::NegotiatedBuffer;

/// The capture protocols this compositor offers, bound once at startup (ADR-0248 decision 1).
struct Backend {
    ext: Option<(
        ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    )>,
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
    output: String,
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
    /// Whether a frame has landed for the current `output`; a one-shot source reads this to want
    /// no more once it has one.
    captured: bool,
    proto: Option<Proto>,
    /// Set on `Failed`, to stop a persistent failure from retrying every turn. Cleared when the
    /// output list changes, since that is when a failure is likely to have a different answer.
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
    /// escape hatch, no retry short of the source being recreated for a new `output`.
    dmabuf_failed: bool,
}

impl CaptureSource {
    fn new(node: &CaptureNode, protocol: Protocol) -> Self {
        CaptureSource {
            output: node.output.clone(),
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

    /// Marks this source's dma-buf attempt permanently failed (amendment decision 4): warned
    /// once, shm from here on until the source is recreated for a new `output`.
    fn fail_dmabuf(&mut self, why: &str) {
        if !self.dmabuf_failed {
            warn!("capture on `{}` {why}; using shm", self.output);
        }
        self.dmabuf_failed = true;
        self.dmabuf_shape = None;
    }
}

/// What a compositor offered for one source's negotiation batch (ADR-0248 amendment decision 1).
struct DmabufOffer<'a> {
    id: NodeId,
    width: u32,
    height: u32,
    offered: &'a [FormatModifier],
}

/// Process-wide dma-buf capability, probed once EGL exists and memoized: `Pending` retries on the
/// next negotiation, `Unsupported`/`Supported` are final for the process's lifetime.
#[derive(Default)]
enum DmabufProbe {
    #[default]
    Pending,
    Unsupported,
    Supported(DmabufSupport),
}

/// Per-node capture sources for this generation's live `capture` nodes (ADR-0248).
pub(super) struct CaptureRegistry {
    backend: Backend,
    sources: HashMap<NodeId, CaptureSource>,
    warned_no_backend: bool,
    /// `zwp_linux_dmabuf_v1`, only ever used for `create_params` (ADR-0248 amendment decision 1);
    /// `None` means this compositor cannot build a dma-buf `wl_buffer` at all.
    dmabuf_manager: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    dmabuf_probe: DmabufProbe,
    /// Nodes whose current front buffer has no texture yet; drained once the canvas's GL context
    /// is current (ADR-0039), by `App::import_ready_dmabufs`.
    pending_import: Vec<NodeId>,
    /// Textures and `EGLImage`s a discarded buffer left behind, freed at the same point.
    pending_free: Vec<(khr::Image, ImageId, glow::NativeTexture)>,
}

impl CaptureRegistry {
    /// Binds each protocol the compositor offers; [`pick_protocol`] chooses per source.
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<App>) -> Self {
        let ext = globals
            .bind::<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1, _, _>(qh, 1..=1, ())
            .ok()
            .zip(
                globals
                    .bind::<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1, _, _>(
                        qh,
                        1..=1,
                        (),
                    )
                    .ok(),
            );
        let wlr = globals.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(qh, 1..=3, ()).ok();
        let backend = Backend { ext, wlr };
        let dmabuf_manager = globals.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(qh, 2..=2, ()).ok();
        CaptureRegistry {
            backend,
            sources: HashMap::new(),
            warned_no_backend: false,
            dmabuf_manager,
            dmabuf_probe: DmabufProbe::default(),
            pending_import: Vec::new(),
            pending_free: Vec::new(),
        }
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
        for source in self.sources.values_mut() {
            source.failed = false;
            source.warned_missing_output = false;
        }
    }

    /// Probes process-wide dma-buf support the first time EGL exists; a `None` `egl` (too early
    /// in startup) leaves the probe pending for the next call instead of failing it outright.
    fn ensure_probed(&mut self, egl: Option<&EglState>) {
        if matches!(self.dmabuf_probe, DmabufProbe::Pending)
            && let Some(egl) = egl
        {
            self.dmabuf_probe = match DmabufSupport::probe(egl) {
                Some(support) => DmabufProbe::Supported(support),
                None => {
                    warn!("this GPU/driver offers no usable dma-buf import; `capture` nodes use the slower shm path");
                    DmabufProbe::Unsupported
                }
            };
        }
    }

    fn support(&self) -> Option<&DmabufSupport> {
        match &self.dmabuf_probe {
            DmabufProbe::Supported(support) => Some(support),
            _ => None,
        }
    }

    /// Picks a format from `offer.offered` and allocates `offer.id`'s back slot for it (ADR-0248
    /// amendment decisions 1 and 3). `false` means shm should run instead this round; if dma-buf
    /// support exists at all, that also marks the source permanently shm-only, warned once
    /// (amendment decision 4).
    fn negotiate_dmabuf(&mut self, egl: Option<&EglState>, qh: &QueueHandle<App>, offer: DmabufOffer<'_>) -> bool {
        let DmabufOffer { id, width, height, offered } = offer;
        self.ensure_probed(egl);
        let support_available = matches!(self.dmabuf_probe, DmabufProbe::Supported(_));
        let previously_failed = self.sources.get(&id).is_some_and(|source| source.dmabuf_failed);
        if !(support_available && !previously_failed) || self.dmabuf_manager.is_none() {
            return false;
        }
        let (DmabufProbe::Supported(support), Some(egl)) = (&self.dmabuf_probe, egl) else { return false };
        let picked = (!offered.is_empty())
            .then(|| {
                let mut seen = std::collections::HashSet::new();
                let importable: Vec<FormatModifier> = offered
                    .iter()
                    .filter(|candidate| seen.insert(candidate.fourcc))
                    .flat_map(|candidate| support.importable_modifiers(egl, candidate.fourcc))
                    .collect();
                dmabuf::pick_dmabuf_format(offered, &importable)
            })
            .flatten();
        let Some(source) = self.sources.get_mut(&id) else { return false };
        let Some(format) = picked else {
            source.fail_dmabuf("cannot use dma-buf (no shared format)");
            return false;
        };
        source.dmabuf_shape = Some(DmabufShape { width, height, format });
        self.ensure_dmabuf_back(qh, id)
    }

    /// Re-checks `id`'s already-negotiated dma-buf shape (the routine per-frame path once
    /// negotiation has run once): a no-op unless the back slot has never been allocated.
    fn ensure_dmabuf_back(&mut self, qh: &QueueHandle<App>, id: NodeId) -> bool {
        let CaptureRegistry { sources, dmabuf_manager, pending_free, dmabuf_probe, .. } = self;
        let Some(source) = sources.get_mut(&id) else { return false };
        let Some(shape) = source.dmabuf_shape else { return false };
        let (DmabufProbe::Supported(support), Some(manager)) = (&*dmabuf_probe, dmabuf_manager.as_ref()) else {
            return false;
        };
        let (ok, discarded) = source.dmabuf.ensure_back(shape, || dmabuf::allocate(support, manager, qh, shape));
        if let Some(mut discarded) = discarded
            && let Some(freed) = discarded.take_texture()
        {
            pending_free.push(freed);
        }
        if !ok {
            source.fail_dmabuf("failed a dma-buf allocation");
        }
        ok
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
            if let Some(mut source) = self.captures.sources.remove(&id) {
                self.captures.pending_free.extend(source.dmabuf.take_textures());
                destroy_proto(source.proto);
            }
            self.capture_cache.forget(id);
        }

        let (has_ext, has_wlr) = (self.captures.backend.ext.is_some(), self.captures.backend.wlr.is_some());
        if !has_ext && !has_wlr {
            if !wanted.is_empty() && !self.captures.warned_no_backend {
                warn!(
                    "a `capture` node is declared but this compositor offers no screencopy protocol; drawing nothing"
                );
                self.captures.warned_no_backend = true;
            }
            return;
        }

        for (id, node) in wanted {
            let Some(protocol) = pick_protocol(node.region.is_some(), has_ext, has_wlr) else { continue };
            let source = self.captures.sources.entry(id).or_insert_with(|| CaptureSource::new(&node, protocol));
            source.live = node.live;
            source.paint_cursor = node.paint_cursor;
            source.region = node.region;
            if source.output != node.output || source.protocol != protocol {
                let mut stale = std::mem::replace(source, CaptureSource::new(&node, protocol));
                self.captures.pending_free.extend(stale.dmabuf.take_textures());
                destroy_proto(stale.proto);
                self.capture_cache.forget(id);
            }
            let info = self.wl_output_named(&node.output).and_then(|output| self.output_state.info(&output));
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
                            node.output
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
        self.output_state
            .outputs()
            .find(|output| self.output_state.info(output).and_then(|info| info.name).as_deref() == Some(name))
    }

    /// Starts one capture request for `id`, whose pacing already said it wants one. An ext source
    /// with a live session of the same `paint_cursor` reuses it; anything else starts fresh.
    fn request_frame(&mut self, id: NodeId) {
        let Some(output_name) = self.captures.sources.get(&id).map(|source| source.output.clone()) else {
            return;
        };
        let Some(output) = self.wl_output_named(&output_name) else {
            if let Some(source) = self.captures.sources.get_mut(&id)
                && !source.warned_missing_output
            {
                warn!("capture node names output `{output_name}`, which is not connected; drawing nothing");
                source.warned_missing_output = true;
            }
            return;
        };
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
            (Protocol::Ext, Backend { ext: Some((manager, sources)), .. }) => {
                let capture_source = sources.create_source(&output, &qh, id);
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

/// Imports every dma-buf frame that landed with no texture yet, and frees any texture a discarded
/// buffer left behind. Must run with the canvas's GL context current (ADR-0039); a free function,
/// not an `App` method, so `wayland::surface::paint_surface` can call it while its own borrow of
/// `self.text_painter` supplies `canvas`.
pub(super) fn import_ready_dmabufs(
    captures: &mut CaptureRegistry,
    capture_cache: &mut CaptureCache,
    egl: Option<&EglState>,
    gl: Option<&glow::Context>,
    canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
) {
    if let (Some(egl), Some(gl)) = (egl, gl) {
        for freed in std::mem::take(&mut captures.pending_free) {
            dmabuf::free_texture(egl, gl, canvas, freed);
        }
    }
    let pending = std::mem::take(&mut captures.pending_import);
    if pending.is_empty() {
        return;
    }
    let (Some(egl), Some(gl)) = (egl, gl) else { return };
    let CaptureRegistry { sources, dmabuf_probe, .. } = captures;
    let DmabufProbe::Supported(support) = dmabuf_probe else { return };
    for id in pending {
        let Some(source) = sources.get_mut(&id) else { continue };
        let Some(buffer) = source.dmabuf.front_mut() else { continue };
        if dmabuf::import(support, egl, gl, canvas, buffer).is_none() {
            source.fail_dmabuf("failed to import a dma-buf texture");
            continue;
        }
        if let Some(image) = buffer.image() {
            let shape = buffer.shape();
            capture_cache.install_texture(id, image, shape.width, shape.height);
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

/// Marks `id`'s source failed (ADR-0248 decision 3's backoff): idle, warned once, no retry short
/// of the output list changing. Shared by the ext and wlr `Failed` handlers.
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
        warn!("capture on `{}` failed; pausing until the output list changes", source.output);
    }
    source.failed = true;
}

/// A dma-buf frame landed for `id`: advances its swapchain (the buffer just filled becomes the
/// front) and either repoints `capture_cache` at its already-imported texture, or queues the
/// import for the next canvas-current pass (ADR-0039, ADR-0248 amendment decision 3). The front
/// slot's `ImageId` changes on every landed frame, alternating between the two slots, so
/// `capture_cache` is repointed every time, not only on the first import.
fn land_dmabuf_frame(captures: &mut CaptureRegistry, capture_cache: &mut CaptureCache, id: &NodeId) {
    let Some(source) = captures.sources.get_mut(id) else { return };
    // Nothing uploads on this path, so the damage the compositor still sends is dropped.
    match source.proto.as_mut() {
        Some(Proto::Ext(ext)) => ext.damage.clear(),
        Some(Proto::Wlr(wlr)) => wlr.damage.clear(),
        None => {}
    }
    source.dmabuf.advance();
    source.captured = true;
    source.in_flight = false;
    match source.dmabuf.front().and_then(DmabufBuffer::image) {
        Some(image) => {
            let shape = source.dmabuf.front().expect("front just returned Some").shape();
            capture_cache.install_texture(*id, image, shape.width, shape.height);
        }
        None => captures.pending_import.push(*id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

//! `capture` node protocol client (ADR-0248): ext-image-copy-capture-v1 first, wlr-screencopy
//! fallback. Hand-dispatched beside SCTK, like ADR-0009's text-input-v3. dma-buf negotiation
//! (ADR-0248 amendment) lives in `wayland::dmabuf`; this module only decides when to attempt it.
//!
//! Only the dma-buf decision functions and `pick_format` are unit tested; the rest is thin
//! protocol translation a mock isn't worth writing.

use std::collections::HashMap;

use femtovg::ImageId;
use khronos_egl as khr;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1, ext_image_copy_capture_session_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1;
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};

use shared::warn;

use crate::image::capture::{CaptureCache, DamageRect, PendingFrame};
use crate::layout::paint::CaptureNode;
use crate::layout::scene::NodeId;

use super::App;
use super::dmabuf::{self, DmabufBuffer, DmabufShape, DmabufSupport, DmabufSwapchain, FormatModifier};
use super::egl::EglState;

/// Which protocol this process captures through, chosen once at startup (ADR-0248 decision 1).
enum Backend {
    None,
    Ext {
        manager: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        sources: ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    Wlr(zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1),
}

/// A capture's negotiated shm buffer, reused across frames while size, stride and format match
/// (ADR-0248 decision 3).
///
/// ponytail: one buffer, not a double-buffer swapchain; correct only because pacing admits one
/// frame in flight per source. Upgrade if a later phase allows more.
struct NegotiatedBuffer {
    pool: SlotPool,
    buffer: Buffer,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
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
    live: bool,
    paint_cursor: bool,
    /// A frame requested and not yet `Ready`/`Failed`: `sync_captures` requests another only once
    /// this clears (ADR-0248 decision 3).
    in_flight: bool,
    /// Whether a frame has landed for the current `output`; a one-shot source reads this to want
    /// no more once it has one.
    captured: bool,
    proto: Option<Proto>,
    /// Set on `Failed`, to stop a persistent failure from retrying every turn. Cleared when the
    /// output list changes, since that is when a failure is likely to have a different answer.
    failed: bool,
    warned_missing_output: bool,
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
    fn new(output: String, live: bool, paint_cursor: bool) -> Self {
        CaptureSource {
            output,
            live,
            paint_cursor,
            in_flight: false,
            captured: false,
            proto: None,
            failed: false,
            warned_missing_output: false,
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
    /// Binds the ext globals if both are present, else the wlr fallback, else neither (ADR-0248
    /// decision 1). `zwp_linux_dmabuf_v1` binds independently of that choice.
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
        let backend = match ext {
            Some((manager, sources)) => Backend::Ext { manager, sources },
            None => match globals.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(qh, 1..=3, ()) {
                Ok(manager) => Backend::Wlr(manager),
                Err(_) => Backend::None,
            },
        };
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
    /// that wants one. Called beside `image_cache.trim`, after every paint.
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
                self.destroy_proto(source.proto);
            }
            self.capture_cache.forget(id);
        }

        if matches!(self.captures.backend, Backend::None) {
            if !wanted.is_empty() && !self.captures.warned_no_backend {
                warn!(
                    "a `capture` node is declared but this compositor offers no screencopy protocol; drawing nothing"
                );
                self.captures.warned_no_backend = true;
            }
            return;
        }

        for (id, node) in wanted {
            let source = self
                .captures
                .sources
                .entry(id)
                .or_insert_with(|| CaptureSource::new(node.output.clone(), node.live, node.paint_cursor));
            source.live = node.live;
            source.paint_cursor = node.paint_cursor;
            if source.output != node.output {
                let mut stale =
                    std::mem::replace(source, CaptureSource::new(node.output.clone(), node.live, node.paint_cursor));
                self.captures.pending_free.extend(stale.dmabuf.take_textures());
                self.destroy_proto(stale.proto);
                self.capture_cache.forget(id);
            }
            let source = &self.captures.sources[&id];
            if !source.failed && !source.in_flight && (node.live || !source.captured) {
                self.request_frame(id);
            }
        }
    }

    fn destroy_proto(&self, proto: Option<Proto>) {
        if let Some(Proto::Ext(ext)) = proto
            && let Some(session) = ext.session
        {
            session.destroy();
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
        let paint_cursor = source.paint_cursor;
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

        match &self.captures.backend {
            Backend::Ext { manager, sources } => {
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
            Backend::Wlr(manager) => {
                let overlay_cursor = i32::from(paint_cursor);
                let _frame = manager.capture_output(overlay_cursor, &output, &qh, id);
                if let Some(source) = self.captures.sources.get_mut(&id)
                    && !matches!(source.proto, Some(Proto::Wlr(_)))
                {
                    // A fresh `Proto::Wlr` only when none exists yet: overwriting it every
                    // request drops the negotiated buffer and reallocates its pool per frame.
                    source.proto = Some(Proto::Wlr(WlrProto::default()));
                }
            }
            Backend::None => {}
        }
    }

    /// Re-issues a frame on an already-negotiated ext session, skipping renegotiation. Attaches
    /// the source's dma-buf back slot when one is active, else the shm buffer (ADR-0248
    /// amendment). A frame already outstanding on this session refuses rather than issuing a
    /// second: the protocol's `duplicate_frame` error.
    fn create_ext_frame(
        &mut self,
        id: NodeId,
        session: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
    ) {
        if self
            .captures
            .sources
            .get(&id)
            .is_some_and(|source| matches!(&source.proto, Some(Proto::Ext(ext)) if ext.frame.is_some()))
        {
            return;
        }
        let qh = self.queue_handle.clone();
        let frame = session.create_frame(&qh, id);
        let has_dmabuf_shape = self.captures.sources.get(&id).is_some_and(|source| source.dmabuf_shape.is_some());
        let use_dmabuf = has_dmabuf_shape && self.captures.ensure_dmabuf_back(&qh, id);
        let Some(source) = self.captures.sources.get_mut(&id) else {
            frame.destroy();
            return;
        };
        if use_dmabuf {
            let Some(buffer) = source.dmabuf.back() else {
                frame.destroy();
                source.in_flight = false;
                return;
            };
            let shape = buffer.shape();
            frame.attach_buffer(buffer.wl_buffer());
            frame.damage_buffer(0, 0, shape.width as i32, shape.height as i32);
            frame.capture();
            if let Some(Proto::Ext(ext)) = source.proto.as_mut() {
                ext.frame = Some(frame);
            }
            return;
        }
        let Some(Proto::Ext(ext)) = source.proto.as_mut() else {
            frame.destroy();
            source.in_flight = false;
            return;
        };
        // After a dma-buf failure there is no shm buffer yet; build it from the cached offer.
        if ext.buffer.is_none()
            && let Some((width, height, format)) = ext.shm_negotiated
        {
            ext.buffer = negotiate_buffer(&self.shm, None, width, height, width * 4, format);
        }
        let Some(negotiated) = ext.buffer.as_ref() else {
            // The first `Done` has not landed; it creates the frame once it does.
            frame.destroy();
            source.in_flight = false;
            return;
        };
        frame.attach_buffer(negotiated.buffer.wl_buffer());
        frame.damage_buffer(0, 0, negotiated.width as i32, negotiated.height as i32);
        frame.capture();
        ext.frame = Some(frame);
    }

    /// Negotiates and, if nothing is in flight, requests a frame from one `Done` batch (ADR-0248
    /// amendment decision 1). Called directly for a fresh session's first `Done`, and from
    /// `Ready`/`Failed` for one deferred while a frame was outstanding.
    fn apply_ext_done(&mut self, id: NodeId, offer: DoneOffer) {
        let DoneOffer { width, height, shm_format, dmabuf_formats } = offer;
        let qh = self.queue_handle.clone();
        let negotiation = DmabufOffer { id, width, height, offered: &dmabuf_formats };
        let used_dmabuf = self.captures.negotiate_dmabuf(self.egl.as_ref(), &qh, negotiation);
        {
            let Some(source) = self.captures.sources.get_mut(&id) else { return };
            let Some(Proto::Ext(ext)) = source.proto.as_mut() else { return };
            if let Some(format) = shm_format {
                ext.shm_negotiated = Some((width, height, format));
            }
            if used_dmabuf {
                ext.buffer = None;
            } else if let Some((width, height, format)) = ext.shm_negotiated {
                let existing = ext.buffer.take();
                ext.buffer = negotiate_buffer(&self.shm, existing, width, height, width * 4, format);
            }
        }
        let session = self.captures.sources.get(&id).and_then(|s| match &s.proto {
            Some(Proto::Ext(ext)) => ext.session.clone(),
            _ => None,
        });
        if let Some(session) = session {
            self.create_ext_frame(id, &session);
        }
    }

    fn request_next_if_live(&mut self, id: NodeId) {
        if self.captures.sources.get(&id).is_some_and(|source| source.live) {
            self.request_frame(id);
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

/// Preferred shm formats: both are wl_shm-mandatory.
fn pick_format(candidates: &[wl_shm::Format]) -> Option<wl_shm::Format> {
    [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888].into_iter().find(|preferred| candidates.contains(preferred))
}

/// One `ShmFormat` event's contribution to a negotiation batch: last-event-wins would let a later
/// unsupported format overwrite an earlier supported pick with `None`, so `current` wins once it
/// is `Some`.
fn keep_first_supported(current: Option<wl_shm::Format>, offered: wl_shm::Format) -> Option<wl_shm::Format> {
    current.or_else(|| pick_format(&[offered]))
}

/// wlr-screencopy's `Buffer` event, checked before it is trusted: an unsupported format would
/// silently corrupt colours (`bgrx_to_rgba` assumes BGRX), and a stride narrower than the copy
/// assumes (`width * 4`) would panic slicing `stage_landed`'s rows.
fn wlr_buffer_valid(format: wl_shm::Format, width: u32, stride: u32) -> bool {
    pick_format(&[format]).is_some() && stride >= width.saturating_mul(4)
}

fn negotiate_buffer(
    shm: &smithay_client_toolkit::shm::Shm,
    existing: Option<NegotiatedBuffer>,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
) -> Option<NegotiatedBuffer> {
    if let Some(buf) = &existing
        && buf.width == width
        && buf.height == height
        && buf.stride == stride
        && buf.format == format
    {
        return existing;
    }
    let mut pool = SlotPool::new((stride * height).max(1) as usize, shm).ok()?;
    let (buffer, _canvas) = pool.create_buffer(width as i32, height as i32, stride as i32, format).ok()?;
    Some(NegotiatedBuffer { pool, buffer, width, height, stride, format })
}

/// Reads `negotiated`'s pool memory and stages it for the next canvas-current upload. Copies only
/// the damaged rows unless the size changed or nothing was reported, when the whole buffer is
/// needed anyway.
fn stage_landed(cache: &mut CaptureCache, id: NodeId, negotiated: &mut NegotiatedBuffer, damage: Vec<DamageRect>) {
    let (width, height, stride) = (negotiated.width, negotiated.height, negotiated.stride as usize);
    let Some(buffer) = negotiated.pool.canvas(&negotiated.buffer) else { return };
    let resized = cache.get(id).is_none_or(|(_, w, h)| (w, h) != (width, height));
    let (y0, y1) = if resized || damage.is_empty() {
        (0, height)
    } else {
        let lo = damage.iter().map(|r| r.y).min().unwrap_or(0).min(height);
        let hi = damage.iter().map(|r| (r.y + r.height).min(height)).max().unwrap_or(height).max(lo);
        (lo, hi)
    };
    let row_bytes = width as usize * 4;
    let mut pixels = Vec::with_capacity(row_bytes * (y1 - y0) as usize);
    for row in y0..y1 {
        let start = row as usize * stride;
        pixels.extend_from_slice(&buffer[start..start + row_bytes]);
    }
    cache.stage(id, PendingFrame { width, height, y_offset: y0, pixels, damage });
}

// The three manager globals have no events of their own; request-only.
delegate_noop!(App: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
delegate_noop!(App: ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(App: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

impl Dispatch<ext_image_capture_source_v1::ExtImageCaptureSourceV1, NodeId> for App {
    fn event(
        _: &mut Self,
        _: &ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        _: ext_image_capture_source_v1::Event,
        _: &NodeId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // No events; its `NodeId` user data rules out `delegate_noop!`.
    }
}

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, NodeId> for App {
    fn event(
        state: &mut Self,
        _proxy: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        id: &NodeId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        let Some(CaptureSource { proto: Some(Proto::Ext(ext)), .. }) = state.captures.sources.get_mut(id) else {
            return;
        };
        match event {
            Event::BufferSize { width, height } => {
                let format = ext.negotiating.and_then(|(_, _, format)| format);
                ext.negotiating = Some((width, height, format));
            }
            Event::ShmFormat { format: WEnum::Value(format) } => {
                let (width, height, current) = ext.negotiating.unwrap_or((0, 0, None));
                ext.negotiating = Some((width, height, keep_first_supported(current, format)));
            }
            Event::ShmFormat { format: WEnum::Unknown(_) } => {}
            Event::DmabufFormat { format, modifiers } => {
                ext.dmabuf_formats.extend(
                    modifiers
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|chunk| FormatModifier { fourcc: format, modifier: u64::from_ne_bytes(*chunk) }),
                );
            }
            Event::Done => {
                let Some((width, height, shm_format)) = ext.negotiating.take() else {
                    return;
                };
                let dmabuf_formats = std::mem::take(&mut ext.dmabuf_formats);
                let frame_outstanding = ext.frame.is_some();
                let offer = DoneOffer { width, height, shm_format, dmabuf_formats };
                if frame_outstanding {
                    if let Some(source) = state.captures.sources.get_mut(id)
                        && let Some(Proto::Ext(ext)) = source.proto.as_mut()
                    {
                        ext.pending_done = Some(offer);
                    }
                    return;
                }
                state.apply_ext_done(*id, offer);
            }
            Event::Stopped => {
                if let Some(source) = state.captures.sources.get_mut(id) {
                    if let Some(Proto::Ext(ext)) = source.proto.take()
                        && let Some(session) = ext.session
                    {
                        session.destroy();
                    }
                    source.in_flight = false;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, NodeId> for App {
    fn event(
        state: &mut Self,
        proxy: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        id: &NodeId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Damage { x, y, width, height } => {
                if let Some(CaptureSource { proto: Some(Proto::Ext(ext)), .. }) = state.captures.sources.get_mut(id) {
                    ext.damage.push(DamageRect {
                        x: x.max(0) as u32,
                        y: y.max(0) as u32,
                        width: width.max(0) as u32,
                        height: height.max(0) as u32,
                    });
                }
            }
            Event::Ready => {
                proxy.destroy();
                let Some(source) = state.captures.sources.get_mut(id) else { return };
                if let Some(Proto::Ext(ext)) = source.proto.as_mut() {
                    ext.frame = None;
                }
                if source.dmabuf_shape.is_some() {
                    land_dmabuf_frame(&mut state.captures, &mut state.capture_cache, id);
                } else {
                    let Some(Proto::Ext(ext)) = source.proto.as_mut() else { return };
                    let damage = std::mem::take(&mut ext.damage);
                    if let Some(negotiated) = ext.buffer.as_mut() {
                        stage_landed(&mut state.capture_cache, *id, negotiated, damage);
                    }
                    source.captured = true;
                    source.in_flight = false;
                }
                let pending = state.captures.sources.get_mut(id).and_then(|source| match source.proto.as_mut() {
                    Some(Proto::Ext(ext)) => ext.pending_done.take(),
                    _ => None,
                });
                match pending {
                    Some(offer) => state.apply_ext_done(*id, offer),
                    None => state.request_next_if_live(*id),
                }
            }
            Event::Failed { .. } => {
                proxy.destroy();
                fail_source(&mut state.captures, id);
            }
            _ => {}
        }
    }
}

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
        Some(Proto::Wlr(wlr)) => wlr.damage.clear(),
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

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, NodeId> for App {
    fn event(
        state: &mut Self,
        proxy: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        id: &NodeId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                let valid = wlr_buffer_valid(format, width, stride);
                if let Some(CaptureSource { proto: Some(Proto::Wlr(wlr)), .. }) = state.captures.sources.get_mut(id) {
                    wlr.negotiating = valid.then_some((width, height, stride, format));
                }
            }
            Event::LinuxDmabuf { format, width, height } => {
                if let Some(CaptureSource { proto: Some(Proto::Wlr(wlr)), .. }) = state.captures.sources.get_mut(id) {
                    wlr.dmabuf_format = Some((width, height, format));
                }
            }
            Event::BufferDone => {
                let Some(CaptureSource { proto: Some(Proto::Wlr(wlr)), .. }) = state.captures.sources.get_mut(id)
                else {
                    return;
                };
                let shm_negotiating = wlr.negotiating.take();
                let dmabuf_offer = wlr.dmabuf_format.take();
                let qh = state.queue_handle.clone();
                let used_dmabuf = dmabuf_offer.is_some_and(|(width, height, fourcc)| {
                    let cached = state.captures.sources.get(id).and_then(|s| s.dmabuf_shape);
                    let unchanged = cached.is_some_and(|shape| {
                        shape.width == width && shape.height == height && shape.format.fourcc == fourcc
                    });
                    if unchanged {
                        state.captures.ensure_dmabuf_back(&qh, *id)
                    } else {
                        // wlr-screencopy names one fourcc with no modifier list (unlike ext's
                        // `dmabuf_format`): trust EGL's own importable set for it instead, the
                        // same "or EGL's own" allowance amendment decision 1 makes for a device.
                        state.captures.ensure_probed(state.egl.as_ref());
                        let offered: Vec<FormatModifier> = match (state.captures.support(), state.egl.as_ref()) {
                            (Some(support), Some(egl)) => support.importable_modifiers(egl, fourcc),
                            _ => Vec::new(),
                        };
                        let offer = DmabufOffer { id: *id, width, height, offered: &offered };
                        state.captures.negotiate_dmabuf(state.egl.as_ref(), &qh, offer)
                    }
                });

                if used_dmabuf {
                    let Some(source) = state.captures.sources.get_mut(id) else { return };
                    if let Some(Proto::Wlr(wlr)) = source.proto.as_mut() {
                        wlr.dmabuf_active = true;
                    }
                    let Some(buffer) = source.dmabuf.back() else {
                        proxy.destroy();
                        fail_source(&mut state.captures, id);
                        return;
                    };
                    proxy.copy_with_damage(buffer.wl_buffer());
                    return;
                }

                let Some(CaptureSource { proto: Some(Proto::Wlr(wlr)), .. }) = state.captures.sources.get_mut(id)
                else {
                    return;
                };
                wlr.dmabuf_active = false;
                // No usable shm offer fails the source: retrying would get the same offer back.
                let Some((width, height, stride, format)) = shm_negotiating else {
                    proxy.destroy();
                    fail_source(&mut state.captures, id);
                    return;
                };
                let existing = wlr.buffer.take();
                wlr.buffer = negotiate_buffer(&state.shm, existing, width, height, stride, format);
                let Some(negotiated) = wlr.buffer.as_ref() else {
                    proxy.destroy();
                    fail_source(&mut state.captures, id);
                    return;
                };
                proxy.copy_with_damage(negotiated.buffer.wl_buffer());
            }
            Event::Damage { x, y, width, height } => {
                if let Some(CaptureSource { proto: Some(Proto::Wlr(wlr)), .. }) = state.captures.sources.get_mut(id) {
                    wlr.damage.push(DamageRect { x, y, width, height });
                }
            }
            Event::Ready { .. } => {
                proxy.destroy();
                let Some(source) = state.captures.sources.get_mut(id) else { return };
                let used_dmabuf = matches!(&source.proto, Some(Proto::Wlr(wlr)) if wlr.dmabuf_active);
                if used_dmabuf {
                    land_dmabuf_frame(&mut state.captures, &mut state.capture_cache, id);
                } else {
                    let Some(Proto::Wlr(wlr)) = source.proto.as_mut() else { return };
                    let damage = std::mem::take(&mut wlr.damage);
                    if let Some(negotiated) = wlr.buffer.as_mut() {
                        stage_landed(&mut state.capture_cache, *id, negotiated, damage);
                    }
                    source.captured = true;
                    source.in_flight = false;
                }
                state.request_next_if_live(*id);
            }
            Event::Failed => {
                proxy.destroy();
                fail_source(&mut state.captures, id);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_formats_are_tried_in_order() {
        assert_eq!(pick_format(&[wl_shm::Format::Argb8888]), Some(wl_shm::Format::Argb8888));
        assert_eq!(pick_format(&[wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]), Some(wl_shm::Format::Xrgb8888));
        assert_eq!(pick_format(&[wl_shm::Format::Bgr888]), None);
    }

    #[test]
    fn a_later_unsupported_shm_format_does_not_overwrite_an_earlier_supported_one() {
        let first = keep_first_supported(None, wl_shm::Format::Xrgb8888);
        assert_eq!(first, Some(wl_shm::Format::Xrgb8888));
        assert_eq!(keep_first_supported(first, wl_shm::Format::Bgr888), Some(wl_shm::Format::Xrgb8888));
    }

    #[test]
    fn an_unsupported_first_format_still_lets_a_later_supported_one_through() {
        let first = keep_first_supported(None, wl_shm::Format::Bgr888);
        assert_eq!(first, None);
        assert_eq!(keep_first_supported(first, wl_shm::Format::Argb8888), Some(wl_shm::Format::Argb8888));
    }

    #[test]
    fn a_wlr_buffer_offer_needs_a_supported_format_and_a_wide_enough_stride() {
        assert!(wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 400));
        assert!(wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 512), "padded stride is still fine");
        assert!(!wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 399), "narrower than width * 4 would panic the copy");
        assert!(!wlr_buffer_valid(wl_shm::Format::Bgr888, 100, 400), "unsupported format, whatever the stride");
    }
}

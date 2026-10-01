//! Capture dma-buf negotiation, swapchain landing and GL texture lifecycle.

use super::*;
use crate::wayland::dmabuf::{self, DmabufBuffer, DmabufSupport};

/// What a compositor offered for one source's negotiation batch (ADR-0248 amendment decision 1).
pub(super) struct DmabufOffer<'a> {
    pub(super) id: NodeId,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) offered: &'a [FormatModifier],
}

/// Process-wide dma-buf capability, probed once EGL exists and memoized: `Pending` retries on the
/// next negotiation, `Unsupported`/`Supported` are final for the process's lifetime.
#[derive(Default)]
pub(super) enum DmabufProbe {
    #[default]
    Pending,
    Unsupported,
    Supported(DmabufSupport),
}

impl CaptureSource {
    /// Marks this source's dma-buf attempt permanently failed (amendment decision 4): warned
    /// once, shm from here on until the source is recreated for a new target.
    fn fail_dmabuf(&mut self, why: &str) {
        if !self.dmabuf_failed {
            warn!("capture on `{}` {why}; using shm", self.target.name());
        }
        self.dmabuf_failed = true;
        self.dmabuf_shape = None;
    }
}

impl CaptureRegistry {
    /// Probes process-wide dma-buf support the first time EGL exists; a `None` `egl` (too early
    /// in startup) leaves the probe pending for the next call instead of failing it outright.
    pub(super) fn ensure_probed(&mut self, egl: Option<&EglState>) {
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

    pub(super) fn support(&self) -> Option<&DmabufSupport> {
        match &self.dmabuf_probe {
            DmabufProbe::Supported(support) => Some(support),
            _ => None,
        }
    }

    /// Picks a format from `offer.offered` and allocates `offer.id`'s back slot for it (ADR-0248
    /// amendment decisions 1 and 3). `false` means shm should run instead this round; if dma-buf
    /// support exists at all, that also marks the source permanently shm-only, warned once
    /// (amendment decision 4).
    pub(super) fn negotiate_dmabuf(
        &mut self,
        egl: Option<&EglState>,
        qh: &QueueHandle<App>,
        offer: DmabufOffer<'_>,
    ) -> bool {
        let DmabufOffer { id, width, height, offered } = offer;
        self.ensure_probed(egl);
        let support_available = matches!(self.dmabuf_probe, DmabufProbe::Supported(_));
        let opaque = self.sources.get(&id).is_some_and(|source| source.target.is_output());
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
                dmabuf::pick_dmabuf_format(offered, &importable, opaque)
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
    pub(super) fn ensure_dmabuf_back(&mut self, qh: &QueueHandle<App>, id: NodeId) -> bool {
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

/// Imports every dma-buf frame that landed with no texture yet, and frees any texture a discarded
/// buffer left behind. Must run with the canvas's GL context current (ADR-0039); a free function,
/// not an `App` method, so `wayland::surface::paint_surface` can call it while its own borrow of
/// `self.text_painter` supplies `canvas`.
pub(in crate::wayland) fn import_ready_dmabufs(
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
        let opaque = source.target.is_output();
        let Some(buffer) = source.dmabuf.front_mut() else { continue };
        if dmabuf::import(support, egl, gl, canvas, buffer, opaque).is_none() {
            source.fail_dmabuf("failed to import a dma-buf texture");
            continue;
        }
        if let Some(image) = buffer.image() {
            let shape = buffer.shape();
            capture_cache.install_texture(id, image, shape.width, shape.height);
        }
    }
}

/// A dma-buf frame landed for `id`: advances its swapchain (the buffer just filled becomes the
/// front) and either repoints `capture_cache` at its already-imported texture, or queues the
/// import for the next canvas-current pass (ADR-0039, ADR-0248 amendment decision 3). The front
/// slot's `ImageId` changes on every landed frame, alternating between the two slots, so
/// `capture_cache` is repointed every time, not only on the first import.
pub(super) fn land_dmabuf_frame(captures: &mut CaptureRegistry, capture_cache: &mut CaptureCache, id: &NodeId) {
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
        None => {
            captures.pending_import.push(*id);
            capture_cache.invalidate(*id);
        }
    }
}

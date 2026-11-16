//! Capture dma-buf negotiation, swapchain landing and GL texture lifecycle.

use wayland_client::Dispatch;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};

use super::*;
use crate::wayland::dmabuf::{self, DmabufSupport};

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
    pub(super) fn fail_dmabuf(&mut self, why: &str) {
        if !matches!(self.dmabuf_state, DmabufState::Failed) {
            warn!("capture on `{}` {why}; using shm", self.target.name());
        }
        self.dmabuf_state = DmabufState::Failed;
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
                Ok(support) => DmabufProbe::Supported(support),
                Err(why) => {
                    warn!("no usable dma-buf import ({why}); `capture` nodes use the slower shm path");
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
        let opaque = self.sources.get(&id).is_some_and(|source| source.target.is_output());
        // A desktop source is unmixed on the CPU, so it stays on shm.
        let previously_failed = self
            .sources
            .get(&id)
            .is_some_and(|source| matches!(source.dmabuf_state, DmabufState::Failed) || source.desktop.is_some());
        if previously_failed || self.dmabuf_manager.is_none() {
            return false;
        }
        let (DmabufProbe::Supported(support), Some(egl)) = (&self.dmabuf_probe, egl) else { return false };
        let mut seen = std::collections::HashSet::new();
        let importable: Vec<FormatModifier> = offered
            .iter()
            .filter(|candidate| seen.insert(candidate.fourcc))
            .flat_map(|candidate| support.importable_modifiers(egl, candidate.fourcc))
            .collect();
        let picked = dmabuf::pick_dmabuf_format(offered, &importable, opaque);
        let Some(source) = self.sources.get_mut(&id) else { return false };
        let Some(format) = picked else {
            source.fail_dmabuf("cannot use dma-buf (no shared format)");
            return false;
        };
        source.dmabuf_state = DmabufState::Shaped(DmabufShape { width, height, format });
        self.ensure_dmabuf_back(qh, id)
    }

    /// Re-checks `id`'s already-negotiated dma-buf shape (the routine per-frame path once
    /// negotiation has run once): a no-op unless the back slot has never been allocated.
    /// `true` means this frame uses the dma-buf slot. While its allocation awaits `created`, ext
    /// still gets `true` and finds no `back()`, so it skips the frame and the params handler
    /// requests it; wlr gets `false` and copies this frame through shm, which its per-frame
    /// `dmabuf_active` lands correctly.
    pub(super) fn ensure_dmabuf_back(&mut self, qh: &QueueHandle<App>, id: NodeId) -> bool {
        let CaptureRegistry { sources, dmabuf_manager, pending_free, dmabuf_probe, .. } = self;
        let Some(source) = sources.get_mut(&id) else { return false };
        let Some(shape) = source.dmabuf_state.shape() else { return false };
        // `Shaped` implies both exist; checked inside the allocation so a matching slot answers alone.
        let support = match dmabuf_probe {
            DmabufProbe::Supported(support) => Some(&*support),
            _ => None,
        };
        let alloc = || dmabuf::allocate(support?, dmabuf_manager.as_ref()?, qh, shape, id);
        let (ok, discarded) = source.dmabuf.ensure_back(shape, alloc);
        if let Some(mut discarded) = discarded
            && let Some(freed) = discarded.take_texture()
        {
            pending_free.push(freed);
        }
        if !ok {
            source.fail_dmabuf("failed a dma-buf allocation");
            return false;
        }
        source.dmabuf.back().is_some() || source.protocol == Protocol::Ext
    }
}

/// The compositor's answer to [`dmabuf::allocate`]'s `create`. `failed` drops the source to shm
/// (amendment decision 4) instead of the protocol error `create_immed` would raise (#65); either
/// answer requests the frame an idle ext source skipped while it waited. An answer to a params the
/// source no longer waits on (replaced, stopped, forgotten) only frees what it delivered.
impl Dispatch<ZwpLinuxBufferParamsV1, NodeId> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        id: &NodeId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let buffer = match &event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => Some(buffer),
            zwp_linux_buffer_params_v1::Event::Failed => None,
            _ => return,
        };
        proxy.destroy();
        let source = state.captures.sources.get_mut(id);
        let Some(source) = source.and_then(|source| source.dmabuf.answer(proxy, buffer).then_some(source)) else {
            buffer.iter().for_each(|buffer| buffer.destroy());
            return;
        };
        if buffer.is_none() {
            source.fail_dmabuf("had its dma-buf rejected by the compositor");
        }
        // Only ext skips a frame while pending; wlr copied it through shm and owes none.
        if matches!(source.proto, Some(Proto::Ext(_))) && !source.in_flight && !source.failed {
            state.request_frame(*id);
        }
    }

    wayland_client::event_created_child!(App, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ()),
    ]);
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
    match source.dmabuf.front().and_then(|front| Some((front.image()?, front.shape()))) {
        Some((image, shape)) => {
            capture_cache.install_texture(*id, image, shape.width, shape.height);
        }
        None => {
            captures.pending_import.push(*id);
            capture_cache.invalidate(*id);
        }
    }
}

#[cfg(test)]
mod tests {
    use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;

    use super::*;

    /// A `failed` answer drops the source to shm and, once the source is idle with a session,
    /// requests the frame it skipped, with the connection still up; an answer to a params the
    /// source no longer waits on frees what it delivered and requests nothing; `created`
    /// completes the slot an ext frame skipped (#65).
    /// Requests read off the mock's wire log.
    #[test]
    fn params_answers_complete_or_reject_the_pending_slot_and_free_stale_ones() {
        let (mut app, _dir, mut queue, mock) = App::test_app_with_wire(
            r#"return panel { id = "bar", layer = "top", output = "TEST", width = 100, height = 32 }"#,
        );
        let qh = app.queue_handle.clone();
        let registry = app.registry_state.registry().clone();
        let manager: ZwpLinuxDmabufV1 = registry.bind(90, 3, &qh, ());
        let ext: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1 = registry.bind(91, 1, &qh, ());
        let sources: ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1 =
            registry.bind(92, 1, &qh, ());
        let output = app.output_state.outputs().next().expect("the mock's output");
        let id = NodeId::test(7);
        let session = ext.create_session(
            &sources.create_source(&output, &qh, id),
            ext_image_copy_capture_manager_v1::Options::empty(),
            &qh,
            id,
        );
        let node = CaptureNode {
            node: id,
            target: CaptureTarget::Output("TEST".into()),
            live: None,
            paint_cursor: false,
            region: None,
        };
        assert!(app.captures.reconcile_source(&node, Some(Protocol::Ext), &mut app.capture_cache).is_some());
        let stale = manager.create_params(&qh, id);
        let shape = DmabufShape { width: 4, height: 4, format: FormatModifier { fourcc: 0, modifier: 0 } };
        let source = app.captures.sources.get_mut(&id).unwrap();
        source.proto = Some(Proto::Ext(ExtProto { session: Some(session.clone()), ..Default::default() }));
        source.dmabuf_state = DmabufState::Shaped(shape);
        source.dmabuf.pending = Some((manager.create_params(&qh, id), 0));
        let (session_id, stale_id) = (session.id().protocol_id(), stale.id().protocol_id());
        let ops = |object: u32| -> Vec<u16> {
            mock.requests.lock().unwrap().iter().filter(|&&(o, ..)| o == object).map(|&(_, op, _)| op).collect()
        };
        // Requests made while dispatching follow the first roundtrip's `sync`; the second is
        // the barrier that puts them in the wire log.
        let mut settle = |app: &mut App, why: &str| (0..2).for_each(|_| queue.roundtrip(app).map(drop).expect(why));
        settle(&mut app, "setup");
        assert!(ops(session_id).is_empty());

        // `zwp_linux_buffer_params_v1.created` is event 0, `failed` 1; `destroy` is request 0, as
        // is `wl_buffer.destroy`. 0xff00_0000 is the first server-allocated id.
        mock.send(stale_id, 0, &0xff00_0000_u32.to_ne_bytes());
        settle(&mut app, "a stale `created` keeps the connection");
        assert_eq!(ops(stale_id), [0], "the stale params is destroyed");
        assert_eq!(ops(0xff00_0000), [0], "and so is the `wl_buffer` it delivered");
        let source = &app.captures.sources[&id];
        assert!(source.dmabuf_state.shape().is_some() && source.dmabuf.pending.is_some(), "nothing else changes");
        assert!(ops(session_id).is_empty(), "a stale answer requests no frame");

        // In flight, stopped, wlr (copied through shm meanwhile), idle ext: only the last
        // requests; `create_frame` is session request 0, here through shm.
        let ext = || Some(Proto::Ext(ExtProto { session: Some(session.clone()), ..Default::default() }));
        let cases =
            [(true, ext(), 0), (false, None, 0), (false, Some(Proto::Wlr(WlrProto::default())), 0), (false, ext(), 1)];
        for (in_flight, proto, requests) in cases {
            let label = format!("in_flight {in_flight}, wlr {}", matches!(proto, Some(Proto::Wlr(_))));
            let rejected = manager.create_params(&qh, id);
            let source = app.captures.sources.get_mut(&id).unwrap();
            source.in_flight = in_flight;
            source.proto = proto;
            source.dmabuf.pending = Some((rejected.clone(), 0));
            mock.send(rejected.id().protocol_id(), 1, &[]);
            settle(&mut app, "a rejected dma-buf keeps the connection");
            assert_eq!(ops(rejected.id().protocol_id()), [0], "the rejected params is destroyed");
            let source = &app.captures.sources[&id];
            assert!(matches!(source.dmabuf_state, DmabufState::Failed) && source.dmabuf.pending.is_none());
            assert_eq!((source.in_flight, ops(session_id).len()), (in_flight, requests), "{label}");
        }

        // An idle ext source with a pending slot skips its frame though shm is on offer, then
        // `created` completes the slot and the requested frame attaches it. 0xff00_0001 is the
        // next server id; `attach_buffer` is frame request 1.
        let completed = manager.create_params(&qh, id);
        let source = app.captures.sources.get_mut(&id).unwrap();
        source.dmabuf_state = DmabufState::Shaped(shape);
        source.dmabuf = DmabufSwapchain::pending_for_test(shape, completed.clone());
        let shm_negotiated = Some((4, 4, wl_shm::Format::Xrgb8888));
        source.proto =
            Some(Proto::Ext(ExtProto { session: Some(session.clone()), shm_negotiated, ..Default::default() }));
        app.request_frame(id);
        let frame = |app: &App| match &app.captures.sources[&id].proto {
            Some(Proto::Ext(ext)) => ext.frame.clone(),
            _ => None,
        };
        assert!(frame(&app).is_none() && !app.captures.sources[&id].in_flight, "the frame waits for `created`");
        mock.send(completed.id().protocol_id(), 0, &0xff00_0001_u32.to_ne_bytes());
        settle(&mut app, "a `created` keeps the connection");
        assert_eq!((ops(completed.id().protocol_id()), ops(0xff00_0001)), (vec![0], vec![]), "only the params goes");
        let frame = frame(&app).expect("the answer requests the skipped frame").id().protocol_id();
        let attached = mock.requests.lock().unwrap().contains(&(frame, 1, 0xff00_0001));
        assert!(attached && app.captures.sources[&id].in_flight, "into the created buffer");

        // A new shape must reallocate, and without dma-buf support that fails the source to shm.
        let source = app.captures.sources.get_mut(&id).unwrap();
        source.dmabuf_state = DmabufState::Shaped(DmabufShape { width: 8, ..shape });
        source.proto =
            Some(Proto::Ext(ExtProto { session: Some(session.clone()), shm_negotiated, ..Default::default() }));
        source.in_flight = false;
        app.request_frame(id);
        assert!(matches!(app.captures.sources[&id].dmabuf_state, DmabufState::Failed), "no stale slot reuse");
        assert!(!app.exit);
    }
}

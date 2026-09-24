//! ext-image-copy-capture-v1: a persistent session per source, renegotiated on each `Done`.

use wayland_client::{Dispatch, WEnum};

use super::shm::{keep_first_supported, negotiate_buffer, stage_landed};
use super::*;

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

impl App {
    /// Re-issues a frame on an already-negotiated ext session, skipping renegotiation. Attaches
    /// the source's dma-buf back slot when one is active, else the shm buffer (ADR-0248
    /// amendment). A frame already outstanding on this session refuses rather than issuing a
    /// second: the protocol's `duplicate_frame` error.
    pub(super) fn create_ext_frame(
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
}

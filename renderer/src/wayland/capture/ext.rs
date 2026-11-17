//! ext-image-copy-capture-v1: a persistent session per source, renegotiated on each `Done`.

use wayland_client::{Dispatch, WEnum};

use super::shm::{keep_supported, negotiate_buffer};
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
        proxy: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        id: &NodeId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        let Some(CaptureSource { proto: Some(Proto::Ext(ext)), target, .. }) =
            state.captures.sources.get_mut(id).filter(|source| source.owns(proxy))
        else {
            return;
        };
        match event {
            Event::BufferSize { width, height } => {
                let format = ext.negotiating.and_then(|(_, _, format)| format);
                ext.negotiating = Some((width, height, format));
            }
            Event::ShmFormat { format: WEnum::Value(format) } => {
                let (width, height, current) = ext.negotiating.unwrap_or((0, 0, None));
                ext.negotiating = Some((width, height, keep_supported(current, format, target.is_output())));
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
                let offer = DoneOffer { width, height, shm_format, dmabuf_formats };
                if ext.frame.is_some() {
                    ext.pending_done = Some(offer);
                    return;
                }
                state.apply_ext_done(*id, offer);
            }
            Event::Stopped => state.captures.stop(*id, &mut state.capture_cache),
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
        use ext_image_copy_capture_frame_v1::{Event, FailureReason};
        let current = state.captures.sources.get(id).is_some_and(|source| source.owns(proxy));
        if !current {
            return;
        }
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
            Event::Transform { transform: WEnum::Value(transform) } => {
                if let Some(CaptureSource { proto: Some(Proto::Ext(ext)), .. }) = state.captures.sources.get_mut(id) {
                    ext.transform = Some(transform);
                }
            }
            Event::Ready => {
                proxy.destroy();
                if let Some(CaptureSource { proto: Some(Proto::Ext(ext)), .. }) = state.captures.sources.get_mut(id) {
                    let transform = ext.transform.take().unwrap_or(wl_output::Transform::Normal);
                    state.capture_cache.set_transform(*id, transform);
                }
                land_ready_frame(&mut state.captures, &mut state.capture_cache, id);
                let pending = state.captures.sources.get_mut(id).and_then(|source| match source.proto.as_mut() {
                    Some(Proto::Ext(ext)) => ext.pending_done.take(),
                    _ => None,
                });
                match pending {
                    Some(offer) => state.apply_ext_done(*id, offer),
                    None => state.request_next_if_live(*id),
                }
            }
            Event::Failed { reason } => {
                proxy.destroy();
                match reason {
                    WEnum::Value(FailureReason::BufferConstraints) => {
                        match constraints_changed(&mut state.captures, id) {
                            Some(offer) => state.apply_ext_done(*id, offer),
                            None => state.request_next_if_live(*id),
                        }
                    }
                    // The session's `Stopped` follows and decides whether the source restarts.
                    WEnum::Value(FailureReason::Stopped) => {}
                    _ => fail_source(&mut state.captures, id),
                }
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
        let has_dmabuf_shape =
            self.captures.sources.get(&id).is_some_and(|source| source.dmabuf_state.shape().is_some());
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
            ext.buffer = negotiate_buffer(&self.shm, None, width, height, width.saturating_mul(4), format);
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
            // Per batch: a dma-buf-only offer must not leave an older size's shm fallback behind.
            ext.shm_negotiated = shm_format.map(|format| (width, height, format));
            if used_dmabuf {
                ext.buffer = None;
            } else if let Some((width, height, format)) = ext.shm_negotiated {
                let existing = ext.buffer.take();
                ext.buffer = negotiate_buffer(&self.shm, existing, width, height, width.saturating_mul(4), format);
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

#[cfg(test)]
mod tests {
    use super::super::tests::{ID, Wire, id};
    use super::*;

    // Session events: `buffer_size` 0, `shm_format` 1, `dmabuf_format` 3, `done` 4, `stopped` 5;
    // requests: `create_frame` 0, `destroy` 1. Frame events: `transform` 0, `damage` 1, `ready` 3,
    // `failed` 4 (`unknown` 0, `buffer_constraints` 1, `stopped` 2); requests: `destroy` 0,
    // `attach_buffer` 1, `damage_buffer` 2, `capture` 3.
    const XRGB: u32 = wl_shm::Format::Xrgb8888 as u32;

    fn session(w: &Wire) -> u32 {
        id(w.ext().session.as_ref())
    }

    fn frame(w: &Wire) -> u32 {
        id(w.ext().frame.as_ref())
    }

    fn buffer(w: &Wire) -> u32 {
        id(w.ext().buffer.as_ref().map(|negotiated| negotiated.buffer.wl_buffer()))
    }

    fn offer(w: &mut Wire, width: u32, height: u32) {
        let session = session(w);
        w.send(session, 0, &[width, height]);
        w.send(session, 1, &[XRGB]);
        w.send(session, 4, &[]);
    }

    /// The first `done` sizes an shm buffer and captures into it; `ready` lands the frame with its
    /// damage and transform, then paces the next: a one-shot stops, an uncapped source asks again
    /// on the same session, a capped one waits for the loop.
    #[test]
    fn a_session_negotiates_shm_and_each_ready_lands_then_paces_the_next() {
        for (live, again) in [(None, false), (Some(f32::INFINITY), true), (Some(0.001), false)] {
            let mut w = Wire::new(Protocol::Ext, live);
            let s = session(&w);
            assert!(w.requests(s).is_empty(), "{live:?}: a fresh session waits for its first `done`");
            offer(&mut w, 4, 2);
            let (f, b) = (frame(&w), buffer(&w));
            let negotiated = w.ext().buffer.as_ref().unwrap();
            assert_eq!((negotiated.width, negotiated.height), (4, 2));
            assert_eq!(w.requests(s), [(0, f)]);
            assert_eq!(w.requests(f), [(1, b), (2, 0), (3, 0)], "attach shm, damage it from 0, capture");

            w.send(f, 1, &[-1_i32 as u32, 1, 3, 1]);
            assert_eq!(w.ext().damage, [DamageRect { x: 0, y: 1, width: 3, height: 1 }], "clamped to the buffer");
            w.send(f, 0, &[wl_output::Transform::_90 as u32]);
            assert_eq!(w.ext().transform, Some(wl_output::Transform::_90));
            w.send(f, 3, &[]);
            assert_eq!(w.requests(f).last(), Some(&(0, 0)), "{live:?}: the landed frame is destroyed");
            assert_eq!(w.app.capture_cache.transform(ID), wl_output::Transform::_90);
            assert!(w.app.capture_cache.poll().contains(&ID), "{live:?}: the frame is staged");
            assert!(w.ext().damage.is_empty() && w.ext().transform.is_none());
            let source = w.source();
            assert!(source.captured);
            assert_eq!((source.in_flight, source.deferred), (again, live == Some(0.001)), "{live:?}");
            assert_eq!(w.requests(s).len(), 1 + usize::from(again), "{live:?}");
            if again {
                assert_eq!(w.requests(frame(&w))[0], (1, b), "the next frame reuses the buffer");
            }
        }

        // A capped source waits for the loop, which requests it once the cap allows.
        let mut w = Wire::new(Protocol::Ext, Some(0.001));
        offer(&mut w, 4, 2);
        let s = session(&w);
        w.send(frame(&w), 3, &[]);
        w.app.request_due_captures();
        w.settle();
        assert_eq!(w.requests(s).len(), 1, "not before its cap");
        w.source_mut().live = Some(f32::INFINITY);
        w.app.request_due_captures();
        w.settle();
        assert_eq!(w.requests(s).len(), 2, "the loop requests a due source");
    }

    /// `dmabuf_format` collects one format/modifier pair per modifier for the batch's `done`.
    #[test]
    fn dmabuf_formats_collect_per_modifier_until_done() {
        let mut w = Wire::new(Protocol::Ext, None);
        let s = session(&w);
        let mut payload = 0x3432_5258_u32.to_ne_bytes().to_vec();
        payload.extend_from_slice(&16_u32.to_ne_bytes());
        payload.extend_from_slice(&0_u64.to_ne_bytes());
        payload.extend_from_slice(&0x0100_0000_0000_0001_u64.to_ne_bytes());
        w.mock.send(s, 3, &payload);
        w.settle();
        let formats = [0, 0x0100_0000_0000_0001].map(|modifier| FormatModifier { fourcc: 0x3432_5258, modifier });
        assert_eq!(w.ext().dmabuf_formats, formats);
        offer(&mut w, 4, 2);
        assert!(w.ext().dmabuf_formats.is_empty(), "consumed by the batch");
        assert_eq!(w.requests(frame(&w))[0], (1, buffer(&w)), "no dma-buf support here: shm");
    }

    /// A `done` that arrives with a frame out is applied once that frame lands, or once it fails on
    /// `buffer_constraints`; that failure with nothing parked just asks again, `stopped` leaves the
    /// decision to the session's own `stopped`, and anything else pauses the source.
    #[test]
    fn a_done_during_a_frame_waits_for_it_and_failures_renegotiate_wait_or_pause() {
        let mut w = Wire::new(Protocol::Ext, Some(f32::INFINITY));
        let s = session(&w);
        offer(&mut w, 4, 2);
        let (f1, b1) = (frame(&w), buffer(&w));
        offer(&mut w, 8, 2);
        assert_eq!((w.requests(s).len(), buffer(&w)), (1, b1), "parked behind the outstanding frame");
        w.send(f1, 3, &[]);
        let (f2, b2) = (frame(&w), buffer(&w));
        assert_ne!(b2, b1);
        assert_eq!(w.ext().buffer.as_ref().unwrap().width, 8);
        assert_eq!(w.requests(f2)[0], (1, b2), "the landed frame's successor uses the parked offer");

        offer(&mut w, 16, 2);
        w.send(f2, 4, &[1]);
        let f3 = frame(&w);
        assert_ne!(f3, f2);
        assert_eq!(w.ext().buffer.as_ref().unwrap().width, 16, "a resize renegotiates");
        assert!(!w.source().failed);

        w.send(f3, 4, &[1]);
        let f4 = frame(&w);
        assert_ne!(f4, f3);
        assert_eq!(w.requests(f4)[0], (1, buffer(&w)), "nothing parked: the next frame, same buffer");

        w.send(f4, 4, &[2]);
        assert_eq!(w.requests(f4).last(), Some(&(0, 0)));
        assert!(!w.source().failed && w.source().in_flight, "`stopped` waits for the session's word");
        w.send(s, 5, &[]);
        assert_eq!(w.requests(s).last(), Some(&(1, 0)), "the stopped session is destroyed");
        let source = w.source();
        assert!(source.proto.is_none() && !source.in_flight && source.deferred, "a live output retries");

        let mut w = Wire::new(Protocol::Ext, Some(f32::INFINITY));
        let s = session(&w);
        offer(&mut w, 4, 2);
        let f = frame(&w);
        w.send(f, 4, &[0]);
        assert_eq!(w.requests(f).last(), Some(&(0, 0)));
        assert!(w.source().failed && !w.source().in_flight, "any other failure pauses");
        w.app.request_due_captures();
        w.settle();
        assert_eq!(w.requests(s).len(), 1, "the loop does not revive a paused source");
    }

    /// A session lives across frames until the source's `paint_cursor` changes.
    #[test]
    fn a_session_is_reused_until_paint_cursor_changes() {
        let node = CaptureNode {
            node: ID,
            target: CaptureTarget::Output("TEST".into()),
            live: Some(f32::INFINITY),
            paint_cursor: true,
            region: None,
        };
        let mut w = Wire::with(Protocol::Ext, &node);
        w.app.request_frame(ID);
        w.settle();
        let manager = id(w.app.captures.backend.ext.as_ref());
        let s = session(&w);
        assert_eq!(w.requests(manager), [(0, s)]);
        offer(&mut w, 4, 2);
        w.send(frame(&w), 3, &[]);
        assert_eq!((w.requests(s).len(), w.requests(manager).len()), (2, 1), "the same session");
        w.source_mut().paint_cursor = false;
        w.send(frame(&w), 3, &[]);
        assert_eq!(w.requests(s).last(), Some(&(1, 0)), "the old session is destroyed");
        let fresh = session(&w);
        assert_eq!(w.requests(manager), [(0, s), (0, fresh)]);
    }
}

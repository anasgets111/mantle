//! wlr-screencopy: one frame object per request, negotiated at `BufferDone`.

use wayland_client::{Dispatch, WEnum};

use super::shm::{negotiate_buffer, pick_format};
use super::*;

/// wlr-screencopy's `Buffer` event, checked before it is trusted: an unsupported format would
/// silently corrupt colours (`bgrx_to_rgba` assumes BGRX), and a stride narrower than the copy
/// assumes (`width * 4`) would panic slicing `stage_landed`'s rows.
fn wlr_buffer_valid(format: wl_shm::Format, width: u32, stride: u32) -> bool {
    pick_format(&[format]).is_some() && stride >= width.saturating_mul(4)
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
        let current = state.captures.sources.get(id).is_some_and(|source| source.owns(proxy));
        if !current {
            return;
        }
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
                    let cached = state.captures.sources.get(id).and_then(|s| s.dmabuf_state.shape());
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
                // ponytail: this includes a frame copied through shm while its dma-buf allocation
                // awaits `created`, failing a dma-buf-only source for good. Hyprland, niri and
                // Pinnacle all offer shm; the upgrade is to skip the frame and re-request it on
                // the answer, as ext does.
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
            Event::Flags { flags } => {
                if let Some(CaptureSource { proto: Some(Proto::Wlr(wlr)), .. }) = state.captures.sources.get_mut(id) {
                    wlr.y_invert =
                        matches!(flags, WEnum::Value(f) if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
                }
            }
            Event::Ready { .. } => {
                proxy.destroy();
                // ponytail: a y-inverted dma-buf copy stays flipped; compositors that set it are expected to offer shm too.
                if let Some(CaptureSource { proto: Some(Proto::Wlr(wlr)), .. }) = state.captures.sources.get_mut(id)
                    && wlr.y_invert
                    && !wlr.dmabuf_active
                    && let Some(buffer) = wlr.buffer.as_mut()
                {
                    buffer.flip_rows(&mut wlr.damage);
                }
                land_ready_frame(&mut state.captures, &mut state.capture_cache, id);
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
    use super::super::tests::{ID, Wire, id};
    use super::*;

    // Frame events: `buffer` 0, `flags` 1, `ready` 2, `failed` 3, `damage` 4, `linux_dmabuf` 5,
    // `buffer_done` 6; requests: `destroy` 1, `copy_with_damage` 2. Manager: `capture_output` 0.
    const XRGB: u32 = wl_shm::Format::Xrgb8888 as u32;

    fn frame(w: &mut Wire) -> u32 {
        id(w.wlr().frame.as_ref())
    }

    fn shm_buffer(w: &mut Wire) -> u32 {
        id(w.wlr().buffer.as_ref().map(|negotiated| negotiated.buffer.wl_buffer()))
    }

    /// A frame copies into the shm buffer its `buffer` event sized, lands upright whichever way
    /// its rows came, and the next frame copies into the same buffer.
    #[test]
    fn a_frame_copies_through_shm_lands_upright_and_reuses_its_buffer() {
        for y_invert in [false, true] {
            let mut w = Wire::new(Protocol::Wlr, Some(f32::INFINITY));
            let manager = id(w.app.captures.backend.wlr.as_ref());
            let f = frame(&mut w);
            assert_eq!(w.requests(manager), [(0, f)]);
            w.send(f, 0, &[XRGB, 1, 3, 4]);
            w.send(f, 6, &[]);
            let b = shm_buffer(&mut w);
            assert_eq!(w.requests(f), [(2, b)], "copy_with_damage into the shm buffer");
            let rows = [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];
            w.wlr().buffer.as_mut().unwrap().canvas_for_test()[..12].copy_from_slice(&rows);
            w.send(f, 4, &[0, 0, 1, 1]);
            w.send(f, 1, &[u32::from(y_invert)]);
            assert_eq!(w.wlr().damage, [DamageRect { x: 0, y: 0, width: 1, height: 1 }]);
            w.send(f, 2, &[0, 0, 0]);
            assert_eq!(w.requests(f).last(), Some(&(1, 0)), "the landed frame is destroyed");
            let landed = w.wlr().buffer.as_mut().unwrap().rows().unwrap().0[..12].to_vec();
            let upright = if y_invert { [3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1] } else { rows };
            assert_eq!(landed, upright, "y_invert {y_invert}");
            assert_eq!(w.app.capture_cache.poll(), [ID]);
            assert!(w.source().captured && w.wlr().damage.is_empty());

            let next = frame(&mut w);
            assert_eq!(w.requests(manager), [(0, f), (0, next)], "an uncapped source asks again");
            w.send(next, 0, &[XRGB, 1, 3, 4]);
            w.send(next, 6, &[]);
            assert_eq!(w.requests(next), [(2, b)], "the negotiated buffer is reused");
        }
    }

    /// No usable shm offer at `buffer_done` fails the source, as `failed` does.
    #[test]
    fn a_frame_without_a_usable_shm_offer_fails_the_source() {
        let bgr = wl_shm::Format::Bgr888 as u32;
        let offers: [&[[u32; 4]]; 3] = [&[], &[[XRGB, 4, 2, 8]], &[[bgr, 4, 2, 16]]];
        for buffers in offers {
            let mut w = Wire::new(Protocol::Wlr, Some(f32::INFINITY));
            let f = frame(&mut w);
            for args in buffers {
                w.send(f, 0, args);
            }
            w.send(f, 6, &[]);
            assert_eq!(w.requests(f), [(1, 0)], "{buffers:?}: destroyed, nothing copied");
            assert!(w.source().failed && !w.source().in_flight, "{buffers:?}");
        }
        let mut w = Wire::new(Protocol::Wlr, Some(f32::INFINITY));
        let f = frame(&mut w);
        w.send(f, 3, &[]);
        assert_eq!(w.requests(f), [(1, 0)]);
        assert!(w.source().failed && !w.source().in_flight, "`failed` pauses the source");
    }

    /// A frame copies into the dma-buf back slot only once the compositor has created it, and only
    /// for the exact shape it was allocated for; anything else copies through shm (this process
    /// has no dma-buf support to renegotiate with), and only a dma-buf frame awaits an import.
    #[test]
    fn a_frame_uses_the_created_dmabuf_slot_only_for_its_exact_shape() {
        const XR24: u32 = 0x3432_5258;
        let shape = DmabufShape { width: 4, height: 2, format: FormatModifier { fourcc: XR24, modifier: 0 } };
        // (slot created, `linux_dmabuf` offer, copies into the slot)
        let cases = [
            (true, Some([XR24, 4, 2]), true),
            (true, Some([XR24, 4, 4]), false),
            (true, Some([XR24, 8, 2]), false),
            (true, Some([0x3432_5241, 4, 2]), false),
            (true, None, false),
            (false, Some([XR24, 4, 2]), false),
        ];
        for (created, linux_dmabuf, into_slot) in cases {
            let label = format!("created {created}, offer {linux_dmabuf:?}");
            let mut w = Wire::new(Protocol::Wlr, None);
            let qh = w.app.queue_handle.clone();
            let params = w.dmabuf.create_params(&qh, ID);
            let source = w.source_mut();
            source.dmabuf_state = DmabufState::Shaped(shape);
            source.dmabuf = DmabufSwapchain::pending_for_test(shape, params.clone());
            if created {
                // `created` is event 0; 0xff00_0000 is the first server-allocated id.
                w.send(params.id().protocol_id(), 0, &[0xff00_0000]);
            }
            let f = frame(&mut w);
            if let Some(args) = linux_dmabuf {
                w.send(f, 5, &args);
            }
            w.send(f, 0, &[XRGB, 4, 2, 16]);
            w.send(f, 6, &[]);
            let into = if into_slot { 0xff00_0000 } else { shm_buffer(&mut w) };
            assert_eq!(w.requests(f), [(2, into)], "{label}");
            assert_eq!(w.wlr().dmabuf_active, into_slot, "{label}");
            w.send(f, 2, &[0, 0, 0]);
            assert_eq!(w.app.captures.pending_import == [ID], into_slot, "{label}");
            assert!(w.source().captured && !w.source().in_flight, "{label}");
        }
    }

    #[test]
    fn a_wlr_buffer_offer_needs_a_supported_format_and_a_wide_enough_stride() {
        assert!(wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 400));
        assert!(wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 512), "padded stride is still fine");
        assert!(!wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 399), "narrower than width * 4 would panic the copy");
        assert!(!wlr_buffer_valid(wl_shm::Format::Bgr888, 100, 400), "unsupported format, whatever the stride");
    }
}

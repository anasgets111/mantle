//! wlr-screencopy: one frame object per request, negotiated at `BufferDone`.

use wayland_client::{Dispatch, WEnum};

use super::shm::{negotiate_buffer, pick_format, stage_landed};
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
                if let Some(Proto::Wlr(wlr)) = source.proto.as_mut() {
                    wlr.frame = None;
                }
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
    fn a_wlr_buffer_offer_needs_a_supported_format_and_a_wide_enough_stride() {
        assert!(wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 400));
        assert!(wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 512), "padded stride is still fine");
        assert!(!wlr_buffer_valid(wl_shm::Format::Xrgb8888, 100, 399), "narrower than width * 4 would panic the copy");
        assert!(!wlr_buffer_valid(wl_shm::Format::Bgr888, 100, 400), "unsupported format, whatever the stride");
    }
}

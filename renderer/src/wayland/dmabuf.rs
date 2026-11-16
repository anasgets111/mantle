//! Client-allocated dma-buf capture buffers (ADR-0248 amendment): gbm allocation on EGL's own
//! render node, `zwp_linux_dmabuf_v1` for the `wl_buffer`, `eglCreateImage` +
//! `glEGLImageTargetTexture2DOES` to import as a femtovg texture. Any failure here falls back to
//! shm; `wayland::capture` decides when to call in.

use std::fs::File;
use std::os::unix::io::AsFd;
use std::path::Path;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageFlags, ImageId, ImageInfo, PixelFormat};
use glow::HasContext;
use khronos_egl as egl;
use wayland_client::{QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};

use super::App;
use super::egl::EglState;
use super::egl_ext::{DmabufEntryPoints, DmabufPlane};
use crate::layout::scene::NodeId;

/// wl_shm's `Xrgb8888`/`Argb8888` preference (`wayland::capture::pick_format`), as the DRM fourcc
/// codes `gbm`/`zwp_linux_dmabuf_v1` use instead.
const PREFERRED_FOURCC: [u32; 2] = [gbm::Format::Xrgb8888 as u32, gbm::Format::Argb8888 as u32];

/// One fourcc/modifier pair, as either a driver or a compositor offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatModifier {
    pub fourcc: u32,
    pub modifier: u64,
}

/// Picks a format/modifier both sides can use: preferred fourcc first, first shared modifier
/// otherwise any shared pair (ADR-0248 amendment decision 1).
pub fn pick_dmabuf_format(
    offered: &[FormatModifier],
    importable: &[FormatModifier],
    opaque: bool,
) -> Option<FormatModifier> {
    let shared = |fourcc: u32| offered.iter().find(|o| o.fourcc == fourcc && importable.contains(o)).copied();
    if !opaque {
        // ponytail: window dma-buf accepts 8/10-bit ARGB/ABGR. Other formats use shm until
        // their alpha semantics are covered here; an opaque format must not discard window alpha.
        return [gbm::Format::Argb8888, gbm::Format::Abgr8888, gbm::Format::Argb2101010, gbm::Format::Abgr2101010]
            .into_iter()
            .find_map(|format| shared(format as u32));
    }
    PREFERRED_FOURCC.into_iter().find_map(shared).or_else(|| offered.iter().find(|o| importable.contains(o)).copied())
}

/// A negotiated buffer's shape: reused across frames while it matches (amendment decision 3),
/// reallocated otherwise, same rule `wayland::capture::negotiate_buffer` applies to shm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufShape {
    pub width: u32,
    pub height: u32,
    pub format: FormatModifier,
}

/// Process-wide dma-buf capability, probed once EGL exists (ADR-0248 amendment decision 1).
/// An `Err` from [`Self::probe`] means this process draws every capture through shm, logged once
/// by the caller with the reason.
pub struct DmabufSupport {
    gbm: gbm::Device<File>,
    entry_points: DmabufEntryPoints,
}

impl DmabufSupport {
    pub fn probe(egl: &EglState) -> Result<Self, String> {
        let entry_points = DmabufEntryPoints::load(egl).ok_or("the driver lacks the EGL dma-buf extensions")?;
        let render_node = entry_points.render_node(egl).ok_or("EGL reports no DRM render node")?;
        Ok(DmabufSupport { gbm: open_gbm(&render_node)?, entry_points })
    }

    pub fn importable_modifiers(&self, egl: &EglState, fourcc: u32) -> Vec<FormatModifier> {
        self.entry_points
            .importable_modifiers(egl, fourcc)
            .into_iter()
            .map(|modifier| FormatModifier { fourcc, modifier })
            .collect()
    }
}

/// Opens `node` as a gbm device, naming whether the open or gbm itself failed.
fn open_gbm(node: &Path) -> Result<gbm::Device<File>, String> {
    let file = File::open(node).map_err(|e| format!("cannot open {}: {e}", node.display()))?;
    gbm::Device::new(file).map_err(|e| format!("gbm cannot use {}: {e}", node.display()))
}

/// A dma-buf-backed capture buffer: the gbm allocation, its `wl_buffer` once the compositor
/// answers `created`, and, once imported at a canvas-current point, the texture every later frame
/// into this same buffer reuses.
pub struct DmabufBuffer {
    /// `None` only for a test buffer, which never reaches `import`.
    bo: Option<gbm::BufferObject<()>>,
    wl_buffer: Option<wayland_client::protocol::wl_buffer::WlBuffer>,
    shape: DmabufShape,
    texture: Option<(egl::Image, ImageId, glow::NativeTexture)>,
}

impl DmabufBuffer {
    pub fn shape(&self) -> DmabufShape {
        self.shape
    }

    /// Only for a buffer [`DmabufSwapchain::back`] returned: one still pending has no `wl_buffer`.
    pub fn wl_buffer(&self) -> &wayland_client::protocol::wl_buffer::WlBuffer {
        self.wl_buffer.as_ref().expect("the swapchain hands out only created buffers")
    }

    pub fn image(&self) -> Option<ImageId> {
        self.texture.map(|(_, image, _)| image)
    }

    /// Takes this buffer's imported resources so the caller can queue them for a canvas-current
    /// free (ADR-0039); dropping a `DmabufBuffer` without calling this first leaks the GL texture
    /// and `EGLImage`, since freeing either needs a current GL context `Drop` cannot guarantee.
    pub fn take_texture(&mut self) -> Option<(egl::Image, ImageId, glow::NativeTexture)> {
        self.texture.take()
    }
}

impl Drop for DmabufBuffer {
    fn drop(&mut self) {
        if let Some(wl_buffer) = &self.wl_buffer {
            wl_buffer.destroy();
        }
    }
}

/// Allocates a gbm buffer object for `shape` and asks the compositor to import it with `create`,
/// whose `created`/`failed` answer reaches `id`'s params handler (#65): `create_immed` would turn
/// a rejection into a protocol error that ends the connection. `None` means the local allocation
/// failed (no format support, no plane fd); the caller falls back to shm for this source
/// (amendment decision 4).
pub fn allocate(
    support: &DmabufSupport,
    dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<App>,
    shape: DmabufShape,
    id: NodeId,
) -> Option<(DmabufBuffer, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1)> {
    let format = gbm::Format::try_from(shape.format.fourcc).ok()?;
    let modifier = gbm::Modifier::from(shape.format.modifier);
    let bo: gbm::BufferObject<()> = support
        .gbm
        .create_buffer_object_with_modifiers2(
            shape.width,
            shape.height,
            format,
            std::iter::once(modifier),
            gbm::BufferObjectFlags::RENDERING,
        )
        .ok()?;

    let params = dmabuf.create_params(qh, id);
    let plane_count = bo.plane_count();
    let mut fds = Vec::with_capacity(plane_count as usize);
    for plane in 0..plane_count {
        let fd = bo.fd_for_plane(plane as i32).ok()?;
        let modifier: u64 = bo.modifier().into();
        params.add(
            fd.as_fd(),
            plane,
            bo.offset(plane as i32),
            bo.stride_for_plane(plane as i32),
            (modifier >> 32) as u32,
            (modifier & 0xFFFF_FFFF) as u32,
        );
        fds.push(fd);
    }
    params.create(
        shape.width as i32,
        shape.height as i32,
        shape.format.fourcc,
        zwp_linux_buffer_params_v1::Flags::empty(),
    );
    drop(fds);

    Some((DmabufBuffer { bo: Some(bo), wl_buffer: None, shape, texture: None }, params))
}

/// Imports `buffer`'s gbm allocation as a `GL_TEXTURE_2D` and wraps it for femtovg, once per
/// buffer (amendment decision 3). Must run with the canvas's GL context current (ADR-0039); any
/// failure leaves `buffer.texture` unset, and the caller treats that as a dma-buf failure
/// (amendment decision 4).
pub fn import(
    support: &DmabufSupport,
    egl: &EglState,
    gl: &glow::Context,
    canvas: &mut Canvas<OpenGl>,
    buffer: &mut DmabufBuffer,
    opaque: bool,
) -> Option<()> {
    if buffer.texture.is_some() {
        return Some(());
    }
    let bo = buffer.bo.as_ref()?;
    let plane_count = bo.plane_count();
    let mut fds = Vec::with_capacity(plane_count as usize);
    let mut planes = Vec::with_capacity(plane_count as usize);
    for plane in 0..plane_count {
        let fd = bo.fd_for_plane(plane as i32).ok()?;
        planes.push(DmabufPlane {
            fd: std::os::unix::io::AsRawFd::as_raw_fd(&fd),
            offset: bo.offset(plane as i32),
            pitch: bo.stride_for_plane(plane as i32),
        });
        fds.push(fd);
    }
    let modifier: u64 = bo.modifier().into();
    let egl_image = support.entry_points.create_dmabuf_image(
        egl,
        buffer.shape.width,
        buffer.shape.height,
        buffer.shape.format.fourcc,
        modifier,
        &planes,
    )?;
    drop(fds);

    // SAFETY: the caller guarantees the GL context is current; `texture` is freshly created and
    // bound to `GL_TEXTURE_2D` immediately below.
    let texture = unsafe {
        let Ok(texture) = gl.create_texture() else {
            let _ = egl.destroy_image(egl_image);
            return None;
        };
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        support.entry_points.image_target_texture_2d_oes(egl_image);
        // EGL maps the fourcc channels. Windows keep alpha; outputs retain opaque capture.
        if opaque {
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_A, glow::ONE as i32);
        }
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.bind_texture(glow::TEXTURE_2D, None);
        texture
    };
    let info = ImageInfo::new(
        ImageFlags::PREMULTIPLIED,
        buffer.shape.width as usize,
        buffer.shape.height as usize,
        PixelFormat::Rgba8,
    );
    let Ok(image) = canvas.create_image_from_native_texture(texture, info) else {
        // SAFETY: as above; `texture` was created here and nothing else holds it.
        unsafe { gl.delete_texture(texture) };
        let _ = egl.destroy_image(egl_image);
        return None;
    };
    buffer.texture = Some((egl_image, image, texture));
    Some(())
}

/// Frees a texture [`DmabufBuffer::take_texture`] handed back, once the canvas's GL context is
/// current. The only place `wayland::dmabuf` deletes a GL object or destroys an `EGLImage`.
pub fn free_texture(
    egl: &EglState,
    gl: &glow::Context,
    canvas: &mut Canvas<OpenGl>,
    (image, femto_image, texture): (egl::Image, ImageId, glow::NativeTexture),
) {
    // `owned == false` on a native-texture image (femtovg's own invariant): `delete_image` frees
    // only femtovg's bookkeeping, so the GL texture is deleted here explicitly.
    canvas.delete_image(femto_image);
    // SAFETY: the caller guarantees the GL context is current; `texture` was created by `import`
    // and not deleted since.
    unsafe { gl.delete_texture(texture) };
    let _ = egl.destroy_image(image);
}

/// A capture source's double buffer (ADR-0248 amendment decision 3): the compositor writes into
/// the back slot while the front slot's texture is what `image::capture::CaptureCache` draws.
#[derive(Default)]
pub struct DmabufSwapchain {
    slots: [Option<DmabufBuffer>; 2],
    front: Option<usize>,
    /// The params of the back slot's allocation still awaiting `created`/`failed`, and that slot.
    pub(super) pending: Option<(zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, usize)>,
}

impl DmabufSwapchain {
    fn back_index(&self) -> usize {
        self.front.map_or(0, |front| 1 - front)
    }

    /// Ensures the back slot matches `wanted`, reallocating through `alloc` if it does not. Any
    /// buffer a reallocation discards is returned so the caller can queue its texture for a
    /// canvas-current free. `false` means the allocation failed; `true` covers one still awaiting
    /// `created`, whose [`Self::back`] is `None` until then. A replaced pending allocation is not
    /// destroyed here: its answer still arrives, and [`Self::answer`] reports it stale.
    pub fn ensure_back(
        &mut self,
        wanted: DmabufShape,
        alloc: impl FnOnce() -> Option<(DmabufBuffer, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1)>,
    ) -> (bool, Option<DmabufBuffer>) {
        let index = self.back_index();
        if self.slots[index].as_ref().map(DmabufBuffer::shape) == Some(wanted) {
            return (true, None);
        }
        match alloc() {
            Some((fresh, params)) => {
                self.pending = Some((params, index));
                (true, self.slots[index].replace(fresh))
            }
            None => (false, self.slots[index].take()),
        }
    }

    /// The compositor answered `params`: `Some(buffer)` (`created`) completes the pending slot,
    /// `None` (`failed`) drops it. `false` for an answer to anything but the current pending
    /// allocation, which the caller cleans up.
    pub fn answer(
        &mut self,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        buffer: Option<&wayland_client::protocol::wl_buffer::WlBuffer>,
    ) -> bool {
        use wayland_client::Proxy;
        let Some((_, index)) = self.pending.take_if(|(pending, _)| pending.id() == params.id()) else {
            return false;
        };
        match (buffer, self.slots[index].as_mut()) {
            (Some(buffer), Some(slot)) => slot.wl_buffer = Some(buffer.clone()),
            (Some(_), None) => return false,
            (None, _) => self.slots[index] = None,
        }
        true
    }

    /// The back slot, once the compositor has created its `wl_buffer`.
    pub fn back(&self) -> Option<&DmabufBuffer> {
        self.slots[self.back_index()].as_ref().filter(|slot| slot.wl_buffer.is_some())
    }

    /// The back slot just landed a frame: it becomes the front (what `image()` reads); the
    /// previous front becomes the back for the next request.
    pub fn advance(&mut self) {
        self.front = Some(self.back_index());
    }

    pub fn front(&self) -> Option<&DmabufBuffer> {
        self.front.and_then(|index| self.slots[index].as_ref())
    }

    pub fn front_mut(&mut self) -> Option<&mut DmabufBuffer> {
        self.front.and_then(move |index| self.slots[index].as_mut())
    }

    /// Every texture still held by either slot, for a full teardown.
    pub fn take_textures(&mut self) -> Vec<(egl::Image, ImageId, glow::NativeTexture)> {
        self.slots.iter_mut().flatten().filter_map(DmabufBuffer::take_texture).collect()
    }
}

#[cfg(test)]
impl DmabufSwapchain {
    /// A back slot of `shape` awaiting `params`, with no gbm buffer behind it.
    pub(super) fn pending_for_test(
        shape: DmabufShape,
        params: zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) -> Self {
        let buffer = DmabufBuffer { bo: None, wl_buffer: None, shape, texture: None };
        DmabufSwapchain { slots: [Some(buffer), None], front: None, pending: Some((params, 0)) }
    }
}

// `format`/`modifier` do fire (legacy pre-feedback advertisement); modifiers come from
// `eglQueryDmaBufModifiersEXT` instead, so both are ignored rather than asserted unreachable.
delegate_noop!(App: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
// `release` does fire: `ext_image_copy_capture_frame_v1`'s own doc says it is unused, and
// wlr-screencopy destroys its frame right after `ready` regardless.
delegate_noop!(App: ignore wayland_client::protocol::wl_buffer::WlBuffer);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_gbm_names_the_failing_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = open_gbm(&dir.path().join("renderD128")).expect_err("no such node");
        assert!(missing.starts_with("cannot open ") && missing.contains("No such file"), "{missing}");
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"").expect("write");
        let not_drm = open_gbm(&plain).expect_err("a regular file is no DRM device");
        assert!(not_drm.starts_with("gbm cannot use "), "{not_drm}");
    }

    fn fm(fourcc: u32, modifier: u64) -> FormatModifier {
        FormatModifier { fourcc, modifier }
    }

    #[test]
    fn preferred_fourcc_wins_in_any_offer_order_and_only_a_format_both_sides_share_is_picked() {
        let offered = [fm(gbm::Format::Argb8888 as u32, 0), fm(gbm::Format::Xrgb8888 as u32, 0)];
        let importable = offered;
        assert_eq!(pick_dmabuf_format(&offered, &importable, true), Some(fm(gbm::Format::Xrgb8888 as u32, 0)));
        assert_eq!(pick_dmabuf_format(&offered, &importable, false), Some(fm(gbm::Format::Argb8888 as u32, 0)));
        let ten_bit = [fm(gbm::Format::Xrgb8888 as u32, 0), fm(gbm::Format::Argb2101010 as u32, 0)];
        assert_eq!(pick_dmabuf_format(&ten_bit, &ten_bit, false), Some(ten_bit[1]));
        assert_eq!(
            pick_dmabuf_format(&ten_bit[..1], &ten_bit[..1], false),
            None,
            "opaque-only dma-buf offers use shm for windows"
        );

        // Only a format both sides share is picked.
        let weird = fm(0x1234_5678, 7);
        assert_eq!(pick_dmabuf_format(&[weird], &[weird], true), Some(weird), "an unpreferred shared format");
        let offered = [fm(gbm::Format::Xrgb8888 as u32, 1)];
        let importable = [fm(gbm::Format::Xrgb8888 as u32, 2)];
        assert_eq!(pick_dmabuf_format(&offered, &importable, true), None, "nothing shared is no format");
    }
}

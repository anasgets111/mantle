//! Raw EGL/GLES entry points ADR-0248's dmabuf amendment needs beyond what `khronos-egl` 6.0
//! wraps: device query, dma-buf import modifier queries, and `glEGLImageTargetTexture2DOES`.
//! Loaded once via `eglGetProcAddress`.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::io::RawFd;
use std::path::PathBuf;

use khronos_egl as egl;

use super::egl::EglState;

const EGL_DEVICE_EXT: egl::Int = 0x322C;
const EGL_DRM_DEVICE_FILE_EXT: egl::Int = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: egl::Int = 0x3377;
pub const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;

/// `EGL_DMA_BUF_PLANEn_{FD,OFFSET,PITCH,MODIFIER_LO,MODIFIER_HI}_EXT`, `n` up to 3. Not
/// arithmetic: `EGL_EXT_image_dma_buf_import`'s token allocation puts plane 3 in its own block.
fn plane_attribs(plane: usize) -> Option<[egl::Attrib; 5]> {
    Some(match plane {
        0 => [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
        1 => [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
        2 => [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
        3 => [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
        _ => return None,
    })
}

/// One plane of a dma-buf, as `wayland::dmabuf` reads it off a `gbm::BufferObject`.
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub pitch: u32,
}

type QueryDisplayAttribExt = unsafe extern "system" fn(egl::EGLDisplay, egl::Int, *mut egl::Attrib) -> egl::Boolean;
type QueryDeviceStringExt = unsafe extern "system" fn(*mut c_void, egl::Int) -> *const c_char;
type QueryDmaBufModifiersExt = unsafe extern "system" fn(
    egl::EGLDisplay,
    u32,
    egl::Int,
    *mut u64,
    *mut egl::Boolean,
    *mut egl::Int,
) -> egl::Boolean;
type ImageTargetTexture2DOes = unsafe extern "system" fn(u32, *mut c_void);

/// Looks up one extension function; `None` means the driver lacks it, read by callers as "no
/// dma-buf import" (ADR-0248 amendment decision 4).
fn load<T>(instance: &egl::Instance<egl::Static>, name: &str) -> Option<T> {
    let addr = instance.get_proc_address(name)?;
    // SAFETY: `name` names a fixed EGL/GLES extension whose C signature matches `T`; a non-null
    // `eglGetProcAddress` return is valid for the process's lifetime.
    Some(unsafe { std::mem::transmute_copy::<extern "system" fn(), T>(&addr) })
}

/// The entry points a dma-buf import needs, resolved once against the live EGL display.
pub struct DmabufEntryPoints {
    query_display_attrib: QueryDisplayAttribExt,
    query_device_string: QueryDeviceStringExt,
    query_modifiers: QueryDmaBufModifiersExt,
    image_target_texture_2d_oes: ImageTargetTexture2DOes,
}

impl DmabufEntryPoints {
    /// `None` if this driver is missing any one of the four extensions; the caller treats that as
    /// "no dma-buf import" (amendment decision 4), same as a missing compositor protocol.
    pub fn load(egl: &EglState) -> Option<Self> {
        Some(DmabufEntryPoints {
            query_display_attrib: load(&egl.instance, "eglQueryDisplayAttribEXT")?,
            query_device_string: load(&egl.instance, "eglQueryDeviceStringEXT")?,
            query_modifiers: load(&egl.instance, "eglQueryDmaBufModifiersEXT")?,
            image_target_texture_2d_oes: load(&egl.instance, "glEGLImageTargetTexture2DOES")?,
        })
    }

    /// This process's own render node: `EGL_EXT_device_query` to the `EGLDeviceEXT` behind
    /// `display`, then `EGL_EXT_device_drm_render_node` on it, falling back to the primary node's
    /// path (`EGL_EXT_device_drm`) for drivers with no render node.
    pub fn render_node(&self, egl: &EglState) -> Option<PathBuf> {
        let mut device: egl::Attrib = 0;
        // SAFETY: `display` is a live EGL display; `device` is out-only, sized for `EGLAttrib`.
        if unsafe { (self.query_display_attrib)(egl.display.as_ptr(), EGL_DEVICE_EXT, &mut device) } == egl::FALSE {
            return None;
        }
        [EGL_DRM_RENDER_NODE_FILE_EXT, EGL_DRM_DEVICE_FILE_EXT].into_iter().find_map(|name| {
            // SAFETY: `device` is the `EGLDeviceEXT` the query above returned.
            let ptr = unsafe { (self.query_device_string)(device as *mut c_void, name) };
            // SAFETY: a non-null return is a NUL-terminated string the driver owns statically.
            (!ptr.is_null()).then(|| PathBuf::from(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()))
        })
    }

    /// Every modifier this driver can import `fourcc` with as a plain (non-external-only)
    /// `GL_TEXTURE_2D`. Decision 2 binds every capture as `GL_TEXTURE_2D`, so an external-only
    /// modifier (planar/YUV) is not a candidate.
    pub fn importable_modifiers(&self, egl: &EglState, fourcc: u32) -> Vec<u64> {
        const MAX: usize = 64;
        let mut modifiers = [0u64; MAX];
        let mut external_only = [0 as egl::Boolean; MAX];
        let mut count: egl::Int = 0;
        // SAFETY: `modifiers` and `external_only` are both sized `MAX`, matching the bound passed.
        let ok = unsafe {
            (self.query_modifiers)(
                egl.display.as_ptr(),
                fourcc,
                MAX as egl::Int,
                modifiers.as_mut_ptr(),
                external_only.as_mut_ptr(),
                &mut count,
            )
        };
        if ok == egl::FALSE {
            return Vec::new();
        }
        (0..(count as usize).min(MAX)).filter(|&i| external_only[i] == egl::FALSE).map(|i| modifiers[i]).collect()
    }

    /// Imports `planes` as an `EGLImage`, `EGL_LINUX_DMA_BUF_EXT` target (ADR-0248 amendment
    /// decision 2). `modifier` is always attached: `wayland::dmabuf::import` never has one to
    /// omit, since `gbm::BufferObject::modifier` answers even an implicit allocation.
    pub fn create_dmabuf_image(
        &self,
        egl: &EglState,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        planes: &[DmabufPlane],
    ) -> Option<egl::Image> {
        let mut attribs: Vec<egl::Attrib> = vec![
            egl::WIDTH as egl::Attrib,
            width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as egl::Attrib,
        ];
        for (index, plane) in planes.iter().enumerate() {
            let keys = plane_attribs(index)?;
            attribs.extend([
                keys[0],
                plane.fd as egl::Attrib,
                keys[1],
                plane.offset as egl::Attrib,
                keys[2],
                plane.pitch as egl::Attrib,
                keys[3],
                (modifier & 0xFFFF_FFFF) as egl::Attrib,
                keys[4],
                (modifier >> 32) as egl::Attrib,
            ]);
        }
        attribs.push(egl::ATTRIB_NONE);
        // SAFETY: `EGL_NO_CONTEXT`/`EGL_NO_CLIENT_BUFFER` are the values
        // `EGL_EXT_image_dma_buf_import` requires for this target; both are null sentinels, not
        // live handles.
        let (ctx, buffer) =
            unsafe { (egl::Context::from_ptr(egl::NO_CONTEXT), egl::ClientBuffer::from_ptr(std::ptr::null_mut())) };
        egl.instance.create_image(egl.display, ctx, EGL_LINUX_DMA_BUF_EXT, buffer, &attribs).ok()
    }

    /// Binds `image` onto whichever `GL_TEXTURE_2D` is currently bound.
    ///
    /// # Safety
    /// The GL context must be current on this thread, a texture must already be bound to
    /// `GL_TEXTURE_2D`, and `image` must not have been destroyed.
    pub unsafe fn image_target_texture_2d_oes(&self, image: egl::Image) {
        // SAFETY: forwarded from this function's own contract.
        unsafe { (self.image_target_texture_2d_oes)(glow::TEXTURE_2D, image.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_attribs_cover_all_four_planes_and_nothing_past_them() {
        assert!(plane_attribs(0).is_some());
        assert!(plane_attribs(3).is_some());
        assert_eq!(plane_attribs(4), None);
        // Every plane's five keys are distinct from every other plane's.
        let all: Vec<egl::Attrib> = (0..4).flat_map(|p| plane_attribs(p).unwrap()).collect();
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(all.len(), unique.len());
    }
}

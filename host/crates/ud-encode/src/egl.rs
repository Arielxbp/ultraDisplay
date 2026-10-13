//! Minimal EGL (runtime-loaded libEGL.so.1) for importing DMA-BUFs as `EGLImage`s on the
//! NVIDIA device, so CUDA can read them without a CPU copy.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use anyhow::{Context, bail};
use libloading::Library;

type EGLDisplay = *mut c_void;
type EGLDeviceEXT = *mut c_void;
pub type EGLImage = *mut c_void;
type EGLBoolean = u32;
type EGLint = i32;

const EGL_NONE: EGLint = 0x3038;
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_EXTENSIONS: EGLint = 0x3055;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;
const EGL_DRM_DEVICE_FILE_EXT: EGLint = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: EGLint = 0x3377;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
pub const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA8: u32 = 0x8058;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_NEAREST: u32 = 0x2600;

/// DRM fourcc codes for the formats niri casts.
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// Marks "modifier unknown / implicit".
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

struct Api {
    get_display: unsafe extern "C" fn(u32, *mut c_void, *const EGLint) -> EGLDisplay,
    initialize: unsafe extern "C" fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean,
    terminate: unsafe extern "C" fn(EGLDisplay) -> EGLBoolean,
    query_string: unsafe extern "C" fn(EGLDisplay, EGLint) -> *const c_char,
    query_devices: unsafe extern "C" fn(EGLint, *mut EGLDeviceEXT, *mut EGLint) -> EGLBoolean,
    query_device_string: unsafe extern "C" fn(EGLDeviceEXT, EGLint) -> *const c_char,
    query_modifiers: unsafe extern "C" fn(
        EGLDisplay,
        EGLint,
        EGLint,
        *mut u64,
        *mut EGLBoolean,
        *mut EGLint,
    ) -> EGLBoolean,
    create_image:
        unsafe extern "C" fn(EGLDisplay, *mut c_void, u32, *mut c_void, *const EGLint) -> EGLImage,
    destroy_image: unsafe extern "C" fn(EGLDisplay, EGLImage) -> EGLBoolean,
    get_error: unsafe extern "C" fn() -> EGLint,
    bind_api: unsafe extern "C" fn(u32) -> EGLBoolean,
    create_context:
        unsafe extern "C" fn(EGLDisplay, *mut c_void, *mut c_void, *const EGLint) -> *mut c_void,
    destroy_context: unsafe extern "C" fn(EGLDisplay, *mut c_void) -> EGLBoolean,
    make_current:
        unsafe extern "C" fn(EGLDisplay, *mut c_void, *mut c_void, *mut c_void) -> EGLBoolean,
    // GL, resolved through eglGetProcAddress (EGL_KHR_get_all_proc_addresses).
    gen_textures: unsafe extern "C" fn(i32, *mut u32),
    delete_textures: unsafe extern "C" fn(i32, *const u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    image_target_texture: unsafe extern "C" fn(u32, *mut c_void),
    gl_get_error: unsafe extern "C" fn() -> u32,
    tex_storage_2d: unsafe extern "C" fn(u32, i32, u32, i32, i32),
    gen_framebuffers: unsafe extern "C" fn(i32, *mut u32),
    delete_framebuffers: unsafe extern "C" fn(i32, *const u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    blit_framebuffer: unsafe extern "C" fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32),
}

pub struct EglDisplay {
    api: Api,
    display: EGLDisplay,
    context: *mut c_void,
    /// Read/draw framebuffers for blits, created with the GL context.
    fbos: [u32; 2],
    pub device: String,
    _lib: Library,
}

unsafe impl Send for EglDisplay {}

/// One single-plane DMA-BUF as delivered by PipeWire.
#[derive(Debug, Clone, Copy)]
pub struct DmaBufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
}

impl EglDisplay {
    /// Opens the EGL device display for the GPU whose DRM node path contains `node_hint`
    /// (e.g. "card1"), or the first DRM-backed device.
    pub fn open(node_hint: Option<&str>) -> anyhow::Result<Self> {
        unsafe {
            let lib = Library::new("libEGL.so.1").context("loading libEGL.so.1")?;
            let get_proc: libloading::Symbol<unsafe extern "C" fn(*const c_char) -> *mut c_void> =
                lib.get(b"eglGetProcAddress\0")?;
            macro_rules! ext {
                ($name:literal) => {{
                    let p = get_proc(concat!($name, "\0").as_ptr().cast());
                    if p.is_null() {
                        bail!("EGL: missing {}", $name);
                    }
                    std::mem::transmute(p)
                }};
            }
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())?
                };
            }
            let api = Api {
                get_display: ext!("eglGetPlatformDisplayEXT"),
                initialize: sym!("eglInitialize"),
                terminate: sym!("eglTerminate"),
                query_string: sym!("eglQueryString"),
                query_devices: ext!("eglQueryDevicesEXT"),
                query_device_string: ext!("eglQueryDeviceStringEXT"),
                query_modifiers: ext!("eglQueryDmaBufModifiersEXT"),
                create_image: ext!("eglCreateImageKHR"),
                destroy_image: ext!("eglDestroyImageKHR"),
                get_error: sym!("eglGetError"),
                bind_api: sym!("eglBindAPI"),
                create_context: sym!("eglCreateContext"),
                destroy_context: sym!("eglDestroyContext"),
                make_current: sym!("eglMakeCurrent"),
                gen_textures: ext!("glGenTextures"),
                delete_textures: ext!("glDeleteTextures"),
                bind_texture: ext!("glBindTexture"),
                image_target_texture: ext!("glEGLImageTargetTexture2DOES"),
                gl_get_error: ext!("glGetError"),
                tex_storage_2d: ext!("glTexStorage2D"),
                gen_framebuffers: ext!("glGenFramebuffers"),
                delete_framebuffers: ext!("glDeleteFramebuffers"),
                bind_framebuffer: ext!("glBindFramebuffer"),
                framebuffer_texture_2d: ext!("glFramebufferTexture2D"),
                check_framebuffer_status: ext!("glCheckFramebufferStatus"),
                blit_framebuffer: ext!("glBlitFramebuffer"),
            };

            let mut devices = [ptr::null_mut(); 16];
            let mut n = 0;
            if (api.query_devices)(16, devices.as_mut_ptr(), &mut n) == 0 {
                bail!("eglQueryDevicesEXT failed");
            }
            let describe = |dev| {
                let s = (api.query_device_string)(dev, EGL_DRM_DEVICE_FILE_EXT);
                let r = (api.query_device_string)(dev, EGL_DRM_RENDER_NODE_FILE_EXT);
                let s = (!s.is_null()).then(|| CStr::from_ptr(s).to_string_lossy().into_owned());
                let r = (!r.is_null()).then(|| CStr::from_ptr(r).to_string_lossy().into_owned());
                s.map(|s| format!("{s} {}", r.unwrap_or_default()))
            };
            let (device, name) = devices[..n as usize]
                .iter()
                .filter_map(|&d| describe(d).map(|name| (d, name)))
                .find(|(_, name)| node_hint.is_none_or(|h| name.contains(h)))
                .context("no DRM-backed EGL device found")?;

            let display = (api.get_display)(EGL_PLATFORM_DEVICE_EXT, device, ptr::null());
            if display.is_null() {
                bail!("eglGetPlatformDisplayEXT failed for {name}");
            }
            let (mut major, mut minor) = (0, 0);
            if (api.initialize)(display, &mut major, &mut minor) == 0 {
                bail!("eglInitialize failed: {:#x}", (api.get_error)());
            }
            let exts = CStr::from_ptr((api.query_string)(display, EGL_EXTENSIONS)).to_string_lossy();
            if !exts.contains("EGL_EXT_image_dma_buf_import_modifiers") {
                bail!("EGL device {name} lacks EGL_EXT_image_dma_buf_import_modifiers");
            }
            log::debug!("EGL {major}.{minor} on {name}");
            Ok(Self {
                api,
                display,
                context: ptr::null_mut(),
                fbos: [0; 2],
                device: name,
                _lib: lib,
            })
        }
    }

    /// Modifiers this device can import for `fourcc` (excluding external-only ones,
    /// which CUDA can't read as a 2D image).
    pub fn modifiers(&self, fourcc: u32) -> anyhow::Result<Vec<u64>> {
        unsafe {
            let mut n = 0;
            let q = self.api.query_modifiers;
            if q(self.display, fourcc as EGLint, 0, ptr::null_mut(), ptr::null_mut(), &mut n) == 0 {
                bail!("eglQueryDmaBufModifiersEXT failed");
            }
            let mut mods = vec![0u64; n as usize];
            let mut external = vec![0u32; n as usize];
            q(self.display, fourcc as EGLint, n, mods.as_mut_ptr(), external.as_mut_ptr(), &mut n);
            Ok(mods
                .into_iter()
                .zip(external)
                .filter(|(_, ext)| *ext == 0)
                .map(|(m, _)| m)
                .collect())
        }
    }

    pub fn import(&self, plane: &DmaBufPlane) -> anyhow::Result<EGLImage> {
        let mut attrs = vec![
            EGL_WIDTH,
            plane.width as EGLint,
            EGL_HEIGHT,
            plane.height as EGLint,
            EGL_LINUX_DRM_FOURCC_EXT,
            plane.fourcc as EGLint,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset as EGLint,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.stride as EGLint,
        ];
        if plane.modifier != DRM_FORMAT_MOD_INVALID {
            attrs.extend([
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (plane.modifier & 0xffff_ffff) as u32 as EGLint,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (plane.modifier >> 32) as u32 as EGLint,
            ]);
        }
        attrs.push(EGL_NONE);
        let image = unsafe {
            (self.api.create_image)(
                self.display,
                ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attrs.as_ptr(),
            )
        };
        if image.is_null() {
            bail!("eglCreateImageKHR(dma-buf) failed: {:#x}", unsafe { (self.api.get_error)() });
        }
        Ok(image)
    }

    /// Creates a surfaceless desktop OpenGL context (CUDA GL interop needs desktop GL) and makes it current on this thread. GL
    /// textures (and their CUDA registrations) must then be used from this thread only.
    pub fn make_gl_current(&mut self) -> anyhow::Result<()> {
        unsafe {
            if self.context.is_null() {
                if (self.api.bind_api)(EGL_OPENGL_API) == 0 {
                    bail!("eglBindAPI(OpenGL) failed: {:#x}", (self.api.get_error)());
                }
                let attrs = [EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE]; // OpenGL 3.x compatibility
                // EGL_NO_CONFIG_KHR + no surface (KHR_no_config/surfaceless_context).
                let ctx = (self.api.create_context)(
                    self.display,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    attrs.as_ptr(),
                );
                if ctx.is_null() {
                    bail!("eglCreateContext failed: {:#x}", (self.api.get_error)());
                }
                self.context = ctx;
            }
            let none = ptr::null_mut();
            if (self.api.make_current)(self.display, none, none, self.context) == 0 {
                bail!("eglMakeCurrent failed: {:#x}", (self.api.get_error)());
            }
            if self.fbos == [0; 2] {
                (self.api.gen_framebuffers)(2, self.fbos.as_mut_ptr());
            }
        }
        Ok(())
    }

    /// A GL texture backed by `image` (needs [`Self::make_gl_current`]).
    pub fn texture_from_image(&self, image: EGLImage) -> anyhow::Result<u32> {
        unsafe {
            let mut tex = 0;
            (self.api.gen_textures)(1, &mut tex);
            (self.api.bind_texture)(GL_TEXTURE_2D, tex);
            (self.api.image_target_texture)(GL_TEXTURE_2D, image);
            (self.api.bind_texture)(GL_TEXTURE_2D, 0);
            let err = (self.api.gl_get_error)();
            if err != 0 {
                (self.api.delete_textures)(1, &tex);
                bail!("glEGLImageTargetTexture2DOES failed: GL error {err:#x}");
            }
            Ok(tex)
        }
    }

    /// A GL-owned 32-bit texture (registrable with CUDA, unlike DMA-BUF-backed ones).
    pub fn create_texture(&self, width: u32, height: u32) -> anyhow::Result<u32> {
        unsafe {
            let mut tex = 0;
            (self.api.gen_textures)(1, &mut tex);
            (self.api.bind_texture)(GL_TEXTURE_2D, tex);
            (self.api.tex_storage_2d)(GL_TEXTURE_2D, 1, GL_RGBA8, width as i32, height as i32);
            (self.api.bind_texture)(GL_TEXTURE_2D, 0);
            let err = (self.api.gl_get_error)();
            if err != 0 {
                (self.api.delete_textures)(1, &tex);
                bail!("glTexStorage2D failed: GL error {err:#x}");
            }
            Ok(tex)
        }
    }

    /// Copies `src` into `dst` (same size) on the GPU. Converts by channel, so the RGBA8
    /// destination holds bytes R,G,B,A whatever the source's layout.
    pub fn blit_texture(&self, src: u32, dst: u32, width: u32, height: u32) -> anyhow::Result<()> {
        let [read, draw] = self.fbos;
        unsafe {
            let a = &self.api;
            (a.bind_framebuffer)(GL_READ_FRAMEBUFFER, read);
            (a.framebuffer_texture_2d)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, src, 0);
            (a.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, draw);
            (a.framebuffer_texture_2d)(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, dst, 0);
            for target in [GL_READ_FRAMEBUFFER, GL_DRAW_FRAMEBUFFER] {
                let status = (a.check_framebuffer_status)(target);
                if status != GL_FRAMEBUFFER_COMPLETE {
                    bail!("framebuffer {target:#x} incomplete: {status:#x}");
                }
            }
            let (w, h) = (width as i32, height as i32);
            (a.blit_framebuffer)(0, 0, w, h, 0, 0, w, h, GL_COLOR_BUFFER_BIT, GL_NEAREST);
            (a.bind_framebuffer)(GL_READ_FRAMEBUFFER, 0);
            (a.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, 0);
            let err = (a.gl_get_error)();
            if err != 0 {
                bail!("glBlitFramebuffer failed: GL error {err:#x}");
            }
        }
        Ok(())
    }

    pub fn delete_texture(&self, tex: u32) {
        unsafe { (self.api.delete_textures)(1, &tex) };
    }

    pub fn destroy(&self, image: EGLImage) {
        unsafe { (self.api.destroy_image)(self.display, image) };
    }
}

impl Drop for EglDisplay {
    fn drop(&mut self) {
        unsafe {
            if !self.context.is_null() {
                if self.fbos != [0; 2] {
                    (self.api.delete_framebuffers)(2, self.fbos.as_ptr());
                }
                let none = ptr::null_mut();
                (self.api.make_current)(self.display, none, none, none);
                (self.api.destroy_context)(self.display, self.context);
            }
            (self.api.terminate)(self.display);
        }
    }
}

//! The CUDA driver API calls NVENC and the zero-copy path need, loaded at runtime from
//! libcuda.so.1. (CUDA's EGLImage interop is deliberately unused: it rejects DMA-BUF
//! images on desktop GPUs; see gpu.rs.) One context is shared by the capture thread (DMA-BUF import + copy) and
//! the encoder thread; each thread makes it current with [`CudaContext::make_current`].

use std::ptr;

use anyhow::{Context, bail};
use libloading::Library;

use crate::sys::cuda::*;

type CuResult = CUresult;

struct Api {
    init: unsafe extern "C" fn(u32) -> CuResult,
    device_get: unsafe extern "C" fn(*mut CUdevice, i32) -> CuResult,
    ctx_create: unsafe extern "C" fn(*mut CUcontext, u32, CUdevice) -> CuResult,
    ctx_destroy: unsafe extern "C" fn(CUcontext) -> CuResult,
    ctx_push: unsafe extern "C" fn(CUcontext) -> CuResult,
    ctx_pop: unsafe extern "C" fn(*mut CUcontext) -> CuResult,
    mem_alloc_pitch: unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, u32) -> CuResult,
    mem_free: unsafe extern "C" fn(CUdeviceptr) -> CuResult,
    memcpy_2d: unsafe extern "C" fn(*const CUDA_MEMCPY2D) -> CuResult,
    unregister: unsafe extern "C" fn(CUgraphicsResource) -> CuResult,
    gl_register: unsafe extern "C" fn(*mut CUgraphicsResource, u32, u32, u32) -> CuResult,
    map: unsafe extern "C" fn(u32, *mut CUgraphicsResource, CUstream) -> CuResult,
    unmap: unsafe extern "C" fn(u32, *mut CUgraphicsResource, CUstream) -> CuResult,
    mapped_array: unsafe extern "C" fn(*mut CUarray, CUgraphicsResource, u32, u32) -> CuResult,
}

pub struct CudaContext {
    api: Api,
    ctx: CUcontext,
    _lib: Library,
}

// The context handle is valid on any thread once made current there.
unsafe impl Send for CudaContext {}
unsafe impl Sync for CudaContext {}

/// A pitched device allocation.
#[derive(Debug, Clone, Copy)]
pub struct DeviceBuffer {
    pub ptr: CUdeviceptr,
    pub pitch: usize,
}

impl CudaContext {
    /// Creates a context on CUDA device `ordinal`. It is not left current anywhere.
    pub fn new(ordinal: i32) -> anyhow::Result<Self> {
        unsafe {
            let lib = Library::new("libcuda.so.1").context("loading libcuda.so.1")?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())
                        .with_context(|| format!("libcuda: missing {}", $name))?
                };
            }
            let api = Api {
                init: sym!("cuInit"),
                device_get: sym!("cuDeviceGet"),
                ctx_create: sym!("cuCtxCreate_v2"),
                ctx_destroy: sym!("cuCtxDestroy_v2"),
                ctx_push: sym!("cuCtxPushCurrent_v2"),
                ctx_pop: sym!("cuCtxPopCurrent_v2"),
                mem_alloc_pitch: sym!("cuMemAllocPitch_v2"),
                mem_free: sym!("cuMemFree_v2"),
                memcpy_2d: sym!("cuMemcpy2D_v2"),
                unregister: sym!("cuGraphicsUnregisterResource"),
                gl_register: sym!("cuGraphicsGLRegisterImage"),
                map: sym!("cuGraphicsMapResources"),
                unmap: sym!("cuGraphicsUnmapResources"),
                mapped_array: sym!("cuGraphicsSubResourceGetMappedArray"),
            };

            check((api.init)(0), "cuInit")?;
            let mut device = 0;
            check((api.device_get)(&mut device, ordinal), "cuDeviceGet")?;
            let mut ctx = ptr::null_mut();
            check((api.ctx_create)(&mut ctx, 0, device), "cuCtxCreate")?;
            let mut popped = ptr::null_mut();
            check((api.ctx_pop)(&mut popped), "cuCtxPopCurrent")?;

            Ok(Self { api, ctx, _lib: lib })
        }
    }

    pub fn raw(&self) -> CUcontext {
        self.ctx
    }

    /// Makes the context current on this thread until the guard is dropped.
    pub fn make_current(&self) -> anyhow::Result<CurrentGuard<'_>> {
        unsafe { check((self.api.ctx_push)(self.ctx), "cuCtxPushCurrent")? };
        Ok(CurrentGuard(self))
    }

    pub fn alloc_pitch(&self, width_bytes: usize, height: usize) -> anyhow::Result<DeviceBuffer> {
        let (mut ptr, mut pitch) = (0, 0);
        // Element size 16 gives the most permissive alignment for 2D copies.
        unsafe {
            check(
                (self.api.mem_alloc_pitch)(&mut ptr, &mut pitch, width_bytes, height, 16),
                "cuMemAllocPitch",
            )?
        };
        Ok(DeviceBuffer { ptr, pitch })
    }

    pub fn free(&self, buf: DeviceBuffer) {
        unsafe { (self.api.mem_free)(buf.ptr) };
    }

    pub fn copy_2d(&self, params: &CUDA_MEMCPY2D) -> anyhow::Result<()> {
        unsafe { check((self.api.memcpy_2d)(params), "cuMemcpy2D") }
    }

    /// Registers a GL texture (current GL context) for read-only CUDA access.
    pub fn register_gl_texture(&self, texture: u32, target: u32) -> anyhow::Result<CUgraphicsResource> {
        let mut res = ptr::null_mut();
        const READ_ONLY: u32 = 1;
        unsafe {
            check(
                (self.api.gl_register)(&mut res, texture, target, READ_ONLY),
                "cuGraphicsGLRegisterImage",
            )?
        };
        Ok(res)
    }

    /// Maps `res` (synchronizing with GL) and returns its level-0 array; pair with
    /// [`Self::unmap`].
    pub fn map_array(&self, res: CUgraphicsResource) -> anyhow::Result<CUarray> {
        let mut res = res;
        let mut array = ptr::null_mut();
        unsafe {
            check((self.api.map)(1, &mut res, ptr::null_mut()), "cuGraphicsMapResources")?;
            if let Err(e) = check(
                (self.api.mapped_array)(&mut array, res, 0, 0),
                "cuGraphicsSubResourceGetMappedArray",
            ) {
                (self.api.unmap)(1, &mut res, ptr::null_mut());
                return Err(e);
            }
        }
        Ok(array)
    }

    pub fn unmap(&self, res: CUgraphicsResource) {
        let mut res = res;
        unsafe { (self.api.unmap)(1, &mut res, ptr::null_mut()) };
    }

    pub fn unregister(&self, res: CUgraphicsResource) {
        unsafe { (self.api.unregister)(res) };
    }
}

impl Drop for CudaContext {
    fn drop(&mut self) {
        unsafe { (self.api.ctx_destroy)(self.ctx) };
    }
}

pub struct CurrentGuard<'a>(&'a CudaContext);

impl Drop for CurrentGuard<'_> {
    fn drop(&mut self) {
        let mut popped = ptr::null_mut();
        unsafe { (self.0.api.ctx_pop)(&mut popped) };
    }
}

fn check(res: CuResult, what: &str) -> anyhow::Result<()> {
    if res != CUresult::CUDA_SUCCESS {
        bail!("{what} failed with CUDA error {}", res.0);
    }
    Ok(())
}

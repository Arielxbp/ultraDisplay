//! Zero-copy capture (no CPU copies): niri's DMA-BUF is imported as an EGLImage-backed GL
//! texture, copied on the GPU into a GL-owned staging texture registered with CUDA, then
//! copied by CUDA into the linear buffer NVENC encodes from.
//!
//! CUDA can't register DMA-BUF-backed images on desktop GPUs (neither through EGLImage nor
//! GL interop), hence the staging texture, as Sunshine does.
//!
//! The importer owns a GL context current on its thread: create and use it on one thread.

use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use anyhow::Context;

use crate::cuda::{CudaContext, DeviceBuffer};
use crate::egl::{
    DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, DmaBufPlane, EGLImage, EglDisplay, GL_TEXTURE_2D,
};
use crate::sys::cuda::*;

struct Imported {
    image: EGLImage,
    texture: u32,
}

/// GL-owned RGBA8 texture CUDA can map, sized like the stream.
struct Staging {
    texture: u32,
    resource: CUgraphicsResource,
    size: (u32, u32),
}

pub struct DmaBufImporter {
    egl: EglDisplay,
    cuda: Arc<CudaContext>,
    /// Keyed by the DMA-BUF's inode: fd numbers get reused, inodes identify the buffer.
    cache: HashMap<(u64, u64, u32, u32), Imported>,
    staging: Option<Staging>,
}

// Only ever used from the capture thread; EGL/CUDA handles are thread-agnostic.
unsafe impl Send for DmaBufImporter {}

impl DmaBufImporter {
    pub fn new(cuda: Arc<CudaContext>) -> anyhow::Result<Self> {
        let mut egl = EglDisplay::open(None)?;
        egl.make_gl_current()?;
        Ok(Self {
            egl,
            cuda,
            cache: HashMap::new(),
            staging: None,
        })
    }

    /// Modifiers to offer niri (importable for both XRGB and ARGB).
    pub fn modifiers(&self) -> anyhow::Result<Vec<u64>> {
        let xrgb = self.egl.modifiers(DRM_FORMAT_XRGB8888)?;
        let argb = self.egl.modifiers(DRM_FORMAT_ARGB8888)?;
        Ok(xrgb.into_iter().filter(|m| argb.contains(m)).collect())
    }

    pub fn device(&self) -> &str {
        &self.egl.device
    }

    /// Copies one frame into `dst` as R,G,B,A bytes (NVENC `ABGR`), `plane.width` x
    /// `plane.height`.
    pub fn copy_to(&mut self, plane: &DmaBufPlane, dst: DeviceBuffer) -> anyhow::Result<()> {
        let cuda = self.cuda.clone();
        let _current = cuda.make_current()?;
        let key = (
            inode(plane.fd)?,
            plane.modifier,
            plane.width,
            plane.height,
        );
        if !self.cache.contains_key(&key) {
            if self.cache.len() >= 16 {
                self.clear(); // buffers were reallocated; drop stale imports
            }
            let imported = self.import(plane)?;
            self.cache.insert(key, imported);
        }
        let size = (plane.width, plane.height);
        if self.staging.as_ref().is_none_or(|st| st.size != size) {
            self.drop_staging();
            let texture = self.egl.create_texture(plane.width, plane.height)?;
            let resource = match self.cuda.register_gl_texture(texture, GL_TEXTURE_2D) {
                Ok(r) => r,
                Err(e) => {
                    self.egl.delete_texture(texture);
                    return Err(e);
                }
            };
            self.staging = Some(Staging {
                texture,
                resource,
                size,
            });
        }
        let staging = self.staging.as_ref().unwrap();
        let src = self.cache[&key].texture;
        self.egl.blit_texture(src, staging.texture, plane.width, plane.height)?;

        // Mapping waits for the GL copy to finish.
        let resource = staging.resource;
        let array = self.cuda.map_array(resource)?;
        let mut copy: CUDA_MEMCPY2D = unsafe { std::mem::zeroed() };
        copy.srcMemoryType = CUmemorytype::CU_MEMORYTYPE_ARRAY;
        copy.srcArray = array;
        copy.dstMemoryType = CUmemorytype::CU_MEMORYTYPE_DEVICE;
        copy.dstDevice = dst.ptr;
        copy.dstPitch = dst.pitch;
        copy.WidthInBytes = plane.width as usize * 4;
        copy.Height = plane.height as usize;
        let result = self.cuda.copy_2d(&copy);
        self.cuda.unmap(resource);
        result
    }

    fn import(&self, plane: &DmaBufPlane) -> anyhow::Result<Imported> {
        let image = self.egl.import(plane)?;
        let texture = match self.egl.texture_from_image(image) {
            Ok(t) => t,
            Err(e) => {
                self.egl.destroy(image);
                return Err(e);
            }
        };
        log::debug!(
            "imported dma-buf {}x{} modifier {:#x} (GL texture {texture})",
            plane.width,
            plane.height,
            plane.modifier
        );
        Ok(Imported { image, texture })
    }

    fn drop_staging(&mut self) {
        if let Some(st) = self.staging.take() {
            self.cuda.unregister(st.resource);
            self.egl.delete_texture(st.texture);
        }
    }

    pub fn clear(&mut self) {
        for (_, imp) in self.cache.drain() {
            self.egl.delete_texture(imp.texture);
            self.egl.destroy(imp.image);
        }
    }
}

impl Drop for DmaBufImporter {
    fn drop(&mut self) {
        let cuda = self.cuda.clone();
        if let Ok(_current) = cuda.make_current() {
            self.clear();
            self.drop_staging();
        }
    }
}

fn inode(fd: i32) -> anyhow::Result<u64> {
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let meta = std::fs::File::from(fd.try_clone_to_owned().context("dup dma-buf fd")?)
        .metadata()
        .context("fstat dma-buf")?;
    Ok(std::os::unix::fs::MetadataExt::ino(&meta))
}

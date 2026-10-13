//! Single-slot, latest-wins handoff from the capture thread to the encoder thread.
//!
//! If the encoder (or the socket behind it) falls behind, older captured frames are
//! replaced rather than queued, so latency never builds up. Frames are either CPU copies
//! (SHM capture; buffers recycled to avoid a 20 MB allocation per frame) or indices into a
//! small pool of CUDA buffers (zero-copy capture).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use ud_encode::{CudaContext, DeviceBuffer};

/// CUDA frame buffers shared by the capture thread (writes) and encoder (reads).
pub struct GpuPool {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub buffers: Vec<DeviceBuffer>,
    cuda: Arc<CudaContext>,
}

impl GpuPool {
    /// Three buffers: one being encoded, one pending, one being written.
    pub fn new(cuda: Arc<CudaContext>, width: u32, height: u32) -> anyhow::Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let buffers = {
            let _current = cuda.make_current()?;
            (0..3)
                .map(|_| cuda.alloc_pitch(width as usize * 4, height as usize))
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            buffers,
            cuda,
        })
    }
}

impl Drop for GpuPool {
    fn drop(&mut self) {
        if let Ok(_current) = self.cuda.make_current() {
            for buf in self.buffers.drain(..) {
                self.cuda.free(buf);
            }
        }
    }
}

pub enum Payload {
    /// BGRx/BGRA bytes, `stride` per row.
    Cpu(Vec<u8>),
    /// R,G,B,A in `pool.buffers[index]`.
    Gpu { pool: Arc<GpuPool>, index: usize },
}

pub struct OwnedFrame {
    pub payload: Payload,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub received: Instant,
}

#[derive(Default)]
struct State {
    pending: Option<OwnedFrame>,
    spare: Vec<Vec<u8>>,
    /// GPU buffer the encoder is reading: (pool id, index).
    encoding: Option<(u64, usize)>,
    closed: bool,
    replaced: u64,
}

#[derive(Default)]
pub struct FrameSlot {
    state: Mutex<State>,
    ready: Condvar,
}

impl FrameSlot {
    /// Copies SHM `data` into the slot, replacing any frame the encoder hasn't taken yet.
    pub fn put_cpu(&self, data: &[u8], width: u32, height: u32, stride: u32, received: Instant) {
        let mut buf = self.state.lock().unwrap().spare.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(data);
        self.put(OwnedFrame {
            payload: Payload::Cpu(buf),
            width,
            height,
            stride,
            received,
        });
    }

    /// A pool buffer the capture thread may overwrite now (not pending, not encoding).
    pub fn free_gpu_index(&self, pool: &GpuPool) -> usize {
        let st = self.state.lock().unwrap();
        let busy = |i: usize| {
            st.encoding == Some((pool.id, i))
                || matches!(&st.pending, Some(OwnedFrame { payload: Payload::Gpu { pool: p, index }, .. })
                    if p.id == pool.id && *index == i)
        };
        (0..pool.buffers.len()).find(|&i| !busy(i)).expect("3 buffers, at most 2 busy")
    }

    /// Publishes `pool.buffers[index]`, filled by the capture thread.
    pub fn put_gpu(&self, pool: Arc<GpuPool>, index: usize, received: Instant) {
        let (width, height) = (pool.width, pool.height);
        let stride = pool.buffers[index].pitch as u32;
        self.put(OwnedFrame {
            payload: Payload::Gpu { pool, index },
            width,
            height,
            stride,
            received,
        });
    }

    fn put(&self, frame: OwnedFrame) {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return;
        }
        if let Some(old) = st.pending.replace(frame) {
            st.replaced += 1;
            if let Payload::Cpu(data) = old.payload {
                st.spare.push(data);
            }
        }
        self.ready.notify_one();
    }

    /// Waits for the next frame; `None` once closed. A GPU frame stays reserved until
    /// [`Self::done`].
    pub fn take(&self) -> Option<OwnedFrame> {
        let mut st = self.state.lock().unwrap();
        loop {
            if st.closed {
                return None;
            }
            if let Some(f) = st.pending.take() {
                if let Payload::Gpu { pool, index } = &f.payload {
                    st.encoding = Some((pool.id, *index));
                }
                return Some(f);
            }
            st = self.ready.wait(st).unwrap();
        }
    }

    /// Whether a newer frame is waiting.
    pub fn has_pending(&self) -> bool {
        self.state.lock().unwrap().pending.is_some()
    }

    /// Takes the pending frame without waiting (reserving its GPU buffer like [`Self::take`]).
    pub fn try_take(&self) -> Option<OwnedFrame> {
        let mut st = self.state.lock().unwrap();
        let f = st.pending.take()?;
        if let Payload::Gpu { pool, index } = &f.payload {
            st.encoding = Some((pool.id, *index));
        }
        Some(f)
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Releases a taken frame: recycles CPU buffers, frees the GPU buffer for reuse.
    pub fn done(&self, payload: Payload) {
        let mut st = self.state.lock().unwrap();
        match payload {
            Payload::Cpu(data) if st.spare.len() < 2 => st.spare.push(data),
            Payload::Cpu(_) => {}
            Payload::Gpu { .. } => st.encoding = None,
        }
    }

    /// Number of captured frames skipped because the encoder was busy.
    pub fn take_replaced(&self) -> u64 {
        std::mem::take(&mut self.state.lock().unwrap().replaced)
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }
}

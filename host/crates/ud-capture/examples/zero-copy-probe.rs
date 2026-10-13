//! Spike: cast a niri output over DMA-BUF, copy each frame GPU-to-GPU into a CUDA buffer,
//! encode it with NVENC from that buffer, and write the HEVC stream to a file.
//!
//! Usage: zero-copy-probe [OUTPUT] [SECONDS] [OUT.h265]

use std::cell::RefCell;
use std::io::Write;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ud_capture::{CursorMode, FrameData, MonitorCast, StreamOptions, stream};
use ud_encode::egl::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, DmaBufPlane};
use ud_encode::gpu::DmaBufImporter;
use ud_encode::{Codec, CudaContext, EncoderConfig, NvEncoder};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,ud_encode=debug")).init();
    let mut args = std::env::args().skip(1);
    let output = args.next().unwrap_or_else(|| "UD-1".to_owned());
    let seconds: u64 = args.next().map_or(Ok(5), |s| s.parse())?;
    let mut out = args.next().map(std::fs::File::create).transpose()?;

    let cuda = Arc::new(CudaContext::new(0)?);
    let mut importer = DmaBufImporter::new(cuda.clone())?;
    let modifiers = importer.modifiers()?;
    println!("EGL {}: offering {} modifiers", importer.device(), modifiers.len());

    let cast = MonitorCast::start(&output, CursorMode::Embedded)?;
    let opts = StreamOptions { width: 2800, height: 1752, max_fps: 120, dmabuf_modifiers: modifiers };

    let state: Rc<RefCell<Option<(NvEncoder, usize)>>> = Rc::default();
    let st_buf: Rc<RefCell<Option<ud_encode::DeviceBuffer>>> = Rc::default();
    let start = Instant::now();
    let (mut dma, mut shm, mut copy_t, mut enc_t, mut bytes) = (0u32, 0u32, Duration::ZERO, Duration::ZERO, 0usize);
    let (_stop, stop_signal) = ud_capture::stop_channel();

    stream::run(cast.node_id(), &opts, stop_signal, move |frame| {
        let FrameData::DmaBuf { fd, offset, modifier } = frame.data else {
            shm += 1;
            return ControlFlow::Continue(());
        };
        dma += 1;
        let mut st = state.borrow_mut();
        if st.is_none() {
            let cfg = EncoderConfig { codec: Codec::Hevc, width: frame.width, height: frame.height, fps: 120, bitrate_bps: 80_000_000 };
            let mut enc = NvEncoder::with_context(cfg, cuda.clone()).expect("encoder");
            let buf = {
                let _c = cuda.make_current().unwrap();
                cuda.alloc_pitch(frame.width as usize * 4, frame.height as usize).expect("alloc")
            };
            let idx = enc.register_cuda_buffer(buf, ud_encode::InputFormat::Rgba).expect("register");
            *st = Some((enc, idx));
            st_buf.replace(Some(buf));
        }
        let buf = st_buf.borrow().unwrap();
        let plane = DmaBufPlane {
            fd, offset, stride: frame.stride, modifier,
            width: frame.width, height: frame.height,
            fourcc: if frame.format == ud_capture::PixelFormat::Bgra { DRM_FORMAT_ARGB8888 } else { DRM_FORMAT_XRGB8888 },
        };
        let t = Instant::now();
        if let Err(e) = importer.copy_to(&plane, buf) {
            eprintln!("import/copy failed: {e:#}");
            return ControlFlow::Break(());
        }
        copy_t += t.elapsed();
        let (enc, idx) = st.as_mut().unwrap();
        let t = Instant::now();
        enc.encode_cuda(*idx, dma as u64, |au| {
            bytes += au.data.len();
            if let Some(f) = out.as_mut() { f.write_all(au.data).unwrap(); }
        }).expect("encode");
        enc_t += t.elapsed();

        if start.elapsed() >= Duration::from_secs(seconds) {
            let secs = start.elapsed().as_secs_f64();
            println!(
                "{dma} dma-buf frames ({:.1} fps), {shm} shm frames; GPU copy {:.3} ms avg, encode {:.2} ms avg, {:.1} Mbit/s",
                dma as f64 / secs,
                copy_t.as_secs_f64() * 1e3 / dma.max(1) as f64,
                enc_t.as_secs_f64() * 1e3 / dma.max(1) as f64,
                bytes as f64 * 8.0 / secs / 1e6
            );
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })?;
    drop(cast);
    Ok(())
}

//! One connected tablet: virtual output -> screencast -> NVENC -> socket.

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use ud_capture::{CursorMode, FrameData, MonitorCast, PixelFormat, StreamOptions};
use ud_encode::egl::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, DmaBufPlane};
use ud_encode::gpu::DmaBufImporter;
use ud_encode::{Codec, CudaContext, EncoderConfig, InputFormat, NvEncoder};
use ud_niri::Niri;
use ud_proto::{ClientStats, Hello, MsgType, Pairing, StreamConfig};

use crate::bitrate;
use crate::config::{Config, Placement};
use crate::frame_slot::{FrameSlot, GpuPool, Payload};
use crate::host::{Event, LiveSession, Shared, pick_fps};

/// Per-session snapshot of the settings that can't change mid-stream.
#[derive(Debug, Clone)]
struct Settings {
    output_name: String,
    scale: f64,
    placement: Placement,
    position: Option<[i32; 2]>,
    max_fps: u32,
    niri_socket: Option<PathBuf>,
    zero_copy: bool,
    lan_port: Option<u16>,
}

impl Settings {
    fn new(cfg: &Config, niri_socket: Option<PathBuf>) -> Self {
        Self {
            output_name: cfg.output.clone(),
            scale: cfg.scale,
            placement: cfg.effective_placement(),
            position: cfg.position,
            max_fps: cfg.max_fps,
            niri_socket,
            zero_copy: cfg.zero_copy,
            lan_port: cfg.lan.then_some(cfg.lan_port),
        }
    }
}

/// Bitrate cap for `transport`, read live so GUI changes apply mid-stream.
fn bitrate_cap(shared: &Shared, transport: Transport) -> u32 {
    let cfg = shared.config.read().unwrap();
    match transport {
        Transport::Usb => cfg.bitrate_mbps,
        Transport::Lan => cfg.lan_bitrate_mbps,
    }
    .saturating_mul(1_000_000)
}

/// Set when a DMA-BUF import failed at runtime; later sessions use SHM capture.
static ZERO_COPY_BROKEN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Loopback via `adb reverse`: trusted, plain TCP, hands out pairing data.
    Usb,
    /// Wi-Fi: TLS, requires a token from a previous USB pairing.
    Lan,
}

/// An accepted connection, already TLS-wrapped for [`Transport::Lan`].
pub struct Conn {
    pub transport: Transport,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Raw socket, used to unblock both halves on teardown.
    pub socket: TcpStream,
}

type Writer = Arc<Mutex<BufWriter<Box<dyn Write + Send>>>>;
/// The tablet's latest STATS, for the encoder's stall diagnostics.
type LastClient = Arc<Mutex<Option<ClientStats>>>;

/// Frames taking longer than this from capture to the socket are logged as a stall.
const STALL: Duration = Duration::from_millis(300);

/// Runs one tablet session to completion, reporting it through `shared`'s events.
pub fn run(conn: Conn, shared: Arc<Shared>) {
    if let Ok(sock) = conn.socket.try_clone() {
        *shared.session.lock().unwrap() = Some((sock, conn.transport));
    }
    let mut device = None;
    let result = run_inner(conn, &shared, &mut device);
    *shared.session.lock().unwrap() = None;

    let error = result.err().map(|e| format!("{e:#}"));
    if let Some(err) = &error {
        log::error!("session ended with error: {err}");
    }
    // Only sessions that got past HELLO count as a tablet having been connected.
    if let Some(device) = device {
        shared.emit(Event::Disconnected { device, error });
    } else if let Some(err) = error {
        shared.emit(Event::Rejected { reason: err });
    }
}

fn run_inner(conn: Conn, shared: &Arc<Shared>, device: &mut Option<String>) -> anyhow::Result<()> {
    let Conn {
        transport,
        reader,
        writer,
        socket,
    } = conn;
    socket.set_nodelay(true)?;
    crate::net::tune_for_latency(&socket);
    if transport == Transport::Lan && crate::net::slow_start_after_idle() {
        log::warn!(
            "net.ipv4.tcp_slow_start_after_idle=1: Wi-Fi gets laggy after every pause. \
             Fix: sudo sysctl -w net.ipv4.tcp_slow_start_after_idle=0 (see README)"
        );
    }
    let mut reader = BufReader::new(reader);
    let writer: Writer = Arc::new(Mutex::new(BufWriter::with_capacity(1 << 20, writer)));
    let settings = Settings::new(&shared.config.read().unwrap(), shared.niri_socket.clone());

    let msg = ud_proto::read_message(&mut reader).context("waiting for HELLO")?;
    if msg.ty != MsgType::Hello {
        bail!("expected HELLO, got {:?}", msg.ty);
    }
    let hello: Hello = msg.json()?;
    log::info!(
        "client {} over {transport:?}: {}x{} @ {:?} Hz, decoders {:?}",
        hello.device,
        hello.width,
        hello.height,
        hello.refresh_rates,
        hello.codecs
    );
    authorize(transport, &hello, shared, settings.lan_port, &writer)?;
    *device = Some(hello.device.clone());
    shared.emit(Event::Connected {
        device: hello.device.clone(),
        transport,
    });

    let codec = pick_codec(&hello.codecs)?;
    let refresh_rates: Vec<u32> = hello.refresh_rates.iter().map(|r| r.round() as u32).collect();
    let fps = pick_fps(&refresh_rates, settings.max_fps);

    let niri = Niri::new(settings.niri_socket.clone())?;
    let output = VirtualOutput::ensure(niri, &settings, shared.clone(), hello.width, hello.height, fps)?;
    let cast = MonitorCast::start(&settings.output_name, CursorMode::Embedded)?;
    let live = Arc::new(LiveSession {
        output: settings.output_name.clone(),
        width: hello.width,
        height: hello.height,
        refresh_rates,
        fps: std::sync::atomic::AtomicU32::new(fps),
    });
    *shared.live.lock().unwrap() = Some(live.clone());
    let _clear_live = ClearOnDrop(shared.clone());

    let slot = Arc::new(FrameSlot::default());
    let force_idr = Arc::new(AtomicBool::new(false));
    let zero_copy_active = Arc::new(AtomicBool::new(false));
    let (stop_capture, stop_signal) = ud_capture::stop_channel();

    let cuda = Arc::new(CudaContext::new(0).context("creating CUDA context")?);
    let zero_copy = settings.zero_copy && !ZERO_COPY_BROKEN.load(Ordering::Relaxed);

    let capture = {
        let slot = slot.clone();
        let cuda = cuda.clone();
        let active = zero_copy_active.clone();
        let (width, height) = (hello.width, hello.height);
        let node = cast.node_id();
        thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                let res = capture_loop(
                    node, width, height, fps, zero_copy, cuda, &slot, stop_signal, &active,
                );
                // Ends the session (encoder sees the closed slot) if capture stops.
                slot.close();
                res
            })?
    };

    let last_client: LastClient = Arc::default();
    let encoder = {
        let slot = slot.clone();
        let last_client = last_client.clone();
        let writer = writer.clone();
        let force_idr = force_idr.clone();
        let socket = socket.try_clone()?;
        let params = EncodeParams { codec, transport };
        let cuda = cuda.clone();
        let shared = shared.clone();
        let active = zero_copy_active.clone();
        let queue_probe = socket.try_clone()?;
        let live = live.clone();
        thread::Builder::new()
            .name("encode".into())
            .spawn(move || {
                let res = encode_loop(
                    &slot, &writer, &force_idr, params, cuda, &shared, &active, &queue_probe, &live,
                    &last_client,
                );
                // Unblock the reader so the session ends on encoder failure too.
                let _ = socket.shutdown(Shutdown::Both);
                res
            })?
    };

    let mut result = read_loop(&mut reader, &writer, &force_idr, shared, &last_client);

    slot.close();
    stop_capture.stop();
    let _ = socket.shutdown(Shutdown::Both);
    // Once the client is gone the encoder's last write fails with a broken pipe; expected.
    let client_gone = result.is_ok();
    match encoder.join() {
        Ok(Err(err)) if client_gone && is_disconnect(&err) => log::debug!("encoder: {err:#}"),
        Ok(Err(err)) if result.is_ok() => result = Err(err.context("encoder")),
        other => log_thread_result("encoder", other),
    }
    match capture.join() {
        Ok(Err(err)) if result.is_ok() => result = Err(err.context("capture")),
        other => log_thread_result("capture", other),
    }
    drop(cast);
    drop(output);
    result
}

/// Clears the shared live-session state when the session ends (any exit path).
struct ClearOnDrop(Arc<Shared>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        *self.0.live.lock().unwrap() = None;
    }
}

/// Runs the PipeWire stream, publishing frames into `slot`: DMA-BUFs are copied on the GPU
/// into a CUDA buffer pool; SHM frames are copied into RAM.
fn capture_loop(
    node: u32,
    width: u32,
    height: u32,
    fps: u32,
    zero_copy: bool,
    cuda: Arc<CudaContext>,
    slot: &Arc<FrameSlot>,
    stop_signal: ud_capture::stream::StopSignal,
    zero_copy_active: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // The importer owns a GL context, so it lives on this thread.
    let importer = if zero_copy {
        match DmaBufImporter::new(cuda.clone()) {
            Ok(imp) => Some(imp),
            Err(err) => {
                log::warn!("zero-copy capture unavailable, using SHM: {err:#}");
                None
            }
        }
    } else {
        None
    };
    let modifiers = match &importer {
        Some(imp) => imp.modifiers().unwrap_or_default(),
        None => Vec::new(),
    };
    let opts = StreamOptions {
        width,
        height,
        max_fps: fps,
        dmabuf_modifiers: modifiers,
    };

    let mut importer = importer;
    let mut pool: Option<Arc<GpuPool>> = None;
    let mut logged_mode = false;
    let slot = slot.clone();
    let zero_copy_active = zero_copy_active.clone();
    ud_capture::stream::run(node, &opts, stop_signal, move |frame| {
        match frame.data {
            FrameData::Shm(data) => {
                if !logged_mode {
                    log::info!("capture: SHM ({}x{}, CPU copies)", frame.width, frame.height);
                    logged_mode = true;
                }
                slot.put_cpu(data, frame.width, frame.height, frame.stride, frame.received);
            }
            FrameData::DmaBuf { fd, offset, modifier } => {
                let Some(imp) = importer.as_mut() else {
                    log::error!("got a DMA-BUF without an importer");
                    return ControlFlow::Break(());
                };
                if !logged_mode {
                    zero_copy_active.store(true, Ordering::Relaxed);
                    log::info!(
                        "capture: zero-copy DMA-BUF ({}x{}, modifier {modifier:#x}) on {}",
                        frame.width,
                        frame.height,
                        imp.device()
                    );
                    logged_mode = true;
                }
                if pool.as_ref().is_none_or(|p| (p.width, p.height) != (frame.width, frame.height)) {
                    match GpuPool::new(cuda.clone(), frame.width, frame.height) {
                        Ok(p) => pool = Some(Arc::new(p)),
                        Err(err) => {
                            log::error!("allocating GPU frame buffers: {err:#}");
                            ZERO_COPY_BROKEN.store(true, Ordering::Relaxed);
                            return ControlFlow::Break(());
                        }
                    }
                }
                let pool = pool.as_ref().unwrap();
                let index = slot.free_gpu_index(pool);
                let plane = DmaBufPlane {
                    fd,
                    offset,
                    stride: frame.stride,
                    modifier,
                    width: frame.width,
                    height: frame.height,
                    fourcc: match frame.format {
                        PixelFormat::Bgra => DRM_FORMAT_ARGB8888,
                        PixelFormat::Bgrx => DRM_FORMAT_XRGB8888,
                    },
                };
                if let Err(err) = imp.copy_to(&plane, pool.buffers[index]) {
                    log::error!("zero-copy import failed, falling back to SHM from the next connection: {err:#}");
                    ZERO_COPY_BROKEN.store(true, Ordering::Relaxed);
                    return ControlFlow::Break(());
                }
                slot.put_gpu(pool.clone(), index, frame.received);
            }
        }
        ControlFlow::Continue(())
    })
}

/// USB: hand out pairing data. Wi-Fi: require a valid token.
fn authorize(
    transport: Transport,
    hello: &Hello,
    shared: &Shared,
    lan_port: Option<u16>,
    writer: &Writer,
) -> anyhow::Result<()> {
    match transport {
        Transport::Usb => {
            let (Some(client_id), Some(lan_port)) = (&hello.client_id, lan_port) else {
                return Ok(());
            };
            let new = !shared.store.is_paired(client_id);
            let token = shared.store.token_for(client_id, &hello.device)?;
            if new {
                shared.emit(Event::Paired {
                    device: hello.device.clone(),
                });
            }
            let msg = Pairing {
                host_id: shared.identity.id().to_owned(),
                host_name: shared.host_name.clone(),
                fingerprint: shared.identity.fingerprint.clone(),
                lan_port,
                token,
            };
            let mut w = writer.lock().unwrap();
            ud_proto::write_json(&mut *w, MsgType::Pairing, &msg)?;
            w.flush()?;
            Ok(())
        }
        Transport::Lan => {
            let ok = match (&hello.client_id, &hello.token) {
                (Some(id), Some(token)) => shared.store.verify(id, token),
                _ => false,
            };
            if !ok {
                let mut w = writer.lock().unwrap();
                let reason = "not paired: connect the tablet over USB once to pair";
                ud_proto::write_parts(&mut *w, MsgType::Bye, 0, &[reason.as_bytes()])?;
                w.flush()?;
                bail!("rejected unpaired Wi-Fi client {}", hello.device);
            }
            Ok(())
        }
    }
}

fn pick_codec(codecs: &[String]) -> anyhow::Result<Codec> {
    if codecs.iter().any(|c| c == "hevc") {
        Ok(Codec::Hevc)
    } else if codecs.iter().any(|c| c == "h264") {
        Ok(Codec::H264)
    } else {
        bail!("client has no supported decoder (offered {codecs:?})")
    }
}

fn read_loop(
    reader: &mut BufReader<Box<dyn Read + Send>>,
    writer: &Writer,
    force_idr: &AtomicBool,
    shared: &Shared,
    last_client: &Mutex<Option<ClientStats>>,
) -> anyhow::Result<()> {
    let mut last_stats_log = Instant::now();
    loop {
        let msg = match ud_proto::read_message(reader) {
            Ok(m) => m,
            Err(ud_proto::Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                log::info!("client disconnected");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        match msg.ty {
            MsgType::Stats => {
                let s: ClientStats = msg.json()?;
                log::debug!(
                    target: "ud_host::diag",
                    "tablet: {:.1} fps, decode {:.1} ms, recv {:.1} Mbit/s, max gap {:.0} ms, ping {:.1} ms, {}",
                    s.fps,
                    s.decode_ms,
                    s.recv_mbps,
                    s.max_gap_ms,
                    s.rtt_ms,
                    wifi_desc(&s)
                );
                *last_client.lock().unwrap() = Some(s.clone());
                shared.emit(Event::ClientStats {
                    fps: s.fps,
                    decode_ms: s.decode_ms,
                    rtt_ms: s.rtt_ms,
                });
                let every = shared.config.read().unwrap().stats_log_secs;
                if every > 0 && last_stats_log.elapsed() >= Duration::from_secs(every.into()) {
                    log::info!(
                        "client: {:.1} fps decoded, decode {:.1} ms, rtt {:.1} ms, {} dropped",
                        s.fps,
                        s.decode_ms,
                        s.rtt_ms,
                        s.dropped
                    );
                    last_stats_log = Instant::now();
                }
            }
            MsgType::RequestIdr => force_idr.store(true, Ordering::Relaxed),
            MsgType::Ping => {
                let mut w = writer.lock().unwrap();
                ud_proto::write_parts(&mut *w, MsgType::Pong, 0, &[&msg.payload])?;
                w.flush()?;
            }
            MsgType::Bye => {
                log::info!("client said bye: {}", String::from_utf8_lossy(&msg.payload));
                return Ok(());
            }
            other => log::debug!("ignoring {other:?} from client"),
        }
    }
}

#[derive(Clone, Copy)]
struct EncodeParams {
    codec: Codec,
    transport: Transport,
}

fn encode_loop(
    slot: &FrameSlot,
    writer: &Writer,
    force_idr: &AtomicBool,
    params: EncodeParams,
    cuda: Arc<CudaContext>,
    shared: &Shared,
    zero_copy_active: &AtomicBool,
    socket: &TcpStream,
    live: &LiveSession,
    last_client: &Mutex<Option<ClientStats>>,
) -> anyhow::Result<()> {
    let epoch = Instant::now();
    let mut diag = Diag::new(socket);
    // GPU pool registered with the current encoder. Declared before `enc` so the encoder
    // (and its NVENC registrations) is dropped first.
    let mut registered_pool: Option<Arc<GpuPool>> = None;
    let mut enc: Option<NvEncoder> = None;
    let mut au = Vec::with_capacity(1 << 20);
    let mut rate = bitrate::Controller::new(bitrate_cap(shared, params.transport));
    let mut announced = false;

    // 1 s window for the bitrate controller, 5 s window for the log line.
    let mut rate_window = (Instant::now(), 0u32, Duration::ZERO);
    let mut log_window = Instant::now();
    let (mut frames, mut bytes, mut encode_time, mut skipped) =
        (0u32, 0usize, Duration::ZERO, 0u64);
    let (mut window_bytes, mut window_encode) = (0usize, Duration::ZERO);

    let mut queue_skips = 0u64;
    let mut worst_queue_ms = 0f32;
    while let Some(mut frame) = slot.take() {
        // Keep latency bounded: while more than ~40 ms of video is still queued in the
        // network stack, wait (swapping in newer frames) instead of piling on more.
        // Waiting rather than dropping guarantees the latest frame is always sent.
        if enc.is_some() {
            let budget = (rate.current() as u64 / 8 * 40 / 1000).max(64 * 1024);
            let waited = Instant::now();
            loop {
                let queued = crate::net::not_sent_bytes(socket) as u64;
                worst_queue_ms =
                    worst_queue_ms.max(queued as f32 * 8.0 / rate.current() as f32 * 1000.0);
                if queued <= budget || slot.is_closed() {
                    break;
                }
                if slot.has_pending() {
                    slot.done(frame.payload);
                    match slot.try_take() {
                        Some(newer) => frame = newer,
                        None => return Ok(()),
                    }
                    queue_skips += 1;
                }
                thread::sleep(Duration::from_millis(1));
            }
            // Time spent waiting for the link is congestion, like a blocked write.
            rate_window.2 += waited.elapsed();
        }

        // (Re)create the encoder from the size niri actually casts, and when GPU frames
        // come from a new buffer pool (its buffers must be registered with NVENC).
        let size = (frame.width, frame.height);
        let frame_pool = match &frame.payload {
            Payload::Gpu { pool, .. } => Some(pool),
            Payload::Cpu(_) => None,
        };
        let size_changed = enc.as_ref().map(|e| (e.config().width, e.config().height)) != Some(size);
        let pool_changed = frame_pool
            .is_some_and(|p| registered_pool.as_ref().is_none_or(|r| r.id != p.id));
        if size_changed || pool_changed {
            drop(enc.take()); // release the old session before opening a new one
            registered_pool = None;
            let config = EncoderConfig {
                codec: params.codec,
                width: frame.width,
                height: frame.height,
                fps: live.fps.load(Ordering::Relaxed),
                bitrate_bps: rate.current(),
            };
            let mut new = NvEncoder::with_context(config, cuda.clone())?;
            if let Some(pool) = frame_pool {
                for buf in &pool.buffers {
                    new.register_cuda_buffer(*buf, InputFormat::Rgba)?;
                }
                registered_pool = Some(pool.clone());
            }
            enc = Some(new);
            announced = false;
            // Frames skipped while the encoder initialized aren't link congestion.
            slot.take_replaced();
            rate_window = (Instant::now(), 0, Duration::ZERO);
            let mut w = writer.lock().unwrap();
            ud_proto::write_json(
                &mut *w,
                MsgType::Config,
                &StreamConfig {
                    codec: match params.codec {
                        Codec::Hevc => "hevc",
                        Codec::H264 => "h264",
                    }
                    .into(),
                    width: frame.width,
                    height: frame.height,
                    fps: live.fps.load(Ordering::Relaxed),
                    bitrate: rate.current(),
                },
            )?;
        }
        let enc = enc.as_mut().unwrap();
        // Max fps changed from the GUI: niri already switched the output's mode.
        let fps = live.fps.load(Ordering::Relaxed);
        if fps != enc.config().fps {
            enc.set_fps(fps)?;
        }
        if !announced {
            announced = true;
            shared.emit(Event::Streaming {
                width: frame.width,
                height: frame.height,
                fps: live.fps.load(Ordering::Relaxed),
                zero_copy: matches!(frame.payload, Payload::Gpu { .. })
                    || zero_copy_active.load(Ordering::Relaxed),
            });
        }
        if force_idr.swap(false, Ordering::Relaxed) {
            enc.request_idr();
        }

        let captured = frame.received;
        let pts = captured.duration_since(epoch).as_micros() as u64;
        let t = Instant::now();
        let mut keyframe = false;
        let on_output = |out: ud_encode::EncodedFrame<'_>| {
            au.clear();
            au.extend_from_slice(out.data);
            keyframe = out.keyframe;
        };
        let result = match &frame.payload {
            Payload::Cpu(data) => enc.encode(data, frame.stride, pts, on_output),
            // Registered in pool order, so the pool index is the NVENC input index.
            Payload::Gpu { index, .. } => enc.encode_cuda(*index, pts, on_output),
        };
        slot.done(frame.payload);
        result?;
        encode_time += t.elapsed();
        window_encode += t.elapsed();

        let t = Instant::now();
        {
            let mut w = writer.lock().unwrap();
            ud_proto::write_video(&mut *w, pts, keyframe, &au)?;
            w.flush()?;
        }
        rate_window.1 += 1;
        rate_window.2 += t.elapsed();
        diag.frame(au.len(), keyframe, t.elapsed(), captured.elapsed());

        frames += 1;
        bytes += au.len();
        window_bytes += au.len();

        if rate_window.0.elapsed() >= Duration::from_secs(1) {
            let window_skipped = slot.take_replaced() + std::mem::take(&mut queue_skips);
            skipped += window_skipped;
            let w = bitrate::Window {
                frames: rate_window.1,
                skipped: window_skipped as u32,
                write_time: rate_window.2,
            };
            let before = rate.current();
            let mut changed = rate.update(w);
            // Cap changes from the GUI apply within a second.
            if let Some(bps) = rate.set_max(bitrate_cap(shared, params.transport)) {
                changed = Some(bps);
            }
            if let Some(bps) = changed {
                log::info!("bitrate -> {} Mbit/s", bps / 1_000_000);
                enc.set_bitrate(bps)?;
                shared.emit(Event::BitrateChanged {
                    target_mbps: bps / 1_000_000,
                    lowered: bps < before && bps < rate.max(),
                });
            }
            let secs = rate_window.0.elapsed().as_secs_f32();
            shared.emit(Event::HostStats {
                fps: rate_window.1 as f32 / secs,
                mbps: window_bytes as f32 * 8.0 / secs / 1e6,
                target_mbps: rate.current() / 1_000_000,
                encode_ms: window_encode.as_secs_f32() * 1e3 / rate_window.1.max(1) as f32,
                skipped: window_skipped,
                queue_ms: std::mem::take(&mut worst_queue_ms),
                net_rtt_ms: crate::net::tcp_rtt_cwnd(socket).map_or(0.0, |(rtt, _)| rtt),
            });
            diag.report(socket, rate.current(), window_bytes, secs, last_client);
            (window_bytes, window_encode) = (0, Duration::ZERO);
            rate_window = (Instant::now(), 0, Duration::ZERO);
        }

        let log_every = shared.config.read().unwrap().stats_log_secs;
        if log_every == 0 {
            // Logging off: keep the window short so counters don't accumulate.
            if log_window.elapsed() >= Duration::from_secs(5) {
                log_window = Instant::now();
                (frames, bytes, encode_time, skipped) = (0, 0, Duration::ZERO, 0);
            }
        } else if log_window.elapsed() >= Duration::from_secs(log_every.into()) {
            let secs = log_window.elapsed().as_secs_f64();
            log::info!(
                "host: {:.1} fps sent, {:.1} Mbit/s (target {}), encode {:.2} ms avg, {} captured frames skipped, tcp rtt {:.1} ms",
                frames as f64 / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                rate.current() / 1_000_000,
                encode_time.as_secs_f64() * 1e3 / frames.max(1) as f64,
                skipped,
                crate::net::tcp_rtt_cwnd(socket).map_or(0.0, |(rtt, _)| rtt),
            );
            log_window = Instant::now();
            (frames, bytes, encode_time, skipped) = (0, 0, Duration::ZERO, 0);
        }
    }
    Ok(())
}

/// Per-second network diagnostics (debug level, i.e. the log file), plus a warning when
/// frames got stuck on their way out.
#[derive(Default)]
struct Diag {
    frames: u32,
    keyframes: u32,
    max_frame: usize,
    max_write: Duration,
    /// Capture to fully handed to the socket, worst frame in the window.
    max_latency: Duration,
    total_retrans: u32,
    total_rto: u16,
}

impl Diag {
    fn new(socket: &TcpStream) -> Self {
        let info = crate::net::tcp_info(socket);
        Self {
            total_retrans: info.map_or(0, |i| i.tcpi_total_retrans),
            total_rto: info.map_or(0, |i| i.tcpi_total_rto),
            ..Self::default()
        }
    }

    fn frame(&mut self, bytes: usize, keyframe: bool, write: Duration, latency: Duration) {
        self.frames += 1;
        self.keyframes += u32::from(keyframe);
        self.max_frame = self.max_frame.max(bytes);
        self.max_write = self.max_write.max(write);
        self.max_latency = self.max_latency.max(latency);
    }

    fn report(
        &mut self,
        socket: &TcpStream,
        target_bps: u32,
        bytes: usize,
        secs: f32,
        last_client: &Mutex<Option<ClientStats>>,
    ) {
        let Some(i) = crate::net::tcp_info(socket) else { return };
        let retrans = i.tcpi_total_retrans.wrapping_sub(self.total_retrans);
        let rto = i.tcpi_total_rto.wrapping_sub(self.total_rto);
        (self.total_retrans, self.total_rto) = (i.tcpi_total_retrans, i.tcpi_total_rto);
        let tcp = format!(
            "tcp rtt {:.1}±{:.1} ms, cwnd {}, ssthresh {}, in flight {}, unsent {} KB, \
             retrans +{retrans}, timeouts +{rto} (backoff {}), delivery {:.0} Mbit/s",
            i.tcpi_rtt as f32 / 1000.0,
            i.tcpi_rttvar as f32 / 1000.0,
            i.tcpi_snd_cwnd,
            if i.tcpi_snd_ssthresh >= 0x7fff_ffff { 0 } else { i.tcpi_snd_ssthresh },
            i.tcpi_unacked,
            i.tcpi_notsent_bytes / 1024,
            i.tcpi_backoff,
            i.tcpi_delivery_rate as f64 * 8.0 / 1e6,
        );
        log::debug!(
            target: "ud_host::diag",
            "host: {:.1} fps, {:.1} Mbit/s (target {}), {} key, max frame {} KB, \
             max write {:.0} ms, max capture->sent {:.0} ms | {tcp}",
            self.frames as f32 / secs,
            bytes as f32 * 8.0 / secs / 1e6,
            target_bps / 1_000_000,
            self.keyframes,
            self.max_frame / 1024,
            self.max_write.as_secs_f32() * 1e3,
            self.max_latency.as_secs_f32() * 1e3,
        );
        if self.max_latency > STALL || rto > 0 {
            let wifi = last_client.lock().unwrap().as_ref().map(wifi_desc);
            log::warn!(
                "stall: a frame took {:.0} ms to leave the host (max write {:.0} ms) | {tcp} | tablet {}",
                self.max_latency.as_secs_f32() * 1e3,
                self.max_write.as_secs_f32() * 1e3,
                wifi.as_deref().unwrap_or("no stats yet"),
            );
        }
        *self = Self { total_retrans: self.total_retrans, total_rto: self.total_rto, ..Self::default() };
    }
}

fn wifi_desc(s: &ClientStats) -> String {
    match s.wifi_rssi_dbm {
        Some(rssi) => format!(
            "Wi-Fi {rssi} dBm ({}), {} MHz, link tx {} / rx {} Mbit/s",
            match rssi {
                -55.. => "strong",
                -67..=-56 => "good",
                -75..=-68 => "weak",
                _ => "very weak",
            },
            s.wifi_freq_mhz.unwrap_or(0),
            s.wifi_tx_mbps.unwrap_or(0),
            s.wifi_rx_mbps.unwrap_or(0),
        ),
        None => "no Wi-Fi info (USB or old app)".into(),
    }
}

fn is_disconnect(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
            )
        })
    })
}

fn log_thread_result(name: &str, res: thread::Result<anyhow::Result<()>>) {
    match res {
        Ok(Ok(())) => {}
        Ok(Err(err)) => log::warn!("{name} thread ended with error: {err:#}"),
        Err(_) => log::error!("{name} thread panicked"),
    }
}

/// The tablet's niri output. Removed again on drop (so niri moves windows back) unless
/// `keep_output` is set.
struct VirtualOutput {
    niri: Niri,
    name: String,
    /// `keep_output` is read at disconnect time, so a GUI change applies to this session.
    shared: Arc<Shared>,
}

impl VirtualOutput {
    fn ensure(
        niri: Niri,
        s: &Settings,
        shared: Arc<Shared>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<Self> {
        let name = s.output_name.clone();
        let want = (width as u16, height as u16, fps * 1000);

        let existing = niri.outputs()?.remove(&name);
        let reuse = existing
            .as_ref()
            .and_then(|o| o.mode())
            .is_some_and(|m| (m.width, m.height, m.refresh_rate.div_ceil(1000) * 1000) == want);

        let mut created = false;
        if !reuse {
            if existing.is_some() {
                log::info!("recreating {name} with the tablet's mode");
                niri.remove_virtual_output(&name)?;
            }
            niri.create_virtual_output(&name, want.0, want.1, fps)?;
            created = true;
        }

        // niri re-applies IPC output settings by name, so a mode chosen live in an earlier
        // session (max fps) would otherwise come back: set it explicitly.
        niri.set_mode(&name, want.0, want.1, fps)?;
        niri.set_scale(&name, s.scale)?;
        // Set even for automatic placement: like the mode, a position set over IPC in an
        // earlier session would otherwise be re-applied by name.
        crate::placement::apply(&niri, &name, s.placement, s.position)?;
        log::info!(
            "virtual output {name}: {width}x{height}@{fps} scale {} ({})",
            s.scale,
            if created { "created" } else { "reused" }
        );

        // The output name is ours, so a reused UD-1 (e.g. left over after a crash) is
        // removed too.
        Ok(Self { niri, name, shared })
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        if self.shared.config.read().unwrap().keep_output {
            log::info!("keeping virtual output {} (keep_output)", self.name);
            return;
        }
        match self.niri.remove_virtual_output(&self.name) {
            Ok(()) => log::info!("removed virtual output {}", self.name),
            Err(err) => log::warn!("removing virtual output {}: {err:#}", self.name),
        }
    }
}

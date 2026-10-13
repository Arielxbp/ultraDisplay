//! PipeWire consumer for a niri screencast node.
//!
//! With `dmabuf_modifiers` set, DMA-BUF formats (those modifiers) are offered first, so
//! frames stay on the GPU; niri picks one it can allocate. SHM (MemFd) formats without a
//! modifier are always offered as the fallback, where niri reads each frame back to RAM.

use std::cell::Cell;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::time::Instant;

use anyhow::Context;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{self, Pod};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx,
    Bgra,
}

/// Pixel storage of a captured frame.
pub enum FrameData<'a> {
    /// Mapped shared memory, `stride` bytes per row.
    Shm(&'a [u8]),
    /// A single-plane DMA-BUF on the GPU. The fd is only valid during the callback.
    DmaBuf { fd: i32, offset: u32, modifier: u64 },
}

/// One captured frame, valid only for the duration of the callback.
pub struct Frame<'a> {
    pub data: FrameData<'a>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    /// Time the frame was dequeued, used for latency accounting.
    pub received: Instant,
}

pub struct StreamOptions {
    /// Preferred size; niri casts the output at its own mode regardless.
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    /// DRM modifiers the consumer can import; empty = SHM only.
    pub dmabuf_modifiers: Vec<u64>,
}

/// Stops a running [`run`] loop from any thread.
#[derive(Clone)]
pub struct StopHandle(pw::channel::Sender<()>);

impl StopHandle {
    pub fn stop(&self) {
        // Fails only if the loop already exited.
        let _ = self.0.send(());
    }
}

/// Receiving half of a [`StopHandle`], consumed by [`run`].
pub struct StopSignal(pw::channel::Receiver<()>);

pub fn stop_channel() -> (StopHandle, StopSignal) {
    let (tx, rx) = pw::channel::channel();
    (StopHandle(tx), StopSignal(rx))
}

/// Connects to `node_id` and calls `on_frame` for every non-empty frame until it returns
/// [`ControlFlow::Break`] or `stop` fires. Blocks the calling thread running the PipeWire
/// main loop.
pub fn run(
    node_id: u32,
    opts: &StreamOptions,
    stop: StopSignal,
    mut on_frame: impl FnMut(Frame<'_>) -> ControlFlow<()> + 'static,
) -> anyhow::Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating PipeWire main loop")?;
    let _stop = stop.0.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    let stream = pw::stream::StreamBox::new(&core, "ultradisplay-capture", props)?;

    let info: Rc<Cell<Option<VideoInfoRaw>>> = Rc::default();
    let failed: Rc<Cell<bool>> = Rc::default();

    let _listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let mainloop = mainloop.clone();
            let failed = failed.clone();
            move |_, _, old, new| {
                log::debug!("pw stream state: {old:?} -> {new:?}");
                if let pw::stream::StreamState::Error(err) = new {
                    log::error!("pw stream error: {err}");
                    failed.set(true);
                    mainloop.quit();
                }
            }
        })
        .param_changed({
            let info = info.clone();
            move |stream, _, id, param| {
                let Some(param) = param else { return };
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let mut raw = VideoInfoRaw::new();
                if let Err(err) = raw.parse(param) {
                    log::warn!("unparseable video format: {err:?}");
                    return;
                }
                log::info!(
                    "negotiated {:?} {}x{} max {}/{} fps, modifier {:#x}",
                    raw.format(),
                    raw.size().width,
                    raw.size().height,
                    raw.max_framerate().num,
                    raw.max_framerate().denom,
                    raw.modifier(),
                );
                info.set(Some(raw));

                // Accept the buffer type matching the negotiated format.
                let data_type = if raw.modifier() != 0 || has_modifier(param) {
                    1 << spa::buffer::DataType::DmaBuf.as_raw()
                } else {
                    (1 << spa::buffer::DataType::MemFd.as_raw())
                        | (1 << spa::buffer::DataType::MemPtr.as_raw())
                };
                let buffers = pod::Object {
                    type_: SpaTypes::ObjectParamBuffers.as_raw(),
                    id: ParamType::Buffers.as_raw(),
                    properties: vec![pod::Property {
                        key: spa::sys::SPA_PARAM_BUFFERS_dataType,
                        flags: pod::PropertyFlags::empty(),
                        value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Flags {
                                default: data_type,
                                flags: vec![data_type],
                            },
                        ))),
                    }],
                };
                match serialize(&buffers) {
                    Ok(bytes) => {
                        if let Some(pod) = Pod::from_bytes(&bytes) {
                            let _ = stream.update_params(&mut [pod]);
                        }
                    }
                    Err(err) => log::warn!("serializing buffers param: {err:#}"),
                }
            }
        })
        .process({
            let mainloop = mainloop.clone();
            let info = info.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(raw) = info.get() else { return };
                let received = Instant::now();

                let datas = buffer.datas_mut();
                let Some(d) = datas.first_mut() else { return };
                let chunk = d.chunk();
                let (offset, size, stride) = (chunk.offset(), chunk.size(), chunk.stride());
                // Empty chunks are metadata-only updates (no new pixels).
                if size == 0 {
                    return;
                }
                let format = match raw.format() {
                    VideoFormat::BGRA => PixelFormat::Bgra,
                    _ => PixelFormat::Bgrx,
                };
                let data = if d.type_() == spa::buffer::DataType::DmaBuf {
                    FrameData::DmaBuf {
                        fd: d.as_raw().fd as i32,
                        offset,
                        modifier: raw.modifier(),
                    }
                } else {
                    let Some(bytes) = d.data() else {
                        log::warn!("SHM buffer not mappable");
                        return;
                    };
                    let end = ((offset + size) as usize).min(bytes.len());
                    FrameData::Shm(&bytes[offset as usize..end])
                };
                let frame = Frame {
                    data,
                    width: raw.size().width,
                    height: raw.size().height,
                    stride: stride.unsigned_abs(),
                    format,
                    received,
                };
                if on_frame(frame).is_break() {
                    mainloop.quit();
                }
            }
        })
        .register()?;

    let formats = enum_formats(opts);
    let bytes: Vec<Vec<u8>> = formats.iter().map(serialize).collect::<anyhow::Result<_>>()?;
    let mut params: Vec<&Pod> = bytes
        .iter()
        .map(|b| Pod::from_bytes(b).context("invalid format pod"))
        .collect::<anyhow::Result<_>>()?;

    stream.connect(
        Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    mainloop.run();

    anyhow::ensure!(!failed.get(), "PipeWire stream failed");
    Ok(())
}

fn serialize(obj: &pod::Object) -> anyhow::Result<Vec<u8>> {
    Ok(pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pod::Value::Object(obj.clone()),
    )
    .context("serializing pod")?
    .0
    .into_inner())
}

/// Whether a format pod carries a (possibly not yet fixated) modifier property.
fn has_modifier(param: &Pod) -> bool {
    param
        .as_object()
        .ok()
        .and_then(|o| o.find_prop(spa::utils::Id(FormatProperties::VideoModifier.as_raw())))
        .is_some()
}

/// DMA-BUF formats first (preferred), then SHM fallbacks.
fn enum_formats(opts: &StreamOptions) -> Vec<pod::Object> {
    let mut out = Vec::new();
    if !opts.dmabuf_modifiers.is_empty() {
        for format in [VideoFormat::BGRx, VideoFormat::BGRA] {
            let mods: Vec<i64> = opts.dmabuf_modifiers.iter().map(|m| *m as i64).collect();
            let mut obj = video_format(opts, format);
            obj.properties.push(pod::Property {
                key: FormatProperties::VideoModifier.as_raw(),
                flags: pod::PropertyFlags::MANDATORY | pod::PropertyFlags::DONT_FIXATE,
                value: pod::Value::Choice(pod::ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: mods[0],
                        alternatives: mods,
                    },
                ))),
            });
            out.push(obj);
        }
    }
    out.push(video_format(opts, VideoFormat::BGRx));
    out.push(video_format(opts, VideoFormat::BGRA));
    out
}

fn video_format(opts: &StreamOptions, format: VideoFormat) -> pod::Object {
    pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pod::property!(FormatProperties::VideoFormat, Id, format),
            pod::property!(
                FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                Rectangle {
                    width: opts.width,
                    height: opts.height
                },
                Rectangle {
                    width: 1,
                    height: 1
                },
                Rectangle {
                    width: 8192,
                    height: 8192
                }
            ),
            pod::property!(
                FormatProperties::VideoFramerate,
                Fraction,
                Fraction { num: 0, denom: 1 }
            ),
            pod::Property {
                key: FormatProperties::VideoMaxFramerate.as_raw(),
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction {
                            num: opts.max_fps,
                            denom: 1,
                        },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction {
                            num: 1000,
                            denom: 1,
                        },
                    },
                ))),
            },
        ],
    }
}

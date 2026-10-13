//! Direct NVENC encoder tuned for interactive streaming.
//!
//! Synchronous mode (async encode isn't supported on Linux), one input and one bitstream
//! buffer, system-memory ARGB input that the driver uploads and converts to YUV.
//! Rate control: CBR with a one-frame VBV, no B-frames, infinite GOP, IDR on demand.

use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use libloading::Library;

use crate::cuda::{CudaContext, DeviceBuffer};
use crate::sys::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

/// One encoded access unit in Annex-B format. Borrowed from NVENC until the callback
/// returns.
/// Byte order of 32-bit input pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    /// B,G,R,A/X in memory (DRM XRGB8888/ARGB8888, PipeWire BGRx/BGRA).
    Bgra,
    /// R,G,B,A in memory (GL RGBA8).
    Rgba,
}

impl InputFormat {
    fn nvenc(self) -> NV_ENC_BUFFER_FORMAT {
        match self {
            Self::Bgra => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            Self::Rgba => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR,
        }
    }
}

pub struct EncodedFrame<'a> {
    pub data: &'a [u8],
    pub keyframe: bool,
    pub pts: u64,
}

pub struct NvEncoder {
    api: NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    input: NV_ENC_INPUT_PTR,
    output: NV_ENC_OUTPUT_PTR,
    config: EncoderConfig,
    // Kept for nvEncReconfigureEncoder; `init.encodeConfig` points into `cfg`.
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    cfg: Box<NV_ENC_CONFIG>,
    force_idr: bool,
    /// CUDA device buffers registered as encoder input (zero-copy path).
    registered: Vec<(NV_ENC_REGISTERED_PTR, u32, InputFormat)>,
    // Dropped after the encoder session (field order matters).
    cuda: Arc<CudaContext>,
    _lib: Library,
}

// The session is only ever used from one thread at a time.
unsafe impl Send for NvEncoder {}

impl NvEncoder {
    /// Encoder with its own CUDA context (system-memory input).
    pub fn new(config: EncoderConfig) -> anyhow::Result<Self> {
        Self::with_context(config, Arc::new(CudaContext::new(0)?))
    }

    /// Encoder on a shared CUDA context, so CUDA buffers filled elsewhere (e.g. from
    /// imported DMA-BUFs) can be encoded with [`Self::encode_cuda`].
    pub fn with_context(config: EncoderConfig, cuda: Arc<CudaContext>) -> anyhow::Result<Self> {
        let _current = cuda.make_current()?;

        unsafe {
            let lib = Library::new("libnvidia-encode.so.1")
                .context("loading libnvidia-encode.so.1 (NVIDIA driver)")?;
            let create_instance = lib
                .get::<unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS>(
                    b"NvEncodeAPICreateInstance\0",
                )?;
            let mut api: NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
            api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            let status = create_instance(&mut api);
            ensure!(
                status == NVENCSTATUS::NV_ENC_SUCCESS,
                "NvEncodeAPICreateInstance failed: {status:?} (driver too old for NVENC API {}.{}?)",
                NVENCAPI_MAJOR_VERSION,
                NVENCAPI_MINOR_VERSION
            );

            let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            open.deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA;
            open.device = cuda.raw().cast();
            open.apiVersion = NVENCAPI_VERSION;
            let mut encoder = ptr::null_mut();
            let status = api.nvEncOpenEncodeSessionEx.unwrap()(&mut open, &mut encoder);
            ensure!(
                status == NVENCSTATUS::NV_ENC_SUCCESS,
                "nvEncOpenEncodeSessionEx failed: {status:?}"
            );

            let mut this = Self {
                api,
                encoder,
                input: ptr::null_mut(),
                output: ptr::null_mut(),
                config,
                init: Box::new(std::mem::zeroed()),
                cfg: Box::new(std::mem::zeroed()),
                force_idr: true,
                registered: Vec::new(),
                cuda: cuda.clone(),
                _lib: lib,
            };
            this.initialize()?;
            Ok(this)
        }
    }

    unsafe fn initialize(&mut self) -> anyhow::Result<()> {
        let c = self.config;
        let (codec_guid, profile_guid) = match c.codec {
            Codec::Hevc => (NV_ENC_CODEC_HEVC_GUID, NV_ENC_HEVC_PROFILE_MAIN_GUID),
            Codec::H264 => (NV_ENC_CODEC_H264_GUID, NV_ENC_H264_PROFILE_HIGH_GUID),
        };
        let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

        unsafe {
            let mut preset: NV_ENC_PRESET_CONFIG = std::mem::zeroed();
            preset.version = NV_ENC_PRESET_CONFIG_VER;
            preset.presetCfg.version = NV_ENC_CONFIG_VER;
            self.check(
                self.api.nvEncGetEncodePresetConfigEx.unwrap()(
                    self.encoder,
                    codec_guid,
                    NV_ENC_PRESET_P1_GUID,
                    tuning,
                    &mut preset,
                ),
                "nvEncGetEncodePresetConfigEx",
            )?;

            let mut cfg = preset.presetCfg;
            cfg.version = NV_ENC_CONFIG_VER;
            cfg.profileGUID = profile_guid;
            cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
            cfg.frameIntervalP = 1; // no B-frames

            let rc = &mut cfg.rcParams;
            rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            rc.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
            rc.averageBitRate = c.bitrate_bps;
            rc.maxBitRate = c.bitrate_bps;
            // One frame of VBV: every frame drains before the next, so no queueing delay.
            rc.vbvBufferSize = c.bitrate_bps / c.fps;
            rc.vbvInitialDelay = rc.vbvBufferSize;
            rc.set_zeroReorderDelay(1);

            match c.codec {
                Codec::Hevc => {
                    let hevc = &mut cfg.encodeCodecConfig.hevcConfig;
                    hevc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    hevc.set_repeatSPSPPS(1);
                    hevc.set_chromaFormatIDC(1);
                }
                Codec::H264 => {
                    let h264 = &mut cfg.encodeCodecConfig.h264Config;
                    h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    h264.set_repeatSPSPPS(1);
                    h264.chromaFormatIDC = 1;
                }
            }

            let mut init: NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
            init.version = NV_ENC_INITIALIZE_PARAMS_VER;
            init.encodeGUID = codec_guid;
            init.presetGUID = NV_ENC_PRESET_P1_GUID;
            init.tuningInfo = tuning;
            init.encodeWidth = c.width;
            init.encodeHeight = c.height;
            init.darWidth = c.width;
            init.darHeight = c.height;
            init.maxEncodeWidth = c.width;
            init.maxEncodeHeight = c.height;
            init.frameRateNum = c.fps;
            init.frameRateDen = 1;
            init.enablePTD = 1;
            init.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
            // Both live in boxes so their addresses stay valid for later reconfigures.
            *self.cfg = cfg;
            init.encodeConfig = &mut *self.cfg;
            *self.init = init;
            let initialize = self.api.nvEncInitializeEncoder.unwrap();
            let status = initialize(self.encoder, &mut *self.init);
            self.check(status, "nvEncInitializeEncoder")?;

            let mut input: NV_ENC_CREATE_INPUT_BUFFER = std::mem::zeroed();
            input.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
            input.width = c.width;
            input.height = c.height;
            input.bufferFmt = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
            self.check(
                self.api.nvEncCreateInputBuffer.unwrap()(self.encoder, &mut input),
                "nvEncCreateInputBuffer",
            )?;
            self.input = input.inputBuffer;

            let mut output: NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
            output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            self.check(
                self.api.nvEncCreateBitstreamBuffer.unwrap()(self.encoder, &mut output),
                "nvEncCreateBitstreamBuffer",
            )?;
            self.output = output.bitstreamBuffer;
        }

        log::info!(
            "NVENC {:?} {}x{}@{} {} kbit/s (P1, ultra-low-latency, CBR)",
            c.codec,
            c.width,
            c.height,
            c.fps,
            c.bitrate_bps / 1000
        );
        Ok(())
    }

    pub fn config(&self) -> EncoderConfig {
        self.config
    }

    /// Changes the CBR bitrate in place (no IDR, no new session).
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> anyhow::Result<()> {
        if bitrate_bps == self.config.bitrate_bps {
            return Ok(());
        }
        let rc = &mut self.cfg.rcParams;
        rc.averageBitRate = bitrate_bps;
        rc.maxBitRate = bitrate_bps;
        rc.vbvBufferSize = bitrate_bps / self.config.fps;
        rc.vbvInitialDelay = rc.vbvBufferSize;
        self.init.encodeConfig = &mut *self.cfg;

        let cuda = self.cuda.clone();
        let _current = cuda.make_current()?;
        unsafe {
            let mut params: NV_ENC_RECONFIGURE_PARAMS = std::mem::zeroed();
            params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
            params.reInitEncodeParams = *self.init;
            self.check(
                self.api.nvEncReconfigureEncoder.unwrap()(self.encoder, &mut params),
                "nvEncReconfigureEncoder",
            )?;
        }
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }

    /// Changes the frame rate rate control assumes (VBV stays one frame), in place.
    pub fn set_fps(&mut self, fps: u32) -> anyhow::Result<()> {
        if fps == self.config.fps || fps == 0 {
            return Ok(());
        }
        self.init.frameRateNum = fps;
        self.init.frameRateDen = 1;
        let rc = &mut self.cfg.rcParams;
        rc.vbvBufferSize = self.config.bitrate_bps / fps;
        rc.vbvInitialDelay = rc.vbvBufferSize;
        self.init.encodeConfig = &mut *self.cfg;

        let cuda = self.cuda.clone();
        let _current = cuda.make_current()?;
        unsafe {
            let mut params: NV_ENC_RECONFIGURE_PARAMS = std::mem::zeroed();
            params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
            params.reInitEncodeParams = *self.init;
            self.check(
                self.api.nvEncReconfigureEncoder.unwrap()(self.encoder, &mut params),
                "nvEncReconfigureEncoder(fps)",
            )?;
        }
        self.config.fps = fps;
        Ok(())
    }

    /// Makes the next frame an IDR with VPS/SPS/PPS, e.g. for a newly connected client.
    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }

    /// Encodes one 32-bit BGRx/BGRA frame (`stride` bytes per row) and hands the
    /// resulting access unit to `on_output`.
    pub fn encode(
        &mut self,
        src: &[u8],
        stride: u32,
        pts: u64,
        on_output: impl FnOnce(EncodedFrame<'_>),
    ) -> anyhow::Result<()> {
        let cuda = self.cuda.clone();
        let _current = cuda.make_current()?;
        let (w, h) = (self.config.width, self.config.height);
        let row_bytes = w as usize * 4;
        ensure!(
            stride as usize >= row_bytes && src.len() >= (h as usize - 1) * stride as usize + row_bytes,
            "frame {}B stride {stride} too small for {w}x{h}",
            src.len()
        );

        unsafe {
            let mut lock: NV_ENC_LOCK_INPUT_BUFFER = std::mem::zeroed();
            lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
            lock.inputBuffer = self.input;
            self.check(
                self.api.nvEncLockInputBuffer.unwrap()(self.encoder, &mut lock),
                "nvEncLockInputBuffer",
            )?;
            let dst = lock.bufferDataPtr as *mut u8;
            let pitch = lock.pitch as usize;
            if pitch == stride as usize {
                ptr::copy_nonoverlapping(src.as_ptr(), dst, pitch * (h as usize - 1) + row_bytes);
            } else {
                for y in 0..h as usize {
                    ptr::copy_nonoverlapping(
                        src.as_ptr().add(y * stride as usize),
                        dst.add(y * pitch),
                        row_bytes,
                    );
                }
            }
            self.check(
                self.api.nvEncUnlockInputBuffer.unwrap()(self.encoder, self.input),
                "nvEncUnlockInputBuffer",
            )?;

            self.encode_input(self.input, pitch as u32, InputFormat::Bgra, pts, on_output)?;
        }
        Ok(())
    }

    /// Registers a CUDA device buffer (ARGB, `config` size) as encoder input. Returns the
    /// index to pass to [`Self::encode_cuda`].
    pub fn register_cuda_buffer(
        &mut self,
        buf: DeviceBuffer,
        format: InputFormat,
    ) -> anyhow::Result<usize> {
        let cuda = self.cuda.clone();
        let _current = cuda.make_current()?;
        unsafe {
            let mut reg: NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
            reg.version = NV_ENC_REGISTER_RESOURCE_VER;
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
            reg.width = self.config.width;
            reg.height = self.config.height;
            reg.pitch = buf.pitch as u32;
            reg.resourceToRegister = buf.ptr as *mut c_void;
            reg.bufferFormat = format.nvenc();
            reg.bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
            let register = self.api.nvEncRegisterResource.unwrap();
            let status = register(self.encoder, &mut reg);
            self.check(status, "nvEncRegisterResource")?;
            self.registered.push((reg.registeredResource, buf.pitch as u32, format));
        }
        Ok(self.registered.len() - 1)
    }

    /// Encodes the registered CUDA buffer `index` (see [`Self::register_cuda_buffer`]).
    pub fn encode_cuda(
        &mut self,
        index: usize,
        pts: u64,
        on_output: impl FnOnce(EncodedFrame<'_>),
    ) -> anyhow::Result<()> {
        let cuda = self.cuda.clone();
        let _current = cuda.make_current()?;
        let (resource, pitch, format) = self.registered[index];
        unsafe {
            let mut map: NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
            map.registeredResource = resource;
            let map_fn = self.api.nvEncMapInputResource.unwrap();
            let status = map_fn(self.encoder, &mut map);
            self.check(status, "nvEncMapInputResource")?;

            let result = self.encode_input(map.mappedResource, pitch, format, pts, on_output);
            self.api.nvEncUnmapInputResource.unwrap()(self.encoder, map.mappedResource);
            result
        }
    }

    /// Encodes one picture from an input handle (locked system buffer or mapped resource).
    unsafe fn encode_input(
        &mut self,
        input: NV_ENC_INPUT_PTR,
        pitch: u32,
        format: InputFormat,
        pts: u64,
        on_output: impl FnOnce(EncodedFrame<'_>),
    ) -> anyhow::Result<()> {
        let (w, h) = (self.config.width, self.config.height);
        unsafe {
            let mut pic: NV_ENC_PIC_PARAMS = std::mem::zeroed();
            pic.version = NV_ENC_PIC_PARAMS_VER;
            pic.inputWidth = w;
            pic.inputHeight = h;
            pic.inputPitch = pitch;
            pic.inputBuffer = input;
            pic.outputBitstream = self.output;
            pic.bufferFmt = format.nvenc();
            pic.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            pic.inputTimeStamp = pts;
            if self.force_idr {
                pic.encodePicFlags = (NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR.0
                    | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS.0)
                    as u32;
            }
            self.check(
                self.api.nvEncEncodePicture.unwrap()(self.encoder, &mut pic),
                "nvEncEncodePicture",
            )?;
            self.force_idr = false;

            let mut bs: NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
            bs.version = NV_ENC_LOCK_BITSTREAM_VER;
            bs.outputBitstream = self.output;
            self.check(
                self.api.nvEncLockBitstream.unwrap()(self.encoder, &mut bs),
                "nvEncLockBitstream",
            )?;
            let data = std::slice::from_raw_parts(
                bs.bitstreamBufferPtr as *const u8,
                bs.bitstreamSizeInBytes as usize,
            );
            let keyframe = bs.pictureType == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR
                || bs.pictureType == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I;
            on_output(EncodedFrame {
                data,
                keyframe,
                pts: bs.outputTimeStamp,
            });
            self.check(
                self.api.nvEncUnlockBitstream.unwrap()(self.encoder, self.output),
                "nvEncUnlockBitstream",
            )?;
        }
        Ok(())
    }

    fn check(&self, status: NVENCSTATUS, what: &str) -> anyhow::Result<()> {
        if status == NVENCSTATUS::NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = unsafe {
            let msg = self.api.nvEncGetLastErrorString.unwrap()(self.encoder);
            if msg.is_null() {
                String::new()
            } else {
                CStr::from_ptr(msg).to_string_lossy().into_owned()
            }
        };
        bail!("{what} failed: {status:?} {detail}")
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        let cuda = self.cuda.clone();
        let _current = cuda.make_current();
        unsafe {
            for (resource, _, _) in self.registered.drain(..) {
                self.api.nvEncUnregisterResource.unwrap()(self.encoder, resource);
            }
            if !self.input.is_null() {
                self.api.nvEncDestroyInputBuffer.unwrap()(self.encoder, self.input);
            }
            if !self.output.is_null() {
                self.api.nvEncDestroyBitstreamBuffer.unwrap()(self.encoder, self.output);
            }
            self.api.nvEncDestroyEncoder.unwrap()(self.encoder);
        }
    }
}

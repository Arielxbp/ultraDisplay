//! Raw NVENC bindings plus the pieces bindgen can't produce: struct version macros and
//! the `static const GUID` values from nvEncodeAPI.h.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]
#![allow(clippy::all, unused_imports, unnecessary_transmutes)]

include!(concat!(env!("OUT_DIR"), "/nvenc_sys.rs"));

pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);

pub const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_CONFIG_VER: u32 = struct_version(9) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5) | (1 << 31);
pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = struct_version(2);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2) | (1 << 31);
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(2) | (1 << 31);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}

pub const NV_ENC_CODEC_H264_GUID: GUID = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub const NV_ENC_CODEC_HEVC_GUID: GUID = guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = guid(
    0xe7cbc309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
pub const NV_ENC_HEVC_PROFILE_MAIN_GUID: GUID = guid(
    0xb514c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
pub const NV_ENC_PRESET_P1_GUID: GUID = guid(
    0xfc0a8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);

/// CUDA driver API types (from ffnvcodec's dynlink_cuda.h).
pub mod cuda {
    #![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]
    #![allow(clippy::all, unused_imports, unnecessary_transmutes)]
    include!(concat!(env!("OUT_DIR"), "/cuda_sys.rs"));
}

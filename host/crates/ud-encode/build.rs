use std::path::PathBuf;

// Types only: the NVENC entry point is resolved at runtime from the driver's
// libnvidia-encode.so.1, so nothing is linked here. Header from `ffnvcodec-headers`.
const HEADER: &str = "/usr/include/ffnvcodec/nvEncodeAPI.h";
const CUDA_HEADER: &str = "/usr/include/ffnvcodec/dynlink_cuda.h";

fn main() {
    println!("cargo:rerun-if-changed={HEADER}");
    println!("cargo:rerun-if-changed={CUDA_HEADER}");

    let bindings = bindgen::Builder::default()
        .header(HEADER)
        .allowlist_type("NV_ENC.*|_NV_ENC.*|NV_ENCODE_API_FUNCTION_LIST|NVENCSTATUS|GUID")
        .allowlist_var("NVENCAPI_(MAJOR|MINOR)_VERSION|NVENC_INFINITE_GOPLENGTH")
        .default_enum_style(bindgen::EnumVariation::NewType {
            is_bitfield: false,
            is_global: false,
        })
        .layout_tests(false)
        .generate_comments(false)
        .generate()
        .expect("generating NVENC bindings (is ffnvcodec-headers installed?)");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out_dir.join("nvenc_sys.rs")).expect("writing NVENC bindings");

    // CUDA driver API types for the zero-copy path (functions are loaded at runtime).
    let cuda = bindgen::Builder::default()
        .header(CUDA_HEADER)
        .allowlist_type("CU.*|CUDA_MEMCPY2D|tcu.*")
        .allowlist_var("CU_.*")
        .default_enum_style(bindgen::EnumVariation::NewType {
            is_bitfield: false,
            is_global: false,
        })
        .layout_tests(false)
        .generate_comments(false)
        .generate()
        .expect("generating CUDA bindings");
    cuda.write_to_file(out_dir.join("cuda_sys.rs")).expect("writing CUDA bindings");
}

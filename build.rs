// Compiles the C shim over llama.cpp (csrc/llama_shim.c) and links the libraries scripts/build-llama.sh built into
// Vendor/llama. The binary finds them through its rpath: Vendor/llama/lib, or next to itself once installed.

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = std::env::var("RECALL_LLAMA_DIR").map(PathBuf::from).unwrap_or_else(|_| root.join("Vendor/llama"));
    let include = vendor.join("include");
    let lib = vendor.join("lib");
    if !lib.join("libllama.so").exists() {
        panic!("llama.cpp isn't built: run scripts/build-llama.sh first (looked in {})", lib.display());
    }
    cc::Build::new()
        .file("csrc/llama_shim.c")
        .include(&include)
        .flag_if_supported("-std=c11")
        .warnings(true)
        .compile("recall_llama_shim");
    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in ["mtmd", "llama", "ggml", "ggml-base"] {
        println!("cargo:rustc-link-lib=dylib={name}");
    }
    println!("cargo:rustc-link-lib=m");
    // Prefer bundled libraries when relocated; fall back to the source build for development.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
    // Packages keep only the relative paths: an absolute build folder would be a library search path on every machine.
    if std::env::var_os("RECALL_RELOCATABLE").is_none() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    }
    println!("cargo:rustc-env=RECALL_LLAMA_LIB_DIR={}", lib.display());
    // The llama.cpp release, for logs and `recall status`.
    let header = std::fs::read_to_string(include.join("llama-build.h")).unwrap_or_default();
    let tag = header.split("LLAMA_BUILD_TAG \"").nth(1).and_then(|rest| rest.split('"').next()).unwrap_or("unknown");
    println!("cargo:rustc-env=RECALL_LLAMA_BUILD_TAG={tag}");
    println!("cargo:rerun-if-changed=csrc/llama_shim.c");
    println!("cargo:rerun-if-changed={}", include.display());
    println!("cargo:rerun-if-env-changed=RECALL_LLAMA_DIR");
    println!("cargo:rerun-if-env-changed=RECALL_RELOCATABLE");
}

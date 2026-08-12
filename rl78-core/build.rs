//! Build tlib (RL78) as a static library and link host callback overrides.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let tlib_src = manifest_dir.join("tlib");
    if !tlib_src.join("CMakeLists.txt").is_file() {
        panic!(
            "tlib sources missing at {}. Run: git submodule update --init --recursive",
            tlib_src.display()
        );
    }

    println!("cargo:rerun-if-changed={}", tlib_src.join("CMakeLists.txt").display());
    println!("cargo:rerun-if-changed={}", tlib_src.join("exports.c").display());
    println!("cargo:rerun-if-changed={}", tlib_src.join("callbacks.c").display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/host_callbacks.c").display()
    );

    // One .o with an anchor symbol: referencing the anchor from Rust pulls every
    // strong tlib_* override out of this archive (overrides weak libtlib stubs).
    cc::Build::new()
        .file(manifest_dir.join("src/host_callbacks.c"))
        .compile("rl78_tlib_callbacks");

    let dst = cmake::Config::new(&tlib_src)
        .define("TARGET_ARCH", "rl78")
        .define("TARGET_WORD_SIZE", "32")
        .define("HOST_ARCH", host_arch())
        .build_target("tlib")
        .profile("Release")
        .build();

    // `build_target` skips install; the archive lands in `<OUT>/build/libtlib.a`.
    // `cc::Build` already emitted `rustc-link-lib=static=rl78_tlib_callbacks`.
    let lib_dir = dst.join("build");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=tlib");
    println!("cargo:rustc-link-lib=dylib=pthread");
}

fn host_arch() -> &'static str {
    match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "x86_64" | "x86" => "i386",
        other => panic!("unsupported host arch for tlib: {other} (x86 / x86_64 only)"),
    }
}

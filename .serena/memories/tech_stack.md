# Tech stack

- Language: Rust edition **2024**, `rust-version = "1.85"`, toolchain channel `stable` (`rust-toolchain.toml`: rustfmt, clippy).
- Workspace resolver `"3"`; members `sim-kernel`, `rl78-core`; root package is the CLI bin only.
- Native: CMake + C compiler; `rl78-core/build.rs` builds tlib (`TARGET_ARCH=rl78`, `TARGET_WORD_SIZE=32`) → `libtlib.a`, links `pthread`, plus `cc` for `host_callbacks.c` → `rl78_tlib_callbacks`.
- Crates of note: `object` 0.40 (ELF), `typenum` 1.17 (`Wire` counts), build-deps `cc`/`cmake`, rl78-core dev-dep `serial_test` 3.5.
- Submodule: `rl78-core/tlib` → https://github.com/Soya-Onishi/tlib.git branch `rl78`.
- Devcontainer present (clangd + rust-analyzer); no separate package manager beyond Cargo.
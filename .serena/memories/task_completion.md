# Task completion

After Rust/C build-script changes affecting this workspace:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

If `rl78-core/tlib` or `host_callbacks.c` changed, ensure a clean link path still works (`cargo test -p rl78-core` exercises FFI). CLI behavior: `cargo test --test cli_lifecycle` or manual `cargo run` with `start`/`stop`/`quit`.

Do not skip submodule init when the tree is fresh — missing `tlib/CMakeLists.txt` panics in `build.rs`.
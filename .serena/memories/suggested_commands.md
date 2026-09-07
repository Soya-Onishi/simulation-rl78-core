# Suggested commands

```bash
git submodule update --init --recursive   # required before first cargo build of rl78-core
cargo test
cargo run                                 # CLI: stdin start|stop|quit
cargo run -- path/to/guest.elf
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test -p sim-cluster
cargo build -p sim-cluster --bins
cargo run -p sim-cluster --bin cluster-server -- python/examples/two_board_uart.py
```

Prereqs: `cmake`, C toolchain, `pthread`. tlib rebuild is driven by `rl78-core/build.rs` (`cargo:rerun-if-changed` on whole `tlib/` tree).

Linux: standard unix forms of `git`/`ls`/`rg` apply — no project-specific shell quirks.
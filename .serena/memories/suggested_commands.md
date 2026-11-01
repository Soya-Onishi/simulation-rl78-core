# Suggested commands

```bash
git submodule update --init --recursive   # required before first cargo build of rl78-core
cargo test
cargo run                                 # CLI: stdin start|stop|quit
cargo run -- path/to/guest.elf
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test -p sim-cluster
cargo build -p sim-cluster --bins -p rl78-minimal-board
cargo run -p sim-cluster --bin cluster-backend -- --config example/config/backend.yaml
cargo run -p sim-cluster --bin simulation-frontend -- --config example/config/frontend.yaml
scripts/create-sim.sh python/examples/two_board_uart.py
# or: python -m topology_dsl create python/examples/two_board_uart.py
# PYTHONPATH=python; override server with --server or SIMULATION_SERVER
```

Prereqs: `cmake`, C toolchain, `pthread`. tlib rebuild is driven by `rl78-core/build.rs` (`cargo:rerun-if-changed` on whole `tlib/` tree).

Linux: standard unix forms of `git`/`ls`/`rg` apply — no project-specific shell quirks.

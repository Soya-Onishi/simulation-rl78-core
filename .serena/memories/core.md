# simulation-rl78-core

RL78 guest sim: Rust workspace + tlib (TCG) submodule. No config files — machine maps are Rust code.

## Crates
- `simulation-rl78-core` (`src/main.rs`): thin CLI bin; REPL thread + sim thread via `sim_kernel::spawn`. GDB RSP is served by `simulation-frontend`, not this CLI.
- `sim-kernel`: arch-independent kernel — virtual clock, event queue, `MemoryBus`/`MemoryMapped`, start/stop/quit, inspect API. See `mem:sim-kernel/core`.
- `rl78-core`: RL78 assembly — tlib static link (`build.rs`+CMake), `Rl78Cpu`, minimal map, Magic probe, ELF load via `Cpu::load_firmware`. See `mem:rl78-core/core`.
- `sim-cluster`: multi-board cluster — logical topology JSON, `simulation-frontend` (axum: sim UI `/{id}` (GET `/` and unknown id are 404), API under `/api/simulation...` + SSE status/log; gdb listen/slot owned by the HTTP/web layer on `127.0.0.1`; talks to `cluster-backend` over internal HTTP `/internal` via `TcpipBackend`; YAML `--config`) and `cluster-backend` (`LocalBackend` + worker + internal HTTP; does not exit when an arbiter thread ends; HTTP create accepts topology JSON only — `elf` must be `http(s)://` or `base64:`; python DSL is client-side via `python -m topology_dsl create` / `scripts/create-sim`). One simulation = one in-process arbiter OS thread (no `cluster-arbiter` binary). Server↔arbiter uses two unbounded mpsc channels (`ipc/server_arbiter.rs`); arbiter↔board/plant uses iceoryx2. `board_runtime::run_board` (same-thread Simulator loop). Default board binary: `rl78-minimal-board`. Python DSL under `python/topology_dsl/`. Issue #37/#47; follow-up orphan recovery: #115.
- `rl78-minimal-board`: RL78 minimal board process; `minimal_machine` + `run_board`.

## Source map (exclude vendored tlib tree unless changing FFI)
- Root: `Cargo.toml` workspace, `tests/cli_lifecycle.rs`, `python/topology_dsl/`, `python/examples/`, `scripts/create-sim.sh`, `scripts/create-sim.bat`.
- `sim-cluster/src/{lib,topology,elf_source,server,backend,contract,http,web,arbiter,board_runtime,control,lifecycle,cluster_stop,time_sync,node,inspect}.rs`, `sim-cluster/src/gdb/{mod,arch,target}.rs`, `sim-cluster/src/ipc/{mod,hash,names,runtime,control_bus,uart_bus,server_arbiter,log_bus,action_bus,probe_*}.rs`, bins `cluster_backend.rs` / `simulation_frontend.rs`, UI `sim-cluster/web/index.html`. Control plane: a2n `ControlToNode` (broadcast variants) and n2a `ControlToArbiter` (`from` sender hash), both `repr(C)` + `ZeroCopySend` / `HostStopReason`. Server↔arbiter host commands / status / logs / action / probe host traffic share one mpsc each direction (enum variants). Logs: `ClusterLog` on iceoryx `log/n2a` (node→arbiter) then mpsc to server. Call sites use the `log` crate. Board/plant node processes install one process-wide log queue; each in-process arbiter thread installs a thread-local queue (mutex, cap 256) so concurrent Simulations do not mix logs. A forwarder thread owns the iceoryx `log/n2a` port. Tests that install that queue are `#[serial]`. The server keeps every log level for the page; display filtering is client-side.
- `sim-kernel/src/{lib,machine,sim,bus,cpu,command,clock,event,stop,breakpoint}.rs` — `Cpu::load_firmware` / `Machine::load_firmware` / `FirmwareError`.
- `rl78-core/src/{lib,cpu,callbacks,ffi,map,magic,elf}.rs`, `build.rs`, `src/host_callbacks.c`, submodule `rl78-core/tlib` (git branch `rl78`).
- `rl78-minimal-board/src/main.rs`.

## Invariants
- Host front-ends talk only through `Command`/`Response` on the sim thread; `Machine` is owned exclusively there.
- Memory map is immutable after `Machine::new` — finish with `MemoryMapBuilder`, then construct once (`Cpu::bind_memory` at construction).
- Magic probe MMIO at guest phys `0xF0000` (size 256); RL78 `MOV !addr16,#imm` reaches `addr16|0xF0000`.
- tlib host arch support in build: x86 / x86_64 only (`HOST_ARCH=i386`).
- First build needs `git submodule update --init --recursive`.
- `sim-cluster` depends on `sim-kernel` only (not `rl78-core`).
- Cluster gdbserver uses a generic `GdbArch` (opaque `g` bytes + PC, 32-bit RSP addresses). Packet layout lives in the CPU crate (`hw/cpus/rl78` via `Core::read_gdb_registers`).
- Topology `elf` is one string. Exclusive forms: filesystem path, `file://`, `http(s)://`, or `base64:` payload. Loaded by `sim-cluster/src/elf_source.rs`. JSON validate checks the form only (no local stat, no fetch). HTTP `POST /api/simulation` accepts `http(s)://` and `base64:` only; the client embeds local files.

## Related
- Stack/tooling: `mem:tech_stack`
- Day-to-day commands: `mem:suggested_commands`
- Style/patterns: `mem:conventions`
- Done checklist: `mem:task_completion`

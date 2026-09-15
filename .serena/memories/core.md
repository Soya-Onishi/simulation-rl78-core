# simulation-rl78-core

RL78 guest sim: Rust workspace + tlib (TCG) submodule. No config files — machine maps are Rust code.

## Crates
- `simulation-rl78-core` (`src/main.rs`): thin CLI bin; REPL thread + sim thread via `sim_kernel::spawn`.
- `sim-kernel`: arch-independent kernel — virtual clock, event queue, `MemoryBus`/`MemoryMapped`, start/stop/quit, inspect API. See `mem:sim-kernel/core`.
- `rl78-core`: RL78 assembly — tlib static link (`build.rs`+CMake), `Rl78Cpu`, minimal map, Magic probe, ELF load via `Cpu::load_firmware`. See `mem:rl78-core/core`.
- `sim-cluster`: multi-board cluster — logical topology JSON, `cluster-server` (Python DSL → arbiter), `cluster-arbiter` (iceoryx2 create + Ready/Start/time ceiling/`HostStop`→`ClusterStop`), `board_runtime::run_board` (same-thread Simulator loop). Default board binary: `rl78-minimal-board`. Python DSL under `python/topology_dsl/`. Issue #37/#47; IPC via iceoryx2 pub/sub (`sim-cluster/src/ipc/`).
- `rl78-minimal-board`: RL78 minimal board process; `minimal_machine` + `run_board`.

## Source map (exclude vendored tlib tree unless changing FFI)
- Root: `Cargo.toml` workspace, `tests/cli_lifecycle.rs`, `python/topology_dsl/`, `python/examples/`.
- `sim-cluster/src/{lib,topology,server,arbiter,board_runtime,control,lifecycle,cluster_stop,time_sync,node}.rs`, `sim-cluster/src/ipc/{mod,hash,names,wire,runtime,control_bus,uart_bus}.rs`, bins `cluster_{server,arbiter}.rs`. Control plane: a2n `ControlToNode` (broadcast variants) and n2a `ControlToArbiter` (`from` sender hash), both `repr(C)` + `ZeroCopySend` / `HostStopReason` — no separate `ControlWire`.
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

## Related
- Stack/tooling: `mem:tech_stack`
- Day-to-day commands: `mem:suggested_commands`
- Style/patterns: `mem:conventions`
- Done checklist: `mem:task_completion`
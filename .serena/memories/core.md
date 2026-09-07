# simulation-rl78-core

RL78 guest sim: Rust workspace + tlib (TCG) submodule. No config files — machine maps are Rust code.

## Crates
- `simulation-rl78-core` (`src/main.rs`): thin CLI bin; REPL thread + sim thread via `sim_kernel::spawn`.
- `sim-kernel`: arch-independent kernel — virtual clock, event queue, `MemoryBus`/`MemoryMapped`, start/stop/quit, inspect API. See `mem:sim-kernel/core`.
- `rl78-core`: RL78 assembly — tlib static link (`build.rs`+CMake), `Rl78Cpu`, minimal map, Magic probe, ELF load. See `mem:rl78-core/core`.
- `sim-cluster`: multi-board cluster — logical topology JSON, singleton `cluster-server` (runs Python DSL → spawns arbiter), `cluster-arbiter` (UDS Ready barrier / later SHM+time), `cluster-node` (per-board). Python DSL under `python/topology_dsl/`. Issue #37.

## Source map (exclude vendored tlib tree unless changing FFI)
- Root: `Cargo.toml` workspace, `tests/cli_lifecycle.rs`, `python/topology_dsl/`, `python/examples/`.
- `sim-cluster/src/{lib,topology,server,arbiter,control,node}.rs`, bins `cluster_{server,arbiter,node}.rs`.
- `sim-kernel/src/{lib,machine,sim,bus,cpu,command,clock,event,stop,breakpoint}.rs`.
- `rl78-core/src/{lib,cpu,callbacks,ffi,map,magic,elf}.rs`, `build.rs`, `src/host_callbacks.c`, submodule `rl78-core/tlib` (git branch `rl78`).

## Invariants
- Host front-ends talk only through `Command`/`Response` on the sim thread; `Machine` is owned exclusively there.
- Memory map is immutable after `Machine::new` — finish with `MemoryMapBuilder`, then construct once (`Cpu::bind_memory` at construction).
- Magic probe MMIO at guest phys `0xF0000` (size 256); RL78 `MOV !addr16,#imm` reaches `addr16|0xF0000`.
- tlib host arch support in build: x86 / x86_64 only (`HOST_ARCH=i386`).
- First build needs `git submodule update --init --recursive`.

## Related
- Stack/tooling: `mem:tech_stack`
- Day-to-day commands: `mem:suggested_commands`
- Style/patterns: `mem:conventions`
- Done checklist: `mem:task_completion`
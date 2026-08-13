# rl78-core

RL78-specific core on top of `sim-kernel` + tlib.

## Layout
- `build.rs`: require submodule; compile `host_callbacks.c`; CMake `tlib` Release static lib; link `tlib` + `pthread`.
- `ffi` / `callbacks`: tlib FFI + host memory/IO CB overrides (strong symbols via archive anchor).
- `cpu::Rl78Cpu`: owns tlib session; `bind_memory` maps ROM/RAM host regions and IO for MMIO (Magic).
- `map`: `Rl78Device` / `MemoryLayout`; default `Generic64k` — ROM `0x00000` 64KiB, RAM `0xF0100` 32KiB (leaves `0xF0000` for probe).
- `magic`: `MagicProbe` + `ProbeSink` (stdout or test buffer).
- `elf`: load RL78 ELF into machine ROM (`object` crate); `EM_RL78`.
- Entry helpers: `minimal_machine` / `minimal_machine_with_probe` / `build_minimal_bus`.

## Invariants
- Do not hand-edit in-tree CMake build dirs; Cargo OUT_DIR owns tlib artifacts.
- Integration smoke: `tests/magic_smoke.rs` (own process semantics for guest execute).
- Shared conventions: `mem:conventions`. Build/test cmds: `mem:suggested_commands`.
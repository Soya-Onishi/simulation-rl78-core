# Conventions

- **Config-in-code**: never add machine/board config files; extend Rust builders (`MinimalMachineConfig`, `MemoryMapBuilder`, `Rl78Device`).
- **Layering**: arch crates implement `sim_kernel::Cpu` + devices. CLI and the in-crate GDB stub talk through `Command`/`Response` / `spawn` (`listen_gdb`). Do not put a second control plane in `rl78-core`.
- **Bus lifetime**: build map → `Machine::new` → one-time `bind_memory`; do not mutate region list afterward. `host_ptr()` for TCG direct map; `None` → IO callbacks.
- **Callback stops**: `PendingStop::request` alone does not leave `tlib_execute` — also `tlib_set_return_request` (or TB interrupt); then `resolve_after_tlib_execute`. Soft BPs use `EXCP_DEBUG` / `map_tlib_exit`, not the latch.
- **tlib tests**: mark with `#[serial]` (`serial_test`); tlib is unsafe to re-enter across concurrent tests / after some guest execute paths (see `rl78-core/tests/magic_smoke.rs` comment).
- **API style**: `#[must_use]` on constructors/getters; module rustdocs in English; public re-exports from each crate `lib.rs`.
- **Naming**: guest phys `Addr = u64`; register ids `RegId(u32)` numeric, not strings.
- README/issue notes may be Japanese; keep code comments English.

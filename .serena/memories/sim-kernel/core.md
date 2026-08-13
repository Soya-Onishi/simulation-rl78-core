# sim-kernel

Arch-independent simulation kernel crate.

## Modules
- `machine`: `Machine<C: Cpu>` — CPU + boxed `MemoryBus` + `VirtualClock` + `EventQueue` + breakpoints; exclusive to sim thread.
- `sim`: `Simulator`, `spawn` → `(SimControl, SimEvents)` mpsc; default quantum `DEFAULT_MAX_QUANTUM` = 10_000 ns, `ns_per_instruction = 1`.
- `bus`: `MemoryMapped`, `MemoryBus`, `MemoryMapBuilder`, `Rom`/`Ram`, `UnmappedPolicy`.
- `cpu`: `Cpu` trait, `Quantum`, `PendingStop`, `TlibExit`, `map_tlib_exit` / `resolve_after_tlib_execute`.
- `command`: `Command` / `Response` / inspect surface for host front-ends.
- `clock` / `event` / `stop` / `breakpoint`: virtual time, scheduled events, stop reasons, BP store.
- `testing` (cfg test): fake CPU for kernel unit tests.

## Invariants
- Guest mutation only while `SimState::Running`.
- Front-ends must not hold `&mut Machine` across threads — use `SimControl`.
- Further style shared with workspace: `mem:conventions`.
# sim-kernel

Arch-independent simulation kernel crate.

## Modules
- `machine`: `Machine<C: Cpu>` — CPU + boxed `MemoryBus` + `VirtualClock` + `EventQueue` + breakpoints; exclusive to sim thread.
- `wiring`: `Wire<T, Sinks, Sources>` (typenum; 1-N or N-1) / `SourcePort<T>`. Ports hold the wire `Arc`. `drive(value)` invokes `'static` sink closures immediately (closures capture sink-side components). Combinational chaining is allowed. Loop detection is not implemented.
- `sim`: `Simulator`, `spawn` → `(SimControl, SimEvents)` mpsc; default quantum `DEFAULT_MAX_QUANTUM` = 10_000 ns, `ns_per_instruction = 1`.
- `bus`: `MemoryMapped`, `MemoryBus`, `MemoryMapBuilder`, `Rom`/`Ram`, `UnmappedPolicy`.
- `cpu`: `Cpu` trait, `Quantum`, `PendingStop`, `TlibExit`, `map_tlib_exit` / `resolve_after_tlib_execute`.
- `command`: `Command` / `Response` / inspect surface for host front-ends.
- `clock` / `event` / `stop` / `breakpoint`: virtual time, scheduled events, stop reasons, BP store. `EventCtx` is bus + stop only.
- `testing` (cfg test): fake CPU for kernel unit tests.

## Invariants
- Guest mutation only while `SimState::Running`.
- Front-ends must not hold `&mut Machine` across threads — use `SimControl`.
- Wires are finished at board construction; no live reconnect; N-N is a compile error (typestate).
- Further style shared with workspace: `mem:conventions`.

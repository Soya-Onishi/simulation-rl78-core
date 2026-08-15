# sim-kernel

Arch-independent simulation kernel crate.

## Modules
- `machine`: `Machine<C: Cpu>` — CPU + boxed `MemoryBus` + `Wiring` + `VirtualClock` + `EventQueue` + breakpoints; exclusive to sim thread. `Machine::new` uses empty `Wiring`; peripherals `drive` via `SourcePort` (not `EventCtx`).
- `wiring`: `Wire<T, Sinks, Sources>` (typenum; 1-N or N-1) / `SolidWire` / `WiringBuilder` / dummy source·sink. Immediate sink callbacks. Nested drive logged and dropped.
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

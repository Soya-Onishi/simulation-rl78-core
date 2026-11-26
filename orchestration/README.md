# orchestration

Workspace area for the split that replaces the monolithic `sim-cluster` crate
(see issues [#141](https://github.com/Soya-Onishi/simulation-rl78-core/issues/141)
and [#139](https://github.com/Soya-Onishi/simulation-rl78-core/issues/139)).

Phased work: [#146](https://github.com/Soya-Onishi/simulation-rl78-core/issues/146)–[#150](https://github.com/Soya-Onishi/simulation-rl78-core/issues/150).
Phase 2 tracker: [#147](https://github.com/Soya-Onishi/simulation-rl78-core/issues/147);
current slice Phase 2-c: [#155](https://github.com/Soya-Onishi/simulation-rl78-core/issues/155).

| Path | Role |
|------|------|
| `contract` | Wire / SHM / REST / iceoryx service contracts and topology, plus the `host_ctrl` SHM mapping, iceoryx node setup / dead-node cleanup, the Command ↔ Node RR service / blocking client, the `sim-node` command line (`NodeConfig`), the Node log record, and `ThreadGroup` (no runtime loops) |
| `sim-command` | Command host (`CommandHost`): Command / Build / Probe / Endpoint threads behind the Orchestrator REST router |
| `sim-node` | Node process runtime: IPC / Probe / Simulation / log threads (`NodeRuntime`) and the `sim-node` binary |

Dependency rule: `sim-command` and `sim-node` depend on `contract`. They must
not depend on `sim-cluster`.

Runtime crates (`sim-orchestrator`, …) land here as the migration proceeds.
New code belongs under `orchestration/`; `sim-cluster` shrinks to a facade
until it can be removed.

## Phase 2 progress

- **2-a (#151)**: `sim-node` crate + `contract::shm::mapping` (`HostCtrl`) (create/open/min_remote_vt/board_vt/bump_stop).
- **2-b (#153)**: Node 3 threads + Command ↔ Node RR (`contract::command2node::rr`). Start / Stop / Reset state machine, `ProbeSubscribe` set (cap 32), `host_ctrl` emergency stop (each `stop_generation` bump stops once). `sim-node` (arguments built by `NodeConfig::to_args`) exits cleanly on `NodeCommand::Shutdown` (Command-internal, not on REST) or when its parent (Command) process exits; signals are not handled.
- **2-c (#155)**: `CommandHost` runs the Command / Build / Probe / Endpoint threads and answers every REST route through the Command thread (none returns 501).
  - `POST /sessions` carries the topology and the `node_key`s this host launches; `host_ctrl` is sized by the topology board count.
  - `build_ensure` launches `sim-node` in its own process group and registers its RR client. `node_command` / `probe_interest` to an unregistered node is 404.
  - terminate / restart send `Shutdown`, then kill after a timeout. On spawn, session directories left by an earlier run are removed after cleaning their dead iceoryx nodes.
  - `status` is `running` from a successful Start until Stop / Reset or a `host_ctrl` `stop_generation` bump.
  - Node threads send `log` records over mpsc to a log thread that writes one JSON line per record (`NodeLogRecord`) to the Orchestrator host stream, a discarding dummy until the TCP link exists.

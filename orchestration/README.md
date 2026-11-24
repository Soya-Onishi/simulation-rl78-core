# orchestration

Workspace area for the split that replaces the monolithic `sim-cluster` crate
(see issues [#141](https://github.com/Soya-Onishi/simulation-rl78-core/issues/141)
and [#139](https://github.com/Soya-Onishi/simulation-rl78-core/issues/139)).

Phased work: [#146](https://github.com/Soya-Onishi/simulation-rl78-core/issues/146)–[#150](https://github.com/Soya-Onishi/simulation-rl78-core/issues/150).
Phase 2 tracker: [#147](https://github.com/Soya-Onishi/simulation-rl78-core/issues/147);
current slice Phase 2-a: [#151](https://github.com/Soya-Onishi/simulation-rl78-core/issues/151).

| Path | Role |
|------|------|
| `contract` | Wire / SHM / REST / iceoryx service contracts, plus the `host_ctrl` SHM mapping (no runtime loops) |
| `sim-command` | Command host: Orchestrator REST router + future 4-thread runtime |
| `sim-node` | Node process runtime (future IPC / Probe / Simulation threads) |

Dependency rule: `sim-command` and `sim-node` depend on `contract`. They must
not depend on `sim-cluster`.

Runtime crates (`sim-orchestrator`, …) land here as the migration proceeds.
New code belongs under `orchestration/`; `sim-cluster` shrinks to a facade
until it can be removed.

## Phase 2 progress

- **2-a (#151)**: `sim-node` crate + `contract::shm::mapping` (`HostCtrl`) (create/open/min_remote_vt/board_vt/bump_stop).
- **Later slices**: 4/3 thread skeletons, REST dispatch, Command↔Node RR, Build dummy process.

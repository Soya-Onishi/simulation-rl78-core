# orchestration

Workspace area for the split that replaces the monolithic `sim-cluster` crate
(see issues [#141](https://github.com/Soya-Onishi/simulation-rl78-core/issues/141)
and [#139](https://github.com/Soya-Onishi/simulation-rl78-core/issues/139)).

| Path | Role |
|------|------|
| `sim-orch-contract` | Wire / SHM / REST / iceoryx service contracts (no runtime loops) |

Runtime crates (`sim-node`, `sim-command`, `sim-orchestrator`, …) land here as
the migration proceeds. New code belongs under `orchestration/`; `sim-cluster`
shrinks to a facade until it can be removed.

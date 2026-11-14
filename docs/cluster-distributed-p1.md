# Cluster distributed simulation — phase 1

Confirmed design: Serena `mem:sim-cluster/distributed-design` (2026-11-14).

## Scope (p1)

- Multicast virtual-time sharing and Cluster Stop between `cluster-backend` processes
- Frontend placement (`max_nodes`, First-Fit), create fanout with compensate
- start/stop/reset fanout; status/log aggregation
- `VtClock` slot 0 = remote hosts' min VT; `until_ns` on `ControlToNode::Start`

## Branches

- Integration: `feature/cluster-distributed`
- This phase: `feature/cluster-distributed-p1`
- Merge to `develop` only after p1–p4 land on the integration branch

## TODO (provisional)

- **Cross-host topology edges**: phase 1 rejects create when an edge connects boards assigned to different backends.
  Remove this rejection after phase 2 implements
  `node → arbiter → multicast → arbiter → node` endpoint forwarding.

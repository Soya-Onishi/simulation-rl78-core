# Phase 1: cluster multicast + VT + control fanout

Source of truth: `mem:sim-cluster/distributed-design`.

## Goal

Enable multi-`cluster-backend` simulations with:

- Multicast virtual-time sharing and Cluster Stop
- Frontend create placement (First-Fit / `max_nodes`), compensate on partial failure
- start/stop/reset fanout and status/log aggregation
- VT slot 0 = remote min; `until_ns` on `ControlToNode::Start`

## Out of scope (later phases)

- Cross-host endpoint forwarding (reject at create; TODO)
- Targeted probe/action/inspect routing
- CSV aggregation

## Dependency order

1. Config + `machine_id` helpers + design memo
2. `VtClock` semantics + `ControlToNode::Start { until_ns }` + node/arbiter until path
3. Multicast wire (VT report + ClusterStop) + arbiter send/recv/throttle
4. Placement + cross-host edge check + create fanout/compensate
5. Frontend multi-backend: start/stop/reset fanout, status/log aggregate
6. Example YAML + unit tests for each slice

## Branch

- Integration: `feature/cluster-distributed`
- Active: `feature/cluster-distributed-p1`

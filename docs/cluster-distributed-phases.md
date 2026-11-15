# Cluster distributed simulation — phases

Confirmed design: Serena `mem:sim-cluster/distributed-design`.

Phases are sized so each can merge after review (foundation-style stop/confirm).
Integration branch: `feature/cluster-distributed`. Merge to `develop` only after all phases land.

| Phase | Branch | Scope | Status |
|-------|--------|-------|--------|
| **p1a** | `feature/cluster-distributed-p1` | Config (`addr`/`port`, `max_nodes`, multicast ranges); `VtClock` slot0 = remote min; `Start { until_ns }`; `cluster_mcast` module (wire + helpers, not arbiter-wired) | merged (#128) |
| **p1b** | `feature/cluster-distributed-p1b-arbiter-mcast` | Arbiter joins multicast when create supplies group; VT publish/throttle; `RemoteVtTable` → slot0; ClusterStop multicast send/recv | merged (#129) |
| **p1c** | `feature/cluster-distributed-p1c-create-place` | Frontend First-Fit, multicast allocate, cross-host edge reject, create fanout + compensate; backend spawns assigned boards only; public create ignores client multicast | this branch |
| **p1d** | `feature/cluster-distributed-p1d-control-agg` | start/stop/reset parallel fanout; status/log aggregate across backends | pending |
| **p2** | `feature/cluster-distributed-p2-endpoint` | Endpoint path `node→arbiter→multicast→arbiter→node`; expand binary kinds | pending |
| **p3** | `feature/cluster-distributed-p3-targeted` | probe/action/inspect only to hosting backend | pending |
| **p4** | `feature/cluster-distributed-p4-csv` | CSV aggregate + time-sort on frontend | pending |

## TODO (provisional until p2)

- Cross-host topology edges: reject at create in p1c; remove after p2 endpoint forwarding.

## p1c notes

- `ClusterFrontend` owns capacity (`max_nodes`) and multicast pool; destroy frees both.
- Shared simulation `id` is chosen by the frontend and sent to each participating backend.
- Until p1d, start/stop/reset/status hit the primary (first) participant only.

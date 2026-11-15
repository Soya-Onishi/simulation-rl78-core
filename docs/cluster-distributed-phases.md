# Cluster distributed simulation — phases

Confirmed design: Serena `mem:sim-cluster/distributed-design`.

Phases are sized so each can merge after review (foundation-style stop/confirm).
Integration branch: `feature/cluster-distributed`. Merge to `develop` only after all phases land.

| Phase | Branch | Scope | Status |
|-------|--------|-------|--------|
| **p1a** | `feature/cluster-distributed-p1` | Config (`addr`/`port`, `max_nodes`, multicast ranges); `VtClock` slot0 = remote min; `Start { until_ns }`; `cluster_mcast` module (wire + helpers, not arbiter-wired) | merged (#128) |
| **p1b** | `feature/cluster-distributed-p1b-arbiter-mcast` | Arbiter joins multicast when create supplies group; VT publish/throttle; `RemoteVtTable` → slot0; ClusterStop multicast send/recv | merged (#129) |
| **p1c** | `feature/cluster-distributed-p1c-create-place` | Frontend First-Fit, multicast allocate, cross-host edge reject, create fanout + compensate; backend spawns assigned boards only; public create ignores client multicast | merged (#130) |
| **p1d** | `feature/cluster-distributed-p1d-control-agg` | start/stop/reset parallel fanout; status/log aggregate across backends | merged (#131) |
| **p2** | `feature/cluster-distributed-p2-endpoint` | Endpoint path `node→arbiter→multicast→arbiter→node`; expand binary kinds | merged (#132) |
| **p3a** | `feature/cluster-distributed-p3a-targeted` | Single-board route: inspect / step / invoke_action / probe_samples → hosting backend | merged (#133) |
| **p3b** | `feature/cluster-distributed-p3b-catalog-watch` | actions / probe_catalog merge; probe_watch_* fanout + merge | this branch |
| **p4** | `feature/cluster-distributed-p4-csv` | CSV aggregate + time-sort on frontend | pending |

## TODO (provisional)

- Cross-host **Digital** edges: still rejected at create until Digital board-edge IPC exists. Uart / Analog / Pulse / Scalar cross-host edges are forwarded in p2.

## p3b notes

- `actions` / `probe_catalog`: fanout + merge (MachineId ascending; first-wins dedupe).
- `probe_watch_put` / `get` / `clear`: partition by board host; clear fans out to all participants.
- `probe_watch_live`: VT cursor (`CoarseLive.cursor_vt`), fetch floor = min VT − 100ms; host-local seq dedupe in `ClusterFrontend`; sim-shared aggregate (reconnect may be briefly empty).
- Spec: `docs/superpowers/specs/2026-11-15-cluster-distributed-p3b-catalog-watch-design.md`.

## p3a notes

- Resolve host by scanning `SimRecord.per_server` with `board_id_hash` (no ledger cache).
- Unknown board hash → `unknown_board` (no `primary_backend` fallback).
- Spec: `docs/superpowers/specs/2026-11-15-cluster-distributed-p3a-targeted-design.md`.

## p2 notes

- Same-host edges remain peer iceoryx. Cross-host: send-host arbiter subscribes, multicast `Endpoint`, recv-host arbiter publishes.
- Wire: `MulticastMsgKind::Endpoint` + `payload_kind` + iceoryx-shaped sample bytes.
- Spec: `docs/superpowers/specs/2026-11-15-cluster-distributed-p2-endpoint-design.md`.

## p1d notes

- start/stop/reset: parallel fanout to `SimRecord.per_server`; partial failure ⇒ overall failure; no auto-rollback.
- status: merge assigned-board nodes; `virtual_time_ns` = min; `running` if any participant is running; omit failed backends.
- log: frontend ring + global `seq` (cap 100); SSE cursor contract unchanged.
- Spec: `docs/superpowers/specs/2026-11-15-cluster-distributed-p1d-design.md`.

## p1c notes

- `ClusterFrontend` owns capacity (`max_nodes`) and multicast pool; destroy frees both.
- Shared simulation `id` is chosen by the frontend and sent to each participating backend.

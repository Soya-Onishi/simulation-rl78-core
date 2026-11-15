# Multi-machine distributed simulation (shared understanding)

Status: **confirmed** (2026-11-14). Do not silently change; re-grill if scope shifts.
Worktree (this chat): `WORKTREE_ID=cluster-ext-899075a9`, path under `~/.cursor/worktrees/cluster-ext-899075a9/`.
Related: `mem:core`, grill session for cluster multicast / VT / phases.

## Deployment
- One `simulation-frontend` for the cluster.
- One `cluster-backend` per physical machine (arbiter thread + local board processes).
- One backend per machine. `MachineId` = IPv4 used for identity, as a newtype over u32 (network octet order). Frontend does **not** assign/notify `MachineId`; each backend derives it.
- `frontend.yaml` `servers[].ip` must match that backend identity IPv4.

## Config
- `frontend.yaml`: `servers[].max_nodes` (required, positive). Multicast address ranges + port ranges required iff `servers.len() >= 2`; optional if exactly one server.
- Range examples: `239.0.0.1-239.0.0.10`, `30000-30100` (multiple ranges allowed).
- `backend.yaml`: top-level `addr` (specific IPv4, not `0.0.0.0`) + `port`. Used for HTTP bind, `machine_id`, and multicast send/join interface (`IP_MULTICAST_IF`). No separate `listen` / `advertise_ip`.
- Phase 1: IPv6 unsupported (error).

## Create
- Frontend First-Fit: walk `boards[]` order; for each board, assign to the first backend with free capacity while iterating a derived `MachineId → free slots` map (`HashMap::iter` order — not YAML `servers[]` order). Free slots = `max_nodes` minus boards assigned in all `SimRecord.per_server` (no separate remaining cache). Capacity is concurrent board processes across all sims on that backend. `ClusterFrontend` keys backends / `per_server` by `MachineId`.
- One node = one topology `boards[]` entry (including plant kinds).
- Payload to each participating backend: full topology JSON, assigned board ids, and (if multicast used) multicast address+port.
- Backend spawns **only** assigned boards; keeps full topology for local edges.
- Multicast addr/port chosen at **create** (not start); free when the sim's `SimRecord` is removed. "Unused" = not present in any live `SimRecord.mcast` (no separate used_mcast cache; assume ranges not used elsewhere; no OS-wide probe).
- Distinct create errors: capacity / multicast exhaustion / duplicate participant IPs / cross-host Digital edge.
- Partial create success → compensate (destroy successes) then fail overall; if compensate fails, surface that remnant may exist.
- **Cross-host edges**: Uart/Analog/Pulse/Scalar allowed (p2 arbiter bridge). **Digital** cross-host still rejected at create (TODO until Digital IPC).


## Virtual time
- Per-host local `VtClock` shm (not cross-machine).
- Slot 0 = min of **other** hosts' reported min board VTs (not until ceiling).
- Local boards remain slots 1..=N.
- Each arbiter tracks per-`machine_id` latest min VT; writes min of others into local slot 0 when that aggregate changes. Exclude self.
- Publish own `min_board_vt` over multicast with `machine_id`.
- Throttle send if: VT advanced by `margin_ns/2` since last send, **OR** wall clock `100ms` since last send (resend even with no VT update).
- `until_ns` via `ControlToNode::Start { until_ns }`; not written to slot 0.
- Node `SetAllowed` effective = `min(allowed_from_VtClock, until_ns.unwrap_or(MAX))` where VtClock allowed ≈ `min(local min board VT, slot0) + margin`.
- Slight overshoot from one-instruction time quantum is OK.
- GDB catch-up: use Start/`until_ns` path; do not `set_ceiling` on slot 0.
- Remote VT table: keep last values if packets stop; drop only when sim ends.

## Cluster Stop
- Any arbiter that decides stop: local `ControlToNode::ClusterStop` + multicast.
- Receivers: apply local ClusterStop to their boards. No leader. Redundant stop is harmless.

## Control / observe (phase 1 / p1d)
- start/stop/reset: frontend fans out in parallel to participating backends; no barrier; partial failure ⇒ overall failure (lowest `MachineId` error); no auto-rollback of successful peers.
- status: merge assigned-board `SessionNode`s from participants (`SimRecord.per_server`); `virtual_time_ns` = min; session `state` = `running` if any participant is `running`; omit backends that return `Err`; all-fail ⇒ error.
- log: frontend owns per-sim global `seq` + ring (cap 100) behind `Arc<Mutex<LogAggregate>>` (serialize pull+ingest per sim only); pull/dedupe per-backend local seq; SSE `log_high_water` / `logs_after` contract unchanged; omit failed/timed-out pulls.
- Spec: `docs/superpowers/specs/2026-11-15-cluster-distributed-p1d-design.md`.

## Endpoint forwarding (p2)
- Path: `board(TX pub) → send-host arbiter(sub) → multicast Endpoint → recv-host arbiter(pub) → board(RX sub)`.
- Same-host edges stay peer iceoryx (arbiter not in path).
- Forwardable kinds: Uart / Analog / Pulse / Scalar. Digital: no board-edge IPC; **create still rejects cross-host Digital** (TODO until Digital IPC).
- Wire: `MulticastMsgKind::Endpoint = 3`; body = `machine_id` BE + `edge_id` LE + `payload_kind` + iceoryx `repr(C)` sample bytes.
- Spec: `docs/superpowers/specs/2026-11-15-cluster-distributed-p2-endpoint-design.md`.

## Multicast wire
- Binary + type enum (not JSON). Kinds: host min VT; ClusterStop; Endpoint (p2).
- Prefer same binary shapes as iceoryx payloads for Endpoint samples.


## Git branches
- Integration: `feature/cluster-distributed` from `develop`.
- Phase branches merge into integration in order; merge integration → `develop` only after all phases complete.
- Phases are **fine-grained** so each PR can stop for review (do not pack a whole original "p1" into one merge).

## Phases (refined)

| Phase | Branch | Scope |
|-------|--------|-------|
| p1a | `feature/cluster-distributed-p1` | Config; VtClock slot0=remote min; Start{until}; `cluster_mcast` helpers (**merged #128**) |
| p1b | `…-p1b-arbiter-mcast` | Arbiter multicast VT + ClusterStop ↔ slot0 (**merged #129**) |
| p1c | `…-p1c-create-place` | First-Fit, mcast allocate, cross-host reject, create fanout/compensate, assigned boards (**merged #130**) |
| p1d | `…-p1d-control-agg` | start/stop/reset fanout; status/log aggregate (**merged #131**) |
| p2 | `…-p2-endpoint` | Endpoint `node→arbiter→multicast→arbiter→node` (**this branch**) |
| p3 | `…-p3-targeted` | probe/action/inspect to hosting backend only |
| p4 | `…-p4-csv` | CSV aggregate + time-sort |

See also `docs/cluster-distributed-phases.md`.

## Out of scope until later phases / re-grill
- Cross-host Digital edges / Digital board-edge IPC.
- Targeted probe/action/inspect routing details (p3).
- CSV merge/sort details (p4).


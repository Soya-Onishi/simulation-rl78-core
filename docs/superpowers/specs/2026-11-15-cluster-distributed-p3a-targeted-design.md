# Cluster distributed p3a — single-board hosting routing

Status: **approved** (implementation on `feature/cluster-distributed-p3a-targeted`).  
Branch: `feature/cluster-distributed-p3a-targeted` (from integration `feature/cluster-distributed`).  
Confirmed baseline: Serena `mem:sim-cluster/distributed-design`, `docs/cluster-distributed-phases.md`, p2 merged (#132).

## Intent

After multi-host create (p1c), boards of one simulation may live on different `cluster-backend` processes. Several frontend APIs name **one** board (`HashedId`). Today `ClusterFrontend` sends those calls to `primary_backend` (an arbitrary participating `MachineId`), so a board on another host never receives them. p3a routes each such call to the backend that hosts that board.

p3 is split: **p3a** = single-board route; **p3b** = sim-wide catalog / action list merge and `probe_watch_*` fanout+merge (later).

## Goals

- Route these `ClusterFrontend` / `SimulationBackend` methods to the hosting backend only:
  - `inspect(id, board_id, req)`
  - `step(id, board_id)`
  - `invoke_action(id, request)` using `request.board_id`
  - `probe_samples(id, board, probe, from_ns, to_ns)` using `board`
- Resolve host from live `SimRecord.per_server` without changing public HTTP JSON shapes.
- Unknown board hash (not in any participant assignment) → clear client error (not silent primary fallback).

## Non-goals

- p3b: `actions`, `probe_catalog`, `probe_watch_get` / `put` / `clear` / `live` merge or fanout.
- p4: CSV start/poll/status aggregate.
- Viewer attach/check/detach (remain interim `primary_backend`).
- Topology fetch (may stay on one participant; full topology is on every backend after create).
- Changing how a single backend maps `board_id` to a local node (already correct once the right backend is chosen).
- Caching `HashedId → MachineId` on `SimRecord` (Approach B; deferred unless profiling demands it).

## Anti-goals

- Fanout of single-board RPCs to every participant “and pick a success”.
- Falling back to `primary_backend` when the board is not in `per_server` (hides mis-routing).

## Constraints

- Placement source of truth: `SimRecord.per_server: HashMap<MachineId, Vec<String>>` where values are topology board id **strings** assigned at create.
- Public board ids on these APIs are `HashedId<u64>` = `board_id_hash(board_id_string)` (same as status nodes / probe refs).
- Lookup: for each `(machine_id, boards)` in `per_server`, if any `board_id_hash(b) == target`, use `self.servers[machine_id].backend`. Linear in total assigned boards (small).
- Simulation missing from ledger: keep existing `primary_backend` “probe servers for exists” behavior only for APIs that still use primary; for p3a helper, if sim is unknown → `not_found`; do not invent placement after restart without ledger (same as other ledger-based APIs). Builder call: if sim absent from `state.sims`, return `not_found` rather than scanning backends for a board (board placement is unknown without `SimRecord`).
- Error when sim exists but board hash matches no assignment: `ApiError::bad_request` or `not_found` with a stable code such as `unknown_board` — builder picks code consistent with nearby APIs; message must mention the board is not assigned to any participant.

## Approaches

| | Idea | Tradeoff |
|---|------|----------|
| **A (chosen)** | Resolve `HashedId → backend` by scanning `per_server` + `board_id_hash` at call time | Simple; no ledger shape change |
| B | Cache map on `SimRecord` at create | Faster lookup; more state to maintain |
| C | Fanout to all participants | Wasteful; ambiguous errors |

## Behavior

1. Add a private helper on `ClusterFrontend`, e.g. `backend_for_board(sim_id, board: HashedId<u64>) -> Result<Arc<dyn SimulationBackend>, ApiError>`.
2. Replace `primary_backend` with that helper in the four methods listed under Goals; remove or retarget their `TODO(cluster-distributed-p3)` comments to note p3a done / p3b remaining where relevant.
3. Leave `primary_backend` for catalog, watch, csv, viewer, topology until later phases.
4. Unit tests with two mock backends and a two-board placement: call `inspect`/`step`/`invoke_action`/`probe_samples` with board on server B and assert only B’s mock receives the call; unknown board hash returns the chosen error code.

## Docs / memory after merge

- Split p3 row in `docs/cluster-distributed-phases.md` into p3a / p3b; mark p2 merged (#132).
- Update Serena `mem:sim-cluster/distributed-design` phases table accordingly.

## What’s left to the builder

- Exact helper name and whether it returns `(MachineId, Arc<dyn SimulationBackend>)` or backend only.
- Exact `ApiError` code/string for unknown board.
- Test placement of helpers (inline in `cluster_frontend_tests.rs` vs small unit on a pure function `machine_for_board(per_server, hash)` extracted for easy testing).

## Builder calls settled in this revision

- **Intentional intermediate:** After p3a alone, `actions` / `probe_catalog` / `probe_watch_*` still use `primary_backend` (multi-host lists remain incomplete until p3b). Accepted by product split p3a→p3b.
- **Branch:** `feature/cluster-distributed-p3a-targeted` from current integration `feature/cluster-distributed` (p2 already merged as #132).
- **Verification bar:** Mock two-backend unit tests in `cluster_frontend_tests` are sufficient for merge; no multi-process HTTP smoke required in p3a.

## Open decisions closed in this doc

- Scope: p3a single-board APIs only; p3b later.
- Architecture: Approach A (scan `per_server` at call time).

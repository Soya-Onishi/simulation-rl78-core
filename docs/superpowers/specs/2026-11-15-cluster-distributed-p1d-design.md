# Cluster distributed p1d — control fanout + status/log aggregate

Status: **approved** (implementation on `feature/cluster-distributed-p1d-control-agg`).  
Branch: `feature/cluster-distributed-p1d-control-agg` (from `feature/cluster-distributed`).  
Confirmed baseline: Serena `mem:sim-cluster/distributed-design`, `docs/cluster-distributed-phases.md`.

## Intent

After p1c place/create, a simulation may span multiple `cluster-backend` processes. The single `simulation-frontend` must (1) send start/stop/reset to every participating backend in parallel, and (2) present one status snapshot and one log stream to HTTP/SSE clients without changing those wire types.

## Goals

- **Control**: `start` / `stop` / `reset` fan out in parallel to every `MachineId` in `SimRecord.per_server`. No barrier. Any backend `Err` ⇒ overall `Err` (first error after all calls finish). No automatic rollback of backends that succeeded.
- **Status**: Parallel fetch from participants; merge into one `StatusEvent`. A backend that returns `Err` is omitted. If at least one status succeeds, return the merge. Only if every participant fails (or there are no participants) return `Err`.
- **Log**: Frontend owns a per-simulation global `seq` space and a ring buffer so existing SSE (`log_high_water` then `logs_after(after)` advancing `after = max(seq)`) keeps working. A backend that returns `Err` on a pull is skipped for that pull.

## Non-goals

- Cross-host endpoint forwarding (p2).
- probe / action / inspect hosting-only routing (p3).
- CSV aggregate / time-sort (p4).
- Changing `StatusEvent` / `LogEvent` / `SessionNode` JSON shapes or the UI.
- Redesigning create / shutdown / capacity / multicast.

## Anti-goals

- Exposing each backend’s native log `seq` to clients (breaks the single-cursor SSE model).
- Concatenating full arbiter `nodes` lists so other hosts’ boards stay `"unknown"`.

## Constraints

- Participation set = keys of `SimRecord.per_server: HashMap<MachineId, Vec<String>>` where values are topology board id strings assigned to that machine. Backend lookup = `ClusterFrontend.servers[machine_id].backend`.
- Each local arbiter publishes status over the full topology; boards not in its peer set appear as `SessionNode.state == "unknown"`. Local VT is that host’s min board VT.
- Each backend assigns log `seq` independently (`Simulation.next_seq` in `server.rs`). `logs_after(id, after)` means `seq > after`. High-water is `next_seq - 1` (or `0` if none).
- Server log ring capacity is `LOG_CAP = 100`; frontend aggregate ring uses the same cap.
- `ClusterFrontend.state: Mutex<FrontendState>` already serializes ledger access; log aggregate updates go under the same mutex (builder choice: field on `FrontendState` vs beside `SimRecord`).

## Approaches

| | Idea | Tradeoff |
|---|------|----------|
| **A (chosen)** | Frontend merges status; owns log ring + global seq | Extra frontend state; keeps public contract |
| B | Pack `(machine_id, local_seq)` into one `u64` seq | Sparse/opaque seq; fragile |
| C | Expose per-backend cursors / pre-merge status to clients | API + UI change in p1d |

## Control fanout

1. Resolve participant list from `SimRecord.per_server` (clone keys under the mutex, then drop the guard before I/O).
2. For each `MachineId`, call the matching `SimulationBackend` method with the same simulation `id` (and `until_ns` for start).
3. Run calls in parallel (builder picks `std::thread::scope` or an existing pattern in-tree).
4. Wait for all. If any `Err`, return one of them (stable preference: lowest `MachineId` among failures — builder call). Successes are left as-is (no compensate).

Single-participant sims behave like today (one call).

## Status merge

Types (unchanged):

- `StatusEvent { id, state, virtual_time_ns, stop_reason, nodes: Vec<SessionNode> }`
- `SessionNode { id: HashedId<u64>, kind, state }` — `id` is `board_id_hash(board_id_string)`.

Algorithm given successful fetches `results: Vec<(MachineId, StatusEvent)>` and `per_server`:

1. `id` = request simulation id.
2. `nodes`: for each `(machine_id, boards)` in `per_server` iteration order, if that `machine_id` has a successful status, take each assigned board id string `b`, find the `SessionNode` whose `id == board_id_hash(b)` in that status’s `nodes`, and append it. Skip missing hashes (should not happen). Do not copy nodes for boards not in that machine’s assignment list (drops `"unknown"` rows for remote boards).
3. `virtual_time_ns` = `min` of successful `StatusEvent.virtual_time_ns` values.
4. `state` = `"running"` if any successful status has `state == "running"`, else `"stopped"`. (Arbiter only emits these two for the session-level field.)
5. `stop_reason` = if merged `state == "running"` then `None`; else first `Some` among successful statuses in the same `MachineId` iteration order as `per_server`.

**Partial failure (observe):** any `Err` from `backend.status` = omit that machine for this call. No retry inside the call.

**Same board on two machines:** impossible by p1c placement; not handled.

## Log aggregate

Per simulation id, frontend keeps:

```text
LogAggregate {
  per_backend_after: HashMap<MachineId, u64>,  // last ingested local seq; missing key ⇒ 0
  next_seq: u64,                               // next global seq to assign; start at 1
  buffer: VecDeque<LogEvent>,                  // global seq already assigned
}
```

Lifecycle:

- Create successful (ledger insert committed and all backend creates ok) → insert empty `LogAggregate`.
- `rollback_ledger` / sim removed after shutdown → drop aggregate with the `SimRecord`.

`logs_after(id, after)`:

1. Under mutex: read participant set + current `per_backend_after` snapshots; drop guard.
2. For each participant, `backend.logs_after(id, after_m)` where `after_m` is the snapshot (default `0`). On `Err`, skip that machine.
3. Under mutex again: for each machine in participant iteration order, for each returned event in vector order, if `event.seq > per_backend_after[m]` then append to `buffer` with `seq = next_seq` (other fields copied), bump `next_seq`, set `per_backend_after[m] = max(per_backend_after[m], event.seq)`. Trim `buffer` to `LOG_CAP` from the front.
4. Return clones of buffer entries with `seq > after` (do not pop).

`log_high_water(id)` = pull+ingest from participants (same as a `logs_after` pull), then return `next_seq.saturating_sub(1)`. This matches single-backend SSE subscribe: connection starts “from now,” without replaying backlog already sitting on backends.

Concurrent `log_high_water` / `logs_after` share a `log_fetch` mutex around the whole pull+ingest so one caller cannot advance global seq under another’s in-flight subscribe.

Status and log fanout use a bounded wait (`OBSERVE_FANOUT_TIMEOUT`, 2s): timed-out participants are treated as `Err` and omitted. Control fanout still waits for every participant.

**Assumptions (local backend):** local seq is strictly increasing per append; `logs_after` may resend seqs already seen — step 3’s `event.seq > per_backend_after[m]` dedupes. Gaps: advance only on ingested seqs (skip hole until it appears or is never filled).

**Concurrency:** multiple SSE clients share one aggregate; mutex serializes ingest + read. Pulls that raced may both fetch the same backend batch; dedupe by local seq prevents double-append.

**Partial failure (observe):** skip failed backend for that pull; other machines still ingest.

## Out of scope call sites

Leave `TODO(cluster-distributed-p3|p4)` on probe/action/inspect/csv/`primary_backend` users unchanged. `topology` may stay on one participant (full topology is on every backend after create).

## Tests (minimum)

- Multi-backend start/stop/reset: all participants called; one failure ⇒ overall error; successes not rolled back.
- Status merge: two backends with disjoint assigned boards; merged nodes are only assigned ones; VT is min; running wins.
- Status: one backend `Err` ⇒ still Ok with the other.
- Log: two backends with overlapping local seq spaces; client sees monotonic global seq; `logs_after` / high-water match SSE cursor rules; ring trims at 100.
- Log: one backend `Err` on pull ⇒ other backend’s new lines still appear.

## Left to the builder

- Exact parallel primitive.
- Whether `LogAggregate` lives inside `SimRecord` or a parallel map in `FrontendState`.
- Error message text / which failed `MachineId` is surfaced when several fail.
- Mock backend harness details in unit tests.

## Builder calls (settled here, not re-grilled)

- Failure preference on control: lowest `MachineId` among `Err` results.
- Observe “missing” = method returned `Err` (timeouts surface as `Err` from the HTTP client); empty `Ok(vec![])` is success with no new logs.
- Status `state` strings compared as exact `"running"` / otherwise treated as not running for the “any running” rule.

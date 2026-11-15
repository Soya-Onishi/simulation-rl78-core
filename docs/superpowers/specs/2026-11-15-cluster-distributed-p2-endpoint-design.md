# Cluster distributed p2 — cross-host endpoint forwarding

Status: **approved** (implementation on `feature/cluster-distributed-p2-endpoint`).  
Branch: `feature/cluster-distributed-p2-endpoint` (from integration `feature/cluster-distributed`).  
Confirmed baseline: Serena `mem:sim-cluster/distributed-design`, `docs/cluster-distributed-phases.md`, p1d merged (#131).  
Product answer (2026-11-15): cross-host `Digital` edges stay **rejected at create**, marked TODO until Digital board-edge IPC exists.

## Intent

A simulation may place boards on different `cluster-backend` hosts (p1c). Same-host board edges already move samples over iceoryx publish/subscribe. Cross-host edges cannot use iceoryx across machines and are rejected at create today. p2 removes that reject and forwards samples along:

`board(TX pub) → send-host arbiter(sub) → cluster multicast → recv-host arbiter(pub) → board(RX sub)`.

## Goals

- Allow create of topologies whose directed edges connect boards on different backends.
- Forward **Uart**, **Analog**, **Pulse**, and **Scalar** samples for those cross-host edges end-to-end.
- Keep same-host edges on the existing peer iceoryx path (no arbiter hop).
- Keep board-process data-plane APIs (`open` / `bind` / `pump_tx` / `pump_rx`) unchanged for both same-host and cross-host ends: a TX board still opens an iceoryx publisher; an RX board still opens a subscriber on the same edge service name string used today.
- Extend the existing per-simulation multicast binary protocol (same magic/version framing as VT / ClusterStop) with endpoint sample messages whose payload bytes match the iceoryx `repr(C)` layouts (`UartFrame`, `AnalogVoltage`, `Pulse`, `Scalar`).

## Non-goals

- `PayloadKind::Digital` board-edge IPC (still out of scope in `data_plane`; use Pulse for trains).
- Changing reliability to TCP / ACK / retransmission (multicast stays best-effort UDP, same class as VT notifications).
- Replacing same-host iceoryx with arbiter-mediated paths.
- Frontend HTTP/SSE contract changes; probe/action/inspect hosting-only routing (p3); CSV aggregate (p4).
- Guaranteeing sample delivery under packet loss or reordering.

## Anti-goals

- Requiring board processes to learn `MachineId` placement or to open a second “remote” send path.
- Putting the frontend in the sample path.
- One multicast message type per bit of UART framing or per GPIO toggle (samples stay the existing payload structs).

## Constraints

- Participation and placement: frontend assigns boards via First-Fit into `SimRecord.per_server`; each backend create receives full topology JSON + `assigned_boards` + (when multi-host) multicast group. Arbiter local set = `assigned_boards` (validated against topology). A board id not in the local set is **remote** for that arbiter.
- Edge identity: `edge_id` = existing `xxh3_64` of `{from_board}:{from_ep}->{to_board}:{to_ep}` (`sim-cluster` `ipc::names::edge_id`). Iceoryx service names stay `uart_edge` / `analog_edge` / `pulse_edge` / `scalar_edge` helpers keyed by `cluster_key` + that id.
- Iceoryx is process-local to one machine: the TX board’s publisher exists only on the send host; the RX board’s subscriber exists only on the recv host. The send-host arbiter must open the complementary **subscriber**; the recv-host arbiter must open the complementary **publisher**, using the same service name and ring capacity rules as `Node*Ports::open_for_board` today (UART uses `LogicalTopology::uart_ring_len`; other kinds use their existing defaults).
- Multicast is already joined per simulation when `multicast` is present on create (`open_mcast_plane` in `arbiter.rs`). Endpoint forwards use that socket only (no new UDP group).
- Self-delivery: UDP multicast may loop back. Endpoint messages carry the sender `MachineId`; an arbiter **ignores** Endpoint messages whose `machine_id` equals its own (local delivery already happened via iceoryx on the TX side; the RX side for a cross-host edge is never local to the sender).
- Drop / loss: if multicast or iceoryx rings drop a sample, that sample is lost. No p2 retry. Same-host behavior for ring-full remains whatever `try_send` / subscriber drain already do.

## Approaches

| | Idea | Tradeoff |
|---|------|----------|
| **A (chosen)** | Arbiter bridge on cross-host edges only; boards unchanged | Arbiter opens half of each cross-host edge; matches design-memo path |
| B | Dedicated n2a/a2n iceoryx bus for remote samples | Board data_plane gains same-host vs cross-host branches |
| C | Route every edge through arbiter | Rewrites working same-host path; larger than p2 need |

## Classification of edges (per arbiter)

For each `DirectedEdge` in the topology with payload in `{Uart, Analog, Pulse, Scalar}`:

| `from` board | `to` board | Action |
|--------------|------------|--------|
| local | local | None (nodes peer over iceoryx) |
| local | remote | **Egress bridge**: subscribe iceoryx; each sample → multicast Endpoint |
| remote | local | **Ingress bridge**: on Endpoint for this `edge_id`, publish iceoryx |
| remote | remote | None |

`Digital` edges are skipped. Unknown / malformed multicast Endpoint payloads are logged and dropped (do not stop the sim).

## Multicast wire

Reuse existing frame: magic + version + kind byte + body (see `cluster_mcast.rs`).

Add:

```text
MulticastMsgKind::Endpoint = 3

Body (little-endian multi-byte fields unless noted):
  machine_id: u32   // network octet order, same as VtMin / ClusterStop
  edge_id:    u64   // LE; same value as iceoryx edge HashedId
  payload_kind: u8  // 1=Uart, 2=Analog, 3=Pulse, 4=Scalar (Digital unused)
  payload:    N bytes  // exact iceoryx sample bytes for that kind
```

`N` is fixed per kind and matches the `repr(C)` **size** of `UartFrame` (24) / `AnalogVoltage` (8) / `Pulse` (16) / `Scalar` (8). Bodies are encoded **field-wise little-endian with zero padding** (not a raw `memcpy` of the in-memory object), so padding never leaks and enum/bool fields are validated on decode (`UartStopBits` ∈ {0,1}, `UartParity` ∈ {0,1,2}, `inverted` ∈ {0,1}). Reject if length ≠ expected or validation fails.

Builder call (recorded here): **one** `MulticastMsgKind::Endpoint` with a `payload_kind` tag, not four separate kind enum values — keeps VT/ClusterStop numbering sparse and matches “binary shapes like iceoryx payloads.”

## Arbiter runtime

1. After local boards are known and multicast plane is open (multi-host sims), build egress/ingress bridge tables from topology + local set.
2. Each arbiter loop iteration (alongside existing `poll_mcast_plane` / control / VT publish):
   - Drain each egress iceoryx subscriber (bounded batch; builder picks limit, default same order as other drains e.g. 256).
   - For each sample, `send` `MulticastMessage::Endpoint { ... }` on the sim’s `ClusterMulticast`.
   - In `poll_mcast_plane` (or sibling), decode Endpoint; skip self `machine_id`; look up ingress pub by `edge_id`; `try_send` sample; on missing pub or kind mismatch, log + drop.
3. Single-host sims (no multicast) open no bridges (no cross-host edges possible without a second participant).

## Create path

- Stop rejecting cross-host edges whose `payload` is **Uart / Analog / Pulse / Scalar** (those are forwarded in p2).
- **TODO (provisional):** still reject create when a directed edge has `PayloadKind::Digital` and its endpoint boards land on different backends. Reason: Digital has no board-edge IPC yet; silent create-success would leave a dead cross-host edge. Error code may stay `cross_host_edge` or become a Digital-specific code — builder call; message should mention Digital / TODO.
- Replace the current “any cross-host edge ⇒ reject” frontend test with: (1) cross-host Uart (or other forwarded kind) **succeeds**; (2) cross-host Digital **still fails**.
- Keep or slim `has_cross_host_edge` / add a Digital-specific helper — builder call.
- Multi-host create still requires multicast allocation (unchanged p1c rule).

## Builder calls settled in this revision

- **Bridge iceoryx open failure:** treat like any other arbiter IPC setup failure — fail that host’s simulation create (so frontend fanout/compensate can run). Do not start a sim with a silently dead cross-host edge.
- **Egress rate:** no extra throttle beyond the arbiter drain batch; best-effort send like VT multicast. Drop on UDP/iceoryx pressure is acceptable for p2.
- **Backend version mix:** out of scope. Participating backends for a multi-host sim are expected to run the same p2-capable binary; unknown `MulticastMsgKind` continues to drop that datagram.

## Tests (minimum)

- `cluster_mcast`: Endpoint encode/decode round-trip per payload kind; reject truncated / wrong-size / unknown kind; self-ignore is arbiter logic (unit-test the ignore predicate if extracted).
- Bridge classification: given local set + edges, expected egress vs ingress edge ids.
- Frontend: create with a two-server placement and a cross-host UART (or Pulse) edge **succeeds** (replace current `cross_host_edge_rejected`).
- Optional in-process: two iceoryx nodes + a loopback or mock forwarder proving TX pub → sub bridge → pub → RX sub for one kind (builder call if harness cost is high; at least one kind required if feasible without multi-process CI).

## Docs / memory after merge

- Update `docs/cluster-distributed-phases.md`: p2 branch status; remove “provisional reject until p2” TODO.
- Update Serena `mem:sim-cluster/distributed-design`: p2 done scope; cross-host edges allowed; Endpoint kind on wire.

## What’s left to the builder

- Module layout (e.g. `endpoint_bridge.rs` vs methods on arbiter).
- Exact drain batch sizes and log wording.
- Whether to delete or keep `has_cross_host_edge` after reject removal.
- Depth of optional integration harness beyond unit tests above.
- `payload_kind` numeric assignments if a cleaner packing appears during impl — must stay documented in this spec or an amendment before merge.

## Open decisions closed in this doc

- Scope: all iceoryx-backed payload kinds (not UART-only); Digital excluded from forwarding.
- Architecture: Approach A (arbiter bridge).
- Wire: single `Endpoint` kind + `payload_kind` + raw sample bytes.
- Reliability: best-effort; loss OK for p2.
- Create: cross-host Digital remains rejected (TODO) until Digital IPC exists; other kinds allowed.

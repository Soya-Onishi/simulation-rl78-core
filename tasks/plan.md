# p4 CSV aggregate — implementation plan

Spec: `docs/superpowers/specs/2026-11-15-cluster-distributed-p4-csv-design.md`

## Tasks

1. **Wire `request_id` into csv_start** — `SimulationBackend`, `LocalBackend`/`JobKind`, `server::start_csv`, internal HTTP body, `TcpipBackend`, all mocks.
2. **ClusterFrontend aggregate** — issue `request_id`, fanout start, merge probes; `CsvMergeState` + k-way poll; status flags; intentional no-cancel comment.
3. **Tests** — multi-mock: shared request_id, probes merge, cross-poll time order, flags.
4. **Docs** — `docs/cluster-distributed-phases.md`, `mem:sim-cluster/distributed-design`.
5. **Verify** — `cargo fmt`, `cargo test -p sim-cluster --lib`.

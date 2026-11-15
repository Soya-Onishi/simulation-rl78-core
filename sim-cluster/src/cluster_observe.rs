//! Status merge and frontend log / probe-watch aggregate for multi-backend sims.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::config::MachineId;
use crate::contract::{LogEvent, StatusEvent};
use crate::host::SessionNode;
use crate::probe_log::CoarseBatch;

/// Same capacity as `server::LOG_CAP` — frontend remapped log ring.
pub(crate) const LOG_CAP: usize = 100;

/// Per-simulation remapped log stream owned by `ClusterFrontend`.
pub(crate) struct LogAggregate {
    /// Last ingested local seq per backend (`0` = none yet).
    per_backend_after: HashMap<MachineId, u64>,
    /// Next global seq to assign (1-based, matching backend `next_seq`).
    next_seq: u64,
    buffer: VecDeque<LogEvent>,
    /// Subscribe could not learn a backend high-water; first successful pull
    /// must advance the cursor over that backlog without emitting lines.
    baseline_pending: HashSet<MachineId>,
}

impl LogAggregate {
    pub(crate) fn new() -> Self {
        Self {
            per_backend_after: HashMap::new(),
            next_seq: 1,
            buffer: VecDeque::new(),
            baseline_pending: HashSet::new(),
        }
    }

    pub(crate) fn high_water(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    pub(crate) fn after_for(&self, machine_id: MachineId) -> u64 {
        self.per_backend_after
            .get(&machine_id)
            .copied()
            .unwrap_or(0)
    }

    /// Skip local seqs up to `local_hw` without ingesting lines (subscribe-time
    /// omit of backlog on a backend that failed the pull).
    pub(crate) fn advance_backend_cursor(&mut self, machine_id: MachineId, local_hw: u64) {
        let cur = self.after_for(machine_id);
        if local_hw > cur {
            self.per_backend_after.insert(machine_id, local_hw);
        }
        self.baseline_pending.remove(&machine_id);
    }

    pub(crate) fn mark_baseline_pending(&mut self, machine_id: MachineId) {
        self.baseline_pending.insert(machine_id);
    }

    /// If this backend still needs a subscribe baseline, consume the flag and
    /// return true so the caller can skip the first recovered batch.
    pub(crate) fn take_baseline_pending(&mut self, machine_id: MachineId) -> bool {
        self.baseline_pending.remove(&machine_id)
    }

    /// Append newly fetched backend batches. `batches` must already be in the
    /// stable participant order used for global seq assignment.
    pub(crate) fn ingest(&mut self, batches: Vec<(MachineId, Vec<LogEvent>)>) {
        for (machine_id, events) in batches {
            let after = self.after_for(machine_id);
            let mut max_seen = after;
            for event in events {
                // Strictly advancing local seq: skips resends and in-batch dups.
                if event.seq <= max_seen {
                    continue;
                }
                max_seen = event.seq;
                let global = LogEvent {
                    seq: self.next_seq,
                    level: event.level,
                    origin: event.origin,
                    board: event.board,
                    text: event.text,
                };
                self.next_seq = self.next_seq.saturating_add(1);
                self.buffer.push_back(global);
                while self.buffer.len() > LOG_CAP {
                    self.buffer.pop_front();
                }
            }
            if max_seen > after {
                self.per_backend_after.insert(machine_id, max_seen);
            }
        }
    }

    pub(crate) fn after(&self, after: u64) -> Vec<LogEvent> {
        self.buffer
            .iter()
            .filter(|event| event.seq > after)
            .cloned()
            .collect()
    }
}

/// Per-simulation host-local coarse-batch seq cursors for live watch merge (p3b).
pub(crate) struct ProbeWatchAggregate {
    /// Last emitted local `CoarseBatch.seq` per backend (`0` = none yet).
    per_backend_seq: HashMap<MachineId, u64>,
}

impl ProbeWatchAggregate {
    pub(crate) fn new() -> Self {
        Self {
            per_backend_seq: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.per_backend_seq.clear();
    }

    /// Keep batches with `seq >` last emitted for `machine_id`; advance cursor.
    pub(crate) fn take_new(
        &mut self,
        machine_id: MachineId,
        batches: Vec<CoarseBatch>,
    ) -> Vec<CoarseBatch> {
        let after = self.per_backend_seq.get(&machine_id).copied().unwrap_or(0);
        let mut out = Vec::new();
        let mut max_seen = after;
        for batch in batches {
            if batch.seq <= after {
                continue;
            }
            max_seen = max_seen.max(batch.seq);
            out.push(batch);
        }
        if max_seen > after {
            self.per_backend_seq.insert(machine_id, max_seen);
        }
        out
    }
}

/// Merge successful participant statuses into one client-facing snapshot.
///
/// `machine_order` is the stable iteration order (ascending `MachineId`).
pub(crate) fn merge_status(
    id: &str,
    per_server: &HashMap<MachineId, Vec<String>>,
    machine_order: &[MachineId],
    results: &[(MachineId, StatusEvent)],
) -> StatusEvent {
    let by_machine: HashMap<MachineId, &StatusEvent> =
        results.iter().map(|(m, s)| (*m, s)).collect();
    let mut nodes = Vec::new();
    for machine_id in machine_order {
        let Some(boards) = per_server.get(machine_id) else {
            continue;
        };
        let Some(status) = by_machine.get(machine_id) else {
            continue;
        };
        let index: HashMap<_, _> = status.nodes.iter().map(|n| (n.id, n)).collect();
        for board in boards {
            let hash = crate::ipc::board_id_hash(board);
            if let Some(node) = index.get(&hash) {
                nodes.push(SessionNode {
                    id: node.id,
                    kind: node.kind.clone(),
                    state: node.state.clone(),
                });
            }
        }
    }
    let virtual_time_ns = results
        .iter()
        .map(|(_, s)| s.virtual_time_ns)
        .min()
        .unwrap_or(0);
    let running = results.iter().any(|(_, s)| s.state == "running");
    let state = if running {
        "running".to_string()
    } else {
        "stopped".to_string()
    };
    let stop_reason = if running {
        None
    } else {
        machine_order.iter().find_map(|machine_id| {
            by_machine
                .get(machine_id)
                .and_then(|s| s.stop_reason.clone())
        })
    };
    StatusEvent {
        id: id.to_string(),
        state,
        virtual_time_ns,
        stop_reason,
        nodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(board: &str, state: &str) -> SessionNode {
        SessionNode {
            id: crate::ipc::board_id_hash(board),
            kind: "rl78".into(),
            state: state.into(),
        }
    }

    fn status(
        id: &str,
        state: &str,
        vt: u64,
        stop_reason: Option<&str>,
        nodes: Vec<SessionNode>,
    ) -> StatusEvent {
        StatusEvent {
            id: id.into(),
            state: state.into(),
            virtual_time_ns: vt,
            stop_reason: stop_reason.map(str::to_string),
            nodes,
        }
    }

    #[test]
    fn merge_takes_assigned_nodes_min_vt_and_running() {
        let m0 = MachineId::from_u32(1);
        let m1 = MachineId::from_u32(2);
        let per_server = HashMap::from([(m0, vec!["a".into()]), (m1, vec!["b".into()])]);
        let order = [m0, m1];
        let results = vec![
            (
                m0,
                status(
                    "sim",
                    "running",
                    100,
                    None,
                    vec![node("a", "running"), node("b", "unknown")],
                ),
            ),
            (
                m1,
                status(
                    "sim",
                    "stopped",
                    40,
                    Some("breakpoint"),
                    vec![node("b", "stopped")],
                ),
            ),
        ];
        let merged = merge_status("sim", &per_server, &order, &results);
        assert_eq!(merged.state, "running");
        assert_eq!(merged.virtual_time_ns, 40);
        assert!(merged.stop_reason.is_none());
        assert_eq!(merged.nodes.len(), 2);
        assert!(!merged.nodes.iter().any(|n| n.state == "unknown"));
    }

    #[test]
    fn merge_stop_reason_uses_machine_order_when_all_stopped() {
        let m0 = MachineId::from_u32(1);
        let m1 = MachineId::from_u32(2);
        let per_server = HashMap::from([(m0, vec!["a".into()]), (m1, vec!["b".into()])]);
        let order = [m0, m1];
        let results = vec![
            (
                m0,
                status("sim", "stopped", 1, None, vec![node("a", "stopped")]),
            ),
            (
                m1,
                status(
                    "sim",
                    "stopped",
                    2,
                    Some("breakpoint"),
                    vec![node("b", "stopped")],
                ),
            ),
        ];
        let merged = merge_status("sim", &per_server, &order, &results);
        assert_eq!(merged.state, "stopped");
        assert_eq!(merged.stop_reason.as_deref(), Some("breakpoint"));
    }

    #[test]
    fn ingest_dedupes_local_seq_assigns_global_and_trims() {
        let mut agg = LogAggregate::new();
        let m0 = MachineId::from_u32(1);
        let m1 = MachineId::from_u32(2);
        let mk = |seq: u64, text: &str| LogEvent {
            seq,
            level: "info".into(),
            origin: "t".into(),
            board: None,
            text: text.into(),
        };
        agg.ingest(vec![
            (m0, vec![mk(1, "a1"), mk(1, "dup"), mk(2, "a2")]),
            (m1, vec![mk(1, "b1")]),
        ]);
        assert_eq!(agg.high_water(), 3);
        assert_eq!(agg.after(0).len(), 3);
        assert_eq!(agg.after(0)[0].text, "a1");
        assert_eq!(agg.after(0)[1].text, "a2");
        assert_eq!(agg.after(0)[2].text, "b1");

        // Resend of already-ingested local seqs is ignored.
        agg.ingest(vec![(m0, vec![mk(1, "a1"), mk(2, "a2")])]);
        assert_eq!(agg.high_water(), 3);

        for i in 0..LOG_CAP {
            agg.ingest(vec![(m0, vec![mk(3 + i as u64, &format!("x{i}"))])]);
        }
        assert_eq!(agg.buffer.len(), LOG_CAP);
        assert_eq!(agg.after(0).first().unwrap().text, "x0");
    }

    #[test]
    fn advance_backend_cursor_skips_without_ingest() {
        let mut agg = LogAggregate::new();
        let m0 = MachineId::from_u32(1);
        agg.advance_backend_cursor(m0, 5);
        assert_eq!(agg.after_for(m0), 5);
        assert_eq!(agg.high_water(), 0);
        let mk = |seq: u64| LogEvent {
            seq,
            level: "info".into(),
            origin: "t".into(),
            board: None,
            text: format!("s{seq}"),
        };
        agg.ingest(vec![(m0, vec![mk(3), mk(5), mk(6)])]);
        assert_eq!(agg.after(0).len(), 1);
        assert_eq!(agg.after(0)[0].text, "s6");
    }

    #[test]
    fn baseline_pending_clears_on_advance() {
        let mut agg = LogAggregate::new();
        let m0 = MachineId::from_u32(1);
        agg.mark_baseline_pending(m0);
        assert!(agg.take_baseline_pending(m0));
        assert!(!agg.take_baseline_pending(m0));
        agg.mark_baseline_pending(m0);
        agg.advance_backend_cursor(m0, 3);
        assert!(!agg.take_baseline_pending(m0));
    }
}

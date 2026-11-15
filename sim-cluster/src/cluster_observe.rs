//! Status merge and frontend log aggregate for multi-backend simulations (p1d).

use std::collections::{HashMap, VecDeque};

use crate::config::MachineId;
use crate::contract::{LogEvent, StatusEvent};
use crate::host::SessionNode;

/// Same capacity as `server::LOG_CAP` — frontend remapped log ring.
pub(crate) const LOG_CAP: usize = 100;

/// Per-simulation remapped log stream owned by `ClusterFrontend`.
pub(crate) struct LogAggregate {
    /// Last ingested local seq per backend (`0` = none yet).
    per_backend_after: HashMap<MachineId, u64>,
    /// Next global seq to assign (1-based, matching backend `next_seq`).
    next_seq: u64,
    buffer: VecDeque<LogEvent>,
}

impl LogAggregate {
    pub(crate) fn new() -> Self {
        Self {
            per_backend_after: HashMap::new(),
            next_seq: 1,
            buffer: VecDeque::new(),
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
}

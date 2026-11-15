//! Unit tests for [`crate::cluster_frontend`].

use super::*;
use crate::host::SessionNode;
use crate::topology::{BoardSpec, DirectedEdge, PayloadKind, PortRef};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;

struct MockBackend {
    name: String,
    creates: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    resets: AtomicUsize,
    fail_create: bool,
    fail_start: AtomicBool,
    fail_status: AtomicBool,
    fail_logs: AtomicBool,
    /// Remaining times `shutdown` should return an error before removing.
    shutdown_failures: AtomicUsize,
    live: Mutex<HashMap<String, Vec<String>>>,
    session_state: Mutex<String>,
    virtual_time_ns: AtomicU64,
    stop_reason: Mutex<Option<String>>,
    /// Extra nodes appended after assigned-board nodes (e.g. remote `"unknown"`).
    extra_nodes: Mutex<Vec<SessionNode>>,
    logs: Mutex<HashMap<String, Vec<LogEvent>>>,
    next_log_seq: Mutex<HashMap<String, u64>>,
}

impl MockBackend {
    fn blank(name: &str, fail_create: bool, shutdown_failures: usize) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            creates: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            resets: AtomicUsize::new(0),
            fail_create,
            fail_start: AtomicBool::new(false),
            fail_status: AtomicBool::new(false),
            fail_logs: AtomicBool::new(false),
            shutdown_failures: AtomicUsize::new(shutdown_failures),
            live: Mutex::new(HashMap::new()),
            session_state: Mutex::new("stopped".into()),
            virtual_time_ns: AtomicU64::new(0),
            stop_reason: Mutex::new(None),
            extra_nodes: Mutex::new(Vec::new()),
            logs: Mutex::new(HashMap::new()),
            next_log_seq: Mutex::new(HashMap::new()),
        })
    }

    fn new(name: &str) -> Arc<Self> {
        Self::blank(name, false, 0)
    }

    fn failing(name: &str) -> Arc<Self> {
        Self::blank(name, true, 0)
    }

    fn with_shutdown_failures(name: &str, n: usize) -> Arc<Self> {
        Self::blank(name, false, n)
    }

    fn push_log(&self, id: &str, text: &str) {
        let mut seqs = self.next_log_seq.lock().unwrap();
        let seq = seqs.entry(id.to_string()).or_insert(1);
        let event = LogEvent {
            seq: *seq,
            level: "info".into(),
            origin: self.name.clone(),
            board: None,
            text: text.into(),
        };
        *seq = seq.saturating_add(1);
        self.logs
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .push(event);
    }
}

impl SimulationBackend for MockBackend {
    fn create(&self, request: CreateSimulationRequest) -> Result<String, ApiError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        if self.fail_create {
            return Err(ApiError::internal(format!("{} boom", self.name)));
        }
        let id = request.id.unwrap_or_else(|| "auto".into());
        let boards = request.assigned_boards.unwrap_or_else(|| {
            request
                .topology
                .boards
                .iter()
                .map(|b| b.id.clone())
                .collect()
        });
        self.live.lock().unwrap().insert(id.clone(), boards);
        self.logs.lock().unwrap().insert(id.clone(), Vec::new());
        self.next_log_seq.lock().unwrap().insert(id.clone(), 1);
        Ok(id)
    }

    fn shutdown(&self, id: &str) -> Result<(), ApiError> {
        let prev = self.shutdown_failures.load(Ordering::SeqCst);
        if prev > 0
            && self
                .shutdown_failures
                .compare_exchange(prev, prev - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(ApiError::internal(format!("{} shutdown boom", self.name)));
        }
        self.logs.lock().unwrap().remove(id);
        self.next_log_seq.lock().unwrap().remove(id);
        self.live
            .lock()
            .unwrap()
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| ApiError::not_found("missing"))
    }

    fn exists(&self, id: &str) -> Result<bool, ApiError> {
        Ok(self.live.lock().unwrap().contains_key(id))
    }

    fn start(&self, _: &str, _: Option<u64>) -> Result<(), ApiError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail_start.load(Ordering::SeqCst) {
            return Err(ApiError::internal(format!("{} start boom", self.name)));
        }
        *self.session_state.lock().unwrap() = "running".into();
        Ok(())
    }
    fn stop(&self, _: &str) -> Result<(), ApiError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        *self.session_state.lock().unwrap() = "stopped".into();
        Ok(())
    }
    fn reset(&self, _: &str) -> Result<(), ApiError> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
        if self.fail_status.load(Ordering::SeqCst) {
            return Err(ApiError::internal(format!("{} status boom", self.name)));
        }
        let boards = self
            .live
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("missing"))?;
        let mut nodes: Vec<SessionNode> = boards
            .iter()
            .map(|board| SessionNode {
                id: crate::ipc::board_id_hash(board),
                kind: "rl78".into(),
                state: self.session_state.lock().unwrap().clone(),
            })
            .collect();
        nodes.extend(self.extra_nodes.lock().unwrap().iter().cloned());
        Ok(StatusEvent {
            id: id.into(),
            state: self.session_state.lock().unwrap().clone(),
            virtual_time_ns: self.virtual_time_ns.load(Ordering::SeqCst),
            stop_reason: self.stop_reason.lock().unwrap().clone(),
            nodes,
        })
    }
    fn topology(&self, _: &str) -> Result<TopologyView, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError> {
        if self.fail_logs.load(Ordering::SeqCst) {
            return Err(ApiError::internal(format!("{} logs boom", self.name)));
        }
        let logs = self.logs.lock().unwrap();
        let Some(entries) = logs.get(id) else {
            return Err(ApiError::not_found("missing"));
        };
        Ok(entries.iter().filter(|e| e.seq > after).cloned().collect())
    }
    fn log_high_water(&self, id: &str) -> Result<u64, ApiError> {
        let seqs = self.next_log_seq.lock().unwrap();
        Ok(seqs.get(id).map(|s| s.saturating_sub(1)).unwrap_or(0))
    }
    fn actions(&self, _: &str) -> Result<ActionList, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn invoke_action(&self, _: &str, _: InvokeActionRequest) -> Result<(), ApiError> {
        Ok(())
    }
    fn probe_catalog(&self, _: &str) -> Result<Vec<crate::probe_log::ProbeCatalogItem>, ApiError> {
        Ok(vec![])
    }
    fn probe_watch_get(&self, _: &str) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn probe_watch_put(
        &self,
        _: &str,
        _: Vec<crate::probe_log::ProbeRef>,
    ) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn probe_watch_clear(&self, _: &str) -> Result<(), ApiError> {
        Ok(())
    }
    fn probe_watch_live(
        &self,
        _: &str,
        _: u64,
    ) -> Result<Vec<crate::probe_log::CoarseBatch>, ApiError> {
        Ok(vec![])
    }
    fn csv_start(
        &self,
        _: &str,
        _: crate::probe_log::ProbeRange,
    ) -> Result<(u64, Vec<crate::probe_log::ProbePointName>), ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn csv_poll(&self, _: &str, _: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn csv_status(&self, _: &str, _: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn probe_samples(
        &self,
        _: &str,
        _: crate::ipc::HashedId<u64>,
        _: crate::ipc::HashedId<u64>,
        _: Option<u64>,
        _: Option<u64>,
    ) -> Result<crate::contract::ProbeSamplesResponse, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn viewer_attach(&self, _: &str) -> Result<String, ApiError> {
        Ok("t".into())
    }
    fn viewer_check(&self, _: &str, _: &str) -> Result<(), ApiError> {
        Ok(())
    }
    fn viewer_detach(&self, _: &str, _: &str) -> Result<(), ApiError> {
        Ok(())
    }
    fn inspect(
        &self,
        _: &str,
        _: crate::ipc::HashedId<u64>,
        _: InspectRequest,
    ) -> Result<InspectReply, ApiError> {
        Err(ApiError::internal("unused"))
    }
    fn step(&self, _: &str, _: crate::ipc::HashedId<u64>) -> Result<(), ApiError> {
        Ok(())
    }
}

fn board(id: &str) -> BoardSpec {
    BoardSpec {
        id: id.into(),
        kind: "rl78".into(),
        elf: Some("base64:QQ==".into()),
        endpoints: vec![],
    }
}

fn topo_two_connected() -> LogicalTopology {
    LogicalTopology {
        boards: vec![board("a"), board("b")],
        edges: vec![DirectedEdge {
            from: PortRef {
                path: "a".into(),
                port: "u".into(),
            },
            to: PortRef {
                path: "b".into(),
                port: "u".into(),
            },
            route: vec![],
            payload: PayloadKind::Uart,
            uart_ring_len: None,
        }],
        margin_ns: 1,
        booting_timeout_ms: 1000,
    }
}

fn topo_two_disconnected() -> LogicalTopology {
    LogicalTopology {
        boards: vec![board("a"), board("b")],
        edges: vec![],
        margin_ns: 1,
        booting_timeout_ms: 1000,
    }
}

#[test]
fn single_server_create_no_multicast() {
    let b0 = MockBackend::new("s0");
    let fe = ClusterFrontend::new(
        vec![(b0.clone(), "s0".into(), 4, MachineId::from_u32(1))],
        vec![],
        vec![],
    )
    .unwrap();
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_connected(),
            id: Some("client-id".into()),
            multicast: Some(CreateMulticast {
                address: "239.1.1.1".into(),
                port: 30000,
            }),
            assigned_boards: Some(vec!["a".into()]),
        })
        .unwrap();
    assert_ne!(id, "client-id");
    assert_eq!(b0.creates.load(Ordering::SeqCst), 1);
    let live = b0.live.lock().unwrap();
    assert_eq!(live.get(&id).unwrap().len(), 2);
}

#[test]
fn cross_host_edge_rejected() {
    let fe = ClusterFrontend::new(
        vec![
            (
                MockBackend::new("s0"),
                "s0".into(),
                1,
                MachineId::from_u32(1),
            ),
            (
                MockBackend::new("s1"),
                "s1".into(),
                1,
                MachineId::from_u32(2),
            ),
        ],
        vec![(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 3))],
        vec![(30000, 30002)],
    )
    .unwrap();
    let err = fe
        .create(CreateSimulationRequest {
            topology: topo_two_connected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap_err();
    assert_eq!(err.code, "cross_host_edge");
}

#[test]
fn multi_server_disconnected_allocates_mcast_and_fans_out() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = ClusterFrontend::new(
        vec![
            (b0.clone(), "s0".into(), 1, MachineId::from_u32(1)),
            (b1.clone(), "s1".into(), 1, MachineId::from_u32(2)),
        ],
        vec![(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 1))],
        vec![(30000, 30000)],
    )
    .unwrap();
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    assert_eq!(b0.creates.load(Ordering::SeqCst), 1);
    assert_eq!(b1.creates.load(Ordering::SeqCst), 1);
    let assigned0 = b0.live.lock().unwrap().get(&id).unwrap().clone();
    let assigned1 = b1.live.lock().unwrap().get(&id).unwrap().clone();
    assert_eq!(assigned0.len(), 1);
    assert_eq!(assigned1.len(), 1);
    let mut all = assigned0;
    all.extend(assigned1);
    all.sort();
    assert_eq!(all, vec!["a".to_string(), "b".to_string()]);
    let state = fe.lock().unwrap();
    assert_eq!(
        ClusterFrontend::used_multicast(&state),
        vec![(Ipv4Addr::new(239, 0, 0, 1), 30000)]
    );
    assert_eq!(
        fe.free_capacity(&state),
        HashMap::from([(MachineId::from_u32(1), 0), (MachineId::from_u32(2), 0)])
    );
}

#[test]
fn partial_create_compensates() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::failing("s1");
    let fe = ClusterFrontend::new(
        vec![
            (b0.clone(), "s0".into(), 1, MachineId::from_u32(1)),
            (b1.clone(), "s1".into(), 1, MachineId::from_u32(2)),
        ],
        vec![(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 1))],
        vec![(30000, 30000)],
    )
    .unwrap();
    let err = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap_err();
    assert!(err.message.contains("compensated"), "{err:?}");
    assert!(b0.live.lock().unwrap().is_empty());
    let state = fe.lock().unwrap();
    assert!(state.sims.is_empty());
    assert_eq!(
        fe.free_capacity(&state),
        HashMap::from([(MachineId::from_u32(1), 1), (MachineId::from_u32(2), 1)])
    );
    assert!(ClusterFrontend::used_multicast(&state).is_empty());
}

#[test]
fn shutdown_frees_capacity_and_mcast() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = ClusterFrontend::new(
        vec![
            (b0.clone(), "s0".into(), 1, MachineId::from_u32(1)),
            (b1.clone(), "s1".into(), 1, MachineId::from_u32(2)),
        ],
        vec![(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 1))],
        vec![(30000, 30000)],
    )
    .unwrap();
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    fe.shutdown(&id).unwrap();
    let state = fe.lock().unwrap();
    assert!(state.sims.is_empty());
    assert!(ClusterFrontend::used_multicast(&state).is_empty());
    assert_eq!(
        fe.free_capacity(&state),
        HashMap::from([(MachineId::from_u32(1), 1), (MachineId::from_u32(2), 1)])
    );
}

#[test]
fn concurrent_partial_shutdown_merges_ledger() {
    let b0 = MockBackend::with_shutdown_failures("s0", 1);
    let b1 = MockBackend::with_shutdown_failures("s1", 1);
    let fe = Arc::new(
        ClusterFrontend::new(
            vec![
                (b0.clone(), "s0".into(), 1, MachineId::from_u32(1)),
                (b1.clone(), "s1".into(), 1, MachineId::from_u32(2)),
            ],
            vec![(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 1))],
            vec![(30000, 30000)],
        )
        .unwrap(),
    );
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..2 {
        let fe = Arc::clone(&fe);
        let id = id.clone();
        handles.push(thread::spawn(move || fe.shutdown(&id)));
    }
    for h in handles {
        let _ = h.join().expect("shutdown thread");
    }

    // Must be clean immediately after concurrent joins — a retry loop would
    // mask the old full-overwrite ledger race.
    assert!(b0.live.lock().unwrap().is_empty());
    assert!(b1.live.lock().unwrap().is_empty());
    let state = fe.lock().unwrap();
    assert!(
        state.sims.is_empty(),
        "ledger leaked after concurrent partial shutdown: {:?}",
        state.sims.keys().collect::<Vec<_>>()
    );
    assert!(ClusterFrontend::used_multicast(&state).is_empty());
    assert_eq!(
        fe.free_capacity(&state),
        HashMap::from([(MachineId::from_u32(1), 1), (MachineId::from_u32(2), 1)])
    );
}

fn two_server_frontend(b0: Arc<MockBackend>, b1: Arc<MockBackend>) -> ClusterFrontend {
    ClusterFrontend::new(
        vec![
            (b0, "s0".into(), 1, MachineId::from_u32(1)),
            (b1, "s1".into(), 1, MachineId::from_u32(2)),
        ],
        vec![(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 1))],
        vec![(30000, 30000)],
    )
    .unwrap()
}

#[test]
fn control_fanout_calls_all_participants() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    fe.start(&id, Some(42)).unwrap();
    fe.stop(&id).unwrap();
    fe.reset(&id).unwrap();
    assert_eq!(b0.starts.load(Ordering::SeqCst), 1);
    assert_eq!(b1.starts.load(Ordering::SeqCst), 1);
    assert_eq!(b0.stops.load(Ordering::SeqCst), 1);
    assert_eq!(b1.stops.load(Ordering::SeqCst), 1);
    assert_eq!(b0.resets.load(Ordering::SeqCst), 1);
    assert_eq!(b1.resets.load(Ordering::SeqCst), 1);
}

#[test]
fn control_fanout_partial_failure_no_rollback() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b1.fail_start.store(true, Ordering::SeqCst);
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    let err = fe.start(&id, None).unwrap_err();
    assert!(err.message.contains("start boom"), "{err:?}");
    assert_eq!(b0.starts.load(Ordering::SeqCst), 1);
    assert_eq!(b1.starts.load(Ordering::SeqCst), 1);
    assert_eq!(b0.session_state.lock().unwrap().as_str(), "running");
    assert_eq!(b1.session_state.lock().unwrap().as_str(), "stopped");
}

#[test]
fn status_merges_assigned_nodes_and_min_vt() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b0.virtual_time_ns.store(100, Ordering::SeqCst);
    b1.virtual_time_ns.store(40, Ordering::SeqCst);
    *b0.session_state.lock().unwrap() = "running".into();
    *b1.session_state.lock().unwrap() = "stopped".into();
    *b1.stop_reason.lock().unwrap() = Some("breakpoint".into());
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    // Inject a remote `"unknown"` row after assignment is known.
    let other_on_b0 = {
        let live0 = b0.live.lock().unwrap();
        let assigned = live0.get(&id).unwrap();
        if assigned.contains(&"a".to_string()) {
            "b"
        } else {
            "a"
        }
    };
    b0.extra_nodes.lock().unwrap().push(SessionNode {
        id: crate::ipc::board_id_hash(other_on_b0),
        kind: "rl78".into(),
        state: "unknown".into(),
    });
    let status = fe.status(&id).unwrap();
    assert_eq!(status.state, "running");
    assert_eq!(status.virtual_time_ns, 40);
    assert!(status.stop_reason.is_none());
    assert_eq!(status.nodes.len(), 2);
    let states: HashMap<_, _> = status
        .nodes
        .iter()
        .map(|n| (n.id, n.state.clone()))
        .collect();
    let board_on_b0 = b0.live.lock().unwrap().get(&id).unwrap()[0].clone();
    let board_on_b1 = b1.live.lock().unwrap().get(&id).unwrap()[0].clone();
    assert_eq!(
        states
            .get(&crate::ipc::board_id_hash(&board_on_b0))
            .map(String::as_str),
        Some("running")
    );
    assert_eq!(
        states
            .get(&crate::ipc::board_id_hash(&board_on_b1))
            .map(String::as_str),
        Some("stopped")
    );
    assert!(!status.nodes.iter().any(|n| n.state == "unknown"));
}

#[test]
fn status_omits_failed_backend() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b1.fail_status.store(true, Ordering::SeqCst);
    b0.virtual_time_ns.store(7, Ordering::SeqCst);
    *b0.session_state.lock().unwrap() = "running".into();
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    let status = fe.status(&id).unwrap();
    assert_eq!(status.state, "running");
    assert_eq!(status.virtual_time_ns, 7);
    assert_eq!(status.nodes.len(), 1);
}

#[test]
fn logs_aggregate_global_seq_across_backends() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    assert_eq!(fe.log_high_water(&id).unwrap(), 0);
    b0.push_log(&id, "from-0");
    b1.push_log(&id, "from-1");
    // Overlapping local seq spaces (both start at 1).
    let batch = fe.logs_after(&id, 0).unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].seq, 1);
    assert_eq!(batch[1].seq, 2);
    assert_eq!(fe.log_high_water(&id).unwrap(), 2);
    // Second poll with after=high_water yields nothing until new lines.
    assert!(fe.logs_after(&id, 2).unwrap().is_empty());
    b0.push_log(&id, "from-0-again");
    let more = fe.logs_after(&id, 2).unwrap();
    assert_eq!(more.len(), 1);
    assert_eq!(more[0].seq, 3);
    assert_eq!(more[0].text, "from-0-again");
}

#[test]
fn logs_skip_failed_backend_pull() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b1.fail_logs.store(true, Ordering::SeqCst);
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    b0.push_log(&id, "ok");
    b1.push_log(&id, "hidden");
    let batch = fe.logs_after(&id, 0).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].text, "ok");
}

#[test]
fn logs_recover_after_backend_pull_failure() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b1.fail_logs.store(true, Ordering::SeqCst);
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    b1.push_log(&id, "was-hidden");
    assert!(fe.logs_after(&id, 0).unwrap().is_empty());
    b1.fail_logs.store(false, Ordering::SeqCst);
    let batch = fe.logs_after(&id, 0).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].text, "was-hidden");
}

#[test]
fn log_high_water_ingests_backlog_for_sse_subscribe() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    b0.push_log(&id, "old-0");
    b1.push_log(&id, "old-1");
    let hw = fe.log_high_water(&id).unwrap();
    assert_eq!(hw, 2);
    // SSE starts at high-water: backlog must not reappear.
    assert!(fe.logs_after(&id, hw).unwrap().is_empty());
    b0.push_log(&id, "new");
    let fresh = fe.logs_after(&id, hw).unwrap();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].text, "new");
    assert_eq!(fresh[0].seq, 3);
}

#[test]
fn control_fanout_prefers_lowest_machine_id_error() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b0.fail_start.store(true, Ordering::SeqCst);
    b1.fail_start.store(true, Ordering::SeqCst);
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    let err = fe.start(&id, None).unwrap_err();
    assert!(
        err.message.contains("s0 start boom"),
        "expected lowest MachineId (s0) error, got {err:?}"
    );
}

#[test]
fn status_all_backends_fail() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b0.fail_status.store(true, Ordering::SeqCst);
    b1.fail_status.store(true, Ordering::SeqCst);
    let fe = two_server_frontend(b0, b1);
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    let err = fe.status(&id).unwrap_err();
    assert!(err.message.contains("status boom"), "{err:?}");
}

#[test]
fn status_stop_reason_when_all_stopped() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    *b0.session_state.lock().unwrap() = "stopped".into();
    *b1.session_state.lock().unwrap() = "stopped".into();
    *b1.stop_reason.lock().unwrap() = Some("breakpoint".into());
    let fe = two_server_frontend(b0, b1);
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    let status = fe.status(&id).unwrap();
    assert_eq!(status.state, "stopped");
    assert_eq!(status.stop_reason.as_deref(), Some("breakpoint"));
}

#[test]
fn shutdown_drops_log_aggregate() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    b0.push_log(&id, "x");
    assert_eq!(fe.log_high_water(&id).unwrap(), 1);
    fe.shutdown(&id).unwrap();
    assert_eq!(fe.log_high_water(&id).unwrap_err().code, "not_found");
    let state = fe.lock().unwrap();
    assert!(!state.logs.contains_key(&id));
}

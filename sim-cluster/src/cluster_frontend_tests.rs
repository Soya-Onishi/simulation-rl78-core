//! Unit tests for [`crate::cluster_frontend`].

use super::*;
use crate::contract::AdvertisedAction;
use crate::host::SessionNode;
use crate::ipc::{HashedId, board_id_hash};
use crate::probe_log::{
    CoarseBatch, ProbeCatalogItem, ProbePointName, ProbeRecord, ProbeRef, ProbeWatchView,
};
use crate::topology::{BoardSpec, DirectedEdge, PayloadKind, PortRef};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

struct MockBackend {
    name: String,
    creates: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    resets: AtomicUsize,
    inspects: AtomicUsize,
    steps: AtomicUsize,
    invokes: AtomicUsize,
    probe_samples: AtomicUsize,
    fail_create: bool,
    fail_start: AtomicBool,
    fail_status: AtomicBool,
    fail_logs: AtomicBool,
    fail_log_high_water: AtomicBool,
    /// Remaining times `shutdown` should return an error before removing.
    shutdown_failures: AtomicUsize,
    status_delay: Mutex<Duration>,
    logs_delay: Mutex<Duration>,
    live: Mutex<HashMap<String, Vec<String>>>,
    session_state: Mutex<String>,
    virtual_time_ns: AtomicU64,
    stop_reason: Mutex<Option<String>>,
    /// Extra nodes appended after assigned-board nodes (e.g. remote `"unknown"`).
    extra_nodes: Mutex<Vec<SessionNode>>,
    logs: Mutex<HashMap<String, Vec<LogEvent>>>,
    next_log_seq: Mutex<HashMap<String, u64>>,
    actions: Mutex<Vec<AdvertisedAction>>,
    catalog: Mutex<Vec<ProbeCatalogItem>>,
    watch: Mutex<Vec<ProbeRef>>,
    /// Coarse batches returned by `probe_watch_live` (filtered by fetch floor).
    coarse: Mutex<Vec<CoarseBatch>>,
    csv_probes: Mutex<Vec<ProbePointName>>,
    csv_started: Mutex<Option<u64>>,
    csv_poll_state: Mutex<MockCsvPollState>,
}

struct MockCsvPollState {
    request_id: u64,
    pending: Vec<ProbeRecord>,
    done: bool,
    failed: bool,
    truncated: bool,
}

impl MockBackend {
    fn blank(name: &str, fail_create: bool, shutdown_failures: usize) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            creates: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            resets: AtomicUsize::new(0),
            inspects: AtomicUsize::new(0),
            steps: AtomicUsize::new(0),
            invokes: AtomicUsize::new(0),
            probe_samples: AtomicUsize::new(0),
            fail_create,
            fail_start: AtomicBool::new(false),
            fail_status: AtomicBool::new(false),
            fail_logs: AtomicBool::new(false),
            fail_log_high_water: AtomicBool::new(false),
            shutdown_failures: AtomicUsize::new(shutdown_failures),
            status_delay: Mutex::new(Duration::ZERO),
            logs_delay: Mutex::new(Duration::ZERO),
            live: Mutex::new(HashMap::new()),
            session_state: Mutex::new("stopped".into()),
            virtual_time_ns: AtomicU64::new(0),
            stop_reason: Mutex::new(None),
            extra_nodes: Mutex::new(Vec::new()),
            logs: Mutex::new(HashMap::new()),
            next_log_seq: Mutex::new(HashMap::new()),
            actions: Mutex::new(Vec::new()),
            catalog: Mutex::new(Vec::new()),
            watch: Mutex::new(Vec::new()),
            coarse: Mutex::new(Vec::new()),
            csv_probes: Mutex::new(Vec::new()),
            csv_started: Mutex::new(None),
            csv_poll_state: Mutex::new(MockCsvPollState {
                request_id: 0,
                pending: Vec::new(),
                done: true,
                failed: false,
                truncated: false,
            }),
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
        let delay = *self.status_delay.lock().unwrap();
        if !delay.is_zero() {
            thread::sleep(delay);
        }
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
        let delay = *self.logs_delay.lock().unwrap();
        if !delay.is_zero() {
            thread::sleep(delay);
        }
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
        if self.fail_log_high_water.load(Ordering::SeqCst) {
            return Err(ApiError::internal(format!("{} high-water boom", self.name)));
        }
        let seqs = self.next_log_seq.lock().unwrap();
        Ok(seqs.get(id).map(|s| s.saturating_sub(1)).unwrap_or(0))
    }
    fn actions(&self, _: &str) -> Result<ActionList, ApiError> {
        Ok(ActionList {
            actions: self.actions.lock().unwrap().clone(),
        })
    }
    fn invoke_action(&self, _: &str, _: InvokeActionRequest) -> Result<(), ApiError> {
        self.invokes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn probe_catalog(&self, _: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
        Ok(self.catalog.lock().unwrap().clone())
    }
    fn probe_watch_get(&self, _: &str) -> Result<ProbeWatchView, ApiError> {
        Ok(ProbeWatchView {
            probes: self.watch.lock().unwrap().clone(),
        })
    }
    fn probe_watch_put(&self, _: &str, probes: Vec<ProbeRef>) -> Result<ProbeWatchView, ApiError> {
        *self.watch.lock().unwrap() = probes.clone();
        Ok(ProbeWatchView { probes })
    }
    fn probe_watch_clear(&self, _: &str) -> Result<(), ApiError> {
        self.watch.lock().unwrap().clear();
        Ok(())
    }
    fn probe_watch_live(
        &self,
        _: &str,
        fetch_from: u64,
    ) -> Result<crate::probe_log::CoarseLive, ApiError> {
        let batches = self
            .coarse
            .lock()
            .unwrap()
            .iter()
            .filter(|batch| batch.min_virtual_time_ns >= fetch_from)
            .cloned()
            .collect();
        Ok(crate::probe_log::CoarseLive {
            cursor_vt: self.virtual_time_ns.load(Ordering::SeqCst),
            batches,
        })
    }
    fn csv_start(
        &self,
        _: &str,
        _: crate::probe_log::ProbeRange,
        request_id: u64,
    ) -> Result<(u64, Vec<crate::probe_log::ProbePointName>), ApiError> {
        let probes = self.csv_probes.lock().unwrap().clone();
        *self.csv_started.lock().unwrap() = Some(request_id);
        self.csv_poll_state.lock().unwrap().request_id = request_id;
        Ok((request_id, probes))
    }
    fn csv_poll(&self, _: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        let mut state = self.csv_poll_state.lock().unwrap();
        if state.request_id != request_id {
            return Err(ApiError::not_found("unknown csv export"));
        }
        let records = std::mem::take(&mut state.pending);
        Ok(crate::probe_log::CsvPoll {
            records,
            done: state.done,
            failed: state.failed,
            truncated: state.truncated,
        })
    }
    fn csv_status(&self, _: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        let state = self.csv_poll_state.lock().unwrap();
        if state.request_id != request_id {
            return Err(ApiError::not_found("unknown csv export"));
        }
        Ok(crate::probe_log::CsvPoll {
            records: Vec::new(),
            done: state.done,
            failed: state.failed,
            truncated: state.truncated,
        })
    }
    fn probe_samples(
        &self,
        _: &str,
        _: crate::ipc::HashedId<u64>,
        _: crate::ipc::HashedId<u64>,
        _: Option<u64>,
        _: Option<u64>,
    ) -> Result<crate::contract::ProbeSamplesResponse, ApiError> {
        self.probe_samples.fetch_add(1, Ordering::SeqCst);
        Ok(crate::contract::ProbeSamplesResponse {
            truncated: false,
            samples: vec![],
        })
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
        self.inspects.fetch_add(1, Ordering::SeqCst);
        Ok(InspectReply::Ok)
    }
    fn step(&self, _: &str, _: crate::ipc::HashedId<u64>) -> Result<(), ApiError> {
        self.steps.fetch_add(1, Ordering::SeqCst);
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
fn cross_host_uart_edge_allowed() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = ClusterFrontend::new(
        vec![
            (b0.clone(), "s0".into(), 1, MachineId::from_u32(1)),
            (b1.clone(), "s1".into(), 1, MachineId::from_u32(2)),
        ],
        vec![(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 3))],
        vec![(30000, 30002)],
    )
    .unwrap();
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_connected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    assert_eq!(b0.creates.load(Ordering::SeqCst), 1);
    assert_eq!(b1.creates.load(Ordering::SeqCst), 1);
    assert!(!id.is_empty());
}

fn topo_two_digital_connected() -> LogicalTopology {
    LogicalTopology {
        boards: vec![board("a"), board("b")],
        edges: vec![DirectedEdge {
            from: PortRef {
                path: "a".into(),
                port: "d".into(),
            },
            to: PortRef {
                path: "b".into(),
                port: "d".into(),
            },
            route: vec![],
            payload: PayloadKind::Digital,
            uart_ring_len: None,
        }],
        margin_ns: 1,
        booting_timeout_ms: 1000,
    }
}

#[test]
fn cross_host_digital_edge_rejected() {
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
            topology: topo_two_digital_connected(),
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
fn log_high_water_skips_failed_backend_backlog() {
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
    b0.push_log(&id, "from-0");
    b1.push_log(&id, "hidden-backlog");
    let hw = fe.log_high_water(&id).unwrap();
    assert_eq!(hw, 1, "only successful backend lines are ingested");
    b1.fail_logs.store(false, Ordering::SeqCst);
    // Cursor was advanced past b1's backlog at subscribe; must not replay.
    assert!(
        fe.logs_after(&id, hw).unwrap().is_empty(),
        "failed-backend backlog must not appear after subscribe high-water"
    );
    b1.push_log(&id, "after-subscribe");
    let fresh = fe.logs_after(&id, hw).unwrap();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].text, "after-subscribe");
}

#[test]
fn log_high_water_defers_skip_when_high_water_also_fails() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b1.fail_logs.store(true, Ordering::SeqCst);
    b1.fail_log_high_water.store(true, Ordering::SeqCst);
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    b1.push_log(&id, "dead-backlog");
    let hw = fe.log_high_water(&id).unwrap();
    assert_eq!(hw, 0);
    b1.fail_logs.store(false, Ordering::SeqCst);
    b1.fail_log_high_water.store(false, Ordering::SeqCst);
    // First recovered pull must skip backlog (baseline pending), not emit it.
    assert!(fe.logs_after(&id, hw).unwrap().is_empty());
    b1.push_log(&id, "after-recover");
    let fresh = fe.logs_after(&id, hw).unwrap();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].text, "after-recover");
}

#[test]
fn concurrent_log_high_water_does_not_skip_ingested_lines() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    // Slow pull so a second client can contend on the same sim's aggregate mutex.
    *b0.logs_delay.lock().unwrap() = Duration::from_millis(50);
    let fe = Arc::new(two_server_frontend(b0.clone(), b1.clone()));
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();

    let fe_hw = Arc::clone(&fe);
    let id_hw = id.clone();
    let hw_thread = thread::spawn(move || fe_hw.log_high_water(&id_hw));

    thread::sleep(Duration::from_millis(10));
    b0.push_log(&id, "during-subscribe");
    let polled = fe.logs_after(&id, 0).unwrap();
    let hw = hw_thread.join().unwrap().unwrap();

    // Everything ingested before/during subscribe must be covered by hw, and
    // a follower starting at hw must not be asked to skip unseen seq gaps.
    if polled.is_empty() {
        // high_water won the race and ingested the line itself.
        assert!(hw >= 1, "hw={hw}");
        assert!(fe.logs_after(&id, hw).unwrap().is_empty());
    } else {
        assert_eq!(polled[0].text, "during-subscribe");
        let max_seen = polled.iter().map(|e| e.seq).max().unwrap();
        assert!(
            hw >= max_seen,
            "high_water {hw} skipped seq already visible to peer ({max_seen})"
        );
    }
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

#[test]
fn machine_for_board_matches_hash() {
    let mid0 = MachineId::from_u32(1);
    let mid1 = MachineId::from_u32(2);
    let map = HashMap::from([(mid0, vec!["a".into()]), (mid1, vec!["b".into()])]);
    assert_eq!(machine_for_board(&map, board_id_hash("a")), Some(mid0));
    assert_eq!(machine_for_board(&map, board_id_hash("b")), Some(mid1));
    assert_eq!(machine_for_board(&map, board_id_hash("missing")), None);
}

#[test]
fn single_board_apis_route_to_hosting_backend() {
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
    let boards0 = b0.live.lock().unwrap().get(&id).cloned().unwrap();
    let boards1 = b1.live.lock().unwrap().get(&id).cloned().unwrap();
    assert_eq!(boards0.len(), 1);
    assert_eq!(boards1.len(), 1);
    let board_on_1 = boards1[0].clone();
    let hash = board_id_hash(&board_on_1);

    fe.inspect(&id, hash, InspectRequest::ReadRegs).unwrap();
    fe.step(&id, hash).unwrap();
    fe.invoke_action(
        &id,
        InvokeActionRequest {
            board_id: hash,
            device: HashedId::new(0),
            action: HashedId::new(0),
            params: Default::default(),
        },
    )
    .unwrap();
    fe.probe_samples(&id, hash, HashedId::new(0), None, None)
        .unwrap();

    assert_eq!(b1.inspects.load(Ordering::SeqCst), 1);
    assert_eq!(b1.steps.load(Ordering::SeqCst), 1);
    assert_eq!(b1.invokes.load(Ordering::SeqCst), 1);
    assert_eq!(b1.probe_samples.load(Ordering::SeqCst), 1);
    assert_eq!(b0.inspects.load(Ordering::SeqCst), 0);
    assert_eq!(b0.steps.load(Ordering::SeqCst), 0);
    assert_eq!(b0.invokes.load(Ordering::SeqCst), 0);
    assert_eq!(b0.probe_samples.load(Ordering::SeqCst), 0);

    let err = fe
        .inspect(&id, board_id_hash("missing"), InspectRequest::ReadRegs)
        .unwrap_err();
    assert_eq!(err.code, "unknown_board");
}

#[test]
fn single_board_apis_unknown_simulation() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    let fe = two_server_frontend(b0, b1);
    let err = fe
        .inspect("no-such-sim", board_id_hash("a"), InspectRequest::ReadRegs)
        .unwrap_err();
    assert_eq!(err.code, "not_found");
}

#[test]
fn catalog_and_actions_merge_across_hosts() {
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
    let boards0 = b0.live.lock().unwrap().get(&id).cloned().unwrap();
    let boards1 = b1.live.lock().unwrap().get(&id).cloned().unwrap();
    let board0 = boards0[0].clone();
    let board1 = boards1[0].clone();
    let h0 = board_id_hash(&board0);
    let h1 = board_id_hash(&board1);

    *b0.catalog.lock().unwrap() = vec![ProbeCatalogItem {
        board: h0,
        board_label: board0.clone(),
        probe: HashedId::new(1),
        probe_label: "p0".into(),
        kind: "digital".into(),
    }];
    *b1.catalog.lock().unwrap() = vec![ProbeCatalogItem {
        board: h1,
        board_label: board1.clone(),
        probe: HashedId::new(2),
        probe_label: "p1".into(),
        kind: "analog".into(),
    }];
    *b0.actions.lock().unwrap() = vec![AdvertisedAction {
        board_id: h0,
        board_label: board0,
        device: HashedId::new(10),
        device_label: "d0".into(),
        action: HashedId::new(11),
        action_label: "a0".into(),
        description: String::new(),
        params: vec![],
    }];
    *b1.actions.lock().unwrap() = vec![AdvertisedAction {
        board_id: h1,
        board_label: board1,
        device: HashedId::new(20),
        device_label: "d1".into(),
        action: HashedId::new(21),
        action_label: "a1".into(),
        description: String::new(),
        params: vec![],
    }];

    let catalog = fe.probe_catalog(&id).unwrap();
    assert_eq!(catalog.len(), 2);
    let actions = fe.actions(&id).unwrap();
    assert_eq!(actions.actions.len(), 2);
}

#[test]
fn probe_watch_put_partitions_by_board_host() {
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
    let boards0 = b0.live.lock().unwrap().get(&id).cloned().unwrap();
    let boards1 = b1.live.lock().unwrap().get(&id).cloned().unwrap();
    let h0 = board_id_hash(&boards0[0]);
    let h1 = board_id_hash(&boards1[0]);

    let view = fe
        .probe_watch_put(
            &id,
            vec![
                ProbeRef {
                    board: h0,
                    name: HashedId::new(1),
                },
                ProbeRef {
                    board: h1,
                    name: HashedId::new(2),
                },
            ],
        )
        .unwrap();
    assert_eq!(view.probes.len(), 2);
    assert_eq!(b0.watch.lock().unwrap().len(), 1);
    assert_eq!(b0.watch.lock().unwrap()[0].board, h0);
    assert_eq!(b1.watch.lock().unwrap().len(), 1);
    assert_eq!(b1.watch.lock().unwrap()[0].board, h1);
}

#[test]
fn probe_watch_live_dedupes_host_seq_and_sets_cursor_vt() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    b0.virtual_time_ns.store(500_000_000, Ordering::SeqCst);
    b1.virtual_time_ns.store(400_000_000, Ordering::SeqCst);
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();

    let mk = |seq: u64| CoarseBatch {
        seq,
        min_virtual_time_ns: 350_000_000,
        samples: vec![],
    };
    *b0.coarse.lock().unwrap() = vec![mk(1)];
    *b1.coarse.lock().unwrap() = vec![mk(1)];

    let first = fe.probe_watch_live(&id, 0).unwrap();
    assert_eq!(first.cursor_vt, 400_000_000);
    assert_eq!(first.batches.len(), 2);

    let second = fe.probe_watch_live(&id, first.cursor_vt).unwrap();
    assert_eq!(second.cursor_vt, 400_000_000);
    assert!(second.batches.is_empty());

    *b0.coarse.lock().unwrap() = vec![mk(1), mk(2)];
    let third = fe.probe_watch_live(&id, second.cursor_vt).unwrap();
    assert_eq!(third.batches.len(), 1);
    assert_eq!(third.batches[0].seq, 2);
}

#[test]
fn csv_start_shares_request_id_and_merges_probes() {
    let b0 = MockBackend::new("s0");
    let b1 = MockBackend::new("s1");
    *b0.csv_probes.lock().unwrap() = vec![ProbePointName {
        probe_id: HashedId::new(1),
        board_hash: board_id_hash("a"),
        board_label: "a".into(),
        name: "p0".into(),
        kind: crate::probe_log::ProbeKind::Digital,
    }];
    *b1.csv_probes.lock().unwrap() = vec![
        ProbePointName {
            probe_id: HashedId::new(1),
            board_hash: board_id_hash("a"),
            board_label: "a".into(),
            name: "p0-dup".into(),
            kind: crate::probe_log::ProbeKind::Digital,
        },
        ProbePointName {
            probe_id: HashedId::new(2),
            board_hash: board_id_hash("b"),
            board_label: "b".into(),
            name: "p1".into(),
            kind: crate::probe_log::ProbeKind::Digital,
        },
    ];
    let fe = two_server_frontend(b0.clone(), b1.clone());
    let id = fe
        .create(CreateSimulationRequest {
            topology: topo_two_disconnected(),
            id: None,
            multicast: None,
            assigned_boards: None,
        })
        .unwrap();
    let (request_id, probes) = fe
        .csv_start(
            &id,
            crate::probe_log::ProbeRange {
                from_ns: None,
                to_ns: None,
            },
            0,
        )
        .unwrap();
    assert_eq!(b0.csv_started.lock().unwrap().as_ref(), Some(&request_id));
    assert_eq!(b1.csv_started.lock().unwrap().as_ref(), Some(&request_id));
    assert_eq!(probes.len(), 2);
    assert_eq!(probes[0].name, "p0");
    assert_eq!(probes[1].name, "p1");
}

#[test]
fn csv_poll_keeps_global_time_order_across_polls() {
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
    let (request_id, _) = fe
        .csv_start(
            &id,
            crate::probe_log::ProbeRange {
                from_ns: None,
                to_ns: None,
            },
            0,
        )
        .unwrap();

    let rec = |vt: u64, probe: u64| ProbeRecord {
        probe_id: HashedId::new(probe),
        virtual_time_ns: vt,
        kind: crate::probe_log::ProbeKind::Digital,
        value: 0,
    };

    // Poll 1: host0 has t=100 done; host1 still waiting (not done, empty) → emit nothing yet.
    {
        let mut s0 = b0.csv_poll_state.lock().unwrap();
        s0.request_id = request_id;
        s0.pending = vec![rec(100, 1)];
        s0.done = true;
    }
    {
        let mut s1 = b1.csv_poll_state.lock().unwrap();
        s1.request_id = request_id;
        s1.pending.clear();
        s1.done = false;
    }
    let first = fe.csv_poll(&id, request_id).unwrap();
    assert!(first.records.is_empty());
    assert!(!first.done);

    // Poll 2: host1 delivers t=50 then done → emit 50 then 100.
    {
        let mut s1 = b1.csv_poll_state.lock().unwrap();
        s1.pending = vec![rec(50, 2)];
        s1.done = true;
    }
    let second = fe.csv_poll(&id, request_id).unwrap();
    assert_eq!(second.records.len(), 2);
    assert_eq!(second.records[0].virtual_time_ns, 50);
    assert_eq!(second.records[1].virtual_time_ns, 100);
    assert!(second.done);
    assert!(!second.failed);
}

//! Frontend placement: First-Fit, multicast allocate, create fanout + compensate.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use crate::backend::SimulationBackend;
use crate::cluster_mcast::{allocate_multicast, has_cross_host_edge, place_boards_first_fit};
use crate::contract::{
    ActionList, ApiError, CreateMulticast, CreateSimulationRequest, InvokeActionRequest, LogEvent,
    StatusEvent, TopologyView,
};
use crate::inspect::{InspectReply, InspectRequest};
use crate::ipc::new_cluster_key;
use crate::topology::LogicalTopology;

struct BackendSlot {
    backend: Arc<dyn SimulationBackend>,
    endpoint: String,
    max_nodes: u32,
}

struct SimRecord {
    /// `(server_index, assigned board ids)` for each participating backend.
    per_server: Vec<(usize, Vec<String>)>,
    mcast: Option<(Ipv4Addr, u16)>,
}

struct FrontendState {
    remaining: Vec<u32>,
    used_mcast: Vec<(Ipv4Addr, u16)>,
    sims: HashMap<String, SimRecord>,
}

/// Multi-backend create planner that implements [`SimulationBackend`].
pub struct ClusterFrontend {
    servers: Vec<BackendSlot>,
    address_ranges: Vec<(Ipv4Addr, Ipv4Addr)>,
    port_ranges: Vec<(u16, u16)>,
    state: Mutex<FrontendState>,
}

impl ClusterFrontend {
    /// Build from configured backends (order = First-Fit server order).
    pub fn new(
        servers: Vec<(Arc<dyn SimulationBackend>, String, u32)>,
        address_ranges: Vec<(Ipv4Addr, Ipv4Addr)>,
        port_ranges: Vec<(u16, u16)>,
    ) -> Result<Self, ApiError> {
        if servers.is_empty() {
            return Err(ApiError::internal("ClusterFrontend: no servers"));
        }
        let remaining: Vec<u32> = servers.iter().map(|(_, _, max)| *max).collect();
        let slots = servers
            .into_iter()
            .map(|(backend, endpoint, max_nodes)| BackendSlot {
                backend,
                endpoint,
                max_nodes,
            })
            .collect();
        Ok(Self {
            servers: slots,
            address_ranges,
            port_ranges,
            state: Mutex::new(FrontendState {
                remaining,
                used_mcast: Vec::new(),
                sims: HashMap::new(),
            }),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, FrontendState>, ApiError> {
        self.state
            .lock()
            .map_err(|_| ApiError::internal("ClusterFrontend state poisoned"))
    }

    fn edge_pairs(topo: &LogicalTopology) -> Vec<(String, String)> {
        topo.edges
            .iter()
            .map(|e| (e.from.path.clone(), e.to.path.clone()))
            .collect()
    }

    fn plan_create(
        &self,
        state: &FrontendState,
        topo: &LogicalTopology,
    ) -> Result<PlannedCreate, ApiError> {
        let board_ids: Vec<String> = topo.boards.iter().map(|b| b.id.clone()).collect();
        let mut remaining = state.remaining.clone();
        let placement = place_boards_first_fit(&board_ids, &mut remaining)
            .map_err(|err| ApiError::bad_request("insufficient_capacity", err.to_string()))?;

        let board_server: HashMap<String, usize> =
            placement.board_to_server.iter().cloned().collect();
        let edges = Self::edge_pairs(topo);
        // TODO(cluster-distributed-p2): remove once cross-host endpoint multicast exists.
        if has_cross_host_edge(&edges, &board_server) {
            return Err(ApiError::bad_request(
                "cross_host_edge",
                "topology has an edge whose endpoints land on different backends (unsupported until p2)",
            ));
        }

        let mut by_server: HashMap<usize, Vec<String>> = HashMap::new();
        for (board_id, server) in &placement.board_to_server {
            by_server.entry(*server).or_default().push(board_id.clone());
        }
        let mut per_server: Vec<(usize, Vec<String>)> = by_server.into_iter().collect();
        per_server.sort_by_key(|(idx, _)| *idx);

        let mcast = if per_server.len() >= 2 {
            if self.address_ranges.is_empty() || self.port_ranges.is_empty() {
                return Err(ApiError::bad_request(
                    "multicast_exhausted",
                    "multi-backend placement requires multicast address/port ranges",
                ));
            }
            let Some(pair) =
                allocate_multicast(&self.address_ranges, &self.port_ranges, &state.used_mcast)
            else {
                return Err(ApiError::bad_request(
                    "multicast_exhausted",
                    "no free multicast address/port in configured ranges",
                ));
            };
            Some(pair)
        } else {
            None
        };

        Ok(PlannedCreate {
            remaining,
            per_server,
            mcast,
        })
    }

    fn primary_backend(&self, id: &str) -> Result<Arc<dyn SimulationBackend>, ApiError> {
        let state = self.lock()?;
        let Some(rec) = state.sims.get(id) else {
            // Fall back: probe servers (e.g. after restart without state).
            drop(state);
            for slot in &self.servers {
                if slot.backend.exists(id)? {
                    return Ok(slot.backend.clone());
                }
            }
            return Err(ApiError::not_found("unknown simulation"));
        };
        let (idx, _) = rec.per_server[0];
        Ok(self.servers[idx].backend.clone())
    }
}

struct PlannedCreate {
    remaining: Vec<u32>,
    per_server: Vec<(usize, Vec<String>)>,
    mcast: Option<(Ipv4Addr, u16)>,
}

impl SimulationBackend for ClusterFrontend {
    fn create(&self, request: CreateSimulationRequest) -> Result<String, ApiError> {
        // Public / frontend path: ignore client-supplied id / multicast / assignment.
        let topo = request.topology;
        let id = new_cluster_key();
        let (plan, prior_remaining, prior_mcast_len) = {
            let mut state = self.lock()?;
            let plan = self.plan_create(&state, &topo)?;
            let prior_remaining = state.remaining.clone();
            let prior_mcast_len = state.used_mcast.len();
            // Reserve capacity / multicast before fanout so concurrent creates cannot
            // double-book the same slots or group.
            state.remaining = plan.remaining.clone();
            if let Some(pair) = plan.mcast {
                state.used_mcast.push(pair);
            }
            state.sims.insert(
                id.clone(),
                SimRecord {
                    per_server: plan.per_server.clone(),
                    mcast: plan.mcast,
                },
            );
            (plan, prior_remaining, prior_mcast_len)
        };

        let mcast_wire = plan.mcast.map(|(addr, port)| CreateMulticast {
            address: addr.to_string(),
            port,
        });

        let rollback = |succeeded: &[usize]| {
            for &prev in succeeded {
                let _ = self.servers[prev].backend.shutdown(&id);
            }
            if let Ok(mut state) = self.lock() {
                state.sims.remove(&id);
                state.remaining = prior_remaining.clone();
                state.used_mcast.truncate(prior_mcast_len);
            }
        };

        let mut succeeded: Vec<usize> = Vec::new();
        for &(server_idx, ref boards) in &plan.per_server {
            let req = CreateSimulationRequest {
                topology: topo.clone(),
                id: Some(id.clone()),
                multicast: mcast_wire.clone(),
                assigned_boards: Some(boards.clone()),
            };
            match self.servers[server_idx].backend.create(req) {
                Ok(remote_id) => {
                    if remote_id != id {
                        let _ = self.servers[server_idx].backend.shutdown(&remote_id);
                        rollback(&succeeded);
                        return Err(ApiError::internal(format!(
                            "backend {} returned id {remote_id}, expected {id}",
                            self.servers[server_idx].endpoint
                        )));
                    }
                    succeeded.push(server_idx);
                }
                Err(err) => {
                    let mut compensate_ok = true;
                    for &prev in &succeeded {
                        if self.servers[prev].backend.shutdown(&id).is_err() {
                            compensate_ok = false;
                        }
                    }
                    if let Ok(mut state) = self.lock() {
                        state.sims.remove(&id);
                        state.remaining = prior_remaining;
                        state.used_mcast.truncate(prior_mcast_len);
                    }
                    let msg = if compensate_ok {
                        format!(
                            "create failed on {}: {err}; compensated",
                            self.servers[server_idx].endpoint
                        )
                    } else {
                        format!(
                            "create failed on {}: {err}; compensate incomplete — remnant may exist with id {id}",
                            self.servers[server_idx].endpoint
                        )
                    };
                    return Err(ApiError::new(err.status, err.code, msg));
                }
            }
        }

        Ok(id)
    }

    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
        // p1d will fan out; until then hit the primary participant only.
        self.primary_backend(id)?.start(id, until_ns)
    }

    fn stop(&self, id: &str) -> Result<(), ApiError> {
        self.primary_backend(id)?.stop(id)
    }

    fn reset(&self, id: &str) -> Result<(), ApiError> {
        self.primary_backend(id)?.reset(id)
    }

    fn shutdown(&self, id: &str) -> Result<(), ApiError> {
        let rec = {
            let mut state = self.lock()?;
            state.sims.remove(id)
        };
        let Some(rec) = rec else {
            // Best-effort: try every backend.
            let mut last_err = None;
            let mut any = false;
            for slot in &self.servers {
                match slot.backend.shutdown(id) {
                    Ok(()) => any = true,
                    Err(err) if err.code == "not_found" => {}
                    Err(err) => last_err = Some(err),
                }
            }
            return if any {
                Ok(())
            } else {
                Err(last_err.unwrap_or_else(|| ApiError::not_found("unknown simulation")))
            };
        };

        let mut first_err: Option<ApiError> = None;
        for &(idx, _) in &rec.per_server {
            if let Err(err) = self.servers[idx].backend.shutdown(id) {
                if first_err.is_none() {
                    first_err = Some(err);
                }
            } else {
                let mut state = self.lock()?;
                let n = rec
                    .per_server
                    .iter()
                    .find(|(i, _)| *i == idx)
                    .map(|(_, boards)| boards.len() as u32)
                    .unwrap_or(0);
                state.remaining[idx] = state.remaining[idx]
                    .saturating_add(n)
                    .min(self.servers[idx].max_nodes);
            }
        }
        if let Some(pair) = rec.mcast {
            let mut state = self.lock()?;
            state.used_mcast.retain(|p| *p != pair);
        }
        match first_err {
            Some(err) => Err(ApiError::new(
                err.status,
                err.code,
                format!("{err}; some backends may still hold simulation {id}"),
            )),
            None => Ok(()),
        }
    }

    fn exists(&self, id: &str) -> Result<bool, ApiError> {
        {
            let state = self.lock()?;
            if state.sims.contains_key(id) {
                return Ok(true);
            }
        }
        for slot in &self.servers {
            if slot.backend.exists(id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
        self.primary_backend(id)?.status(id)
    }

    fn topology(&self, id: &str) -> Result<TopologyView, ApiError> {
        self.primary_backend(id)?.topology(id)
    }

    fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError> {
        self.primary_backend(id)?.logs_after(id, after)
    }

    fn log_high_water(&self, id: &str) -> Result<u64, ApiError> {
        self.primary_backend(id)?.log_high_water(id)
    }

    fn actions(&self, id: &str) -> Result<ActionList, ApiError> {
        self.primary_backend(id)?.actions(id)
    }

    fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError> {
        self.primary_backend(id)?.invoke_action(id, request)
    }

    fn probe_catalog(&self, id: &str) -> Result<Vec<crate::probe_log::ProbeCatalogItem>, ApiError> {
        self.primary_backend(id)?.probe_catalog(id)
    }

    fn probe_watch_get(&self, id: &str) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        self.primary_backend(id)?.probe_watch_get(id)
    }

    fn probe_watch_put(
        &self,
        id: &str,
        probes: Vec<crate::probe_log::ProbeRef>,
    ) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        self.primary_backend(id)?.probe_watch_put(id, probes)
    }

    fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
        self.primary_backend(id)?.probe_watch_clear(id)
    }

    fn probe_watch_live(
        &self,
        id: &str,
        after: u64,
    ) -> Result<Vec<crate::probe_log::CoarseBatch>, ApiError> {
        self.primary_backend(id)?.probe_watch_live(id, after)
    }

    fn csv_start(
        &self,
        id: &str,
        range: crate::probe_log::ProbeRange,
    ) -> Result<(u64, Vec<crate::probe_log::ProbePointName>), ApiError> {
        self.primary_backend(id)?.csv_start(id, range)
    }

    fn csv_poll(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        self.primary_backend(id)?.csv_poll(id, request_id)
    }

    fn csv_status(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        self.primary_backend(id)?.csv_status(id, request_id)
    }

    fn probe_samples(
        &self,
        id: &str,
        board: crate::ipc::HashedId<u64>,
        probe: crate::ipc::HashedId<u64>,
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Result<crate::contract::ProbeSamplesResponse, ApiError> {
        self.primary_backend(id)?
            .probe_samples(id, board, probe, from_ns, to_ns)
    }

    fn viewer_attach(&self, id: &str) -> Result<String, ApiError> {
        self.primary_backend(id)?.viewer_attach(id)
    }

    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError> {
        self.primary_backend(id)?.viewer_check(id, token)
    }

    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError> {
        self.primary_backend(id)?.viewer_detach(id, token)
    }

    fn inspect(
        &self,
        id: &str,
        board_id: crate::ipc::HashedId<u64>,
        req: InspectRequest,
    ) -> Result<InspectReply, ApiError> {
        self.primary_backend(id)?.inspect(id, board_id, req)
    }

    fn step(&self, id: &str, board_id: crate::ipc::HashedId<u64>) -> Result<(), ApiError> {
        self.primary_backend(id)?.step(id, board_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{BoardSpec, DirectedEdge, PayloadKind, PortRef};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockBackend {
        name: String,
        creates: AtomicUsize,
        fail_create: bool,
        live: Mutex<HashMap<String, Vec<String>>>,
    }

    impl MockBackend {
        fn new(name: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                creates: AtomicUsize::new(0),
                fail_create: false,
                live: Mutex::new(HashMap::new()),
            })
        }

        fn failing(name: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                creates: AtomicUsize::new(0),
                fail_create: true,
                live: Mutex::new(HashMap::new()),
            })
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
            Ok(id)
        }

        fn shutdown(&self, id: &str) -> Result<(), ApiError> {
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
            Ok(())
        }
        fn stop(&self, _: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn reset(&self, _: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn status(&self, _: &str) -> Result<StatusEvent, ApiError> {
            Err(ApiError::internal("unused"))
        }
        fn topology(&self, _: &str) -> Result<TopologyView, ApiError> {
            Err(ApiError::internal("unused"))
        }
        fn logs_after(&self, _: &str, _: u64) -> Result<Vec<LogEvent>, ApiError> {
            Ok(vec![])
        }
        fn log_high_water(&self, _: &str) -> Result<u64, ApiError> {
            Ok(0)
        }
        fn actions(&self, _: &str) -> Result<ActionList, ApiError> {
            Err(ApiError::internal("unused"))
        }
        fn invoke_action(&self, _: &str, _: InvokeActionRequest) -> Result<(), ApiError> {
            Ok(())
        }
        fn probe_catalog(
            &self,
            _: &str,
        ) -> Result<Vec<crate::probe_log::ProbeCatalogItem>, ApiError> {
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
        let fe = ClusterFrontend::new(vec![(b0.clone(), "s0".into(), 4)], vec![], vec![]).unwrap();
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
                (MockBackend::new("s0"), "s0".into(), 1),
                (MockBackend::new("s1"), "s1".into(), 1),
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
            vec![(b0.clone(), "s0".into(), 1), (b1.clone(), "s1".into(), 1)],
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
        assert_eq!(
            b0.live.lock().unwrap().get(&id).unwrap(),
            &vec!["a".to_string()]
        );
        assert_eq!(
            b1.live.lock().unwrap().get(&id).unwrap(),
            &vec!["b".to_string()]
        );
        let state = fe.lock().unwrap();
        assert_eq!(state.used_mcast, vec![(Ipv4Addr::new(239, 0, 0, 1), 30000)]);
        assert_eq!(state.remaining, vec![0, 0]);
    }

    #[test]
    fn partial_create_compensates() {
        let b0 = MockBackend::new("s0");
        let b1 = MockBackend::failing("s1");
        let fe = ClusterFrontend::new(
            vec![(b0.clone(), "s0".into(), 1), (b1.clone(), "s1".into(), 1)],
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
        assert_eq!(state.remaining, vec![1, 1]);
        assert!(state.used_mcast.is_empty());
    }

    #[test]
    fn shutdown_frees_capacity_and_mcast() {
        let b0 = MockBackend::new("s0");
        let b1 = MockBackend::new("s1");
        let fe = ClusterFrontend::new(
            vec![(b0.clone(), "s0".into(), 1), (b1.clone(), "s1".into(), 1)],
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
        assert!(state.used_mcast.is_empty());
        assert_eq!(state.remaining, vec![1, 1]);
    }
}

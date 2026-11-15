//! Frontend placement: First-Fit, multicast allocate, create fanout + compensate.
//! Control fanout and status/log aggregate across participating backends (p1d).

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::SimulationBackend;
use crate::cluster_mcast::{allocate_multicast, has_cross_host_edge, place_boards_first_fit};
use crate::cluster_observe::LogAggregate;
use crate::config::MachineId;
use crate::contract::{
    ActionList, ApiError, CreateMulticast, CreateSimulationRequest, InvokeActionRequest, LogEvent,
    StatusEvent, TopologyView,
};
use crate::inspect::{InspectReply, InspectRequest};
use crate::ipc::new_cluster_key;
use crate::topology::LogicalTopology;

/// Bound for status/log fanout so one unreachable backend cannot freeze SSE
/// for the full TCP RPC timeout (60s). Timed-out participants are omitted.
const OBSERVE_FANOUT_TIMEOUT: Duration = Duration::from_secs(2);

struct BackendSlot {
    machine_id: MachineId,
    backend: Arc<dyn SimulationBackend>,
    endpoint: String,
    max_nodes: u32,
}

#[derive(Clone)]
struct SimRecord {
    /// Participating backends and the board ids assigned to each.
    per_server: HashMap<MachineId, Vec<String>>,
    mcast: Option<(Ipv4Addr, u16)>,
}

struct FrontendState {
    sims: HashMap<String, SimRecord>,
    logs: HashMap<String, LogAggregate>,
}

/// Live ledger participants keyed by `MachineId`.
struct Participants {
    per_server: HashMap<MachineId, Vec<String>>,
    backends: HashMap<MachineId, Arc<dyn SimulationBackend>>,
}

impl Participants {
    /// Ascending `MachineId` order for stable status/log merge.
    fn machine_order(&self) -> Vec<MachineId> {
        let mut ids: Vec<_> = self.backends.keys().copied().collect();
        ids.sort_by_key(|id| id.as_u32());
        ids
    }
}

/// Multi-backend create planner that implements [`SimulationBackend`].
pub struct ClusterFrontend {
    servers: HashMap<MachineId, BackendSlot>,
    address_ranges: Vec<(Ipv4Addr, Ipv4Addr)>,
    port_ranges: Vec<(u16, u16)>,
    state: Mutex<FrontendState>,
    /// Serializes log pull+ingest so concurrent `log_high_water` / `logs_after`
    /// cannot advance global seq under another caller's feet (SSE gap).
    log_fetch: Mutex<()>,
}

impl ClusterFrontend {
    /// Build from configured backends keyed by `machine_id`.
    ///
    /// Each entry is `(backend, endpoint, max_nodes, machine_id)`.
    pub fn new(
        servers: Vec<(Arc<dyn SimulationBackend>, String, u32, MachineId)>,
        address_ranges: Vec<(Ipv4Addr, Ipv4Addr)>,
        port_ranges: Vec<(u16, u16)>,
    ) -> Result<Self, ApiError> {
        if servers.is_empty() {
            return Err(ApiError::internal("ClusterFrontend: no servers"));
        }
        let mut slots = HashMap::new();
        for (backend, endpoint, max_nodes, machine_id) in servers {
            if slots.contains_key(&machine_id) {
                return Err(ApiError::internal(format!(
                    "ClusterFrontend: duplicate machine_id {machine_id}"
                )));
            }
            slots.insert(
                machine_id,
                BackendSlot {
                    machine_id,
                    backend,
                    endpoint,
                    max_nodes,
                },
            );
        }
        Ok(Self {
            servers: slots,
            address_ranges,
            port_ranges,
            state: Mutex::new(FrontendState {
                sims: HashMap::new(),
                logs: HashMap::new(),
            }),
            log_fetch: Mutex::new(()),
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

    /// Multicast pairs reserved by live `SimRecord`s (no separate used_mcast cache).
    fn used_multicast(state: &FrontendState) -> Vec<(Ipv4Addr, u16)> {
        state.sims.values().filter_map(|rec| rec.mcast).collect()
    }

    /// Free board slots per backend: `max_nodes` minus boards assigned in `sims`.
    fn free_capacity(&self, state: &FrontendState) -> HashMap<MachineId, u32> {
        let mut used: HashMap<MachineId, u32> = HashMap::new();
        for rec in state.sims.values() {
            for (machine_id, boards) in &rec.per_server {
                *used.entry(*machine_id).or_insert(0) += boards.len() as u32;
            }
        }
        let mut free = HashMap::with_capacity(self.servers.len());
        for (machine_id, slot) in &self.servers {
            let taken = used.get(machine_id).copied().unwrap_or(0);
            free.insert(*machine_id, slot.max_nodes.saturating_sub(taken));
        }
        free
    }

    /// First-Fit placement + optional multicast allocation as a `SimRecord`.
    /// Caller inserts it into `sims` to reserve board slots and multicast.
    fn allocate_sim_record(
        &self,
        state: &FrontendState,
        topo: &LogicalTopology,
    ) -> Result<SimRecord, ApiError> {
        let board_ids: Vec<String> = topo.boards.iter().map(|b| b.id.clone()).collect();
        let free = self.free_capacity(state);
        let placement = place_boards_first_fit(&board_ids, &free)
            .map_err(|err| ApiError::bad_request("insufficient_capacity", err.to_string()))?;

        let board_server: HashMap<String, MachineId> =
            placement.board_to_server.iter().cloned().collect();
        let edges = Self::edge_pairs(topo);
        // TODO(cluster-distributed-p2): remove once cross-host endpoint multicast exists.
        if has_cross_host_edge(&edges, &board_server) {
            return Err(ApiError::bad_request(
                "cross_host_edge",
                "topology has an edge whose endpoints land on different backends (unsupported until p2)",
            ));
        }

        let mut per_server: HashMap<MachineId, Vec<String>> = HashMap::new();
        for (board_id, machine_id) in &placement.board_to_server {
            per_server
                .entry(*machine_id)
                .or_default()
                .push(board_id.clone());
        }

        let mcast = if per_server.len() >= 2 {
            if self.address_ranges.is_empty() || self.port_ranges.is_empty() {
                return Err(ApiError::bad_request(
                    "multicast_exhausted",
                    "multi-backend placement requires multicast address/port ranges",
                ));
            }
            let used = Self::used_multicast(state);
            let Some(pair) = allocate_multicast(&self.address_ranges, &self.port_ranges, &used)
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

        Ok(SimRecord { per_server, mcast })
    }

    /// Drop this sim's `SimRecord` after compensation cleaned backends.
    /// Board-slot and multicast reservations are derived from `sims`, so remove
    /// alone frees both. Also drops the log aggregate.
    fn rollback_ledger(&self, id: &str) {
        if let Ok(mut state) = self.lock() {
            state.sims.remove(id);
            state.logs.remove(id);
        }
    }

    /// Participating backends for a live ledger entry (I/O-ready clones).
    ///
    /// Requires a `SimRecord` in the frontend ledger (no exists-probe fallback).
    fn participants(&self, id: &str) -> Result<Participants, ApiError> {
        let state = self.lock()?;
        let Some(rec) = state.sims.get(id) else {
            return Err(ApiError::not_found("unknown simulation"));
        };
        let per_server = rec.per_server.clone();
        let mut backends = HashMap::with_capacity(per_server.len());
        for &machine_id in per_server.keys() {
            let backend = self
                .servers
                .get(&machine_id)
                .ok_or_else(|| {
                    ApiError::internal(format!("missing backend for machine_id {machine_id}"))
                })?
                .backend
                .clone();
            backends.insert(machine_id, backend);
        }
        Ok(Participants {
            per_server,
            backends,
        })
    }

    /// Run `f` on every participant and wait for all (control plane).
    /// Single-participant skips thread spawn.
    fn parallel_map<T: Send>(
        backends: &HashMap<MachineId, Arc<dyn SimulationBackend>>,
        f: impl Fn(MachineId, &dyn SimulationBackend) -> Result<T, ApiError> + Sync + Send,
    ) -> Vec<(MachineId, Result<T, ApiError>)> {
        if backends.is_empty() {
            return Vec::new();
        }
        if backends.len() == 1 {
            let (machine_id, backend) = backends.iter().next().expect("len checked");
            return vec![(*machine_id, f(*machine_id, backend.as_ref()))];
        }
        thread::scope(|scope| {
            let f = &f;
            let handles: Vec<_> = backends
                .iter()
                .map(|(machine_id, backend)| {
                    let machine_id = *machine_id;
                    let backend = Arc::clone(backend);
                    (
                        machine_id,
                        scope.spawn(move || f(machine_id, backend.as_ref())),
                    )
                })
                .collect();
            handles
                .into_iter()
                .map(|(machine_id, handle)| match handle.join() {
                    Ok(result) => (machine_id, result),
                    Err(_) => (
                        machine_id,
                        Err(ApiError::internal("participant worker panicked")),
                    ),
                })
                .collect()
        })
    }

    /// Observe fanout: return as soon as `timeout` elapses. Slow/unreachable
    /// participants become `Err` and are omitted by status/log merge. Workers
    /// are detached so a hung RPC cannot block the caller until TCP timeout.
    fn parallel_map_timeout<T: Send + 'static>(
        backends: &HashMap<MachineId, Arc<dyn SimulationBackend>>,
        timeout: Duration,
        f: impl Fn(MachineId, &dyn SimulationBackend) -> Result<T, ApiError> + Send + Sync + 'static,
    ) -> Vec<(MachineId, Result<T, ApiError>)> {
        if backends.is_empty() {
            return Vec::new();
        }
        let f = Arc::new(f);
        let (tx, rx) = mpsc::channel();
        let mut pending = HashSet::with_capacity(backends.len());
        for (&machine_id, backend) in backends {
            pending.insert(machine_id);
            let tx = tx.clone();
            let backend = Arc::clone(backend);
            let f = Arc::clone(&f);
            thread::spawn(move || {
                let result = f(machine_id, backend.as_ref());
                let _ = tx.send((machine_id, result));
            });
        }
        drop(tx);
        let deadline = Instant::now() + timeout;
        let mut results = Vec::with_capacity(backends.len());
        while !pending.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok((machine_id, result)) => {
                    pending.remove(&machine_id);
                    results.push((machine_id, result));
                }
                Err(mpsc::RecvTimeoutError::Timeout)
                | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }
        for machine_id in pending {
            results.push((
                machine_id,
                Err(ApiError::internal("observe fanout timed out")),
            ));
        }
        results
    }

    /// Parallel unit call to every participant. Returns the error from the
    /// lowest `MachineId` among failures (after all calls finish).
    fn control_fanout(
        &self,
        id: &str,
        call: impl Fn(&dyn SimulationBackend) -> Result<(), ApiError> + Sync + Send,
    ) -> Result<(), ApiError> {
        let participants = self.participants(id)?;
        let results = Self::parallel_map(&participants.backends, |_, backend| call(backend));
        let mut failures: Vec<(MachineId, ApiError)> = results
            .into_iter()
            .filter_map(|(machine_id, result)| match result {
                Ok(()) => None,
                Err(err) => Some((machine_id, err)),
            })
            .collect();
        if failures.is_empty() {
            return Ok(());
        }
        failures.sort_by_key(|(machine_id, _)| machine_id.as_u32());
        Err(failures.remove(0).1)
    }

    /// Fetch new local logs, then ingest and read under one mutex hold so a
    /// concurrent pull cannot trim events before this caller snapshots them.
    ///
    /// The whole pull+ingest is also gated by `log_fetch` so another client's
    /// ingest cannot advance global seq while this call's `log_high_water` is
    /// still in flight (would make a new SSE skip those lines forever).
    fn with_pulled_logs<R>(
        &self,
        id: &str,
        read: impl FnOnce(&LogAggregate) -> R,
    ) -> Result<R, ApiError> {
        let _fetch = self
            .log_fetch
            .lock()
            .map_err(|_| ApiError::internal("ClusterFrontend log_fetch poisoned"))?;
        let participants = self.participants(id)?;
        let cursors: HashMap<MachineId, u64> = {
            let state = self.lock()?;
            let Some(agg) = state.logs.get(id) else {
                return Err(ApiError::not_found("unknown simulation"));
            };
            participants
                .backends
                .keys()
                .map(|machine_id| (*machine_id, agg.after_for(*machine_id)))
                .collect()
        };
        let id_owned = id.to_string();
        let fetched = Self::parallel_map_timeout(
            &participants.backends,
            OBSERVE_FANOUT_TIMEOUT,
            move |machine_id, backend| {
                let after = cursors.get(&machine_id).copied().unwrap_or(0);
                backend.logs_after(&id_owned, after)
            },
        );
        let mut by_machine: HashMap<MachineId, Vec<LogEvent>> = HashMap::new();
        for (machine_id, result) in fetched {
            if let Ok(events) = result {
                by_machine.insert(machine_id, events);
            }
        }
        let mut batches = Vec::new();
        for machine_id in participants.machine_order() {
            if let Some(events) = by_machine.remove(&machine_id) {
                batches.push((machine_id, events));
            }
        }
        let mut state = self.lock()?;
        let Some(agg) = state.logs.get_mut(id) else {
            return Err(ApiError::not_found("unknown simulation"));
        };
        agg.ingest(batches);
        Ok(read(agg))
    }

    /// Shut down `id` on every backend in `succeeded`. Returns false if any
    /// shutdown fails or a machine_id is missing from `self.servers`.
    fn compensate_previous(&self, succeeded: &[MachineId], id: &str) -> bool {
        let mut ok = true;
        for &prev in succeeded {
            match self.servers.get(&prev) {
                Some(slot) if slot.backend.shutdown(id).is_ok() => {}
                _ => ok = false,
            }
        }
        ok
    }

    /// Roll back the ledger when compensation succeeded, then return the create
    /// error. When compensation failed, leave the `SimRecord` pinned so another
    /// create cannot reuse its board slots / multicast until `shutdown(id)`
    /// tears down remnants.
    fn fail_after_compensate(
        &self,
        id: &str,
        head: String,
        compensate_ok: bool,
        err: Option<ApiError>,
    ) -> Result<String, ApiError> {
        if compensate_ok {
            self.rollback_ledger(id);
        }
        let msg = if compensate_ok {
            format!("{head}; compensated")
        } else {
            format!("{head}; compensate incomplete — remnant may exist with id {id}")
        };
        match err {
            Some(err) => Err(ApiError::new(err.status, err.code, msg)),
            None => Err(ApiError::internal(msg)),
        }
    }

    /// One participating backend (HashMap key order). Interim for APIs that do
    /// not yet fan out, aggregate, or route to the hosting backend — see
    /// `TODO(cluster-distributed-p*)` on those call sites.
    fn primary_backend(&self, id: &str) -> Result<Arc<dyn SimulationBackend>, ApiError> {
        let state = self.lock()?;
        let Some(rec) = state.sims.get(id) else {
            // Fall back: probe servers (e.g. after restart without state).
            drop(state);
            for slot in self.servers.values() {
                if slot.backend.exists(id)? {
                    return Ok(slot.backend.clone());
                }
            }
            return Err(ApiError::not_found("unknown simulation"));
        };
        let machine_id = *rec
            .per_server
            .keys()
            .next()
            .ok_or_else(|| ApiError::internal("simulation has no participating backends"))?;
        self.servers
            .get(&machine_id)
            .map(|slot| slot.backend.clone())
            .ok_or_else(|| {
                ApiError::internal(format!("missing backend for machine_id {machine_id}"))
            })
    }

    fn slot(&self, machine_id: MachineId) -> Result<&BackendSlot, ApiError> {
        let slot = self.servers.get(&machine_id).ok_or_else(|| {
            ApiError::internal(format!("missing backend for machine_id {machine_id}"))
        })?;
        debug_assert_eq!(slot.machine_id, machine_id);
        Ok(slot)
    }
}

impl SimulationBackend for ClusterFrontend {
    fn create(&self, request: CreateSimulationRequest) -> Result<String, ApiError> {
        // Public / frontend path: ignore client-supplied id / multicast / assignment.
        let topo = request.topology;
        let id = new_cluster_key();
        let record = {
            let mut state = self.lock()?;
            let record = self.allocate_sim_record(&state, &topo)?;
            // Inserting the record reserves board slots and multicast (derived from
            // sims) before fanout. Log aggregate is created with the ledger entry.
            state.sims.insert(id.clone(), record.clone());
            state.logs.insert(id.clone(), LogAggregate::new());
            record
        };

        let mcast_wire = record.mcast.map(|(addr, port)| CreateMulticast {
            address: addr.to_string(),
            port,
        });

        let mut succeeded: Vec<MachineId> = Vec::new();
        for (&machine_id, boards) in &record.per_server {
            let (backend, endpoint) = {
                let slot = self.slot(machine_id)?;
                (slot.backend.clone(), slot.endpoint.clone())
            };
            let req = CreateSimulationRequest {
                topology: topo.clone(),
                id: Some(id.clone()),
                multicast: mcast_wire.clone(),
                assigned_boards: Some(boards.clone()),
            };
            match backend.create(req) {
                Ok(remote_id) if remote_id == id => {
                    succeeded.push(machine_id);
                }
                Ok(remote_id) => {
                    let cleanup_ok = backend.shutdown(&remote_id).is_ok();
                    let compensate_ok = cleanup_ok && self.compensate_previous(&succeeded, &id);
                    return self.fail_after_compensate(
                        &id,
                        format!("backend {endpoint} returned id {remote_id}, expected {id}"),
                        compensate_ok,
                        None,
                    );
                }
                Err(err) => {
                    let compensate_ok = self.compensate_previous(&succeeded, &id);
                    return self.fail_after_compensate(
                        &id,
                        format!("create failed on {endpoint}: {err}"),
                        compensate_ok,
                        Some(err),
                    );
                }
            }
        }

        Ok(id)
    }

    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
        // Arbiter `broadcast_start` stays local iceoryx; cross-host is frontend fanout.
        self.control_fanout(id, |backend| backend.start(id, until_ns))
    }

    fn stop(&self, id: &str) -> Result<(), ApiError> {
        self.control_fanout(id, |backend| backend.stop(id))
    }

    fn reset(&self, id: &str) -> Result<(), ApiError> {
        self.control_fanout(id, |backend| backend.reset(id))
    }

    fn shutdown(&self, id: &str) -> Result<(), ApiError> {
        let rec = {
            let state = self.lock()?;
            state.sims.get(id).cloned()
        };
        let Some(rec) = rec else {
            // Best-effort: try every backend.
            let mut last_err = None;
            let mut any = false;
            for slot in self.servers.values() {
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

        // Release only MachineIds this call confirmed gone; merge into the live
        // ledger under the mutex so concurrent shutdowns cannot resurrect slots.
        let mut first_err: Option<ApiError> = None;
        let mut released: Vec<MachineId> = Vec::new();
        for &machine_id in rec.per_server.keys() {
            let Some(slot) = self.servers.get(&machine_id) else {
                if first_err.is_none() {
                    first_err = Some(ApiError::internal(format!(
                        "missing backend for machine_id {machine_id}"
                    )));
                }
                continue;
            };
            match slot.backend.shutdown(id) {
                Ok(()) => released.push(machine_id),
                Err(err) if err.code == "not_found" => released.push(machine_id),
                Err(err) => {
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }

        let mut state = self.lock()?;
        let emptied = if let Some(entry) = state.sims.get_mut(id) {
            for machine_id in &released {
                entry.per_server.remove(machine_id);
            }
            entry.per_server.is_empty()
        } else {
            false
        };
        if emptied {
            state.sims.remove(id);
            state.logs.remove(id);
        }
        let still_present = state.sims.contains_key(id);

        match first_err {
            Some(err) if still_present => Err(ApiError::new(
                err.status,
                err.code,
                format!("{err}; some backends may still hold simulation {id}"),
            )),
            _ => Ok(()),
        }
    }

    fn exists(&self, id: &str) -> Result<bool, ApiError> {
        {
            let state = self.lock()?;
            if state.sims.contains_key(id) {
                return Ok(true);
            }
        }
        for slot in self.servers.values() {
            if slot.backend.exists(id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
        let participants = self.participants(id)?;
        let id_owned = id.to_string();
        let results = Self::parallel_map_timeout(
            &participants.backends,
            OBSERVE_FANOUT_TIMEOUT,
            move |_, backend| backend.status(&id_owned),
        );
        let mut ok = Vec::new();
        let mut errs = Vec::new();
        for (machine_id, result) in results {
            match result {
                Ok(status) => ok.push((machine_id, status)),
                Err(err) => errs.push((machine_id, err)),
            }
        }
        if ok.is_empty() {
            errs.sort_by_key(|(machine_id, _)| machine_id.as_u32());
            return Err(errs
                .into_iter()
                .next()
                .map(|(_, err)| err)
                .unwrap_or_else(|| ApiError::not_found("unknown simulation")));
        }
        let order = participants.machine_order();
        Ok(crate::cluster_observe::merge_status(
            id,
            &participants.per_server,
            &order,
            &ok,
        ))
    }

    fn topology(&self, id: &str) -> Result<TopologyView, ApiError> {
        // Full topology is sent to every participant at create, so one backend
        // suffices; not deferred aggregation.
        self.primary_backend(id)?.topology(id)
    }

    fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError> {
        self.with_pulled_logs(id, |agg| agg.after(after))
    }

    fn log_high_water(&self, id: &str) -> Result<u64, ApiError> {
        // Pull+ingest first so SSE subscribe skips backlog already on backends
        // (same "from connection onward" contract as a single LocalBackend).
        self.with_pulled_logs(id, LogAggregate::high_water)
    }

    fn actions(&self, id: &str) -> Result<ActionList, ApiError> {
        // TODO(cluster-distributed-p3): route to hosting backend(s) only.
        self.primary_backend(id)?.actions(id)
    }

    fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError> {
        // TODO(cluster-distributed-p3): route to the backend that hosts the target.
        self.primary_backend(id)?.invoke_action(id, request)
    }

    fn probe_catalog(&self, id: &str) -> Result<Vec<crate::probe_log::ProbeCatalogItem>, ApiError> {
        // TODO(cluster-distributed-p3): route / merge from hosting backends.
        self.primary_backend(id)?.probe_catalog(id)
    }

    fn probe_watch_get(&self, id: &str) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        // TODO(cluster-distributed-p3): route to hosting backend(s) only.
        self.primary_backend(id)?.probe_watch_get(id)
    }

    fn probe_watch_put(
        &self,
        id: &str,
        probes: Vec<crate::probe_log::ProbeRef>,
    ) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        // TODO(cluster-distributed-p3): route to hosting backend(s) only.
        self.primary_backend(id)?.probe_watch_put(id, probes)
    }

    fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
        // TODO(cluster-distributed-p3): route to hosting backend(s) only.
        self.primary_backend(id)?.probe_watch_clear(id)
    }

    fn probe_watch_live(
        &self,
        id: &str,
        after: u64,
    ) -> Result<Vec<crate::probe_log::CoarseBatch>, ApiError> {
        // TODO(cluster-distributed-p3): route / merge from hosting backends.
        self.primary_backend(id)?.probe_watch_live(id, after)
    }

    fn csv_start(
        &self,
        id: &str,
        range: crate::probe_log::ProbeRange,
    ) -> Result<(u64, Vec<crate::probe_log::ProbePointName>), ApiError> {
        // TODO(cluster-distributed-p4): start CSV on hosting backends; aggregate later.
        self.primary_backend(id)?.csv_start(id, range)
    }

    fn csv_poll(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        // TODO(cluster-distributed-p4): aggregate CSV poll across backends (time-sort).
        self.primary_backend(id)?.csv_poll(id, request_id)
    }

    fn csv_status(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        // TODO(cluster-distributed-p4): aggregate CSV status across backends.
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
        // TODO(cluster-distributed-p3): route to the backend that hosts `board`.
        self.primary_backend(id)?
            .probe_samples(id, board, probe, from_ns, to_ns)
    }

    fn viewer_attach(&self, id: &str) -> Result<String, ApiError> {
        // TODO(cluster-distributed): viewer routing across backends is unspecified;
        // interim primary only.
        self.primary_backend(id)?.viewer_attach(id)
    }

    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError> {
        // TODO(cluster-distributed): viewer routing across backends is unspecified;
        // interim primary only.
        self.primary_backend(id)?.viewer_check(id, token)
    }

    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError> {
        // TODO(cluster-distributed): viewer routing across backends is unspecified;
        // interim primary only.
        self.primary_backend(id)?.viewer_detach(id, token)
    }

    fn inspect(
        &self,
        id: &str,
        board_id: crate::ipc::HashedId<u64>,
        req: InspectRequest,
    ) -> Result<InspectReply, ApiError> {
        // TODO(cluster-distributed-p3): route to the backend that hosts `board_id`.
        self.primary_backend(id)?.inspect(id, board_id, req)
    }

    fn step(&self, id: &str, board_id: crate::ipc::HashedId<u64>) -> Result<(), ApiError> {
        // TODO(cluster-distributed-p3): route to the backend that hosts `board_id`.
        self.primary_backend(id)?.step(id, board_id)
    }
}

#[cfg(test)]
#[path = "cluster_frontend_tests.rs"]
mod tests;

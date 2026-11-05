//! Frontend-side [`SimulationBackend`] over several `cluster-backend` servers.
//!
//! `create` runs the two-phase orchestration in [`super::create`].
//!
//! Operations that read the whole simulation fan in over **every** backend that
//! hosts a part of it (the `members` of the simulation), asking each with its
//! own simulation id and folding the answers into one ([`super::aggregate`]):
//!
//! - `status`: members' nodes concatenated; one `exited` member -> `exited`;
//!   a member not ready (nodes not past the ready barrier, or probe catalog
//!   not complete) -> `starting`; one member not `running` -> `stopped`.
//! - `probe_catalog`, `actions`: concatenated and sorted. `probe_catalog`
//!   answers `503 not_ready` while any member's catalog is incomplete;
//!   `actions` answers an empty list until every member's catalog is complete
//!   (a backend has no separate "actions complete" signal, so its probe
//!   catalog completeness, reached at the same ready barrier, is used).
//! - `csv_*`: the frontend's request id stands for one export per member;
//!   rows are released once all members are done, sorted by virtual time.
//! - `logs_after`, `log_high_water`: lines of all members ordered by virtual
//!   time. The client cursor is a `seq` this frontend assigns to the merged
//!   lines (members' own `seq` stay internal), so the SSE log stream keeps
//!   working unchanged.
//!
//! Requests that name one board go **only** to the backend that owns that
//! board (looked up in the simulation's board -> backend assignment made at
//! create time): `invoke_action`, `inspect`, `probe_samples`. A board the
//! assignment does not contain is answered `404 not_found` without calling any
//! backend.
//!
//! The probe watch is a list of probes of several boards. `probe_watch_put`
//! splits the list by owning backend and sends each part only to its owner;
//! a backend that held a watch from an earlier PUT but owns none of the new
//! probes gets an empty list (so its old watch is replaced). `probe_watch_get`,
//! `probe_watch_clear` and `probe_watch_live` only talk to the backends that
//! currently hold a watch (those named by earlier PUTs), never to the others.
//! `probe_watch_live` batches of those backends are re-numbered with one
//! `seq` of this frontend, ordered by the virtual time of their first sample.
//!
//! A simulation with a single member, or an id this process did not create,
//! is forwarded unchanged to that backend.
//!
//! `start`, `stop`, `reset` and `shutdown` fan out to every participant over
//! HTTP, each under its own simulation id (not via the arbiter control TCP,
//! which carries node-originated stops). `start`/`stop`/`reset` are best
//! effort: all members are attempted, nothing is rolled back, the first error
//! is returned. `shutdown` also finishes the cleanup of a failed create whose
//! rollback was incomplete. Known gap (later task): viewer attach/check/detach
//! and `topology` still reach the primary only.
//!
//! `step` goes only to the backend that owns the board like `inspect` does;
//! that backend's arbiter tells the other arbiters over the control TCP mesh
//! (`cluster_net::ControlMessage::StepBegin` and the messages after it).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::backend::{SimulationBackend, TcpipBackend};
use crate::config::ServerAddr;
use crate::contract::{
    ActionList, ApiError, InvokeActionRequest, LogEvent, ProbeSamplesResponse, StatusEvent,
    TopologyView,
};
use crate::inspect::{InspectReply, InspectRequest};
use crate::probe_log::{
    CoarseBatch, CsvPoll, ProbeCatalogItem, ProbePointName, ProbeRange, ProbeRef, ProbeWatchView,
};
use crate::topology::LogicalTopology;

use super::aggregate::{
    CsvMerge, MergedLog, all_nodes_ready, any_exited, merge_actions, merge_csv_catalogs,
    merge_probe_catalogs, merge_status,
};
use super::create::destroy_members;
use super::{
    BackendKey, ClusterCreated, ClusterMember, ClusterNode, CreatePlan, Leftover,
    MulticastAllocator, backend_key, create_cluster, plan_create, probe_servers,
};
use crate::ipc::new_cluster_key;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// Distinguishes two clusters created in the same nanosecond by this process.
static CLUSTER_SEQ: AtomicU64 = AtomicU64::new(0);

/// A key for one cluster-wide create; [`super::create::member_id`] derives each
/// participant's simulation id from it.
fn next_cluster_key() -> String {
    format!(
        "{}-{}",
        new_cluster_key(),
        CLUSTER_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Multi-server backend used by `simulation-frontend` when `servers` has several entries.
pub struct MultiBackend {
    servers: Vec<ServerAddr>,
    nodes: HashMap<BackendKey, Arc<dyn SimulationBackend>>,
    registry: Registry,
    fan_in: FanIn,
}

/// Most merged log lines kept per simulation (each backend keeps its own
/// shorter history; the SSE stream asks every 100 ms).
const MERGED_LOG_CAP: usize = 1000;

/// One backend that hosts a part of a simulation, with that part's id there.
#[derive(Clone)]
struct Member {
    id: String,
    backend: Arc<dyn SimulationBackend>,
}

/// An export in progress: one CSV job per member, merged by [`CsvMerge`].
struct CsvExport {
    /// The id handed to the client.
    request_id: u64,
    /// Request id of each member's own job, in member order.
    member_requests: Vec<u64>,
    merge: CsvMerge,
}

/// State kept between calls for the fan-in operations, by client-facing id.
#[derive(Default)]
struct FanIn {
    logs: Mutex<HashMap<String, Arc<Mutex<MergedLog>>>>,
    csv: Mutex<HashMap<String, Arc<Mutex<CsvExport>>>>,
    /// Simulations whose members all advertised a complete probe catalog.
    /// Catalogs only grow, so a simulation stays in here until it is shut down.
    catalogs_complete: Mutex<HashSet<String>>,
    next_csv_request: AtomicU64,
    /// Which members hold a probe watch, by client-facing id.
    watches: Mutex<HashMap<String, Arc<Mutex<WatchState>>>>,
}

impl FanIn {
    fn forget(&self, id: &str) {
        lock(&self.logs).remove(id);
        lock(&self.csv).remove(id);
        lock(&self.catalogs_complete).remove(id);
        lock(&self.watches).remove(id);
    }
}

/// Which members of one simulation hold a probe watch, and how far their
/// live batches have been read.
struct WatchState {
    /// Indexes (in member order) of members that hold a non-empty watch, or
    /// may hold one because an update to them failed.
    holders: BTreeSet<usize>,
    /// Highest `seq` read from each member's live batches.
    cursors: Vec<u64>,
    /// `seq` of the next batch handed to the client.
    next_seq: u64,
}

impl WatchState {
    fn new(members: usize) -> Self {
        Self {
            holders: BTreeSet::new(),
            cursors: vec![0; members],
            next_seq: 1,
        }
    }
}

/// Call `f` for every member at the same time; one result per member, in order.
fn fan_out_all<T: Send>(
    members: &[Member],
    f: impl Fn(usize, &Member) -> Result<T, ApiError> + Sync,
) -> Vec<Result<T, ApiError>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = members
            .iter()
            .enumerate()
            .map(|(index, member)| {
                let f = &f;
                scope.spawn(move || f(index, member))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(ApiError::internal("backend call panicked")))
            })
            .collect()
    })
}

/// Like [`fan_out_all`], but the first member's error (in member order) wins.
fn fan_out<T: Send>(
    members: &[Member],
    f: impl Fn(usize, &Member) -> Result<T, ApiError> + Sync,
) -> Result<Vec<T>, ApiError> {
    fan_out_all(members, f).into_iter().collect()
}

/// The members at `indexes` (a subset of `members`' positions), with their
/// positions, both in member order.
fn pick_members(members: &[Member], indexes: &BTreeSet<usize>) -> (Vec<usize>, Vec<Member>) {
    indexes
        .iter()
        .filter_map(|index| members.get(*index).map(|member| (*index, member.clone())))
        .unzip()
}

/// `503 not_ready`: the backend has not finished its catalog yet.
fn is_not_ready(err: &ApiError) -> bool {
    err.status == 503 && err.code == "not_ready"
}

/// Pick the error to show when some members failed: `not_ready` first (the
/// client retries on it), then the first in member order.
fn pick_error(errors: Vec<ApiError>) -> Option<ApiError> {
    let index = errors.iter().position(is_not_ready).unwrap_or(0);
    errors.into_iter().nth(index)
}

/// Which simulations exist, and which parts of them are still to be destroyed.
#[derive(Default)]
struct Registry {
    /// Client-facing simulation id -> every backend hosting a part of it.
    sims: Mutex<HashMap<String, ClusterCreated>>,
    /// Parts of a simulation that could not be destroyed, by a failed create
    /// rollback or by `shutdown`. A later DELETE of any id of such a part
    /// retries exactly those parts.
    undestroyed: Mutex<HashMap<String, Leftover>>,
    multicast: Mutex<MulticastAllocator>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Registry {
    /// Create a cluster under `cluster_key`. Parts a failed create could not
    /// destroy are kept in `undestroyed`, so DELETE can finish the cleanup.
    fn create<N: ClusterNode + ?Sized>(
        &self,
        topology: &LogicalTopology,
        plan: &CreatePlan,
        cluster_key: &str,
        nodes: &HashMap<BackendKey, Arc<N>>,
    ) -> Result<String, ApiError> {
        match create_cluster(topology, plan, cluster_key, nodes, &self.multicast) {
            Ok(created) => {
                let id = created.id.clone();
                lock(&self.sims).insert(id.clone(), created);
                Ok(id)
            }
            Err(failure) => {
                if let Some(leftover) = failure.leftover {
                    if let Some(first) = leftover.members.first() {
                        lock(&self.undestroyed).insert(first.id.clone(), leftover);
                    }
                }
                Err(failure.error)
            }
        }
    }

    /// Remove the leftover that contains a part with this id.
    fn take_undestroyed(&self, id: &str) -> Option<Leftover> {
        let mut map = lock(&self.undestroyed);
        let key = map
            .iter()
            .find(|(key, left)| key.as_str() == id || left.members.iter().any(|m| m.id == id))
            .map(|(key, _)| key.clone())?;
        map.remove(&key)
    }

    /// Destroy every part of `id`. `None` when `id` is not known here.
    ///
    /// Every part is attempted even after one fails; the first error is
    /// returned. Parts that could not be destroyed stay registered (and keep
    /// their multicast group reserved), so repeating the DELETE retries
    /// exactly those.
    fn shutdown<N: ClusterNode + ?Sized>(
        &self,
        nodes: &HashMap<BackendKey, Arc<N>>,
        id: &str,
    ) -> Option<Result<(), ApiError>> {
        let removed = lock(&self.sims).remove(id);
        let leftover = match removed {
            Some(created) => Leftover {
                members: created.members,
                multicast: created.multicast,
            },
            None => self.take_undestroyed(id)?,
        };
        let mut failed = destroy_members(&leftover.members, nodes).into_iter();
        let Some((first_member, first_err)) = failed.next() else {
            if let Some(group) = &leftover.multicast {
                lock(&self.multicast).release(group);
            }
            return Some(Ok(()));
        };
        let members: Vec<_> = std::iter::once(first_member)
            .chain(failed.map(|(member, _)| member))
            .collect();
        lock(&self.undestroyed).insert(
            id.to_string(),
            Leftover {
                members,
                multicast: leftover.multicast,
            },
        );
        Some(Err(first_err))
    }

    /// Every part of `id`, primary first, if this process created it.
    fn members(&self, id: &str) -> Option<Vec<ClusterMember>> {
        lock(&self.sims)
            .get(id)
            .map(|created| created.members.clone())
    }

    /// Position (in [`Self::members`] order) of the member whose backend owns
    /// `board`. `None` when `id` is not known here or the assignment does not
    /// contain `board`.
    fn member_index_of_board(&self, id: &str, board: &str) -> Option<usize> {
        let sims = lock(&self.sims);
        let created = sims.get(id)?;
        let backend = created.assignment.backend_for_board(board)?;
        created
            .members
            .iter()
            .position(|member| member.backend == backend)
    }

    /// Backend of the first participant of `id`, if this process created it.
    fn primary(&self, id: &str) -> Option<BackendKey> {
        lock(&self.sims)
            .get(id)
            .map(|created| created.members[0].backend.clone())
    }
}

impl MultiBackend {
    /// HTTP clients for `servers` (must be non-empty).
    #[must_use]
    pub fn new(servers: Vec<ServerAddr>) -> Arc<Self> {
        assert!(
            !servers.is_empty(),
            "MultiBackend needs at least one server"
        );
        let nodes = servers
            .iter()
            .map(|s| {
                let client: Arc<dyn SimulationBackend> =
                    TcpipBackend::new(format!("http://{}:{}", s.ip, s.port));
                (backend_key(&s.ip, s.port), client)
            })
            .collect();
        Arc::new(Self {
            servers,
            nodes,
            registry: Registry::default(),
            fan_in: FanIn::default(),
        })
    }

    /// Backend that serves operations for `id`.
    ///
    /// Ids this process did not create (for example after a frontend restart)
    /// go to `servers[0]`, the pre-multi-backend behaviour; that backend answers
    /// 404 when it does not know the id either.
    fn route(&self, id: &str) -> Arc<dyn SimulationBackend> {
        let key = self
            .registry
            .primary(id)
            .unwrap_or_else(|| backend_key(&self.servers[0].ip, self.servers[0].port));
        self.nodes[&key].clone()
    }

    /// Every backend hosting a part of `id`, primary first.
    ///
    /// An id this process did not create has the single member [`Self::route`]
    /// picks, under the id it was asked with.
    fn members(&self, id: &str) -> Vec<Member> {
        match self.registry.members(id) {
            Some(members) => members
                .into_iter()
                .map(|member| Member {
                    backend: self.nodes[&member.backend].clone(),
                    id: member.id,
                })
                .collect(),
            None => vec![Member {
                id: id.to_string(),
                backend: self.route(id),
            }],
        }
    }

    /// Sends one run-control command (start, stop or reset) to every member
    /// of `id` at the same time, each under its own simulation id.
    ///
    /// Best effort, no rollback: every member is attempted even after another
    /// one failed, and members that succeeded keep the effect (for example
    /// some members may be running while the one that failed is not). The
    /// first error in member order is returned; the client sees the real state
    /// in the next `status`, and repeating the command is safe. A simulation
    /// with one member (or an id this process did not create) is forwarded
    /// unchanged to that member.
    fn command_all(
        &self,
        id: &str,
        command: impl Fn(&Member) -> Result<(), ApiError> + Sync,
    ) -> Result<(), ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return command(only);
        }
        fan_out_all(&members, |_, member| command(member))
            .into_iter()
            .collect()
    }

    /// The member of `id` that owns `board`.
    ///
    /// A simulation with one member (or an id this process did not create) has
    /// only that member, which is asked unchanged and answers for the board
    /// itself. With several members the assignment decides, and a board it does
    /// not contain is `404 not_found` before any backend is called.
    fn board_member(&self, id: &str, board: &str) -> Result<Member, ApiError> {
        let mut members = self.members(id);
        if members.len() == 1 {
            return Ok(members.remove(0));
        }
        let index = self
            .registry
            .member_index_of_board(id, board)
            .filter(|index| *index < members.len())
            .ok_or_else(|| ApiError::not_found("unknown board"))?;
        Ok(members.swap_remove(index))
    }

    /// The probe-watch state of `id`, created on first use.
    fn watch_state(&self, id: &str, members: usize) -> Arc<Mutex<WatchState>> {
        lock(&self.fan_in.watches)
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(WatchState::new(members))))
            .clone()
    }

    /// `true` when every member of `id` has advertised its whole probe catalog.
    ///
    /// Asks the members until the answer is yes, then remembers it. A member
    /// answering `503 not_ready` means "no"; any other error is returned.
    fn catalogs_complete(&self, id: &str, members: &[Member]) -> Result<bool, ApiError> {
        if lock(&self.fan_in.catalogs_complete).contains(id) {
            return Ok(true);
        }
        let mut complete = true;
        for result in fan_out_all(members, |_, member| {
            member.backend.probe_catalog(&member.id)
        }) {
            match result {
                Ok(_) => {}
                Err(err) if is_not_ready(&err) => complete = false,
                Err(err) => return Err(err),
            }
        }
        if complete {
            lock(&self.fan_in.catalogs_complete).insert(id.to_string());
        }
        Ok(complete)
    }

    /// Fetch the lines every member logged since the last fetch into `log`.
    fn pull_logs(members: &[Member], log: &mut MergedLog) -> Result<(), ApiError> {
        let cursors: Vec<u64> = (0..members.len()).map(|i| log.cursor(i)).collect();
        let batches = fan_out(members, |index, member| {
            member.backend.logs_after(&member.id, cursors[index])
        })?;
        log.absorb(batches);
        Ok(())
    }

    /// The merged log of `id`, created on first use.
    fn merged_log(&self, id: &str, members: usize) -> Arc<Mutex<MergedLog>> {
        lock(&self.fan_in.logs)
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(MergedLog::new(members, MERGED_LOG_CAP))))
            .clone()
    }

    /// The export `request_id` of `id`, if that is the current one.
    fn csv_export(&self, id: &str, request_id: u64) -> Result<Arc<Mutex<CsvExport>>, ApiError> {
        lock(&self.fan_in.csv)
            .get(id)
            .filter(|export| lock(export).request_id == request_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("unknown csv export"))
    }

    #[cfg(test)]
    fn for_tests(
        nodes: HashMap<BackendKey, Arc<dyn SimulationBackend>>,
        created: ClusterCreated,
    ) -> Self {
        let first = created.members[0].backend.clone();
        let (ip, port) = first.rsplit_once(':').expect("backend key is ip:port");
        let servers = vec![ServerAddr {
            ip: ip.to_string(),
            port: port.parse().expect("port"),
        }];
        let registry = Registry::default();
        lock(&registry.sims).insert(created.id.clone(), created);
        Self {
            servers,
            nodes,
            registry,
            fan_in: FanIn::default(),
        }
    }
}

impl SimulationBackend for MultiBackend {
    fn create(&self, topology: LogicalTopology) -> Result<String, ApiError> {
        let probes = probe_servers(&self.servers, HEALTH_TIMEOUT);
        let plan = plan_create(&topology, &probes)?;
        eprintln!(
            "simulation-frontend: create over {} backend(s): {}",
            plan.participants.len(),
            plan.participants.join(", ")
        );
        self.registry
            .create(&topology, &plan, &next_cluster_key(), &self.nodes)
    }

    /// Starts every member, each under its own simulation id, at the same time.
    /// See [`Self::command_all`] for what happens when a member fails.
    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
        self.command_all(id, |member| member.backend.start(&member.id, until_ns))
    }

    /// Stops every member, each under its own simulation id, at the same time.
    /// See [`Self::command_all`] for what happens when a member fails.
    fn stop(&self, id: &str) -> Result<(), ApiError> {
        self.command_all(id, |member| member.backend.stop(&member.id))
    }

    /// Resets every member, each under its own simulation id, at the same time.
    /// See [`Self::command_all`] for what happens when a member fails.
    fn reset(&self, id: &str) -> Result<(), ApiError> {
        self.command_all(id, |member| member.backend.reset(&member.id))
    }

    /// Destroys the simulation on every participant.
    ///
    /// Every participant is attempted even after one fails; the first error is
    /// returned. Participants that could not be destroyed stay registered (and
    /// keep their multicast group reserved), so repeating the DELETE retries
    /// exactly those. This also finishes a create whose rollback was incomplete:
    /// DELETE any id named in that create's error message.
    fn shutdown(&self, id: &str) -> Result<(), ApiError> {
        let result = match self.registry.shutdown(&self.nodes, id) {
            Some(result) => result,
            None => self.route(id).shutdown(id),
        };
        self.fan_in.forget(id);
        result
    }

    fn exists(&self, id: &str) -> Result<bool, ApiError> {
        self.route(id).exists(id)
    }

    /// One status from every member (see the module docs for the rules).
    ///
    /// Probe catalogs are only asked once every node is past its ready barrier
    /// and no member has exited; until they are complete the state is `starting`.
    fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.status(&only.id);
        }
        let statuses = fan_out(&members, |_, member| member.backend.status(&member.id))?;
        let catalogs_complete = if any_exited(&statuses) || !all_nodes_ready(&statuses) {
            false
        } else {
            self.catalogs_complete(id, &members)?
        };
        Ok(merge_status(id, statuses, catalogs_complete))
    }

    fn topology(&self, id: &str) -> Result<TopologyView, ApiError> {
        self.route(id).topology(id)
    }

    /// Merged lines with `seq > after`, in virtual-time order. `after` is a
    /// merged `seq` (see [`MergedLog`]), not a member's.
    fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.logs_after(&only.id, after);
        }
        let log = self.merged_log(id, members.len());
        let mut log = lock(&log);
        Self::pull_logs(&members, &mut log)?;
        Ok(log.after(after))
    }

    fn log_high_water(&self, id: &str) -> Result<u64, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.log_high_water(&only.id);
        }
        let log = self.merged_log(id, members.len());
        let mut log = lock(&log);
        Self::pull_logs(&members, &mut log)?;
        Ok(log.high_water())
    }

    /// Every member's actions in one list. Empty until every member's catalog
    /// is complete, so the client keeps asking instead of keeping a partial list.
    fn actions(&self, id: &str) -> Result<ActionList, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.actions(&only.id);
        }
        let lists = fan_out_all(&members, |_, member| member.backend.actions(&member.id));
        let mut found = Vec::new();
        let mut errors = Vec::new();
        for result in lists {
            match result {
                Ok(list) => found.push(list),
                Err(err) => errors.push(err),
            }
        }
        if let Some(err) = pick_error(errors) {
            return Err(err);
        }
        if !self.catalogs_complete(id, &members)? {
            return Ok(ActionList {
                actions: Vec::new(),
            });
        }
        Ok(merge_actions(found))
    }

    fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError> {
        let member = self.board_member(id, &request.board_id)?;
        member.backend.invoke_action(&member.id, request)
    }

    /// Every member's probes in one list; `503 not_ready` while any member's
    /// catalog is incomplete.
    fn probe_catalog(&self, id: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.probe_catalog(&only.id);
        }
        let mut catalogs = Vec::new();
        let mut errors = Vec::new();
        for result in fan_out_all(&members, |_, member| {
            member.backend.probe_catalog(&member.id)
        }) {
            match result {
                Ok(items) => catalogs.push(items),
                Err(err) => errors.push(err),
            }
        }
        if let Some(err) = pick_error(errors) {
            return Err(err);
        }
        lock(&self.fan_in.catalogs_complete).insert(id.to_string());
        Ok(merge_probe_catalogs(catalogs))
    }

    /// The watches of the members that hold one, in one list. No backend is
    /// asked when none holds a watch.
    fn probe_watch_get(&self, id: &str) -> Result<ProbeWatchView, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.probe_watch_get(&only.id);
        }
        let state = self.watch_state(id, members.len());
        let state = lock(&state);
        let (_, holders) = pick_members(&members, &state.holders);
        let views = fan_out(&holders, |_, member| {
            member.backend.probe_watch_get(&member.id)
        })?;
        Ok(ProbeWatchView {
            probes: views.into_iter().flat_map(|view| view.probes).collect(),
        })
    }

    /// Replaces the watch of `id`. Each probe goes only to the member owning
    /// its board; a member holding an older watch but owning none of the new
    /// probes is given an empty list. An unknown board fails the whole request
    /// before any backend is called.
    fn probe_watch_put(&self, id: &str, probes: Vec<ProbeRef>) -> Result<ProbeWatchView, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.probe_watch_put(&only.id, probes);
        }
        let mut parts: Vec<Vec<ProbeRef>> = vec![Vec::new(); members.len()];
        for probe in probes {
            let index = self
                .registry
                .member_index_of_board(id, &probe.board)
                .filter(|index| *index < members.len())
                .ok_or_else(|| ApiError::not_found("unknown board"))?;
            parts[index].push(probe);
        }
        let state = self.watch_state(id, members.len());
        let mut state = lock(&state);
        let wanted: BTreeSet<usize> = (0..members.len())
            .filter(|index| !parts[*index].is_empty() || state.holders.contains(index))
            .collect();
        let (indexes, targets) = pick_members(&members, &wanted);
        let results = fan_out_all(&targets, |position, member| {
            member
                .backend
                .probe_watch_put(&member.id, parts[indexes[position]].clone())
        });
        let mut probes = Vec::new();
        let mut first_error = None;
        for (position, result) in results.into_iter().enumerate() {
            let index = indexes[position];
            match result {
                Ok(view) => {
                    probes.extend(view.probes);
                    if parts[index].is_empty() {
                        state.holders.remove(&index);
                    } else {
                        state.holders.insert(index);
                    }
                }
                Err(err) => {
                    // The member may or may not have taken the new watch.
                    if !parts[index].is_empty() {
                        state.holders.insert(index);
                    }
                    first_error = first_error.or(Some(err));
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(ProbeWatchView { probes }),
        }
    }

    /// Clears the watch on the members that hold one; the others are not asked.
    fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.probe_watch_clear(&only.id);
        }
        let state = self.watch_state(id, members.len());
        let mut state = lock(&state);
        let (indexes, holders) = pick_members(&members, &state.holders);
        let mut first_error = None;
        for (position, result) in fan_out_all(&holders, |_, member| {
            member.backend.probe_watch_clear(&member.id)
        })
        .into_iter()
        .enumerate()
        {
            match result {
                Ok(()) => {
                    state.holders.remove(&indexes[position]);
                }
                Err(err) => first_error = first_error.or(Some(err)),
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// New live batches of the members that hold a watch, under one `seq`.
    ///
    /// The members' batches are taken (a backend hands each batch out once), so
    /// when one member fails while others delivered, the delivered batches are
    /// returned and the failure shows up again on the next call. `after == 0`
    /// starts a new subscription: members are read from their beginning.
    fn probe_watch_live(&self, id: &str, after: u64) -> Result<Vec<CoarseBatch>, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.probe_watch_live(&only.id, after);
        }
        let state = self.watch_state(id, members.len());
        let mut state = lock(&state);
        if after == 0 {
            state.cursors.iter_mut().for_each(|cursor| *cursor = 0);
        }
        let (indexes, holders) = pick_members(&members, &state.holders);
        let cursors = state.cursors.clone();
        let results = fan_out_all(&holders, |position, member| {
            member
                .backend
                .probe_watch_live(&member.id, cursors[indexes[position]])
        });
        let mut found: Vec<(usize, CoarseBatch)> = Vec::new();
        let mut first_error = None;
        for (position, result) in results.into_iter().enumerate() {
            let index = indexes[position];
            match result {
                Ok(batches) => {
                    for batch in batches {
                        state.cursors[index] = state.cursors[index].max(batch.seq);
                        found.push((index, batch));
                    }
                }
                Err(err) => first_error = first_error.or(Some(err)),
            }
        }
        if found.is_empty() {
            return match first_error {
                Some(err) => Err(err),
                None => Ok(Vec::new()),
            };
        }
        found.sort_by_key(|(index, batch)| {
            let first_sample_ns = batch
                .samples
                .iter()
                .map(|sample| sample.virtual_time_ns)
                .min()
                .unwrap_or(0);
            (first_sample_ns, *index, batch.seq)
        });
        Ok(found
            .into_iter()
            .map(|(_, mut batch)| {
                batch.seq = state.next_seq;
                state.next_seq += 1;
                batch
            })
            .collect())
    }

    /// Start one export on every member. The returned id is this frontend's;
    /// it replaces any earlier export of `id`.
    fn csv_start(
        &self,
        id: &str,
        range: ProbeRange,
    ) -> Result<(u64, Vec<ProbePointName>), ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.csv_start(&only.id, range);
        }
        let started = fan_out(&members, |_, member| {
            member.backend.csv_start(&member.id, range)
        })?;
        let (member_requests, catalogs): (Vec<u64>, Vec<Vec<ProbePointName>>) =
            started.into_iter().unzip();
        let request_id = self.fan_in.next_csv_request.fetch_add(1, Ordering::Relaxed) + 1;
        lock(&self.fan_in.csv).insert(
            id.to_string(),
            Arc::new(Mutex::new(CsvExport {
                request_id,
                member_requests,
                merge: CsvMerge::new(members.len()),
            })),
        );
        Ok((request_id, merge_csv_catalogs(catalogs)))
    }

    /// Polls every member that is not done yet. Records come back all at once
    /// (sorted by virtual time) in the poll that finds every member done.
    fn csv_poll(&self, id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.csv_poll(&only.id, request_id);
        }
        let export = self.csv_export(id, request_id)?;
        let mut export = lock(&export);
        let polled = fan_out_all(&members, |index, member| {
            if export.merge.is_drained(index) {
                return Ok(None);
            }
            member
                .backend
                .csv_poll(&member.id, export.member_requests[index])
                .map(Some)
        });
        let mut first_error = None;
        for (index, result) in polled.into_iter().enumerate() {
            match result {
                Ok(Some(poll)) => export.merge.absorb(index, poll),
                Ok(None) => {}
                Err(err) => first_error = first_error.or(Some(err)),
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(export.merge.poll()),
        }
    }

    /// `done` when every member's export is done, `failed` when any failed.
    fn csv_status(&self, id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
        let members = self.members(id);
        if let [only] = members.as_slice() {
            return only.backend.csv_status(&only.id, request_id);
        }
        let export = self.csv_export(id, request_id)?;
        let export = lock(&export);
        let statuses = fan_out(&members, |index, member| {
            member
                .backend
                .csv_status(&member.id, export.member_requests[index])
        })?;
        Ok(CsvPoll {
            records: Vec::new(),
            done: statuses.iter().all(|poll| poll.done),
            failed: statuses.iter().any(|poll| poll.failed),
            truncated: statuses.iter().any(|poll| poll.truncated),
        })
    }

    fn probe_samples(
        &self,
        id: &str,
        board: &str,
        probe: &str,
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Result<ProbeSamplesResponse, ApiError> {
        let member = self.board_member(id, board)?;
        member
            .backend
            .probe_samples(&member.id, board, probe, from_ns, to_ns)
    }

    fn viewer_attach(&self, id: &str) -> Result<String, ApiError> {
        self.route(id).viewer_attach(id)
    }

    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError> {
        self.route(id).viewer_check(id, token)
    }

    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError> {
        self.route(id).viewer_detach(id, token)
    }

    fn inspect(
        &self,
        id: &str,
        board_id: &str,
        req: InspectRequest,
    ) -> Result<InspectReply, ApiError> {
        let member = self.board_member(id, board_id)?;
        member.backend.inspect(&member.id, board_id, req)
    }

    fn step(&self, id: &str, board_id: &str) -> Result<(), ApiError> {
        // Only the backend that runs `board_id` executes the GDB step; the other
        // arbiters are told over the control TCP mesh, not by another HTTP call.
        let member = self.board_member(id, board_id)?;
        member.backend.step(&member.id, board_id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::super::{
        ArbiterEndpoints, AssignmentTable, BackendCapacity, BoardAssignment, ClusterSettings,
        Phase1CreateRequest, Phase1CreateResponse, Phase2ConnectRequest, Phase2ConnectResponse,
        ServerProbe,
    };
    use super::*;
    use crate::contract::AdvertisedAction;
    use crate::host::SessionNode;
    use crate::topology::BoardSpec;

    /// Records destroyed ids; `destroy` can be made to fail.
    #[derive(Default)]
    struct Node {
        key: String,
        fail_phase2: bool,
        destroy_fails: AtomicBool,
        destroyed: Mutex<Vec<String>>,
    }

    impl Node {
        fn new(key: &str) -> Self {
            Self {
                key: key.into(),
                ..Self::default()
            }
        }
    }

    impl ClusterNode for Node {
        fn phase1_create(
            &self,
            req: Phase1CreateRequest,
        ) -> Result<Phase1CreateResponse, ApiError> {
            Ok(Phase1CreateResponse {
                id: req.id,
                endpoints: Some(ArbiterEndpoints {
                    backend: self.key.clone(),
                    advertise_ip: "127.0.0.1".into(),
                    control_tcp_port: 9000,
                    data_udp_port: 9001,
                }),
            })
        }

        fn phase2_connect(
            &self,
            _req: Phase2ConnectRequest,
        ) -> Result<Phase2ConnectResponse, ApiError> {
            if self.fail_phase2 {
                return Err(ApiError::internal("connect failed"));
            }
            Ok(Phase2ConnectResponse { ok: true })
        }

        fn destroy(&self, id: &str) -> Result<(), ApiError> {
            if self.destroy_fails.load(Ordering::SeqCst) {
                return Err(ApiError::internal("destroy refused"));
            }
            self.destroyed.lock().unwrap().push(id.to_string());
            Ok(())
        }
    }

    fn setup(
        first: Node,
        second: Node,
    ) -> (LogicalTopology, CreatePlan, HashMap<BackendKey, Arc<Node>>) {
        let topology = LogicalTopology {
            boards: ["a", "b"]
                .iter()
                .map(|id| BoardSpec {
                    id: (*id).into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![],
                })
                .collect(),
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 200,
            ready_timeout_ms: 10_000,
        };
        let probe = |backend: &str| {
            ServerProbe::Healthy(BackendCapacity {
                backend: backend.into(),
                max_boards: 1,
                running_boards: 0,
                settings: ClusterSettings {
                    time_sync_period_ms: 100,
                    data_udp_window: 32,
                    multicast_base: "239.255.0.0".into(),
                },
            })
        };
        let plan = plan_create(&topology, &[probe("s0:1"), probe("s1:1")]).unwrap();
        let nodes = [first, second]
            .into_iter()
            .map(|n| (n.key.clone(), Arc::new(n)))
            .collect();
        (topology, plan, nodes)
    }

    /// A backend that answers fan-in reads from canned data and refuses to
    /// answer for any simulation id other than its own.
    struct Fake {
        id: String,
        status: Mutex<StatusEvent>,
        probes: Mutex<Result<Vec<ProbeCatalogItem>, ApiError>>,
        actions: Vec<AdvertisedAction>,
        logs: Mutex<Vec<LogEvent>>,
        csv_polls: Mutex<std::collections::VecDeque<CsvPoll>>,
        csv_request: u64,
        csv_done: bool,
        log_asks: Mutex<Vec<u64>>,
        /// Board-addressed and watch calls received, as `"op:detail"`.
        calls: Mutex<Vec<String>>,
        watch: Mutex<Vec<ProbeRef>>,
        live: Mutex<Vec<CoarseBatch>>,
        live_asks: Mutex<Vec<u64>>,
        run_control_fails: AtomicBool,
    }

    impl Fake {
        fn new(id: &str, board: &str) -> Self {
            Self {
                id: id.into(),
                status: Mutex::new(StatusEvent {
                    id: id.into(),
                    state: "stopped".into(),
                    virtual_time_ns: 0,
                    stop_reason: None,
                    nodes: vec![SessionNode {
                        id: board.into(),
                        kind: "rl78".into(),
                        state: "ready".into(),
                    }],
                }),
                probes: Mutex::new(Ok(Vec::new())),
                actions: Vec::new(),
                logs: Mutex::new(Vec::new()),
                csv_polls: Mutex::new(Default::default()),
                csv_request: 0,
                csv_done: false,
                log_asks: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                watch: Mutex::new(Vec::new()),
                live: Mutex::new(Vec::new()),
                live_asks: Mutex::new(Vec::new()),
                run_control_fails: AtomicBool::new(false),
            }
        }

        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// Makes start/stop/reset answer with an error.
        fn fail_run_control(&self) {
            self.run_control_fails.store(true, Ordering::SeqCst);
        }

        fn run_control_result(&self) -> Result<(), ApiError> {
            if self.run_control_fails.load(Ordering::SeqCst) {
                Err(ApiError::internal(format!("{} refused", self.id)))
            } else {
                Ok(())
            }
        }

        fn own(&self, id: &str) -> Result<(), ApiError> {
            if id == self.id {
                Ok(())
            } else {
                Err(ApiError::not_found(format!(
                    "{} asked for {id}, not its own simulation",
                    self.id
                )))
            }
        }

        fn set_state(&self, state: &str, vt: u64) {
            let mut status = self.status.lock().unwrap();
            status.state = state.into();
            status.virtual_time_ns = vt;
        }

        fn set_node_state(&self, state: &str) {
            self.status.lock().unwrap().nodes[0].state = state.into();
        }

        fn set_probes(&self, board: &str, names: &[&str]) {
            *self.probes.lock().unwrap() = Ok(names
                .iter()
                .map(|name| ProbeCatalogItem {
                    board: board.into(),
                    probe: (*name).into(),
                    kind: "digital".into(),
                })
                .collect());
        }

        fn set_probes_not_ready(&self) {
            *self.probes.lock().unwrap() = Err(ApiError::new(
                503,
                "not_ready",
                "probe catalog is not ready",
            ));
        }

        fn push_log(&self, seq: u64, vt: u64, text: &str) {
            self.logs.lock().unwrap().push(LogEvent {
                seq,
                level: "info".into(),
                origin: "node".into(),
                board: None,
                text: text.into(),
                virtual_time_ns: vt,
            });
        }
    }

    fn unused<T>() -> Result<T, ApiError> {
        Err(ApiError::internal("unused in fan-in tests"))
    }

    impl SimulationBackend for Fake {
        fn create(&self, _topology: LogicalTopology) -> Result<String, ApiError> {
            unused()
        }
        fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
            self.own(id)?;
            self.record(format!("start:{id}:{until_ns:?}"));
            self.run_control_result()
        }
        fn stop(&self, id: &str) -> Result<(), ApiError> {
            self.own(id)?;
            self.record(format!("stop:{id}"));
            self.run_control_result()
        }
        fn reset(&self, id: &str) -> Result<(), ApiError> {
            self.own(id)?;
            self.record(format!("reset:{id}"));
            self.run_control_result()
        }
        fn shutdown(&self, id: &str) -> Result<(), ApiError> {
            self.own(id)?;
            self.record(format!("shutdown:{id}"));
            Ok(())
        }
        fn exists(&self, _id: &str) -> Result<bool, ApiError> {
            unused()
        }
        fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
            self.own(id)?;
            Ok(self.status.lock().unwrap().clone())
        }
        fn topology(&self, _id: &str) -> Result<TopologyView, ApiError> {
            unused()
        }
        fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError> {
            self.own(id)?;
            self.log_asks.lock().unwrap().push(after);
            Ok(self
                .logs
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.seq > after)
                .cloned()
                .collect())
        }
        fn log_high_water(&self, _id: &str) -> Result<u64, ApiError> {
            unused()
        }
        fn actions(&self, id: &str) -> Result<ActionList, ApiError> {
            self.own(id)?;
            Ok(ActionList {
                actions: self.actions.clone(),
            })
        }
        fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError> {
            self.own(id)?;
            self.record(format!("action:{}:{}", id, request.board_id));
            Ok(())
        }
        fn probe_catalog(&self, id: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
            self.own(id)?;
            self.probes.lock().unwrap().clone()
        }
        fn probe_watch_get(&self, id: &str) -> Result<ProbeWatchView, ApiError> {
            self.own(id)?;
            self.record(format!("watch_get:{id}"));
            Ok(ProbeWatchView {
                probes: self.watch.lock().unwrap().clone(),
            })
        }
        fn probe_watch_put(
            &self,
            id: &str,
            probes: Vec<ProbeRef>,
        ) -> Result<ProbeWatchView, ApiError> {
            self.own(id)?;
            self.record(format!("watch_put:{id}:{}", probes.len()));
            *self.watch.lock().unwrap() = probes.clone();
            Ok(ProbeWatchView { probes })
        }
        fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
            self.own(id)?;
            self.record(format!("watch_clear:{id}"));
            self.watch.lock().unwrap().clear();
            Ok(())
        }
        fn probe_watch_live(&self, id: &str, after: u64) -> Result<Vec<CoarseBatch>, ApiError> {
            self.own(id)?;
            self.live_asks.lock().unwrap().push(after);
            let mut live = self.live.lock().unwrap();
            live.retain(|batch| batch.seq > after);
            Ok(live.drain(..).collect())
        }
        fn csv_start(
            &self,
            id: &str,
            _range: ProbeRange,
        ) -> Result<(u64, Vec<ProbePointName>), ApiError> {
            self.own(id)?;
            Ok((self.csv_request, Vec::new()))
        }
        fn csv_poll(&self, id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
            self.own(id)?;
            assert_eq!(request_id, self.csv_request, "member got the client's id");
            Ok(self
                .csv_polls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(CsvPoll {
                    records: Vec::new(),
                    done: true,
                    failed: false,
                    truncated: false,
                }))
        }
        fn csv_status(&self, id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
            self.own(id)?;
            assert_eq!(request_id, self.csv_request, "member got the client's id");
            Ok(CsvPoll {
                records: Vec::new(),
                done: self.csv_done,
                failed: false,
                truncated: false,
            })
        }
        fn probe_samples(
            &self,
            id: &str,
            board: &str,
            _probe: &str,
            _from_ns: Option<u64>,
            _to_ns: Option<u64>,
        ) -> Result<ProbeSamplesResponse, ApiError> {
            self.own(id)?;
            self.record(format!("samples:{id}:{board}"));
            Ok(ProbeSamplesResponse {
                truncated: false,
                samples: Vec::new(),
            })
        }
        fn viewer_attach(&self, _id: &str) -> Result<String, ApiError> {
            unused()
        }
        fn viewer_check(&self, _id: &str, _token: &str) -> Result<(), ApiError> {
            unused()
        }
        fn viewer_detach(&self, _id: &str, _token: &str) -> Result<(), ApiError> {
            unused()
        }
        fn inspect(
            &self,
            id: &str,
            board_id: &str,
            _req: InspectRequest,
        ) -> Result<InspectReply, ApiError> {
            self.own(id)?;
            self.record(format!("inspect:{id}:{board_id}"));
            Ok(InspectReply::Ok)
        }
        fn step(&self, id: &str, board_id: &str) -> Result<(), ApiError> {
            self.own(id)?;
            self.record(format!("step:{id}:{board_id}"));
            Ok(())
        }
    }

    /// Two backends hosting `key-0` (client id) and `key-1`, boards `a` and `b`.
    fn fan_in_pair() -> (Arc<Fake>, Arc<Fake>, MultiBackend) {
        let first = Arc::new(Fake::new("key-0", "a"));
        let second = Arc::new(Fake::new("key-1", "b"));
        (first.clone(), second.clone(), multi_of(first, second))
    }

    fn multi_of(first: Arc<Fake>, second: Arc<Fake>) -> MultiBackend {
        let nodes: HashMap<BackendKey, Arc<dyn SimulationBackend>> = HashMap::from([
            ("s0:1".to_string(), first as Arc<dyn SimulationBackend>),
            ("s1:1".to_string(), second as Arc<dyn SimulationBackend>),
        ]);
        let created = ClusterCreated {
            id: "key-0".into(),
            assignment: AssignmentTable {
                entries: vec![
                    BoardAssignment {
                        board_id: "a".into(),
                        backend: "s0:1".into(),
                    },
                    BoardAssignment {
                        board_id: "b".into(),
                        backend: "s1:1".into(),
                    },
                ],
            },
            members: vec![
                ClusterMember {
                    backend: "s0:1".into(),
                    id: "key-0".into(),
                },
                ClusterMember {
                    backend: "s1:1".into(),
                    id: "key-1".into(),
                },
            ],
            multicast: None,
        };
        MultiBackend::for_tests(nodes, created)
    }

    fn catalog_names(items: &[ProbeCatalogItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| format!("{}/{}", item.board, item.probe))
            .collect()
    }

    #[test]
    fn probe_catalogs_of_all_members_are_one_list() {
        let (first, second, multi) = fan_in_pair();
        first.set_probes("a", &["p1", "p0"]);
        second.set_probes("b", &["p0"]);

        let items = multi.probe_catalog("key-0").unwrap();

        assert_eq!(catalog_names(&items), ["a/p0", "a/p1", "b/p0"]);
    }

    #[test]
    fn probe_catalog_is_not_ready_until_every_member_is_complete() {
        let (first, second, multi) = fan_in_pair();
        first.set_probes("a", &["p0"]);
        second.set_probes_not_ready();

        let err = multi.probe_catalog("key-0").unwrap_err();
        assert_eq!((err.status, err.code), (503, "not_ready"));

        second.set_probes("b", &["p0"]);
        assert_eq!(multi.probe_catalog("key-0").unwrap().len(), 2);
    }

    #[test]
    fn actions_of_all_members_are_one_list_once_catalogs_are_complete() {
        let action = |board: &str| AdvertisedAction {
            board_id: board.into(),
            device: "sw".into(),
            action: "press".into(),
            description: String::new(),
            params: vec![],
        };
        let mut first = Fake::new("key-0", "a");
        first.actions = vec![action("a")];
        let mut second = Fake::new("key-1", "b");
        second.actions = vec![action("b")];
        let (first, second) = (Arc::new(first), Arc::new(second));
        let multi = multi_of(first.clone(), second.clone());
        first.set_probes("a", &["p0"]);
        second.set_probes_not_ready();

        // One member is still advertising: no partial list.
        assert!(multi.actions("key-0").unwrap().actions.is_empty());

        second.set_probes("b", &["p0"]);
        let boards: Vec<_> = multi
            .actions("key-0")
            .unwrap()
            .actions
            .into_iter()
            .map(|a| a.board_id)
            .collect();
        assert_eq!(boards, ["a", "b"]);
    }

    #[test]
    fn status_is_stopped_when_any_member_is_stopped_under_the_client_id() {
        let (first, second, multi) = fan_in_pair();
        first.set_probes("a", &["p0"]);
        second.set_probes("b", &["p0"]);
        first.set_state("running", 900);
        second.set_state("stopped", 700);

        let status = multi.status("key-0").unwrap();

        assert_eq!(status.id, "key-0");
        assert_eq!(status.state, "stopped");
        assert_eq!(status.virtual_time_ns, 700);
        let nodes: Vec<_> = status.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(nodes, ["a", "b"]);

        second.set_state("running", 950);
        assert_eq!(multi.status("key-0").unwrap().state, "running");
    }

    #[test]
    fn status_is_not_ready_until_members_and_catalogs_are_ready() {
        let (first, second, multi) = fan_in_pair();
        first.set_probes("a", &["p0"]);
        second.set_probes("b", &["p0"]);
        first.set_state("running", 0);
        second.set_state("running", 0);

        // A member's node has not passed the ready barrier.
        second.set_node_state("starting");
        assert_eq!(multi.status("key-0").unwrap().state, "starting");

        // Nodes are ready but one member's probe catalog is incomplete.
        second.set_node_state("running");
        second.set_probes_not_ready();
        assert_eq!(multi.status("key-0").unwrap().state, "starting");

        second.set_probes("b", &["p0"]);
        assert_eq!(multi.status("key-0").unwrap().state, "running");

        // Once complete, catalogs are not asked again.
        second.set_probes_not_ready();
        assert_eq!(multi.status("key-0").unwrap().state, "running");
    }

    #[test]
    fn status_of_an_exited_member_is_exited() {
        let (first, _second, multi) = fan_in_pair();
        first.set_state("exited", 5);
        assert_eq!(multi.status("key-0").unwrap().state, "exited");
    }

    #[test]
    fn logs_of_all_members_come_back_in_virtual_time_order_under_one_cursor() {
        let (first, second, multi) = fan_in_pair();
        first.push_log(1, 100, "a@100");
        first.push_log(2, 300, "a@300");
        second.push_log(1, 50, "b@50");
        second.push_log(2, 200, "b@200");

        // Subscribing: the cursor is taken at the end of the history.
        let high_water = multi.log_high_water("key-0").unwrap();
        assert_eq!(high_water, 4);
        assert!(multi.logs_after("key-0", high_water).unwrap().is_empty());

        let all = multi.logs_after("key-0", 0).unwrap();
        let texts: Vec<_> = all.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["b@50", "a@100", "b@200", "a@300"]);
        let vts: Vec<_> = all.iter().map(|e| e.virtual_time_ns).collect();
        assert!(vts.windows(2).all(|w| w[0] <= w[1]));

        // New lines arrive under the next merged seq; members are asked only
        // for what they have not delivered yet.
        second.push_log(3, 400, "b@400");
        let fresh = multi.logs_after("key-0", high_water).unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!((fresh[0].seq, fresh[0].text.as_str()), (5, "b@400"));
        assert_eq!(*first.log_asks.lock().unwrap().last().unwrap(), 2);
        assert_eq!(*second.log_asks.lock().unwrap().last().unwrap(), 2);
    }

    fn csv_poll_of(records: &[(u64, u64)], done: bool, failed: bool) -> CsvPoll {
        CsvPoll {
            records: records
                .iter()
                .map(|(probe_id, vt)| crate::probe_log::ProbeRecord {
                    probe_id: *probe_id,
                    virtual_time_ns: *vt,
                    kind: crate::probe_log::ProbeKind::Digital,
                    value: 0,
                })
                .collect(),
            done,
            failed,
            truncated: false,
        }
    }

    #[test]
    fn csv_merges_member_exports_by_virtual_time_under_one_request_id() {
        let mut first = Fake::new("key-0", "a");
        first.csv_request = 11;
        first.csv_done = true;
        first
            .csv_polls
            .lock()
            .unwrap()
            .extend([csv_poll_of(&[(1, 30), (1, 10)], true, false)]);
        let mut second = Fake::new("key-1", "b");
        second.csv_request = 22;
        second.csv_polls.lock().unwrap().extend([
            csv_poll_of(&[(2, 20)], false, false),
            csv_poll_of(&[(2, 40)], true, false),
        ]);
        let (first, second) = (Arc::new(first), Arc::new(second));
        let multi = multi_of(first, second);

        let (request_id, _catalog) = multi
            .csv_start(
                "key-0",
                ProbeRange {
                    from_ns: None,
                    to_ns: None,
                },
            )
            .unwrap();

        // One member is still working: nothing is released, not done.
        let status = multi.csv_status("key-0", request_id).unwrap();
        assert!(!status.done);
        let partial = multi.csv_poll("key-0", request_id).unwrap();
        assert!(!partial.done);
        assert!(partial.records.is_empty());

        let finished = multi.csv_poll("key-0", request_id).unwrap();
        assert!(finished.done);
        assert!(!finished.failed);
        let order: Vec<_> = finished
            .records
            .iter()
            .map(|r| (r.virtual_time_ns, r.probe_id))
            .collect();
        assert_eq!(order, [(10, 1), (20, 2), (30, 1), (40, 2)]);

        let err = multi.csv_poll("key-0", request_id + 1).unwrap_err();
        assert_eq!(err.status, 404);
    }

    #[test]
    fn csv_fails_when_any_member_export_fails() {
        let first = Fake::new("key-0", "a");
        let second = Fake::new("key-1", "b");
        second
            .csv_polls
            .lock()
            .unwrap()
            .push_back(csv_poll_of(&[], true, true));
        let multi = multi_of(Arc::new(first), Arc::new(second));
        let (request_id, _) = multi
            .csv_start(
                "key-0",
                ProbeRange {
                    from_ns: None,
                    to_ns: None,
                },
            )
            .unwrap();

        let poll = multi.csv_poll("key-0", request_id).unwrap();

        assert!(poll.done);
        assert!(poll.failed);
    }

    fn invoke(board: &str) -> InvokeActionRequest {
        InvokeActionRequest {
            board_id: board.into(),
            device: "sw".into(),
            action: "press".into(),
            params: Default::default(),
        }
    }

    fn probe_ref(board: &str, name: &str) -> ProbeRef {
        ProbeRef {
            board: board.into(),
            name: name.into(),
        }
    }

    #[test]
    fn action_inspect_and_samples_reach_only_the_owning_backend_under_its_own_id() {
        let (first, second, multi) = fan_in_pair();

        multi.invoke_action("key-0", invoke("b")).unwrap();
        assert_eq!(
            multi
                .inspect("key-0", "b", InspectRequest::ReadRegs)
                .unwrap(),
            InspectReply::Ok
        );
        multi.probe_samples("key-0", "b", "p0", None, None).unwrap();
        assert!(first.calls().is_empty(), "{:?}", first.calls());
        assert_eq!(
            second.calls(),
            ["action:key-1:b", "inspect:key-1:b", "samples:key-1:b"]
        );

        multi.invoke_action("key-0", invoke("a")).unwrap();
        multi
            .inspect("key-0", "a", InspectRequest::ReadRegs)
            .unwrap();
        multi.probe_samples("key-0", "a", "p0", None, None).unwrap();
        assert_eq!(
            first.calls(),
            ["action:key-0:a", "inspect:key-0:a", "samples:key-0:a"]
        );
        assert_eq!(second.calls().len(), 3);
    }

    #[test]
    fn step_reaches_only_the_owning_backend_under_its_own_id() {
        let (first, second, multi) = fan_in_pair();

        multi.step("key-0", "b").unwrap();
        assert!(first.calls().is_empty(), "{:?}", first.calls());
        assert_eq!(second.calls(), ["step:key-1:b"]);

        multi.step("key-0", "a").unwrap();
        assert_eq!(first.calls(), ["step:key-0:a"]);
        assert_eq!(second.calls().len(), 1);

        let err = multi.step("key-0", "zzz").unwrap_err();
        assert_eq!((err.status, err.code), (404, "not_found"));
        assert_eq!(first.calls().len() + second.calls().len(), 2);
    }

    #[test]
    fn start_stop_and_reset_reach_every_member_under_its_own_id() {
        let (first, second, multi) = fan_in_pair();

        multi.start("key-0", Some(500)).unwrap();
        multi.stop("key-0").unwrap();
        multi.reset("key-0").unwrap();

        assert_eq!(
            first.calls(),
            ["start:key-0:Some(500)", "stop:key-0", "reset:key-0"]
        );
        assert_eq!(
            second.calls(),
            ["start:key-1:Some(500)", "stop:key-1", "reset:key-1"]
        );
    }

    #[test]
    fn run_control_attempts_every_member_and_reports_the_first_error() {
        let (first, second, multi) = fan_in_pair();
        first.fail_run_control();

        let err = multi.start("key-0", None).unwrap_err();

        assert_eq!(err.message, "key-0 refused");
        // The member after the failing one was still started.
        assert_eq!(first.calls(), ["start:key-0:None"]);
        assert_eq!(second.calls(), ["start:key-1:None"]);

        second.fail_run_control();
        let err = multi.stop("key-0").unwrap_err();
        assert_eq!(err.message, "key-0 refused", "first member's error wins");
        assert_eq!(first.calls().len() + second.calls().len(), 4);
    }

    #[test]
    fn run_control_of_a_single_member_or_unregistered_id_is_forwarded_unchanged() {
        let only = Arc::new(Fake::new("key-0", "a"));
        let nodes: HashMap<BackendKey, Arc<dyn SimulationBackend>> = HashMap::from([(
            "s0:1".to_string(),
            only.clone() as Arc<dyn SimulationBackend>,
        )]);
        let created = ClusterCreated {
            id: "key-0".into(),
            assignment: Default::default(),
            members: vec![ClusterMember {
                backend: "s0:1".into(),
                id: "key-0".into(),
            }],
            multicast: None,
        };
        let multi = MultiBackend::for_tests(nodes, created);

        multi.start("key-0", None).unwrap();
        multi.stop("key-0").unwrap();
        // An id this process did not create goes to the primary server as is.
        let err = multi.reset("other").unwrap_err();

        assert_eq!(err.status, 404, "the backend answers for the unknown id");
        assert_eq!(only.calls(), ["start:key-0:None", "stop:key-0"]);
    }

    /// A `MultiBackend` whose only member is `only` (client id `key-0`).
    fn single_member_of(only: Arc<Fake>) -> MultiBackend {
        let nodes: HashMap<BackendKey, Arc<dyn SimulationBackend>> =
            HashMap::from([("s0:1".to_string(), only as Arc<dyn SimulationBackend>)]);
        let created = ClusterCreated {
            id: "key-0".into(),
            assignment: AssignmentTable {
                entries: vec![BoardAssignment {
                    board_id: "a".into(),
                    backend: "s0:1".into(),
                }],
            },
            members: vec![ClusterMember {
                backend: "s0:1".into(),
                id: "key-0".into(),
            }],
            multicast: None,
        };
        MultiBackend::for_tests(nodes, created)
    }

    /// With one member the multi-member merge is bypassed: the member's own
    /// answers (status, catalog, actions, log and live-batch `seq`) come back
    /// unchanged, nothing is merged or renumbered, and no merge state is kept.
    #[test]
    fn single_member_lifecycle_passes_member_answers_through_unmerged() {
        let mut fake = Fake::new("key-0", "a");
        fake.actions = vec![AdvertisedAction {
            board_id: "a".into(),
            device: "sw".into(),
            action: "press".into(),
            description: String::new(),
            params: vec![],
        }];
        let only = Arc::new(fake);
        // A member that is not stopped-and-ready would make a merge report
        // `starting`; the single member's own answer must come back as is.
        only.set_state("running", 1234);
        only.set_node_state("starting");
        only.set_probes_not_ready();
        only.push_log(5, 10, "first");
        only.push_log(9, 20, "second");
        only.live.lock().unwrap().push(live_batch(7, 30, "a"));
        let multi = single_member_of(only.clone());

        let status = multi.status("key-0").unwrap();
        assert_eq!(
            (status.state.as_str(), status.virtual_time_ns),
            ("running", 1234)
        );
        assert_eq!(status.nodes[0].state, "starting");

        multi.start("key-0", Some(500)).unwrap();
        multi.stop("key-0").unwrap();
        multi.reset("key-0").unwrap();

        // The member's 503 is forwarded; once complete the catalog is returned
        // without the merged-list sorting or fan-in bookkeeping.
        let err = multi.probe_catalog("key-0").unwrap_err();
        assert_eq!((err.status, err.code), (503, "not_ready"));
        only.set_probes("a", &["p1", "p0"]);
        assert_eq!(
            catalog_names(&multi.probe_catalog("key-0").unwrap()),
            ["a/p1", "a/p0"]
        );

        // Actions are not held back while the catalog is incomplete.
        only.set_probes_not_ready();
        assert_eq!(multi.actions("key-0").unwrap().actions.len(), 1);

        multi.invoke_action("key-0", invoke("a")).unwrap();

        multi
            .probe_watch_put("key-0", vec![probe_ref("a", "p0")])
            .unwrap();
        assert_eq!(multi.probe_watch_get("key-0").unwrap().probes.len(), 1);
        let live = multi.probe_watch_live("key-0", 0).unwrap();
        assert_eq!(live.iter().map(|batch| batch.seq).collect::<Vec<_>>(), [7]);
        multi.probe_watch_clear("key-0").unwrap();

        let logs = multi.logs_after("key-0", 0).unwrap();
        assert_eq!(logs.iter().map(|log| log.seq).collect::<Vec<_>>(), [5, 9]);
        assert_eq!(*only.log_asks.lock().unwrap(), [0]);

        multi.shutdown("key-0").unwrap();

        assert_eq!(
            only.calls(),
            [
                "start:key-0:Some(500)",
                "stop:key-0",
                "reset:key-0",
                "action:key-0:a",
                "watch_put:key-0:1",
                "watch_get:key-0",
                "watch_clear:key-0",
                "shutdown:key-0",
            ]
        );
        // No multi-member state was created for the single member.
        assert!(lock(&multi.fan_in.logs).is_empty());
        assert!(lock(&multi.fan_in.watches).is_empty());
        assert!(lock(&multi.fan_in.catalogs_complete).is_empty());
        assert!(lock(&multi.fan_in.csv).is_empty());
        // The simulation is forgotten after shutdown.
        assert!(multi.registry.members("key-0").is_none());
    }

    #[test]
    fn unknown_board_is_not_found_and_reaches_no_backend() {
        let (first, second, multi) = fan_in_pair();

        let errors = [
            multi.invoke_action("key-0", invoke("zzz")).unwrap_err(),
            multi
                .inspect("key-0", "zzz", InspectRequest::ReadRegs)
                .unwrap_err(),
            multi
                .probe_samples("key-0", "zzz", "p0", None, None)
                .unwrap_err(),
            multi
                .probe_watch_put("key-0", vec![probe_ref("a", "p0"), probe_ref("zzz", "p0")])
                .unwrap_err(),
        ];

        for err in errors {
            assert_eq!((err.status, err.code), (404, "not_found"), "{err:?}");
        }
        assert!(first.calls().is_empty() && second.calls().is_empty());
    }

    #[test]
    fn single_member_simulation_forwards_the_board_unchecked() {
        let only = Arc::new(Fake::new("key-0", "a"));
        let nodes: HashMap<BackendKey, Arc<dyn SimulationBackend>> = HashMap::from([(
            "s0:1".to_string(),
            only.clone() as Arc<dyn SimulationBackend>,
        )]);
        let created = ClusterCreated {
            id: "key-0".into(),
            assignment: Default::default(),
            members: vec![ClusterMember {
                backend: "s0:1".into(),
                id: "key-0".into(),
            }],
            multicast: None,
        };
        let multi = MultiBackend::for_tests(nodes, created);

        multi.invoke_action("key-0", invoke("whatever")).unwrap();
        multi
            .probe_watch_put("key-0", vec![probe_ref("whatever", "p0")])
            .unwrap();

        assert_eq!(only.calls(), ["action:key-0:whatever", "watch_put:key-0:1"]);
    }

    #[test]
    fn probe_watch_put_splits_probes_by_owner_and_skips_unrelated_backends() {
        let (first, second, multi) = fan_in_pair();

        let view = multi
            .probe_watch_put("key-0", vec![probe_ref("b", "p0"), probe_ref("b", "p1")])
            .unwrap();

        assert_eq!(view.probes.len(), 2);
        assert!(first.calls().is_empty(), "{:?}", first.calls());
        assert_eq!(second.calls(), ["watch_put:key-1:2"]);

        // Probes of both boards: each part to its owner; the view is the union.
        let view = multi
            .probe_watch_put("key-0", vec![probe_ref("b", "p0"), probe_ref("a", "p0")])
            .unwrap();
        assert_eq!(view.probes, [probe_ref("a", "p0"), probe_ref("b", "p0")]);
        assert_eq!(first.calls(), ["watch_put:key-0:1"]);
    }

    #[test]
    fn probe_watch_put_empties_the_watch_of_a_member_that_lost_all_its_probes() {
        let (first, second, multi) = fan_in_pair();
        multi
            .probe_watch_put("key-0", vec![probe_ref("a", "p0"), probe_ref("b", "p0")])
            .unwrap();

        let view = multi
            .probe_watch_put("key-0", vec![probe_ref("b", "p0")])
            .unwrap();

        assert_eq!(view.probes, [probe_ref("b", "p0")]);
        assert_eq!(first.calls(), ["watch_put:key-0:1", "watch_put:key-0:0"]);
        assert!(first.watch.lock().unwrap().is_empty());
        // `a` no longer holds a watch, so it is not asked again.
        multi.probe_watch_get("key-0").unwrap();
        multi.probe_watch_clear("key-0").unwrap();
        assert_eq!(first.calls().len(), 2);
        assert!(second.watch.lock().unwrap().is_empty());
    }

    #[test]
    fn probe_watch_get_and_clear_only_ask_members_holding_a_watch() {
        let (first, second, multi) = fan_in_pair();

        // Nothing watched yet: no backend is asked.
        assert!(multi.probe_watch_get("key-0").unwrap().probes.is_empty());
        multi.probe_watch_clear("key-0").unwrap();
        assert!(first.calls().is_empty() && second.calls().is_empty());

        multi
            .probe_watch_put("key-0", vec![probe_ref("a", "p0")])
            .unwrap();
        let view = multi.probe_watch_get("key-0").unwrap();
        assert_eq!(view.probes, [probe_ref("a", "p0")]);
        multi.probe_watch_clear("key-0").unwrap();

        assert_eq!(
            first.calls(),
            ["watch_put:key-0:1", "watch_get:key-0", "watch_clear:key-0"]
        );
        assert!(second.calls().is_empty(), "{:?}", second.calls());
        assert!(multi.probe_watch_get("key-0").unwrap().probes.is_empty());
        assert_eq!(first.calls().len(), 3);
    }

    fn live_batch(seq: u64, vt: u64, board: &str) -> CoarseBatch {
        CoarseBatch {
            seq,
            samples: vec![crate::probe_log::ProbeView {
                virtual_time_ns: vt,
                board: board.into(),
                probe: "p0".into(),
                kind: "digital".into(),
                value: "1".into(),
            }],
        }
    }

    #[test]
    fn probe_watch_live_merges_holders_under_one_seq_in_virtual_time_order() {
        let (first, second, multi) = fan_in_pair();
        multi
            .probe_watch_put("key-0", vec![probe_ref("a", "p0"), probe_ref("b", "p0")])
            .unwrap();
        first
            .live
            .lock()
            .unwrap()
            .extend([live_batch(7, 300, "a"), live_batch(8, 500, "a")]);
        second
            .live
            .lock()
            .unwrap()
            .extend([live_batch(3, 100, "b"), live_batch(4, 400, "b")]);

        let batches = multi.probe_watch_live("key-0", 0).unwrap();

        let seen: Vec<_> = batches
            .iter()
            .map(|b| {
                (
                    b.seq,
                    b.samples[0].board.as_str(),
                    b.samples[0].virtual_time_ns,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [(1, "b", 100), (2, "a", 300), (3, "b", 400), (4, "a", 500)]
        );

        // Next poll: members are asked from their own last seq; new batches
        // continue the merged numbering.
        second.live.lock().unwrap().push(live_batch(5, 600, "b"));
        let batches = multi.probe_watch_live("key-0", 4).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].seq, 5);
        assert_eq!(*first.live_asks.lock().unwrap(), [0, 8]);
        assert_eq!(*second.live_asks.lock().unwrap(), [0, 4]);
    }

    #[test]
    fn probe_watch_live_only_asks_holders() {
        let (first, second, multi) = fan_in_pair();
        multi
            .probe_watch_put("key-0", vec![probe_ref("a", "p0")])
            .unwrap();

        multi.probe_watch_live("key-0", 0).unwrap();

        assert_eq!(first.live_asks.lock().unwrap().len(), 1);
        assert!(second.live_asks.lock().unwrap().is_empty());
    }

    fn next_group(registry: &Registry) -> String {
        lock(&registry.multicast)
            .allocate("239.255.0.0".parse().unwrap())
            .unwrap()
            .ip
    }

    #[test]
    fn failed_create_with_undestroyable_member_stays_registered_until_delete_succeeds() {
        let mut second = Node::new("s1:1");
        second.fail_phase2 = true;
        let first = Node::new("s0:1");
        first.destroy_fails.store(true, Ordering::SeqCst);
        let (topology, plan, nodes) = setup(first, second);
        let registry = Registry::default();

        let err = registry
            .create(&topology, &plan, "key", &nodes)
            .unwrap_err();

        // The error says which id to DELETE, and the member is still tracked.
        assert!(err.message.contains("key-0 on s0:1"), "{err:?}");
        assert_eq!(lock(&registry.undestroyed).len(), 1);
        assert!(lock(&registry.sims).is_empty());
        // key-1 was destroyed by the rollback; the group stays reserved.
        assert_eq!(*nodes["s1:1"].destroyed.lock().unwrap(), vec!["key-1"]);
        assert_eq!(next_group(&registry), "239.255.0.1");

        // DELETE while the backend still refuses: error, still tracked.
        assert!(registry.shutdown(&nodes, "key-0").unwrap().is_err());
        assert_eq!(lock(&registry.undestroyed).len(), 1);

        // Backend recovers: DELETE finishes the cleanup and frees the group.
        nodes["s0:1"].destroy_fails.store(false, Ordering::SeqCst);
        assert!(registry.shutdown(&nodes, "key-0").unwrap().is_ok());
        assert_eq!(*nodes["s0:1"].destroyed.lock().unwrap(), vec!["key-0"]);
        assert!(lock(&registry.undestroyed).is_empty());
        assert_eq!(next_group(&registry), "239.255.0.0");
        // Nothing left to delete: the id is unknown to the registry again.
        assert!(registry.shutdown(&nodes, "key-0").is_none());
    }

    #[test]
    fn leftover_is_found_by_any_of_its_member_ids() {
        // Both members refuse destruction; the first listed id is not the one deleted.
        let mut second = Node::new("s1:1");
        second.fail_phase2 = true;
        second.destroy_fails.store(true, Ordering::SeqCst);
        let first = Node::new("s0:1");
        first.destroy_fails.store(true, Ordering::SeqCst);
        let (topology, plan, nodes) = setup(first, second);
        let registry = Registry::default();
        registry
            .create(&topology, &plan, "key", &nodes)
            .unwrap_err();
        assert_eq!(lock(&registry.undestroyed).len(), 1);

        for node in nodes.values() {
            node.destroy_fails.store(false, Ordering::SeqCst);
        }
        // Deleting by the second member's id retries both members.
        assert!(registry.shutdown(&nodes, "key-1").unwrap().is_ok());
        assert_eq!(*nodes["s0:1"].destroyed.lock().unwrap(), vec!["key-0"]);
        assert_eq!(*nodes["s1:1"].destroyed.lock().unwrap(), vec!["key-1"]);
        assert!(lock(&registry.undestroyed).is_empty());
    }

    #[test]
    fn clean_rollback_registers_nothing() {
        let mut second = Node::new("s1:1");
        second.fail_phase2 = true;
        let (topology, plan, nodes) = setup(Node::new("s0:1"), second);
        let registry = Registry::default();

        registry
            .create(&topology, &plan, "key", &nodes)
            .unwrap_err();

        assert!(lock(&registry.undestroyed).is_empty());
        assert!(lock(&registry.sims).is_empty());
        assert_eq!(next_group(&registry), "239.255.0.0");
    }
}

//! Two-phase create orchestration across `cluster-backend` instances.
//!
//! 1. Pick participants from health ([`plan_create`]): assign boards, then
//!    require identical [`ClusterSettings`] on every participant.
//! 2. Phase 1: send each participant the full topology (inline `base64:` ELF
//!    only for its own boards) plus the assignment; collect advertised endpoints.
//! 3. Phase 2: allocate a multicast group and send every participant the other
//!    participants' endpoints.
//!
//! A single participant skips phase 2 and allocates no multicast group: its
//! arbiter binds no peer sockets, so there is nothing to connect.
//! Any failure after phase 1 started destroys the already-created simulations.
//!
//! The frontend picks every simulation id itself ([`member_id`]) and sends it in
//! [`Phase1CreateRequest::id`], so the rollback knows what to `DELETE` even when
//! a phase-1 reply is lost or unparseable. Members that cannot be destroyed are
//! handed back in [`CreateFailure::leftover`] for the caller to keep and retry.

use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, PoisonError};

use crate::backend::SimulationBackend;
use crate::contract::ApiError;
use crate::topology::LogicalTopology;

use super::{
    AssignError, AssignmentTable, BackendKey, ClusterSettings, MulticastGroup, Phase1CreateRequest,
    Phase1CreateResponse, Phase2ConnectRequest, Phase2ConnectResponse, ServerProbe, assign_boards,
    healthy_capacities,
};

/// UDP port shared by every time-sync multicast group (groups differ by address).
pub const TIME_SYNC_MCAST_PORT: u16 = 47000;

/// Backend operations the create orchestration needs.
///
/// Implemented for every [`SimulationBackend`]; tests substitute a recording mock.
/// Method names differ from [`SimulationBackend`] on purpose so both traits can
/// be in scope without ambiguity.
pub trait ClusterNode {
    fn phase1_create(&self, req: Phase1CreateRequest) -> Result<Phase1CreateResponse, ApiError>;
    fn phase2_connect(&self, req: Phase2ConnectRequest) -> Result<Phase2ConnectResponse, ApiError>;
    /// Best-effort teardown of a simulation created in phase 1.
    fn destroy(&self, id: &str) -> Result<(), ApiError>;
}

impl<T: SimulationBackend + ?Sized> ClusterNode for T {
    fn phase1_create(&self, req: Phase1CreateRequest) -> Result<Phase1CreateResponse, ApiError> {
        self.create_phase1(req)
    }

    fn phase2_connect(&self, req: Phase2ConnectRequest) -> Result<Phase2ConnectResponse, ApiError> {
        self.connect_phase2(req)
    }

    fn destroy(&self, id: &str) -> Result<(), ApiError> {
        self.shutdown(id)
    }
}

/// Outcome of the pre-create checks (nothing has been sent to any backend yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePlan {
    pub assignment: AssignmentTable,
    /// Backends owning at least one board, in first-seen assignment order.
    pub participants: Vec<BackendKey>,
    /// Settings shared by every participant.
    pub settings: ClusterSettings,
}

/// One backend's simulation in a cluster-wide create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterMember {
    pub backend: BackendKey,
    /// Simulation id on that backend (its arbiter cluster key).
    pub id: String,
}

/// Result of a successful two-phase create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterCreated {
    /// Client-facing id: the first participant's own simulation id.
    pub id: String,
    pub assignment: AssignmentTable,
    pub members: Vec<ClusterMember>,
    /// `None` when only one backend participates.
    pub multicast: Option<MulticastGroup>,
}

/// Parts of a simulation that still have to be destroyed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leftover {
    pub members: Vec<ClusterMember>,
    /// Group kept reserved until every member is gone.
    pub multicast: Option<MulticastGroup>,
}

/// A failed [`create_cluster`].
#[derive(Debug)]
pub struct CreateFailure {
    /// What went wrong; the message names members the rollback could not destroy.
    pub error: ApiError,
    /// Members the rollback could not destroy. The caller must keep them
    /// registered so a later `DELETE` retries; `None` when everything is gone.
    pub leftover: Option<Leftover>,
}

impl CreateFailure {
    fn clean(error: ApiError) -> Self {
        Self {
            error,
            leftover: None,
        }
    }
}

/// Longest simulation id a backend accepts.
const MAX_SIMULATION_ID_LEN: usize = 128;

/// Check a frontend-chosen simulation id.
///
/// The id becomes part of file-system paths, iceoryx service names and URL
/// paths, so only ASCII letters, digits, `-` and `_` are allowed.
pub fn validate_simulation_id(id: &str) -> Result<(), ApiError> {
    let ok = !id.is_empty()
        && id.len() <= MAX_SIMULATION_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "invalid_id",
            format!(
                "simulation id must be 1-{MAX_SIMULATION_ID_LEN} characters of A-Z a-z 0-9 - _, got {id:?}"
            ),
        ))
    }
}

/// Id of the simulation on the `index`-th participant of the cluster `cluster_key`.
///
/// Distinct per participant, so two backends on one machine never share an
/// iceoryx root. Index 0 is the client-facing id.
#[must_use]
pub fn member_id(cluster_key: &str, index: usize) -> String {
    format!("{cluster_key}-{index}")
}

/// Hands out multicast groups sequentially starting at a base address.
#[derive(Debug, Default)]
pub struct MulticastAllocator {
    in_use: BTreeSet<u32>,
}

impl MulticastAllocator {
    /// Lowest free address `>= base` (IPv4 multicast range only).
    pub fn allocate(&mut self, base: Ipv4Addr) -> Result<MulticastGroup, String> {
        if !base.is_multicast() {
            return Err(format!("multicast_base {base} is not a multicast address"));
        }
        let max = u32::from(Ipv4Addr::new(239, 255, 255, 255));
        let mut candidate = u32::from(base);
        while self.in_use.contains(&candidate) {
            candidate += 1;
        }
        if candidate > max {
            return Err(format!(
                "no free multicast group at or above {base} (range exhausted)"
            ));
        }
        self.in_use.insert(candidate);
        Ok(MulticastGroup {
            ip: Ipv4Addr::from(candidate).to_string(),
            port: TIME_SYNC_MCAST_PORT,
        })
    }

    /// Return a group handed out by [`Self::allocate`].
    pub fn release(&mut self, group: &MulticastGroup) {
        if let Ok(ip) = group.ip.parse::<Ipv4Addr>() {
            self.in_use.remove(&u32::from(ip));
        }
    }
}

/// Drop inline `base64:` ELF payloads from boards that `backend` does not own.
///
/// Every backend still receives the complete board and edge lists; only the
/// (potentially large) firmware bytes are withheld from machines that never run
/// that board. `http(s)://` URLs are kept because they are tiny.
#[must_use]
pub fn trim_topology_for_backend(
    topology: &LogicalTopology,
    assignment: &AssignmentTable,
    backend: &str,
) -> LogicalTopology {
    let mut trimmed = topology.clone();
    for board in &mut trimmed.boards {
        let local = assignment.backend_for_board(&board.id) == Some(backend);
        if !local
            && board
                .elf
                .as_deref()
                .is_some_and(|e| e.starts_with("base64:"))
        {
            board.elf = None;
        }
    }
    trimmed
}

/// Check that `assignment` covers exactly the topology's boards, each once.
///
/// Run by the backend on phase 1 so a malformed table is rejected before any
/// arbiter is spawned.
pub fn validate_assignment(
    topology: &LogicalTopology,
    assignment: &AssignmentTable,
) -> Result<(), ApiError> {
    let bad = |msg: String| Err(ApiError::bad_request("invalid_assignment", msg));
    let mut assigned = std::collections::HashSet::new();
    for entry in &assignment.entries {
        if !topology.boards.iter().any(|b| b.id == entry.board_id) {
            return bad(format!(
                "assignment names unknown board `{}`",
                entry.board_id
            ));
        }
        if !assigned.insert(entry.board_id.as_str()) {
            return bad(format!("board `{}` is assigned twice", entry.board_id));
        }
    }
    if let Some(board) = topology
        .boards
        .iter()
        .find(|b| !assigned.contains(b.id.as_str()))
    {
        return bad(format!("board `{}` has no assignment", board.id));
    }
    Ok(())
}

/// Assign boards to healthy backends and verify participant settings agree.
pub fn plan_create(
    topology: &LogicalTopology,
    probes: &[ServerProbe],
) -> Result<CreatePlan, ApiError> {
    let capacities = healthy_capacities(probes);
    let board_ids: Vec<String> = topology.boards.iter().map(|b| b.id.clone()).collect();
    let assignment = assign_boards(&board_ids, &capacities).map_err(|err| match err {
        AssignError::NoCandidates => ApiError::new(
            503,
            "no_backend",
            format!("{err}; {}", describe_failures(probes)),
        ),
        AssignError::InsufficientCapacity { .. } => {
            ApiError::new(409, "insufficient_capacity", err.to_string())
        }
    })?;
    let participants = assignment.participating_backends();

    // Only participants must agree: an unused, differently configured backend
    // does not affect this simulation.
    let mut settings: Option<(&BackendKey, &ClusterSettings)> = None;
    for key in &participants {
        let cap = capacities
            .iter()
            .find(|c| &c.backend == key)
            .expect("participants come from the capacity list");
        match settings {
            None => settings = Some((key, &cap.settings)),
            Some((first_key, first)) => {
                if let Some(diff) = settings_diff(first, &cap.settings) {
                    return Err(ApiError::new(
                        409,
                        "config_mismatch",
                        format!("backends {first_key} and {key} disagree: {diff}"),
                    ));
                }
            }
        }
    }
    let settings = settings
        .map(|(_, s)| s.clone())
        .expect("assignment has at least one participant");

    Ok(CreatePlan {
        assignment,
        participants,
        settings,
    })
}

fn describe_failures(probes: &[ServerProbe]) -> String {
    let failed: Vec<String> = probes
        .iter()
        .filter_map(|p| match p {
            ServerProbe::Failed { server, error } => {
                Some(format!("{}:{}: {error}", server.ip, server.port))
            }
            ServerProbe::Healthy(_) => None,
        })
        .collect();
    if failed.is_empty() {
        "no servers configured".into()
    } else {
        format!("health failures: {}", failed.join("; "))
    }
}

fn settings_diff(a: &ClusterSettings, b: &ClusterSettings) -> Option<String> {
    let mut diffs = Vec::new();
    if a.time_sync_period_ms != b.time_sync_period_ms {
        diffs.push(format!(
            "time_sync_period_ms {} vs {}",
            a.time_sync_period_ms, b.time_sync_period_ms
        ));
    }
    if a.data_udp_window != b.data_udp_window {
        diffs.push(format!(
            "data_udp_window {} vs {}",
            a.data_udp_window, b.data_udp_window
        ));
    }
    if a.multicast_base != b.multicast_base {
        diffs.push(format!(
            "multicast_base {} vs {}",
            a.multicast_base, b.multicast_base
        ));
    }
    (!diffs.is_empty()).then(|| diffs.join(", "))
}

/// Run phase 1 and phase 2 for `plan`; participant `i` gets the id
/// [`member_id`]`(cluster_key, i)`.
///
/// On error every simulation that may exist is destroyed. Members that cannot be
/// destroyed are returned in [`CreateFailure::leftover`] (with the multicast
/// group still reserved), never silently dropped.
pub fn create_cluster<N: ClusterNode + ?Sized>(
    topology: &LogicalTopology,
    plan: &CreatePlan,
    cluster_key: &str,
    nodes: &HashMap<BackendKey, Arc<N>>,
    multicast: &Mutex<MulticastAllocator>,
) -> Result<ClusterCreated, CreateFailure> {
    let multi = plan.participants.len() > 1;

    // Phase 1: sequential, so a failure leaves a well-defined set to roll back.
    let mut members: Vec<ClusterMember> = Vec::with_capacity(plan.participants.len());
    let mut endpoints = Vec::new();
    for (index, key) in plan.participants.iter().enumerate() {
        let id = member_id(cluster_key, index);
        match phase1_one(topology, plan, nodes, key, &id, multi) {
            Ok((member, ep)) => {
                members.push(member);
                endpoints.extend(ep);
            }
            Err(failure) => {
                // A backend whose reply was lost or unusable may already run
                // its simulation, so it is rolled back with the others.
                members.extend(failure.created);
                return Err(rollback(&members, nodes, None, multicast, failure.error));
            }
        }
    }

    // Phase 2 (only when peers exist).
    let group = if multi {
        match allocate_group(plan, multicast) {
            Ok(group) => {
                if let Err(err) = connect_all(nodes, &members, cluster_key, &endpoints, &group) {
                    return Err(rollback(&members, nodes, Some(group), multicast, err));
                }
                Some(group)
            }
            Err(err) => return Err(rollback(&members, nodes, None, multicast, err)),
        }
    } else {
        None
    };

    Ok(ClusterCreated {
        id: members[0].id.clone(),
        assignment: plan.assignment.clone(),
        members,
        multicast: group,
    })
}

fn node_for<'a, N: ClusterNode + ?Sized>(
    nodes: &'a HashMap<BackendKey, Arc<N>>,
    key: &str,
) -> Result<&'a Arc<N>, ApiError> {
    nodes
        .get(key)
        .ok_or_else(|| ApiError::internal(format!("no client for backend {key}")))
}

/// Phase 1 failed on one backend.
struct Phase1Failure {
    /// Simulations this attempt may have created on the failing backend; the
    /// caller destroys them together with the earlier members. Empty when the
    /// backend explicitly refused the request.
    created: Vec<ClusterMember>,
    error: ApiError,
}

impl From<ApiError> for Phase1Failure {
    fn from(error: ApiError) -> Self {
        Self {
            created: Vec::new(),
            error,
        }
    }
}

/// Whether a failed phase-1 call may still have left a simulation behind.
///
/// A 4xx answer is an explicit refusal (bad request, capacity, id already in
/// use), so nothing was created and, importantly, the id must not be destroyed:
/// after a "id already in use" it would belong to someone else. Any other
/// failure (connection error, timeout, unreadable reply, 5xx) is ambiguous: the
/// backend may have created the simulation before the answer was lost.
fn may_have_created(err: &ApiError) -> bool {
    !(400..500).contains(&err.status)
}

fn phase1_one<N: ClusterNode + ?Sized>(
    topology: &LogicalTopology,
    plan: &CreatePlan,
    nodes: &HashMap<BackendKey, Arc<N>>,
    key: &BackendKey,
    id: &str,
    multi: bool,
) -> Result<(ClusterMember, Option<super::ArbiterEndpoints>), Phase1Failure> {
    let node = node_for(nodes, key)?;
    // The id is ours, so it can be destroyed whatever the reply looks like.
    let member = ClusterMember {
        backend: key.clone(),
        id: id.to_string(),
    };
    let reply = node.phase1_create(Phase1CreateRequest {
        id: id.to_string(),
        topology: trim_topology_for_backend(topology, &plan.assignment, key),
        assignment: plan.assignment.clone(),
        local_backend: key.clone(),
    });
    let reply = match reply {
        Ok(reply) => reply,
        Err(err) => {
            let created = if may_have_created(&err) {
                vec![member]
            } else {
                Vec::new()
            };
            return Err(Phase1Failure {
                created,
                error: prefix_error(key, "phase 1", err),
            });
        }
    };
    // The simulation exists on the backend from here on, so a bad reply must
    // still be rolled back by the caller.
    let unusable = |created: Vec<ClusterMember>, message: String| Phase1Failure {
        created,
        error: ApiError::internal(message),
    };
    if reply.id != member.id {
        // Not expected from a backend honouring `Phase1CreateRequest::id`; the
        // simulation it did create is destroyed as well.
        let other = ClusterMember {
            backend: key.clone(),
            id: reply.id.clone(),
        };
        return Err(unusable(
            vec![member, other],
            format!(
                "backend {key} created simulation {:?} instead of the requested {id:?}",
                reply.id
            ),
        ));
    }
    let endpoints = if multi {
        match reply.endpoints {
            Some(ep) if &ep.backend == key => Some(ep),
            Some(ep) => {
                return Err(unusable(
                    vec![member],
                    format!("backend {key} advertised endpoints for {}", ep.backend),
                ));
            }
            None => {
                return Err(unusable(
                    vec![member],
                    format!("backend {key} returned no arbiter endpoints"),
                ));
            }
        }
    } else {
        None
    };
    Ok((member, endpoints))
}

fn allocate_group(
    plan: &CreatePlan,
    multicast: &Mutex<MulticastAllocator>,
) -> Result<MulticastGroup, ApiError> {
    let base: Ipv4Addr = plan.settings.multicast_base.parse().map_err(|_| {
        ApiError::internal(format!(
            "backends reported invalid multicast_base {:?}",
            plan.settings.multicast_base
        ))
    })?;
    multicast
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .allocate(base)
        .map_err(|msg| ApiError::new(503, "multicast_exhausted", msg))
}

/// Send every member its peers and `group`. The group is not released here:
/// the caller owns it and decides after the rollback.
fn connect_all<N: ClusterNode + ?Sized>(
    nodes: &HashMap<BackendKey, Arc<N>>,
    members: &[ClusterMember],
    cluster_key: &str,
    endpoints: &[super::ArbiterEndpoints],
    group: &MulticastGroup,
) -> Result<(), ApiError> {
    for member in members {
        let peers = endpoints
            .iter()
            .filter(|ep| ep.backend != member.backend)
            .cloned()
            .collect();
        let result = node_for(nodes, &member.backend).and_then(|node| {
            node.phase2_connect(Phase2ConnectRequest {
                id: member.id.clone(),
                cluster_key: cluster_key.to_string(),
                peers,
                multicast: group.clone(),
            })
            .map_err(|err| prefix_error(&member.backend, "phase 2", err))
        });
        let failure = match result {
            Ok(reply) if reply.ok => continue,
            Ok(_) => ApiError::internal(format!(
                "backend {} rejected phase 2 without an error",
                member.backend
            )),
            Err(err) => err,
        };
        return Err(failure);
    }
    Ok(())
}

/// Destroy every member, newest first, and return the error that caused the
/// rollback.
///
/// When every member is gone the multicast `group` (if any) is released.
/// Members that could not be destroyed are named in the message and returned
/// in [`CreateFailure::leftover`] together with the still-reserved group, so
/// the caller can keep them and retry on a later `DELETE`.
fn rollback<N: ClusterNode + ?Sized>(
    members: &[ClusterMember],
    nodes: &HashMap<BackendKey, Arc<N>>,
    group: Option<MulticastGroup>,
    multicast: &Mutex<MulticastAllocator>,
    cause: ApiError,
) -> CreateFailure {
    let failed = destroy_members(members.iter().rev(), nodes);
    if failed.is_empty() {
        if let Some(group) = &group {
            multicast
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .release(group);
        }
        return CreateFailure::clean(cause);
    }
    let described: Vec<String> = failed
        .iter()
        .map(|(member, err)| format!("{} on {}: {}", member.id, member.backend, err.message))
        .collect();
    let error = ApiError {
        message: format!(
            "{}; rollback incomplete, simulation(s) may remain (DELETE one of these ids to retry): {}",
            cause.message,
            described.join("; ")
        ),
        ..cause
    };
    CreateFailure {
        error,
        leftover: Some(Leftover {
            members: failed.into_iter().map(|(member, _)| member).collect(),
            multicast: group,
        }),
    }
}

/// Destroy each of `members` on its backend; every member is attempted even
/// after a failure.
///
/// A backend answering 404 no longer knows the simulation, which is the
/// desired end state. Any other failure is retried once, because a transient
/// connection error must not leave an arbiter running. Members that still
/// failed are returned with their last error.
pub(super) fn destroy_members<'a, N: ClusterNode + ?Sized>(
    members: impl IntoIterator<Item = &'a ClusterMember>,
    nodes: &HashMap<BackendKey, Arc<N>>,
) -> Vec<(ClusterMember, ApiError)> {
    let mut failed = Vec::new();
    for member in members {
        let result = match nodes.get(&member.backend) {
            Some(node) => destroy_with_retry(node.as_ref(), member),
            None => Err(ApiError::internal(format!(
                "no client for backend {}",
                member.backend
            ))),
        };
        if let Err(err) = result {
            eprintln!(
                "simulation-frontend: destroy of {} on {} failed: {err}",
                member.id, member.backend
            );
            failed.push((member.clone(), err));
        }
    }
    failed
}

fn destroy_with_retry<N: ClusterNode + ?Sized>(
    node: &N,
    member: &ClusterMember,
) -> Result<(), ApiError> {
    let gone = |err: &ApiError| err.status == 404;
    match node.destroy(&member.id) {
        Err(err) if !gone(&err) => match node.destroy(&member.id) {
            Err(retry) if !gone(&retry) => Err(retry),
            _ => Ok(()),
        },
        Err(_) | Ok(()) => Ok(()),
    }
}

fn prefix_error(backend: &str, stage: &str, err: ApiError) -> ApiError {
    ApiError {
        message: format!("{stage} on {backend}: {}", err.message),
        ..err
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use crate::config::ServerAddr;
    use crate::topology::BoardSpec;

    use super::super::{ArbiterEndpoints, BackendCapacity, BoardAssignment};
    use super::*;

    fn settings() -> ClusterSettings {
        ClusterSettings {
            time_sync_period_ms: 100,
            data_udp_window: 32,
            multicast_base: "239.255.0.0".into(),
        }
    }

    fn healthy(backend: &str, max: u32, running: u32) -> ServerProbe {
        ServerProbe::Healthy(BackendCapacity {
            backend: backend.into(),
            max_boards: max,
            running_boards: running,
            settings: settings(),
        })
    }

    fn topo(boards: &[(&str, Option<&str>)]) -> LogicalTopology {
        LogicalTopology {
            boards: boards
                .iter()
                .map(|(id, elf)| BoardSpec {
                    id: (*id).into(),
                    kind: "rl78".into(),
                    elf: elf.map(str::to_string),
                    endpoints: vec![],
                })
                .collect(),
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 200,
            ready_timeout_ms: 10_000,
        }
    }

    /// Records every call in order; failures are injected by label.
    #[derive(Default)]
    struct MockNode {
        key: String,
        log: Arc<StdMutex<Vec<String>>>,
        fail_phase1: bool,
        fail_phase2: bool,
        /// Phase 1 succeeds but advertises no endpoints (unusable reply).
        omit_endpoints: bool,
        /// Remaining `destroy` calls that fail with a 500.
        destroy_failures: std::sync::atomic::AtomicU32,
        /// `destroy` answers 404 (the backend no longer knows the simulation).
        destroy_not_found: bool,
        /// Phase 1 is answered with this 4xx status without creating anything.
        refuse_phase1: Option<u16>,
        /// Phase 1 creates the simulation but the reply never arrives.
        lose_phase1_reply: bool,
        /// Phase 1 reports this id instead of the requested one.
        reply_id: Option<String>,
        /// Ids this backend actually created (whatever the frontend saw).
        created: StdMutex<Vec<String>>,
        seen_topology: StdMutex<Vec<Phase1CreateRequest>>,
        seen_phase2: StdMutex<Vec<Phase2ConnectRequest>>,
    }

    impl MockNode {
        fn new(key: &str, log: &Arc<StdMutex<Vec<String>>>) -> Self {
            Self {
                key: key.into(),
                log: log.clone(),
                ..Self::default()
            }
        }
    }

    impl ClusterNode for MockNode {
        fn phase1_create(
            &self,
            req: Phase1CreateRequest,
        ) -> Result<Phase1CreateResponse, ApiError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("phase1:{}", self.key));
            let multi = req.assignment.participating_backends().len() > 1;
            let id = req.id.clone();
            self.seen_topology.lock().unwrap().push(req);
            if let Some(status) = self.refuse_phase1 {
                return Err(ApiError::new(status, "refused", "request refused"));
            }
            if self.fail_phase1 {
                return Err(ApiError::internal("boom"));
            }
            if self.lose_phase1_reply {
                // The simulation now exists, but the frontend never learns it.
                self.created.lock().unwrap().push(id);
                return Err(ApiError::internal("connection reset while reading reply"));
            }
            self.created.lock().unwrap().push(id.clone());
            Ok(Phase1CreateResponse {
                id: self.reply_id.clone().unwrap_or(id),
                endpoints: (multi && !self.omit_endpoints).then(|| ArbiterEndpoints {
                    backend: self.key.clone(),
                    advertise_ip: "127.0.0.1".into(),
                    control_tcp_port: 9000,
                    data_udp_port: 9001,
                }),
            })
        }

        fn phase2_connect(
            &self,
            req: Phase2ConnectRequest,
        ) -> Result<Phase2ConnectResponse, ApiError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("phase2:{}:peers={}", self.key, req.peers.len()));
            self.seen_phase2.lock().unwrap().push(req);
            if self.fail_phase2 {
                return Err(ApiError::internal("connect failed"));
            }
            Ok(Phase2ConnectResponse { ok: true })
        }

        fn destroy(&self, id: &str) -> Result<(), ApiError> {
            self.log.lock().unwrap().push(format!("destroy:{id}"));
            if self.destroy_not_found {
                return Err(ApiError::not_found("unknown simulation"));
            }
            let failing = self
                .destroy_failures
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok();
            if failing {
                return Err(ApiError::internal("destroy refused"));
            }
            Ok(())
        }
    }

    fn nodes(mocks: Vec<MockNode>) -> HashMap<BackendKey, Arc<MockNode>> {
        mocks
            .into_iter()
            .map(|m| (m.key.clone(), Arc::new(m)))
            .collect()
    }

    #[test]
    fn plan_rejects_insufficient_capacity_before_any_create() {
        let t = topo(&[("a", None), ("b", None), ("c", None)]);
        let probes = [healthy("s0:1", 1, 0), healthy("s1:1", 1, 0)];
        let err = plan_create(&t, &probes).unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.code, "insufficient_capacity");
    }

    #[test]
    fn plan_reports_health_failures_when_no_candidates() {
        let t = topo(&[("a", None)]);
        let probes = [ServerProbe::Failed {
            server: ServerAddr {
                ip: "10.0.0.9".into(),
                port: 8091,
            },
            error: "connection refused".into(),
        }];
        let err = plan_create(&t, &probes).unwrap_err();
        assert_eq!(err.status, 503);
        assert!(err.message.contains("10.0.0.9:8091"));
        assert!(err.message.contains("connection refused"));
    }

    #[test]
    fn plan_rejects_settings_mismatch_between_participants() {
        let t = topo(&[("a", None), ("b", None)]);
        let mut other = healthy("s1:1", 1, 0);
        if let ServerProbe::Healthy(cap) = &mut other {
            cap.settings.data_udp_window = 64;
        }
        let probes = [healthy("s0:1", 1, 0), other];
        let err = plan_create(&t, &probes).unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.code, "config_mismatch");
        assert!(err.message.contains("data_udp_window 32 vs 64"));
    }

    #[test]
    fn plan_ignores_settings_of_non_participants() {
        let t = topo(&[("a", None)]);
        let mut other = healthy("s1:1", 4, 3);
        if let ServerProbe::Healthy(cap) = &mut other {
            cap.settings.multicast_base = "239.1.0.0".into();
        }
        let probes = [healthy("s0:1", 4, 0), other];
        let plan = plan_create(&t, &probes).unwrap();
        assert_eq!(plan.participants, vec!["s0:1".to_string()]);
        assert_eq!(plan.settings, settings());
    }

    #[test]
    fn trim_keeps_base64_only_for_owned_boards() {
        let t = topo(&[
            ("a", Some("base64:QUFB")),
            ("b", Some("base64:QkJC")),
            ("c", Some("https://example.test/c.elf")),
            ("d", None),
        ]);
        let assignment = AssignmentTable {
            entries: ["a", "b", "c", "d"]
                .iter()
                .zip(["s0:1", "s1:1", "s1:1", "s1:1"])
                .map(|(b, s)| BoardAssignment {
                    board_id: (*b).into(),
                    backend: s.into(),
                })
                .collect(),
        };
        let for_s0 = trim_topology_for_backend(&t, &assignment, "s0:1");
        assert_eq!(for_s0.boards[0].elf.as_deref(), Some("base64:QUFB"));
        assert_eq!(for_s0.boards[1].elf, None);
        // URLs stay: they carry no firmware bytes.
        assert_eq!(
            for_s0.boards[2].elf.as_deref(),
            Some("https://example.test/c.elf")
        );
        assert_eq!(for_s0.boards.len(), 4);

        let for_s1 = trim_topology_for_backend(&t, &assignment, "s1:1");
        assert_eq!(for_s1.boards[0].elf, None);
        assert_eq!(for_s1.boards[1].elf.as_deref(), Some("base64:QkJC"));
        let json = serde_json::to_string(&for_s1).unwrap();
        assert!(!json.contains("QUFB"));
    }

    #[test]
    fn two_backends_run_phase1_then_phase2_and_trim_elf() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let t = topo(&[("a", Some("base64:QUFB")), ("b", Some("base64:QkJC"))]);
        let probes = [healthy("s0:1", 1, 0), healthy("s1:1", 1, 0)];
        let plan = plan_create(&t, &probes).unwrap();
        let map = nodes(vec![
            MockNode::new("s0:1", &log),
            MockNode::new("s1:1", &log),
        ]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let created = create_cluster(&t, &plan, "k", &map, &mcast).unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "phase1:s0:1",
                "phase1:s1:1",
                "phase2:s0:1:peers=1",
                "phase2:s1:1:peers=1"
            ]
        );
        assert_eq!(created.id, "k-0");
        assert_eq!(created.members.len(), 2);
        assert_eq!(created.multicast.as_ref().unwrap().ip, "239.255.0.0");

        let s0 = map["s0:1"].seen_topology.lock().unwrap();
        let wire0 = serde_json::to_string(&s0[0]).unwrap();
        assert!(wire0.contains("QUFB"));
        assert!(!wire0.contains("QkJC"), "other machine's base64 leaked");
        let s1 = map["s1:1"].seen_topology.lock().unwrap();
        let wire1 = serde_json::to_string(&s1[0]).unwrap();
        assert!(wire1.contains("QkJC"));
        assert!(!wire1.contains("QUFB"), "other machine's base64 leaked");
    }

    #[test]
    fn single_participant_skips_phase2_and_multicast() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let t = topo(&[("a", None), ("b", None)]);
        let probes = [healthy("s0:1", 4, 0), healthy("s1:1", 4, 0)];
        let plan = plan_create(&t, &probes).unwrap();
        assert_eq!(plan.participants.len(), 1);
        let map = nodes(vec![
            MockNode::new("s0:1", &log),
            MockNode::new("s1:1", &log),
        ]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let created = create_cluster(&t, &plan, "k", &map, &mcast).unwrap();

        assert_eq!(*log.lock().unwrap(), vec!["phase1:s0:1"]);
        assert_eq!(created.multicast, None);
        assert_eq!(created.id, "k-0");
    }

    #[test]
    fn phase1_failure_destroys_earlier_backends_and_skips_phase2() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let t = topo(&[("a", None), ("b", None)]);
        let probes = [healthy("s0:1", 1, 0), healthy("s1:1", 1, 0)];
        let plan = plan_create(&t, &probes).unwrap();
        let mut second = MockNode::new("s1:1", &log);
        second.fail_phase1 = true;
        let map = nodes(vec![MockNode::new("s0:1", &log), second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        assert!(err.message.contains("phase 1 on s1:1"));
        assert_eq!(
            *log.lock().unwrap(),
            // A 500 may hide a half-finished create, so the failing member's
            // id is destroyed too (a backend that knows nothing answers 404).
            vec!["phase1:s0:1", "phase1:s1:1", "destroy:k-1", "destroy:k-0"]
        );
    }

    #[test]
    fn each_member_gets_a_distinct_frontend_chosen_id_in_phase1_and_phase2() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let map = nodes(vec![
            MockNode::new("s0:1", &log),
            MockNode::new("s1:1", &log),
        ]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let created = create_cluster(&t, &plan, "k", &map, &mcast).unwrap();

        assert_eq!(*map["s0:1"].created.lock().unwrap(), vec!["k-0"]);
        assert_eq!(*map["s1:1"].created.lock().unwrap(), vec!["k-1"]);
        assert_eq!(map["s0:1"].seen_topology.lock().unwrap()[0].id, "k-0");
        assert_eq!(map["s1:1"].seen_topology.lock().unwrap()[0].id, "k-1");
        assert_eq!(
            created
                .members
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["k-0", "k-1"]
        );
        assert_eq!(created.id, "k-0");
    }

    #[test]
    fn every_phase2_request_carries_the_same_shared_cluster_key() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let map = nodes(vec![
            MockNode::new("s0:1", &log),
            MockNode::new("s1:1", &log),
        ]);
        let mcast = Mutex::new(MulticastAllocator::default());

        create_cluster(&t, &plan, "k", &map, &mcast).unwrap();

        let first = map["s0:1"].seen_phase2.lock().unwrap();
        let second = map["s1:1"].seen_phase2.lock().unwrap();
        // The ids differ per member; the shared key (no `-N` suffix) does not.
        assert_eq!(
            (first[0].id.as_str(), second[0].id.as_str()),
            ("k-0", "k-1")
        );
        assert_eq!(first[0].cluster_key, "k");
        assert_eq!(second[0].cluster_key, "k");
    }

    #[test]
    fn lost_phase1_reply_is_still_rolled_back_by_the_requested_id() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut second = MockNode::new("s1:1", &log);
        second.lose_phase1_reply = true;
        let map = nodes(vec![MockNode::new("s0:1", &log), second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let failure = create_cluster(&t, &plan, "k", &map, &mcast).unwrap_err();

        // The reply never arrived, yet the simulation s1 created is destroyed.
        assert!(failure.error.message.contains("phase 1 on s1:1"));
        assert_eq!(*map["s1:1"].created.lock().unwrap(), vec!["k-1"]);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["phase1:s0:1", "phase1:s1:1", "destroy:k-1", "destroy:k-0"]
        );
        assert_eq!(failure.leftover, None);
    }

    #[test]
    fn explicit_phase1_refusal_does_not_destroy_the_refused_id() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut second = MockNode::new("s1:1", &log);
        // e.g. "simulation id already exists": that id is not ours to destroy.
        second.refuse_phase1 = Some(409);
        let map = nodes(vec![MockNode::new("s0:1", &log), second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let failure = create_cluster(&t, &plan, "k", &map, &mcast).unwrap_err();

        assert_eq!(failure.error.status, 409);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["phase1:s0:1", "phase1:s1:1", "destroy:k-0"]
        );
    }

    #[test]
    fn reply_with_a_different_id_destroys_both_the_requested_and_the_reported_id() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut second = MockNode::new("s1:1", &log);
        second.reply_id = Some("other".into());
        let map = nodes(vec![MockNode::new("s0:1", &log), second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        assert!(err.message.contains("instead of the requested"), "{err:?}");
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "phase1:s0:1",
                "phase1:s1:1",
                "destroy:other",
                "destroy:k-1",
                "destroy:k-0"
            ]
        );
    }

    #[test]
    fn incomplete_rollback_returns_the_undestroyed_members_and_keeps_the_group_reserved() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut first = MockNode::new("s0:1", &log);
        first.destroy_failures = std::sync::atomic::AtomicU32::new(u32::MAX);
        let mut second = MockNode::new("s1:1", &log);
        second.fail_phase2 = true;
        let map = nodes(vec![first, second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let failure = create_cluster(&t, &plan, "k", &map, &mcast).unwrap_err();

        let leftover = failure.leftover.expect("k-0 could not be destroyed");
        assert_eq!(
            leftover.members,
            vec![ClusterMember {
                backend: "s0:1".into(),
                id: "k-0".into()
            }]
        );
        assert_eq!(
            leftover.multicast,
            Some(MulticastGroup {
                ip: "239.255.0.0".into(),
                port: TIME_SYNC_MCAST_PORT
            })
        );
        assert!(failure.error.message.contains("k-0 on s0:1"));
        // The group is still reserved, so the next allocation skips it.
        let next = mcast
            .lock()
            .unwrap()
            .allocate("239.255.0.0".parse().unwrap())
            .unwrap();
        assert_eq!(next.ip, "239.255.0.1");
    }

    #[test]
    fn simulation_ids_are_limited_to_safe_characters() {
        for ok in ["a", "1700-42-0-1", "A_b-9", &"x".repeat(128)] {
            assert!(validate_simulation_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "a/b", "..", "a b", "é", "a.b", &"x".repeat(129)] {
            let err = validate_simulation_id(bad).unwrap_err();
            assert_eq!((err.status, err.code), (400, "invalid_id"), "{bad:?}");
        }
    }

    #[test]
    fn phase2_failure_destroys_all_and_releases_multicast() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let t = topo(&[("a", None), ("b", None)]);
        let probes = [healthy("s0:1", 1, 0), healthy("s1:1", 1, 0)];
        let plan = plan_create(&t, &probes).unwrap();
        let mut second = MockNode::new("s1:1", &log);
        second.fail_phase2 = true;
        let map = nodes(vec![MockNode::new("s0:1", &log), second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        assert!(err.message.contains("phase 2 on s1:1"));
        let calls = log.lock().unwrap().clone();
        assert_eq!(&calls[calls.len() - 2..], ["destroy:k-1", "destroy:k-0"]);
        // The released group is handed out again.
        let again = mcast
            .lock()
            .unwrap()
            .allocate("239.255.0.0".parse().unwrap())
            .unwrap();
        assert_eq!(again.ip, "239.255.0.0");
    }

    fn two_backend_setup() -> (LogicalTopology, CreatePlan) {
        let t = topo(&[("a", None), ("b", None)]);
        let probes = [healthy("s0:1", 1, 0), healthy("s1:1", 1, 0)];
        let plan = plan_create(&t, &probes).unwrap();
        (t, plan)
    }

    #[test]
    fn phase2_failure_on_first_member_still_destroys_every_member() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut first = MockNode::new("s0:1", &log);
        first.fail_phase2 = true;
        let map = nodes(vec![first, MockNode::new("s1:1", &log)]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        assert!(err.message.contains("phase 2 on s0:1"));
        // s1 never reached phase 2 but its arbiter is running, so it is destroyed.
        let calls = log.lock().unwrap().clone();
        assert_eq!(&calls[calls.len() - 2..], ["destroy:k-1", "destroy:k-0"]);
    }

    #[test]
    fn unusable_phase1_reply_destroys_that_backend_and_earlier_ones() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut second = MockNode::new("s1:1", &log);
        second.omit_endpoints = true;
        let map = nodes(vec![MockNode::new("s0:1", &log), second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        assert!(err.message.contains("no arbiter endpoints"));
        assert_eq!(
            *log.lock().unwrap(),
            vec!["phase1:s0:1", "phase1:s1:1", "destroy:k-1", "destroy:k-0"]
        );
    }

    #[test]
    fn rollback_retries_a_failed_destroy_once_and_stays_silent_on_success() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut first = MockNode::new("s0:1", &log);
        first.destroy_failures = std::sync::atomic::AtomicU32::new(1);
        let mut second = MockNode::new("s1:1", &log);
        second.fail_phase2 = true;
        let map = nodes(vec![first, second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        assert!(!err.message.contains("rollback incomplete"), "{err:?}");
        let calls = log.lock().unwrap().clone();
        assert_eq!(
            &calls[calls.len() - 3..],
            ["destroy:k-1", "destroy:k-0", "destroy:k-0"]
        );
    }

    #[test]
    fn rollback_attempts_every_member_and_reports_the_ones_it_could_not_destroy() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut first = MockNode::new("s0:1", &log);
        first.destroy_failures = std::sync::atomic::AtomicU32::new(u32::MAX);
        let mut second = MockNode::new("s1:1", &log);
        second.fail_phase2 = true;
        let map = nodes(vec![first, second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        // Original cause and status survive; the leftover is named.
        assert_eq!(err.status, 500);
        assert!(err.message.contains("phase 2 on s1:1"), "{err:?}");
        assert!(
            err.message
                .contains("rollback incomplete, simulation(s) may remain (DELETE one of these ids to retry): k-0 on s0:1"),
            "{err:?}"
        );
        assert!(!err.message.contains("k-1 on s1:1"), "{err:?}");
    }

    #[test]
    fn rollback_treats_not_found_as_already_destroyed() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (t, plan) = two_backend_setup();
        let mut first = MockNode::new("s0:1", &log);
        first.destroy_not_found = true;
        let mut second = MockNode::new("s1:1", &log);
        second.fail_phase1 = true;
        let map = nodes(vec![first, second]);
        let mcast = Mutex::new(MulticastAllocator::default());

        let err = create_cluster(&t, &plan, "k", &map, &mcast)
            .unwrap_err()
            .error;

        assert!(!err.message.contains("rollback incomplete"), "{err:?}");
        let calls = log.lock().unwrap().clone();
        // 404 is final: no retry.
        assert_eq!(calls.iter().filter(|c| *c == "destroy:k-0").count(), 1);
    }

    #[test]
    fn multicast_allocator_is_sequential_and_reuses_released() {
        let mut alloc = MulticastAllocator::default();
        let base: Ipv4Addr = "239.255.0.0".parse().unwrap();
        let g0 = alloc.allocate(base).unwrap();
        let g1 = alloc.allocate(base).unwrap();
        assert_eq!(g0.ip, "239.255.0.0");
        assert_eq!(g1.ip, "239.255.0.1");
        assert_eq!(g0.port, TIME_SYNC_MCAST_PORT);
        alloc.release(&g0);
        assert_eq!(alloc.allocate(base).unwrap().ip, "239.255.0.0");
        assert!(alloc.allocate("10.0.0.1".parse().unwrap()).is_err());
        let last: Ipv4Addr = "239.255.255.255".parse().unwrap();
        alloc.allocate(last).unwrap();
        assert!(alloc.allocate(last).unwrap_err().contains("exhausted"));
    }
}

//! Long-lived simulation worker used by `cluster-backend`.
//!
//! Internal HTTP is served by [`crate::backend::serve_internal`]; this module
//! owns arbiter processes and the control-plane job queue used by
//! [`crate::backend::LocalBackend`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use iceoryx2::prelude::{Node, ipc};

use crate::action::{ActionRequest, ActionUpstream};
use crate::arbiter::PeerAdvertise;
use crate::backend::{BackendInfo, LocalBackend};
use crate::contract::{
    ActionList, AdvertisedAction, ApiError, InvokeActionRequest, LogEvent, ProbeSamplesResponse,
    StatusEvent, TopologyView, prepare_create_topology,
};
use crate::host::{HostCommand, SessionNode};
use crate::inspect::{InspectReply, InspectRequest, inspect_rpc, inspect_sock_path};
use crate::ipc::{
    IpcError, MSG_CATALOG, MSG_CATALOG_END, MSG_SAMPLE, ProbeQueryWire, ProbeWatchWire,
    ServerActions, ServerHostPort, ServerLogInbox, ServerProbeCtrl, ServerProbeInbox,
    board_hash_table, board_id_hash, create_node, isolated_config, new_cluster_key,
    root_path_for_cluster,
};
use crate::log_msg::{ClusterLog, LogOrigin};
use crate::orchestrate::{
    ArbiterEndpoints, AssignmentTable, Phase1CreateRequest, Phase1CreateResponse,
    Phase2ConnectRequest, Phase2ConnectResponse, validate_assignment, validate_simulation_id,
};
use crate::probe_log::{
    CoarseBatch, ProbeCatalogItem, ProbeKind, ProbePointName, ProbeRange, ProbeRecord, ProbeRef,
    ProbeView, ProbeWatchView, json_view, record_view,
};
use crate::topology::{LogicalTopology, TopologyError};

/// Live SSE readers start at the high-water mark, so this only covers logs
/// that arrive between polls.
const LOG_CAP: usize = 100;

/// Live batches kept while `/probes/stream` is between reconnects.
const WATCH_PENDING_KEEP: usize = 64;

/// How long phase 1 waits for a spawned arbiter to publish its peer endpoints.
const ENDPOINTS_WAIT: Duration = Duration::from_secs(10);
/// How long a deleted simulation's arbiter gets to stop its nodes and exit
/// after `Shutdown` before its whole process group is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Options for [`spawn_worker`].
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Path to the `cluster-arbiter` executable.
    pub arbiter_bin: PathBuf,
    /// `BackendConfig.advertise_ip`: address peers use to reach this machine's arbiters.
    pub advertise_ip: String,
    /// `BackendConfig.time_sync_period_ms`: period of the arbiters' time-sync multicast.
    pub time_sync_period_ms: u64,
    /// `BackendConfig.data_udp_window`: messages in flight per peer on the arbiters' data UDP link.
    pub data_udp_window: u32,
}

impl ServerOptions {
    /// Sibling `cluster-arbiter` next to this process.
    pub fn from_current_exe(advertise_ip: impl Into<String>) -> Result<Self, ServerError> {
        let exe = std::env::current_exe().map_err(ServerError::Io)?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| ServerError::Message("current_exe has no parent".into()))?;
        Ok(Self {
            arbiter_bin: exe_dir.join("cluster-arbiter"),
            advertise_ip: advertise_ip.into(),
            time_sync_period_ms: crate::arbiter::DEFAULT_TIME_SYNC_PERIOD_MS,
            data_udp_window: crate::cluster_net::DEFAULT_DATA_UDP_WINDOW,
        })
    }
}

/// Server failures.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Message(String),
}

#[derive(Clone, Debug)]
struct StoredLog {
    seq: u64,
    level: String,
    origin: String,
    board: Option<String>,
    text: String,
    virtual_time_ns: u64,
}

impl StoredLog {
    fn to_event(&self) -> LogEvent {
        LogEvent {
            seq: self.seq,
            level: self.level.clone(),
            origin: self.origin.clone(),
            board: self.board.clone(),
            text: self.text.clone(),
            virtual_time_ns: self.virtual_time_ns,
        }
    }
}

/// Work item for the simulation worker thread.
pub struct Job {
    pub kind: JobKind,
    pub reply: Sender<JobReply>,
}

/// Operations handled by the worker.
#[derive(Debug)]
pub enum JobKind {
    Create(LogicalTopology),
    CreatePhase1(Phase1CreateRequest),
    ConnectPhase2(Phase2ConnectRequest),
    Command {
        id: String,
        cmd: HostCommand,
    },
    Delete {
        id: String,
    },
    Exists {
        id: String,
    },
    GetStatus {
        id: String,
    },
    GetLogs {
        id: String,
        after: u64,
    },
    LogHighWater {
        id: String,
    },
    ListActions {
        id: String,
    },
    Invoke {
        id: String,
        request: InvokeActionRequest,
    },
    ProbeCatalog {
        id: String,
    },
    ProbeWatchGet {
        id: String,
    },
    ProbeWatchPut {
        id: String,
        probes: Vec<ProbeRef>,
    },
    ProbeWatchClear {
        id: String,
    },
    ProbeWatchLive {
        id: String,
        after: u64,
    },
    ProbeCsvStart {
        id: String,
        range: ProbeRange,
    },
    ProbeCsvPoll {
        id: String,
        request_id: u64,
    },
    ProbeCsvStatus {
        id: String,
        request_id: u64,
    },
    ProbeSamples {
        id: String,
        board: String,
        probe: String,
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    },
    GetTopology {
        id: String,
    },
    ViewerAttach {
        id: String,
    },
    ViewerCheck {
        id: String,
        token: String,
    },
    ViewerDetach {
        id: String,
        token: String,
    },
    Inspect {
        id: String,
        board_id: String,
        req: InspectRequest,
    },
    Step {
        id: String,
        board_id: String,
    },
}

/// Worker → backend replies.
#[derive(Debug)]
pub enum JobReply {
    Created {
        id: String,
    },
    CreatedPhase1(Phase1CreateResponse),
    Phase2Connected(Phase2ConnectResponse),
    Accepted,
    Deleted,
    Exists(bool),
    Status(StatusEvent),
    Logs(Vec<LogEvent>),
    HighWater(u64),
    Actions(ActionList),
    ProbeCatalog(Vec<ProbeCatalogItem>),
    ProbeWatch(ProbeWatchView),
    ProbeCoarse(Vec<CoarseBatch>),
    ProbeCsvStart {
        request_id: u64,
        catalog: Vec<ProbePointName>,
    },
    ProbeCsvPoll(crate::probe_log::CsvPoll),
    ProbeSamples(ProbeSamplesResponse),
    Topology(TopologyView),
    ViewerToken(String),
    Inspect(InspectReply),
    Err(ApiError),
}

struct World {
    sims: HashMap<String, Simulation>,
    running_boards: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// `BackendConfig.max_boards`: upper bound on boards running on this backend.
    max_boards: u32,
}

type IoxNode = Node<ipc::Service>;

struct Simulation {
    id: String,
    topology: TopologyView,
    child: Option<Child>,
    host: ServerHostPort,
    state: String,
    virtual_time_ns: u64,
    stop_reason: Option<String>,
    nodes: Vec<SessionNode>,
    logs: VecDeque<StoredLog>,
    next_seq: u64,
    boards: HashMap<u64, String>,
    /// Boards this backend's arbiter runs (all topology boards unless phase 1 split them).
    local_board_count: u32,
    /// Peer sockets advertised in phase 1; `None` for a single-backend simulation.
    local_endpoints: Option<ArbiterEndpoints>,
    /// Phase-2 payload (peers and multicast group), also handed to the arbiter via `phase2_path`.
    peer_link: Option<Phase2ConnectRequest>,
    /// File the arbiter watches (`--phase2-in`); `Some` only for a multi-backend simulation.
    phase2_path: Option<PathBuf>,
    inbox: ServerLogInbox,
    actions: ServerActions,
    probe_inbox: ServerProbeInbox,
    probe_ctrl: ServerProbeCtrl,
    probe_catalog: HashMap<u64, ProbePointName>,
    catalog_ready: bool,
    action_catalog_ready: bool,
    watch: ProbeWatch,
    catalog: Vec<crate::contract::AdvertisedAction>,
    next_request_id: u64,
    viewer: Option<String>,
    csv_job: Option<CsvJob>,
    iox_root: PathBuf,
    _iox: IoxNode,
    /// Declared last so it is removed after every iceoryx handle above is closed.
    _temp: TempPaths,
}

struct CsvJob {
    request_id: u64,
    expected: HashSet<u64>,
    pending: Vec<ProbeRecord>,
    truncated: bool,
    failed: bool,
    done: bool,
    deadline: Instant,
}

struct ProbeWatch {
    probes: Vec<ProbeRef>,
    probe_ids: Vec<u64>,
    next_seq: u64,
    pending: VecDeque<CoarseBatch>,
}

impl ProbeWatch {
    fn view(&self) -> ProbeWatchView {
        ProbeWatchView {
            probes: self.probes.clone(),
        }
    }

    fn push_batch(&mut self, batch: CoarseBatch) {
        self.pending.push_back(batch);
        while self.pending.len() > WATCH_PENDING_KEEP {
            self.pending.pop_front();
        }
    }
}

/// Start the worker thread and return a [`LocalBackend`] to it.
///
/// `max_boards` is `BackendConfig.max_boards`; a create that would push
/// `running_boards` past it is rejected with HTTP 409 before any arbiter is spawned.
pub fn spawn_worker(
    opts: ServerOptions,
    running_boards: std::sync::Arc<std::sync::atomic::AtomicU32>,
    max_boards: u32,
) -> Result<std::sync::Arc<LocalBackend>, ServerError> {
    if !opts.arbiter_bin.is_file() {
        return Err(ServerError::Message(format!(
            "cluster-arbiter not found at {} (build the arbiter binary first)",
            opts.arbiter_bin.display()
        )));
    }
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || worker_loop(rx, opts, running_boards, max_boards));
    Ok(LocalBackend::new(tx))
}

/// Accept internal HTTP until the process is killed.
///
/// Arbiter processes come and go; this function does not return when one exits.
pub fn run_backend(
    listen: &str,
    opts: ServerOptions,
    info: BackendInfo,
) -> Result<(), ServerError> {
    let running_boards = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let backend = spawn_worker(opts, running_boards.clone(), info.max_boards)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(crate::backend::serve_internal(
        listen,
        backend,
        info,
        running_boards,
    ))?;
    Ok(())
}

fn worker_loop(
    rx: Receiver<Job>,
    opts: ServerOptions,
    running_boards: std::sync::Arc<std::sync::atomic::AtomicU32>,
    max_boards: u32,
) {
    let mut world = World {
        sims: HashMap::new(),
        running_boards,
        max_boards,
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(job) => dispatch_job(&mut world, job, &opts),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        for sim in world.sims.values_mut() {
            poll_simulation(sim);
        }
    }
}

fn dispatch_job(world: &mut World, job: Job, opts: &ServerOptions) {
    let reply = match job.kind {
        JobKind::Create(topo) => match prepare_create_topology(topo) {
            Err(err) => JobReply::Err(err),
            Ok(topo) => match check_board_capacity(world, topo.boards.len()) {
                Err(err) => JobReply::Err(err),
                Ok(()) => match create_simulation(topo, opts, None, new_cluster_key()) {
                    Ok(sim) => JobReply::Created {
                        id: register_simulation(world, sim),
                    },
                    Err(err) => JobReply::Err(ApiError::internal(err.to_string())),
                },
            },
        },
        JobKind::CreatePhase1(req) => match create_phase1(world, req, opts) {
            Ok(sim) => {
                let endpoints = sim.local_endpoints.clone();
                JobReply::CreatedPhase1(Phase1CreateResponse {
                    id: register_simulation(world, sim),
                    endpoints,
                })
            }
            Err(err) => JobReply::Err(err),
        },
        JobKind::ConnectPhase2(req) => match world.sims.get_mut(&req.id) {
            Some(sim) => match connect_phase2(sim, req) {
                Ok(()) => JobReply::Phase2Connected(Phase2ConnectResponse { ok: true }),
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::Command { id, cmd } => match world.sims.get_mut(&id) {
            Some(sim) => match command_simulation(sim, cmd) {
                Ok(()) => JobReply::Accepted,
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::Delete { id } => match world.sims.remove(&id) {
            Some(mut sim) => {
                let boards = sim.local_board_count;
                world
                    .running_boards
                    .fetch_sub(boards, std::sync::atomic::Ordering::Relaxed);
                // Reply only after the arbiter, its nodes and the temp files
                // are gone, so a finished DELETE leaves nothing behind.
                sim.terminate();
                drop(sim);
                JobReply::Deleted
            }
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::Exists { id } => JobReply::Exists(world.sims.contains_key(&id)),
        JobKind::GetStatus { id } => match world.sims.get(&id) {
            Some(sim) => JobReply::Status(status_event(sim)),
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::GetTopology { id } => match world.sims.get(&id) {
            Some(sim) => JobReply::Topology(sim.topology.clone()),
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::GetLogs { id, after } => match world.sims.get(&id) {
            Some(sim) => {
                let logs = sim
                    .logs
                    .iter()
                    .filter(|log| log.seq > after)
                    .map(StoredLog::to_event)
                    .collect();
                JobReply::Logs(logs)
            }
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::LogHighWater { id } => match world.sims.get(&id) {
            Some(sim) => JobReply::HighWater(sim.next_seq.saturating_sub(1)),
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ListActions { id } => match world.sims.get(&id) {
            Some(sim) => JobReply::Actions(ActionList {
                actions: sim.catalog.clone(),
            }),
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::Invoke { id, request } => match world.sims.get_mut(&id) {
            Some(sim) => match queue_invoke(sim, &request) {
                Ok(()) => JobReply::Accepted,
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeCatalog { id } => match world.sims.get(&id) {
            Some(sim) => match probe_catalog_items(sim) {
                Ok(items) => JobReply::ProbeCatalog(items),
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeWatchGet { id } => match world.sims.get(&id) {
            Some(sim) => JobReply::ProbeWatch(sim.watch.view()),
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeWatchPut { id, probes } => match world.sims.get_mut(&id) {
            Some(sim) => match put_watch(sim, probes) {
                Ok(view) => JobReply::ProbeWatch(view),
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeWatchClear { id } => match world.sims.get_mut(&id) {
            Some(sim) => match clear_watch(sim) {
                Ok(()) => JobReply::Accepted,
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeWatchLive { id, after } => match world.sims.get_mut(&id) {
            Some(sim) => JobReply::ProbeCoarse(live_watch(sim, after)),
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeCsvStart { id, range } => match world.sims.get_mut(&id) {
            Some(sim) => match start_csv(sim, range) {
                Ok((request_id, catalog)) => JobReply::ProbeCsvStart {
                    request_id,
                    catalog,
                },
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeCsvPoll { id, request_id } => match world.sims.get_mut(&id) {
            Some(sim) => match poll_csv_job(sim, request_id) {
                Ok(poll) => JobReply::ProbeCsvPoll(poll),
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeCsvStatus { id, request_id } => match world.sims.get(&id) {
            Some(sim) => match csv_status(sim, request_id) {
                Ok(poll) => JobReply::ProbeCsvPoll(poll),
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ProbeSamples {
            id,
            board,
            probe,
            from_ns,
            to_ns,
        } => match world.sims.get_mut(&id) {
            Some(sim) => match query_samples(sim, &board, &probe, from_ns, to_ns) {
                Ok(body) => JobReply::ProbeSamples(body),
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ViewerAttach { id } => match world.sims.get_mut(&id) {
            Some(sim) => {
                if sim.viewer.is_some() {
                    JobReply::Err(ApiError::viewer_busy())
                } else {
                    let token = format!("{:016x}", next_probe_request(sim));
                    sim.viewer = Some(token.clone());
                    JobReply::ViewerToken(token)
                }
            }
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ViewerCheck { id, token } => match world.sims.get(&id) {
            Some(sim) => {
                if sim.viewer.as_deref() == Some(token.as_str()) {
                    JobReply::Accepted
                } else {
                    JobReply::Err(ApiError::viewer_busy())
                }
            }
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::ViewerDetach { id, token } => match world.sims.get_mut(&id) {
            Some(sim) => {
                if sim.viewer.as_deref() == Some(token.as_str()) {
                    sim.viewer = None;
                }
                JobReply::Accepted
            }
            None => JobReply::Accepted,
        },
        JobKind::Inspect { id, board_id, req } => match world.sims.get(&id) {
            Some(sim) => {
                if !sim.nodes.iter().any(|node| node.id == board_id) {
                    JobReply::Err(ApiError::not_found("unknown board"))
                } else {
                    let path = inspect_sock_path(&sim.iox_root, &board_id);
                    match inspect_rpc(&path, &req) {
                        Ok(reply) => JobReply::Inspect(reply),
                        Err(err) => JobReply::Err(ApiError::internal(err)),
                    }
                }
            }
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::Step { id, board_id } => match world.sims.get_mut(&id) {
            Some(sim) => {
                if sim.state == "exited" {
                    JobReply::Err(ApiError::conflict("arbiter has exited"))
                } else if !sim.nodes.iter().any(|node| node.id == board_id) {
                    JobReply::Err(ApiError::not_found("unknown board"))
                } else {
                    match send_cmd(
                        sim,
                        HostCommand::Step {
                            board_hash: board_id_hash(&board_id),
                        },
                    ) {
                        Ok(()) => JobReply::Accepted,
                        Err(err) => JobReply::Err(ApiError::internal(err)),
                    }
                }
            }
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
    };
    let _ = job.reply.send(reply);
}

/// Reject a create whose `new_boards` would push `running_boards` past `max_boards`.
///
/// Runs before any arbiter is spawned. `running_boards` is only written from the
/// worker thread (see [`register_simulation`] and `JobKind::Delete`), so the
/// check and the later registration cannot interleave with another create.
fn check_board_capacity(world: &World, new_boards: usize) -> Result<(), ApiError> {
    let running = u64::from(
        world
            .running_boards
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let requested = new_boards as u64;
    if running + requested > u64::from(world.max_boards) {
        return Err(ApiError::conflict(format!(
            "backend capacity exceeded: {requested} board(s) requested, {running} running, max_boards is {}",
            world.max_boards
        )));
    }
    Ok(())
}

/// Insert a freshly created simulation and count its local boards as running.
fn register_simulation(world: &mut World, sim: Simulation) -> String {
    let id = sim.id.clone();
    world
        .running_boards
        .fetch_add(sim.local_board_count, std::sync::atomic::Ordering::Relaxed);
    world.sims.insert(id.clone(), sim);
    id
}

/// How a phase-1 create splits the topology on this backend.
struct Placement {
    local_board_ids: Vec<String>,
    /// `Some` only when other backends participate.
    peer_advertise: Option<PeerAdvertise>,
    /// Full board → backend placement; the arbiter derives its cross-machine edges from it.
    assignment: AssignmentTable,
}

/// Validate a phase-1 request and spawn the local arbiter.
fn create_phase1(
    world: &World,
    req: Phase1CreateRequest,
    opts: &ServerOptions,
) -> Result<Simulation, ApiError> {
    // The frontend rolls back by this id even when it never sees the reply, so
    // the simulation must be created under exactly it, and never over another.
    validate_simulation_id(&req.id)?;
    if world.sims.contains_key(&req.id) {
        return Err(ApiError::conflict(format!(
            "simulation id `{}` already exists on this backend",
            req.id
        )));
    }
    let topo = prepare_create_topology(req.topology)?;
    validate_assignment(&topo, &req.assignment)?;
    let local_board_ids: Vec<String> = req
        .assignment
        .boards_for(&req.local_backend)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if local_board_ids.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_assignment",
            format!("no boards assigned to backend {}", req.local_backend),
        ));
    }
    // `--local-boards` is a comma-separated list.
    if let Some(bad) = local_board_ids.iter().find(|id| id.contains(',')) {
        return Err(ApiError::bad_request(
            "invalid_assignment",
            format!("board id `{bad}` must not contain a comma"),
        ));
    }
    check_board_capacity(world, local_board_ids.len())?;
    let multi = req.assignment.participating_backends().len() > 1;
    let placement = Placement {
        local_board_ids,
        peer_advertise: multi.then(|| PeerAdvertise {
            advertise_ip: opts.advertise_ip.clone(),
            backend_key: req.local_backend.clone(),
        }),
        assignment: req.assignment,
    };
    create_simulation(topo, opts, Some(placement), req.id)
        .map_err(|err| ApiError::internal(err.to_string()))
}

/// Record the phase-2 peer list and multicast group.
fn connect_phase2(sim: &mut Simulation, req: Phase2ConnectRequest) -> Result<(), ApiError> {
    if sim.peer_link.is_some() {
        return Err(ApiError::conflict("phase 2 was already applied"));
    }
    check_phase2(sim.local_endpoints.as_ref(), &req)?;
    if sim.local_endpoints.is_none() {
        // Single backend: nothing to store.
        return Ok(());
    }
    if let Some(path) = &sim.phase2_path {
        write_phase2_file(path, &req)
            .map_err(|err| ApiError::internal(format!("hand phase 2 to arbiter: {err}")))?;
    }
    eprintln!(
        "cluster-backend {}: phase 2 accepted ({} peer(s), multicast {}:{})",
        sim.id,
        req.peers.len(),
        req.multicast.ip,
        req.multicast.port
    );
    sim.peer_link = Some(req);
    Ok(())
}

/// Write via a sibling temp file and rename so the arbiter never reads partial JSON.
fn write_phase2_file(path: &std::path::Path, req: &Phase2ConnectRequest) -> std::io::Result<()> {
    let json = serde_json::to_vec(req).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)
}

/// Validate a phase-2 request against this simulation's own advertised endpoints
/// (`None` for a single-backend simulation, which accepts only an empty peer list).
fn check_phase2(
    local: Option<&ArbiterEndpoints>,
    req: &Phase2ConnectRequest,
) -> Result<(), ApiError> {
    let Some(local) = local else {
        return if req.peers.is_empty() {
            Ok(())
        } else {
            Err(ApiError::conflict(
                "simulation was created without peer sockets (single backend)",
            ))
        };
    };
    if req.peers.is_empty() {
        return Err(ApiError::bad_request(
            "bad_param",
            "phase 2 needs at least one peer for a multi-backend simulation",
        ));
    }
    // Stamped on every time-sync packet; the wire format holds at most 255 bytes.
    if req.cluster_key.is_empty() || req.cluster_key.len() > usize::from(u8::MAX) {
        return Err(ApiError::bad_request(
            "bad_param",
            "phase 2 needs a cluster_key of 1..=255 bytes",
        ));
    }
    let group_ok = req
        .multicast
        .ip
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_multicast());
    if !group_ok || req.multicast.port == 0 {
        return Err(ApiError::bad_request(
            "bad_param",
            format!(
                "invalid multicast group {}:{}",
                req.multicast.ip, req.multicast.port
            ),
        ));
    }
    let mut seen = HashSet::new();
    for peer in &req.peers {
        if peer.backend == local.backend || !seen.insert(peer.backend.as_str()) {
            return Err(ApiError::bad_request(
                "bad_param",
                format!("peer list repeats or includes backend {}", peer.backend),
            ));
        }
        if peer.control_tcp_port == 0 || peer.data_udp_port == 0 {
            return Err(ApiError::bad_request(
                "bad_param",
                format!("peer {} has a zero port", peer.backend),
            ));
        }
    }
    Ok(())
}

fn create_simulation(
    topo: LogicalTopology,
    opts: &ServerOptions,
    placement: Option<Placement>,
    id: String,
) -> Result<Simulation, ServerError> {
    let iox_root = root_path_for_cluster(&id);
    // Declared before every iceoryx handle so that on any early return it is
    // dropped after them and removes what this function created.
    let mut temp = TempPaths::default();
    temp.push(iox_root.clone());
    let config = isolated_config(&iox_root)?;
    let iox = create_node(&config, &format!("server-{id}"))?;
    let inbox = ServerLogInbox::create(&iox, &id)?;
    let host = ServerHostPort::create(&iox, &id)?;
    let actions = ServerActions::create(&iox, &id, topo.boards.len())?;
    let probe_inbox = ServerProbeInbox::create(&iox, &id)?;
    let probe_ctrl = ServerProbeCtrl::create(&iox, &id, topo.boards.len())?;
    let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str()))
        .map_err(ServerError::Message)?;
    let topology = TopologyView::from_logical(&topo);
    let topo_path = write_temp_topology(&topo)?;
    temp.push(topo_path.clone());
    let endpoints_path = placement
        .as_ref()
        .is_some_and(|p| p.peer_advertise.is_some())
        .then(|| temp_path("sim-cluster-endpoints", "json"));
    if let Some(path) = &endpoints_path {
        temp.push(path.clone());
    }
    let phase2_path = endpoints_path
        .is_some()
        .then(|| temp_path("sim-cluster-phase2", "json"));
    if let Some(path) = &phase2_path {
        temp.push(path.clone());
        temp.push(path.with_extension("tmp"));
    }
    // Multi-backend only: the arbiter needs the full placement to tell
    // cross-machine edges from same-machine ones.
    let assignment_path = match (&placement, &endpoints_path) {
        (Some(p), Some(_)) => {
            let path = temp_path("sim-cluster-assignment", "json");
            temp.push(path.clone());
            let json = serde_json::to_vec(&p.assignment)
                .map_err(|err| ServerError::Message(format!("encode assignment: {err}")))?;
            fs::write(&path, json)?;
            Some(path)
        }
        _ => None,
    };
    let mut cmd = Command::new(&opts.arbiter_bin);
    cmd.arg("--topology")
        .arg(&topo_path)
        .arg("--cluster-key")
        .arg(&id)
        .arg("--iox-root")
        .arg(&iox_root);
    if let Some(placement) = &placement {
        cmd.arg("--local-boards")
            .arg(placement.local_board_ids.join(","));
        if let (Some(peer), Some(path)) = (&placement.peer_advertise, &endpoints_path) {
            cmd.arg("--advertise-ip")
                .arg(&peer.advertise_ip)
                .arg("--backend-key")
                .arg(&peer.backend_key)
                .arg("--endpoints-out")
                .arg(path);
            if let Some(phase2) = &phase2_path {
                cmd.arg("--phase2-in")
                    .arg(phase2)
                    .arg("--time-sync-period-ms")
                    .arg(opts.time_sync_period_ms.to_string())
                    .arg("--data-udp-window")
                    .arg(opts.data_udp_window.to_string());
            }
            if let Some(assignment) = &assignment_path {
                cmd.arg("--assignment-in").arg(assignment);
            }
        }
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        // Own process group: killing the group also takes down the nodes the
        // arbiter spawned, which a plain kill of the arbiter would orphan.
        .process_group(0)
        .spawn()?;
    let local_endpoints = match &endpoints_path {
        None => None,
        Some(path) => {
            let result = await_endpoints(&mut child, path);
            let _ = fs::remove_file(path);
            match result {
                Ok(endpoints) => Some(endpoints),
                Err(err) => {
                    kill_process_tree(&mut child);
                    return Err(err);
                }
            }
        }
    };
    let local_ids: Vec<&str> = match &placement {
        Some(p) => p.local_board_ids.iter().map(String::as_str).collect(),
        None => topo.boards.iter().map(|b| b.id.as_str()).collect(),
    };
    let nodes = topo
        .boards
        .iter()
        .filter(|board| local_ids.contains(&board.id.as_str()))
        .map(|board| SessionNode {
            id: board.id.clone(),
            kind: board.kind.clone(),
            state: "starting".into(),
        })
        .collect::<Vec<_>>();
    let local_board_count = nodes.len() as u32;
    Ok(Simulation {
        id,
        topology,
        child: Some(child),
        host,
        state: "stopped".into(),
        virtual_time_ns: 0,
        stop_reason: None,
        nodes,
        logs: VecDeque::new(),
        next_seq: 1,
        boards,
        local_board_count,
        local_endpoints,
        peer_link: None,
        phase2_path,
        inbox,
        actions,
        probe_inbox,
        probe_ctrl,
        probe_catalog: HashMap::new(),
        catalog_ready: false,
        action_catalog_ready: false,
        watch: ProbeWatch {
            probes: Vec::new(),
            probe_ids: Vec::new(),
            next_seq: 1,
            pending: VecDeque::new(),
        },
        catalog: Vec::new(),
        next_request_id: 1,
        viewer: None,
        csv_job: None,
        iox_root,
        _iox: iox,
        _temp: temp,
    })
}

/// Wait for the arbiter to write its bound peer endpoints (see `--endpoints-out`).
fn await_endpoints(
    child: &mut Child,
    path: &std::path::Path,
) -> Result<ArbiterEndpoints, ServerError> {
    let deadline = Instant::now() + ENDPOINTS_WAIT;
    loop {
        match fs::read(path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes).map_err(|err| {
                    ServerError::Message(format!("invalid arbiter endpoints file: {err}"))
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(ServerError::Io(err)),
        }
        if let Some(status) = child.try_wait()? {
            return Err(ServerError::Message(format!(
                "cluster-arbiter exited ({status}) before advertising peer endpoints"
            )));
        }
        if Instant::now() >= deadline {
            return Err(ServerError::Message(format!(
                "cluster-arbiter did not advertise peer endpoints within {}s",
                ENDPOINTS_WAIT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn command_simulation(sim: &mut Simulation, cmd: HostCommand) -> Result<(), ApiError> {
    if sim.state == "exited" {
        return Err(ApiError::conflict("arbiter has exited"));
    }
    send_cmd(sim, cmd).map_err(ApiError::internal)
}

fn send_cmd(sim: &mut Simulation, cmd: HostCommand) -> Result<(), String> {
    sim.host.publish_cmd(cmd).map_err(|err| err.to_string())
}

fn status_event(sim: &Simulation) -> StatusEvent {
    StatusEvent {
        id: sim.id.clone(),
        state: sim.state.clone(),
        virtual_time_ns: sim.virtual_time_ns,
        stop_reason: sim.stop_reason.clone(),
        nodes: sim.nodes.clone(),
    }
}

fn queue_invoke(sim: &mut Simulation, request: &InvokeActionRequest) -> Result<(), ApiError> {
    if sim.state == "exited" {
        return Err(ApiError::conflict("arbiter has exited"));
    }
    let board_hash = sim_kernel::id_hash(&request.board_id);
    if !sim.boards.contains_key(&board_hash) {
        return Err(ApiError::not_found("unknown board"));
    }
    if !sim.action_catalog_ready {
        return Err(ApiError::conflict("action catalog is not ready"));
    }
    if let Some(err) = catalog_invoke_error(&sim.catalog, request) {
        return Err(err);
    }
    let raw: Vec<(u64, f64)> = request
        .params
        .iter()
        .map(|(name, value)| (sim_kernel::id_hash(name), *value))
        .collect();
    let request_id = sim.next_request_id;
    sim.next_request_id = sim.next_request_id.saturating_add(1);
    let sample = ActionRequest::new(
        request_id,
        board_hash,
        sim_kernel::id_hash(&request.device),
        sim_kernel::id_hash(&request.action),
        &raw,
    )
    .map_err(|_| ApiError::bad_request("bad_param", "invalid action parameter"))?;
    sim.actions
        .publish_request(&sample)
        .map_err(|err| ApiError::internal(err.to_string()))?;
    Ok(())
}

fn catalog_invoke_error(
    catalog: &[AdvertisedAction],
    request: &InvokeActionRequest,
) -> Option<ApiError> {
    let on_board = catalog
        .iter()
        .filter(|row| row.board_id == request.board_id)
        .collect::<Vec<_>>();
    if on_board
        .iter()
        .any(|row| row.device == request.device && row.action == request.action)
    {
        return None;
    }
    if on_board.iter().any(|row| row.device == request.device) {
        Some(ApiError::not_found("unknown action"))
    } else {
        Some(ApiError::not_found("unknown device"))
    }
}

fn poll_actions(sim: &mut Simulation) {
    loop {
        match sim.actions.try_upstream() {
            Ok(Some(ActionUpstream::Advertise(sample))) => {
                let Some(board_id) = sim.boards.get(&sample.from).cloned() else {
                    continue;
                };
                match AdvertisedAction::new(&board_id, &sample) {
                    Ok(body) => upsert_catalog(sim, body),
                    Err(err) => {
                        eprintln!("cluster-backend {}: action advertise: {err}", sim.id);
                    }
                }
            }
            Ok(Some(ActionUpstream::CatalogEnd)) => {
                sim.action_catalog_ready = true;
            }
            Ok(Some(ActionUpstream::Result(_))) => {}
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-backend {}: action inbox: {err}", sim.id);
                break;
            }
        }
    }
}

fn upsert_catalog(sim: &mut Simulation, body: crate::contract::AdvertisedAction) {
    if let Some(existing) = sim.catalog.iter_mut().find(|row| {
        row.board_id == body.board_id && row.device == body.device && row.action == body.action
    }) {
        *existing = body;
    } else {
        sim.catalog.push(body);
    }
}

fn poll_probes(sim: &mut Simulation) {
    let mut live = Vec::new();
    for _ in 0..1024 {
        match sim.probe_inbox.try_recv_catalog() {
            Ok(Some(sample)) => {
                let _ = ingest_probe(sim, &sample);
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-backend {}: probe catalog: {err}", sim.id);
                break;
            }
        }
    }
    for _ in 0..1024 {
        match sim.probe_inbox.try_recv_sample() {
            Ok(Some(sample)) => {
                if let Some(record) = ingest_probe(sim, &sample) {
                    live.push(record);
                }
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-backend {}: probe inbox: {err}", sim.id);
                break;
            }
        }
    }
    if !live.is_empty() {
        fanout_live(sim, &live);
    }
}

fn ingest_probe(sim: &mut Simulation, sample: &crate::ipc::ProbeToServer) -> Option<ProbeRecord> {
    if sample.msg == MSG_CATALOG_END {
        sim.catalog_ready = true;
        return None;
    }
    let kind = ProbeKind::from_wire(sample.wire_kind)?;
    match sample.msg {
        MSG_CATALOG => {
            let name_len = usize::from(sample.name_len).min(sample.name.len());
            let name = String::from_utf8_lossy(&sample.name[..name_len]).into_owned();
            let board_id = sim
                .boards
                .get(&sample.board_hash)
                .cloned()
                .unwrap_or_default();
            sim.probe_catalog.insert(
                sample.probe_id,
                ProbePointName {
                    probe_id: sample.probe_id,
                    board_id,
                    name,
                    kind,
                },
            );
            None
        }
        MSG_SAMPLE => Some(ProbeRecord {
            probe_id: sample.probe_id,
            virtual_time_ns: sample.virtual_time_ns,
            kind,
            value: sample.value,
        }),
        _ => None,
    }
}

fn require_catalog(sim: &Simulation) -> Result<(), ApiError> {
    if sim.catalog_ready {
        Ok(())
    } else {
        Err(ApiError::new(
            503,
            "not_ready",
            "probe catalog is not ready",
        ))
    }
}

fn probe_catalog_items(sim: &Simulation) -> Result<Vec<ProbeCatalogItem>, ApiError> {
    require_catalog(sim)?;
    let mut items: Vec<ProbeCatalogItem> = sim
        .probe_catalog
        .values()
        .map(|point| ProbeCatalogItem {
            board: point.board_id.clone(),
            probe: point.name.clone(),
            kind: point.kind.as_str().to_string(),
        })
        .collect();
    items.sort_by(|a, b| (&a.board, &a.probe).cmp(&(&b.board, &b.probe)));
    Ok(items)
}

fn resolve_probes(sim: &Simulation, probes: &[ProbeRef]) -> Result<Vec<u64>, ApiError> {
    require_catalog(sim)?;
    let mut ids = Vec::with_capacity(probes.len());
    for probe in probes {
        let Some((id, _)) = sim
            .probe_catalog
            .iter()
            .find(|(_, point)| point.board_id == probe.board && point.name == probe.name)
        else {
            return Err(ApiError::not_found(format!(
                "unknown probe {}/{}",
                probe.board, probe.name
            )));
        };
        ids.push(*id);
    }
    Ok(ids)
}

fn put_watch(sim: &mut Simulation, probes: Vec<ProbeRef>) -> Result<ProbeWatchView, ApiError> {
    let ids = resolve_probes(sim, &probes)?;
    sim.watch.probes = probes;
    sim.watch.probe_ids = ids;
    let view = sim.watch.view();
    refresh_watch(sim)?;
    let snapshot = snapshot_watch(sim)?;
    if !snapshot.samples.is_empty() {
        sim.watch.push_batch(snapshot);
    }
    Ok(view)
}

fn clear_watch(sim: &mut Simulation) -> Result<(), ApiError> {
    sim.watch.probes.clear();
    sim.watch.probe_ids.clear();
    sim.watch.pending.clear();
    refresh_watch(sim)
}

fn live_watch(sim: &mut Simulation, after: u64) -> Vec<CoarseBatch> {
    sim.watch.pending.retain(|batch| batch.seq > after);
    sim.watch.pending.drain(..).collect()
}

fn fanout_live(sim: &mut Simulation, records: &[ProbeRecord]) {
    let samples: Vec<ProbeView> = records
        .iter()
        .filter(|record| sim.watch.probe_ids.contains(&record.probe_id))
        .map(|record| record_view(&sim.probe_catalog, record))
        .collect();
    if samples.is_empty() {
        return;
    }
    let seq = sim.watch.next_seq;
    sim.watch.next_seq = sim.watch.next_seq.saturating_add(1);
    sim.watch.push_batch(CoarseBatch { seq, samples });
}

fn snapshot_watch(sim: &mut Simulation) -> Result<CoarseBatch, ApiError> {
    let probe_ids = sim.watch.probe_ids.clone();
    let seq = sim.watch.next_seq;
    sim.watch.next_seq = sim.watch.next_seq.saturating_add(1);
    if probe_ids.is_empty() {
        return Ok(CoarseBatch {
            seq,
            samples: Vec::new(),
        });
    }
    let (records, _) = query_latest(sim, &probe_ids)?;
    Ok(CoarseBatch {
        seq,
        samples: records
            .iter()
            .map(|rec| record_view(&sim.probe_catalog, rec))
            .collect(),
    })
}

fn refresh_watch(sim: &mut Simulation) -> Result<(), ApiError> {
    let mut by_board: HashMap<u64, HashSet<u64>> = sim
        .boards
        .keys()
        .copied()
        .map(|hash| (hash, HashSet::new()))
        .collect();
    for id in &sim.watch.probe_ids {
        if let Some(point) = sim.probe_catalog.get(id) {
            let board_hash = crate::ipc::board_id_hash(&point.board_id);
            by_board.entry(board_hash).or_default().insert(*id);
        }
    }
    for (board_hash, ids) in by_board {
        let ids: Vec<u64> = ids.into_iter().collect();
        sim.probe_ctrl
            .publish_watch(ProbeWatchWire::new(board_hash, &ids))
            .map_err(|err| ApiError::internal(err.to_string()))?;
    }
    Ok(())
}

fn query_latest(
    sim: &mut Simulation,
    probe_ids: &[u64],
) -> Result<(Vec<ProbeRecord>, bool), ApiError> {
    let mut by_board: HashMap<u64, Vec<u64>> = HashMap::new();
    for id in probe_ids {
        if let Some(point) = sim.probe_catalog.get(id) {
            by_board
                .entry(crate::ipc::board_id_hash(&point.board_id))
                .or_default()
                .push(*id);
        }
    }
    let expected: HashSet<u64> = by_board.keys().copied().collect();
    if expected.is_empty() {
        return Ok((Vec::new(), false));
    }
    let request_id = next_probe_request(sim);
    for (board_hash, ids) in by_board {
        sim.probe_ctrl
            .publish_query(ProbeQueryWire::latest(request_id, board_hash, &ids))
            .map_err(|err| ApiError::internal(err.to_string()))?;
    }
    collect_query(sim, request_id, expected, Duration::from_secs(5))
}

fn query_range(
    sim: &mut Simulation,
    probe_ids: &[u64],
    range: ProbeRange,
) -> Result<(Vec<ProbeRecord>, bool), ApiError> {
    let mut by_board: HashMap<u64, Vec<u64>> = HashMap::new();
    for id in probe_ids {
        if let Some(point) = sim.probe_catalog.get(id) {
            by_board
                .entry(crate::ipc::board_id_hash(&point.board_id))
                .or_default()
                .push(*id);
        }
    }
    let expected: HashSet<u64> = by_board.keys().copied().collect();
    if expected.is_empty() {
        return Ok((Vec::new(), false));
    }
    let request_id = next_probe_request(sim);
    for (board_hash, ids) in by_board {
        sim.probe_ctrl
            .publish_query(ProbeQueryWire::range(
                request_id,
                board_hash,
                &ids,
                range.from_ns,
                range.to_ns,
            ))
            .map_err(|err| ApiError::internal(err.to_string()))?;
    }
    collect_query(sim, request_id, expected, Duration::from_secs(30))
}

fn query_samples(
    sim: &mut Simulation,
    board: &str,
    probe: &str,
    from_ns: Option<u64>,
    to_ns: Option<u64>,
) -> Result<ProbeSamplesResponse, ApiError> {
    require_catalog(sim)?;
    if board.is_empty() || probe.is_empty() {
        return Err(ApiError::bad_request(
            "bad_param",
            "board and probe are required",
        ));
    }
    let ids = resolve_probes(
        sim,
        &[ProbeRef {
            board: board.to_string(),
            name: probe.to_string(),
        }],
    )?;
    let (records, truncated) = if from_ns.is_none() && to_ns.is_none() {
        query_latest(sim, &ids)?
    } else {
        let range = ProbeRange { from_ns, to_ns };
        query_range(sim, &ids, range)?
    };
    Ok(ProbeSamplesResponse {
        truncated,
        samples: records
            .iter()
            .map(|record| json_view(&sim.probe_catalog, record))
            .collect(),
    })
}

fn start_csv(
    sim: &mut Simulation,
    range: ProbeRange,
) -> Result<(u64, Vec<ProbePointName>), ApiError> {
    require_catalog(sim)?;
    if sim
        .csv_job
        .as_ref()
        .is_some_and(|job| !job.done && Instant::now() < job.deadline)
    {
        return Err(ApiError::conflict("csv export is busy"));
    }
    let mut by_board: HashMap<u64, Vec<u64>> = HashMap::new();
    for point in sim.probe_catalog.values() {
        by_board
            .entry(crate::ipc::board_id_hash(&point.board_id))
            .or_default()
            .push(point.probe_id);
    }
    let expected: HashSet<u64> = by_board.keys().copied().collect();
    let done = expected.is_empty();
    let request_id = next_probe_request(sim);
    for (board_hash, ids) in by_board {
        sim.probe_ctrl
            .publish_csv(ProbeQueryWire::csv(
                request_id,
                board_hash,
                &ids,
                range.from_ns,
                range.to_ns,
            ))
            .map_err(|err| ApiError::internal(err.to_string()))?;
    }
    sim.csv_job = Some(CsvJob {
        request_id,
        expected,
        pending: Vec::new(),
        truncated: false,
        failed: false,
        done,
        deadline: Instant::now() + Duration::from_secs(60),
    });
    let catalog = sim.probe_catalog.values().cloned().collect();
    Ok((request_id, catalog))
}

fn ingest_csv_replies(sim: &mut Simulation) {
    let Some(job) = sim.csv_job.as_mut() else {
        return;
    };
    if job.done {
        return;
    }
    loop {
        match sim.probe_ctrl.try_csv_reply() {
            Ok(Some(reply)) if reply.request_id == job.request_id => {
                job.pending.extend(reply.records());
                job.truncated |= reply.is_truncated();
                if reply.is_last() {
                    job.expected.remove(&reply.from);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-backend {}: csv reply: {err}", sim.id);
                job.failed = true;
                job.done = true;
                return;
            }
        }
    }
    if job.expected.is_empty() {
        job.done = true;
    } else if Instant::now() >= job.deadline {
        job.failed = true;
        job.done = true;
    }
}

fn poll_csv_job(
    sim: &mut Simulation,
    request_id: u64,
) -> Result<crate::probe_log::CsvPoll, ApiError> {
    let Some(job) = sim.csv_job.as_mut() else {
        return Err(ApiError::not_found("unknown csv export"));
    };
    if job.request_id != request_id {
        return Err(ApiError::not_found("unknown csv export"));
    }
    Ok(crate::probe_log::CsvPoll {
        records: std::mem::take(&mut job.pending),
        done: job.done,
        failed: job.failed,
        truncated: job.truncated,
    })
}

fn csv_status(sim: &Simulation, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
    let Some(job) = sim.csv_job.as_ref() else {
        return Err(ApiError::not_found("unknown csv export"));
    };
    if job.request_id != request_id {
        return Err(ApiError::not_found("unknown csv export"));
    }
    Ok(crate::probe_log::CsvPoll {
        records: Vec::new(),
        done: job.done,
        failed: job.failed,
        truncated: job.truncated,
    })
}

fn next_probe_request(sim: &mut Simulation) -> u64 {
    let id = sim.next_request_id;
    sim.next_request_id = sim.next_request_id.saturating_add(1);
    id
}

fn collect_query(
    sim: &mut Simulation,
    request_id: u64,
    mut expected: HashSet<u64>,
    timeout: Duration,
) -> Result<(Vec<ProbeRecord>, bool), ApiError> {
    let deadline = Instant::now() + timeout;
    let mut records = Vec::new();
    let mut truncated = false;
    while !expected.is_empty() && Instant::now() < deadline {
        match sim.probe_ctrl.try_reply() {
            Ok(Some(reply)) if reply.request_id == request_id => {
                records.extend(reply.records());
                truncated |= reply.is_truncated();
                if reply.is_last() {
                    expected.remove(&reply.from);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(err) => return Err(ApiError::internal(err.to_string())),
        }
    }
    if !expected.is_empty() {
        return Err(ApiError::new(
            504,
            "timeout",
            "probe query was not answered before the deadline",
        ));
    }
    records.sort_by_key(|rec| (rec.virtual_time_ns, rec.probe_id));
    Ok((records, truncated))
}

fn poll_simulation(sim: &mut Simulation) {
    loop {
        match sim.host.try_status() {
            Ok(Some(status)) => {
                sim.state = status.state;
                sim.virtual_time_ns = status.virtual_time_ns;
                sim.stop_reason = status.stop_reason;
                sim.nodes = status.nodes;
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-backend {}: host status: {err}", sim.id);
                break;
            }
        }
    }
    loop {
        match sim.inbox.try_recv() {
            Ok(Some(record)) => push_log(sim, &record),
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-backend {}: log inbox: {err}", sim.id);
                break;
            }
        }
    }
    poll_actions(sim);
    poll_probes(sim);
    ingest_csv_replies(sim);
    if let Some(child) = sim.child.as_mut() {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                sim.state = "exited".into();
                let virtual_time_ns = sim.virtual_time_ns;
                append_log(
                    sim,
                    "info",
                    "server",
                    None,
                    &format!("arbiter exited ({code})"),
                    virtual_time_ns,
                );
                sim.child = None;
            }
            Ok(None) => {}
            Err(err) => eprintln!("cluster-backend {}: wait arbiter: {err}", sim.id),
        }
    }
}

fn push_log(sim: &mut Simulation, record: &ClusterLog) {
    let board = if record.origin() == LogOrigin::Node {
        sim.boards.get(&record.board_hash()).cloned()
    } else {
        None
    };
    let who = board.as_deref().unwrap_or(record.origin().as_str());
    println!(
        "[{}] [{}] {who}: {}",
        sim.id,
        record.level().as_str(),
        record.text()
    );
    append_log(
        sim,
        record.level().as_str(),
        record.origin().as_str(),
        board,
        record.text(),
        record.virtual_time_ns(),
    );
}

fn append_log(
    sim: &mut Simulation,
    level: &str,
    origin: &str,
    board: Option<String>,
    text: &str,
    virtual_time_ns: u64,
) {
    let seq = sim.next_seq;
    sim.next_seq = sim.next_seq.saturating_add(1);
    sim.logs.push_back(StoredLog {
        seq,
        level: level.to_string(),
        origin: origin.to_string(),
        board,
        text: text.to_string(),
        virtual_time_ns,
    });
    while sim.logs.len() > LOG_CAP {
        sim.logs.pop_front();
    }
}

fn write_temp_topology(topo: &LogicalTopology) -> Result<PathBuf, ServerError> {
    let path = temp_path("sim-cluster-topology", "json");
    fs::write(&path, topo.to_json_string()?)?;
    Ok(path)
}

fn temp_path(prefix: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}.{ext}", new_cluster_key()))
}

impl Simulation {
    /// Stop the arbiter and everything it started, then wait until it is reaped.
    ///
    /// Sends `Shutdown` so the arbiter can stop its nodes itself and waits up to
    /// [`SHUTDOWN_GRACE`]. If it does not exit in time (for example it is still
    /// waiting for nodes to become ready and does not read host commands), the
    /// whole process group is killed. Safe to call more than once.
    fn terminate(&mut self) {
        self.terminate_within(SHUTDOWN_GRACE);
    }

    fn terminate_within(&mut self, grace: Duration) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = send_cmd(self, HostCommand::Shutdown);
        if !wait_for_exit(&mut child, grace) {
            kill_process_tree(&mut child);
        }
    }
}

impl Drop for Simulation {
    fn drop(&mut self) {
        // Temp files and the iceoryx root are removed by `_temp` afterwards.
        self.terminate();
    }
}

/// Poll until `child` exits; `false` when `timeout` elapsed first.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // Cannot tell whether it runs; let the caller kill it.
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// SIGKILL the arbiter's process group (arbiter plus nodes), then reap the arbiter.
///
/// The arbiter is spawned with `process_group(0)`, so its pid is the group id.
/// The child has not been reaped yet, so the pid cannot have been reused.
fn kill_process_tree(child: &mut Child) {
    if let Ok(pgid) = i32::try_from(child.id()) {
        // SAFETY: `kill` has no memory-safety preconditions; a negative pid
        // addresses the process group.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Files and directories removed when dropped (best effort, in push order).
#[derive(Default)]
struct TempPaths(Vec<PathBuf>);

impl TempPaths {
    fn push(&mut self, path: PathBuf) {
        self.0.push(path);
    }
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        for path in &self.0 {
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrate::MulticastGroup;

    fn endpoints(backend: &str, tcp: u16, udp: u16) -> ArbiterEndpoints {
        ArbiterEndpoints {
            backend: backend.into(),
            advertise_ip: "127.0.0.1".into(),
            control_tcp_port: tcp,
            data_udp_port: udp,
        }
    }

    fn phase2(peers: Vec<ArbiterEndpoints>, ip: &str, port: u16) -> Phase2ConnectRequest {
        Phase2ConnectRequest {
            id: "sim".into(),
            cluster_key: "sim".into(),
            peers,
            multicast: MulticastGroup {
                ip: ip.into(),
                port,
            },
        }
    }

    fn capacity_world(running: u32, max_boards: u32) -> World {
        World {
            sims: HashMap::new(),
            running_boards: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(running)),
            max_boards,
        }
    }

    /// Options whose arbiter does not exist: any attempt to spawn fails with 500,
    /// so a 409 proves the capacity check ran before the spawn.
    fn missing_arbiter_opts() -> ServerOptions {
        ServerOptions {
            arbiter_bin: PathBuf::from("/nonexistent/cluster-arbiter"),
            advertise_ip: "127.0.0.1".into(),
            time_sync_period_ms: 100,
            data_udp_window: 32,
        }
    }

    fn two_board_topology() -> LogicalTopology {
        let board = |id: &str| crate::topology::BoardSpec {
            id: id.into(),
            kind: "rl78".into(),
            elf: None,
            endpoints: vec![],
        };
        LogicalTopology {
            boards: vec![board("a"), board("b")],
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 200,
            ready_timeout_ms: 10_000,
        }
    }

    fn dispatch_reply(world: &mut World, kind: JobKind) -> JobReply {
        let (reply_tx, reply_rx) = mpsc::channel();
        dispatch_job(
            world,
            Job {
                kind,
                reply: reply_tx,
            },
            &missing_arbiter_opts(),
        );
        reply_rx.recv().expect("dispatch_job replies")
    }

    fn expect_conflict(reply: JobReply) {
        match reply {
            JobReply::Err(err) => {
                assert_eq!(err.status, 409, "{err:?}");
                assert_eq!(err.code, "conflict");
                assert!(err.message.contains("max_boards"), "{}", err.message);
            }
            other => panic!("expected 409 conflict, got {other:?}"),
        }
    }

    #[test]
    fn board_capacity_allows_up_to_max_boards() {
        assert!(check_board_capacity(&capacity_world(0, 2), 2).is_ok());
        assert!(check_board_capacity(&capacity_world(1, 2), 1).is_ok());
        assert_eq!(
            check_board_capacity(&capacity_world(1, 2), 2)
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            check_board_capacity(&capacity_world(2, 2), 1)
                .unwrap_err()
                .status,
            409
        );
    }

    #[test]
    fn create_rejects_when_max_boards_would_be_exceeded() {
        // 1 running + 2 requested > max 2.
        let mut world = capacity_world(1, 2);
        let reply = dispatch_reply(&mut world, JobKind::Create(two_board_topology()));
        expect_conflict(reply);
        assert!(world.sims.is_empty());
    }

    #[test]
    fn create_phase1_rejects_when_max_boards_would_be_exceeded() {
        // 1 running + 2 local boards > max 2.
        let mut world = capacity_world(1, 2);
        let reply = dispatch_reply(&mut world, JobKind::CreatePhase1(phase1_request("sim-1")));
        expect_conflict(reply);
        assert!(world.sims.is_empty());
    }

    fn phase1_request(id: &str) -> Phase1CreateRequest {
        use crate::orchestrate::{AssignmentTable, BoardAssignment};
        Phase1CreateRequest {
            id: id.into(),
            topology: two_board_topology(),
            assignment: AssignmentTable {
                entries: ["a", "b"]
                    .into_iter()
                    .map(|board_id| BoardAssignment {
                        board_id: board_id.into(),
                        backend: "127.0.0.1:8091".into(),
                    })
                    .collect(),
            },
            local_backend: "127.0.0.1:8091".into(),
        }
    }

    #[test]
    fn create_phase1_rejects_an_unsafe_simulation_id_before_spawning() {
        for bad in ["", "a/b", "..", "sim 1", "x\n", &"a".repeat(129)] {
            let mut world = capacity_world(0, 8);
            match dispatch_reply(&mut world, JobKind::CreatePhase1(phase1_request(bad))) {
                JobReply::Err(err) => {
                    assert_eq!(err.status, 400, "id {bad:?}: {err:?}");
                    assert_eq!(err.code, "invalid_id", "id {bad:?}");
                }
                other => panic!("id {bad:?}: expected 400, got {other:?}"),
            }
        }
    }

    #[test]
    fn create_phase1_uses_the_requested_id_and_rejects_a_duplicate() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-arbiter.sh");
        fs::write(&script, "#!/bin/sh\nsleep 300 &\nwait\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let opts = ServerOptions {
            arbiter_bin: script,
            advertise_ip: "127.0.0.1".into(),
            time_sync_period_ms: 100,
            data_udp_window: 32,
        };
        let mut world = capacity_world(0, 8);
        let run = |world: &mut World, id: &str| {
            let (reply_tx, reply_rx) = mpsc::channel();
            dispatch_job(
                world,
                Job {
                    kind: JobKind::CreatePhase1(phase1_request(id)),
                    reply: reply_tx,
                },
                &opts,
            );
            reply_rx.recv().expect("dispatch_job replies")
        };

        match run(&mut world, "frontend-chosen-0") {
            JobReply::CreatedPhase1(reply) => assert_eq!(reply.id, "frontend-chosen-0"),
            other => panic!("expected created simulation, got {other:?}"),
        }
        assert!(world.sims.contains_key("frontend-chosen-0"));

        match run(&mut world, "frontend-chosen-0") {
            JobReply::Err(err) => {
                assert_eq!(err.status, 409, "{err:?}");
                assert!(err.message.contains("already exists"), "{err:?}");
            }
            other => panic!("expected 409 for a duplicate id, got {other:?}"),
        }
        // The existing simulation is untouched by the rejected duplicate.
        assert_eq!(world.sims.len(), 1);

        for sim in world.sims.values_mut() {
            sim.terminate_within(Duration::from_millis(200));
        }
    }

    #[test]
    fn create_within_max_boards_reaches_arbiter_spawn() {
        // 0 running + 2 requested == max 2 passes the check; the missing arbiter
        // then fails the spawn with a non-409 error.
        let mut world = capacity_world(0, 2);
        match dispatch_reply(&mut world, JobKind::Create(two_board_topology())) {
            JobReply::Err(err) => assert_ne!(err.status, 409, "{err:?}"),
            other => panic!("expected spawn failure, got {other:?}"),
        }
    }

    #[test]
    fn write_phase2_file_is_complete_json_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase2.json");
        let req = phase2(vec![endpoints("b:1", 20, 21)], "239.255.0.1", 47000);
        write_phase2_file(&path, &req).unwrap();
        let back: Phase2ConnectRequest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.cluster_key, req.cluster_key);
        assert_eq!(back.peers, req.peers);
        assert_eq!(back.multicast, req.multicast);
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn phase2_single_backend_accepts_only_empty_peers() {
        assert!(check_phase2(None, &phase2(vec![], "239.255.0.0", 47000)).is_ok());
        let err = check_phase2(
            None,
            &phase2(vec![endpoints("b:1", 1, 2)], "239.255.0.0", 47000),
        )
        .unwrap_err();
        assert_eq!(err.status, 409);
    }

    #[test]
    fn phase2_multi_backend_validates_peers_and_group() {
        let local = endpoints("a:1", 10, 11);
        let ok = phase2(vec![endpoints("b:1", 20, 21)], "239.255.0.1", 47000);
        assert!(check_phase2(Some(&local), &ok).is_ok());

        let cases = [
            phase2(vec![], "239.255.0.1", 47000),
            phase2(vec![endpoints("b:1", 20, 21)], "10.0.0.1", 47000),
            phase2(vec![endpoints("b:1", 20, 21)], "239.255.0.1", 0),
            phase2(vec![endpoints("a:1", 20, 21)], "239.255.0.1", 47000),
            phase2(
                vec![endpoints("b:1", 20, 21), endpoints("b:1", 22, 23)],
                "239.255.0.1",
                47000,
            ),
            phase2(vec![endpoints("b:1", 0, 21)], "239.255.0.1", 47000),
        ];
        for req in cases {
            let err = check_phase2(Some(&local), &req).unwrap_err();
            assert_eq!(err.status, 400, "{req:?}");
        }

        // The shared cluster_key is stamped on every time-sync packet.
        for bad_key in [String::new(), "k".repeat(256)] {
            let req = Phase2ConnectRequest {
                cluster_key: bad_key,
                ..ok.clone()
            };
            let err = check_phase2(Some(&local), &req).unwrap_err();
            assert_eq!(err.status, 400, "{req:?}");
        }
        let longest = Phase2ConnectRequest {
            cluster_key: "k".repeat(255),
            ..ok
        };
        assert!(check_phase2(Some(&local), &longest).is_ok());
    }

    #[test]
    fn topology_dsl_unittests() {
        let python_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../python");
        let python_root = python_root.canonicalize().expect("python dir");
        let status = std::process::Command::new("python3")
            .env("PYTHONPATH", &python_root)
            .args(["-m", "unittest", "discover", "-s"])
            .arg(python_root.join("tests"))
            .status()
            .expect("python3");
        assert!(status.success(), "python topology_dsl tests failed");
    }

    fn invoke_request(board_id: &str, device: &str, action: &str) -> InvokeActionRequest {
        InvokeActionRequest {
            board_id: board_id.into(),
            device: device.into(),
            action: action.into(),
            params: Default::default(),
        }
    }

    fn advertised(board_id: &str, device: &str, action: &str) -> AdvertisedAction {
        AdvertisedAction {
            board_id: board_id.into(),
            device: device.into(),
            action: action.into(),
            description: String::new(),
            params: Vec::new(),
        }
    }

    #[test]
    fn catalog_mismatch_is_unknown_device_when_board_has_no_rows() {
        let err = catalog_invoke_error(&[], &invoke_request("a", "sw1", "hold")).unwrap();
        assert_eq!(err.status, 404);
        assert_eq!(err.message, "unknown device");
    }

    #[test]
    fn catalog_mismatch_is_unknown_action_when_device_exists() {
        let catalog = [advertised("a", "sw1", "hold")];
        let err = catalog_invoke_error(&catalog, &invoke_request("a", "sw1", "missing")).unwrap();
        assert_eq!(err.status, 404);
        assert_eq!(err.message, "unknown action");
    }

    #[test]
    fn catalog_mismatch_accepts_known_device_and_action() {
        let catalog = [advertised("a", "sw1", "hold")];
        assert!(catalog_invoke_error(&catalog, &invoke_request("a", "sw1", "hold")).is_none());
    }

    #[test]
    fn watch_pending_drops_oldest_when_full() {
        let mut watch = ProbeWatch {
            probes: Vec::new(),
            probe_ids: Vec::new(),
            next_seq: 0,
            pending: VecDeque::new(),
        };
        for seq in 0..(WATCH_PENDING_KEEP as u64 + 3) {
            watch.push_batch(CoarseBatch {
                seq,
                samples: Vec::new(),
            });
        }
        assert_eq!(watch.pending.len(), WATCH_PENDING_KEEP);
        assert_eq!(watch.pending.front().map(|batch| batch.seq), Some(3));
        assert_eq!(
            watch.pending.back().map(|batch| batch.seq),
            Some(WATCH_PENDING_KEEP as u64 + 2)
        );
    }

    /// `true` while `pid` is a live (non-zombie) process.
    fn process_alive(pid: u32) -> bool {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            // Format: `pid (comm) state ...`; comm may contain spaces.
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| !rest.starts_with('Z')),
            Err(_) => false,
        }
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn temp_paths_remove_files_and_directories_on_drop() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("iox-root");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/inspect.sock"), b"x").unwrap();
        let file = base.path().join("topology.json");
        fs::write(&file, b"{}").unwrap();

        let mut temp = TempPaths::default();
        temp.push(dir.clone());
        temp.push(file.clone());
        temp.push(base.path().join("never-created"));
        drop(temp);

        assert!(!dir.exists());
        assert!(!file.exists());
    }

    #[test]
    fn kill_process_tree_also_kills_grandchildren() {
        use std::io::Read;

        // The shell stands in for the arbiter; `sleep` for a node it started.
        let mut shell = Command::new("sh")
            .arg("-c")
            .arg("sleep 300 & echo $!; wait")
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut out = shell.stdout.take().unwrap();
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while out.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
            line.push(byte[0]);
        }
        let grandchild: u32 = String::from_utf8(line).unwrap().trim().parse().unwrap();
        assert!(process_alive(grandchild));

        kill_process_tree(&mut shell);

        assert!(
            shell.try_wait().unwrap().is_some(),
            "arbiter must be reaped"
        );
        wait_until("grandchild to die", || !process_alive(grandchild));
    }

    #[test]
    fn terminating_a_simulation_removes_its_processes_and_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("node.pid");
        let script = dir.path().join("fake-arbiter.sh");
        // Ignores the host Shutdown command, like an arbiter still waiting for
        // its nodes, and leaves one child process behind.
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 300 &\necho $! > {}\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let opts = ServerOptions {
            arbiter_bin: script,
            advertise_ip: "127.0.0.1".into(),
            time_sync_period_ms: 100,
            data_udp_window: 32,
        };

        let mut sim =
            create_simulation(two_board_topology(), &opts, None, new_cluster_key()).unwrap();
        let iox_root = sim.iox_root.clone();
        assert!(iox_root.is_dir());
        wait_until("fake node pid", || {
            fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<u32>().is_ok())
        });
        let node_pid: u32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(process_alive(node_pid));

        sim.terminate_within(Duration::from_millis(200));
        assert!(sim.child.is_none());
        drop(sim);

        wait_until("node to die", || !process_alive(node_pid));
        assert!(!iox_root.exists(), "iceoryx root must be removed");
    }
}

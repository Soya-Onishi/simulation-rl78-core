//! Long-lived simulation server worker and process entry.
//!
//! HTTP is served by [`crate::http`]; this module owns arbiter processes and
//! the control-plane job queue used by [`crate::backend::LocalBackend`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use iceoryx2::prelude::{Node, ipc};

use crate::action::{ActionRequest, ActionUpstream};
use crate::backend::{LocalBackend, SimulationBackend};
use crate::contract::{
    ActionList, AdvertisedAction, ApiError, InvokeActionRequest, LogEvent, ProbeSamplesResponse,
    StatusEvent,
};
use crate::elf_source;
use crate::host::{HostCommand, SessionNode};
use crate::ipc::{
    IpcError, MSG_CATALOG, MSG_CATALOG_END, MSG_SAMPLE, ProbeQueryWire, ProbeWatchWire,
    ServerActions, ServerHostPort, ServerLogInbox, ServerProbeCtrl, ServerProbeInbox,
    board_hash_table, create_node, isolated_config, new_cluster_key, root_path_for_cluster,
};
use crate::log_msg::{ClusterLog, LogOrigin};
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

/// Options for [`run_server`].
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// `host:port` to bind. Port `0` picks a free port.
    pub listen: String,
    /// Path to the `cluster-arbiter` executable.
    pub arbiter_bin: PathBuf,
    /// Directory containing `topology_dsl` (added to `PYTHONPATH`).
    pub python_path: PathBuf,
    /// Python interpreter (default `python3`).
    pub python_bin: PathBuf,
    /// Topology applied as soon as the listener is up (CLI convenience).
    pub topology: Option<PathBuf>,
}

impl ServerOptions {
    /// Sibling `cluster-arbiter` and the in-tree `python/` package.
    pub fn new(listen: impl Into<String>) -> Result<Self, ServerError> {
        let exe = std::env::current_exe().map_err(ServerError::Io)?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| ServerError::Message("current_exe has no parent".into()))?;
        Ok(Self {
            listen: listen.into(),
            arbiter_bin: exe_dir.join("cluster-arbiter"),
            python_path: discover_python_package_root()?,
            python_bin: PathBuf::from("python3"),
            topology: None,
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
    #[error("python exited with status {status}: {stderr}")]
    PythonFailed { status: String, stderr: String },
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
}

impl StoredLog {
    fn to_event(&self) -> LogEvent {
        LogEvent {
            seq: self.seq,
            level: self.level.clone(),
            origin: self.origin.clone(),
            board: self.board.clone(),
            text: self.text.clone(),
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
}

/// Worker → backend replies.
#[derive(Debug)]
pub enum JobReply {
    Created {
        id: String,
    },
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
    ViewerToken(String),
    Err(ApiError),
}

struct World {
    sims: HashMap<String, Simulation>,
}

type IoxNode = Node<ipc::Service>;

struct Simulation {
    id: String,
    topo_path: PathBuf,
    child: Option<Child>,
    host: ServerHostPort,
    state: String,
    virtual_time_ns: u64,
    stop_reason: Option<String>,
    nodes: Vec<SessionNode>,
    logs: VecDeque<StoredLog>,
    next_seq: u64,
    boards: HashMap<u64, String>,
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
    _iox: IoxNode,
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

/// Accept control requests until the process is killed.
///
/// Arbiter processes come and go; this function does not return when one exits.
pub fn run_server(opts: &ServerOptions) -> Result<(), ServerError> {
    if !opts.arbiter_bin.is_file() {
        return Err(ServerError::Message(format!(
            "cluster-arbiter not found at {} (build the arbiter binary first)",
            opts.arbiter_bin.display()
        )));
    }
    let (tx, rx) = mpsc::channel();
    let worker_opts = opts.clone();
    thread::spawn(move || worker_loop(rx, worker_opts));

    let backend = LocalBackend::new(tx);
    if let Some(path) = opts.topology.clone() {
        match topology_from_file(&path, &opts.python_bin, &opts.python_path) {
            Ok(topo) => match backend.create(topo) {
                Ok(id) => eprintln!("loaded topology as simulation {id} (stopped)"),
                Err(err) => eprintln!("cluster-server: failed to create simulation: {err}"),
            },
            Err(err) => eprintln!("cluster-server: failed to load --topology: {err}"),
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(crate::http::serve(&opts.listen, backend))?;
    Ok(())
}

/// Run a topology script and return its logical JSON.
pub fn execute_python_script(
    python_bin: &Path,
    python_path: &Path,
    script: &Path,
) -> Result<String, ServerError> {
    let output = Command::new(python_bin)
        .env(
            "PYTHONPATH",
            prepend_pythonpath(python_path, std::env::var_os("PYTHONPATH")),
        )
        .arg("-m")
        .arg("topology_dsl")
        .arg("run")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(ServerError::PythonFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(ServerError::Message(
            "python topology script produced empty stdout".into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Write `source` to a temp file and run it as a topology script.
pub fn execute_python_source(
    python_bin: &Path,
    python_path: &Path,
    source: &str,
) -> Result<String, ServerError> {
    let path = temp_path("sim-cluster-topology", "py");
    fs::write(&path, source)?;
    let result = execute_python_script(python_bin, python_path, &path);
    let _ = fs::remove_file(&path);
    result
}

/// Locate `python/topology_dsl` from the manifest or the executable path.
pub fn discover_python_package_root() -> Result<PathBuf, ServerError> {
    let mut candidates = Vec::new();
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest_dir).join("../python"));
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        for _ in 0..6 {
            if let Some(d) = dir {
                candidates.push(d.join("python"));
                dir = d.parent().map(Path::to_path_buf);
            }
        }
    }
    for candidate in candidates {
        if candidate.join("topology_dsl").is_dir() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    Err(ServerError::Message(
        "could not locate python/topology_dsl; set ServerOptions.python_path".into(),
    ))
}

/// Validate topology JSON and embed local ELF paths for create.
pub fn prepare_topology(
    mut topology: LogicalTopology,
    base_dir: &Path,
) -> Result<LogicalTopology, ApiError> {
    topology
        .validate()
        .map_err(|err| ApiError::bad_request("invalid_topology", err.to_string()))?;
    embed_local_elfs(&mut topology, base_dir)
        .map_err(|err| ApiError::bad_request("invalid_topology", err.to_string()))?;
    Ok(topology)
}

fn worker_loop(rx: Receiver<Job>, opts: ServerOptions) {
    let mut world = World {
        sims: HashMap::new(),
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
        JobKind::Create(topo) => match create_simulation(topo, opts) {
            Ok(sim) => {
                let id = sim.id.clone();
                world.sims.insert(id.clone(), sim);
                JobReply::Created { id }
            }
            Err(err) => JobReply::Err(ApiError::internal(err.to_string())),
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
                let _ = send_cmd(&mut sim, HostCommand::Shutdown);
                JobReply::Deleted
            }
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
        },
        JobKind::Exists { id } => JobReply::Exists(world.sims.contains_key(&id)),
        JobKind::GetStatus { id } => match world.sims.get(&id) {
            Some(sim) => JobReply::Status(status_event(sim)),
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
    };
    let _ = job.reply.send(reply);
}

fn topology_from_file(
    path: &Path,
    python_bin: &Path,
    python_path: &Path,
) -> Result<LogicalTopology, String> {
    let base = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut topo = if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
        let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
        LogicalTopology::from_json_str(&text).map_err(|err| err.to_string())?
    } else {
        let json =
            execute_python_script(python_bin, python_path, path).map_err(|err| err.to_string())?;
        LogicalTopology::from_json_str(&json).map_err(|err| err.to_string())?
    };
    embed_local_elfs(&mut topo, base).map_err(|err| err.to_string())?;
    Ok(topo)
}

fn create_simulation(
    topo: LogicalTopology,
    opts: &ServerOptions,
) -> Result<Simulation, ServerError> {
    let id = new_cluster_key();
    let iox_root = root_path_for_cluster(&id);
    let config = isolated_config(&iox_root)?;
    let iox = create_node(&config, &format!("server-{id}"))?;
    let inbox = ServerLogInbox::create(&iox, &id)?;
    let host = ServerHostPort::create(&iox, &id)?;
    let actions = ServerActions::create(&iox, &id, topo.boards.len())?;
    let probe_inbox = ServerProbeInbox::create(&iox, &id)?;
    let probe_ctrl = ServerProbeCtrl::create(&iox, &id, topo.boards.len())?;
    let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str()))
        .map_err(ServerError::Message)?;
    let topo_path = write_temp_topology(&topo)?;
    let child = Command::new(&opts.arbiter_bin)
        .arg("--topology")
        .arg(&topo_path)
        .arg("--cluster-key")
        .arg(&id)
        .arg("--iox-root")
        .arg(&iox_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let nodes = topo
        .boards
        .iter()
        .map(|board| SessionNode {
            id: board.id.clone(),
            kind: board.kind.clone(),
            state: "starting".into(),
        })
        .collect();
    Ok(Simulation {
        id,
        topo_path,
        child: Some(child),
        host,
        state: "stopped".into(),
        virtual_time_ns: 0,
        stop_reason: None,
        nodes,
        logs: VecDeque::new(),
        next_seq: 1,
        boards,
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
        _iox: iox,
    })
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
                        eprintln!("cluster-server {}: action advertise: {err}", sim.id);
                    }
                }
            }
            Ok(Some(ActionUpstream::CatalogEnd)) => {
                sim.action_catalog_ready = true;
            }
            Ok(Some(ActionUpstream::Result(_))) => {}
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-server {}: action inbox: {err}", sim.id);
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
                eprintln!("cluster-server {}: probe catalog: {err}", sim.id);
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
                eprintln!("cluster-server {}: probe inbox: {err}", sim.id);
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
                eprintln!("cluster-server {}: csv reply: {err}", sim.id);
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
                eprintln!("cluster-server {}: host status: {err}", sim.id);
                break;
            }
        }
    }
    loop {
        match sim.inbox.try_recv() {
            Ok(Some(record)) => push_log(sim, &record),
            Ok(None) => break,
            Err(err) => {
                eprintln!("cluster-server {}: log inbox: {err}", sim.id);
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
                append_log(
                    sim,
                    "info",
                    "server",
                    None,
                    &format!("arbiter exited ({code})"),
                );
                sim.child = None;
            }
            Ok(None) => {}
            Err(err) => eprintln!("cluster-server {}: wait arbiter: {err}", sim.id),
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
    );
}

fn append_log(sim: &mut Simulation, level: &str, origin: &str, board: Option<String>, text: &str) {
    let seq = sim.next_seq;
    sim.next_seq = sim.next_seq.saturating_add(1);
    sim.logs.push_back(StoredLog {
        seq,
        level: level.to_string(),
        origin: origin.to_string(),
        board,
        text: text.to_string(),
    });
    while sim.logs.len() > LOG_CAP {
        sim.logs.pop_front();
    }
}

fn prepend_pythonpath(extra: &Path, existing: Option<std::ffi::OsString>) -> std::ffi::OsString {
    let mut out = extra.as_os_str().to_os_string();
    if let Some(prev) = existing {
        out.push(":");
        out.push(prev);
    }
    out
}

fn write_temp_topology(topo: &LogicalTopology) -> Result<PathBuf, ServerError> {
    let path = temp_path("sim-cluster-topology", "json");
    fs::write(&path, topo.to_json_string()?)?;
    Ok(path)
}

fn temp_path(prefix: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}.{ext}", new_cluster_key()))
}

impl Drop for Simulation {
    fn drop(&mut self) {
        let _ = send_cmd(self, HostCommand::Shutdown);
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(&self.topo_path);
    }
}

fn embed_local_elfs(topo: &mut LogicalTopology, base_dir: &Path) -> Result<(), ServerError> {
    for board in &mut topo.boards {
        let Some(elf) = board.elf.clone() else {
            continue;
        };
        if elf.starts_with("base64:") || elf.starts_with("http://") || elf.starts_with("https://") {
            elf_source::check_spec(&elf)
                .map_err(|err| ServerError::Message(format!("board {}: {err}", board.id)))?;
            continue;
        }
        let path = if elf.starts_with("file://") {
            let url = url::Url::parse(&elf)
                .map_err(|err| ServerError::Message(format!("invalid file elf URL: {err}")))?;
            url.to_file_path()
                .map_err(|()| ServerError::Message("elf file URL is not a local path".into()))?
        } else {
            let path = PathBuf::from(&elf);
            if path.is_absolute() {
                path
            } else {
                base_dir.join(path)
            }
        };
        if !path.is_file() {
            return Err(ServerError::Message(format!(
                "elf file not found for board {}: {}",
                board.id,
                path.display()
            )));
        }
        let bytes = fs::read(&path)?;
        board.elf = Some(format!("base64:{}", STANDARD.encode(bytes)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn python_round_trip_via_module() {
        let python_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../python");
        let python_root = python_root.canonicalize().expect("python dir");
        let mut script = tempfile::NamedTempFile::new().unwrap();
        write!(
            script,
            r#"
from topology_dsl import Topology, connect, emit

t = Topology(margin_ns=1000, headroom_threshold_ns=200)
a = t.board("a", kind="rl78", elf="a.elf")
b = t.board("b", kind="rl78")
a.endpoint("uart0_tx", direction="out", payload="uart")
a.endpoint("uart0_rx", direction="in", payload="uart")
b.endpoint("uart0_tx", direction="out", payload="uart")
b.endpoint("uart0_rx", direction="in", payload="uart")
connect(a.port("uart0_tx"), b.port("uart0_rx"))
connect(b.port("uart0_tx"), a.port("uart0_rx"), uart_ring_len=128)
emit(t)
"#
        )
        .unwrap();

        let json = execute_python_script(&PathBuf::from("python3"), &python_root, script.path())
            .expect("python run");
        let topo = LogicalTopology::from_json_str(&json).expect("parse");
        assert_eq!(topo.boards.len(), 2);
        assert_eq!(topo.edges.len(), 2);
        assert_eq!(topo.margin_ns, 1000);
    }

    #[test]
    fn prepare_topology_requires_boards() {
        let err = prepare_topology(
            LogicalTopology {
                boards: vec![],
                edges: vec![],
                margin_ns: 1000,
                headroom_threshold_ns: 200,
                ready_timeout_ms: 10_000,
            },
            Path::new("."),
        )
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "invalid_topology");
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
}

//! Long-lived simulation server worker and process entry.
//!
//! HTTP is served by [`crate::http`]; this module owns arbiter processes and
//! the control-plane job queue used by [`crate::backend::LocalBackend`].

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use iceoryx2::prelude::{Node, ipc};

use crate::action::{ActionRequest, ActionStatus, ActionUpstream};
use crate::backend::{LocalBackend, SimulationBackend};
use crate::contract::{
    ActionList, AdvertisedAction, ApiError, InvokeActionRequest, LogEvent, StatusEvent,
};
use crate::elf_source;
use crate::host::{HostCommand, SessionNode};
use crate::ipc::{
    IpcError, ServerActions, ServerHostPort, ServerLogInbox, board_hash_table, create_node,
    isolated_config, new_cluster_key, root_path_for_cluster,
};
use crate::log_msg::{ClusterLog, LogOrigin};
use crate::topology::{LogicalTopology, TopologyError};

/// Live SSE readers start at the high-water mark, so this only covers logs
/// that arrive between polls.
const LOG_CAP: usize = 100;

/// Wall-clock wait for a node to apply an action. This is not the hold duration.
const ACTION_WAIT: Duration = Duration::from_secs(5);

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
}

/// Worker → backend replies.
#[derive(Debug)]
pub enum JobReply {
    Created { id: String },
    Accepted,
    Deleted,
    Exists(bool),
    Status(StatusEvent),
    Logs(Vec<LogEvent>),
    HighWater(u64),
    Actions(ActionList),
    Applied,
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
    nodes: Vec<SessionNode>,
    logs: VecDeque<StoredLog>,
    next_seq: u64,
    boards: HashMap<u64, String>,
    inbox: ServerLogInbox,
    actions: ServerActions,
    catalog: Vec<crate::contract::AdvertisedAction>,
    pending: HashMap<u64, PendingAction>,
    next_request_id: u64,
    _iox: IoxNode,
}

struct PendingAction {
    reply: Sender<JobReply>,
    deadline: Instant,
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
                Ok(request_id) => {
                    sim.pending.insert(
                        request_id,
                        PendingAction {
                            reply: job.reply,
                            deadline: Instant::now() + ACTION_WAIT,
                        },
                    );
                    return;
                }
                Err(err) => JobReply::Err(err),
            },
            None => JobReply::Err(ApiError::not_found("unknown simulation")),
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
        nodes,
        logs: VecDeque::new(),
        next_seq: 1,
        boards,
        inbox,
        actions,
        catalog: Vec::new(),
        pending: HashMap::new(),
        next_request_id: 1,
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
        nodes: sim.nodes.clone(),
    }
}

fn queue_invoke(sim: &mut Simulation, request: &InvokeActionRequest) -> Result<u64, ApiError> {
    if sim.state == "exited" {
        return Err(ApiError::conflict("arbiter has exited"));
    }
    if sim.state != "running" {
        return Err(ApiError::new(409, "stopped", "simulation is stopped"));
    }
    let board_hash = sim_kernel::id_hash(&request.board_id);
    if !sim.boards.contains_key(&board_hash) {
        return Err(ApiError::not_found("unknown board"));
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
    Ok(request_id)
}

fn poll_actions(sim: &mut Simulation) {
    let now = Instant::now();
    let expired: Vec<u64> = sim
        .pending
        .iter()
        .filter(|(_, pending)| now >= pending.deadline)
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        if let Some(pending) = sim.pending.remove(&id) {
            let _ = pending.reply.send(JobReply::Err(ApiError::new(
                504,
                "timeout",
                "action was not applied before the deadline",
            )));
        }
    }
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
            Ok(Some(ActionUpstream::Result(result))) => {
                let Some(pending) = sim.pending.remove(&result.request_id) else {
                    continue;
                };
                let reply = match ActionStatus::from_u8(result.status) {
                    Some(ActionStatus::Applied) => JobReply::Applied,
                    Some(status) => JobReply::Err(action_status_error(status)),
                    None => JobReply::Err(ApiError::internal(format!(
                        "unknown action status {}",
                        result.status
                    ))),
                };
                let _ = pending.reply.send(reply);
            }
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

fn action_status_error(status: ActionStatus) -> ApiError {
    match status {
        ActionStatus::Applied => ApiError::internal("applied is not an error"),
        ActionStatus::Stopped => ApiError::new(409, "stopped", "simulation is stopped"),
        ActionStatus::Busy => ApiError::new(409, "busy", "device has an action in progress"),
        ActionStatus::NotFoundDevice => ApiError::not_found("unknown device"),
        ActionStatus::NotFoundAction => ApiError::not_found("unknown action"),
        ActionStatus::BadParam => ApiError::bad_request("bad_param", "invalid action parameter"),
    }
}

fn poll_simulation(sim: &mut Simulation) {
    loop {
        match sim.host.try_status() {
            Ok(Some(status)) => {
                sim.state = status.state;
                sim.virtual_time_ns = status.virtual_time_ns;
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
                fail_pending(sim, ApiError::conflict("arbiter has exited"));
            }
            Ok(None) => {}
            Err(err) => eprintln!("cluster-server {}: wait arbiter: {err}", sim.id),
        }
    }
}

fn fail_pending(sim: &mut Simulation, err: ApiError) {
    for (_, pending) in sim.pending.drain() {
        let _ = pending.reply.send(JobReply::Err(err.clone()));
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
        for (_, pending) in self.pending.drain() {
            let _ = pending
                .reply
                .send(JobReply::Err(ApiError::not_found("unknown simulation")));
        }
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
}

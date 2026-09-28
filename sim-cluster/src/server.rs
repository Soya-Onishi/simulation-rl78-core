//! Long-lived simulation server.
//!
//! One process serves the control page and owns each simulation's arbiter.
//! Logs stay in that process. The page filters them for display only.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use iceoryx2::prelude::{Node, ipc};
use serde::Deserialize;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::elf_source;
use crate::host::{HostCommand, SessionNode};
use crate::http_util::{self, Incoming, Outgoing};
use crate::ipc::{
    IpcError, ServerHostPort, ServerLogInbox, board_hash_table, create_node, isolated_config,
    new_cluster_key, root_path_for_cluster,
};
use crate::log_msg::{ClusterLog, LogOrigin};
use crate::topology::{LogicalTopology, TopologyError};

const LOG_CAP: usize = 100_000;
const PUSH_TIMEOUT: Duration = Duration::from_secs(2);
pub const PAGE_HTML: &str = include_str!("../web/index.html");

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
    /// Topology applied to the built-in page as soon as the listener is up.
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
    #[error("http error: {0}")]
    Http(#[from] crate::http_util::HttpError),
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    #[serde(default)]
    topology: Option<LogicalTopology>,
    #[serde(default)]
    python: Option<String>,
    #[serde(default)]
    log_sink: Option<String>,
}

#[derive(Clone, serde::Serialize)]
struct StoredLog {
    seq: u64,
    level: String,
    origin: String,
    board: Option<String>,
    text: String,
}

#[derive(Debug)]
struct PreparedCreate {
    topo: LogicalTopology,
    log_sink: Option<String>,
}

enum JobKind {
    Create(PreparedCreate),
    OpenPage(PreparedCreate),
    PageNote(String),
    PageSnapshot { after: u64 },
    PageCommand(HostCommand),
    Command { id: String, cmd: HostCommand },
    Delete { id: String },
    Get { id: String },
    Logs { id: String, after: u64 },
}

struct Job {
    kind: JobKind,
    reply: Sender<Outgoing>,
}

struct World {
    sims: HashMap<String, Simulation>,
    /// Simulation shown by the built-in page.
    page: Option<String>,
    page_error: Option<String>,
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
    log_sink: Option<String>,
    logs: VecDeque<StoredLog>,
    next_seq: u64,
    pushed_seq: u64,
    boards: HashMap<u64, String>,
    inbox: ServerLogInbox,
    _iox: IoxNode,
    status_dirty: bool,
    retry_push_at: Option<std::time::Instant>,
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

    let python_bin = opts.python_bin.clone();
    let python_path = opts.python_path.clone();
    let (listener, addr) = http_util::bind(&opts.listen)?;
    if let Some(path) = opts.topology.clone() {
        match topology_from_file(&path, &python_bin, &python_path) {
            Ok(prepared) => {
                let _ = rpc(&tx, JobKind::OpenPage(prepared));
            }
            Err(err) => {
                let _ = rpc(&tx, JobKind::PageNote(err));
            }
        }
    }
    eprintln!("cluster-server listening on http://{addr}/");
    http_util::serve(listener, move |req| {
        handle_http(&req, &tx, &python_bin, &python_path)
    });
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

fn handle_http(
    req: &Incoming,
    tx: &Sender<Job>,
    python_bin: &Path,
    python_path: &Path,
) -> Outgoing {
    let path = req.path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    let result = match (req.method.as_str(), path) {
        ("GET", "/") => return Outgoing::html(PAGE_HTML),
        ("GET", "/api/snapshot") => {
            let after = query_u64(&req.query, "after").unwrap_or(0);
            rpc(tx, JobKind::PageSnapshot { after })
        }
        ("POST", "/api/start") => rpc(tx, JobKind::PageCommand(HostCommand::Start)),
        ("POST", "/api/stop") => rpc(tx, JobKind::PageCommand(HostCommand::Stop)),
        ("POST", "/api/reset") => rpc(tx, JobKind::PageCommand(HostCommand::Reset)),
        ("POST", "/api/topology") => {
            let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match prepare_create(&req.body, python_bin, python_path, &base) {
                Ok(prepared) => rpc(tx, JobKind::OpenPage(prepared)),
                Err(err) => Ok(err),
            }
        }
        ("POST", "/v1/simulations") => {
            let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match prepare_create(&req.body, python_bin, python_path, &base) {
                Ok(prepared) => rpc(tx, JobKind::Create(prepared)),
                Err(err) => Ok(err),
            }
        }
        ("GET", path) if path.starts_with("/v1/simulations/") && path.ends_with("/logs") => {
            let id = sim_id(path, "/logs").to_string();
            let after = query_u64(&req.query, "after").unwrap_or(0);
            rpc(tx, JobKind::Logs { id, after })
        }
        ("GET", path) if path.starts_with("/v1/simulations/") => {
            let id = path.trim_start_matches("/v1/simulations/").to_string();
            if id.is_empty() || id.contains('/') {
                Ok(Outgoing::json_error(404, "not found"))
            } else {
                rpc(tx, JobKind::Get { id })
            }
        }
        ("POST", path) if path.starts_with("/v1/simulations/") => {
            let rest = path.trim_start_matches("/v1/simulations/");
            match rest.rsplit_once('/') {
                Some((id, "start")) => rpc(
                    tx,
                    JobKind::Command {
                        id: id.to_string(),
                        cmd: HostCommand::Start,
                    },
                ),
                Some((id, "stop")) => rpc(
                    tx,
                    JobKind::Command {
                        id: id.to_string(),
                        cmd: HostCommand::Stop,
                    },
                ),
                Some((id, "reset")) => rpc(
                    tx,
                    JobKind::Command {
                        id: id.to_string(),
                        cmd: HostCommand::Reset,
                    },
                ),
                _ => Ok(Outgoing::json_error(404, "not found")),
            }
        }
        ("DELETE", path) if path.starts_with("/v1/simulations/") => {
            let id = path.trim_start_matches("/v1/simulations/").to_string();
            rpc(tx, JobKind::Delete { id })
        }
        _ => Ok(Outgoing::json_error(404, "not found")),
    };
    match result {
        Ok(outgoing) => outgoing,
        Err(err) => Outgoing::json_error(500, err),
    }
}

fn sim_id<'a>(path: &'a str, suffix: &str) -> &'a str {
    path.trim_start_matches("/v1/simulations/")
        .trim_end_matches(suffix)
        .trim_end_matches('/')
}

fn prepare_create(
    body: &[u8],
    python_bin: &Path,
    python_path: &Path,
    base_dir: &Path,
) -> Result<PreparedCreate, Outgoing> {
    let parsed: CreateBody = serde_json::from_slice(body)
        .map_err(|err| Outgoing::json_error(400, format!("invalid json: {err}")))?;
    if parsed.topology.is_some() && parsed.python.is_some() {
        return Err(Outgoing::json_error(
            400,
            "provide topology or python, not both",
        ));
    }
    let topo = if let Some(topo) = parsed.topology {
        topo.validate()
            .map_err(|err| Outgoing::json_error(400, err.to_string()))?;
        topo
    } else if let Some(source) = parsed.python {
        if source.trim().is_empty() {
            return Err(Outgoing::json_error(400, "python script is empty"));
        }
        let json = execute_python_source(python_bin, python_path, &source)
            .map_err(|err| Outgoing::json_error(400, err.to_string()))?;
        LogicalTopology::from_json_str(&json)
            .map_err(|err| Outgoing::json_error(400, err.to_string()))?
    } else {
        return Err(Outgoing::json_error(
            400,
            "topology json or python source is required",
        ));
    };
    let mut topo = topo;
    embed_local_elfs(&mut topo, base_dir)
        .map_err(|err| Outgoing::json_error(400, err.to_string()))?;
    if let Some(sink) = &parsed.log_sink {
        if !sink.starts_with("http://") {
            return Err(Outgoing::json_error(400, "log_sink must be an http:// URL"));
        }
    }
    Ok(PreparedCreate {
        topo,
        log_sink: parsed.log_sink,
    })
}

fn rpc(tx: &Sender<Job>, kind: JobKind) -> Result<Outgoing, String> {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(Job {
        kind,
        reply: reply_tx,
    })
    .map_err(|_| "cluster-server worker is not running".to_string())?;
    reply_rx
        .recv_timeout(Duration::from_secs(60))
        .map_err(|_| "cluster-server worker timed out".to_string())
}

fn worker_loop(rx: Receiver<Job>, opts: ServerOptions) {
    let mut world = World {
        sims: HashMap::new(),
        page: None,
        page_error: None,
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
    let outgoing = match job.kind {
        JobKind::Create(prepared) => match create_simulation(prepared, opts) {
            Ok(sim) => {
                let body = snapshot_json(&sim, true);
                adopt(world, sim, false);
                Outgoing::json(201, &body)
            }
            Err(err) => Outgoing::json_error(500, err.to_string()),
        },
        JobKind::OpenPage(prepared) => match create_simulation(prepared, opts) {
            Ok(sim) => {
                let body = snapshot_json(&sim, true);
                adopt(world, sim, true);
                Outgoing::json(201, &body)
            }
            Err(err) => {
                world.page_error = Some(err.to_string());
                Outgoing::json_error(500, world.page_error.clone().unwrap_or_default())
            }
        },
        JobKind::PageNote(message) => {
            world.page_error = Some(message.clone());
            Outgoing::json_error(400, message)
        }
        JobKind::PageSnapshot { after } => Outgoing::json(200, &page_snapshot(world, after)),
        JobKind::PageCommand(cmd) => match world.page.clone() {
            Some(id) => match world.sims.get_mut(&id) {
                Some(sim) => command_simulation(sim, cmd),
                None => Outgoing::json_error(409, "トポロジがまだ適用されていません"),
            },
            None => Outgoing::json_error(409, "トポロジがまだ適用されていません"),
        },
        JobKind::Command { id, cmd } => match world.sims.get_mut(&id) {
            Some(sim) => command_simulation(sim, cmd),
            None => Outgoing::json_error(404, "unknown simulation"),
        },
        JobKind::Delete { id } => match world.sims.remove(&id) {
            Some(mut sim) => {
                let _ = send_cmd(&mut sim, HostCommand::Shutdown);
                if world.page.as_deref() == Some(id.as_str()) {
                    world.page = None;
                }
                sim.state = "deleted".into();
                Outgoing::json(200, &serde_json::json!({"id": id, "state": "deleted"}))
            }
            None => Outgoing::json_error(404, "unknown simulation"),
        },
        JobKind::Get { id } => match world.sims.get(&id) {
            Some(sim) => Outgoing::json(200, &snapshot_json(sim, false)),
            None => Outgoing::json_error(404, "unknown simulation"),
        },
        JobKind::Logs { id, after } => match world.sims.get(&id) {
            Some(sim) => {
                let logs: Vec<_> = sim
                    .logs
                    .iter()
                    .filter(|log| log.seq > after)
                    .cloned()
                    .collect();
                Outgoing::json(200, &serde_json::json!({"simulation_id": id, "logs": logs}))
            }
            None => Outgoing::json_error(404, "unknown simulation"),
        },
    };
    let _ = job.reply.send(outgoing);
}

fn adopt(world: &mut World, sim: Simulation, replace_page: bool) {
    if replace_page {
        if let Some(old) = world.page.take() {
            if old != sim.id {
                if let Some(mut prev) = world.sims.remove(&old) {
                    let _ = send_cmd(&mut prev, HostCommand::Shutdown);
                }
            }
        }
    }
    if world.page.is_none() || replace_page {
        world.page = Some(sim.id.clone());
        world.page_error = None;
    }
    world.sims.insert(sim.id.clone(), sim);
}

fn page_snapshot(world: &World, after: u64) -> serde_json::Value {
    let Some(id) = world.page.as_deref() else {
        return serde_json::json!({
            "simulation_id": null,
            "state": "stopped",
            "virtual_time_ns": 0,
            "nodes": [],
            "logs": [],
            "error": world.page_error,
        });
    };
    let Some(sim) = world.sims.get(id) else {
        return serde_json::json!({
            "simulation_id": null,
            "state": "stopped",
            "virtual_time_ns": 0,
            "nodes": [],
            "logs": [],
            "error": world.page_error,
        });
    };
    let logs: Vec<_> = sim
        .logs
        .iter()
        .filter(|log| log.seq > after)
        .cloned()
        .collect();
    serde_json::json!({
        "simulation_id": sim.id,
        "state": sim.state,
        "virtual_time_ns": sim.virtual_time_ns,
        "nodes": sim.nodes,
        "logs": logs,
        "error": world.page_error,
    })
}

fn topology_from_file(
    path: &Path,
    python_bin: &Path,
    python_path: &Path,
) -> Result<PreparedCreate, String> {
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
    Ok(PreparedCreate {
        topo,
        log_sink: None,
    })
}

fn create_simulation(
    prepared: PreparedCreate,
    opts: &ServerOptions,
) -> Result<Simulation, ServerError> {
    let id = new_cluster_key();
    let iox_root = root_path_for_cluster(&id);
    let config = isolated_config(&iox_root)?;
    let iox = create_node(&config, &format!("server-{id}"))?;
    let inbox = ServerLogInbox::create(&iox, &id)?;
    let host = ServerHostPort::create(&iox, &id)?;
    let boards = board_hash_table(prepared.topo.boards.iter().map(|b| b.id.as_str()))
        .map_err(ServerError::Message)?;
    let topo_path = write_temp_topology(&prepared.topo)?;
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
    let nodes = prepared
        .topo
        .boards
        .iter()
        .map(|board| SessionNode {
            id: board.id.clone(),
            kind: board.kind.clone(),
            state: "starting".into(),
        })
        .collect();
    // The node process resolves base64:, http(s), and filesystem paths.
    Ok(Simulation {
        id,
        topo_path,
        child: Some(child),
        host,
        state: "stopped".into(),
        virtual_time_ns: 0,
        nodes,
        log_sink: prepared.log_sink,
        logs: VecDeque::new(),
        next_seq: 1,
        pushed_seq: 0,
        boards,
        inbox,
        _iox: iox,
        status_dirty: true,
        retry_push_at: None,
    })
}

fn command_simulation(sim: &mut Simulation, cmd: HostCommand) -> Outgoing {
    if sim.state == "exited" && !matches!(cmd, HostCommand::Reset | HostCommand::Shutdown) {
        return Outgoing::json_error(409, "arbiter has exited; reset or create again");
    }
    if let Err(err) = send_cmd(sim, cmd) {
        return Outgoing::json_error(500, err);
    }
    let name = match cmd {
        HostCommand::Start => "start",
        HostCommand::Stop => "stop",
        HostCommand::Reset => "reset",
        HostCommand::Shutdown => "shutdown",
    };
    Outgoing::json(
        202,
        &serde_json::json!({"id": sim.id, "accepted": name, "state": sim.state}),
    )
}

fn send_cmd(sim: &mut Simulation, cmd: HostCommand) -> Result<(), String> {
    sim.host.publish_cmd(cmd).map_err(|err| err.to_string())
}

fn poll_simulation(sim: &mut Simulation) {
    loop {
        match sim.host.try_status() {
            Ok(Some(status)) => {
                if sim.state != status.state
                    || sim.virtual_time_ns != status.virtual_time_ns
                    || sim.nodes != status.nodes
                {
                    sim.status_dirty = true;
                }
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
    if let Some(child) = sim.child.as_mut() {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                sim.state = "exited".into();
                sim.status_dirty = true;
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
    maybe_push(sim);
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
    sim.status_dirty = true;
}

fn maybe_push(sim: &mut Simulation) {
    let Some(sink) = sim.log_sink.clone() else {
        sim.pushed_seq = sim.next_seq.saturating_sub(1);
        sim.status_dirty = false;
        return;
    };
    let logs: Vec<_> = sim
        .logs
        .iter()
        .filter(|log| log.seq > sim.pushed_seq)
        .cloned()
        .collect();
    if logs.is_empty() && !sim.status_dirty {
        return;
    }
    if sim
        .retry_push_at
        .is_some_and(|at| std::time::Instant::now() < at)
    {
        return;
    }
    let body = serde_json::json!({
        "simulation_id": sim.id,
        "state": sim.state,
        "virtual_time_ns": sim.virtual_time_ns,
        "nodes": sim.nodes,
        "logs": logs,
    });
    let bytes = match serde_json::to_vec(&body) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("cluster-server {}: encode log push: {err}", sim.id);
            return;
        }
    };
    match http_util::exchange_timeout("POST", &sink, "application/json", &bytes, PUSH_TIMEOUT) {
        Ok(resp) if (200..300).contains(&resp.status) => {
            if let Some(last) = logs.last() {
                sim.pushed_seq = last.seq;
            }
            sim.status_dirty = false;
            sim.retry_push_at = None;
        }
        Ok(resp) => {
            sim.retry_push_at = Some(std::time::Instant::now() + Duration::from_secs(1));
            eprintln!(
                "cluster-server {}: log sink {} returned {}",
                sim.id, sink, resp.status
            );
        }
        Err(err) => {
            sim.retry_push_at = Some(std::time::Instant::now() + Duration::from_secs(1));
            eprintln!("cluster-server {}: log sink {sink}: {err}", sim.id);
        }
    }
}

fn snapshot_json(sim: &Simulation, include_log_count: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "id": sim.id,
        "state": sim.state,
        "virtual_time_ns": sim.virtual_time_ns,
        "nodes": sim.nodes,
    });
    if include_log_count {
        body["log_count"] = serde_json::json!(sim.logs.len());
    }
    body
}

fn query_u64(query: &str, key: &str) -> Option<u64> {
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            return value.parse().ok();
        }
    }
    None
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
    fn create_rejects_empty_body() {
        let err = prepare_create(
            b"{}",
            Path::new("python3"),
            Path::new("/tmp"),
            Path::new("."),
        )
        .unwrap_err();
        assert_eq!(err.status, 400);
    }
}

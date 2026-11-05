//! Arbiter: create iceoryx services, spawn nodes, Ready barrier, then wait.
//!
//! The guest stays stopped until a host `start` command. `stop` halts the
//! guest without exiting this process, so a later `start` or `reset` can
//! run. `shutdown` on the server→arbiter iceoryx channel ends the process.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::action::ActionUpstream;
use crate::cluster_net::{
    ControlMesh, DEFAULT_DATA_UDP_WINDOW, DataLink, MeshNotice, PeerListeners, TimeMcast,
};
use crate::control::{ControlToArbiter, ControlToNode, HostStopReason};
use crate::edge_proxy::{EdgeProxy, plan_cross_edges};
use crate::host::{HostCommand, SessionNode, SessionStatus};
use crate::ipc::{
    ArbiterControl, ArbiterHostPort, ArbiterLogBus, ArbiterNodeActions, ArbiterProbeCtrl,
    ArbiterProbeForward, ArbiterServerActions, IpcError, board_hash_table, create_node,
    isolated_config, new_cluster_key, root_path_for_cluster,
};
use crate::lifecycle::{PeerEffect, PeerState};
use crate::orchestrate::{AssignmentTable, Phase2ConnectRequest};
use crate::probe::ProbeWireKind;
use crate::probe_hub::{ArbiterProbeHub, ProbeSample};
use crate::time_sync::TimeCeiling;
use crate::topology::{LogicalTopology, TopologyError};

/// Idle poll while waiting for Ready / time-sync (arbiter side).
const ARBITER_POLL_IDLE: Duration = Duration::from_millis(1);

/// Arbiter failures.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("ready barrier timed out after {timeout_ms}ms; missing boards: {missing}")]
    ReadyTimeout { timeout_ms: u64, missing: String },
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Message(String),
}

/// Options for [`run_arbiter`].
#[derive(Clone, Debug)]
pub struct ArbiterOptions {
    pub topology_path: PathBuf,
    pub node_bin: PathBuf,
    /// Plant node binary (`kind == "plant"`). Default: `minimal-plant` next to the arbiter.
    pub plant_bin: PathBuf,
    /// Unique key for this cluster instance (iceoryx isolation + service names).
    pub cluster_key: String,
    /// Absolute iceoryx root path for this cluster.
    pub iox_root: PathBuf,
    /// When set, only these board ids are spawned and waited on for Ready.
    /// `None` means every board in the topology (single-backend default).
    pub local_board_ids: Option<Vec<String>>,
    /// When set, bind peer listen sockets and advertise them.
    /// `None` skips arbiter↔arbiter networking (single-backend path).
    pub peer_advertise: Option<PeerAdvertise>,
    /// When set (together with `peer_advertise`), the bound
    /// [`ArbiterEndpoints`](crate::orchestrate::ArbiterEndpoints) are written here
    /// as JSON so the spawning `cluster-backend` can answer create phase 1.
    pub endpoints_out: Option<PathBuf>,
    /// Multi-backend only: JSON file holding the `Phase2ConnectRequest` that the
    /// spawning `cluster-backend` writes once phase 2 is accepted. The arbiter
    /// waits for it, then joins the time-sync multicast group it names.
    /// Requires `peer_advertise`; `None` keeps the local-only TimeCeiling path.
    pub phase2_in: Option<PathBuf>,
    /// Period (ms) of the time-sync multicast report
    /// (`BackendConfig.time_sync_period_ms`).
    pub time_sync_period_ms: u64,
    /// Multi-backend only: JSON file holding the full
    /// [`AssignmentTable`](crate::orchestrate::AssignmentTable) (board → backend)
    /// from create phase 1. It tells the arbiter which edges cross machines and
    /// which peer owns the remote board. `None`: no cross-machine edge proxy.
    pub assignment_in: Option<PathBuf>,
    /// Messages in flight per peer on the data UDP link
    /// (`BackendConfig.data_udp_window`).
    pub data_udp_window: u32,
}

/// Default for [`ArbiterOptions::time_sync_period_ms`].
pub const DEFAULT_TIME_SYNC_PERIOD_MS: u64 = 100;

/// Identity used when binding peer listen sockets for multi-backend sims.
#[derive(Clone, Debug)]
pub struct PeerAdvertise {
    pub advertise_ip: String,
    pub backend_key: String,
}

impl ArbiterOptions {
    pub fn from_topology_path(topology_path: PathBuf) -> Result<Self, ArbiterError> {
        let exe = std::env::current_exe().map_err(ArbiterError::Io)?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| ArbiterError::Message("current_exe has no parent".into()))?;
        let cluster_key = new_cluster_key();
        let iox_root = root_path_for_cluster(&cluster_key);
        Ok(Self {
            topology_path,
            node_bin: exe_dir.join("rl78-minimal-board"),
            plant_bin: exe_dir.join("minimal-plant"),
            cluster_key,
            iox_root,
            local_board_ids: None,
            peer_advertise: None,
            endpoints_out: None,
            phase2_in: None,
            time_sync_period_ms: DEFAULT_TIME_SYNC_PERIOD_MS,
            assignment_in: None,
            data_udp_window: DEFAULT_DATA_UDP_WINDOW,
        })
    }
}

pub(crate) fn bind_peer_listeners(
    opts: &ArbiterOptions,
) -> Result<Option<crate::cluster_net::PeerListeners>, ArbiterError> {
    let Some(peer) = &opts.peer_advertise else {
        return Ok(None);
    };
    let listeners =
        crate::cluster_net::PeerListeners::bind(peer.backend_key.clone(), &peer.advertise_ip)
            .map_err(|err| ArbiterError::Message(err.to_string()))?;
    let ep = listeners.endpoints();
    log::info!(
        "peer listen advertise control={}:{} data={}:{} backend={}",
        ep.advertise_ip,
        ep.control_tcp_port,
        ep.advertise_ip,
        ep.data_udp_port,
        ep.backend
    );
    if let Some(path) = &opts.endpoints_out {
        write_endpoints_file(path, ep)?;
    }
    Ok(Some(listeners))
}

/// Write via a sibling temp file and rename so readers never see partial JSON.
fn write_endpoints_file(
    path: &std::path::Path,
    endpoints: &crate::orchestrate::ArbiterEndpoints,
) -> Result<(), ArbiterError> {
    let json = serde_json::to_vec(endpoints)
        .map_err(|err| ArbiterError::Message(format!("encode endpoints: {err}")))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Boards this arbiter owns: `local_board_ids` when set, otherwise the full topology.
pub fn select_local_boards<'a>(
    topo: &'a LogicalTopology,
    local_board_ids: Option<&[String]>,
) -> Result<Vec<&'a crate::topology::BoardSpec>, ArbiterError> {
    let Some(ids) = local_board_ids else {
        return Ok(topo.boards.iter().collect());
    };
    if ids.is_empty() {
        return Err(ArbiterError::Message(
            "local_board_ids must not be empty when set".into(),
        ));
    }
    let mut out = Vec::with_capacity(ids.len());
    let mut seen = std::collections::HashSet::<&str>::new();
    for id in ids {
        if !seen.insert(id.as_str()) {
            return Err(ArbiterError::Message(format!(
                "local_board_ids contains duplicate board `{id}`"
            )));
        }
        let Some(board) = topo.boards.iter().find(|b| b.id == *id) else {
            return Err(ArbiterError::Message(format!(
                "local_board_ids refers to unknown board `{id}`"
            )));
        };
        out.push(board);
    }
    Ok(out)
}

/// Load topology, spawn nodes, wait for Ready, then follow iceoryx host commands.
pub fn run_arbiter(opts: &ArbiterOptions) -> Result<(), ArbiterError> {
    let text = fs::read_to_string(&opts.topology_path)?;
    let topo = LogicalTopology::from_json_str(&text)?;
    run_arbiter_with_topology(opts, &topo)
}

/// Spawn nodes and stay up until the server publishes `shutdown`.
///
/// Host commands arrive on `ctrl/s2a`. Status is published on `status/a2s`.
pub fn run_arbiter_with_topology(
    opts: &ArbiterOptions,
    topo: &LogicalTopology,
) -> Result<(), ArbiterError> {
    ensure_node_bins(opts, topo)?;
    run_host_session(opts, topo)
}

/// Test helper: Ready barrier, immediate Start, then a short time-sync window.
#[cfg(test)]
fn ready_barrier_and_start(
    topo: &LogicalTopology,
    control: &ArbiterControl,
) -> Result<(), ArbiterError> {
    let mut probes = ArbiterProbeHub::new("test");
    let local_ids: Vec<String> = topo.boards.iter().map(|b| b.id.clone()).collect();
    let mut peers = await_ready(topo, &local_ids, control, &mut probes, None, None)?;
    control.publish(&ControlToNode::Start { allowed_ns: 0 })?;
    for state in peers.values_mut() {
        *state = state.on_start_broadcast();
    }
    log::info!("Start broadcast");
    run_time_sync(topo, control, &mut peers)?;
    Ok(())
}

fn await_ready(
    topo: &LogicalTopology,
    local_board_ids: &[String],
    control: &ArbiterControl,
    probes: &mut ArbiterProbeHub,
    actions: Option<&ArbiterNodeActions>,
    server: Option<&ArbiterServerActions>,
) -> Result<HashMap<String, PeerState>, ArbiterError> {
    let timeout = Duration::from_millis(topo.ready_timeout_ms);
    let deadline = Instant::now() + timeout;
    let mut peers: HashMap<String, PeerState> = local_board_ids
        .iter()
        .map(|id| (id.clone(), PeerState::AwaitingReady))
        .collect();

    let startup = ControlToNode::StartupRecord {
        margin_ns: topo.margin_ns,
        headroom_threshold_ns: topo.headroom_threshold_ns,
    };
    control.publish(&startup)?;
    let mut last_startup = Instant::now();

    while peers.values().any(|p| *p != PeerState::Ready) {
        if Instant::now() >= deadline {
            return Err(ready_timeout(topo, &peers));
        }
        if last_startup.elapsed() > Duration::from_millis(50)
            && peers.values().any(|p| *p == PeerState::AwaitingReady)
        {
            let _ = control.publish(&startup);
            last_startup = Instant::now();
        }

        let mut progressed = false;
        forward_catalog(actions, server)?;
        while let Some(msg) = control.try_recv()? {
            match msg {
                ControlToArbiter::ProbeAdvertise {
                    from,
                    name_hash,
                    probe_id,
                    kind,
                    name_len,
                    name,
                } => {
                    let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
                        continue;
                    };
                    let Some(kind) = ProbeWireKind::from_u8(kind) else {
                        log::warn!("ignoring probe advertise with bad kind {kind} from {from:#x}");
                        continue;
                    };
                    let n = (name_len as usize).min(name.len());
                    probes.record_wire_advertise(
                        board_id,
                        from,
                        name_hash,
                        probe_id,
                        kind,
                        &name[..n],
                    );
                    progressed = true;
                }
                ControlToArbiter::Ready { from } => {
                    let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
                        continue;
                    };
                    let Some(state) = peers.get_mut(&board_id) else {
                        continue;
                    };
                    let (next, effect) = state.on_message(msg);
                    warn_peer(&board_id, effect)?;
                    if next != *state {
                        log::trace!("peer '{board_id}' {state:?} -> {next:?}");
                    }
                    *state = next;
                    progressed = true;
                }
                other => {
                    log::warn!("ignoring unexpected before Start: {other:?}");
                }
            }
        }
        if !progressed {
            thread::sleep(ARBITER_POLL_IDLE);
        }
    }

    // Advertisements are send_copy'd before Ready, but they can land in the
    // action subscriber between this iteration's drain and the control read
    // that observes Ready. Drain once more before later advertisements are
    // discarded.
    forward_catalog(actions, server)?;
    if let Some(server) = server {
        server.publish_upstream(&ActionUpstream::CatalogEnd)?;
    }

    log::info!(
        "Ready barrier complete ({} nodes, {} probes)",
        peers.len(),
        probes.catalog().len()
    );
    Ok(peers)
}

#[cfg(test)]
fn run_time_sync(
    topo: &LogicalTopology,
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    use crate::time_sync::TimeCeiling;

    let mut ceiling = TimeCeiling::new(peers.keys().cloned(), topo.margin_ns);
    let mut allowed = ceiling.allowed_ns();
    control.publish(&ControlToNode::Allowed {
        allowed_ns: allowed,
    })?;
    log::info!("initial Allowed={allowed}");

    // TODO(cluster-loop): MVP wall-clock window only. Replace with an open loop
    // until ControlToNode::Shutdown (or equivalent) ends the run.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let mut changed = false;
        let mut cluster_stop: Option<(String, HostStopReason)> = None;
        while let Some(msg) = control.try_recv()? {
            let from = msg.from_board();
            let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
                continue;
            };
            let Some(state) = peers.get_mut(&board_id) else {
                continue;
            };
            let (next, effect) = state.on_message(msg);
            match effect.clone() {
                Some(PeerEffect::TimeReport {
                    from: _,
                    virtual_time_ns,
                }) => {
                    ceiling.report(&board_id, virtual_time_ns);
                    changed = true;
                }
                Some(PeerEffect::HostStop { from: _, reason }) => {
                    // HostStopReason variants are all cluster-relevant by construction.
                    cluster_stop = Some((board_id.clone(), reason));
                }
                other => warn_peer(&board_id, other)?,
            }
            *state = next;
        }
        if let Some((source, reason)) = cluster_stop {
            log::info!("HostStop from '{source}' ({reason}); broadcasting ClusterStop");
            broadcast_cluster_stop(control, peers, &source, reason)?;
            return Ok(());
        }
        if changed {
            let next_allowed = ceiling.allowed_ns();
            if next_allowed != allowed {
                allowed = next_allowed;
                control.publish(&ControlToNode::Allowed {
                    allowed_ns: allowed,
                })?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SyncStep {
    Continue,
    /// Stopped by this arbiter itself (`until_ns` reached).
    Stopped(HostStopReason),
    /// Stopped because a local node reported HostStop; peers must be told.
    NodeStopped(HostStopReason),
    /// The stepped local board finished its instruction at `target_vt_ns` and
    /// the local nodes now catch up. Peer arbiters must be told when this
    /// arbiter opened the step towards them (`seq` is `Some`).
    GdbInsn {
        seq: Option<u64>,
        target_vt_ns: u64,
        reason: HostStopReason,
    },
}

/// GDB single-step of one board, as seen by this arbiter.
///
/// The arbiter that runs the stepped board (*owner*) goes through
/// `WaitInsn` -> `CatchUp`. Every other arbiter of a multi-backend simulation
/// goes through `PeerWait` -> `PeerCatchUp` and never executes the step on a
/// board of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GdbStepPhase {
    /// Owner: wait for the stepped local board's instruction. `seq` is the step
    /// number announced to the peers (`None` without peer arbiters).
    WaitInsn { board_id: String, seq: Option<u64> },
    /// Owner: the local nodes run until the stepped board's virtual time.
    CatchUp {
        reason: HostStopReason,
        seq: Option<u64>,
    },
    /// Peer: the owner `origin` announced step `seq`; local nodes are stopped
    /// until its catch-up message arrives.
    PeerWait { origin: String, seq: u64 },
    /// Peer: the local nodes run until the owner's target virtual time.
    PeerCatchUp {
        origin: String,
        seq: u64,
        reason: HostStopReason,
    },
}

impl GdbStepPhase {
    /// Step number this arbiter announced to its peers (owner phases only).
    fn announced_seq(&self) -> Option<u64> {
        match self {
            Self::WaitInsn { seq, .. } | Self::CatchUp { seq, .. } => *seq,
            Self::PeerWait { .. } | Self::PeerCatchUp { .. } => None,
        }
    }

    /// `true` for a step that follows the owner `origin`.
    fn follows(&self, origin: &str, step_seq: u64) -> bool {
        match self {
            Self::PeerWait { origin: o, seq } | Self::PeerCatchUp { origin: o, seq, .. } => {
                o == origin && *seq == step_seq
            }
            Self::WaitInsn { .. } | Self::CatchUp { .. } => false,
        }
    }
}

/// Drop the current GDB step because the host moved on (start / stop / reset).
/// A step announced to the peers is cancelled there too.
fn cancel_gdb_step(gdb_step: &mut Option<GdbStepPhase>, mesh: Option<&mut ControlMesh>) {
    let Some(phase) = gdb_step.take() else {
        return;
    };
    if let (Some(seq), Some(mesh)) = (phase.announced_seq(), mesh) {
        mesh.broadcast_step_cancel(seq);
    }
}

/// Forward catalog advertisements that arrived before this iteration's control read.
///
/// Nodes `send_copy` every advertisement before `Ready`, so the samples are already
/// in the action subscriber when `Ready` becomes visible. Draining here before the
/// control receive keeps that order. The arbiter then publishes `CatalogEnd` on the
/// server action channel so the HTTP layer can tell the catalog is complete.
fn forward_catalog(
    actions: Option<&ArbiterNodeActions>,
    server: Option<&ArbiterServerActions>,
) -> Result<(), ArbiterError> {
    let Some(actions) = actions else {
        return Ok(());
    };
    while let Some(msg) = actions.try_upstream()? {
        if let ActionUpstream::Advertise(ad) = msg {
            if let Some(server) = server {
                server.publish_upstream(&ActionUpstream::Advertise(ad))?;
            }
        }
    }
    Ok(())
}

/// After the ready barrier, advertisements are discarded. Requests are always
/// forwarded; nodes queue them while Stopped and apply on Start.
fn forward_invokes(
    actions: &ArbiterNodeActions,
    server: &ArbiterServerActions,
) -> Result<(), ArbiterError> {
    while let Some(req) = server.try_request()? {
        actions.publish_request(&req)?;
    }
    while let Some(msg) = actions.try_upstream()? {
        if let ActionUpstream::Result(result) = msg {
            server.publish_upstream(&ActionUpstream::Result(result))?;
        }
    }
    Ok(())
}

fn run_host_session(opts: &ArbiterOptions, topo: &LogicalTopology) -> Result<(), ArbiterError> {
    let local = select_local_boards(topo, opts.local_board_ids.as_deref())?;
    let local_ids: Vec<String> = local.iter().map(|b| b.id.clone()).collect();
    // Read before the endpoints file is written: the spawning backend may delete
    // its handoff files as soon as it sees the endpoints.
    let assignment = read_assignment_file(opts.assignment_in.as_deref())?;
    // Peer sockets live for the whole session: the control mesh dups the control
    // listener, the edge proxy dups the data UDP socket.
    let peer_listeners = bind_peer_listeners(opts)?;
    // iceoryx pub/sub limits must match Node opens, which size from the full topology.
    let ipc_board_count = topo.boards.len();
    let board_ids: Vec<&str> = topo.boards.iter().map(|b| b.id.as_str()).collect();
    let boards = board_hash_table(board_ids).map_err(ArbiterError::Message)?;

    let config = isolated_config(&opts.iox_root)?;
    let node = create_node(&config, &format!("arbiter-{}", opts.cluster_key))?;
    let host = ArbiterHostPort::open(&node, &opts.cluster_key)?;
    let control =
        ArbiterControl::create(&node, &opts.cluster_key, ipc_board_count, boards.clone())?;
    let node_actions = ArbiterNodeActions::create(&node, &opts.cluster_key, ipc_board_count)?;
    let server_actions = ArbiterServerActions::open(&node, &opts.cluster_key, ipc_board_count)?;
    let probe_ctrl = ArbiterProbeCtrl::open(&node, &opts.cluster_key, ipc_board_count)?;
    let mut logs =
        ArbiterLogBus::create(&opts.iox_root, &opts.cluster_key, ipc_board_count, boards)?;
    log::info!(
        "loaded topology ({} local / {} total boards, {} edges, margin_ns={}, cluster_key={})",
        local.len(),
        topo.boards.len(),
        topo.edges.len(),
        topo.margin_ns,
        opts.cluster_key
    );

    // Dropped on every exit path, including `?` returns, so no node outlives
    // the arbiter.
    let mut children = NodeChildren(spawn_nodes(opts, &local)?);
    let mut probe_hub = ArbiterProbeHub::new(opts.cluster_key.clone());
    let mut peers = match await_ready(
        topo,
        &local_ids,
        &control,
        &mut probe_hub,
        Some(&node_actions),
        Some(&server_actions),
    ) {
        Ok(peers) => peers,
        Err(err) => {
            stop_children(&mut children.0);
            logs.shutdown();
            return Err(err);
        }
    };
    if let Err(err) = probe_hub.subscribe_catalog(&node) {
        stop_children(&mut children.0);
        logs.shutdown();
        return Err(ArbiterError::Message(err.to_string()));
    }
    let probe_tx = match ArbiterProbeForward::open(&node, &opts.cluster_key) {
        Ok(tx) => tx,
        Err(err) => {
            stop_children(&mut children.0);
            logs.shutdown();
            return Err(ArbiterError::Ipc(err));
        }
    };
    for entry in probe_hub.catalog().values() {
        if let Err(err) = probe_tx.publish_catalog(
            entry.board_hash,
            entry.probe_id,
            entry.kind as u8,
            &entry.name,
        ) {
            log::warn!("probe catalog forward failed: {err}");
        }
    }
    if let Err(err) = probe_tx.publish_catalog_done() {
        log::warn!("probe catalog end failed: {err}");
    }
    // Multi-backend: stay non-startable until every peer arbiter reports Ready too.
    // Single-backend: no mesh, local Ready is enough.
    let mut mesh = match join_control_mesh(
        opts,
        peer_listeners.as_ref(),
        Duration::from_millis(topo.ready_timeout_ms),
    ) {
        Ok(mesh) => mesh,
        Err(err) => {
            stop_children(&mut children.0);
            logs.shutdown();
            return Err(err);
        }
    };
    // Cross-machine UART/analog edges: this arbiter becomes the iceoryx end that
    // the local Node does not hold and relays to the peer arbiter over data UDP.
    // Edges between two local boards are left to the Nodes.
    let mut edge_proxy = if mesh.is_some() {
        match open_edge_proxy(
            opts,
            topo,
            &local_ids,
            assignment.as_ref(),
            peer_listeners.as_ref(),
            &node,
        ) {
            Ok(proxy) => proxy,
            Err(err) => {
                stop_children(&mut children.0);
                logs.shutdown();
                return Err(err);
            }
        }
    } else {
        None
    };
    let mut ceiling = TimeCeiling::new(local_ids.iter().cloned(), topo.margin_ns);
    let mut time_link = TimeLink::new(opts);
    if time_link.is_some() {
        // Until the phase-2 multicast group is joined no peer has been heard:
        // hold Allowed at its floor instead of letting local boards run ahead.
        ceiling.set_remote(Some(0), true);
    }
    let mut allowed = ceiling.allowed_ns();
    let mut running = false;
    let mut until_cap: Option<u64> = None;
    let mut last_stop_reason: Option<HostStopReason> = None;
    let mut gdb_step: Option<GdbStepPhase> = None;
    let mut last_status = Instant::now();
    write_status(
        &host,
        running,
        ceiling.virtual_time_ns(),
        None,
        topo,
        &peers,
    )?;
    let mut reported_vt = ceiling.virtual_time_ns();
    let mut reported_running = running;
    let mut reported_stop_reason = last_stop_reason;

    let result = loop {
        crate::ipc::set_log_virtual_time_ns(ceiling.virtual_time_ns());
        let mut shutdown = false;
        loop {
            match host.try_cmd()? {
                Some(HostCommand::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Some(HostCommand::Start { until_ns }) => {
                    cancel_gdb_step(&mut gdb_step, mesh.as_mut());
                    apply_start(
                        &control,
                        &mut peers,
                        &ceiling,
                        &mut allowed,
                        &mut until_cap,
                        &mut last_stop_reason,
                        &mut running,
                        until_ns,
                    )?;
                }
                Some(HostCommand::Stop) if running => {
                    log::info!("host Stop");
                    cancel_gdb_step(&mut gdb_step, mesh.as_mut());
                    broadcast_cluster_stop(
                        &control,
                        &mut peers,
                        "server",
                        HostStopReason::ExternalStop,
                    )?;
                    running = false;
                    until_cap = None;
                    last_stop_reason = Some(HostStopReason::ExternalStop);
                }
                Some(HostCommand::Reset) => {
                    log::info!("host Reset");
                    cancel_gdb_step(&mut gdb_step, mesh.as_mut());
                    until_cap = None;
                    broadcast_cluster_stop(
                        &control,
                        &mut peers,
                        "server",
                        HostStopReason::ExternalStop,
                    )?;

                    control.publish(&ControlToNode::Reset)?;
                    allowed = allowed_after_reset(&mut ceiling, allowed);
                    begin_running(&control, &mut peers, allowed)?;
                    running = true;
                    last_stop_reason = None;
                    log::info!("host Reset: Start");
                }
                Some(HostCommand::Stop) => {}
                Some(HostCommand::Step { board_hash }) => {
                    apply_gdb_step(
                        &control,
                        &mut peers,
                        board_hash,
                        &mut running,
                        &mut until_cap,
                        &mut last_stop_reason,
                        &mut gdb_step,
                        mesh.as_mut(),
                    )?;
                }
                None => break,
            }
        }

        if shutdown {
            break Ok(());
        }

        forward_invokes(&node_actions, &server_actions)?;
        if let Err(err) = forward_probe_ctrl(&control, &probe_ctrl) {
            log::warn!("probe control forward: {err}");
        }
        if let Err(err) = forward_probes(&probe_hub, &probe_tx) {
            log::warn!("probe forward: {err}");
        }

        if let Some(link) = time_link.as_mut() {
            link.poll(&mut ceiling)?;
        }

        if let Some(proxy) = edge_proxy.as_mut() {
            if let Err(err) = proxy.pump(Instant::now()) {
                log::warn!("edge proxy: {err}");
            }
        }

        if let Some(mesh) = mesh.as_mut() {
            for notice in mesh.poll(Instant::now()) {
                apply_mesh_notice(
                    notice,
                    &control,
                    &mut peers,
                    &ceiling,
                    &mut allowed,
                    &mut until_cap,
                    &mut running,
                    &mut gdb_step,
                    &mut last_stop_reason,
                )?;
            }
        }

        let step = step_time_sync(
            &control,
            &mut peers,
            &mut ceiling,
            &mut allowed,
            &mut until_cap,
            &mut running,
            &mut gdb_step,
            &mut last_stop_reason,
        )?;
        finish_sync_step(
            step,
            mesh.as_mut(),
            &mut running,
            &mut until_cap,
            &mut gdb_step,
            &mut last_stop_reason,
        );

        let vt = ceiling.virtual_time_ns();
        crate::ipc::set_log_virtual_time_ns(vt);
        let published_reason = if running { None } else { last_stop_reason };
        if running != reported_running
            || vt != reported_vt
            || published_reason != reported_stop_reason
            || last_status.elapsed() >= Duration::from_millis(100)
        {
            write_status(&host, running, vt, published_reason, topo, &peers)?;
            reported_running = running;
            reported_vt = vt;
            reported_stop_reason = published_reason;
            last_status = Instant::now();
        }
    };

    logs.shutdown();
    stop_children(&mut children.0);
    result
}

fn forward_probe_ctrl(
    control: &ArbiterControl,
    ctrl: &ArbiterProbeCtrl,
) -> Result<(), ArbiterError> {
    while let Some(watch) = ctrl.try_watch()? {
        control.publish(&ControlToNode::ProbeWatch {
            board_hash: watch.board_hash,
            count: watch.count,
            probe_ids: watch.probe_ids,
        })?;
    }
    while let Some(query) = ctrl.try_query()? {
        ctrl.publish_query_to_nodes(query)?;
    }
    while let Some(reply) = ctrl.try_node_reply()? {
        ctrl.publish_reply_to_server(reply)?;
    }
    while let Some(csv) = ctrl.try_csv()? {
        ctrl.publish_csv_to_nodes(csv)?;
    }
    while let Some(chunk) = ctrl.try_node_csv()? {
        ctrl.publish_csv_to_server(chunk)?;
    }
    Ok(())
}

fn forward_probes(hub: &ArbiterProbeHub, tx: &ArbiterProbeForward) -> Result<(), ArbiterError> {
    for sample in hub.drain(256)? {
        let result = match sample {
            ProbeSample::Digital(sample) => tx.publish_sample(
                sample.probe_id,
                sample.virtual_time_ns,
                ProbeWireKind::Digital as u8,
                u16::from(sample.value),
            ),
            ProbeSample::Uart(sample) => tx.publish_sample(
                sample.probe_id,
                sample.virtual_time_ns,
                ProbeWireKind::Uart as u8,
                sample.value.data,
            ),
            ProbeSample::Analog(sample) => tx.publish_sample(
                sample.probe_id,
                sample.virtual_time_ns,
                ProbeWireKind::Analog as u8,
                analog_millivolts_u16(sample.value.0),
            ),
        };
        result?;
    }
    Ok(())
}

fn begin_running(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    allowed: u64,
) -> Result<(), ArbiterError> {
    broadcast_start(control, peers, allowed)
}

/// `until_ns` is already at or behind `now`: do not resume.
#[must_use]
fn should_stay_stopped(until_ns: Option<u64>, now_ns: u64) -> bool {
    until_ns.is_some_and(|until| until <= now_ns)
}

#[must_use]
fn reached_until(until_ns: Option<u64>, now_ns: u64) -> bool {
    until_ns.is_some_and(|until| now_ns >= until)
}

fn publish_allowed(control: &ArbiterControl, allowed: u64) -> Result<(), ArbiterError> {
    control.publish(&ControlToNode::Allowed {
        allowed_ns: allowed,
    })?;
    Ok(())
}

/// Apply a host Start: stay/stop if `until_ns` is not ahead of now, otherwise
/// broadcast Start with the run ceiling (Stopped resumes; Running retargets).
#[allow(clippy::too_many_arguments)]
fn apply_start(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    ceiling: &TimeCeiling,
    allowed: &mut u64,
    until_cap: &mut Option<u64>,
    last_stop_reason: &mut Option<HostStopReason>,
    running: &mut bool,
    until_ns: Option<u64>,
) -> Result<(), ArbiterError> {
    let now = ceiling.virtual_time_ns();
    if should_stay_stopped(until_ns, now) {
        if *running {
            log::info!("start until_ns={until_ns:?} <= now={now}; Host Stop");
            broadcast_cluster_stop(control, peers, "server", HostStopReason::Until)?;
            *running = false;
        } else {
            log::info!("start until_ns={until_ns:?} <= now={now}; stay stopped");
        }
        *until_cap = None;
        last_stop_reason.get_or_insert(HostStopReason::Until);
        return Ok(());
    }

    *until_cap = until_ns;
    *allowed = allowed_for_start(ceiling, *until_cap, *allowed);
    broadcast_start(control, peers, *allowed)?;
    *running = true;
    *last_stop_reason = None;
    Ok(())
}

/// Fold the outcome of one [`step_time_sync`] into the session and tell the
/// peer arbiters what they need to know.
fn finish_sync_step(
    step: SyncStep,
    mesh: Option<&mut ControlMesh>,
    running: &mut bool,
    until_cap: &mut Option<u64>,
    gdb_step: &mut Option<GdbStepPhase>,
    last_stop_reason: &mut Option<HostStopReason>,
) {
    match step {
        SyncStep::Continue => {}
        SyncStep::GdbInsn {
            seq,
            target_vt_ns,
            reason,
        } => {
            // Peers run to the stepped board's virtual time and stop there.
            if let (Some(seq), Some(mesh)) = (seq, mesh) {
                mesh.broadcast_step_catch_up(seq, target_vt_ns, reason);
            }
        }
        SyncStep::Stopped(reason) | SyncStep::NodeStopped(reason) => {
            if matches!(step, SyncStep::NodeStopped(_)) {
                // A local node's HostStop: tell the peer arbiters, which stop
                // their own nodes. Stops received from peers never go back out.
                if let Some(mesh) = mesh {
                    mesh.broadcast_host_stop(reason);
                }
            }
            *running = false;
            *until_cap = None;
            *gdb_step = None;
            *last_stop_reason = Some(reason);
        }
    }
}

/// ClusterStop if needed, then one insn on `board_hash`. Catch-up starts
/// when that node reports [`HostStopReason`].
///
/// With peer arbiters (`mesh`) they are told first (`StepBegin`) so that they
/// stop their nodes and wait; the catch-up message follows in
/// [`SyncStep::GdbInsn`]. A board this arbiter does not run is refused: the
/// frontend sends the step to the owning backend only.
#[allow(clippy::too_many_arguments)]
fn apply_gdb_step(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    board_hash: u64,
    running: &mut bool,
    until_cap: &mut Option<u64>,
    last_stop_reason: &mut Option<HostStopReason>,
    gdb_step: &mut Option<GdbStepPhase>,
    mesh: Option<&mut ControlMesh>,
) -> Result<(), ArbiterError> {
    let Some(board_id) = control.resolve_board(board_hash).map(str::to_owned) else {
        log::warn!("gdb Step: unknown board_hash={board_hash:#x}");
        return Ok(());
    };
    if !peers.contains_key(&board_id) {
        log::warn!("gdb Step: board '{board_id}' is not run by this arbiter; ignoring");
        return Ok(());
    }
    // A step still open towards the peers is replaced by this one.
    let seq = mesh.map(ControlMesh::broadcast_step_begin);
    if *running {
        log::info!("gdb Step: ClusterStop before insn on '{board_id}'");
        broadcast_cluster_stop(control, peers, "server", HostStopReason::ExternalStop)?;
        *running = false;
        *until_cap = None;
        *last_stop_reason = Some(HostStopReason::ExternalStop);
    }
    control.publish(&ControlToNode::GdbStep { board_hash })?;
    *gdb_step = Some(GdbStepPhase::WaitInsn { board_id, seq });
    Ok(())
}

/// Fold one control-mesh notice into this arbiter's session.
///
/// - `ClusterStop`: a node of a peer stopped; stop the local nodes when running
///   and drop a step that followed that peer. Never forwarded to other peers.
/// - `StepBegin`: the peer runs the stepped board; stop the local nodes and
///   wait. Nothing is executed on a local board.
/// - `StepCatchUp`: run the local nodes until the stepped board's virtual time
///   and stop there (at once when they are already there).
/// - `StepCancel`: the peer gave the step up; stop and forget it.
#[allow(clippy::too_many_arguments)]
fn apply_mesh_notice(
    notice: MeshNotice,
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    ceiling: &TimeCeiling,
    allowed: &mut u64,
    until_cap: &mut Option<u64>,
    running: &mut bool,
    gdb_step: &mut Option<GdbStepPhase>,
    last_stop_reason: &mut Option<HostStopReason>,
) -> Result<(), ArbiterError> {
    match notice {
        MeshNotice::PeerReady { .. } => {}
        MeshNotice::ClusterStop { origin, reason } => {
            if apply_peer_cluster_stop(control, peers, &origin, reason, *running)? {
                *running = false;
                *until_cap = None;
                *gdb_step = None;
                *last_stop_reason = Some(reason);
            } else if matches!(
                gdb_step,
                Some(GdbStepPhase::PeerWait { origin: o, .. }) if *o == origin
            ) {
                *gdb_step = None;
            }
        }
        MeshNotice::StepBegin { origin, seq } => {
            log::info!("gdb Step from peer arbiter '{origin}' (step {seq}): waiting");
            if *running {
                broadcast_cluster_stop(control, peers, &origin, HostStopReason::ExternalStop)?;
                *running = false;
                *until_cap = None;
                *last_stop_reason = Some(HostStopReason::ExternalStop);
            }
            *gdb_step = Some(GdbStepPhase::PeerWait { origin, seq });
        }
        MeshNotice::StepCatchUp {
            origin,
            seq,
            target_vt_ns,
            reason,
        } => {
            if !matches!(gdb_step, Some(phase @ GdbStepPhase::PeerWait { .. }) if phase.follows(&origin, seq))
            {
                log::debug!("gdb step {seq} of '{origin}': catch-up ignored (not waiting for it)");
                return Ok(());
            }
            let now = ceiling.virtual_time_ns();
            if reached_until(Some(target_vt_ns), now) {
                log::info!(
                    "gdb step {seq} of '{origin}': already at vt={now} >= {target_vt_ns}; stay stopped"
                );
                *gdb_step = None;
                *until_cap = None;
                *last_stop_reason = Some(reason);
                return Ok(());
            }
            log::info!("gdb step {seq} of '{origin}': catch-up to vt={target_vt_ns} ({reason})");
            *until_cap = Some(target_vt_ns);
            *allowed = allowed_for_start(ceiling, *until_cap, *allowed);
            broadcast_start(control, peers, *allowed)?;
            *running = true;
            *last_stop_reason = None;
            *gdb_step = Some(GdbStepPhase::PeerCatchUp {
                origin,
                seq,
                reason,
            });
        }
        MeshNotice::StepCancel { origin, seq } => {
            if !gdb_step
                .as_ref()
                .is_some_and(|phase| phase.follows(&origin, seq))
            {
                return Ok(());
            }
            log::info!("gdb step {seq} of '{origin}' cancelled");
            if *running {
                broadcast_cluster_stop(control, peers, &origin, HostStopReason::ExternalStop)?;
                *running = false;
                *last_stop_reason = Some(HostStopReason::ExternalStop);
            }
            *until_cap = None;
            *gdb_step = None;
        }
    }
    Ok(())
}

/// Allowed to put into a Start command, optionally hard-capped at `until_ns`.
///
/// While a peer arbiter is silent (`ceiling.is_frozen()`) Allowed updates are
/// held, so the last Allowed stays in force instead of being recomputed from
/// the (stale) remote view. The cap still applies.
#[must_use]
fn allowed_for_start(ceiling: &TimeCeiling, until_ns: Option<u64>, last_allowed: u64) -> u64 {
    if !ceiling.is_frozen() {
        return ceiling.allowed_ns_until(until_ns);
    }
    until_ns.map_or(last_allowed, |until| last_allowed.min(until))
}

/// Allowed to put into the Start that follows a host Reset.
///
/// Guest clocks may already equal the last Allowed. Treat that as `now` so the
/// republished ceiling is `now + margin` and boards can leave the old ceiling.
/// With peer arbiters the remote minimum is raised to `now` as well, otherwise a
/// stale remote value would keep Allowed below `now + margin`; the next
/// time-sync poll replaces it with the peers' real report.
///
/// While frozen (a peer arbiter is silent) Allowed must not move past what the
/// freeze holds: the last Allowed is returned unchanged. The local reports are
/// still raised, so Allowed becomes `now + margin` when the peers return.
fn allowed_after_reset(ceiling: &mut TimeCeiling, last_allowed: u64) -> u64 {
    let now = last_allowed.max(ceiling.virtual_time_ns());
    ceiling.advance_to(now);
    if ceiling.is_frozen() {
        return last_allowed;
    }
    if let Some(remote) = ceiling.remote_min_ns() {
        ceiling.set_remote(Some(remote.max(now)), false);
    }
    ceiling.allowed_ns()
}

fn broadcast_start(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    allowed_ns: u64,
) -> Result<(), ArbiterError> {
    control.publish(&ControlToNode::Start { allowed_ns })?;
    for state in peers.values_mut() {
        *state = state.on_start_broadcast();
    }
    log::info!("Start broadcast allowed_ns={allowed_ns}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn step_time_sync(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    ceiling: &mut TimeCeiling,
    allowed: &mut u64,
    until_ns: &mut Option<u64>,
    running: &mut bool,
    gdb_step: &mut Option<GdbStepPhase>,
    last_stop_reason: &mut Option<HostStopReason>,
) -> Result<SyncStep, ArbiterError> {
    let mut changed = false;
    let mut cluster_stop: Option<(String, HostStopReason)> = None;
    let mut gdb_insn: Option<(String, HostStopReason, Option<u64>)> = None;
    while let Some(msg) = control.try_recv()? {
        let from = msg.from_board();
        let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
            continue;
        };
        let Some(state) = peers.get_mut(&board_id) else {
            continue;
        };
        let (next, effect) = state.on_message(msg);
        match effect.clone() {
            Some(PeerEffect::TimeReport {
                from: _,
                virtual_time_ns,
            }) => {
                ceiling.report(&board_id, virtual_time_ns);
                changed = true;
            }
            Some(PeerEffect::HostStop { from: _, reason }) => match gdb_step.as_ref() {
                Some(GdbStepPhase::WaitInsn {
                    board_id: stepped,
                    seq,
                }) if stepped == &board_id => {
                    gdb_insn = Some((board_id.clone(), reason, *seq));
                }
                _ => cluster_stop = Some((board_id.clone(), reason)),
            },
            other => warn_peer(&board_id, other)?,
        }
        *state = next;
    }
    if let Some((source, reason)) = cluster_stop {
        log::info!("HostStop from '{source}' ({reason}); broadcasting ClusterStop");
        *gdb_step = None;
        broadcast_cluster_stop(control, peers, &source, reason)?;
        return Ok(SyncStep::NodeStopped(reason));
    }
    if let Some((source, reason, seq)) = gdb_insn {
        let target = ceiling.last_report(&source).unwrap_or(0);
        log::info!("gdb Step from '{source}' ({reason}) vt={target}; catch-up");
        *until_ns = Some(target);
        *allowed = allowed_for_start(ceiling, *until_ns, *allowed);
        broadcast_start(control, peers, *allowed)?;
        *running = true;
        *last_stop_reason = None;
        *gdb_step = Some(GdbStepPhase::CatchUp { reason, seq });
        return Ok(SyncStep::GdbInsn {
            seq,
            target_vt_ns: target,
            reason,
        });
    }
    // A silent peer arbiter freezes Allowed at its last value; the ceiling is
    // marked dirty again when the peer returns, so the update is not lost.
    let remote_changed = ceiling.take_remote_dirty();
    if (changed || remote_changed) && !ceiling.is_frozen() {
        let next_allowed = ceiling.allowed_ns_until(*until_ns);
        if next_allowed != *allowed {
            *allowed = next_allowed;
            publish_allowed(control, *allowed)?;
        }
    }
    if reached_until(*until_ns, ceiling.virtual_time_ns()) {
        let reason = match gdb_step {
            Some(
                GdbStepPhase::CatchUp { reason, .. } | GdbStepPhase::PeerCatchUp { reason, .. },
            ) => *reason,
            _ => HostStopReason::Until,
        };
        log::info!(
            "until_ns reached at vt={} ({reason}); broadcasting ClusterStop",
            ceiling.virtual_time_ns()
        );
        *gdb_step = None;
        broadcast_cluster_stop(control, peers, "server", reason)?;
        return Ok(SyncStep::Stopped(reason));
    }

    Ok(SyncStep::Continue)
}

/// Stop this arbiter's nodes because a peer arbiter reported a node HostStop.
///
/// Returns `true` when ClusterStop was published (the session was running).
/// The stop is not sent on to other peers: the origin already wrote it to every
/// peer, so forwarding would only make it circulate.
fn apply_peer_cluster_stop(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    origin: &str,
    reason: HostStopReason,
    running: bool,
) -> Result<bool, ArbiterError> {
    if !running {
        log::debug!("peer '{origin}' HostStop ({reason}) ignored: not running");
        return Ok(false);
    }
    log::info!("HostStop from peer arbiter '{origin}' ({reason}); broadcasting ClusterStop");
    broadcast_cluster_stop(control, peers, origin, reason)?;
    Ok(true)
}

/// Multi-backend Ready barrier over the control TCP mesh.
///
/// Waits (within `timeout`) for the phase-2 file, joins the mesh with the peers
/// it lists, announces that this arbiter's nodes are all Ready, and returns once
/// every peer arbiter has announced the same. `Ok(None)` without any TCP when
/// there is nothing to wait for: no peer identity, no phase-2 file option, or a
/// phase-2 request without peers (single-backend path).
///
/// The caller must have completed its local Ready barrier first.
fn join_control_mesh(
    opts: &ArbiterOptions,
    listeners: Option<&PeerListeners>,
    timeout: Duration,
) -> Result<Option<ControlMesh>, ArbiterError> {
    let (Some(listeners), Some(phase2_in), Some(peer)) = (
        listeners,
        opts.phase2_in.as_ref(),
        opts.peer_advertise.as_ref(),
    ) else {
        return Ok(None);
    };
    let deadline = Instant::now() + timeout;
    let req = loop {
        if let Some(req) = read_phase2_file(phase2_in)? {
            break req;
        }
        if Instant::now() >= deadline {
            return Err(ArbiterError::ReadyTimeout {
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                missing: "phase-2 request (peer list)".into(),
            });
        }
        thread::sleep(PHASE2_POLL);
    };
    let Some(mut mesh) = ControlMesh::open(
        &listeners.control_tcp,
        &req.cluster_key,
        &peer.backend_key,
        &req.peers,
        Instant::now(),
    )
    .map_err(|err| ArbiterError::Message(err.to_string()))?
    else {
        return Ok(None);
    };
    log::info!(
        "control mesh: {} peer(s); announcing local Ready",
        req.peers.len()
    );
    mesh.set_local_ready();
    await_peers_ready(&mut mesh, deadline, timeout)?;
    log::info!("control mesh: all peer arbiters Ready");
    Ok(Some(mesh))
}

/// Poll `mesh` until every peer is Ready or `deadline` passes.
fn await_peers_ready(
    mesh: &mut ControlMesh,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), ArbiterError> {
    while !mesh.peers_all_ready() {
        let now = Instant::now();
        if now >= deadline {
            let missing = mesh
                .peers_missing()
                .into_iter()
                .map(|backend| format!("peer:{backend}"))
                .collect::<Vec<_>>()
                .join(",");
            return Err(ArbiterError::ReadyTimeout {
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                missing,
            });
        }
        for notice in mesh.poll(now) {
            // No node has been started yet, so a peer cannot legitimately stop.
            log::warn!("control mesh: ignoring {notice:?} before the Ready barrier completed");
        }
        thread::sleep(ARBITER_POLL_IDLE);
    }
    Ok(())
}

/// How often a not-yet-linked arbiter looks for the phase-2 file.
const PHASE2_POLL: Duration = Duration::from_millis(20);

/// Multi-backend time sync: waits for the phase-2 file, then reports to and
/// follows the simulation's multicast group.
struct TimeLink {
    phase2_in: PathBuf,
    backend_key: String,
    advertise_ip: String,
    period: Duration,
    last_poll: Option<Instant>,
    mcast: Option<TimeMcast>,
}

impl TimeLink {
    /// `None` for the single-backend path (no phase-2 file or no peer identity).
    fn new(opts: &ArbiterOptions) -> Option<Self> {
        let (path, peer) = (opts.phase2_in.as_ref()?, opts.peer_advertise.as_ref()?);
        Some(Self {
            phase2_in: path.clone(),
            backend_key: peer.backend_key.clone(),
            advertise_ip: peer.advertise_ip.clone(),
            period: Duration::from_millis(opts.time_sync_period_ms.max(1)),
            last_poll: None,
            mcast: None,
        })
    }

    /// Join the group once the phase-2 file exists; afterwards exchange reports
    /// and feed the remote view into `ceiling`.
    ///
    /// Packets are stamped with and filtered by `Phase2ConnectRequest::cluster_key`
    /// (shared by every arbiter of the simulation), not by
    /// `ArbiterOptions::cluster_key`, which is this member's own iceoryx id.
    fn poll(&mut self, ceiling: &mut TimeCeiling) -> Result<(), ArbiterError> {
        let now = Instant::now();
        if self.mcast.is_none() {
            if self
                .last_poll
                .is_some_and(|last| now.duration_since(last) < PHASE2_POLL)
            {
                return Ok(());
            }
            self.last_poll = Some(now);
            let Some(req) = read_phase2_file(&self.phase2_in)? else {
                return Ok(());
            };
            let mcast = TimeMcast::open(
                &req.cluster_key,
                &self.backend_key,
                &self.advertise_ip,
                &req.multicast,
                &req.peers,
                self.period,
                now,
            )
            .map_err(|err| ArbiterError::Message(err.to_string()))?;
            log::info!(
                "time sync multicast {}:{} joined ({} peer(s), period={}ms)",
                req.multicast.ip,
                req.multicast.port,
                req.peers.len(),
                self.period.as_millis()
            );
            self.mcast = Some(mcast);
        }
        if let Some(mcast) = self.mcast.as_mut() {
            let remote = mcast.poll(ceiling.virtual_time_ns(), now);
            if remote.frozen != ceiling.is_frozen() {
                log::warn!(
                    "time sync {}: Allowed updates {}",
                    if remote.frozen {
                        "peer silent for over 1s"
                    } else {
                        "peers are back"
                    },
                    if remote.frozen { "frozen" } else { "resumed" }
                );
            }
            ceiling.set_remote(Some(remote.remote_min_ns), remote.frozen);
        }
        Ok(())
    }
}

/// `Ok(None)` when no assignment file was configured.
fn read_assignment_file(path: Option<&Path>) -> Result<Option<AssignmentTable>, ArbiterError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| ArbiterError::Message(format!("invalid assignment file: {err}")))
}

/// Build the cross-machine edge proxy once the phase-2 peer list is known.
///
/// `Ok(None)` when there is nothing to proxy: no peer sockets / phase-2 file
/// (single-backend path), no placement, or every edge stays on this machine.
fn open_edge_proxy(
    opts: &ArbiterOptions,
    topo: &LogicalTopology,
    local_ids: &[String],
    assignment: Option<&AssignmentTable>,
    listeners: Option<&PeerListeners>,
    node: &iceoryx2::prelude::Node<iceoryx2::prelude::ipc::Service>,
) -> Result<Option<EdgeProxy>, ArbiterError> {
    let (Some(listeners), Some(phase2_in), Some(peer)) = (
        listeners,
        opts.phase2_in.as_ref(),
        opts.peer_advertise.as_ref(),
    ) else {
        return Ok(None);
    };
    let Some(assignment) = assignment else {
        log::warn!("no assignment file: cross-machine edges are not proxied");
        return Ok(None);
    };
    let Some(req) = read_phase2_file(phase2_in)? else {
        return Ok(None);
    };
    let plan = plan_cross_edges(&topo.edges, local_ids, assignment)?;
    if plan.is_empty() {
        log::info!("edge proxy: no cross-machine edges");
        return Ok(None);
    }
    // Every peer that owns a remote board of a cross edge must be a phase-2 peer.
    for cross in plan.egress.iter().chain(&plan.ingress) {
        if !req.peers.iter().any(|p| p.backend == cross.peer) {
            return Err(ArbiterError::Message(format!(
                "edge {}: backend {} is not in the phase-2 peer list",
                cross.id, cross.peer
            )));
        }
    }
    let socket = listeners.data_udp.try_clone()?;
    let link = DataLink::new(
        socket,
        &req.cluster_key,
        &peer.backend_key,
        &req.peers,
        opts.data_udp_window,
    )
    .map_err(|err| ArbiterError::Message(err.to_string()))?;
    log::info!(
        "edge proxy: {} egress / {} ingress cross-machine edge(s), window={}",
        plan.egress.len(),
        plan.ingress.len(),
        opts.data_udp_window
    );
    EdgeProxy::open(node, &opts.cluster_key, plan, link).map(Some)
}

/// `None` while the file does not exist yet.
fn read_phase2_file(path: &Path) -> Result<Option<Phase2ConnectRequest>, ArbiterError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| ArbiterError::Message(format!("invalid phase-2 file: {err}"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ArbiterError::Io(err)),
    }
}

fn analog_millivolts_u16(uv: i64) -> u16 {
    uv.saturating_div(1000).clamp(0, i64::from(u16::MAX)) as u16
}

fn binary_for_kind<'a>(opts: &'a ArbiterOptions, kind: &str) -> Result<&'a PathBuf, ArbiterError> {
    match kind {
        "rl78" => Ok(&opts.node_bin),
        "plant" => Ok(&opts.plant_bin),
        other => Err(ArbiterError::Message(format!(
            "unknown board kind `{other}` (expected rl78 or plant)"
        ))),
    }
}

fn ensure_node_bins(opts: &ArbiterOptions, topo: &LogicalTopology) -> Result<(), ArbiterError> {
    let local = select_local_boards(topo, opts.local_board_ids.as_deref())?;
    let mut need_rl78 = false;
    let mut need_plant = false;
    for board in local {
        match board.kind.as_str() {
            "rl78" => need_rl78 = true,
            "plant" => need_plant = true,
            other => {
                return Err(ArbiterError::Message(format!(
                    "unknown board kind `{other}` (expected rl78 or plant)"
                )));
            }
        }
    }
    if need_rl78 && !opts.node_bin.is_file() {
        return Err(ArbiterError::Message(format!(
            "board binary not found at {} (build rl78-minimal-board or pass --node-bin)",
            opts.node_bin.display()
        )));
    }
    if need_plant && !opts.plant_bin.is_file() {
        return Err(ArbiterError::Message(format!(
            "plant binary not found at {} (build minimal-plant or pass --plant-bin)",
            opts.plant_bin.display()
        )));
    }
    Ok(())
}

/// Node processes started by the arbiter; killed and reaped on drop.
struct NodeChildren(Vec<Child>);

impl Drop for NodeChildren {
    fn drop(&mut self) {
        stop_children(&mut self.0);
    }
}

fn spawn_nodes(
    opts: &ArbiterOptions,
    local_boards: &[&crate::topology::BoardSpec],
) -> Result<Vec<Child>, ArbiterError> {
    // If a later spawn fails, the guard stops the nodes already started.
    let mut children = NodeChildren(Vec::new());
    for board in local_boards {
        let bin = binary_for_kind(opts, &board.kind)?;
        let child = Command::new(bin)
            .arg("--board-id")
            .arg(&board.id)
            .arg("--cluster-key")
            .arg(&opts.cluster_key)
            .arg("--iox-root")
            .arg(&opts.iox_root)
            .arg("--topology")
            .arg(&opts.topology_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        children.0.push(child);
    }
    Ok(std::mem::take(&mut children.0))
}

fn stop_children(children: &mut Vec<Child>) {
    for child in children.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    children.clear();
}

fn write_status(
    host: &ArbiterHostPort,
    running: bool,
    virtual_time_ns: u64,
    stop_reason: Option<HostStopReason>,
    topo: &LogicalTopology,
    peers: &HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    let nodes = local_session_nodes(topo, peers);
    let status = SessionStatus {
        state: if running { "running" } else { "stopped" }.to_string(),
        virtual_time_ns,
        stop_reason: if running {
            None
        } else {
            stop_reason.map(|reason| reason.as_str().to_string())
        },
        nodes,
    };
    host.publish_status(&status)?;
    Ok(())
}

/// Nodes this arbiter reports in its status: only the boards it spawned.
///
/// `peers` holds exactly the local board ids (see `await_ready`), so boards of
/// `topo` that run under another arbiter are skipped. Listing them would make
/// the aggregating backend see them as `unknown` and duplicated across members.
fn local_session_nodes(
    topo: &LogicalTopology,
    peers: &HashMap<String, PeerState>,
) -> Vec<SessionNode> {
    topo.boards
        .iter()
        .filter_map(|board| {
            peers.get(&board.id).map(|state| SessionNode {
                id: board.id.clone(),
                kind: board.kind.clone(),
                state: peer_state_name(state).to_string(),
            })
        })
        .collect()
}

fn peer_state_name(state: &PeerState) -> &'static str {
    match state {
        PeerState::AwaitingReady => "awaiting_ready",
        PeerState::Ready => "ready",
        PeerState::Running => "running",
        PeerState::Stopped => "stopped",
    }
}

fn broadcast_cluster_stop(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    _source: &str,
    reason: HostStopReason,
) -> Result<(), ArbiterError> {
    control.publish(&ControlToNode::ClusterStop { reason })?;
    for state in peers.values_mut() {
        *state = PeerState::Stopped;
    }
    Ok(())
}

fn ready_timeout(topo: &LogicalTopology, peers: &HashMap<String, PeerState>) -> ArbiterError {
    let missing = peers
        .iter()
        .filter(|(_, p)| **p != PeerState::Ready)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>()
        .join(",");
    ArbiterError::ReadyTimeout {
        timeout_ms: topo.ready_timeout_ms,
        missing,
    }
}

fn warn_peer(board_id: &str, effect: Option<PeerEffect>) -> Result<(), ArbiterError> {
    match effect {
        Some(PeerEffect::Warn(msg)) => {
            log::warn!("warning [{board_id}]: {msg}");
        }
        Some(PeerEffect::TimeReport { .. }) | Some(PeerEffect::HostStop { .. }) | None => {}
    }

    Ok(())
}

/// Convenience for the binary: topology path only.
pub fn run_arbiter_from_path(topology_path: &Path) -> Result<(), ArbiterError> {
    let opts = ArbiterOptions::from_topology_path(topology_path.to_path_buf())?;
    run_arbiter(&opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{
        ArbiterControl, NodeControl, board_hash_table, board_id_hash, create_node, isolated_config,
    };
    use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, PayloadKind};
    use serial_test::serial;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn link_opts(dir: &Path, me: &str) -> ArbiterOptions {
        ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: PathBuf::from("/tmp/rl78"),
            plant_bin: PathBuf::from("/tmp/plant"),
            // Member-local id (iceoryx); differs between the linked arbiters.
            cluster_key: format!("sim-link-{me}"),
            iox_root: PathBuf::from("/tmp/iox"),
            local_board_ids: None,
            peer_advertise: Some(PeerAdvertise {
                advertise_ip: "127.0.0.1".into(),
                backend_key: me.into(),
            }),
            endpoints_out: None,
            phase2_in: Some(dir.join("phase2.json")),
            time_sync_period_ms: 5,
            assignment_in: None,
            data_udp_window: 32,
        }
    }

    #[test]
    fn time_link_is_absent_without_phase2_file_option() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = link_opts(dir.path(), "a:1");
        assert!(TimeLink::new(&opts).is_some());
        opts.phase2_in = None;
        assert!(TimeLink::new(&opts).is_none());
    }

    #[test]
    fn time_link_waits_for_phase2_file_then_follows_peer() {
        use crate::orchestrate::{ArbiterEndpoints, MulticastGroup};
        let dir = tempfile::tempdir().unwrap();
        let ep = |b: &str| ArbiterEndpoints {
            backend: b.into(),
            advertise_ip: "127.0.0.1".into(),
            control_tcp_port: 1,
            data_udp_port: 2,
        };
        let mut a = TimeLink::new(&link_opts(dir.path(), "a:1")).unwrap();
        let mut ceil_a = TimeCeiling::new(["x".into()], 1000);
        ceil_a.set_remote(Some(0), true);
        ceil_a.report("x", 400);
        a.poll(&mut ceil_a).unwrap();
        // No phase-2 file yet: still frozen at the floor.
        assert!(ceil_a.is_frozen());
        assert_eq!(ceil_a.allowed_ns(), 1000);

        let req = Phase2ConnectRequest {
            id: "sim-link-0".into(),
            // Shared by both members, unlike their own `ArbiterOptions::cluster_key`.
            cluster_key: "sim-link".into(),
            peers: vec![ep("b:1")],
            multicast: MulticastGroup {
                ip: "239.255.77.3".into(),
                port: 47003,
            },
        };
        std::fs::write(
            dir.path().join("phase2.json"),
            serde_json::to_vec(&req).unwrap(),
        )
        .unwrap();
        let mut b = TimeLink::new(&link_opts(dir.path(), "b:1")).unwrap();
        let mut ceil_b = TimeCeiling::new(["y".into()], 1000);
        ceil_b.set_remote(Some(0), true);
        ceil_b.report("y", 900);
        // Peer `b` lists `a` as its peer.
        let req_b = Phase2ConnectRequest {
            id: "sim-link-1".into(),
            peers: vec![ep("a:1")],
            ..req.clone()
        };
        let b_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            b_dir.path().join("phase2.json"),
            serde_json::to_vec(&req_b).unwrap(),
        )
        .unwrap();
        b.phase2_in = b_dir.path().join("phase2.json");

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !(ceil_a.allowed_ns() == 1400 && ceil_b.allowed_ns() == 1400)
        {
            if a.poll(&mut ceil_a).is_err() || b.poll(&mut ceil_b).is_err() {
                eprintln!("skipping: multicast join on loopback unavailable");
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        if ceil_a.allowed_ns() == 1000 {
            eprintln!("skipping: no multicast delivery on loopback in this environment");
            return;
        }
        // min(local 400, remote 900) + margin 1000 on `a`; `b` sees min(900, 400) + 1000.
        assert_eq!(ceil_a.allowed_ns(), 1400);
        assert_eq!(ceil_b.allowed_ns(), 1400);
        assert!(!ceil_a.is_frozen());
    }

    #[test]
    fn start_from_stopped_uses_run_ceiling_not_now() {
        let mut ceiling = TimeCeiling::new(["a".into()], 1000);
        ceiling.report("a", 500);
        assert_eq!(ceiling.virtual_time_ns(), 500);
        assert_eq!(ceiling.allowed_ns_until(None), 1500);
        assert_eq!(ceiling.allowed_ns_until(Some(800)), 800);
        assert_eq!(ceiling.allowed_ns_until(Some(10_000)), 1500);
    }

    #[test]
    fn stay_stopped_when_until_is_not_ahead_of_now() {
        assert!(should_stay_stopped(Some(0), 0));
        assert!(should_stay_stopped(Some(1000), 5000));
        assert!(!should_stay_stopped(Some(1000), 999));
        assert!(!should_stay_stopped(None, 0));
        assert!(!should_stay_stopped(None, 5000));
    }

    #[test]
    fn reached_until_is_inclusive() {
        assert!(reached_until(Some(1000), 1000));
        assert!(reached_until(Some(1000), 1001));
        assert!(!reached_until(Some(1000), 999));
        assert!(!reached_until(None, 10_000));
    }

    #[test]
    fn time_ceiling_ignores_boards_outside_local_set() {
        let topo = LogicalTopology {
            boards: [("a", "rl78"), ("b", "plant")]
                .into_iter()
                .map(|(id, kind)| crate::topology::BoardSpec {
                    id: id.into(),
                    kind: kind.into(),
                    elf: None,
                    endpoints: vec![],
                })
                .collect(),
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 1,
            ready_timeout_ms: 1000,
        };
        let local = select_local_boards(&topo, Some(&["b".into()])).unwrap();
        let local_ids: Vec<String> = local.iter().map(|b| b.id.clone()).collect();
        let mut ceiling = TimeCeiling::new(local_ids.iter().cloned(), topo.margin_ns);
        ceiling.report("b", 400);
        assert_eq!(ceiling.virtual_time_ns(), 400);
        let before = ceiling.allowed_ns();
        ceiling.report("a", 9999);
        assert_eq!(ceiling.virtual_time_ns(), 400);
        assert_eq!(ceiling.allowed_ns(), before);
    }

    #[test]
    fn local_session_nodes_lists_only_local_boards() {
        // Topology has boards "a" and "b"; this arbiter only runs "b".
        let topo = two_board_topo(1000);
        let mut peers = HashMap::new();
        peers.insert("b".to_string(), PeerState::Ready);

        let nodes = local_session_nodes(&topo, &peers);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, "b");
        assert_eq!(nodes[0].kind, "rl78");
        assert_eq!(nodes[0].state, "ready");
        assert!(nodes.iter().all(|n| n.state != "unknown"));

        // All boards local: both listed in topology order.
        peers.insert("a".to_string(), PeerState::AwaitingReady);
        let nodes = local_session_nodes(&topo, &peers);
        let listed: Vec<_> = nodes
            .iter()
            .map(|n| (n.id.as_str(), n.state.as_str()))
            .collect();
        assert_eq!(listed, [("a", "awaiting_ready"), ("b", "ready")]);
    }

    #[test]
    fn select_local_boards_defaults_to_all() {
        let topo = LogicalTopology {
            boards: vec![
                crate::topology::BoardSpec {
                    id: "a".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![],
                },
                crate::topology::BoardSpec {
                    id: "b".into(),
                    kind: "plant".into(),
                    elf: None,
                    endpoints: vec![],
                },
            ],
            edges: vec![],
            margin_ns: 1,
            headroom_threshold_ns: 1,
            ready_timeout_ms: 1000,
        };
        let all = select_local_boards(&topo, None).unwrap();
        assert_eq!(all.len(), 2);
        let one = select_local_boards(&topo, Some(&["b".into()])).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "b");
        assert!(select_local_boards(&topo, Some(&["missing".into()])).is_err());
        assert!(select_local_boards(&topo, Some(&[])).is_err());
        assert!(select_local_boards(&topo, Some(&["a".into(), "a".into()])).is_err());
    }

    #[test]
    fn binary_for_kind_selects_rl78_and_plant() {
        let opts = ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: PathBuf::from("/tmp/rl78-minimal-board"),
            plant_bin: PathBuf::from("/tmp/minimal-plant"),
            cluster_key: "k".into(),
            iox_root: PathBuf::from("/tmp/iox"),
            local_board_ids: None,
            peer_advertise: None,
            endpoints_out: None,
            phase2_in: None,
            time_sync_period_ms: 100,
            assignment_in: None,
            data_udp_window: 32,
        };
        assert_eq!(binary_for_kind(&opts, "rl78").unwrap(), &opts.node_bin);
        assert_eq!(binary_for_kind(&opts, "plant").unwrap(), &opts.plant_bin);
        assert!(binary_for_kind(&opts, "avr").is_err());
    }

    #[test]
    fn reset_raises_allowed_to_now_plus_margin() {
        let mut ceiling = TimeCeiling::new(["a".into(), "b".into()], 1000);
        ceiling.report("a", 400);
        ceiling.report("b", 500);
        // Last published ceiling; guests may already sit here.
        let last_allowed = 1500;
        assert_eq!(allowed_after_reset(&mut ceiling, last_allowed), 2500);
        assert_eq!(ceiling.virtual_time_ns(), 1500);
        assert_eq!(ceiling.allowed_ns(), 2500);
    }

    #[test]
    fn start_while_frozen_keeps_last_allowed_but_respects_until() {
        let mut ceiling = TimeCeiling::new(["a".into()], 1000);
        ceiling.report("a", 500);
        ceiling.set_remote(Some(0), true);
        // Natural ceiling would be 0 + 1000, but the freeze holds the last value.
        assert_eq!(allowed_for_start(&ceiling, None, 1500), 1500);
        assert_eq!(allowed_for_start(&ceiling, Some(800), 1500), 800);
        assert_eq!(allowed_for_start(&ceiling, Some(9000), 1500), 1500);
        // Once peers are back the ceiling is recomputed as usual.
        ceiling.set_remote(Some(700), false);
        assert_eq!(allowed_for_start(&ceiling, None, 1500), 1500);
        ceiling.set_remote(Some(300), false);
        assert_eq!(allowed_for_start(&ceiling, None, 1500), 1300);
        assert_eq!(allowed_for_start(&ceiling, Some(900), 1500), 900);
    }

    #[test]
    fn reset_while_frozen_keeps_last_allowed() {
        let mut ceiling = TimeCeiling::new(["a".into()], 1000);
        ceiling.report("a", 500);
        ceiling.set_remote(Some(200), true);
        assert_eq!(allowed_after_reset(&mut ceiling, 1500), 1500);
        assert!(ceiling.is_frozen());
        // Local reports were raised, so resuming gives now + margin.
        assert_eq!(ceiling.virtual_time_ns(), 1500);
        ceiling.set_remote(Some(1500), false);
        assert_eq!(ceiling.allowed_ns(), 2500);
    }

    #[test]
    fn reset_with_peers_ignores_stale_remote_minimum() {
        let mut ceiling = TimeCeiling::new(["a".into()], 1000);
        ceiling.report("a", 500);
        ceiling.set_remote(Some(200), false);
        assert_eq!(allowed_after_reset(&mut ceiling, 1500), 2500);
        assert_eq!(ceiling.remote_min_ns(), Some(1500));
        // Without peers there is no remote minimum to touch.
        let mut solo = TimeCeiling::new(["a".into()], 1000);
        solo.report("a", 500);
        assert_eq!(allowed_after_reset(&mut solo, 1500), 2500);
        assert_eq!(solo.remote_min_ns(), None);
    }

    fn two_board_topo(ready_timeout_ms: u64) -> LogicalTopology {
        LogicalTopology {
            boards: vec![
                BoardSpec {
                    id: "a".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![
                        EndpointSpec {
                            name: "uart0_tx".into(),
                            direction: EndpointDirection::Out,
                            payload: PayloadKind::Uart,
                        },
                        EndpointSpec {
                            name: "uart0_rx".into(),
                            direction: EndpointDirection::In,
                            payload: PayloadKind::Uart,
                        },
                    ],
                },
                BoardSpec {
                    id: "b".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![
                        EndpointSpec {
                            name: "uart0_tx".into(),
                            direction: EndpointDirection::Out,
                            payload: PayloadKind::Uart,
                        },
                        EndpointSpec {
                            name: "uart0_rx".into(),
                            direction: EndpointDirection::In,
                            payload: PayloadKind::Uart,
                        },
                    ],
                },
            ],
            edges: vec![
                DirectedEdge {
                    from_board: "a".into(),
                    from_endpoint: "uart0_tx".into(),
                    to_board: "b".into(),
                    to_endpoint: "uart0_rx".into(),
                    payload: PayloadKind::Uart,
                    uart_ring_len: None,
                },
                DirectedEdge {
                    from_board: "b".into(),
                    from_endpoint: "uart0_tx".into(),
                    to_board: "a".into(),
                    to_endpoint: "uart0_rx".into(),
                    payload: PayloadKind::Uart,
                    uart_ring_len: None,
                },
            ],
            margin_ns: 1000,
            headroom_threshold_ns: 100,
            ready_timeout_ms,
        }
    }

    fn fake_node_loop(
        cluster_key: String,
        iox_root: PathBuf,
        board_id: String,
        send_ready: bool,
        inject_host_stop: bool,
        saw_cluster_stop: Option<Arc<Mutex<bool>>>,
    ) {
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, &format!("fake-{board_id}-{cluster_key}")).unwrap();
        let control = NodeControl::open(&node, &cluster_key, boards).unwrap();
        // UART attach is covered by dedicated tests; skip here to isolate control timing.

        let my_hash = board_id_hash(&board_id);
        let end = Instant::now() + Duration::from_secs(3);
        let mut got_start = false;
        while Instant::now() < end {
            while let Ok(Some(msg)) = control.try_recv() {
                match msg {
                    ControlToNode::StartupRecord { .. } => {
                        if send_ready {
                            let _ = control.publish(&ControlToArbiter::Ready { from: my_hash });
                            let _ = control.publish(&ControlToArbiter::Ready { from: my_hash });
                        }
                    }
                    ControlToNode::Start { .. } => {
                        got_start = true;
                        if inject_host_stop {
                            let _ = control.publish(&ControlToArbiter::HostStop {
                                from: my_hash,
                                reason: HostStopReason::Breakpoint,
                            });
                        }
                    }
                    ControlToNode::ClusterStop { reason } => {
                        assert_eq!(reason, HostStopReason::Breakpoint);
                        if let Some(flag) = &saw_cluster_stop {
                            *flag.lock().unwrap() = true;
                        }
                        return;
                    }
                    _ => {}
                }
            }
            if got_start && !inject_host_stop && saw_cluster_stop.is_none() {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[serial]
    fn ready_barrier_succeeds_with_fake_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-ready-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-ready").unwrap();
        let _logs =
            ArbiterLogBus::create(&iox_root, &cluster_key, topo.boards.len(), boards.clone())
                .unwrap();
        let control =
            ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards).unwrap();

        let mut joins = Vec::new();
        for board in &topo.boards {
            let ck = cluster_key.clone();
            let root = iox_root.clone();
            let id = board.id.clone();
            joins.push(thread::spawn(move || {
                fake_node_loop(ck, root, id, true, false, None);
            }));
        }

        ready_barrier_and_start(&topo, &control).unwrap();
        for j in joins {
            j.join().unwrap();
        }
    }

    #[test]
    #[serial]
    fn ready_barrier_times_out_when_node_silent() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-timeout-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(200);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-timeout").unwrap();
        let _logs =
            ArbiterLogBus::create(&iox_root, &cluster_key, topo.boards.len(), boards.clone())
                .unwrap();
        let control =
            ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards).unwrap();

        let mut joins = Vec::new();
        for (i, board) in topo.boards.iter().enumerate() {
            let ck = cluster_key.clone();
            let root = iox_root.clone();
            let id = board.id.clone();
            let send = i == 0;
            joins.push(thread::spawn(move || {
                fake_node_loop(ck, root, id, send, false, None);
            }));
        }

        let err = ready_barrier_and_start(&topo, &control).unwrap_err();
        assert!(matches!(err, ArbiterError::ReadyTimeout { .. }));
        for j in joins {
            let _ = j.join();
        }
    }

    #[test]
    #[serial]
    fn host_stop_broadcasts_cluster_stop() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-hoststop-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-hoststop").unwrap();
        let _logs =
            ArbiterLogBus::create(&iox_root, &cluster_key, topo.boards.len(), boards.clone())
                .unwrap();
        let control =
            ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards).unwrap();

        let saw_cluster_stop = Arc::new(Mutex::new(false));
        let mut joins = Vec::new();
        for board in &topo.boards {
            let ck = cluster_key.clone();
            let root = iox_root.clone();
            let id = board.id.clone();
            let inject = id == "a";
            let flag = if inject {
                None
            } else {
                Some(Arc::clone(&saw_cluster_stop))
            };
            joins.push(thread::spawn(move || {
                fake_node_loop(ck, root, id, true, inject, flag);
            }));
        }

        ready_barrier_and_start(&topo, &control).unwrap();
        for j in joins {
            j.join().unwrap();
        }
        assert!(
            *saw_cluster_stop.lock().unwrap(),
            "peer should receive ClusterStop after HostStop"
        );
    }

    #[test]
    fn duplicate_service_create_fails() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-dup-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let boards = board_hash_table(["a", "b"]).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node1 = create_node(&config, "arb-dup-1").unwrap();
        let _c1 = ArbiterControl::create(&node1, &cluster_key, 2, boards.clone()).unwrap();
        let node2 = create_node(&config, "arb-dup-2").unwrap();
        let err = ArbiterControl::create(&node2, &cluster_key, 2, boards);
        assert!(
            err.is_err(),
            "expected duplicate create to fail, got Ok(...)"
        );
        let err = err.err().unwrap();
        assert!(
            err.to_string().contains("create"),
            "expected create failure, got {err}"
        );
    }

    fn mesh_endpoint(listeners: &PeerListeners) -> crate::orchestrate::ArbiterEndpoints {
        listeners.endpoints().clone()
    }

    fn write_phase2(path: &Path, peers: Vec<crate::orchestrate::ArbiterEndpoints>) {
        let req = Phase2ConnectRequest {
            id: "sim-mesh-0".into(),
            cluster_key: "sim-mesh".into(),
            peers,
            multicast: crate::orchestrate::MulticastGroup {
                ip: "239.255.77.4".into(),
                port: 47004,
            },
        };
        std::fs::write(path, serde_json::to_vec(&req).unwrap()).unwrap();
    }

    #[test]
    fn control_mesh_is_absent_on_the_single_backend_path() {
        let dir = tempfile::tempdir().unwrap();
        let listeners = PeerListeners::bind("a:1".into(), "127.0.0.1").unwrap();
        let t = Duration::from_millis(100);

        // No peer identity / no phase-2 option: nothing to wait for, no file needed.
        let mut opts = link_opts(dir.path(), "a:1");
        opts.peer_advertise = None;
        assert!(
            join_control_mesh(&opts, Some(&listeners), t)
                .unwrap()
                .is_none()
        );
        let mut opts = link_opts(dir.path(), "a:1");
        opts.phase2_in = None;
        assert!(
            join_control_mesh(&opts, Some(&listeners), t)
                .unwrap()
                .is_none()
        );
        assert!(join_control_mesh(&opts, None, t).unwrap().is_none());

        // Phase-2 request without peers: no mesh and no connection to the listener.
        let opts = link_opts(dir.path(), "a:1");
        write_phase2(&dir.path().join("phase2.json"), vec![]);
        assert!(
            join_control_mesh(&opts, Some(&listeners), t)
                .unwrap()
                .is_none()
        );
        assert!(listeners.control_tcp.accept().is_err());
    }

    #[test]
    fn control_mesh_times_out_without_phase2_file_or_peer_ready() {
        let dir = tempfile::tempdir().unwrap();
        let listeners = PeerListeners::bind("a:1".into(), "127.0.0.1").unwrap();
        let opts = link_opts(dir.path(), "a:1");

        let err = join_control_mesh(&opts, Some(&listeners), Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(matches!(err, ArbiterError::ReadyTimeout { .. }), "{err}");

        // Peer `b:1` listens but never reports Ready.
        let other = PeerListeners::bind("b:1".into(), "127.0.0.1").unwrap();
        write_phase2(&dir.path().join("phase2.json"), vec![mesh_endpoint(&other)]);
        let err = join_control_mesh(&opts, Some(&listeners), Duration::from_millis(300))
            .err()
            .unwrap();
        match err {
            ArbiterError::ReadyTimeout { missing, .. } => assert_eq!(missing, "peer:b:1"),
            other => panic!("expected ReadyTimeout, got {other}"),
        }
    }

    #[test]
    fn control_mesh_barrier_waits_for_peer_and_propagates_host_stop() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let la = PeerListeners::bind("a:1".into(), "127.0.0.1").unwrap();
        let lb = PeerListeners::bind("b:1".into(), "127.0.0.1").unwrap();
        write_phase2(&dir_a.path().join("phase2.json"), vec![mesh_endpoint(&lb)]);
        write_phase2(&dir_b.path().join("phase2.json"), vec![mesh_endpoint(&la)]);
        let opts_a = link_opts(dir_a.path(), "a:1");
        let opts_b = link_opts(dir_b.path(), "b:1");

        let started = Instant::now();
        let b_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let b_stop_rx = Arc::clone(&b_stop);
        let b_thread = thread::spawn(move || {
            // `b` becomes Ready late: `a` must not pass its barrier before.
            thread::sleep(Duration::from_millis(300));
            let mut mesh = join_control_mesh(&opts_b, Some(&lb), Duration::from_secs(5))
                .unwrap()
                .unwrap();
            // The arbiter main loop keeps polling the mesh after the barrier;
            // that is what finishes writing `b`'s own Ready to `a`.
            while !b_stop_rx.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = mesh.poll(Instant::now());
                thread::sleep(Duration::from_millis(1));
            }
            (mesh, lb)
        });
        let mut mesh_a = join_control_mesh(&opts_a, Some(&la), Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "a passed the barrier before b announced Ready"
        );
        assert!(mesh_a.peers_all_ready());
        b_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let (mut mesh_b, _lb) = b_thread.join().unwrap();

        // A node HostStop on `a` reaches `b` exactly once.
        mesh_a.broadcast_host_stop(HostStopReason::Breakpoint);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stops = Vec::new();
        while Instant::now() < deadline && stops.is_empty() {
            stops.extend(mesh_a.poll(Instant::now()));
            stops.extend(mesh_b.poll(Instant::now()));
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            stops,
            [MeshNotice::ClusterStop {
                origin: "a:1".into(),
                reason: HostStopReason::Breakpoint,
            }]
        );
    }

    #[test]
    #[serial]
    fn peer_host_stop_publishes_cluster_stop_to_local_nodes_only_when_running() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-peerstop-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-peerstop").unwrap();
        let _logs =
            ArbiterLogBus::create(&iox_root, &cluster_key, topo.boards.len(), boards.clone())
                .unwrap();
        let control =
            ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards).unwrap();

        let flags: Vec<Arc<Mutex<bool>>> = topo
            .boards
            .iter()
            .map(|_| Arc::new(Mutex::new(false)))
            .collect();
        let mut joins = Vec::new();
        for (board, flag) in topo.boards.iter().zip(&flags) {
            let ck = cluster_key.clone();
            let root = iox_root.clone();
            let id = board.id.clone();
            let flag = Arc::clone(flag);
            joins.push(thread::spawn(move || {
                fake_node_loop(ck, root, id, true, false, Some(flag));
            }));
        }

        let mut probes = ArbiterProbeHub::new("test");
        let ids: Vec<String> = topo.boards.iter().map(|b| b.id.clone()).collect();
        let mut peers = await_ready(&topo, &ids, &control, &mut probes, None, None).unwrap();

        // Not running: the peer's stop is ignored and nodes keep their state.
        assert!(
            !apply_peer_cluster_stop(
                &control,
                &mut peers,
                "b:1",
                HostStopReason::Breakpoint,
                false
            )
            .unwrap()
        );
        assert!(peers.values().all(|p| *p == PeerState::Ready));
        thread::sleep(Duration::from_millis(50));
        assert!(flags.iter().all(|f| !*f.lock().unwrap()));

        assert!(
            apply_peer_cluster_stop(
                &control,
                &mut peers,
                "b:1",
                HostStopReason::Breakpoint,
                true
            )
            .unwrap()
        );
        assert!(peers.values().all(|p| *p == PeerState::Stopped));
        for j in joins {
            j.join().unwrap();
        }
        assert!(flags.iter().all(|f| *f.lock().unwrap()));
    }

    // ---- GDB step across two arbiters (loopback control mesh + fake nodes) ----

    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    const STEP_INSN_NS: u64 = 70;

    /// What a fake node saw / where it is, read by the test.
    #[derive(Default)]
    struct FakeStats {
        gdb_steps: AtomicUsize,
        starts: AtomicUsize,
        cluster_stops: AtomicUsize,
        vt: AtomicU64,
    }

    /// Node of one board: Ready on StartupRecord; one instruction (`STEP_INSN_NS`)
    /// on a GdbStep for its own board; while running it moves straight to the
    /// ceiling and reports it.
    fn step_fake_node(
        cluster_key: String,
        iox_root: PathBuf,
        board_id: String,
        initial_vt: u64,
        stats: Arc<FakeStats>,
        quit: Arc<AtomicBool>,
    ) {
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, &format!("step-fake-{board_id}-{cluster_key}")).unwrap();
        let control = NodeControl::open(&node, &cluster_key, boards).unwrap();
        let my_hash = board_id_hash(&board_id);
        let mut vt = initial_vt;
        let mut allowed = 0;
        let mut running = false;
        stats.vt.store(vt, Ordering::SeqCst);
        while !quit.load(Ordering::SeqCst) {
            while let Ok(Some(msg)) = control.try_recv() {
                match msg {
                    ControlToNode::StartupRecord { .. } => {
                        let _ = control.publish(&ControlToArbiter::Ready { from: my_hash });
                    }
                    ControlToNode::Start { allowed_ns } => {
                        stats.starts.fetch_add(1, Ordering::SeqCst);
                        running = true;
                        allowed = allowed_ns;
                    }
                    ControlToNode::Allowed { allowed_ns } => allowed = allowed_ns,
                    ControlToNode::ClusterStop { .. } => {
                        stats.cluster_stops.fetch_add(1, Ordering::SeqCst);
                        running = false;
                    }
                    ControlToNode::GdbStep { board_hash } => {
                        stats.gdb_steps.fetch_add(1, Ordering::SeqCst);
                        if board_hash == my_hash {
                            running = false;
                            vt += STEP_INSN_NS;
                            let _ = control.publish(&ControlToArbiter::TimeReport {
                                from: my_hash,
                                virtual_time_ns: vt,
                            });
                            let _ = control.publish(&ControlToArbiter::HostStop {
                                from: my_hash,
                                reason: HostStopReason::Step,
                            });
                        }
                    }
                    _ => {}
                }
            }
            if running && vt < allowed {
                vt = allowed;
                let _ = control.publish(&ControlToArbiter::TimeReport {
                    from: my_hash,
                    virtual_time_ns: vt,
                });
            }
            stats.vt.store(vt, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// One arbiter of the step scenarios: the state `run_host_session` keeps
    /// for one machine, one local board, a fake node and the control mesh.
    struct StepMember {
        board: &'static str,
        control: ArbiterControl,
        _node: iceoryx2::prelude::Node<iceoryx2::prelude::ipc::Service>,
        peers: HashMap<String, PeerState>,
        ceiling: TimeCeiling,
        mesh: ControlMesh,
        allowed: u64,
        until_cap: Option<u64>,
        running: bool,
        last_stop_reason: Option<HostStopReason>,
        gdb_step: Option<GdbStepPhase>,
        stats: Arc<FakeStats>,
    }

    impl StepMember {
        fn board_hash(&self) -> u64 {
            board_id_hash(self.board)
        }

        /// What `run_host_session` does for one HostCommand::Step.
        fn host_step(&mut self, board_hash: u64) {
            apply_gdb_step(
                &self.control,
                &mut self.peers,
                board_hash,
                &mut self.running,
                &mut self.until_cap,
                &mut self.last_stop_reason,
                &mut self.gdb_step,
                Some(&mut self.mesh),
            )
            .unwrap();
        }

        /// What `run_host_session` does for one HostCommand::Start.
        fn host_start(&mut self) {
            cancel_gdb_step(&mut self.gdb_step, Some(&mut self.mesh));
            apply_start(
                &self.control,
                &mut self.peers,
                &self.ceiling,
                &mut self.allowed,
                &mut self.until_cap,
                &mut self.last_stop_reason,
                &mut self.running,
                None,
            )
            .unwrap();
        }

        /// One pass of the host-session loop body that concerns the control mesh
        /// and time sync.
        fn tick(&mut self) {
            for notice in self.mesh.poll(Instant::now()) {
                apply_mesh_notice(
                    notice,
                    &self.control,
                    &mut self.peers,
                    &self.ceiling,
                    &mut self.allowed,
                    &mut self.until_cap,
                    &mut self.running,
                    &mut self.gdb_step,
                    &mut self.last_stop_reason,
                )
                .unwrap();
            }
            let step = step_time_sync(
                &self.control,
                &mut self.peers,
                &mut self.ceiling,
                &mut self.allowed,
                &mut self.until_cap,
                &mut self.running,
                &mut self.gdb_step,
                &mut self.last_stop_reason,
            )
            .unwrap();
            finish_sync_step(
                step,
                Some(&mut self.mesh),
                &mut self.running,
                &mut self.until_cap,
                &mut self.gdb_step,
                &mut self.last_stop_reason,
            );
        }

        fn vt(&self) -> u64 {
            self.stats.vt.load(Ordering::SeqCst)
        }

        fn idle(&self) -> bool {
            !self.running && self.gdb_step.is_none()
        }
    }

    /// Two arbiters, boards `a` (arbiter `a:1`) and `b` (arbiter `b:1`), joined by
    /// a loopback control mesh. The nodes start at `a_vt` / `b_vt`.
    struct StepPair {
        members: [StepMember; 2],
        quit: Arc<AtomicBool>,
        joins: Vec<thread::JoinHandle<()>>,
        _dir: tempfile::TempDir,
    }

    impl StepPair {
        fn new(tag: &str, a_vt: u64, b_vt: u64) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let topo = two_board_topo(2_000);
            let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
            let quit = Arc::new(AtomicBool::new(false));
            let listeners = [
                PeerListeners::bind("a:1".into(), "127.0.0.1").unwrap(),
                PeerListeners::bind("b:1".into(), "127.0.0.1").unwrap(),
            ];
            let mut joins = Vec::new();
            let mut members = Vec::new();
            for (i, (board, vt)) in [("a", a_vt), ("b", b_vt)].into_iter().enumerate() {
                // Member-local iceoryx identity, as in a real multi-backend run.
                let cluster_key = format!("t-step-{tag}-{board}-{}", std::process::id());
                let iox_root = dir.path().join(format!("iox-{board}"));
                let config = isolated_config(&iox_root).unwrap();
                let node = create_node(&config, &format!("arb-step-{tag}-{board}")).unwrap();
                let control =
                    ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards.clone())
                        .unwrap();
                let stats = Arc::new(FakeStats::default());
                {
                    let (key, root, stats, quit) = (
                        cluster_key.clone(),
                        iox_root.clone(),
                        Arc::clone(&stats),
                        Arc::clone(&quit),
                    );
                    joins.push(thread::spawn(move || {
                        step_fake_node(key, root, board.to_string(), vt, stats, quit);
                    }));
                }
                let mut probes = ArbiterProbeHub::new("test");
                let local = vec![board.to_string()];
                let peers = await_ready(&topo, &local, &control, &mut probes, None, None).unwrap();
                let mut ceiling = TimeCeiling::new(local, topo.margin_ns);
                ceiling.report(board, vt);
                let other = &listeners[1 - i];
                let mesh = ControlMesh::open(
                    &listeners[i].control_tcp,
                    "sim-step",
                    if i == 0 { "a:1" } else { "b:1" },
                    &[other.endpoints().clone()],
                    Instant::now(),
                )
                .unwrap()
                .unwrap();
                members.push(StepMember {
                    board,
                    control,
                    _node: node,
                    peers,
                    ceiling,
                    mesh,
                    allowed: 0,
                    until_cap: None,
                    running: false,
                    last_stop_reason: None,
                    gdb_step: None,
                    stats,
                });
            }
            let [a, b]: [StepMember; 2] = members.try_into().ok().unwrap();
            let mut pair = Self {
                members: [a, b],
                quit,
                joins,
                _dir: dir,
            };
            for m in &mut pair.members {
                m.mesh.set_local_ready();
            }
            assert!(pair.run_until(Duration::from_secs(3), |p| {
                p.members.iter().all(|m| m.mesh.peers_all_ready())
            }));
            pair
        }

        /// Run both arbiters (and the time view each gets of the other, which the
        /// multicast provides in a real run) until `done` holds.
        fn run_until(&mut self, limit: Duration, done: impl Fn(&Self) -> bool) -> bool {
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline {
                for i in 0..2 {
                    let other_vt = self.members[1 - i].ceiling.virtual_time_ns();
                    self.members[i].ceiling.set_remote(Some(other_vt), false);
                    self.members[i].tick();
                }
                if done(self) {
                    return true;
                }
                thread::sleep(Duration::from_millis(2));
            }
            false
        }
    }

    impl Drop for StepPair {
        fn drop(&mut self) {
            self.quit.store(true, Ordering::SeqCst);
            for j in self.joins.drain(..) {
                let _ = j.join();
            }
        }
    }

    #[test]
    #[serial]
    fn gdb_step_aligns_a_lagging_peer_arbiter_to_the_stepped_board() {
        let mut pair = StepPair::new("lag", 1000, 400);
        let hash_a = pair.members[0].board_hash();

        pair.members[0].host_step(hash_a);
        assert!(pair.run_until(Duration::from_secs(5), |p| {
            p.members.iter().all(StepMember::idle)
                && p.members[1].vt() == 1000 + STEP_INSN_NS
                && p.members[1].stats.cluster_stops.load(Ordering::SeqCst) >= 1
        }));

        let [a, b] = &pair.members;
        // Both nodes stand at the stepped board's time, stopped, reason Step.
        assert_eq!(a.vt(), 1000 + STEP_INSN_NS);
        assert_eq!(b.vt(), 1000 + STEP_INSN_NS);
        assert_eq!(a.last_stop_reason, Some(HostStopReason::Step));
        assert_eq!(b.last_stop_reason, Some(HostStopReason::Step));
        assert_eq!(a.ceiling.virtual_time_ns(), 1000 + STEP_INSN_NS);
        assert_eq!(b.ceiling.virtual_time_ns(), 1000 + STEP_INSN_NS);
        // Only the owner's node executed an instruction; the peer's node was
        // never asked to step.
        assert_eq!(a.stats.gdb_steps.load(Ordering::SeqCst), 1);
        assert_eq!(b.stats.gdb_steps.load(Ordering::SeqCst), 0);
        // The peer ran (catch-up) and was stopped by its own arbiter.
        assert_eq!(b.stats.starts.load(Ordering::SeqCst), 1);
        assert!(b.stats.cluster_stops.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    #[serial]
    fn gdb_step_leaves_a_peer_that_is_already_ahead_stopped() {
        let mut pair = StepPair::new("ahead", 1000, 5000);
        let hash_a = pair.members[0].board_hash();

        pair.members[0].host_step(hash_a);
        assert!(pair.run_until(Duration::from_secs(5), |p| {
            p.members[0].idle() && p.members[1].idle() && p.members[1].last_stop_reason.is_some()
        }));

        let [a, b] = &pair.members;
        assert_eq!(a.vt(), 1000 + STEP_INSN_NS);
        // Already past the target: it is neither started nor moved back.
        assert_eq!(b.vt(), 5000);
        assert_eq!(b.stats.starts.load(Ordering::SeqCst), 0);
        assert_eq!(b.stats.gdb_steps.load(Ordering::SeqCst), 0);
        assert_eq!(b.last_stop_reason, Some(HostStopReason::Step));
    }

    #[test]
    #[serial]
    fn gdb_step_stops_a_running_peer_arbiter_first() {
        // `b` runs from 0 towards its ceiling (min + margin) while `a` waits at 1000.
        let mut pair = StepPair::new("running", 1000, 0);
        pair.members[1].host_start();
        assert!(pair.run_until(Duration::from_secs(3), |p| p.members[1].vt() == 1000));
        assert!(pair.members[1].running);
        let stops_before = pair.members[1].stats.cluster_stops.load(Ordering::SeqCst);

        let hash_a = pair.members[0].board_hash();
        pair.members[0].host_step(hash_a);
        assert!(pair.run_until(Duration::from_secs(5), |p| {
            p.members[0].idle()
                && p.members[1].idle()
                && p.members[1].vt() >= 1000 + STEP_INSN_NS
                && p.members[1].stats.cluster_stops.load(Ordering::SeqCst) > stops_before
        }));

        let [a, b] = &pair.members;
        assert_eq!(a.vt(), 1000 + STEP_INSN_NS);
        // `b` was running free when the step began, so it may already be up to
        // one margin ahead of the stepped board (nothing runs it back); it is
        // never behind.
        assert!(b.vt() >= 1000 + STEP_INSN_NS, "b at {}", b.vt());
        assert!(b.ceiling.virtual_time_ns() >= 1000 + STEP_INSN_NS);
        // The peer's node got a ClusterStop for the all-stop.
        assert!(b.stats.cluster_stops.load(Ordering::SeqCst) > stops_before);
        assert_eq!(b.stats.gdb_steps.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[serial]
    fn gdb_step_for_a_board_of_another_arbiter_is_refused() {
        let mut pair = StepPair::new("foreign", 1000, 400);
        let hash_a = pair.members[0].board_hash();

        // `b` does not run board `a`: nothing is published, no step is opened.
        pair.members[1].host_step(hash_a);
        pair.run_until(Duration::from_millis(300), |_| false);

        let [a, b] = &pair.members;
        assert!(a.gdb_step.is_none() && b.gdb_step.is_none());
        assert_eq!(a.stats.gdb_steps.load(Ordering::SeqCst), 0);
        assert_eq!(b.stats.gdb_steps.load(Ordering::SeqCst), 0);
        assert_eq!((a.vt(), b.vt()), (1000, 400));
    }

    #[test]
    #[serial]
    fn host_start_cancels_the_step_the_peer_is_waiting_for() {
        let mut pair = StepPair::new("cancel", 1000, 400);
        let hash_a = pair.members[0].board_hash();

        // The owner opens the step but gives up before its node reports.
        pair.members[0].host_step(hash_a);
        pair.members[0].host_start();
        assert!(pair.members[0].gdb_step.is_none());
        assert!(pair.run_until(Duration::from_secs(3), |p| p.members[1].gdb_step.is_none()));
        assert!(pair.members[1].gdb_step.is_none());
        assert!(!pair.members[1].running, "the peer never caught up");
    }
}

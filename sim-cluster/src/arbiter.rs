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
use crate::control::{ControlToArbiter, ControlToNode, HostStopReason};
use crate::host::{HostCommand, SessionNode, SessionStatus};
use crate::ipc::{
    ArbiterControl, ArbiterHostPort, ArbiterLogBus, ArbiterNodeActions, ArbiterProbeCtrl,
    ArbiterProbeForward, ArbiterServerActions, IpcError, board_hash_table, create_node,
    isolated_config, new_cluster_key, root_path_for_cluster,
};
use crate::lifecycle::{PeerEffect, PeerState};
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
        })
    }
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
    let mut peers = await_ready(topo, control, &mut probes, None, None)?;
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
    control: &ArbiterControl,
    probes: &mut ArbiterProbeHub,
    actions: Option<&ArbiterNodeActions>,
    server: Option<&ArbiterServerActions>,
) -> Result<HashMap<String, PeerState>, ArbiterError> {
    let timeout = Duration::from_millis(topo.ready_timeout_ms);
    let deadline = Instant::now() + timeout;
    let mut peers: HashMap<String, PeerState> = topo
        .boards
        .iter()
        .map(|b| (b.id.clone(), PeerState::AwaitingReady))
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

    let mut ceiling = TimeCeiling::new(topo.boards.iter().map(|b| b.id.clone()), topo.margin_ns);
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

enum SyncStep {
    Continue,
    Stopped(HostStopReason),
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
    let board_ids: Vec<&str> = topo.boards.iter().map(|b| b.id.as_str()).collect();
    let boards = board_hash_table(board_ids).map_err(ArbiterError::Message)?;

    let config = isolated_config(&opts.iox_root)?;
    let node = create_node(&config, &format!("arbiter-{}", opts.cluster_key))?;
    let host = ArbiterHostPort::open(&node, &opts.cluster_key)?;
    let control =
        ArbiterControl::create(&node, &opts.cluster_key, topo.boards.len(), boards.clone())?;
    let node_actions = ArbiterNodeActions::create(&node, &opts.cluster_key, topo.boards.len())?;
    let server_actions = ArbiterServerActions::open(&node, &opts.cluster_key, topo.boards.len())?;
    let probe_ctrl = ArbiterProbeCtrl::open(&node, &opts.cluster_key, topo.boards.len())?;
    let mut logs =
        ArbiterLogBus::create(&opts.iox_root, &opts.cluster_key, topo.boards.len(), boards)?;
    log::info!(
        "loaded topology ({} boards, {} edges, margin_ns={}, cluster_key={})",
        topo.boards.len(),
        topo.edges.len(),
        topo.margin_ns,
        opts.cluster_key
    );

    let mut children = spawn_nodes(opts, topo)?;
    let mut probe_hub = ArbiterProbeHub::new(opts.cluster_key.clone());
    let mut peers = match await_ready(
        topo,
        &control,
        &mut probe_hub,
        Some(&node_actions),
        Some(&server_actions),
    ) {
        Ok(peers) => peers,
        Err(err) => {
            stop_children(&mut children);
            logs.shutdown();
            return Err(err);
        }
    };
    if let Err(err) = probe_hub.subscribe_catalog(&node) {
        stop_children(&mut children);
        logs.shutdown();
        return Err(ArbiterError::Message(err.to_string()));
    }
    let probe_tx = match ArbiterProbeForward::open(&node, &opts.cluster_key) {
        Ok(tx) => tx,
        Err(err) => {
            stop_children(&mut children);
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
    let mut ceiling = TimeCeiling::new(topo.boards.iter().map(|b| b.id.clone()), topo.margin_ns);
    let mut allowed = ceiling.allowed_ns();
    let mut running = false;
    let mut until_cap: Option<u64> = None;
    let mut last_stop_reason: Option<HostStopReason> = None;
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
        let mut shutdown = false;
        loop {
            match host.try_cmd()? {
                Some(HostCommand::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Some(HostCommand::Start { until_ns }) => {
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

        match step_time_sync(&control, &mut peers, &mut ceiling, &mut allowed, until_cap)? {
            SyncStep::Continue => {}
            SyncStep::Stopped(reason) => {
                running = false;
                until_cap = None;
                last_stop_reason = Some(reason);
            }
        }

        let vt = ceiling.virtual_time_ns();
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
    stop_children(&mut children);
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
    *allowed = ceiling.allowed_ns_until(*until_cap);
    broadcast_start(control, peers, *allowed)?;
    *running = true;
    *last_stop_reason = None;
    Ok(())
}

/// Guest clocks may already equal the last Allowed. Treat that as `now` so the
/// republished ceiling is `now + margin` and boards can leave the old ceiling.
fn allowed_after_reset(ceiling: &mut TimeCeiling, last_allowed: u64) -> u64 {
    let now = last_allowed.max(ceiling.virtual_time_ns());
    ceiling.advance_to(now);
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

fn step_time_sync(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    ceiling: &mut TimeCeiling,
    allowed: &mut u64,
    until_ns: Option<u64>,
) -> Result<SyncStep, ArbiterError> {
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
                cluster_stop = Some((board_id.clone(), reason));
            }
            other => warn_peer(&board_id, other)?,
        }
        *state = next;
    }
    if let Some((source, reason)) = cluster_stop {
        log::info!("HostStop from '{source}' ({reason}); broadcasting ClusterStop");
        broadcast_cluster_stop(control, peers, &source, reason)?;
        return Ok(SyncStep::Stopped(reason));
    }
    if changed {
        let next_allowed = ceiling.allowed_ns_until(until_ns);
        if next_allowed != *allowed {
            *allowed = next_allowed;
            publish_allowed(control, *allowed)?;
        }
    }
    if reached_until(until_ns, ceiling.virtual_time_ns()) {
        log::info!(
            "until_ns reached at vt={}; broadcasting ClusterStop",
            ceiling.virtual_time_ns()
        );
        broadcast_cluster_stop(control, peers, "server", HostStopReason::Until)?;
        return Ok(SyncStep::Stopped(HostStopReason::Until));
    }

    Ok(SyncStep::Continue)
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
    let mut need_rl78 = false;
    let mut need_plant = false;
    for board in &topo.boards {
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

fn spawn_nodes(opts: &ArbiterOptions, topo: &LogicalTopology) -> Result<Vec<Child>, ArbiterError> {
    let mut children = Vec::new();
    for board in &topo.boards {
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
        children.push(child);
    }
    Ok(children)
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
    let nodes = topo
        .boards
        .iter()
        .map(|board| SessionNode {
            id: board.id.clone(),
            kind: board.kind.clone(),
            state: peers
                .get(&board.id)
                .map(peer_state_name)
                .unwrap_or("unknown")
                .to_string(),
        })
        .collect();
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
    fn binary_for_kind_selects_rl78_and_plant() {
        let opts = ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: PathBuf::from("/tmp/rl78-minimal-board"),
            plant_bin: PathBuf::from("/tmp/minimal-plant"),
            cluster_key: "k".into(),
            iox_root: PathBuf::from("/tmp/iox"),
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
}

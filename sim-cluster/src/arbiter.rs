//! Arbiter: create iceoryx services, spawn nodes, Booting barrier, then wait.
//!
//! The guest stays stopped until a host `start` command. `stop` halts the
//! guest without exiting this process, so a later `start` or `reset` can
//! run. `shutdown` on the server→arbiter iceoryx channel ends the process.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::action::ActionUpstream;
use crate::arbiter_catalog::{
    apply_probe_catalogs, collect_board_catalogs, publish_action_catalogs,
};
use crate::control::{ControlToArbiter, ControlToNode, HostStopReason, NodeStateWire};
use crate::host::{HostCommand, SessionNode, SessionStatus};
use crate::ipc::{
    ArbiterControl, ArbiterHostPort, ArbiterLogBus, ArbiterNodeActions, ArbiterProbeCtrl,
    ArbiterProbeForward, ArbiterServerActions, IpcError, board_hash_table, create_node,
    isolated_config, new_cluster_key, root_path_for_cluster,
};
use crate::lifecycle::{PeerEffect, PeerState};
use crate::probe::ProbeWireKind;
use crate::probe_hub::{ArbiterProbeHub, ProbeSample};
use crate::topology::{LogicalTopology, TopologyError};
use crate::vt_clock::{VtClock, VtClockLease, board_slot_index};

/// Idle poll while waiting for Booting barrier / time-sync (arbiter side).
const ARBITER_POLL_IDLE: Duration = Duration::from_millis(1);

/// Arbiter failures.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("booting barrier timed out after {timeout_ms}ms; missing boards: {missing}")]
    BootingTimeout { timeout_ms: u64, missing: String },
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Message(String),
}

/// Options for [`run_arbiter`].
#[derive(Clone, Debug)]
pub struct ArbiterOptions {
    pub topology_path: PathBuf,
    /// Binary for [`crate::KIND_RL78`]. Default: `rl78-minimal-board` next to the arbiter.
    pub node_bin: PathBuf,
    /// Binary for [`crate::KIND_PLANT`]. Default: `minimal-plant` next to the arbiter.
    pub plant_bin: PathBuf,
    /// Binary for [`crate::KIND_PIPING_PLANT`]. Default: `piping-plant` next to the arbiter.
    pub piping_plant_bin: PathBuf,
    /// Binary for [`crate::KIND_BOILER_PLANT`]. Default: `boiler-plant` next to the arbiter.
    pub boiler_plant_bin: PathBuf,
    /// Binary for [`crate::KIND_RL78BOILER`]. Default: `boiler_board` next to the arbiter.
    pub rl78boiler_bin: PathBuf,
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
            piping_plant_bin: exe_dir.join("piping-plant"),
            boiler_plant_bin: exe_dir.join("boiler-plant"),
            rl78boiler_bin: exe_dir.join("boiler_board"),
            cluster_key,
            iox_root,
        })
    }
}

/// Load topology, spawn nodes, wait until nodes leave Booting, then follow iceoryx host commands.
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
    crate::topology::build_topology_hashes(topo)
        .map_err(|err| ArbiterError::Message(err.to_string()))?;
    ensure_node_bins(opts, topo)?;
    run_host_session(opts, topo)
}

/// Test helper: Booting barrier, immediate Start, then a short time-sync window.
#[cfg(test)]
fn booting_barrier_and_start(
    topo: &LogicalTopology,
    control: &ArbiterControl,
) -> Result<(), ArbiterError> {
    let mut peers = await_booting(topo, control)?;
    control.publish(&ControlToNode::Start)?;
    for state in peers.values_mut() {
        *state = state.on_start_broadcast();
    }
    log::info!("Start broadcast");
    run_time_sync(control, &mut peers)?;
    Ok(())
}

/// How often to re-broadcast [`ControlToNode::GetState`] during the Booting barrier.
const GET_STATE_REPUBLISH: Duration = Duration::from_millis(50);

fn await_booting(
    topo: &LogicalTopology,
    control: &ArbiterControl,
) -> Result<HashMap<String, PeerState>, ArbiterError> {
    let timeout = Duration::from_millis(topo.booting_timeout_ms);
    let deadline = Instant::now() + timeout;
    let mut peers: HashMap<String, PeerState> = topo
        .boards
        .iter()
        .map(|b| (b.id.clone(), PeerState::Booting))
        .collect();

    control.publish(&ControlToNode::GetState)?;
    let mut last_get_state = Instant::now();

    while peers.values().any(|p| *p == PeerState::Booting) {
        if Instant::now() >= deadline {
            return Err(booting_timeout(topo, &peers));
        }

        if last_get_state.elapsed() >= GET_STATE_REPUBLISH
            && peers.values().any(|p| *p == PeerState::Booting)
        {
            control.publish(&ControlToNode::GetState)?;
            last_get_state = Instant::now();
        }

        let mut progressed = false;
        while let Some(msg) = control.try_recv()? {
            match msg {
                ControlToArbiter::State { from, state } => {
                    let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
                        continue;
                    };
                    let Some(peer) = peers.get_mut(&board_id) else {
                        continue;
                    };
                    let next = PeerState::from_node_wire(state);
                    if next != *peer {
                        log::trace!("peer '{board_id}' {peer:?} -> {next:?}");
                        *peer = next;
                        progressed = true;
                    }
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

    log::info!("Booting barrier complete ({} nodes)", peers.len());
    Ok(peers)
}

/// After [`ControlToNode::ClusterStop`], wait until every peer reports
/// [`NodeStateWire::Stopped`] so the hard ceiling can be reopened safely.
fn await_peers_stopped(
    control: &ArbiterControl,
    peers: &HashMap<String, PeerState>,
    timeout: Duration,
) -> Result<(), ArbiterError> {
    let mut pending: HashSet<String> = peers.keys().cloned().collect();
    if pending.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    control.publish(&ControlToNode::GetState)?;
    let mut last_get_state = Instant::now();

    while !pending.is_empty() {
        if Instant::now() >= deadline {
            let mut missing: Vec<_> = pending.into_iter().collect();
            missing.sort();
            return Err(ArbiterError::Message(format!(
                "reset stop barrier timed out; missing Stopped from: {}",
                missing.join(", ")
            )));
        }
        if last_get_state.elapsed() >= GET_STATE_REPUBLISH {
            control.publish(&ControlToNode::GetState)?;
            last_get_state = Instant::now();
        }

        let mut progressed = false;
        while let Some(msg) = control.try_recv()? {
            match msg {
                ControlToArbiter::State {
                    from,
                    state: NodeStateWire::Stopped,
                } => {
                    let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
                        continue;
                    };
                    if pending.remove(&board_id) {
                        progressed = true;
                    }
                }
                ControlToArbiter::State { from, state } => {
                    if let Some(board_id) = control.resolve_board(from) {
                        log::trace!("reset barrier: '{board_id}' still {state:?}");
                    }
                }
                ControlToArbiter::HostStop { .. } => {
                    // Late HostStop after ClusterStop; ignore during the barrier.
                }
            }
        }
        if !progressed {
            thread::sleep(ARBITER_POLL_IDLE);
        }
    }
    log::info!("reset stop barrier complete");
    Ok(())
}

#[cfg(test)]
fn run_time_sync(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    // TODO(cluster-loop): MVP wall-clock window only. Replace with an open loop
    // until ControlToNode::Shutdown (or equivalent) ends the run.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
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
        thread::sleep(ARBITER_POLL_IDLE);
    }
    Ok(())
}

enum SyncStep {
    Continue,
    Stopped(HostStopReason),
}

/// GDB single-step: wait for the debug node's insn, then catch peers up.
enum GdbStepPhase {
    WaitInsn { board_id: String },
    CatchUp { reason: HostStopReason },
}

/// Forward action requests and results. Late `Advertise` samples from nodes are
/// ignored. Requests are always forwarded; nodes queue them while Stopped and
/// apply on Start.
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

    let catalogs = match collect_board_catalogs(opts, topo) {
        Ok(catalogs) => catalogs,
        Err(err) => {
            logs.shutdown();
            return Err(err);
        }
    };
    let mut probe_hub = ArbiterProbeHub::new(opts.cluster_key.clone());
    if let Err(err) = apply_probe_catalogs(&mut probe_hub, &catalogs) {
        logs.shutdown();
        return Err(err);
    }
    log::info!(
        "loaded catalogs via --list-catalog ({} boards, {} probes)",
        catalogs.len(),
        probe_hub.catalog().len()
    );

    let vt = match VtClock::create(&opts.iox_root, topo.boards.len()) {
        Ok(vt) => vt,
        Err(err) => {
            logs.shutdown();
            return Err(ArbiterError::Io(err));
        }
    };
    // Unlink vt_clock on any exit from this session (including early `?`).
    let _vt_lease = VtClockLease::new(&opts.iox_root);
    let mut children = match spawn_nodes(opts, topo) {
        Ok(children) => children,
        Err(err) => {
            logs.shutdown();
            return Err(err);
        }
    };
    let mut peers = match await_booting(topo, &control) {
        Ok(peers) => peers,
        Err(err) => {
            stop_children(&mut children);
            logs.shutdown();
            return Err(err);
        }
    };
    if let Err(err) = publish_action_catalogs(&server_actions, &catalogs) {
        stop_children(&mut children);
        logs.shutdown();
        return Err(err);
    }
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
    let mut running = false;
    let mut until_cap: Option<u64> = None;
    let mut last_stop_reason: Option<HostStopReason> = None;
    let mut gdb_step: Option<GdbStepPhase> = None;
    let mut last_status = Instant::now();
    write_status(&host, running, vt.min_board_vt(), None, topo, &peers)?;
    let mut reported_vt = vt.min_board_vt();
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
                    gdb_step = None;
                    apply_start(
                        &control,
                        &mut peers,
                        &vt,
                        &mut until_cap,
                        &mut last_stop_reason,
                        &mut running,
                        until_ns,
                    )?;
                }
                Some(HostCommand::Stop) if running => {
                    log::info!("host Stop");
                    gdb_step = None;
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
                    gdb_step = None;
                    until_cap = None;
                    // ClusterStop, freeze ceiling so stragglers cannot race, wait until
                    // every board reports Stopped, then Reset and open the ceiling.
                    broadcast_cluster_stop(
                        &control,
                        &mut peers,
                        "server",
                        HostStopReason::ExternalStop,
                    )?;
                    vt.set_ceiling(vt.min_board_vt());
                    await_peers_stopped(
                        &control,
                        &peers,
                        Duration::from_millis(topo.booting_timeout_ms),
                    )?;
                    control.publish(&ControlToNode::Reset)?;
                    vt.set_ceiling(u64::MAX);
                    broadcast_start(&control, &mut peers)?;
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

        match step_time_sync(
            &control,
            &mut peers,
            &vt,
            topo,
            &mut until_cap,
            &mut running,
            &mut gdb_step,
            &mut last_stop_reason,
        )? {
            SyncStep::Continue => {}
            SyncStep::Stopped(reason) => {
                running = false;
                until_cap = None;
                gdb_step = None;
                last_stop_reason = Some(reason);
            }
        }

        let now_vt = vt.min_board_vt();
        let published_reason = if running { None } else { last_stop_reason };
        if running != reported_running
            || now_vt != reported_vt
            || published_reason != reported_stop_reason
            || last_status.elapsed() >= Duration::from_millis(100)
        {
            write_status(&host, running, now_vt, published_reason, topo, &peers)?;
            reported_running = running;
            reported_vt = now_vt;
            reported_stop_reason = published_reason;
            last_status = Instant::now();
        }
    };

    logs.shutdown();
    stop_children(&mut children);
    // `_vt_lease` Drop unlinks vt_clock.
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

/// Virtual time of `source` for GDB catch-up (`until` / ceiling).
fn gdb_catchup_target(
    topo: &LogicalTopology,
    vt: &VtClock,
    source: &str,
) -> Result<u64, ArbiterError> {
    let slot = board_slot(topo, source).ok_or_else(|| {
        ArbiterError::Message(format!(
            "gdb catch-up: board '{source}' not found in topology"
        ))
    })?;
    Ok(vt.board_vt(slot))
}

/// Slot index in [`VtClock`] for `board_id` (topology order, 1-based).
fn board_slot(topo: &LogicalTopology, board_id: &str) -> Option<usize> {
    topo.boards
        .iter()
        .position(|b| b.id == board_id)
        .map(board_slot_index)
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

/// Apply a host Start: stay/stop if `until_ns` is not ahead of now, otherwise
/// write the shared-memory ceiling and broadcast Start (Stopped resumes;
/// Running retargets).
fn apply_start(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    vt: &VtClock,
    until_cap: &mut Option<u64>,
    last_stop_reason: &mut Option<HostStopReason>,
    running: &mut bool,
    until_ns: Option<u64>,
) -> Result<(), ArbiterError> {
    let now = vt.min_board_vt();
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
    vt.set_ceiling(until_ns.unwrap_or(u64::MAX));
    broadcast_start(control, peers)?;
    *running = true;
    *last_stop_reason = None;
    Ok(())
}

/// ClusterStop if needed, then one insn on `board_hash`. Catch-up starts
/// when that node reports [`HostStopReason`].
#[allow(clippy::too_many_arguments)]
fn apply_gdb_step(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    board_hash: crate::ipc::HashedId<u64>,
    running: &mut bool,
    until_cap: &mut Option<u64>,
    last_stop_reason: &mut Option<HostStopReason>,
    gdb_step: &mut Option<GdbStepPhase>,
) -> Result<(), ArbiterError> {
    let Some(board_id) = control.resolve_board(board_hash).map(str::to_owned) else {
        log::warn!("gdb Step: unknown board_hash={board_hash}");
        return Ok(());
    };
    if *running {
        log::info!("gdb Step: ClusterStop before insn on '{board_id}'");
        broadcast_cluster_stop(control, peers, "server", HostStopReason::ExternalStop)?;
        *running = false;
        *until_cap = None;
        *last_stop_reason = Some(HostStopReason::ExternalStop);
    }
    control.publish(&ControlToNode::GdbStep { board_hash })?;
    *gdb_step = Some(GdbStepPhase::WaitInsn { board_id });
    Ok(())
}

fn broadcast_start(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    control.publish(&ControlToNode::Start)?;
    for state in peers.values_mut() {
        *state = state.on_start_broadcast();
    }
    log::info!("Start broadcast");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn step_time_sync(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    vt: &VtClock,
    topo: &LogicalTopology,
    until_ns: &mut Option<u64>,
    running: &mut bool,
    gdb_step: &mut Option<GdbStepPhase>,
    last_stop_reason: &mut Option<HostStopReason>,
) -> Result<SyncStep, ArbiterError> {
    let mut cluster_stop: Option<(String, HostStopReason)> = None;
    let mut gdb_insn: Option<(String, HostStopReason)> = None;
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
            Some(PeerEffect::HostStop { from: _, reason }) => match gdb_step.as_ref() {
                Some(GdbStepPhase::WaitInsn { board_id: stepped }) if stepped == &board_id => {
                    gdb_insn = Some((board_id.clone(), reason));
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
        return Ok(SyncStep::Stopped(reason));
    }
    if let Some((source, reason)) = gdb_insn {
        let target = gdb_catchup_target(topo, vt, &source)?;
        log::info!("gdb Step from '{source}' ({reason}) vt={target}; catch-up");
        *until_ns = Some(target);
        vt.set_ceiling(target);
        broadcast_start(control, peers)?;
        *running = true;
        *last_stop_reason = None;
        *gdb_step = Some(GdbStepPhase::CatchUp { reason });
        return Ok(SyncStep::Continue);
    }
    let now_vt = vt.min_board_vt();
    if reached_until(*until_ns, now_vt) {
        let reason = match gdb_step {
            Some(GdbStepPhase::CatchUp { reason }) => *reason,
            _ => HostStopReason::Until,
        };
        log::info!("until_ns reached at vt={now_vt} ({reason}); broadcasting ClusterStop");
        *gdb_step = None;
        broadcast_cluster_stop(control, peers, "server", reason)?;
        return Ok(SyncStep::Stopped(reason));
    }

    Ok(SyncStep::Continue)
}

fn analog_millivolts_u16(uv: i64) -> u16 {
    uv.saturating_div(1000).clamp(0, i64::from(u16::MAX)) as u16
}

const KNOWN_KINDS: &str = "rl78, plant, piping-plant, boiler-plant, or rl78boiler";

pub(crate) fn binary_for_kind<'a>(
    opts: &'a ArbiterOptions,
    kind: &str,
) -> Result<&'a PathBuf, ArbiterError> {
    match kind {
        crate::KIND_RL78 => Ok(&opts.node_bin),
        crate::KIND_PLANT => Ok(&opts.plant_bin),
        crate::KIND_PIPING_PLANT => Ok(&opts.piping_plant_bin),
        crate::KIND_BOILER_PLANT => Ok(&opts.boiler_plant_bin),
        crate::KIND_RL78BOILER => Ok(&opts.rl78boiler_bin),
        other => Err(ArbiterError::Message(format!(
            "unknown board kind `{other}` (expected {KNOWN_KINDS})"
        ))),
    }
}

fn missing_bin_hint(kind: &str) -> &'static str {
    match kind {
        crate::KIND_RL78 => "build rl78-minimal-board or pass --node-bin",
        crate::KIND_PLANT => "build minimal-plant or pass --plant-bin",
        crate::KIND_PIPING_PLANT => "build piping-plant or pass --piping-plant-bin",
        crate::KIND_BOILER_PLANT => "build boiler-plant or pass --boiler-plant-bin",
        crate::KIND_RL78BOILER => "build boiler_board or pass --rl78boiler-bin",
        _ => "pass the matching --*-bin flag",
    }
}

fn ensure_node_bins(opts: &ArbiterOptions, topo: &LogicalTopology) -> Result<(), ArbiterError> {
    let mut checked = HashSet::new();
    for board in &topo.boards {
        if !checked.insert(board.kind.as_str()) {
            continue;
        }
        let bin = binary_for_kind(opts, &board.kind)?;
        if !bin.is_file() {
            return Err(ArbiterError::Message(format!(
                "{} binary not found at {} ({})",
                board.kind,
                bin.display(),
                missing_bin_hint(&board.kind)
            )));
        }
    }
    Ok(())
}

fn spawn_nodes(opts: &ArbiterOptions, topo: &LogicalTopology) -> Result<Vec<Child>, ArbiterError> {
    let mut children = Vec::new();
    for (index, board) in topo.boards.iter().enumerate() {
        let bin = binary_for_kind(opts, &board.kind)?;
        let child = Command::new(bin)
            .arg("--board-name")
            .arg(&board.id)
            .arg("--cluster-key")
            .arg(&opts.cluster_key)
            .arg("--iox-root")
            .arg(&opts.iox_root)
            .arg("--topology")
            .arg(&opts.topology_path)
            .arg("--margin-ns")
            .arg(topo.margin_ns.to_string())
            .arg("--vt-slot")
            .arg(board_slot_index(index).to_string())
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
            id: crate::ipc::board_id_hash(&board.id),
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
        PeerState::Booting => "booting",
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

fn booting_timeout(topo: &LogicalTopology, peers: &HashMap<String, PeerState>) -> ArbiterError {
    let missing = peers
        .iter()
        .filter(|(_, p)| **p == PeerState::Booting)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>()
        .join(",");
    ArbiterError::BootingTimeout {
        timeout_ms: topo.booting_timeout_ms,
        missing,
    }
}

fn warn_peer(board_id: &str, effect: Option<PeerEffect>) -> Result<(), ArbiterError> {
    match effect {
        Some(PeerEffect::Warn(msg)) => {
            log::warn!("warning [{board_id}]: {msg}");
        }
        Some(PeerEffect::HostStop { .. }) | None => {}
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
    use crate::topology::{
        BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, PayloadKind, PortRef,
    };
    use serial_test::serial;
    use std::sync::{Arc, Mutex};
    use std::thread;

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
    fn binary_for_kind_selects_known_kinds() {
        let opts = ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: PathBuf::from("/tmp/rl78-minimal-board"),
            plant_bin: PathBuf::from("/tmp/minimal-plant"),
            piping_plant_bin: PathBuf::from("/tmp/piping-plant"),
            boiler_plant_bin: PathBuf::from("/tmp/boiler-plant"),
            rl78boiler_bin: PathBuf::from("/tmp/boiler_board"),
            cluster_key: "k".into(),
            iox_root: PathBuf::from("/tmp/iox"),
        };
        assert_eq!(
            binary_for_kind(&opts, crate::KIND_RL78).unwrap(),
            &opts.node_bin
        );
        assert_eq!(
            binary_for_kind(&opts, crate::KIND_PLANT).unwrap(),
            &opts.plant_bin
        );
        assert_eq!(
            binary_for_kind(&opts, crate::KIND_PIPING_PLANT).unwrap(),
            &opts.piping_plant_bin
        );
        assert_eq!(
            binary_for_kind(&opts, crate::KIND_BOILER_PLANT).unwrap(),
            &opts.boiler_plant_bin
        );
        assert_eq!(
            binary_for_kind(&opts, crate::KIND_RL78BOILER).unwrap(),
            &opts.rl78boiler_bin
        );
        assert!(binary_for_kind(&opts, "avr").is_err());
    }

    #[test]
    fn ensure_node_bins_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present-bin");
        fs::write(&present, b"x").unwrap();
        let opts = ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: present,
            plant_bin: dir.path().join("missing-plant"),
            piping_plant_bin: dir.path().join("missing-piping"),
            boiler_plant_bin: dir.path().join("missing-boiler"),
            rl78boiler_bin: dir.path().join("missing-rl78boiler"),
            cluster_key: "k".into(),
            iox_root: PathBuf::from("/tmp/iox"),
        };
        let topo = LogicalTopology {
            boards: vec![BoardSpec {
                id: "p".into(),
                kind: crate::KIND_PLANT.into(),
                elf: None,
                endpoints: vec![],
            }],
            edges: vec![],
            margin_ns: 1,
            booting_timeout_ms: 1000,
        };
        let err = ensure_node_bins(&opts, &topo).unwrap_err().to_string();
        assert!(err.contains("plant binary not found"), "{err}");
        assert!(err.contains("--plant-bin"), "{err}");
    }

    #[test]
    fn board_slot_follows_topology_order() {
        let topo = two_board_topo(1_000);
        assert_eq!(board_slot(&topo, "a"), Some(1));
        assert_eq!(board_slot(&topo, "b"), Some(2));
        assert_eq!(board_slot(&topo, "c"), None);
    }

    #[test]
    fn gdb_catchup_target_reads_source_slot() {
        let dir = tempfile::tempdir().unwrap();
        let topo = two_board_topo(1_000);
        let vt = VtClock::create(dir.path(), topo.boards.len()).unwrap();
        vt.write_board_vt(1, 111);
        vt.write_board_vt(2, 222);
        assert_eq!(gdb_catchup_target(&topo, &vt, "a").unwrap(), 111);
        assert_eq!(gdb_catchup_target(&topo, &vt, "b").unwrap(), 222);
        assert!(gdb_catchup_target(&topo, &vt, "missing").is_err());
    }

    #[test]
    fn apply_start_writes_ceiling_from_until() {
        let dir = tempfile::tempdir().unwrap();
        let vt = VtClock::create(dir.path(), 2).unwrap();
        vt.write_board_vt(1, 10);
        vt.write_board_vt(2, 20);
        // Unlimited start.
        vt.set_ceiling(u64::MAX);
        assert_eq!(vt.allowed_ns(5), 15);
        // Hard until below min+margin clamps allowed.
        vt.set_ceiling(12);
        assert_eq!(vt.allowed_ns(5), 12);
        // until behind now → stay-stopped helper.
        assert!(should_stay_stopped(Some(10), 20));
        assert!(!should_stay_stopped(Some(30), 20));
    }

    fn two_board_topo(booting_timeout_ms: u64) -> LogicalTopology {
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
                    from: PortRef {
                        path: "a".into(),
                        port: "uart0_tx".into(),
                    },
                    to: PortRef {
                        path: "b".into(),
                        port: "uart0_rx".into(),
                    },
                    route: vec![
                        PortRef {
                            path: "a".into(),
                            port: "uart0_tx".into(),
                        },
                        PortRef {
                            path: "b".into(),
                            port: "uart0_rx".into(),
                        },
                    ],
                    payload: PayloadKind::Uart,
                    uart_ring_len: None,
                },
                DirectedEdge {
                    from: PortRef {
                        path: "b".into(),
                        port: "uart0_tx".into(),
                    },
                    to: PortRef {
                        path: "a".into(),
                        port: "uart0_rx".into(),
                    },
                    route: vec![
                        PortRef {
                            path: "b".into(),
                            port: "uart0_tx".into(),
                        },
                        PortRef {
                            path: "a".into(),
                            port: "uart0_rx".into(),
                        },
                    ],
                    payload: PayloadKind::Uart,
                    uart_ring_len: None,
                },
            ],
            margin_ns: 1000,
            booting_timeout_ms,
        }
    }

    fn fake_node_loop(
        cluster_key: String,
        iox_root: PathBuf,
        board_id: String,
        reply_state: bool,
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
                    ControlToNode::GetState => {
                        if reply_state {
                            let _ = control.publish(&ControlToArbiter::State {
                                from: my_hash,
                                state: crate::control::NodeStateWire::Stopped,
                            });
                        }
                    }
                    ControlToNode::Start => {
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
    fn booting_barrier_succeeds_with_fake_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-booting-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-booting").unwrap();
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

        booting_barrier_and_start(&topo, &control).unwrap();
        for j in joins {
            j.join().unwrap();
        }
    }

    #[test]
    #[serial]
    fn booting_barrier_times_out_when_node_silent() {
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

        let err = booting_barrier_and_start(&topo, &control).unwrap_err();
        match err {
            ArbiterError::BootingTimeout {
                timeout_ms,
                missing,
            } => {
                assert_eq!(timeout_ms, 200);
                assert!(
                    missing.contains('b') || missing == "b",
                    "expected silent board in missing={missing}"
                );
            }
            other => panic!("expected BootingTimeout, got {other:?}"),
        }
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

        booting_barrier_and_start(&topo, &control).unwrap();
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

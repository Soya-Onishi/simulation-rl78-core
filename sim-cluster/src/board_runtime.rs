//! Shared board-process framework for cluster participation.
//!
//! Board binaries supply a machine factory; this module owns CLI parsing, IPC,
//! topology lookup, firmware loading, and the same-thread guest loop.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use sim_kernel::{
    ActionFailure, BoardPorts, Command, Entity, FirmwareError, ProbePoint, Response, SimConfig,
    SimState, Simulator, StopReason, Tick,
};

use crate::action::{
    ACTION_RECV_CAP, ActionPackError, ActionRequest, ActionResult, ActionStatus, ActionUpstream,
};
use crate::control::{ControlToArbiter, HostStopReason};
use crate::data_plane::{DataPlaneError, bind_data_planes, open_data_planes};
use crate::elf_source;
use crate::inspect::InspectServer;
use crate::ipc::{
    IpcError, NodeActions, NodeControl, NodeLog, board_id_hash, create_node, isolated_config,
};
use crate::lifecycle::{NodeEffect, NodeState};
use crate::probe::{NodeProbeRuntime, ProbeError, ProbeWireKind};
use crate::topology::{BoardSpec, LogicalTopology, TopologyError};
use crate::vt_clock::VtClock;

/// Options for [`run_node`].
#[derive(Clone, Debug)]
pub struct BoardOptions {
    /// When true, [`node!`] calls [`list_catalog`] instead of [`run_node`].
    pub list_catalog: bool,
    pub board_name: String,
    pub cluster_key: String,
    pub iox_root: PathBuf,
    pub topology_path: PathBuf,
    /// Virtual-time skew margin (ns); from arbiter CLI (topology copy).
    ///
    /// Used to compute the allowed ceiling from shared memory:
    /// `min(min(board slots) + margin_ns, arbiter ceiling)`.
    pub margin_ns: u64,
    /// This board's slot in the shared-memory [`VtClock`] (`1..=N`, topology order).
    pub vt_slot: usize,
}

/// Failures from the board framework.
#[derive(Debug, thiserror::Error)]
pub enum BoardError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("data plane error: {0}")]
    DataPlane(#[from] DataPlaneError),
    #[error("firmware error: {0}")]
    Firmware(#[from] FirmwareError),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Action(#[from] ActionPackError),
    #[error("{0}")]
    ElfSource(#[from] crate::elf_source::ElfSourceError),
    #[error("{0}")]
    Message(String),
}

/// CLI parse failure (usage / missing args).
#[derive(Debug)]
pub enum BoardCliError {
    Help,
    Message(String),
}

/// Parse board binary argv (after the program name).
///
/// `--list-catalog` skips the cluster-required flags. Those flags may still appear
/// and are ignored in that mode; unknown flags are always an error.
pub fn parse_board_args(
    prog: &str,
    args: impl IntoIterator<Item = String>,
) -> Result<BoardOptions, BoardCliError> {
    let mut list_catalog = false;
    let mut board_name: Option<String> = None;
    let mut cluster_key: Option<String> = None;
    let mut iox_root: Option<PathBuf> = None;
    let mut topology: Option<PathBuf> = None;
    let mut margin_ns: Option<u64> = None;
    let mut vt_slot: Option<usize> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(BoardCliError::Help),
            "--list-catalog" => list_catalog = true,
            "--board-name" => {
                board_name = Some(iter.next().ok_or_else(|| {
                    BoardCliError::Message(format!("{prog}: --board-name requires a value"))
                })?);
            }
            "--cluster-key" => {
                cluster_key = Some(iter.next().ok_or_else(|| {
                    BoardCliError::Message(format!("{prog}: --cluster-key requires a value"))
                })?);
            }
            "--iox-root" => {
                iox_root = Some(PathBuf::from(iter.next().ok_or_else(|| {
                    BoardCliError::Message(format!("{prog}: --iox-root requires a path"))
                })?));
            }
            "--topology" => {
                topology = Some(PathBuf::from(iter.next().ok_or_else(|| {
                    BoardCliError::Message(format!("{prog}: --topology requires a path"))
                })?));
            }
            "--margin-ns" => {
                let raw = iter.next().ok_or_else(|| {
                    BoardCliError::Message(format!("{prog}: --margin-ns requires a value"))
                })?;
                margin_ns = Some(raw.parse().map_err(|_| {
                    BoardCliError::Message(format!("{prog}: --margin-ns must be u64"))
                })?);
            }
            "--vt-slot" => {
                let raw = iter.next().ok_or_else(|| {
                    BoardCliError::Message(format!("{prog}: --vt-slot requires a value"))
                })?;
                vt_slot = Some(raw.parse().map_err(|_| {
                    BoardCliError::Message(format!("{prog}: --vt-slot must be usize"))
                })?);
            }
            other => {
                return Err(BoardCliError::Message(format!(
                    "{prog}: unknown argument {other}"
                )));
            }
        }
    }
    if list_catalog {
        return Ok(BoardOptions {
            list_catalog: true,
            board_name: board_name.unwrap_or_default(),
            cluster_key: cluster_key.unwrap_or_default(),
            iox_root: iox_root.unwrap_or_default(),
            topology_path: topology.unwrap_or_default(),
            margin_ns: margin_ns.unwrap_or(0),
            vt_slot: vt_slot.unwrap_or(0),
        });
    }
    let (
        Some(board_name),
        Some(cluster_key),
        Some(iox_root),
        Some(topology_path),
        Some(margin_ns),
        Some(vt_slot),
    ) = (
        board_name,
        cluster_key,
        iox_root,
        topology,
        margin_ns,
        vt_slot,
    )
    else {
        return Err(BoardCliError::Message(format!(
            "{prog}: --board-name, --cluster-key, --iox-root, --topology, --margin-ns, and \
             --vt-slot are required"
        )));
    };
    Ok(BoardOptions {
        list_catalog: false,
        board_name,
        cluster_key,
        iox_root,
        topology_path,
        margin_ns,
        vt_slot,
    })
}

/// Usage text for board binaries.
#[must_use]
pub fn board_usage(prog: &str) -> String {
    format!(
        "Usage:\n\
         {prog} --board-name <name> --cluster-key <key> --iox-root <path> --topology <json> \
--margin-ns <ns> --vt-slot <1..=N>\n\
         {prog} --list-catalog\n\
         \n\
         Cluster mode is spawned by cluster-arbiter. After init the control loop is\n\
         Stopped and runs only after Start.\n\
         \n\
         --list-catalog prints this node's probe and action catalog as JSON on\n\
         stdout and exits. Cluster flags are not required in that mode.\n"
    )
}

/// Expand a board/plant binary `main` that parses CLI args and runs the node.
///
/// Place at crate root (e.g. `main.rs`). Defines `fn main`, so the crate must not
/// define another `main`. The argument must satisfy
/// `FnOnce() -> (impl Entity, BoardPorts, Vec<ProbePoint>)` (same as [`run_node`]'s
/// builder). With `--list-catalog`, calls [`list_catalog`] instead of [`run_node`].
///
/// ```ignore
/// sim_cluster::node!(g23_machine);
/// ```
#[macro_export]
macro_rules! node {
    ($build:expr) => {
        fn main() {
            let mut argv = ::std::env::args();
            let prog = argv
                .next()
                .unwrap_or_else(|| ::std::env!("CARGO_PKG_NAME").into());
            let opts = match $crate::parse_board_args(&prog, argv) {
                ::std::result::Result::Ok(opts) => opts,
                ::std::result::Result::Err($crate::BoardCliError::Help) => {
                    ::std::eprint!("{}", $crate::board_usage(&prog));
                    ::std::process::exit(0);
                }
                ::std::result::Result::Err($crate::BoardCliError::Message(msg)) => {
                    ::std::eprintln!("{msg}");
                    ::std::eprint!("{}", $crate::board_usage(&prog));
                    ::std::process::exit(2);
                }
            };

            let run = if opts.list_catalog {
                $crate::list_catalog($build)
            } else {
                $crate::run_node(opts, $build)
            };
            if let ::std::result::Result::Err(err) = run {
                ::std::eprintln!("{}: {err}", ::std::env!("CARGO_PKG_NAME"));
                ::std::process::exit(1);
            }
        }
    };
}

/// JSON body printed by [`list_catalog`] / `--list-catalog` (one document on stdout).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CatalogDump {
    pub probes: Vec<CatalogProbe>,
    pub actions: Vec<CatalogAction>,
}

impl CatalogDump {
    /// Parse stdout text as a single [`CatalogDump`] JSON document.
    pub fn parse_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text.trim())
    }
}

/// One probe row in [`CatalogDump`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CatalogProbe {
    /// `probe_name_hash` of the probe name (serde: 16-digit hex).
    pub probe: crate::ipc::HashedId<u64>,
    /// Human-readable probe name.
    pub probe_label: String,
    pub kind: String,
}

/// One action row in [`CatalogDump`]. Shape matches public advertise (hex + labels).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CatalogAction {
    /// `id_hash` of the device name (serde: 16-digit hex).
    pub device: crate::ipc::HashedId<u64>,
    pub device_label: String,
    /// `id_hash` of the action name (serde: 16-digit hex).
    pub action: crate::ipc::HashedId<u64>,
    pub action_label: String,
    pub description: String,
    pub params: Vec<crate::contract::AdvertisedActionParam>,
}

fn probe_kind_str(point: &ProbePoint) -> &'static str {
    match point {
        ProbePoint::Digital(_) => ProbeWireKind::Digital.as_catalog_str(),
        ProbePoint::Uart(_) => ProbeWireKind::Uart.as_catalog_str(),
        ProbePoint::Analog(_) => ProbeWireKind::Analog.as_catalog_str(),
    }
}

fn catalog_param(param: sim_kernel::ParamSpec) -> crate::contract::AdvertisedActionParam {
    crate::contract::AdvertisedActionParam {
        name: crate::ipc::HashedId::new(param.name_hash),
        name_label: param.name,
        unit: param.unit,
        description: param.description,
        min: param.min,
        max: param.max,
        default: param.default,
    }
}

fn catalog_dump_from_entity<E: Entity>(entity: &E, points: &[ProbePoint]) -> CatalogDump {
    let probes = points
        .iter()
        .map(|point| CatalogProbe {
            probe: crate::ipc::probe_name_hash(point.name()),
            probe_label: point.name().to_string(),
            kind: probe_kind_str(point).to_string(),
        })
        .collect();
    let actions = entity
        .action_catalog()
        .into_iter()
        .map(|entry| CatalogAction {
            device: crate::ipc::HashedId::new(entry.device_hash),
            device_label: entry.device,
            action: crate::ipc::HashedId::new(entry.action_hash),
            action_label: entry.action,
            description: entry.description,
            params: entry.params.into_iter().map(catalog_param).collect(),
        })
        .collect();
    CatalogDump { probes, actions }
}

/// Build the entity via `build`, print probe/action catalogs as JSON, and return.
///
/// Does not open iceoryx or read topology. [`BoardPorts`] from `build` are ignored.
/// Entries are taken from [`Entity::action_catalog`] / [`ProbePoint`] as-is and are
/// not revalidated through the iceoryx advertise pack path.
pub fn list_catalog<E: Entity>(
    build: impl FnOnce() -> (E, BoardPorts, Vec<ProbePoint>),
) -> Result<(), BoardError> {
    let (entity, _ports, points) = build();
    let dump = catalog_dump_from_entity(&entity, &points);
    let text = serde_json::to_string(&dump).map_err(|err| BoardError::Message(err.to_string()))?;
    println!("{text}");
    Ok(())
}

/// Map a simulator stop to a cluster [`HostStopReason`], if any.
///
/// [`StopReason::Halt`] is guest-local (sim stays Running) and must not be forwarded.
#[must_use]
pub fn cluster_host_stop_reason(reason: &StopReason) -> Option<HostStopReason> {
    match reason {
        StopReason::Halt => None,
        StopReason::Breakpoint { .. } => Some(HostStopReason::Breakpoint),
        StopReason::ExternalStop => Some(HostStopReason::ExternalStop),
        StopReason::Step => Some(HostStopReason::Step),
        StopReason::Unmapped { .. } => Some(HostStopReason::Unmapped),
    }
}

/// Run the node framework with an entity built by `build`.
///
/// `build` assembles the entity (board machine or plant) and board-edge ports.
/// It must not load firmware. When the topology board entry has an `elf` source,
/// this function loads those bytes and calls [`Entity::load_firmware`].
/// An empty [`Entity::action_catalog`] skips [`service_actions`].
///
/// The third return value is the wiring-side [`ProbePoint`] list. This function
/// opens one publisher per point. Probe/action catalogs are listed out-of-band by
/// the arbiter via [`list_catalog`] / `--list-catalog`, not over control IPC.
///
/// Startup order: control open → [`open_data_planes`] → `build` → firmware/reset →
/// [`bind_data_planes`] → loop `pump_rx` / guest poll / `pump_tx`.
pub fn run_node<E: Entity>(
    opts: BoardOptions,
    build: impl FnOnce() -> (E, BoardPorts, Vec<ProbePoint>),
) -> Result<(), BoardError> {
    let board_name = opts.board_name.as_str();
    let board_hash = board_id_hash(board_name);
    let text = fs::read_to_string(&opts.topology_path)?;
    let topo = LogicalTopology::from_json_str(&text)?;
    let hashes = crate::topology::build_topology_hashes(&topo)
        .map_err(|err| BoardError::Message(err.to_string()))?;
    let board = find_board(&topo, board_name)?;
    let boards = hashes.boards;

    // Shared-memory virtual-time clock created by the arbiter.
    let vt = VtClock::open(&opts.iox_root, topo.boards.len())?;
    if !(1..=topo.boards.len()).contains(&opts.vt_slot) {
        return Err(BoardError::Message(format!(
            "--vt-slot {} out of range 1..={}",
            opts.vt_slot,
            topo.boards.len()
        )));
    }

    let config = isolated_config(&opts.iox_root)?;
    let iox_node = create_node(&config, &format!("node-{board_name}-{}", opts.cluster_key))?;
    let control = NodeControl::open(&iox_node, &opts.cluster_key, boards.clone())?;
    let actions = NodeActions::open(&iox_node, &opts.cluster_key, topo.boards.len())?;
    let _node_log = NodeLog::start(&opts.iox_root, &opts.cluster_key, board_hash, boards.len())?;
    let plane_maps = open_data_planes(&iox_node, &opts.cluster_key, board_name, board, &topo)?;

    let (entity, ports, probe_points) = build();
    let mut probes = NodeProbeRuntime::open(
        &iox_node,
        &opts.cluster_key,
        board_hash,
        board_name,
        topo.boards.len(),
        probe_points,
    )?;
    let mut sim = Simulator::new(entity, SimConfig::default());
    if let Some(elf) = &board.elf {
        let image = elf_source::load(elf)?;
        sim.entity_mut().load_firmware(&image)?;
        log::info!(
            "loaded firmware {} ({} bytes)",
            elf_source::describe(elf),
            image.len()
        );
    }
    sim.entity_mut().reset();
    ports.clear_pending();

    let mut inspect = if board.kind == "rl78" {
        Some(InspectServer::bind(&opts.iox_root, board_hash)?)
    } else {
        None
    };

    let mut data_planes = bind_data_planes(plane_maps, &ports)?;

    // Process init above is the Booting phase; the control loop starts Stopped.
    let mut state = NodeState::Stopped;
    let has_actions = !sim.entity().action_catalog().is_empty();
    let mut pending_actions: VecDeque<ActionRequest> = VecDeque::new();
    let mut last_allowed: Option<u64> = None;

    // TODO: exit when ControlToNode gains a Shutdown (arbiter currently OS-kills).
    loop {
        while let Some(msg) = control.try_recv()? {
            let prev = state;
            let (next, effect) = state.on_message(msg);
            if let Some(effect) = effect {
                match effect {
                    NodeEffect::ReportState => {
                        control.publish(&ControlToArbiter::State {
                            from: board_hash,
                            state: next.to_wire(),
                        })?;
                        log::trace!("State {next:?}");
                    }
                    NodeEffect::Reset => {
                        pending_actions.clear();
                        sim.entity_mut().reset();
                        // Rewrite this board's slot immediately so peers do not
                        // keep a stale virtual time after Reset.
                        vt.write_board_vt(opts.vt_slot, sim.entity().now().0);
                        last_allowed = None;
                        log::info!("simulator Reset");
                        for plane in &mut data_planes {
                            plane.discard_rx()?;
                        }
                        log::info!("discarded queued rx");
                    }
                    NodeEffect::ProbeWatch {
                        board_hash,
                        probe_ids,
                    } => {
                        probes.set_watch(board_hash, &probe_ids);
                    }
                    NodeEffect::GdbStep { board_hash: target } => {
                        if target == board_hash {
                            perform_local_step(&mut sim, &control, board_hash, &vt, opts.vt_slot)?;
                        }
                    }
                    NodeEffect::Warn(msg) => {
                        log::warn!("{msg}");
                    }
                }
            }
            if next != state {
                log::trace!("{state:?} -> {next:?}");
            }
            state = next;
            if prev != NodeState::Running && state == NodeState::Running {
                publish_vt_and_maybe_refresh_allowed(
                    &mut sim,
                    &vt,
                    opts.vt_slot,
                    opts.margin_ns,
                    &mut last_allowed,
                );
                let _ = sim.command(Command::Start);
                probes.set_running(true);
                log::info!("simulator Start");
            } else if prev == NodeState::Running && state != NodeState::Running {
                let _ = sim.command(Command::Stop);
                probes.set_running(false);
                log::info!("simulator Stop");
            }
        }

        if let Some(inspect) = inspect.as_mut() {
            inspect.service(&mut sim)?;
        }
        if has_actions {
            service_actions(&actions, &mut sim, state, board_hash, &mut pending_actions)?;
        }
        probes.service_queries()?;
        probes.service_csv()?;

        if state == NodeState::Running {
            for plane in &mut data_planes {
                plane.pump_rx()?;
            }

            // Publish VT and refresh the allowed ceiling when the next quantum
            // could hit it, so a node blocked on the ceiling sees peers advance
            // without a fixed sleep.
            publish_vt_and_maybe_refresh_allowed(
                &mut sim,
                &vt,
                opts.vt_slot,
                opts.margin_ns,
                &mut last_allowed,
            );

            if sim.state() != SimState::Running || sim.waiting_on_allowed_ceiling() {
                // Do not pump_tx while the guest is not advancing: cross-board
                // UART must not race ahead of virtual-time sync (ceiling / stop).
                // Yield instead of sleeping so peers' VT updates are noticed promptly.
                thread::yield_now();
                continue;
            }

            let polled = sim.poll();
            // Publish the post-poll virtual time before any HostStop so peers
            // see it, and pick up the ceiling derived from the updated slots.
            publish_vt_and_maybe_refresh_allowed(
                &mut sim,
                &vt,
                opts.vt_slot,
                opts.margin_ns,
                &mut last_allowed,
            );
            match polled {
                Some(Response::Stopped(reason)) => {
                    let virtual_time_ns = sim.entity().now().0;
                    if let Some(host_reason) = cluster_host_stop_reason(&reason) {
                        control.publish(&ControlToArbiter::HostStop {
                            from: board_hash,
                            reason: host_reason,
                        })?;
                        log::info!("HostStop ({host_reason}) at vt={virtual_time_ns}");
                        // Stay in the loop: sim is Stopped so the next iteration
                        // skips guest work; this iteration still falls through to
                        // pump_tx below. ClusterStop moves NodeState to Stopped.
                    } else {
                        // Guest Halt is not surfaced as Response::Stopped anymore.
                        log::error!("unexpected non-host stop {reason:?}");
                    }
                }
                Some(other) => {
                    log::warn!("unexpected sim response {other:?}");
                }
                None => {}
            }

            for plane in &mut data_planes {
                plane.pump_tx()?;
            }
            probes.ingest_and_emit();
        }
    }
}

/// Drain the request inbox. Stopped nodes queue; Running nodes apply FIFO
/// (queued first) before the guest poll of this iteration.
///
/// [`ACTION_RECV_CAP`] is only how many samples the subscriber can hold;
/// anything past that is dropped by the transport.
fn service_actions<E: Entity>(
    actions: &NodeActions,
    sim: &mut Simulator<E>,
    state: NodeState,
    board_hash: crate::ipc::HashedId<u64>,
    pending: &mut VecDeque<ActionRequest>,
) -> Result<(), BoardError> {
    for _ in 0..ACTION_RECV_CAP {
        match actions.try_request()? {
            Some(req) if req.board_hash == board_hash => pending.push_back(req),
            Some(_) => {}
            None => break,
        }
    }
    if state != NodeState::Running {
        return Ok(());
    }
    let now = sim.entity().now();
    sim.entity_mut().poll_actions(now);

    while let Some(req) = pending.pop_front() {
        let now = sim.entity().now();
        let raw: Vec<(u64, f64)> = req
            .raw_params()
            .iter()
            .map(|(h, v)| (h.get(), *v))
            .collect();
        let status = match sim.entity_mut().apply_action(
            req.device_hash.get(),
            req.action_hash.get(),
            now,
            &raw,
        ) {
            Ok(()) => ActionStatus::Applied,
            Err(ActionFailure::NotFoundDevice) => ActionStatus::NotFoundDevice,
            Err(ActionFailure::NotFoundAction) => ActionStatus::NotFoundAction,
            Err(ActionFailure::Busy) => ActionStatus::Busy,
            Err(ActionFailure::BadParam) => ActionStatus::BadParam,
        };
        reply(actions, req.request_id, status)?;
    }
    Ok(())
}

fn reply(actions: &NodeActions, request_id: u64, status: ActionStatus) -> Result<(), BoardError> {
    actions.publish_upstream(&ActionUpstream::Result(ActionResult {
        request_id,
        status: status as u8,
    }))?;
    Ok(())
}

fn find_board<'a>(
    topo: &'a LogicalTopology,
    board_name: &str,
) -> Result<&'a BoardSpec, BoardError> {
    topo.boards
        .iter()
        .find(|b| b.id == board_name)
        .ok_or_else(|| BoardError::Message(format!("board name `{board_name}` not in topology")))
}

/// Write this board's virtual time to its shared-memory slot, then maybe apply
/// the recomputed allowed ceiling (`min(min(board slots) + margin_ns, ceiling)`).
fn publish_vt_and_maybe_refresh_allowed<E: Entity>(
    sim: &mut Simulator<E>,
    vt: &VtClock,
    vt_slot: usize,
    margin_ns: u64,
    last_allowed: &mut Option<u64>,
) {
    vt.write_board_vt(vt_slot, sim.entity().now().0);
    maybe_refresh_allowed(sim, vt, margin_ns, last_allowed);
}

/// Apply the allowed ceiling from shared memory when needed.
///
/// Always sets an initial ceiling. Afterwards, recomputes only when the next
/// quantum could reach the last ceiling, and skips `SetAllowed` when unchanged.
fn maybe_refresh_allowed<E: Entity>(
    sim: &mut Simulator<E>,
    vt: &VtClock,
    margin_ns: u64,
    last_allowed: &mut Option<u64>,
) {
    if let Some(last) = *last_allowed {
        if !sim.next_quantum_reaches_allowed(Tick(last)) {
            return;
        }
    }

    let allowed = vt.allowed_ns(margin_ns);
    if *last_allowed == Some(allowed) {
        return;
    }
    let _ = sim.command(Command::SetAllowed {
        tick: Tick(allowed),
    });
    *last_allowed = Some(allowed);
}

fn perform_local_step<E: Entity>(
    sim: &mut Simulator<E>,
    control: &NodeControl,
    board_hash: crate::ipc::HashedId<u64>,
    vt: &VtClock,
    vt_slot: usize,
) -> Result<(), BoardError> {
    match sim.command(Command::Step) {
        Response::Started => {}
        other => {
            log::warn!("gdb step command: {other:?}");
            return Ok(());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match sim.poll() {
            Some(Response::Stopped(reason)) => {
                let virtual_time_ns = sim.entity().now().0;
                vt.write_board_vt(vt_slot, virtual_time_ns);
                if let Some(host_reason) = cluster_host_stop_reason(&reason) {
                    control.publish(&ControlToArbiter::HostStop {
                        from: board_hash,
                        reason: host_reason,
                    })?;
                    log::info!("gdb HostStop ({host_reason}) at vt={virtual_time_ns}");
                }
                return Ok(());
            }
            Some(other) => {
                log::warn!("unexpected sim response during gdb step {other:?}");
                return Ok(());
            }
            None if Instant::now() >= deadline => {
                log::warn!("gdb step timed out");
                return Ok(());
            }
            None => thread::yield_now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{
        Addr, Breakpoint, Core, Cpu, EventCtl, HasMemoryMap, Machine, MapError, MemoryBus,
        MemoryMapBuilder, Quantum, RegId, Resettable, Rom, SimError, Tick,
    };

    #[test]
    fn halt_is_not_cluster_host_stop() {
        assert!(cluster_host_stop_reason(&StopReason::Halt).is_none());
    }

    #[test]
    fn breakpoint_maps_to_host_stop() {
        assert_eq!(
            cluster_host_stop_reason(&StopReason::Breakpoint {
                id: sim_kernel::BreakpointId(1)
            }),
            Some(HostStopReason::Breakpoint)
        );
    }

    #[test]
    fn unmapped_maps_to_host_stop() {
        assert_eq!(
            cluster_host_stop_reason(&StopReason::Unmapped {
                addr: 0,
                write: true
            }),
            Some(HostStopReason::Unmapped)
        );
    }

    #[test]
    fn parse_board_args_ok() {
        let opts = parse_board_args(
            "prog",
            [
                "--board-name".into(),
                "a".into(),
                "--cluster-key".into(),
                "k".into(),
                "--iox-root".into(),
                "/tmp/iox".into(),
                "--topology".into(),
                "/tmp/t.json".into(),
                "--margin-ns".into(),
                "1000".into(),
                "--vt-slot".into(),
                "2".into(),
            ],
        )
        .unwrap();
        assert!(!opts.list_catalog);
        assert_eq!(opts.board_name, "a");
        assert_eq!(opts.cluster_key, "k");
        assert_eq!(opts.margin_ns, 1000);
        assert_eq!(opts.vt_slot, 2);
    }

    #[test]
    fn parse_board_args_help() {
        assert!(matches!(
            parse_board_args("prog", ["--help".into()]),
            Err(BoardCliError::Help)
        ));
    }

    #[test]
    fn parse_board_args_list_catalog_alone() {
        let opts = parse_board_args("prog", ["--list-catalog".into()]).unwrap();
        assert!(opts.list_catalog);
    }

    #[test]
    fn parse_board_args_list_catalog_ignores_cluster_flags() {
        let opts = parse_board_args(
            "prog",
            [
                "--list-catalog".into(),
                "--board-name".into(),
                "a".into(),
                "--cluster-key".into(),
                "k".into(),
                "--iox-root".into(),
                "/tmp/iox".into(),
                "--topology".into(),
                "/tmp/t.json".into(),
            ],
        )
        .unwrap();
        assert!(opts.list_catalog);
        assert_eq!(opts.board_name, "a");
    }

    #[test]
    fn parse_board_args_list_catalog_rejects_unknown() {
        let err =
            parse_board_args("prog", ["--list-catalog".into(), "--bogus".into()]).unwrap_err();
        assert!(matches!(err, BoardCliError::Message(_)));
    }

    #[test]
    fn parse_board_args_requires_margin_and_vt_slot() {
        let err = parse_board_args(
            "prog",
            [
                "--board-name".into(),
                "a".into(),
                "--cluster-key".into(),
                "k".into(),
                "--iox-root".into(),
                "/tmp/iox".into(),
                "--topology".into(),
                "/tmp/t.json".into(),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, BoardCliError::Message(_)));
    }

    #[test]
    fn parse_board_args_rejects_bad_margin() {
        let err = parse_board_args(
            "prog",
            [
                "--board-name".into(),
                "a".into(),
                "--cluster-key".into(),
                "k".into(),
                "--iox-root".into(),
                "/tmp/iox".into(),
                "--topology".into(),
                "/tmp/t.json".into(),
                "--margin-ns".into(),
                "nope".into(),
                "--vt-slot".into(),
                "1".into(),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, BoardCliError::Message(_)));
    }

    #[test]
    fn parse_board_args_rejects_bad_vt_slot() {
        let err = parse_board_args(
            "prog",
            [
                "--board-name".into(),
                "a".into(),
                "--cluster-key".into(),
                "k".into(),
                "--iox-root".into(),
                "/tmp/iox".into(),
                "--topology".into(),
                "/tmp/t.json".into(),
                "--margin-ns".into(),
                "1000".into(),
                "--vt-slot".into(),
                "nope".into(),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, BoardCliError::Message(_)));
    }

    struct CatalogStub {
        entries: Vec<sim_kernel::ActionCatalogEntry>,
    }

    impl Entity for CatalogStub {
        fn poll(&mut self, _ctx: sim_kernel::AdvanceCtx) -> sim_kernel::EntityPoll {
            sim_kernel::EntityPoll::NoProgress
        }

        fn reset(&mut self) {}

        fn now(&self) -> Tick {
            Tick(0)
        }

        fn action_catalog(&self) -> Vec<sim_kernel::ActionCatalogEntry> {
            self.entries.clone()
        }
    }

    #[test]
    fn catalog_dump_matches_agreed_schema() {
        use sim_kernel::{ActionCatalogEntry, ParamSpec, ProbePolicy, ProbeTap};

        let uart = ProbeTap::new("uart0", ProbePolicy::OnChange);
        let dac = ProbeTap::new("dac0", ProbePolicy::OnChange);
        let points = vec![ProbePoint::Uart(uart), ProbePoint::Analog(dac)];
        let entity = CatalogStub {
            entries: vec![ActionCatalogEntry {
                device: "sw1".into(),
                device_hash: 1,
                action: "hold".into(),
                action_hash: 2,
                description: "hold the switch".into(),
                params: vec![ParamSpec {
                    name: "level".into(),
                    name_hash: 3,
                    unit: Some("V".into()),
                    description: Some("drive level".into()),
                    min: Some(0.0),
                    max: Some(5.0),
                    default: Some(0.0),
                }],
            }],
        };
        let dump = catalog_dump_from_entity(&entity, &points);
        let value = serde_json::to_value(&dump).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "probes": [
                    {
                        "probe": crate::ipc::probe_name_hash("uart0").to_string(),
                        "probe_label": "uart0",
                        "kind": "uart"
                    },
                    {
                        "probe": crate::ipc::probe_name_hash("dac0").to_string(),
                        "probe_label": "dac0",
                        "kind": "analog"
                    }
                ],
                "actions": [
                    {
                        "device": crate::ipc::HashedId::new(1).to_string(),
                        "device_label": "sw1",
                        "action": crate::ipc::HashedId::new(2).to_string(),
                        "action_label": "hold",
                        "description": "hold the switch",
                        "params": [
                            {
                                "name": crate::ipc::HashedId::new(3).to_string(),
                                "name_label": "level",
                                "unit": "V",
                                "description": "drive level",
                                "min": 0.0,
                                "max": 5.0,
                                "default": 0.0
                            }
                        ]
                    }
                ]
            })
        );
        let roundtrip = CatalogDump::parse_json(&serde_json::to_string(&dump).unwrap()).unwrap();
        assert_eq!(roundtrip, dump);
    }

    /// Minimal CPU for framework unit tests (mirrors ScriptedCpu halt/nop).
    struct FakeCpu {
        pc: u64,
        ops: Vec<FakeOp>,
        idx: usize,
    }

    #[derive(Clone)]
    enum FakeOp {
        Nop,
        Halt,
        Breakpoint,
    }

    impl FakeCpu {
        fn script(ops: Vec<FakeOp>) -> Self {
            Self { pc: 0, ops, idx: 0 }
        }
    }

    impl Resettable for FakeCpu {
        fn reset(&mut self, _bus: &mut MemoryBus) {
            self.pc = 0;
            self.idx = 0;
        }
    }

    impl Core for FakeCpu {
        fn load_firmware(
            &mut self,
            _bus: &mut MemoryBus,
            _image: &[u8],
        ) -> Result<(), FirmwareError> {
            Ok(())
        }

        fn run_quantum(&mut self, max_instructions: u32) -> Quantum {
            let mut instructions = 0u64;
            while instructions < u64::from(max_instructions) {
                let op = self.ops.get(self.idx).cloned();
                self.idx = self.idx.saturating_add(1);
                instructions += 1;
                self.pc = self.pc.wrapping_add(1);
                match op {
                    None | Some(FakeOp::Halt) => {
                        return Quantum {
                            instructions,
                            stop: Some(StopReason::Halt),
                        };
                    }
                    Some(FakeOp::Nop) => {}
                    Some(FakeOp::Breakpoint) => {
                        return Quantum {
                            instructions,
                            stop: Some(StopReason::Breakpoint {
                                id: sim_kernel::BreakpointId(1),
                            }),
                        };
                    }
                }
            }
            Quantum {
                instructions,
                stop: None,
            }
        }

        fn read_reg(&self, id: RegId) -> Result<u64, SimError> {
            if id.0 == 0 {
                Ok(self.pc)
            } else {
                Err(SimError::UnknownRegister(id))
            }
        }

        fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError> {
            if id.0 == 0 {
                self.pc = value;
                Ok(())
            } else {
                Err(SimError::UnknownRegister(id))
            }
        }

        fn pc(&self) -> Addr {
            self.pc
        }

        fn set_pc(&mut self, pc: Addr) {
            self.pc = pc;
        }

        fn sync_breakpoints(&mut self, _breakpoints: &[Breakpoint]) {}
    }

    impl HasMemoryMap for FakeCpu {
        fn memory_map(&self) -> Result<MemoryMapBuilder, MapError> {
            Ok(MemoryMapBuilder::new())
        }
    }

    impl Cpu for FakeCpu {
        type Core = Self;

        fn core(&self) -> &Self {
            self
        }

        fn core_mut(&mut self) -> &mut Self {
            self
        }

        fn instruction_period(&self) -> Option<Tick> {
            Some(Tick(1))
        }
    }

    fn fake_machine(ops: Vec<FakeOp>) -> Machine<FakeCpu> {
        let bus = MemoryMapBuilder::new()
            .map(0, Box::new(Rom::new(64)))
            .expect("map")
            .build()
            .unwrap();
        Machine::new(FakeCpu::script(ops), bus, EventCtl::new())
    }

    #[test]
    fn simulator_clock_advances_without_placeholder() {
        let mut sim = Simulator::new(fake_machine(vec![FakeOp::Nop; 100]), SimConfig::default());
        let _ = sim.command(Command::SetAllowed { tick: Tick(50) });
        let _ = sim.command(Command::Start);
        for _ in 0..5 {
            let _ = sim.poll();
        }
        assert!(sim.machine().clock().now().0 > 0);
        assert!(sim.machine().clock().now().0 <= 50);
    }

    #[test]
    fn halt_keeps_sim_running_without_host_reason() {
        let mut sim = Simulator::new(fake_machine(vec![FakeOp::Halt]), SimConfig::default());
        let _ = sim.command(Command::SetAllowed { tick: Tick(1_000) });
        let _ = sim.command(Command::Start);
        assert_eq!(sim.poll(), None);
        assert!(cluster_host_stop_reason(&StopReason::Halt).is_none());
        assert_eq!(sim.state(), SimState::Running);
    }

    #[test]
    fn breakpoint_is_cluster_relevant() {
        let mut sim = Simulator::new(fake_machine(vec![FakeOp::Breakpoint]), SimConfig::default());
        let _ = sim.command(Command::SetAllowed { tick: Tick(1_000) });
        let _ = sim.command(Command::Start);
        match sim.poll() {
            Some(Response::Stopped(reason)) => {
                assert_eq!(
                    cluster_host_stop_reason(&reason),
                    Some(HostStopReason::Breakpoint)
                );
            }
            other => panic!("expected breakpoint stop, got {other:?}"),
        }
    }

    #[test]
    fn load_firmware_ok_on_fake_cpu() {
        let mut machine = fake_machine(vec![FakeOp::Nop]);
        machine.load_firmware(b"ignored").unwrap();
    }
}

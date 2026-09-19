//! Shared board-process framework for cluster participation.
//!
//! Board binaries supply a machine factory; this module owns CLI parsing, IPC,
//! topology lookup, firmware file I/O, and the same-thread guest loop.

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use sim_kernel::{
    Command, Cpu, FirmwareError, Machine, Response, SimConfig, SimState, Simulator, StopReason,
    Tick,
};

use crate::control::{ControlToArbiter, ControlToNode, HostStopReason};
use crate::ipc::{
    IpcError, NodeControl, NodeUartPorts, board_hash_table, board_id_hash, create_node,
    isolated_config,
};
use crate::lifecycle::{NodeEffect, NodeState};
use crate::topology::{BoardSpec, LogicalTopology, TopologyError};

/// Idle sleep when not Running, waiting on the allowed ceiling, or guest-halted.
pub const BOARD_IDLE_POLL: Duration = Duration::from_millis(1);

/// Options for [`run_board`].
#[derive(Clone, Debug)]
pub struct BoardOptions {
    pub board_id: String,
    pub cluster_key: String,
    pub iox_root: PathBuf,
    pub topology_path: PathBuf,
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
    #[error("firmware error: {0}")]
    Firmware(#[from] FirmwareError),
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
pub fn parse_board_args(
    prog: &str,
    args: impl IntoIterator<Item = String>,
) -> Result<BoardOptions, BoardCliError> {
    let mut board_id: Option<String> = None;
    let mut cluster_key: Option<String> = None;
    let mut iox_root: Option<PathBuf> = None;
    let mut topology: Option<PathBuf> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(BoardCliError::Help),
            "--board-id" => {
                board_id = Some(iter.next().ok_or_else(|| {
                    BoardCliError::Message(format!("{prog}: --board-id requires a value"))
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
            other => {
                return Err(BoardCliError::Message(format!(
                    "{prog}: unknown argument {other}"
                )));
            }
        }
    }
    let (Some(board_id), Some(cluster_key), Some(iox_root), Some(topology_path)) =
        (board_id, cluster_key, iox_root, topology)
    else {
        return Err(BoardCliError::Message(format!(
            "{prog}: --board-id, --cluster-key, --iox-root, and --topology are required"
        )));
    };
    Ok(BoardOptions {
        board_id,
        cluster_key,
        iox_root,
        topology_path,
    })
}

/// Usage text for board binaries.
#[must_use]
pub fn board_usage(prog: &str) -> String {
    format!(
        "Usage: {prog} --board-id <id> --cluster-key <key> --iox-root <path> --topology <json>\n\
         \n\
         Spawned by cluster-arbiter. Starts AwaitingStartup, becomes Stopped after\n\
         StartupRecord (Ready), and runs only after Start.\n"
    )
}

/// Map a simulator stop to a cluster [`HostStopReason`], if any.
///
/// [`StopReason::Halt`] is guest-local and must not be forwarded.
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

/// Run the board framework with a machine built by `build`.
///
/// `build` assembles ROM/RAM (and peripherals) only — it must not load firmware.
/// When the topology board entry has an `elf` path, this function reads the file
/// and calls [`Machine::load_firmware`].
pub fn run_board<C: Cpu>(
    opts: BoardOptions,
    build: impl FnOnce() -> Machine<C>,
) -> Result<(), BoardError> {
    let board_id = opts.board_id.as_str();
    let board_hash = board_id_hash(board_id);
    let text = fs::read_to_string(&opts.topology_path)?;
    let topo = LogicalTopology::from_json_str(&text)?;
    let board = find_board(&topo, board_id)?;
    let boards =
        board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).map_err(BoardError::Message)?;

    let config = isolated_config(&opts.iox_root)?;
    let iox_node = create_node(&config, &format!("node-{board_id}-{}", opts.cluster_key))?;
    let control = NodeControl::open(&iox_node, &opts.cluster_key, boards)?;
    let uart = NodeUartPorts::open_for_board(&iox_node, &opts.cluster_key, board_id, &topo.edges)?;

    let mut sim = Simulator::new(build(), SimConfig::default());
    if let Some(elf_path) = &board.elf {
        let image = fs::read(elf_path)?;
        sim.machine_mut().load_firmware(&image)?;
        eprintln!("board[{board_id}]: loaded firmware {}", elf_path);
    }
    sim.machine_mut().reset();

    let mut state = NodeState::AwaitingStartup;
    let mut headroom_threshold_ns = 0_u64;
    let mut allowed_ns = 0_u64;
    let mut last_time_report: Option<u64> = None;
    let mut guest_started = false;

    // TODO: exit when ControlToNode gains a Shutdown (arbiter currently OS-kills).
    loop {
        while let Some(msg) = control.try_recv()? {
            if let ControlToNode::StartupRecord {
                headroom_threshold_ns: thr,
                ..
            } = msg
            {
                headroom_threshold_ns = thr;
            }
            let (next, effect) = state.on_message(msg);
            if let Some(effect) = effect {
                apply_effect(
                    board_id,
                    board_hash,
                    &control,
                    effect,
                    &mut allowed_ns,
                    &mut sim,
                )?;
            }
            if next != state {
                eprintln!("board[{board_id}]: {state:?} -> {next:?}");
            }
            state = next;
        }

        if state == NodeState::Running {
            if !guest_started {
                let _ = sim.command(Command::Start);
                guest_started = true;
                eprintln!("board[{board_id}]: simulator Start");
            }

            let drained = uart.drain_all(64)?;
            for (edge_id, frames) in drained {
                if !frames.is_empty() {
                    eprintln!(
                        "board[{board_id}]: uart recv {} frames on {edge_id}",
                        frames.len()
                    );
                }
            }

            if sim.waiting_on_allowed_ceiling() || sim.state() != SimState::Running {
                // Ceiling wait, or guest-local Halt left the sim Stopped.
                thread::sleep(BOARD_IDLE_POLL);
                continue;
            }

            match sim.poll() {
                Some(Response::Stopped(reason)) => {
                    let virtual_time_ns = sim.machine().clock().now().0;
                    maybe_time_report(
                        &control,
                        board_hash,
                        virtual_time_ns,
                        allowed_ns,
                        headroom_threshold_ns,
                        &mut last_time_report,
                    )?;
                    if let Some(host_reason) = cluster_host_stop_reason(&reason) {
                        control.publish(&ControlToArbiter::HostStop {
                            from: board_hash,
                            reason: host_reason,
                        })?;
                        eprintln!(
                            "board[{board_id}]: HostStop ({host_reason}) at vt={virtual_time_ns}"
                        );
                        return Ok(());
                    }
                    // Halt: keep NodeState::Running, do not notify arbiter.
                    eprintln!("board[{board_id}]: guest Halt (local) at vt={virtual_time_ns}");
                }
                Some(other) => {
                    eprintln!("board[{board_id}]: unexpected sim response {other:?}");
                }
                None => {
                    let virtual_time_ns = sim.machine().clock().now().0;
                    maybe_time_report(
                        &control,
                        board_hash,
                        virtual_time_ns,
                        allowed_ns,
                        headroom_threshold_ns,
                        &mut last_time_report,
                    )?;
                }
            }
        } else {
            thread::sleep(BOARD_IDLE_POLL);
        }
    }
}

fn find_board<'a>(topo: &'a LogicalTopology, board_id: &str) -> Result<&'a BoardSpec, BoardError> {
    topo.boards
        .iter()
        .find(|b| b.id == board_id)
        .ok_or_else(|| BoardError::Message(format!("board id `{board_id}` not in topology")))
}

fn apply_effect<C: Cpu>(
    board_id: &str,
    board_hash: u64,
    control: &NodeControl,
    effect: NodeEffect,
    allowed_ns: &mut u64,
    sim: &mut Simulator<C>,
) -> Result<(), BoardError> {
    match effect {
        NodeEffect::SendReady => {
            control.publish(&ControlToArbiter::Ready { from: board_hash })?;
            eprintln!("board[{board_id}]: Ready");
        }
        NodeEffect::SetAllowed { allowed_ns: next } => {
            *allowed_ns = next;
            let _ = sim.command(Command::SetAllowed { tick: Tick(next) });
            eprintln!("board[{board_id}]: Allowed={next}");
        }
        NodeEffect::Warn(msg) => {
            eprintln!("board[{board_id}]: warning: {msg}");
        }
    }
    Ok(())
}

fn maybe_time_report(
    control: &NodeControl,
    board_hash: u64,
    virtual_time_ns: u64,
    allowed_ns: u64,
    headroom_threshold_ns: u64,
    last_time_report: &mut Option<u64>,
) -> Result<(), BoardError> {
    let headroom = allowed_ns.saturating_sub(virtual_time_ns);
    if headroom < headroom_threshold_ns && *last_time_report != Some(virtual_time_ns) {
        control.publish(&ControlToArbiter::TimeReport {
            from: board_hash,
            virtual_time_ns,
        })?;
        *last_time_report = Some(virtual_time_ns);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{
        Addr, Breakpoint, EventCtl, MemoryBus, MemoryMapBuilder, Quantum, RegId, Resettable, Rom,
        SimError,
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
                "--board-id".into(),
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
        assert_eq!(opts.board_id, "a");
        assert_eq!(opts.cluster_key, "k");
    }

    #[test]
    fn parse_board_args_help() {
        assert!(matches!(
            parse_board_args("prog", ["--help".into()]),
            Err(BoardCliError::Help)
        ));
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

    impl Cpu for FakeCpu {
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

    fn fake_machine(ops: Vec<FakeOp>) -> Machine<FakeCpu> {
        let bus = MemoryMapBuilder::new()
            .map(0, Box::new(Rom::new(64)))
            .expect("map")
            .build();
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
    fn halt_leaves_sim_stopped_without_host_reason() {
        let mut sim = Simulator::new(fake_machine(vec![FakeOp::Halt]), SimConfig::default());
        let _ = sim.command(Command::SetAllowed { tick: Tick(1_000) });
        let _ = sim.command(Command::Start);
        let resp = sim.poll();
        match resp {
            Some(Response::Stopped(StopReason::Halt)) => {
                assert!(cluster_host_stop_reason(&StopReason::Halt).is_none());
                assert_eq!(sim.state(), SimState::Stopped);
            }
            other => panic!("expected Halt, got {other:?}"),
        }
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

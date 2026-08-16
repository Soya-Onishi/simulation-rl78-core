use std::time::Duration;

use crate::bus::{MemoryBus, MemoryMapBuilder, Ram, UnmappedPolicy};
use crate::clock::Tick;
use crate::command::{Command, InspectResult, Response, SimError};
use crate::cpu::RegId;
use crate::event::{EventCtl, EventCtx, SimEvent};
use crate::machine::Machine;
use crate::sim::{SimConfig, SimState, Simulator, spawn};
use crate::stop::StopReason;
use crate::testing::{ScriptOp, ScriptedCpu};

struct HaltAtFire;

impl SimEvent for HaltAtFire {
    fn fire(&mut self, ctx: &mut EventCtx<'_>) {
        ctx.stop = Some(StopReason::Halt);
    }
}

fn empty_machine(cpu: ScriptedCpu) -> Machine<ScriptedCpu> {
    Machine::new(cpu, MemoryBus::new(), EventCtl::new())
}

fn run_until_stop(sim: &mut Simulator<ScriptedCpu>) -> Response {
    sim.command(Command::Start);
    loop {
        if let Some(response) = sim.poll() {
            return response;
        }
        assert_eq!(sim.state(), SimState::Running);
    }
}

#[test]
fn start_stop_quit_are_idempotent_enough() {
    let mut sim = Simulator::new(empty_machine(ScriptedCpu::nops(8)), SimConfig::default());
    assert_eq!(sim.command(Command::Start), Response::Started);
    assert_eq!(sim.state(), SimState::Running);
    assert_eq!(
        sim.command(Command::Stop),
        Response::Stopped(StopReason::ExternalStop)
    );
    assert_eq!(sim.state(), SimState::Stopped);
    assert_eq!(sim.command(Command::Quit), Response::Quit);
    assert_eq!(sim.state(), SimState::Quit);
}

#[test]
fn inspect_rejected_while_running() {
    let mut sim = Simulator::new(empty_machine(ScriptedCpu::nops(8)), SimConfig::default());
    sim.command(Command::Start);
    assert_eq!(
        sim.command(Command::ReadReg { id: RegId(0) }),
        Response::Error(SimError::Running)
    );
}

#[test]
fn inspect_reg_and_mem_when_stopped() {
    let bus = MemoryMapBuilder::new()
        .map(0x2000, Box::new(Ram::new(16)))
        .unwrap()
        .build();
    let mut machine = Machine::new(ScriptedCpu::new(vec![]), bus, EventCtl::new());
    machine.write_reg(RegId(3), 0x55).unwrap();
    let mut sim = Simulator::new(machine, SimConfig::default());

    assert_eq!(
        sim.command(Command::ReadReg { id: RegId(3) }),
        Response::Inspect(InspectResult::Reg {
            id: RegId(3),
            value: 0x55
        })
    );
    assert_eq!(
        sim.command(Command::WriteMem {
            addr: 0x2002,
            data: vec![9, 8],
        }),
        Response::Inspect(InspectResult::Ok)
    );
    match sim.command(Command::ReadMem {
        addr: 0x2002,
        len: 2,
    }) {
        Response::Inspect(InspectResult::Mem { data, .. }) => assert_eq!(data, vec![9, 8]),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn scripted_cpu_halts() {
    let mut sim = Simulator::new(
        empty_machine(ScriptedCpu::new(vec![ScriptOp::Nop, ScriptOp::Halt])),
        SimConfig::default(),
    );
    assert_eq!(
        run_until_stop(&mut sim),
        Response::Stopped(StopReason::Halt)
    );
    assert_eq!(sim.machine().clock().now(), Tick(2));
}

#[test]
fn quantum_does_not_pass_next_event() {
    let mut machine = empty_machine(ScriptedCpu::nops(100));
    machine.events_mut().schedule(Tick(4), Box::new(HaltAtFire));
    let mut sim = Simulator::new(
        machine,
        SimConfig {
            max_quantum: Tick(50),
            ..SimConfig::default()
        },
    );
    sim.command(Command::Start);
    assert_eq!(sim.poll(), Some(Response::Stopped(StopReason::Halt)));
    assert_eq!(sim.machine().clock().now(), Tick(4));
}

#[test]
fn sub_instruction_remainder_advances_to_event() {
    // Event at 5ns with 10ns/insn: first poll cannot retire an instruction, but
    // must still advance to the deadline so the event fires (no spin).
    let mut machine = empty_machine(ScriptedCpu::nops(100));
    machine.events_mut().schedule(Tick(5), Box::new(HaltAtFire));
    let mut sim = Simulator::new(
        machine,
        SimConfig {
            ns_per_instruction: Tick(10),
            ..SimConfig::default()
        },
    );
    sim.command(Command::Start);
    assert_eq!(sim.poll(), Some(Response::Stopped(StopReason::Halt)));
    assert_eq!(sim.machine().clock().now(), Tick(5));
}

#[test]
fn mmio_write_reaches_ram() {
    let bus = MemoryMapBuilder::new()
        .map(0x8000, Box::new(Ram::new(8)))
        .unwrap()
        .build();
    let machine = Machine::new(
        ScriptedCpu::new(vec![ScriptOp::Write {
            addr: 0x8000,
            data: b"hi".to_vec(),
        }]),
        bus,
        EventCtl::new(),
    );
    let mut sim = Simulator::new(machine, SimConfig::default());
    assert_eq!(
        run_until_stop(&mut sim),
        Response::Stopped(StopReason::Halt)
    );
    let mut buf = [0u8; 2];
    sim.machine_mut().read_mem(0x8000, &mut buf).unwrap();
    assert_eq!(&buf, b"hi");
}

#[test]
fn unmapped_write_stops_when_cpu_reports_it() {
    let mut sim = Simulator::new(
        empty_machine(ScriptedCpu::new(vec![ScriptOp::Write {
            addr: 0xFFFF,
            data: vec![1],
        }])),
        SimConfig::default(),
    );
    assert_eq!(
        run_until_stop(&mut sim),
        Response::Stopped(StopReason::Unmapped {
            addr: 0xFFFF,
            write: true
        })
    );
}

#[test]
fn trap_policy_stops_even_if_cpu_continues() {
    let bus = MemoryMapBuilder::new().policy(UnmappedPolicy::Trap).build();
    let machine = Machine::new(
        ScriptedCpu::new(vec![ScriptOp::WriteIgnoreError {
            addr: 0x1,
            data: vec![0],
        }]),
        bus,
        EventCtl::new(),
    );
    let mut sim = Simulator::new(machine, SimConfig::default());
    assert_eq!(
        run_until_stop(&mut sim),
        Response::Stopped(StopReason::Unmapped {
            addr: 0x1,
            write: true
        })
    );
}

#[test]
fn breakpoint_reported_by_cpu_stop_reason() {
    let mut sim = Simulator::new(
        empty_machine(ScriptedCpu::new(vec![ScriptOp::SetPc(0x100)])),
        SimConfig {
            max_quantum: Tick(1),
            ..SimConfig::default()
        },
    );
    let Response::Inspect(InspectResult::Breakpoint { id }) =
        sim.command(Command::AddBreakpoint { addr: 0x100 })
    else {
        panic!("expected breakpoint id");
    };
    assert_eq!(
        run_until_stop(&mut sim),
        Response::Stopped(StopReason::Breakpoint { id })
    );
}

#[test]
fn ns_per_instruction_scales_virtual_time() {
    let mut sim = Simulator::new(
        empty_machine(ScriptedCpu::new(vec![
            ScriptOp::Nop,
            ScriptOp::Nop,
            ScriptOp::Halt,
        ])),
        SimConfig {
            ns_per_instruction: Tick(10),
            ..SimConfig::default()
        },
    );
    assert_eq!(
        run_until_stop(&mut sim),
        Response::Stopped(StopReason::Halt)
    );
    // 3 instructions × 10 ns
    assert_eq!(sim.machine().clock().now(), Tick(30));
}

#[test]
fn stays_halted_until_start() {
    let mut sim = Simulator::new(empty_machine(ScriptedCpu::nops(8)), SimConfig::default());
    assert_eq!(sim.poll(), None);
    assert_eq!(sim.state(), SimState::Stopped);
    assert_eq!(sim.machine().clock().now(), Tick(0));
}

#[test]
fn step_runs_one_instruction_then_stops() {
    let mut sim = Simulator::new(empty_machine(ScriptedCpu::nops(8)), SimConfig::default());
    assert_eq!(sim.command(Command::Step), Response::Started);
    let mut response = None;
    for _ in 0..8 {
        if let Some(r) = sim.poll() {
            response = Some(r);
            break;
        }
    }
    assert_eq!(response, Some(Response::Stopped(StopReason::Step)));
    assert_eq!(sim.state(), SimState::Stopped);
    assert_eq!(sim.machine().clock().now(), Tick(1));
}

#[test]
fn notify_halt_does_not_change_state() {
    let mut sim = Simulator::new(empty_machine(ScriptedCpu::nops(8)), SimConfig::default());
    assert_eq!(
        sim.command(Command::NotifyHalt {
            reason: StopReason::Halt
        }),
        Response::Inspect(InspectResult::Ok)
    );
    assert_eq!(sim.state(), SimState::Stopped);
    assert_eq!(sim.poll(), None);
}

#[test]
fn spawn_start_stop_quit() {
    let (ctrl, events) = spawn(
        empty_machine(ScriptedCpu::nops(1_000_000)),
        SimConfig {
            max_quantum: Tick(64),
            ..SimConfig::default()
        },
    );
    ctrl.start().unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_secs(1)).unwrap(),
        Response::Started
    );
    ctrl.stop().unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_secs(1)).unwrap(),
        Response::Stopped(StopReason::ExternalStop)
    );
    ctrl.quit().unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_secs(1)).unwrap(),
        Response::Quit
    );
}

use std::time::Duration;

use crate::bus::{MemoryBus, MemoryMapBuilder, Ram, UnmappedPolicy};
use crate::clock::Tick;
use crate::command::{Command, InspectResult, Response, SimError};
use crate::cpu::RegId;
use crate::event::{EventCtx, SimEvent};
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
    Machine::new(cpu, MemoryBus::new())
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
    let mut machine = Machine::new(ScriptedCpu::new(vec![]), bus);
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
        },
    );
    sim.command(Command::Start);
    assert!(sim.poll().is_none());
    assert_eq!(sim.machine().clock().now(), Tick(4));
    assert_eq!(sim.poll(), Some(Response::Stopped(StopReason::Halt)));
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
fn spawn_start_stop_quit() {
    let (ctrl, events) = spawn(
        empty_machine(ScriptedCpu::nops(1_000_000)),
        SimConfig {
            max_quantum: Tick(64),
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

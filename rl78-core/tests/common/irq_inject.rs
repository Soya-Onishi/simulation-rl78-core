//! INTC → tlib IRQ injection helpers (issue #15).

#![allow(dead_code)]

use rl78_core::IrqId;
use sim_kernel::{Cpu, EventCtx, Machine, RegId, Tick};

pub const REG_PC: RegId = RegId(8);
pub const REG_SP: RegId = RegId(9);
pub const REG_IE: RegId = RegId(17);

pub const ISR_TM00: u64 = 0x0200;
pub const ISR_ST0: u64 = 0x0300;
pub const MAIN: u64 = 0x0100;
pub const SP: u64 = 0xFE00;

pub fn fire_due(machine: &mut Machine<rl78_core::Rl78Cpu>) {
    loop {
        let now = machine.clock().now();
        let due = machine.events_mut().pop_due(now);
        let Some((_, mut event)) = due else {
            break;
        };
        let mut ctx = EventCtx {
            now,
            bus: machine.bus_mut(),
            stop: None,
        };
        event.fire(&mut ctx);
    }
}

pub fn load_vectors_and_idle(machine: &mut Machine<rl78_core::Rl78Cpu>) {
    let mut image = vec![0u8; 0x302];
    image[0] = MAIN as u8;
    image[1] = (MAIN >> 8) as u8;
    let v_st0 = 4 + usize::from(IrqId::INTST0.index()) * 2;
    let v_tm00 = 4 + usize::from(IrqId::INTTM00.index()) * 2;
    image[v_st0] = ISR_ST0 as u8;
    image[v_st0 + 1] = (ISR_ST0 >> 8) as u8;
    image[v_tm00] = ISR_TM00 as u8;
    image[v_tm00 + 1] = (ISR_TM00 >> 8) as u8;
    image[MAIN as usize] = 0xef;
    image[MAIN as usize + 1] = 0xfe;
    image[ISR_TM00 as usize] = 0xef;
    image[ISR_TM00 as usize + 1] = 0xfe;
    image[ISR_ST0 as usize] = 0xef;
    image[ISR_ST0 as usize + 1] = 0xfe;
    machine.bus_mut().load(0, &image).unwrap();
}

pub fn arm_cpu(machine: &mut Machine<rl78_core::Rl78Cpu>) {
    machine.cpu_mut().set_pc(MAIN);
    machine.write_reg(REG_SP, SP).unwrap();
    machine.write_reg(REG_IE, 1).unwrap();
}

pub fn if0(machine: &mut Machine<rl78_core::Rl78Cpu>) -> u16 {
    let mut buf = [0u8; 2];
    machine.bus_mut().read(0xFFFE0, &mut buf).unwrap();
    u16::from_le_bytes(buf)
}

pub fn expire_tau0(machine: &mut Machine<rl78_core::Rl78Cpu>) {
    machine.bus_mut().write(0xFFF18, &[31, 0]).unwrap();
    machine.bus_mut().write(0xF01B2, &[0x01, 0x00]).unwrap();
    fire_due(machine);
    machine.advance_clock(Tick(1000));
    fire_due(machine);
}

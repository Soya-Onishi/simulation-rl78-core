//! TAU overflow with MK clear → INTTM00 ISR (own process — tlib after execute).

mod common;

use common::irq_inject::{ISR_TM00, REG_PC, arm_cpu, expire_tau0, if0, load_vectors_and_idle};
use rl78_core::{G23MachineConfig, g23_machine};
use sim_kernel::Cpu;

#[test]
fn tau_interval_unmasked_enters_inttm00_isr() {
    let mut machine = g23_machine(G23MachineConfig::default());
    load_vectors_and_idle(&mut machine);
    arm_cpu(&mut machine);
    machine.bus_mut().write(0xFFFE4, &[0xFF, 0xBF]).unwrap();
    expire_tau0(&mut machine);
    assert_eq!(if0(&mut machine) & 0x4000, 0x4000);

    let _ = machine.cpu_mut().run_quantum(8);
    assert_eq!(machine.read_reg(REG_PC).unwrap(), ISR_TM00);
    assert_eq!(if0(&mut machine) & 0x4000, 0);
}

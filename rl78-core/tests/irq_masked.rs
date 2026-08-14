//! Masked TAU sets IF only; CPU does not take the vector (own process).

mod common;

use common::irq_inject::{MAIN, REG_PC, arm_cpu, expire_tau0, if0, load_vectors_and_idle};
use rl78_core::{G23MachineConfig, g23_machine};
use sim_kernel::Cpu;

#[test]
fn masked_tau_sets_if_but_does_not_enter_isr() {
    let mut machine = g23_machine(G23MachineConfig::default());
    load_vectors_and_idle(&mut machine);
    arm_cpu(&mut machine);
    expire_tau0(&mut machine);
    assert_eq!(if0(&mut machine) & 0x4000, 0x4000);

    let _ = machine.cpu_mut().run_quantum(8);
    assert_eq!(machine.read_reg(REG_PC).unwrap(), MAIN);
    assert_eq!(if0(&mut machine) & 0x4000, 0x4000);
}

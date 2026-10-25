//! Masked TAU sets IF only; CPU does not take the vector (own process).

mod common;

use common::irq_inject::{
    MAIN, REG_PC, arm_cpu, expire_tau0, g23_test_machine, if0, load_vectors_and_idle,
};
use sim_kernel::{Core, Cpu};

#[test]
fn masked_tau_sets_if_but_does_not_enter_isr() {
    let mut machine = g23_test_machine();
    load_vectors_and_idle(&mut machine);
    machine.reset();
    arm_cpu(&mut machine);
    expire_tau0(&mut machine);
    assert_eq!(if0(&mut machine) & 0x4000, 0x4000);

    let _ = machine.cpu_mut().core_mut().run_quantum(8);
    assert_eq!(machine.read_reg(REG_PC).unwrap(), MAIN);
    assert_eq!(if0(&mut machine) & 0x4000, 0x4000);
}

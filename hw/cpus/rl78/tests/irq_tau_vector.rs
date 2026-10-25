//! TAU overflow with MK clear → INTTM00 ISR (own process — tlib after execute).

mod common;

use common::irq_inject::{
    ISR_TM00, REG_PC, arm_cpu, expire_tau0, g23_test_machine, if0, load_vectors_and_idle,
};
use sim_kernel::{Core, Cpu};

#[test]
fn tau_interval_unmasked_enters_inttm00_isr() {
    let mut machine = g23_test_machine();
    load_vectors_and_idle(&mut machine);
    machine.reset();
    arm_cpu(&mut machine);
    machine.bus_mut().write(0xFFFE4, &[0xFF, 0xBF]).unwrap();
    expire_tau0(&mut machine);
    assert_eq!(if0(&mut machine) & 0x4000, 0x4000);

    let _ = machine.cpu_mut().core_mut().run_quantum(8);
    assert_eq!(machine.read_reg(REG_PC).unwrap(), ISR_TM00);
    assert_eq!(if0(&mut machine) & 0x4000, 0);
}

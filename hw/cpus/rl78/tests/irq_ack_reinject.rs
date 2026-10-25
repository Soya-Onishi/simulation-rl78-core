//! IRQ ack clears IF and the next pending line is reinjected (own process).

mod common;

use common::irq_inject::{
    ISR_ST0, ISR_TM00, REG_IE, REG_PC, arm_cpu, g23_test_machine, if0, load_vectors_and_idle,
};
use sim_kernel::{Core, Cpu};

#[test]
fn ack_clears_if_and_reinjects_next_pending() {
    let mut machine = g23_test_machine();
    load_vectors_and_idle(&mut machine);
    machine.reset();
    arm_cpu(&mut machine);
    machine.bus_mut().write(0xFFFE4, &[0xFF, 0x9F]).unwrap();
    machine.bus_mut().write(0xFFFE0, &[0x00, 0x60]).unwrap();

    let _ = machine.cpu_mut().core_mut().run_quantum(8);
    assert_eq!(machine.read_reg(REG_PC).unwrap(), ISR_ST0);
    let flags = if0(&mut machine);
    assert_eq!(flags & 0x2000, 0);
    assert_eq!(flags & 0x4000, 0x4000);

    machine.write_reg(REG_IE, 1).unwrap();
    let _ = machine.cpu_mut().core_mut().run_quantum(8);
    assert_eq!(machine.read_reg(REG_PC).unwrap(), ISR_TM00);
    assert_eq!(if0(&mut machine) & 0x4000, 0);
}

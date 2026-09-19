//! Guest Magic probe smoke (own process — tlib is unsafe after guest execute).

mod common;

use std::sync::{Arc, Mutex};

use common::{magic_probe_guest_code, write_minimal_elf32};
use rl78_core::{
    MinimalMachineConfig, ProbeSink, load_elf_into_machine, minimal_machine_with_probe,
};
use sim_kernel::Cpu;

#[derive(Clone, Default)]
struct BufferSink {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl ProbeSink for BufferSink {
    fn emit(&mut self, bytes: &[u8]) {
        self.buf
            .lock()
            .expect("probe buffer")
            .extend_from_slice(bytes);
    }
}

#[test]
fn guest_writes_magic_probe() {
    let sink = BufferSink::default();
    let captured = Arc::clone(&sink.buf);
    let mut machine = minimal_machine_with_probe(MinimalMachineConfig::default(), sink);
    let code = magic_probe_guest_code(b"Hi\n");
    let entry = 0x100u32;
    let mut payload = vec![0xFFu8; entry as usize + code.len()];
    payload[0] = (entry & 0xff) as u8;
    payload[1] = (entry >> 8) as u8;
    payload[entry as usize..].copy_from_slice(&code);
    let image = write_minimal_elf32(0, entry, &payload);
    load_elf_into_machine(&image, &mut machine).unwrap();
    machine.reset();

    for _ in 0..32 {
        let q = machine.cpu_mut().run_quantum(16);
        if captured.lock().unwrap().len() >= 3 {
            break;
        }
        if q.instructions == 0 {
            break;
        }
    }
    assert_eq!(&captured.lock().unwrap()[..], b"Hi\n");
}

//! Shared fixtures for rl78-core integration tests.

/// ELF `e_machine` value for Renesas RL78 (`EM_RL78`).
pub const EM_RL78: u16 = 197;

/// Build a minimal little-endian ELF32 (ET_EXEC) with one PT_LOAD segment.
pub fn write_minimal_elf32(load_addr: u32, entry: u32, payload: &[u8]) -> Vec<u8> {
    const EH_SIZE: usize = 52;
    const PH_SIZE: usize = 32;
    let mut out = vec![0u8; EH_SIZE + PH_SIZE + payload.len()];

    out[0..4].copy_from_slice(b"\x7fELF");
    out[4] = 1; // ELFCLASS32
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT

    out[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_EXEC
    out[18..20].copy_from_slice(&EM_RL78.to_le_bytes());
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[24..28].copy_from_slice(&entry.to_le_bytes());
    out[28..32].copy_from_slice(&(EH_SIZE as u32).to_le_bytes());
    out[40..42].copy_from_slice(&(EH_SIZE as u16).to_le_bytes());
    out[42..44].copy_from_slice(&(PH_SIZE as u16).to_le_bytes());
    out[44..46].copy_from_slice(&1u16.to_le_bytes());

    let ph = &mut out[EH_SIZE..EH_SIZE + PH_SIZE];
    ph[0..4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    ph[4..8].copy_from_slice(&((EH_SIZE + PH_SIZE) as u32).to_le_bytes());
    ph[8..12].copy_from_slice(&load_addr.to_le_bytes());
    ph[12..16].copy_from_slice(&load_addr.to_le_bytes());
    let sz = payload.len() as u32;
    ph[16..20].copy_from_slice(&sz.to_le_bytes());
    ph[20..24].copy_from_slice(&sz.to_le_bytes());
    ph[24..28].copy_from_slice(&7u32.to_le_bytes()); // RWX
    ph[28..32].copy_from_slice(&1u32.to_le_bytes());

    out[EH_SIZE + PH_SIZE..].copy_from_slice(payload);
    out
}

/// RL78 guest bytes: write `msg` to Magic probe via `MOV !addr16, #imm`, then `BR $`.
pub fn magic_probe_guest_code(msg: &[u8]) -> Vec<u8> {
    let mut code = Vec::with_capacity(msg.len() * 4 + 2);
    for (i, byte) in msg.iter().enumerate() {
        let addr = i as u16;
        code.push(0xcf); // MOV ABS16, IMM8
        code.extend_from_slice(&addr.to_le_bytes());
        code.push(*byte);
    }
    code.push(0xef); // BR REL8
    code.push(0xfe); // rel = -2 → branch to self
    code
}

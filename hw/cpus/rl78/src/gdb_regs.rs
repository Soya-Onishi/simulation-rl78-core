//! GNU `rl78-tdep` `g`/`G` packet layout assembled from tlib's 18 inspect regs.
//!
//! Other register banks are filled with 0. PMC and MEM are 0.

/// Raw GDB `g` packet length (GNU RL78 tdep).
pub const GDB_G_PACKET_LEN: usize = 43;

const REG_X: usize = 0;
const REG_PC: usize = 8;
const REG_SP: usize = 9;
const REG_ES: usize = 10;
const REG_CS: usize = 11;
const REG_PSW_CY: usize = 12;
const REG_PSW_ISP: usize = 13;
const REG_PSW_RBS: usize = 14;
const REG_PSW_AC: usize = 15;
const REG_PSW_Z: usize = 16;
const REG_PSW_IE: usize = 17;

/// Pack tlib inspect regs (`RegId` 0..17) into a GNU RL78 `g` packet.
#[must_use]
pub fn pack_gdb_g(values: [u32; 18]) -> [u8; GDB_G_PACKET_LEN] {
    let mut out = [0u8; GDB_G_PACKET_LEN];
    let rbs = (values[REG_PSW_RBS] as u8) & 3;
    let bank_base = usize::from(rbs) * 8;
    for (gpr, idx) in (REG_X..=7).enumerate() {
        out[bank_base + gpr] = values[idx] as u8;
    }
    out[32] = pack_psw(values);
    out[33] = values[REG_ES] as u8;
    out[34] = values[REG_CS] as u8;
    out[35..39].copy_from_slice(&values[REG_PC].to_le_bytes());
    let sp = values[REG_SP];
    out[39] = sp as u8;
    out[40] = (sp >> 8) as u8;
    out
}

/// Unpack a GNU RL78 `G` packet into tlib inspect regs (current bank only).
pub fn unpack_gdb_g(bytes: &[u8]) -> Result<[u32; 18], ()> {
    if bytes.len() != GDB_G_PACKET_LEN {
        return Err(());
    }
    let psw = bytes[32];
    let rbs = unpack_rbs(psw);
    let bank_base = usize::from(rbs) * 8;
    let mut values = [0u32; 18];
    for gpr in 0..8 {
        values[gpr] = u32::from(bytes[bank_base + gpr]);
    }
    values[REG_PC] = u32::from_le_bytes(bytes[35..39].try_into().map_err(|_| ())?);
    values[REG_SP] = u32::from(bytes[39]) | (u32::from(bytes[40]) << 8);
    values[REG_ES] = u32::from(bytes[33]);
    values[REG_CS] = u32::from(bytes[34]);
    values[REG_PSW_CY] = u32::from(psw & 1);
    values[REG_PSW_ISP] = u32::from((psw >> 1) & 3);
    values[REG_PSW_RBS] = u32::from(rbs);
    values[REG_PSW_AC] = u32::from((psw >> 4) & 1);
    values[REG_PSW_Z] = u32::from((psw >> 6) & 1);
    values[REG_PSW_IE] = u32::from((psw >> 7) & 1);
    Ok(values)
}

fn pack_psw(values: [u32; 18]) -> u8 {
    let cy = (values[REG_PSW_CY] != 0) as u8;
    let isp = (values[REG_PSW_ISP] as u8) & 3;
    let rbs = (values[REG_PSW_RBS] as u8) & 3;
    let ac = (values[REG_PSW_AC] != 0) as u8;
    let z = (values[REG_PSW_Z] != 0) as u8;
    let ie = (values[REG_PSW_IE] != 0) as u8;
    cy | (isp << 1) | ((rbs & 1) << 3) | (ac << 4) | (((rbs >> 1) & 1) << 5) | (z << 6) | (ie << 7)
}

fn unpack_rbs(psw: u8) -> u8 {
    ((psw >> 3) & 1) | ((psw >> 4) & 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_puts_current_bank_gprs_and_pc() {
        let mut values = [0u32; 18];
        values[REG_X] = 0x11;
        values[1] = 0x22;
        values[REG_PC] = 0x0001_2345;
        values[REG_SP] = 0xF800;
        values[REG_PSW_RBS] = 2;
        values[REG_PSW_IE] = 1;
        let pkt = pack_gdb_g(values);
        assert_eq!(pkt[16], 0x11);
        assert_eq!(pkt[17], 0x22);
        assert_eq!(&pkt[35..39], &[0x45, 0x23, 0x01, 0x00]);
        assert_eq!(pkt[39], 0x00);
        assert_eq!(pkt[40], 0xF8);
        assert_eq!(pkt[32] & 0x80, 0x80);
        assert_eq!(unpack_rbs(pkt[32]), 2);
    }

    #[test]
    fn round_trip_current_bank() {
        let mut values = [0u32; 18];
        for (i, slot) in values.iter_mut().enumerate() {
            *slot = (i as u32) & 0xff;
        }
        values[REG_PC] = 0xABCDE;
        values[REG_SP] = 0x1234;
        values[REG_PSW_RBS] = 1;
        values[REG_PSW_ISP] = 2;
        let back = unpack_gdb_g(&pack_gdb_g(values)).unwrap();
        assert_eq!(&back[0..8], &values[0..8]);
        assert_eq!(back[REG_PC], values[REG_PC]);
        assert_eq!(back[REG_SP], values[REG_SP]);
        assert_eq!(back[REG_PSW_RBS], 1);
        assert_eq!(back[REG_PSW_ISP], 2);
    }
}

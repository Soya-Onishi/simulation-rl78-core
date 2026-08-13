//! ELF loader for guest images (ELF only; no bin/mot).
//!
//! Parsing uses the [`object`] crate (`PT_LOAD` segments).

use object::{Endianness, Object, ObjectSegment};
use sim_kernel::{Cpu, Machine, MemoryBus};

use crate::Rl78Cpu;

/// ELF `e_machine` value for Renesas RL78 (`EM_RL78`).
pub const EM_RL78: u16 = 197;

/// Result of a successful load (entry point and such).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfLoad {
    pub entry: u64,
}

/// ELF load failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    NotElf,
    Unsupported(&'static str),
    Truncated,
    Bus(sim_kernel::BusError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotElf => write!(f, "image is not an ELF file"),
            Self::Unsupported(msg) => write!(f, "unsupported ELF: {msg}"),
            Self::Truncated => write!(f, "ELF image truncated"),
            Self::Bus(err) => write!(f, "bus error while loading ELF: {err}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Load a guest ELF into `bus` (PT_LOAD segments only).
pub fn load_elf(image: &[u8], bus: &mut MemoryBus) -> Result<ElfLoad, LoadError> {
    let file = object::File::parse(image).map_err(|_| {
        if image.len() < 4 || &image[0..4] != b"\x7fELF" {
            LoadError::NotElf
        } else {
            LoadError::Truncated
        }
    })?;
    match file {
        object::File::Elf32(_) => {}
        _ => return Err(LoadError::Unsupported("only ELF32 is supported")),
    }
    if file.endianness() != Endianness::Little {
        return Err(LoadError::Unsupported("ELF must be little-endian"));
    }

    let mut loaded = false;
    for segment in file.segments() {
        let addr = segment.address();
        let data = segment.data().map_err(|_| LoadError::Truncated)?;
        let mem_size = usize::try_from(segment.size()).map_err(|_| LoadError::Truncated)?;
        if data.is_empty() && mem_size == 0 {
            continue;
        }
        if !data.is_empty() {
            bus.load(addr, data).map_err(LoadError::Bus)?;
        }
        if mem_size > data.len() {
            let zeros = vec![0u8; mem_size - data.len()];
            bus.load(addr + data.len() as u64, &zeros)
                .map_err(LoadError::Bus)?;
        }
        loaded = true;
    }

    if !loaded {
        return Err(LoadError::Unsupported("ELF has no PT_LOAD segments"));
    }

    Ok(ElfLoad {
        entry: file.entry(),
    })
}

/// Load `image` into `machine` and set the CPU PC to the ELF entry.
pub fn load_elf_into_machine(
    image: &[u8],
    machine: &mut Machine<Rl78Cpu>,
) -> Result<ElfLoad, LoadError> {
    let loaded = load_elf(image, machine.bus_mut())?;
    // ROM bytes changed under an already-bound host map; drop stale TBs.
    unsafe { crate::ffi::tlib_invalidate_translation_cache() };
    machine.cpu_mut().set_pc(loaded.entry);
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MinimalMachineConfig, minimal_machine};
    use serial_test::serial;

    /// Build a minimal little-endian ELF32 (ET_EXEC) with one PT_LOAD segment.
    fn write_minimal_elf32(load_addr: u32, entry: u32, payload: &[u8]) -> Vec<u8> {
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

    #[serial]
    #[test]
    fn load_elf_writes_payload_and_sets_entry() {
        let payload = [0x00u8, 0x00, 0x11, 0x22];
        let image = write_minimal_elf32(0x200, 0x200, &payload);
        let mut machine = minimal_machine(MinimalMachineConfig::default());
        let loaded = load_elf_into_machine(&image, &mut machine).unwrap();
        assert_eq!(loaded.entry, 0x200);
        assert_eq!(machine.cpu().pc(), 0x200);
        let mut buf = [0u8; 4];
        machine.bus_mut().read(0x200, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn rejects_non_elf() {
        let mut bus = sim_kernel::MemoryBus::new();
        assert!(matches!(
            load_elf(b"not elf", &mut bus),
            Err(LoadError::NotElf)
        ));
    }
}

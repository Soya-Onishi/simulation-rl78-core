//! ELF loader for guest images (ELF only; no bin/mot).
//!
//! Parsing uses the [`object`] crate (`PT_LOAD` segments). This module writes
//! bytes onto a [`MemoryBus`]; it does not talk to a translation engine.
//! Architecture crates flush their own caches in [`Core::load_firmware`].

use object::{read::elf::ProgramHeader, Endianness, Object};

use crate::{BusError, Cpu, FirmwareError, Machine, MemoryBus};

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
    Bus(BusError),
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

impl From<FirmwareError> for LoadError {
    fn from(err: FirmwareError) -> Self {
        match err {
            FirmwareError::InvalidImage => Self::NotElf,
            FirmwareError::Truncated => Self::Truncated,
            FirmwareError::Unsupported(msg) => Self::Unsupported(msg),
            FirmwareError::Bus(err) => Self::Bus(err),
        }
    }
}

impl From<LoadError> for FirmwareError {
    fn from(err: LoadError) -> Self {
        match err {
            LoadError::NotElf => Self::InvalidImage,
            LoadError::Truncated => Self::Truncated,
            LoadError::Unsupported(msg) => Self::Unsupported(msg),
            LoadError::Bus(err) => Self::Bus(err),
        }
    }
}

/// Load a guest ELF into `bus` (PT_LOAD segments only).
pub fn load_elf(image: &[u8], bus: &mut MemoryBus) -> Result<ElfLoad, LoadError> {
    let file = object::File::parse(image).map_err(|_| {
        if image.len() < 4 || &image[0..4] != b"\x7fELF" {
            LoadError::NotElf
        } else {
            LoadError::Truncated
        }
    })?;

    let elf = match file {
        object::File::Elf32(ref elf) => elf,
        _ => return Err(LoadError::Unsupported("only ELF32 is supported")),
    };

    let endianness = file.endianness();
    if endianness != Endianness::Little {
        return Err(LoadError::Unsupported("ELF must be little-endian"));
    }

    let mut loaded = false;
    for ph in elf.elf_program_headers() {
        let paddr = ph.p_paddr(endianness);
        let addr = if paddr == 0 {
            ph.p_vaddr(endianness)
        } else {
            paddr
        };
        let data = ph
            .data(endianness, elf.data())
            .map_err(|_| LoadError::Truncated)?;

        if data.is_empty() || ph.p_type(endianness) != object::elf::PT_LOAD {
            continue;
        }

        bus.load(addr as u64, data).map_err(LoadError::Bus)?;
        loaded = true;
    }

    if !loaded {
        return Err(LoadError::Unsupported("ELF has no PT_LOAD segments"));
    }

    Ok(ElfLoad {
        entry: file.entry(),
    })
}

/// Load `image` into `machine` via [`Machine::load_firmware`]. Does not change PC.
///
/// The CPU implementation owns any engine-specific follow-up (for example a
/// translation-cache flush). This helper only reports the ELF entry point.
pub fn load_elf_into_machine<C: Cpu>(
    image: &[u8],
    machine: &mut Machine<C>,
) -> Result<ElfLoad, LoadError> {
    let entry = elf_entry(image)?;
    machine.load_firmware(image)?;
    // TODO: returning entrypoint when loading firmware is not appropriate. Do when trigger reset.
    Ok(ElfLoad { entry })
}

fn elf_entry(image: &[u8]) -> Result<u64, LoadError> {
    let file = object::File::parse(image).map_err(|_| {
        if image.len() < 4 || &image[0..4] != b"\x7fELF" {
            LoadError::NotElf
        } else {
            LoadError::Truncated
        }
    })?;
    if !matches!(file, object::File::Elf32(_)) {
        return Err(LoadError::Unsupported("only ELF32 is supported"));
    }
    if file.endianness() != Endianness::Little {
        return Err(LoadError::Unsupported("ELF must be little-endian"));
    }
    Ok(file.entry())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedCpu;
    use crate::{EventCtl, Machine, MemoryMapBuilder, Rom};

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

    fn machine_with_rom(size: usize) -> Machine<ScriptedCpu> {
        let bus = MemoryMapBuilder::new()
            .map(0, Box::new(Rom::new(size)))
            .unwrap()
            .build()
            .unwrap();
        Machine::new(ScriptedCpu::new(vec![]), bus, EventCtl::new())
    }

    #[test]
    fn load_elf_writes_payload_and_reports_entry() {
        let payload = [0x00u8, 0x00, 0x11, 0x22];
        let image = write_minimal_elf32(0x200, 0x200, &payload);
        let mut machine = machine_with_rom(0x1000);
        let loaded = load_elf_into_machine(&image, &mut machine).unwrap();
        assert_eq!(loaded.entry, 0x200);
        let mut buf = [0u8; 4];
        machine.bus_mut().read(0x200, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn machine_load_firmware_matches_wrapper() {
        let payload = [0xAAu8, 0xBB];
        let image = write_minimal_elf32(0x300, 0x300, &payload);
        let mut machine = machine_with_rom(0x1000);
        machine.load_firmware(&image).unwrap();
        let mut buf = [0u8; 2];
        machine.bus_mut().read(0x300, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn rejects_non_elf() {
        let mut bus = MemoryBus::new();
        assert!(matches!(
            load_elf(b"not elf", &mut bus),
            Err(LoadError::NotElf)
        ));
    }
}

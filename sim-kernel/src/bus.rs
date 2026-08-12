//! Memory bus and [`MemoryMapped`] device contract.

use std::fmt;

/// Guest physical address. Architecture width is not encoded here.
pub type Addr = u64;

/// Error raised by the bus or a mapped device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusError {
    Unmapped { addr: Addr, len: usize },
    OutOfRange { addr: Addr, offset: u64, len: usize },
    ReadOnly { addr: Addr },
}

/// Failure to register a device on the bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    EmptyDevice,
    Overlap { base: Addr, size: u64 },
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmapped { addr, len } => {
                write!(f, "unmapped access addr={addr:#x} len={len}")
            }
            Self::OutOfRange { addr, offset, len } => {
                write!(
                    f,
                    "out of range addr={addr:#x} offset={offset:#x} len={len}"
                )
            }
            Self::ReadOnly { addr } => write!(f, "write to read-only addr={addr:#x}"),
        }
    }
}

impl std::error::Error for BusError {}

/// MMIO / memory region. Devices (including Magic probe) implement this.
pub trait MemoryMapped: Send {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError>;
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError>;
}

/// How the bus treats accesses that hit no region.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnmappedPolicy {
    /// Record the access and return [`BusError::Unmapped`]. The CPU may continue.
    #[default]
    Log,
    /// Same recording, but [`MemoryBus::take_trap`] reports it so the kernel stops.
    Trap,
}

/// One recorded unmapped access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmappedAccess {
    pub addr: Addr,
    pub len: usize,
    pub write: bool,
}

struct MappedRegion {
    base: Addr,
    size: u64,
    device: Box<dyn MemoryMapped>,
}

/// Flat physical bus. Built via [`MemoryMapBuilder`]; runtime only serves accesses.
#[derive(Default)]
pub struct MemoryBus {
    regions: Vec<MappedRegion>,
    policy: UnmappedPolicy,
    unmapped_log: Vec<UnmappedAccess>,
    trap: Option<UnmappedAccess>,
}

impl MemoryBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn policy(&self) -> UnmappedPolicy {
        self.policy
    }

    pub fn read(&mut self, addr: Addr, buf: &mut [u8]) -> Result<(), BusError> {
        let len = buf.len();
        let Some(index) = self.find_region(addr) else {
            return self.unmapped(addr, len, false);
        };
        let region = &mut self.regions[index];
        let offset = addr - region.base;
        if offset.saturating_add(len as u64) > region.size {
            return Err(BusError::OutOfRange { addr, offset, len });
        }
        region.device.read(offset, buf)
    }

    pub fn write(&mut self, addr: Addr, buf: &[u8]) -> Result<(), BusError> {
        let len = buf.len();
        let Some(index) = self.find_region(addr) else {
            return self.unmapped(addr, len, true);
        };
        let region = &mut self.regions[index];
        let offset = addr - region.base;
        if offset.saturating_add(len as u64) > region.size {
            return Err(BusError::OutOfRange { addr, offset, len });
        }
        region.device.write(offset, buf)
    }

    #[must_use]
    pub fn take_unmapped_log(&mut self) -> Vec<UnmappedAccess> {
        std::mem::take(&mut self.unmapped_log)
    }

    #[must_use]
    pub fn take_trap(&mut self) -> Option<UnmappedAccess> {
        self.trap.take()
    }

    fn find_region(&self, addr: Addr) -> Option<usize> {
        self.regions
            .iter()
            .position(|r| addr >= r.base && addr < r.base.saturating_add(r.size))
    }

    fn unmapped(&mut self, addr: Addr, len: usize, write: bool) -> Result<(), BusError> {
        let access = UnmappedAccess { addr, len, write };
        self.unmapped_log.push(access.clone());
        if self.policy == UnmappedPolicy::Trap {
            self.trap = Some(access);
        }
        Err(BusError::Unmapped { addr, len })
    }
}

fn overlaps(a_base: Addr, a_size: u64, b_base: Addr, b_size: u64) -> bool {
    a_base < b_base.saturating_add(b_size) && b_base < a_base.saturating_add(a_size)
}

/// Immutable-style memory map construction.
///
/// Each [`MemoryMapBuilder::map`] / [`MemoryMapBuilder::policy`] takes `self` by
/// value and returns the updated builder. The finished map is passed into
/// [`crate::Machine::new`] — callers do not mutate a live bus region-by-region.
#[derive(Default)]
pub struct MemoryMapBuilder {
    regions: Vec<MappedRegion>,
    policy: UnmappedPolicy,
}

impl MemoryMapBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn policy(mut self, policy: UnmappedPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn map(mut self, base: Addr, device: Box<dyn MemoryMapped>) -> Result<Self, MapError> {
        let size = device.len();
        if size == 0 {
            return Err(MapError::EmptyDevice);
        }
        if self
            .regions
            .iter()
            .any(|r| overlaps(r.base, r.size, base, size))
        {
            return Err(MapError::Overlap { base, size });
        }
        self.regions.push(MappedRegion { base, size, device });
        Ok(self)
    }

    #[must_use]
    pub fn build(self) -> MemoryBus {
        MemoryBus {
            regions: self.regions,
            policy: self.policy,
            unmapped_log: Vec::new(),
            trap: None,
        }
    }
}

/// Writable RAM region.
pub struct Ram {
    data: Vec<u8>,
}

impl Ram {
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
        }
    }

    #[must_use]
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self { data }
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }
}

impl MemoryMapped for Ram {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        copy_from(&self.data, offset, buf)
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        copy_into(&mut self.data, offset, buf)
    }
}

/// Read-only memory region.
pub struct Rom {
    data: Vec<u8>,
}

impl Rom {
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
        }
    }

    #[must_use]
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl MemoryMapped for Rom {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        copy_from(&self.data, offset, buf)
    }

    fn write(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::ReadOnly {
            addr: offset, // caller sees bus address via wrapper if needed
        })
    }
}

fn copy_from(data: &[u8], offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
    let start = usize::try_from(offset).map_err(|_| BusError::OutOfRange {
        addr: offset,
        offset,
        len: buf.len(),
    })?;
    let end = start.checked_add(buf.len()).ok_or(BusError::OutOfRange {
        addr: offset,
        offset,
        len: buf.len(),
    })?;
    let src = data.get(start..end).ok_or(BusError::OutOfRange {
        addr: offset,
        offset,
        len: buf.len(),
    })?;
    buf.copy_from_slice(src);
    Ok(())
}

fn copy_into(data: &mut [u8], offset: u64, buf: &[u8]) -> Result<(), BusError> {
    let start = usize::try_from(offset).map_err(|_| BusError::OutOfRange {
        addr: offset,
        offset,
        len: buf.len(),
    })?;
    let end = start.checked_add(buf.len()).ok_or(BusError::OutOfRange {
        addr: offset,
        offset,
        len: buf.len(),
    })?;
    let dst = data.get_mut(start..end).ok_or(BusError::OutOfRange {
        addr: offset,
        offset,
        len: buf.len(),
    })?;
    dst.copy_from_slice(buf);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_roundtrip() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(16)))
            .unwrap()
            .build();
        bus.write(0x1004, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        bus.read(0x1004, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn rom_rejects_writes() {
        let mut bus = MemoryMapBuilder::new()
            .map(0, Box::new(Rom::from_bytes(vec![0xAA, 0xBB])))
            .unwrap()
            .build();
        assert!(matches!(
            bus.write(0, &[0x00]),
            Err(BusError::ReadOnly { .. })
        ));
        let mut buf = [0u8; 1];
        bus.read(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAA);
    }

    #[test]
    fn unmapped_is_logged() {
        let mut bus = MemoryBus::new();
        assert!(bus.read(0xdead, &mut [0u8; 2]).is_err());
        let log = bus.take_unmapped_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].addr, 0xdead);
        assert!(!log[0].write);
        assert!(bus.take_trap().is_none());
    }

    #[test]
    fn unmapped_trap_policy() {
        let mut bus = MemoryMapBuilder::new().policy(UnmappedPolicy::Trap).build();
        let _ = bus.write(0x10, &[0xFF]);
        let trap = bus.take_trap().unwrap();
        assert!(trap.write);
        assert_eq!(trap.addr, 0x10);
    }

    #[test]
    fn overlap_is_rejected() {
        let builder = MemoryMapBuilder::new()
            .map(0x100, Box::new(Ram::new(32)))
            .unwrap();
        assert!(matches!(
            builder.map(0x110, Box::new(Ram::new(16))),
            Err(MapError::Overlap { .. })
        ));
    }
}

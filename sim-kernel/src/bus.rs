//! Memory bus and [`MemoryMapped`] device contract.

use std::fmt;

/// Guest physical address. Architecture width is not encoded here.
pub type Addr = u64;

/// Error raised by the bus or a mapped device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusError {
    Unmapped {
        addr: Addr,
        len: usize,
    },
    OutOfRange {
        addr: Addr,
        offset: u64,
        len: usize,
    },
    ReadOnly {
        addr: Addr,
    },
    /// [`MemoryMapped::load`] on a device that is not image-loadable (not ROM).
    NotLoadable {
        addr: Addr,
    },
}

/// Failure to register a device on the bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    EmptyDevice,
    Overlap { base: Addr, size: u64 },
    /// [`MemoryMapBuilder::alias`] `target` hits no mapped region.
    AliasTargetMissing { target: Addr },
    /// Alias window starting at `target` with `size` exceeds the hit region.
    AliasOutOfRange {
        target: Addr,
        size: u64,
        region_base: Addr,
        region_size: u64,
    },
    /// Alias chain loops back on itself (detected in [`MemoryMapBuilder::build`]).
    /// `path` lists guest bases in visit order, ending with the repeated base.
    AliasCycle { path: Vec<Addr> },
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
            Self::NotLoadable { addr } => {
                write!(f, "image load not supported at addr={addr:#x}")
            }
        }
    }
}

impl std::error::Error for BusError {}

/// Device that can emit a [`MemoryMapBuilder`] (on-chip core, SoC part, board).
pub trait HasMemoryMap {
    fn memory_map(&self) -> Result<MemoryMapBuilder, MapError>;
}

/// MMIO / memory region. Devices (including Magic probe) implement this.
pub trait MemoryMapped: Send {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError>;
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError>;

    /// Host pointer for TCG direct mapping. `None` means MMIO (IO callbacks).
    ///
    /// The pointer must stay valid for the lifetime of the mapping (the device
    /// `Vec<u8>` owned by the bus). Architecture CPUs call this from
    /// [`crate::Cpu::bind_memory`].
    fn host_ptr(&mut self) -> Option<*mut u8>;

    /// Image / flash load path. Only ROM-like devices accept this; others return
    /// [`BusError::NotLoadable`].
    fn load(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError>;
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

enum RegionBacking {
    Device(Box<dyn MemoryMapped>),
    /// Window onto an existing [`RegionBacking::Device`] mapped at `target_base`
    /// (QEMU `memory_region_init_alias` style).
    Alias {
        target_base: Addr,
        target_offset: u64,
    },
}

struct MappedRegion {
    base: Addr,
    size: u64,
    backing: RegionBacking,
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
        let (dev_idx, offset) = match self.resolve(addr, len) {
            Ok(v) => v,
            Err(BusError::Unmapped { .. }) => return self.unmapped(addr, len, false),
            Err(e) => return Err(e),
        };
        self.device_mut(dev_idx)
            .read(offset, buf)
            .map_err(|err| rewrite_bus_error(err, addr))
    }

    pub fn write(&mut self, addr: Addr, buf: &[u8]) -> Result<(), BusError> {
        let len = buf.len();
        let (dev_idx, offset) = match self.resolve(addr, len) {
            Ok(v) => v,
            Err(BusError::Unmapped { .. }) => return self.unmapped(addr, len, true),
            Err(e) => return Err(e),
        };
        self.device_mut(dev_idx)
            .write(offset, buf)
            .map_err(|err| rewrite_bus_error(err, addr))
    }

    /// Load an image through [`MemoryMapped::load`] (ROM / flash programming).
    pub fn load(&mut self, addr: Addr, buf: &[u8]) -> Result<(), BusError> {
        let len = buf.len();
        let (dev_idx, offset) = match self.resolve(addr, len) {
            Ok(v) => v,
            Err(BusError::Unmapped { .. }) => return self.unmapped(addr, len, true),
            Err(e) => return Err(e),
        };
        self.device_mut(dev_idx)
            .load(offset, buf)
            .map_err(|err| rewrite_bus_error(err, addr))
    }

    #[must_use]
    pub fn take_unmapped_log(&mut self) -> Vec<UnmappedAccess> {
        std::mem::take(&mut self.unmapped_log)
    }

    #[must_use]
    pub fn take_trap(&mut self) -> Option<UnmappedAccess> {
        self.trap.take()
    }

    /// Walk mapped regions for CPU bind (`host_ptr` is `Some` for RAM/ROM).
    ///
    /// Alias windows are reported too, with the target's `host_ptr` advanced by
    /// the alias offset, so TCG can map the same backing at both guest bases.
    pub fn for_each_region(&mut self, mut f: impl FnMut(Addr, u64, Option<*mut u8>)) {
        for i in 0..self.regions.len() {
            let base = self.regions[i].base;
            let size = self.regions[i].size;
            let alias = match &self.regions[i].backing {
                RegionBacking::Device(_) => None,
                RegionBacking::Alias {
                    target_base,
                    target_offset,
                } => Some((*target_base, *target_offset)),
            };
            let host = match alias {
                None => self.device_mut(i).host_ptr(),
                Some((target_base, target_offset)) => {
                    let target_idx = self
                        .find_base(target_base)
                        .expect("alias target validated at map time");
                    self.device_mut(target_idx).host_ptr().map(|p| {
                        // Safety: offset was checked against the target size when
                        // the alias was registered; the host buffer outlives the bus.
                        unsafe { p.add(target_offset as usize) }
                    })
                }
            };
            f(base, size, host);
        }
    }

    /// Resolve `addr` to `(device region index, offset within that device)`.
    fn resolve(&self, addr: Addr, len: usize) -> Result<(usize, u64), BusError> {
        let Some(index) = self.find_region(addr) else {
            return Err(BusError::Unmapped { addr, len });
        };
        let region = &self.regions[index];
        let offset_in_window = addr - region.base;
        if offset_in_window.saturating_add(len as u64) > region.size {
            return Err(BusError::OutOfRange {
                addr,
                offset: offset_in_window,
                len,
            });
        }
        match &region.backing {
            RegionBacking::Device(_) => Ok((index, offset_in_window)),
            RegionBacking::Alias {
                target_base,
                target_offset,
            } => {
                let target_idx = self.find_base(*target_base).ok_or(BusError::Unmapped {
                    addr,
                    len,
                })?;
                Ok((
                    target_idx,
                    target_offset.saturating_add(offset_in_window),
                ))
            }
        }
    }

    fn device_mut(&mut self, index: usize) -> &mut dyn MemoryMapped {
        match &mut self.regions[index].backing {
            RegionBacking::Device(device) => device.as_mut(),
            RegionBacking::Alias { .. } => {
                panic!("MemoryBus: expected device backing at index {index}")
            }
        }
    }

    fn find_region(&self, addr: Addr) -> Option<usize> {
        self.regions
            .iter()
            .position(|r| addr >= r.base && addr < r.base.saturating_add(r.size))
    }

    fn find_base(&self, base: Addr) -> Option<usize> {
        self.regions.iter().position(|r| r.base == base)
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

/// Devices report offsets; rewrite to the guest bus address for callers.
fn rewrite_bus_error(err: BusError, addr: Addr) -> BusError {
    match err {
        BusError::ReadOnly { .. } => BusError::ReadOnly { addr },
        BusError::NotLoadable { .. } => BusError::NotLoadable { addr },
        BusError::OutOfRange { offset, len, .. } => BusError::OutOfRange { addr, offset, len },
        BusError::Unmapped { len, .. } => BusError::Unmapped { addr, len },
    }
}

/// Immutable-style memory map construction.
///
/// Each [`MemoryMapBuilder::map`] / [`MemoryMapBuilder::alias`] /
/// [`MemoryMapBuilder::policy`] takes `self` by value and returns the updated
/// builder. The finished map is passed into [`crate::Machine::new`] — callers
/// do not mutate a live bus region-by-region.
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
        self.regions.push(MappedRegion {
            base,
            size,
            backing: RegionBacking::Device(device),
        });
        Ok(self)
    }

    /// Map `size` bytes starting at absolute guest address `target` onto
    /// `alias_base` (QEMU `memory_region_init_alias` style).
    ///
    /// `target` is resolved with a region hit test (`target_base + offset`).
    /// Alias-of-alias is allowed; [`Self::build`] flattens chains and rejects
    /// cycles.
    pub fn alias(
        mut self,
        alias_base: Addr,
        target: Addr,
        size: u64,
    ) -> Result<Self, MapError> {
        if size == 0 {
            return Err(MapError::EmptyDevice);
        }
        let target_idx = self
            .regions
            .iter()
            .position(|r| target >= r.base && target < r.base.saturating_add(r.size))
            .ok_or(MapError::AliasTargetMissing { target })?;
        let region_base = self.regions[target_idx].base;
        let region_size = self.regions[target_idx].size;
        let target_offset = target - region_base;
        if target_offset.saturating_add(size) > region_size {
            return Err(MapError::AliasOutOfRange {
                target,
                size,
                region_base,
                region_size,
            });
        }
        if self
            .regions
            .iter()
            .any(|r| overlaps(r.base, r.size, alias_base, size))
        {
            return Err(MapError::Overlap {
                base: alias_base,
                size,
            });
        }
        self.regions.push(MappedRegion {
            base: alias_base,
            size,
            backing: RegionBacking::Alias {
                target_base: region_base,
                target_offset,
            },
        });
        Ok(self)
    }

    /// Fold another unfinished map into this one. Overlaps are rejected.
    /// [`Self::policy`] on `self` is kept; `other`'s policy is ignored.
    pub fn merge(mut self, other: Self) -> Result<Self, MapError> {
        for region in other.regions {
            self = match region.backing {
                RegionBacking::Device(device) => self.map(region.base, device)?,
                RegionBacking::Alias {
                    target_base,
                    target_offset,
                } => self.alias(
                    region.base,
                    target_base.saturating_add(target_offset),
                    region.size,
                )?,
            };
        }
        Ok(self)
    }

    /// Finish the map: flatten alias chains onto their ultimate devices and
    /// reject cycles (`A → B → A`).
    pub fn build(self) -> Result<MemoryBus, MapError> {
        let mut regions = self.regions;
        Self::flatten_aliases(&mut regions)?;
        Ok(MemoryBus {
            regions,
            policy: self.policy,
            unmapped_log: Vec::new(),
            trap: None,
        })
    }

    /// Resolve every alias to `(device_base, offset_in_device)`.
    fn flatten_aliases(regions: &mut [MappedRegion]) -> Result<(), MapError> {
        let mut resolved = Vec::new();
        for i in 0..regions.len() {
            if matches!(regions[i].backing, RegionBacking::Device(_)) {
                continue;
            }
            let (device_base, offset) = Self::resolve_alias_chain(regions, i)?;
            let size = regions[i].size;
            let device_idx = regions
                .iter()
                .position(|r| r.base == device_base)
                .expect("resolve_alias_chain returns a device base");
            let region_size = regions[device_idx].size;
            if offset.saturating_add(size) > region_size {
                return Err(MapError::AliasOutOfRange {
                    target: device_base.saturating_add(offset),
                    size,
                    region_base: device_base,
                    region_size,
                });
            }
            resolved.push((i, device_base, offset));
        }
        for (i, device_base, target_offset) in resolved {
            regions[i].backing = RegionBacking::Alias {
                target_base: device_base,
                target_offset,
            };
        }
        Ok(())
    }

    /// Walk `start`'s alias chain to the owning device. `path` tracks visited
    /// alias bases for cycle detection.
    fn resolve_alias_chain(
        regions: &[MappedRegion],
        start: usize,
    ) -> Result<(Addr, u64), MapError> {
        let mut path: Vec<Addr> = Vec::new();
        let mut idx = start;
        let mut accum = 0u64;
        loop {
            let base = regions[idx].base;
            match &regions[idx].backing {
                RegionBacking::Device(_) => return Ok((base, accum)),
                RegionBacking::Alias {
                    target_base,
                    target_offset,
                } => {
                    if path.contains(&base) {
                        path.push(base);
                        return Err(MapError::AliasCycle { path });
                    }
                    path.push(base);
                    accum = accum.saturating_add(*target_offset);
                    idx = regions
                        .iter()
                        .position(|r| r.base == *target_base)
                        .ok_or(MapError::AliasTargetMissing {
                            target: target_base.saturating_add(*target_offset),
                        })?;
                }
            }
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

    fn host_ptr(&mut self) -> Option<*mut u8> {
        Some(self.data.as_mut_ptr())
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
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

    /// Blank flash (erased cells read as `0xFF`).
    #[must_use]
    pub fn erased(size: usize) -> Self {
        Self {
            data: vec![0xFF; size],
        }
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
        // `addr` is the device-local offset; [`MemoryBus`] rewrites it to the
        // guest address before returning to callers.
        Err(BusError::ReadOnly { addr: offset })
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        // TCG fetches from the same buffer; guest stores via the bus API still
        // hit [`BusError::ReadOnly`]. Direct TCG stores are out of M1 scope.
        Some(self.data.as_mut_ptr())
    }

    fn load(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        copy_into(&mut self.data, offset, buf)
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
            .build().unwrap();
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
            .build().unwrap();
        assert!(matches!(
            bus.write(0, &[0x00]),
            Err(BusError::ReadOnly { .. })
        ));
        let mut buf = [0u8; 1];
        bus.read(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAA);
    }

    #[test]
    fn rom_readonly_error_uses_guest_address() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x8000, Box::new(Rom::from_bytes(vec![0; 16])))
            .unwrap()
            .build().unwrap();
        assert_eq!(
            bus.write(0x8004, &[0xFF]),
            Err(BusError::ReadOnly { addr: 0x8004 })
        );
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
        let mut bus = MemoryMapBuilder::new().policy(UnmappedPolicy::Trap).build().unwrap();
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

    #[test]
    fn rom_load_accepts_image_bytes() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Rom::new(16)))
            .unwrap()
            .build().unwrap();
        bus.load(0x1000, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        bus.read(0x1000, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn ram_rejects_image_load() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(16)))
            .unwrap()
            .build().unwrap();
        assert!(matches!(
            bus.load(0x1000, &[1, 2, 3, 4]),
            Err(BusError::NotLoadable { addr: 0x1000 })
        ));
    }

    #[test]
    fn merge_combines_non_overlapping_maps() {
        let a = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(16)))
            .unwrap();
        let b = MemoryMapBuilder::new()
            .map(0x2000, Box::new(Ram::new(8)))
            .unwrap();
        let mut bus = a.merge(b).unwrap().build().unwrap();
        bus.write(0x1000, &[1]).unwrap();
        bus.write(0x2000, &[2]).unwrap();
    }

    #[test]
    fn merge_rejects_overlap() {
        let a = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(16)))
            .unwrap();
        let b = MemoryMapBuilder::new()
            .map(0x1008, Box::new(Ram::new(8)))
            .unwrap();
        assert!(matches!(a.merge(b), Err(MapError::Overlap { .. })));
    }

    #[test]
    fn alias_ram_shares_backing() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(32)))
            .unwrap()
            .alias(0x2000, 0x1000, 32)
            .unwrap()
            .build()
            .unwrap();
        bus.write(0x1004, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        bus.read(0x2004, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
        bus.write(0x2010, &[9, 8]).unwrap();
        bus.read(0x1010, &mut buf[..2]).unwrap();
        assert_eq!(&buf[..2], &[9, 8]);
    }

    #[test]
    fn alias_partial_window_with_offset() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(64)))
            .unwrap()
            .alias(0x3000, 0x1010, 16)
            .unwrap()
            .build()
            .unwrap();
        bus.write(0x1010, &[0xAA, 0xBB]).unwrap();
        let mut buf = [0u8; 2];
        bus.read(0x3000, &mut buf).unwrap();
        assert_eq!(buf, [0xAA, 0xBB]);
    }

    #[test]
    fn alias_rom_load_and_readonly() {
        let mut bus = MemoryMapBuilder::new()
            .map(0, Box::new(Rom::new(32)))
            .unwrap()
            .alias(0xF0000, 0, 32)
            .unwrap()
            .build()
            .unwrap();
        bus.load(0, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        bus.read(0xF0000, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
        assert_eq!(
            bus.write(0xF0000, &[0]),
            Err(BusError::ReadOnly { addr: 0xF0000 })
        );
    }

    #[test]
    fn alias_for_each_region_reports_offset_host_ptr() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(64)))
            .unwrap()
            .alias(0x2000, 0x1008, 16)
            .unwrap()
            .build()
            .unwrap();
        let mut primary = None;
        let mut mirrored = None;
        bus.for_each_region(|base, size, host| match base {
            0x1000 => {
                assert_eq!(size, 64);
                primary = host;
            }
            0x2000 => {
                assert_eq!(size, 16);
                mirrored = host;
            }
            _ => panic!("unexpected region {base:#x}"),
        });
        let primary = primary.expect("primary host_ptr");
        let mirrored = mirrored.expect("alias host_ptr");
        assert_eq!(mirrored, unsafe { primary.add(8) });
    }

    #[test]
    fn alias_target_missing_is_rejected() {
        assert!(matches!(
            MemoryMapBuilder::new().alias(0x2000, 0x1000, 16),
            Err(MapError::AliasTargetMissing { target: 0x1000 })
        ));
    }

    #[test]
    fn alias_out_of_range_is_rejected() {
        let builder = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(16)))
            .unwrap();
        assert!(matches!(
            builder.alias(0x2000, 0x1008, 16),
            Err(MapError::AliasOutOfRange { .. })
        ));
    }

    #[test]
    fn alias_of_alias_flattens_at_build() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(64)))
            .unwrap()
            .alias(0x2000, 0x1008, 32)
            .unwrap()
            .alias(0x3000, 0x2004, 16)
            .unwrap()
            .build()
            .unwrap();
        bus.write(0x1008 + 4, &[0xAA, 0xBB]).unwrap();
        let mut buf = [0u8; 2];
        bus.read(0x3000, &mut buf).unwrap();
        assert_eq!(buf, [0xAA, 0xBB]);

        let mut primary = None;
        let mut outer = None;
        bus.for_each_region(|base, _size, host| match base {
            0x1000 => primary = host,
            0x3000 => outer = host,
            0x2000 => {}
            _ => panic!("unexpected region {base:#x}"),
        });
        let primary = primary.expect("primary");
        let outer = outer.expect("outer alias");
        assert_eq!(outer, unsafe { primary.add(12) }); // 8 + 4
    }

    #[test]
    fn alias_cycle_is_rejected_at_build() {
        // Public `alias` cannot retarget an existing window, so craft A→B→A
        // for [`MemoryMapBuilder::build`].
        let builder = MemoryMapBuilder {
            regions: vec![
                MappedRegion {
                    base: 0x2000,
                    size: 16,
                    backing: RegionBacking::Alias {
                        target_base: 0x3000,
                        target_offset: 0,
                    },
                },
                MappedRegion {
                    base: 0x3000,
                    size: 16,
                    backing: RegionBacking::Alias {
                        target_base: 0x2000,
                        target_offset: 0,
                    },
                },
            ],
            policy: UnmappedPolicy::default(),
        };
        assert!(matches!(
            builder.build(),
            Err(MapError::AliasCycle { path }) if path == vec![0x2000, 0x3000, 0x2000]
        ));
    }

    #[test]
    fn alias_self_cycle_is_rejected_at_build() {
        let builder = MemoryMapBuilder {
            regions: vec![MappedRegion {
                base: 0x2000,
                size: 16,
                backing: RegionBacking::Alias {
                    target_base: 0x2000,
                    target_offset: 0,
                },
            }],
            policy: UnmappedPolicy::default(),
        };
        assert!(matches!(
            builder.build(),
            Err(MapError::AliasCycle { path }) if path == vec![0x2000, 0x2000]
        ));
    }

    #[test]
    fn alias_overlap_is_rejected() {
        let builder = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(32)))
            .unwrap()
            .map(0x2000, Box::new(Ram::new(16)))
            .unwrap();
        assert!(matches!(
            builder.alias(0x2000, 0x1000, 16),
            Err(MapError::Overlap { .. })
        ));
    }

    #[test]
    fn merge_preserves_aliases() {
        let a = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::new(16)))
            .unwrap()
            .alias(0x2000, 0x1000, 16)
            .unwrap();
        let b = MemoryMapBuilder::new()
            .map(0x3000, Box::new(Ram::new(8)))
            .unwrap();
        let mut bus = a.merge(b).unwrap().build().unwrap();
        bus.write(0x1000, &[7]).unwrap();
        let mut buf = [0u8];
        bus.read(0x2000, &mut buf).unwrap();
        assert_eq!(buf[0], 7);
    }
}

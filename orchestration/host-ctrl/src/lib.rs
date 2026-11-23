//! Mapped `{iox_root}/host_ctrl` shared memory for Command and Node (#147 / #151).
//!
//! Layout bytes live in [`contract::shm`]. This crate owns create/open/mmap and
//! safe accessors over [`contract::HostCtrlAtoms`].

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use contract::{
    HOST_CTRL_FILE, HOST_CTRL_MAGIC, HOST_CTRL_VERSION, HostCtrlAtoms, HostCtrlHeader,
    HostCtrlLayout,
};
use memmap2::{MmapMut, MmapOptions};

/// Absolute path of the `host_ctrl` mapping for a cluster root.
#[must_use]
pub fn host_ctrl_path(iox_root: &Path) -> PathBuf {
    iox_root.join(HOST_CTRL_FILE)
}

/// Unlinks `{iox_root}/host_ctrl` when dropped (best-effort).
///
/// Hold this for the lifetime of a Command host session so early `?` returns
/// still remove the file.
pub struct HostCtrlLease {
    root: PathBuf,
}

impl HostCtrlLease {
    #[must_use]
    pub fn new(iox_root: &Path) -> Self {
        Self {
            root: iox_root.to_path_buf(),
        }
    }
}

impl Drop for HostCtrlLease {
    fn drop(&mut self) {
        HostCtrl::unlink(&self.root);
    }
}

/// Mapped host control block shared by Command and Node processes.
pub struct HostCtrl {
    mmap: MmapMut,
    layout: HostCtrlLayout,
}

impl HostCtrl {
    /// Create and initialize `{iox_root}/host_ctrl`.
    ///
    /// Removes any previous file first, then creates exclusively (`create_new`)
    /// so a stale mapping from a dead session cannot be truncated underfoot.
    pub fn create(iox_root: &Path, board_count: u32) -> io::Result<Self> {
        let layout = HostCtrlLayout::new(board_count).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "host_ctrl requires at least one board",
            )
        })?;
        let bytes = layout.total_bytes();
        std::fs::create_dir_all(iox_root)?;
        let path = host_ctrl_path(iox_root);
        let _ = std::fs::remove_file(&path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.set_len(bytes as u64)?;
        // SAFETY: exclusive new file of `bytes`; we initialize the header below.
        let mut mmap = unsafe { MmapOptions::new().len(bytes).map_mut(&file)? };
        Self::write_fresh_header(&mut mmap, layout)?;
        Ok(Self { mmap, layout })
    }

    /// Open an existing `{iox_root}/host_ctrl` created by Command.
    pub fn open(iox_root: &Path, board_count: u32) -> io::Result<Self> {
        let layout = HostCtrlLayout::new(board_count).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "host_ctrl requires at least one board",
            )
        })?;
        let need = layout.total_bytes();
        let path = host_ctrl_path(iox_root);
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        // SAFETY: file was created by [`Self::create`] with matching length.
        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        if mmap.len() != need {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "host_ctrl size mismatch: got {} bytes, need {need} for {board_count} boards",
                    mmap.len()
                ),
            ));
        }
        Self::validate_header(&mmap, layout)?;
        Ok(Self { mmap, layout })
    }

    /// Remove the mapping file (best-effort).
    pub fn unlink(iox_root: &Path) {
        let _ = std::fs::remove_file(host_ctrl_path(iox_root));
    }

    #[must_use]
    pub fn board_count(&self) -> u32 {
        self.layout.board_count
    }

    #[must_use]
    pub fn layout(&self) -> HostCtrlLayout {
        self.layout
    }

    /// Atomic view of concurrently updated slots.
    #[must_use]
    pub fn atoms(&self) -> HostCtrlAtoms<'_> {
        // SAFETY: mmap length and header were validated on create/open; pointer
        // is page-aligned (hence 8-byte aligned).
        unsafe { HostCtrlAtoms::from_mapping(self.mmap.as_ptr() as *mut u8, self.layout) }
    }

    #[must_use]
    pub fn ceiling_ns(&self) -> u64 {
        self.atoms().ceiling_ns.load(Ordering::Acquire)
    }

    pub fn set_ceiling_ns(&self, ceiling_ns: u64) {
        self.atoms().ceiling_ns.store(ceiling_ns, Ordering::Release);
    }

    /// Read board VT for topology index `board_index` (`0..N`).
    pub fn board_vt(&self, board_index: u32) -> io::Result<u64> {
        let atoms = self.atoms();
        let slot = atoms.board_vt.get(board_index as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("board_index {board_index} out of range"),
            )
        })?;
        Ok(slot.load(Ordering::Acquire))
    }

    /// Write board VT for topology index `board_index` (`0..N`).
    pub fn set_board_vt(&self, board_index: u32, virtual_time_ns: u64) -> io::Result<()> {
        let atoms = self.atoms();
        let slot = atoms.board_vt.get(board_index as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("board_index {board_index} out of range"),
            )
        })?;
        slot.store(virtual_time_ns, Ordering::Release);
        Ok(())
    }

    /// Saturating bump of `stop_generation` ([`STOP_GEN_SATURATED`] at the top).
    pub fn bump_stop(&self) -> u64 {
        self.atoms().bump_stop()
    }

    #[must_use]
    pub fn stop_generation(&self) -> u64 {
        self.atoms().stop_generation.load(Ordering::Acquire)
    }

    fn write_fresh_header(mmap: &mut MmapMut, layout: HostCtrlLayout) -> io::Result<()> {
        if mmap.len() < layout.total_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "host_ctrl mapping shorter than layout",
            ));
        }
        let header = layout.fresh_header();
        // SAFETY: mapping is exclusively owned and large enough for the header.
        unsafe {
            let dst = mmap.as_mut_ptr().cast::<HostCtrlHeader>();
            dst.write(header);
        }
        Ok(())
    }

    fn validate_header(mmap: &MmapMut, layout: HostCtrlLayout) -> io::Result<()> {
        if mmap.len() < std::mem::size_of::<HostCtrlHeader>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "host_ctrl mapping shorter than header",
            ));
        }
        // SAFETY: length checked; header is repr(C) at offset 0.
        let header = unsafe { mmap.as_ptr().cast::<HostCtrlHeader>().read() };
        if header.magic != HOST_CTRL_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "host_ctrl magic mismatch: got {:#x}, want {:#x}",
                    header.magic, HOST_CTRL_MAGIC
                ),
            ));
        }
        if header.version != HOST_CTRL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "host_ctrl version mismatch: got {}, want {HOST_CTRL_VERSION}",
                    header.version
                ),
            ));
        }
        if header.board_count != layout.board_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "host_ctrl board_count mismatch: got {}, want {}",
                    header.board_count, layout.board_count
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::STOP_GEN_SATURATED;

    #[test]
    fn create_open_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let created = HostCtrl::create(dir.path(), 2).unwrap();
        assert_eq!(created.board_count(), 2);
        assert_eq!(created.ceiling_ns(), u64::MAX);
        assert_eq!(created.stop_generation(), 0);
        assert_eq!(created.board_vt(0).unwrap(), 0);
        assert_eq!(created.board_vt(1).unwrap(), 0);

        created.set_board_vt(0, 100).unwrap();
        created.set_board_vt(1, 40).unwrap();
        created.set_ceiling_ns(500);
        assert_eq!(created.bump_stop(), 1);

        let opened = HostCtrl::open(dir.path(), 2).unwrap();
        assert_eq!(opened.board_vt(0).unwrap(), 100);
        assert_eq!(opened.board_vt(1).unwrap(), 40);
        assert_eq!(opened.ceiling_ns(), 500);
        assert_eq!(opened.stop_generation(), 1);
    }

    #[test]
    fn create_replaces_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = HostCtrl::create(dir.path(), 1).unwrap();
        first.set_board_vt(0, 99).unwrap();
        drop(first);
        let second = HostCtrl::create(dir.path(), 2).unwrap();
        assert_eq!(second.board_count(), 2);
        assert_eq!(second.board_vt(0).unwrap(), 0);
        assert_eq!(second.ceiling_ns(), u64::MAX);
        assert_eq!(second.stop_generation(), 0);
    }

    #[test]
    fn open_rejects_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let _ = HostCtrl::create(dir.path(), 1).unwrap();
        match HostCtrl::open(dir.path(), 2) {
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::InvalidData),
            Ok(_) => panic!("expected size mismatch"),
        }
    }

    #[test]
    fn bump_stop_saturates_at_max() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl = HostCtrl::create(dir.path(), 1).unwrap();
        ctrl.atoms()
            .stop_generation
            .store(STOP_GEN_SATURATED - 1, Ordering::Release);
        assert_eq!(ctrl.bump_stop(), STOP_GEN_SATURATED);
        assert_eq!(ctrl.bump_stop(), STOP_GEN_SATURATED);
        assert_eq!(ctrl.stop_generation(), STOP_GEN_SATURATED);
    }

    #[test]
    fn lease_unlinks_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let _ctrl = HostCtrl::create(dir.path(), 1).unwrap();
        {
            let _lease = HostCtrlLease::new(dir.path());
            assert!(host_ctrl_path(dir.path()).is_file());
        }
        assert!(!host_ctrl_path(dir.path()).exists());
    }

    #[test]
    fn board_index_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl = HostCtrl::create(dir.path(), 1).unwrap();
        assert_eq!(
            ctrl.board_vt(1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

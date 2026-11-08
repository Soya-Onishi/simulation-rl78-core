//! Shared-memory virtual-time slots between arbiter and nodes.
//!
//! Layout (each slot is an [`AtomicU64`], nanoseconds):
//! - index 0: arbiter hard ceiling (`u64::MAX` = unlimited)
//! - indices 1..=N: each board's current virtual time

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::{MmapMut, MmapOptions};

/// File name under the cluster `iox_root`.
pub const VT_CLOCK_FILE: &str = "vt_clock";

/// Absolute path of the vt_clock mapping for a cluster.
#[must_use]
pub fn vt_clock_path(iox_root: &Path) -> PathBuf {
    iox_root.join(VT_CLOCK_FILE)
}

/// Board slot index for topology board order (`0..N` → `1..=N`).
#[must_use]
pub fn board_slot_index(board_index: usize) -> usize {
    board_index + 1
}

fn slot_bytes(board_count: usize) -> io::Result<(usize, usize)> {
    if board_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "vt_clock requires at least one board",
        ));
    }
    let slot_count = board_count + 1;
    let bytes = slot_count
        .checked_mul(std::mem::size_of::<u64>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "vt_clock size overflow"))?;
    Ok((slot_count, bytes))
}

/// Unlinks `{iox_root}/vt_clock` when dropped (best-effort).
///
/// Hold this for the lifetime of an arbiter host session so early `?` returns
/// still remove the file.
pub struct VtClockLease {
    root: PathBuf,
}

impl VtClockLease {
    #[must_use]
    pub fn new(iox_root: &Path) -> Self {
        Self {
            root: iox_root.to_path_buf(),
        }
    }
}

impl Drop for VtClockLease {
    fn drop(&mut self) {
        VtClock::unlink(&self.root);
    }
}

/// Mapped virtual-time clock shared by arbiter and nodes.
pub struct VtClock {
    mmap: MmapMut,
    /// Number of `u64` slots (`board_count + 1`).
    slot_count: usize,
}

impl VtClock {
    /// Create and initialize `{iox_root}/vt_clock` (slot 0 = `u64::MAX`, boards = 0).
    ///
    /// Removes any previous file first, then creates exclusively (`create_new`)
    /// so a stale mapping from a dead session cannot be truncated underfoot.
    pub fn create(iox_root: &Path, board_count: usize) -> io::Result<Self> {
        let (slot_count, bytes) = slot_bytes(board_count)?;
        std::fs::create_dir_all(iox_root)?;
        let path = vt_clock_path(iox_root);
        let _ = std::fs::remove_file(&path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.set_len(bytes as u64)?;
        // SAFETY: exclusive create_new of a fresh file of known length.
        let mut mmap = unsafe { MmapOptions::new().len(bytes).map_mut(&file)? };
        // New file is zero-filled by the OS after set_len; set ceiling only.
        // SAFETY: mapping is page-aligned and holds `slot_count` u64s.
        let slots = unsafe { Self::as_atomics_mut(&mut mmap, slot_count) };
        slots[0].store(u64::MAX, Ordering::Relaxed);
        Ok(Self { mmap, slot_count })
    }

    /// Open an existing `{iox_root}/vt_clock` created by the arbiter.
    pub fn open(iox_root: &Path, board_count: usize) -> io::Result<Self> {
        let (slot_count, need) = slot_bytes(board_count)?;
        let path = vt_clock_path(iox_root);
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        // SAFETY: file was created by [`Self::create`] with matching length.
        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        if mmap.len() != need {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "vt_clock size mismatch: got {} bytes, need {need} for {board_count} boards",
                    mmap.len()
                ),
            ));
        }
        Ok(Self { mmap, slot_count })
    }

    /// Remove the mapping file (best-effort).
    pub fn unlink(iox_root: &Path) {
        let _ = std::fs::remove_file(vt_clock_path(iox_root));
    }

    #[must_use]
    pub fn board_count(&self) -> usize {
        self.slot_count - 1
    }

    /// Hard ceiling in slot 0.
    #[must_use]
    pub fn ceiling(&self) -> u64 {
        self.slot(0).load(Ordering::Acquire)
    }

    /// Write the arbiter hard ceiling (`u64::MAX` = unlimited).
    pub fn set_ceiling(&self, ceiling_ns: u64) {
        self.slot(0).store(ceiling_ns, Ordering::Release);
    }

    /// Read one board slot (`1..=N`).
    #[must_use]
    pub fn board_vt(&self, slot: usize) -> u64 {
        debug_assert!((1..self.slot_count).contains(&slot));
        self.slot(slot).load(Ordering::Acquire)
    }

    /// Write this board's current virtual time into `slot` (`1..=N`).
    pub fn write_board_vt(&self, slot: usize, virtual_time_ns: u64) {
        debug_assert!((1..self.slot_count).contains(&slot));
        self.slot(slot).store(virtual_time_ns, Ordering::Release);
    }

    /// Minimum of board slots `1..=N`.
    #[must_use]
    pub fn min_board_vt(&self) -> u64 {
        (1..self.slot_count)
            .map(|i| self.board_vt(i))
            .min()
            .unwrap_or(0)
    }

    /// `min(min(board slots) + margin_ns, ceiling)`.
    #[must_use]
    pub fn allowed_ns(&self, margin_ns: u64) -> u64 {
        self.min_board_vt()
            .saturating_add(margin_ns)
            .min(self.ceiling())
    }

    fn slot(&self, index: usize) -> &AtomicU64 {
        // SAFETY: index < slot_count; mapping holds that many aligned u64s.
        unsafe { &*Self::as_atomics(&self.mmap, self.slot_count).as_ptr().add(index) }
    }

    unsafe fn as_atomics(mmap: &MmapMut, slot_count: usize) -> &[AtomicU64] {
        debug_assert!(mmap.as_ptr() as usize % std::mem::align_of::<AtomicU64>() == 0);
        unsafe { std::slice::from_raw_parts(mmap.as_ptr().cast::<AtomicU64>(), slot_count) }
    }

    unsafe fn as_atomics_mut(mmap: &mut MmapMut, slot_count: usize) -> &mut [AtomicU64] {
        debug_assert!(mmap.as_mut_ptr() as usize % std::mem::align_of::<AtomicU64>() == 0);
        unsafe { std::slice::from_raw_parts_mut(mmap.as_mut_ptr().cast::<AtomicU64>(), slot_count) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_open_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let created = VtClock::create(dir.path(), 2).unwrap();
        assert_eq!(created.ceiling(), u64::MAX);
        assert_eq!(created.board_vt(1), 0);
        assert_eq!(created.board_vt(2), 0);
        created.write_board_vt(1, 100);
        created.write_board_vt(2, 40);
        created.set_ceiling(500);
        assert_eq!(created.allowed_ns(10), 50);

        let opened = VtClock::open(dir.path(), 2).unwrap();
        assert_eq!(opened.board_vt(1), 100);
        assert_eq!(opened.board_vt(2), 40);
        assert_eq!(opened.ceiling(), 500);
        assert_eq!(opened.allowed_ns(10), 50);
        assert_eq!(opened.board_count(), 2);
    }

    #[test]
    fn create_replaces_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = VtClock::create(dir.path(), 1).unwrap();
        first.write_board_vt(1, 99);
        drop(first);
        let second = VtClock::create(dir.path(), 2).unwrap();
        assert_eq!(second.board_count(), 2);
        assert_eq!(second.board_vt(1), 0);
        assert_eq!(second.ceiling(), u64::MAX);
    }

    #[test]
    fn open_rejects_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let _ = VtClock::create(dir.path(), 1).unwrap();
        match VtClock::open(dir.path(), 2) {
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::InvalidData),
            Ok(_) => panic!("expected size mismatch"),
        }
    }

    #[test]
    fn lease_unlinks_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let _clock = VtClock::create(dir.path(), 1).unwrap();
        {
            let _lease = VtClockLease::new(dir.path());
            assert!(vt_clock_path(dir.path()).is_file());
        }
        assert!(!vt_clock_path(dir.path()).exists());
    }

    #[test]
    fn unlink_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let _ = VtClock::create(dir.path(), 1).unwrap();
        assert!(vt_clock_path(dir.path()).is_file());
        VtClock::unlink(dir.path());
        assert!(!vt_clock_path(dir.path()).exists());
    }
}

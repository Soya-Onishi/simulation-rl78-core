//! Breakpoint registry.
//!
//! Milestone 1 does not implement a GDB RSP server, but the registration
//! surface must exist so later hook-up does not force a control-plane rewrite.

use crate::bus::Addr;

/// Opaque breakpoint identifier assigned by the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BreakpointId(pub u64);

/// A software breakpoint recorded on the simulation thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: BreakpointId,
    pub addr: Addr,
    pub enabled: bool,
}

/// In-kernel breakpoint table. Architecture backends may mirror it into tlib.
#[derive(Clone, Debug, Default)]
pub struct BreakpointStore {
    next_id: u64,
    items: Vec<Breakpoint>,
}

impl BreakpointStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, addr: Addr) -> BreakpointId {
        self.next_id += 1;
        let id = BreakpointId(self.next_id);
        self.items.push(Breakpoint {
            id,
            addr,
            enabled: true,
        });
        id
    }

    pub fn remove(&mut self, id: BreakpointId) -> bool {
        let before = self.items.len();
        self.items.retain(|bp| bp.id != id);
        self.items.len() != before
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    #[must_use]
    pub fn get(&self, id: BreakpointId) -> Option<&Breakpoint> {
        self.items.iter().find(|bp| bp.id == id)
    }

    #[must_use]
    pub fn hit_at(&self, pc: Addr) -> Option<BreakpointId> {
        self.items
            .iter()
            .find(|bp| bp.enabled && bp.addr == pc)
            .map(|bp| bp.id)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Breakpoint] {
        &self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_hit_and_remove() {
        let mut store = BreakpointStore::new();
        let id = store.insert(0x100);
        assert_eq!(store.hit_at(0x100), Some(id));
        assert_eq!(store.hit_at(0x101), None);
        assert!(store.remove(id));
        assert_eq!(store.hit_at(0x100), None);
    }

    #[test]
    fn clear_removes_all() {
        let mut store = BreakpointStore::new();
        let _ = store.insert(0x100);
        let _ = store.insert(0x200);
        store.clear();
        assert!(store.as_slice().is_empty());
        assert_eq!(store.hit_at(0x100), None);
    }
}

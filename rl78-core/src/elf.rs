//! ELF loader hook. Phase E fills this in; only ELF is in scope (no bin/mot).

use sim_kernel::MemoryBus;

/// Result of a successful load (entry point and such).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfLoad {
    pub entry: u64,
}

/// ELF load failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    NotImplemented,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplemented => write!(f, "ELF loader is not implemented yet"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Load a guest ELF into `bus`. Stub until phase E.
pub fn load_elf(_image: &[u8], _bus: &mut MemoryBus) -> Result<ElfLoad, LoadError> {
    Err(LoadError::NotImplemented)
}

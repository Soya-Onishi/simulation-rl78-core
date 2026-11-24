//! Node process runtime skeleton (#141 / #147 / #151).
//!
//! Phase 2-a ships crate placement and re-exports the `host_ctrl` mapping the
//! Node opens. IPC / Probe / Simulation threads land in later Phase 2 slices.

pub use contract::{HostCtrl, host_ctrl_path};

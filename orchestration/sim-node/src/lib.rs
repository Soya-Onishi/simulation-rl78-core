//! Node process runtime skeleton (#141 / #147 / #151).
//!
//! Phase 2-a ships crate placement and re-exports the shared [`host_ctrl`]
//! mapping API. IPC / Probe / Simulation threads land in later Phase 2 slices.

pub use host_ctrl::{HostCtrl, HostCtrlLease, host_ctrl_path};

//! Host control shared memory `{iox_root}/host_ctrl` (min remote VT, board VT,
//! stop generation).
//!
//! Replaces the legacy `vt_clock` file (arbiter ceiling + board VT).
//! [`layout`] is the versioned bytes-in-memory contract; [`mapping`] creates,
//! opens, and accesses the file mapping shared by Command and Node.
//!
//! Same-host stop sync uses this SHM only. Endpoint iceoryx payloads on the
//! same host must not carry a stop mark (#141).

pub mod layout;
pub mod mapping;

pub use layout::{
    HOST_CTRL_FILE, HOST_CTRL_MAGIC, HOST_CTRL_VERSION, HostCtrlAtoms, HostCtrlHeader,
    HostCtrlLayout, STOP_GEN_SATURATED,
};
pub use mapping::{HostCtrl, HostCtrlLease, host_ctrl_path};

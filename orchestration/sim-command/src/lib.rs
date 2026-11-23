//! Command host runtime skeleton (#141 / #139).
//!
//! Owns the Orchestrator-facing axum router. Handlers are stubs until the
//! four Command threads (command / build / probe / endpoint) are wired.
//!
//! Phase 2-a: [`host_ctrl`] mapping is available for session create; thread
//! wiring is a later Phase 2 slice.

pub mod router;

pub use host_ctrl::{HostCtrl, HostCtrlLease, host_ctrl_path};
pub use router::command_router;

//! Command host runtime skeleton (#141 / #139).
//!
//! Owns the Orchestrator-facing axum router. Handlers are stubs until the
//! four Command threads (command / build / probe / endpoint) are wired.
//!
//! Phase 2-a: the `host_ctrl` mapping ([`contract::HostCtrl`]) is available for
//! session create; thread wiring is a later Phase 2 slice.

pub mod router;

pub use router::command_router;

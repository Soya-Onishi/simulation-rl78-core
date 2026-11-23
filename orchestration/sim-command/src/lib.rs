//! Command host runtime skeleton (#141 / #139).
//!
//! Owns the Orchestrator-facing axum router. Handlers are stubs until the
//! four Command threads (command / build / probe / endpoint) are wired.

pub mod router;

pub use router::command_router;

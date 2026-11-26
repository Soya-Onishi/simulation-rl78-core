//! Command host runtime (#141 / #139 / #147).
//!
//! [`CommandHost`] runs the Command / Build / Probe / Endpoint threads and
//! serves the Orchestrator-facing REST API ([`CommandHost::router`]). Handlers
//! forward each request to the Command thread, which owns the sessions, the
//! node registration table, and the Command → Node RR clients.

mod build;
mod command;
mod endpoint;
mod host;
mod probe;
mod router;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use host::{CommandConfig, CommandHost, CommandHostError};

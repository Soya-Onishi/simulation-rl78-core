//! Opaque / structured identifiers shared across wire, SHM paths, and REST.

pub mod cluster;
pub mod hashed;

pub use cluster::{ClusterId, ParseClusterIdError};
pub use hashed::{HashedId, Hexable};

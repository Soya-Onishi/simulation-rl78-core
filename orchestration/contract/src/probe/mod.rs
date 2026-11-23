//! Probe live wire samples and on-disk JSON log records.

pub mod log;
pub mod wire;

pub use log::{DigitalProbeData, ProbeLogRecord};
pub use wire::{DigitalLiveValue, ProbeLiveSample};

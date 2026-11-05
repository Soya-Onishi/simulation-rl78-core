//! JSON bodies used only on the internal HTTP API.

use serde::{Deserialize, Serialize};

use crate::probe_log::ProbePointName;

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthBody {
    pub status: String,
    /// Configured board capacity for this backend.
    pub max_boards: u32,
    /// Boards occupying capacity on this backend (sum of boards across all
    /// simulations still retained by the worker, including stopped ones).
    pub running_boards: u32,
    /// `BackendConfig.time_sync_period_ms`; must match across participating backends.
    pub time_sync_period_ms: u64,
    /// `BackendConfig.data_udp_window`; must match across participating backends.
    pub data_udp_window: u32,
    /// `BackendConfig.multicast_base`; must match across participating backends.
    pub multicast_base: String,
}

/// Static `BackendConfig` values the internal API reports via health.
#[derive(Clone, Debug)]
pub struct BackendInfo {
    pub max_boards: u32,
    pub time_sync_period_ms: u64,
    pub data_udp_window: u32,
    pub multicast_base: String,
}

impl BackendInfo {
    /// Same defaults as `test_backend_yaml` (tests only).
    #[cfg(test)]
    pub(crate) fn for_tests(max_boards: u32) -> Self {
        Self {
            max_boards,
            time_sync_period_ms: 100,
            data_udp_window: 32,
            multicast_base: "239.255.0.0".into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExistsBody {
    pub exists: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HighWaterBody {
    pub seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewerTokenBody {
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CsvStartBody {
    pub request_id: u64,
    pub probes: Vec<ProbePointName>,
}

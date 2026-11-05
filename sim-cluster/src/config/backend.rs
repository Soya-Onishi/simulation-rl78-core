//! `cluster-backend` YAML.

use std::net::Ipv4Addr;
use std::path::Path;

use serde::Deserialize;

use super::{ConfigError, ListenAddr, validate_ip_or_hostname, validate_port};

/// `cluster-backend` process configuration. Every field is required.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub listen: ListenAddr,
    /// Maximum number of boards this backend may run concurrently.
    pub max_boards: u32,
    /// IPv4 address peers use to reach this machine's arbiter endpoints.
    pub advertise_ip: String,
    /// Wall-clock interval between time-sync multicast sends (milliseconds).
    pub time_sync_period_ms: u64,
    /// Sliding-window size (messages) for cross-machine UART/analog UDP.
    pub data_udp_window: u32,
    /// IPv4 multicast base address used when allocating per-simulation groups.
    pub multicast_base: String,
}

impl BackendConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let cfg: Self = super::load_yaml(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        validate_ip_or_hostname(&self.listen.ip, "listen.ip")?;
        validate_port(self.listen.port, "listen.port")?;
        if self.max_boards == 0 {
            return Err(ConfigError::Message(
                "max_boards: must be greater than 0".into(),
            ));
        }
        validate_advertise_ip(&self.advertise_ip)?;
        if self.time_sync_period_ms == 0 {
            return Err(ConfigError::Message(
                "time_sync_period_ms: must be greater than 0".into(),
            ));
        }
        if self.data_udp_window == 0 {
            return Err(ConfigError::Message(
                "data_udp_window: must be greater than 0".into(),
            ));
        }
        validate_multicast_base(&self.multicast_base)?;
        Ok(())
    }
}

fn validate_advertise_ip(value: &str) -> Result<(), ConfigError> {
    let Ok(addr) = value.parse::<Ipv4Addr>() else {
        return Err(ConfigError::Message(format!(
            "advertise_ip: expected IPv4 address, got {value:?}"
        )));
    };
    if addr.is_unspecified() {
        return Err(ConfigError::Message(
            "advertise_ip: must not be 0.0.0.0".into(),
        ));
    }
    if addr.is_multicast() {
        return Err(ConfigError::Message(
            "advertise_ip: must not be a multicast address".into(),
        ));
    }
    Ok(())
}

fn validate_multicast_base(value: &str) -> Result<(), ConfigError> {
    let Ok(addr) = value.parse::<Ipv4Addr>() else {
        return Err(ConfigError::Message(format!(
            "multicast_base: expected IPv4 multicast address, got {value:?}"
        )));
    };
    if !addr.is_multicast() {
        return Err(ConfigError::Message(format!(
            "multicast_base: expected multicast address (224.0.0.0/4), got {value}"
        )));
    }
    Ok(())
}

/// Minimal valid backend YAML body for tests.
#[cfg(test)]
pub(crate) fn test_backend_yaml(listen_ip: &str, listen_port: u16) -> String {
    format!(
        "listen:\n  ip: {listen_ip}\n  port: {listen_port}\n\
         max_boards: 8\n\
         advertise_ip: 127.0.0.1\n\
         time_sync_period_ms: 100\n\
         data_udp_window: 32\n\
         multicast_base: 239.255.0.0\n"
    )
}

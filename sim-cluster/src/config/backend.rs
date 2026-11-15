//! `cluster-backend` YAML.

use std::net::Ipv4Addr;
use std::path::Path;

use serde::Deserialize;

use super::{ConfigError, parse_ipv4_strict, validate_port};

/// `cluster-backend` process configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    /// Bind address, `machine_id`, and multicast send/join interface (IPv4).
    ///
    /// Must be a specific host address (not `0.0.0.0`). Matches
    /// `frontend.yaml` `servers[].ip`.
    pub addr: String,
    pub port: u16,
}

impl BackendConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let cfg: Self = super::load_yaml(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let _ = self.ipv4()?;
        validate_port(self.port, "port")?;
        Ok(())
    }

    /// Parsed `addr` (rejects hostnames and `0.0.0.0`).
    pub fn ipv4(&self) -> Result<Ipv4Addr, ConfigError> {
        let ip = parse_ipv4_strict(&self.addr, "addr")?;
        if ip.is_unspecified() {
            return Err(ConfigError::Message(
                "addr: must be a specific IPv4 address (not 0.0.0.0)".into(),
            ));
        }
        Ok(ip)
    }

    /// `machine_id` = IPv4 octets as big-endian [`MachineId`].
    pub fn machine_id(&self) -> Result<super::MachineId, ConfigError> {
        Ok(super::machine_id_from_ipv4(self.ipv4()?))
    }

    /// HTTP listen address (`addr:port`).
    #[must_use]
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.addr, self.port)
    }
}

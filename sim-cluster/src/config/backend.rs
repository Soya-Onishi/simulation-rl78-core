//! `cluster-backend` YAML.

use std::net::Ipv4Addr;
use std::path::Path;

use serde::Deserialize;

use super::{
    ConfigError, ListenAddr, parse_ipv4_strict, validate_ip_or_hostname, validate_port,
};

/// `cluster-backend` process configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub listen: ListenAddr,
    /// Identity IPv4 for `machine_id`. Required when `listen.ip` is `0.0.0.0`.
    #[serde(default)]
    pub advertise_ip: Option<String>,
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
        let _ = self.identity_ipv4()?;
        Ok(())
    }

    /// IPv4 used for `machine_id` (and matching `frontend.yaml` `servers[].ip`).
    pub fn identity_ipv4(&self) -> Result<Ipv4Addr, ConfigError> {
        if let Some(ip) = self.advertise_ip.as_deref() {
            return parse_ipv4_strict(ip, "advertise_ip");
        }
        let listen = parse_listen_ip_for_identity(&self.listen.ip)?;
        if listen.is_unspecified() {
            return Err(ConfigError::Message(
                "advertise_ip: required when listen.ip is 0.0.0.0".into(),
            ));
        }
        Ok(listen)
    }

    /// `machine_id` = IPv4 octets as big-endian `u32`.
    pub fn machine_id(&self) -> Result<u32, ConfigError> {
        Ok(super::machine_id_from_ipv4(self.identity_ipv4()?))
    }
}

fn parse_listen_ip_for_identity(ip: &str) -> Result<Ipv4Addr, ConfigError> {
    if let Ok(addr) = ip.parse::<Ipv4Addr>() {
        return Ok(addr);
    }
    Err(ConfigError::Message(format!(
        "listen.ip: machine_id requires IPv4 (got {ip:?}); set advertise_ip to an IPv4 address"
    )))
}

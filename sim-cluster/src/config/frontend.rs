//! `simulation-frontend` YAML.

use std::path::Path;

use serde::Deserialize;

use super::{ConfigError, ListenAddr, validate_ip_or_hostname, validate_port};

/// One backend in `servers`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerAddr {
    pub ip: String,
    pub port: u16,
}

/// `simulation-frontend` process configuration. Every field is required.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendConfig {
    pub listen: ListenAddr,
    pub servers: Vec<ServerAddr>,
}

impl FrontendConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let cfg: Self = super::load_yaml(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        validate_ip_or_hostname(&self.listen.ip, "listen.ip")?;
        validate_port(self.listen.port, "listen.port")?;
        if self.servers.is_empty() {
            return Err(ConfigError::Message(
                "servers: at least one backend is required".into(),
            ));
        }
        for (index, server) in self.servers.iter().enumerate() {
            validate_ip_or_hostname(&server.ip, &format!("servers[{index}].ip"))?;
            validate_port(server.port, &format!("servers[{index}].port"))?;
        }
        Ok(())
    }
}

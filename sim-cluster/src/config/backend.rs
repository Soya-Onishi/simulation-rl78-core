//! `cluster-backend` YAML.

use std::path::Path;

use serde::Deserialize;

use super::{ConfigError, ListenAddr, validate_ip_or_hostname, validate_port};

/// `cluster-backend` process configuration. Every field is required.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub listen: ListenAddr,
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
        Ok(())
    }
}

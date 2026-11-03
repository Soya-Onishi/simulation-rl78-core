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
    /// Directory of the Vite build (`index.html` + `assets/`).
    ///
    /// Relative paths are resolved from the directory that contains the YAML.
    pub web_root: std::path::PathBuf,
}

impl FrontendConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut cfg: Self = super::load_yaml(path)?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        if !cfg.web_root.is_absolute() {
            cfg.web_root = base.join(&cfg.web_root);
        }
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
        if !self.web_root.is_dir() {
            return Err(ConfigError::Message(format!(
                "web_root: not a directory: {}",
                self.web_root.display()
            )));
        }
        if !self.web_root.join("index.html").is_file() {
            return Err(ConfigError::Message(format!(
                "web_root: missing index.html under {}",
                self.web_root.display()
            )));
        }
        Ok(())
    }
}

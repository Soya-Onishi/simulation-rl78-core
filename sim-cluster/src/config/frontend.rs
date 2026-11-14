//! `simulation-frontend` YAML.

use std::net::Ipv4Addr;
use std::path::Path;

use serde::Deserialize;

use super::{
    ConfigError, ListenAddr, MulticastConfig, parse_ipv4_strict, validate_ip_or_hostname,
    validate_port,
};

/// One backend in `servers`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerAddr {
    pub ip: String,
    pub port: u16,
    /// Max concurrent board processes this backend may host (across all sims).
    pub max_nodes: u32,
}

/// `simulation-frontend` process configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendConfig {
    pub listen: ListenAddr,
    pub servers: Vec<ServerAddr>,
    /// Multicast address/port pools. Required when `servers.len() >= 2`.
    #[serde(default)]
    pub multicast: Option<MulticastConfig>,
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
            // `machine_id` is derived from IPv4; hostnames are not allowed here.
            parse_ipv4_strict(&server.ip, &format!("servers[{index}].ip"))?;
            validate_port(server.port, &format!("servers[{index}].port"))?;
            if server.max_nodes == 0 {
                return Err(ConfigError::Message(format!(
                    "servers[{index}].max_nodes: must be >= 1"
                )));
            }
            if let Some(first) = self.servers[..index]
                .iter()
                .position(|prev| prev.ip == server.ip && prev.port == server.port)
            {
                return Err(ConfigError::Message(format!(
                    "servers[{index}]: duplicate endpoint {}:{} (same as servers[{first}])",
                    server.ip, server.port
                )));
            }
        }
        if self.servers.len() >= 2 {
            let Some(multicast) = self.multicast.as_ref() else {
                return Err(ConfigError::Message(
                    "multicast: required when servers has 2 or more entries".into(),
                ));
            };
            multicast.validate("multicast")?;
            // Participating backends must have unique IPs (one backend per machine).
            for (index, server) in self.servers.iter().enumerate() {
                if let Some(first) = self.servers[..index]
                    .iter()
                    .position(|prev| prev.ip == server.ip)
                {
                    return Err(ConfigError::Message(format!(
                        "servers[{index}].ip: duplicate IPv4 {} (same as servers[{first}]); \
                         one cluster-backend per machine",
                        server.ip
                    )));
                }
            }
        } else if let Some(multicast) = self.multicast.as_ref() {
            multicast.validate("multicast")?;
        }
        if !self.web_root.is_dir() {
            return Err(ConfigError::Message(format!(
                "web_root: not a directory: {} (run `npm run build` in sim-cluster/frontend)",
                self.web_root.display()
            )));
        }
        if !self.web_root.join("index.html").is_file() {
            return Err(ConfigError::Message(format!(
                "web_root: missing index.html under {} (run `npm run build` in sim-cluster/frontend)",
                self.web_root.display()
            )));
        }
        Ok(())
    }

    /// IPv4 of `servers[index]` for `machine_id` / placement keys.
    #[must_use]
    pub fn server_ipv4(&self, index: usize) -> Option<Ipv4Addr> {
        self.servers
            .get(index)
            .and_then(|s| s.ip.parse::<Ipv4Addr>().ok())
    }
}

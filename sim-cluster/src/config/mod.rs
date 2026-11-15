//! YAML process configuration for `simulation-frontend` and `cluster-backend`.

mod backend;
mod frontend;

use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;

pub use backend::BackendConfig;
pub use frontend::{FrontendConfig, ServerAddr};

/// `listen.ip` / `listen.port` for `frontend.yaml` (backend uses top-level `addr` / `port`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenAddr {
    pub ip: String,
    pub port: u16,
}

/// Multicast address and port pools in `frontend.yaml`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MulticastConfig {
    /// Inclusive IPv4 ranges, e.g. `"239.0.0.1-239.0.0.10"`.
    pub address_ranges: Vec<String>,
    /// Inclusive port ranges, e.g. `"30000-30100"`.
    pub port_ranges: Vec<String>,
}

impl MulticastConfig {
    pub fn validate(&self, field: &str) -> Result<(), ConfigError> {
        if self.address_ranges.is_empty() {
            return Err(ConfigError::Message(format!(
                "{field}.address_ranges: at least one range is required"
            )));
        }
        if self.port_ranges.is_empty() {
            return Err(ConfigError::Message(format!(
                "{field}.port_ranges: at least one range is required"
            )));
        }
        for (i, range) in self.address_ranges.iter().enumerate() {
            parse_ipv4_range(range).map_err(|msg| {
                ConfigError::Message(format!("{field}.address_ranges[{i}]: {msg}"))
            })?;
        }
        for (i, range) in self.port_ranges.iter().enumerate() {
            parse_port_range(range)
                .map_err(|msg| ConfigError::Message(format!("{field}.port_ranges[{i}]: {msg}")))?;
        }
        Ok(())
    }

    /// Parsed inclusive IPv4 ranges.
    pub fn ipv4_ranges(&self) -> Result<Vec<(Ipv4Addr, Ipv4Addr)>, ConfigError> {
        self.address_ranges
            .iter()
            .enumerate()
            .map(|(i, range)| {
                parse_ipv4_range(range).map_err(|msg| {
                    ConfigError::Message(format!("multicast.address_ranges[{i}]: {msg}"))
                })
            })
            .collect()
    }

    /// Parsed inclusive port ranges.
    pub fn port_u16_ranges(&self) -> Result<Vec<(u16, u16)>, ConfigError> {
        self.port_ranges
            .iter()
            .enumerate()
            .map(|(i, range)| {
                parse_port_range(range).map_err(|msg| {
                    ConfigError::Message(format!("multicast.port_ranges[{i}]: {msg}"))
                })
            })
            .collect()
    }
}

/// Backend / multicast host identity derived from an IPv4 address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MachineId(u32);

impl MachineId {
    /// Wrap a raw big-endian IPv4 `u32` (wire / config form).
    #[must_use]
    pub const fn from_u32(raw: u32) -> Self {
        Self(raw)
    }

    /// Raw value used on the multicast wire and in hex logs.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    #[must_use]
    pub fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    #[must_use]
    pub fn from_be_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

impl std::fmt::Display for MachineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#010x}", self.0)
    }
}

/// `MachineId` from an IPv4 address (network byte order / big-endian octets).
#[must_use]
pub fn machine_id_from_ipv4(ip: Ipv4Addr) -> MachineId {
    MachineId::from_be_bytes(ip.octets())
}

/// Parse a single IPv4 address (reject hostnames).
pub fn parse_ipv4_strict(value: &str, field: &str) -> Result<Ipv4Addr, ConfigError> {
    value
        .parse::<Ipv4Addr>()
        .map_err(|_| ConfigError::Message(format!("{field}: expected IPv4 address, got {value:?}")))
}

/// Parse `"a.b.c.d-e.f.g.h"` (inclusive). Single address allowed (`"a.b.c.d"`).
pub fn parse_ipv4_range(text: &str) -> Result<(Ipv4Addr, Ipv4Addr), String> {
    let text = text.trim();
    if let Some((start, end)) = text.split_once('-') {
        let start: Ipv4Addr = start
            .trim()
            .parse()
            .map_err(|_| format!("invalid IPv4 range start in {text:?}"))?;
        let end: Ipv4Addr = end
            .trim()
            .parse()
            .map_err(|_| format!("invalid IPv4 range end in {text:?}"))?;
        if u32::from(start) > u32::from(end) {
            return Err(format!("IPv4 range start > end in {text:?}"));
        }
        if !start.is_multicast() || !end.is_multicast() {
            return Err(format!(
                "IPv4 range must be multicast addresses (224.0.0.0/4), got {text:?}"
            ));
        }
        Ok((start, end))
    } else {
        let addr: Ipv4Addr = text
            .parse()
            .map_err(|_| format!("invalid IPv4 address {text:?}"))?;
        if !addr.is_multicast() {
            return Err(format!(
                "IPv4 must be a multicast address (224.0.0.0/4), got {text:?}"
            ));
        }
        Ok((addr, addr))
    }
}

/// Parse `"lo-hi"` (inclusive). Single port allowed (`"30000"`).
pub fn parse_port_range(text: &str) -> Result<(u16, u16), String> {
    let text = text.trim();
    if let Some((start, end)) = text.split_once('-') {
        let start: u16 = start
            .trim()
            .parse()
            .map_err(|_| format!("invalid port range start in {text:?}"))?;
        let end: u16 = end
            .trim()
            .parse()
            .map_err(|_| format!("invalid port range end in {text:?}"))?;
        if start == 0 || end == 0 {
            return Err(format!("port must be 1..=65535 in {text:?}"));
        }
        if start > end {
            return Err(format!("port range start > end in {text:?}"));
        }
        Ok((start, end))
    } else {
        let port: u16 = text.parse().map_err(|_| format!("invalid port {text:?}"))?;
        if port == 0 {
            return Err(format!("port must be 1..=65535, got {text:?}"));
        }
        Ok((port, port))
    }
}

impl ListenAddr {
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }

    pub fn http_base(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }
}

/// CLI / file errors while loading YAML.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Message(String),
}

/// `--help` versus a failed parse. `exit_code` is the process status.
#[derive(Debug)]
pub enum ConfigCli {
    Help,
    Failed { message: String, exit_code: i32 },
}

/// Read `--config <path>` from argv. Relative paths use the process cwd.
pub fn parse_config_path(
    args: impl IntoIterator<Item = String>,
    bin: &str,
) -> Result<PathBuf, ConfigCli> {
    let mut args = args.into_iter();
    let mut config = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(ConfigCli::Help),
            "--config" => {
                let Some(path) = args.next() else {
                    return Err(ConfigCli::Failed {
                        message: format!("{bin}: --config requires a path"),
                        exit_code: 2,
                    });
                };
                if config.is_some() {
                    return Err(ConfigCli::Failed {
                        message: format!("{bin}: --config specified more than once"),
                        exit_code: 2,
                    });
                }
                config = Some(path);
            }
            other => {
                return Err(ConfigCli::Failed {
                    message: format!("{bin}: unknown argument {other}"),
                    exit_code: 2,
                });
            }
        }
    }
    let Some(path) = config else {
        return Err(ConfigCli::Failed {
            message: format!("{bin}: --config <path> is required"),
            exit_code: 2,
        });
    };
    Ok(resolve_config_path(&path))
}

/// Resolve a config path. Relative paths are joined to the cwd.
#[must_use]
pub fn resolve_config_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path,
        }
    }
}

pub fn load_yaml<T: DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let text = fs::read_to_string(path)
        .map_err(|err| ConfigError::Message(format!("failed to read {}: {err}", path.display())))?;
    serde_yaml::from_str(&text)
        .map_err(|err| ConfigError::Message(format!("invalid YAML in {}: {err}", path.display())))
}

pub fn validate_ip_or_hostname(value: &str, field: &str) -> Result<(), ConfigError> {
    if !is_valid_ip_or_hostname(value) {
        return Err(ConfigError::Message(format!(
            "{field}: expected IPv4 address or hostname, got {value:?}"
        )));
    }
    Ok(())
}

pub fn validate_port(port: u16, field: &str) -> Result<(), ConfigError> {
    if port == 0 {
        return Err(ConfigError::Message(format!(
            "{field}: port must be 1..=65535"
        )));
    }
    Ok(())
}

fn is_valid_ip_or_hostname(value: &str) -> bool {
    if value.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    is_valid_hostname(value)
}

fn is_valid_hostname(value: &str) -> bool {
    if value.len() > 253 {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// `listen.ip` for GDB `host`. `0.0.0.0` becomes `127.0.0.1`.
#[must_use]
pub fn advertise_host_from_ip(ip: &str) -> String {
    if ip
        .parse::<Ipv4Addr>()
        .ok()
        .is_some_and(|addr| addr.is_unspecified())
    {
        Ipv4Addr::LOCALHOST.to_string()
    } else {
        ip.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, body: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .prefix(name)
            .suffix(".yaml")
            .tempfile()
            .unwrap();
        file.write_all(body.as_bytes()).unwrap();
        file
    }

    #[test]
    fn parse_config_path_requires_flag() {
        let err = parse_config_path(Vec::<String>::new(), "bin").unwrap_err();
        match err {
            ConfigCli::Failed { exit_code, message } => {
                assert_eq!(exit_code, 2);
                assert!(message.contains("--config"));
            }
            ConfigCli::Help => panic!("expected failed"),
        }
    }

    #[test]
    fn parse_config_path_help() {
        assert!(matches!(
            parse_config_path(["--help".into()], "bin"),
            Err(ConfigCli::Help)
        ));
    }

    #[test]
    fn parse_config_path_relative_uses_cwd() {
        let path = parse_config_path(
            ["--config".into(), "example/config/frontend.yaml".into()],
            "bin",
        )
        .unwrap();
        assert!(path.is_absolute());
        assert!(path.ends_with("example/config/frontend.yaml"));
    }

    #[test]
    fn frontend_yaml_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
        let yaml = dir.path().join("frontend.yaml");
        std::fs::write(
            &yaml,
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers:\n  - ip: 127.0.0.1\n    port: 8091\n    max_nodes: 4\nweb_root: web\n",
        )
        .unwrap();
        let cfg = FrontendConfig::load(&yaml).unwrap();
        assert_eq!(cfg.listen.ip, "127.0.0.1");
        assert_eq!(cfg.listen.port, 8090);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].port, 8091);
        assert_eq!(cfg.servers[0].max_nodes, 4);
        assert!(cfg.multicast.is_none());
        assert_eq!(cfg.web_root, web);
    }

    #[test]
    fn frontend_requires_multicast_for_two_servers() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
        let yaml = dir.path().join("frontend.yaml");
        std::fs::write(
            &yaml,
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers:\n  - ip: 10.0.0.1\n    port: 8091\n    max_nodes: 2\n  - ip: 10.0.0.2\n    port: 8092\n    max_nodes: 2\nweb_root: web\n",
        )
        .unwrap();
        let err = FrontendConfig::load(&yaml).unwrap_err();
        assert!(err.to_string().contains("multicast"), "{err}");
    }

    #[test]
    fn frontend_two_servers_with_multicast_ok() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
        let yaml = dir.path().join("frontend.yaml");
        std::fs::write(
            &yaml,
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers:\n  - ip: 10.0.0.1\n    port: 8091\n    max_nodes: 2\n  - ip: 10.0.0.2\n    port: 8092\n    max_nodes: 2\nmulticast:\n  address_ranges:\n    - \"239.0.0.1-239.0.0.3\"\n  port_ranges:\n    - \"30000-30002\"\nweb_root: web\n",
        )
        .unwrap();
        let cfg = FrontendConfig::load(&yaml).unwrap();
        assert_eq!(cfg.servers.len(), 2);
        assert!(cfg.multicast.is_some());
    }

    #[test]
    fn backend_yaml_round_trip() {
        let file = write_temp("backend", "addr: 127.0.0.1\nport: 8091\n");
        let cfg = BackendConfig::load(file.path()).unwrap();
        assert_eq!(cfg.port, 8091);
        assert_eq!(cfg.ipv4().unwrap(), Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(
            cfg.machine_id().unwrap(),
            machine_id_from_ipv4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(cfg.socket_addr(), "127.0.0.1:8091");
    }

    #[test]
    fn backend_rejects_unspecified_addr() {
        let file = write_temp("backend0", "addr: 0.0.0.0\nport: 8091\n");
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("addr"), "{err}");
    }

    #[test]
    fn backend_addr_specific_ipv4() {
        let file = write_temp("backend-addr", "addr: 10.0.0.5\nport: 8091\n");
        let cfg = BackendConfig::load(file.path()).unwrap();
        assert_eq!(cfg.ipv4().unwrap(), Ipv4Addr::new(10, 0, 0, 5));
        assert_eq!(
            cfg.machine_id().unwrap(),
            machine_id_from_ipv4(Ipv4Addr::new(10, 0, 0, 5))
        );
    }

    #[test]
    fn machine_id_uses_be_octets() {
        assert_eq!(
            machine_id_from_ipv4(Ipv4Addr::new(10, 1, 2, 3)),
            MachineId::from_u32(u32::from_be_bytes([10, 1, 2, 3]))
        );
    }

    #[test]
    fn parse_ranges() {
        assert_eq!(
            parse_ipv4_range("239.0.0.1-239.0.0.3").unwrap(),
            (Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 3))
        );
        assert_eq!(parse_port_range("30000-30002").unwrap(), (30000, 30002));
        assert!(
            parse_ipv4_range("10.0.0.1")
                .unwrap_err()
                .contains("multicast")
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        let file = write_temp("unknown", "addr: 127.0.0.1\nport: 8091\nextra: 1\n");
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("invalid YAML"), "{err}");
    }

    #[test]
    fn empty_servers_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
        let yaml = dir.path().join("frontend.yaml");
        std::fs::write(
            &yaml,
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers: []\nweb_root: web\n",
        )
        .unwrap();
        let err = FrontendConfig::load(&yaml).unwrap_err();
        assert!(err.to_string().contains("servers"), "{err}");
    }

    #[test]
    fn port_zero_rejected() {
        let file = write_temp("port0", "addr: 127.0.0.1\nport: 0\n");
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
    }

    #[test]
    fn empty_addr_rejected() {
        let file = write_temp("empty-addr", "addr: \"\"\nport: 8091\n");
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("addr"), "{err}");
    }

    #[test]
    fn hostname_addr_rejected() {
        let file = write_temp("host", "addr: localhost\nport: 8091\n");
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(
            err.to_string().contains("addr") || err.to_string().contains("IPv4"),
            "{err}"
        );
    }

    #[test]
    fn advertise_host_maps_unspecified() {
        assert_eq!(advertise_host_from_ip("0.0.0.0"), "127.0.0.1");
        assert_eq!(advertise_host_from_ip("192.168.1.10"), "192.168.1.10");
        assert_eq!(advertise_host_from_ip("localhost"), "localhost");
    }

    #[test]
    fn example_yaml_files_load() {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../example/config");
        BackendConfig::load(&example.join("backend.yaml")).unwrap();

        // frontend.yaml under example/ points at the Vite build output (gitignored).
        // Load a checked-in fixture so clean checkouts do not need `npm run build`.
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/frontend.yaml");
        let cfg = FrontendConfig::load(&fixture).unwrap();
        assert!(cfg.web_root.join("index.html").is_file());
    }
}

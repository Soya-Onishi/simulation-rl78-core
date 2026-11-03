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

/// `listen.ip` / `listen.port` shared by both YAML files.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenAddr {
    pub ip: String,
    pub port: u16,
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
        let file = write_temp(
            "frontend",
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers:\n  - ip: 127.0.0.1\n    port: 8091\n",
        );
        let cfg = FrontendConfig::load(file.path()).unwrap();
        assert_eq!(cfg.listen.ip, "127.0.0.1");
        assert_eq!(cfg.listen.port, 8090);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].port, 8091);
    }

    #[test]
    fn backend_yaml_round_trip() {
        let file = write_temp("backend", "listen:\n  ip: 127.0.0.1\n  port: 8091\n");
        let cfg = BackendConfig::load(file.path()).unwrap();
        assert_eq!(cfg.listen.port, 8091);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let file = write_temp(
            "unknown",
            "listen:\n  ip: 127.0.0.1\n  port: 8091\nextra: 1\n",
        );
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("invalid YAML"), "{err}");
    }

    #[test]
    fn empty_servers_rejected() {
        let file = write_temp(
            "empty-servers",
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers: []\n",
        );
        let err = FrontendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("servers"), "{err}");
    }

    #[test]
    fn port_zero_rejected() {
        let file = write_temp("port0", "listen:\n  ip: 127.0.0.1\n  port: 0\n");
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
    }

    #[test]
    fn empty_ip_rejected() {
        let file = write_temp("empty-ip", "listen:\n  ip: \"\"\n  port: 8091\n");
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("ip"), "{err}");
    }

    #[test]
    fn hostname_is_accepted() {
        let file = write_temp("host", "listen:\n  ip: localhost\n  port: 8091\n");
        let cfg = BackendConfig::load(file.path()).unwrap();
        assert_eq!(cfg.listen.ip, "localhost");
    }

    #[test]
    fn advertise_host_maps_unspecified() {
        assert_eq!(advertise_host_from_ip("0.0.0.0"), "127.0.0.1");
        assert_eq!(advertise_host_from_ip("192.168.1.10"), "192.168.1.10");
        assert_eq!(advertise_host_from_ip("localhost"), "localhost");
    }

    #[test]
    fn example_yaml_files_load() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../example/config");
        FrontendConfig::load(&root.join("frontend.yaml")).unwrap();
        BackendConfig::load(&root.join("backend.yaml")).unwrap();
    }
}

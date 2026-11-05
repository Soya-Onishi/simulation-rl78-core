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
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
        let yaml = dir.path().join("frontend.yaml");
        std::fs::write(
            &yaml,
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers:\n  - ip: 127.0.0.1\n    port: 8091\nweb_root: web\n",
        )
        .unwrap();
        let cfg = FrontendConfig::load(&yaml).unwrap();
        assert_eq!(cfg.listen.ip, "127.0.0.1");
        assert_eq!(cfg.listen.port, 8090);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].port, 8091);
        assert_eq!(cfg.web_root, web);
    }

    #[test]
    fn backend_yaml_round_trip() {
        let file = write_temp("backend", &backend::test_backend_yaml("127.0.0.1", 8091));
        let cfg = BackendConfig::load(file.path()).unwrap();
        assert_eq!(cfg.listen.port, 8091);
        assert_eq!(cfg.max_boards, 8);
        assert_eq!(cfg.advertise_ip, "127.0.0.1");
        assert_eq!(cfg.time_sync_period_ms, 100);
        assert_eq!(cfg.data_udp_window, 32);
        assert_eq!(cfg.multicast_base, "239.255.0.0");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let mut body = backend::test_backend_yaml("127.0.0.1", 8091);
        body.push_str("extra: 1\n");
        let file = write_temp("unknown", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("invalid YAML"), "{err}");
    }

    #[test]
    fn max_boards_zero_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091)
            .replace("max_boards: 8\n", "max_boards: 0\n");
        let file = write_temp("max0", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("max_boards"), "{err}");
    }

    #[test]
    fn advertise_ip_unspecified_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091)
            .replace("advertise_ip: 127.0.0.1\n", "advertise_ip: 0.0.0.0\n");
        let file = write_temp("adv0", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("advertise_ip"), "{err}");
    }

    #[test]
    fn multicast_base_must_be_multicast() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091).replace(
            "multicast_base: 239.255.0.0\n",
            "multicast_base: 192.168.0.1\n",
        );
        let file = write_temp("mcast", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("multicast_base"), "{err}");
    }

    #[test]
    fn missing_max_boards_rejected() {
        let file = write_temp(
            "missing",
            "listen:\n  ip: 127.0.0.1\n  port: 8091\n\
             advertise_ip: 127.0.0.1\n\
             time_sync_period_ms: 100\n\
             data_udp_window: 32\n\
             multicast_base: 239.255.0.0\n",
        );
        let err = BackendConfig::load(file.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid YAML"), "{err}");
        assert!(msg.contains("max_boards"), "{err}");
    }

    #[test]
    fn time_sync_period_zero_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091)
            .replace("time_sync_period_ms: 100\n", "time_sync_period_ms: 0\n");
        let file = write_temp("period0", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("time_sync_period_ms"), "{err}");
    }

    #[test]
    fn data_udp_window_zero_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091)
            .replace("data_udp_window: 32\n", "data_udp_window: 0\n");
        let file = write_temp("win0", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("data_udp_window"), "{err}");
    }

    #[test]
    fn advertise_ip_non_ipv4_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091)
            .replace("advertise_ip: 127.0.0.1\n", "advertise_ip: localhost\n");
        let file = write_temp("adv-host", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("advertise_ip"), "{err}");
    }

    #[test]
    fn advertise_ip_multicast_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091)
            .replace("advertise_ip: 127.0.0.1\n", "advertise_ip: 239.255.0.1\n");
        let file = write_temp("adv-mcast", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("advertise_ip"), "{err}");
    }

    #[test]
    fn multicast_base_non_ipv4_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 8091).replace(
            "multicast_base: 239.255.0.0\n",
            "multicast_base: not-an-ip\n",
        );
        let file = write_temp("mcast-bad", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("multicast_base"), "{err}");
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
    fn duplicate_servers_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("index.html"), "<!doctype html>").unwrap();
        let yaml = dir.path().join("frontend.yaml");
        std::fs::write(
            &yaml,
            "listen:\n  ip: 127.0.0.1\n  port: 8090\nservers:\n  - ip: 127.0.0.1\n    port: 8091\n  - ip: 127.0.0.1\n    port: 8092\n  - ip: 127.0.0.1\n    port: 8091\nweb_root: web\n",
        )
        .unwrap();
        let err = FrontendConfig::load(&yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate endpoint 127.0.0.1:8091"), "{err}");
        assert!(msg.contains("servers[2]"), "{err}");
        assert!(msg.contains("servers[0]"), "{err}");
    }

    #[test]
    fn port_zero_rejected() {
        let body = backend::test_backend_yaml("127.0.0.1", 0);
        let file = write_temp("port0", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
    }

    #[test]
    fn empty_ip_rejected() {
        let body = backend::test_backend_yaml("", 8091);
        let file = write_temp("empty-ip", &body);
        let err = BackendConfig::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("ip"), "{err}");
    }

    #[test]
    fn hostname_is_accepted() {
        let body = backend::test_backend_yaml("localhost", 8091);
        let file = write_temp("host", &body);
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
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../example/config");
        BackendConfig::load(&example.join("backend.yaml")).unwrap();

        // frontend.yaml under example/ points at the Vite build output (gitignored).
        // Load a checked-in fixture so clean checkouts do not need `npm run build`.
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/frontend.yaml");
        let cfg = FrontendConfig::load(&fixture).unwrap();
        assert!(cfg.web_root.join("index.html").is_file());
    }
}

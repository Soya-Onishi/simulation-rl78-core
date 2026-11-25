//! `sim-node` command line: Command builds it, Node parses it (#147 / #153).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::id::{ClusterId, HashedId};

/// Identity and shared-memory location of one Node process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    /// iceoryx `root_path` and `host_ctrl` directory for the cluster on this host.
    pub iox_root: PathBuf,
    pub cluster: ClusterId,
    pub node_key: HashedId<u64>,
    /// Board count of this host's `host_ctrl` mapping.
    pub board_count: u32,
}

/// Malformed `sim-node` command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeArgsError(pub String);

impl fmt::Display for NodeArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NodeArgsError {}

impl NodeConfig {
    pub const USAGE: &'static str =
        "usage: sim-node --iox-root <dir> --cluster <id-nano> --node-key <hex16> --board-count <n>";

    /// Arguments that make [`Self::from_args`] return `self`.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        vec![
            "--iox-root".into(),
            self.iox_root.clone().into(),
            "--cluster".into(),
            self.cluster.to_string().into(),
            "--node-key".into(),
            self.node_key.to_string().into(),
            "--board-count".into(),
            self.board_count.to_string().into(),
        ]
    }

    /// Parse `sim-node` arguments (without the program name).
    pub fn from_args(args: impl IntoIterator<Item = OsString>) -> Result<Self, NodeArgsError> {
        let mut args = args.into_iter();
        let mut iox_root = None;
        let mut cluster = None;
        let mut node_key = None;
        let mut board_count = None;
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| {
                NodeArgsError(format!("missing value for {}", flag.to_string_lossy()))
            })?;
            match flag.to_str() {
                Some("--iox-root") => iox_root = Some(PathBuf::from(value)),
                Some("--cluster") => cluster = Some(parse(&flag, &value)?),
                Some("--node-key") => node_key = Some(parse(&flag, &value)?),
                Some("--board-count") => board_count = Some(parse(&flag, &value)?),
                _ => {
                    return Err(NodeArgsError(format!(
                        "unknown argument {}",
                        flag.to_string_lossy()
                    )));
                }
            }
        }
        Ok(Self {
            iox_root: iox_root.ok_or_else(|| missing("--iox-root"))?,
            cluster: cluster.ok_or_else(|| missing("--cluster"))?,
            node_key: node_key.ok_or_else(|| missing("--node-key"))?,
            board_count: board_count.ok_or_else(|| missing("--board-count"))?,
        })
    }
}

fn parse<T: FromStr>(flag: &OsStr, value: &OsStr) -> Result<T, NodeArgsError> {
    value.to_str().and_then(|s| s.parse().ok()).ok_or_else(|| {
        NodeArgsError(format!(
            "invalid {} {}",
            flag.to_string_lossy(),
            value.to_string_lossy()
        ))
    })
}

fn missing(flag: &str) -> NodeArgsError {
    NodeArgsError(format!("missing {flag}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NodeConfig {
        NodeConfig {
            iox_root: PathBuf::from("/tmp/cluster root"),
            cluster: ClusterId::new(7, 1_700_000_000_123_456_789),
            node_key: HashedId::new(0xdead_beef),
            board_count: 3,
        }
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn to_args_round_trips() {
        let config = config();
        assert_eq!(NodeConfig::from_args(config.to_args()), Ok(config));
    }

    #[test]
    fn missing_flag_is_reported() {
        let mut list = config().to_args();
        list.truncate(6);
        assert_eq!(
            NodeConfig::from_args(list),
            Err(NodeArgsError("missing --board-count".into()))
        );
    }

    #[test]
    fn flag_without_value_is_reported() {
        assert_eq!(
            NodeConfig::from_args(args(&["--iox-root"])),
            Err(NodeArgsError("missing value for --iox-root".into()))
        );
    }

    #[test]
    fn invalid_and_unknown_flags_are_rejected() {
        assert_eq!(
            NodeConfig::from_args(args(&["--board-count", "many"])),
            Err(NodeArgsError("invalid --board-count many".into()))
        );
        assert_eq!(
            NodeConfig::from_args(args(&["--node-key", "xyz"])),
            Err(NodeArgsError("invalid --node-key xyz".into()))
        );
        assert_eq!(
            NodeConfig::from_args(args(&["--verbose", "1"])),
            Err(NodeArgsError("unknown argument --verbose".into()))
        );
    }
}

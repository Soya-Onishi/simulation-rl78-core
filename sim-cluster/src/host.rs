//! Logical host commands and status shared by `cluster-server` and `cluster-arbiter`.
//!
//! The samples themselves travel on iceoryx (`ctrl/s2a` and `status/a2s`).

use serde::{Deserialize, Serialize};

/// Host command delivered to a live arbiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCommand {
    /// Leave Stopped and broadcast [`crate::control::ControlToNode::Start`]
    /// (the Start sample carries the run ceiling).
    ///
    /// `until_ns` caps virtual time for this start. `None` means unlimited.
    Start { until_ns: Option<u64> },
    /// Broadcast cluster stop and stay in Stopped (process keeps running).
    Stop,
    /// Stop, [`sim_kernel::Resettable`] reset, raise the ceiling to
    /// `now + margin`, then start.
    Reset,
    /// Leave the session and exit the arbiter process.
    Shutdown,
}

#[derive(Deserialize)]
struct CmdLine {
    cmd: String,
    #[serde(default)]
    until_ns: Option<u64>,
}

impl HostCommand {
    /// Parse one stdin line. Empty and unknown lines are ignored.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        if let Ok(parsed) = serde_json::from_str::<CmdLine>(line) {
            return match parsed.cmd.as_str() {
                "start" => Some(Self::Start {
                    until_ns: parsed.until_ns,
                }),
                "stop" => Some(Self::Stop),
                "reset" => Some(Self::Reset),
                "shutdown" => Some(Self::Shutdown),
                _ => None,
            };
        }
        match line {
            "start" => Some(Self::Start { until_ns: None }),
            "stop" => Some(Self::Stop),
            "reset" => Some(Self::Reset),
            "shutdown" => Some(Self::Shutdown),
            _ => None,
        }
    }

    /// One command as a single JSON line, including the trailing newline.
    #[must_use]
    pub fn to_line(self) -> String {
        match self {
            Self::Start {
                until_ns: Some(until_ns),
            } => format!("{{\"cmd\":\"start\",\"until_ns\":{until_ns}}}\n"),
            Self::Start { until_ns: None } => "{\"cmd\":\"start\"}\n".to_string(),
            Self::Stop => "{\"cmd\":\"stop\"}\n".to_string(),
            Self::Reset => "{\"cmd\":\"reset\"}\n".to_string(),
            Self::Shutdown => "{\"cmd\":\"shutdown\"}\n".to_string(),
        }
    }
}

/// One node row in an arbiter status line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNode {
    pub id: String,
    pub kind: String,
    pub state: String,
}

/// Arbiter → server status line (not a log record).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub state: String,
    pub virtual_time_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub nodes: Vec<SessionNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_and_bare_words() {
        assert_eq!(
            HostCommand::parse("{\"cmd\":\"start\"}\n"),
            Some(HostCommand::Start { until_ns: None })
        );
        assert_eq!(
            HostCommand::parse("{\"cmd\":\"start\",\"until_ns\":1000}\n"),
            Some(HostCommand::Start {
                until_ns: Some(1000)
            })
        );
        assert_eq!(HostCommand::parse("stop"), Some(HostCommand::Stop));
        assert_eq!(HostCommand::parse("  "), None);
        assert_eq!(HostCommand::parse("nope"), None);
    }

    #[test]
    fn line_round_trips() {
        for cmd in [
            HostCommand::Start { until_ns: None },
            HostCommand::Start { until_ns: Some(42) },
            HostCommand::Stop,
            HostCommand::Reset,
            HostCommand::Shutdown,
        ] {
            assert_eq!(HostCommand::parse(&cmd.to_line()), Some(cmd));
        }
    }
}

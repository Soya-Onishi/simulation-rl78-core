//! Logical host commands and status shared by `cluster-server` and `cluster-arbiter`.
//!
//! The samples themselves travel on iceoryx (`ctrl/s2a` and `status/a2s`).

use serde::{Deserialize, Serialize};

/// Host command delivered to a live arbiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCommand {
    /// Leave Stopped and broadcast [`crate::control::ControlToNode::Start`].
    Start,
    /// Broadcast cluster stop and stay in Stopped (process keeps running).
    Stop,
    /// Stop, [`sim_kernel::Resettable`] reset, republish the time ceiling, then start.
    Reset,
    /// Leave the session and exit the arbiter process.
    Shutdown,
}

#[derive(Deserialize)]
struct CmdLine {
    cmd: String,
}

impl HostCommand {
    /// Parse one stdin line. Empty and unknown lines are ignored.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let cmd = match serde_json::from_str::<CmdLine>(line) {
            Ok(parsed) => parsed.cmd,
            Err(_) => line.to_string(),
        };
        match cmd.as_str() {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "reset" => Some(Self::Reset),
            "shutdown" => Some(Self::Shutdown),
            _ => None,
        }
    }

    /// One command as a single JSON line, including the trailing newline.
    #[must_use]
    pub fn to_line(self) -> String {
        let name = match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Reset => "reset",
            Self::Shutdown => "shutdown",
        };
        format!("{{\"cmd\":\"{name}\"}}\n")
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
    pub nodes: Vec<SessionNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_and_bare_words() {
        assert_eq!(
            HostCommand::parse("{\"cmd\":\"start\"}\n"),
            Some(HostCommand::Start)
        );
        assert_eq!(HostCommand::parse("stop"), Some(HostCommand::Stop));
        assert_eq!(HostCommand::parse("  "), None);
        assert_eq!(HostCommand::parse("nope"), None);
    }

    #[test]
    fn line_round_trips() {
        for cmd in [
            HostCommand::Start,
            HostCommand::Stop,
            HostCommand::Reset,
            HostCommand::Shutdown,
        ] {
            assert_eq!(HostCommand::parse(&cmd.to_line()), Some(cmd));
        }
    }
}

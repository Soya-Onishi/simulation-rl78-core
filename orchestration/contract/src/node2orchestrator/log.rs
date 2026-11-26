//! Node log stream to the Orchestrator host: one JSON object per line over TCP.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// One log line emitted by a Node thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeLogRecord {
    pub level: NodeLogLevel,
    /// `log` target (module path unless overridden).
    pub target: String,
    pub message: String,
}

impl NodeLogRecord {
    /// Stream frame: the JSON object followed by `\n` (JSON escapes newlines in strings).
    #[must_use]
    pub fn to_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).expect("NodeLogRecord serializes to JSON");
        line.push(b'\n');
        line
    }

    /// Parse one frame produced by [`Self::to_line`] (trailing `\n` optional).
    pub fn from_line(line: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(line.strip_suffix(b"\n").unwrap_or(line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(message: &str) -> NodeLogRecord {
        NodeLogRecord {
            level: NodeLogLevel::Warn,
            target: "sim_node::ipc".into(),
            message: message.into(),
        }
    }

    #[test]
    fn line_is_single_json_object_with_snake_case_level() {
        let line = record("dropped").to_line();
        assert_eq!(line.last(), Some(&b'\n'));
        let v: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"level": "warn", "target": "sim_node::ipc", "message": "dropped"})
        );
    }

    #[test]
    fn multiline_message_stays_on_one_line() {
        let original = record("first\nsecond");
        let line = original.to_line();
        assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(NodeLogRecord::from_line(&line).unwrap(), original);
    }
}

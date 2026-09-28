//! Cluster log samples carried node → arbiter → server.
//!
//! [`ClusterLog`] is the iceoryx2 sample. [`LogLevel`] and [`LogOrigin`] say
//! how severe the line is and which process produced it. The wire record has
//! no cluster id: each simulation uses its own iceoryx service, and the
//! server forwards that inbox only to the web front-end that created it.
//! The server does not drop records by level; the web page filters display.

use std::fmt;

use iceoryx2::prelude::ZeroCopySend;

/// UTF-8 payload capacity of one [`ClusterLog`] (bytes).
///
/// Longer text is truncated on a char boundary in [`ClusterLog::record`].
pub const LOG_TEXT_CAP: usize = 4096;

/// Severity. Lower discriminants are more severe.
///
/// [`LogLevel::permits`] treats `self` as the maximum verbosity to show.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// `true` when `record` is at least as severe as this verbosity ceiling.
    #[must_use]
    pub fn permits(self, record: Self) -> bool {
        (record as u8) <= (self as u8)
    }

    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }
}

impl LogOrigin {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arbiter => "arbiter",
            Self::Node => "node",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Process that produced a [`ClusterLog`].
///
/// The node id itself is [`ClusterLog::board_hash`], resolved by the printer.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum LogOrigin {
    Arbiter = 1,
    Node = 2,
}

/// One log sample on the node→arbiter and arbiter→server buses.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum ClusterLog {
    Record {
        /// Board hash when [`LogOrigin::Node`]; `0` for the arbiter.
        board_hash: u64,
        level: LogLevel,
        origin: LogOrigin,
        text_len: u16,
        text: [u8; LOG_TEXT_CAP],
    },
}

impl ClusterLog {
    #[must_use]
    pub fn record(level: LogLevel, origin: LogOrigin, board_hash: u64, text: &str) -> Self {
        let fitted = fit_text(text, LOG_TEXT_CAP);
        let mut buf = [0u8; LOG_TEXT_CAP];
        buf[..fitted.len()].copy_from_slice(fitted.as_bytes());
        let board_hash = match origin {
            LogOrigin::Arbiter => 0,
            LogOrigin::Node => board_hash,
        };
        Self::Record {
            board_hash,
            level,
            origin,
            text_len: fitted.len() as u16,
            text: buf,
        }
    }

    #[must_use]
    pub fn level(self) -> LogLevel {
        match self {
            Self::Record { level, .. } => level,
        }
    }

    #[must_use]
    pub fn origin(self) -> LogOrigin {
        match self {
            Self::Record { origin, .. } => origin,
        }
    }

    #[must_use]
    pub fn board_hash(self) -> u64 {
        match self {
            Self::Record { board_hash, .. } => board_hash,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Record { text, text_len, .. } => {
                let n = (*text_len as usize).min(text.len());
                std::str::from_utf8(&text[..n]).unwrap_or("")
            }
        }
    }

    /// `[{level}] arbiter: …` or `[{level}] node {id}: …`.
    #[must_use]
    pub fn render<'a>(&self, board_name: impl Fn(u64) -> Option<&'a str>) -> String {
        let who = match self.origin() {
            LogOrigin::Arbiter => "arbiter".to_string(),
            LogOrigin::Node => {
                let name = board_name(self.board_hash()).unwrap_or("unknown");
                format!("node {name}")
            }
        };
        format!("[{}] {who}: {}", self.level().as_str(), self.text())
    }
}

/// Server-side policy: keep records up to `verbosity`, then route by level.
///
/// Error and warn go to stderr. Info, debug, and trace go to stdout.
#[derive(Clone, Copy, Debug)]
pub struct LogConsole {
    verbosity: LogLevel,
}

impl LogConsole {
    #[must_use]
    pub fn new(verbosity: LogLevel) -> Self {
        Self { verbosity }
    }

    #[must_use]
    pub fn verbosity(self) -> LogLevel {
        self.verbosity
    }

    pub fn accept<'a>(&self, record: &ClusterLog, board_name: impl Fn(u64) -> Option<&'a str>) {
        if !self.verbosity.permits(record.level()) {
            return;
        }
        let line = record.render(board_name);
        match record.level() {
            LogLevel::Error | LogLevel::Warn => eprintln!("{line}"),
            LogLevel::Info | LogLevel::Debug | LogLevel::Trace => println!("{line}"),
        }
    }
}

fn fit_text(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_ceiling_hides_more_verbose_levels() {
        assert!(LogLevel::Info.permits(LogLevel::Error));
        assert!(LogLevel::Info.permits(LogLevel::Warn));
        assert!(LogLevel::Info.permits(LogLevel::Info));
        assert!(!LogLevel::Info.permits(LogLevel::Debug));
        assert!(!LogLevel::Warn.permits(LogLevel::Info));
        assert!(LogLevel::Trace.permits(LogLevel::Trace));
    }

    #[test]
    fn from_label_accepts_warning_alias() {
        assert_eq!(LogLevel::from_label("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::from_label("trace"), Some(LogLevel::Trace));
        assert_eq!(LogLevel::from_label("verbose"), None);
    }

    #[test]
    fn render_names_arbiter_and_node() {
        let arb = ClusterLog::record(LogLevel::Info, LogOrigin::Arbiter, 99, "Start broadcast");
        assert_eq!(arb.origin(), LogOrigin::Arbiter);
        assert_eq!(arb.board_hash(), 0);
        assert_eq!(
            arb.render(|_| Some("nope")),
            "[info] arbiter: Start broadcast"
        );

        let node = ClusterLog::record(LogLevel::Warn, LogOrigin::Node, 7, "duplicate Ready");
        assert_eq!(
            node.render(|h| if h == 7 { Some("mcu0") } else { None }),
            "[warn] node mcu0: duplicate Ready"
        );
        assert_eq!(
            node.render(|_| None),
            "[warn] node unknown: duplicate Ready"
        );
    }

    #[test]
    fn long_text_is_cut_on_a_char_boundary() {
        let text = format!("start-{}-end", "あ".repeat(80));
        let record = ClusterLog::record(LogLevel::Debug, LogOrigin::Arbiter, 0, &text);
        let shown = record.text();
        assert!(shown.len() <= LOG_TEXT_CAP);
        assert!(shown.is_char_boundary(shown.len()));
        assert!(shown.starts_with("start-"));
    }
}

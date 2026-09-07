//! Control-plane messages over a Unix domain socket (star topology).

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Absolute maximum control-message payload (bytes).
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Control messages between arbiter and nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Arbiter → node: bound resources for this board.
    StartupRecord {
        board_id: String,
        /// SHM link names this node must attach (Phase 3 fills these).
        shm_segments: Vec<ShmBinding>,
        /// Virtual-time skew margin (ns); copied from topology for the node.
        margin_ns: u64,
        /// Report when `allowed - now` falls below this (ns).
        headroom_threshold_ns: u64,
    },
    /// Node → arbiter: control + SHM attach complete.
    Ready { board_id: String },
    /// Arbiter → node: simulation may enter Running.
    Start,
    /// Node → arbiter: virtual time report.
    TimeReport {
        board_id: String,
        virtual_time_ns: u64,
    },
    /// Arbiter → node: new virtual-time ceiling.
    Allowed { allowed_ns: u64 },
    /// Node → arbiter: host-initiated stop (BP / external / step / unmapped).
    /// Guest-local Halt must not be sent.
    HostStop { board_id: String, reason: String },
    /// Arbiter → node: host-initiated cluster stop (no ack on same host).
    ClusterStop { reason: String },
}

/// One SHM segment binding in a startup record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShmBinding {
    pub edge_id: String,
    pub flink_name: String,
    pub role: ShmRole,
}

/// Whether this node writes or reads the SHM segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShmRole {
    Producer,
    Consumer,
}

/// Write a length-prefixed JSON frame.
pub fn write_message(stream: &mut impl Write, msg: &ControlMessage) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME as usize {
        return Err(io::Error::other("control message too large"));
    }
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

/// Read one length-prefixed JSON frame.
pub fn read_message(stream: &mut impl Read) -> io::Result<ControlMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::other("control frame length exceeds limit"));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

/// Bind a fresh UDS listener, removing a stale path if present.
pub fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    UnixListener::bind(path)
}

/// Accept with an optional overall deadline (used for Ready barrier).
pub fn accept_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<Option<UnixStream>> {
    listener.set_nonblocking(true)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                listener.set_nonblocking(false)?;
                return Ok(Some(stream));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    listener.set_nonblocking(false)?;
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                listener.set_nonblocking(false)?;
                return Err(err);
            }
        }
    }
}

/// Read a message with a deadline on the socket read timeout.
pub fn read_message_timeout(
    stream: &mut UnixStream,
    timeout: Duration,
) -> io::Result<ControlMessage> {
    stream.set_read_timeout(Some(timeout))?;
    let result = read_message(stream);
    let _ = stream.set_read_timeout(None);
    result
}

/// Default control socket directory under the runtime dir.
#[must_use]
pub fn default_control_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sim-cluster")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn frame_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let listener = bind_listener(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let (mut server, _) = listener.accept().unwrap();

        let msg = ControlMessage::Ready {
            board_id: "a".into(),
        };
        write_message(&mut client, &msg).unwrap();
        let got = read_message(&mut server).unwrap();
        assert_eq!(got, msg);
    }
}

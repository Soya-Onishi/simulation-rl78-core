//! Minimal cluster node (Phase 2): control connect, Ready, wait for Start.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::control::{ControlMessage, read_message, write_message};

/// Node-side failures.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

/// Connect to the arbiter control socket and complete the Ready / Start handshake.
///
/// Phase 2: exits after Start. Later phases keep the simulation thread running.
pub fn run_node(board_id: &str, control: &Path) -> Result<(), NodeError> {
    let mut stream = connect_with_retry(control, Duration::from_secs(5))?;
    let startup = read_message(&mut stream)?;
    match startup {
        ControlMessage::StartupRecord {
            board_id: id,
            shm_segments,
        } => {
            if id != board_id {
                return Err(NodeError::Message(format!(
                    "startup board_id '{id}' != --board-id '{board_id}'"
                )));
            }
            eprintln!(
                "cluster-node[{board_id}]: StartupRecord ({} shm segments)",
                shm_segments.len()
            );
        }
        other => {
            return Err(NodeError::Message(format!(
                "expected StartupRecord, got {other:?}"
            )));
        }
    }

    write_message(
        &mut stream,
        &ControlMessage::Ready {
            board_id: board_id.to_string(),
        },
    )?;
    eprintln!("cluster-node[{board_id}]: Ready");

    let start = read_message(&mut stream)?;
    match start {
        ControlMessage::Start => {
            eprintln!("cluster-node[{board_id}]: Start");
        }
        other => {
            return Err(NodeError::Message(format!("expected Start, got {other:?}")));
        }
    }

    // Hold the process briefly so the arbiter can observe a live child.
    thread::sleep(Duration::from_millis(100));
    Ok(())
}

fn connect_with_retry(path: &Path, budget: Duration) -> Result<UnixStream, NodeError> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if std::time::Instant::now() >= deadline {
                    return Err(NodeError::Io(err));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Helper used by the binary for typed args.
#[derive(Clone, Debug)]
pub struct NodeOptions {
    pub board_id: String,
    pub control: PathBuf,
}

//! Minimal cluster node: event-driven control handshake, then hold.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::control::{ControlMessage, read_message, write_message};
use crate::lifecycle::{NodeEffect, NodeState};

/// Node-side failures (I/O / unrecoverable only).
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

/// Connect to the arbiter control socket and drive the node state machine.
///
/// Phase 2: exits shortly after reaching [`NodeState::Running`]. Later phases
/// keep the simulation thread alive inside Running.
pub fn run_node(board_id: &str, control: &Path) -> Result<(), NodeError> {
    let mut stream = connect_with_retry(control, Duration::from_secs(5))?;
    let mut state = NodeState::AwaitingStartup;

    while !matches!(state, NodeState::Running | NodeState::Stopped) {
        let msg = read_message(&mut stream)?;
        let (next, effect) = state.on_message(board_id, msg);
        if let Some(effect) = effect {
            apply_effect(board_id, &mut stream, effect)?;
        }
        if next != state {
            eprintln!("cluster-node[{board_id}]: {state:?} -> {next:?}");
        }
        state = next;
    }

    if state == NodeState::Running {
        // Hold briefly so the arbiter can observe a live child.
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn apply_effect(
    board_id: &str,
    stream: &mut UnixStream,
    effect: NodeEffect,
) -> Result<(), NodeError> {
    match effect {
        NodeEffect::SendReady => {
            write_message(
                stream,
                &ControlMessage::Ready {
                    board_id: board_id.to_string(),
                },
            )?;
            eprintln!("cluster-node[{board_id}]: Ready");
        }
        NodeEffect::Warn(msg) => {
            eprintln!("cluster-node[{board_id}]: warning: {msg}");
        }
    }
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

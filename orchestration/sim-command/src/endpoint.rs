//! Endpoint thread: tracks session info from the Command thread (#147 Phase 2;
//! the same-host data plane is Phase 3).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use contract::ClusterId;

/// Session info the Command thread hands to the Endpoint thread.
pub(crate) enum EndpointEvent {
    SessionOpened {
        cluster: ClusterId,
        iox_root: PathBuf,
    },
    SessionClosed {
        cluster: ClusterId,
    },
}

/// Runs until the Command thread drops its sender.
pub(crate) fn endpoint_loop(events: &Receiver<EndpointEvent>) {
    let mut sessions = HashMap::new();
    for event in events {
        match event {
            EndpointEvent::SessionOpened { cluster, iox_root } => {
                sessions.insert(cluster, iox_root);
            }
            EndpointEvent::SessionClosed { cluster } => {
                sessions.remove(&cluster);
            }
        }
    }
}

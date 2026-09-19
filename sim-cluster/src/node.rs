//! Host-stop notification helper for board processes.

use crate::board_runtime::BoardError;
use crate::control::{ControlToArbiter, HostStopReason};
use crate::ipc::{NodeControl, board_id_hash};

/// Notify the arbiter of a host-initiated stop when the reason is cluster-relevant.
///
/// Returns `Ok(true)` if [`ControlToArbiter::HostStop`] was sent.
pub fn notify_host_stop(
    control: &NodeControl,
    board_id: &str,
    reason: &str,
) -> Result<bool, BoardError> {
    let Some(reason) = HostStopReason::from_label(reason) else {
        return Ok(false);
    };
    control.publish(&ControlToArbiter::HostStop {
        from: board_id_hash(board_id),
        reason,
    })?;
    Ok(true)
}

//! Cluster-stop policy: which host stops propagate to peer boards.

/// Reasons that must be broadcast as [`crate::control::ControlMessage::ClusterStop`].
///
/// Guest-local `halt` (RL78 STOP / WFI) is intentionally absent.
pub fn is_cluster_relevant_reason(reason: &str) -> bool {
    matches!(
        reason,
        // Wire labels (snake_case). Display strings like "external" are also accepted.
        "breakpoint" | "external_stop" | "external" | "step" | "unmapped"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halt_is_not_cluster_relevant() {
        assert!(!is_cluster_relevant_reason("halt"));
        assert!(is_cluster_relevant_reason("breakpoint"));
        assert!(is_cluster_relevant_reason("external_stop"));
        assert!(is_cluster_relevant_reason("external"));
        assert!(is_cluster_relevant_reason("step"));
        assert!(is_cluster_relevant_reason("unmapped"));
    }
}

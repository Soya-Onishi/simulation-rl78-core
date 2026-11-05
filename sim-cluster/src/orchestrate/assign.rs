//! Board placement across healthy backends.

use crate::backend::HealthBody;

use super::{AssignmentTable, BackendKey, BoardAssignment, ClusterSettings, backend_key};

/// Capacity snapshot for one backend (from health).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendCapacity {
    pub backend: BackendKey,
    pub max_boards: u32,
    pub running_boards: u32,
    /// Cluster-wide settings this backend reported (compared at create time).
    pub settings: ClusterSettings,
}

impl BackendCapacity {
    #[must_use]
    pub fn free(&self) -> u32 {
        self.max_boards.saturating_sub(self.running_boards)
    }
}

/// Why assignment failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssignError {
    /// No healthy backends were candidates.
    NoCandidates,
    /// Sum of free slots is less than the number of boards.
    InsufficientCapacity { boards: usize, free_slots: u32 },
}

impl std::fmt::Display for AssignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCandidates => write!(f, "no healthy cluster-backend candidates"),
            Self::InsufficientCapacity { boards, free_slots } => write!(
                f,
                "not enough board capacity: need {boards} boards, free slots {free_slots}"
            ),
        }
    }
}

impl std::error::Error for AssignError {}

/// Assign `board_ids` (topology order) onto backends.
///
/// Candidates are sorted by ascending `running_boards` (stable by original order on ties).
/// Each backend is filled up to `max_boards` before the next candidate is used.
/// Capacity is checked up-front; on shortage nothing is assigned.
pub fn assign_boards(
    board_ids: &[String],
    candidates: &[BackendCapacity],
) -> Result<AssignmentTable, AssignError> {
    if candidates.is_empty() {
        return Err(AssignError::NoCandidates);
    }
    let free_slots: u32 = candidates.iter().map(BackendCapacity::free).sum();
    if (board_ids.len() as u32) > free_slots {
        return Err(AssignError::InsufficientCapacity {
            boards: board_ids.len(),
            free_slots,
        });
    }

    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&i| candidates[i].running_boards);

    let mut remaining: Vec<u32> = candidates.iter().map(BackendCapacity::free).collect();
    let mut entries = Vec::with_capacity(board_ids.len());
    let mut cursor = 0usize;

    for board_id in board_ids {
        while cursor < order.len() && remaining[order[cursor]] == 0 {
            cursor += 1;
        }
        debug_assert!(cursor < order.len(), "pre-checked capacity must suffice");
        let idx = order[cursor];
        remaining[idx] -= 1;
        entries.push(BoardAssignment {
            board_id: board_id.clone(),
            backend: candidates[idx].backend.clone(),
        });
    }

    Ok(AssignmentTable { entries })
}

/// Build a capacity entry from listen coordinates and a health reply.
#[must_use]
pub fn capacity_from_health(ip: &str, port: u16, health: &HealthBody) -> BackendCapacity {
    BackendCapacity {
        backend: backend_key(ip, port),
        max_boards: health.max_boards,
        running_boards: health.running_boards,
        settings: ClusterSettings {
            time_sync_period_ms: health.time_sync_period_ms,
            data_udp_window: health.data_udp_window,
            multicast_base: health.multicast_base.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(backend: &str, max: u32, running: u32) -> BackendCapacity {
        BackendCapacity {
            backend: backend.into(),
            max_boards: max,
            running_boards: running,
            settings: ClusterSettings {
                time_sync_period_ms: 100,
                data_udp_window: 32,
                multicast_base: "239.255.0.0".into(),
            },
        }
    }

    #[test]
    fn packs_onto_least_loaded_first() {
        let boards = ["a", "b", "c", "d"].map(str::to_string);
        let candidates = [cap("10.0.0.1:1", 4, 2), cap("10.0.0.2:1", 4, 0)];
        let table = assign_boards(&boards, &candidates).unwrap();
        // Prefer running=0 backend and fill up to max (4).
        assert_eq!(table.boards_for("10.0.0.2:1"), vec!["a", "b", "c", "d"]);
        assert!(table.boards_for("10.0.0.1:1").is_empty());
    }

    #[test]
    fn spills_to_next_backend_when_full() {
        let boards = ["a", "b", "c"].map(str::to_string);
        let candidates = [cap("s0:1", 2, 0), cap("s1:1", 2, 0)];
        let table = assign_boards(&boards, &candidates).unwrap();
        // Tie on running=0: stable by original order → s0 first.
        assert_eq!(table.boards_for("s0:1"), vec!["a", "b"]);
        assert_eq!(table.boards_for("s1:1"), vec!["c"]);
    }

    #[test]
    fn insufficient_capacity_errors_without_partial() {
        let boards = ["a", "b", "c"].map(str::to_string);
        let candidates = [cap("s0:1", 1, 0), cap("s1:1", 1, 0)];
        let err = assign_boards(&boards, &candidates).unwrap_err();
        assert_eq!(
            err,
            AssignError::InsufficientCapacity {
                boards: 3,
                free_slots: 2
            }
        );
    }

    #[test]
    fn no_candidates() {
        let boards = ["a".into()];
        assert_eq!(
            assign_boards(&boards, &[]).unwrap_err(),
            AssignError::NoCandidates
        );
    }
}

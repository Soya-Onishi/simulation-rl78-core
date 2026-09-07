//! Arbiter-side virtual-time ceiling: `allowed = min(reports) + margin`.

use std::collections::HashMap;

/// Tracks last reported virtual times and computes the shared ceiling.
#[derive(Clone, Debug)]
pub struct TimeCeiling {
    margin_ns: u64,
    reports: HashMap<String, u64>,
}

impl TimeCeiling {
    #[must_use]
    pub fn new(board_ids: impl IntoIterator<Item = String>, margin_ns: u64) -> Self {
        let mut reports = HashMap::new();
        for id in board_ids {
            reports.insert(id, 0);
        }
        Self { margin_ns, reports }
    }

    /// Update one board's last reported virtual time (kept even while stopped).
    pub fn report(&mut self, board_id: &str, virtual_time_ns: u64) {
        if let Some(slot) = self.reports.get_mut(board_id) {
            *slot = virtual_time_ns;
        }
    }

    /// `allowed = min(reports) + margin`.
    #[must_use]
    pub fn allowed_ns(&self) -> u64 {
        let min = self.reports.values().copied().min().unwrap_or(0);
        min.saturating_add(self.margin_ns)
    }

    #[must_use]
    pub fn margin_ns(&self) -> u64 {
        self.margin_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_is_min_plus_margin() {
        let mut ceil = TimeCeiling::new(["a".into(), "b".into()], 1000);
        ceil.report("a", 50);
        ceil.report("b", 200);
        assert_eq!(ceil.allowed_ns(), 1050);
        // Stopped/halted board keeps last report and pins the min.
        ceil.report("b", 5000);
        assert_eq!(ceil.allowed_ns(), 1050);
    }
}

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

    /// Update one board's last reported virtual time.
    ///
    /// A report older than the stored value is ignored so a stale sample cannot
    /// pull the ceiling backwards.
    pub fn report(&mut self, board_id: &str, virtual_time_ns: u64) -> bool {
        let Some(slot) = self.reports.get_mut(board_id) else {
            return false;
        };
        if virtual_time_ns <= *slot {
            return false;
        }
        *slot = virtual_time_ns;
        true
    }

    /// Put every board's report at `virtual_time_ns`.
    pub fn place_reports_at(&mut self, virtual_time_ns: u64) {
        for slot in self.reports.values_mut() {
            *slot = virtual_time_ns;
        }
    }

    /// `allowed = min(reports) + margin`.
    #[must_use]
    pub fn allowed_ns(&self) -> u64 {
        self.virtual_time_ns().saturating_add(self.margin_ns)
    }

    /// Minimum reported virtual time across boards (`0` before any report).
    #[must_use]
    pub fn virtual_time_ns(&self) -> u64 {
        self.reports.values().copied().min().unwrap_or(0)
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
        assert!(
            !ceil.report("b", 40),
            "older reports must not move time backwards"
        );
        assert_eq!(ceil.allowed_ns(), 1050);
    }
}

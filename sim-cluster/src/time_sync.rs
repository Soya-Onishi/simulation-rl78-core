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
    ///
    /// Virtual time is monotonic: an older sample (late or leftover after Reset)
    /// is ignored so it cannot rewind [`Self::allowed_ns`].
    pub fn report(&mut self, board_id: &str, virtual_time_ns: u64) {
        if let Some(slot) = self.reports.get_mut(board_id) {
            if virtual_time_ns > *slot {
                *slot = virtual_time_ns;
            }
        }
    }

    /// Raise every board's report to at least `now_ns`.
    ///
    /// Host Reset uses this so the next [`Self::allowed_ns`] is `now + margin`
    /// even when guests had already reached the previous ceiling.
    pub fn advance_to(&mut self, now_ns: u64) {
        for slot in self.reports.values_mut() {
            if *slot < now_ns {
                *slot = now_ns;
            }
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
    }

    #[test]
    fn advance_to_raises_allowed_to_now_plus_margin() {
        let mut ceil = TimeCeiling::new(["a".into(), "b".into()], 1000);
        ceil.report("a", 500);
        ceil.report("b", 800);
        // Boards sat at the previous ceiling (min report + margin).
        ceil.advance_to(1500);
        assert_eq!(ceil.virtual_time_ns(), 1500);
        assert_eq!(ceil.allowed_ns(), 2500);
    }

    #[test]
    fn advance_to_does_not_rewind_ahead_reports() {
        let mut ceil = TimeCeiling::new(["a".into(), "b".into()], 1000);
        ceil.report("a", 5000);
        ceil.report("b", 100);
        ceil.advance_to(1000);
        ceil.report("b", 4000);
        assert_eq!(ceil.virtual_time_ns(), 4000);
        assert_eq!(ceil.allowed_ns(), 5000);
    }

    #[test]
    fn leftover_report_does_not_rewind_after_advance() {
        let mut ceil = TimeCeiling::new(["a".into(), "b".into()], 1000);
        ceil.report("a", 400);
        ceil.report("b", 500);
        ceil.advance_to(1500);
        ceil.report("a", 400);
        ceil.report("b", 500);
        assert_eq!(ceil.virtual_time_ns(), 1500);
        assert_eq!(ceil.allowed_ns(), 2500);
    }
}

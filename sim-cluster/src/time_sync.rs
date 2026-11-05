//! Arbiter-side virtual-time ceiling: `allowed = min(reports) + margin`.
//!
//! With peer arbiters (multi-backend), the minimum also covers the remote
//! arbiters' minimum reported over multicast; see [`TimeCeiling::set_remote`].

use std::collections::HashMap;

/// Tracks last reported virtual times and computes the shared ceiling.
#[derive(Clone, Debug)]
pub struct TimeCeiling {
    margin_ns: u64,
    reports: HashMap<String, u64>,
    /// Minimum virtual time reported by the other arbiters; `None` without peers.
    remote_min_ns: Option<u64>,
    /// A peer arbiter went silent: Allowed updates are held at their last value.
    frozen: bool,
    /// Remote state changed since the last [`Self::take_remote_dirty`].
    remote_dirty: bool,
}

impl TimeCeiling {
    #[must_use]
    pub fn new(board_ids: impl IntoIterator<Item = String>, margin_ns: u64) -> Self {
        let mut reports = HashMap::new();
        for id in board_ids {
            reports.insert(id, 0);
        }
        Self {
            margin_ns,
            reports,
            remote_min_ns: None,
            frozen: false,
            remote_dirty: false,
        }
    }

    /// Update one board's last reported virtual time (kept even while stopped).
    pub fn report(&mut self, board_id: &str, virtual_time_ns: u64) {
        if let Some(slot) = self.reports.get_mut(board_id) {
            *slot = virtual_time_ns;
        }
    }

    /// Last TimeReport for `board_id`.
    #[must_use]
    pub fn last_report(&self, board_id: &str) -> Option<u64> {
        self.reports.get(board_id).copied()
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

    /// Set the other arbiters' minimum virtual time and whether Allowed
    /// updates are frozen. Marks the ceiling dirty when either changed.
    pub fn set_remote(&mut self, remote_min_ns: Option<u64>, frozen: bool) {
        if self.remote_min_ns != remote_min_ns || self.frozen != frozen {
            self.remote_dirty = true;
        }
        self.remote_min_ns = remote_min_ns;
        self.frozen = frozen;
    }

    /// Minimum virtual time last reported by the other arbiters; `None` when
    /// there are no peers (single backend).
    #[must_use]
    pub fn remote_min_ns(&self) -> Option<u64> {
        self.remote_min_ns
    }

    /// `true` while Allowed updates must be held at their last value.
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Whether [`Self::set_remote`] changed anything since the last call.
    pub fn take_remote_dirty(&mut self) -> bool {
        std::mem::take(&mut self.remote_dirty)
    }

    /// Minimum over local boards and, when set, the remote arbiters.
    #[must_use]
    pub fn cluster_min_ns(&self) -> u64 {
        match self.remote_min_ns {
            Some(remote) => self.virtual_time_ns().min(remote),
            None => self.virtual_time_ns(),
        }
    }

    /// `allowed = min(local reports, remote arbiters' minimum) + margin`.
    #[must_use]
    pub fn allowed_ns(&self) -> u64 {
        self.cluster_min_ns().saturating_add(self.margin_ns)
    }

    /// Natural ceiling, optionally hard-capped at `until_ns`.
    #[must_use]
    pub fn allowed_ns_until(&self, until_ns: Option<u64>) -> u64 {
        match until_ns {
            Some(until) => self.allowed_ns().min(until),
            None => self.allowed_ns(),
        }
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
        assert_eq!(ceil.last_report("a"), Some(0));
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
    fn allowed_ns_until_caps_natural_ceiling() {
        let mut ceil = TimeCeiling::new(["a".into()], 1000);
        ceil.report("a", 100);
        assert_eq!(ceil.allowed_ns_until(None), 1100);
        assert_eq!(ceil.allowed_ns_until(Some(500)), 500);
        assert_eq!(ceil.allowed_ns_until(Some(5000)), 1100);
    }
}

//! Probe thread subscription set (#147 Phase 2: hold only, no publish).

use std::collections::BTreeSet;

use contract::{HashedId, MAX_NODE_PROBE_IDS, NodeCommandError, NodeCommandResult};

#[derive(Default)]
pub(crate) struct ProbeSubscriptions {
    ids: BTreeSet<HashedId<u64>>,
}

impl ProbeSubscriptions {
    /// Add `ids` (duplicates ignored). Rejects the whole add with `BadParam`
    /// when the union would exceed [`MAX_NODE_PROBE_IDS`].
    pub(crate) fn subscribe(&mut self, ids: &[HashedId<u64>]) -> NodeCommandResult {
        let added = ids.iter().filter(|id| !self.ids.contains(id));
        let added: BTreeSet<_> = added.copied().collect();
        if self.ids.len() + added.len() > MAX_NODE_PROBE_IDS {
            return NodeCommandResult::Error(NodeCommandError::BadParam);
        }
        self.ids.extend(added);
        NodeCommandResult::Success
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(range: std::ops::Range<u64>) -> Vec<HashedId<u64>> {
        range.map(HashedId::new).collect()
    }

    #[test]
    fn merge_ignores_duplicates() {
        let mut subs = ProbeSubscriptions::default();
        assert_eq!(subs.subscribe(&ids(0..4)), NodeCommandResult::Success);
        assert_eq!(subs.subscribe(&ids(2..6)), NodeCommandResult::Success);
        assert_eq!(
            subs.subscribe(&[HashedId::new(7), HashedId::new(7)]),
            NodeCommandResult::Success
        );
        assert_eq!(subs.len(), 7);
    }

    #[test]
    fn overflowing_add_is_rejected_and_set_kept() {
        let mut subs = ProbeSubscriptions::default();
        assert_eq!(subs.subscribe(&ids(0..20)), NodeCommandResult::Success);
        assert_eq!(
            subs.subscribe(&ids(20..33)),
            NodeCommandResult::Error(NodeCommandError::BadParam)
        );
        assert_eq!(subs.len(), 20);
        assert_eq!(subs.subscribe(&ids(10..32)), NodeCommandResult::Success);
        assert_eq!(subs.len(), MAX_NODE_PROBE_IDS);
        assert_eq!(subs.subscribe(&ids(0..32)), NodeCommandResult::Success);
    }
}

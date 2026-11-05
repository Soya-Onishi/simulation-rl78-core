//! Pure merge helpers for the frontend fan-in over several `cluster-backend`s.
//!
//! [`super::MultiBackend`] asks every backend that hosts a part of one
//! simulation and folds the answers into the single answer a client expects.
//! Nothing here does I/O, so each rule can be tested on plain values.
//!
//! - [`merge_status`]: one [`StatusEvent`] from the members' statuses.
//! - [`merge_probe_catalogs`], [`merge_actions`]: catalogs concatenated in a stable order.
//! - [`CsvMerge`]: probe records of every member, released in virtual-time order.
//! - [`MergedLog`]: log lines of every member, ordered by virtual time, under one cursor.

use std::collections::VecDeque;

use crate::contract::{ActionList, LogEvent, StatusEvent};
use crate::host::SessionNode;
use crate::probe_log::{CsvPoll, ProbeCatalogItem, ProbePointName, ProbeRecord};

/// Overall state while some member is not ready yet (see [`merge_status`]).
pub(super) const STATE_STARTING: &str = "starting";

/// `true` when a node has finished the ready barrier of its backend.
///
/// A backend reports a node as `starting` until its arbiter has written the
/// first status, and the arbiter reports `awaiting_ready` until that node sent
/// `Ready`. Both, and `unknown`, mean "not ready".
fn node_is_ready(node: &SessionNode) -> bool {
    !matches!(
        node.state.as_str(),
        "starting" | "awaiting_ready" | "unknown"
    )
}

/// Every node of every member has passed its ready barrier.
pub(super) fn all_nodes_ready(members: &[StatusEvent]) -> bool {
    members
        .iter()
        .all(|status| status.nodes.iter().all(node_is_ready))
}

/// Any member's arbiter has exited.
pub(super) fn any_exited(members: &[StatusEvent]) -> bool {
    members.iter().any(|status| status.state == "exited")
}

/// One status for the whole simulation, shown to the client under `client_id`.
///
/// `catalogs_complete` tells whether every member has advertised its full probe
/// catalog. The overall `state` is the first rule that applies:
///
/// 1. any member `exited` -> `exited`;
/// 2. some node is not ready, or `catalogs_complete` is false -> `starting`
///    (the client must not treat the simulation as ready yet);
/// 3. every member `running` -> `running`;
/// 4. otherwise -> `stopped` (one stopped member stops the whole simulation).
///
/// `virtual_time_ns` is the smallest member time: the time every member has
/// reached. `stop_reason` is the first one a stopped member reports. `nodes`
/// are the members' nodes in member order.
pub(super) fn merge_status(
    client_id: &str,
    members: Vec<StatusEvent>,
    catalogs_complete: bool,
) -> StatusEvent {
    let state = if any_exited(&members) {
        "exited"
    } else if !all_nodes_ready(&members) || !catalogs_complete {
        STATE_STARTING
    } else if !members.is_empty() && members.iter().all(|status| status.state == "running") {
        "running"
    } else {
        "stopped"
    };
    let stop_reason = if state == "stopped" {
        members
            .iter()
            .filter(|status| status.state != "running")
            .find_map(|status| status.stop_reason.clone())
    } else {
        None
    };
    StatusEvent {
        id: client_id.to_string(),
        state: state.to_string(),
        virtual_time_ns: members
            .iter()
            .map(|status| status.virtual_time_ns)
            .min()
            .unwrap_or(0),
        stop_reason,
        nodes: members
            .into_iter()
            .flat_map(|status| status.nodes)
            .collect(),
    }
}

/// Probe catalogs of all members as one list sorted by `(board, probe)`.
/// The sort is stable and identical rows are kept once.
pub(super) fn merge_probe_catalogs(members: Vec<Vec<ProbeCatalogItem>>) -> Vec<ProbeCatalogItem> {
    let mut items: Vec<ProbeCatalogItem> = members.into_iter().flatten().collect();
    items.sort_by(|a, b| (&a.board, &a.probe).cmp(&(&b.board, &b.probe)));
    items.dedup();
    items
}

/// Action catalogs of all members as one list sorted by `(board, device, action)`.
pub(super) fn merge_actions(members: Vec<ActionList>) -> ActionList {
    let mut actions: Vec<_> = members.into_iter().flat_map(|list| list.actions).collect();
    actions.sort_by(|a, b| {
        (&a.board_id, &a.device, &a.action).cmp(&(&b.board_id, &b.device, &b.action))
    });
    ActionList { actions }
}

/// Probe points of all members for a CSV export, sorted by `(board, name)`;
/// a probe id seen twice is kept once.
pub(super) fn merge_csv_catalogs(members: Vec<Vec<ProbePointName>>) -> Vec<ProbePointName> {
    let mut points: Vec<ProbePointName> = members.into_iter().flatten().collect();
    points.sort_by(|a, b| {
        (&a.board_id, &a.name, a.probe_id).cmp(&(&b.board_id, &b.name, b.probe_id))
    });
    points.dedup_by_key(|point| point.probe_id);
    points
}

/// Collects the CSV polls of every member of one export.
///
/// Records are held back until every member has reported `done`, then handed
/// out once, sorted by `(virtual_time_ns, probe_id)`. A streamed export cannot
/// reorder rows it has already sent, and a member that is still working may yet
/// produce rows earlier than the ones another member already delivered.
#[derive(Debug)]
pub(super) struct CsvMerge {
    drained: Vec<bool>,
    buffer: Vec<ProbeRecord>,
    failed: bool,
    truncated: bool,
}

impl CsvMerge {
    pub(super) fn new(members: usize) -> Self {
        Self {
            drained: vec![false; members],
            buffer: Vec::new(),
            failed: false,
            truncated: false,
        }
    }

    /// `true` once `member` reported `done` (nothing more to poll there).
    pub(super) fn is_drained(&self, member: usize) -> bool {
        self.drained[member]
    }

    pub(super) fn all_drained(&self) -> bool {
        self.drained.iter().all(|done| *done)
    }

    /// Take one poll answer of `member`.
    pub(super) fn absorb(&mut self, member: usize, poll: CsvPoll) {
        self.buffer.extend(poll.records);
        self.failed |= poll.failed;
        self.truncated |= poll.truncated;
        if poll.done {
            self.drained[member] = true;
        }
    }

    /// The merged answer: `done` when every member is done (then the sorted
    /// records are returned once), `failed` / `truncated` when any member says so.
    pub(super) fn poll(&mut self) -> CsvPoll {
        let done = self.all_drained();
        let records = if done {
            let mut records = std::mem::take(&mut self.buffer);
            records.sort_by_key(|rec| (rec.virtual_time_ns, rec.probe_id));
            records
        } else {
            Vec::new()
        };
        CsvPoll {
            records,
            done,
            failed: self.failed,
            truncated: self.truncated,
        }
    }
}

/// Log lines of all members of one simulation under one client-facing cursor.
///
/// Each backend numbers its own lines. The client follows a single `seq`, so
/// this renumbers the merged lines: every [`MergedLog::absorb`] sorts the lines
/// newly fetched from the members by `(virtual_time_ns, member index, member seq)`
/// and appends them with the next merged `seq`.
///
/// Order is exact inside one fetch and inside the stored history. Lines are
/// only seen when a member is asked, so a line that a slow member delivers
/// after a later fetch with a larger virtual time is appended after it.
#[derive(Debug)]
pub(super) struct MergedLog {
    /// Highest member `seq` taken so far, per member.
    cursors: Vec<u64>,
    next_seq: u64,
    events: VecDeque<LogEvent>,
    cap: usize,
}

impl MergedLog {
    pub(super) fn new(members: usize, cap: usize) -> Self {
        Self {
            cursors: vec![0; members],
            next_seq: 1,
            events: VecDeque::new(),
            cap,
        }
    }

    /// `after` to ask member `index` for lines not taken yet.
    pub(super) fn cursor(&self, index: usize) -> u64 {
        self.cursors[index]
    }

    /// Take the lines fetched from each member (`batches[i]` is member `i`'s).
    pub(super) fn absorb(&mut self, batches: Vec<Vec<LogEvent>>) {
        let mut fresh: Vec<(u64, usize, u64, LogEvent)> = Vec::new();
        for (member, batch) in batches.into_iter().enumerate() {
            for event in batch {
                self.cursors[member] = self.cursors[member].max(event.seq);
                fresh.push((event.virtual_time_ns, member, event.seq, event));
            }
        }
        fresh.sort_by_key(|(vt, member, seq, _)| (*vt, *member, *seq));
        for (_, _, _, mut event) in fresh {
            event.seq = self.next_seq;
            self.next_seq = self.next_seq.saturating_add(1);
            self.events.push_back(event);
        }
        while self.events.len() > self.cap {
            self.events.pop_front();
        }
    }

    /// Merged lines with `seq > after`.
    pub(super) fn after(&self, after: u64) -> Vec<LogEvent> {
        self.events
            .iter()
            .filter(|event| event.seq > after)
            .cloned()
            .collect()
    }

    /// Highest merged `seq` (`0` if none).
    pub(super) fn high_water(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::AdvertisedAction;
    use crate::probe_log::ProbeKind;

    fn node(id: &str, state: &str) -> SessionNode {
        SessionNode {
            id: id.into(),
            kind: "rl78".into(),
            state: state.into(),
        }
    }

    fn status(state: &str, vt: u64, nodes: Vec<SessionNode>) -> StatusEvent {
        StatusEvent {
            id: "member".into(),
            state: state.into(),
            virtual_time_ns: vt,
            stop_reason: None,
            nodes,
        }
    }

    fn ready_status(state: &str, vt: u64, board: &str) -> StatusEvent {
        status(state, vt, vec![node(board, state)])
    }

    #[test]
    fn status_all_running_is_running_with_client_id_min_time_and_all_nodes() {
        let merged = merge_status(
            "client",
            vec![
                ready_status("running", 500, "a"),
                ready_status("running", 400, "b"),
            ],
            true,
        );
        assert_eq!(merged.id, "client");
        assert_eq!(merged.state, "running");
        assert_eq!(merged.virtual_time_ns, 400);
        let ids: Vec<_> = merged.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(merged.stop_reason, None);
    }

    #[test]
    fn status_one_stopped_member_stops_the_simulation_and_keeps_its_reason() {
        let mut stopped = ready_status("stopped", 300, "b");
        stopped.stop_reason = Some("breakpoint".into());
        let merged = merge_status(
            "client",
            vec![ready_status("running", 500, "a"), stopped],
            true,
        );
        assert_eq!(merged.state, "stopped");
        assert_eq!(merged.stop_reason.as_deref(), Some("breakpoint"));
        assert_eq!(merged.virtual_time_ns, 300);
    }

    #[test]
    fn status_exited_member_wins_over_everything() {
        let merged = merge_status(
            "client",
            vec![
                ready_status("exited", 10, "a"),
                status("stopped", 0, vec![node("b", "starting")]),
            ],
            false,
        );
        assert_eq!(merged.state, "exited");
    }

    #[test]
    fn status_not_ready_node_or_incomplete_catalog_is_starting() {
        // The backend shows "stopped" while its nodes are still "starting".
        let not_ready = merge_status(
            "client",
            vec![
                ready_status("stopped", 0, "a"),
                status("stopped", 0, vec![node("b", "starting")]),
            ],
            true,
        );
        assert_eq!(not_ready.state, STATE_STARTING);

        let awaiting = merge_status(
            "client",
            vec![status("stopped", 0, vec![node("a", "awaiting_ready")])],
            true,
        );
        assert_eq!(awaiting.state, STATE_STARTING);

        let no_catalog = merge_status(
            "client",
            vec![
                ready_status("stopped", 0, "a"),
                ready_status("stopped", 0, "b"),
            ],
            false,
        );
        assert_eq!(no_catalog.state, STATE_STARTING);
        assert_eq!(no_catalog.stop_reason, None);

        let ready = merge_status(
            "client",
            vec![
                ready_status("stopped", 0, "a"),
                ready_status("stopped", 0, "b"),
            ],
            true,
        );
        assert_eq!(ready.state, "stopped");
    }

    #[test]
    fn status_members_with_disjoint_local_nodes_merge_to_unique_ready_nodes() {
        let members = vec![
            status("stopped", 0, vec![node("a", "ready")]),
            status("stopped", 0, vec![node("b", "ready")]),
        ];
        assert!(all_nodes_ready(&members));
        let merged = merge_status("client", members, true);
        assert_eq!(merged.state, "stopped");
        let ids: Vec<_> = merged.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn status_member_listing_remote_node_as_unknown_is_not_ready() {
        // Regression guard: a member must not report boards of other members.
        let members = vec![
            status("stopped", 0, vec![node("a", "ready"), node("b", "unknown")]),
            status("stopped", 0, vec![node("b", "ready")]),
        ];
        assert!(!all_nodes_ready(&members));
        assert_eq!(merge_status("client", members, true).state, STATE_STARTING);
    }

    fn probe(board: &str, name: &str) -> ProbeCatalogItem {
        ProbeCatalogItem {
            board: board.into(),
            probe: name.into(),
            kind: "digital".into(),
        }
    }

    #[test]
    fn probe_catalogs_merge_into_one_sorted_list_without_duplicates() {
        let merged = merge_probe_catalogs(vec![
            vec![probe("b", "p1"), probe("b", "p0")],
            vec![probe("a", "p0"), probe("b", "p1")],
        ]);
        let names: Vec<_> = merged
            .iter()
            .map(|p| format!("{}/{}", p.board, p.probe))
            .collect();
        assert_eq!(names, ["a/p0", "b/p0", "b/p1"]);
    }

    fn action(board: &str, device: &str, name: &str) -> AdvertisedAction {
        AdvertisedAction {
            board_id: board.into(),
            device: device.into(),
            action: name.into(),
            description: String::new(),
            params: vec![],
        }
    }

    #[test]
    fn actions_merge_into_one_sorted_list() {
        let merged = merge_actions(vec![
            ActionList {
                actions: vec![action("b", "sw", "press")],
            },
            ActionList {
                actions: vec![action("a", "sw", "release"), action("a", "sw", "press")],
            },
        ]);
        let names: Vec<_> = merged
            .actions
            .iter()
            .map(|a| format!("{}/{}/{}", a.board_id, a.device, a.action))
            .collect();
        assert_eq!(names, ["a/sw/press", "a/sw/release", "b/sw/press"]);
    }

    #[test]
    fn csv_catalogs_keep_each_probe_id_once() {
        let point = |id: u64, board: &str, name: &str| ProbePointName {
            probe_id: id,
            board_id: board.into(),
            name: name.into(),
            kind: ProbeKind::Digital,
        };
        let merged = merge_csv_catalogs(vec![
            vec![point(2, "b", "p0"), point(1, "a", "p0")],
            vec![point(2, "b", "p0")],
        ]);
        let ids: Vec<_> = merged.iter().map(|p| p.probe_id).collect();
        assert_eq!(ids, [1, 2]);
    }

    fn rec(probe_id: u64, vt: u64) -> ProbeRecord {
        ProbeRecord {
            probe_id,
            virtual_time_ns: vt,
            kind: ProbeKind::Digital,
            value: 0,
        }
    }

    fn csv(records: Vec<ProbeRecord>, done: bool, failed: bool) -> CsvPoll {
        CsvPoll {
            records,
            done,
            failed,
            truncated: false,
        }
    }

    #[test]
    fn csv_holds_records_until_every_member_is_done_then_sorts_them() {
        let mut merge = CsvMerge::new(2);
        merge.absorb(0, csv(vec![rec(1, 30), rec(1, 10)], true, false));
        let partial = merge.poll();
        assert!(!partial.done);
        assert!(partial.records.is_empty());
        assert!(merge.is_drained(0));
        assert!(!merge.is_drained(1));

        merge.absorb(1, csv(vec![rec(2, 20), rec(2, 10)], true, false));
        let all = merge.poll();
        assert!(all.done);
        assert!(!all.failed);
        let order: Vec<_> = all
            .records
            .iter()
            .map(|r| (r.virtual_time_ns, r.probe_id))
            .collect();
        assert_eq!(order, [(10, 1), (10, 2), (20, 2), (30, 1)]);

        // Records are handed out once.
        let again = merge.poll();
        assert!(again.done);
        assert!(again.records.is_empty());
    }

    #[test]
    fn csv_failed_when_any_member_failed() {
        let mut merge = CsvMerge::new(2);
        merge.absorb(0, csv(vec![], true, false));
        merge.absorb(1, csv(vec![], true, true));
        let poll = merge.poll();
        assert!(poll.done);
        assert!(poll.failed);
    }

    fn log(seq: u64, vt: u64, text: &str) -> LogEvent {
        LogEvent {
            seq,
            level: "info".into(),
            origin: "node".into(),
            board: None,
            text: text.into(),
            virtual_time_ns: vt,
        }
    }

    fn texts(events: &[LogEvent]) -> Vec<&str> {
        events.iter().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn logs_are_ordered_by_virtual_time_and_renumbered() {
        let mut merged = MergedLog::new(2, 100);
        merged.absorb(vec![
            vec![log(1, 100, "a@100"), log(2, 300, "a@300")],
            vec![log(1, 50, "b@50"), log(2, 200, "b@200")],
        ]);
        let all = merged.after(0);
        assert_eq!(texts(&all), ["b@50", "a@100", "b@200", "a@300"]);
        let seqs: Vec<_> = all.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
        assert_eq!(merged.high_water(), 4);
        assert_eq!(merged.cursor(0), 2);
        assert_eq!(merged.cursor(1), 2);
    }

    #[test]
    fn log_ties_break_by_member_then_member_seq() {
        let mut merged = MergedLog::new(2, 100);
        merged.absorb(vec![
            vec![log(1, 10, "a1"), log(2, 10, "a2")],
            vec![log(1, 10, "b1")],
        ]);
        assert_eq!(texts(&merged.after(0)), ["a1", "a2", "b1"]);
    }

    #[test]
    fn logs_after_returns_only_newer_lines_and_later_fetches_continue_the_numbering() {
        let mut merged = MergedLog::new(1, 100);
        merged.absorb(vec![vec![log(7, 1, "first")]]);
        assert_eq!(merged.high_water(), 1);
        assert!(merged.after(1).is_empty());

        merged.absorb(vec![vec![log(8, 2, "second")]]);
        let newer = merged.after(1);
        assert_eq!(texts(&newer), ["second"]);
        assert_eq!(newer[0].seq, 2);
        assert_eq!(merged.cursor(0), 8);
    }

    #[test]
    fn log_history_is_capped_but_numbering_keeps_growing() {
        let mut merged = MergedLog::new(1, 2);
        merged.absorb(vec![vec![log(1, 1, "a"), log(2, 2, "b"), log(3, 3, "c")]]);
        let kept = merged.after(0);
        assert_eq!(texts(&kept), ["b", "c"]);
        assert_eq!(kept[0].seq, 2);
        assert_eq!(merged.high_water(), 3);
    }
}

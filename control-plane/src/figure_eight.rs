use serde::Serialize;

use crate::{
    model::{Command, LogEntry, PersistentState},
    raft::{candidate_log_is_at_least_as_up_to_date, highest_committable_index},
};

const CLUSTER_SIZE: usize = 5;
const MAJORITY: usize = 3;
const SERVER_IDS: [&str; CLUSTER_SIZE] = ["S1", "S2", "S3", "S4", "S5"];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FigureEightServerLog {
    pub server_id: String,
    pub entry_terms: Vec<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FigureEightStage {
    pub label: String,
    pub leader_id: String,
    pub leader_term: u64,
    pub logs: Vec<FigureEightServerLog>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OldTermMajorityObservation {
    pub index: u64,
    pub entry_term: u64,
    pub leader_term: u64,
    pub replica_count: usize,
    pub majority_only_candidate: Option<u64>,
    pub current_term_rule_candidate: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnsafeFigureEightBranch {
    pub label: String,
    pub candidate_id: String,
    pub candidate_term: u64,
    pub eligible_voters: Vec<String>,
    pub vote_count: usize,
    pub majority_reached: bool,
    pub overwritten_index: u64,
    pub overwritten_entry_term: u64,
    pub old_entry_replicas_after_overwrite: usize,
    pub old_entry_survives_on_majority: bool,
    pub logs_after_overwrite: Vec<FigureEightServerLog>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SafeFigureEightBranch {
    pub label: String,
    pub leader_id: String,
    pub leader_term: u64,
    pub current_term_entry_index: u64,
    pub current_term_entry_term: u64,
    pub current_term_entry_replicas: usize,
    pub current_term_rule_candidate_before_majority: Option<u64>,
    pub current_term_rule_candidate: Option<u64>,
    pub prior_entry_index: u64,
    pub prior_entry_committed_indirectly: bool,
    pub challenger_id: String,
    pub challenger_term: u64,
    pub challenger_eligible_voters: Vec<String>,
    pub challenger_vote_count: usize,
    pub challenger_reaches_majority: bool,
    pub logs_after_current_term_replication: Vec<FigureEightServerLog>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FigureEightAssertions {
    pub term_three_election_reaches_majority: bool,
    pub term_four_election_reaches_majority: bool,
    pub old_term_entry_reaches_majority: bool,
    pub majority_only_rule_would_commit_old_term: bool,
    pub current_term_rule_rejects_old_term: bool,
    pub conflicting_future_leader_can_win_unsafe_branch: bool,
    pub allegedly_committed_entry_can_be_overwritten: bool,
    pub current_term_entry_waits_for_majority: bool,
    pub current_term_entry_commits_safe_branch: bool,
    pub prior_entry_commits_indirectly: bool,
    pub conflicting_future_leader_blocked_safe_branch: bool,
}

impl FigureEightAssertions {
    fn all_hold(&self) -> bool {
        self.term_three_election_reaches_majority
            && self.term_four_election_reaches_majority
            && self.old_term_entry_reaches_majority
            && self.majority_only_rule_would_commit_old_term
            && self.current_term_rule_rejects_old_term
            && self.conflicting_future_leader_can_win_unsafe_branch
            && self.allegedly_committed_entry_can_be_overwritten
            && self.current_term_entry_waits_for_majority
            && self.current_term_entry_commits_safe_branch
            && self.prior_entry_commits_indirectly
            && self.conflicting_future_leader_blocked_safe_branch
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FigureEightSafetyReport {
    pub schema: String,
    pub schema_version: u32,
    pub scenario: String,
    pub cluster_size: usize,
    pub majority: usize,
    pub stages: Vec<FigureEightStage>,
    pub old_term_majority: OldTermMajorityObservation,
    pub unsafe_branch: UnsafeFigureEightBranch,
    pub safe_branch: SafeFigureEightBranch,
    pub assertions: FigureEightAssertions,
    pub passed: bool,
}

/// Replays the two branches of Figure 8 from the extended Raft paper.
///
/// The report deliberately calls the same commit-index and vote-freshness
/// predicates as a live [`crate::RaftNode`]. It is deterministic evidence of
/// the algorithmic safety rule, not a network or timing simulation.
#[must_use]
pub fn figure_eight_safety_report() -> FigureEightSafetyReport {
    let mut logs = vec![vec![entry(1, 1)]; CLUSTER_SIZE];

    // Figure 8(a): term-2 leader S1 partially replicates index 2 to S2.
    logs[0].push(entry(2, 2));
    logs[1] = logs[0].clone();
    let stage_a = stage("a", "S1", 2, &logs);

    // Figure 8(b): S5 can win term 3 from S3, S4, and itself, then appends a
    // conflicting index 2 locally before crashing.
    let s5_pre_append = last_position(&logs[4]);
    let term_three_voters = [2, 3, 4]
        .into_iter()
        .filter(|voter| candidate_is_current(s5_pre_append, &logs[*voter]))
        .count();
    logs[4].push(entry(2, 3));
    let stage_b = stage("b", "S5", 3, &logs);

    // Figure 8(c): S1 wins term 4 and extends its old term-2 entry to S3.
    // Index 2 now appears on three servers, but still cannot be committed by
    // counting replicas because it is not from S1's current term.
    let s1_position = last_position(&logs[0]);
    let term_four_voters = [0, 1, 2]
        .into_iter()
        .filter(|voter| candidate_is_current(s1_position, &logs[*voter]))
        .count();
    logs[2] = logs[0].clone();
    let stage_c = stage("c", "S1", 4, &logs);
    let leader_at_c = persistent_state(4, logs[0].clone());
    let match_indexes_at_c = [2, 2, 2, 1, 1];
    let majority_only_candidate =
        highest_replica_counted_index(&leader_at_c, &match_indexes_at_c, MAJORITY);
    let current_term_rule_candidate = highest_committable_index(
        &leader_at_c,
        leader_at_c.current_term,
        &match_indexes_at_c,
        MAJORITY,
    );
    let old_term_replica_count = count_entry(&logs, 2, 2);

    // Figure 8(d): if S1 crashes now, S5's term-3 suffix is newer than the
    // logs of S2, S3, and S4. S5 can win and overwrite the term-2 entry that
    // the unsafe rule would already have declared committed.
    let mut unsafe_logs = logs.clone();
    let s5_position = last_position(&unsafe_logs[4]);
    let eligible_unsafe_voters = [1, 2, 3, 4]
        .into_iter()
        .filter(|voter| candidate_is_current(s5_position, &unsafe_logs[*voter]))
        .map(|voter| SERVER_IDS[voter].to_owned())
        .collect::<Vec<_>>();
    for follower in [1, 2, 3] {
        unsafe_logs[follower] = unsafe_logs[4].clone();
    }
    let old_entry_replicas_after_overwrite = count_entry(&unsafe_logs, 2, 2);
    let unsafe_branch = UnsafeFigureEightBranch {
        label: "d".to_owned(),
        candidate_id: "S5".to_owned(),
        candidate_term: 5,
        vote_count: eligible_unsafe_voters.len(),
        majority_reached: eligible_unsafe_voters.len() >= MAJORITY,
        eligible_voters: eligible_unsafe_voters,
        overwritten_index: 2,
        overwritten_entry_term: 2,
        old_entry_replicas_after_overwrite,
        old_entry_survives_on_majority: old_entry_replicas_after_overwrite >= MAJORITY,
        logs_after_overwrite: observe_logs(&unsafe_logs),
    };

    // Figure 8(e): alternatively, S1 appends a term-4 entry at index 3 and
    // replicates it to S2 and S3. Committing index 3 commits index 2 through
    // the Log Matching Property, and S5's conflicting shorter log can no
    // longer obtain a majority in a future term.
    let mut safe_logs = logs.clone();
    safe_logs[0].push(entry(3, 4));
    let leader_at_e = persistent_state(4, safe_logs[0].clone());
    let match_indexes_before_majority = [3, 2, 2, 1, 1];
    let candidate_before_majority = highest_committable_index(
        &leader_at_e,
        leader_at_e.current_term,
        &match_indexes_before_majority,
        MAJORITY,
    );
    safe_logs[1] = safe_logs[0].clone();
    safe_logs[2] = safe_logs[0].clone();
    let match_indexes_at_e = [3, 3, 3, 1, 1];
    let safe_commit_candidate = highest_committable_index(
        &leader_at_e,
        leader_at_e.current_term,
        &match_indexes_at_e,
        MAJORITY,
    );
    let eligible_safe_voters = [1, 2, 3, 4]
        .into_iter()
        .filter(|voter| candidate_is_current(s5_position, &safe_logs[*voter]))
        .map(|voter| SERVER_IDS[voter].to_owned())
        .collect::<Vec<_>>();
    let safe_branch = SafeFigureEightBranch {
        label: "e".to_owned(),
        leader_id: "S1".to_owned(),
        leader_term: 4,
        current_term_entry_index: 3,
        current_term_entry_term: 4,
        current_term_entry_replicas: count_entry(&safe_logs, 3, 4),
        current_term_rule_candidate_before_majority: candidate_before_majority,
        current_term_rule_candidate: safe_commit_candidate,
        prior_entry_index: 2,
        prior_entry_committed_indirectly: safe_commit_candidate.is_some_and(|index| index >= 2),
        challenger_id: "S5".to_owned(),
        challenger_term: 5,
        challenger_vote_count: eligible_safe_voters.len(),
        challenger_reaches_majority: eligible_safe_voters.len() >= MAJORITY,
        challenger_eligible_voters: eligible_safe_voters,
        logs_after_current_term_replication: observe_logs(&safe_logs),
    };

    let old_term_majority = OldTermMajorityObservation {
        index: 2,
        entry_term: 2,
        leader_term: 4,
        replica_count: old_term_replica_count,
        majority_only_candidate,
        current_term_rule_candidate,
    };
    let assertions = FigureEightAssertions {
        term_three_election_reaches_majority: term_three_voters >= MAJORITY,
        term_four_election_reaches_majority: term_four_voters >= MAJORITY,
        old_term_entry_reaches_majority: old_term_replica_count >= MAJORITY,
        majority_only_rule_would_commit_old_term: majority_only_candidate == Some(2),
        current_term_rule_rejects_old_term: current_term_rule_candidate.is_none(),
        conflicting_future_leader_can_win_unsafe_branch: unsafe_branch.majority_reached,
        allegedly_committed_entry_can_be_overwritten: !unsafe_branch.old_entry_survives_on_majority,
        current_term_entry_waits_for_majority: candidate_before_majority.is_none(),
        current_term_entry_commits_safe_branch: safe_commit_candidate == Some(3),
        prior_entry_commits_indirectly: safe_branch.prior_entry_committed_indirectly,
        conflicting_future_leader_blocked_safe_branch: !safe_branch.challenger_reaches_majority,
    };
    let passed = assertions.all_hold();

    FigureEightSafetyReport {
        schema: "inferlab.raft-figure-eight.v0.25".to_owned(),
        schema_version: 1,
        scenario: "raft-paper-figure-8".to_owned(),
        cluster_size: CLUSTER_SIZE,
        majority: MAJORITY,
        stages: vec![stage_a, stage_b, stage_c],
        old_term_majority,
        unsafe_branch,
        safe_branch,
        assertions,
        passed,
    }
}

fn entry(index: u64, term: u64) -> LogEntry {
    LogEntry {
        index,
        term,
        command: Command::Noop,
    }
}

fn persistent_state(current_term: u64, log: Vec<LogEntry>) -> PersistentState {
    PersistentState {
        cluster_id: "figure-eight-proof".to_owned(),
        current_term,
        voted_for: None,
        log,
        commit_index: 1,
    }
}

fn last_position(log: &[LogEntry]) -> (u64, u64) {
    log.last()
        .map(|entry| (entry.index, entry.term))
        .unwrap_or((0, 0))
}

fn candidate_is_current(candidate: (u64, u64), voter_log: &[LogEntry]) -> bool {
    let voter = last_position(voter_log);
    candidate_log_is_at_least_as_up_to_date(candidate.0, candidate.1, voter.0, voter.1)
}

fn stage(
    label: &str,
    leader_id: &str,
    leader_term: u64,
    logs: &[Vec<LogEntry>],
) -> FigureEightStage {
    FigureEightStage {
        label: label.to_owned(),
        leader_id: leader_id.to_owned(),
        leader_term,
        logs: observe_logs(logs),
    }
}

fn observe_logs(logs: &[Vec<LogEntry>]) -> Vec<FigureEightServerLog> {
    SERVER_IDS
        .iter()
        .zip(logs)
        .map(|(server_id, entries)| FigureEightServerLog {
            server_id: (*server_id).to_owned(),
            entry_terms: entries.iter().map(|entry| entry.term).collect(),
        })
        .collect()
}

fn count_entry(logs: &[Vec<LogEntry>], index: u64, term: u64) -> usize {
    logs.iter()
        .filter(|log| {
            usize::try_from(index.saturating_sub(1))
                .ok()
                .and_then(|position| log.get(position))
                .is_some_and(|entry| entry.index == index && entry.term == term)
        })
        .count()
}

fn highest_replica_counted_index(
    state: &PersistentState,
    match_indexes: &[u64],
    majority: usize,
) -> Option<u64> {
    (state.commit_index.saturating_add(1)..=u64::try_from(state.log.len()).unwrap_or(u64::MAX))
        .rev()
        .find(|candidate| {
            match_indexes
                .iter()
                .filter(|matched| **matched >= *candidate)
                .count()
                >= majority
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_eight_requires_a_current_term_entry_before_counting_replicas_is_safe() {
        let report = figure_eight_safety_report();

        assert!(report.passed, "{report:#?}");
        assert_eq!(report.old_term_majority.replica_count, 3);
        assert_eq!(report.old_term_majority.majority_only_candidate, Some(2));
        assert_eq!(report.old_term_majority.current_term_rule_candidate, None);
        assert_eq!(report.unsafe_branch.old_entry_replicas_after_overwrite, 1);
        assert_eq!(
            report
                .safe_branch
                .current_term_rule_candidate_before_majority,
            None
        );
        assert_eq!(report.safe_branch.current_term_rule_candidate, Some(3));
        assert!(report.safe_branch.prior_entry_committed_indirectly);
        assert_eq!(report.safe_branch.challenger_eligible_voters, ["S4", "S5"]);
    }
}

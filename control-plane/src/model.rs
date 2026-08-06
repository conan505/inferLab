use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::RaftError;

pub const DEFAULT_CLUSTER_ID: &str = "inferlab-default";

pub fn validate_cluster_id(cluster_id: &str) -> Result<(), RaftError> {
    let valid = !cluster_id.is_empty()
        && cluster_id.len() <= 128
        && cluster_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(RaftError::Invalid(
            "cluster_id must contain 1 to 128 ASCII letters, digits, '.', '_', or '-'".to_owned(),
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerConfiguration {
    pub id: String,
    pub base_url: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoutingConfiguration {
    pub routing_policy: String,
    pub workers: Vec<WorkerConfiguration>,
}

impl RoutingConfiguration {
    pub fn validate(&self) -> Result<(), RaftError> {
        const POLICIES: [&str; 5] = [
            "round-robin",
            "least-in-flight",
            "weighted-round-robin",
            "ewma-latency",
            "consistent-hash",
        ];
        if !POLICIES.contains(&self.routing_policy.as_str()) {
            return Err(RaftError::Invalid(format!(
                "routing_policy must be one of {}",
                POLICIES.join(", ")
            )));
        }
        if self.workers.is_empty() || self.workers.len() > 100 {
            return Err(RaftError::Invalid(
                "workers must contain between 1 and 100 entries".to_owned(),
            ));
        }
        let mut identities = HashSet::new();
        for worker in &self.workers {
            if worker.id.trim().is_empty() || worker.id.len() > 100 {
                return Err(RaftError::Invalid(
                    "worker IDs must contain 1 to 100 bytes".to_owned(),
                ));
            }
            if !identities.insert(worker.id.trim()) {
                return Err(RaftError::Invalid(format!(
                    "worker ID {} is duplicated",
                    worker.id
                )));
            }
            if worker.weight == 0 {
                return Err(RaftError::Invalid(format!(
                    "worker {} must have a positive weight",
                    worker.id
                )));
            }
            if !(worker.base_url.starts_with("http://") || worker.base_url.starts_with("https://"))
            {
                return Err(RaftError::Invalid(format!(
                    "worker {} base_url must start with http:// or https://",
                    worker.id
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommittedConfiguration {
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,
    pub revision: u64,
    pub term: u64,
    pub configuration: RoutingConfiguration,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Noop,
    SetRoutingConfiguration { configuration: RoutingConfiguration },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LogEntry {
    pub index: u64,
    pub term: u64,
    pub command: Command,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PersistentState {
    #[serde(default)]
    pub cluster_id: String,
    pub current_term: u64,
    pub voted_for: Option<String>,
    pub log: Vec<LogEntry>,
    pub commit_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestVoteRequest {
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,
    pub term: u64,
    pub candidate_id: String,
    pub last_log_index: u64,
    pub last_log_term: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestVoteResponse {
    pub term: u64,
    pub vote_granted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppendEntriesRequest {
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,
    pub term: u64,
    pub leader_id: String,
    pub prev_log_index: u64,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppendEntriesResponse {
    pub term: u64,
    pub success: bool,
    pub match_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConfigWriteResponse {
    pub node_id: String,
    pub cluster_id: String,
    pub revision: u64,
    pub term: u64,
    pub configuration: RoutingConfiguration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeStatus {
    pub node_id: String,
    pub cluster_id: String,
    pub role: Role,
    pub term: u64,
    pub leader_id: Option<String>,
    pub voted_for: Option<String>,
    pub commit_index: u64,
    pub last_applied: u64,
    pub last_log_index: u64,
    pub last_log_term: u64,
    pub committed_configuration: Option<CommittedConfiguration>,
    pub elections_started: u64,
    pub leadership_terms: u64,
    pub votes_granted: u64,
    pub append_entries_accepted: u64,
    pub append_entries_rejected: u64,
    pub replication_successes: u64,
    pub replication_failures: u64,
    pub storage_healthy: bool,
    pub peers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TraceEvent {
    pub at_ms: u64,
    pub node_id: String,
    pub event: String,
    pub term: u64,
    pub role: Role,
    pub leader_id: Option<String>,
    pub log_index: Option<u64>,
    pub detail: String,
}

fn default_weight() -> u32 {
    1
}

fn default_cluster_id() -> String {
    DEFAULT_CLUSTER_ID.to_owned()
}

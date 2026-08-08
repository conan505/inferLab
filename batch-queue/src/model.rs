use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Claimed,
    Completed,
    DeadLetter,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaimLease {
    pub consumer_id: String,
    pub claim_token: String,
    pub visibility_deadline_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JobRecord {
    pub id: String,
    pub idempotency_key: String,
    pub payload: Value,
    pub max_attempts: u32,
    pub attempts: u32,
    pub status: JobStatus,
    pub active_claim: Option<ClaimLease>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EnqueueRequest {
    pub idempotency_key: String,
    pub payload: Value,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct EnqueueResponse {
    pub created: bool,
    pub job: JobRecord,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClaimRequest {
    pub consumer_id: String,
    #[serde(default = "default_visibility_timeout_ms")]
    pub visibility_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClaimResponse {
    pub job_id: String,
    pub idempotency_key: String,
    pub payload: Value,
    pub attempt: u32,
    pub max_attempts: u32,
    pub consumer_id: String,
    pub claim_token: String,
    pub visibility_deadline_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AckRequest {
    pub consumer_id: String,
    pub claim_token: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FailRequest {
    pub consumer_id: String,
    pub claim_token: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueueSnapshot {
    pub wal_path: String,
    pub wal_bytes: u64,
    pub wal_events: u64,
    pub jobs_total: usize,
    pub pending: usize,
    pub claimed: usize,
    pub completed: usize,
    pub dead_letter: usize,
    pub claims_total: u64,
    pub acknowledgments_total: u64,
    pub redeliveries_total: u64,
    pub explicit_failures_total: u64,
    pub dead_lettered_total: u64,
    pub torn_tail_records_discarded: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueMetricsSnapshot {
    pub wal_bytes: u64,
    pub wal_events: u64,
    pub pending: u64,
    pub claimed: u64,
    pub completed: u64,
    pub dead_letter: u64,
    pub claims_total: u64,
    pub acknowledgments_total: u64,
    pub redeliveries_total: u64,
    pub explicit_failures_total: u64,
    pub dead_lettered_total: u64,
    pub torn_tail_records_discarded: u64,
}

pub fn default_max_attempts() -> u32 {
    3
}

pub fn default_visibility_timeout_ms() -> u64 {
    30_000
}

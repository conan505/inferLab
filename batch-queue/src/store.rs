use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{
    QueueError,
    model::{
        AckRequest, ClaimLease, ClaimRequest, ClaimResponse, EnqueueRequest, EnqueueResponse,
        FailRequest, JobRecord, JobStatus, QueueMetricsSnapshot, QueueSnapshot,
    },
    wal::{ReplayResult, Wal, WalEvent, enqueued_job},
};

const MAX_IDEMPOTENCY_KEY_BYTES: usize = 200;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 1_000;
const MAX_ATTEMPTS: u32 = 100;
const MAX_VISIBILITY_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Debug, Default)]
struct QueueData {
    jobs: BTreeMap<String, JobRecord>,
    idempotency_index: HashMap<String, String>,
    next_job_number: u64,
    next_claim_number: u64,
    pending: u64,
    claimed: u64,
    completed: u64,
    dead_letter: u64,
    claims_total: u64,
    acknowledgments_total: u64,
    redeliveries_total: u64,
    explicit_failures_total: u64,
    dead_lettered_total: u64,
    torn_tail_records_discarded: u64,
}

#[derive(Debug)]
struct StoreInner {
    wal: Wal,
    data: QueueData,
}

#[derive(Debug)]
pub struct QueueStore {
    inner: Mutex<StoreInner>,
}

impl QueueStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>, QueueError> {
        let (wal, replay) = Wal::open(path)?;
        let mut data = QueueData {
            next_job_number: 1,
            next_claim_number: 1,
            torn_tail_records_discarded: u64::from(replay.discarded_torn_tail),
            ..QueueData::default()
        };
        replay_events(&mut data, &replay)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(StoreInner { wal, data }),
        }))
    }

    pub fn enqueue(
        &self,
        request: EnqueueRequest,
        now_ms: u64,
    ) -> Result<EnqueueResponse, QueueError> {
        validate_nonempty_limited(
            "idempotency_key",
            &request.idempotency_key,
            MAX_IDEMPOTENCY_KEY_BYTES,
        )?;
        if !(1..=MAX_ATTEMPTS).contains(&request.max_attempts) {
            return Err(QueueError::Invalid(format!(
                "max_attempts must be between 1 and {MAX_ATTEMPTS}"
            )));
        }
        let payload_bytes = serde_json::to_vec(&request.payload)
            .map_err(|error| QueueError::Invalid(format!("payload is not valid JSON: {error}")))?
            .len();
        if payload_bytes > MAX_PAYLOAD_BYTES {
            return Err(QueueError::Invalid(format!(
                "payload must be at most {MAX_PAYLOAD_BYTES} encoded bytes"
            )));
        }

        let mut inner = self.lock()?;
        if let Some(existing_id) = inner
            .data
            .idempotency_index
            .get(&request.idempotency_key)
            .cloned()
        {
            let existing = inner.data.jobs.get(&existing_id).ok_or_else(|| {
                QueueError::Storage(format!(
                    "idempotency index references missing job {existing_id}"
                ))
            })?;
            if existing.payload != request.payload || existing.max_attempts != request.max_attempts
            {
                return Err(QueueError::IdempotencyConflict(format!(
                    "idempotency key {} is already bound to a different request",
                    request.idempotency_key
                )));
            }
            return Ok(EnqueueResponse {
                created: false,
                job: existing.clone(),
            });
        }

        let job_id = format!("batch-{:08}", inner.data.next_job_number);
        let event = WalEvent::Enqueued {
            job_id,
            idempotency_key: request.idempotency_key,
            payload: request.payload,
            max_attempts: request.max_attempts,
            at_ms: now_ms,
        };
        inner.wal.append(&event)?;
        apply_event(&mut inner.data, &event)?;
        let job = enqueued_job(&event).expect("enqueue event always creates a job");
        Ok(EnqueueResponse { created: true, job })
    }

    pub fn claim(
        &self,
        request: ClaimRequest,
        now_ms: u64,
    ) -> Result<Option<ClaimResponse>, QueueError> {
        validate_nonempty_limited("consumer_id", &request.consumer_id, 200)?;
        if !(1..=MAX_VISIBILITY_TIMEOUT_MS).contains(&request.visibility_timeout_ms) {
            return Err(QueueError::Invalid(format!(
                "visibility_timeout_ms must be between 1 and {MAX_VISIBILITY_TIMEOUT_MS}"
            )));
        }

        let mut inner = self.lock()?;
        refresh_expired_claims(&mut inner, now_ms)?;
        let Some(job_id) = inner
            .data
            .jobs
            .iter()
            .find_map(|(id, job)| (job.status == JobStatus::Pending).then(|| id.clone()))
        else {
            return Ok(None);
        };
        let attempt = inner
            .data
            .jobs
            .get(&job_id)
            .map(|job| job.attempts.saturating_add(1))
            .ok_or_else(|| QueueError::Storage(format!("selected job {job_id} disappeared")))?;
        let claim_token = format!("claim-{:012}", inner.data.next_claim_number);
        let visibility_deadline_ms = now_ms.saturating_add(request.visibility_timeout_ms);
        let event = WalEvent::Claimed {
            job_id: job_id.clone(),
            consumer_id: request.consumer_id.clone(),
            claim_token: claim_token.clone(),
            visibility_deadline_ms,
            attempt,
            at_ms: now_ms,
        };
        inner.wal.append(&event)?;
        apply_event(&mut inner.data, &event)?;
        let job = inner
            .data
            .jobs
            .get(&job_id)
            .ok_or_else(|| QueueError::Storage(format!("claimed job {job_id} disappeared")))?;
        Ok(Some(ClaimResponse {
            job_id,
            idempotency_key: job.idempotency_key.clone(),
            payload: job.payload.clone(),
            attempt,
            max_attempts: job.max_attempts,
            consumer_id: request.consumer_id,
            claim_token,
            visibility_deadline_ms,
        }))
    }

    pub fn acknowledge(
        &self,
        job_id: &str,
        request: AckRequest,
        now_ms: u64,
    ) -> Result<JobRecord, QueueError> {
        validate_nonempty_limited("consumer_id", &request.consumer_id, 200)?;
        validate_nonempty_limited("claim_token", &request.claim_token, 200)?;
        let mut inner = self.lock()?;
        refresh_expired_claims(&mut inner, now_ms)?;
        require_active_claim(
            &inner.data,
            job_id,
            &request.consumer_id,
            &request.claim_token,
        )?;
        let event = WalEvent::Acknowledged {
            job_id: job_id.to_owned(),
            claim_token: request.claim_token,
            at_ms: now_ms,
        };
        inner.wal.append(&event)?;
        apply_event(&mut inner.data, &event)?;
        clone_job(&inner.data, job_id)
    }

    pub fn fail(
        &self,
        job_id: &str,
        request: FailRequest,
        now_ms: u64,
    ) -> Result<JobRecord, QueueError> {
        validate_nonempty_limited("consumer_id", &request.consumer_id, 200)?;
        validate_nonempty_limited("claim_token", &request.claim_token, 200)?;
        validate_nonempty_limited("error", &request.error, MAX_ERROR_BYTES)?;
        let mut inner = self.lock()?;
        refresh_expired_claims(&mut inner, now_ms)?;
        require_active_claim(
            &inner.data,
            job_id,
            &request.consumer_id,
            &request.claim_token,
        )?;
        let exhausted = inner
            .data
            .jobs
            .get(job_id)
            .map(|job| job.attempts >= job.max_attempts)
            .ok_or_else(|| QueueError::NotFound(format!("job {job_id} does not exist")))?;
        let event = if exhausted {
            WalEvent::DeadLettered {
                job_id: job_id.to_owned(),
                claim_token: request.claim_token,
                reason: request.error,
                expired: false,
                at_ms: now_ms,
            }
        } else {
            WalEvent::Released {
                job_id: job_id.to_owned(),
                claim_token: request.claim_token,
                reason: request.error,
                expired: false,
                at_ms: now_ms,
            }
        };
        inner.wal.append(&event)?;
        apply_event(&mut inner.data, &event)?;
        clone_job(&inner.data, job_id)
    }

    pub fn get_job(&self, job_id: &str, now_ms: u64) -> Result<JobRecord, QueueError> {
        let mut inner = self.lock()?;
        refresh_expired_claims(&mut inner, now_ms)?;
        clone_job(&inner.data, job_id)
    }

    pub fn dead_letters(&self, now_ms: u64) -> Result<Vec<JobRecord>, QueueError> {
        let mut inner = self.lock()?;
        refresh_expired_claims(&mut inner, now_ms)?;
        Ok(inner
            .data
            .jobs
            .values()
            .filter(|job| job.status == JobStatus::DeadLetter)
            .cloned()
            .collect())
    }

    pub fn snapshot(&self, now_ms: u64) -> Result<QueueSnapshot, QueueError> {
        let mut inner = self.lock()?;
        refresh_expired_claims(&mut inner, now_ms)?;
        Ok(QueueSnapshot {
            wal_path: inner.wal.path().display().to_string(),
            wal_bytes: inner.wal.bytes(),
            wal_events: inner.wal.events(),
            jobs_total: inner.data.jobs.len(),
            pending: usize::try_from(inner.data.pending).unwrap_or(usize::MAX),
            claimed: usize::try_from(inner.data.claimed).unwrap_or(usize::MAX),
            completed: usize::try_from(inner.data.completed).unwrap_or(usize::MAX),
            dead_letter: usize::try_from(inner.data.dead_letter).unwrap_or(usize::MAX),
            claims_total: inner.data.claims_total,
            acknowledgments_total: inner.data.acknowledgments_total,
            redeliveries_total: inner.data.redeliveries_total,
            explicit_failures_total: inner.data.explicit_failures_total,
            dead_lettered_total: inner.data.dead_lettered_total,
            torn_tail_records_discarded: inner.data.torn_tail_records_discarded,
        })
    }

    /// Returns only scalar counters and gauges for the metrics exporter.
    ///
    /// Unlike the operator-facing snapshot, this does not refresh leases, scan
    /// jobs, allocate identifiers, or write the WAL. A scrape is therefore
    /// observational and its lock hold is constant with respect to queue size.
    pub fn metrics_snapshot(&self) -> Result<QueueMetricsSnapshot, QueueError> {
        let inner = self.lock()?;
        Ok(QueueMetricsSnapshot {
            wal_bytes: inner.wal.bytes(),
            wal_events: inner.wal.events(),
            pending: inner.data.pending,
            claimed: inner.data.claimed,
            completed: inner.data.completed,
            dead_letter: inner.data.dead_letter,
            claims_total: inner.data.claims_total,
            acknowledgments_total: inner.data.acknowledgments_total,
            redeliveries_total: inner.data.redeliveries_total,
            explicit_failures_total: inner.data.explicit_failures_total,
            dead_lettered_total: inner.data.dead_lettered_total,
            torn_tail_records_discarded: inner.data.torn_tail_records_discarded,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, StoreInner>, QueueError> {
        self.inner
            .lock()
            .map_err(|_| QueueError::Storage("queue state mutex was poisoned".to_owned()))
    }
}

fn replay_events(data: &mut QueueData, replay: &ReplayResult) -> Result<(), QueueError> {
    for event in &replay.events {
        apply_event(data, event)?;
    }
    Ok(())
}

fn refresh_expired_claims(inner: &mut StoreInner, now_ms: u64) -> Result<(), QueueError> {
    let expired: Vec<(String, String, bool)> = inner
        .data
        .jobs
        .iter()
        .filter_map(|(id, job)| {
            let lease = job.active_claim.as_ref()?;
            (job.status == JobStatus::Claimed && lease.visibility_deadline_ms <= now_ms).then(
                || {
                    (
                        id.clone(),
                        lease.claim_token.clone(),
                        job.attempts >= job.max_attempts,
                    )
                },
            )
        })
        .collect();
    for (job_id, claim_token, exhausted) in expired {
        let event = if exhausted {
            WalEvent::DeadLettered {
                job_id,
                claim_token,
                reason: "visibility_timeout".to_owned(),
                expired: true,
                at_ms: now_ms,
            }
        } else {
            WalEvent::Released {
                job_id,
                claim_token,
                reason: "visibility_timeout".to_owned(),
                expired: true,
                at_ms: now_ms,
            }
        };
        inner.wal.append(&event)?;
        apply_event(&mut inner.data, &event)?;
    }
    Ok(())
}

fn apply_event(data: &mut QueueData, event: &WalEvent) -> Result<(), QueueError> {
    match event {
        WalEvent::Enqueued {
            job_id,
            idempotency_key,
            ..
        } => {
            if data.jobs.contains_key(job_id) {
                return Err(logical_wal_error(format!("duplicate job id {job_id}")));
            }
            if data.idempotency_index.contains_key(idempotency_key) {
                return Err(logical_wal_error(format!(
                    "duplicate idempotency key {idempotency_key}"
                )));
            }
            let job = enqueued_job(event)
                .ok_or_else(|| logical_wal_error("enqueue event could not create job"))?;
            data.jobs.insert(job_id.clone(), job);
            data.pending = data.pending.saturating_add(1);
            data.idempotency_index
                .insert(idempotency_key.clone(), job_id.clone());
            data.next_job_number = data
                .next_job_number
                .max(parse_sequence(job_id, "batch-").saturating_add(1));
        }
        WalEvent::Claimed {
            job_id,
            consumer_id,
            claim_token,
            visibility_deadline_ms,
            attempt,
            at_ms,
        } => {
            let job = data.jobs.get_mut(job_id).ok_or_else(|| {
                logical_wal_error(format!("claim references missing job {job_id}"))
            })?;
            if job.status != JobStatus::Pending || *attempt != job.attempts.saturating_add(1) {
                return Err(logical_wal_error(format!(
                    "invalid claim transition for job {job_id}"
                )));
            }
            job.status = JobStatus::Claimed;
            job.attempts = *attempt;
            job.active_claim = Some(ClaimLease {
                consumer_id: consumer_id.clone(),
                claim_token: claim_token.clone(),
                visibility_deadline_ms: *visibility_deadline_ms,
            });
            job.updated_at_ms = *at_ms;
            data.pending = data.pending.saturating_sub(1);
            data.claimed = data.claimed.saturating_add(1);
            data.claims_total = data.claims_total.saturating_add(1);
            if *attempt > 1 {
                data.redeliveries_total = data.redeliveries_total.saturating_add(1);
            }
            data.next_claim_number = data
                .next_claim_number
                .max(parse_sequence(claim_token, "claim-").saturating_add(1));
        }
        WalEvent::Released {
            job_id,
            claim_token,
            reason,
            expired,
            at_ms,
        } => {
            let job = transition_claimed_job(data, job_id, claim_token)?;
            job.status = JobStatus::Pending;
            job.active_claim = None;
            job.updated_at_ms = *at_ms;
            job.last_error = Some(reason.clone());
            if !expired {
                data.explicit_failures_total = data.explicit_failures_total.saturating_add(1);
            }
            data.claimed = data.claimed.saturating_sub(1);
            data.pending = data.pending.saturating_add(1);
        }
        WalEvent::Acknowledged {
            job_id,
            claim_token,
            at_ms,
        } => {
            let job = transition_claimed_job(data, job_id, claim_token)?;
            job.status = JobStatus::Completed;
            job.active_claim = None;
            job.updated_at_ms = *at_ms;
            data.acknowledgments_total = data.acknowledgments_total.saturating_add(1);
            data.claimed = data.claimed.saturating_sub(1);
            data.completed = data.completed.saturating_add(1);
        }
        WalEvent::DeadLettered {
            job_id,
            claim_token,
            reason,
            expired,
            at_ms,
        } => {
            let job = transition_claimed_job(data, job_id, claim_token)?;
            job.status = JobStatus::DeadLetter;
            job.active_claim = None;
            job.updated_at_ms = *at_ms;
            job.last_error = Some(reason.clone());
            if !expired {
                data.explicit_failures_total = data.explicit_failures_total.saturating_add(1);
            }
            data.dead_lettered_total = data.dead_lettered_total.saturating_add(1);
            data.claimed = data.claimed.saturating_sub(1);
            data.dead_letter = data.dead_letter.saturating_add(1);
        }
    }
    Ok(())
}

fn transition_claimed_job<'a>(
    data: &'a mut QueueData,
    job_id: &str,
    claim_token: &str,
) -> Result<&'a mut JobRecord, QueueError> {
    let job = data
        .jobs
        .get_mut(job_id)
        .ok_or_else(|| logical_wal_error(format!("transition references missing job {job_id}")))?;
    let matching_token = job
        .active_claim
        .as_ref()
        .is_some_and(|lease| lease.claim_token == claim_token);
    if job.status != JobStatus::Claimed || !matching_token {
        return Err(logical_wal_error(format!(
            "transition uses inactive claim {claim_token} for job {job_id}"
        )));
    }
    Ok(job)
}

fn require_active_claim(
    data: &QueueData,
    job_id: &str,
    consumer_id: &str,
    claim_token: &str,
) -> Result<(), QueueError> {
    let job = data
        .jobs
        .get(job_id)
        .ok_or_else(|| QueueError::NotFound(format!("job {job_id} does not exist")))?;
    let active = job.active_claim.as_ref();
    if job.status != JobStatus::Claimed
        || !active.is_some_and(|lease| {
            lease.consumer_id == consumer_id && lease.claim_token == claim_token
        })
    {
        return Err(QueueError::StaleClaim(format!(
            "claim token is stale or does not own job {job_id}"
        )));
    }
    Ok(())
}

fn clone_job(data: &QueueData, job_id: &str) -> Result<JobRecord, QueueError> {
    data.jobs
        .get(job_id)
        .cloned()
        .ok_or_else(|| QueueError::NotFound(format!("job {job_id} does not exist")))
}

fn validate_nonempty_limited(
    field: &str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), QueueError> {
    if value.trim().is_empty() {
        return Err(QueueError::Invalid(format!("{field} must not be empty")));
    }
    if value.len() > maximum_bytes {
        return Err(QueueError::Invalid(format!(
            "{field} must be at most {maximum_bytes} bytes"
        )));
    }
    Ok(())
}

fn parse_sequence(value: &str, prefix: &str) -> u64 {
    value
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or(0)
}

fn logical_wal_error(message: impl Into<String>) -> QueueError {
    QueueError::Storage(format!("invalid WAL transition: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use serde_json::json;

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestWal {
        directory: PathBuf,
        path: PathBuf,
    }

    impl TestWal {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "inferlab-batch-queue-{}-{name}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).expect("create isolated test directory");
            let path = directory.join("events.wal");
            Self { directory, path }
        }
    }

    impl Drop for TestWal {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn enqueue_request(key: &str, max_attempts: u32) -> EnqueueRequest {
        EnqueueRequest {
            idempotency_key: key.to_owned(),
            payload: json!({"prompt": "explain durable queues"}),
            max_attempts,
        }
    }

    fn claim_request(consumer_id: &str, visibility_timeout_ms: u64) -> ClaimRequest {
        ClaimRequest {
            consumer_id: consumer_id.to_owned(),
            visibility_timeout_ms,
        }
    }

    #[test]
    fn enqueue_is_durable_and_idempotent_after_reopen() {
        let wal = TestWal::new("idempotent");
        let first_id;
        {
            let store = QueueStore::open(&wal.path).expect("open queue");
            let first = store
                .enqueue(enqueue_request("request-1", 3), 100)
                .expect("enqueue");
            assert!(first.created);
            first_id = first.job.id;
        }

        let reopened = QueueStore::open(&wal.path).expect("replay queue");
        let duplicate = reopened
            .enqueue(enqueue_request("request-1", 3), 200)
            .expect("deduplicate request");
        assert!(!duplicate.created);
        assert_eq!(duplicate.job.id, first_id);
        assert!(matches!(
            reopened.enqueue(
                EnqueueRequest {
                    idempotency_key: "request-1".to_owned(),
                    payload: json!({"prompt": "different"}),
                    max_attempts: 3,
                },
                201,
            ),
            Err(QueueError::IdempotencyConflict(_))
        ));
        assert_eq!(reopened.snapshot(202).expect("snapshot").wal_events, 1);
    }

    #[test]
    fn expired_claim_redelivers_after_restart_and_fences_old_worker() {
        let wal = TestWal::new("redelivery");
        let first_claim;
        {
            let store = QueueStore::open(&wal.path).expect("open queue");
            store
                .enqueue(enqueue_request("crash-job", 3), 100)
                .expect("enqueue");
            first_claim = store
                .claim(claim_request("consumer-a", 50), 110)
                .expect("claim")
                .expect("job available");
            assert_eq!(first_claim.attempt, 1);
        }

        let reopened = QueueStore::open(&wal.path).expect("replay after crash");
        let second_claim = reopened
            .claim(claim_request("consumer-b", 50), 161)
            .expect("redelivery")
            .expect("expired job available");
        assert_eq!(second_claim.job_id, first_claim.job_id);
        assert_eq!(second_claim.attempt, 2);
        assert_ne!(second_claim.claim_token, first_claim.claim_token);

        let stale = reopened.acknowledge(
            &first_claim.job_id,
            AckRequest {
                consumer_id: "consumer-a".to_owned(),
                claim_token: first_claim.claim_token,
            },
            162,
        );
        assert!(matches!(stale, Err(QueueError::StaleClaim(_))));

        let completed = reopened
            .acknowledge(
                &second_claim.job_id,
                AckRequest {
                    consumer_id: "consumer-b".to_owned(),
                    claim_token: second_claim.claim_token,
                },
                163,
            )
            .expect("current owner can acknowledge");
        assert_eq!(completed.status, JobStatus::Completed);
        let snapshot = reopened.snapshot(164).expect("snapshot");
        assert_eq!(snapshot.redeliveries_total, 1);
        assert_eq!(snapshot.acknowledgments_total, 1);
    }

    #[test]
    fn bounded_failures_move_poison_job_to_dead_letter_queue() {
        let wal = TestWal::new("dead-letter");
        let store = QueueStore::open(&wal.path).expect("open queue");
        let job = store
            .enqueue(enqueue_request("poison", 2), 100)
            .expect("enqueue")
            .job;

        let first = store
            .claim(claim_request("consumer-a", 100), 101)
            .expect("claim")
            .expect("job");
        let released = store
            .fail(
                &job.id,
                FailRequest {
                    consumer_id: first.consumer_id,
                    claim_token: first.claim_token,
                    error: "worker rejected payload".to_owned(),
                },
                102,
            )
            .expect("release first failure");
        assert_eq!(released.status, JobStatus::Pending);

        let second = store
            .claim(claim_request("consumer-b", 100), 103)
            .expect("claim")
            .expect("job");
        let dead = store
            .fail(
                &job.id,
                FailRequest {
                    consumer_id: second.consumer_id,
                    claim_token: second.claim_token,
                    error: "worker rejected payload again".to_owned(),
                },
                104,
            )
            .expect("dead-letter exhausted job");
        assert_eq!(dead.status, JobStatus::DeadLetter);
        assert_eq!(store.dead_letters(105).expect("dead letters"), vec![dead]);
        let snapshot = store.snapshot(106).expect("snapshot");
        assert_eq!(snapshot.explicit_failures_total, 2);
        assert_eq!(snapshot.dead_lettered_total, 1);
    }

    #[test]
    fn replay_discards_only_an_incomplete_final_record() {
        let wal = TestWal::new("torn-tail");
        {
            let store = QueueStore::open(&wal.path).expect("open queue");
            store
                .enqueue(enqueue_request("safe-record", 3), 100)
                .expect("enqueue");
        }
        let valid_length = fs::metadata(&wal.path).expect("metadata").len();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&wal.path)
            .expect("open WAL");
        file.write_all(br#"{"type":"enqueued","job_id":"torn"#)
            .expect("append torn record");
        file.sync_data().expect("sync torn record");
        drop(file);

        let reopened = QueueStore::open(&wal.path).expect("ignore torn final record");
        let snapshot = reopened.snapshot(200).expect("snapshot");
        assert_eq!(snapshot.jobs_total, 1);
        assert_eq!(snapshot.wal_events, 1);
        assert_eq!(snapshot.wal_bytes, valid_length);
        assert_eq!(snapshot.torn_tail_records_discarded, 1);
        assert_eq!(
            fs::metadata(&wal.path).expect("metadata").len(),
            valid_length
        );
    }

    #[test]
    fn replay_rejects_malformed_complete_record() {
        let wal = TestWal::new("malformed");
        fs::write(&wal.path, b"{not-json}\n").expect("write malformed record");
        assert!(matches!(
            QueueStore::open(&wal.path),
            Err(QueueError::Storage(_))
        ));
    }
}

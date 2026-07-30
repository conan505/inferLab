use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{QueueError, model::JobRecord};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WalEvent {
    Enqueued {
        job_id: String,
        idempotency_key: String,
        payload: Value,
        max_attempts: u32,
        at_ms: u64,
    },
    Claimed {
        job_id: String,
        consumer_id: String,
        claim_token: String,
        visibility_deadline_ms: u64,
        attempt: u32,
        at_ms: u64,
    },
    Released {
        job_id: String,
        claim_token: String,
        reason: String,
        expired: bool,
        at_ms: u64,
    },
    Acknowledged {
        job_id: String,
        claim_token: String,
        at_ms: u64,
    },
    DeadLettered {
        job_id: String,
        claim_token: String,
        reason: String,
        expired: bool,
        at_ms: u64,
    },
}

#[derive(Debug)]
pub(crate) struct ReplayResult {
    pub events: Vec<WalEvent>,
    pub valid_bytes: u64,
    pub discarded_torn_tail: bool,
}

#[derive(Debug)]
pub(crate) struct Wal {
    path: PathBuf,
    file: File,
    bytes: u64,
    events: u64,
    poisoned: Option<String>,
}

impl Wal {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<(Self, ReplayResult), QueueError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(QueueError::storage)?;
        }
        let replay = replay(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(QueueError::storage)?;
        if replay.discarded_torn_tail {
            file.set_len(replay.valid_bytes)
                .map_err(QueueError::storage)?;
            file.sync_data().map_err(QueueError::storage)?;
        }
        let events = u64::try_from(replay.events.len()).unwrap_or(u64::MAX);
        Ok((
            Self {
                path,
                file,
                bytes: replay.valid_bytes,
                events,
                poisoned: None,
            },
            replay,
        ))
    }

    pub(crate) fn append(&mut self, event: &WalEvent) -> Result<(), QueueError> {
        if let Some(reason) = &self.poisoned {
            return Err(QueueError::Storage(format!(
                "WAL is unavailable after an earlier append failure: {reason}"
            )));
        }
        let mut serialized = serde_json::to_vec(event)
            .map_err(|error| QueueError::Storage(format!("serialize WAL event: {error}")))?;
        serialized.push(b'\n');
        if let Err(error) = self.file.write_all(&serialized) {
            let message = format!("append WAL record: {error}");
            self.poisoned = Some(message.clone());
            return Err(QueueError::Storage(message));
        }
        // The API never confirms a state transition before its WAL bytes reach
        // stable storage according to the host filesystem's sync_data contract.
        if let Err(error) = self.file.sync_data() {
            let message = format!("sync WAL record: {error}");
            self.poisoned = Some(message.clone());
            return Err(QueueError::Storage(message));
        }
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(serialized.len()).unwrap_or(u64::MAX));
        self.events = self.events.saturating_add(1);
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn events(&self) -> u64 {
        self.events
    }
}

fn replay(path: &Path) -> Result<ReplayResult, QueueError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(QueueError::storage(error)),
    };
    let mut events = Vec::new();
    let mut offset = 0_usize;
    let mut valid_bytes = 0_usize;
    let mut discarded_torn_tail = false;

    while offset < bytes.len() {
        let remainder = &bytes[offset..];
        let Some(relative_newline) = remainder.iter().position(|byte| *byte == b'\n') else {
            discarded_torn_tail = true;
            break;
        };
        let end = offset + relative_newline;
        let line = &bytes[offset..end];
        if line.is_empty() {
            return Err(QueueError::Storage(format!(
                "WAL {} contains an empty record at byte {offset}",
                path.display()
            )));
        }
        let event = serde_json::from_slice::<WalEvent>(line).map_err(|error| {
            QueueError::Storage(format!(
                "WAL {} has an invalid complete record at byte {offset}: {error}",
                path.display()
            ))
        })?;
        events.push(event);
        offset = end + 1;
        valid_bytes = offset;
    }

    Ok(ReplayResult {
        events,
        valid_bytes: u64::try_from(valid_bytes).unwrap_or(u64::MAX),
        discarded_torn_tail,
    })
}

pub(crate) fn enqueued_job(event: &WalEvent) -> Option<JobRecord> {
    let WalEvent::Enqueued {
        job_id,
        idempotency_key,
        payload,
        max_attempts,
        at_ms,
    } = event
    else {
        return None;
    };
    Some(JobRecord {
        id: job_id.clone(),
        idempotency_key: idempotency_key.clone(),
        payload: payload.clone(),
        max_attempts: *max_attempts,
        attempts: 0,
        status: crate::model::JobStatus::Pending,
        active_claim: None,
        created_at_ms: *at_ms,
        updated_at_ms: *at_ms,
        last_error: None,
    })
}

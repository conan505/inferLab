use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::routing::RoutingPolicy;

pub const ROUTING_SNAPSHOT_SCHEMA: &str = "inferlab.gateway-routing-snapshot.v1";
pub const DEFAULT_CONTROL_CLUSTER_ID: &str = "inferlab-default";
const MAX_SNAPSHOT_BYTES: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotFreshnessPolicy {
    pub maximum_age_ms: Option<u64>,
    pub maximum_future_skew_ms: u64,
}

impl Default for SnapshotFreshnessPolicy {
    fn default() -> Self {
        Self {
            maximum_age_ms: None,
            maximum_future_skew_ms: 1_000,
        }
    }
}

impl SnapshotFreshnessPolicy {
    pub fn expires_at_ms(self, saved_at_ms: u64) -> Option<u64> {
        self.maximum_age_ms
            .map(|maximum_age_ms| saved_at_ms.saturating_add(maximum_age_ms))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotFreshness {
    pub observed_age_ms: u64,
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommittedRoutingConfiguration {
    #[serde(default = "default_control_cluster_id")]
    pub cluster_id: String,
    pub revision: u64,
    pub term: u64,
    pub configuration: StoredRoutingConfiguration,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoredRoutingConfiguration {
    pub routing_policy: String,
    pub workers: Vec<StoredWorkerConfiguration>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoredWorkerConfiguration {
    pub id: String,
    pub base_url: String,
    pub weight: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PersistedRoutingSnapshot {
    pub schema: String,
    pub saved_at_ms: u64,
    #[serde(flatten)]
    pub committed: CommittedRoutingConfiguration,
}

#[derive(Clone, Debug)]
pub struct RoutingSnapshotStore {
    path: PathBuf,
}

impl RoutingSnapshotStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<PersistedRoutingSnapshot> {
        let metadata = fs::metadata(&self.path)?;
        if metadata.len() > MAX_SNAPSHOT_BYTES {
            return Err(invalid_data(format!(
                "routing snapshot {} is {} bytes; maximum is {MAX_SNAPSHOT_BYTES}",
                self.path.display(),
                metadata.len()
            )));
        }
        let bytes = fs::read(&self.path)?;
        let snapshot =
            serde_json::from_slice::<PersistedRoutingSnapshot>(&bytes).map_err(|error| {
                invalid_data(format!(
                    "cannot decode routing snapshot {}: {error}",
                    self.path.display()
                ))
            })?;
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    pub fn save(
        &self,
        committed: &CommittedRoutingConfiguration,
    ) -> io::Result<PersistedRoutingSnapshot> {
        let snapshot = PersistedRoutingSnapshot {
            schema: ROUTING_SNAPSHOT_SCHEMA.to_owned(),
            saved_at_ms: now_ms(),
            committed: committed.clone(),
        };
        validate_snapshot(&snapshot)?;
        let bytes = serde_json::to_vec_pretty(&snapshot)
            .map_err(|error| io::Error::other(format!("serialize routing snapshot: {error}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
            return Err(invalid_data(format!(
                "serialized routing snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
            )));
        }

        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let directory = parent.unwrap_or_else(|| Path::new("."));
        let file_name = self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| invalid_data("routing snapshot path must end in a UTF-8 file name"))?;
        let temporary = directory.join(format!(".{file_name}.{}.tmp", process::id()));
        let write_result: io::Result<()> = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            sync_directory(directory)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result?;
        Ok(snapshot)
    }
}

fn validate_snapshot(snapshot: &PersistedRoutingSnapshot) -> io::Result<()> {
    if snapshot.schema != ROUTING_SNAPSHOT_SCHEMA {
        return Err(invalid_data(format!(
            "unsupported routing snapshot schema '{}'; expected '{ROUTING_SNAPSHOT_SCHEMA}'",
            snapshot.schema
        )));
    }
    validate_committed(&snapshot.committed)
}

pub fn validate_committed(committed: &CommittedRoutingConfiguration) -> io::Result<()> {
    validate_control_cluster_id(&committed.cluster_id)?;
    if committed.revision == 0 || committed.term == 0 {
        return Err(invalid_data(
            "routing snapshot revision and term must both be positive",
        ));
    }
    committed
        .configuration
        .routing_policy
        .parse::<RoutingPolicy>()
        .map_err(invalid_data)?;
    if committed.configuration.workers.is_empty() {
        return Err(invalid_data(
            "routing snapshot must contain at least one worker",
        ));
    }
    let mut identities = HashSet::new();
    for worker in &committed.configuration.workers {
        if worker.id.trim().is_empty() || worker.base_url.trim().is_empty() {
            return Err(invalid_data(
                "routing snapshot worker id and base_url must be non-empty",
            ));
        }
        if worker.weight == 0 {
            return Err(invalid_data(format!(
                "routing snapshot worker '{}' must have positive weight",
                worker.id
            )));
        }
        if !identities.insert(worker.id.as_str()) {
            return Err(invalid_data(format!(
                "routing snapshot contains duplicate worker id '{}'",
                worker.id
            )));
        }
    }
    Ok(())
}

pub fn validate_control_cluster_id(cluster_id: &str) -> io::Result<()> {
    let valid = !cluster_id.is_empty()
        && cluster_id.len() <= 128
        && cluster_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(invalid_data(
            "control cluster identity must contain 1 to 128 ASCII letters, digits, '.', '_', or '-'",
        ))
    }
}

pub fn validate_expected_control_cluster(
    committed: &CommittedRoutingConfiguration,
    expected_cluster_id: &str,
) -> io::Result<()> {
    validate_control_cluster_id(expected_cluster_id)?;
    if committed.cluster_id == expected_cluster_id {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "control cluster identity mismatch: expected '{expected_cluster_id}', observed '{}'",
            committed.cluster_id
        )))
    }
}

fn default_control_cluster_id() -> String {
    DEFAULT_CONTROL_CLUSTER_ID.to_owned()
}

pub fn validate_snapshot_freshness(
    saved_at_ms: u64,
    observed_at_ms: u64,
    policy: SnapshotFreshnessPolicy,
) -> io::Result<SnapshotFreshness> {
    let future_delta_ms = saved_at_ms.saturating_sub(observed_at_ms);
    if saved_at_ms > observed_at_ms && future_delta_ms > policy.maximum_future_skew_ms {
        return Err(invalid_data(format!(
            "routing snapshot timestamp is {future_delta_ms} ms in the future; maximum allowed clock skew is {} ms",
            policy.maximum_future_skew_ms
        )));
    }

    let observed_age_ms = observed_at_ms.saturating_sub(saved_at_ms);
    if let Some(maximum_age_ms) = policy.maximum_age_ms
        && observed_age_ms > maximum_age_ms
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "routing snapshot expired: observed age {observed_age_ms} ms exceeds maximum {maximum_age_ms} ms"
            ),
        ));
    }

    Ok(SnapshotFreshness {
        observed_age_ms,
        expires_at_ms: policy.expires_at_ms(saved_at_ms),
    })
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "inferlab-routing-snapshot-{name}-{}-{}",
            process::id(),
            now_ms()
        ))
    }

    fn committed(revision: u64) -> CommittedRoutingConfiguration {
        CommittedRoutingConfiguration {
            cluster_id: DEFAULT_CONTROL_CLUSTER_ID.to_owned(),
            revision,
            term: 3,
            configuration: StoredRoutingConfiguration {
                routing_policy: "weighted-round-robin".to_owned(),
                workers: vec![
                    StoredWorkerConfiguration {
                        id: "worker-a".to_owned(),
                        base_url: "http://127.0.0.1:9001".to_owned(),
                        weight: 3,
                    },
                    StoredWorkerConfiguration {
                        id: "worker-b".to_owned(),
                        base_url: "http://127.0.0.1:9002".to_owned(),
                        weight: 1,
                    },
                ],
            },
        }
    }

    #[test]
    fn atomically_round_trips_a_committed_routing_snapshot() {
        let directory = test_directory("round-trip");
        let store = RoutingSnapshotStore::new(directory.join("routing.json"));
        let saved = store.save(&committed(9)).expect("save snapshot");
        let loaded = store.load().expect("load snapshot");

        assert_eq!(loaded, saved);
        assert_eq!(loaded.schema, ROUTING_SNAPSHOT_SCHEMA);
        assert_eq!(loaded.committed.revision, 9);
        let remaining_files = fs::read_dir(&directory)
            .expect("read snapshot directory")
            .map(|entry| {
                entry
                    .expect("read snapshot directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(remaining_files, vec!["routing.json"]);
        fs::remove_dir_all(directory).expect("remove exact test directory");
    }

    #[test]
    fn a_new_save_replaces_the_old_revision() {
        let directory = test_directory("replace");
        let store = RoutingSnapshotStore::new(directory.join("routing.json"));
        store.save(&committed(9)).expect("save first snapshot");
        store.save(&committed(12)).expect("save second snapshot");

        assert_eq!(
            store.load().expect("load replacement").committed.revision,
            12
        );
        fs::remove_dir_all(directory).expect("remove exact test directory");
    }

    #[test]
    fn rejects_corruption_unknown_schema_and_invalid_workers() {
        let directory = test_directory("invalid");
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("routing.json");
        fs::write(&path, b"{not-json\n").expect("write corrupt fixture");
        let store = RoutingSnapshotStore::new(&path);
        assert_eq!(
            store.load().expect_err("reject corrupt JSON").kind(),
            io::ErrorKind::InvalidData
        );

        let mut snapshot = PersistedRoutingSnapshot {
            schema: "future-schema".to_owned(),
            saved_at_ms: now_ms(),
            committed: committed(9),
        };
        fs::write(
            &path,
            serde_json::to_vec(&snapshot).expect("serialize fixture"),
        )
        .expect("write schema fixture");
        assert_eq!(
            store.load().expect_err("reject schema").kind(),
            io::ErrorKind::InvalidData
        );

        snapshot.schema = ROUTING_SNAPSHOT_SCHEMA.to_owned();
        snapshot.committed.configuration.workers[0].weight = 0;
        fs::write(
            &path,
            serde_json::to_vec(&snapshot).expect("serialize fixture"),
        )
        .expect("write worker fixture");
        assert_eq!(
            store.load().expect_err("reject worker").kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(directory).expect("remove exact test directory");
    }

    #[test]
    fn cluster_identity_is_persisted_and_fenced_against_the_expected_cluster() {
        let directory = test_directory("cluster-identity");
        let store = RoutingSnapshotStore::new(directory.join("routing.json"));
        let mut primary = committed(9);
        primary.cluster_id = "inferlab-primary".to_owned();
        store.save(&primary).expect("save primary snapshot");

        let loaded = store.load().expect("load primary snapshot");
        validate_expected_control_cluster(&loaded.committed, "inferlab-primary")
            .expect("accept expected cluster");
        let mismatch = validate_expected_control_cluster(&loaded.committed, "inferlab-foreign")
            .expect_err("reject foreign expectation");

        assert_eq!(loaded.committed.cluster_id, "inferlab-primary");
        assert!(mismatch.to_string().contains("identity mismatch"));
        fs::remove_dir_all(directory).expect("remove exact test directory");
    }

    #[test]
    fn rejects_invalid_cluster_identity_syntax() {
        let mut invalid = committed(9);
        invalid.cluster_id = "contains spaces".to_owned();

        let error = validate_committed(&invalid).expect_err("reject invalid cluster ID");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("control cluster identity"));
    }

    #[test]
    fn freshness_accepts_the_age_boundary_and_an_unbounded_policy() {
        let bounded = SnapshotFreshnessPolicy {
            maximum_age_ms: Some(5_000),
            maximum_future_skew_ms: 100,
        };
        assert_eq!(
            validate_snapshot_freshness(10_000, 15_000, bounded).expect("accept boundary"),
            SnapshotFreshness {
                observed_age_ms: 5_000,
                expires_at_ms: Some(15_000),
            }
        );

        let unbounded = SnapshotFreshnessPolicy {
            maximum_age_ms: None,
            maximum_future_skew_ms: 100,
        };
        assert_eq!(
            validate_snapshot_freshness(10_000, u64::MAX, unbounded)
                .expect("accept any past age without a maximum"),
            SnapshotFreshness {
                observed_age_ms: u64::MAX - 10_000,
                expires_at_ms: None,
            }
        );
    }

    #[test]
    fn freshness_rejects_expiration_and_excessive_future_skew() {
        let policy = SnapshotFreshnessPolicy {
            maximum_age_ms: Some(5_000),
            maximum_future_skew_ms: 100,
        };
        let expired = validate_snapshot_freshness(10_000, 15_001, policy)
            .expect_err("reject one millisecond past the age boundary");
        assert_eq!(expired.kind(), io::ErrorKind::TimedOut);
        assert!(expired.to_string().contains("expired"));

        assert_eq!(
            validate_snapshot_freshness(15_100, 15_000, policy)
                .expect("accept configured future skew")
                .observed_age_ms,
            0
        );
        let future = validate_snapshot_freshness(15_101, 15_000, policy)
            .expect_err("reject excessive future skew");
        assert_eq!(future.kind(), io::ErrorKind::InvalidData);
        assert!(future.to_string().contains("in the future"));
    }
}

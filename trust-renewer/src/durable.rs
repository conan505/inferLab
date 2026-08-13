use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};
use service_auth::ServiceTrustSnapshot;
use sha2::{Digest, Sha256};

#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize},
};

pub const TRUST_RENEWER_STATE_SCHEMA: &str = "inferlab.trust-renewer-state.v1";
pub const MAX_TRUST_RENEWER_STATE_BYTES: usize = 1024 * 1024;
const MAX_TEMP_ATTEMPTS: usize = 128;
static NEXT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenewalCounters {
    pub attempts: u64,
    pub successful_renewals: u64,
    pub transient_failures: u64,
    pub rejected_states: u64,
    pub late_recoveries: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedRenewal {
    pub generation: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub snapshot_sha256: String,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRenewal {
    pub generation: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub snapshot_sha256: String,
    pub exact_snapshot_json: String,
    pub late_recovery: bool,
}

impl fmt::Debug for PendingRenewal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingRenewal")
            .field("generation", &self.generation)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("snapshot_sha256", &self.snapshot_sha256)
            .field("exact_snapshot_json", &"<redacted>")
            .field("late_recovery", &self.late_recovery)
            .finish()
    }
}

impl PendingRenewal {
    pub fn from_snapshot(
        snapshot: &ServiceTrustSnapshot,
        late_recovery: bool,
    ) -> Result<Self, DurableStateError> {
        let exact_bytes =
            serde_json::to_vec(snapshot).map_err(|_| DurableStateError::InvalidPendingSnapshot)?;
        if exact_bytes.len() > MAX_TRUST_RENEWER_STATE_BYTES {
            return Err(DurableStateError::StateTooLarge);
        }
        let exact_snapshot_json = String::from_utf8(exact_bytes)
            .map_err(|_| DurableStateError::InvalidPendingSnapshot)?;
        let expires_at_ms = snapshot
            .policy
            .expires_at_ms
            .ok_or(DurableStateError::InvalidPendingSnapshot)?;
        let pending = Self {
            generation: snapshot.policy.generation,
            issued_at_ms: snapshot.policy.issued_at_ms,
            expires_at_ms,
            snapshot_sha256: snapshot_sha256(exact_snapshot_json.as_bytes()),
            exact_snapshot_json,
            late_recovery,
        };
        pending.validate()?;
        Ok(pending)
    }

    pub fn snapshot(&self) -> Result<ServiceTrustSnapshot, DurableStateError> {
        self.validate()?;
        serde_json::from_str(&self.exact_snapshot_json)
            .map_err(|_| DurableStateError::InvalidPendingSnapshot)
    }

    #[must_use]
    pub fn exact_bytes(&self) -> &[u8] {
        self.exact_snapshot_json.as_bytes()
    }

    fn validate(&self) -> Result<(), DurableStateError> {
        if self.generation == 0
            || self.issued_at_ms == 0
            || self.expires_at_ms <= self.issued_at_ms
            || !valid_sha256(&self.snapshot_sha256)
            || self.exact_snapshot_json.len() > MAX_TRUST_RENEWER_STATE_BYTES
            || snapshot_sha256(self.exact_snapshot_json.as_bytes()) != self.snapshot_sha256
        {
            return Err(DurableStateError::InvalidPendingSnapshot);
        }
        let snapshot = serde_json::from_str::<ServiceTrustSnapshot>(&self.exact_snapshot_json)
            .map_err(|_| DurableStateError::InvalidPendingSnapshot)?;
        let canonical =
            serde_json::to_vec(&snapshot).map_err(|_| DurableStateError::InvalidPendingSnapshot)?;
        if canonical != self.exact_snapshot_json.as_bytes()
            || snapshot.policy.generation != self.generation
            || snapshot.policy.issued_at_ms != self.issued_at_ms
            || snapshot.policy.expires_at_ms != Some(self.expires_at_ms)
        {
            return Err(DurableStateError::InvalidPendingSnapshot);
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurableRenewalState {
    pub schema: String,
    pub authority_fingerprint: String,
    pub template_fingerprint: String,
    pub committed: Option<CommittedRenewal>,
    pub pending: Option<PendingRenewal>,
    pub counters: RenewalCounters,
}

impl fmt::Debug for DurableRenewalState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableRenewalState")
            .field("schema", &self.schema)
            .field("authority_fingerprint", &self.authority_fingerprint)
            .field("template_fingerprint", &self.template_fingerprint)
            .field("committed", &self.committed)
            .field("pending", &self.pending)
            .field("counters", &self.counters)
            .finish()
    }
}

impl DurableRenewalState {
    #[must_use]
    pub fn empty(authority_fingerprint: String, template_fingerprint: String) -> Self {
        Self {
            schema: TRUST_RENEWER_STATE_SCHEMA.to_owned(),
            authority_fingerprint,
            template_fingerprint,
            committed: None,
            pending: None,
            counters: RenewalCounters::default(),
        }
    }

    pub fn validate(
        &self,
        expected_authority_fingerprint: &str,
        expected_template_fingerprint: &str,
    ) -> Result<(), DurableStateError> {
        if self.schema != TRUST_RENEWER_STATE_SCHEMA {
            return Err(DurableStateError::InvalidSchema);
        }
        if self.authority_fingerprint != expected_authority_fingerprint
            || self.template_fingerprint != expected_template_fingerprint
        {
            return Err(DurableStateError::AuthorityMismatch);
        }
        if !valid_sha256(&self.authority_fingerprint) || !valid_sha256(&self.template_fingerprint) {
            return Err(DurableStateError::InvalidFingerprint);
        }
        if self.committed.as_ref().is_some_and(|committed| {
            committed.generation == 0
                || committed.issued_at_ms == 0
                || committed.expires_at_ms <= committed.issued_at_ms
                || !valid_sha256(&committed.snapshot_sha256)
        }) {
            return Err(DurableStateError::InvalidCommittedSnapshot);
        }
        if let Some(pending) = &self.pending {
            pending.validate()?;
            if let Some(committed) = &self.committed {
                match committed.generation.checked_add(1) {
                    Some(expected) if pending.generation == expected => {}
                    _ => return Err(DurableStateError::InvalidPendingSnapshot),
                }
            } else if pending.generation != 1 {
                return Err(DurableStateError::InvalidPendingSnapshot);
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct DurableStateStore {
    path: PathBuf,
    #[cfg(test)]
    test_control: Arc<DurableStateStoreTestControl>,
}

#[cfg(test)]
#[derive(Default)]
struct DurableStateStoreTestControl {
    inject_parent_sync_uncertainty_once: AtomicBool,
    persist_calls: AtomicUsize,
    replacements: AtomicUsize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceObservation {
    pub persist_calls: usize,
    pub replacements: usize,
}

impl fmt::Debug for DurableStateStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableStateStore")
            .field("path", &"<redacted>")
            .finish()
    }
}

impl DurableStateStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(test)]
            test_control: Arc::new(DurableStateStoreTestControl::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_parent_sync_uncertainty_once(&self) {
        self.test_control
            .inject_parent_sync_uncertainty_once
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn persistence_observation(&self) -> PersistenceObservation {
        PersistenceObservation {
            persist_calls: self.test_control.persist_calls.load(Ordering::SeqCst),
            replacements: self.test_control.replacements.load(Ordering::SeqCst),
        }
    }

    pub fn load_or_initialize(
        &self,
        authority_fingerprint: &str,
        template_fingerprint: &str,
    ) -> Result<DurableRenewalState, DurableStateError> {
        match self.load() {
            Ok(state) => {
                state.validate(authority_fingerprint, template_fingerprint)?;
                Ok(state)
            }
            Err(DurableStateError::Missing) => {
                let state = DurableRenewalState::empty(
                    authority_fingerprint.to_owned(),
                    template_fingerprint.to_owned(),
                );
                self.persist(&state)?;
                Ok(state)
            }
            Err(error) => Err(error),
        }
    }

    pub fn load(&self) -> Result<DurableRenewalState, DurableStateError> {
        let parent = self.path.parent().ok_or(DurableStateError::InvalidPath)?;
        validate_parent_directory(parent)?;
        let mut file = open_state_file(&self.path)?;
        let metadata = file
            .metadata()
            .map_err(|_| DurableStateError::UnsafeOrUnavailable)?;
        validate_state_metadata(&metadata)?;
        if metadata.len() > MAX_TRUST_RENEWER_STATE_BYTES as u64 {
            return Err(DurableStateError::StateTooLarge);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take((MAX_TRUST_RENEWER_STATE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| DurableStateError::UnsafeOrUnavailable)?;
        if bytes.is_empty() || bytes.len() > MAX_TRUST_RENEWER_STATE_BYTES {
            return Err(if bytes.is_empty() {
                DurableStateError::InvalidJson
            } else {
                DurableStateError::StateTooLarge
            });
        }
        let after = file
            .metadata()
            .map_err(|_| DurableStateError::UnsafeOrUnavailable)?;
        validate_state_metadata(&after)?;
        if !same_file(&metadata, &after) || after.len() != bytes.len() as u64 {
            return Err(DurableStateError::ChangedDuringRead);
        }
        serde_json::from_slice(&bytes).map_err(|_| DurableStateError::InvalidJson)
    }

    pub fn persist(&self, state: &DurableRenewalState) -> Result<(), DurableStateError> {
        #[cfg(test)]
        self.test_control
            .persist_calls
            .fetch_add(1, Ordering::SeqCst);
        let bytes = serde_json::to_vec(state).map_err(|_| DurableStateError::InvalidJson)?;
        if bytes.len() > MAX_TRUST_RENEWER_STATE_BYTES {
            return Err(DurableStateError::StateTooLarge);
        }
        let parent = self.path.parent().ok_or(DurableStateError::InvalidPath)?;
        validate_parent_directory(parent)?;
        let file_name = self
            .path
            .file_name()
            .ok_or(DurableStateError::InvalidPath)?
            .to_string_lossy();
        let mut temp_path = None;
        let mut temp_file = None;
        for _ in 0..MAX_TEMP_ATTEMPTS {
            let sequence = NEXT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".{file_name}.tmp.{}.{sequence}",
                std::process::id()
            ));
            match create_temp_file(&candidate) {
                Ok(file) => {
                    temp_path = Some(candidate);
                    temp_file = Some(file);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(DurableStateError::PersistenceFailed),
            }
        }
        let temp_path = temp_path.ok_or(DurableStateError::PersistenceFailed)?;
        let mut temp_file = temp_file.ok_or(DurableStateError::PersistenceFailed)?;
        let before_rename = (|| {
            temp_file
                .write_all(&bytes)
                .map_err(|_| DurableStateError::PersistenceFailed)?;
            temp_file
                .sync_all()
                .map_err(|_| DurableStateError::PersistenceFailed)?;
            validate_state_metadata(
                &temp_file
                    .metadata()
                    .map_err(|_| DurableStateError::PersistenceFailed)?,
            )?;
            fs::rename(&temp_path, &self.path).map_err(|_| DurableStateError::PersistenceFailed)?;
            Ok(())
        })();
        if let Err(error) = before_rename {
            drop(temp_file);
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        drop(temp_file);
        #[cfg(test)]
        {
            self.test_control
                .replacements
                .fetch_add(1, Ordering::SeqCst);
            if self
                .test_control
                .inject_parent_sync_uncertainty_once
                .swap(false, Ordering::SeqCst)
            {
                return Err(DurableStateError::DurabilityUncertain);
            }
        }
        sync_parent_directory(parent).map_err(|_| DurableStateError::DurabilityUncertain)
    }
}

#[cfg(unix)]
fn open_state_file(path: &Path) -> Result<File, DurableStateError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => DurableStateError::Missing,
            _ => DurableStateError::UnsafeOrUnavailable,
        })
}

#[cfg(not(unix))]
fn open_state_file(_path: &Path) -> Result<File, DurableStateError> {
    Err(DurableStateError::UnsupportedPlatform)
}

#[cfg(unix)]
fn create_temp_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn create_temp_file(_path: &Path) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "durable state requires Unix file custody",
    ))
}

#[cfg(unix)]
fn validate_state_metadata(metadata: &fs::Metadata) -> Result<(), DurableStateError> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.is_file()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(DurableStateError::UnsafeOrUnavailable);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_state_metadata(_metadata: &fs::Metadata) -> Result<(), DurableStateError> {
    Err(DurableStateError::UnsupportedPlatform)
}

#[cfg(unix)]
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev() && before.ino() == after.ino()
}

#[cfg(not(unix))]
fn same_file(_before: &fs::Metadata, _after: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn validate_parent_directory(path: &Path) -> Result<(), DurableStateError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path).map_err(|_| DurableStateError::InvalidPath)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(DurableStateError::UnsafeParentDirectory);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_parent_directory(_path: &Path) -> Result<(), DurableStateError> {
    Err(DurableStateError::UnsupportedPlatform)
}

fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[must_use]
pub fn snapshot_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableStateError {
    Missing,
    InvalidPath,
    UnsupportedPlatform,
    UnsafeParentDirectory,
    UnsafeOrUnavailable,
    ChangedDuringRead,
    StateTooLarge,
    InvalidJson,
    InvalidSchema,
    InvalidFingerprint,
    AuthorityMismatch,
    InvalidCommittedSnapshot,
    InvalidPendingSnapshot,
    PersistenceFailed,
    DurabilityUncertain,
}

impl DurableStateError {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Missing => "state_missing",
            Self::InvalidPath => "invalid_state_path",
            Self::UnsupportedPlatform => "unsupported_state_platform",
            Self::UnsafeParentDirectory => "unsafe_state_directory",
            Self::UnsafeOrUnavailable => "unsafe_or_unavailable_state",
            Self::ChangedDuringRead => "state_changed_during_read",
            Self::StateTooLarge => "state_too_large",
            Self::InvalidJson => "invalid_state_json",
            Self::InvalidSchema => "invalid_state_schema",
            Self::InvalidFingerprint => "invalid_state_fingerprint",
            Self::AuthorityMismatch => "state_authority_mismatch",
            Self::InvalidCommittedSnapshot => "invalid_committed_state",
            Self::InvalidPendingSnapshot => "invalid_pending_state",
            Self::PersistenceFailed => "state_persistence_failed",
            Self::DurabilityUncertain => "state_durability_uncertain",
        }
    }
}

impl fmt::Display for DurableStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.kind())
    }
}

impl std::error::Error for DurableStateError {}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };

    use service_auth::{
        SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2, SERVICE_TRUST_POLICY_SCHEMA_V2,
        ServiceTrustPolicyPayload, ServiceTrustSnapshotAuthentication,
    };

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory() -> PathBuf {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "inferlab-trust-renewer-state-{}-{sequence}",
            std::process::id()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("test directory");
        path
    }

    fn snapshot() -> ServiceTrustSnapshot {
        ServiceTrustSnapshot {
            policy: ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA_V2.to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                generation: 1,
                issued_at_ms: 100,
                expires_at_ms: Some(1_100),
                trusted_credentials: vec![],
                revoked_service_ids: vec![],
                revoked_credentials: vec![],
                gateway_service_ids: vec![],
            },
            authentication: ServiceTrustSnapshotAuthentication {
                schema: SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2.to_owned(),
                algorithm: "ed25519".to_owned(),
                key_id: "root-a".to_owned(),
                signature: "fixture-signature".to_owned(),
            },
        }
    }

    #[test]
    fn initializes_and_round_trips_strict_state() {
        let directory = test_directory();
        let path = directory.join("state.json");
        let store = DurableStateStore::new(path.clone());
        let authority = "a".repeat(64);
        let template = "b".repeat(64);
        let state = store
            .load_or_initialize(&authority, &template)
            .expect("initialize");
        assert_eq!(state.schema, TRUST_RENEWER_STATE_SCHEMA);
        assert_eq!(store.load().expect("load"), state);
        assert_eq!(
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn rejects_unsafe_state_and_symlink() {
        let directory = test_directory();
        let path = directory.join("state.json");
        fs::write(&path, b"{}").expect("fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            DurableStateStore::new(path.clone()).load().unwrap_err(),
            DurableStateError::UnsafeOrUnavailable
        );
        fs::remove_file(&path).expect("remove fixture");
        let target = directory.join("target.json");
        fs::write(&target, b"{}").expect("target");
        symlink(&target, &path).expect("symlink");
        assert_eq!(
            DurableStateStore::new(path).load().unwrap_err(),
            DurableStateError::UnsafeOrUnavailable
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn pending_requires_exact_canonical_bytes() {
        let mut pending = PendingRenewal::from_snapshot(&snapshot(), false).expect("pending");
        assert_eq!(pending.snapshot().expect("snapshot"), snapshot());
        let debug = format!("{pending:?}");
        for forbidden in [
            pending.exact_snapshot_json.as_str(),
            "fixture-signature",
            "trusted_credentials",
        ] {
            assert!(!debug.contains(forbidden));
        }
        pending.exact_snapshot_json.push(' ');
        assert_eq!(
            pending.validate().unwrap_err(),
            DurableStateError::InvalidPendingSnapshot
        );
    }

    #[test]
    fn rejects_generation_exhaustion_between_committed_and_pending() {
        let mut state = DurableRenewalState::empty("a".repeat(64), "b".repeat(64));
        state.committed = Some(CommittedRenewal {
            generation: u64::MAX,
            issued_at_ms: 1,
            expires_at_ms: 2,
            snapshot_sha256: "c".repeat(64),
        });
        state.pending = Some(PendingRenewal::from_snapshot(&snapshot(), false).expect("pending"));
        assert_eq!(
            state.validate(&"a".repeat(64), &"b".repeat(64)),
            Err(DurableStateError::InvalidPendingSnapshot)
        );
    }

    #[test]
    fn store_debug_redacts_state_path() {
        let path = PathBuf::from("/secret/renewal/state.json");
        let store = DurableStateStore::new(path.clone());
        assert!(!format!("{store:?}").contains(path.to_string_lossy().as_ref()));
    }
}

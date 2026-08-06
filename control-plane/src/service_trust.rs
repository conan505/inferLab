use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use service_auth::{
    ServiceTrustSnapshot, TrustedServiceTrustRootKeyRing, VerifiedServiceTrustSnapshot,
};
use tokio::time;
use tracing::{info, warn};

use crate::ServiceAuthorizer;

pub const SERVICE_TRUST_FLOOR_SCHEMA: &str = "inferlab.service-trust-floor.v1";
const MAX_SNAPSHOT_BYTES: u64 = 262_144;
const MAX_FLOOR_BYTES: u64 = 16_384;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PersistedServiceTrustFloor {
    pub schema: String,
    pub cluster_id: String,
    pub generation: u64,
    pub signing_key_id: String,
    pub signature: String,
}

impl PersistedServiceTrustFloor {
    fn from_verified(snapshot: &VerifiedServiceTrustSnapshot) -> Self {
        Self {
            schema: SERVICE_TRUST_FLOOR_SCHEMA.to_owned(),
            cluster_id: snapshot.policy.cluster_id.clone(),
            generation: snapshot.policy.generation,
            signing_key_id: snapshot.signing_key_id.clone(),
            signature: snapshot.signature.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServiceTrustFloorStore {
    path: PathBuf,
}

impl ServiceTrustFloorStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<Option<PersistedServiceTrustFloor>> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.len() > MAX_FLOOR_BYTES {
            return Err(invalid_data(format!(
                "service-trust floor {} is {} bytes; maximum is {MAX_FLOOR_BYTES}",
                self.path.display(),
                metadata.len()
            )));
        }
        let bytes = fs::read(&self.path)?;
        let floor =
            serde_json::from_slice::<PersistedServiceTrustFloor>(&bytes).map_err(|error| {
                invalid_data(format!(
                    "cannot decode service-trust floor {}: {error}",
                    self.path.display()
                ))
            })?;
        validate_floor(&floor)?;
        Ok(Some(floor))
    }

    pub fn save(&self, floor: &PersistedServiceTrustFloor) -> io::Result<()> {
        validate_floor(floor)?;
        let bytes = serde_json::to_vec_pretty(floor)
            .map_err(|error| io::Error::other(format!("serialize service-trust floor: {error}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FLOOR_BYTES {
            return Err(invalid_data(format!(
                "serialized service-trust floor exceeds {MAX_FLOOR_BYTES} bytes"
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
            .ok_or_else(|| invalid_data("service-trust floor path needs a UTF-8 file name"))?;
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
        write_result
    }
}

pub struct SignedServiceTrustBootstrap {
    pub authorizer: ServiceAuthorizer,
    pub watcher: ServiceTrustWatcher,
}

#[derive(Debug)]
pub struct ServiceTrustWatcher {
    snapshot_path: PathBuf,
    floor_store: ServiceTrustFloorStore,
    cluster_id: String,
    roots: TrustedServiceTrustRootKeyRing,
    local_service_credential: String,
    poll_interval: Duration,
    accepted_floor: PersistedServiceTrustFloor,
    last_observed_bytes: Vec<u8>,
    last_source_error: Option<String>,
}

impl ServiceTrustWatcher {
    pub async fn run(mut self, authorizer: Arc<ServiceAuthorizer>) {
        let mut interval = time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match self.reload_once(&authorizer) {
                Ok(Some(generation)) => {
                    info!(
                        generation,
                        snapshot_path = %self.snapshot_path.display(),
                        floor_path = %self.floor_store.path().display(),
                        "applied signed service-trust snapshot"
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    let message = error.to_string();
                    authorizer.record_trust_policy_rejection(message.clone());
                    warn!(
                        error = %message,
                        snapshot_path = %self.snapshot_path.display(),
                        "rejected service-trust snapshot; retaining last known good policy"
                    );
                }
            }
        }
    }

    fn reload_once(&mut self, authorizer: &ServiceAuthorizer) -> io::Result<Option<u64>> {
        let bytes = match read_bounded(&self.snapshot_path, MAX_SNAPSHOT_BYTES) {
            Ok(bytes) => {
                self.last_source_error = None;
                bytes
            }
            Err(error) => {
                let message = error.to_string();
                if self.last_source_error.as_deref() == Some(&message) {
                    return Ok(None);
                }
                self.last_source_error = Some(message);
                return Err(error);
            }
        };
        if bytes == self.last_observed_bytes {
            return Ok(None);
        }
        self.last_observed_bytes = bytes.clone();
        let verified = decode_and_verify(&bytes, &self.cluster_id, &self.roots)?;
        validate_local_credential(&verified, &self.local_service_credential)?;
        validate_candidate_floor(&verified, Some(&self.accepted_floor))?;
        if verified.policy.generation == self.accepted_floor.generation {
            return Ok(None);
        }
        let next_floor = PersistedServiceTrustFloor::from_verified(&verified);
        self.floor_store.save(&next_floor)?;
        let generation = verified.policy.generation;
        if !authorizer
            .apply_signed_snapshot(verified, now_ms())
            .map_err(invalid_data)?
        {
            return Err(invalid_data(format!(
                "service-trust generation {generation} was not newer than the active policy"
            )));
        }
        self.accepted_floor = next_floor;
        Ok(Some(generation))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn bootstrap_signed_service_trust(
    snapshot_path: PathBuf,
    floor_path: PathBuf,
    cluster_id: String,
    roots: TrustedServiceTrustRootKeyRing,
    local_service_credential: String,
    poll_interval: Duration,
    max_age_ms: u64,
    max_future_skew_ms: u64,
) -> io::Result<SignedServiceTrustBootstrap> {
    if poll_interval.is_zero() {
        return Err(invalid_data(
            "service-trust snapshot poll interval must be positive",
        ));
    }
    let bytes = read_bounded(&snapshot_path, MAX_SNAPSHOT_BYTES)?;
    let verified = decode_and_verify(&bytes, &cluster_id, &roots)?;
    validate_local_credential(&verified, &local_service_credential)?;
    let floor_store = ServiceTrustFloorStore::new(floor_path);
    let prior_floor = floor_store.load()?;
    validate_candidate_floor(&verified, prior_floor.as_ref())?;
    let accepted_floor = PersistedServiceTrustFloor::from_verified(&verified);
    if prior_floor.as_ref() != Some(&accepted_floor) {
        floor_store.save(&accepted_floor)?;
    }
    let authorizer = ServiceAuthorizer::required_from_signed_snapshot(
        verified,
        roots.trusted_key_ids(),
        roots.revoked_key_ids(),
        max_age_ms,
        max_future_skew_ms,
        now_ms(),
    )
    .map_err(invalid_data)?;
    Ok(SignedServiceTrustBootstrap {
        authorizer,
        watcher: ServiceTrustWatcher {
            snapshot_path,
            floor_store,
            cluster_id,
            roots,
            local_service_credential,
            poll_interval,
            accepted_floor,
            last_observed_bytes: bytes,
            last_source_error: None,
        },
    })
}

fn validate_local_credential(
    snapshot: &VerifiedServiceTrustSnapshot,
    local_service_credential: &str,
) -> io::Result<()> {
    if !snapshot
        .compiled
        .keys
        .trusted_service_credentials()
        .iter()
        .any(|credential| credential == local_service_credential)
    {
        return Err(invalid_data(format!(
            "service-trust policy generation {} does not trust local signing credential '{local_service_credential}'",
            snapshot.policy.generation
        )));
    }
    if snapshot
        .compiled
        .keys
        .revoked_service_credentials()
        .iter()
        .any(|credential| credential == local_service_credential)
    {
        return Err(invalid_data(format!(
            "service-trust policy generation {} revokes local signing credential '{local_service_credential}'",
            snapshot.policy.generation
        )));
    }
    let local_service_id = local_service_credential
        .split_once('/')
        .map_or(local_service_credential, |(service_id, _)| service_id);
    if snapshot
        .compiled
        .keys
        .revoked_service_ids()
        .iter()
        .any(|service_id| service_id == local_service_id)
    {
        return Err(invalid_data(format!(
            "service-trust policy generation {} revokes local service identity '{local_service_id}'",
            snapshot.policy.generation
        )));
    }
    Ok(())
}

fn decode_and_verify(
    bytes: &[u8],
    expected_cluster_id: &str,
    roots: &TrustedServiceTrustRootKeyRing,
) -> io::Result<VerifiedServiceTrustSnapshot> {
    let snapshot = serde_json::from_slice::<ServiceTrustSnapshot>(bytes)
        .map_err(|error| invalid_data(format!("cannot decode service-trust snapshot: {error}")))?;
    if snapshot.policy.cluster_id != expected_cluster_id {
        return Err(invalid_data(format!(
            "service-trust cluster mismatch: expected '{expected_cluster_id}', observed '{}'",
            snapshot.policy.cluster_id
        )));
    }
    roots.verify(&snapshot).map_err(invalid_data)
}

fn validate_candidate_floor(
    snapshot: &VerifiedServiceTrustSnapshot,
    floor: Option<&PersistedServiceTrustFloor>,
) -> io::Result<()> {
    let Some(floor) = floor else {
        return Ok(());
    };
    if snapshot.policy.cluster_id != floor.cluster_id {
        return Err(invalid_data(format!(
            "service-trust floor cluster '{}' does not match snapshot cluster '{}'",
            floor.cluster_id, snapshot.policy.cluster_id
        )));
    }
    if snapshot.policy.generation < floor.generation {
        return Err(invalid_data(format!(
            "service-trust rollback rejected: snapshot generation {} is below durable floor {}",
            snapshot.policy.generation, floor.generation
        )));
    }
    if snapshot.policy.generation == floor.generation
        && (snapshot.signing_key_id != floor.signing_key_id
            || snapshot.signature != floor.signature)
    {
        return Err(invalid_data(format!(
            "service-trust generation {} conflicts with the durable accepted snapshot",
            snapshot.policy.generation
        )));
    }
    Ok(())
}

fn validate_floor(floor: &PersistedServiceTrustFloor) -> io::Result<()> {
    if floor.schema != SERVICE_TRUST_FLOOR_SCHEMA {
        return Err(invalid_data(format!(
            "unsupported service-trust floor schema '{}'; expected '{SERVICE_TRUST_FLOOR_SCHEMA}'",
            floor.schema
        )));
    }
    if floor.cluster_id.trim().is_empty()
        || floor.generation == 0
        || floor.signing_key_id.trim().is_empty()
        || floor.signature.trim().is_empty()
    {
        return Err(invalid_data(
            "service-trust floor cluster, generation, key ID, and signature must be present",
        ));
    }
    Ok(())
}

fn read_bounded(path: &Path, maximum: u64) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > maximum {
        return Err(invalid_data(format!(
            "service-trust snapshot {} is {} bytes; maximum is {maximum}",
            path.display(),
            metadata.len()
        )));
    }
    fs::read(path)
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
    use service_auth::{
        SERVICE_TRUST_POLICY_SCHEMA, ServiceSigningIdentity, ServiceTrustCredential,
        ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity,
    };

    const ROOT_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const SERVICE_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";

    fn signed(generation: u64) -> (ServiceTrustSnapshot, TrustedServiceTrustRootKeyRing) {
        let root = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("root");
        let service = ServiceSigningIdentity::from_base64_seed_with_credential(
            "gateway-primary",
            "key-a",
            SERVICE_SEED,
        )
        .expect("service");
        let policy = ServiceTrustPolicyPayload {
            schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
            cluster_id: "inferlab-primary".to_owned(),
            generation,
            issued_at_ms: 1_700_000_000_000 + generation,
            trusted_credentials: vec![ServiceTrustCredential {
                service_id: "gateway-primary".to_owned(),
                credential_id: "key-a".to_owned(),
                public_key_base64: service.public_key_base64(),
            }],
            revoked_service_ids: Vec::new(),
            revoked_credentials: Vec::new(),
            gateway_service_ids: vec!["gateway-primary".to_owned()],
        };
        let snapshot = root.sign(&policy).expect("snapshot");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", root.public_key_base64()),
            "",
        )
        .expect("roots");
        (snapshot, roots)
    }

    #[test]
    fn durable_floor_rejects_rollback_and_same_generation_fork() {
        let (generation_two, roots) = signed(2);
        let verified_two = roots.verify(&generation_two).expect("verified two");
        let floor = PersistedServiceTrustFloor::from_verified(&verified_two);
        let (generation_one, _) = signed(1);
        let verified_one = roots.verify(&generation_one).expect("verified one");
        assert!(
            validate_candidate_floor(&verified_one, Some(&floor))
                .expect_err("rollback")
                .to_string()
                .contains("rollback")
        );

        let mut forked_policy = generation_two.policy.clone();
        forked_policy.issued_at_ms += 1;
        let root = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("root");
        let forked = root.sign(&forked_policy).expect("forked");
        let verified_fork = roots.verify(&forked).expect("verified fork");
        assert!(
            validate_candidate_floor(&verified_fork, Some(&floor))
                .expect_err("fork")
                .to_string()
                .contains("conflicts")
        );
    }
}

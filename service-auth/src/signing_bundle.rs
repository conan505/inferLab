use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    io::Read,
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::SigningKey;
use serde::Deserialize;

use crate::{
    AuthenticationError, ServiceAuthentication, ServiceRequestPayload, ServiceSigningIdentity,
    ServiceTrustApplicationReceipt, VerifiedServiceTrustSnapshot, authenticate_with_signing_key,
    decode_exact, next_service_nonce, now_ms, trust_receipt::sign_trust_receipt_with_key,
    validate_id,
};

pub const SERVICE_SIGNING_BUNDLE_SCHEMA: &str = "inferlab.service-signing-bundle.v1";
pub const MAX_SERVICE_SIGNING_BUNDLE_BYTES: usize = 16 * 1024;
const MAX_SERVICE_SIGNING_CREDENTIALS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceSigningErrorKind {
    SourceUnavailable,
    NotRegularFile,
    UnsafePermissions,
    BundleTooLarge,
    InvalidJson,
    InvalidSchema,
    InvalidClusterId,
    InvalidServiceId,
    InvalidGeneration,
    InvalidCredentialSet,
    InvalidPrivateKey,
    UnknownActiveCredential,
    StaticSigner,
    ClusterMismatch,
    ServiceMismatch,
    StaleGeneration,
    GenerationFork,
    CandidateRejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSigningError {
    kind: ServiceSigningErrorKind,
    message: &'static str,
}

impl ServiceSigningError {
    const fn new(kind: ServiceSigningErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub const fn kind(&self) -> ServiceSigningErrorKind {
        self.kind
    }

    pub const fn candidate_rejected() -> Self {
        Self::new(
            ServiceSigningErrorKind::CandidateRejected,
            "service signing bundle candidate was rejected by local policy",
        )
    }
}

impl fmt::Display for ServiceSigningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ServiceSigningError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedServiceSigningCredential {
    credential_id: String,
    private_key_base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedServiceSigningBundle {
    schema: String,
    cluster_id: String,
    generation: u64,
    service_id: String,
    active_credential_id: String,
    credentials: Vec<EncodedServiceSigningCredential>,
}

struct ServiceSigningCredential {
    credential_id: String,
    signing_key: SigningKey,
}

impl fmt::Debug for ServiceSigningCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceSigningCredential")
            .field("credential_id", &self.credential_id)
            .finish_non_exhaustive()
    }
}

pub struct VerifiedServiceSigningBundle {
    cluster_id: String,
    generation: u64,
    service_id: String,
    active_credential_id: String,
    credentials: BTreeMap<String, Arc<ServiceSigningCredential>>,
}

impl fmt::Debug for VerifiedServiceSigningBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedServiceSigningBundle")
            .field("cluster_id", &self.cluster_id)
            .field("generation", &self.generation)
            .field("service_id", &self.service_id)
            .field("active_credential_id", &self.active_credential_id)
            .field("configured_credential_count", &self.credentials.len())
            .finish_non_exhaustive()
    }
}

impl VerifiedServiceSigningBundle {
    pub fn load(
        path: impl AsRef<Path>,
        expected_cluster_id: &str,
        expected_service_id: &str,
    ) -> Result<Self, ServiceSigningError> {
        let bytes = read_bundle_file(path.as_ref())?;
        Self::decode(&bytes, expected_cluster_id, expected_service_id)
    }

    pub fn decode(
        bytes: &[u8],
        expected_cluster_id: &str,
        expected_service_id: &str,
    ) -> Result<Self, ServiceSigningError> {
        if bytes.len() > MAX_SERVICE_SIGNING_BUNDLE_BYTES {
            return Err(ServiceSigningError::new(
                ServiceSigningErrorKind::BundleTooLarge,
                "service signing bundle exceeds the byte limit",
            ));
        }
        validate_id(expected_cluster_id, "cluster ID").map_err(|_| {
            ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidClusterId,
                "expected service signing cluster ID is invalid",
            )
        })?;
        validate_id(expected_service_id, "service ID").map_err(|_| {
            ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidServiceId,
                "expected service signing service ID is invalid",
            )
        })?;
        let encoded: EncodedServiceSigningBundle = serde_json::from_slice(bytes).map_err(|_| {
            ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidJson,
                "service signing bundle is not exact valid JSON",
            )
        })?;
        if encoded.schema != SERVICE_SIGNING_BUNDLE_SCHEMA {
            return Err(ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidSchema,
                "service signing bundle schema is unsupported",
            ));
        }
        validate_id(&encoded.cluster_id, "cluster ID").map_err(|_| {
            ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidClusterId,
                "service signing bundle cluster ID is invalid",
            )
        })?;
        if encoded.cluster_id != expected_cluster_id {
            return Err(ServiceSigningError::new(
                ServiceSigningErrorKind::ClusterMismatch,
                "service signing bundle cluster ID does not match this process",
            ));
        }
        validate_id(&encoded.service_id, "service ID").map_err(|_| {
            ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidServiceId,
                "service signing bundle service ID is invalid",
            )
        })?;
        if encoded.service_id != expected_service_id {
            return Err(ServiceSigningError::new(
                ServiceSigningErrorKind::ServiceMismatch,
                "service signing bundle service ID does not match this process",
            ));
        }
        if encoded.generation == 0 {
            return Err(ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidGeneration,
                "service signing bundle generation must be positive",
            ));
        }
        validate_id(&encoded.active_credential_id, "credential ID").map_err(|_| {
            ServiceSigningError::new(
                ServiceSigningErrorKind::UnknownActiveCredential,
                "service signing bundle active credential ID is invalid",
            )
        })?;
        if !(1..=MAX_SERVICE_SIGNING_CREDENTIALS).contains(&encoded.credentials.len()) {
            return Err(ServiceSigningError::new(
                ServiceSigningErrorKind::InvalidCredentialSet,
                "service signing bundle must contain 1 to 16 credentials",
            ));
        }

        let mut credentials = BTreeMap::new();
        let mut public_keys = BTreeSet::new();
        for encoded_credential in encoded.credentials {
            validate_id(&encoded_credential.credential_id, "credential ID").map_err(|_| {
                ServiceSigningError::new(
                    ServiceSigningErrorKind::InvalidCredentialSet,
                    "service signing bundle contains an invalid credential ID",
                )
            })?;
            let seed = decode_exact::<32>(
                &encoded_credential.private_key_base64,
                "Ed25519 private seed",
            )
            .map_err(|_| {
                ServiceSigningError::new(
                    ServiceSigningErrorKind::InvalidPrivateKey,
                    "service signing bundle contains an invalid Ed25519 private seed",
                )
            })?;
            let signing_key = SigningKey::from_bytes(&seed);
            let public_key = signing_key.verifying_key().to_bytes();
            if !public_keys.insert(public_key)
                || credentials
                    .insert(
                        encoded_credential.credential_id.clone(),
                        Arc::new(ServiceSigningCredential {
                            credential_id: encoded_credential.credential_id,
                            signing_key,
                        }),
                    )
                    .is_some()
            {
                return Err(ServiceSigningError::new(
                    ServiceSigningErrorKind::InvalidCredentialSet,
                    "service signing bundle credential IDs and public keys must be unique",
                ));
            }
        }
        if !credentials.contains_key(&encoded.active_credential_id) {
            return Err(ServiceSigningError::new(
                ServiceSigningErrorKind::UnknownActiveCredential,
                "service signing bundle active credential is not configured",
            ));
        }
        Ok(Self {
            cluster_id: encoded.cluster_id,
            generation: encoded.generation,
            service_id: encoded.service_id,
            active_credential_id: encoded.active_credential_id,
            credentials,
        })
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn service_id(&self) -> &str {
        &self.service_id
    }

    pub fn active_credential_id(&self) -> &str {
        &self.active_credential_id
    }

    pub fn configured_credential_count(&self) -> usize {
        self.credentials.len()
    }

    fn active_credential(&self) -> Arc<ServiceSigningCredential> {
        Arc::clone(
            self.credentials
                .get(&self.active_credential_id)
                .expect("verified bundle has an active credential"),
        )
    }

    fn semantically_matches(&self, state: &ServiceSignerState) -> bool {
        self.cluster_id == state.cluster_id.as_deref().unwrap_or_default()
            && self.service_id == state.service_id
            && self.active_credential_id == state.active.credential_id
            && self.credentials.len() == state.credentials.len()
            && self.credentials.iter().all(|(credential_id, credential)| {
                state.credentials.get(credential_id).is_some_and(|current| {
                    credential.signing_key.verifying_key().to_bytes()
                        == current.signing_key.verifying_key().to_bytes()
                })
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceSignerMode {
    Static,
    WatchedBundle,
}

impl ServiceSignerMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::WatchedBundle => "watched-bundle",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSignerStatus {
    pub mode: ServiceSignerMode,
    pub service_id: String,
    pub active_credential_id: String,
    pub bundle_generation: Option<u64>,
    pub configured_credential_count: usize,
    pub successful_activations: u64,
    pub rejected_reloads: u64,
    pub last_error_kind: Option<ServiceSigningErrorKind>,
}

struct ServiceSignerState {
    mode: ServiceSignerMode,
    cluster_id: Option<String>,
    generation: Option<u64>,
    service_id: String,
    active: Arc<ServiceSigningCredential>,
    credentials: BTreeMap<String, Arc<ServiceSigningCredential>>,
}

struct ServiceSignerInner {
    state: RwLock<ServiceSignerState>,
    sequence: Arc<AtomicU64>,
    successful_activations: AtomicU64,
    rejected_reloads: AtomicU64,
    last_error: RwLock<Option<ServiceSigningErrorKind>>,
}

#[derive(Clone)]
pub struct ServiceSigner {
    inner: Arc<ServiceSignerInner>,
}

impl fmt::Debug for ServiceSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceSigner")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ServiceSignerSnapshot {
    service_id: String,
    credential: Arc<ServiceSigningCredential>,
    cluster_id: Option<String>,
    bundle_generation: Option<u64>,
    configured_credential_count: usize,
    inner: Arc<ServiceSignerInner>,
}

impl fmt::Debug for ServiceSignerSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceSignerSnapshot")
            .field("service_id", &self.service_id)
            .field("credential_id", &self.credential.credential_id)
            .field("cluster_id", &self.cluster_id)
            .field("bundle_generation", &self.bundle_generation)
            .field(
                "configured_credential_count",
                &self.configured_credential_count,
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceSignerActivationOutcome {
    Activated,
    Unchanged,
}

impl ServiceSigner {
    pub fn from_static(identity: Arc<ServiceSigningIdentity>) -> Self {
        let credential = Arc::new(ServiceSigningCredential {
            credential_id: identity.credential_id.clone(),
            signing_key: identity.signing_key.clone(),
        });
        let mut credentials = BTreeMap::new();
        credentials.insert(identity.credential_id.clone(), Arc::clone(&credential));
        Self {
            inner: Arc::new(ServiceSignerInner {
                state: RwLock::new(ServiceSignerState {
                    mode: ServiceSignerMode::Static,
                    cluster_id: None,
                    generation: None,
                    service_id: identity.service_id.clone(),
                    active: credential,
                    credentials,
                }),
                sequence: Arc::clone(&identity.sequence),
                successful_activations: AtomicU64::new(0),
                rejected_reloads: AtomicU64::new(0),
                last_error: RwLock::new(None),
            }),
        }
    }

    pub fn from_bundle(bundle: VerifiedServiceSigningBundle) -> Self {
        let active = bundle.active_credential();
        Self {
            inner: Arc::new(ServiceSignerInner {
                state: RwLock::new(ServiceSignerState {
                    mode: ServiceSignerMode::WatchedBundle,
                    cluster_id: Some(bundle.cluster_id),
                    generation: Some(bundle.generation),
                    service_id: bundle.service_id,
                    active,
                    credentials: bundle.credentials,
                }),
                sequence: Arc::new(AtomicU64::new(0)),
                successful_activations: AtomicU64::new(0),
                rejected_reloads: AtomicU64::new(0),
                last_error: RwLock::new(None),
            }),
        }
    }

    pub fn snapshot(&self) -> ServiceSignerSnapshot {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshot_from_state(&self.inner, &state)
    }

    pub fn with_current<T>(&self, operation: impl FnOnce(&ServiceSignerSnapshot) -> T) -> T {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let snapshot = snapshot_from_state(&self.inner, &state);
        operation(&snapshot)
    }

    pub fn activate_bundle(
        &self,
        candidate: VerifiedServiceSigningBundle,
        validator: impl FnOnce(&ServiceSignerSnapshot) -> bool,
    ) -> Result<ServiceSignerActivationOutcome, ServiceSigningError> {
        let mut state = self
            .inner
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = (|| {
            if state.mode != ServiceSignerMode::WatchedBundle {
                return Err(ServiceSigningError::new(
                    ServiceSigningErrorKind::StaticSigner,
                    "a static service signer cannot activate a watched bundle",
                ));
            }
            if candidate.cluster_id != state.cluster_id.as_deref().unwrap_or_default() {
                return Err(ServiceSigningError::new(
                    ServiceSigningErrorKind::ClusterMismatch,
                    "service signing bundle cluster ID does not match the active signer",
                ));
            }
            if candidate.service_id != state.service_id {
                return Err(ServiceSigningError::new(
                    ServiceSigningErrorKind::ServiceMismatch,
                    "service signing bundle service ID does not match the active signer",
                ));
            }
            let current_generation = state.generation.expect("watched signer has generation");
            if candidate.generation < current_generation {
                return Err(ServiceSigningError::new(
                    ServiceSigningErrorKind::StaleGeneration,
                    "service signing bundle generation is older than the active generation",
                ));
            }
            if candidate.generation == current_generation {
                if candidate.semantically_matches(&state) {
                    return Ok(ServiceSignerActivationOutcome::Unchanged);
                }
                return Err(ServiceSigningError::new(
                    ServiceSigningErrorKind::GenerationFork,
                    "service signing bundle reuses the active generation with different contents",
                ));
            }
            let candidate_snapshot = ServiceSignerSnapshot {
                service_id: candidate.service_id.clone(),
                credential: candidate.active_credential(),
                cluster_id: Some(candidate.cluster_id.clone()),
                bundle_generation: Some(candidate.generation),
                configured_credential_count: candidate.credentials.len(),
                inner: Arc::clone(&self.inner),
            };
            if !validator(&candidate_snapshot) {
                return Err(ServiceSigningError::candidate_rejected());
            }
            let active = candidate.active_credential();
            state.cluster_id = Some(candidate.cluster_id);
            state.generation = Some(candidate.generation);
            state.service_id = candidate.service_id;
            state.active = active;
            state.credentials = candidate.credentials;
            Ok(ServiceSignerActivationOutcome::Activated)
        })();
        match &result {
            Ok(ServiceSignerActivationOutcome::Activated) => {
                self.inner
                    .successful_activations
                    .fetch_add(1, Ordering::Relaxed);
                *self
                    .inner
                    .last_error
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            Ok(ServiceSignerActivationOutcome::Unchanged) => {
                *self
                    .inner
                    .last_error
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            Err(error) => {
                self.record_rejection(error.kind());
            }
        }
        result
    }

    /// Records a bundle source/decoding rejection that happened before
    /// `activate_bundle`. Callers own byte/error deduplication across polls.
    pub fn record_rejection(&self, kind: ServiceSigningErrorKind) {
        self.inner.rejected_reloads.fetch_add(1, Ordering::Relaxed);
        *self
            .inner
            .last_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(kind);
    }

    pub fn status(&self) -> ServiceSignerStatus {
        let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ServiceSignerStatus {
            mode: state.mode,
            service_id: state.service_id.clone(),
            active_credential_id: state.active.credential_id.clone(),
            bundle_generation: state.generation,
            configured_credential_count: state.credentials.len(),
            successful_activations: self.inner.successful_activations.load(Ordering::Relaxed),
            rejected_reloads: self.inner.rejected_reloads.load(Ordering::Relaxed),
            last_error_kind: *self
                .inner
                .last_error
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }
}

impl ServiceSignerSnapshot {
    pub fn service_id(&self) -> &str {
        &self.service_id
    }

    pub fn credential_id(&self) -> &str {
        &self.credential.credential_id
    }

    pub const fn bundle_generation(&self) -> Option<u64> {
        self.bundle_generation
    }

    pub const fn configured_credential_count(&self) -> usize {
        self.configured_credential_count
    }

    pub fn public_key_base64(&self) -> String {
        STANDARD.encode(self.credential.signing_key.verifying_key().as_bytes())
    }

    pub fn authenticate_now(
        &self,
        method: &str,
        path: &str,
        cluster_id: &str,
        audience_id: &str,
        body: &[u8],
    ) -> Result<ServiceAuthentication, AuthenticationError> {
        if self
            .cluster_id
            .as_deref()
            .is_some_and(|expected| expected != cluster_id)
        {
            return Err(AuthenticationError::new(
                "service signing request cluster does not match the active bundle",
            ));
        }
        let issued_at_ms = now_ms()?;
        self.authenticate_at(method, path, cluster_id, audience_id, body, issued_at_ms)
    }

    pub fn authenticate(
        &self,
        payload: &ServiceRequestPayload<'_>,
    ) -> Result<ServiceAuthentication, AuthenticationError> {
        if self
            .cluster_id
            .as_deref()
            .is_some_and(|expected| expected != payload.cluster_id)
        {
            return Err(AuthenticationError::new(
                "service signing request cluster does not match the active bundle",
            ));
        }
        authenticate_with_signing_key(&self.service_id, &self.credential.signing_key, payload)
    }

    pub fn sign_trust_receipt(
        &self,
        snapshot: &VerifiedServiceTrustSnapshot,
        applied_at_ms: u64,
    ) -> Result<ServiceTrustApplicationReceipt, AuthenticationError> {
        if self
            .cluster_id
            .as_deref()
            .is_some_and(|expected| expected != snapshot.policy.cluster_id)
        {
            return Err(AuthenticationError::new(
                "service trust receipt cluster does not match the active bundle",
            ));
        }
        sign_trust_receipt_with_key(
            &self.service_id,
            &self.credential.credential_id,
            &self.credential.signing_key,
            snapshot,
            applied_at_ms,
        )
    }

    fn authenticate_at(
        &self,
        method: &str,
        path: &str,
        cluster_id: &str,
        audience_id: &str,
        body: &[u8],
        issued_at_ms: u64,
    ) -> Result<ServiceAuthentication, AuthenticationError> {
        let nonce = next_service_nonce(&self.inner.sequence, issued_at_ms)?;
        self.authenticate(&ServiceRequestPayload {
            method,
            path,
            cluster_id,
            audience_id,
            issued_at_ms,
            nonce: &nonce,
            body,
        })
    }
}

fn snapshot_from_state(
    inner: &Arc<ServiceSignerInner>,
    state: &ServiceSignerState,
) -> ServiceSignerSnapshot {
    ServiceSignerSnapshot {
        service_id: state.service_id.clone(),
        credential: Arc::clone(&state.active),
        cluster_id: state.cluster_id.clone(),
        bundle_generation: state.generation,
        configured_credential_count: state.credentials.len(),
        inner: Arc::clone(inner),
    }
}

fn read_bundle_file(path: &Path) -> Result<Vec<u8>, ServiceSigningError> {
    let source_metadata = std::fs::symlink_metadata(path).map_err(|_| {
        ServiceSigningError::new(
            ServiceSigningErrorKind::SourceUnavailable,
            "service signing bundle metadata is unavailable",
        )
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        return Err(ServiceSigningError::new(
            ServiceSigningErrorKind::NotRegularFile,
            "service signing bundle must be a regular file and not a symbolic link",
        ));
    }
    let mut file = File::open(path).map_err(|_| {
        ServiceSigningError::new(
            ServiceSigningErrorKind::SourceUnavailable,
            "service signing bundle file is unavailable",
        )
    })?;
    let metadata = file.metadata().map_err(|_| {
        ServiceSigningError::new(
            ServiceSigningErrorKind::SourceUnavailable,
            "service signing bundle metadata is unavailable",
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(ServiceSigningError::new(
            ServiceSigningErrorKind::NotRegularFile,
            "service signing bundle must be a regular file",
        ));
    }
    validate_opened_file_identity(&source_metadata, &metadata)?;
    validate_file_permissions(&metadata)?;
    if metadata.len() > MAX_SERVICE_SIGNING_BUNDLE_BYTES as u64 {
        return Err(ServiceSigningError::new(
            ServiceSigningErrorKind::BundleTooLarge,
            "service signing bundle exceeds the byte limit",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SERVICE_SIGNING_BUNDLE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ServiceSigningError::new(
                ServiceSigningErrorKind::SourceUnavailable,
                "service signing bundle file could not be read",
            )
        })?;
    if bytes.len() > MAX_SERVICE_SIGNING_BUNDLE_BYTES {
        return Err(ServiceSigningError::new(
            ServiceSigningErrorKind::BundleTooLarge,
            "service signing bundle exceeds the byte limit",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn validate_opened_file_identity(
    source_metadata: &std::fs::Metadata,
    opened_metadata: &std::fs::Metadata,
) -> Result<(), ServiceSigningError> {
    use std::os::unix::fs::MetadataExt as _;

    if source_metadata.dev() != opened_metadata.dev()
        || source_metadata.ino() != opened_metadata.ino()
    {
        return Err(ServiceSigningError::new(
            ServiceSigningErrorKind::SourceUnavailable,
            "service signing bundle source changed before it could be read",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_opened_file_identity(
    _source_metadata: &std::fs::Metadata,
    _opened_metadata: &std::fs::Metadata,
) -> Result<(), ServiceSigningError> {
    Ok(())
}

#[cfg(unix)]
fn validate_file_permissions(metadata: &std::fs::Metadata) -> Result<(), ServiceSigningError> {
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(ServiceSigningError::new(
            ServiceSigningErrorKind::UnsafePermissions,
            "service signing bundle permissions must be exactly 0600",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file_permissions(_metadata: &std::fs::Metadata) -> Result<(), ServiceSigningError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        fs,
        sync::{Arc, Barrier},
        thread,
    };

    use super::*;
    use crate::{
        SERVICE_TRUST_POLICY_SCHEMA, ServiceTrustCredential, ServiceTrustPolicyPayload,
        ServiceTrustRootSigningIdentity, TrustedServiceKeyRing, TrustedServiceTrustRootKeyRing,
    };

    const SEED_A: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const SEED_B: &str = "oRHYnSe9L9fS2eMjpnvZPZ7tg09poPfRXpAMlzsqHkg=";

    fn encoded(generation: u64, active: &str, seed_a: &str, seed_b: &str) -> Vec<u8> {
        format!(
            r#"{{"schema":"{SERVICE_SIGNING_BUNDLE_SCHEMA}","cluster_id":"inferlab-primary","generation":{generation},"service_id":"node-a","active_credential_id":"{active}","credentials":[{{"credential_id":"key-a","private_key_base64":"{seed_a}"}},{{"credential_id":"key-b","private_key_base64":"{seed_b}"}}]}}"#
        )
        .into_bytes()
    }

    fn bundle(generation: u64, active: &str) -> VerifiedServiceSigningBundle {
        VerifiedServiceSigningBundle::decode(
            &encoded(generation, active, SEED_A, SEED_B),
            "inferlab-primary",
            "node-a",
        )
        .expect("bundle")
    }

    fn trust_snapshot(cluster_id: &str) -> VerifiedServiceTrustSnapshot {
        let receiver =
            ServiceSigningIdentity::from_base64_seed_with_credential("node-a", "key-a", SEED_A)
                .expect("receiver");
        let root =
            ServiceTrustRootSigningIdentity::from_base64_seed("root-a", SEED_B).expect("root");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("root-a={}", root.public_key_base64()),
            "",
        )
        .expect("roots");
        let signed = root
            .sign(&ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
                cluster_id: cluster_id.to_owned(),
                generation: 7,
                issued_at_ms: 1_700_000_000_000,
                expires_at_ms: None,
                trusted_credentials: vec![ServiceTrustCredential {
                    service_id: "node-a".to_owned(),
                    credential_id: "key-a".to_owned(),
                    public_key_base64: receiver.public_key_base64(),
                }],
                revoked_service_ids: Vec::new(),
                revoked_credentials: Vec::new(),
                gateway_service_ids: vec!["node-a".to_owned()],
            })
            .expect("signed snapshot");
        roots.verify(&signed).expect("verified snapshot")
    }

    #[test]
    fn strict_bundle_validation_is_bounded_and_redacted() {
        let error = VerifiedServiceSigningBundle::decode(
            br#"{"schema":"wrong","cluster_id":"inferlab-primary","generation":1,"service_id":"node-a","active_credential_id":"key-a","credentials":[]}"#,
            "inferlab-primary",
            "node-a",
        )
        .expect_err("schema");
        assert_eq!(error.kind(), ServiceSigningErrorKind::InvalidSchema);
        assert!(!error.to_string().contains("wrong"));

        let unknown = format!(
            r#"{{"schema":"{SERVICE_SIGNING_BUNDLE_SCHEMA}","cluster_id":"inferlab-primary","generation":1,"service_id":"node-a","active_credential_id":"key-a","credentials":[],"private":"{SEED_A}"}}"#
        );
        let error =
            VerifiedServiceSigningBundle::decode(unknown.as_bytes(), "inferlab-primary", "node-a")
                .expect_err("unknown field");
        assert_eq!(error.kind(), ServiceSigningErrorKind::InvalidJson);
        assert!(!error.to_string().contains(SEED_A));

        let too_large = vec![b' '; MAX_SERVICE_SIGNING_BUNDLE_BYTES + 1];
        assert_eq!(
            VerifiedServiceSigningBundle::decode(&too_large, "inferlab-primary", "node-a")
                .expect_err("size")
                .kind(),
            ServiceSigningErrorKind::BundleTooLarge
        );
        assert!(!format!("{:?}", bundle(1, "key-a")).contains(SEED_A));
    }

    #[test]
    fn duplicate_ids_keys_and_unknown_active_are_rejected() {
        let duplicate_id = encoded(1, "key-a", SEED_A, SEED_A);
        assert_eq!(
            VerifiedServiceSigningBundle::decode(&duplicate_id, "inferlab-primary", "node-a")
                .expect_err("duplicate public key")
                .kind(),
            ServiceSigningErrorKind::InvalidCredentialSet
        );
        let unknown = encoded(1, "key-c", SEED_A, SEED_B);
        assert_eq!(
            VerifiedServiceSigningBundle::decode(&unknown, "inferlab-primary", "node-a")
                .expect_err("unknown active")
                .kind(),
            ServiceSigningErrorKind::UnknownActiveCredential
        );

        assert_eq!(
            VerifiedServiceSigningBundle::decode(
                &encoded(1, "key-a", "not-base64", SEED_B),
                "inferlab-primary",
                "node-a",
            )
            .expect_err("private key")
            .kind(),
            ServiceSigningErrorKind::InvalidPrivateKey
        );

        let credentials = (0..17)
            .map(|index| {
                format!(r#"{{"credential_id":"key-{index}","private_key_base64":"{SEED_A}"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        let too_many = format!(
            r#"{{"schema":"{SERVICE_SIGNING_BUNDLE_SCHEMA}","cluster_id":"inferlab-primary","generation":1,"service_id":"node-a","active_credential_id":"key-0","credentials":[{credentials}]}}"#
        );
        assert_eq!(
            VerifiedServiceSigningBundle::decode(
                too_many.as_bytes(),
                "inferlab-primary",
                "node-a",
            )
            .expect_err("credential count")
            .kind(),
            ServiceSigningErrorKind::InvalidCredentialSet
        );
    }

    #[test]
    fn bundle_is_bound_to_positive_generation_cluster_and_service() {
        let valid = encoded(1, "key-a", SEED_A, SEED_B);
        assert_eq!(
            VerifiedServiceSigningBundle::decode(&valid, "other-cluster", "node-a")
                .expect_err("cluster")
                .kind(),
            ServiceSigningErrorKind::ClusterMismatch
        );
        assert_eq!(
            VerifiedServiceSigningBundle::decode(&valid, "inferlab-primary", "node-b")
                .expect_err("service")
                .kind(),
            ServiceSigningErrorKind::ServiceMismatch
        );
        assert_eq!(
            VerifiedServiceSigningBundle::decode(
                &encoded(0, "key-a", SEED_A, SEED_B),
                "inferlab-primary",
                "node-a",
            )
            .expect_err("generation")
            .kind(),
            ServiceSigningErrorKind::InvalidGeneration
        );
    }

    #[test]
    fn rollback_fork_and_policy_rejection_retain_last_known_good() {
        let signer = ServiceSigner::from_bundle(bundle(2, "key-a"));
        assert_eq!(
            signer
                .activate_bundle(bundle(1, "key-b"), |_| true)
                .expect_err("rollback")
                .kind(),
            ServiceSigningErrorKind::StaleGeneration
        );
        assert_eq!(
            signer
                .activate_bundle(bundle(2, "key-b"), |_| true)
                .expect_err("fork")
                .kind(),
            ServiceSigningErrorKind::GenerationFork
        );
        assert_eq!(
            signer
                .activate_bundle(bundle(3, "key-b"), |_| false)
                .expect_err("policy")
                .kind(),
            ServiceSigningErrorKind::CandidateRejected
        );
        let snapshot = signer.snapshot();
        assert_eq!(snapshot.credential_id(), "key-a");
        assert_eq!(snapshot.bundle_generation(), Some(2));
        assert_eq!(signer.status().rejected_reloads, 3);
    }

    #[test]
    fn same_millisecond_concurrent_handoff_never_reuses_nonce() {
        let signer = ServiceSigner::from_bundle(bundle(1, "key-a"));
        let before = signer.snapshot();
        signer
            .activate_bundle(bundle(2, "key-b"), |_| true)
            .expect("activate");
        let after = signer.snapshot();
        let barrier = Arc::new(Barrier::new(17));
        let mut threads = Vec::new();
        for index in 0..16 {
            let snapshot = if index % 2 == 0 {
                before.clone()
            } else {
                after.clone()
            };
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                barrier.wait();
                snapshot
                    .authenticate_at(
                        "POST",
                        "/raft/request-vote",
                        "inferlab-primary",
                        "node-b",
                        b"{}",
                        1_700_000_000_000,
                    )
                    .expect("authentication")
            }));
        }
        barrier.wait();
        let authentications = threads
            .into_iter()
            .map(|thread| thread.join().expect("thread"))
            .collect::<Vec<_>>();
        let nonces = authentications
            .iter()
            .map(|authentication| authentication.nonce.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(nonces.len(), 16);

        let key_a =
            ServiceSigningIdentity::from_base64_seed_with_credential("node-a", "key-a", SEED_A)
                .expect("a");
        let key_b =
            ServiceSigningIdentity::from_base64_seed_with_credential("node-a", "key-b", SEED_B)
                .expect("b");
        let ring = TrustedServiceKeyRing::parse(
            &format!(
                "node-a/key-a={},node-a/key-b={}",
                key_a.public_key_base64(),
                key_b.public_key_base64()
            ),
            "",
        )
        .expect("ring");
        for authentication in authentications {
            ring.verify(
                &ServiceRequestPayload {
                    method: "POST",
                    path: "/raft/request-vote",
                    cluster_id: "inferlab-primary",
                    audience_id: "node-b",
                    issued_at_ms: authentication.issued_at_ms,
                    nonce: &authentication.nonce,
                    body: b"{}",
                },
                &authentication,
            )
            .expect("one credential verifies");
        }
    }

    #[test]
    fn nonce_sequence_exhaustion_fails_closed_without_wrapping() {
        let signer = ServiceSigner::from_bundle(bundle(1, "key-a"));
        signer.inner.sequence.store(u64::MAX, Ordering::Relaxed);
        let error = signer
            .snapshot()
            .authenticate_at(
                "POST",
                "/raft/request-vote",
                "inferlab-primary",
                "node-b",
                b"{}",
                1_700_000_000_000,
            )
            .expect_err("sequence exhaustion");
        assert_eq!(
            error.to_string(),
            "service signing nonce sequence is exhausted"
        );
        assert_eq!(signer.inner.sequence.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn legacy_identity_and_multiple_static_wrappers_share_one_nonce_sequence() {
        let identity = Arc::new(
            ServiceSigningIdentity::from_base64_seed_with_credential("node-a", "key-a", SEED_A)
                .expect("identity"),
        );
        let first = ServiceSigner::from_static(Arc::clone(&identity)).snapshot();
        let second = ServiceSigner::from_static(Arc::clone(&identity)).snapshot();
        let barrier = Arc::new(Barrier::new(4));

        let identity_thread = {
            let identity = Arc::clone(&identity);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                identity
                    .authenticate_at(
                        "POST",
                        "/raft/request-vote",
                        "inferlab-primary",
                        "node-b",
                        b"{}",
                        1_700_000_000_000,
                    )
                    .expect("identity authentication")
            })
        };
        let first_thread = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                first
                    .authenticate_at(
                        "POST",
                        "/raft/request-vote",
                        "inferlab-primary",
                        "node-b",
                        b"{}",
                        1_700_000_000_000,
                    )
                    .expect("first wrapper authentication")
            })
        };
        let second_thread = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                second
                    .authenticate_at(
                        "POST",
                        "/raft/request-vote",
                        "inferlab-primary",
                        "node-b",
                        b"{}",
                        1_700_000_000_000,
                    )
                    .expect("second wrapper authentication")
            })
        };

        barrier.wait();
        let nonces = [
            identity_thread.join().expect("identity thread").nonce,
            first_thread.join().expect("first wrapper thread").nonce,
            second_thread.join().expect("second wrapper thread").nonce,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        assert_eq!(nonces.len(), 3);
        assert_eq!(identity.sequence.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn legacy_identity_nonce_exhaustion_fails_closed_without_wrapping() {
        let identity =
            ServiceSigningIdentity::from_base64_seed_with_credential("node-a", "key-a", SEED_A)
                .expect("identity");
        identity.sequence.store(u64::MAX - 1, Ordering::Relaxed);
        let last = identity
            .authenticate_at(
                "POST",
                "/raft/request-vote",
                "inferlab-primary",
                "node-b",
                b"{}",
                1_700_000_000_000,
            )
            .expect("last nonce");
        assert!(last.nonce.ends_with(&format!(".{}", u64::MAX - 1)));
        let error = identity
            .authenticate_at(
                "POST",
                "/raft/request-vote",
                "inferlab-primary",
                "node-b",
                b"{}",
                1_700_000_000_000,
            )
            .expect_err("sequence exhaustion");
        assert_eq!(
            error.to_string(),
            "service signing nonce sequence is exhausted"
        );
        assert_eq!(identity.sequence.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn watched_receipts_are_cluster_bound_while_static_receipts_remain_compatible() {
        let matching = trust_snapshot("inferlab-primary");
        let mismatched = trust_snapshot("other-cluster");
        let watched = ServiceSigner::from_bundle(bundle(1, "key-a"));
        watched
            .snapshot()
            .sign_trust_receipt(&matching, 1_700_000_000_001)
            .expect("matching watched receipt");
        let error = watched
            .snapshot()
            .sign_trust_receipt(&mismatched, 1_700_000_000_001)
            .expect_err("mismatched watched receipt");
        assert_eq!(error.kind(), crate::AuthenticationErrorKind::Invalid);
        assert_eq!(
            error.to_string(),
            "service trust receipt cluster does not match the active bundle"
        );

        let identity = Arc::new(
            ServiceSigningIdentity::from_base64_seed_with_credential("node-a", "key-a", SEED_A)
                .expect("identity"),
        );
        let static_receipt = ServiceSigner::from_static(identity)
            .snapshot()
            .sign_trust_receipt(&mismatched, 1_700_000_000_001)
            .expect("legacy static receipt");
        assert_eq!(static_receipt.payload.cluster_id, "other-cluster");
    }

    #[cfg(unix)]
    #[test]
    fn opened_file_identity_mismatch_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let suffix = format!("{}-{}", std::process::id(), now_ms().expect("clock"));
        let source_path =
            std::env::temp_dir().join(format!("inferlab-signing-source-{suffix}.json"));
        let replacement_path =
            std::env::temp_dir().join(format!("inferlab-signing-replacement-{suffix}.json"));
        fs::write(&source_path, encoded(1, "key-a", SEED_A, SEED_B)).expect("source");
        fs::write(&replacement_path, encoded(2, "key-b", SEED_A, SEED_B)).expect("replacement");
        fs::set_permissions(&source_path, fs::Permissions::from_mode(0o600)).expect("permissions");
        fs::set_permissions(&replacement_path, fs::Permissions::from_mode(0o600))
            .expect("permissions");

        let source_metadata = fs::symlink_metadata(&source_path).expect("source metadata");
        let opened_source = File::open(&source_path).expect("open source");
        validate_opened_file_identity(
            &source_metadata,
            &opened_source.metadata().expect("opened metadata"),
        )
        .expect("same file");
        let opened_replacement = File::open(&replacement_path).expect("open replacement");
        assert_eq!(
            validate_opened_file_identity(
                &source_metadata,
                &opened_replacement.metadata().expect("replacement metadata"),
            )
            .expect_err("mismatched file identity")
            .kind(),
            ServiceSigningErrorKind::SourceUnavailable
        );

        fs::remove_file(source_path).expect("cleanup source");
        fs::remove_file(replacement_path).expect("cleanup replacement");
    }

    #[cfg(unix)]
    #[test]
    fn load_requires_exact_0600_regular_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = std::env::temp_dir().join(format!(
            "inferlab-signing-bundle-{}-{}.json",
            std::process::id(),
            now_ms().expect("clock")
        ));
        fs::write(&path, encoded(1, "key-a", SEED_A, SEED_B)).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert_eq!(
            VerifiedServiceSigningBundle::load(&path, "inferlab-primary", "node-a")
                .expect_err("unsafe")
                .kind(),
            ServiceSigningErrorKind::UnsafePermissions
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("permissions");
        VerifiedServiceSigningBundle::load(&path, "inferlab-primary", "node-a").expect("safe");
        fs::remove_file(path).expect("cleanup");

        let target = std::env::temp_dir().join(format!(
            "inferlab-signing-bundle-target-{}-{}.json",
            std::process::id(),
            now_ms().expect("clock")
        ));
        let link = target.with_extension("link.json");
        fs::write(&target, encoded(1, "key-a", SEED_A, SEED_B)).expect("write");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("permissions");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert_eq!(
            VerifiedServiceSigningBundle::load(&link, "inferlab-primary", "node-a")
                .expect_err("symlink")
                .kind(),
            ServiceSigningErrorKind::NotRegularFile
        );
        fs::remove_file(link).expect("cleanup link");
        fs::remove_file(target).expect("cleanup target");

        let directory = std::env::temp_dir().join(format!(
            "inferlab-signing-bundle-dir-{}-{}",
            std::process::id(),
            now_ms().expect("clock")
        ));
        fs::create_dir(&directory).expect("directory");
        assert_eq!(
            VerifiedServiceSigningBundle::load(&directory, "inferlab-primary", "node-a")
                .expect_err("regular")
                .kind(),
            ServiceSigningErrorKind::NotRegularFile
        );
        fs::remove_dir(directory).expect("cleanup");
    }

    #[test]
    fn static_wrapper_preserves_legacy_surface() {
        let identity = Arc::new(
            ServiceSigningIdentity::from_base64_seed_with_credential("node-a", "key-a", SEED_A)
                .expect("identity"),
        );
        let signer = ServiceSigner::from_static(identity);
        let snapshot = signer.snapshot();
        assert_eq!(snapshot.service_id(), "node-a");
        assert_eq!(snapshot.credential_id(), "key-a");
        assert_eq!(snapshot.bundle_generation(), None);
        assert_eq!(signer.status().mode, ServiceSignerMode::Static);
        assert_eq!(
            signer
                .activate_bundle(bundle(2, "key-b"), |_| true)
                .expect_err("static")
                .kind(),
            ServiceSigningErrorKind::StaticSigner
        );
    }
}

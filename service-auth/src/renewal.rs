use std::{fmt, fs::File, io::Read, path::Path};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::{
    SERVICE_TRUST_POLICY_SCHEMA_V2, ServiceCredentialReference, ServiceTrustCredential,
    ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity, ServiceTrustSnapshot,
    trust_snapshot::compile_policy, validate_id,
};

pub const SERVICE_TRUST_RENEWAL_TEMPLATE_SCHEMA: &str =
    "inferlab.service-trust-renewal-template.v1";
pub const MAX_SERVICE_TRUST_RENEWAL_TEMPLATE_BYTES: usize = 256 * 1024;
pub const MIN_SERVICE_TRUST_RENEWAL_POLICY_LIFETIME_MS: u64 = 250;
pub const MAX_SERVICE_TRUST_RENEWAL_POLICY_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const MAX_SERVICE_TRUST_RENEWAL_INTERVAL_MS: u64 = MAX_SERVICE_TRUST_RENEWAL_POLICY_LIFETIME_MS;

const TEMPLATE_FINGERPRINT_DOMAIN: &[u8] =
    b"inferlab.service-trust-renewal-template-fingerprint.v1\0";
const AUTHORITY_FINGERPRINT_DOMAIN: &[u8] =
    b"inferlab.service-trust-renewal-authority-fingerprint.v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceTrustRenewalErrorKind {
    SourceUnavailable,
    NotRegularFile,
    UnsafePermissions,
    TemplateTooLarge,
    InvalidJson,
    InvalidTemplateSchema,
    InvalidPolicySchema,
    InvalidClusterId,
    ClusterMismatch,
    InvalidRootKeyId,
    InvalidTemplate,
    InvalidTiming,
    InvalidTime,
    ArithmeticOverflow,
    GenerationExhausted,
    SemanticDrift,
    WrongRootKey,
    SigningFailed,
}

impl ServiceTrustRenewalErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceUnavailable => "source_unavailable",
            Self::NotRegularFile => "not_regular_file",
            Self::UnsafePermissions => "unsafe_permissions",
            Self::TemplateTooLarge => "template_too_large",
            Self::InvalidJson => "invalid_json",
            Self::InvalidTemplateSchema => "invalid_template_schema",
            Self::InvalidPolicySchema => "invalid_policy_schema",
            Self::InvalidClusterId => "invalid_cluster_id",
            Self::ClusterMismatch => "cluster_mismatch",
            Self::InvalidRootKeyId => "invalid_root_key_id",
            Self::InvalidTemplate => "invalid_template",
            Self::InvalidTiming => "invalid_timing",
            Self::InvalidTime => "invalid_time",
            Self::ArithmeticOverflow => "arithmetic_overflow",
            Self::GenerationExhausted => "generation_exhausted",
            Self::SemanticDrift => "semantic_drift",
            Self::WrongRootKey => "wrong_root_key",
            Self::SigningFailed => "signing_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceTrustRenewalError {
    kind: ServiceTrustRenewalErrorKind,
    message: &'static str,
}

impl ServiceTrustRenewalError {
    const fn new(kind: ServiceTrustRenewalErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub const fn kind(&self) -> ServiceTrustRenewalErrorKind {
        self.kind
    }
}

impl fmt::Display for ServiceTrustRenewalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ServiceTrustRenewalError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedRenewalTemplate {
    schema: String,
    cluster_id: String,
    policy_schema: String,
    trusted_credentials: Vec<ServiceTrustCredential>,
    revoked_service_ids: Vec<String>,
    revoked_credentials: Vec<ServiceCredentialReference>,
    gateway_service_ids: Vec<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct RenewalTemplate {
    schema: String,
    cluster_id: String,
    policy_schema: String,
    trusted_credentials: Vec<ServiceTrustCredential>,
    revoked_service_ids: Vec<String>,
    revoked_credentials: Vec<ServiceCredentialReference>,
    gateway_service_ids: Vec<String>,
    semantic_fingerprint: String,
}

impl fmt::Debug for RenewalTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenewalTemplate")
            .field("schema", &self.schema)
            .field("cluster_id", &self.cluster_id)
            .field("policy_schema", &self.policy_schema)
            .field("semantic_fingerprint", &self.semantic_fingerprint)
            .field("trusted_credential_count", &self.trusted_credentials.len())
            .field("revoked_service_count", &self.revoked_service_ids.len())
            .field("revoked_credential_count", &self.revoked_credentials.len())
            .field("gateway_service_count", &self.gateway_service_ids.len())
            .finish()
    }
}

impl RenewalTemplate {
    pub fn load(
        path: impl AsRef<Path>,
        expected_cluster_id: &str,
    ) -> Result<Self, ServiceTrustRenewalError> {
        Self::from_path(path, expected_cluster_id)
    }

    pub fn from_path(
        path: impl AsRef<Path>,
        expected_cluster_id: &str,
    ) -> Result<Self, ServiceTrustRenewalError> {
        let bytes = read_template_file(path.as_ref())?;
        Self::decode(&bytes, expected_cluster_id)
    }

    pub fn decode(
        bytes: &[u8],
        expected_cluster_id: &str,
    ) -> Result<Self, ServiceTrustRenewalError> {
        if bytes.len() > MAX_SERVICE_TRUST_RENEWAL_TEMPLATE_BYTES {
            return Err(ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::TemplateTooLarge,
                "service-trust renewal template exceeds the byte limit",
            ));
        }
        validate_id(expected_cluster_id, "cluster ID").map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidClusterId,
                "expected service-trust renewal cluster ID is invalid",
            )
        })?;
        let encoded: EncodedRenewalTemplate = serde_json::from_slice(bytes).map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidJson,
                "service-trust renewal template is not exact valid JSON",
            )
        })?;
        if encoded.schema != SERVICE_TRUST_RENEWAL_TEMPLATE_SCHEMA {
            return Err(ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidTemplateSchema,
                "service-trust renewal template schema is unsupported",
            ));
        }
        validate_id(&encoded.cluster_id, "cluster ID").map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidClusterId,
                "service-trust renewal template cluster ID is invalid",
            )
        })?;
        if encoded.cluster_id != expected_cluster_id {
            return Err(ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::ClusterMismatch,
                "service-trust renewal template cluster does not match this authority",
            ));
        }
        if encoded.policy_schema != SERVICE_TRUST_POLICY_SCHEMA_V2 {
            return Err(ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidPolicySchema,
                "service-trust renewal template requires policy schema v2",
            ));
        }

        let mut template = Self {
            schema: encoded.schema,
            cluster_id: encoded.cluster_id,
            policy_schema: encoded.policy_schema,
            trusted_credentials: encoded.trusted_credentials,
            revoked_service_ids: encoded.revoked_service_ids,
            revoked_credentials: encoded.revoked_credentials,
            gateway_service_ids: encoded.gateway_service_ids,
            semantic_fingerprint: String::new(),
        };
        compile_policy(&template.policy_for_validation()).map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidTemplate,
                "service-trust renewal template policy semantics are invalid",
            )
        })?;
        template.semantic_fingerprint = sha256_hex(&template.canonical_semantics());
        Ok(template)
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    pub fn policy_schema(&self) -> &str {
        &self.policy_schema
    }

    pub fn fingerprint(&self) -> &str {
        self.semantic_fingerprint()
    }

    pub fn semantic_fingerprint(&self) -> &str {
        &self.semantic_fingerprint
    }

    pub fn authority_fingerprint(
        &self,
        root_key_id: &str,
        root_public_key_base64: &str,
    ) -> Result<String, ServiceTrustRenewalError> {
        validate_id(root_key_id, "service-trust root key ID").map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidRootKeyId,
                "service-trust renewal root key ID is invalid",
            )
        })?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(AUTHORITY_FINGERPRINT_DOMAIN);
        append_bytes(&mut bytes, &self.canonical_semantics());
        append_string(&mut bytes, root_key_id);
        append_string(&mut bytes, root_public_key_base64);
        Ok(sha256_hex(&bytes))
    }

    pub fn authority_fingerprint_for_signer(
        &self,
        signer: &ServiceTrustRootSigningIdentity,
    ) -> Result<String, ServiceTrustRenewalError> {
        self.authority_fingerprint(signer.key_id(), &signer.public_key_base64())
    }

    pub fn semantically_matches_policy(&self, policy: &ServiceTrustPolicyPayload) -> bool {
        policy.schema == self.policy_schema
            && policy.cluster_id == self.cluster_id
            && policy.trusted_credentials == self.trusted_credentials
            && policy.revoked_service_ids == self.revoked_service_ids
            && policy.revoked_credentials == self.revoked_credentials
            && policy.gateway_service_ids == self.gateway_service_ids
    }

    pub fn validate_policy_semantics(
        &self,
        policy: &ServiceTrustPolicyPayload,
    ) -> Result<(), ServiceTrustRenewalError> {
        if !self.semantically_matches_policy(policy) {
            return Err(ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::SemanticDrift,
                "service-trust snapshot differs from renewal template semantics",
            ));
        }
        compile_policy(policy).map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::SemanticDrift,
                "service-trust snapshot policy semantics are invalid",
            )
        })?;
        Ok(())
    }

    pub fn validate_snapshot_semantics(
        &self,
        snapshot: &ServiceTrustSnapshot,
        expected_root_key_id: &str,
    ) -> Result<(), ServiceTrustRenewalError> {
        validate_id(expected_root_key_id, "service-trust root key ID").map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidRootKeyId,
                "expected service-trust renewal root key ID is invalid",
            )
        })?;
        if snapshot.authentication.key_id != expected_root_key_id {
            return Err(ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::WrongRootKey,
                "service-trust snapshot was signed by a different root authority",
            ));
        }
        self.validate_policy_semantics(&snapshot.policy)
    }

    pub fn derive_next_policy(
        &self,
        previous_generation: Option<u64>,
        issued_at_ms: u64,
        timing: &RenewalTimingConfig,
    ) -> Result<ServiceTrustPolicyPayload, ServiceTrustRenewalError> {
        if issued_at_ms == 0 {
            return Err(ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidTime,
                "service-trust renewal issue time must be positive",
            ));
        }
        let generation = match previous_generation {
            None => 1,
            Some(previous) => previous.checked_add(1).ok_or_else(|| {
                ServiceTrustRenewalError::new(
                    ServiceTrustRenewalErrorKind::GenerationExhausted,
                    "service-trust renewal generation is exhausted",
                )
            })?,
        };
        let expires_at_ms = issued_at_ms
            .checked_add(timing.policy_lifetime_ms)
            .ok_or_else(|| {
                ServiceTrustRenewalError::new(
                    ServiceTrustRenewalErrorKind::ArithmeticOverflow,
                    "service-trust renewal expiry cannot be represented",
                )
            })?;
        let policy = ServiceTrustPolicyPayload {
            schema: self.policy_schema.clone(),
            cluster_id: self.cluster_id.clone(),
            generation,
            issued_at_ms,
            expires_at_ms: Some(expires_at_ms),
            trusted_credentials: self.trusted_credentials.clone(),
            revoked_service_ids: self.revoked_service_ids.clone(),
            revoked_credentials: self.revoked_credentials.clone(),
            gateway_service_ids: self.gateway_service_ids.clone(),
        };
        compile_policy(&policy).map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::InvalidTemplate,
                "service-trust renewal could not derive a valid policy",
            )
        })?;
        Ok(policy)
    }

    pub fn sign_next(
        &self,
        previous_generation: Option<u64>,
        issued_at_ms: u64,
        timing: &RenewalTimingConfig,
        signer: &ServiceTrustRootSigningIdentity,
    ) -> Result<ServiceTrustSnapshot, ServiceTrustRenewalError> {
        let policy = self.derive_next_policy(previous_generation, issued_at_ms, timing)?;
        let snapshot = signer.sign(&policy).map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::SigningFailed,
                "service-trust renewal signing failed",
            )
        })?;
        self.validate_snapshot_semantics(&snapshot, signer.key_id())?;
        Ok(snapshot)
    }

    fn policy_for_validation(&self) -> ServiceTrustPolicyPayload {
        ServiceTrustPolicyPayload {
            schema: self.policy_schema.clone(),
            cluster_id: self.cluster_id.clone(),
            generation: 1,
            issued_at_ms: 1,
            expires_at_ms: Some(2),
            trusted_credentials: self.trusted_credentials.clone(),
            revoked_service_ids: self.revoked_service_ids.clone(),
            revoked_credentials: self.revoked_credentials.clone(),
            gateway_service_ids: self.gateway_service_ids.clone(),
        }
    }

    fn canonical_semantics(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(TEMPLATE_FINGERPRINT_DOMAIN);
        append_string(&mut bytes, &self.schema);
        append_string(&mut bytes, &self.cluster_id);
        append_string(&mut bytes, &self.policy_schema);
        append_count(&mut bytes, self.trusted_credentials.len());
        for credential in &self.trusted_credentials {
            append_string(&mut bytes, &credential.service_id);
            append_string(&mut bytes, &credential.credential_id);
            append_string(&mut bytes, &credential.public_key_base64);
        }
        append_count(&mut bytes, self.revoked_service_ids.len());
        for service_id in &self.revoked_service_ids {
            append_string(&mut bytes, service_id);
        }
        append_count(&mut bytes, self.revoked_credentials.len());
        for credential in &self.revoked_credentials {
            append_string(&mut bytes, &credential.service_id);
            append_string(&mut bytes, &credential.credential_id);
        }
        append_count(&mut bytes, self.gateway_service_ids.len());
        for service_id in &self.gateway_service_ids {
            append_string(&mut bytes, service_id);
        }
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenewalTimingConfig {
    policy_lifetime_ms: u64,
    renew_before_ms: u64,
    poll_interval_ms: u64,
    retry_interval_ms: u64,
    request_timeout_ms: u64,
}

impl RenewalTimingConfig {
    pub fn new(
        policy_lifetime_ms: u64,
        renew_before_ms: u64,
        poll_interval_ms: u64,
        retry_interval_ms: u64,
        request_timeout_ms: u64,
    ) -> Result<Self, ServiceTrustRenewalError> {
        if !(MIN_SERVICE_TRUST_RENEWAL_POLICY_LIFETIME_MS
            ..=MAX_SERVICE_TRUST_RENEWAL_POLICY_LIFETIME_MS)
            .contains(&policy_lifetime_ms)
        {
            return Err(invalid_timing());
        }
        if renew_before_ms == 0 || renew_before_ms >= policy_lifetime_ms {
            return Err(invalid_timing());
        }
        for interval in [poll_interval_ms, retry_interval_ms, request_timeout_ms] {
            if interval == 0 || interval > MAX_SERVICE_TRUST_RENEWAL_INTERVAL_MS {
                return Err(invalid_timing());
            }
        }
        let first_timeout_and_retry_delay = request_timeout_ms
            .checked_add(retry_interval_ms)
            .ok_or_else(invalid_timing)?;
        if first_timeout_and_retry_delay >= renew_before_ms {
            return Err(invalid_timing());
        }
        Ok(Self {
            policy_lifetime_ms,
            renew_before_ms,
            poll_interval_ms,
            retry_interval_ms,
            request_timeout_ms,
        })
    }

    pub const fn policy_lifetime_ms(self) -> u64 {
        self.policy_lifetime_ms
    }

    pub const fn renew_before_ms(self) -> u64 {
        self.renew_before_ms
    }

    pub const fn poll_interval_ms(self) -> u64 {
        self.poll_interval_ms
    }

    pub const fn retry_interval_ms(self) -> u64 {
        self.retry_interval_ms
    }

    pub const fn request_timeout_ms(self) -> u64 {
        self.request_timeout_ms
    }

    pub fn renewal_deadline_ms(self, expires_at_ms: u64) -> Result<u64, ServiceTrustRenewalError> {
        expires_at_ms
            .checked_sub(self.renew_before_ms)
            .ok_or_else(|| {
                ServiceTrustRenewalError::new(
                    ServiceTrustRenewalErrorKind::ArithmeticOverflow,
                    "service-trust renewal deadline cannot be represented",
                )
            })
    }

    pub fn schedule(
        self,
        expires_at_ms: u64,
        effective_now_ms: u64,
    ) -> Result<RenewalSchedule, ServiceTrustRenewalError> {
        let deadline_ms = self.renewal_deadline_ms(expires_at_ms)?;
        if effective_now_ms < deadline_ms {
            return Ok(RenewalSchedule::Waiting {
                deadline_ms,
                wait_ms: (deadline_ms - effective_now_ms).min(self.poll_interval_ms),
            });
        }
        if effective_now_ms < expires_at_ms {
            return Ok(RenewalSchedule::Due { deadline_ms });
        }
        Ok(RenewalSchedule::Late {
            deadline_ms,
            late_by_ms: effective_now_ms - expires_at_ms,
        })
    }
}

fn invalid_timing() -> ServiceTrustRenewalError {
    ServiceTrustRenewalError::new(
        ServiceTrustRenewalErrorKind::InvalidTiming,
        "service-trust renewal timing configuration is invalid",
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenewalSchedule {
    Waiting { deadline_ms: u64, wait_ms: u64 },
    Due { deadline_ms: u64 },
    Late { deadline_ms: u64, late_by_ms: u64 },
}

impl RenewalSchedule {
    pub const fn is_due(self) -> bool {
        matches!(self, Self::Due { .. } | Self::Late { .. })
    }

    pub const fn is_late(self) -> bool {
        matches!(self, Self::Late { .. })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenewalEffectiveClock {
    effective_now_ms: Option<u64>,
}

impl RenewalEffectiveClock {
    pub const fn new() -> Self {
        Self {
            effective_now_ms: None,
        }
    }

    pub fn observe(&mut self, observed_wall_now_ms: u64) -> u64 {
        let effective = self
            .effective_now_ms
            .map_or(observed_wall_now_ms, |previous| {
                previous.max(observed_wall_now_ms)
            });
        self.effective_now_ms = Some(effective);
        effective
    }

    pub const fn current(self) -> Option<u64> {
        self.effective_now_ms
    }
}

fn read_template_file(path: &Path) -> Result<Vec<u8>, ServiceTrustRenewalError> {
    let source_metadata = std::fs::symlink_metadata(path).map_err(|_| {
        ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::SourceUnavailable,
            "service-trust renewal template metadata is unavailable",
        )
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::NotRegularFile,
            "service-trust renewal template must be a regular non-symlink file",
        ));
    }
    let mut file = open_template_file(path)?;
    let opened_metadata = file.metadata().map_err(|_| {
        ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::SourceUnavailable,
            "service-trust renewal template metadata is unavailable",
        )
    })?;
    if !opened_metadata.file_type().is_file() {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::NotRegularFile,
            "service-trust renewal template must be a regular file",
        ));
    }
    validate_opened_file_identity(&source_metadata, &opened_metadata)?;
    validate_file_permissions(&opened_metadata)?;
    if opened_metadata.len() > MAX_SERVICE_TRUST_RENEWAL_TEMPLATE_BYTES as u64 {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::TemplateTooLarge,
            "service-trust renewal template exceeds the byte limit",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SERVICE_TRUST_RENEWAL_TEMPLATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::SourceUnavailable,
                "service-trust renewal template could not be read",
            )
        })?;
    let final_metadata = file.metadata().map_err(|_| {
        ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::SourceUnavailable,
            "service-trust renewal template metadata is unavailable after read",
        )
    })?;
    validate_opened_file_identity(&opened_metadata, &final_metadata)?;
    if final_metadata.len() != opened_metadata.len()
        || final_metadata.len() != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
    {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::SourceUnavailable,
            "service-trust renewal template changed while it was read",
        ));
    }
    if bytes.len() > MAX_SERVICE_TRUST_RENEWAL_TEMPLATE_BYTES {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::TemplateTooLarge,
            "service-trust renewal template exceeds the byte limit",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_template_file(path: &Path) -> Result<File, ServiceTrustRenewalError> {
    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt as _};

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| {
            ServiceTrustRenewalError::new(
                ServiceTrustRenewalErrorKind::SourceUnavailable,
                "service-trust renewal template is unavailable",
            )
        })
}

#[cfg(not(unix))]
fn open_template_file(path: &Path) -> Result<File, ServiceTrustRenewalError> {
    File::open(path).map_err(|_| {
        ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::SourceUnavailable,
            "service-trust renewal template is unavailable",
        )
    })
}

#[cfg(unix)]
fn validate_opened_file_identity(
    source_metadata: &std::fs::Metadata,
    opened_metadata: &std::fs::Metadata,
) -> Result<(), ServiceTrustRenewalError> {
    use std::os::unix::fs::MetadataExt as _;

    if source_metadata.dev() != opened_metadata.dev()
        || source_metadata.ino() != opened_metadata.ino()
    {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::SourceUnavailable,
            "service-trust renewal template changed before it could be read",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_opened_file_identity(
    _source_metadata: &std::fs::Metadata,
    _opened_metadata: &std::fs::Metadata,
) -> Result<(), ServiceTrustRenewalError> {
    Ok(())
}

#[cfg(unix)]
fn validate_file_permissions(metadata: &std::fs::Metadata) -> Result<(), ServiceTrustRenewalError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::UnsafePermissions,
            "service-trust renewal template permissions must be exactly 0600",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
        return Err(ServiceTrustRenewalError::new(
            ServiceTrustRenewalErrorKind::UnsafePermissions,
            "service-trust renewal template must be owned by the effective user and have one link",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file_permissions(
    _metadata: &std::fs::Metadata,
) -> Result<(), ServiceTrustRenewalError> {
    Ok(())
}

fn append_count(bytes: &mut Vec<u8>, count: usize) {
    let count = u32::try_from(count).expect("validated renewal template counts fit in u32");
    bytes.extend_from_slice(&count.to_be_bytes());
}

fn append_string(bytes: &mut Vec<u8>, value: &str) {
    append_bytes(bytes, value.as_bytes());
}

fn append_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).expect("bounded renewal values fit in u32");
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value);
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};

    use super::*;
    use crate::{SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2, TrustedServiceTrustRootKeyRing};

    const ROOT_SEED_A: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const ROOT_SEED_B: &str = "oRHYnSe9L9fS2eMjpnvZPZ7tg09poPfRXpAMlzsqHkg=";
    const SERVICE_PUBLIC_KEY_A: &str = "PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw=";
    const SERVICE_PUBLIC_KEY_B: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "inferlab-renewal-{name}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).expect("write template");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                    .expect("set template mode");
            }
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn template_value() -> Value {
        json!({
            "schema": SERVICE_TRUST_RENEWAL_TEMPLATE_SCHEMA,
            "cluster_id": "inferlab-primary",
            "policy_schema": SERVICE_TRUST_POLICY_SCHEMA_V2,
            "trusted_credentials": [
                {
                    "service_id": "gateway-primary",
                    "credential_id": "key-a",
                    "public_key_base64": SERVICE_PUBLIC_KEY_A,
                },
                {
                    "service_id": "node-a",
                    "credential_id": "key-b",
                    "public_key_base64": SERVICE_PUBLIC_KEY_B,
                }
            ],
            "revoked_service_ids": [],
            "revoked_credentials": [],
            "gateway_service_ids": ["gateway-primary"]
        })
    }

    fn decode_value(value: &Value) -> RenewalTemplate {
        RenewalTemplate::decode(
            &serde_json::to_vec(value).expect("encode template"),
            "inferlab-primary",
        )
        .expect("decode template")
    }

    fn template() -> RenewalTemplate {
        decode_value(&template_value())
    }

    fn timing() -> RenewalTimingConfig {
        RenewalTimingConfig::new(2_000, 800, 100, 200, 300).expect("timing")
    }

    #[test]
    fn strict_decode_rejects_unknown_duplicate_wrong_schema_and_cluster() {
        let mut unknown = template_value();
        unknown["private_seed"] = Value::String(ROOT_SEED_A.to_owned());
        let error = RenewalTemplate::decode(
            &serde_json::to_vec(&unknown).expect("unknown JSON"),
            "inferlab-primary",
        )
        .expect_err("unknown field");
        assert_eq!(error.kind(), ServiceTrustRenewalErrorKind::InvalidJson);
        assert!(!error.to_string().contains(ROOT_SEED_A));

        let duplicate = format!(
            r#"{{"schema":"{SERVICE_TRUST_RENEWAL_TEMPLATE_SCHEMA}","schema":"{SERVICE_TRUST_RENEWAL_TEMPLATE_SCHEMA}","cluster_id":"inferlab-primary","policy_schema":"{SERVICE_TRUST_POLICY_SCHEMA_V2}","trusted_credentials":[],"revoked_service_ids":[],"revoked_credentials":[],"gateway_service_ids":[]}}"#
        );
        assert_eq!(
            RenewalTemplate::decode(duplicate.as_bytes(), "inferlab-primary")
                .expect_err("duplicate field")
                .kind(),
            ServiceTrustRenewalErrorKind::InvalidJson
        );

        let mut wrong_template_schema = template_value();
        wrong_template_schema["schema"] = Value::String("wrong".to_owned());
        assert_eq!(
            RenewalTemplate::decode(
                &serde_json::to_vec(&wrong_template_schema).expect("schema JSON"),
                "inferlab-primary",
            )
            .expect_err("template schema")
            .kind(),
            ServiceTrustRenewalErrorKind::InvalidTemplateSchema
        );

        let mut wrong_policy_schema = template_value();
        wrong_policy_schema["policy_schema"] =
            Value::String("inferlab.service-trust-policy.v1".to_owned());
        assert_eq!(
            RenewalTemplate::decode(
                &serde_json::to_vec(&wrong_policy_schema).expect("policy schema JSON"),
                "inferlab-primary",
            )
            .expect_err("policy schema")
            .kind(),
            ServiceTrustRenewalErrorKind::InvalidPolicySchema
        );

        assert_eq!(
            RenewalTemplate::decode(
                &serde_json::to_vec(&template_value()).expect("template JSON"),
                "other-cluster",
            )
            .expect_err("cluster mismatch")
            .kind(),
            ServiceTrustRenewalErrorKind::ClusterMismatch
        );

        let oversized = vec![b' '; MAX_SERVICE_TRUST_RENEWAL_TEMPLATE_BYTES + 1];
        assert_eq!(
            RenewalTemplate::decode(&oversized, "inferlab-primary")
                .expect_err("oversized")
                .kind(),
            ServiceTrustRenewalErrorKind::TemplateTooLarge
        );
    }

    #[test]
    fn fingerprint_is_json_canonical_but_array_order_is_semantic() {
        let compact = serde_json::to_vec(&template_value()).expect("compact");
        let pretty = serde_json::to_vec_pretty(&template_value()).expect("pretty");
        let compact = RenewalTemplate::decode(&compact, "inferlab-primary").expect("compact");
        let pretty = RenewalTemplate::decode(&pretty, "inferlab-primary").expect("pretty");
        assert_eq!(compact.fingerprint(), pretty.fingerprint());

        let reordered_object = br#"{
          "gateway_service_ids":["gateway-primary"],
          "revoked_credentials":[],
          "trusted_credentials":[
            {"public_key_base64":"PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw=","credential_id":"key-a","service_id":"gateway-primary"},
            {"credential_id":"key-b","service_id":"node-a","public_key_base64":"11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo="}
          ],
          "schema":"inferlab.service-trust-renewal-template.v1",
          "revoked_service_ids":[],
          "policy_schema":"inferlab.service-trust-policy.v2",
          "cluster_id":"inferlab-primary"
        }"#;
        let reordered = RenewalTemplate::decode(reordered_object, "inferlab-primary")
            .expect("object key order");
        assert_eq!(compact.fingerprint(), reordered.fingerprint());

        let mut array_reordered = template_value();
        array_reordered["trusted_credentials"]
            .as_array_mut()
            .expect("credentials")
            .reverse();
        let array_reordered = decode_value(&array_reordered);
        assert_ne!(compact.fingerprint(), array_reordered.fingerprint());
        assert!(
            !compact.semantically_matches_policy(
                &array_reordered
                    .derive_next_policy(None, 1_000, &timing())
                    .expect("policy")
            )
        );
    }

    #[test]
    fn authority_fingerprint_binds_template_root_id_and_public_key() {
        let template = template();
        let root_a = ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED_A)
            .expect("root A");
        let root_b_same_id =
            ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED_B)
                .expect("root B");
        let root_a_other_id =
            ServiceTrustRootSigningIdentity::from_base64_seed("root-b", ROOT_SEED_A)
                .expect("root other ID");
        assert_ne!(
            template
                .authority_fingerprint_for_signer(&root_a)
                .expect("authority A"),
            template
                .authority_fingerprint_for_signer(&root_b_same_id)
                .expect("authority B")
        );
        assert_ne!(
            template
                .authority_fingerprint_for_signer(&root_a)
                .expect("authority A"),
            template
                .authority_fingerprint_for_signer(&root_a_other_id)
                .expect("authority other ID")
        );
        let fingerprint = template
            .authority_fingerprint(root_a.key_id(), &root_a.public_key_base64())
            .expect("explicit authority");
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn semantic_validation_detects_every_fixed_field_drift() {
        let template = template();
        let policy = template
            .derive_next_policy(None, 1_000, &timing())
            .expect("policy");
        assert!(template.validate_policy_semantics(&policy).is_ok());

        let mut drifts = Vec::new();
        let mut cluster = policy.clone();
        cluster.cluster_id = "other-cluster".to_owned();
        drifts.push(cluster);
        let mut schema = policy.clone();
        schema.schema = "inferlab.service-trust-policy.v1".to_owned();
        schema.expires_at_ms = None;
        drifts.push(schema);
        let mut credentials = policy.clone();
        credentials.trusted_credentials.swap(0, 1);
        drifts.push(credentials);
        let mut revocations = policy.clone();
        revocations.revoked_service_ids.push("node-a".to_owned());
        drifts.push(revocations);
        let mut revoked_credentials = policy.clone();
        revoked_credentials
            .revoked_credentials
            .push(ServiceCredentialReference {
                service_id: "node-a".to_owned(),
                credential_id: "key-b".to_owned(),
            });
        drifts.push(revoked_credentials);
        let mut gateways = policy.clone();
        gateways.gateway_service_ids.push("node-a".to_owned());
        drifts.push(gateways);

        for drift in drifts {
            assert_eq!(
                template
                    .validate_policy_semantics(&drift)
                    .expect_err("semantic drift")
                    .kind(),
                ServiceTrustRenewalErrorKind::SemanticDrift
            );
        }
    }

    #[test]
    fn timing_bounds_deadline_and_clock_edges_are_exact() {
        assert_eq!(
            RenewalTimingConfig::new(249, 100, 10, 10, 10)
                .expect_err("short lifetime")
                .kind(),
            ServiceTrustRenewalErrorKind::InvalidTiming
        );
        assert!(RenewalTimingConfig::new(250, 100, 10, 20, 30).is_ok());
        assert!(
            RenewalTimingConfig::new(
                MAX_SERVICE_TRUST_RENEWAL_POLICY_LIFETIME_MS,
                100,
                10,
                20,
                30,
            )
            .is_ok()
        );
        assert!(
            RenewalTimingConfig::new(
                MAX_SERVICE_TRUST_RENEWAL_POLICY_LIFETIME_MS + 1,
                100,
                10,
                20,
                30,
            )
            .is_err()
        );
        for invalid in [
            RenewalTimingConfig::new(1_000, 0, 10, 20, 30),
            RenewalTimingConfig::new(1_000, 1_000, 10, 20, 30),
            RenewalTimingConfig::new(1_000, 100, 0, 20, 30),
            RenewalTimingConfig::new(1_000, 100, 10, 0, 30),
            RenewalTimingConfig::new(1_000, 100, 10, 20, 0),
            RenewalTimingConfig::new(1_000, 50, 10, 20, 30),
        ] {
            assert_eq!(
                invalid.expect_err("invalid timing").kind(),
                ServiceTrustRenewalErrorKind::InvalidTiming
            );
        }

        let timing = timing();
        assert_eq!(timing.renewal_deadline_ms(5_000), Ok(4_200));
        assert_eq!(
            timing.schedule(5_000, 4_000),
            Ok(RenewalSchedule::Waiting {
                deadline_ms: 4_200,
                wait_ms: 100,
            })
        );
        assert_eq!(
            timing.schedule(5_000, 4_200),
            Ok(RenewalSchedule::Due { deadline_ms: 4_200 })
        );
        assert_eq!(
            timing.schedule(5_000, 5_000),
            Ok(RenewalSchedule::Late {
                deadline_ms: 4_200,
                late_by_ms: 0,
            })
        );
        assert_eq!(
            timing.schedule(5_000, 5_123),
            Ok(RenewalSchedule::Late {
                deadline_ms: 4_200,
                late_by_ms: 123,
            })
        );
        assert_eq!(
            timing
                .renewal_deadline_ms(799)
                .expect_err("deadline underflow")
                .kind(),
            ServiceTrustRenewalErrorKind::ArithmeticOverflow
        );

        let mut clock = RenewalEffectiveClock::new();
        assert_eq!(clock.observe(1_000), 1_000);
        assert_eq!(clock.observe(900), 1_000);
        assert_eq!(clock.observe(5_000), 5_000);
        assert_eq!(clock.current(), Some(5_000));
        assert!(
            timing
                .schedule(5_000, clock.current().expect("clock"))
                .unwrap()
                .is_late()
        );
    }

    #[test]
    fn generation_expiry_signing_and_signature_verification_are_exact() {
        let template = template();
        let timing = timing();
        let root =
            ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED_A).expect("root");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("root-a={}", root.public_key_base64()),
            "",
        )
        .expect("roots");

        let generation_one = template
            .sign_next(None, 1_000, &timing, &root)
            .expect("generation one");
        let generation_two = template
            .sign_next(Some(1), 2_000, &timing, &root)
            .expect("generation two");
        assert_eq!(generation_one.policy.generation, 1);
        assert_eq!(generation_one.policy.issued_at_ms, 1_000);
        assert_eq!(generation_one.policy.expires_at_ms, Some(3_000));
        assert_eq!(generation_two.policy.generation, 2);
        assert_eq!(generation_two.policy.expires_at_ms, Some(4_000));
        assert_eq!(
            generation_two.authentication.schema,
            SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2
        );
        assert_eq!(generation_two.authentication.key_id, "root-a");
        roots.verify(&generation_one).expect("verify g1");
        roots.verify(&generation_two).expect("verify g2");
        template
            .validate_snapshot_semantics(&generation_two, "root-a")
            .expect("snapshot semantics");

        let repeat = template
            .sign_next(Some(1), 2_000, &timing, &root)
            .expect("deterministic repeat");
        assert_eq!(repeat, generation_two);
        assert_eq!(
            template
                .derive_next_policy(Some(u64::MAX), 2_000, &timing)
                .expect_err("generation overflow")
                .kind(),
            ServiceTrustRenewalErrorKind::GenerationExhausted
        );
        assert_eq!(
            template
                .derive_next_policy(None, u64::MAX, &timing)
                .expect_err("expiry overflow")
                .kind(),
            ServiceTrustRenewalErrorKind::ArithmeticOverflow
        );
        assert_eq!(
            template
                .derive_next_policy(None, 0, &timing)
                .expect_err("zero issue time")
                .kind(),
            ServiceTrustRenewalErrorKind::InvalidTime
        );

        let other_root = ServiceTrustRootSigningIdentity::from_base64_seed("root-b", ROOT_SEED_B)
            .expect("other root");
        let other = template
            .sign_next(Some(1), 2_000, &timing, &other_root)
            .expect("other snapshot");
        assert_eq!(
            template
                .validate_snapshot_semantics(&other, "root-a")
                .expect_err("wrong root")
                .kind(),
            ServiceTrustRenewalErrorKind::WrongRootKey
        );
    }

    #[test]
    fn debug_and_errors_are_redacted() {
        let template = template();
        let debug = format!("{template:?}");
        assert!(!debug.contains(SERVICE_PUBLIC_KEY_A));
        assert!(!debug.contains("gateway-primary/key-a"));
        let unknown = format!(
            r#"{{"schema":"{SERVICE_TRUST_RENEWAL_TEMPLATE_SCHEMA}","cluster_id":"inferlab-primary","policy_schema":"{SERVICE_TRUST_POLICY_SCHEMA_V2}","trusted_credentials":[],"revoked_service_ids":[],"revoked_credentials":[],"gateway_service_ids":[],"seed":"{ROOT_SEED_A}"}}"#
        );
        let error = RenewalTemplate::decode(unknown.as_bytes(), "inferlab-primary")
            .expect_err("private unknown");
        assert!(!error.to_string().contains(ROOT_SEED_A));
        assert!(!format!("{error:?}").contains(ROOT_SEED_A));
        assert_eq!(error.kind().as_str(), "invalid_json");
    }

    #[test]
    fn strict_file_loader_requires_safe_regular_single_link_source() {
        let directory = TestDirectory::new("custody");
        let bytes = serde_json::to_vec_pretty(&template_value()).expect("template bytes");
        let path = directory.write("template.json", &bytes);
        let loaded = RenewalTemplate::from_path(&path, "inferlab-primary").expect("safe file");
        assert_eq!(loaded, template());

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt as _, symlink};

            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("unsafe mode");
            assert_eq!(
                RenewalTemplate::from_path(&path, "inferlab-primary")
                    .expect_err("permissions")
                    .kind(),
                ServiceTrustRenewalErrorKind::UnsafePermissions
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("restore mode");

            let link = directory.0.join("template-link.json");
            symlink(&path, &link).expect("symlink");
            assert_eq!(
                RenewalTemplate::from_path(&link, "inferlab-primary")
                    .expect_err("symlink")
                    .kind(),
                ServiceTrustRenewalErrorKind::NotRegularFile
            );

            let hard_link = directory.0.join("template-hard-link.json");
            fs::hard_link(&path, &hard_link).expect("hard link");
            assert_eq!(
                RenewalTemplate::from_path(&path, "inferlab-primary")
                    .expect_err("multiple links")
                    .kind(),
                ServiceTrustRenewalErrorKind::UnsafePermissions
            );
        }

        let directory = TestDirectory::new("oversize");
        let oversized = directory.write(
            "oversized.json",
            &vec![b' '; MAX_SERVICE_TRUST_RENEWAL_TEMPLATE_BYTES + 1],
        );
        assert_eq!(
            RenewalTemplate::from_path(&oversized, "inferlab-primary")
                .expect_err("oversized file")
                .kind(),
            ServiceTrustRenewalErrorKind::TemplateTooLarge
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_source_is_rejected_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

        let directory = TestDirectory::new("fifo");
        let path = directory.0.join("template.fifo");
        let encoded = CString::new(path.as_os_str().as_bytes()).expect("FIFO path");
        let result = unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) };
        assert_eq!(result, 0, "mkfifo failed");
        assert_eq!(
            RenewalTemplate::from_path(&path, "inferlab-primary")
                .expect_err("FIFO")
                .kind(),
            ServiceTrustRenewalErrorKind::NotRegularFile
        );
    }

    #[test]
    fn template_public_keys_are_valid_base64_fixture_material() {
        assert_eq!(
            STANDARD.decode(SERVICE_PUBLIC_KEY_A).expect("key A").len(),
            32
        );
        assert_eq!(
            STANDARD.decode(SERVICE_PUBLIC_KEY_B).expect("key B").len(),
            32
        );
    }
}

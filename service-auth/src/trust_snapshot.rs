use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::TrustedServiceKeyRing;

pub const SERVICE_TRUST_POLICY_SCHEMA_V1: &str = "inferlab.service-trust-policy.v1";
pub const SERVICE_TRUST_POLICY_SCHEMA_V2: &str = "inferlab.service-trust-policy.v2";
pub const SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1: &str = "inferlab.service-trust-authentication.v1";
pub const SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2: &str = "inferlab.service-trust-authentication.v2";

/// Historical aliases retained for callers that construct v1 policy fixtures.
pub const SERVICE_TRUST_POLICY_SCHEMA: &str = SERVICE_TRUST_POLICY_SCHEMA_V1;
pub const SERVICE_TRUST_AUTHENTICATION_SCHEMA: &str = SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1;
const SIGNATURE_ALGORITHM: &str = "ed25519";
const PAYLOAD_DOMAIN_V1: &[u8] = b"inferlab.service-trust-policy.v1\0";
const PAYLOAD_DOMAIN_V2: &[u8] = b"inferlab.service-trust-policy.v2\0";
const MAX_ID_BYTES: usize = 128;
const MAX_ROOT_KEYS: usize = 16;
const MAX_POLICY_CREDENTIALS: usize = 256;
const MAX_POLICY_SERVICE_IDS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceTrustPolicyVersion {
    V1,
    V2,
}

impl ServiceTrustPolicyVersion {
    pub const fn policy_schema(self) -> &'static str {
        match self {
            Self::V1 => SERVICE_TRUST_POLICY_SCHEMA_V1,
            Self::V2 => SERVICE_TRUST_POLICY_SCHEMA_V2,
        }
    }

    pub const fn authentication_schema(self) -> &'static str {
        match self {
            Self::V1 => SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1,
            Self::V2 => SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2,
        }
    }

    const fn payload_domain(self) -> &'static [u8] {
        match self {
            Self::V1 => PAYLOAD_DOMAIN_V1,
            Self::V2 => PAYLOAD_DOMAIN_V2,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceTrustCredential {
    pub service_id: String,
    pub credential_id: String,
    pub public_key_base64: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ServiceCredentialReference {
    pub service_id: String,
    pub credential_id: String,
}

impl ServiceCredentialReference {
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.service_id, self.credential_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceTrustPolicyPayload {
    pub schema: String,
    pub cluster_id: String,
    pub generation: u64,
    pub issued_at_ms: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_expiry"
    )]
    pub expires_at_ms: Option<u64>,
    pub trusted_credentials: Vec<ServiceTrustCredential>,
    pub revoked_service_ids: Vec<String>,
    pub revoked_credentials: Vec<ServiceCredentialReference>,
    pub gateway_service_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceTrustSnapshotAuthentication {
    pub schema: String,
    pub algorithm: String,
    pub key_id: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceTrustSnapshot {
    #[serde(flatten)]
    pub policy: ServiceTrustPolicyPayload,
    pub authentication: ServiceTrustSnapshotAuthentication,
}

impl ServiceTrustPolicyPayload {
    pub fn version(&self) -> Result<ServiceTrustPolicyVersion, ServiceTrustError> {
        policy_version(&self.schema)
    }
}

fn deserialize_optional_expiry<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| de::Error::custom("service-trust policy expiry cannot be null"))
}

#[derive(Debug, Eq, PartialEq)]
pub struct ServiceTrustError(String);

impl ServiceTrustError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ServiceTrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ServiceTrustError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceTrustValidityErrorKind {
    InvalidConfiguration,
    LegacyV1Disallowed,
    IssuedInFuture,
    LifetimeExceeded,
    Expired,
}

impl ServiceTrustValidityErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::LegacyV1Disallowed => "legacy_v1_disallowed",
            Self::IssuedInFuture => "issued_in_future",
            Self::LifetimeExceeded => "lifetime_exceeded",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceTrustValidityError {
    kind: ServiceTrustValidityErrorKind,
    message: String,
}

impl ServiceTrustValidityError {
    fn new(kind: ServiceTrustValidityErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> ServiceTrustValidityErrorKind {
        self.kind
    }
}

impl fmt::Display for ServiceTrustValidityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServiceTrustValidityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceTrustReceiverValidityConfig {
    allow_v1: bool,
    max_future_skew_ms: u64,
    max_lifetime_ms: u64,
}

impl ServiceTrustReceiverValidityConfig {
    pub fn new(
        allow_v1: bool,
        max_future_skew_ms: u64,
        max_lifetime_ms: u64,
    ) -> Result<Self, ServiceTrustValidityError> {
        if max_lifetime_ms == 0 {
            return Err(ServiceTrustValidityError::new(
                ServiceTrustValidityErrorKind::InvalidConfiguration,
                "service-trust maximum policy lifetime must be positive",
            ));
        }
        Ok(Self {
            allow_v1,
            max_future_skew_ms,
            max_lifetime_ms,
        })
    }

    pub const fn allow_v1(self) -> bool {
        self.allow_v1
    }

    pub const fn max_future_skew_ms(self) -> u64 {
        self.max_future_skew_ms
    }

    pub const fn max_lifetime_ms(self) -> u64 {
        self.max_lifetime_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceTrustReceiverValidity {
    pub version: ServiceTrustPolicyVersion,
    pub expires_at_ms: Option<u64>,
}

pub struct ServiceTrustRootSigningIdentity {
    key_id: String,
    signing_key: SigningKey,
}

impl fmt::Debug for ServiceTrustRootSigningIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceTrustRootSigningIdentity")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl ServiceTrustRootSigningIdentity {
    pub fn from_base64_seed(
        key_id: impl Into<String>,
        encoded_seed: &str,
    ) -> Result<Self, ServiceTrustError> {
        let key_id = key_id.into();
        validate_id(&key_id, "service-trust root key ID")?;
        let bytes = decode_exact::<32>(encoded_seed, "Ed25519 private seed")?;
        Ok(Self {
            key_id,
            signing_key: SigningKey::from_bytes(&bytes),
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn public_key_base64(&self) -> String {
        STANDARD.encode(self.signing_key.verifying_key().as_bytes())
    }

    pub fn sign(
        &self,
        policy: &ServiceTrustPolicyPayload,
    ) -> Result<ServiceTrustSnapshot, ServiceTrustError> {
        compile_policy(policy)?;
        let version = policy.version()?;
        let authentication = ServiceTrustSnapshotAuthentication {
            schema: version.authentication_schema().to_owned(),
            algorithm: SIGNATURE_ALGORITHM.to_owned(),
            key_id: self.key_id.clone(),
            signature: String::new(),
        };
        let message = canonical_payload(policy, &authentication)?;
        let signature = self.signing_key.sign(&message);
        Ok(ServiceTrustSnapshot {
            policy: policy.clone(),
            authentication: ServiceTrustSnapshotAuthentication {
                signature: STANDARD.encode(signature.to_bytes()),
                ..authentication
            },
        })
    }
}

#[derive(Clone, Debug)]
pub struct TrustedServiceTrustRootKeyRing {
    keys: BTreeMap<String, VerifyingKey>,
    ordered_key_ids: Vec<String>,
    revoked_key_ids: BTreeSet<String>,
}

impl TrustedServiceTrustRootKeyRing {
    pub fn parse(encoded_keys: &str, revoked_key_ids: &str) -> Result<Self, ServiceTrustError> {
        let mut keys = BTreeMap::new();
        let mut ordered_key_ids = Vec::new();
        for raw_entry in encoded_keys.split(',') {
            let entry = raw_entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (key_id, encoded_key) = entry.split_once('=').ok_or_else(|| {
                ServiceTrustError::new(format!(
                    "trusted service-trust root '{entry}' must use key-id=base64-public-key"
                ))
            })?;
            let key_id = key_id.trim();
            validate_id(key_id, "service-trust root key ID")?;
            if keys.len() >= MAX_ROOT_KEYS {
                return Err(ServiceTrustError::new(format!(
                    "service-trust root key ring exceeds the {MAX_ROOT_KEYS}-key bound"
                )));
            }
            let bytes = decode_exact::<32>(encoded_key.trim(), "Ed25519 public key")?;
            let verifying_key = VerifyingKey::from_bytes(&bytes).map_err(|error| {
                ServiceTrustError::new(format!(
                    "service-trust root '{key_id}' is not a valid Ed25519 public key: {error}"
                ))
            })?;
            if keys.insert(key_id.to_owned(), verifying_key).is_some() {
                return Err(ServiceTrustError::new(format!(
                    "service-trust root key ID '{key_id}' is duplicated"
                )));
            }
            ordered_key_ids.push(key_id.to_owned());
        }
        if keys.is_empty() {
            return Err(ServiceTrustError::new(
                "at least one trusted service-trust root public key is required",
            ));
        }

        let mut revoked = BTreeSet::new();
        for raw_key_id in revoked_key_ids.split(',') {
            let key_id = raw_key_id.trim();
            if key_id.is_empty() {
                continue;
            }
            validate_id(key_id, "service-trust root key ID")?;
            if !keys.contains_key(key_id) {
                return Err(ServiceTrustError::new(format!(
                    "revoked service-trust root '{key_id}' is missing from trusted roots"
                )));
            }
            if !revoked.insert(key_id.to_owned()) {
                return Err(ServiceTrustError::new(format!(
                    "revoked service-trust root key ID '{key_id}' is duplicated"
                )));
            }
        }

        Ok(Self {
            keys,
            ordered_key_ids,
            revoked_key_ids: revoked,
        })
    }

    pub fn verify(
        &self,
        snapshot: &ServiceTrustSnapshot,
    ) -> Result<VerifiedServiceTrustSnapshot, ServiceTrustError> {
        validate_authentication(&snapshot.authentication)?;
        if self
            .revoked_key_ids
            .contains(&snapshot.authentication.key_id)
        {
            return Err(ServiceTrustError::new(format!(
                "service-trust root '{}' is revoked",
                snapshot.authentication.key_id
            )));
        }
        let verifying_key = self
            .keys
            .get(&snapshot.authentication.key_id)
            .ok_or_else(|| {
                ServiceTrustError::new(format!(
                    "service-trust root '{}' is not trusted",
                    snapshot.authentication.key_id
                ))
            })?;
        let message = canonical_payload(&snapshot.policy, &snapshot.authentication)?;
        let signature_bytes = decode_exact::<64>(
            &snapshot.authentication.signature,
            "service-trust Ed25519 signature",
        )?;
        let signature = Signature::from_bytes(&signature_bytes);
        verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| ServiceTrustError::new("service-trust signature verification failed"))?;
        let compiled = compile_policy(&snapshot.policy)?;
        Ok(VerifiedServiceTrustSnapshot {
            policy: snapshot.policy.clone(),
            signing_key_id: snapshot.authentication.key_id.clone(),
            signature: snapshot.authentication.signature.clone(),
            compiled,
        })
    }

    pub fn trusted_key_ids(&self) -> Vec<String> {
        self.ordered_key_ids.clone()
    }

    pub fn revoked_key_ids(&self) -> Vec<String> {
        self.revoked_key_ids.iter().cloned().collect()
    }
}

#[derive(Clone, Debug)]
pub struct CompiledServiceTrustPolicy {
    pub keys: TrustedServiceKeyRing,
    pub gateway_service_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct VerifiedServiceTrustSnapshot {
    pub policy: ServiceTrustPolicyPayload,
    pub signing_key_id: String,
    pub signature: String,
    pub compiled: CompiledServiceTrustPolicy,
}

impl VerifiedServiceTrustSnapshot {
    pub fn validate_receiver_validity(
        &self,
        now_ms: u64,
        config: &ServiceTrustReceiverValidityConfig,
    ) -> Result<ServiceTrustReceiverValidity, ServiceTrustValidityError> {
        validate_policy_receiver_validity(&self.policy, now_ms, config)
    }
}

fn validate_policy_receiver_validity(
    policy: &ServiceTrustPolicyPayload,
    now_ms: u64,
    config: &ServiceTrustReceiverValidityConfig,
) -> Result<ServiceTrustReceiverValidity, ServiceTrustValidityError> {
    validate_policy_shape(policy).map_err(|error| {
        ServiceTrustValidityError::new(
            ServiceTrustValidityErrorKind::InvalidConfiguration,
            format!("cannot validate malformed service-trust policy: {error}"),
        )
    })?;
    let version = policy.version().map_err(|error| {
        ServiceTrustValidityError::new(
            ServiceTrustValidityErrorKind::InvalidConfiguration,
            format!("cannot validate unsupported service-trust policy: {error}"),
        )
    })?;
    match version {
        ServiceTrustPolicyVersion::V1 => {
            if !config.allow_v1 {
                return Err(ServiceTrustValidityError::new(
                    ServiceTrustValidityErrorKind::LegacyV1Disallowed,
                    "legacy non-expiring service-trust policy v1 is disabled",
                ));
            }
            Ok(ServiceTrustReceiverValidity {
                version,
                expires_at_ms: None,
            })
        }
        ServiceTrustPolicyVersion::V2 => {
            let expires_at_ms = policy.expires_at_ms.ok_or_else(|| {
                ServiceTrustValidityError::new(
                    ServiceTrustValidityErrorKind::InvalidConfiguration,
                    "service-trust policy v2 is missing its required expiry",
                )
            })?;
            let latest_allowed_issue = now_ms.saturating_add(config.max_future_skew_ms);
            if policy.issued_at_ms > latest_allowed_issue {
                return Err(ServiceTrustValidityError::new(
                    ServiceTrustValidityErrorKind::IssuedInFuture,
                    format!(
                        "service-trust policy issue time exceeds the configured future-skew allowance of {} ms",
                        config.max_future_skew_ms
                    ),
                ));
            }
            let lifetime_ms = expires_at_ms
                .checked_sub(policy.issued_at_ms)
                .ok_or_else(|| {
                    ServiceTrustValidityError::new(
                        ServiceTrustValidityErrorKind::InvalidConfiguration,
                        "service-trust policy expiry must be later than its issue time",
                    )
                })?;
            if lifetime_ms > config.max_lifetime_ms {
                return Err(ServiceTrustValidityError::new(
                    ServiceTrustValidityErrorKind::LifetimeExceeded,
                    format!(
                        "service-trust policy lifetime exceeds the configured {} ms maximum",
                        config.max_lifetime_ms
                    ),
                ));
            }
            if now_ms >= expires_at_ms {
                return Err(ServiceTrustValidityError::new(
                    ServiceTrustValidityErrorKind::Expired,
                    "service-trust policy is expired",
                ));
            }
            Ok(ServiceTrustReceiverValidity {
                version,
                expires_at_ms: Some(expires_at_ms),
            })
        }
    }
}

pub(crate) fn compile_policy(
    policy: &ServiceTrustPolicyPayload,
) -> Result<CompiledServiceTrustPolicy, ServiceTrustError> {
    validate_policy_shape(policy)?;
    let encoded_keys = policy
        .trusted_credentials
        .iter()
        .map(|credential| {
            format!(
                "{}/{}={}",
                credential.service_id, credential.credential_id, credential.public_key_base64
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let revoked_service_ids = policy.revoked_service_ids.join(",");
    let revoked_credentials = policy
        .revoked_credentials
        .iter()
        .map(ServiceCredentialReference::qualified_id)
        .collect::<Vec<_>>()
        .join(",");
    let keys = TrustedServiceKeyRing::parse_with_revoked_credentials(
        &encoded_keys,
        &revoked_service_ids,
        &revoked_credentials,
    )
    .map_err(|error| ServiceTrustError::new(format!("invalid service-trust policy: {error}")))?;

    let trusted_service_ids = keys
        .trusted_service_ids()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut gateways = BTreeSet::new();
    for gateway_id in &policy.gateway_service_ids {
        validate_id(gateway_id, "gateway service ID")?;
        if !trusted_service_ids.contains(gateway_id) {
            return Err(ServiceTrustError::new(format!(
                "gateway service ID '{gateway_id}' is missing from trusted credentials"
            )));
        }
        if !gateways.insert(gateway_id.clone()) {
            return Err(ServiceTrustError::new(format!(
                "gateway service ID '{gateway_id}' is duplicated"
            )));
        }
    }
    if gateways.is_empty() {
        return Err(ServiceTrustError::new(
            "at least one gateway service ID is required",
        ));
    }

    Ok(CompiledServiceTrustPolicy {
        keys,
        gateway_service_ids: policy.gateway_service_ids.clone(),
    })
}

fn validate_policy_shape(policy: &ServiceTrustPolicyPayload) -> Result<(), ServiceTrustError> {
    let version = policy.version()?;
    validate_id(&policy.cluster_id, "cluster ID")?;
    if policy.generation == 0 {
        return Err(ServiceTrustError::new(
            "service-trust policy generation must be positive",
        ));
    }
    if policy.issued_at_ms == 0 {
        return Err(ServiceTrustError::new(
            "service-trust policy issue time must be positive",
        ));
    }
    match (version, policy.expires_at_ms) {
        (ServiceTrustPolicyVersion::V1, None) => {}
        (ServiceTrustPolicyVersion::V1, Some(_)) => {
            return Err(ServiceTrustError::new(
                "service-trust policy v1 must omit expires_at_ms",
            ));
        }
        (ServiceTrustPolicyVersion::V2, None) => {
            return Err(ServiceTrustError::new(
                "service-trust policy v2 must include expires_at_ms",
            ));
        }
        (ServiceTrustPolicyVersion::V2, Some(expires_at_ms))
            if expires_at_ms <= policy.issued_at_ms =>
        {
            return Err(ServiceTrustError::new(
                "service-trust policy v2 expiry must be later than its issue time",
            ));
        }
        (ServiceTrustPolicyVersion::V2, Some(_)) => {}
    }
    if policy.trusted_credentials.len() > MAX_POLICY_CREDENTIALS {
        return Err(ServiceTrustError::new(format!(
            "service-trust policy exceeds the {MAX_POLICY_CREDENTIALS}-credential bound"
        )));
    }
    if policy.revoked_service_ids.len() > MAX_POLICY_SERVICE_IDS
        || policy.revoked_credentials.len() > MAX_POLICY_CREDENTIALS
        || policy.gateway_service_ids.len() > MAX_POLICY_SERVICE_IDS
    {
        return Err(ServiceTrustError::new(
            "service-trust policy identity list exceeds its configured bound",
        ));
    }
    for credential in &policy.trusted_credentials {
        validate_id(&credential.service_id, "service ID")?;
        validate_id(&credential.credential_id, "credential ID")?;
        if credential.public_key_base64.len() > 128 {
            return Err(ServiceTrustError::new(
                "service public key encoding exceeds 128 bytes",
            ));
        }
    }
    for service_id in &policy.revoked_service_ids {
        validate_id(service_id, "service ID")?;
    }
    for credential in &policy.revoked_credentials {
        validate_id(&credential.service_id, "service ID")?;
        validate_id(&credential.credential_id, "credential ID")?;
    }
    for gateway_id in &policy.gateway_service_ids {
        validate_id(gateway_id, "gateway service ID")?;
    }
    Ok(())
}

fn validate_authentication(
    authentication: &ServiceTrustSnapshotAuthentication,
) -> Result<ServiceTrustPolicyVersion, ServiceTrustError> {
    let version = authentication_version(&authentication.schema)?;
    if authentication.algorithm != SIGNATURE_ALGORITHM {
        return Err(ServiceTrustError::new(format!(
            "unsupported service-trust signature algorithm '{}'; expected '{SIGNATURE_ALGORITHM}'",
            authentication.algorithm
        )));
    }
    validate_id(&authentication.key_id, "service-trust root key ID")?;
    Ok(version)
}

fn canonical_payload(
    policy: &ServiceTrustPolicyPayload,
    authentication: &ServiceTrustSnapshotAuthentication,
) -> Result<Vec<u8>, ServiceTrustError> {
    validate_policy_shape(policy)?;
    let policy_version = policy.version()?;
    let authentication_version = validate_authentication(authentication)?;
    if policy_version != authentication_version {
        return Err(ServiceTrustError::new(format!(
            "service-trust policy schema '{}' cannot use authentication schema '{}'",
            policy.schema, authentication.schema
        )));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(policy_version.payload_domain());
    append_string(&mut bytes, &policy.schema)?;
    append_string(&mut bytes, &policy.cluster_id)?;
    bytes.extend_from_slice(&policy.generation.to_be_bytes());
    bytes.extend_from_slice(&policy.issued_at_ms.to_be_bytes());
    if let ServiceTrustPolicyVersion::V2 = policy_version {
        bytes.extend_from_slice(
            &policy
                .expires_at_ms
                .ok_or_else(|| {
                    ServiceTrustError::new("service-trust policy v2 must include expires_at_ms")
                })?
                .to_be_bytes(),
        );
    }
    append_count(&mut bytes, policy.trusted_credentials.len())?;
    for credential in &policy.trusted_credentials {
        append_string(&mut bytes, &credential.service_id)?;
        append_string(&mut bytes, &credential.credential_id)?;
        append_string(&mut bytes, &credential.public_key_base64)?;
    }
    append_count(&mut bytes, policy.revoked_service_ids.len())?;
    for service_id in &policy.revoked_service_ids {
        append_string(&mut bytes, service_id)?;
    }
    append_count(&mut bytes, policy.revoked_credentials.len())?;
    for credential in &policy.revoked_credentials {
        append_string(&mut bytes, &credential.service_id)?;
        append_string(&mut bytes, &credential.credential_id)?;
    }
    append_count(&mut bytes, policy.gateway_service_ids.len())?;
    for gateway_id in &policy.gateway_service_ids {
        append_string(&mut bytes, gateway_id)?;
    }
    append_string(&mut bytes, &authentication.schema)?;
    append_string(&mut bytes, &authentication.algorithm)?;
    append_string(&mut bytes, &authentication.key_id)?;
    Ok(bytes)
}

fn policy_version(schema: &str) -> Result<ServiceTrustPolicyVersion, ServiceTrustError> {
    match schema {
        SERVICE_TRUST_POLICY_SCHEMA_V1 => Ok(ServiceTrustPolicyVersion::V1),
        SERVICE_TRUST_POLICY_SCHEMA_V2 => Ok(ServiceTrustPolicyVersion::V2),
        _ => Err(ServiceTrustError::new(format!(
            "unsupported service-trust policy schema '{schema}'; expected '{SERVICE_TRUST_POLICY_SCHEMA_V1}' or '{SERVICE_TRUST_POLICY_SCHEMA_V2}'"
        ))),
    }
}

fn authentication_version(schema: &str) -> Result<ServiceTrustPolicyVersion, ServiceTrustError> {
    match schema {
        SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1 => Ok(ServiceTrustPolicyVersion::V1),
        SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2 => Ok(ServiceTrustPolicyVersion::V2),
        _ => Err(ServiceTrustError::new(format!(
            "unsupported service-trust authentication schema '{schema}'; expected '{SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1}' or '{SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2}'"
        ))),
    }
}

fn append_count(bytes: &mut Vec<u8>, count: usize) -> Result<(), ServiceTrustError> {
    let count = u32::try_from(count)
        .map_err(|_| ServiceTrustError::new("service-trust list exceeds canonical limit"))?;
    bytes.extend_from_slice(&count.to_be_bytes());
    Ok(())
}

fn append_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), ServiceTrustError> {
    let length = u32::try_from(value.len())
        .map_err(|_| ServiceTrustError::new("service-trust value exceeds canonical limit"))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn validate_id(value: &str, label: &str) -> Result<(), ServiceTrustError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ServiceTrustError::new(format!(
            "{label} must contain 1 to {MAX_ID_BYTES} ASCII letters, digits, '.', '_', or '-'"
        )))
    }
}

fn decode_exact<const N: usize>(encoded: &str, label: &str) -> Result<[u8; N], ServiceTrustError> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| ServiceTrustError::new(format!("invalid base64 {label}: {error}")))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        ServiceTrustError::new(format!(
            "{label} must decode to {N} bytes; observed {}",
            bytes.len()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceSigningIdentity;

    const ROOT_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const SERVICE_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";

    fn policy() -> ServiceTrustPolicyPayload {
        let service = ServiceSigningIdentity::from_base64_seed_with_credential(
            "gateway-primary",
            "key-a",
            SERVICE_SEED,
        )
        .expect("service");
        ServiceTrustPolicyPayload {
            schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
            cluster_id: "inferlab-primary".to_owned(),
            generation: 1,
            issued_at_ms: 1_700_000_000_000,
            expires_at_ms: None,
            trusted_credentials: vec![ServiceTrustCredential {
                service_id: "gateway-primary".to_owned(),
                credential_id: "key-a".to_owned(),
                public_key_base64: service.public_key_base64(),
            }],
            revoked_service_ids: Vec::new(),
            revoked_credentials: Vec::new(),
            gateway_service_ids: vec!["gateway-primary".to_owned()],
        }
    }

    fn policy_v2() -> ServiceTrustPolicyPayload {
        ServiceTrustPolicyPayload {
            schema: SERVICE_TRUST_POLICY_SCHEMA_V2.to_owned(),
            expires_at_ms: Some(1_700_000_060_000),
            ..policy()
        }
    }

    #[test]
    fn signs_verifies_and_compiles_a_trust_snapshot() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", signer.public_key_base64()),
            "",
        )
        .expect("roots");
        let snapshot = signer.sign(&policy()).expect("snapshot");
        let verified = roots.verify(&snapshot).expect("verified");

        assert_eq!(verified.policy.generation, 1);
        assert_eq!(verified.signing_key_id, "trust-root-a");
        assert_eq!(
            verified.compiled.keys.trusted_service_credentials(),
            vec!["gateway-primary/key-a"]
        );
    }

    #[test]
    fn generation_and_policy_bytes_are_signature_bound() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", signer.public_key_base64()),
            "",
        )
        .expect("roots");
        let mut snapshot = signer.sign(&policy()).expect("snapshot");
        snapshot.policy.generation = 2;

        assert!(roots.verify(&snapshot).is_err());
    }

    #[test]
    fn v1_wire_shape_and_signature_domain_remain_unchanged() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");
        let snapshot = signer.sign(&policy()).expect("snapshot");
        assert_eq!(snapshot.policy.version(), Ok(ServiceTrustPolicyVersion::V1));
        assert_eq!(
            snapshot.authentication.schema,
            SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1
        );
        let encoded = serde_json::to_value(&snapshot).expect("encode snapshot");
        assert!(
            encoded.get("expires_at_ms").is_none(),
            "v1 JSON must remain byte-shape compatible and omit expiry"
        );

        let unsigned = ServiceTrustSnapshotAuthentication {
            signature: String::new(),
            ..snapshot.authentication
        };
        let canonical = canonical_payload(&snapshot.policy, &unsigned).expect("canonical");
        assert!(canonical.starts_with(PAYLOAD_DOMAIN_V1));
        assert!(!canonical.starts_with(PAYLOAD_DOMAIN_V2));
    }

    #[test]
    fn v2_uses_distinct_schemas_and_binds_expiry() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", signer.public_key_base64()),
            "",
        )
        .expect("roots");
        let snapshot = signer.sign(&policy_v2()).expect("snapshot");
        assert_eq!(snapshot.policy.version(), Ok(ServiceTrustPolicyVersion::V2));
        assert_eq!(
            snapshot.authentication.schema,
            SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2
        );
        assert_eq!(
            roots
                .verify(&snapshot)
                .expect("verify")
                .policy
                .expires_at_ms,
            Some(1_700_000_060_000)
        );

        let mut altered = snapshot.clone();
        altered.policy.expires_at_ms = Some(1_700_000_060_001);
        assert!(roots.verify(&altered).is_err());
    }

    #[test]
    fn mixed_policy_and_authentication_versions_fail() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", signer.public_key_base64()),
            "",
        )
        .expect("roots");

        let mut v1_policy_v2_auth = signer.sign(&policy()).expect("v1");
        v1_policy_v2_auth.authentication.schema = SERVICE_TRUST_AUTHENTICATION_SCHEMA_V2.to_owned();
        let error = roots
            .verify(&v1_policy_v2_auth)
            .expect_err("mixed v1/v2 must fail");
        assert!(
            error
                .to_string()
                .contains("cannot use authentication schema")
        );

        let mut v2_policy_v1_auth = signer.sign(&policy_v2()).expect("v2");
        v2_policy_v1_auth.authentication.schema = SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1.to_owned();
        assert!(roots.verify(&v2_policy_v1_auth).is_err());
    }

    #[test]
    fn policy_versions_enforce_exact_expiry_shape() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");

        let mut v1_with_expiry = policy();
        v1_with_expiry.expires_at_ms = Some(v1_with_expiry.issued_at_ms + 1);
        assert!(
            signer
                .sign(&v1_with_expiry)
                .expect_err("v1 expiry")
                .to_string()
                .contains("must omit")
        );

        let mut v2_without_expiry = policy_v2();
        v2_without_expiry.expires_at_ms = None;
        assert!(
            signer
                .sign(&v2_without_expiry)
                .expect_err("v2 missing expiry")
                .to_string()
                .contains("must include")
        );

        let mut reversed = policy_v2();
        reversed.expires_at_ms = Some(reversed.issued_at_ms);
        assert!(
            signer
                .sign(&reversed)
                .expect_err("reversed window")
                .to_string()
                .contains("later")
        );

        let mut zero_issue = policy_v2();
        zero_issue.issued_at_ms = 0;
        assert!(
            signer
                .sign(&zero_issue)
                .expect_err("zero issue time")
                .to_string()
                .contains("must be positive")
        );
    }

    #[test]
    fn explicit_null_expiry_is_not_treated_as_omission() {
        let mut encoded = serde_json::to_value(policy()).expect("encode policy");
        encoded["expires_at_ms"] = serde_json::Value::Null;
        let error = serde_json::from_value::<ServiceTrustPolicyPayload>(encoded)
            .expect_err("explicit null must fail");
        assert!(error.to_string().contains("cannot be null"));
    }

    #[test]
    fn receiver_validity_requires_an_explicit_v1_compatibility_flag() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", signer.public_key_base64()),
            "",
        )
        .expect("roots");
        let verified = roots
            .verify(&signer.sign(&policy()).expect("sign"))
            .expect("verify");
        let denied =
            ServiceTrustReceiverValidityConfig::new(false, 0, 60_000).expect("deny config");
        assert_eq!(
            verified
                .validate_receiver_validity(1_700_000_000_000, &denied)
                .expect_err("v1 denied")
                .kind(),
            ServiceTrustValidityErrorKind::LegacyV1Disallowed
        );

        let allowed =
            ServiceTrustReceiverValidityConfig::new(true, 0, 60_000).expect("allow config");
        assert_eq!(
            verified
                .validate_receiver_validity(1_700_000_000_000, &allowed)
                .expect("v1 allowed"),
            ServiceTrustReceiverValidity {
                version: ServiceTrustPolicyVersion::V1,
                expires_at_ms: None,
            }
        );
    }

    #[test]
    fn receiver_validity_enforces_exclusive_expiry_and_exact_window_edges() {
        let policy = policy_v2();
        let config = ServiceTrustReceiverValidityConfig::new(false, 100, 60_000).expect("config");

        assert_eq!(
            validate_policy_receiver_validity(
                &policy,
                policy.issued_at_ms.saturating_sub(100),
                &config,
            )
            .expect("future skew boundary")
            .expires_at_ms,
            policy.expires_at_ms
        );
        assert_eq!(
            validate_policy_receiver_validity(
                &policy,
                policy.expires_at_ms.expect("expiry") - 1,
                &config,
            )
            .expect("last valid millisecond")
            .version,
            ServiceTrustPolicyVersion::V2
        );
        assert_eq!(
            validate_policy_receiver_validity(
                &policy,
                policy.expires_at_ms.expect("expiry"),
                &config,
            )
            .expect_err("exclusive expiry")
            .kind(),
            ServiceTrustValidityErrorKind::Expired
        );

        let too_early = policy.issued_at_ms.saturating_sub(101);
        assert_eq!(
            validate_policy_receiver_validity(&policy, too_early, &config)
                .expect_err("future skew exceeded")
                .kind(),
            ServiceTrustValidityErrorKind::IssuedInFuture
        );
    }

    #[test]
    fn receiver_validity_enforces_maximum_lifetime_and_valid_config() {
        assert_eq!(
            ServiceTrustReceiverValidityConfig::new(false, 0, 0)
                .expect_err("zero lifetime")
                .kind(),
            ServiceTrustValidityErrorKind::InvalidConfiguration
        );

        let mut policy = policy_v2();
        policy.expires_at_ms = Some(policy.issued_at_ms + 60_001);
        let config = ServiceTrustReceiverValidityConfig::new(false, 0, 60_000).expect("config");
        assert_eq!(
            validate_policy_receiver_validity(&policy, policy.issued_at_ms, &config)
                .expect_err("lifetime")
                .kind(),
            ServiceTrustValidityErrorKind::LifetimeExceeded
        );
        assert_eq!(
            ServiceTrustValidityErrorKind::LifetimeExceeded.as_str(),
            "lifetime_exceeded"
        );

        let saturated = ServiceTrustPolicyPayload {
            issued_at_ms: u64::MAX - 1,
            expires_at_ms: Some(u64::MAX),
            ..policy_v2()
        };
        assert_eq!(
            validate_policy_receiver_validity(
                &saturated,
                u64::MAX - 100,
                &ServiceTrustReceiverValidityConfig::new(false, 100, 60_000)
                    .expect("saturating config"),
            )
            .expect("future-skew addition saturates safely")
            .expires_at_ms,
            Some(u64::MAX)
        );
    }

    #[test]
    fn revoked_root_cannot_authorize_a_policy() {
        let signer = ServiceTrustRootSigningIdentity::from_base64_seed("trust-root-a", ROOT_SEED)
            .expect("signer");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("trust-root-a={}", signer.public_key_base64()),
            "trust-root-a",
        )
        .expect("roots");
        let snapshot = signer.sign(&policy()).expect("snapshot");

        assert!(
            roots
                .verify(&snapshot)
                .expect_err("revoked")
                .to_string()
                .contains("revoked")
        );
    }
}

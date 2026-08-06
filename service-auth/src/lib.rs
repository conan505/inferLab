use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

mod trust_snapshot;

pub use trust_snapshot::{
    CompiledServiceTrustPolicy, SERVICE_TRUST_AUTHENTICATION_SCHEMA, SERVICE_TRUST_POLICY_SCHEMA,
    ServiceCredentialReference, ServiceTrustCredential, ServiceTrustError,
    ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity, ServiceTrustSnapshot,
    ServiceTrustSnapshotAuthentication, TrustedServiceTrustRootKeyRing,
    VerifiedServiceTrustSnapshot,
};

pub const SERVICE_AUTHENTICATION_SCHEMA: &str = "inferlab.service-authentication.v1";
pub const SIGNATURE_ALGORITHM: &str = "ed25519";
pub const HEADER_SCHEMA: &str = "x-inferlab-service-auth-schema";
pub const HEADER_ALGORITHM: &str = "x-inferlab-service-auth-algorithm";
pub const HEADER_SERVICE_ID: &str = "x-inferlab-service-id";
pub const HEADER_AUDIENCE_ID: &str = "x-inferlab-service-audience";
pub const HEADER_ISSUED_AT_MS: &str = "x-inferlab-service-issued-at-ms";
pub const HEADER_NONCE: &str = "x-inferlab-service-nonce";
pub const HEADER_SIGNATURE: &str = "x-inferlab-service-signature";

const PAYLOAD_DOMAIN: &[u8] = b"inferlab.service-request.v1\0";
const MAX_ID_BYTES: usize = 128;
const MIN_NONCE_BYTES: usize = 16;
const MAX_NONCE_BYTES: usize = 160;
const MAX_CREDENTIALS_PER_SERVICE: usize = 16;
const MAX_TRUSTED_CREDENTIALS: usize = 256;
pub const LEGACY_CREDENTIAL_ID: &str = "legacy";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceAuthentication {
    pub schema: String,
    pub algorithm: String,
    pub service_id: String,
    pub audience_id: String,
    pub issued_at_ms: u64,
    pub nonce: String,
    pub signature: String,
}

#[derive(Clone, Copy, Debug)]
pub struct ServiceRequestPayload<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub cluster_id: &'a str,
    pub audience_id: &'a str,
    pub issued_at_ms: u64,
    pub nonce: &'a str,
    pub body: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticationErrorKind {
    Invalid,
    UnknownService,
    RevokedService,
    RevokedCredential,
    Signature,
}

#[derive(Debug, Eq, PartialEq)]
pub struct AuthenticationError {
    kind: AuthenticationErrorKind,
    credential_id: Option<String>,
    message: String,
}

impl AuthenticationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            kind: AuthenticationErrorKind::Invalid,
            credential_id: None,
            message: message.into(),
        }
    }

    fn classified(
        kind: AuthenticationErrorKind,
        credential_id: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            credential_id,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> AuthenticationErrorKind {
        self.kind
    }

    pub fn credential_id(&self) -> Option<&str> {
        self.credential_id.as_deref()
    }
}

impl fmt::Display for AuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuthenticationError {}

pub struct ServiceSigningIdentity {
    service_id: String,
    credential_id: String,
    signing_key: SigningKey,
    sequence: AtomicU64,
}

impl fmt::Debug for ServiceSigningIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceSigningIdentity")
            .field("service_id", &self.service_id)
            .field("credential_id", &self.credential_id)
            .finish_non_exhaustive()
    }
}

impl ServiceSigningIdentity {
    pub fn from_base64_seed(
        service_id: impl Into<String>,
        encoded_seed: &str,
    ) -> Result<Self, AuthenticationError> {
        Self::from_base64_seed_with_credential(service_id, LEGACY_CREDENTIAL_ID, encoded_seed)
    }

    pub fn from_base64_seed_with_credential(
        service_id: impl Into<String>,
        credential_id: impl Into<String>,
        encoded_seed: &str,
    ) -> Result<Self, AuthenticationError> {
        let service_id = service_id.into();
        let credential_id = credential_id.into();
        validate_id(&service_id, "service ID")?;
        validate_id(&credential_id, "credential ID")?;
        let bytes = decode_exact::<32>(encoded_seed, "Ed25519 private seed")?;
        Ok(Self {
            service_id,
            credential_id,
            signing_key: SigningKey::from_bytes(&bytes),
            sequence: AtomicU64::new(0),
        })
    }

    pub fn service_id(&self) -> &str {
        &self.service_id
    }

    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }

    pub fn public_key_base64(&self) -> String {
        STANDARD.encode(self.signing_key.verifying_key().as_bytes())
    }

    pub fn authenticate_now(
        &self,
        method: &str,
        path: &str,
        cluster_id: &str,
        audience_id: &str,
        body: &[u8],
    ) -> Result<ServiceAuthentication, AuthenticationError> {
        let issued_at_ms = now_ms()?;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let nonce = format!("{issued_at_ms}.{}.{}", std::process::id(), sequence);
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

    pub fn authenticate(
        &self,
        payload: &ServiceRequestPayload<'_>,
    ) -> Result<ServiceAuthentication, AuthenticationError> {
        let message = canonical_payload(payload, &self.service_id)?;
        let signature = self.signing_key.sign(&message);
        Ok(ServiceAuthentication {
            schema: SERVICE_AUTHENTICATION_SCHEMA.to_owned(),
            algorithm: SIGNATURE_ALGORITHM.to_owned(),
            service_id: self.service_id.clone(),
            audience_id: payload.audience_id.to_owned(),
            issued_at_ms: payload.issued_at_ms,
            nonce: payload.nonce.to_owned(),
            signature: STANDARD.encode(signature.to_bytes()),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedServiceCredential {
    pub service_id: String,
    pub credential_id: String,
}

impl VerifiedServiceCredential {
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.service_id, self.credential_id)
    }
}

#[derive(Clone, Debug)]
struct TrustedCredential {
    credential_id: String,
    verifying_key: VerifyingKey,
}

#[derive(Clone, Debug)]
pub struct TrustedServiceKeyRing {
    keys: BTreeMap<String, Vec<TrustedCredential>>,
    ordered_service_ids: Vec<String>,
    ordered_credentials: Vec<String>,
    revoked_service_ids: BTreeSet<String>,
    revoked_credentials: BTreeSet<(String, String)>,
}

impl TrustedServiceKeyRing {
    pub fn parse(
        encoded_keys: &str,
        revoked_service_ids: &str,
    ) -> Result<Self, AuthenticationError> {
        Self::parse_with_revoked_credentials(encoded_keys, revoked_service_ids, "")
    }

    pub fn parse_with_revoked_credentials(
        encoded_keys: &str,
        revoked_service_ids: &str,
        revoked_credentials: &str,
    ) -> Result<Self, AuthenticationError> {
        let mut keys = BTreeMap::<String, Vec<TrustedCredential>>::new();
        let mut ordered_service_ids = Vec::new();
        let mut ordered_credentials = Vec::new();
        let mut seen_services = BTreeSet::new();
        let mut credential_count = 0_usize;
        for raw_entry in encoded_keys.split(',') {
            let entry = raw_entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (qualified_id, encoded_key) = entry.split_once('=').ok_or_else(|| {
                AuthenticationError::new(format!(
                    "trusted service '{entry}' must use service-id[/credential-id]=base64-public-key"
                ))
            })?;
            let (service_id, credential_id) = parse_qualified_credential(qualified_id.trim())?;
            let bytes = decode_exact::<32>(encoded_key.trim(), "Ed25519 public key")?;
            let verifying_key = VerifyingKey::from_bytes(&bytes).map_err(|error| {
                AuthenticationError::new(format!(
                    "trusted credential '{service_id}/{credential_id}' is not a valid Ed25519 public key: {error}"
                ))
            })?;
            let credentials = keys.entry(service_id.clone()).or_default();
            if credentials
                .iter()
                .any(|credential| credential.credential_id == credential_id)
            {
                return Err(AuthenticationError::new(format!(
                    "trusted credential '{service_id}/{credential_id}' is duplicated"
                )));
            }
            if credentials
                .iter()
                .any(|credential| credential.verifying_key.as_bytes() == verifying_key.as_bytes())
            {
                return Err(AuthenticationError::new(format!(
                    "trusted service '{service_id}' assigns the same public key to more than one credential ID"
                )));
            }
            if credentials.len() >= MAX_CREDENTIALS_PER_SERVICE {
                return Err(AuthenticationError::new(format!(
                    "trusted service '{service_id}' exceeds the {MAX_CREDENTIALS_PER_SERVICE}-credential verification bound"
                )));
            }
            credential_count = credential_count.saturating_add(1);
            if credential_count > MAX_TRUSTED_CREDENTIALS {
                return Err(AuthenticationError::new(format!(
                    "trusted service key ring exceeds the {MAX_TRUSTED_CREDENTIALS}-credential bound"
                )));
            }
            credentials.push(TrustedCredential {
                credential_id: credential_id.clone(),
                verifying_key,
            });
            if seen_services.insert(service_id.clone()) {
                ordered_service_ids.push(service_id.clone());
            }
            ordered_credentials.push(format!("{service_id}/{credential_id}"));
        }
        if keys.is_empty() {
            return Err(AuthenticationError::new(
                "at least one trusted Ed25519 service public key is required",
            ));
        }

        let mut revoked = BTreeSet::new();
        for raw_service_id in revoked_service_ids.split(',') {
            let service_id = raw_service_id.trim();
            if service_id.is_empty() {
                continue;
            }
            validate_id(service_id, "service ID")?;
            if !revoked.insert(service_id.to_owned()) {
                return Err(AuthenticationError::new(format!(
                    "revoked service ID '{service_id}' is duplicated"
                )));
            }
        }

        let mut revoked_credential_set = BTreeSet::new();
        for raw_credential in revoked_credentials.split(',') {
            let qualified_id = raw_credential.trim();
            if qualified_id.is_empty() {
                continue;
            }
            if !qualified_id.contains('/') {
                return Err(AuthenticationError::new(format!(
                    "revoked credential '{qualified_id}' must use service-id/credential-id"
                )));
            }
            let credential = parse_qualified_credential(qualified_id)?;
            let configured = keys.get(&credential.0).is_some_and(|credentials| {
                credentials
                    .iter()
                    .any(|candidate| candidate.credential_id == credential.1)
            });
            if !configured {
                return Err(AuthenticationError::new(format!(
                    "revoked credential '{}/{}' is missing from trusted service keys",
                    credential.0, credential.1
                )));
            }
            if !revoked_credential_set.insert(credential.clone()) {
                return Err(AuthenticationError::new(format!(
                    "revoked credential '{}/{}' is duplicated",
                    credential.0, credential.1
                )));
            }
        }

        Ok(Self {
            keys,
            ordered_service_ids,
            ordered_credentials,
            revoked_service_ids: revoked,
            revoked_credentials: revoked_credential_set,
        })
    }

    pub fn verify(
        &self,
        payload: &ServiceRequestPayload<'_>,
        authentication: &ServiceAuthentication,
    ) -> Result<VerifiedServiceCredential, AuthenticationError> {
        validate_authentication(authentication)?;
        if authentication.audience_id != payload.audience_id
            || authentication.issued_at_ms != payload.issued_at_ms
            || authentication.nonce != payload.nonce
        {
            return Err(AuthenticationError::new(
                "service authentication metadata does not match the signed payload",
            ));
        }
        if self
            .revoked_service_ids
            .contains(&authentication.service_id)
        {
            return Err(AuthenticationError::classified(
                AuthenticationErrorKind::RevokedService,
                None,
                format!(
                    "service identity '{}' is revoked",
                    authentication.service_id
                ),
            ));
        }
        let credentials = self.keys.get(&authentication.service_id).ok_or_else(|| {
            AuthenticationError::classified(
                AuthenticationErrorKind::UnknownService,
                None,
                format!(
                    "service identity '{}' is not trusted",
                    authentication.service_id
                ),
            )
        })?;
        let signature_bytes = decode_exact::<64>(&authentication.signature, "Ed25519 signature")?;
        let signature = Signature::from_bytes(&signature_bytes);
        let message = canonical_payload(payload, &authentication.service_id)?;
        for credential in credentials {
            if credential
                .verifying_key
                .verify_strict(&message, &signature)
                .is_err()
            {
                continue;
            }
            let verified = VerifiedServiceCredential {
                service_id: authentication.service_id.clone(),
                credential_id: credential.credential_id.clone(),
            };
            if self
                .revoked_credentials
                .contains(&(verified.service_id.clone(), verified.credential_id.clone()))
            {
                return Err(AuthenticationError::classified(
                    AuthenticationErrorKind::RevokedCredential,
                    Some(verified.credential_id.clone()),
                    format!(
                        "service credential '{}' is revoked",
                        verified.qualified_id()
                    ),
                ));
            }
            return Ok(verified);
        }
        Err(AuthenticationError::classified(
            AuthenticationErrorKind::Signature,
            None,
            "service request signature verification failed",
        ))
    }

    pub fn trusted_service_ids(&self) -> Vec<String> {
        self.ordered_service_ids.clone()
    }

    pub fn trusted_service_credentials(&self) -> Vec<String> {
        self.ordered_credentials.clone()
    }

    pub fn revoked_service_ids(&self) -> Vec<String> {
        self.revoked_service_ids.iter().cloned().collect()
    }

    pub fn revoked_service_credentials(&self) -> Vec<String> {
        self.revoked_credentials
            .iter()
            .map(|(service_id, credential_id)| format!("{service_id}/{credential_id}"))
            .collect()
    }
}

fn parse_qualified_credential(value: &str) -> Result<(String, String), AuthenticationError> {
    let (service_id, credential_id) = value.split_once('/').map_or(
        (value, LEGACY_CREDENTIAL_ID),
        |(service_id, credential_id)| (service_id, credential_id),
    );
    validate_id(service_id, "service ID")?;
    validate_id(credential_id, "credential ID")?;
    Ok((service_id.to_owned(), credential_id.to_owned()))
}

pub fn canonical_json_body<T: Serialize>(value: &T) -> Result<Vec<u8>, AuthenticationError> {
    let value = serde_json::to_value(value)
        .map_err(|error| AuthenticationError::new(format!("serialize JSON body: {error}")))?;
    serde_json::to_vec(&value)
        .map_err(|error| AuthenticationError::new(format!("encode canonical JSON body: {error}")))
}

pub fn validate_service_id(value: &str) -> Result<(), AuthenticationError> {
    validate_id(value, "service ID")
}

fn validate_authentication(
    authentication: &ServiceAuthentication,
) -> Result<(), AuthenticationError> {
    if authentication.schema != SERVICE_AUTHENTICATION_SCHEMA {
        return Err(AuthenticationError::new(format!(
            "unsupported service authentication schema '{}'; expected '{SERVICE_AUTHENTICATION_SCHEMA}'",
            authentication.schema
        )));
    }
    if authentication.algorithm != SIGNATURE_ALGORITHM {
        return Err(AuthenticationError::new(format!(
            "unsupported service signature algorithm '{}'; expected '{SIGNATURE_ALGORITHM}'",
            authentication.algorithm
        )));
    }
    validate_id(&authentication.service_id, "service ID")?;
    validate_id(&authentication.audience_id, "audience ID")?;
    validate_nonce(&authentication.nonce)
}

fn canonical_payload(
    payload: &ServiceRequestPayload<'_>,
    service_id: &str,
) -> Result<Vec<u8>, AuthenticationError> {
    validate_id(service_id, "service ID")?;
    validate_id(payload.audience_id, "audience ID")?;
    validate_nonce(payload.nonce)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PAYLOAD_DOMAIN);
    append_string(&mut bytes, SERVICE_AUTHENTICATION_SCHEMA)?;
    append_string(&mut bytes, SIGNATURE_ALGORITHM)?;
    append_string(&mut bytes, service_id)?;
    append_string(&mut bytes, payload.audience_id)?;
    append_string(&mut bytes, payload.method)?;
    append_string(&mut bytes, payload.path)?;
    append_string(&mut bytes, payload.cluster_id)?;
    bytes.extend_from_slice(&payload.issued_at_ms.to_be_bytes());
    append_string(&mut bytes, payload.nonce)?;
    append_bytes(&mut bytes, payload.body)?;
    Ok(bytes)
}

fn validate_id(value: &str, label: &str) -> Result<(), AuthenticationError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AuthenticationError::new(format!(
            "{label} must contain 1 to {MAX_ID_BYTES} ASCII letters, digits, '.', '_', or '-'"
        )))
    }
}

fn validate_nonce(nonce: &str) -> Result<(), AuthenticationError> {
    let valid = (MIN_NONCE_BYTES..=MAX_NONCE_BYTES).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AuthenticationError::new(format!(
            "nonce must contain {MIN_NONCE_BYTES} to {MAX_NONCE_BYTES} ASCII letters, digits, '.', '_', or '-'"
        )))
    }
}

fn append_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), AuthenticationError> {
    append_bytes(bytes, value.as_bytes())
}

fn append_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), AuthenticationError> {
    let length = u32::try_from(value.len())
        .map_err(|_| AuthenticationError::new("value exceeds canonical payload limit"))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

fn decode_exact<const N: usize>(
    encoded: &str,
    label: &str,
) -> Result<[u8; N], AuthenticationError> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| AuthenticationError::new(format!("invalid base64 {label}: {error}")))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        AuthenticationError::new(format!(
            "{label} must decode to {N} bytes; observed {}",
            bytes.len()
        ))
    })
}

fn now_ms() -> Result<u64, AuthenticationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            AuthenticationError::new(format!("system clock is before epoch: {error}"))
        })?
        .as_millis()
        .try_into()
        .map_err(|_| AuthenticationError::new("system clock does not fit in u64 milliseconds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const ROTATED_SEED: &str = "oRHYnSe9L9fS2eMjpnvZPZ7tg09poPfRXpAMlzsqHkg=";

    fn payload<'a>(body: &'a [u8]) -> ServiceRequestPayload<'a> {
        ServiceRequestPayload {
            method: "POST",
            path: "/raft/request-vote",
            cluster_id: "inferlab-primary",
            audience_id: "node-a",
            issued_at_ms: 1_700_000_000_000,
            nonce: "node-b.1700000000000.1",
            body,
        }
    }

    #[test]
    fn signs_and_verifies_a_service_request() {
        let signer = ServiceSigningIdentity::from_base64_seed("node-b", SEED).expect("signer");
        let ring =
            TrustedServiceKeyRing::parse(&format!("node-b={}", signer.public_key_base64()), "")
                .expect("ring");
        let authentication = signer.authenticate(&payload(b"{}")).expect("sign");
        let verified = ring
            .verify(&payload(b"{}"), &authentication)
            .expect("verify");
        assert_eq!(authentication.audience_id, "node-a");
        assert_eq!(verified.qualified_id(), "node-b/legacy");
    }

    #[test]
    fn method_path_audience_cluster_and_body_are_bound() {
        let signer = ServiceSigningIdentity::from_base64_seed("node-b", SEED).expect("signer");
        let ring =
            TrustedServiceKeyRing::parse(&format!("node-b={}", signer.public_key_base64()), "")
                .expect("ring");
        let authentication = signer.authenticate(&payload(b"original")).expect("sign");
        let mut changed = payload(b"changed");
        assert!(ring.verify(&changed, &authentication).is_err());
        changed = payload(b"original");
        changed.audience_id = "node-c";
        assert!(ring.verify(&changed, &authentication).is_err());
        changed = payload(b"original");
        changed.path = "/raft/append-entries";
        assert!(ring.verify(&changed, &authentication).is_err());
    }

    #[test]
    fn unknown_and_revoked_service_ids_are_distinguished() {
        let signer = ServiceSigningIdentity::from_base64_seed("node-b", SEED).expect("signer");
        let authentication = signer.authenticate(&payload(b"{}")).expect("sign");
        let other = ServiceSigningIdentity::from_base64_seed("node-c", SEED).expect("other");
        let unknown =
            TrustedServiceKeyRing::parse(&format!("node-c={}", other.public_key_base64()), "")
                .expect("unknown");
        let revoked = TrustedServiceKeyRing::parse(
            &format!("node-b={}", signer.public_key_base64()),
            "node-b",
        )
        .expect("revoked");
        assert!(
            unknown
                .verify(&payload(b"{}"), &authentication)
                .expect_err("unknown")
                .to_string()
                .contains("not trusted")
        );
        assert!(
            revoked
                .verify(&payload(b"{}"), &authentication)
                .expect_err("revoked")
                .to_string()
                .contains("revoked")
        );
    }

    #[test]
    fn overlapping_credentials_verify_and_identify_the_matching_key() {
        let key_a =
            ServiceSigningIdentity::from_base64_seed_with_credential("node-b", "key-a", SEED)
                .expect("key a");
        let key_b = ServiceSigningIdentity::from_base64_seed_with_credential(
            "node-b",
            "key-b",
            ROTATED_SEED,
        )
        .expect("key b");
        let ring = TrustedServiceKeyRing::parse_with_revoked_credentials(
            &format!(
                "node-b/key-a={},node-b/key-b={}",
                key_a.public_key_base64(),
                key_b.public_key_base64()
            ),
            "",
            "",
        )
        .expect("ring");

        let verified_a = ring
            .verify(
                &payload(b"a"),
                &key_a.authenticate(&payload(b"a")).expect("sign a"),
            )
            .expect("verify a");
        let verified_b = ring
            .verify(
                &payload(b"b"),
                &key_b.authenticate(&payload(b"b")).expect("sign b"),
            )
            .expect("verify b");

        assert_eq!(verified_a.qualified_id(), "node-b/key-a");
        assert_eq!(verified_b.qualified_id(), "node-b/key-b");
        assert_eq!(
            ring.trusted_service_credentials(),
            vec!["node-b/key-a", "node-b/key-b"]
        );
    }

    #[test]
    fn credential_revocation_rejects_only_the_matching_key() {
        let key_a =
            ServiceSigningIdentity::from_base64_seed_with_credential("node-b", "key-a", SEED)
                .expect("key a");
        let key_b = ServiceSigningIdentity::from_base64_seed_with_credential(
            "node-b",
            "key-b",
            ROTATED_SEED,
        )
        .expect("key b");
        let ring = TrustedServiceKeyRing::parse_with_revoked_credentials(
            &format!(
                "node-b/key-a={},node-b/key-b={}",
                key_a.public_key_base64(),
                key_b.public_key_base64()
            ),
            "",
            "node-b/key-a",
        )
        .expect("ring");

        let error = ring
            .verify(
                &payload(b"a"),
                &key_a.authenticate(&payload(b"a")).expect("sign a"),
            )
            .expect_err("key a must be revoked");
        let verified_b = ring
            .verify(
                &payload(b"b"),
                &key_b.authenticate(&payload(b"b")).expect("sign b"),
            )
            .expect("key b remains valid");

        assert_eq!(error.kind(), AuthenticationErrorKind::RevokedCredential);
        assert_eq!(error.credential_id(), Some("key-a"));
        assert_eq!(verified_b.qualified_id(), "node-b/key-b");
        assert_eq!(ring.revoked_service_credentials(), vec!["node-b/key-a"]);
    }

    #[test]
    fn duplicate_public_keys_cannot_create_ambiguous_credential_identity() {
        let signer = ServiceSigningIdentity::from_base64_seed("node-b", SEED).expect("signer");
        let error = TrustedServiceKeyRing::parse(
            &format!(
                "node-b/key-a={},node-b/key-b={}",
                signer.public_key_base64(),
                signer.public_key_base64()
            ),
            "",
        )
        .expect_err("duplicate public key must fail");

        assert!(error.to_string().contains("same public key"));
    }

    #[test]
    fn credential_count_per_service_is_bounded() {
        let entries = (0..=MAX_CREDENTIALS_PER_SERVICE)
            .map(|index| {
                let seed = STANDARD.encode([u8::try_from(index + 1).expect("small index"); 32]);
                let signer = ServiceSigningIdentity::from_base64_seed_with_credential(
                    "node-b",
                    format!("key-{index}"),
                    &seed,
                )
                .expect("signer");
                format!("node-b/key-{index}={}", signer.public_key_base64())
            })
            .collect::<Vec<_>>()
            .join(",");

        let error = TrustedServiceKeyRing::parse(&entries, "")
            .expect_err("seventeenth credential must exceed the bound");
        assert!(
            error
                .to_string()
                .contains("16-credential verification bound")
        );
    }
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Eq, PartialEq)]
pub struct AuthenticationError(String);

impl AuthenticationError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AuthenticationError {}

pub struct ServiceSigningIdentity {
    service_id: String,
    signing_key: SigningKey,
    sequence: AtomicU64,
}

impl fmt::Debug for ServiceSigningIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceSigningIdentity")
            .field("service_id", &self.service_id)
            .finish_non_exhaustive()
    }
}

impl ServiceSigningIdentity {
    pub fn from_base64_seed(
        service_id: impl Into<String>,
        encoded_seed: &str,
    ) -> Result<Self, AuthenticationError> {
        let service_id = service_id.into();
        validate_id(&service_id, "service ID")?;
        let bytes = decode_exact::<32>(encoded_seed, "Ed25519 private seed")?;
        Ok(Self {
            service_id,
            signing_key: SigningKey::from_bytes(&bytes),
            sequence: AtomicU64::new(0),
        })
    }

    pub fn service_id(&self) -> &str {
        &self.service_id
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

#[derive(Clone, Debug)]
pub struct TrustedServiceKeyRing {
    keys: BTreeMap<String, VerifyingKey>,
    ordered_service_ids: Vec<String>,
    revoked_service_ids: BTreeSet<String>,
}

impl TrustedServiceKeyRing {
    pub fn parse(
        encoded_keys: &str,
        revoked_service_ids: &str,
    ) -> Result<Self, AuthenticationError> {
        let mut keys = BTreeMap::new();
        let mut ordered_service_ids = Vec::new();
        for raw_entry in encoded_keys.split(',') {
            let entry = raw_entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (service_id, encoded_key) = entry.split_once('=').ok_or_else(|| {
                AuthenticationError::new(format!(
                    "trusted service '{entry}' must use service-id=base64-public-key"
                ))
            })?;
            let service_id = service_id.trim();
            validate_id(service_id, "service ID")?;
            let bytes = decode_exact::<32>(encoded_key.trim(), "Ed25519 public key")?;
            let verifying_key = VerifyingKey::from_bytes(&bytes).map_err(|error| {
                AuthenticationError::new(format!(
                    "trusted service '{service_id}' is not a valid Ed25519 public key: {error}"
                ))
            })?;
            if keys.insert(service_id.to_owned(), verifying_key).is_some() {
                return Err(AuthenticationError::new(format!(
                    "trusted service ID '{service_id}' is duplicated"
                )));
            }
            ordered_service_ids.push(service_id.to_owned());
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
        Ok(Self {
            keys,
            ordered_service_ids,
            revoked_service_ids: revoked,
        })
    }

    pub fn verify(
        &self,
        payload: &ServiceRequestPayload<'_>,
        authentication: &ServiceAuthentication,
    ) -> Result<(), AuthenticationError> {
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
            return Err(AuthenticationError::new(format!(
                "service identity '{}' is revoked",
                authentication.service_id
            )));
        }
        let verifying_key = self.keys.get(&authentication.service_id).ok_or_else(|| {
            AuthenticationError::new(format!(
                "service identity '{}' is not trusted",
                authentication.service_id
            ))
        })?;
        let signature_bytes = decode_exact::<64>(&authentication.signature, "Ed25519 signature")?;
        let signature = Signature::from_bytes(&signature_bytes);
        let message = canonical_payload(payload, &authentication.service_id)?;
        verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| AuthenticationError::new("service request signature verification failed"))
    }

    pub fn trusted_service_ids(&self) -> Vec<String> {
        self.ordered_service_ids.clone()
    }

    pub fn revoked_service_ids(&self) -> Vec<String> {
        self.revoked_service_ids.iter().cloned().collect()
    }
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
        ring.verify(&payload(b"{}"), &authentication)
            .expect("verify");
        assert_eq!(authentication.audience_id, "node-a");
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
}

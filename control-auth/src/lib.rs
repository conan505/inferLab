use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

pub const AUTHENTICATION_SCHEMA: &str = "inferlab.control-authentication.v1";
pub const SIGNATURE_ALGORITHM: &str = "ed25519";
const PAYLOAD_DOMAIN: &[u8] = b"inferlab.control-routing.v1\0";
const MAX_KEY_ID_BYTES: usize = 128;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlAuthentication {
    pub schema: String,
    pub algorithm: String,
    pub key_id: String,
    pub signature: String,
}

#[derive(Clone, Copy, Debug)]
pub struct RoutingWorker<'a> {
    pub id: &'a str,
    pub base_url: &'a str,
    pub weight: u32,
}

#[derive(Clone, Debug)]
pub struct RoutingPayload<'a> {
    pub cluster_id: &'a str,
    pub revision: u64,
    pub term: u64,
    pub routing_policy: &'a str,
    pub workers: Vec<RoutingWorker<'a>>,
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

pub struct SigningIdentity {
    key_id: String,
    signing_key: SigningKey,
}

impl SigningIdentity {
    pub fn from_base64_seed(
        key_id: impl Into<String>,
        encoded_seed: &str,
    ) -> Result<Self, AuthenticationError> {
        let key_id = key_id.into();
        validate_key_id(&key_id)?;
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
        payload: &RoutingPayload<'_>,
    ) -> Result<ControlAuthentication, AuthenticationError> {
        let message = canonical_payload(payload, &self.key_id)?;
        let signature = self.signing_key.sign(&message);
        Ok(ControlAuthentication {
            schema: AUTHENTICATION_SCHEMA.to_owned(),
            algorithm: SIGNATURE_ALGORITHM.to_owned(),
            key_id: self.key_id.clone(),
            signature: STANDARD.encode(signature.to_bytes()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct TrustedKeyRing {
    keys: BTreeMap<String, VerifyingKey>,
    key_preferences: BTreeMap<String, usize>,
    ordered_key_ids: Vec<String>,
    revoked_key_ids: BTreeSet<String>,
}

impl TrustedKeyRing {
    pub fn parse(encoded_keys: &str, revoked_key_ids: &str) -> Result<Self, AuthenticationError> {
        let mut keys = BTreeMap::new();
        let mut key_preferences = BTreeMap::new();
        let mut ordered_key_ids = Vec::new();
        for raw_entry in encoded_keys.split(',') {
            let entry = raw_entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (key_id, encoded_key) = entry.split_once('=').ok_or_else(|| {
                AuthenticationError::new(format!(
                    "trusted key '{entry}' must use key-id=base64-public-key"
                ))
            })?;
            let key_id = key_id.trim();
            validate_key_id(key_id)?;
            let bytes = decode_exact::<32>(encoded_key.trim(), "Ed25519 public key")?;
            let verifying_key = VerifyingKey::from_bytes(&bytes).map_err(|error| {
                AuthenticationError::new(format!(
                    "trusted key '{key_id}' is not a valid Ed25519 public key: {error}"
                ))
            })?;
            if keys.insert(key_id.to_owned(), verifying_key).is_some() {
                return Err(AuthenticationError::new(format!(
                    "trusted key ID '{key_id}' is duplicated"
                )));
            }
            key_preferences.insert(key_id.to_owned(), ordered_key_ids.len());
            ordered_key_ids.push(key_id.to_owned());
        }
        if keys.is_empty() {
            return Err(AuthenticationError::new(
                "at least one trusted Ed25519 public key is required",
            ));
        }

        let mut revoked = BTreeSet::new();
        for raw_key_id in revoked_key_ids.split(',') {
            let key_id = raw_key_id.trim();
            if key_id.is_empty() {
                continue;
            }
            validate_key_id(key_id)?;
            if !revoked.insert(key_id.to_owned()) {
                return Err(AuthenticationError::new(format!(
                    "revoked key ID '{key_id}' is duplicated"
                )));
            }
        }

        Ok(Self {
            keys,
            key_preferences,
            ordered_key_ids,
            revoked_key_ids: revoked,
        })
    }

    pub fn verify(
        &self,
        payload: &RoutingPayload<'_>,
        authentication: &ControlAuthentication,
    ) -> Result<(), AuthenticationError> {
        validate_authentication(authentication)?;
        if self.revoked_key_ids.contains(&authentication.key_id) {
            return Err(AuthenticationError::new(format!(
                "control signing key '{}' is revoked",
                authentication.key_id
            )));
        }
        let verifying_key = self.keys.get(&authentication.key_id).ok_or_else(|| {
            AuthenticationError::new(format!(
                "control signing key '{}' is not trusted",
                authentication.key_id
            ))
        })?;
        let signature_bytes = decode_exact::<64>(&authentication.signature, "Ed25519 signature")?;
        let signature = Signature::from_bytes(&signature_bytes);
        let message = canonical_payload(payload, &authentication.key_id)?;
        verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| AuthenticationError::new("control signature verification failed"))
    }

    pub fn trusted_key_ids(&self) -> Vec<String> {
        self.ordered_key_ids.clone()
    }

    pub fn revoked_key_ids(&self) -> Vec<String> {
        self.revoked_key_ids.iter().cloned().collect()
    }

    pub fn key_preference(&self, key_id: &str) -> Option<usize> {
        self.key_preferences.get(key_id).copied()
    }
}

fn validate_authentication(
    authentication: &ControlAuthentication,
) -> Result<(), AuthenticationError> {
    if authentication.schema != AUTHENTICATION_SCHEMA {
        return Err(AuthenticationError::new(format!(
            "unsupported control authentication schema '{}'; expected '{AUTHENTICATION_SCHEMA}'",
            authentication.schema
        )));
    }
    if authentication.algorithm != SIGNATURE_ALGORITHM {
        return Err(AuthenticationError::new(format!(
            "unsupported control signature algorithm '{}'; expected '{SIGNATURE_ALGORITHM}'",
            authentication.algorithm
        )));
    }
    validate_key_id(&authentication.key_id)
}

fn validate_key_id(key_id: &str) -> Result<(), AuthenticationError> {
    let valid = !key_id.is_empty()
        && key_id.len() <= MAX_KEY_ID_BYTES
        && key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AuthenticationError::new(
            "key ID must contain 1 to 128 ASCII letters, digits, '.', '_', or '-'",
        ))
    }
}

fn canonical_payload(
    payload: &RoutingPayload<'_>,
    key_id: &str,
) -> Result<Vec<u8>, AuthenticationError> {
    validate_key_id(key_id)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PAYLOAD_DOMAIN);
    append_string(&mut bytes, AUTHENTICATION_SCHEMA)?;
    append_string(&mut bytes, SIGNATURE_ALGORITHM)?;
    append_string(&mut bytes, key_id)?;
    append_string(&mut bytes, payload.cluster_id)?;
    bytes.extend_from_slice(&payload.revision.to_be_bytes());
    bytes.extend_from_slice(&payload.term.to_be_bytes());
    append_string(&mut bytes, payload.routing_policy)?;
    let worker_count = u32::try_from(payload.workers.len())
        .map_err(|_| AuthenticationError::new("worker count exceeds canonical payload limit"))?;
    bytes.extend_from_slice(&worker_count.to_be_bytes());
    for worker in &payload.workers {
        append_string(&mut bytes, worker.id)?;
        append_string(&mut bytes, worker.base_url)?;
        bytes.extend_from_slice(&worker.weight.to_be_bytes());
    }
    Ok(bytes)
}

fn append_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), AuthenticationError> {
    let length = u32::try_from(value.len())
        .map_err(|_| AuthenticationError::new("string exceeds canonical payload limit"))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";

    fn payload<'a>(workers: Vec<RoutingWorker<'a>>) -> RoutingPayload<'a> {
        RoutingPayload {
            cluster_id: "inferlab-primary",
            revision: 2,
            term: 1,
            routing_policy: "round-robin",
            workers,
        }
    }

    fn worker() -> RoutingWorker<'static> {
        RoutingWorker {
            id: "cpu-primary",
            base_url: "http://127.0.0.1:9894",
            weight: 1,
        }
    }

    #[test]
    fn signs_and_verifies_the_canonical_routing_payload() {
        let signer = SigningIdentity::from_base64_seed("primary-2026-a", SEED).expect("signer");
        assert_eq!(
            signer.public_key_base64(),
            "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo="
        );
        let keys = TrustedKeyRing::parse(
            &format!("primary-2026-a={}", signer.public_key_base64()),
            "",
        )
        .expect("trusted keys");
        let payload = payload(vec![worker()]);
        let authentication = signer.sign(&payload).expect("sign payload");

        keys.verify(&payload, &authentication)
            .expect("verify signature");
        assert_eq!(authentication.algorithm, SIGNATURE_ALGORITHM);
        assert_eq!(authentication.schema, AUTHENTICATION_SCHEMA);
    }

    #[test]
    fn any_signed_routing_field_change_is_rejected() {
        let signer = SigningIdentity::from_base64_seed("primary-2026-a", SEED).expect("signer");
        let keys = TrustedKeyRing::parse(
            &format!("primary-2026-a={}", signer.public_key_base64()),
            "",
        )
        .expect("trusted keys");
        let authentication = signer.sign(&payload(vec![worker()])).expect("sign payload");
        let tampered_worker = RoutingWorker {
            id: "cpu-rogue",
            ..worker()
        };

        let error = keys
            .verify(&payload(vec![tampered_worker]), &authentication)
            .expect_err("reject tampered worker");
        assert_eq!(error.to_string(), "control signature verification failed");
    }

    #[test]
    fn key_id_is_bound_into_the_signature() {
        let signer = SigningIdentity::from_base64_seed("primary-2026-a", SEED).expect("signer");
        let public_key = signer.public_key_base64();
        let keys = TrustedKeyRing::parse(
            &format!("primary-2026-a={public_key},primary-alias={public_key}"),
            "",
        )
        .expect("trusted keys");
        let mut authentication = signer.sign(&payload(vec![worker()])).expect("sign payload");
        authentication.key_id = "primary-alias".to_owned();

        assert_eq!(
            keys.verify(&payload(vec![worker()]), &authentication)
                .expect_err("reject relabelled key")
                .to_string(),
            "control signature verification failed"
        );
    }

    #[test]
    fn unknown_and_revoked_keys_are_distinguished() {
        let signer = SigningIdentity::from_base64_seed("primary-2026-a", SEED).expect("signer");
        let authentication = signer.sign(&payload(vec![worker()])).expect("sign payload");
        let other = SigningIdentity::from_base64_seed("other", SEED).expect("other signer");
        let unknown = TrustedKeyRing::parse(&format!("other={}", other.public_key_base64()), "")
            .expect("unknown ring");
        let revoked = TrustedKeyRing::parse(
            &format!("primary-2026-a={}", signer.public_key_base64()),
            "primary-2026-a",
        )
        .expect("revoked ring");

        assert!(
            unknown
                .verify(&payload(vec![worker()]), &authentication)
                .expect_err("unknown key")
                .to_string()
                .contains("is not trusted")
        );
        assert!(
            revoked
                .verify(&payload(vec![worker()]), &authentication)
                .expect_err("revoked key")
                .to_string()
                .contains("is revoked")
        );
    }
}

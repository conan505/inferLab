use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::TrustedServiceKeyRing;

pub const SERVICE_TRUST_POLICY_SCHEMA: &str = "inferlab.service-trust-policy.v1";
pub const SERVICE_TRUST_AUTHENTICATION_SCHEMA: &str = "inferlab.service-trust-authentication.v1";
const SIGNATURE_ALGORITHM: &str = "ed25519";
const PAYLOAD_DOMAIN: &[u8] = b"inferlab.service-trust-policy.v1\0";
const MAX_ID_BYTES: usize = 128;
const MAX_ROOT_KEYS: usize = 16;
const MAX_POLICY_CREDENTIALS: usize = 256;
const MAX_POLICY_SERVICE_IDS: usize = 256;

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
        let authentication = ServiceTrustSnapshotAuthentication {
            schema: SERVICE_TRUST_AUTHENTICATION_SCHEMA.to_owned(),
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

fn compile_policy(
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
    if policy.schema != SERVICE_TRUST_POLICY_SCHEMA {
        return Err(ServiceTrustError::new(format!(
            "unsupported service-trust policy schema '{}'; expected '{SERVICE_TRUST_POLICY_SCHEMA}'",
            policy.schema
        )));
    }
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
) -> Result<(), ServiceTrustError> {
    if authentication.schema != SERVICE_TRUST_AUTHENTICATION_SCHEMA {
        return Err(ServiceTrustError::new(format!(
            "unsupported service-trust authentication schema '{}'; expected '{SERVICE_TRUST_AUTHENTICATION_SCHEMA}'",
            authentication.schema
        )));
    }
    if authentication.algorithm != SIGNATURE_ALGORITHM {
        return Err(ServiceTrustError::new(format!(
            "unsupported service-trust signature algorithm '{}'; expected '{SIGNATURE_ALGORITHM}'",
            authentication.algorithm
        )));
    }
    validate_id(&authentication.key_id, "service-trust root key ID")
}

fn canonical_payload(
    policy: &ServiceTrustPolicyPayload,
    authentication: &ServiceTrustSnapshotAuthentication,
) -> Result<Vec<u8>, ServiceTrustError> {
    validate_policy_shape(policy)?;
    validate_authentication(authentication)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PAYLOAD_DOMAIN);
    append_string(&mut bytes, &policy.schema)?;
    append_string(&mut bytes, &policy.cluster_id)?;
    bytes.extend_from_slice(&policy.generation.to_be_bytes());
    bytes.extend_from_slice(&policy.issued_at_ms.to_be_bytes());
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

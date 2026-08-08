use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer};
use serde::{Deserialize, Serialize};

use crate::{
    AuthenticationError, AuthenticationErrorKind, SIGNATURE_ALGORITHM, ServiceSigningIdentity,
    TrustedServiceKeyRing, VerifiedServiceTrustSnapshot, append_string, decode_exact, validate_id,
};

pub const SERVICE_TRUST_RECEIPT_SCHEMA: &str = "inferlab.service-trust-receipt.v1";
pub const SERVICE_TRUST_RECEIPT_AUTHENTICATION_SCHEMA: &str =
    "inferlab.service-trust-receipt-authentication.v1";

const RECEIPT_DOMAIN: &[u8] = b"inferlab.service-trust-receipt.v1\0";

/// The facts a receiver attests after activating one complete, root-signed
/// service-trust snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceTrustReceiptPayload {
    pub schema: String,
    pub cluster_id: String,
    pub generation: u64,
    pub root_key_id: String,
    pub snapshot_signature: String,
    pub receiver_service_id: String,
    pub receiver_credential_id: String,
    pub applied_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceTrustReceiptAuthentication {
    pub schema: String,
    pub algorithm: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceTrustApplicationReceipt {
    #[serde(flatten)]
    pub payload: ServiceTrustReceiptPayload,
    pub authentication: ServiceTrustReceiptAuthentication,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedServiceTrustReceipt {
    pub payload: ServiceTrustReceiptPayload,
}

impl VerifiedServiceTrustReceipt {
    pub fn receiver(&self) -> String {
        format!(
            "{}/{}",
            self.payload.receiver_service_id, self.payload.receiver_credential_id
        )
    }
}

impl ServiceSigningIdentity {
    /// Sign a convergence receipt for a snapshot that has already passed root
    /// signature verification. The service identity and credential are copied
    /// from this signer, so callers cannot accidentally attest for a peer.
    pub fn sign_trust_receipt(
        &self,
        snapshot: &VerifiedServiceTrustSnapshot,
        applied_at_ms: u64,
    ) -> Result<ServiceTrustApplicationReceipt, AuthenticationError> {
        let payload = ServiceTrustReceiptPayload {
            schema: SERVICE_TRUST_RECEIPT_SCHEMA.to_owned(),
            cluster_id: snapshot.policy.cluster_id.clone(),
            generation: snapshot.policy.generation,
            root_key_id: snapshot.signing_key_id.clone(),
            snapshot_signature: snapshot.signature.clone(),
            receiver_service_id: self.service_id.clone(),
            receiver_credential_id: self.credential_id.clone(),
            applied_at_ms,
        };
        let authentication = ServiceTrustReceiptAuthentication {
            schema: SERVICE_TRUST_RECEIPT_AUTHENTICATION_SCHEMA.to_owned(),
            algorithm: SIGNATURE_ALGORITHM.to_owned(),
            signature: String::new(),
        };
        let message = canonical_receipt(&payload, &authentication)?;
        let signature = self.signing_key.sign(&message);
        Ok(ServiceTrustApplicationReceipt {
            payload,
            authentication: ServiceTrustReceiptAuthentication {
                signature: STANDARD.encode(signature.to_bytes()),
                ..authentication
            },
        })
    }
}

impl TrustedServiceKeyRing {
    /// Verify a receipt against one explicit service credential. Unlike normal
    /// request authentication, receipt verification never tries another
    /// credential after the receipt names its receiver credential.
    pub fn verify_trust_receipt(
        &self,
        receipt: &ServiceTrustApplicationReceipt,
    ) -> Result<VerifiedServiceTrustReceipt, AuthenticationError> {
        validate_receipt(&receipt.payload, &receipt.authentication)?;
        let service_id = &receipt.payload.receiver_service_id;
        let credential_id = &receipt.payload.receiver_credential_id;
        if self.revoked_service_ids.contains(service_id) {
            return Err(AuthenticationError::classified(
                AuthenticationErrorKind::RevokedService,
                None,
                format!("service identity '{service_id}' is revoked"),
            ));
        }
        let credentials = self.keys.get(service_id).ok_or_else(|| {
            AuthenticationError::classified(
                AuthenticationErrorKind::UnknownService,
                Some(credential_id.clone()),
                format!("service identity '{service_id}' is not trusted"),
            )
        })?;
        let credential = credentials
            .iter()
            .find(|candidate| candidate.credential_id == *credential_id)
            .ok_or_else(|| {
                AuthenticationError::classified(
                    AuthenticationErrorKind::UnknownService,
                    Some(credential_id.clone()),
                    format!("service credential '{service_id}/{credential_id}' is not trusted"),
                )
            })?;
        if self
            .revoked_credentials
            .contains(&(service_id.clone(), credential_id.clone()))
        {
            return Err(AuthenticationError::classified(
                AuthenticationErrorKind::RevokedCredential,
                Some(credential_id.clone()),
                format!("service credential '{service_id}/{credential_id}' is revoked"),
            ));
        }
        let message = canonical_receipt(&receipt.payload, &receipt.authentication)?;
        let signature_bytes = decode_exact::<64>(
            &receipt.authentication.signature,
            "service-trust receipt Ed25519 signature",
        )?;
        let signature = Signature::from_bytes(&signature_bytes);
        credential
            .verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| {
                AuthenticationError::classified(
                    AuthenticationErrorKind::Signature,
                    Some(credential_id.clone()),
                    "service-trust receipt signature verification failed",
                )
            })?;
        Ok(VerifiedServiceTrustReceipt {
            payload: receipt.payload.clone(),
        })
    }
}

fn canonical_receipt(
    payload: &ServiceTrustReceiptPayload,
    authentication: &ServiceTrustReceiptAuthentication,
) -> Result<Vec<u8>, AuthenticationError> {
    validate_receipt(payload, authentication)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(RECEIPT_DOMAIN);
    append_string(&mut bytes, &payload.schema)?;
    append_string(&mut bytes, &payload.cluster_id)?;
    bytes.extend_from_slice(&payload.generation.to_be_bytes());
    append_string(&mut bytes, &payload.root_key_id)?;
    append_string(&mut bytes, &payload.snapshot_signature)?;
    append_string(&mut bytes, &payload.receiver_service_id)?;
    append_string(&mut bytes, &payload.receiver_credential_id)?;
    bytes.extend_from_slice(&payload.applied_at_ms.to_be_bytes());
    append_string(&mut bytes, &authentication.schema)?;
    append_string(&mut bytes, &authentication.algorithm)?;
    Ok(bytes)
}

fn validate_receipt(
    payload: &ServiceTrustReceiptPayload,
    authentication: &ServiceTrustReceiptAuthentication,
) -> Result<(), AuthenticationError> {
    if payload.schema != SERVICE_TRUST_RECEIPT_SCHEMA {
        return Err(AuthenticationError::new(format!(
            "unsupported service-trust receipt schema '{}'; expected '{SERVICE_TRUST_RECEIPT_SCHEMA}'",
            payload.schema
        )));
    }
    validate_id(&payload.cluster_id, "cluster ID")?;
    if payload.generation == 0 {
        return Err(AuthenticationError::new(
            "service-trust receipt generation must be positive",
        ));
    }
    validate_id(&payload.root_key_id, "service-trust root key ID")?;
    decode_exact::<64>(
        &payload.snapshot_signature,
        "root service-trust snapshot signature",
    )?;
    validate_id(&payload.receiver_service_id, "receiver service ID")?;
    validate_id(&payload.receiver_credential_id, "receiver credential ID")?;
    if payload.applied_at_ms == 0 {
        return Err(AuthenticationError::new(
            "service-trust receipt application time must be positive",
        ));
    }
    if authentication.schema != SERVICE_TRUST_RECEIPT_AUTHENTICATION_SCHEMA {
        return Err(AuthenticationError::new(format!(
            "unsupported service-trust receipt authentication schema '{}'; expected '{SERVICE_TRUST_RECEIPT_AUTHENTICATION_SCHEMA}'",
            authentication.schema
        )));
    }
    if authentication.algorithm != SIGNATURE_ALGORITHM {
        return Err(AuthenticationError::new(format!(
            "unsupported service-trust receipt signature algorithm '{}'; expected '{SIGNATURE_ALGORITHM}'",
            authentication.algorithm
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        SERVICE_TRUST_POLICY_SCHEMA, SERVICE_TRUST_POLICY_SCHEMA_V2, ServiceTrustCredential,
        ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity, TrustedServiceTrustRootKeyRing,
    };

    const ROOT_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const RECEIVER_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";
    type ReceiptMutation = Box<dyn Fn(&mut ServiceTrustApplicationReceipt)>;

    fn fixture() -> (
        ServiceSigningIdentity,
        TrustedServiceKeyRing,
        VerifiedServiceTrustSnapshot,
    ) {
        let receiver = ServiceSigningIdentity::from_base64_seed_with_credential(
            "control-a",
            "key-a",
            RECEIVER_SEED,
        )
        .expect("receiver");
        let root =
            ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED).expect("root");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("root-a={}", root.public_key_base64()),
            "",
        )
        .expect("roots");
        let snapshot = root
            .sign(&ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                generation: 7,
                issued_at_ms: 1_700_000_000_000,
                expires_at_ms: None,
                trusted_credentials: vec![ServiceTrustCredential {
                    service_id: "control-a".to_owned(),
                    credential_id: "key-a".to_owned(),
                    public_key_base64: receiver.public_key_base64(),
                }],
                revoked_service_ids: Vec::new(),
                revoked_credentials: Vec::new(),
                gateway_service_ids: vec!["control-a".to_owned()],
            })
            .expect("sign snapshot");
        let verified = roots.verify(&snapshot).expect("verify snapshot");
        let ring = verified.compiled.keys.clone();
        (receiver, ring, verified)
    }

    #[test]
    fn signs_and_verifies_every_bound_snapshot_and_receiver_fact() {
        let (receiver, ring, snapshot) = fixture();
        let receipt = receiver
            .sign_trust_receipt(&snapshot, 1_700_000_000_123)
            .expect("sign receipt");
        let verified = ring.verify_trust_receipt(&receipt).expect("verify receipt");
        assert_eq!(verified.payload.cluster_id, "inferlab-primary");
        assert_eq!(verified.payload.generation, 7);
        assert_eq!(verified.payload.root_key_id, "root-a");
        assert_eq!(verified.payload.snapshot_signature, snapshot.signature);
        assert_eq!(verified.receiver(), "control-a/key-a");
        assert_eq!(verified.payload.applied_at_ms, 1_700_000_000_123);
    }

    #[test]
    fn every_receipt_fact_is_covered_by_the_signature() {
        let (receiver, ring, snapshot) = fixture();
        let receipt = receiver
            .sign_trust_receipt(&snapshot, 1_700_000_000_123)
            .expect("sign receipt");
        let mut mutations: Vec<ReceiptMutation> = vec![
            Box::new(|value| value.payload.cluster_id = "other-cluster".to_owned()),
            Box::new(|value| value.payload.generation += 1),
            Box::new(|value| value.payload.root_key_id = "root-b".to_owned()),
            Box::new(|value| {
                value.payload.snapshot_signature = STANDARD.encode([9_u8; 64]);
            }),
            Box::new(|value| value.payload.receiver_service_id = "control-b".to_owned()),
            Box::new(|value| value.payload.receiver_credential_id = "key-b".to_owned()),
            Box::new(|value| value.payload.applied_at_ms += 1),
        ];
        for mutate in mutations.drain(..) {
            let mut tampered = receipt.clone();
            mutate(&mut tampered);
            assert!(ring.verify_trust_receipt(&tampered).is_err());
        }
    }

    #[test]
    fn a_different_service_credential_cannot_claim_the_receiver() {
        let (receiver, ring, snapshot) = fixture();
        let other = ServiceSigningIdentity::from_base64_seed_with_credential(
            "control-b",
            "key-b",
            ROOT_SEED,
        )
        .expect("other");
        let mut receipt = other
            .sign_trust_receipt(&snapshot, 1_700_000_000_123)
            .expect("sign other");
        receipt.payload.receiver_service_id = receiver.service_id().to_owned();
        receipt.payload.receiver_credential_id = receiver.credential_id().to_owned();
        assert!(ring.verify_trust_receipt(&receipt).is_err());
    }

    #[test]
    fn v2_receipt_remains_bound_to_the_exact_snapshot_signature() {
        let (receiver, ring, snapshot_v1) = fixture();
        let root =
            ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED).expect("root");
        let roots = TrustedServiceTrustRootKeyRing::parse(
            &format!("root-a={}", root.public_key_base64()),
            "",
        )
        .expect("roots");
        let mut policy_v2 = snapshot_v1.policy;
        policy_v2.schema = SERVICE_TRUST_POLICY_SCHEMA_V2.to_owned();
        policy_v2.expires_at_ms = Some(policy_v2.issued_at_ms + 60_000);
        let snapshot_v2 = roots
            .verify(&root.sign(&policy_v2).expect("sign v2"))
            .expect("verify v2");
        let receipt = receiver
            .sign_trust_receipt(&snapshot_v2, policy_v2.issued_at_ms + 1)
            .expect("sign receipt");
        assert_eq!(receipt.payload.snapshot_signature, snapshot_v2.signature);
        ring.verify_trust_receipt(&receipt).expect("verify receipt");

        let mut rebound = receipt;
        rebound.payload.snapshot_signature = snapshot_v1.signature;
        assert!(ring.verify_trust_receipt(&rebound).is_err());
    }
}

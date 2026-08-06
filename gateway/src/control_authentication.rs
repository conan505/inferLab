use std::io;

use control_auth::{RoutingPayload, RoutingWorker, TrustedKeyRing};

use crate::routing_snapshot_store::CommittedRoutingConfiguration;

#[derive(Clone, Debug)]
pub enum ControlAuthenticator {
    Disabled,
    Required(TrustedKeyRing),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigningKeyTransition {
    Disabled,
    Same,
    Upgrade,
    Downgrade,
}

impl ControlAuthenticator {
    pub fn from_configuration(
        trusted_keys: Option<&str>,
        revoked_key_ids: Option<&str>,
    ) -> io::Result<Self> {
        match trusted_keys {
            None => {
                if revoked_key_ids.is_some_and(|value| !value.trim().is_empty()) {
                    return Err(invalid_data(
                        "INFERLAB_CONTROL_REVOKED_KEY_IDS requires INFERLAB_CONTROL_TRUSTED_KEYS",
                    ));
                }
                Ok(Self::Disabled)
            }
            Some(keys) => TrustedKeyRing::parse(keys, revoked_key_ids.unwrap_or_default())
                .map(Self::Required)
                .map_err(invalid_data),
        }
    }

    pub fn required(&self) -> bool {
        matches!(self, Self::Required(_))
    }

    pub fn trusted_key_ids(&self) -> Vec<String> {
        match self {
            Self::Disabled => Vec::new(),
            Self::Required(keys) => keys.trusted_key_ids(),
        }
    }

    pub fn revoked_key_ids(&self) -> Vec<String> {
        match self {
            Self::Disabled => Vec::new(),
            Self::Required(keys) => keys.revoked_key_ids(),
        }
    }

    pub fn verify(&self, committed: &CommittedRoutingConfiguration) -> io::Result<Option<String>> {
        let Self::Required(keys) = self else {
            return Ok(None);
        };
        let authentication = committed.authentication.as_ref().ok_or_else(|| {
            invalid_data("control configuration is unsigned but signature verification is required")
        })?;
        keys.verify(&routing_payload(committed), authentication)
            .map_err(invalid_data)?;
        Ok(Some(authentication.key_id.clone()))
    }

    pub fn key_transition(
        &self,
        current_key_id: Option<&str>,
        observed_key_id: Option<&str>,
    ) -> SigningKeyTransition {
        let Self::Required(keys) = self else {
            return SigningKeyTransition::Disabled;
        };
        let Some(observed_key_id) = observed_key_id else {
            return SigningKeyTransition::Downgrade;
        };
        let Some(current_key_id) = current_key_id else {
            return SigningKeyTransition::Upgrade;
        };
        if observed_key_id == current_key_id {
            return SigningKeyTransition::Same;
        }
        match (
            keys.key_preference(current_key_id),
            keys.key_preference(observed_key_id),
        ) {
            (Some(current), Some(observed)) if observed > current => SigningKeyTransition::Upgrade,
            _ => SigningKeyTransition::Downgrade,
        }
    }
}

pub fn same_routing_payload(
    left: &CommittedRoutingConfiguration,
    right: &CommittedRoutingConfiguration,
) -> bool {
    left.cluster_id == right.cluster_id
        && left.revision == right.revision
        && left.term == right.term
        && left.configuration == right.configuration
}

fn routing_payload(committed: &CommittedRoutingConfiguration) -> RoutingPayload<'_> {
    RoutingPayload {
        cluster_id: &committed.cluster_id,
        revision: committed.revision,
        term: committed.term,
        routing_policy: &committed.configuration.routing_policy,
        workers: committed
            .configuration
            .workers
            .iter()
            .map(|worker| RoutingWorker {
                id: &worker.id,
                base_url: &worker.base_url,
                weight: worker.weight,
            })
            .collect(),
    }
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use control_auth::{RoutingPayload, RoutingWorker, SigningIdentity};

    use super::*;
    use crate::routing_snapshot_store::{StoredRoutingConfiguration, StoredWorkerConfiguration};

    const SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";

    fn committed() -> CommittedRoutingConfiguration {
        CommittedRoutingConfiguration {
            cluster_id: "inferlab-primary".to_owned(),
            revision: 2,
            term: 1,
            configuration: StoredRoutingConfiguration {
                routing_policy: "round-robin".to_owned(),
                workers: vec![StoredWorkerConfiguration {
                    id: "cpu-primary".to_owned(),
                    base_url: "http://127.0.0.1:9894".to_owned(),
                    weight: 1,
                }],
            },
            authentication: None,
        }
    }

    fn sign(committed: &mut CommittedRoutingConfiguration, signer: &SigningIdentity) {
        let workers = committed
            .configuration
            .workers
            .iter()
            .map(|worker| RoutingWorker {
                id: &worker.id,
                base_url: &worker.base_url,
                weight: worker.weight,
            })
            .collect();
        committed.authentication = Some(
            signer
                .sign(&RoutingPayload {
                    cluster_id: &committed.cluster_id,
                    revision: committed.revision,
                    term: committed.term,
                    routing_policy: &committed.configuration.routing_policy,
                    workers,
                })
                .expect("sign configuration"),
        );
    }

    #[test]
    fn required_authentication_accepts_only_the_signed_payload() {
        let signer = SigningIdentity::from_base64_seed("primary-a", SEED).expect("signer");
        let authenticator = ControlAuthenticator::from_configuration(
            Some(&format!("primary-a={}", signer.public_key_base64())),
            None,
        )
        .expect("authenticator");
        let mut configuration = committed();
        sign(&mut configuration, &signer);
        assert_eq!(
            authenticator.verify(&configuration).expect("verify"),
            Some("primary-a".to_owned())
        );

        configuration.configuration.workers[0].id = "cpu-tampered".to_owned();
        assert!(
            authenticator
                .verify(&configuration)
                .expect_err("reject tamper")
                .to_string()
                .contains("signature verification failed")
        );
    }

    #[test]
    fn required_authentication_rejects_unsigned_and_revoked_routes() {
        let signer = SigningIdentity::from_base64_seed("primary-a", SEED).expect("signer");
        let encoded = format!("primary-a={}", signer.public_key_base64());
        let required =
            ControlAuthenticator::from_configuration(Some(&encoded), None).expect("required");
        assert!(
            required
                .verify(&committed())
                .expect_err("reject unsigned")
                .to_string()
                .contains("unsigned")
        );

        let revoked = ControlAuthenticator::from_configuration(Some(&encoded), Some("primary-a"))
            .expect("revoked");
        let mut configuration = committed();
        sign(&mut configuration, &signer);
        assert!(
            revoked
                .verify(&configuration)
                .expect_err("reject revoked")
                .to_string()
                .contains("is revoked")
        );
    }

    #[test]
    fn signature_rotation_does_not_change_the_consensus_payload() {
        let signer = SigningIdentity::from_base64_seed("primary-a", SEED).expect("signer");
        let mut first = committed();
        sign(&mut first, &signer);
        let mut second = first.clone();
        second
            .authentication
            .as_mut()
            .expect("authentication")
            .key_id = "primary-b".to_owned();

        assert_ne!(first, second);
        assert!(same_routing_payload(&first, &second));
    }

    #[test]
    fn trust_ring_order_makes_key_rotation_monotonic() {
        let signer = SigningIdentity::from_base64_seed("primary-a", SEED).expect("signer");
        let public_key = signer.public_key_base64();
        let authenticator = ControlAuthenticator::from_configuration(
            Some(&format!(
                "primary-a={public_key},primary-b={public_key},primary-c={public_key}"
            )),
            None,
        )
        .expect("authenticator");

        assert_eq!(
            authenticator.key_transition(Some("primary-a"), Some("primary-b")),
            SigningKeyTransition::Upgrade
        );
        assert_eq!(
            authenticator.key_transition(Some("primary-b"), Some("primary-a")),
            SigningKeyTransition::Downgrade
        );
        assert_eq!(
            authenticator.key_transition(Some("primary-b"), Some("primary-b")),
            SigningKeyTransition::Same
        );
    }
}

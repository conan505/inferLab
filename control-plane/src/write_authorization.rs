use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use control_auth::{ControlWritePayload, RoutingWorker, TrustedWriterKeyRing};
use serde::Serialize;

use crate::model::{CommittedWriteProvenance, ConfigurationWriteRequest, RoutingConfiguration};

#[derive(Debug)]
pub struct AuthorizedProposal {
    pub configuration: RoutingConfiguration,
    pub expected_revision: Option<u64>,
    pub writer: Option<CommittedWriteProvenance>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectionKind {
    Authentication,
    Freshness,
}

#[derive(Debug)]
pub struct WriteAuthorizationError {
    pub kind: RejectionKind,
    pub writer_id: Option<String>,
    pub message: String,
}

enum Mode {
    Disabled,
    Required {
        keys: TrustedWriterKeyRing,
        max_age_ms: u64,
        max_future_skew_ms: u64,
    },
}

pub struct WriteAuthorizer {
    mode: Mode,
    verified_intents: AtomicU64,
    committed_writes: AtomicU64,
    authentication_rejections: AtomicU64,
    freshness_rejections: AtomicU64,
    revision_conflicts: AtomicU64,
    last_authorized_writer_id: Mutex<Option<String>>,
    last_rejected_writer_id: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct WriteAuthorizationStatus {
    pub required: bool,
    pub trusted_writer_ids: Vec<String>,
    pub revoked_writer_ids: Vec<String>,
    pub max_age_ms: Option<u64>,
    pub max_future_skew_ms: Option<u64>,
    pub verified_intents: u64,
    pub committed_writes: u64,
    pub authentication_rejections: u64,
    pub freshness_rejections: u64,
    pub revision_conflicts: u64,
    pub last_authorized_writer_id: Option<String>,
    pub last_rejected_writer_id: Option<String>,
    pub last_error: Option<String>,
}

impl WriteAuthorizer {
    pub fn disabled() -> Self {
        Self::new(Mode::Disabled)
    }

    pub fn required(keys: TrustedWriterKeyRing, max_age_ms: u64, max_future_skew_ms: u64) -> Self {
        Self::new(Mode::Required {
            keys,
            max_age_ms,
            max_future_skew_ms,
        })
    }

    fn new(mode: Mode) -> Self {
        Self {
            mode,
            verified_intents: AtomicU64::new(0),
            committed_writes: AtomicU64::new(0),
            authentication_rejections: AtomicU64::new(0),
            freshness_rejections: AtomicU64::new(0),
            revision_conflicts: AtomicU64::new(0),
            last_authorized_writer_id: Mutex::new(None),
            last_rejected_writer_id: Mutex::new(None),
            last_error: Mutex::new(None),
        }
    }

    pub fn authorize(
        &self,
        request: ConfigurationWriteRequest,
        cluster_id: &str,
        now_ms: u64,
    ) -> Result<AuthorizedProposal, WriteAuthorizationError> {
        match (&self.mode, request) {
            (Mode::Disabled, ConfigurationWriteRequest::Legacy(configuration)) => {
                Ok(AuthorizedProposal {
                    configuration,
                    expected_revision: None,
                    writer: None,
                })
            }
            (Mode::Disabled, ConfigurationWriteRequest::Authorized(request)) => Err(self.reject(
                RejectionKind::Authentication,
                Some(request.authorization.writer_id),
                "control write authorization is not configured; refusing an authorization envelope"
                    .to_owned(),
            )),
            (Mode::Required { .. }, ConfigurationWriteRequest::Legacy(_)) => Err(self.reject(
                RejectionKind::Authentication,
                None,
                "control write authorization is required".to_owned(),
            )),
            (
                Mode::Required {
                    keys,
                    max_age_ms,
                    max_future_skew_ms,
                },
                ConfigurationWriteRequest::Authorized(request),
            ) => {
                let writer_id = request.authorization.writer_id.clone();
                let payload = ControlWritePayload {
                    cluster_id,
                    expected_revision: request.expected_revision,
                    issued_at_ms: request.authorization.issued_at_ms,
                    nonce: &request.authorization.nonce,
                    routing_policy: &request.configuration.routing_policy,
                    workers: request
                        .configuration
                        .workers
                        .iter()
                        .map(|worker| RoutingWorker {
                            id: &worker.id,
                            base_url: &worker.base_url,
                            weight: worker.weight,
                        })
                        .collect(),
                };
                if let Err(error) = keys.verify(&payload, &request.authorization) {
                    return Err(self.reject(
                        RejectionKind::Authentication,
                        Some(writer_id),
                        error.to_string(),
                    ));
                }
                self.verified_intents.fetch_add(1, Ordering::Relaxed);

                let latest_acceptable = now_ms.saturating_add(*max_future_skew_ms);
                if request.authorization.issued_at_ms > latest_acceptable {
                    return Err(self.reject(
                        RejectionKind::Freshness,
                        Some(writer_id),
                        format!(
                            "control write was issued {} ms in the future; maximum future skew is {} ms",
                            request.authorization.issued_at_ms.saturating_sub(now_ms),
                            max_future_skew_ms
                        ),
                    ));
                }
                let age_ms = now_ms.saturating_sub(request.authorization.issued_at_ms);
                if age_ms > *max_age_ms {
                    return Err(self.reject(
                        RejectionKind::Freshness,
                        Some(writer_id),
                        format!("control write is {age_ms} ms old; maximum age is {max_age_ms} ms"),
                    ));
                }

                Ok(AuthorizedProposal {
                    configuration: request.configuration,
                    expected_revision: Some(request.expected_revision),
                    writer: Some(CommittedWriteProvenance {
                        writer_id,
                        issued_at_ms: request.authorization.issued_at_ms,
                        nonce: request.authorization.nonce,
                    }),
                })
            }
        }
    }

    pub fn record_committed(&self, writer_id: Option<&str>) {
        self.committed_writes.fetch_add(1, Ordering::Relaxed);
        if let Some(writer_id) = writer_id {
            replace(&self.last_authorized_writer_id, Some(writer_id.to_owned()));
        }
        replace(&self.last_error, None);
    }

    pub fn record_revision_conflict(&self, writer_id: Option<&str>, message: &str) {
        self.revision_conflicts.fetch_add(1, Ordering::Relaxed);
        replace(&self.last_rejected_writer_id, writer_id.map(str::to_owned));
        replace(&self.last_error, Some(message.to_owned()));
    }

    pub fn status(&self) -> WriteAuthorizationStatus {
        let (required, trusted_writer_ids, revoked_writer_ids, max_age_ms, max_future_skew_ms) =
            match &self.mode {
                Mode::Disabled => (false, Vec::new(), Vec::new(), None, None),
                Mode::Required {
                    keys,
                    max_age_ms,
                    max_future_skew_ms,
                } => (
                    true,
                    keys.trusted_writer_ids(),
                    keys.revoked_writer_ids(),
                    Some(*max_age_ms),
                    Some(*max_future_skew_ms),
                ),
            };
        WriteAuthorizationStatus {
            required,
            trusted_writer_ids,
            revoked_writer_ids,
            max_age_ms,
            max_future_skew_ms,
            verified_intents: self.verified_intents.load(Ordering::Relaxed),
            committed_writes: self.committed_writes.load(Ordering::Relaxed),
            authentication_rejections: self.authentication_rejections.load(Ordering::Relaxed),
            freshness_rejections: self.freshness_rejections.load(Ordering::Relaxed),
            revision_conflicts: self.revision_conflicts.load(Ordering::Relaxed),
            last_authorized_writer_id: clone_locked(&self.last_authorized_writer_id),
            last_rejected_writer_id: clone_locked(&self.last_rejected_writer_id),
            last_error: clone_locked(&self.last_error),
        }
    }

    fn reject(
        &self,
        kind: RejectionKind,
        writer_id: Option<String>,
        message: String,
    ) -> WriteAuthorizationError {
        match kind {
            RejectionKind::Authentication => {
                self.authentication_rejections
                    .fetch_add(1, Ordering::Relaxed);
            }
            RejectionKind::Freshness => {
                self.freshness_rejections.fetch_add(1, Ordering::Relaxed);
            }
        }
        replace(&self.last_rejected_writer_id, writer_id.clone());
        replace(&self.last_error, Some(message.clone()));
        WriteAuthorizationError {
            kind,
            writer_id,
            message,
        }
    }
}

fn replace<T>(slot: &Mutex<T>, value: T) {
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
}

fn clone_locked<T: Clone>(slot: &Mutex<T>) -> T {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthorizedRoutingConfiguration, WorkerConfiguration};
    use control_auth::{ControlWritePayload, WriterSigningIdentity};

    const SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";

    fn configuration() -> RoutingConfiguration {
        RoutingConfiguration {
            routing_policy: "round-robin".to_owned(),
            workers: vec![WorkerConfiguration {
                id: "cpu-a".to_owned(),
                base_url: "http://127.0.0.1:9904".to_owned(),
                weight: 1,
            }],
        }
    }

    fn request(issued_at_ms: u64) -> ConfigurationWriteRequest {
        let signer = WriterSigningIdentity::from_base64_seed("deploy-bot", SEED).expect("signer");
        let configuration = configuration();
        let payload = ControlWritePayload {
            cluster_id: "inferlab-primary",
            expected_revision: 0,
            issued_at_ms,
            nonce: "deploy-0000000001",
            routing_policy: &configuration.routing_policy,
            workers: configuration
                .workers
                .iter()
                .map(|worker| RoutingWorker {
                    id: &worker.id,
                    base_url: &worker.base_url,
                    weight: worker.weight,
                })
                .collect(),
        };
        ConfigurationWriteRequest::Authorized(AuthorizedRoutingConfiguration {
            expected_revision: 0,
            authorization: signer.sign(&payload).expect("authorization"),
            configuration,
        })
    }

    fn required() -> WriteAuthorizer {
        let signer = WriterSigningIdentity::from_base64_seed("deploy-bot", SEED).expect("signer");
        let keys =
            TrustedWriterKeyRing::parse(&format!("deploy-bot={}", signer.public_key_base64()), "")
                .expect("keys");
        WriteAuthorizer::required(keys, 1_000, 100)
    }

    #[test]
    fn required_mode_rejects_legacy_and_accepts_fresh_signed_intent() {
        let authorizer = required();
        let missing = authorizer
            .authorize(
                ConfigurationWriteRequest::Legacy(configuration()),
                "inferlab-primary",
                10_000,
            )
            .expect_err("reject missing authorization");
        assert_eq!(missing.kind, RejectionKind::Authentication);

        let proposal = authorizer
            .authorize(request(9_500), "inferlab-primary", 10_000)
            .expect("fresh write");
        assert_eq!(proposal.expected_revision, Some(0));
        assert_eq!(proposal.writer.expect("writer").writer_id, "deploy-bot");
        let status = authorizer.status();
        assert_eq!(status.verified_intents, 1);
        assert_eq!(status.authentication_rejections, 1);
    }

    #[test]
    fn disabled_mode_accepts_only_legacy_shape_and_never_ignores_a_claimed_signature() {
        let authorizer = WriteAuthorizer::disabled();
        let legacy = authorizer
            .authorize(
                ConfigurationWriteRequest::Legacy(configuration()),
                "inferlab-primary",
                10_000,
            )
            .expect("legacy compatibility");
        assert_eq!(legacy.expected_revision, None);
        assert_eq!(legacy.writer, None);

        let signed = authorizer
            .authorize(request(10_000), "inferlab-primary", 10_000)
            .expect_err("never silently ignore a claimed signature");
        assert_eq!(signed.kind, RejectionKind::Authentication);
        assert!(signed.message.contains("is not configured"));
    }

    #[test]
    fn stale_and_future_intents_fail_after_signature_verification() {
        let authorizer = required();
        let stale = authorizer
            .authorize(request(8_999), "inferlab-primary", 10_000)
            .expect_err("stale");
        let future = authorizer
            .authorize(request(10_101), "inferlab-primary", 10_000)
            .expect_err("future");

        assert_eq!(stale.kind, RejectionKind::Freshness);
        assert_eq!(future.kind, RejectionKind::Freshness);
        let status = authorizer.status();
        assert_eq!(status.verified_intents, 2);
        assert_eq!(status.freshness_rejections, 2);
    }
}

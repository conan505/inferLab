use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use service_auth::{
    SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1, SERVICE_TRUST_POLICY_SCHEMA,
    SERVICE_TRUST_POLICY_SCHEMA_V2, ServiceCredentialReference, ServiceSigningIdentity,
    ServiceTrustApplicationReceipt, ServiceTrustCredential, ServiceTrustPolicyPayload,
    ServiceTrustRootSigningIdentity, ServiceTrustSnapshot, TrustedServiceTrustRootKeyRing,
    VerifiedServiceTrustSnapshot,
};
use tokio::{net::TcpListener, task::JoinHandle};
use transport_security::ServerTransportStatus;
use trust_distributor::{
    DEFAULT_MAX_BODY_BYTES, DistributorConfig, MAX_BODY_BYTES, TrustDistributor, app,
    parse_expected_receivers,
};

const ROOT_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
const RECEIVER_A_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";
const RECEIVER_B_SEED: &str = "oRHYnSe9L9fS2eMjpnvZPZ7tg09poPfRXpAMlzsqHkg=";
const RECEIVER_C_SEED: &str = "H7LmO0yQx5tfsa30VuOYwgoiQavZbDaUI7DhpwgIn+U=";
static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    config: DistributorConfig,
    root: ServiceTrustRootSigningIdentity,
    receiver_a: ServiceSigningIdentity,
    receiver_b: ServiceSigningIdentity,
    receiver_c: ServiceSigningIdentity,
}

impl Fixture {
    fn new(max_body_bytes: usize) -> Self {
        let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "inferlab-trust-distributor-test-{}-{sequence}",
            std::process::id()
        ));
        let receiver_a = ServiceSigningIdentity::from_base64_seed_with_credential(
            "control-a",
            "key-a",
            RECEIVER_A_SEED,
        )
        .expect("receiver a");
        let receiver_b = ServiceSigningIdentity::from_base64_seed_with_credential(
            "control-b",
            "key-a",
            RECEIVER_B_SEED,
        )
        .expect("receiver b");
        let receiver_c = ServiceSigningIdentity::from_base64_seed_with_credential(
            "control-c",
            "key-a",
            RECEIVER_C_SEED,
        )
        .expect("receiver c");
        Self {
            config: DistributorConfig {
                cluster_id: "inferlab-primary".to_owned(),
                state_path: directory.join("state.json"),
                expected_receivers: BTreeSet::from([
                    "control-a/key-a".to_owned(),
                    "control-b/key-a".to_owned(),
                ]),
                max_body_bytes,
                transport_security: ServerTransportStatus::Http,
            },
            directory,
            root: ServiceTrustRootSigningIdentity::from_base64_seed("root-a", ROOT_SEED)
                .expect("root"),
            receiver_a,
            receiver_b,
            receiver_c,
        }
    }

    fn roots(&self) -> TrustedServiceTrustRootKeyRing {
        TrustedServiceTrustRootKeyRing::parse(
            &format!("root-a={}", self.root.public_key_base64()),
            "",
        )
        .expect("roots")
    }

    fn snapshot(&self, generation: u64, issued_at_ms: u64) -> ServiceTrustSnapshot {
        self.root
            .sign(&ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                generation,
                issued_at_ms,
                expires_at_ms: None,
                trusted_credentials: vec![
                    ServiceTrustCredential {
                        service_id: "control-a".to_owned(),
                        credential_id: "key-a".to_owned(),
                        public_key_base64: self.receiver_a.public_key_base64(),
                    },
                    ServiceTrustCredential {
                        service_id: "control-b".to_owned(),
                        credential_id: "key-a".to_owned(),
                        public_key_base64: self.receiver_b.public_key_base64(),
                    },
                    ServiceTrustCredential {
                        service_id: "control-c".to_owned(),
                        credential_id: "key-a".to_owned(),
                        public_key_base64: self.receiver_c.public_key_base64(),
                    },
                ],
                revoked_service_ids: Vec::new(),
                revoked_credentials: Vec::new(),
                gateway_service_ids: vec!["control-a".to_owned()],
            })
            .expect("snapshot")
    }

    fn snapshot_without_receiver_b(&self) -> ServiceTrustSnapshot {
        self.root
            .sign(&ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                generation: 1,
                issued_at_ms: 1_700_000_000_000,
                expires_at_ms: None,
                trusted_credentials: vec![ServiceTrustCredential {
                    service_id: "control-a".to_owned(),
                    credential_id: "key-a".to_owned(),
                    public_key_base64: self.receiver_a.public_key_base64(),
                }],
                revoked_service_ids: Vec::new(),
                revoked_credentials: Vec::new(),
                gateway_service_ids: vec!["control-a".to_owned()],
            })
            .expect("incomplete convergence snapshot")
    }

    fn snapshot_v2(
        &self,
        generation: u64,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> ServiceTrustSnapshot {
        let mut policy = self.snapshot(generation, issued_at_ms).policy;
        policy.schema = SERVICE_TRUST_POLICY_SCHEMA_V2.to_owned();
        policy.expires_at_ms = Some(expires_at_ms);
        self.root.sign(&policy).expect("v2 snapshot")
    }

    fn snapshot_with_revoked_receiver_b(&self) -> ServiceTrustSnapshot {
        let mut snapshot = self.snapshot(1, 1_700_000_000_000).policy;
        snapshot
            .revoked_credentials
            .push(ServiceCredentialReference {
                service_id: "control-b".to_owned(),
                credential_id: "key-a".to_owned(),
            });
        self.root
            .sign(&snapshot)
            .expect("revoked receiver snapshot")
    }

    fn verified(&self, snapshot: &ServiceTrustSnapshot) -> VerifiedServiceTrustSnapshot {
        self.roots().verify(snapshot).expect("verified snapshot")
    }

    fn open(&self) -> TrustDistributor {
        TrustDistributor::open(self.config.clone(), self.roots()).expect("distributor")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

async fn serve(distributor: TrustDistributor) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app(distributor))
            .await
            .expect("serve distributor");
    });
    (format!("http://{address}"), task)
}

async fn json(response: reqwest::Response) -> Value {
    response.json().await.expect("JSON response")
}

#[tokio::test]
async fn publishes_caches_acknowledges_and_recovers_durable_state() {
    let fixture = Fixture::new(DEFAULT_MAX_BODY_BYTES);
    let generation_one = fixture.snapshot(1, 1_700_000_000_001);
    let distributor = fixture.open();
    let (base, task) = serve(distributor).await;
    let client = Client::new();

    assert_eq!(
        client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("ready before")
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/service-trust/snapshot"))
            .send()
            .await
            .expect("get before")
            .status(),
        StatusCode::NOT_FOUND
    );

    let published = client
        .post(format!("{base}/v1/service-trust/snapshot"))
        .json(&generation_one)
        .send()
        .await
        .expect("publish");
    assert_eq!(published.status(), StatusCode::CREATED);
    assert_eq!(json(published).await["outcome"], "published");
    assert_eq!(
        client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("ready after")
            .status(),
        StatusCode::OK
    );

    let fetched = client
        .get(format!("{base}/v1/service-trust/snapshot"))
        .send()
        .await
        .expect("fetch");
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(fetched.headers()[header::CACHE_CONTROL], "no-cache");
    let etag = fetched.headers()[header::ETAG]
        .to_str()
        .expect("etag")
        .to_owned();
    assert_eq!(
        fetched
            .json::<ServiceTrustSnapshot>()
            .await
            .expect("snapshot body"),
        generation_one
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/service-trust/snapshot"))
            .header(header::IF_NONE_MATCH, &etag)
            .send()
            .await
            .expect("conditional fetch")
            .status(),
        StatusCode::NOT_MODIFIED
    );

    let unchanged = client
        .post(format!("{base}/v1/service-trust/snapshot"))
        .json(&generation_one)
        .send()
        .await
        .expect("idempotent publish");
    assert_eq!(unchanged.status(), StatusCode::OK);
    assert_eq!(json(unchanged).await["outcome"], "unchanged");

    let receipt = fixture
        .receiver_a
        .sign_trust_receipt(&fixture.verified(&generation_one), 1_700_000_000_101)
        .expect("receipt");
    let recorded = client
        .post(format!("{base}/v1/service-trust/receipts"))
        .json(&receipt)
        .send()
        .await
        .expect("post receipt");
    assert_eq!(recorded.status(), StatusCode::CREATED);
    assert_eq!(json(recorded).await["outcome"], "recorded");
    let duplicate = client
        .post(format!("{base}/v1/service-trust/receipts"))
        .json(&receipt)
        .send()
        .await
        .expect("duplicate receipt");
    assert_eq!(duplicate.status(), StatusCode::OK);
    assert_eq!(json(duplicate).await["outcome"], "duplicate");

    let status = json(
        client
            .get(format!("{base}/v1/service-trust/status"))
            .send()
            .await
            .expect("status"),
    )
    .await;
    assert_eq!(status["snapshot"]["generation"], 1);
    assert_eq!(
        status["snapshot"]["policy_schema"],
        SERVICE_TRUST_POLICY_SCHEMA
    );
    assert_eq!(status["snapshot"]["expires_at_ms"], Value::Null);
    assert_eq!(status["receipt_count"], 1);
    assert_eq!(
        status["acked_receivers"],
        serde_json::json!(["control-a/key-a"])
    );
    assert_eq!(
        status["pending_receivers"],
        serde_json::json!(["control-b/key-a"])
    );
    assert_eq!(status["storage"]["mutation_poisoned"], false);
    assert_eq!(status["transport_security"]["mode"], "insecure-http");
    assert_eq!(
        status["transport_security"]["client_certificate_required"],
        false
    );
    assert_eq!(
        status["transport_security"]["minimum_protocol"],
        Value::Null
    );
    let auditable_receipts =
        serde_json::from_value::<Vec<ServiceTrustApplicationReceipt>>(status["receipts"].clone())
            .expect("signed receipts in status");
    assert_eq!(auditable_receipts, vec![receipt.clone()]);
    fixture
        .verified(&generation_one)
        .compiled
        .keys
        .verify_trust_receipt(&auditable_receipts[0])
        .expect("status receipt remains independently verifiable");
    assert_eq!(
        auditable_receipts[0].payload.cluster_id,
        generation_one.policy.cluster_id
    );
    assert_eq!(
        auditable_receipts[0].payload.generation,
        generation_one.policy.generation
    );
    assert_eq!(
        auditable_receipts[0].payload.root_key_id,
        generation_one.authentication.key_id
    );
    assert_eq!(
        auditable_receipts[0].payload.snapshot_signature,
        generation_one.authentication.signature
    );

    task.abort();
    let restarted = fixture.open();
    let (base, restarted_task) = serve(restarted).await;
    let recovered = json(
        client
            .get(format!("{base}/v1/service-trust/status"))
            .send()
            .await
            .expect("recovered status"),
    )
    .await;
    assert_eq!(recovered["snapshot"]["generation"], 1);
    assert_eq!(
        recovered["acked_receivers"],
        serde_json::json!(["control-a/key-a"])
    );
    let receipt_b = fixture
        .receiver_b
        .sign_trust_receipt(&fixture.verified(&generation_one), 1_700_000_000_102)
        .expect("receipt b");
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/receipts"))
            .json(&receipt_b)
            .send()
            .await
            .expect("post receipt b")
            .status(),
        StatusCode::CREATED
    );
    let converged = json(
        client
            .get(format!("{base}/v1/service-trust/status"))
            .send()
            .await
            .expect("converged status"),
    )
    .await;
    assert_eq!(converged["receipt_count"], 2);
    assert_eq!(
        converged["acked_receivers"],
        serde_json::json!(["control-a/key-a", "control-b/key-a"])
    );
    assert_eq!(converged["pending_receivers"], serde_json::json!([]));
    let converged_receipts = serde_json::from_value::<Vec<ServiceTrustApplicationReceipt>>(
        converged["receipts"].clone(),
    )
    .expect("converged signed receipts");
    assert_eq!(converged_receipts.len(), 2);
    for receipt in &converged_receipts {
        fixture
            .verified(&generation_one)
            .compiled
            .keys
            .verify_trust_receipt(receipt)
            .expect("every convergence receipt is auditable");
    }
    restarted_task.abort();
}

#[tokio::test]
async fn transports_persists_and_reports_v2_without_claiming_receiver_validity() {
    let fixture = Fixture::new(DEFAULT_MAX_BODY_BYTES);
    let expired_for_any_real_clock = fixture.snapshot_v2(1, 1, 2);
    let distributor = fixture.open();
    let (base, task) = serve(distributor).await;
    let client = Client::new();

    let published = client
        .post(format!("{base}/v1/service-trust/snapshot"))
        .json(&expired_for_any_real_clock)
        .send()
        .await
        .expect("publish v2");
    assert_eq!(published.status(), StatusCode::CREATED);

    let receipt = fixture
        .receiver_a
        .sign_trust_receipt(&fixture.verified(&expired_for_any_real_clock), 3)
        .expect("v2 receipt");
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/receipts"))
            .json(&receipt)
            .send()
            .await
            .expect("post v2 receipt")
            .status(),
        StatusCode::CREATED
    );

    let status = json(
        client
            .get(format!("{base}/v1/service-trust/status"))
            .send()
            .await
            .expect("v2 status"),
    )
    .await;
    assert_eq!(
        status["snapshot"]["policy_schema"],
        SERVICE_TRUST_POLICY_SCHEMA_V2
    );
    assert_eq!(status["snapshot"]["issued_at_ms"], 1);
    assert_eq!(status["snapshot"]["expires_at_ms"], 2);
    assert!(status["snapshot"].get("valid").is_none());
    assert!(status["snapshot"].get("validity").is_none());
    assert!(status["snapshot"].get("remaining_ms").is_none());
    assert_eq!(status["receipt_count"], 1);

    task.abort();
    let restarted = fixture.open();
    let (base, restarted_task) = serve(restarted).await;
    let recovered = json(
        client
            .get(format!("{base}/v1/service-trust/status"))
            .send()
            .await
            .expect("recovered v2 status"),
    )
    .await;
    assert_eq!(recovered["snapshot"]["expires_at_ms"], 2);
    assert_eq!(recovered["receipt_count"], 1);
    restarted_task.abort();
}

#[tokio::test]
async fn rejects_mixed_v1_v2_schemas_and_expiry_tampering() {
    let fixture = Fixture::new(DEFAULT_MAX_BODY_BYTES);
    let snapshot = fixture.snapshot_v2(1, 1_700_000_000_000, 1_700_000_060_000);
    let (base, task) = serve(fixture.open()).await;
    let client = Client::new();

    let mut mixed = snapshot.clone();
    mixed.authentication.schema = SERVICE_TRUST_AUTHENTICATION_SCHEMA_V1.to_owned();
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&mixed)
            .send()
            .await
            .expect("mixed schema")
            .status(),
        StatusCode::BAD_REQUEST
    );

    let mut altered_expiry = snapshot.clone();
    altered_expiry.policy.expires_at_ms = Some(1_700_000_060_001);
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&altered_expiry)
            .send()
            .await
            .expect("altered expiry")
            .status(),
        StatusCode::BAD_REQUEST
    );

    let mut missing_expiry = snapshot;
    missing_expiry.policy.expires_at_ms = None;
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&missing_expiry)
            .send()
            .await
            .expect("missing expiry")
            .status(),
        StatusCode::BAD_REQUEST
    );
    task.abort();
}

#[tokio::test]
async fn rejects_tamper_rollback_forks_stale_receipts_and_unexpected_receivers() {
    let fixture = Fixture::new(DEFAULT_MAX_BODY_BYTES);
    let generation_one = fixture.snapshot(1, 1_700_000_000_001);
    let generation_two = fixture.snapshot(2, 1_700_000_000_002);
    let same_generation_fork = fixture.snapshot(2, 1_700_000_000_099);
    let (base, task) = serve(fixture.open()).await;
    let client = Client::new();

    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&fixture.snapshot_without_receiver_b())
            .send()
            .await
            .expect("snapshot missing expected receiver")
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&fixture.snapshot_with_revoked_receiver_b())
            .send()
            .await
            .expect("snapshot revoking expected receiver")
            .status(),
        StatusCode::BAD_REQUEST
    );

    let mut tampered = generation_one.clone();
    tampered.policy.issued_at_ms += 1;
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&tampered)
            .send()
            .await
            .expect("tampered snapshot")
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&generation_one)
            .send()
            .await
            .expect("generation one")
            .status(),
        StatusCode::CREATED
    );
    let stale_receipt = fixture
        .receiver_a
        .sign_trust_receipt(&fixture.verified(&generation_one), 1_700_000_000_101)
        .expect("stale receipt");
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&generation_two)
            .send()
            .await
            .expect("generation two")
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&generation_one)
            .send()
            .await
            .expect("rollback")
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&same_generation_fork)
            .send()
            .await
            .expect("fork")
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/receipts"))
            .json(&stale_receipt)
            .send()
            .await
            .expect("stale receipt")
            .status(),
        StatusCode::CONFLICT
    );

    let unexpected = fixture
        .receiver_c
        .sign_trust_receipt(&fixture.verified(&generation_two), 1_700_000_000_202)
        .expect("unexpected receipt");
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/receipts"))
            .json(&unexpected)
            .send()
            .await
            .expect("unexpected receiver")
            .status(),
        StatusCode::FORBIDDEN
    );

    let mut bad_signature = fixture
        .receiver_a
        .sign_trust_receipt(&fixture.verified(&generation_two), 1_700_000_000_203)
        .expect("receipt");
    bad_signature.payload.applied_at_ms += 1;
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/receipts"))
            .json(&bad_signature)
            .send()
            .await
            .expect("invalid signature")
            .status(),
        StatusCode::BAD_REQUEST
    );

    let status = json(
        client
            .get(format!("{base}/v1/service-trust/status"))
            .send()
            .await
            .expect("status"),
    )
    .await;
    assert_eq!(status["snapshot"]["generation"], 2);
    assert_eq!(status["receipt_count"], 0);
    task.abort();
}

#[tokio::test]
async fn request_and_configuration_bounds_are_enforced() {
    let fixture = Fixture::new(256);
    let (base, task) = serve(fixture.open()).await;
    assert_eq!(
        Client::new()
            .post(format!("{base}/v1/service-trust/snapshot"))
            .body(vec![b'x'; 257])
            .send()
            .await
            .expect("oversized request")
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    task.abort();

    assert!(parse_expected_receivers("control-a/key-a,control-a/key-a").is_err());
    let too_many = (0..257)
        .map(|index| format!("control-{index}/key-a"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_expected_receivers(&too_many).is_err());

    let mut invalid = fixture.config.clone();
    invalid.max_body_bytes = MAX_BODY_BYTES + 1;
    assert!(TrustDistributor::open(invalid, fixture.roots()).is_err());
}

#[tokio::test]
async fn corrupt_durable_snapshot_or_receipt_fails_closed_on_restart() {
    let fixture = Fixture::new(DEFAULT_MAX_BODY_BYTES);
    let snapshot = fixture.snapshot(1, 1_700_000_000_001);
    let (base, task) = serve(fixture.open()).await;
    let client = Client::new();
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/snapshot"))
            .json(&snapshot)
            .send()
            .await
            .expect("publish")
            .status(),
        StatusCode::CREATED
    );
    let receipt = fixture
        .receiver_a
        .sign_trust_receipt(&fixture.verified(&snapshot), 1_700_000_000_101)
        .expect("receipt");
    assert_eq!(
        client
            .post(format!("{base}/v1/service-trust/receipts"))
            .json(&receipt)
            .send()
            .await
            .expect("receipt post")
            .status(),
        StatusCode::CREATED
    );
    task.abort();

    let mut state: Value = serde_json::from_slice(
        &std::fs::read(&fixture.config.state_path).expect("read durable state"),
    )
    .expect("decode durable state");
    state["receipts"][0]["applied_at_ms"] = Value::from(1_700_000_000_999_u64);
    std::fs::write(
        &fixture.config.state_path,
        serde_json::to_vec(&state).expect("encode tamper"),
    )
    .expect("write tamper");
    assert!(
        TrustDistributor::open(fixture.config.clone(), fixture.roots()).is_err(),
        "receipt tamper must fail restart"
    );
}

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(test)]
use std::sync::atomic::AtomicBool;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use service_auth::{
    ServiceTrustApplicationReceipt, ServiceTrustSnapshot, TrustedServiceTrustRootKeyRing,
    VerifiedServiceTrustSnapshot,
};
use tokio::{
    sync::{Mutex, OwnedMutexGuard},
    task,
};
use tracing::info;
use transport_security::ServerTransportStatus;

mod metrics;
pub use metrics::TrustDistributorMetrics;

pub const STATUS_SCHEMA: &str = "inferlab.trust-distributor-status.v1";
pub const PUBLISH_SCHEMA: &str = "inferlab.trust-distributor-publish.v1";
pub const RECEIPT_ACCEPTANCE_SCHEMA: &str = "inferlab.trust-distributor-receipt-acceptance.v1";
const STATE_SCHEMA: &str = "inferlab.trust-distributor-state.v1";
const MAX_EXPECTED_RECEIVERS: usize = 256;
pub const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_ATOMIC_TEMP_ATTEMPTS: usize = 128;
static NEXT_STATE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct DistributorConfig {
    pub cluster_id: String,
    pub state_path: PathBuf,
    pub expected_receivers: BTreeSet<String>,
    pub max_body_bytes: usize,
    pub transport_security: ServerTransportStatus,
}

impl DistributorConfig {
    pub fn validate(&self) -> Result<(), DistributorError> {
        service_auth::validate_service_id(&self.cluster_id)
            .map_err(|error| DistributorError::configuration(error.to_string()))?;
        if self.state_path.as_os_str().is_empty() {
            return Err(DistributorError::configuration(
                "trust-distributor state path cannot be empty",
            ));
        }
        if self.expected_receivers.is_empty() {
            return Err(DistributorError::configuration(
                "at least one expected receiver is required",
            ));
        }
        if self.expected_receivers.len() > MAX_EXPECTED_RECEIVERS {
            return Err(DistributorError::configuration(format!(
                "expected receivers exceed the {MAX_EXPECTED_RECEIVERS}-receiver bound"
            )));
        }
        for receiver in &self.expected_receivers {
            validate_qualified_receiver(receiver)?;
        }
        if !(1..=MAX_BODY_BYTES).contains(&self.max_body_bytes) {
            return Err(DistributorError::configuration(format!(
                "request body bound must be between 1 and {MAX_BODY_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DurableState {
    schema: String,
    current_snapshot: Option<ServiceTrustSnapshot>,
    receipts: Vec<ServiceTrustApplicationReceipt>,
}

impl Default for DurableState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA.to_owned(),
            current_snapshot: None,
            receipts: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct RuntimeState {
    durable: DurableState,
    mutation_poison: Option<&'static str>,
}

const MUTATION_DURABILITY_UNCERTAIN: &str = "durability_uncertain_after_replace";
const MUTATION_STORAGE_TASK_FAILED: &str = "storage_task_failed";

#[derive(Debug)]
pub struct DistributorError {
    message: String,
}

impl DistributorError {
    fn configuration(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn storage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DistributorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DistributorError {}

#[derive(Clone)]
pub struct TrustDistributor {
    inner: Arc<Inner>,
}

struct Inner {
    config: DistributorConfig,
    roots: TrustedServiceTrustRootKeyRing,
    state: Arc<Mutex<RuntimeState>>,
    metrics: DistributorMetrics,
    #[cfg(test)]
    fail_next_directory_sync: AtomicBool,
}

#[derive(Debug, Default)]
struct DistributorMetrics {
    snapshot_served: AtomicU64,
    snapshot_not_modified: AtomicU64,
    snapshot_unavailable: AtomicU64,
    snapshot_published: AtomicU64,
    snapshot_unchanged: AtomicU64,
    snapshot_rejected: AtomicU64,
    snapshot_storage_errors: AtomicU64,
    receipts_recorded: AtomicU64,
    receipts_duplicate: AtomicU64,
    receipts_rejected: AtomicU64,
    receipt_storage_errors: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrustDistributorMetricsSnapshot {
    pub snapshot_served: u64,
    pub snapshot_not_modified: u64,
    pub snapshot_unavailable: u64,
    pub snapshot_published: u64,
    pub snapshot_unchanged: u64,
    pub snapshot_rejected: u64,
    pub snapshot_storage_errors: u64,
    pub receipts_recorded: u64,
    pub receipts_duplicate: u64,
    pub receipts_rejected: u64,
    pub receipt_storage_errors: u64,
    pub generation: u64,
    pub expected_receivers: u64,
    pub acked_receivers: u64,
    pub pending_receivers: u64,
    pub storage_healthy: bool,
}

impl TrustDistributor {
    pub fn open(
        config: DistributorConfig,
        roots: TrustedServiceTrustRootKeyRing,
    ) -> Result<Self, DistributorError> {
        config.validate()?;
        let state = load_state(&config.state_path, config.max_body_bytes)?;
        validate_durable_state(&config, &roots, &state)?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                roots,
                state: Arc::new(Mutex::new(RuntimeState {
                    durable: state,
                    mutation_poison: None,
                })),
                metrics: DistributorMetrics::default(),
                #[cfg(test)]
                fail_next_directory_sync: AtomicBool::new(false),
            }),
        })
    }

    pub async fn has_snapshot(&self) -> bool {
        self.state().await.durable.current_snapshot.is_some()
    }

    /// Captures bounded scalar state for a scrape without cloning a snapshot or
    /// any signed receipt. Encoding happens after the async state lock is gone.
    pub async fn metrics_snapshot(&self) -> TrustDistributorMetricsSnapshot {
        let state = self.state().await;
        self.metrics_snapshot_from_state(&state)
    }

    /// Best-effort nonblocking variant used by the synchronous OpenMetrics
    /// encoder. A concurrent durable mutation leaves the previous scrape value
    /// intact instead of blocking the mutation or entering async work.
    pub fn try_metrics_snapshot(&self) -> Option<TrustDistributorMetricsSnapshot> {
        let state = self.inner.state.try_lock().ok()?;
        Some(self.metrics_snapshot_from_state(&state))
    }

    fn metrics_snapshot_from_state(&self, state: &RuntimeState) -> TrustDistributorMetricsSnapshot {
        let expected =
            u64::try_from(self.inner.config.expected_receivers.len()).unwrap_or(u64::MAX);
        let acked = u64::try_from(state.durable.receipts.len()).unwrap_or(u64::MAX);
        TrustDistributorMetricsSnapshot {
            snapshot_served: self.inner.metrics.snapshot_served.load(Ordering::Relaxed),
            snapshot_not_modified: self
                .inner
                .metrics
                .snapshot_not_modified
                .load(Ordering::Relaxed),
            snapshot_unavailable: self
                .inner
                .metrics
                .snapshot_unavailable
                .load(Ordering::Relaxed),
            snapshot_published: self
                .inner
                .metrics
                .snapshot_published
                .load(Ordering::Relaxed),
            snapshot_unchanged: self
                .inner
                .metrics
                .snapshot_unchanged
                .load(Ordering::Relaxed),
            snapshot_rejected: self.inner.metrics.snapshot_rejected.load(Ordering::Relaxed),
            snapshot_storage_errors: self
                .inner
                .metrics
                .snapshot_storage_errors
                .load(Ordering::Relaxed),
            receipts_recorded: self.inner.metrics.receipts_recorded.load(Ordering::Relaxed),
            receipts_duplicate: self
                .inner
                .metrics
                .receipts_duplicate
                .load(Ordering::Relaxed),
            receipts_rejected: self.inner.metrics.receipts_rejected.load(Ordering::Relaxed),
            receipt_storage_errors: self
                .inner
                .metrics
                .receipt_storage_errors
                .load(Ordering::Relaxed),
            generation: state
                .durable
                .current_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.policy.generation),
            expected_receivers: expected,
            acked_receivers: acked,
            pending_receivers: expected.saturating_sub(acked),
            storage_healthy: state.mutation_poison.is_none(),
        }
    }

    async fn state(&self) -> tokio::sync::MutexGuard<'_, RuntimeState> {
        self.inner.state.lock().await
    }

    async fn owned_state(&self) -> OwnedMutexGuard<RuntimeState> {
        Arc::clone(&self.inner.state).lock_owned().await
    }

    async fn persist_next(
        &self,
        mut state: OwnedMutexGuard<RuntimeState>,
        next: DurableState,
    ) -> Result<(), ApiError> {
        let path = self.inner.config.state_path.clone();
        let persisted = next.clone();
        #[cfg(test)]
        let fail_directory_sync = self
            .inner
            .fail_next_directory_sync
            .swap(false, Ordering::SeqCst);
        #[cfg(not(test))]
        let fail_directory_sync = false;
        let result = task::spawn_blocking(move || {
            match persist_state(&path, &persisted, fail_directory_sync) {
                Ok(()) => {
                    state.durable = next;
                    Ok(())
                }
                Err(error) if error.phase == PersistenceFailurePhase::AfterReplace => {
                    state.durable = next;
                    state.mutation_poison = Some(MUTATION_DURABILITY_UNCERTAIN);
                    Err(())
                }
                Err(_) => Err(()),
            }
        })
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(ApiError::internal()),
            Err(_) => {
                let mut state = self.owned_state().await;
                state.mutation_poison = Some(MUTATION_STORAGE_TASK_FAILED);
                Err(ApiError::internal())
            }
        }
    }

    #[cfg(test)]
    fn fail_next_directory_sync_for_test(&self) {
        self.inner
            .fail_next_directory_sync
            .store(true, Ordering::SeqCst);
    }

    fn verify_snapshot(
        &self,
        snapshot: &ServiceTrustSnapshot,
    ) -> Result<VerifiedServiceTrustSnapshot, ApiError> {
        let verified = self.inner.roots.verify(snapshot).map_err(|error| {
            ApiError::bad_request("invalid_snapshot", format!("snapshot rejected: {error}"))
        })?;
        if verified.policy.cluster_id != self.inner.config.cluster_id {
            return Err(ApiError::bad_request(
                "cluster_mismatch",
                "snapshot cluster does not match the distributor cluster",
            ));
        }
        validate_expected_receivers(&self.inner.config, &verified)
            .map_err(|message| ApiError::bad_request("untrusted_expected_receiver", message))?;
        Ok(verified)
    }
}

pub fn app(distributor: TrustDistributor) -> Router {
    let max_body_bytes = distributor.inner.config.max_body_bytes;
    Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readiness))
        .route("/v1/service-trust/status", get(status))
        .route(
            "/v1/service-trust/snapshot",
            get(get_snapshot).post(publish_snapshot),
        )
        .route("/v1/service-trust/receipts", post(post_receipt))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(distributor)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "service": "inferlab-trust-distributor"}))
}

async fn readiness(State(distributor): State<TrustDistributor>) -> Response {
    let state = distributor.state().await;
    let snapshot_available = state.durable.current_snapshot.is_some();
    if let Some(error_code) = state.mutation_poison {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not-ready",
                "reason": "storage_mutation_poisoned",
                "snapshot_available": snapshot_available,
                "storage_error_code": error_code,
            })),
        )
            .into_response();
    }
    match snapshot_available {
        true => (
            StatusCode::OK,
            Json(json!({"status": "ready", "snapshot_available": true})),
        )
            .into_response(),
        false => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not-ready",
                "reason": "snapshot_unavailable",
                "snapshot_available": false
            })),
        )
            .into_response(),
    }
}

async fn get_snapshot(State(distributor): State<TrustDistributor>, headers: HeaderMap) -> Response {
    let state = distributor.state().await;
    let Some(snapshot) = state.durable.current_snapshot.as_ref() else {
        distributor
            .inner
            .metrics
            .snapshot_unavailable
            .fetch_add(1, Ordering::Relaxed);
        return ApiError::new(
            StatusCode::NOT_FOUND,
            "snapshot_unavailable",
            "no service-trust snapshot has been published",
        )
        .into_response();
    };
    let etag = snapshot_etag(snapshot);
    if if_none_match(&headers, &etag) {
        distributor
            .inner
            .metrics
            .snapshot_not_modified
            .fetch_add(1, Ordering::Relaxed);
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        insert_snapshot_cache_headers(response.headers_mut(), &etag);
        return response;
    }
    distributor
        .inner
        .metrics
        .snapshot_served
        .fetch_add(1, Ordering::Relaxed);
    let mut response = Json(snapshot.clone()).into_response();
    insert_snapshot_cache_headers(response.headers_mut(), &etag);
    response
}

async fn publish_snapshot(State(distributor): State<TrustDistributor>, body: Bytes) -> Response {
    let snapshot = match serde_json::from_slice::<ServiceTrustSnapshot>(&body) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            distributor
                .inner
                .metrics
                .snapshot_rejected
                .fetch_add(1, Ordering::Relaxed);
            return ApiError::bad_request(
                "invalid_json",
                "request body must contain one complete service-trust snapshot",
            )
            .into_response();
        }
    };
    let verified = match distributor.verify_snapshot(&snapshot) {
        Ok(verified) => verified,
        Err(error) => {
            distributor
                .inner
                .metrics
                .snapshot_rejected
                .fetch_add(1, Ordering::Relaxed);
            return error.into_response();
        }
    };
    let state = distributor.owned_state().await;
    if state.mutation_poison.is_some() {
        distributor
            .inner
            .metrics
            .snapshot_storage_errors
            .fetch_add(1, Ordering::Relaxed);
        return ApiError::mutation_poisoned().into_response();
    }
    if let Some(current) = state.durable.current_snapshot.as_ref() {
        let current_generation = current.policy.generation;
        if verified.policy.generation < current_generation {
            distributor
                .inner
                .metrics
                .snapshot_rejected
                .fetch_add(1, Ordering::Relaxed);
            return ApiError::new(
                StatusCode::CONFLICT,
                "snapshot_rollback",
                "snapshot generation is below the durable current generation",
            )
            .into_response();
        }
        if verified.policy.generation == current_generation {
            if current == &snapshot {
                distributor
                    .inner
                    .metrics
                    .snapshot_unchanged
                    .fetch_add(1, Ordering::Relaxed);
                return publish_response(StatusCode::OK, "unchanged", &snapshot);
            }
            distributor
                .inner
                .metrics
                .snapshot_rejected
                .fetch_add(1, Ordering::Relaxed);
            return ApiError::new(
                StatusCode::CONFLICT,
                "snapshot_fork",
                "a different valid snapshot already occupies this generation",
            )
            .into_response();
        }
    }

    let mut next = state.durable.clone();
    next.current_snapshot = Some(snapshot.clone());
    next.receipts.clear();
    if let Err(error) = distributor.persist_next(state, next).await {
        distributor
            .inner
            .metrics
            .snapshot_storage_errors
            .fetch_add(1, Ordering::Relaxed);
        return error.into_response();
    }
    distributor
        .inner
        .metrics
        .snapshot_published
        .fetch_add(1, Ordering::Relaxed);
    info!(
        cluster_id = %snapshot.policy.cluster_id,
        generation = snapshot.policy.generation,
        root_key_id = %snapshot.authentication.key_id,
        "published service-trust snapshot"
    );
    publish_response(StatusCode::CREATED, "published", &snapshot)
}

async fn post_receipt(State(distributor): State<TrustDistributor>, body: Bytes) -> Response {
    let receipt = match serde_json::from_slice::<ServiceTrustApplicationReceipt>(&body) {
        Ok(receipt) => receipt,
        Err(_) => {
            distributor
                .inner
                .metrics
                .receipts_rejected
                .fetch_add(1, Ordering::Relaxed);
            return ApiError::bad_request(
                "invalid_json",
                "request body must contain one signed service-trust receipt",
            )
            .into_response();
        }
    };
    let receiver = format!(
        "{}/{}",
        receipt.payload.receiver_service_id, receipt.payload.receiver_credential_id
    );
    let state = distributor.owned_state().await;
    if state.mutation_poison.is_some() {
        distributor
            .inner
            .metrics
            .receipt_storage_errors
            .fetch_add(1, Ordering::Relaxed);
        return ApiError::mutation_poisoned().into_response();
    }
    let Some(snapshot) = state.durable.current_snapshot.as_ref() else {
        distributor
            .inner
            .metrics
            .receipts_rejected
            .fetch_add(1, Ordering::Relaxed);
        return ApiError::new(
            StatusCode::NOT_FOUND,
            "snapshot_unavailable",
            "a snapshot must be published before receipts are accepted",
        )
        .into_response();
    };
    if !distributor
        .inner
        .config
        .expected_receivers
        .contains(&receiver)
    {
        distributor
            .inner
            .metrics
            .receipts_rejected
            .fetch_add(1, Ordering::Relaxed);
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "unexpected_receiver",
            "receipt receiver is outside the configured convergence set",
        )
        .into_response();
    }
    if !receipt_matches_snapshot(&receipt, snapshot) {
        distributor
            .inner
            .metrics
            .receipts_rejected
            .fetch_add(1, Ordering::Relaxed);
        return ApiError::new(
            StatusCode::CONFLICT,
            "receipt_snapshot_mismatch",
            "receipt does not identify the exact current snapshot",
        )
        .into_response();
    }
    let verified_snapshot = match distributor.verify_snapshot(snapshot) {
        Ok(verified) => verified,
        Err(_) => {
            distributor
                .inner
                .metrics
                .receipt_storage_errors
                .fetch_add(1, Ordering::Relaxed);
            return ApiError::internal().into_response();
        }
    };
    if let Err(error) = verified_snapshot
        .compiled
        .keys
        .verify_trust_receipt(&receipt)
    {
        distributor
            .inner
            .metrics
            .receipts_rejected
            .fetch_add(1, Ordering::Relaxed);
        return ApiError::bad_request(
            "invalid_receipt_signature",
            format!("receipt rejected: {error}"),
        )
        .into_response();
    }
    if state.durable.receipts.iter().any(|existing| {
        existing.payload.receiver_service_id == receipt.payload.receiver_service_id
            && existing.payload.receiver_credential_id == receipt.payload.receiver_credential_id
    }) {
        distributor
            .inner
            .metrics
            .receipts_duplicate
            .fetch_add(1, Ordering::Relaxed);
        return receipt_response(StatusCode::OK, "duplicate", &receipt);
    }

    let mut next = state.durable.clone();
    next.receipts.push(receipt.clone());
    if let Err(error) = distributor.persist_next(state, next).await {
        distributor
            .inner
            .metrics
            .receipt_storage_errors
            .fetch_add(1, Ordering::Relaxed);
        return error.into_response();
    }
    distributor
        .inner
        .metrics
        .receipts_recorded
        .fetch_add(1, Ordering::Relaxed);
    info!(
        cluster_id = %receipt.payload.cluster_id,
        generation = receipt.payload.generation,
        receiver = %receiver,
        "recorded service-trust convergence receipt"
    );
    receipt_response(StatusCode::CREATED, "recorded", &receipt)
}

async fn status(State(distributor): State<TrustDistributor>) -> Response {
    let state = distributor.state().await;
    let acked = state
        .durable
        .receipts
        .iter()
        .map(|receipt| {
            format!(
                "{}/{}",
                receipt.payload.receiver_service_id, receipt.payload.receiver_credential_id
            )
        })
        .collect::<BTreeSet<_>>();
    let pending = distributor
        .inner
        .config
        .expected_receivers
        .difference(&acked)
        .cloned()
        .collect::<Vec<_>>();
    let snapshot = state.durable.current_snapshot.as_ref().map(|snapshot| {
        json!({
            "generation": snapshot.policy.generation,
            "issued_at_ms": snapshot.policy.issued_at_ms,
            "root_key_id": snapshot.authentication.key_id,
            "etag": snapshot_etag(snapshot),
        })
    });
    Json(json!({
        "schema": STATUS_SCHEMA,
        "cluster_id": distributor.inner.config.cluster_id,
        "snapshot": snapshot,
        "expected_receivers": distributor.inner.config.expected_receivers,
        "acked_receivers": acked,
        "pending_receivers": pending,
        "receipt_count": state.durable.receipts.len(),
        "receipts": state.durable.receipts,
        "storage": {
            "mutation_poisoned": state.mutation_poison.is_some(),
            "error_code": state.mutation_poison,
        },
        "transport_security": {
            "mode": distributor.inner.config.transport_security.mode(),
            "client_certificate_required": distributor
                .inner
                .config
                .transport_security
                .client_certificate_required(),
            "minimum_protocol": distributor
                .inner
                .config
                .transport_security
                .minimum_protocol(),
        },
    }))
    .into_response()
}

fn publish_response(
    status: StatusCode,
    outcome: &'static str,
    snapshot: &ServiceTrustSnapshot,
) -> Response {
    let etag = snapshot_etag(snapshot);
    let mut response = (
        status,
        Json(json!({
            "schema": PUBLISH_SCHEMA,
            "outcome": outcome,
            "generation": snapshot.policy.generation,
            "root_key_id": snapshot.authentication.key_id,
            "etag": etag,
        })),
    )
        .into_response();
    insert_snapshot_cache_headers(response.headers_mut(), &etag);
    response
}

fn receipt_response(
    status: StatusCode,
    outcome: &'static str,
    receipt: &ServiceTrustApplicationReceipt,
) -> Response {
    (
        status,
        Json(json!({
            "schema": RECEIPT_ACCEPTANCE_SCHEMA,
            "outcome": outcome,
            "generation": receipt.payload.generation,
            "receiver": format!(
                "{}/{}",
                receipt.payload.receiver_service_id,
                receipt.payload.receiver_credential_id
            ),
        })),
    )
        .into_response()
}

fn receipt_matches_snapshot(
    receipt: &ServiceTrustApplicationReceipt,
    snapshot: &ServiceTrustSnapshot,
) -> bool {
    receipt.payload.cluster_id == snapshot.policy.cluster_id
        && receipt.payload.generation == snapshot.policy.generation
        && receipt.payload.root_key_id == snapshot.authentication.key_id
        && receipt.payload.snapshot_signature == snapshot.authentication.signature
}

fn snapshot_etag(snapshot: &ServiceTrustSnapshot) -> String {
    format!(
        "\"{}:{}:{}:{}\"",
        snapshot.policy.cluster_id,
        snapshot.policy.generation,
        snapshot.authentication.key_id,
        snapshot.authentication.signature
    )
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag))
}

fn insert_snapshot_cache_headers(headers: &mut HeaderMap, etag: &str) {
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(value) = HeaderValue::from_str(etag) {
        headers.insert(ETAG, value);
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "trust-distributor could not durably complete the operation",
        )
    }

    fn mutation_poisoned() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_mutation_poisoned",
            "trust-distributor mutations are disabled until restart reconciles durable state",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {"code": self.code, "message": self.message}
            })),
        )
            .into_response()
    }
}

fn load_state(path: &Path, max_body_bytes: usize) -> Result<DurableState, DistributorError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(DistributorError::storage(format!(
                "cannot inspect trust-distributor state {}: {error}",
                path.display()
            )));
        }
    };
    let Some(metadata) = metadata else {
        return Ok(DurableState::default());
    };
    let max_state_bytes = maximum_state_bytes(max_body_bytes);
    if metadata.len() > u64::try_from(max_state_bytes).unwrap_or(u64::MAX) {
        return Err(DistributorError::storage(format!(
            "trust-distributor state exceeds the {max_state_bytes}-byte bound"
        )));
    }
    let bytes = fs::read(path).map_err(|error| {
        DistributorError::storage(format!(
            "cannot read trust-distributor state {}: {error}",
            path.display()
        ))
    })?;
    if bytes.len() > max_state_bytes {
        return Err(DistributorError::storage(format!(
            "trust-distributor state exceeds the {max_state_bytes}-byte bound"
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        DistributorError::storage(format!(
            "cannot decode trust-distributor state {}: {error}",
            path.display()
        ))
    })
}

fn validate_durable_state(
    config: &DistributorConfig,
    roots: &TrustedServiceTrustRootKeyRing,
    state: &DurableState,
) -> Result<(), DistributorError> {
    if state.schema != STATE_SCHEMA {
        return Err(DistributorError::storage(format!(
            "unsupported durable state schema '{}'; expected '{STATE_SCHEMA}'",
            state.schema
        )));
    }
    let Some(snapshot) = state.current_snapshot.as_ref() else {
        if state.receipts.is_empty() {
            return Ok(());
        }
        return Err(DistributorError::storage(
            "durable receipts cannot exist without a current snapshot",
        ));
    };
    let verified = roots.verify(snapshot).map_err(|error| {
        DistributorError::storage(format!("persisted snapshot is invalid: {error}"))
    })?;
    if verified.policy.cluster_id != config.cluster_id {
        return Err(DistributorError::storage(
            "persisted snapshot cluster does not match configured cluster",
        ));
    }
    let snapshot_bytes = serde_json::to_vec(snapshot).map_err(|error| {
        DistributorError::storage(format!("encode persisted snapshot: {error}"))
    })?;
    if snapshot_bytes.len() > config.max_body_bytes {
        return Err(DistributorError::storage(format!(
            "persisted snapshot exceeds the {}-byte response bound",
            config.max_body_bytes
        )));
    }
    validate_expected_receivers(config, &verified).map_err(DistributorError::storage)?;
    if state.receipts.len() > config.expected_receivers.len() {
        return Err(DistributorError::storage(
            "persisted receipt count exceeds expected receiver bound",
        ));
    }
    let mut seen = BTreeSet::new();
    for receipt in &state.receipts {
        let receiver = format!(
            "{}/{}",
            receipt.payload.receiver_service_id, receipt.payload.receiver_credential_id
        );
        if !config.expected_receivers.contains(&receiver) {
            return Err(DistributorError::storage(format!(
                "persisted receipt receiver '{receiver}' is not expected"
            )));
        }
        if !seen.insert(receiver.clone()) {
            return Err(DistributorError::storage(format!(
                "persisted receipt receiver '{receiver}' is duplicated"
            )));
        }
        if !receipt_matches_snapshot(receipt, snapshot) {
            return Err(DistributorError::storage(format!(
                "persisted receipt for '{receiver}' does not identify the current snapshot"
            )));
        }
        verified
            .compiled
            .keys
            .verify_trust_receipt(receipt)
            .map_err(|error| {
                DistributorError::storage(format!(
                    "persisted receipt for '{receiver}' is invalid: {error}"
                ))
            })?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistenceFailurePhase {
    BeforeReplace,
    AfterReplace,
}

#[derive(Debug)]
struct PersistenceFailure {
    phase: PersistenceFailurePhase,
    error: DistributorError,
}

impl PersistenceFailure {
    fn before_replace(message: impl Into<String>) -> Self {
        Self {
            phase: PersistenceFailurePhase::BeforeReplace,
            error: DistributorError::storage(message),
        }
    }

    fn after_replace(message: impl Into<String>) -> Self {
        Self {
            phase: PersistenceFailurePhase::AfterReplace,
            error: DistributorError::storage(message),
        }
    }
}

impl std::fmt::Display for PersistenceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

fn persist_state(
    path: &Path,
    state: &DurableState,
    fail_directory_sync: bool,
) -> Result<(), PersistenceFailure> {
    persist_state_with_sequence(path, state, fail_directory_sync, &NEXT_STATE_TEMP_SEQUENCE)
}

fn persist_state_with_sequence(
    path: &Path,
    state: &DurableState,
    fail_directory_sync: bool,
    sequence: &AtomicU64,
) -> Result<(), PersistenceFailure> {
    let bytes = serde_json::to_vec(state)
        .map_err(|error| PersistenceFailure::before_replace(format!("serialize state: {error}")))?;
    if let Some(parent) = explicit_parent(path) {
        fs::create_dir_all(parent).map_err(|error| {
            PersistenceFailure::before_replace(format!("create state directory: {error}"))
        })?;
    }
    let (temporary, mut file) = open_unique_state_temporary(path, sequence)?;
    let write_result = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(PersistenceFailure::before_replace(format!(
            "write or sync state: {error}"
        )));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(PersistenceFailure::before_replace(format!(
            "replace state: {error}"
        )));
    }
    if fail_directory_sync {
        return Err(PersistenceFailure::after_replace(
            "injected state-directory sync failure",
        ));
    }
    let durability_parent = explicit_parent(path).unwrap_or_else(|| Path::new("."));
    File::open(durability_parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            PersistenceFailure::after_replace(format!("sync state directory: {error}"))
        })?;
    Ok(())
}

fn open_unique_state_temporary(
    path: &Path,
    sequence: &AtomicU64,
) -> Result<(PathBuf, File), PersistenceFailure> {
    for _ in 0..MAX_ATOMIC_TEMP_ATTEMPTS {
        let nonce = sequence.fetch_add(1, Ordering::Relaxed);
        let temporary = state_temporary_path(path, nonce);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PersistenceFailure::before_replace(format!(
                    "open temporary state: {error}"
                )));
            }
        }
    }
    Err(PersistenceFailure::before_replace(format!(
        "could not allocate a unique temporary state file after {MAX_ATOMIC_TEMP_ATTEMPTS} attempts"
    )))
}

fn state_temporary_path(path: &Path, nonce: u64) -> PathBuf {
    path.with_extension(format!("tmp-{}-{nonce}", std::process::id()))
}

fn explicit_parent(path: &Path) -> Option<&Path> {
    path.parent().filter(|path| !path.as_os_str().is_empty())
}

fn maximum_state_bytes(max_body_bytes: usize) -> usize {
    max_body_bytes
        .saturating_mul(2)
        .saturating_add(MAX_EXPECTED_RECEIVERS.saturating_mul(1024))
}

fn validate_qualified_receiver(receiver: &str) -> Result<(), DistributorError> {
    let Some((service_id, credential_id)) = receiver.split_once('/') else {
        return Err(DistributorError::configuration(format!(
            "expected receiver '{receiver}' must use service-id/credential-id"
        )));
    };
    if credential_id.contains('/') {
        return Err(DistributorError::configuration(format!(
            "expected receiver '{receiver}' must contain exactly one '/'"
        )));
    }
    service_auth::validate_service_id(service_id)
        .map_err(|error| DistributorError::configuration(error.to_string()))?;
    service_auth::validate_service_id(credential_id)
        .map_err(|error| DistributorError::configuration(error.to_string()))
}

fn validate_expected_receivers(
    config: &DistributorConfig,
    verified: &VerifiedServiceTrustSnapshot,
) -> Result<(), String> {
    let trusted = verified
        .compiled
        .keys
        .trusted_service_credentials()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let revoked_services = verified
        .compiled
        .keys
        .revoked_service_ids()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let revoked_credentials = verified
        .compiled
        .keys
        .revoked_service_credentials()
        .into_iter()
        .collect::<BTreeSet<_>>();
    for receiver in &config.expected_receivers {
        let service_id = receiver
            .split_once('/')
            .map(|(service_id, _)| service_id)
            .unwrap_or_default();
        if !trusted.contains(receiver)
            || revoked_services.contains(service_id)
            || revoked_credentials.contains(receiver)
        {
            return Err(format!(
                "expected receiver '{receiver}' must be trusted and unrevoked in the published snapshot"
            ));
        }
    }
    Ok(())
}

pub fn parse_expected_receivers(value: &str) -> Result<BTreeSet<String>, DistributorError> {
    let mut receivers = BTreeSet::new();
    for raw in value.split(',') {
        let receiver = raw.trim();
        if receiver.is_empty() {
            continue;
        }
        validate_qualified_receiver(receiver)?;
        if !receivers.insert(receiver.to_owned()) {
            return Err(DistributorError::configuration(format!(
                "expected receiver '{receiver}' is duplicated"
            )));
        }
        if receivers.len() > MAX_EXPECTED_RECEIVERS {
            return Err(DistributorError::configuration(format!(
                "expected receivers exceed the {MAX_EXPECTED_RECEIVERS}-receiver bound"
            )));
        }
    }
    if receivers.is_empty() {
        return Err(DistributorError::configuration(
            "at least one expected receiver is required",
        ));
    }
    Ok(receivers)
}

#[cfg(test)]
mod failpoint_tests {
    use axum::{body::to_bytes, extract::State};
    use service_auth::{
        SERVICE_TRUST_POLICY_SCHEMA, ServiceSigningIdentity, ServiceTrustCredential,
        ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity,
    };

    use super::*;

    const ROOT_SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
    const RECEIVER_SEED: &str = "TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs=";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn state_persistence_skips_an_existing_tls_temp_candidate() {
        let sequence_number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "inferlab-trust-distributor-temp-collision-{}-{sequence_number}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let state_path = directory.join("state.json");
        let sequence = AtomicU64::new(0);
        let tls_key_path = state_temporary_path(&state_path, 0);
        let tls_key_bytes = b"protected TLS private-key material";
        fs::write(&tls_key_path, tls_key_bytes).expect("write protected TLS key fixture");

        persist_state_with_sequence(&state_path, &DurableState::default(), false, &sequence)
            .expect("persist through a unique temporary file");

        assert_eq!(
            fs::read(&tls_key_path).expect("read protected TLS key fixture"),
            tls_key_bytes,
            "an existing TLS file at the first temporary candidate must never be truncated or renamed"
        );
        let persisted = fs::read(&state_path).expect("read persisted state");
        serde_json::from_slice::<DurableState>(&persisted).expect("decode persisted state");
        assert_eq!(sequence.load(Ordering::Relaxed), 2);

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn post_replace_failure_reconciles_memory_and_fail_stops_until_restart() {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "inferlab-trust-distributor-failpoint-{}-{sequence}",
            std::process::id()
        ));
        let config = DistributorConfig {
            cluster_id: "inferlab-primary".to_owned(),
            state_path: directory.join("state.json"),
            expected_receivers: BTreeSet::from(["control-a/key-a".to_owned()]),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            transport_security: ServerTransportStatus::Http,
        };
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
        let make_snapshot = |generation| {
            root.sign(&ServiceTrustPolicyPayload {
                schema: SERVICE_TRUST_POLICY_SCHEMA.to_owned(),
                cluster_id: "inferlab-primary".to_owned(),
                generation,
                issued_at_ms: 1_700_000_000_000 + generation,
                trusted_credentials: vec![ServiceTrustCredential {
                    service_id: "control-a".to_owned(),
                    credential_id: "key-a".to_owned(),
                    public_key_base64: receiver.public_key_base64(),
                }],
                revoked_service_ids: Vec::new(),
                revoked_credentials: Vec::new(),
                gateway_service_ids: vec!["control-a".to_owned()],
            })
            .expect("snapshot")
        };
        let generation_one = make_snapshot(1);
        let generation_two = make_snapshot(2);
        let distributor = TrustDistributor::open(config.clone(), roots.clone()).expect("open");
        distributor.fail_next_directory_sync_for_test();

        let failed = publish_snapshot(
            State(distributor.clone()),
            Bytes::from(serde_json::to_vec(&generation_one).expect("encode generation one")),
        )
        .await;
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        {
            let state = distributor.state().await;
            assert_eq!(
                state
                    .durable
                    .current_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.policy.generation),
                Some(1),
                "memory must reconcile to the snapshot already renamed onto disk"
            );
            assert_eq!(state.mutation_poison, Some(MUTATION_DURABILITY_UNCERTAIN));
        }

        let ready = readiness(State(distributor.clone())).await;
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        let ready_body = to_bytes(ready.into_body(), DEFAULT_MAX_BODY_BYTES)
            .await
            .expect("ready body");
        let ready_json: serde_json::Value =
            serde_json::from_slice(&ready_body).expect("ready JSON");
        assert_eq!(ready_json["reason"], "storage_mutation_poisoned");
        assert_eq!(
            ready_json["storage_error_code"],
            MUTATION_DURABILITY_UNCERTAIN
        );

        let status_response = status(State(distributor.clone())).await;
        let status_body = to_bytes(status_response.into_body(), DEFAULT_MAX_BODY_BYTES)
            .await
            .expect("status body");
        let status_json: serde_json::Value =
            serde_json::from_slice(&status_body).expect("status JSON");
        assert_eq!(status_json["snapshot"]["generation"], 1);
        assert_eq!(status_json["storage"]["mutation_poisoned"], true);
        assert_eq!(
            status_json["storage"]["error_code"],
            MUTATION_DURABILITY_UNCERTAIN
        );
        let failed_metrics = distributor.metrics_snapshot().await;
        assert_eq!(failed_metrics.snapshot_storage_errors, 1);
        assert_eq!(failed_metrics.generation, 1);
        assert!(!failed_metrics.storage_healthy);

        let rejected = publish_snapshot(
            State(distributor),
            Bytes::from(serde_json::to_vec(&generation_two).expect("encode generation two")),
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        let rejected_body = to_bytes(rejected.into_body(), DEFAULT_MAX_BODY_BYTES)
            .await
            .expect("rejection body");
        let rejected_json: serde_json::Value =
            serde_json::from_slice(&rejected_body).expect("rejection JSON");
        assert_eq!(rejected_json["error"]["code"], "storage_mutation_poisoned");

        let restarted = TrustDistributor::open(config, roots).expect("restart from renamed state");
        {
            let state = restarted.state().await;
            assert_eq!(
                state
                    .durable
                    .current_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.policy.generation),
                Some(1)
            );
            assert_eq!(state.mutation_poison, None);
        }
        let accepted = publish_snapshot(
            State(restarted),
            Bytes::from(serde_json::to_vec(&generation_two).expect("encode generation two")),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::CREATED);

        let _ = std::fs::remove_dir_all(directory);
    }
}

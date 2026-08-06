pub mod admission;
pub mod circuit_breaker;
pub mod resilience;
pub mod routing;
pub mod routing_lease;
pub mod routing_snapshot_store;

use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{
        HeaderMap, StatusCode,
        header::{CONTENT_TYPE, RETRY_AFTER},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::time::{Instant as TokioInstant, sleep, timeout, timeout_at};
use tracing::{info, warn};

use crate::{
    admission::{AdmissionConfig, AdmissionController, ExecutionGuard, RequestAdmissionPermit},
    resilience::{RequestContext, ResilienceConfig, ResilienceController},
    routing::{RoutingPolicy, WorkerLease, WorkerPool},
    routing_lease::{RoutingLeaseAdmission, SharedRoutingLease},
};

#[derive(Clone)]
struct AppState {
    client: Client,
    routing: SharedRoutingSnapshot,
    control_plane: Option<SharedControlPlaneStatus>,
    routing_lease: Option<SharedRoutingLease>,
    admission: Arc<AdmissionController>,
    resilience: Arc<ResilienceController>,
}

#[derive(Clone)]
struct RequestMiddlewareState {
    admission: Arc<AdmissionController>,
    resilience: Arc<ResilienceController>,
}

struct CompletedAttempt {
    response: reqwest::Response,
    lease: WorkerLease,
    execution_guard: ExecutionGuard,
    worker_id: String,
    attempt_number: usize,
}

enum RetrySchedule {
    Retry,
    Stop,
    DeadlineExceeded,
}

pub type SharedRoutingSnapshot = Arc<RwLock<RoutingSnapshot>>;
pub type SharedControlPlaneStatus = Arc<RwLock<ControlPlaneStatus>>;

#[derive(Clone)]
pub struct RoutingSnapshot {
    pub workers: Arc<WorkerPool>,
    pub control_revision: Option<u64>,
    pub control_term: Option<u64>,
}

impl RoutingSnapshot {
    pub fn static_workers(workers: Arc<WorkerPool>) -> Self {
        Self {
            workers,
            control_revision: None,
            control_term: None,
        }
    }

    pub fn committed(workers: Arc<WorkerPool>, revision: u64, term: u64) -> Self {
        Self {
            workers,
            control_revision: Some(revision),
            control_term: Some(term),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ControlPlaneStatus {
    pub enabled: bool,
    pub bootstrap_source: Option<String>,
    pub source_url: Option<String>,
    pub revision: Option<u64>,
    pub term: Option<u64>,
    pub last_refresh_ms: Option<u64>,
    pub last_error: Option<String>,
    pub snapshot_path: Option<String>,
    pub snapshot_max_age_ms: Option<u64>,
    pub snapshot_max_future_skew_ms: Option<u64>,
    pub bootstrap_snapshot_age_ms: Option<u64>,
    pub persisted_revision: Option<u64>,
    pub persisted_at_ms: Option<u64>,
    pub persisted_expires_at_ms: Option<u64>,
}

pub fn app(workers: Arc<WorkerPool>) -> Router {
    app_with_admission(workers, AdmissionConfig::default())
        .expect("default admission configuration is valid")
}

pub fn app_with_admission(
    workers: Arc<WorkerPool>,
    admission_config: AdmissionConfig,
) -> Result<Router, String> {
    app_with_config(workers, admission_config, ResilienceConfig::default())
}

pub fn app_with_config(
    workers: Arc<WorkerPool>,
    admission_config: AdmissionConfig,
    resilience_config: ResilienceConfig,
) -> Result<Router, String> {
    app_with_dynamic_config(
        Arc::new(RwLock::new(RoutingSnapshot::static_workers(workers))),
        None,
        admission_config,
        resilience_config,
    )
}

pub fn app_with_dynamic_config(
    routing: SharedRoutingSnapshot,
    control_plane: Option<SharedControlPlaneStatus>,
    admission_config: AdmissionConfig,
    resilience_config: ResilienceConfig,
) -> Result<Router, String> {
    app_with_runtime_config(
        routing,
        control_plane,
        None,
        admission_config,
        resilience_config,
    )
}

pub fn app_with_runtime_config(
    routing: SharedRoutingSnapshot,
    control_plane: Option<SharedControlPlaneStatus>,
    routing_lease: Option<SharedRoutingLease>,
    admission_config: AdmissionConfig,
    resilience_config: ResilienceConfig,
) -> Result<Router, String> {
    let execution_capacity = current_routing(&routing).workers.total_execution_capacity();
    let admission = AdmissionController::new(admission_config, execution_capacity)?;
    let resilience = ResilienceController::new(resilience_config)?;
    let state = AppState {
        // Reusing a client preserves its connection pool. Constructing one per request would pay
        // repeated connection setup costs and hide the behavior of a real gateway.
        client: Client::new(),
        routing,
        control_plane,
        routing_lease,
        admission: Arc::clone(&admission),
        resilience: Arc::clone(&resilience),
    };
    let completion_route =
        post(proxy_chat_completions).route_layer(middleware::from_fn_with_state(
            RequestMiddlewareState {
                admission,
                resilience,
            },
            admission_middleware,
        ));

    Ok(Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readiness))
        .route("/internal/workers", get(worker_status))
        .route("/v1/chat/completions", completion_route)
        .with_state(state))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "service": "inferlab-gateway"}))
}

async fn readiness(State(state): State<AppState>) -> Response {
    let routing_lease = state.routing_lease.as_ref().map(|lease| lease.snapshot());
    let ready = routing_lease
        .as_ref()
        .is_none_or(|lease| lease.accepting_new_requests);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status": if ready { "ready" } else { "not-ready" },
            "reason": (!ready).then_some("routing_lease_expired"),
            "routing_lease": routing_lease
        })),
    )
        .into_response()
}

async fn worker_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let routing = current_routing(&state.routing);
    let workers = &routing.workers;
    let control_plane = state.control_plane.as_ref().map(read_control_plane_status);
    let routing_lease = state.routing_lease.as_ref().map(|lease| lease.snapshot());
    Json(json!({
        "routing_policy": workers.policy(),
        "routing_snapshot": {
            "control_revision": routing.control_revision,
            "control_term": routing.control_term,
        },
        "admission": state.admission.snapshot(),
        "resilience": state.resilience.snapshot(),
        "workers": workers.snapshots(),
        "control_plane": control_plane,
        "routing_lease": routing_lease
    }))
}

async fn admission_middleware(
    State(middleware_state): State<RequestMiddlewareState>,
    mut request: Request,
    next: Next,
) -> Response {
    let permit = match middleware_state.admission.try_admit_request() {
        Ok(permit) => permit,
        Err(_) => return overload_error(),
    };
    let request_context = middleware_state.resilience.start_request();
    request.extensions_mut().insert(permit);
    request.extensions_mut().insert(request_context);
    next.run(request).await
}

async fn proxy_chat_completions(
    State(state): State<AppState>,
    Extension(request_permit): Extension<RequestAdmissionPermit>,
    Extension(request_context): Extension<RequestContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The runtime lease is an admission fence, not a cancellation deadline. Once this check
    // passes, the request keeps the routing identity it owns even if a long SSE crosses expiry.
    if let Some(lease) = state.routing_lease.as_ref()
        && lease.admit_new() == RoutingLeaseAdmission::Rejected
    {
        return routing_lease_expired_error();
    }
    // One request holds one immutable pool snapshot. A control-plane refresh can
    // replace the shared pointer without changing ownership midway through a
    // stream or retry sequence.
    let routing = current_routing(&state.routing);
    let workers = Arc::clone(&routing.workers);
    let routing_key = (workers.policy() == RoutingPolicy::ConsistentHash)
        .then(|| prompt_affinity_key(&headers, &body));
    let mut attempted_workers = HashSet::new();
    let mut retries_used = 0;
    let mut attempts_started = 0;

    let CompletedAttempt {
        response: upstream,
        mut lease,
        execution_guard,
        worker_id,
        attempt_number,
    } = loop {
        let Some(_) = request_context.remaining() else {
            state.resilience.record_deadline_exceeded();
            return deadline_error(request_context, attempts_started);
        };
        let selected = if retries_used == 0 {
            match routing_key.as_deref() {
                Some(key) => workers.try_choose_for_key(key),
                None => workers.try_choose(),
            }
        } else {
            workers.try_choose_retry(&attempted_workers)
        };
        let Some(mut lease) = selected else {
            warn!(
                request_number = request_context.request_number(),
                attempts = attempts_started,
                "all worker circuits rejected the routing attempt"
            );
            return no_available_workers_error(attempts_started);
        };
        let worker_id = lease.id().to_owned();
        attempted_workers.insert(worker_id.clone());
        let endpoint = lease.endpoint("/v1/chat/completions");
        let execution_guard = match timeout_at(
            TokioInstant::from_std(request_context.deadline()),
            state.admission.admit_worker(&lease),
        )
        .await
        {
            Ok(Ok(guard)) => guard,
            Ok(Err(_)) => return overload_error(),
            Err(_) => {
                state.resilience.record_deadline_exceeded();
                return deadline_error(request_context, attempts_started);
            }
        };
        let Some(remaining) = request_context.remaining() else {
            state.resilience.record_deadline_exceeded();
            return deadline_error(request_context, attempts_started);
        };
        attempts_started += 1;
        let attempt_number = attempts_started;
        let attempt_timeout = remaining.min(state.resilience.attempt_timeout());
        state.resilience.record_attempt();

        info!(
            request_number = request_context.request_number(),
            %worker_id,
            %endpoint,
            attempt = attempt_number,
            timeout_ms = duration_header_millis(attempt_timeout),
            policy = %workers.policy(),
            "routing chat completion attempt"
        );

        let result = timeout(
            attempt_timeout,
            state
                .client
                .post(endpoint)
                .header(CONTENT_TYPE, "application/json")
                .header(
                    "x-inferlab-timeout-ms",
                    duration_header_millis(attempt_timeout),
                )
                .header("x-inferlab-attempt", attempt_number)
                .body(body.clone())
                .send(),
        )
        .await;

        match result {
            Ok(Ok(response)) => {
                if is_transient_status(response.status()) {
                    lease.record_circuit_failure();
                    state.resilience.record_transient_failure();
                    if let Some((reservation, delay)) =
                        reserve_retry_plan(&state.resilience, retries_used)
                    {
                        // Backoff must not occupy a scarce worker execution permit.
                        drop(response);
                        drop(execution_guard);
                        drop(lease);
                        match wait_for_reserved_retry(
                            &state.resilience,
                            request_context,
                            retries_used,
                            reservation,
                            delay,
                        )
                        .await
                        {
                            RetrySchedule::Retry => {
                                retries_used += 1;
                                continue;
                            }
                            RetrySchedule::DeadlineExceeded => {
                                return deadline_error(request_context, attempts_started);
                            }
                            RetrySchedule::Stop => {
                                unreachable!("a reserved retry either runs or reaches its deadline")
                            }
                        }
                    }
                } else {
                    lease.record_circuit_success();
                }
                break CompletedAttempt {
                    response,
                    lease,
                    execution_guard,
                    worker_id,
                    attempt_number,
                };
            }
            Ok(Err(error)) => {
                let retryable = is_transient_error(&error);
                if retryable {
                    lease.record_circuit_failure();
                    state.resilience.record_transient_failure();
                }
                warn!(
                    request_number = request_context.request_number(),
                    %worker_id,
                    attempt = attempt_number,
                    %error,
                    retryable,
                    "worker attempt failed before response headers"
                );
                drop(execution_guard);
                drop(lease);

                if request_context.remaining().is_none() {
                    state.resilience.record_deadline_exceeded();
                    return deadline_error(request_context, attempts_started);
                }
                if retryable {
                    match schedule_retry(&state.resilience, request_context, retries_used).await {
                        RetrySchedule::Retry => {
                            retries_used += 1;
                            continue;
                        }
                        RetrySchedule::DeadlineExceeded => {
                            return deadline_error(request_context, attempts_started);
                        }
                        RetrySchedule::Stop => {}
                    }
                }
                let (status, kind) = if error.is_timeout() {
                    (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout")
                } else {
                    (StatusCode::SERVICE_UNAVAILABLE, "worker_connection_failed")
                };
                return gateway_error_with_attempts(
                    status,
                    kind,
                    format!("attempt {attempt_number} failed on {worker_id}"),
                    attempt_number,
                );
            }
            Err(_) => {
                lease.record_circuit_failure();
                state.resilience.record_transient_failure();
                warn!(
                    request_number = request_context.request_number(),
                    %worker_id,
                    attempt = attempt_number,
                    timeout_ms = duration_header_millis(attempt_timeout),
                    "worker attempt timed out before response headers"
                );
                drop(execution_guard);
                drop(lease);

                if request_context.remaining().is_none() {
                    state.resilience.record_deadline_exceeded();
                    return deadline_error(request_context, attempts_started);
                }
                match schedule_retry(&state.resilience, request_context, retries_used).await {
                    RetrySchedule::Retry => {
                        retries_used += 1;
                        continue;
                    }
                    RetrySchedule::DeadlineExceeded => {
                        return deadline_error(request_context, attempts_started);
                    }
                    RetrySchedule::Stop => {
                        return gateway_error_with_attempts(
                            StatusCode::GATEWAY_TIMEOUT,
                            "upstream_timeout",
                            format!(
                                "attempt {attempt_number} timed out on {worker_id} before response headers"
                            ),
                            attempt_number,
                        );
                    }
                }
            }
        }
    };

    let status = upstream.status();
    let content_type = upstream.headers().get(CONTENT_TYPE).cloned();
    let observe_latency = status.is_success();
    let deadline_controller = Arc::clone(&state.resilience);
    let deadline_future = async move {
        sleep_until_std(request_context.deadline()).await;
        deadline_controller.record_deadline_exceeded();
        warn!(
            request_number = request_context.request_number(),
            elapsed_ms = request_context.elapsed().as_millis(),
            "request deadline ended downstream stream"
        );
    };

    // The lease is moved into this generator. It stays live after headers are sent and is dropped
    // only when the entire body completes or the downstream client abandons it.
    let mut first_chunk = true;
    let body_stream = upstream
        .bytes_stream()
        .map(move |chunk| {
            if first_chunk {
                first_chunk = false;
                if observe_latency && chunk.is_ok() {
                    lease.observe_latency();
                }
            }
            // Referencing the captured lease makes its ownership intentional: the mapping closure,
            // and therefore the body stream, owns it until completion or cancellation.
            let _keep_lease_alive = &lease;
            let _keep_execution_slot = &execution_guard;
            let _keep_request_admitted = &request_permit;
            chunk
        })
        .take_until(deadline_future);

    let mut builder = Response::builder()
        .status(status)
        .header("x-inferlab-worker", worker_id)
        .header("x-inferlab-attempts", attempt_number);
    if let Some(revision) = routing.control_revision {
        builder = builder.header("x-inferlab-config-revision", revision);
    }
    if let Some(term) = routing.control_term {
        builder = builder.header("x-inferlab-config-term", term);
    }
    if let Some(value) = content_type {
        builder = builder.header(CONTENT_TYPE, value);
    }

    builder
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|_| {
            gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_build_failed",
                "could not build downstream response",
            )
        })
}

fn current_routing(routing: &SharedRoutingSnapshot) -> RoutingSnapshot {
    routing
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn read_control_plane_status(status: &SharedControlPlaneStatus) -> ControlPlaneStatus {
    status
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

async fn schedule_retry(
    resilience: &Arc<ResilienceController>,
    request_context: RequestContext,
    retries_used: usize,
) -> RetrySchedule {
    let Some((reservation, delay)) = reserve_retry_plan(resilience, retries_used) else {
        return RetrySchedule::Stop;
    };
    wait_for_reserved_retry(
        resilience,
        request_context,
        retries_used,
        reservation,
        delay,
    )
    .await
}

fn reserve_retry_plan(
    resilience: &Arc<ResilienceController>,
    retries_used: usize,
) -> Option<(crate::resilience::RetryReservation, Duration)> {
    if retries_used >= resilience.max_retries() {
        resilience.record_retry_limit_exhausted();
        return None;
    }
    resilience.reserve_retry(retries_used)
}

async fn wait_for_reserved_retry(
    resilience: &Arc<ResilienceController>,
    request_context: RequestContext,
    retries_used: usize,
    reservation: crate::resilience::RetryReservation,
    delay: Duration,
) -> RetrySchedule {
    info!(
        request_number = request_context.request_number(),
        retry = retries_used + 1,
        delay_ms = delay.as_millis(),
        "retry budget granted with full-jitter backoff"
    );
    if timeout_at(
        TokioInstant::from_std(request_context.deadline()),
        sleep(delay),
    )
    .await
    .is_err()
    {
        resilience.record_deadline_exceeded();
        return RetrySchedule::DeadlineExceeded;
    }
    if request_context.remaining().is_none() {
        resilience.record_deadline_exceeded();
        return RetrySchedule::DeadlineExceeded;
    }
    reservation.commit();
    RetrySchedule::Retry
}

async fn sleep_until_std(deadline: std::time::Instant) {
    tokio::time::sleep_until(TokioInstant::from_std(deadline)).await;
}

fn is_transient_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

fn is_transient_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

fn duration_header_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn deadline_error(request_context: RequestContext, attempts_started: usize) -> Response {
    gateway_error_with_attempts(
        StatusCode::GATEWAY_TIMEOUT,
        "request_deadline_exceeded",
        format!(
            "request {} exhausted its time budget after {} ms",
            request_context.request_number(),
            request_context.elapsed().as_millis()
        ),
        attempts_started,
    )
}

fn overload_error() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(RETRY_AFTER, "1")],
        Json(json!({
            "error": {
                "type": "gateway_overloaded",
                "reason": "admission_queue_full",
                "message": "gateway execution and waiting capacity are full",
                "retryable": true
            }
        })),
    )
        .into_response()
}

fn routing_lease_expired_error() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "type": "routing_lease_expired",
                "reason": "runtime_routing_lease_expired",
                "message": "gateway cannot verify its routing configuration; retry after control-plane recovery",
                "retryable": true
            }
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, "1".parse().expect("static header value"));
    response.headers_mut().insert(
        "x-inferlab-attempts",
        "0".parse().expect("static header value"),
    );
    response
}

fn no_available_workers_error(attempts: usize) -> Response {
    let mut response = gateway_error_with_attempts(
        StatusCode::SERVICE_UNAVAILABLE,
        "no_available_workers",
        "all worker circuits are open or already running a half-open probe",
        attempts,
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, "1".parse().expect("static header value"));
    response
}

fn prompt_affinity_key(headers: &HeaderMap, body: &Bytes) -> Vec<u8> {
    if let Some(value) = headers
        .get("x-inferlab-cache-key")
        .filter(|value| !value.as_bytes().is_empty())
    {
        return value.as_bytes().to_vec();
    }

    // Sampling parameters and `stream` change how a completion is delivered, not which prompt
    // prefix could be cached. Canonical JSON also removes insignificant object-key ordering.
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|request| {
            serde_json::to_vec(&json!({
                "messages": request.get("messages").cloned().unwrap_or(Value::Null),
                "model": request.get("model").cloned().unwrap_or(Value::Null),
            }))
            .ok()
        })
        .unwrap_or_else(|| body.to_vec())
}

fn gateway_error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "type": kind,
                "message": message.into()
            }
        })),
    )
        .into_response()
}

fn gateway_error_with_attempts(
    status: StatusCode,
    kind: &str,
    message: impl Into<String>,
    attempts: usize,
) -> Response {
    let mut response = gateway_error(status, kind, message);
    if let Ok(value) = attempts.to_string().parse() {
        response.headers_mut().insert("x-inferlab-attempts", value);
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Bytes,
        http::{HeaderMap, HeaderValue},
    };

    use super::prompt_affinity_key;

    #[test]
    fn explicit_cache_key_overrides_the_request_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-inferlab-cache-key",
            HeaderValue::from_static("tenant-7/prefix-42"),
        );

        assert_eq!(
            prompt_affinity_key(&headers, &Bytes::from_static(b"{\"messages\":[]}")),
            b"tenant-7/prefix-42"
        );
    }

    #[test]
    fn fallback_key_ignores_stream_and_sampling_options() {
        let headers = HeaderMap::new();
        let first = Bytes::from_static(
            br#"{"model":"tiny","stream":true,"temperature":0.1,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let second = Bytes::from_static(
            br#"{"messages":[{"content":"hi","role":"user"}],"temperature":0.9,"stream":false,"model":"tiny"}"#,
        );

        assert_eq!(
            prompt_affinity_key(&headers, &first),
            prompt_affinity_key(&headers, &second)
        );
    }
}

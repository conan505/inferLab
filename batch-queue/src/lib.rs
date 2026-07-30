pub mod model;
mod store;
mod wal;

use std::{
    fmt, io,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use model::{AckRequest, ClaimRequest, EnqueueRequest, FailRequest};
pub use store::QueueStore;

#[derive(Debug)]
pub enum QueueError {
    Invalid(String),
    IdempotencyConflict(String),
    NotFound(String),
    StaleClaim(String),
    Storage(String),
}

impl QueueError {
    pub(crate) fn storage(error: io::Error) -> Self {
        Self::Storage(error.to_string())
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::IdempotencyConflict(_) => "idempotency_conflict",
            Self::NotFound(_) => "not_found",
            Self::StaleClaim(_) => "stale_claim",
            Self::Storage(_) => "storage_error",
        }
    }
}

impl fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::IdempotencyConflict(message)
            | Self::NotFound(message)
            | Self::StaleClaim(message)
            | Self::Storage(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for QueueError {}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl IntoResponse for QueueError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::IdempotencyConflict(_) | Self::StaleClaim(_) => StatusCode::CONFLICT,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message: self.to_string(),
            },
        };
        (status, Json(body)).into_response()
    }
}

pub fn app(store: Arc<QueueStore>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/batch/jobs", post(enqueue))
        .route("/v1/batch/claim", post(claim))
        .route("/v1/batch/jobs/{job_id}", get(get_job))
        .route("/v1/batch/jobs/{job_id}/ack", post(acknowledge))
        .route("/v1/batch/jobs/{job_id}/fail", post(fail))
        .route("/v1/batch/dead-letter", get(dead_letters))
        .route("/internal/status", get(status))
        .with_state(store)
}

async fn health() -> &'static str {
    "ok"
}

async fn enqueue(
    State(store): State<Arc<QueueStore>>,
    Json(request): Json<EnqueueRequest>,
) -> Result<Response, QueueError> {
    let response = blocking(move || store.enqueue(request, now_ms())).await?;
    let status = if response.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(response)).into_response())
}

async fn claim(
    State(store): State<Arc<QueueStore>>,
    Json(request): Json<ClaimRequest>,
) -> Result<Response, QueueError> {
    match blocking(move || store.claim(request, now_ms())).await? {
        Some(response) => Ok((StatusCode::OK, Json(response)).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn acknowledge(
    State(store): State<Arc<QueueStore>>,
    Path(job_id): Path<String>,
    Json(request): Json<AckRequest>,
) -> Result<Response, QueueError> {
    let job = blocking(move || store.acknowledge(&job_id, request, now_ms())).await?;
    Ok((StatusCode::OK, Json(job)).into_response())
}

async fn fail(
    State(store): State<Arc<QueueStore>>,
    Path(job_id): Path<String>,
    Json(request): Json<FailRequest>,
) -> Result<Response, QueueError> {
    let job = blocking(move || store.fail(&job_id, request, now_ms())).await?;
    Ok((StatusCode::OK, Json(job)).into_response())
}

async fn get_job(
    State(store): State<Arc<QueueStore>>,
    Path(job_id): Path<String>,
) -> Result<Response, QueueError> {
    let job = blocking(move || store.get_job(&job_id, now_ms())).await?;
    Ok((StatusCode::OK, Json(job)).into_response())
}

async fn dead_letters(State(store): State<Arc<QueueStore>>) -> Result<Response, QueueError> {
    let jobs = blocking(move || store.dead_letters(now_ms())).await?;
    Ok((StatusCode::OK, Json(jobs)).into_response())
}

async fn status(State(store): State<Arc<QueueStore>>) -> Result<Response, QueueError> {
    let snapshot = blocking(move || store.snapshot(now_ms())).await?;
    Ok((StatusCode::OK, Json(snapshot)).into_response())
}

async fn blocking<T, F>(operation: F) -> Result<T, QueueError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, QueueError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| QueueError::Storage(format!("blocking queue task failed: {error}")))?
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

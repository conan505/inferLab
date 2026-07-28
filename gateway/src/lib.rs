pub mod routing;

use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use tracing::{info, warn};

use crate::routing::WorkerPool;

#[derive(Clone)]
struct AppState {
    client: Client,
    workers: Arc<WorkerPool>,
}

pub fn app(workers: Arc<WorkerPool>) -> Router {
    let state = AppState {
        // Reusing a client preserves its connection pool. Constructing one per request would pay
        // repeated connection setup costs and hide the behavior of a real gateway.
        client: Client::new(),
        workers,
    };

    Router::new()
        .route("/health", get(health))
        .route("/internal/workers", get(worker_status))
        .route("/v1/chat/completions", post(proxy_chat_completions))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "service": "inferlab-gateway"}))
}

async fn worker_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({"workers": state.workers.snapshots()}))
}

async fn proxy_chat_completions(State(state): State<AppState>, body: Bytes) -> Response {
    let lease = state.workers.choose_round_robin();
    let worker_id = lease.id().to_owned();
    let endpoint = lease.endpoint("/v1/chat/completions");

    info!(%worker_id, %endpoint, "routing chat completion");

    let upstream = match state
        .client
        .post(endpoint)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(%worker_id, %error, "worker connection failed");
            return gateway_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "worker_connection_failed",
                format!("could not connect to {worker_id}"),
            );
        }
    };

    let status = upstream.status();
    let content_type = upstream.headers().get(CONTENT_TYPE).cloned();

    // The lease is moved into this generator. It stays live after headers are sent and is dropped
    // only when the entire body completes or the downstream client abandons it.
    let body_stream = upstream.bytes_stream().map(move |chunk| {
        // Referencing the captured lease makes its ownership intentional: the mapping closure,
        // and therefore the body stream, owns it until completion or cancellation.
        let _keep_lease_alive = &lease;
        chunk
    });

    let mut builder = Response::builder()
        .status(status)
        .header("x-inferlab-worker", worker_id);
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

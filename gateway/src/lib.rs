pub mod routing;

use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::routing::{RoutingPolicy, WorkerPool};

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
    Json(json!({
        "routing_policy": state.workers.policy(),
        "workers": state.workers.snapshots()
    }))
}

async fn proxy_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut lease = match state.workers.policy() {
        RoutingPolicy::ConsistentHash => {
            let routing_key = prompt_affinity_key(&headers, &body);
            state.workers.choose_for_key(&routing_key)
        }
        _ => state.workers.choose(),
    };
    let worker_id = lease.id().to_owned();
    let endpoint = lease.endpoint("/v1/chat/completions");

    info!(
        %worker_id,
        %endpoint,
        policy = %state.workers.policy(),
        "routing chat completion"
    );

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
    let observe_latency = status.is_success();

    // The lease is moved into this generator. It stays live after headers are sent and is dropped
    // only when the entire body completes or the downstream client abandons it.
    let mut first_chunk = true;
    let body_stream = upstream.bytes_stream().map(move |chunk| {
        if first_chunk {
            first_chunk = false;
            if observe_latency && chunk.is_ok() {
                lease.observe_latency();
            }
        }
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

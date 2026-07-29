use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::sleep;

const WORKER_HEADER: &str = "x-inferlab-worker";

#[derive(Clone, Debug)]
pub struct Config {
    pub id: String,
    pub initial_delay: Duration,
    pub token_delay: Duration,
    pub fail_first: Option<u64>,
    pub fail_every: Option<u64>,
}

impl Config {
    pub fn for_test(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            initial_delay: Duration::ZERO,
            token_delay: Duration::from_millis(2),
            fail_first: None,
            fail_every: None,
        }
    }
}

#[derive(Clone)]
struct WorkerState {
    config: Config,
    requests: Arc<AtomicU64>,
    last_timeout_ms: Arc<AtomicU64>,
    last_attempt: Arc<AtomicU64>,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[serde(default = "default_model")]
    model: String,
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[allow(dead_code)]
    role: String,
    content: Value,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    worker_id: String,
    requests: u64,
    last_timeout_ms: Option<u64>,
    last_attempt: Option<u64>,
}

pub fn app(config: Config) -> Router {
    let state = WorkerState {
        config,
        requests: Arc::new(AtomicU64::new(0)),
        last_timeout_ms: Arc::new(AtomicU64::new(0)),
        last_attempt: Arc::new(AtomicU64::new(0)),
    };

    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

async fn health(State(state): State<WorkerState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        worker_id: state.config.id.clone(),
        requests: state.requests.load(Ordering::Relaxed),
        last_timeout_ms: nonzero(state.last_timeout_ms.load(Ordering::Relaxed)),
        last_attempt: nonzero(state.last_attempt.load(Ordering::Relaxed)),
    })
}

async fn chat_completions(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> Response {
    let request_number = state.requests.fetch_add(1, Ordering::Relaxed) + 1;
    let worker_id = state.config.id.clone();
    if let Some(timeout_ms) = numeric_header(&headers, "x-inferlab-timeout-ms") {
        state.last_timeout_ms.store(timeout_ms, Ordering::Relaxed);
    }
    if let Some(attempt) = numeric_header(&headers, "x-inferlab-attempt") {
        state.last_attempt.store(attempt, Ordering::Relaxed);
    }

    if state
        .config
        .fail_first
        .is_some_and(|count| request_number <= count)
        || state
            .config
            .fail_every
            .is_some_and(|interval| request_number % interval == 0)
    {
        return with_worker_header(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": {
                        "type": "fake_worker_failure",
                        "message": format!("{worker_id} failed deterministic request {request_number}")
                    }
                })),
            )
                .into_response(),
            &worker_id,
        );
    }

    sleep(state.config.initial_delay).await;

    let created = unix_timestamp();
    let completion_id = format!("chatcmpl-{worker_id}-{request_number}");
    let prompt = last_message_text(&request.messages);
    let answer = format!("Hello from {worker_id}. You said: {prompt}");

    let response = if request.stream {
        streaming_response(
            completion_id,
            request.model,
            answer,
            created,
            state.config.token_delay,
        )
    } else {
        Json(json!({
            "id": completion_id,
            "object": "chat.completion",
            "created": created,
            "model": request.model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": answer},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
        }))
        .into_response()
    };

    with_worker_header(response, &worker_id)
}

fn streaming_response(
    completion_id: String,
    model: String,
    answer: String,
    created: u64,
    token_delay: Duration,
) -> Response {
    let mut payloads = Vec::new();

    payloads.push(
        json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
        })
        .to_string(),
    );

    for token in answer.split_inclusive(' ') {
        payloads.push(
            json!({
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {"content": token}, "finish_reason": null}]
            })
            .to_string(),
        );
    }

    payloads.push(
        json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        })
        .to_string(),
    );
    payloads.push("[DONE]".to_owned());

    let events = stream::iter(payloads).then(move |payload| async move {
        sleep(token_delay).await;
        Ok::<Event, Infallible>(Event::default().data(payload))
    });

    Sse::new(events).into_response()
}

fn with_worker_header(mut response: Response, worker_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(worker_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(WORKER_HEADER), value);
    }
    response
}

fn last_message_text(messages: &[ChatMessage]) -> String {
    match messages.last().map(|message| &message.content) {
        Some(Value::String(content)) => content.clone(),
        Some(content) => content.to_string(),
        None => "(no message)".to_owned(),
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_model() -> String {
    "inferlab-fake".to_owned()
}

fn numeric_header(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}
